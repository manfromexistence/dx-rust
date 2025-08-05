use ahash::AHashMap as HashMap;
use ahash::AHashSet as HashSet;
use blake3::Hasher;
use colored::Colorize;
use crossbeam::channel::{self, Receiver, Sender};
use glob::glob;
use lru::LruCache;
use memmap2::MmapMut;
use mimalloc::MiMalloc;
use polling::{Event, Poller};
use rkyv::{Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};
use rayon::prelude::*;
use std::collections::BinaryHeap;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use swc_common::{sync::Lrc, SourceMap, GLOBALS};
use swc_ecma_ast::{JSXAttrName, JSXAttrOrSpread, JSXAttrValue, JSXOpeningElement, Lit, Module, Program};
use swc_ecma_minifier::optimize;
use swc_ecma_parser::{lexer::Lexer, Parser, StringInput, Syntax, TsSyntax};
use swc_ecma_visit::{Visit, VisitMut, VisitWith};
use tokio::fs;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
struct CachedModule {
    jsx_nodes: Vec<JSXOpeningElement>,
}

#[derive(bincode::Encode, bincode::Decode)]
struct CacheEntry {
    hash: String,
    classnames: HashSet<String>,
}

#[derive(PartialEq, Eq)]
struct FilePriority {
    path: PathBuf,
    change_count: usize,
}

impl Ord for FilePriority {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other.change_count.cmp(&self.change_count)
    }
}

impl PartialOrd for FilePriority {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

struct JSXOnlyCollector<'a> {
    classnames: &'a mut Vec<String>,
    jsx_nodes: &'a mut Vec<JSXOpeningElement>,
}

impl<'a> Visit for JSXOnlyCollector<'a> {
    fn visit_jsx_opening_element(&mut self, elem: &JSXOpeningElement) {
        for attr in &elem.attrs {
            if let JSXAttrOrSpread::JSXAttr(attr) = attr {
                if let JSXAttrName::Ident(ident) = &attr.name {
                    if ident.sym == "className" {
                        if let Some(JSXAttrValue::Lit(Lit::Str(s))) = &attr.value {
                            self.classnames.extend(s.value.split_whitespace().map(String::from));
                        }
                    }
                }
            }
        }
        self.jsx_nodes.push(elem.clone());
    }
}

struct JSXOnlyUpdater<'a> {
    classnames: &'a mut Vec<String>,
    jsx_nodes: &'a mut Vec<JSXOpeningElement>,
}

impl<'a> VisitMut for JSXOnlyUpdater<'a> {
    fn visit_mut_jsx_opening_element(&mut self, elem: &mut JSXOpeningElement) {
        for attr in &mut elem.attrs {
            if let JSXAttrOrSpread::JSXAttr(attr) = attr {
                if let JSXAttrName::Ident(ident) = &attr.name {
                    if ident.sym == "className" {
                        if let Some(JSXAttrValue::Lit(Lit::Str(s))) = &attr.value {
                            self.classnames.extend(s.value.split_whitespace().map(String::from));
                        }
                    }
                }
            }
        }
        self.jsx_nodes.push(elem.clone());
    }
}

struct JSXPruner;

