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
use rkyv::{Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize, AlignedVec};
use rayon::prelude::*;
use redis::{Commands, RedisResult};
use rustlearn::linear_models::sgdclassifier::Hyperparameters as SgdHyperparameters;
use rustlearn::prelude::*;
use similar::{Algorithm, TextDiff};
use slab::Slab;
use std::collections::BinaryHeap;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use swc_common::{sync::Lrc, SourceMap, Span, GLOBALS};
use swc_ecma_ast::{JSXAttrName, JSXAttrOrSpread, JSXAttrValue, JSXOpeningElement, Lit, Module, Program};
use swc_ecma_minifier::optimize;
use swc_ecma_parser::{lexer::Lexer, Parser, StringInput, Syntax, TsSyntax};
use swc_ecma_visit::{Visit, VisitMut, VisitWith};
use sysinfo::{System, SystemExt};
use tokio::fs;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[archive_attr(align(64))]
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
    jsx_nodes: &'a mut Slab<JSXOpeningElement>,
}

impl<'a> Visit for JSXOnlyCollector<'a> {
    fn visit_jsx_opening_element(&mut self, elem: &JSXOpeningElement) {
        let attrs = &elem.attrs;
        let mut i = 0;
        while i < attrs.len() {
            if let (Some(JSXAttrOrSpread::JSXAttr(attr1)), Some(JSXAttrOrSpread::JSXAttr(attr2))) = (attrs.get(i), attrs.get(i + 1)) {
                if let JSXAttrName::Ident(ident) = &attr1.name {
                    if ident.sym == "className" {
                        if let Some(JSXAttrValue::Lit(Lit::Str(s))) = &attr1.value {
                            self.classnames.extend(s.value.split_whitespace().map(String::from));
                        }
                    }
                }
                if let JSXAttrName::Ident(ident) = &attr2.name {
                    if ident.sym == "className" {
                        if let Some(JSXAttrValue::Lit(Lit::Str(s))) = &attr2.value {
                            self.classnames.extend(s.value.split_whitespace().map(String::from));
                        }
                    }
                }
                i += 2;
            } else if let Some(JSXAttrOrSpread::JSXAttr(attr)) = attrs.get(i) {
                if let JSXAttrName::Ident(ident) = &attr.name {
                    if ident.sym == "className" {
                        if let Some(JSXAttrValue::Lit(Lit::Str(s))) = &attr.value {
                            self.classnames.extend(s.value.split_whitespace().map(String::from));
                        }
                    }
                }
                i += 1;
            } else {
                i += 1;
            }
        }
        self.jsx_nodes.insert(elem.clone());
    }
}

struct JSXOnlyUpdater<'a> {
    classnames: &'a mut Vec<String>,
    jsx_nodes: &'a mut Slab<JSXOpeningElement>,
}

