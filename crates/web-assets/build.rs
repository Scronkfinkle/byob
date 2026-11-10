//! Embed whatever `cargo xtask web` put in `assets/` (gitignored). Without it the list is
//! empty and byob's server explains how to build the UI instead.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn main() {
    let dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default()).join("assets");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir).map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_file()).collect()).unwrap_or_default();
    files.sort();
    let mut out = String::from("pub static ASSETS: &[Asset] = &[\n");
    for file in &files {
        println!("cargo:rerun-if-changed={}", file.display());
        let Some(name) = file.file_name().and_then(|n| n.to_str()) else { continue };
        // Prefer the gzipped copy when there is one; skip the raw twin.
        let (path, gzip) = match name.strip_suffix(".gz") {
            Some(raw) => (raw, true),
            None if dir.join(format!("{name}.gz")).is_file() => continue,
            None => (name, false),
        };
        let etag = etag(file);
        let _ = writeln!(out, "    Asset {{ path: {path:?}, gzip: {gzip}, etag: {etag:?}, bytes: include_bytes!({:?}) }},", file.display().to_string());
    }
    out.push_str("];\n");
    let target = PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or_default()).join("assets.rs");
    if let Err(e) = std::fs::write(&target, out) {
        println!("cargo:warning=couldn't write {}: {e}", target.display());
    }
}

/// A strong ETag from the file's bytes (FNV-1a), so a rebuilt UI is never served stale.
fn etag(file: &Path) -> String {
    let bytes = std::fs::read(file).unwrap_or_default();
    let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3));
    format!("\"{hash:016x}\"")
}
