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

fn parse_file(path: &Path) -> Option<HashSet<String>> {
    let file = File::open(path).ok()?;
    let mmap = unsafe { Mmap::map(&file).ok()? };
    let cm: Lrc<SourceMap> = Default::default();
    let fm = cm.new_source_file(
        FileName::Real(path.to_path_buf()).into(),
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
    println!("{}", "🚀 dx-styles starting initial scan...".bold().bright_magenta());
    let start = Instant::now();
    let paths: Vec<_> = glob("./src/**/*.tsx")
        .expect("Failed to read glob pattern")
        .filter_map(Result::ok)
        .collect();
    let file_map: HashMap<PathBuf, HashSet<String>> = paths
        .par_iter()
        .filter_map(|path| {
            let classnames = parse_file(path)?;
            Some((path.clone(), classnames))
        })
        .collect();
    let global_classnames = calculate_global_classnames(&file_map);
    write_css(&global_classnames);
    let duration = start.elapsed();
    println!(
        "{} initial build (+{},-0) -> styles.css (+{},-0) \u{2022} {}",
        "✓".bright_green(),
        global_classnames.len().to_string().bright_green(),
        global_classnames.len().to_string().bright_green(),
        format_duration(duration)
    );
    (file_map, global_classnames)
}

fn process_change(
    path: &Path,
    file_map: &mut HashMap<PathBuf, HashSet<String>>,
    old_global_classnames: &HashSet<String>,
) -> Option<HashSet<String>> {
    let start = Instant::now();
    let old_file_classnames = file_map.get(path).cloned().unwrap_or_default();
    let new_file_classnames = if path.exists() {
        parse_file(path).unwrap_or_default()
    } else {
        HashSet::new()
    };
    
    let source_added_set: HashSet<_> = new_file_classnames.difference(&old_file_classnames).collect();
    let source_added = source_added_set.len();
    let source_removed = old_file_classnames.difference(&new_file_classnames).count();

    let display_name = if !source_added_set.is_empty() {
        let mut id_chars: Vec<char> = source_added_set.iter().filter_map(|s| s.chars().next()).collect();
        id_chars.sort_unstable();
        let id: String = id_chars.into_iter().collect();
        format!("id={}", id)
    } else {
        path.strip_prefix("./").unwrap_or(path).to_string_lossy().to_string()
    };

    if new_file_classnames.is_empty() && !path.exists() {
        file_map.remove(path);
    } else {
        file_map.insert(path.to_path_buf(), new_file_classnames);
    }

    let new_global_classnames = calculate_global_classnames(file_map);
    let output_added = new_global_classnames.difference(old_global_classnames).count();
    let output_removed = old_global_classnames.difference(&new_global_classnames).count();

    if output_added > 0 || output_removed > 0 {
        write_css(&new_global_classnames);
    }

    let duration = start.elapsed();
    println!(
        "{} (+{},-{}) -> styles.css (+{},-{}) \u{2022} {}",
        display_name.bright_cyan(),
        source_added.to_string().bright_green(),
        source_removed.to_string().bright_red(),
        output_added.to_string().bright_green(),
        output_removed.to_string().bright_red(),
        format_duration(duration)
    );
    Some(new_global_classnames)
}

fn main() {
    let (mut file_map, mut global_classnames) = initial_scan();
    let (tx, rx) = mpsc::channel();
    let mut watcher = RecommendedWatcher::new(tx, Config::default().with_poll_interval(Duration::from_millis(200)))
        .expect("Failed to create file watcher");
    watcher
        .watch(Path::new("./src"), RecursiveMode::Recursive)
        .expect("Failed to watch ./src directory");
    println!("{}", "👀 Watching for file changes in ./src...".bright_yellow());

    loop {
        match rx.recv() {
            Ok(Ok(event)) => {
                let mut changed_paths = HashSet::new();
                for path in event.paths {
                    if path.extension().and_then(|s| s.to_str()) == Some("tsx") {
                        changed_paths.insert(path);
                    }
                }
                
                std::thread::sleep(Duration::from_millis(50));
                while let Ok(Ok(next_event)) = rx.try_recv() {
                    for path in next_event.paths {
                         if path.extension().and_then(|s| s.to_str()) == Some("tsx") {
                            changed_paths.insert(path);
                        }
                    }
                }

                for path in changed_paths {
                    if let Some(new_globals) = process_change(&path, &mut file_map, &global_classnames) {
                        global_classnames = new_globals;
                    }
                }
            }
            Ok(Err(e)) => eprintln!("{} {:?}", "Watch error:".red().bold(), e),
            Err(e) => eprintln!("{} {:?}", "Channel error:".red().bold(), e),
        }
    }
}
