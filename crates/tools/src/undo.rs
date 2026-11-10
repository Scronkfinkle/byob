//! The undo journal: what a turn's tools changed, recorded so the changes can be reversed.
//!
//! Tools that change files record an [`UndoOp`] per change through
//! [`ToolContext::journal`](crate::ToolContext::journal), stashing the previous content of a
//! file they overwrite in the journal's backup folder (`.byob/undo/<turn>/`) first. [`undo`]
//! walks a turn's ops backwards. It first checks the whole plan against the disk and refuses,
//! changing nothing, if any file changed since; then applies it, re-checking every step.
//!
//! [`UndoOp`] has the same JSON shape as `byob_store::UndoOp` (plus optional fingerprints,
//! which that type ignores), so the journal can be persisted as is.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use crate::sandbox;

/// Where backups go, relative to the workspace.
pub const UNDO_DIR: &str = ".byob/undo";
/// Files larger than this are not backed up (the change is then not undoable).
pub const MAX_BACKUP_BYTES: u64 = 64 * 1024 * 1024;

/// Identifies a file's content: length plus a 64-bit FNV-1a hash. Stable across builds,
/// so it can be persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Fingerprint {
    pub len: u64,
    pub hash: u64,
}

impl Fingerprint {
    pub fn of_bytes(bytes: &[u8]) -> Self {
        let mut h = Fnv::default();
        h.write(bytes);
        Self { len: bytes.len() as u64, hash: h.0 }
    }

    /// Fingerprint of a regular file (not following a final symlink). `None` for anything
    /// else (missing, folder, link, unreadable).
    pub fn of_file(path: &Path) -> Option<Self> {
        let meta = std::fs::symlink_metadata(path).ok()?;
        if !meta.is_file() {
            return None;
        }
        let mut f = File::open(path).ok()?;
        let mut h = Fnv::default();
        let mut buf = vec![0u8; 64 * 1024];
        let mut len = 0u64;
        loop {
            match f.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    h.write(buf.get(..n).unwrap_or_default());
                    len = len.saturating_add(n as u64);
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => return None,
            }
        }
        Some(Self { len, hash: h.0 })
    }
}

struct Fnv(u64);

impl Default for Fnv {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl Fnv {
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
}

/// One reversible file-system change. Paths are absolute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UndoOp {
    /// A new file; undo deletes it if it still has content `fingerprint`.
    Created {
        path: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fingerprint: Option<Fingerprint>,
    },
    /// An overwritten file; `backup` holds the previous content. Undo restores it if the
    /// file still has content `fingerprint`.
    Replaced {
        path: PathBuf,
        backup: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fingerprint: Option<Fingerprint>,
    },
    Moved {
        from: PathBuf,
        to: PathBuf,
    },
    /// Moved to the trash at `backup`; `None` when it could not be kept.
    Deleted {
        path: PathBuf,
        backup: Option<PathBuf>,
    },
}

/// A turn's journal: ops in order, and the folder backups go to. Shared by the turn's
/// tool calls through [`ToolContext`](crate::ToolContext).
#[derive(Debug)]
pub struct Journal {
    /// Absolute backup folder, created on first use.
    dir: PathBuf,
    ops: Mutex<Vec<UndoOp>>,
    seq: AtomicU64,
}

