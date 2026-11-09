//! `delete_file`: move a file or folder to the workspace trash instead of erasing it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use super::{arg_str, lexists, req_string, resolve_mut, short_name};
use crate::{Access, FileChange, Tool, ToolContext, ToolOutput, ToolSpec, sandbox};

/// Default trash folder, relative to the workspace.
pub const DEFAULT_TRASH_DIR: &str = ".yah/trash";

/// Deleted items go to `<workspace>/<trash_dir>/<id>/<original relative path>`, so they can
/// be restored by moving them back. `trash_dir` must be relative and inside the workspace.
pub struct DeleteFile {
    trash_dir: PathBuf,
}

impl DeleteFile {
    pub fn new(trash_dir: impl Into<PathBuf>) -> Self {
        Self { trash_dir: trash_dir.into() }
    }

    pub fn trash_dir(&self) -> &Path {
        &self.trash_dir
    }
}

impl Default for DeleteFile {
    fn default() -> Self {
        Self::new(DEFAULT_TRASH_DIR)
    }
}

impl Tool for DeleteFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "delete_file".into(),
            description: "Delete a file or folder in the workspace. It goes to the trash, so it can be restored.".into(),
            parameters: json!({
                "type": "object",
                "properties": {"path": {"type": "string", "description": "File or folder path, relative to the workspace"}},
                "required": ["path"]
            }),
        }
    }

    fn title(&self, args: &Value) -> String {
        format!("Deleted {}", short_name(arg_str(args, "path").unwrap_or("a file")))
    }

    fn access(&self, _args: &Value) -> Access {
        Access::Write
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        match self.delete(args, ctx) {
            Ok(out) => out,
            Err(e) => ToolOutput::err(e),
        }
    }
}

/// Unique per process and call, sortable by time.
fn trash_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    format!("{ms}-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed))
}

impl DeleteFile {
    fn delete(&self, args: &Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, String> {
        let path = req_string(args, "path")?;
        let target = resolve_mut(ctx.workspace, &path)?;
        let rel = target.rel();
        if !lexists(&target.abs) {
            return Err(format!("{} doesn't exist; use list_files to see what's there", target.show()));
        }
        // every folder on the way to the trash is checked like any other path
        let dest = sandbox::resolve(ctx.workspace, &self.trash_dir.join(trash_id()).join(&rel).to_string_lossy())
            .map_err(|e| format!("the trash folder is not usable: {e}"))?;
        if dest.abs.starts_with(&target.abs) {
            return Err(format!("can't delete {}: it contains the trash folder", target.show()));
        }
        if let Some(parent) = dest.abs.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("can't create the trash folder: {e}"))?;
        }
        std::fs::rename(&target.abs, &dest.abs).map_err(|e| format!("can't delete {}: {e}", target.show()))?;
        Ok(ToolOutput {
            text: format!("Deleted {} (moved to {}; move it back to restore)", target.show(), dest.show()),
            failed: false,
            changes: vec![FileChange::Deleted(target.abs)],
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::fs::testutil::run;

    #[test]
    fn moves_to_trash() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("docs/sub")).unwrap();
        fs::write(root.join("docs/sub/a.txt"), "keep me").unwrap();
        let out = run(&DeleteFile::default(), d.path(), json!({"path": "docs/sub/a.txt"}));
        assert!(!out.failed, "{}", out.text);
        assert_eq!(out.changes, vec![FileChange::Deleted(root.join("docs/sub/a.txt"))]);
        assert!(!root.join("docs/sub/a.txt").exists());
        let batches: Vec<_> = fs::read_dir(root.join(".yah/trash")).unwrap().collect();
        assert_eq!(batches.len(), 1);
        let saved = batches[0].as_ref().unwrap().path().join("docs/sub/a.txt");
        assert_eq!(fs::read_to_string(saved).unwrap(), "keep me");
        // folders too, and twice the same name doesn't collide
        fs::write(root.join("docs/sub/a.txt"), "again").unwrap();
        assert!(!run(&DeleteFile::default(), d.path(), json!({"path": "docs/sub/a.txt"})).failed);
        assert!(!run(&DeleteFile::default(), d.path(), json!({"path": "docs"})).failed);
        assert!(!root.join("docs").exists());
        assert_eq!(fs::read_dir(root.join(".yah/trash")).unwrap().count(), 3);
    }

    #[test]
    fn custom_trash_dir() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("x"), "").unwrap();
        let tool = DeleteFile::new("bin");
        assert!(!run(&tool, d.path(), json!({"path": "x"})).failed);
        assert_eq!(fs::read_dir(d.path().join("bin")).unwrap().count(), 1);
        // the trash folder itself can't be deleted into itself
        assert!(run(&tool, d.path(), json!({"path": "bin"})).failed);
        // a trash outside the workspace is refused
        fs::write(d.path().join("y"), "").unwrap();
        assert!(run(&DeleteFile::new("/tmp"), d.path(), json!({"path": "y"})).failed);
        assert!(run(&DeleteFile::new("../t"), d.path(), json!({"path": "y"})).failed);
        assert!(d.path().join("y").exists());
    }

    #[test]
    fn refusals() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join(".yah/trash")).unwrap();
        for args in [
            json!({}),
            json!({"path": "."}),
            json!({"path": ""}),
            json!({"path": ".."}),
            json!({"path": "nope"}),
            json!({"path": ".yah"}),
            json!({"path": ".yah/trash"}),
        ] {
            let out = run(&DeleteFile::default(), d.path(), args.clone());
            assert!(out.failed && out.changes.is_empty(), "{args}: {}", out.text);
        }
        assert!(d.path().join(".yah/trash").exists());
    }

    #[cfg(unix)]
    #[test]
    fn links() {
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("victim"), "safe").unwrap();
        let d = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path().join("victim"), d.path().join("link")).unwrap();
        // deleting the link moves the link, not its target
        assert!(!run(&DeleteFile::default(), d.path(), json!({"path": "link"})).failed);
        assert_eq!(fs::read_to_string(outside.path().join("victim")).unwrap(), "safe");
        // a trash folder that links outside is refused
        fs::remove_dir_all(d.path().join(".yah")).unwrap();
        fs::write(d.path().join("f"), "").unwrap();
        std::os::unix::fs::symlink(outside.path(), d.path().join(".yah")).unwrap();
        assert!(run(&DeleteFile::default(), d.path(), json!({"path": "f"})).failed);
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 1);
    }

    #[test]
    fn title() {
        assert_eq!(DeleteFile::default().title(&json!({"path": "a/b.txt"})), "Deleted b.txt");
    }
}
