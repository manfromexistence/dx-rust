use colored::*;
use glob::glob;
use memmap2::Mmap;
use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use rayon::prelude::*;
use regex::Regex;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use swc_common::{Span, SourceMap, FileName};
use swc_ecma_ast::{
    IdentName, JSXAttr, JSXAttrName, JSXAttrOrSpread, JSXAttrValue, JSXOpeningElement, Lit, Str, Module,
};
use swc_ecma_codegen::{text_writer::JsWriter, Emitter};
use swc_ecma_parser::{lexer::Lexer, Parser, StringInput, Syntax, TsSyntax};
use swc_ecma_visit::{Visit, VisitMut, VisitMutWith, VisitWith};

fn is_managed_id(id: &str) -> bool {
    let base_id: String = id.chars().filter(|c| c.is_alphabetic()).collect();
    if base_id.is_empty() {
        return false;
    }
    if base_id.chars().any(|c| !c.is_lowercase()) {
        return false;
    }
    let mut chars: Vec<char> = base_id.chars().collect();
    let original_len = chars.len();
    chars.sort_unstable();
    chars.dedup();
    let sorted_unique_base_id: String = chars.into_iter().collect();
    
    base_id == sorted_unique_base_id && base_id.len() == original_len
}

#[derive(Debug, Clone)]
struct ElementInfo {
    span: Span,
    class_names: Vec<String>,
    current_id: Option<String>,
}

struct InfoCollector {
    elements: Vec<ElementInfo>,
}

