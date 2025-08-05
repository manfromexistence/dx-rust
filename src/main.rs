use colored::*;
use glob::glob;
use memmap2::Mmap;
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use swc_common::{sync::Lrc, FileName, SourceMap};
use swc_ecma_ast::{JSXAttrName, JSXAttrOrSpread, JSXAttrValue, JSXOpeningElement, Lit, JSXAttr, Ident, Str, JSXElementName};
use swc_ecma_parser::{lexer::Lexer, Parser, StringInput, Syntax, TsSyntax};
use swc_ecma_visit::{Visit, VisitWith, VisitMut, VisitMutWith};
use swc_ecma_codegen::{Emitter, text_writer::JsWriter};

struct ClassNameCollector<'a> {
    classnames: &'a mut HashSet<String>,
}

impl<'a> Visit for ClassNameCollector<'a> {
    fn visit_jsx_opening_element(&mut self, elem: &JSXOpeningElement) {
        for attr in &elem.attrs {
            if let JSXAttrOrSpread::JSXAttr(attr) = attr {
                if let JSXAttrName::Ident(ident) = &attr.name {
                    if ident.sym == "className" {
                        if let Some(JSXAttrValue::Lit(Lit::Str(s))) = &attr.value {
                            for classname in s.value.split_whitespace() {
                                self.classnames.insert(classname.to_string());
                            }
                        }
                    }
                }
            }
        }
        elem.visit_children_with(self);
    }
}

struct IdAdder;

impl VisitMut for IdAdder {
    fn visit_mut_jsx_opening_element(&mut self, elem: &mut JSXOpeningElement) {
        let mut classnames = Vec::new();
        let mut has_classname = false;

        for attr in &elem.attrs {
            if let JSXAttrOrSpread::JSXAttr(attr) = attr {
                if let JSXAttrName::Ident(ident) = &attr.name {
                    if ident.sym == "className" {
                        if let Some(JSXAttrValue::Lit(Lit::Str(s))) = &attr.value {
                            classnames = s.value.split_whitespace().collect();
                            has_classname = true;
                        }
                    }
                }
            }
        }

        if has_classname {
            let mut id_chars: Vec<char> = classnames.iter().filter_map(|s| s.chars().next()).collect();
            id_chars.sort_unstable();
            id_chars.dedup();
            let id: String = id_chars.into_iter().collect();

            let id_attr = JSXAttrOrSpread::JSXAttr(JSXAttr {
                name: JSXAttrName::Ident(Ident::new("id".into(), Default::default())),
                value: Some(JSXAttrValue::Lit(Lit::Str(Str {
                    value: id.into(),
                    span: Default::default(),
                    raw: None,
                }))),
                span: Default::default(),
            });

            elem.attrs.retain(|attr| {
                if let JSXAttrOrSpread::JSXAttr(a) = attr {
                    if let JSXAttrName::Ident(ident) = &a.name {
                        return ident.sym != "id";
                    }
                }
                true
            });

            elem.attrs.push(id_attr);
        }

        elem.visit_mut_children_with(self);
    }
}

