//! Bundled plugins on disk: a bundled plugin's files live in the binary, but a backend or a
//! script tool needs them as files a process can run. [`materialize`] writes them once to a
//! versioned cache folder, `<root>/<id>-<byob version>-<content hash>/`, and returns it.
//!
//! - The folder name changes whenever the files or byob's version do, so a folder that
//!   exists is complete and current: it is written under a temporary name and renamed into
//!   place (two byob processes racing both end up with the same folder).
//! - Its files are made read-only: edits belong in a fork (`keep_plugin`), not here. Python
//!   may still add `__pycache__`.
//! - Older folders of the same plugin are removed when a new one is made (best effort; a
//!   folder another running byob still uses is left alone if it can't be removed).

use std::collections::BTreeMap;
use std::hash::Hasher as _;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::manifest::{path_inside, plugin_id_ok};

/// What marks a materialized folder (and holds its hash).
pub const MARKER: &str = ".byob-bundled";

/// Most bytes one materialized plugin may hold.
const MAX_BYTES: usize = 64 * 1024 * 1024;

/// FNV-1a over the files (path, NUL, text, NUL), stable across runs.
pub fn content_hash(files: &BTreeMap<String, String>) -> u64 {
    struct Fnv(u64);
    impl std::hash::Hasher for Fnv {
        fn finish(&self) -> u64 {
            self.0
        }
        fn write(&mut self, bytes: &[u8]) {
            for b in bytes {
                self.0 ^= u64::from(*b);
                self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
            }
        }
    }
    let mut h = Fnv(0xcbf2_9ce4_8422_2325);
    for (path, text) in files {
        h.write(path.as_bytes());
        h.write_u8(0);
        h.write(text.as_bytes());
        h.write_u8(0);
    }
    h.finish()
}

/// The folder `id`'s files go to under `root` for this byob `version`.
pub fn folder(root: &Path, id: &str, version: &str, files: &BTreeMap<String, String>) -> PathBuf {
    let version: String = version.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' { c } else { '_' }).take(32).collect();
    root.join(format!("{id}-{version}-{:016x}", content_hash(files)))
}

/// Write plugin `id`'s `files` under `root` (if not there yet) and return the folder.
pub fn materialize(root: &Path, id: &str, version: &str, files: &BTreeMap<String, String>) -> Result<PathBuf, String> {
    if !plugin_id_ok(id) {
        return Err(format!("{id:?} isn't a plugin id"));
    }
    let dir = folder(root, id, version, files);
    if dir.join(MARKER).is_file() {
        return Ok(dir);
    }
    let total: usize = files.values().map(String::len).sum();
    if total > MAX_BYTES {
        return Err(format!("plugin {id} is too big to write out ({total} bytes)"));
    }
    std::fs::create_dir_all(root).map_err(|e| format!("can't create {}: {e}", root.display()))?;
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let tmp = root.join(format!(".tmp-{id}-{}-{nanos:x}", std::process::id()));
    let written = write_all(&tmp, files);
    if let Err(e) = written {
        remove_tree(&tmp);
        return Err(e);
    }
    match std::fs::rename(&tmp, &dir) {
        Ok(()) => {}
        // Someone else made it first (a rename onto a non-empty folder fails).
        Err(_) if dir.join(MARKER).is_file() => remove_tree(&tmp),
        Err(e) => {
            remove_tree(&tmp);
            return Err(format!("can't write {}: {e}", dir.display()));
        }
    }
    prune(root, id, &dir);
    Ok(dir)
}

fn write_all(tmp: &Path, files: &BTreeMap<String, String>) -> Result<(), String> {
    std::fs::create_dir(tmp).map_err(|e| format!("can't create {}: {e}", tmp.display()))?;
    for (rel, text) in files {
        if !path_inside(rel) {
            return Err(format!("{rel:?} isn't a path inside the plugin"));
        }
        let path = tmp.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("can't create {}: {e}", parent.display()))?;
        }
        std::fs::write(&path, text).map_err(|e| format!("can't write {}: {e}", path.display()))?;
        set_readonly(&path, true);
    }
    let hash = format!("{:016x}\n", content_hash(files));
    std::fs::write(tmp.join(MARKER), hash).map_err(|e| format!("can't write {}: {e}", tmp.display()))
}

