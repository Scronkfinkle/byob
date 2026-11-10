//! `write_file`: create or replace a text file.

use std::fs::OpenOptions;
use std::io::{ErrorKind, Write as _};

use serde_json::{Value, json};

use super::{arg_str, bytes, req_string, resolve_mut, short_name};
use crate::preview::{self, Preview};
use crate::undo::{Fingerprint, UndoOp};
use crate::{Access, FileChange, Tool, ToolContext, ToolOutput, ToolSpec};

/// Existing files up to this size are diffed in previews.
const MAX_DIFF_BYTES: u64 = 1024 * 1024;

/// Largest content accepted in one call.
pub const MAX_WRITE_BYTES: usize = 16 * 1024 * 1024;

pub struct WriteFile;

impl Tool for WriteFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_file".into(),
            description: "Write a text file in the workspace, replacing it if it exists. Creates folders as needed.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, relative to the workspace"},
                    "content": {"type": "string", "description": "The whole new content of the file"}
                },
                "required": ["path", "content"]
            }),
        }
    }

    fn title(&self, args: &Value) -> String {
        format!("Wrote {}", short_name(arg_str(args, "path").unwrap_or("a file")))
    }

    fn pending_title(&self, args: &Value) -> String {
        format!("Write {}", short_name(arg_str(args, "path").unwrap_or("a file")))
    }

    fn preview(&self, args: &Value, workspace: &std::path::Path) -> Option<Preview> {
        let (Some(path), Some(content)) = (arg_str(args, "path"), arg_str(args, "content")) else {
            return Some(preview::args(args));
        };
        let shown = super::sandbox::resolve(workspace, path).map(|r| r.show()).unwrap_or_else(|_| preview::clip_line(path));
        let old = super::sandbox::resolve(workspace, path).ok().and_then(|r| {
            let meta = std::fs::symlink_metadata(&r.abs).ok()?;
            Some(if meta.is_file() && meta.len() <= MAX_DIFF_BYTES { std::fs::read(&r.abs).ok().and_then(|b| String::from_utf8(b).ok()) } else { None })
        });
        let replaces = old.is_some();
        let (lines, more) = match old.flatten() {
            Some(old) => preview::diff(&old, content),
            None => preview::head(content),
        };
        Some(Preview::Write { path: shown, bytes: content.len() as u64, replaces, lines, more })
    }

    fn access(&self, _args: &Value) -> Access {
        Access::Write
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        match write(args, ctx) {
            Ok(out) => out,
            Err(e) => ToolOutput::err(e),
        }
    }
}

