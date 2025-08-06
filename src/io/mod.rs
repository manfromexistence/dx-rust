use regex::Regex;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

pub fn write_file(path: &Path, content: &str) {
    let file = File::create(path).expect("Could not create file");
    let mut writer = BufWriter::new(file);
    writer
        .write_all(content.as_bytes())
        .expect("Failed to write to file");
}

pub fn read_existing_css(path: &Path) -> (HashSet<String>, HashSet<String>) {
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

pub fn write_css(classnames: &HashSet<String>, ids: &HashSet<String>, output_path: &Path) {
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