impl Journal {
    /// A journal whose backups go to `<workspace>/.byob/undo/<name>/`. `name` must be a
    /// single plain path component (e.g. `"12-1731000000000"`).
    pub fn new(workspace: &Path, name: &str) -> Self {
        let name: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')).take(64).collect();
        let name = if name.is_empty() { "turn".to_owned() } else { name };
        Self { dir: workspace.join(UNDO_DIR).join(name), ops: Mutex::new(Vec::new()), seq: AtomicU64::new(0) }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn record(&self, op: UndoOp) {
        self.ops.lock().unwrap_or_else(PoisonError::into_inner).push(op);
    }

    /// Take the ops recorded so far.
    pub fn take(&self) -> Vec<UndoOp> {
        std::mem::take(&mut *self.ops.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Copy the regular file at `path` into the backup folder; returns the copy's path.
    pub fn backup(&self, workspace: &Path, path: &Path) -> Result<PathBuf, String> {
        let meta = std::fs::symlink_metadata(path).map_err(|e| format!("can't read it: {e}"))?;
        if !meta.is_file() {
            return Err("it is not a regular file".into());
        }
        if meta.len() > MAX_BACKUP_BYTES {
            return Err("it is too large to keep a copy".into());
        }
        // the backup folder is checked like any other path (no links out of the workspace)
        let rel = self.dir.strip_prefix(workspace).map_err(|_| "the undo folder is outside the workspace".to_owned())?;
        let dir = sandbox::resolve(workspace, &rel.to_string_lossy()).map_err(|e| format!("the undo folder is not usable: {e}"))?;
        std::fs::create_dir_all(&dir.abs).map_err(|e| format!("can't create the undo folder: {e}"))?;
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let dest = dir.abs.join(format!("{n}-{name}"));
        std::fs::copy(path, &dest).map_err(|e| format!("can't copy it: {e}"))?;
        Ok(dest)
    }
}

/// How an [`undo`] went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoReport {
    /// One line per reversed change, in the order they were undone.
    pub done: Vec<String>,
}

/// What the plan believes is at a path while simulating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    Absent,
    /// A regular file with this content.
    File(Fingerprint),
    /// Something else (a folder, a link) or unreadable.
    Other,
}

fn probe(p: &Path) -> Slot {
    match std::fs::symlink_metadata(p) {
        Err(_) => Slot::Absent,
        Ok(m) if m.is_file() => Fingerprint::of_file(p).map_or(Slot::Other, Slot::File),
        Ok(_) => Slot::Other,
    }
}

/// Paths in a journal may come from storage: accept only plain absolute paths inside the
/// workspace (no `..`), and show them relative to it.
fn check_path(ws: &Path, p: &Path) -> Result<String, String> {
    let rel = p.strip_prefix(ws).map_err(|_| format!("{} is outside the workspace", p.display()))?;
    if rel.as_os_str().is_empty() || rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(format!("{} is not a usable path", p.display()));
    }
    // no links on the way that lead out of the workspace
    sandbox::resolve(ws, &rel.to_string_lossy())?;
    Ok(rel.display().to_string())
}

struct Sim<'a> {
    ws: &'a Path,
    slots: BTreeMap<PathBuf, Slot>,
}

impl Sim<'_> {
    fn get(&mut self, p: &Path) -> Slot {
        *self.slots.entry(p.to_path_buf()).or_insert_with(|| probe(p))
    }

    /// Check one op against the simulated state and apply it there.
    fn step(&mut self, op: &UndoOp) -> Result<(), String> {
        match op {
            UndoOp::Created { path, fingerprint } => {
                let name = check_path(self.ws, path)?;
                match (self.get(path), fingerprint) {
                    (Slot::Absent, _) => return Err(format!("{name} is already gone")),
                    (Slot::File(now), Some(fp)) if now == *fp => {}
                    _ => return Err(format!("{name} changed since; not deleting it")),
                }
                self.slots.insert(path.clone(), Slot::Absent);
            }
            UndoOp::Replaced { path, backup, fingerprint } => {
                let name = check_path(self.ws, path)?;
                check_path(self.ws, backup)?;
                match (self.get(path), fingerprint) {
                    (Slot::File(now), Some(fp)) if now == *fp => {}
                    (Slot::Absent, _) => return Err(format!("{name} is gone since; not restoring the old version")),
                    _ => return Err(format!("{name} changed since; not restoring the old version")),
                }
                let Slot::File(old) = self.get(backup) else {
                    return Err(format!("the saved copy of {name} is missing"));
                };
                self.slots.insert(path.clone(), Slot::File(old));
            }
            UndoOp::Moved { from, to } => {
                let from_name = check_path(self.ws, from)?;
                let to_name = check_path(self.ws, to)?;
                let moved = self.get(to);
                if moved == Slot::Absent {
                    return Err(format!("{to_name} is gone since; can't move it back"));
                }
                if self.get(from) != Slot::Absent {
                    return Err(format!("{from_name} exists again; not moving {to_name} back over it"));
                }
                self.slots.insert(from.clone(), moved);
                self.slots.insert(to.clone(), Slot::Absent);
            }
            UndoOp::Deleted { path, backup } => {
                let name = check_path(self.ws, path)?;
                let Some(backup) = backup else { return Err(format!("{name} was not kept in the trash")) };
                check_path(self.ws, backup)?;
                if self.get(path) != Slot::Absent {
                    return Err(format!("{name} exists again; not restoring it from the trash"));
                }
                let kept = self.get(backup);
                if kept == Slot::Absent {
                    return Err(format!("{name} is no longer in the trash"));
                }
                self.slots.insert(path.clone(), kept);
                self.slots.insert(backup.clone(), Slot::Absent);
            }
        }
        Ok(())
    }
}

