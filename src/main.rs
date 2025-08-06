use colored::*;
use glob::glob;
use memmap2::Mmap;
use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use swc_common::{SourceMap, FileName};
use swc_ecma_codegen::{text_writer::JsWriter, Emitter};
use swc_ecma_parser::{lexer::Lexer, Parser, StringInput, Syntax, TsSyntax};
use swc_ecma_visit::{VisitMutWith};

pub mod id;
pub mod io;
use id::{determine_css_entities_and_updates, IdApplier};
use io::{read_existing_css, write_css, write_file};

fn parse_and_modify_file(
    path: &Path,
    cm: &Arc<SourceMap>,
) -> Option<(HashSet<String>, HashSet<String>, String, String)> {
    let file = std::fs::File::open(path).ok()?;
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
    let file = std::fs::File::open(path).ok()?;
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