fn set_readonly(path: &Path, on: bool) {
    if let Ok(m) = std::fs::metadata(path) {
        let mut p = m.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        p.set_readonly(on);
        let _ = std::fs::set_permissions(path, p);
    }
}

/// Remove a folder of ours, files made writable first (Windows won't delete read-only files).
fn remove_tree(dir: &Path) {
    fn writable(dir: &Path, depth: usize) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() && depth < 16 => writable(&p, depth + 1),
                Ok(t) if t.is_file() => set_readonly(&p, false),
                _ => {}
            }
        }
    }
    writable(dir, 0);
    match std::fs::remove_dir_all(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => log::debug!("couldn't remove {}: {e}", dir.display()),
    }
}

/// Remove `id`'s other materialized folders (older versions or contents).
fn prune(root: &Path, id: &str, keep: &Path) {
    let Ok(rd) = std::fs::read_dir(root) else { return };
    let prefix = format!("{id}-");
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        // `<id>-<version>-<16 hex>`; another plugin's id may start with ours plus a dash, so
        // check the shape after the prefix (a version, a dash, the hash).
        let Some(rest) = name.strip_prefix(&prefix) else { continue };
        let ours = rest.rsplit_once('-').is_some_and(|(v, h)| h.len() == 16 && h.bytes().all(|b| b.is_ascii_hexdigit()) && !v.contains('-'));
        if ours && p != keep && p.join(MARKER).is_file() {
            remove_tree(&p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(text: &str) -> BTreeMap<String, String> {
        [("byob-plugin.json".to_owned(), "{}".to_owned()), ("lib/backend.py".to_owned(), text.to_owned())].into_iter().collect()
    }

    #[test]
    fn writes_once_read_only_and_keyed_by_version_and_content() {
        let root = tempfile::tempdir().unwrap();
        let a = materialize(root.path(), "sheets", "0.2.1", &files("print(1)")).unwrap();
        assert_eq!(std::fs::read_to_string(a.join("lib/backend.py")).unwrap(), "print(1)");
        assert!(std::fs::metadata(a.join("lib/backend.py")).unwrap().permissions().readonly());
        // Again: the same folder, untouched.
        let mtime = std::fs::metadata(a.join(MARKER)).unwrap().modified().unwrap();
        assert_eq!(materialize(root.path(), "sheets", "0.2.1", &files("print(1)")).unwrap(), a);
        assert_eq!(std::fs::metadata(a.join(MARKER)).unwrap().modified().unwrap(), mtime);
        // A new byob version or new contents: a new folder; the old one is pruned.
        let b = materialize(root.path(), "sheets", "0.3.0", &files("print(1)")).unwrap();
        assert_ne!(a, b);
        assert!(!a.exists(), "old folder pruned");
        let c = materialize(root.path(), "sheets", "0.3.0", &files("print(2)")).unwrap();
        assert_ne!(b, c);
        assert!(c.file_name().unwrap().to_string_lossy().starts_with("sheets-0.3.0-"));
        // Another plugin whose id starts with ours keeps its folder.
        let d = materialize(root.path(), "sheets-extra", "0.3.0", &files("x")).unwrap();
        let _ = materialize(root.path(), "sheets", "0.3.0", &files("print(3)")).unwrap();
        assert!(d.exists());
        // No temporary folders left behind.
        assert!(std::fs::read_dir(root.path()).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().starts_with(".tmp")));
    }

    #[test]
    fn refuses_paths_outside_and_bad_ids() {
        let root = tempfile::tempdir().unwrap();
        let mut bad = files("x");
        bad.insert("../evil.py".into(), "x".into());
        assert!(materialize(root.path(), "p", "1", &bad).is_err());
        assert!(!root.path().parent().unwrap().join("evil.py").exists());
        assert!(materialize(root.path(), "../p", "1", &files("x")).is_err());
        assert!(std::fs::read_dir(root.path()).unwrap().next().is_none(), "nothing left behind");
    }
}