impl Visit for InfoCollector {
    fn visit_jsx_opening_element(&mut self, elem: &JSXOpeningElement) {
        let mut class_names = Vec::new();
        let mut current_id = None;

        for attr in &elem.attrs {
            if let JSXAttrOrSpread::JSXAttr(attr) = attr {
                if let JSXAttrName::Ident(ident) = &attr.name {
                    match ident.sym.as_ref() {
                        "className" => {
                            if let Some(JSXAttrValue::Lit(Lit::Str(s))) = &attr.value {
                                if !s.value.is_empty() {
                                    class_names = s.value.split_whitespace().map(String::from).collect();
                                }
                            }
                        }
                        "id" => {
                            if let Some(JSXAttrValue::Lit(Lit::Str(s))) = &attr.value {
                                if !s.value.is_empty() {
                                    current_id = Some(s.value.to_string());
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        if !class_names.is_empty() || current_id.is_some() {
            self.elements.push(ElementInfo {
                span: elem.span,
                class_names,
                current_id,
            });
        }
        
        elem.visit_children_with(self);
    }
}

struct IdApplier<'a> {
    id_map: &'a HashMap<Span, String>,
}

impl<'a> VisitMut for IdApplier<'a> {
    fn visit_mut_jsx_opening_element(&mut self, elem: &mut JSXOpeningElement) {
        if let Some(new_id) = self.id_map.get(&elem.span) {
            let mut has_id_attr = false;
            for attr in &mut elem.attrs {
                if let JSXAttrOrSpread::JSXAttr(jsx_attr) = attr {
                    if let JSXAttrName::Ident(ident) = &jsx_attr.name {
                        if ident.sym == "id" {
                            jsx_attr.value = Some(JSXAttrValue::Lit(Lit::Str(Str {
                                value: new_id.clone().into(),
                                span: Default::default(),
                                raw: None,
                            })));
                            has_id_attr = true;
                            break;
                        }
                    }
                }
            }

            if !has_id_attr {
                elem.attrs.push(JSXAttrOrSpread::JSXAttr(JSXAttr {
                    name: JSXAttrName::Ident(IdentName::new("id".into(), Default::default())),
                    value: Some(JSXAttrValue::Lit(Lit::Str(Str {
                        value: new_id.clone().into(),
                        span: Default::default(),
                        raw: None,
                    }))),
                    span: Default::default(),
                }));
            }
        }
        elem.visit_mut_children_with(self);
    }
}

fn determine_css_entities_and_updates(module: &Module) -> (HashSet<String>, HashSet<String>, HashMap<Span, String>) {
    let mut info_collector = InfoCollector { elements: Vec::new() };
    info_collector.visit_module(&module);

    let mut final_ids = HashSet::new();
    let mut final_classnames = HashSet::new();
    let mut id_updates = HashMap::new();
    let mut base_id_counts = HashMap::new();
    let mut elements_by_base_id: BTreeMap<String, Vec<ElementInfo>> = BTreeMap::new();
    let group_class_name = "group".to_string();

    for el in &info_collector.elements {
        for cn in &el.class_names {
            final_classnames.insert(cn.clone());
        }

        if !el.class_names.contains(&group_class_name) {
            if let Some(id) = &el.current_id {
                final_ids.insert(id.clone());
            }
            continue;
        }

        let mut id_chars: Vec<char> = el.class_names.iter().filter(|&cn| *cn != group_class_name).filter_map(|s| s.chars().next()).collect();
        id_chars.sort_unstable();
        id_chars.dedup();
        let expected_id: String = id_chars.into_iter().collect();

        let should_manage_id = match &el.current_id {
            Some(id) => is_managed_id(id),
            None => true,
        };

        if should_manage_id {
            elements_by_base_id.entry(expected_id.clone()).or_insert_with(Vec::new).push(el.clone());
            *base_id_counts.entry(expected_id).or_insert(0) += 1;
        } else if let Some(id) = &el.current_id {
            final_ids.insert(id.clone());
        }
    }

    for (base_id, elements) in elements_by_base_id {
        let count = base_id_counts.get(&base_id).cloned().unwrap_or(0);
        if count > 1 {
            for (i, el) in elements.iter().enumerate() {
                let unique_id = format!("{}{}", base_id, i + 1);
                if el.current_id.as_deref() != Some(&unique_id) {
                    id_updates.insert(el.span, unique_id.clone());
                }
                final_ids.insert(unique_id);
            }
        } else if let Some(el) = elements.first() {
            if el.current_id.as_deref() != Some(&base_id) {
                 id_updates.insert(el.span, base_id.clone());
            }
            final_ids.insert(base_id);
        }
    }
    
    (final_classnames, final_ids, id_updates)
}

fn parse_and_modify_file(
    path: &Path,
    cm: &Arc<SourceMap>,
) -> Option<(HashSet<String>, HashSet<String>, String, String)> {
    let file = File::open(path).ok()?;
    let mmap = unsafe { Mmap::map(&file).ok()? };
    let source = String::from_utf8_lossy(&mmap).to_string();
    let fm = cm.new_source_file(
        Arc::new(FileName::Real(path.to_path_buf())),
        source.clone(),
    );
    let lexer = Lexer::new(
        Syntax::Typescript(TsSyntax { tsx: true, ..Default::default() }),
        Default::default(),
        StringInput::from(&*fm),
        None,
    );
    let mut parser = Parser::new_from(lexer);
    let mut module = match parser.parse_module() {
        Ok(module) => module,
        Err(_) => return None,
    };

    let (final_classnames, final_ids, id_updates) = determine_css_entities_and_updates(&module);

    if !id_updates.is_empty() {
        let mut applier = IdApplier { id_map: &id_updates };
        module.visit_mut_with(&mut applier);
    }

    let mut output = Vec::new();
    let mut emitter = Emitter {
        cfg: Default::default(),
        cm: cm.clone(),
        comments: None,
        wr: JsWriter::new(cm.clone(), "\n", &mut output, None),
    };
    emitter.emit_module(&module).ok()?;
    let modified_code = String::from_utf8(output).ok()?;

    Some((final_classnames, final_ids, modified_code, source))
}

fn collect_css_entities(
    path: &Path,
    cm: &Arc<SourceMap>,
) -> Option<(HashSet<String>, HashSet<String>)> {
    let file = File::open(path).ok()?;
    let mmap = unsafe { Mmap::map(&file).ok()? };
    let source = String::from_utf8_lossy(&mmap);
    let fm = cm.new_source_file(
        Arc::new(FileName::Real(path.to_path_buf())),
        source.into_owned(),
    );
    let lexer = Lexer::new(
        Syntax::Typescript(TsSyntax { tsx: true, ..Default::default() }),
        Default::default(),
        StringInput::from(&*fm),
        None,
    );
    let mut parser = Parser::new_from(lexer);
    let module = match parser.parse_module() {
        Ok(module) => module,
        Err(_) => return None,
    };

    let (classnames, ids, _) = determine_css_entities_and_updates(&module);
    Some((classnames, ids))
}


fn write_file(path: &Path, content: &str) {
    let file = File::create(path).expect("Could not create file");
    let mut writer = BufWriter::new(file);
    writer
        .write_all(content.as_bytes())
        .expect("Failed to write to file");
}

fn calculate_global_classnames_and_ids(
    file_map: &HashMap<PathBuf, (HashSet<String>, HashSet<String>)>,
) -> (HashSet<String>, HashSet<String>) {
    let classnames = file_map
        .par_iter()
        .flat_map(|(_, (classes, _))| classes.clone())
        .collect();
    let ids = file_map
        .par_iter()
        .flat_map(|(_, (_, ids))| ids.clone())
        .collect();
    (classnames, ids)
}

fn read_existing_css(path: &Path) -> (HashSet<String>, HashSet<String>) {
    let mut classes = HashSet::new();
    let mut ids = HashSet::new();

    if !path.exists() {
        return (classes, ids);
    }

    let file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return (classes, ids),
    };

    let re = match Regex::new(r"^\s*[.#]([\w-]+)") {
        Ok(re) => re,
        Err(_) => return (classes, ids),
    };

    for line in BufReader::new(file).lines() {
        if let Ok(line_content) = line {
            if let Some(caps) = re.captures(&line_content) {
                if let Some(name_match) = caps.get(1) {
                    let name = name_match.as_str().to_string();
                    if line_content.trim().starts_with('.') {
                        classes.insert(name);
                    } else if line_content.trim().starts_with('#') {
                        ids.insert(name);
                    }
                }
            }
        }
    }

    (classes, ids)
}

fn write_css(classnames: &HashSet<String>, ids: &HashSet<String>, output_path: &Path) {
    let file = File::create(output_path).expect("Could not create styles.css for writing");
    let mut writer = BufWriter::new(file);

    let mut sorted_classnames: Vec<_> = classnames.iter().collect();
    sorted_classnames.sort();
    for classname in sorted_classnames {
        writeln!(writer, ".{} {{}}", classname).expect("Failed to write to styles.css");
    }

    let mut sorted_ids: Vec<_> = ids.iter().collect();
    sorted_ids.sort();
    for id in sorted_ids {
        writeln!(writer, "#{} {{}}", id).expect("Failed to write to styles.css");
    }
}

fn format_duration(duration: Duration) -> String {
    let micros = duration.as_micros();
    if micros < 1000 {
        format!("{}µs", micros)
    } else {
        format!("{:.2}ms", micros as f64 / 1000.0)
    }
}

fn initial_scan() -> (
    HashMap<PathBuf, (HashSet<String>, HashSet<String>)>,
    HashSet<String>,
    HashSet<String>,
) {
    println!(
        "{}",
        "🚀 dx-styles starting initial scan...".bold().bright_purple()
    );
    let start = Instant::now();
    let cm: Arc<SourceMap> = Default::default();
    let output_path = PathBuf::from("./styles.css");

    let (existing_classnames, existing_ids) = read_existing_css(&output_path);

    let current_dir = env::current_dir().expect("Failed to get current directory");
    let paths: Vec<_> = glob("./src/**/*.tsx")
        .expect("Failed to read glob pattern")
        .filter_map(Result::ok)
        .map(|path| path.canonicalize().unwrap_or_else(|_| current_dir.join(path)))
        .collect();

    let check_results: Vec<_> = paths
        .par_iter()
        .filter_map(|path| collect_css_entities(path, &cm))
        .collect();

    let mut expected_classnames = HashSet::new();
    let mut expected_ids = HashSet::new();
    for (classes, ids) in &check_results {
        expected_classnames.extend(classes.clone());
        expected_ids.extend(ids.clone());
    }

    if expected_classnames == existing_classnames && expected_ids == existing_ids {
        println!(
            "{} CSS is up-to-date. Skipping file modifications. \u{2022} {}",
            "✓".bright_green(),
            format_duration(start.elapsed()).bright_cyan()
        );
        let file_map: HashMap<_, _> = paths
            .par_iter()
            .filter_map(|path| {
                collect_css_entities(path, &cm).map(|(classes, ids)| (path.clone(), (classes, ids)))
            })
            .collect();
        return (file_map, existing_classnames, existing_ids);
    }

    println!("{}", "Changes detected, performing full scan and modification...".yellow());
    let file_map: HashMap<PathBuf, (HashSet<String>, HashSet<String>)> = paths
        .par_iter()
        .filter_map(|path| {
            if let Some((classnames, ids, modified_code, original_code)) =
                parse_and_modify_file(path, &cm)
            {
                if original_code != modified_code {
                    write_file(path, &modified_code);
                }
                Some((path.clone(), (classnames, ids)))
            } else {
                None
            }
        })
        .collect();

    let (global_classnames, global_ids) = calculate_global_classnames_and_ids(&file_map);
    write_css(&global_classnames, &global_ids, &output_path);

    let duration = start.elapsed();
    println!(
        "{} Initial scan found {} classes and {} IDs in {} files \u{2022} {}",
        "✓".bright_green(),
        global_classnames.len().to_string().bright_green(),
        global_ids.len().to_string().bright_green(),
        paths.len().to_string().bright_yellow(),
        format_duration(duration).bright_cyan()
    );
    (file_map, global_classnames, global_ids)
}

fn process_change(
    path: &Path,
    file_map: &mut HashMap<PathBuf, (HashSet<String>, HashSet<String>)>,
    old_global_classnames: &HashSet<String>,
    old_global_ids: &HashSet<String>,
) -> Option<(HashSet<String>, HashSet<String>)> {
    let start = Instant::now();
    let cm: Arc<SourceMap> = Default::default();

    let (old_file_classnames, old_file_ids) = file_map.get(path).cloned().unwrap_or_default();

    if !path.exists() {
        file_map.remove(path);
        let (new_global_classnames, new_global_ids) = calculate_global_classnames_and_ids(file_map);
        if &new_global_classnames != old_global_classnames || &new_global_ids != old_global_ids {
             write_css(&new_global_classnames, &new_global_ids, &PathBuf::from("./styles.css"));
        }
        return Some((new_global_classnames, new_global_ids));
    }

    let (new_file_classnames, new_file_ids, modified_code, original_code) =
        if let Some(data) = parse_and_modify_file(path, &cm) {
            data
        } else {
            return None;
        };

    let code_was_modified = original_code != modified_code;
    let data_was_modified =
        new_file_classnames != old_file_classnames || new_file_ids != old_file_ids;

    if !code_was_modified && !data_was_modified {
        return None;
    }

    file_map.insert(
        path.to_path_buf(),
        (new_file_classnames.clone(), new_file_ids.clone()),
    );

    if code_was_modified {
        write_file(path, &modified_code);
    }

    let (new_global_classnames, new_global_ids) = calculate_global_classnames_and_ids(file_map);
    
    let globals_did_change =
        &new_global_classnames != old_global_classnames || &new_global_ids != old_global_ids;

    if !globals_did_change {
        return Some((new_global_classnames, new_global_ids));
    }

    let source_added = new_file_classnames.difference(&old_file_classnames).count();
    let source_removed = old_file_classnames.difference(&new_file_classnames).count();

    let path_str = path.to_string_lossy().to_string();
    let display_name = path_str.bright_blue();

    let output_added = new_global_classnames
        .difference(old_global_classnames)
        .count()
        + new_global_ids.difference(old_global_ids).count();
    let output_removed = old_global_classnames
        .difference(&new_global_classnames)
        .count()
        + old_global_ids.difference(&new_global_ids).count();

    let output_path = PathBuf::from("./styles.css");
    write_css(&new_global_classnames, &new_global_ids, &output_path);

    let output_path_str = output_path
        .canonicalize()
        .unwrap_or(output_path.clone())
        .to_string_lossy()
        .to_string();
    let output_display = output_path_str.bright_yellow();

    let duration = start.elapsed();
    println!(
        "{} (+{}, -{}) -> {} (+{}, -{}) \u{2022} {}",
        display_name,
        source_added.to_string().bright_green(),
        source_removed.to_string().bright_red(),
        output_display,
        output_added.to_string().bright_green(),
        output_removed.to_string().bright_red(),
        format_duration(duration).bright_cyan()
    );

    Some((new_global_classnames, new_global_ids))
}

fn main() {
    let (mut file_map, mut global_classnames, mut global_ids) = initial_scan();
    let (tx, rx) = mpsc::channel();
    let mut watcher = RecommendedWatcher::new(
        tx,
        Config::default().with_poll_interval(Duration::from_millis(200)),
    )
    .expect("Failed to create file watcher");

    let watch_path = env::current_dir().unwrap().join("src");
    watcher
        .watch(&watch_path, RecursiveMode::Recursive)
        .expect("Failed to watch ./src directory");

    println!(
        "{}",
        "👀 Watching for file changes in ./src...".bold().bright_purple()
    );

    let mut debounce_map: HashMap<PathBuf, Instant> = HashMap::new();
    let debounce_duration = Duration::from_millis(100);

    loop {
        while let Ok(Ok(event)) = rx.try_recv() {
            if matches!(
                event.kind,
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
            ) {
                for path in event.paths {
                    if path.extension().and_then(|s| s.to_str()) == Some("tsx") {
                        let canonical_path = path.canonicalize().unwrap_or(path);
                        debounce_map.insert(canonical_path, Instant::now());
                    }
                }
            }
        }

        let mut paths_to_process = Vec::new();
        debounce_map.retain(|_path, last_event_time| {
            if last_event_time.elapsed() > debounce_duration {
                paths_to_process.push(_path.clone());
                false
            } else {
                true
            }
        });

        for path in paths_to_process {
            if let Some((new_classnames, new_ids)) =
                process_change(&path, &mut file_map, &global_classnames, &global_ids)
            {
                global_classnames = new_classnames;
                global_ids = new_ids;
            }
        }

        thread::sleep(Duration::from_millis(50));
    }
}