/// Check that `ops` (oldest first) can be reversed in `workspace` right now.
pub fn check(ops: &[UndoOp], workspace: &Path) -> Result<(), String> {
    let root = sandbox::root(workspace)?;
    let mut sim = Sim { ws: &root, slots: BTreeMap::new() };
    ops.iter().rev().try_for_each(|op| sim.step(op))
}

fn make_parent(p: &Path) -> Result<(), String> {
    match p.parent() {
        Some(parent) => std::fs::create_dir_all(parent).map_err(|e| format!("can't create {}: {e}", parent.display())),
        None => Ok(()),
    }
}

/// Reverse `ops` (oldest first) in `workspace`, newest first. Refuses, changing nothing,
/// if any file changed since; stops at the first failure while applying (the disk changed
/// under us between the check and the change).
pub fn undo(ops: &[UndoOp], workspace: &Path) -> Result<UndoReport, String> {
    let root = sandbox::root(workspace)?;
    let workspace = root.as_path();
    check(ops, workspace)?;
    let mut done = Vec::new();
    for op in ops.iter().rev() {
        // re-check each op against the real disk just before applying it
        let mut sim = Sim { ws: workspace, slots: BTreeMap::new() };
        sim.step(op).map_err(|e| partial(&done, e))?;
        let show = |p: &Path| p.strip_prefix(workspace).unwrap_or(p).display().to_string();
        let line = match op {
            UndoOp::Created { path, .. } => {
                std::fs::remove_file(path).map_err(|e| partial(&done, format!("can't delete {}: {e}", show(path))))?;
                format!("deleted {}", show(path))
            }
            UndoOp::Replaced { path, backup, .. } => {
                // copy over the file rather than renaming, so its permissions stay
                std::fs::copy(backup, path).map_err(|e| partial(&done, format!("can't restore {}: {e}", show(path))))?;
                let _ = std::fs::remove_file(backup);
                format!("restored the previous {}", show(path))
            }
            UndoOp::Moved { from, to } => {
                make_parent(from).map_err(|e| partial(&done, e))?;
                std::fs::rename(to, from).map_err(|e| partial(&done, format!("can't move {} back: {e}", show(to))))?;
                format!("moved {} back to {}", show(to), show(from))
            }
            UndoOp::Deleted { path, backup } => {
                let Some(backup) = backup else { return Err(partial(&done, format!("{} was not kept", show(path)))) };
                make_parent(path).map_err(|e| partial(&done, e))?;
                std::fs::rename(backup, path).map_err(|e| partial(&done, format!("can't restore {}: {e}", show(path))))?;
                format!("restored {} from the trash", show(path))
            }
        };
        done.push(line);
    }
    Ok(UndoReport { done })
}

