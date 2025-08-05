use glob::glob;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
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

fn process_all_files(changed_path: &Path, cm: &SourceMap) {
    let start = Instant::now();
    let mut classnames = HashSet::new();
    for entry in glob("./src/**/*.tsx").expect("Failed to read glob pattern") {
        if let Ok(path) = entry {
            if let Ok(module) = parse_file(cm, &path) {
                let mut collector = ClassNameCollector {
                    classnames: &mut classnames,
                };
                module.visit_with(&mut collector);
            }
        }
    }
    write_css(&classnames);
    let duration = start.elapsed().as_millis();
    println!(
        "{} (+0,-0) -> styles.css (+0,-0) \u{2022} {}ms",
        changed_path.display(),
        duration
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
    process_all_files(Path::new("initial"), &cm);
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
                            process_all_files(&path, &cm);
                        }
                    }
                }
            }
            Ok(Err(e)) => println!("watch error: {:?}", e),
            Err(e) => println!("channel error: {:?}", e),
        }
    }
}