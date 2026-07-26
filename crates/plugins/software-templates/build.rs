//! Embeds the scaffolds in the binary. Every file under `scaffolds/<language>/<part>/…` becomes an
//! entry in one table, so a plugin running from a container image carries the whole library with
//! it. The scaffolds are ordinary files on disk — Go that gofmt reads, Rust that rustfmt reads —
//! rather than string literals in Rust, which is what keeps them worth reading.

use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("the crate's directory"))
        .join("scaffolds");
    let out =
        PathBuf::from(std::env::var("OUT_DIR").expect("a build directory")).join("scaffolds.rs");
    println!("cargo:rerun-if-changed=scaffolds");

    let mut entries = Vec::new();
    for language in sorted(&root) {
        let language_name = name(&language);
        for part in sorted(&language) {
            let part_name = name(&part);
            let mut files = Vec::new();
            walk(&part, &part, &mut files);
            files.sort();
            for (path, on_disk) in files {
                println!("cargo:rerun-if-changed={}", on_disk.display());
                entries.push(format!(
                    "    ({:?}, {:?}, {:?}, include_str!({:?})),",
                    language_name,
                    part_name,
                    path,
                    on_disk.display().to_string()
                ));
            }
        }
    }

    let generated = format!(
        "/// Every scaffold file: the language, the part (`base` or an application type), the path \
         it is written to, and what it holds.\npub static FILES: &[(&str, &str, &str, &str)] = \
         &[\n{}\n];\n",
        entries.join("\n")
    );
    fs::write(&out, generated).expect("writing the scaffold table");
}

fn name(path: &Path) -> String {
    path.file_name().unwrap_or_default().to_string_lossy().into_owned()
}

/// A directory's entries, in a fixed order, so the table does not shuffle between builds.
fn sorted(path: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(path)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    found
}

/// Every file under `at`, as the path it is written to in the repository the template makes.
fn walk(root: &Path, at: &Path, found: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = fs::read_dir(at) else { return };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, found);
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .map(|relative| relative.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        if !relative.is_empty() {
            found.push((relative, path));
        }
    }
}
