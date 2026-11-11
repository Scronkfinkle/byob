//! Embed every file under `plugins/<id>/` (the bundled plugins' sources) so the binary needs
//! nothing on disk. Text files only; hidden files are skipped.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn walk(dir: &Path, rel: &str, out: &mut Vec<(String, PathBuf)>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = e.path();
        let rel = if rel.is_empty() { name } else { format!("{rel}/{name}") };
        if path.is_dir() {
            walk(&path, &rel, out);
        } else if path.is_file() {
            out.push((rel, path));
        }
    }
}

fn main() {
    let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default()).join("plugins");
    println!("cargo:rerun-if-changed={}", root.display());
    let mut files = Vec::new();
    walk(&root, "", &mut files);
    let mut out = String::from("pub static FILES: &[File] = &[\n");
    for (rel, path) in &files {
        println!("cargo:rerun-if-changed={}", path.display());
        let Some((plugin, inner)) = rel.split_once('/') else { continue };
        let _ = writeln!(out, "    File {{ plugin: {plugin:?}, path: {inner:?}, text: include_str!({:?}) }},", path.display().to_string());
    }
    out.push_str("];\n");
    let target = PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or_default()).join("bundled.rs");
    if let Err(e) = std::fs::write(&target, out) {
        println!("cargo:warning=couldn't write {}: {e}", target.display());
    }
}