fn parse_and_modify_file(path: &Path, cm: &Lrc<SourceMap>) -> Option<(HashSet<String>, String)> {
    let file = File::open(path).ok()?;
    let mmap = unsafe { Mmap::map(&file).ok()? };
    let source = String::from_utf8_lossy(&mmap).to_string();
    let fm = cm.new_source_file(
        FileName::Real(path.to_path_buf()).into(),
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
    let mut module = parser.parse_module().ok()?;

    let mut local_classnames = HashSet::new();
    let mut collector = ClassNameCollector {
        classnames: &mut local_classnames,
    };
    module.visit_with(&mut collector);

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

    Some((local_classnames, modified_code))
}

fn write_file(path: &Path, content: &str) {
    let file = File::create(path).expect("Could not create file");
    let mut writer = BufWriter::new(file);
    writer.write_all(content.as_bytes()).expect("Failed to write to file");
}

fn calculate_global_classnames(file_map: &HashMap<PathBuf, HashSet<String>>) -> HashSet<String> {
    file_map.values().flatten().cloned().collect()
}

fn write_css(classnames: &HashSet<String>) {
    let file = File::create("styles.css").expect("Could not create styles.css");
    let mut writer = BufWriter::new(file);
    let mut sorted_classnames: Vec<_> = classnames.iter().collect();
    sorted_classnames.sort();
    for classname in sorted_classnames {
        writeln!(writer, ".{} {{}}", classname).expect("Failed to write to styles.css");
    }
}

fn format_duration(duration: Duration) -> String {
    let micros = duration.as_micros();
    if micros < 1000 {
        format!("{}µs", micros)
    } else {
        format!("{}ms", micros / 1000)
    }
}

fn initial_scan() -> (HashMap<PathBuf, HashSet<String>>, HashSet<String>) {
    println!("{}", "🚀 dx-styles starting initial scan...".bold().bright_purple());
    let start = Instant::now();
    let cm: Lrc<SourceMap> = Default::default();
    let paths: Vec<_> = glob("./src/**/*.tsx")
        .expect("Failed to read glob pattern")
        .filter_map(Result::ok)
        .collect();
    let file_map: HashMap<PathBuf, HashSet<String>> = paths
        .par_iter()
        .filter_map(|path| {
            let (classnames, modified_code) = parse_and_modify_file(path, &cm)?;
            write_file(path, &modified_code);
            Some((path.clone(), classnames))
        })
        .collect();
    let global_classnames = calculate_global_classnames(&file_map);
    write_css(&global_classnames);
    let duration = start.elapsed();
    println!(
        "{} initial build (+{},{}) -> {} (+{},{}) \u{2022} {}",
        "✓".bright_green(),
        global_classnames.len().to_string().bright_green(),
        "0".bright_red(),
        "styles.css".bright_yellow(),
        global_classnames.len().to_string().bright_green(),
        "0".bright_red(),
        format_duration(duration).bright_cyan()
    );
    (file_map, global_classnames)
}

fn process_change(
    path: &Path,
    file_map: &mut HashMap<PathBuf, HashSet<String>>,
    old_global_classnames: &HashSet<String>,
) -> Option<HashSet<String>> {
    let start = Instant::now();
    let cm: Lrc<SourceMap> = Default::default();
    let old_file_classnames = file_map.get(path).cloned().unwrap_or_default();

    let (new_file_classnames, modified_code) = if path.exists() {
        parse_and_modify_file(path, &cm).unwrap_or_default()
    } else {
        (HashSet::new(), String::new())
    };

    let source_added_set: HashSet<_> = new_file_classnames.difference(&old_file_classnames).collect();
    let source_removed_set: HashSet<_> = old_file_classnames.difference(&new_file_classnames).collect();
    let source_added = source_added_set.len();
    let source_removed = source_removed_set.len();

    let display_name = if !source_added_set.is_empty() {
        let mut id_chars: Vec<char> = source_added_set
            .iter()
            .filter_map(|s| s.chars().next())
            .collect();
        id_chars.sort_unstable();
        id_chars.dedup();
        let id: String = id_chars.into_iter().collect();
        format!("id={}", id)
    } else {
        path.strip_prefix("./").unwrap_or(path).to_string_lossy().to_string()
    };

    if path.exists() {
        write_file(path, &modified_code);
    }

    if new_file_classnames.is_empty() && !path.exists() {
        file_map.remove(path);
    } else {
        file_map.insert(path.to_path_buf(), new_file_classnames.clone());
    }

    let new_global_classnames = calculate_global_classnames(file_map);
    let output_added_set: HashSet<_> = new_global_classnames.difference(old_global_classnames).collect();
    let output_removed_set: HashSet<_> = old_global_classnames.difference(&new_global_classnames).collect();
    let output_added = output_added_set.len();
    let output_removed = output_removed_set.len();

    if output_added > 0 || output_removed > 0 {
        write_css(&new_global_classnames);
    }

    let output_display = if !output_added_set.is_empty() || !output_removed_set.is_empty() {
        let mut id_chars: Vec<char> = output_added_set
            .iter()
            .chain(output_removed_set.iter())
            .filter_map(|s| s.chars().next())
            .collect();
        id_chars.sort_unstable();
        id_chars.dedup();
        let id: String = id_chars.into_iter().collect();
        format!("styles.css(id={})", id)
    } else {
        "styles.css".to_string()
    };

    let duration = start.elapsed();
    println!(
        "{} (+{},{}) -> {} (+{},{}) \u{2022} {}",
        display_name.bright_blue(),
        source_added.to_string().bright_green(),
        source_removed.to_string().bright_red(),
        output_display.bright_yellow(),
        output_added.to_string().bright_green(),
        output_removed.to_string().bright_red(),
        format_duration(duration).bright_cyan()
    );
    Some(new_global_classnames)
}

fn main() {
    let (mut file_map, mut global_classnames) = initial_scan();
    let (tx, rx) = mpsc::channel();
    let mut watcher = RecommendedWatcher::new(
        tx,
        Config::default()
            .with_poll_interval(Duration::from_millis(500))
            .with_compare_contents(true),
    )
    .expect("Failed to create file watcher");
    watcher
        .watch(Path::new("./src"), RecursiveMode::Recursive)
        .expect("Failed to watch ./src directory");
    println!("{}", "👀 Watching for file changes in ./src...".bold().bright_purple());

    let mut last_processed = Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_millis(1000)) {
            Ok(Ok(event)) => {
                if last_processed.elapsed() < Duration::from_millis(1000) {
                    continue;
                }
                let mut changed_paths = HashSet::new();
                for path in event.paths {
                    if path.extension().and_then(|s| s.to_str()) == Some("tsx")
                        && path.file_name() != Some(std::ffi::OsStr::new("styles.css"))
                    {
                        changed_paths.insert(path);
                    }
                }

                let debounce_timeout = Instant::now();
                while Instant::now().duration_since(debounce_timeout) < Duration::from_millis(300) {
                    if let Ok(Ok(next_event)) = rx.try_recv() {
                        for path in next_event.paths {
                            if path.extension().and_then(|s| s.to_str()) == Some("tsx")
                                && path.file_name() != Some(std::ffi::OsStr::new("styles.css"))
                            {
                                changed_paths.insert(path);
                            }
                        }
                    }
                }

                if !changed_paths.is_empty() {
                    last_processed = Instant::now();
                    for path in changed_paths {
                        if let Some(new_globals) = process_change(&path, &mut file_map, &global_classnames) {
                            global_classnames = new_globals;
                        }
                    }
                }
            }
            Ok(Err(e)) => eprintln!("{} {:?}", "Watch error:".bright_red().bold(), e),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                eprintln!("{} Channel disconnected", "Error:".bright_red().bold());
                break;
            }
        }
    }
}