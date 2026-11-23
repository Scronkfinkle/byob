//! Workspace sandbox: turn an untrusted, model-supplied path into an absolute path that is
//! guaranteed to sit inside the workspace.
//!
//! Rules:
//! - Relative paths are taken relative to the workspace. Absolute paths are accepted only
//!   when they already point inside it (by its given or canonical spelling).
//! - `.` and `..` are resolved lexically first; climbing above the workspace is refused.
//! - Every *existing* directory on the way is checked: a symlink that resolves outside the
//!   workspace is refused. Components that don't exist yet can't be links, so a target that
//!   is about to be created is fine.
//! - The last component is not followed by [`resolve`]: callers that operate on the entry
//!   itself (write, move, delete) get the link, never its target. [`resolve_follow`] also
//!   follows the last component and checks where it lands (read, list).
//!
//! - A workspace (project) can link other folders ([`set_linked`]). An absolute path inside
//!   one of them resolves there, under the same rules with that folder as the root (a link
//!   out of it is refused too). Relative paths always mean the workspace's own folder.
//!
//! Races (someone swapping a directory for a symlink between the check and the use) are
//! out of scope: the model can't do that without an execute-level tool.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::sync::{PoisonError, RwLock};

/// The other folders each workspace links, by its canonical root (see [`set_linked`]).
static LINKED: RwLock<BTreeMap<PathBuf, Vec<PathBuf>>> = RwLock::new(BTreeMap::new());

/// Record the other folders `workspace` works in, replacing what was recorded. Each is
/// canonicalized; ones that don't exist, aren't folders, are a filesystem root, or are (or
/// hold) the workspace's own folder are left out. Empty forgets them.
pub fn set_linked(workspace: &Path, folders: &[PathBuf]) {
    let Ok(root) = workspace.canonicalize() else { return };
    let mut kept: Vec<PathBuf> = Vec::new();
    for f in folders {
        let Ok(c) = f.canonicalize() else { continue };
        if c.is_dir() && c.parent().is_some() && !root.starts_with(&c) && !kept.contains(&c) {
            kept.push(c);
        }
    }
    let mut map = LINKED.write().unwrap_or_else(PoisonError::into_inner);
    if kept.is_empty() {
        map.remove(&root);
    } else {
        map.insert(root, kept);
    }
}

/// The other folders `workspace` works in (canonical), as [`set_linked`] recorded them.
/// One that is no longer there as it was linked (gone, or swapped for a link to somewhere
/// else) is left out, as the workspace's own folder is canonicalized on every use.
pub fn linked(workspace: &Path) -> Vec<PathBuf> {
    let Ok(root) = workspace.canonicalize() else { return Vec::new() };
    let mut out = LINKED.read().unwrap_or_else(PoisonError::into_inner).get(&root).cloned().unwrap_or_default();
    out.retain(|f| f.canonicalize().is_ok_and(|c| c == *f && c.is_dir()));
    out
}

/// Is `root` a folder some workspace links (paths in it show in full)?
fn is_linked_root(root: &Path) -> bool {
    LINKED.read().unwrap_or_else(PoisonError::into_inner).values().any(|v| v.iter().any(|f| f == root))
}

/// Longest path we accept from the model, in bytes.
pub const MAX_PATH_LEN: usize = 4096;

/// Hidden folder for byob's own bookkeeping (trash, undo). File tools may not touch it,
/// except the parts that are the workspace's own extensions ([`EDITABLE_DIRS`]).
pub const RESERVED_DIR: &str = ".byob";

/// Folders inside [`RESERVED_DIR`] that file tools may read and write: the workspace's
/// plugins, subagents and skills (the model builds them with the ordinary file tools).
pub const EDITABLE_DIRS: [&str; 3] = ["plugins", "agents", "skills"];

/// A (workspace-relative) path as text with `/` separators on every platform — what the
/// model writes and what the protocol carries, so messages read the same on Windows.
pub fn slash(path: &Path) -> String {
    // Absolute paths (outside the workspace) keep their native form.
    if path.has_root() {
        return path.display().to_string();
    }
    let parts: Vec<String> = path.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    if parts.is_empty() { ".".to_owned() } else { parts.join("/") }
}

/// A path inside the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// Canonical workspace root.
    pub root: PathBuf,
    /// Absolute path; directories on the way are canonical, the last component is as given.
    pub abs: PathBuf,
}

