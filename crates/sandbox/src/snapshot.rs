//! Spot what a run did to a folder: a bounded before/after walk comparing sizes and
//! modification times.
//!
//! Bounded on purpose: at most [`MAX_FILES`] files and [`MAX_DEPTH`] folders deep; hidden
//! entries (`.git`, `.yah`, `.venv`, …), `node_modules` and `__pycache__` are skipped and
//! symlinks aren't followed. A change in a skipped place isn't reported; deletions are
//! only reported when both walks were complete.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub const MAX_FILES: usize = 20_000;
pub const MAX_DEPTH: usize = 8;
const SKIP: &[&str] = &["node_modules", "__pycache__"];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    files: BTreeMap<PathBuf, (u64, Option<SystemTime>)>,
    /// The walk saw everything (not cut off by the limits).
    pub complete: bool,
}

/// Files that appeared, changed or disappeared between two snapshots (absolute paths).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changes {
    pub created: Vec<PathBuf>,
    pub modified: Vec<PathBuf>,
    pub deleted: Vec<PathBuf>,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.created.is_empty() && self.modified.is_empty() && self.deleted.is_empty()
    }
}

impl Snapshot {
    pub fn take(root: &Path) -> Self {
        Self::take_bounded(root, MAX_FILES, MAX_DEPTH)
    }

    pub fn take_bounded(root: &Path, max_files: usize, max_depth: usize) -> Self {
        let mut s = Self { files: BTreeMap::new(), complete: true };
        let mut stack = vec![(root.to_path_buf(), 0usize)];
        while let Some((dir, depth)) = stack.pop() {
            let Ok(rd) = fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let name = e.file_name();
                let name = name.to_string_lossy();
                if name.starts_with('.') || SKIP.contains(&name.as_ref()) {
                    continue;
                }
                let Ok(meta) = e.path().symlink_metadata() else { continue };
                if meta.is_dir() {
                    if depth + 1 < max_depth {
                        stack.push((e.path(), depth + 1));
                    } else {
                        s.complete = false;
                    }
                } else if meta.is_file() {
                    if s.files.len() >= max_files {
                        s.complete = false;
                        return s;
                    }
                    s.files.insert(e.path(), (meta.len(), meta.modified().ok()));
                }
            }
        }
        s
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// What changed from `self` (before) to `after`.
    pub fn changes(&self, after: &Snapshot) -> Changes {
        let mut c = Changes::default();
        for (path, meta) in &after.files {
            match self.files.get(path) {
                None => c.created.push(path.clone()),
                Some(old) if old != meta => c.modified.push(path.clone()),
                Some(_) => {}
            }
        }
        if self.complete && after.complete {
            c.deleted = self.files.keys().filter(|p| !after.files.contains_key(*p)).cloned().collect();
        }
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_created_modified_deleted() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        fs::write(r.join("keep"), "a").unwrap();
        fs::write(r.join("change"), "a").unwrap();
        fs::write(r.join("gone"), "a").unwrap();
        fs::create_dir_all(r.join(".git")).unwrap();
        let before = Snapshot::take(r);
        assert_eq!(before.len(), 3);
        fs::write(r.join("change"), "abc").unwrap();
        fs::remove_file(r.join("gone")).unwrap();
        fs::create_dir_all(r.join("sub/deeper")).unwrap();
        fs::write(r.join("sub/deeper/new.png"), "x").unwrap();
        fs::write(r.join(".git/ignored"), "x").unwrap();
        let c = before.changes(&Snapshot::take(r));
        assert_eq!(c.created, vec![r.join("sub/deeper/new.png")]);
        assert_eq!(c.modified, vec![r.join("change")]);
        assert_eq!(c.deleted, vec![r.join("gone")]);
    }

    #[test]
    fn bounded_walk_is_incomplete_and_reports_no_deletions() {
        let d = tempfile::tempdir().unwrap();
        for i in 0..10 {
            fs::write(d.path().join(format!("f{i}")), "x").unwrap();
        }
        let s = Snapshot::take_bounded(d.path(), 5, 8);
        assert!(!s.complete && s.len() == 5);
        assert!(s.changes(&Snapshot::default()).deleted.is_empty());
        fs::create_dir_all(d.path().join("a/b/c")).unwrap();
        assert!(!Snapshot::take_bounded(d.path(), 100, 2).complete);
    }
}