impl<'a> VisitMut for JSXOnlyUpdater<'a> {
    fn visit_mut_jsx_opening_element(&mut self, elem: &mut JSXOpeningElement) {
        let attrs = &mut elem.attrs;
        let mut i = 0;
        while i < attrs.len() {
            if let (Some(JSXAttrOrSpread::JSXAttr(attr1)), Some(JSXAttrOrSpread::JSXAttr(attr2))) = (attrs.get(i), attrs.get(i + 1)) {
                if let JSXAttrName::Ident(ident) = &attr1.name {
                    if ident.sym == "className" {
                        if let Some(JSXAttrValue::Lit(Lit::Str(s))) = &attr1.value {
                            self.classnames.extend(s.value.split_whitespace().map(String::from));
                        }
                    }
                }
                if let JSXAttrName::Ident(ident) = &attr2.name {
                    if ident.sym == "className" {
                        if let Some(JSXAttrValue::Lit(Lit::Str(s))) = &attr2.value {
                            self.classnames.extend(s.value.split_whitespace().map(String::from));
                        }
                    }
                }
                i += 2;
            } else if let Some(JSXAttrOrSpread::JSXAttr(attr)) = attrs.get(i) {
                if let JSXAttrName::Ident(ident) = &attr.name {
                    if ident.sym == "className" {
                        if let Some(JSXAttrValue::Lit(Lit::Str(s))) = &attr.value {
                            self.classnames.extend(s.value.split_whitespace().map(String::from));
                        }
                    }
                }
                i += 1;
            } else {
                i += 1;
            }
        }
        self.jsx_nodes.insert(elem.clone());
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

fn compute_node_hash(node: &JSXOpeningElement) -> String {
    let mut hasher = Hasher::new();
    for attr in &node.attrs {
        if let JSXAttrOrSpread::JSXAttr(attr) = attr {
            if let JSXAttrName::Ident(ident) = &attr.name {
                hasher.update(ident.sym.as_bytes());
                if let Some(JSXAttrValue::Lit(Lit::Str(s))) = &attr.value {
                    hasher.update(s.value.as_bytes());
                }
            }
        }
    }
    hasher.finalize().to_string()
}

fn parse_and_minify_file(
    cm: &SourceMap,
    path: &Path,
    content: &[u8],
    cached_nodes: Option<&Arc<AlignedVec>>,
    old_content: Option<&[u8]>,
    node_slab: &mut Slab<JSXOpeningElement>,
    node_map: &mut HashMap<String, usize>,
) -> Result<(Module, Vec<usize>), String> {
    let new_text = String::from_utf8_lossy(content).to_string();
    let mut spans_to_parse = Vec::new();
    if let Some(old_content) = old_content {
        let old_text = String::from_utf8_lossy(old_content).to_string();
        let diff = TextDiff::configure()
            .algorithm(Algorithm::Myers)
            .diff_lines(&old_text, &new_text);
        for change in diff.iter_all_changes() {
            if let Some(pos) = change.new_index() {
                if change.tag().is_insert() || change.tag().is_replace() {
                    let start_line = pos.saturating_sub(1);
                    let end_line = pos + 1;
                    spans_to_parse.push((start_line, end_line));
                }
            }
        }
        if spans_to_parse.is_empty() || diff.ratio() < 0.5 {
            spans_to_parse.clear();
        }
    }
    let fm = cm.new_source_file(
        swc_common::FileName::Custom(path.to_string_lossy().into()),
        new_text.clone(),
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
            jsx_nodes: node_slab,
        };
        for node in &archived.jsx_nodes {
            let mut node = node.clone();
            updater.visit_mut_jsx_opening_element(&mut node);
        }
        module
    } else {
        parser.parse_module().map_err(|e| format!("{:?}", e))?
        // Placeholder for future SWC GPU parsing support
        // e.g., offload to CUDA/OpenCL if available
    };
    let mut jsx_nodes = Vec::new();
    if !spans_to_parse.is_empty() {
        for (start_line, end_line) in spans_to_parse {
            let start_pos = fm.line_to_pos(start_line).unwrap_or_default();
            let end_pos = fm.line_to_pos(end_line).unwrap_or(fm.end_pos);
            let span = Span::new(start_pos, end_pos, Default::default());
            let sub_module = parser
                .parse_module()
                .map_err(|e| format!("{:?}", e))?;
            let mut collector = JSXOnlyCollector {
                classnames: &mut Vec::new(),
                jsx_nodes: node_slab,
            };
            sub_module.visit_with(&mut collector);
            for node in node_slab.iter().skip(module.body.len()) {
                let node_hash = compute_node_hash(&node.1);
                if !node_map.contains_key(&node_hash) {
                    node_map.insert(node_hash, node.0);
                    jsx_nodes.push(node.0);
                } else {
                    jsx_nodes.push(*node_map.get(&node_hash).unwrap());
                }
            }
            module.body.extend(sub_module.body);
        }
    } else {
        let mut collector = JSXOnlyCollector {
            classnames: &mut Vec::new(),
            jsx_nodes: node_slab,
        };
        module.visit_with(&mut collector);
        for node in node_slab.iter() {
            let node_hash = compute_node_hash(&node.1);
            if !node_map.contains_key(&node_hash) {
                node_map.insert(node_hash, node.0);
                jsx_nodes.push(node.0);
            } else {
                jsx_nodes.push(*node_map.get(&node_hash).unwrap());
            }
        }
    }
    let mut pruner = JSXPruner;
    module.visit_mut_with(&mut pruner);
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
fn process_file(
    cm: &SourceMap,
    path: &Path,
    content: &[u8],
    cached_nodes: Option<&Arc<AlignedVec>>,
    old_content: Option<&[u8]>,
    node_slab: &mut Slab<JSXOpeningElement>,
    node_map: &mut HashMap<String, usize>,
) -> (HashSet<String>, String, Arc<AlignedVec>) {
    let mut classnames = Vec::with_capacity(50);
    let hash = compute_file_hash(content);
    let (module, jsx_node_ids) = parse_and_minify_file(cm, path, content, cached_nodes, old_content, node_slab, node_map)
        .map_or_else(|_| (Module::default(), Vec::new()), |(m, nodes)| (m, nodes));
    let mut collector = JSXOnlyCollector {
        classnames: &mut classnames,
        jsx_nodes: &mut Slab::new(),
    };
    module.visit_with(&mut collector);
    let mut aligned = AlignedVec::with_capacity(1024);
    let mut encoder = zstd::stream::write::Encoder::new(&mut aligned, 3).unwrap();
    let jsx_nodes = jsx_node_ids.iter().map(|id| node_slab.get(*id).unwrap().clone()).collect();
    rkyv::to_bytes::<_, 1024>(&CachedModule { jsx_nodes }).unwrap().write_to(&mut encoder).unwrap();
    encoder.finish().unwrap();
    (
        classnames.into_iter().collect::<HashSet<String>>(),
        hash,
        Arc::new(aligned),
    )
}

#[inline]
fn update_from_cached_module(module: Arc<AlignedVec>, node_slab: &mut Slab<JSXOpeningElement>, node_map: &mut HashMap<String, usize>) -> HashSet<String> {
    let mut classnames = Vec::with_capacity(50);
    let mut decoder = zstd::stream::read::Decoder::new(&module[..]).unwrap();
    let aligned = unsafe { rkyv::archived_root::<CachedModule>(decoder.read_to_end().unwrap().as_slice()) };
    let mut updater = JSXOnlyUpdater {
        classnames: &mut classnames,
        jsx_nodes: node_slab,
    };
    for node in &aligned.jsx_nodes {
        let node_hash = compute_node_hash(node);
        let node_id = if let Some(id) = node_map.get(&node_hash) {
            *id
        } else {
            let id = node_slab.insert(node.clone());
            node_map.insert(node_hash, id);
            id
        };
        let mut node = node_slab.get(node_id).unwrap().clone();
        updater.visit_mut_jsx_opening_element(&mut node);
    }
    classnames.into_iter().collect::<HashSet<String>>()
}

fn get_cache_path(path: &Path) -> PathBuf {
    let dir = path.parent().unwrap_or(Path::new(""));
    let dir_hash = compute_file_hash(dir.to_string_lossy().as_bytes());
    PathBuf::from(format!("dx-styles-cache-{}.bin", dir_hash))
}

fn save_cache(cache: &HashMap<PathBuf, (String, Arc<AlignedVec>)>, paths: &[PathBuf]) {
    let mut by_dir: HashMap<PathBuf, HashMap<String, CacheEntry>> = HashMap::new();
    for path in paths {
        let dir = path.parent().unwrap_or(Path::new(""));
        let dir_entry = by_dir.entry(dir.to_path_buf()).or_insert_with(HashMap::new);
        let (hash, module) = cache.get(path).unwrap();
        let classnames = update_from_cached_module(Arc::clone(module), &mut Slab::new(), &mut HashMap::new());
        dir_entry.insert(
            path.to_string_lossy().into(),
            CacheEntry {
                hash: hash.clone(),
                classnames,
            },
        );
    }
    by_dir.par_iter().for_each(|(dir, cache_to_save)| {
        let cache_path = get_cache_path(dir);
        let file = File::create(&cache_path).unwrap();
        let mut encoder = zstd::stream::write::Encoder::new(file, 3).unwrap();
        let cache_bytes = bincode::serialize(&cache_to_save).unwrap();
        let checksum = compute_cache_checksum(&cache_bytes);
        bincode::serialize_into(&mut encoder, &(checksum, &cache_bytes)).unwrap();
        encoder.finish().unwrap();
    });
}

fn load_cache(
    cm: &SourceMap,
    cache: &mut HashMap<PathBuf, (String, Arc<AlignedVec>)>,
    global_classnames: &mut Arc<HashSet<String>>,
    dir: &Path,
    node_slab: &mut Slab<JSXOpeningElement>,
    node_map: &mut HashMap<String, usize>,
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
                                        let (module, jsx_node_ids) = parse_and_minify_file(cm, &path, &content, None, None, node_slab, node_map)
                                            .map_or_else(|_| (Module::default(), Vec::new()), |x| x);
                                        let mut aligned = AlignedVec::with_capacity(1024);
                                        let mut encoder = zstd::stream::write::Encoder::new(&mut aligned, 3).unwrap();
                                        let jsx_nodes = jsx_node_ids.iter().map(|id| node_slab.get(*id).unwrap().clone()).collect();
                                        rkyv::to_bytes::<_, 1024>(&CachedModule { jsx_nodes }).unwrap().write_to(&mut encoder).unwrap();
                                        encoder.finish().unwrap();
                                        cache.insert(path.clone(), (current_hash, Arc::new(aligned)));
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

fn prefetch_caches(
    cm: &SourceMap,
    cache: &Arc<Mutex<HashMap<PathBuf, (String, Arc<AlignedVec>)>>>,
    global_classnames: &Arc<Mutex<Arc<HashSet<String>>>>,
    change_history: &Arc<Mutex<BinaryHeap<FilePriority>>>,
    node_slab: &Arc<Mutex<Slab<JSXOpeningElement>>>,
    node_map: &Arc<Mutex<HashMap<String, usize>>>,
) -> Vec<PathBuf> {
    let history = change_history.lock().unwrap();
    let features: Vec<_> = history
        .iter()
        .map(|p| vec![p.change_count as f32, p.path.to_string_lossy().len() as f32])
        .collect();
    let labels: Vec<f32> = history.iter().map(|p| (p.change_count > 5) as i32 as f32).collect();
    let mut model = SgdHyperparameters::new(2)
        .learning_rate(0.01)
        .l2_penalty(0.1)
        .build()
        .unwrap();
    if !features.is_empty() {
        let feature_matrix = Array::from(&features);
        model.fit(&feature_matrix, &Array::from(&labels)).unwrap();
    }
    let top_dirs: Vec<PathBuf> = history
        .iter()
        .filter(|p| {
            let feature = vec![p.change_count as f32, p.path.to_string_lossy().len() as f32];
            let prediction = model.predict(&Array::from(&[feature])).unwrap();
            prediction[0] > 0.5
        })
        .take(10)
        .map(|p| p.path.parent().unwrap_or(Path::new("")).to_path_buf())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    top_dirs.par_iter().for_each(|dir| {
        load_cache(
            cm,
            &mut cache.lock().unwrap(),
            &mut global_classnames.lock().unwrap(),
            dir,
            &mut node_slab.lock().unwrap(),
            &mut node_map.lock().unwrap(),
        );
    });
    top_dirs
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
    cache: Arc<Mutex<HashMap<PathBuf, (String, Arc<AlignedVec>)>>>,
    global_classnames: Arc<Mutex<Arc<HashSet<String>>>>,
    change_history: Arc<Mutex<BinaryHeap<FilePriority>>>,
    node_cache: Arc<Mutex<LruCache<PathBuf, Arc<AlignedVec>>>>,
    node_slab: Arc<Mutex<Slab<JSXOpeningElement>>>,
    node_map: Arc<Mutex<HashMap<String, usize>>>,
) {
    let mut interval = Duration::from_secs(1);
    let mut iteration = 0;
    let mut system = System::new_all();
    loop {
        if iteration % 10 == 0 {
            prefetch_caches(&cm, &cache, &global_classnames, &change_history, &node_slab, &node_map);
        }
        let files: Vec<PathBuf> = {
            let history = change_history.lock().unwrap();
            let change_rate = history.iter().map(|p| p.change_count).sum::<usize>() as f64 / history.len().max(1) as f64;
            system.refresh_cpu();
            let cpu_usage = system.global_cpu_info().cpu_usage();
            interval = Duration::from_millis(
                (1000.0 / (1.0 + change_rate) * (1.0 + cpu_usage / 100.0)).clamp(50.0, 500.0) as u64,
            );
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
        let paths_to_save = files.clone();
        for path in files {
            if let Ok(content) = read_file_async(&path).await {
                let new_hash = compute_file_hash(&content);
                let mut cache = cache.lock().unwrap();
                let cached_module = cache.get(&path).map(|(_, m)| Arc::clone(m));
                let old_content = cache.get(&path).and_then(|(h, _)| {
                    if h == &new_hash {
                        Some(futures::executor::block_on(read_file_async(&path)).unwrap_or_default())
                    } else {
                        None
                    }
                });
                if !cache.contains_key(&path) || cache.get(&path).map(|(h, _)| h != &new_hash).unwrap_or(true) {
                    let (classnames, hash, module) = process_file(
                        &cm,
                        &path,
                        &content,
                        cached_module.as_ref(),
                        old_content.as_deref(),
                        &mut node_slab.lock().unwrap(),
                        &mut node_map.lock().unwrap(),
                    );
                    cache.insert(path.clone(), (hash, Arc::clone(&module)));
                    let mut node_cache = node_cache.lock().unwrap();
                    node_cache.put(path.clone(), Arc::clone(&module));
                    let mut global = global_classnames.lock().unwrap();
                    Arc::make_mut(&mut global).extend(classnames);
                }
            }
        }
        save_cache(&cache.lock().unwrap(), &paths_to_save);
        iteration += 1;
        tokio::time::sleep(interval).await;
    }
}

fn process_redis_tasks(
    cm: &SourceMap,
    cache: &Arc<Mutex<HashMap<PathBuf, (String, Arc<AlignedVec>)>>>,
    global_classnames: &Arc<Mutex<Arc<HashSet<String>>>>,
    change_history: &Arc<Mutex<BinaryHeap<FilePriority>>>,
    node_cache: &Arc<Mutex<LruCache<PathBuf, Arc<AlignedVec>>>>,
    node_slab: &Arc<Mutex<Slab<JSXOpeningElement>>>,
    node_map: &Arc<Mutex<HashMap<String, usize>>>,
) {
    let client = redis::Client::open("redis://127.0.0.1/").unwrap();
    let mut con = client.get_connection().unwrap();
    let mut pubsub = client.get_connection().unwrap().as_pubsub();
    pubsub.subscribe("dx-styles:cache").unwrap();
    loop {
        if let Ok(message) = pubsub.get_message() {
            if let Ok(cache_update) = message.get_payload::<String>() {
                if let Ok(cached) = bincode::deserialize::<HashMap<String, CacheEntry>>(&cache_update.as_bytes()) {
                    let mut cache = cache.lock().unwrap();
                    let mut global = global_classnames.lock().unwrap();
                    for (path, entry) in cached {
                        let path = PathBuf::from(path);
                        if path.exists() {
                            let (module, jsx_node_ids) = parse_and_minify_file(
                                cm,
                                &path,
                                &futures::executor::block_on(read_file_async(&path)).unwrap_or_default(),
                                None,
                                None,
                                &mut node_slab.lock().unwrap(),
                                &mut node_map.lock().unwrap(),
                            )
                            .map_or_else(|_| (Module::default(), Vec::new()), |x| x);
                            let mut aligned = AlignedVec::with_capacity(1024);
                            let mut encoder = zstd::stream::write::Encoder::new(&mut aligned, 3).unwrap();
                            let jsx_nodes = jsx_node_ids.iter().map(|id| node_slab.lock().unwrap().get(*id).unwrap().clone()).collect();
                            rkyv::to_bytes::<_, 1024>(&CachedModule { jsx_nodes }).unwrap().write_to(&mut encoder).unwrap();
                            encoder.finish().unwrap();
                            cache.insert(path.clone(), (entry.hash, Arc::new(aligned)));
                            Arc::make_mut(&mut global).extend(entry.classnames);
                        }
                    }
                }
            }
        }
        let path_str: Option<String> = {
            let dir_queues: Vec<String> = change_history
                .lock()
                .unwrap()
                .iter()
                .map(|p| {
                    let dir = p.path.parent().unwrap_or(Path::new(""));
                    format!("dx-styles:queue:{}", compute_file_hash(dir.to_string_lossy().as_bytes()))
                })
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            let mut result = None;
            for queue in dir_queues {
                if let Ok(path) = con.blpop::<_, String>(&queue, 1.0) {
                    result = Some(path);
                    break;
                }
            }
            result
        };
        if let Some(path_str) = path_str {
            let path = PathBuf::from(path_str);
            if let Ok(content) = futures::executor::block_on(read_file_async(&path)) {
                let new_hash = compute_file_hash(&content);
                let mut cache = cache.lock().unwrap();
                let cached_module = cache.get(&path).map(|(_, m)| Arc::clone(m));
                let old_content = cache.get(&path).and_then(|(h, _)| {
                    if h == &new_hash {
                        Some(futures::executor::block_on(read_file_async(&path)).unwrap_or_default())
                    } else {
                        None
                    }
                });
                if !cache.contains_key(&path) || cache.get(&path).map(|(h, _)| h != &new_hash).unwrap_or(true) {
                    let (classnames, hash, module) = process_file(
                        cm,
                        &path,
                        &content,
                        cached_module.as_ref(),
                        old_content.as_deref(),
                        &mut node_slab.lock().unwrap(),
                        &mut node_map.lock().unwrap(),
                    );
                    cache.insert(path.clone(), (hash.clone(), Arc::clone(&module)));
                    let mut node_cache = node_cache.lock().unwrap();
                    node_cache.put(path.clone(), Arc::clone(&module));
                    let mut global = global_classnames.lock().unwrap();
                    let old_classnames = Arc::clone(&global);
                    Arc::make_mut(&mut global).extend(classnames.clone());
                    write_css(&global, &old_classnames);
                    save_cache(&cache, &[path.clone()]);
                    let cache_update = bincode::serialize(&HashMap::from([(
                        path.to_string_lossy().to_string(),
                        CacheEntry { hash, classnames },
                    )]))
                    .unwrap();
                    con.publish("dx-styles:cache", cache_update).unwrap();
                    let mut history = change_history.lock().unwrap();
                    if let Some(idx) = history.iter().position(|p| p.path == path) {
                        let mut item = history.take(idx).unwrap();
                        item.change_count += 1;
                        history.push(item);
                    } else {
                        history.push(FilePriority {
                            path,
                            change_count: 1,
                        });
                    }
                }
            }
        }
    }
}

fn update_styles(
    paths: Vec<PathBuf>,
    cm: &SourceMap,
    cache: &mut HashMap<PathBuf, (String, Arc<AlignedVec>)>,
    global_classnames: &mut Arc<HashSet<String>>,
    change_history: &Arc<Mutex<BinaryHeap<FilePriority>>>,
    node_cache: &Arc<Mutex<LruCache<PathBuf, Arc<AlignedVec>>>>,
    node_slab: &Arc<Mutex<Slab<JSXOpeningElement>>>,
    node_map: &Arc<Mutex<HashMap<String, usize>>>,
) {
    let start = Instant::now();
    let old_classnames = Arc::clone(global_classnames);
    let client = redis::Client::open("redis://127.0.0.1/").unwrap();
    let mut con = client.get_connection().unwrap();
    for path in paths.iter() {
        if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
            if ext == "tsx" && !path.to_string_lossy().contains(".tmp") && !path.to_string_lossy().contains(".swp") {
                if let Ok(content) = futures::executor::block_on(read_file_async(path)) {
                    let new_hash = compute_file_hash(&content);
                    let old_content = cache.get(path).and_then(|(h, _)| {
                        if h == &new_hash {
                            Some(futures::executor::block_on(read_file_async(path)).unwrap_or_default())
                        } else {
                            None
                        }
                    });
                    let (new_classnames, module) = if let Some((old_hash, old_module)) = cache.get(path) {
                        if old_hash == &new_hash {
                            (
                                update_from_cached_module(Arc::clone(old_module), &mut node_slab.lock().unwrap(), &mut node_map.lock().unwrap()),
                                Arc::clone(old_module),
                            )
                        } else {
                            let (new_classnames, _, new_module) = process_file(
                                cm,
                                path,
                                &content,
                                Some(old_module),
                                old_content.as_deref(),
                                &mut node_slab.lock().unwrap(),
                                &mut node_map.lock().unwrap(),
                            );
                            (new_classnames, new_module)
                        }
                    } else {
                        let (new_classnames, _, new_module) = process_file(
                            cm,
                            path,
                            &content,
                            None,
                            None,
                            &mut node_slab.lock().unwrap(),
                            &mut node_map.lock().unwrap(),
                        );
                        (new_classnames, new_module)
                    };
                    cache.insert(path.to_path_buf(), (new_hash.clone(), Arc::clone(&module)));
                    let mut node_cache = node_cache.lock().unwrap();
                    node_cache.put(path.to_path_buf(), Arc::clone(&module));
                    let mut new_global = Arc::make_mut(global_classnames);
                    new_global.extend(new_classnames.clone());
                    let cache_update = bincode::serialize(&HashMap::from([(
                        path.to_string_lossy().to_string(),
                        CacheEntry {
                            hash: new_hash,
                            classnames: new_classnames,
                        },
                    )]))
                    .unwrap();
                    con.publish("dx-styles:cache", cache_update).unwrap();
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
    save_cache(cache, &paths);
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
    let cache: Arc<Mutex<HashMap<PathBuf, (String, Arc<AlignedVec>)>>> = Arc::new(Mutex::new(HashMap::new()));
    let global_classnames: Arc<Mutex<Arc<HashSet<String>>>> = Arc::new(Mutex::new(Arc::new(HashSet::new())));
    let change_history: Arc<Mutex<BinaryHeap<FilePriority>>> = Arc::new(Mutex::new(BinaryHeap::new()));
    let node_cache: Arc<Mutex<LruCache<PathBuf, Arc<AlignedVec>>>> = Arc::new(Mutex::new(LruCache::new(10000)));
    let node_slab: Arc<Mutex<Slab<JSXOpeningElement>>> = Arc::new(Mutex::new(Slab::new()));
    let node_map: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));
    let initial_dirs: Vec<PathBuf> = glob("./src/**/")
        .expect("Failed to read glob pattern")
        .filter_map(Result::ok)
        .collect();
    initial_dirs.par_iter().for_each(|dir| {
        load_cache(
            &cm,
            &mut cache.lock().unwrap(),
            &mut global_classnames.lock().unwrap(),
            dir,
            &mut node_slab.lock().unwrap(),
            &mut node_map.lock().unwrap(),
        );
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
                let (classnames, hash, module) = process_file(
                    &cm,
                    path,
                    &content,
                    cached_module,
                    None,
                    &mut node_slab.lock().unwrap(),
                    &mut node_map.lock().unwrap(),
                );
                cache_lock.insert(path.to_path_buf(), (hash, Arc::clone(&module)));
                let mut node_cache = node_cache.lock().unwrap();
                node_cache.put(path.to_path_buf(), Arc::clone(&module));
                let mut global = global_classnames.lock().unwrap();
                Arc::make_mut(&mut global).extend(classnames);
            }
        }
    });
    save_cache(&cache.lock().unwrap(), &initial_files);
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
    let redis_client = redis::Client::open("redis://127.0.0.1/").unwrap();
    let redis_task = tokio::spawn({
        let cm = cm.clone();
        let cache = Arc::clone(&cache);
        let global_classnames = Arc::clone(&global_classnames);
        let change_history = Arc::clone(&change_history);
        let node_cache = Arc::clone(&node_cache);
        let node_slab = Arc::clone(&node_slab);
        let node_map = Arc::clone(&node_map);
        async move {
            process_redis_tasks(&cm, &cache, &global_classnames, &change_history, &node_cache, &node_slab, &node_map);
        }
    });
    let speculative_task = tokio::spawn(speculative_parse(
        cm.clone(),
        Arc::clone(&cache),
        Arc::clone(&global_classnames),
        Arc::clone(&change_history),
        Arc::clone(&node_cache),
        Arc::clone(&node_slab),
        Arc::clone(&node_map),
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
                            pending_paths.push(path.clone());
                            let dir = path.parent().unwrap_or(Path::new(""));
                            let queue = format!("dx-styles:queue:{}", compute_file_hash(dir.to_string_lossy().as_bytes()));
                            let mut con = redis_client.get_connection().unwrap();
                            con.rpush(&queue, path.to_string_lossy().to_string()).unwrap();
                        }
                    }
                }
            }
            Ok(event) = rx.recv() => {
                if let Ok(event) = event {
                    if event.readable && paths.contains_key(&event.key) {
                        let path = paths.get(&event.key).unwrap().clone();
                        if !path.to_string_lossy().contains(".tmp") && !path.to_string_lossy().contains(".swp") {
                            pending_paths.push(path.clone());
                            let dir = path.parent().unwrap_or(Path::new(""));
                            let queue = format!("dx-styles:queue:{}", compute_file_hash(dir.to_string_lossy().as_bytes()));
                            let mut con = redis_client.get_connection().unwrap();
                            con.rpush(&queue, path.to_string_lossy().to_string()).unwrap();
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
                        &node_slab,
                        &node_map,
                    );
                    pending_paths.clear();
                }
            }
        }
        cache.lock().unwrap().retain(|path, _| path.exists());
    }
}