impl Resolved {
    /// Path relative to the workspace, for messages. The root itself is `.`.
    pub fn rel(&self) -> PathBuf {
        match self.abs.strip_prefix(&self.root) {
            Ok(r) if r.as_os_str().is_empty() => PathBuf::from("."),
            Ok(r) => r.to_path_buf(),
            Err(_) => self.abs.clone(),
        }
    }

    /// Display form of [`Resolved::rel`], with `/` separators on every platform. A path in a
    /// linked folder shows in full (relative, it would read as the workspace's own).
    pub fn show(&self) -> String {
        if is_linked_root(&self.root) {
            return slash(&self.abs);
        }
        slash(&self.rel())
    }

    pub fn is_root(&self) -> bool {
        self.abs == self.root
    }

    /// True if this is byob's own `.byob` folder or inside it, apart from the folders in
    /// [`EDITABLE_DIRS`] and what they hold (`.byob/plugins/x/…` is fair game; `.byob`,
    /// `.byob/trash` and `.byob/undo` are not).
    pub fn is_reserved(&self) -> bool {
        let Ok(rel) = self.abs.strip_prefix(&self.root) else { return false };
        let mut parts = rel.components();
        if parts.next().is_none_or(|c| c.as_os_str() != RESERVED_DIR) {
            return false;
        }
        !parts.next().is_some_and(|c| EDITABLE_DIRS.iter().any(|d| c.as_os_str() == *d))
    }
}

/// Canonical workspace root, with a model-readable error.
pub fn root(workspace: &Path) -> Result<PathBuf, String> {
    let root = workspace.canonicalize().map_err(|e| format!("the workspace folder is not available: {e}"))?;
    if !root.is_dir() {
        return Err("the workspace is not a folder".into());
    }
    Ok(root)
}

/// Which root `input` is in and its components there: the workspace's own folder, else
/// (for an absolute path outside it) the linked folder that holds it.
fn locate(workspace: &Path, input: &str) -> Result<(PathBuf, Vec<OsString>), String> {
    let root = root(workspace)?;
    match normalize(workspace, &root, input) {
        Ok(parts) => Ok((root, parts)),
        Err(e) => {
            let trimmed = input.trim();
            if !Path::new(trimmed).has_root() || trimmed.len() > MAX_PATH_LEN || trimmed.contains('\0') {
                return Err(e);
            }
            for other in linked(&root) {
                if Path::new(trimmed).starts_with(&other)
                    && let Ok(parts) = normalize(&other, &other, input)
                {
                    return Ok((other, parts));
                }
            }
            Err(e)
        }
    }
}

/// Resolve `input` inside `workspace` (or a folder it links) without following its last
/// component.
pub fn resolve(workspace: &Path, input: &str) -> Result<Resolved, String> {
    let (root, parts) = locate(workspace, input)?;
    let Some((name, dirs)) = parts.split_last() else {
        return Ok(Resolved { abs: root.clone(), root });
    };
    let mut cur = root.clone();
    let mut missing = false;
    for part in dirs {
        let next = cur.join(part);
        if missing {
            cur = next;
            continue;
        }
        match std::fs::symlink_metadata(&next) {
            Ok(m) if m.file_type().is_symlink() => {
                let target = next.canonicalize().map_err(|_| format!("{} is a broken link", show_input(input)))?;
                if !target.starts_with(&root) {
                    return Err(outside(input));
                }
                cur = target;
            }
            Ok(_) => cur = next,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                missing = true;
                cur = next;
            }
            Err(e) => return Err(format!("can't access {}: {e}", show_input(input))),
        }
    }
    Ok(Resolved { abs: cur.join(name), root })
}

/// Resolve `input` and, if it exists, follow it to its real location, which must also be
/// inside the workspace.
pub fn resolve_follow(workspace: &Path, input: &str) -> Result<Resolved, String> {
    let r = resolve(workspace, input)?;
    match std::fs::symlink_metadata(&r.abs) {
        Ok(m) if m.file_type().is_symlink() => {
            let target = r.abs.canonicalize().map_err(|_| format!("{} is a broken link", show_input(input)))?;
            if !target.starts_with(&r.root) {
                return Err(outside(input));
            }
            Ok(Resolved { abs: target, root: r.root })
        }
        _ => Ok(r),
    }
}

