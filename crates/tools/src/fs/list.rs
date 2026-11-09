//! `list_files`: a bounded, sorted listing of a folder, optionally a few levels deep.

use std::fmt::Write as _;
use std::path::Path;

use serde_json::{Value, json};

use super::{arg_str, bytes, opt_string, opt_u64, short_name};
use crate::{Access, Tool, ToolContext, ToolOutput, ToolSpec, sandbox};

/// Most entries shown in one listing.
pub const MAX_ENTRIES: usize = 300;
/// Deepest `depth` accepted.
pub const MAX_DEPTH: u64 = 4;

pub struct ListFiles;

impl Tool for ListFiles {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "list_files".into(),
            description: "List files and folders in a folder of the workspace. Folders end with /. Hidden files are skipped.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Folder, relative to the workspace; omit for the workspace itself"},
                    "depth": {"type": "integer", "description": "How many levels deep to list (1-4, default 1)"}
                }
            }),
        }
    }

    fn title(&self, args: &Value) -> String {
        match arg_str(args, "path").map(str::trim) {
            Some(p) if !p.is_empty() && p != "." => format!("Looked in {}", short_name(p)),
            _ => "Looked at the files".into(),
        }
    }

    fn pending_title(&self, args: &Value) -> String {
        match arg_str(args, "path").map(str::trim) {
            Some(p) if !p.is_empty() && p != "." => format!("Look in {}", short_name(p)),
            _ => "Look at the files".into(),
        }
    }

    /// The title says it all.
    fn preview(&self, _args: &Value, _workspace: &std::path::Path) -> Option<crate::Preview> {
        None
    }

    fn access(&self, _args: &Value) -> Access {
        Access::Read
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        match list(args, ctx) {
            Ok(text) => ToolOutput::ok(text),
            Err(e) => ToolOutput::err(e),
        }
    }
}

struct Walk<'a, 'c> {
    ctx: &'a ToolContext<'c>,
    lines: Vec<String>,
    /// Entries seen beyond the cap.
    more: usize,
}

fn list(args: &Value, ctx: &ToolContext<'_>) -> Result<String, String> {
    let path = opt_string(args, "path")?.unwrap_or_default();
    let depth = opt_u64(args, "depth")?.unwrap_or(1).clamp(1, MAX_DEPTH);
    let dir = sandbox::resolve_follow(ctx.workspace, &path)?;
    let meta = std::fs::metadata(&dir.abs).map_err(|e| format!("can't open {}: {}", dir.show(), io_reason(&e)))?;
    if !meta.is_dir() {
        return Err(format!("{} is a file, not a folder; use read_file to read it", dir.show()));
    }
    let mut walk = Walk { ctx, lines: Vec::new(), more: 0 };
    walk.dir(&dir.abs, 0, depth)?;
    let mut out = format!("Folder: {}\n", dir.show());
    if walk.lines.is_empty() {
        out.push_str("(empty)");
    } else {
        out.push_str(&walk.lines.join("\n"));
    }
    if walk.more > 0 {
        let _ = write!(out, "\n… and {} more (list a subfolder to see them)", walk.more);
    }
    if ctx.cancelled() {
        out.push_str("\n(stopped: cancelled)");
    }
    Ok(out)
}

impl Walk<'_, '_> {
    fn dir(&mut self, dir: &Path, level: u64, depth: u64) -> Result<(), String> {
        let rd = match std::fs::read_dir(dir) {
            Ok(rd) => rd,
            // the top level must be readable; deeper folders just say so
            Err(e) if level == 0 => return Err(format!("can't list the folder: {}", io_reason(&e))),
            Err(e) => {
                self.push(level, format!("(can't list: {})", io_reason(&e)));
                return Ok(());
            }
        };
        let mut entries: Vec<(String, std::fs::DirEntry)> =
            rd.filter_map(Result::ok).map(|e| (e.file_name().to_string_lossy().into_owned(), e)).filter(|(n, _)| !n.starts_with('.')).collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, entry) in entries {
            if self.ctx.cancelled() {
                return Ok(());
            }
            if self.lines.len() >= MAX_ENTRIES {
                self.more = self.more.saturating_add(1);
                continue;
            }
            let ft = entry.file_type().ok();
            let is_link = ft.is_some_and(|t| t.is_symlink());
            let is_dir = ft.is_some_and(|t| t.is_dir());
            if is_link {
                let kind = if std::fs::metadata(entry.path()).is_ok_and(|m| m.is_dir()) { "/" } else { "" };
                self.push(level, format!("{name}{kind} (link)"));
            } else if is_dir {
                self.push(level, format!("{name}/"));
                if level.saturating_add(1) < depth {
                    self.dir(&entry.path(), level.saturating_add(1), depth)?;
                }
            } else {
                let size = entry.metadata().map(|m| bytes(m.len())).unwrap_or_else(|_| "?".into());
                self.push(level, format!("{name} ({size})"));
            }
        }
        Ok(())
    }

    fn push(&mut self, level: u64, text: String) {
        let indent = usize::try_from(level).unwrap_or(0).saturating_mul(2);
        self.lines.push(format!("{:indent$}{text}", ""));
    }
}