fn partial(done: &[String], e: String) -> String {
    if done.is_empty() { e } else { format!("{e} (after undoing {} change(s))", done.len()) }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;

    use super::*;
    use crate::fs::testutil::run_journaled;
    use crate::{DeleteFile, MoveFile, MoveFiles, WriteFile};

    fn ws() -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().canonicalize().unwrap();
        (d, root)
    }

    #[test]
    fn fingerprints_are_stable() {
        // FNV-1a test vectors
        assert_eq!(Fingerprint::of_bytes(b"").hash, 0xcbf2_9ce4_8422_2325);
        assert_eq!(Fingerprint::of_bytes(b"a").hash, 0xaf63_dc4c_8601_ec8c);
        let (_d, root) = ws();
        fs::write(root.join("f"), "hello").unwrap();
        assert_eq!(Fingerprint::of_file(&root.join("f")), Some(Fingerprint::of_bytes(b"hello")));
        assert_eq!(Fingerprint::of_file(&root), None);
        assert_eq!(Fingerprint::of_file(&root.join("nope")), None);
    }

    #[test]
    fn json_shape_matches_the_store() {
        let op = UndoOp::Deleted { path: "/w/a".into(), backup: Some("/w/.byob/trash/1/a".into()) };
        assert_eq!(serde_json::to_value(&op).unwrap(), json!({"type": "deleted", "path": "/w/a", "backup": "/w/.byob/trash/1/a"}));
        let op = UndoOp::Created { path: "/w/a".into(), fingerprint: None };
        assert_eq!(serde_json::to_value(&op).unwrap(), json!({"type": "created", "path": "/w/a"}));
        let back: UndoOp = serde_json::from_value(json!({"type": "replaced", "path": "/w/a", "backup": "/w/b"})).unwrap();
        assert_eq!(back, UndoOp::Replaced { path: "/w/a".into(), backup: "/w/b".into(), fingerprint: None });
    }

    #[test]
    fn undo_created() {
        let (_d, root) = ws();
        let j = Journal::new(&root, "t1");
        assert!(!run_journaled(&WriteFile, &root, &j, json!({"path": "new/a.txt", "content": "hi"})).failed);
        let ops = j.take();
        assert!(matches!(&ops[..], [UndoOp::Created { fingerprint: Some(_), .. }]));
        let report = undo(&ops, &root).unwrap();
        assert_eq!(report.done, ["deleted new/a.txt"]);
        assert!(!root.join("new/a.txt").exists());
    }

    #[test]
    fn undo_created_refuses_when_edited_since() {
        let (_d, root) = ws();
        let j = Journal::new(&root, "t1");
        run_journaled(&WriteFile, &root, &j, json!({"path": "a.txt", "content": "hi"}));
        fs::write(root.join("a.txt"), "the user's edit").unwrap();
        let err = undo(&j.take(), &root).unwrap_err();
        assert!(err.contains("changed since"), "{err}");
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "the user's edit");
    }

    #[test]
    fn undo_replaced() {
        let (_d, root) = ws();
        fs::write(root.join("a.txt"), "old").unwrap();
        let j = Journal::new(&root, "t2");
        run_journaled(&WriteFile, &root, &j, json!({"path": "a.txt", "content": "new"}));
        run_journaled(&WriteFile, &root, &j, json!({"path": "a.txt", "content": "newer"}));
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "newer");
        let ops = j.take();
        assert_eq!(ops.len(), 2);
        // backups live under .byob/undo/<turn>/
        let UndoOp::Replaced { backup, .. } = &ops[0] else { panic!("{ops:?}") };
        assert!(backup.starts_with(root.join(".byob/undo/t2")), "{}", backup.display());
        undo(&ops, &root).unwrap();
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "old");
    }

    #[test]
    fn undo_replaced_refuses_when_edited_since_and_changes_nothing() {
        let (_d, root) = ws();
        fs::write(root.join("a.txt"), "old").unwrap();
        let j = Journal::new(&root, "t");
        run_journaled(&WriteFile, &root, &j, json!({"path": "b.txt", "content": "made"}));
        run_journaled(&WriteFile, &root, &j, json!({"path": "a.txt", "content": "new"}));
        fs::write(root.join("a.txt"), "user").unwrap();
        let ops = j.take();
        assert!(undo(&ops, &root).unwrap_err().contains("a.txt changed since"));
        // nothing was undone, not even the unrelated new file
        assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "made");
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "user");
    }

    #[test]
    fn undo_deleted_and_conflict() {
        let (_d, root) = ws();
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(root.join("docs/a.txt"), "keep").unwrap();
        let j = Journal::new(&root, "t");
        assert!(!run_journaled(&DeleteFile::default(), &root, &j, json!({"path": "docs"})).failed);
        let ops = j.take();
        assert!(matches!(&ops[..], [UndoOp::Deleted { backup: Some(_), .. }]));
        // something new took its place: refuse
        fs::create_dir(root.join("docs")).unwrap();
        assert!(undo(&ops, &root).unwrap_err().contains("exists again"));
        fs::remove_dir(root.join("docs")).unwrap();
        assert_eq!(undo(&ops, &root).unwrap().done, ["restored docs from the trash"]);
        assert_eq!(fs::read_to_string(root.join("docs/a.txt")).unwrap(), "keep");
        // a second undo finds nothing to restore
        assert!(undo(&ops, &root).is_err());
    }

    #[test]
    fn undo_moves_in_order() {
        let (_d, root) = ws();
        fs::write(root.join("a"), "A").unwrap();
        fs::write(root.join("b"), "B").unwrap();
        let j = Journal::new(&root, "t");
        // write, move, then write again at the new name: undo must walk back in reverse
        run_journaled(&WriteFile, &root, &j, json!({"path": "a", "content": "A2"}));
        assert!(!run_journaled(&MoveFile, &root, &j, json!({"from": "a", "to": "sub/c"})).failed);
        assert!(!run_journaled(&MoveFiles, &root, &j, json!({"moves": [{"from": "b", "to": "d"}]})).failed);
        run_journaled(&WriteFile, &root, &j, json!({"path": "sub/c", "content": "A3"}));
        let ops = j.take();
        assert_eq!(ops.len(), 4);
        undo(&ops, &root).unwrap();
        assert_eq!(fs::read_to_string(root.join("a")).unwrap(), "A");
        assert_eq!(fs::read_to_string(root.join("b")).unwrap(), "B");
        assert!(!root.join("sub/c").exists() && !root.join("d").exists());
    }

    #[test]
    fn undo_move_conflict() {
        let (_d, root) = ws();
        fs::write(root.join("a"), "A").unwrap();
        let j = Journal::new(&root, "t");
        run_journaled(&MoveFile, &root, &j, json!({"from": "a", "to": "b"}));
        let ops = j.take();
        fs::write(root.join("a"), "new a").unwrap();
        assert!(undo(&ops, &root).unwrap_err().contains("exists again"));
        fs::remove_file(root.join("a")).unwrap();
        fs::remove_file(root.join("b")).unwrap();
        assert!(undo(&ops, &root).unwrap_err().contains("gone since"));
    }

    #[test]
    fn hostile_journal_paths_are_refused() {
        let (_d, root) = ws();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("victim"), "safe").unwrap();
        let fp = Some(Fingerprint::of_bytes(b"safe"));
        for op in [
            UndoOp::Created { path: outside.path().join("victim"), fingerprint: fp },
            UndoOp::Created { path: root.join("../victim"), fingerprint: fp },
            UndoOp::Replaced { path: root.join("x"), backup: outside.path().join("victim"), fingerprint: fp },
            UndoOp::Created { path: root.clone(), fingerprint: fp },
        ] {
            assert!(undo(std::slice::from_ref(&op), &root).is_err(), "{op:?}");
        }
        assert!(outside.path().join("victim").exists());
    }

    #[test]
    fn journal_names_are_sanitized() {
        let j = Journal::new(Path::new("/w"), "../../etc");
        assert_eq!(j.dir(), Path::new("/w/.byob/undo/etc"));
        assert_eq!(Journal::new(Path::new("/w"), "").dir(), Path::new("/w/.byob/undo/turn"));
    }
}