/// Lexically normalized components of `input` relative to the workspace root.
fn normalize(workspace: &Path, root: &Path, input: &str) -> Result<Vec<OsString>, String> {
    if input.len() > MAX_PATH_LEN {
        return Err(format!("path is too long ({} bytes; max {MAX_PATH_LEN})", input.len()));
    }
    if input.contains('\0') {
        return Err("path contains a NUL character".into());
    }
    let trimmed = input.trim();
    if trimmed == "~" || trimmed.starts_with("~/") || trimmed.starts_with("$HOME") || trimmed.starts_with("${HOME}") {
        return Err(outside(input));
    }
    let path = Path::new(trimmed);
    let rel: &Path = if path.has_root() {
        match path.strip_prefix(root).or_else(|_| path.strip_prefix(workspace)) {
            Ok(r) => r,
            Err(_) => return Err(outside(input)),
        }
    } else {
        path
    };
    let mut out: Vec<OsString> = Vec::new();
    for c in rel.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if out.pop().is_none() {
                    return Err(outside(input));
                }
            }
            Component::Normal(s) => out.push(s.to_os_string()),
            Component::RootDir | Component::Prefix(_) => return Err(outside(input)),
        }
    }
    Ok(out)
}

fn show_input(input: &str) -> String {
    let mut s: String = input.chars().filter(|c| !c.is_control()).take(120).collect();
    if input.chars().count() > 120 {
        s.push('…');
    }
    format!("'{s}'")
}