impl VisitMut for JSXPruner {
    fn visit_mut_module(&mut self, module: &mut Module) {
        module.body.retain(|item| matches!(item, swc_ecma_ast::ModuleItem::Stmt(swc_ecma_ast::Stmt::Decl(swc_ecma_ast::Decl::TsInterface(_))) || item.is_module_decl());
        for item in &mut module.body {
            if let swc_ecma_ast::ModuleItem::Stmt(swc_ecma_ast::Stmt::Expr(expr)) = item {
                expr.visit_mut_children_with(self);
            }
        }
    }

    fn visit_mut_jsx_element(&mut self, elem: &mut swc_ecma_ast::JSXElement) {
        elem.children.retain(|child| matches!(child, swc_ecma_ast::JSXElementChild::JSXElement(_)));
        for child in &mut elem.children {
            child.visit_mut_children_with(self);
        }
    }
}

async fn read_file_async(path: &Path) -> Result<Vec<u8>, String> {
    fs::read(path).await.map_err(|e| format!("{:?}", e))
}

#[inline]
fn compute_file_hash(content: &[u8]) -> String {
    use ahash::AHasher;
    use std::hash::Hasher;
    let mut hasher = AHasher::default();
    hasher.write(content);
    format!("{:x}", hasher.finish())
}

fn compute_cache_checksum(content: &[u8]) -> String {
    let mut hasher = Hasher::new();
    hasher.update(content);
    hasher.finalize().to_string()
}

fn parse_and_minify_file(cm: &SourceMap, path: &Path, content: &[u8], cached_nodes: Option<&Arc<ArchivedModule>>) -> Result<(Module, Vec<JSXOpeningElement>), String> {
    let fm = cm.new_source_file(
        swc_common::FileName::Custom(path.to_string_lossy().into()),
        String::from_utf8_lossy(content).into(),
    );
    let lexer = Lexer::new(
        Syntax::Typescript(TsSyntax {
            tsx: true,
            ..Default::default()
        }),
        Default::default(),
        StringInput::from(&*fm),
        None,
    );
    let mut parser = Parser::new_from(lexer);
    let mut module = if let Some(cached) = cached_nodes {
        let archived = unsafe { rkyv::archived_root::<CachedModule>(&cached[..]) };
        let mut module = Module::default();
        let mut updater = JSXOnlyUpdater {
            classnames: &mut Vec::new(),
            jsx_nodes: &mut Vec::new(),
        };
        for node in &archived.jsx_nodes {
            updater.visit_mut_jsx_opening_element(&mut module.body.push(node.clone().into()));
        }
        module
    } else {
        parser.parse_module().map_err(|e| format!("{:?}", e))?
    };
    let mut pruner = JSXPruner;
    module.visit_mut_with(&mut pruner);
    let mut jsx_nodes = Vec::new();
    let mut collector = JSXOnlyCollector {
        classnames: &mut Vec::new(),
        jsx_nodes: &mut jsx_nodes,
    };
    module.visit_with(&mut collector);
    GLOBALS.set(&Default::default(), || {
        let program = Program::Module(module);
        let optimized = optimize(
            program,
            cm.clone(),
            None,
            None,
            &swc_ecma_minifier::option::MinifyOptions {
                compress: Some(Default::default()),
                mangle: None,
                ..Default::default()
            },
            &Default::default(),
        );
        match optimized {
            Program::Module(m) => Ok((m, jsx_nodes)),
            _ => Err("Minified program is not a module".to_string()),
        }
    })
}

#[inline]
fn process_file(cm: &SourceMap, path: &Path, content: &[u8], cached_nodes: Option<&Arc<ArchivedModule>>) -> (HashSet<String>, String, Arc<ArchivedModule>) {
    let mut classnames = Vec::with_capacity(50);
    let hash = compute_file_hash(content);
    let (module, jsx_nodes) = parse_and_minify_file(cm, path, content, cached_nodes).map_or_else(
        |_| (Module::default(), Vec::new()),
        |(m, nodes)| (m, nodes),
    );
    let mut collector = JSXOnlyCollector {
        classnames: &mut classnames,
        jsx_nodes: &mut Vec::new(),
    };
    module.visit_with(&mut collector);
    let archived = rkyv::to_bytes::<_, 1024>(&CachedModule { jsx_nodes }).unwrap().into();
    (
        classnames.into_iter().collect::<HashSet<String>>(),
        hash,
        Arc::new(archived),
    )
}

#[inline]
fn update_from_cached_module(module: Arc<ArchivedModule>) -> HashSet<String> {
    let mut classnames = Vec::with_capacity(50);
    let mut updater = JSXOnlyUpdater {
        classnames: &mut classnames,
        jsx_nodes: &mut Vec::new(),
    };
    let archived = unsafe { rkyv::archived_root::<CachedModule>(&module[..]) };
    for node in &archived.jsx_nodes {
        let mut node = node.clone();
        updater.visit_mut_jsx_opening_element(&mut node);
    }
    classnames.into_iter().collect::<HashSet<String>>()
}

fn get_cache_path(path: &Path) -> PathBuf {
    let dir = path.parent().unwrap_or(Path::new(""));
    let dir_hash = compute_file_hash(dir.to_string_lossy().as_bytes());
    PathBuf::from(format!("dx-styles-cache-{}.bin", dir_hash))
}

fn save_cache(cache: &HashMap<PathBuf, (String, Arc<ArchivedModule>)>, path: &Path) {
    let cache_to_save: HashMap<String, CacheEntry> = cache
        .iter()
        .filter(|(p, _)| p.starts_with(path.parent().unwrap_or(Path::new(""))))
        .map(|(p, (hash, module))| {
            let classnames = update_from_cached_module(Arc::clone(module));
            (
                p.to_string_lossy().into(),
                CacheEntry {
                    hash: hash.clone(),
                    classnames,
                },
            )
        })
        .collect();
    let cache_path = get_cache_path(path);
    let file = File::create(&cache_path).unwrap();
    let mut encoder = zstd::stream::write::Encoder::new(file, 3).unwrap();
    let cache_bytes = bincode::serialize(&cache_to_save).unwrap();
    let checksum = compute_cache_checksum(&cache_bytes);
    bincode::serialize_into(&mut encoder, &(checksum, &cache_bytes)).unwrap();
    encoder.finish().unwrap();
}

fn load_cache(
    cm: &SourceMap,
    cache: &mut HashMap<PathBuf, (String, Arc<ArchivedModule>)>,
    global_classnames: &mut Arc<HashSet<String>>,
    dir: &Path,
) {
    let cache_path = get_cache_path(dir);
    if let Ok(cache_file) = fs::read(&cache_path) {
        if let Ok(mut decoder) = zstd::stream::read::Decoder::new(&*cache_file) {
            if let Ok((checksum, cache_bytes)) = bincode::deserialize_from::<_, (String, Vec<u8>)>(&mut decoder) {
                if checksum == compute_cache_checksum(&cache_bytes) {
                    if let Ok(cached) = bincode::deserialize::<HashMap<String, CacheEntry>>(&cache_bytes) {
                        for (path, entry) in cached {
                            let path = PathBuf::from(path);
                            if path.exists() {
                                if let Ok(content) = futures::executor::block_on(read_file_async(&path)) {
                                    let current_hash = compute_file_hash(&content);
                                    if current_hash == entry.hash {
                                        let (module, jsx_nodes) = parse_and_minify_file(cm, &path, &content, None)
                                            .map_or_else(|_| (Module::default(), Vec::new()), |x| x);
                                        let archived = rkyv::to_bytes::<_, 1024>(&CachedModule { jsx_nodes })
                                            .unwrap()
                                            .into();
                                        cache.insert(path.clone(), (current_hash, Arc::new(archived)));
                                        Arc::make_mut(global_classnames).extend(entry.classnames);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn write_css(new_classnames: &HashSet<String>, old_classnames: &HashSet<String>) {
    let added: Vec<_> = new_classnames.difference(old_classnames).collect();
    if !added.is_empty() {
        let file = File::create("styles.css").unwrap();
        let mut mmap = MmapMut::map_anon(new_classnames.len() * 16).unwrap();
        let mut cursor = 0;
        for classname in new_classnames {
            let line = format!(".{} {{}}\n", classname);
            let bytes = line.as_bytes();
            mmap[cursor..cursor + bytes.len()].copy_from_slice(bytes);
            cursor += bytes.len();
        }
        fs::write("styles.css", &mmap[..cursor]).unwrap();
    }
}

async fn speculative_parse(
    cm: Lrc<SourceMap>,
    cache: Arc<Mutex<HashMap<PathBuf, (String, Arc<ArchivedModule>)>>>,
    global_classnames: Arc<Mutex<Arc<HashSet<String>>>>,
    change_history: Arc<Mutex<BinaryHeap<FilePriority>>>,
    node_cache: Arc<Mutex<LruCache<PathBuf, Arc<ArchivedModule>>>>,
) {
    let mut interval = Duration::from_secs(1);
    loop {
        let files: Vec<PathBuf> = {
            let history = change_history.lock().unwrap();
            let change_rate = history.iter().map(|p| p.change_count).sum::<usize>() as f64 / history.len().max(1) as f64;
            interval = Duration::from_millis((1000.0 / (1.0 + change_rate)).clamp(500.0, 5000.0) as u64);
            history
                .iter()
                .map(|p| p.path.clone())
                .chain(
                    glob("./src/**/*.tsx")
                        .expect("Failed to read glob pattern")
                        .filter_map(Result::ok),
                )
                .collect()
        };
        for path in files {
            if let Ok(content) = read_file_async(&path).await {
                let new_hash = compute_file_hash(&content);
                let mut cache = cache.lock().unwrap();
                let cached_module = cache.get(&path).map(|(_, m)| Arc::clone(m));
                if !cache.contains_key(&path) || cache.get(&path).map(|(h, _)| h != &new_hash).unwrap_or(true) {
                    let (classnames, hash, module) = process_file(&cm, &path, &content, cached_module.as_ref());
                    cache.insert(path.clone(), (hash, Arc::clone(&module)));
                    let mut node_cache = node_cache.lock().unwrap();
                    node_cache.put(path.clone(), Arc::clone(&module));
                    let mut global = global_classnames.lock().unwrap();
                    Arc::make_mut(&mut global).extend(classnames);
                    save_cache(&cache, &path);
                }
            }
        }
        tokio::time::sleep(interval).await;
    }
}

fn update_styles(
    paths: Vec<PathBuf>,
    cm: &SourceMap,
    cache: &mut HashMap<PathBuf, (String, Arc<ArchivedModule>)>,
    global_classnames: &mut Arc<HashSet<String>>,
    change_history: &Arc<Mutex<BinaryHeap<FilePriority>>>,
    node_cache: &Arc<Mutex<LruCache<PathBuf, Arc<ArchivedModule>>>>,
) {
    let start = Instant::now();
    let old_classnames = Arc::clone(global_classnames);
    for path in paths.iter() {
        if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
            if ext == "tsx" && !path.to_string_lossy().contains(".tmp") && !path.to_string_lossy().contains(".swp") {
                if let Ok(content) = futures::executor::block_on(read_file_async(path)) {
                    let new_hash = compute_file_hash(&content);
                    let (new_classnames, module) = if let Some((old_hash, old_module)) = cache.get(path) {
                        if old_hash == &new_hash {
                            (update_from_cached_module(Arc::clone(old_module)), Arc::clone(old_module))
                        } else {
                            let (new_classnames, _, new_module) = process_file(cm, path, &content, Some(old_module));
                            (new_classnames, new_module)
                        }
                    } else {
                        let (new_classnames, _, new_module) = process_file(cm, path, &content, None);
                        (new_classnames, new_module)
                    };
                    cache.insert(path.to_path_buf(), (new_hash, Arc::clone(&module)));
                    let mut node_cache = node_cache.lock().unwrap();
                    node_cache.put(path.to_path_buf(), Arc::clone(&module));
                    let mut new_global = Arc::make_mut(global_classnames);
                    new_global.extend(new_classnames);
                    save_cache(cache, path);
                    let mut history = change_history.lock().unwrap();
                    if let Some(idx) = history.iter().position(|p| p.path == *path) {
                        let mut item = history.take(idx).unwrap();
                        item.change_count += 1;
                        history.push(item);
                    } else {
                        history.push(FilePriority {
                            path: path.to_path_buf(),
                            change_count: 1,
                        });
                    }
                }
            }
        }
    }
    let added: Vec<_> = global_classnames.difference(&old_classnames).collect();
    let removed: Vec<_> = old_classnames.difference(&global_classnames).collect();
    let added_count = added.len();
    let removed_count = removed.len();
    write_css(&global_classnames, &old_classnames);
    let duration = start.elapsed();
    let time_str = if duration.as_millis() < 1 {
        format!("{}µs", duration.as_micros())
    } else {
        format!("{:.1}ms", duration.as_secs_f64() * 1000.0)
    };
    for path in paths {
        if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
            if ext == "tsx" && !path.to_string_lossy().contains(".tmp") && !path.to_string_lossy().contains(".swp") {
                println!(
                    "{} ({}, {}) -> {} ({}, {}) \u{2022} {}",
                    path.display().to_string().yellow(),
                    format!("+{}", added_count).green(),
                    format!("-{}", removed_count).red(),
                    "styles.css".cyan(),
                    format!("+{}", added_count).green(),
                    format!("-{}", removed_count).red(),
                    time_str
                );
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let core_count = num_cpus::get();
    rayon::ThreadPoolBuilder::new()
        .num_threads((core_count as f64 * 0.75).ceil() as usize)
        .build_global()
        .unwrap();
    let cm: Lrc<SourceMap> = Default::default();
    let cache: Arc<Mutex<HashMap<PathBuf, (String, Arc<ArchivedModule>)>>> = Arc::new(Mutex::new(HashMap::new()));
    let global_classnames: Arc<Mutex<Arc<HashSet<String>>>> = Arc::new(Mutex::new(Arc::new(HashSet::new())));
    let change_history: Arc<Mutex<BinaryHeap<FilePriority>>> = Arc::new(Mutex::new(BinaryHeap::new()));
    let node_cache: Arc<Mutex<LruCache<PathBuf, Arc<ArchivedModule>>>> = Arc::new(Mutex::new(LruCache::new(10000)));
    let initial_dirs: Vec<PathBuf> = glob("./src/**/")
        .expect("Failed to read glob pattern")
        .filter_map(Result::ok)
        .collect();
    initial_dirs.par_iter().for_each(|dir| {
        load_cache(&cm, &mut cache.lock().unwrap(), &mut global_classnames.lock().unwrap(), dir);
    });
    let initial_files: Vec<PathBuf> = glob("./src/**/*.tsx")
        .expect("Failed to read glob pattern")
        .filter_map(Result::ok)
        .collect();
    initial_files.par_iter().for_each(|path| {
        if let Ok(content) = futures::executor::block_on(read_file_async(path)) {
            let mut cache_lock = cache.lock().unwrap();
            if !cache_lock.contains_key(path) {
                let cached_module = None;
                let (classnames, hash, module) = process_file(&cm, path, &content, cached_module);
                cache_lock.insert(path.to_path_buf(), (hash, Arc::clone(&module)));
                let mut node_cache = node_cache.lock().unwrap();
                node_cache.put(path.to_path_buf(), Arc::clone(&module));
                let mut global = global_classnames.lock().unwrap();
                Arc::make_mut(&mut global).extend(classnames);
                save_cache(&cache_lock, path);
            }
        }
    });
    write_css(&global_classnames.lock().unwrap(), &HashSet::new());
    let poller = Poller::new().unwrap();
    let mut paths = HashMap::new();
    for path in glob("./src/**/*").expect("Failed to read glob pattern").filter_map(Result::ok) {
        if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("tsx") {
            poller.add(&path).unwrap();
            paths.insert(path.to_string_lossy().to_string(), path);
        }
    }
    let (tx, rx): (Sender<std::result::Result<Event, std::io::Error>>, Receiver<std::result::Result<Event, std::io::Error>>) = channel::unbounded();
    let speculative_task = tokio::spawn(speculative_parse(
        cm.clone(),
        Arc::clone(&cache),
        Arc::clone(&global_classnames),
        Arc::clone(&change_history),
        Arc::clone(&node_cache),
    ));
    let mut pending_paths = Vec::new();
    let mut events = Vec::new();
    loop {
        select! {
            _ = poller.wait(&mut events, Duration::from_millis(100)) => {
                for event in events.drain(..) {
                    if event.readable && paths.contains_key(&event.key) {
                        let path = paths.get(&event.key).unwrap().clone();
                        if !path.to_string_lossy().contains(".tmp") && !path.to_string_lossy().contains(".swp") {
                            pending_paths.push(path);
                        }
                    }
                }
            }
            Ok(event) = rx.recv() => {
                if let Ok(event) = event {
                    if event.readable && paths.contains_key(&event.key) {
                        let path = paths.get(&event.key).unwrap().clone();
                        if !path.to_string_lossy().contains(".tmp") && !path.to_string_lossy().contains(".swp") {
                            pending_paths.push(path);
                        }
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                if !pending_paths.is_empty() {
                    update_styles(
                        pending_paths.clone(),
                        &cm,
                        &mut cache.lock().unwrap(),
                        &mut global_classnames.lock().unwrap(),
                        &change_history,
                        &node_cache,
                    );
                    pending_paths.clear();
                }
            }
        }
        cache.lock().unwrap().retain(|path, _| path.exists());
    }
}