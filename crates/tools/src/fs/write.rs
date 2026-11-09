//! `write_file`: create or replace a text file.

use std::fs::OpenOptions;
use std::io::{ErrorKind, Write as _};

use serde_json::{Value, json};

use super::{arg_str, bytes, req_string, resolve_mut, short_name};
use crate::{Access, FileChange, Tool, ToolContext, ToolOutput, ToolSpec};

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
    fn refusals() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir(d.path().join("dir")).unwrap();
        for args in [
            json!({"path": "x"}),
            json!({"content": "x"}),
            json!({"path": "../x", "content": ""}),
            json!({"path": "/tmp/yah-escape-test", "content": ""}),
            json!({"path": "dir", "content": ""}),
            json!({"path": "new/", "content": ""}),
            json!({"path": ".", "content": ""}),
            json!({"path": ".yah/trash/x", "content": ""}),
        ] {
            let out = run(&WriteFile, d.path(), args.clone());
            assert!(out.failed && out.changes.is_empty(), "{args}");
        }
        assert!(!std::path::Path::new("/tmp/yah-escape-test").exists());
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
    fn title() {
        assert_eq!(WriteFile.title(&json!({"path": "a/notes.md"})), "Wrote notes.md");
        assert_eq!(WriteFile.access(&json!({})), Access::Write);
    }
}
