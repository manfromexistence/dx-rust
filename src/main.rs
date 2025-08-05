use colored::*;
use glob::glob;
use memmap2::Mmap;
use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use rayon::prelude::*;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use swc_common::{sync::Lrc, FileName, SourceMap};
use swc_ecma_ast::{JSXAttrName, JSXAttrOrSpread, JSXAttrValue, JSXOpeningElement, Lit, Module};
use swc_ecma_parser::{lexer::Lexer, Parser, StringInput, Syntax, TsSyntax};
use swc_ecma_visit::{Visit, VisitWith};

/// A visitor to traverse the SWC AST and collect all unique CSS class names
/// from `className` props in JSX elements. This is from your provided logic.
struct ClassNameCollector<'a> {
    classnames: &'a mut HashSet<String>,
}

impl<'a> Visit for ClassNameCollector<'a> {
    fn visit_jsx_opening_element(&mut self, elem: &JSXOpeningElement) {
        for attr in &elem.attrs {
            if let JSXAttrOrSpread::JSXAttr(attr) = attr {
                if let JSXAttrName::Ident(ident) = &attr.name {
                    // We only care about the "className" attribute.
                    if ident.sym == "className" {
                        if let Some(JSXAttrValue::Lit(Lit::Str(s))) = &attr.value {
                            // Split the string by whitespace to handle multiple class names.
                            for classname in s.value.split_whitespace() {
                                self.classnames.insert(classname.to_string());
                            }
                        }
                    }
                }
            }
        }
        // Continue visiting children to find nested elements.
        elem.visit_children_with(self);
    }
}

/// Scans all .tsx files in the ./src directory, parses them in parallel,
/// collects all unique class names, and writes them to a CSS file.
fn process_all_files(change_reason: &str) {
    let start = Instant::now();

    // 1. Use `glob` to find all .tsx files and collect their paths.
    let paths: Vec<_> = glob("./src/**/*.tsx")
        .expect("Failed to read glob pattern")
        .filter_map(Result::ok)
        .collect();

    // 2. Use Rayon to process files in parallel for maximum speed.
    let classnames: HashSet<String> = paths
        .par_iter() // Switch to a parallel iterator!
        .filter_map(|path| {
            // Use memory-mapped files for efficient I/O.
            let file = File::open(path).ok()?;
            // SAFETY: We are opening the file in read-only mode.
            let mmap = unsafe { Mmap::map(&file).ok()? };

            // Each parallel thread needs its own SourceMap instance for thread-safety.
            let cm: Lrc<SourceMap> = Default::default();
            let fm = cm.new_source_file(
                FileName::Real(path.clone()),
                String::from_utf8_lossy(&mmap).to_string(),
            );

            let lexer = Lexer::new(
                // Use the TsSyntax struct as in your provided code.
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
        // 3. Reduce the results from all threads into a single HashSet.
        .reduce(HashSet::new, |mut acc, set| {
            acc.extend(set);
            acc
        });

    // 4. Write the collected class names to the output file.
    write_css(&classnames);

    // 5. Log the result with fancy colors and timing.
    let duration = start.elapsed();
    println!(
        "{} {} {} {} {} {:.2?}",
        "✓".green(),
        change_reason.cyan(),
        "->".bright_black(),
        "styles.css".green(),
        "in".bright_black(),
        duration
    );
}

/// Writes the set of unique class names to the styles.css file.
fn write_css(classnames: &HashSet<String>) {
    let file = File::create("styles.css").expect("Could not create styles.css");
    let mut writer = BufWriter::new(file);
    // For deterministic output, it's good practice to sort the class names.
    let mut sorted_classnames: Vec<_> = classnames.iter().collect();
    sorted_classnames.sort();

    for classname in sorted_classnames {
        writeln!(writer, ".{} {{}}", classname).expect("Failed to write to styles.css");
    }
}

fn main() {
    println!("{}", "🚀 dx-styles starting...".bold().magenta());
    // Perform an initial scan and build when the tool starts.
    process_all_files("Initial build");

    let (tx, rx) = mpsc::channel();

    // Create a file watcher with a small poll interval.
    let mut watcher = RecommendedWatcher::new(tx, Config::default().with_poll_interval(Duration::from_millis(200)))
        .expect("Failed to create file watcher");

    // Watch the ./src directory recursively for any changes.
    watcher
        .watch(Path::new("./src"), RecursiveMode::Recursive)
        .expect("Failed to watch ./src directory");

    println!("{}", "👀 Watching for file changes in ./src...".yellow());

    // Main event loop to handle file change notifications.
    loop {
        match rx.recv() {
            Ok(Ok(event)) => {
                // We are interested in any modification, creation, or deletion.
                if let EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_) = event.kind {
                    // This is a simple debounce. Wait a moment for subsequent file events
                    // to arrive, then drain the queue to process only the latest state.
                    std::thread::sleep(Duration::from_millis(50));
                    while rx.try_recv().is_ok() {
                        // Drain the channel.
                    }

                    if let Some(path) = event.paths.first() {
                        // Only re-process if a .tsx file was changed.
                        if path.extension().and_then(|s| s.to_str()) == Some("tsx") {
                            let display_path = path.strip_prefix("./").unwrap_or(path);
                            process_all_files(&display_path.to_string_lossy());
                        }
                    }
                }
            }
            Ok(Err(e)) => eprintln!("{} {:?}", "Watch error:".red().bold(), e),
            Err(e) => eprintln!("{} {:?}", "Channel error:".red().bold(), e),
        }
    }
}