fn write(args: &Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, String> {
    let path = req_string(args, "path")?;
    let content = req_string(args, "content")?;
    if content.len() > MAX_WRITE_BYTES {
        return Err(format!("content is too large ({}; max {})", bytes(content.len() as u64), bytes(MAX_WRITE_BYTES as u64)));
    }
    if path.trim().ends_with(['/', '\\']) {
        return Err("path names a folder; give a file name".into());
    }
    let file = resolve_mut(ctx.workspace, &path)?;
    let name = file.show();
    let existed = match std::fs::symlink_metadata(&file.abs) {
        Ok(m) if m.file_type().is_symlink() => return Err(format!("{name} is a link; write to a regular file instead")),
        Ok(m) if m.is_dir() => return Err(format!("{name} is a folder; give a file name")),
        Ok(m) if !m.is_file() => return Err(format!("{name} is not a regular file")),
        Ok(_) => true,
        Err(e) if e.kind() == ErrorKind::NotFound => false,
        Err(e) => return Err(format!("can't access {name}: {e}")),
    };
    // keep the old content so the change can be undone
    let backup = match ctx.journal() {
        Some(j) if existed => Some(j.backup(ctx.workspace, &file.abs).map_err(|e| format!("can't keep a copy of {name} for undo ({e}); not writing it"))?),
        _ => None,
    };
    if let Some(parent) = file.abs.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("can't create the folder for {name}: {e}"))?;
    }
    let mut opts = OpenOptions::new();
    opts.write(true);
    if existed {
        opts.truncate(true);
    } else {
        // a new file stays new: never clobber (or follow a link to) one that appeared since the check
        opts.create_new(true);
    }
    let mut f = opts.open(&file.abs).map_err(|e| match e.kind() {
        ErrorKind::AlreadyExists => format!("{name} appeared meanwhile; read it first, or pick another name"),
        _ => format!("can't write {name}: {e}"),
    })?;
    f.write_all(content.as_bytes()).map_err(|e| format!("can't write {name}: {e}"))?;
    let fingerprint = Some(Fingerprint::of_bytes(content.as_bytes()));
    ctx.record(match backup {
        Some(backup) => UndoOp::Replaced { path: file.abs.clone(), backup, fingerprint },
        None => UndoOp::Created { path: file.abs.clone(), fingerprint },
    });
    let change = if existed { FileChange::Modified(file.abs.clone()) } else { FileChange::Created(file.abs.clone()) };
    let verb = if existed { "Replaced" } else { "Created" };
    Ok(ToolOutput { text: format!("{verb} {name} ({})", bytes(content.len() as u64)), failed: false, changes: vec![change] })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::fs::testutil::run;

    #[test]
    fn creates_then_modifies() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().canonicalize().unwrap();
        let out = run(&WriteFile, d.path(), json!({"path": "a/b/c.txt", "content": "hi"}));
        assert!(!out.failed, "{}", out.text);
        assert_eq!(out.text, "Created a/b/c.txt (2 B)");
        assert_eq!(out.changes, vec![FileChange::Created(root.join("a/b/c.txt"))]);
        let out = run(&WriteFile, d.path(), json!({"path": "./a/b/c.txt", "content": "bye!"}));
        assert_eq!(out.changes, vec![FileChange::Modified(root.join("a/b/c.txt"))]);
        assert_eq!(fs::read_to_string(root.join("a/b/c.txt")).unwrap(), "bye!");
    }

    #[test]
    fn writes_the_workspace_extensions_but_not_byobs_own_data() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().canonicalize().unwrap();
        for p in [".byob/plugins/weather/byob-plugin.json", ".byob/plugins/weather/tools/get.py", ".byob/agents/explorer.md", ".byob/skills/plan/SKILL.md"] {
            let out = run(&WriteFile, d.path(), json!({"path": p, "content": "x"}));
            assert!(!out.failed, "{p}: {}", out.text);
            assert_eq!(fs::read_to_string(root.join(p)).unwrap(), "x");
        }
        for p in [".byob/undo/t/x", ".byob/x.json", ".byob/artifacts/a/v1.html"] {
            assert!(run(&WriteFile, d.path(), json!({"path": p, "content": "x"})).failed, "{p}");
        }
    }

    #[test]
    fn refusals() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir(d.path().join("dir")).unwrap();
        for args in [
            json!({"path": "x"}),
            json!({"content": "x"}),
            json!({"path": "../x", "content": ""}),
            json!({"path": "/tmp/byob-escape-test", "content": ""}),
            json!({"path": "dir", "content": ""}),
            json!({"path": "new/", "content": ""}),
            json!({"path": ".", "content": ""}),
            json!({"path": ".byob/trash/x", "content": ""}),
        ] {
            let out = run(&WriteFile, d.path(), args.clone());
            assert!(out.failed && out.changes.is_empty(), "{args}");
        }
        assert!(!std::path::Path::new("/tmp/byob-escape-test").exists());
    }

    #[cfg(unix)]
    #[test]
    fn never_writes_through_links() {
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("victim"), "safe").unwrap();
        let d = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path().join("victim"), d.path().join("link")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("new"), d.path().join("dangling")).unwrap();
        std::os::unix::fs::symlink(outside.path(), d.path().join("outdir")).unwrap();
        for p in ["link", "dangling", "outdir/x", "outdir/sub/x"] {
            let out = run(&WriteFile, d.path(), json!({"path": p, "content": "pwned"}));
            assert!(out.failed, "{p}");
        }
        assert_eq!(fs::read_to_string(outside.path().join("victim")).unwrap(), "safe");
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 1);
    }

    #[test]
    fn previews() {
        use crate::preview::{DiffLine, LineKind};
        let d = tempfile::tempdir().unwrap();
        let p = WriteFile.preview(&json!({"path": "./new.md", "content": "# Hi\n- a\n"}), d.path()).unwrap();
        let Preview::Write { path, bytes, replaces, lines, more } = p else { panic!("{p:?}") };
        assert_eq!((path.as_str(), bytes, replaces, lines.len(), more), ("new.md", 9, false, 2, 0));
        fs::write(d.path().join("old.md"), "one\ntwo\n").unwrap();
        let p = WriteFile.preview(&json!({"path": "old.md", "content": "one\nTWO\n"}), d.path()).unwrap();
        let Preview::Write { replaces: true, lines, .. } = p else { panic!("{p:?}") };
        assert_eq!(lines[1], DiffLine { kind: LineKind::Removed, text: "two".into() });
        assert_eq!(lines[2], DiffLine { kind: LineKind::Added, text: "TWO".into() });
        // missing content: fall back to the arguments
        assert!(matches!(WriteFile.preview(&json!({"path": "x"}), d.path()), Some(Preview::Args { .. })));
        // nothing outside the workspace is read
        let p = WriteFile.preview(&json!({"path": "/etc/passwd", "content": "x"}), d.path()).unwrap();
        assert!(matches!(p, Preview::Write { replaces: false, .. }), "{p:?}");
    }

    #[test]
    fn title() {
        assert_eq!(WriteFile.title(&json!({"path": "a/notes.md"})), "Wrote notes.md");
        assert_eq!(WriteFile.pending_title(&json!({"path": "a/notes.md"})), "Write notes.md");
        assert_eq!(WriteFile.pending_title(&json!(null)), "Write a file");
        assert_eq!(WriteFile.access(&json!({})), Access::Write);
    }
}