fn outside(input: &str) -> String {
    format!("{} is outside the workspace; use a path relative to the workspace folder", show_input(input))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn slash_uses_forward_slashes_for_relative_paths() {
        assert_eq!(slash(&Path::new("a").join("b").join("c.txt")), "a/b/c.txt");
        assert_eq!(slash(Path::new("")), ".");
        let abs = std::env::temp_dir();
        assert_eq!(slash(&abs), abs.display().to_string());
    }

    #[test]
    fn relative_and_dot_paths() {
        let d = ws();
        let root = d.path().canonicalize().unwrap();
        assert_eq!(resolve(d.path(), "a/b.txt").unwrap().abs, root.join("a/b.txt"));
        assert_eq!(resolve(d.path(), "./a/../b").unwrap().abs, root.join("b"));
        assert!(resolve(d.path(), "").unwrap().is_root());
        assert!(resolve(d.path(), ".").unwrap().is_root());
        assert!(resolve(d.path(), "a/..").unwrap().is_root());
        assert_eq!(resolve(d.path(), "a/b").unwrap().show(), "a/b");
        assert_eq!(resolve(d.path(), ".").unwrap().show(), ".");
    }

    /// A workspace's linked folders: absolute paths inside them resolve there under the same
    /// rules; relative paths still mean the workspace; nothing else opens up.
    #[cfg(unix)]
    #[test]
    fn linked_folders_open_only_themselves() {
        let home = ws();
        let other = ws();
        let elsewhere = ws();
        let other_root = other.path().canonicalize().unwrap();
        std::fs::create_dir(other_root.join("src")).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), other_root.join("out")).unwrap();
        let inside = other_root.join("src/main.rs");
        // Not linked yet: outside.
        assert!(resolve(home.path(), inside.to_str().unwrap()).is_err());
        set_linked(home.path(), &[other.path().to_path_buf()]);
        let r = resolve(home.path(), inside.to_str().unwrap()).unwrap();
        assert_eq!((r.abs.clone(), r.root.clone()), (inside.clone(), other_root.clone()));
        assert_eq!(r.show(), inside.display().to_string(), "shown in full, not as if it were the workspace's");
        assert_eq!(linked(home.path()), std::slice::from_ref(&other_root));
        // Relative paths are still the workspace's own.
        assert_eq!(resolve(home.path(), "src/main.rs").unwrap().root, home.path().canonicalize().unwrap());
        // Escapes from the linked folder are refused like escapes from the workspace.
        for bad in [format!("{}/../x", other_root.display()), format!("{}/out/x", other_root.display()), format!("{}-evil/x", other_root.display())] {
            assert!(resolve(home.path(), &bad).is_err(), "{bad} should be refused");
        }
        assert!(resolve_follow(home.path(), &format!("{}/out", other_root.display())).is_err(), "a link out of it");
        // A filesystem root, the workspace itself or a folder holding it is never linked.
        set_linked(home.path(), &[PathBuf::from("/"), home.path().to_path_buf(), home.path().parent().unwrap().to_path_buf(), other.path().to_path_buf()]);
        assert_eq!(linked(home.path()), std::slice::from_ref(&other_root));
        assert!(resolve(home.path(), "/etc/passwd").is_err());
        // Unlinked: closed again.
        set_linked(home.path(), &[]);
        assert!(linked(home.path()).is_empty());
        assert!(resolve(home.path(), inside.to_str().unwrap()).is_err());
    }

    /// Regression: a linked folder replaced by a link after it was linked (moved away, a
    /// link to somewhere else left in its place) opened that somewhere else to the file tools
    /// and to commands. Linked folders that no longer are what was linked are left out.
    #[cfg(unix)]
    #[test]
    fn a_linked_folder_swapped_for_a_link_is_closed() {
        let home = ws();
        let parent = ws();
        let secret = ws();
        std::fs::write(secret.path().join("key"), "s3cret").unwrap();
        let app = parent.path().canonicalize().unwrap().join("app");
        std::fs::create_dir(&app).unwrap();
        set_linked(home.path(), std::slice::from_ref(&app));
        assert!(resolve(home.path(), &format!("{}/key", app.display())).is_ok());
        std::fs::rename(&app, parent.path().join("app-moved")).unwrap();
        std::os::unix::fs::symlink(secret.path(), &app).unwrap();
        assert!(linked(home.path()).is_empty(), "no longer the folder that was linked");
        for p in [format!("{}/key", app.display()), format!("{}/sub/key", app.display())] {
            assert!(resolve(home.path(), &p).is_err(), "{p} should be refused");
            assert!(resolve_follow(home.path(), &p).is_err(), "{p} should be refused");
        }
        // Back in place: open again.
        std::fs::remove_file(&app).unwrap();
        std::fs::rename(parent.path().join("app-moved"), &app).unwrap();
        assert_eq!(linked(home.path()), std::slice::from_ref(&app));
        set_linked(home.path(), &[]);
    }

    #[test]
    fn rejects_escapes() {
        let d = ws();
        for p in ["..", "../x", "a/../../x", "/etc/passwd", "~", "~/x", "$HOME/x", "a\0b", "/"] {
            assert!(resolve(d.path(), p).is_err(), "{p:?} should be refused");
        }
        assert!(resolve(d.path(), &"a/".repeat(5000)).is_err());
    }

    #[test]
    fn absolute_inside_is_fine() {
        let d = ws();
        let root = d.path().canonicalize().unwrap();
        let p = root.join("x.txt");
        assert_eq!(resolve(d.path(), p.to_str().unwrap()).unwrap().abs, p);
        let raw = d.path().join("y.txt");
        assert_eq!(resolve(d.path(), raw.to_str().unwrap()).unwrap().abs, root.join("y.txt"));
        // a sibling folder whose name starts with the workspace's
        let sib = format!("{}-evil/x", root.display());
        assert!(resolve(d.path(), &sib).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_out_are_refused() {
        let out = ws();
        let d = ws();
        std::os::unix::fs::symlink(out.path(), d.path().join("out")).unwrap();
        std::os::unix::fs::symlink(out.path().join("f"), d.path().join("lf")).unwrap();
        std::fs::create_dir(d.path().join("in")).unwrap();
        std::os::unix::fs::symlink(d.path().join("in"), d.path().join("ok")).unwrap();
        assert!(resolve(d.path(), "out/x").is_err());
        assert!(resolve(d.path(), "out/new/deeper").is_err());
        // the link itself may be named (to delete or move it), but not followed
        assert!(resolve(d.path(), "out").is_ok());
        assert!(resolve_follow(d.path(), "out").is_err());
        assert!(resolve_follow(d.path(), "lf").is_err());
        let root = d.path().canonicalize().unwrap();
        assert_eq!(resolve(d.path(), "ok/x").unwrap().abs, root.join("in/x"));
        assert_eq!(resolve_follow(d.path(), "ok").unwrap().abs, root.join("in"));
    }

    #[test]
    fn reserved_dir() {
        let d = ws();
        assert!(resolve(d.path(), ".byob/trash/x").unwrap().is_reserved());
        assert!(resolve(d.path(), ".byob").unwrap().is_reserved());
        assert!(!resolve(d.path(), "a/.byob").unwrap().is_reserved());
        assert!(!resolve(d.path(), ".").unwrap().is_reserved());
        assert!(resolve(d.path(), ".byob/undo/t1/x").unwrap().is_reserved());
        assert!(resolve(d.path(), ".byob/pluginsx/a").unwrap().is_reserved());
        for p in [".byob/plugins", ".byob/plugins/weather/byob-plugin.json", ".byob/agents/explorer.md", ".byob/skills/plan/SKILL.md"] {
            assert!(!resolve(d.path(), p).unwrap().is_reserved(), "{p}");
        }
    }

    #[test]
    fn missing_workspace() {
        assert!(resolve(Path::new("/definitely/not/here/byob"), "x").is_err());
    }
}
