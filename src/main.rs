use colored::*;
use glob::glob;
use memmap2::Mmap;
use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use rayon::prelude::*;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use swc_common::{sync::Lrc, FileName, SourceMap};
use swc_ecma_ast::{JSXAttrName, JSXAttrOrSpread, JSXAttrValue, JSXOpeningElement, Lit};
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

fn process_all_files(change_reason: &str, last_classnames: &HashSet<String>) -> HashSet<String> {
    let start = Instant::now();

    let paths: Vec<_> = glob("./src/**/*.tsx")
        .expect("Failed to read glob pattern")
        .filter_map(Result::ok)
        .collect();

    let classnames: HashSet<String> = paths
        .par_iter()
        .filter_map(|path| {
            let file = File::open(path).ok()?;
            let mmap = unsafe { Mmap::map(&file).ok()? };

            let cm: Lrc<SourceMap> = Default::default();
            let fm = cm.new_source_file(
                FileName::Real(path.clone()).into(),
                String::from_utf8_lossy(&mmap).to_string(),
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
            let module = parser.parse_module().ok()?;

            let mut local_classnames = HashSet::new();
            let mut collector = ClassNameCollector {
                classnames: &mut local_classnames,
            };
            module.visit_with(&mut collector);
            Some(local_classnames)
        })
        .reduce(HashSet::new, |mut acc, set| {
            acc.extend(set);
            acc
        });

    write_css(&classnames);

    let added = classnames.difference(last_classnames).count();
    let removed = last_classnames.difference(&classnames).count();
    let duration = start.elapsed().as_millis();

    println!(
        "{} (+{},-{}) -> styles.css (+{},-{}) \u{2022} {}ms",
        change_reason.cyan(),
        added.to_string().green(),
        removed.to_string().red(),
        added.to_string().green(),
        removed.to_string().red(),
        duration
    );

    classnames
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

fn main() {
    let mut classnames = process_all_files("initial", &HashSet::new());

    let (tx, rx) = mpsc::channel();

    let mut watcher = RecommendedWatcher::new(tx, Config::default().with_poll_interval(Duration::from_millis(200)))
        .expect("Failed to create file watcher");

    watcher
        .watch(Path::new("./src"), RecursiveMode::Recursive)
        .expect("Failed to watch ./src directory");

    loop {
        match rx.recv() {
            Ok(Ok(event)) => {
                if let EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_) = event.kind {
                    std::thread::sleep(Duration::from_millis(50));
                    while rx.try_recv().is_ok() {}

                    if let Some(path) = event.paths.first() {
                        if path.extension().and_then(|s| s.to_str()) == Some("tsx") {
                            let display_path = path.strip_prefix("./").unwrap_or(path);
                            classnames = process_all_files(&display_path.to_string_lossy(), &classnames);
                        }
                    }
                }
            }
            Ok(Err(e)) => eprintln!("watch error: {:?}", e),
            Err(e) => eprintln!("channel error: {:?}", e),
        }
    }
}
