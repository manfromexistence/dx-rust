use colored::Colorize;
use glob::glob;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use swc_common::{sync::Lrc, SourceMap};
use swc_ecma_ast::{JSXAttrName, JSXAttrOrSpread, JSXAttrValue, JSXOpeningElement, Lit, Module};
use swc_ecma_parser::{lexer::Lexer, Parser, StringInput, Syntax, TsSyntax};
use swc_ecma_visit::{Visit, VisitWith};

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

fn parse_file(cm: &SourceMap, path: &Path) -> Result<Module, String> {
    let fm = cm.load_file(path).map_err(|e| format!("{:?}", e))?;
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
    parser.parse_module().map_err(|e| format!("{:?}", e))
}

fn process_file(cm: &SourceMap, path: &Path, cache: &mut HashMap<PathBuf, HashSet<String>>) -> HashSet<String> {
    let mut classnames = HashSet::new();
    if let Ok(module) = parse_file(cm, path) {
        let mut collector = ClassNameCollector {
            classnames: &mut classnames,
        };
        module.visit_with(&mut collector);
    }
    cache.insert(path.to_path_buf(), classnames.clone());
    classnames
}

fn update_styles(
    changed_path: &Path,
    cm: &SourceMap,
    cache: &mut HashMap<PathBuf, HashSet<String>>,
    global_classnames: &mut HashSet<String>,
) {
    let start = Instant::now();
    let old_classnames = global_classnames.clone();
    let new_classnames = process_file(cm, changed_path, cache);
    global_classnames.extend(new_classnames.clone());
    let added: Vec<_> = new_classnames.difference(&old_classnames).collect();
    let removed: Vec<_> = old_classnames.difference(&new_classnames).collect();
    let added_count = added.len();
    let removed_count = removed.len();
    write_css(global_classnames);
    let duration = start.elapsed();
    let time_str = if duration.as_millis() < 1 {
        format!("{}µs", duration.as_micros())
    } else {
        format!("{:.1}ms", duration.as_secs_f64() * 1000.0)
    };
    println!(
        "{} ({}, {}) -> {} ({}, {}) \u{2022} {}",
        changed_path.display().to_string().yellow(),
        format!("+{}", added_count).green(),
        format!("-{}", removed_count).red(),
        "styles.css".cyan(),
        format!("+{}", added_count).green(),
        format!("-{}", removed_count).red(),
        time_str
    );
}

fn write_css(classnames: &HashSet<String>) {
    let file = File::create("styles.css").unwrap();
    let mut writer = BufWriter::new(file);
    for classname in classnames {
        writeln!(writer, ".{} {{}}", classname).unwrap();
    }
}

fn main() {
    let cm: Lrc<SourceMap> = Default::default();
    let mut cache: HashMap<PathBuf, HashSet<String>> = HashMap::new();
    let mut global_classnames: HashSet<String> = HashSet::new();
    for entry in glob("./src/**/*.tsx").expect("Failed to read glob pattern") {
        if let Ok(path) = entry {
            global_classnames.extend(process_file(&cm, &path, &mut cache));
        }
    }
    write_css(&global_classnames);
    let (tx, rx) = mpsc::channel();
    let config = Config::default().with_poll_interval(Duration::from_millis(100));
    let mut watcher = RecommendedWatcher::new(tx, config).unwrap();
    watcher.watch(Path::new("./src"), RecursiveMode::Recursive).unwrap();
    loop {
        match rx.recv() {
            Ok(Ok(event)) => {
                if let Event {
                    kind: EventKind::Modify(_),
                    paths,
                    ..
                } = event
                {
                    for path in paths {
                        if path.extension().and_then(|s| s.to_str()) == Some("tsx") {
                            update_styles(&path, &cm, &mut cache, &mut global_classnames);
                        }
                    }
                }
            }
            Ok(Err(e)) => println!("watch error: {:?}", e),
            Err(e) => println!("channel error: {:?}", e),
        }
    }
}