fn io_reason(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => "it doesn't exist".into(),
        std::io::ErrorKind::PermissionDenied => "permission denied".into(),
        _ => e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::fs::testutil::run;

    fn tree() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join("docs/deep/deeper")).unwrap();
        fs::write(d.path().join("b.txt"), "hello").unwrap();
        fs::write(d.path().join("docs/a.md"), "x".repeat(2048)).unwrap();
        fs::write(d.path().join("docs/deep/c.txt"), "").unwrap();
        fs::write(d.path().join(".hidden"), "").unwrap();
        fs::create_dir_all(d.path().join(".yah/trash")).unwrap();
        d
    }

    #[test]
    fn lists_sorted_and_skips_hidden() {
        let d = tree();
        let out = run(&ListFiles, d.path(), json!({}));
        assert!(!out.failed, "{}", out.text);
        assert_eq!(out.text, "Folder: .\nb.txt (5 B)\ndocs/");
    }

    #[test]
    fn depth_and_subfolder() {
        let d = tree();
        let out = run(&ListFiles, d.path(), json!({"path": "docs", "depth": "3"}));
        assert_eq!(out.text, "Folder: docs\na.md (2.0 KB)\ndeep/\n  c.txt (0 B)\n  deeper/");
        let out = run(&ListFiles, d.path(), json!({"depth": 99}));
        assert!(out.text.contains("      deeper/") || out.text.contains("    deeper/"), "{}", out.text);
    }

    #[test]
    fn errors() {
        let d = tree();
        assert!(run(&ListFiles, d.path(), json!({"path": "b.txt"})).failed);
        assert!(run(&ListFiles, d.path(), json!({"path": "nope"})).failed);
        assert!(run(&ListFiles, d.path(), json!({"path": ".."})).failed);
        assert!(run(&ListFiles, d.path(), json!({"path": 3})).failed);
        assert!(run(&ListFiles, d.path(), json!({"depth": -1})).failed);
    }

    #[test]
    fn caps_entries() {
        let d = tempfile::tempdir().unwrap();
        for i in 0..(MAX_ENTRIES + 7) {
            fs::write(d.path().join(format!("f{i:04}")), "").unwrap();
        }
        let out = run(&ListFiles, d.path(), json!({}));
        assert!(out.text.ends_with("… and 7 more (list a subfolder to see them)"), "{}", &out.text[out.text.len() - 80..]);
    }

    #[cfg(unix)]
    #[test]
    fn links_out_are_not_followed() {
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), "s").unwrap();
        let d = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), d.path().join("out")).unwrap();
        let out = run(&ListFiles, d.path(), json!({"depth": 4}));
        assert_eq!(out.text, "Folder: .\nout/ (link)");
        assert!(run(&ListFiles, d.path(), json!({"path": "out"})).failed);
    }

    #[test]
    fn titles() {
        assert_eq!(ListFiles.title(&json!({"path": "a/docs/"})), "Looked in docs");
        assert_eq!(ListFiles.title(&json!({})), "Looked at the files");
        assert_eq!(ListFiles.title(&json!(null)), "Looked at the files");
        assert_eq!(ListFiles.pending_title(&json!({"path": "a/docs/"})), "Look in docs");
        assert_eq!(ListFiles.pending_title(&json!({})), "Look at the files");
    }
}
