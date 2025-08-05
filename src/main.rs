use colored::*;
use glob::glob;
use memmap2::Mmap;
use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use rayon::prelude::*;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use swc_common::{FileName, SourceMap};
use swc_ecma_ast::{
    IdentName, JSXAttr, JSXAttrName, JSXAttrOrSpread, JSXAttrValue, JSXOpeningElement, Lit, Str,
};
use swc_ecma_codegen::{text_writer::JsWriter, Emitter};
use swc_ecma_parser::{lexer::Lexer, Parser, StringInput, Syntax, TsSyntax};
use swc_ecma_visit::{VisitMut, VisitMutWith};

struct ClassNameCollector<'a> {
    classnames: &'a mut HashSet<String>,
    ids: &'a mut HashSet<String>,
}

impl<'a> VisitMut for ClassNameCollector<'a> {
    fn visit_mut_jsx_opening_element(&mut self, elem: &mut JSXOpeningElement) {
        elem.visit_mut_children_with(self); // Ensure children are visited

        let mut classnames_on_element = Vec::new();
        let mut has_classname_attr = false;

        for attr in &elem.attrs {
            if let JSXAttrOrSpread::JSXAttr(attr) = attr {
                if let JSXAttrName::Ident(ident) = &attr.name {
                    if ident.sym == "className" {
                        has_classname_attr = true;
                        if let Some(JSXAttrValue::Lit(Lit::Str(s))) = &attr.value {
                            if !s.value.is_empty() {
                                classnames_on_element = s.value.split_whitespace().collect();
                                for classname in &classnames_on_element {
                                    self.classnames.insert(classname.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }

        if has_classname_attr && !classnames_on_element.is_empty() {
            let mut id_chars: Vec<char> = classnames_on_element
                .iter()
                .filter_map(|s| s.chars().next())
                .collect();
            id_chars.sort_unstable();
            id_chars.dedup();
            let id: String = id_chars.into_iter().collect();
            self.ids.insert(id);
        }
    }
}

// OPTIMIZATION: This visitor now checks if the correct `id` already exists.
// It will only modify the AST if the `id` is missing or incorrect.
// This prevents unnecessary code generation and file writes for already-correct files.
struct IdAdder;

impl VisitMut for IdAdder {
    fn visit_mut_jsx_opening_element(&mut self, elem: &mut JSXOpeningElement) {
        elem.visit_mut_children_with(self); // Traverse deeper first

        let mut classnames = Vec::new();
        let mut has_classname = false;

        for attr in &elem.attrs {
            if let JSXAttrOrSpread::JSXAttr(attr) = attr {
                if let JSXAttrName::Ident(ident) = &attr.name {
                    if ident.sym == "className" {
                        if let Some(JSXAttrValue::Lit(Lit::Str(s))) = &attr.value {
                            if !s.value.is_empty() {
                                classnames = s.value.split_whitespace().collect();
                                has_classname = true;
                            }
                        }
                    }
                }
            }
        }

        if has_classname {
            let mut id_chars: Vec<char> = classnames.iter().filter_map(|s| s.chars().next()).collect();
            id_chars.sort_unstable();
            id_chars.dedup();
            let new_id: String = id_chars.into_iter().collect();

            let mut has_correct_id = false;
            let mut has_any_id = false;

            // Check for an existing 'id' attribute.
            for attr in &elem.attrs {
                if let JSXAttrOrSpread::JSXAttr(a) = attr {
                    if let JSXAttrName::Ident(ident) = &a.name {
                        if ident.sym == "id" {
                            has_any_id = true;
                            if let Some(JSXAttrValue::Lit(Lit::Str(s))) = &a.value {
                                if s.value.as_ref() == new_id {
                                    has_correct_id = true;
                                }
                            }
                            break;
                        }
                    }
                }
            }

            // Only modify the AST if we don't already have the correct id.
            if !has_correct_id {
                // Remove the old 'id' attribute if it exists.
                if has_any_id {
                    elem.attrs.retain(|attr| {
                        if let JSXAttrOrSpread::JSXAttr(a) = attr {
                            if let JSXAttrName::Ident(ident) = &a.name {
                                return ident.sym != "id";
                            }
                        }
                        true
                    });
                }

                // Add the new, correct 'id' attribute.
                let id_attr = JSXAttrOrSpread::JSXAttr(JSXAttr {
                    name: JSXAttrName::Ident(IdentName::new("id".into(), Default::default())),
                    value: Some(JSXAttrValue::Lit(Lit::Str(Str {
                        value: new_id.into(),
                        span: Default::default(),
                        raw: None,
                    }))),
                    span: Default::default(),
                });
                elem.attrs.push(id_attr);
            }
        }
    }
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
        Syntax::Typescript(TsSyntax {
            tsx: true,
            ..Default::default()
        }),
        Default::default(),
        StringInput::from(&*fm),
        None,
    );
    let mut parser = Parser::new_from(lexer);
    let mut module = match parser.parse_module() {
        Ok(module) => module,
        Err(_) => return None,
    };

    let mut local_classnames = HashSet::new();
    let mut local_ids = HashSet::new();
    let mut collector = ClassNameCollector {
        classnames: &mut local_classnames,
        ids: &mut local_ids,
    };
    module.visit_mut_with(&mut collector);

    let mut id_adder = IdAdder;
    module.visit_mut_with(&mut id_adder);

    let mut output = Vec::new();
    let mut emitter = Emitter {
        cfg: Default::default(),
        cm: cm.clone(),
        comments: None,
        wr: JsWriter::new(cm.clone(), "\n", &mut output, None),
    };
    emitter.emit_module(&module).ok()?;
    let modified_code = String::from_utf8(output).ok()?;

    Some((local_classnames, local_ids, modified_code, source))
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

// NEW: Helper function to read existing classes and IDs from a CSS file.
// This helps prevent rewriting the file if nothing has changed.
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

    // This regex is simple but effective for the format this tool generates.
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

    let current_dir = env::current_dir().expect("Failed to get current directory");
    let paths: Vec<_> = glob("./src/**/*.tsx")
        .expect("Failed to read glob pattern")
        .filter_map(Result::ok)
        .map(|path| path.canonicalize().unwrap_or_else(|_| current_dir.join(path)))
        .collect();

    let file_map: HashMap<PathBuf, (HashSet<String>, HashSet<String>)> = paths
        .par_iter()
        .filter_map(|path| {
            if let Some((classnames, ids, modified_code, original_code)) =
                parse_and_modify_file(path, &cm)
            {
                // The optimized IdAdder reduces how often this is true.
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
    let output_path = PathBuf::from("./styles.css");

    // OPTIMIZATION: Check if the CSS file needs to be written at all.
    // This compares the newly generated sets with what's already on disk.
    let (existing_classnames, existing_ids) = read_existing_css(&output_path);
    if global_classnames != existing_classnames || global_ids != existing_ids {
        println!("{}", "CSS changes detected, regenerating styles.css...".yellow());
        write_css(&global_classnames, &global_ids, &output_path);
    }

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

    let (old_file_classnames, _) = file_map.get(path).cloned().unwrap_or_default();

    let (new_file_classnames, new_file_ids, modified_code, original_code) = if path.exists() {
        parse_and_modify_file(path, &cm).unwrap_or_default()
    } else {
        (HashSet::new(), HashSet::new(), String::new(), String::new())
    };

    // If the file exists but our optimized visitors made no changes, we can stop early.
    if path.exists() && original_code == modified_code {
        return None;
    }

    if path.exists() {
        file_map.insert(
            path.to_path_buf(),
            (new_file_classnames.clone(), new_file_ids.clone()),
        );
    } else {
        file_map.remove(path);
    }

    let (new_global_classnames, new_global_ids) = calculate_global_classnames_and_ids(file_map);

    // If the global sets haven't changed, no need to write CSS.
    if &new_global_classnames == old_global_classnames && &new_global_ids == old_global_ids {
        // Still write the source file if it changed
        if path.exists() && original_code != modified_code {
            write_file(path, &modified_code);
        }
        return Some((new_global_classnames, new_global_ids));
    }

    if path.exists() && original_code != modified_code {
        write_file(path, &modified_code);
    }

    let source_added = new_file_classnames.difference(&old_file_classnames).count();
    let source_removed = old_file_classnames.difference(&new_file_classnames).count();

    let path_str = path.to_string_lossy().to_string();
    let display_name = path_str.bright_blue();

    let output_added = new_global_classnames.difference(old_global_classnames).count()
        + new_global_ids.difference(old_global_ids).count();
    let output_removed = old_global_classnames.difference(&new_global_classnames).count()
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
        "{} (+{},{}) -> {} (+{},{}) \u{2022} {}",
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
