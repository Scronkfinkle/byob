//! `move_file` and `move_files`: move or rename inside the workspace.

use std::path::Path;

use serde_json::{Value, json};

use super::{arg_str, clip, lexists, opt_string, req_string, resolve_mut, short_name};
use crate::preview::{self, MAX_PREVIEW_LINES, Preview};
use crate::sandbox::{self, Resolved};
use crate::undo::UndoOp;
use crate::{Access, FileChange, Tool, ToolContext, ToolOutput, ToolSpec};

/// Most moves in one `move_files` call.
pub const MAX_MOVES: usize = 1000;
/// Most problems listed when a batch is refused.
const MAX_PROBLEMS: usize = 30;

pub struct MoveFile;

impl Tool for MoveFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "move_file".into(),
            description: "Move or rename a file or folder in the workspace. If `to` is an existing folder, the item goes inside it.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "from": {"type": "string", "description": "Current path, relative to the workspace"},
                    "to": {"type": "string", "description": "New path, or an existing folder to move it into"}
                },
                "required": ["from", "to"]
            }),
        }
    }

    fn title(&self, args: &Value) -> String {
        let from = short_name(arg_str(args, "from").unwrap_or("a file"));
        let to = arg_str(args, "to").map(|t| clip(t.trim(), 60)).unwrap_or_else(|| "?".into());
        format!("Moved {from} → {to}")
    }

    fn pending_title(&self, args: &Value) -> String {
        let from = short_name(arg_str(args, "from").unwrap_or("a file"));
        let to = arg_str(args, "to").map(|t| clip(t.trim(), 60)).unwrap_or_else(|| "?".into());
        format!("Move {from} → {to}")
    }

    fn preview(&self, args: &Value, workspace: &Path) -> Option<Preview> {
        match (arg_str(args, "from"), arg_str(args, "to")) {
            (Some(from), Some(to)) => Some(Preview::Move { moves: vec![shown_pair(workspace, from, to)], more: 0 }),
            _ => Some(preview::args(args)),
        }
    }

    fn access(&self, _args: &Value) -> Access {
        Access::Write
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        let plan = req_string(args, "from").and_then(|f| req_string(args, "to").and_then(|t| plan(ctx.workspace, &f, &t)));
        match plan {
            Err(e) => ToolOutput::err(e),
            Ok(Plan::Done { to }) => ToolOutput::ok(format!("{} is already there. Nothing to do.", to.show())),
            Ok(Plan::Move { from, to }) => match apply(ctx, &from, &to) {
                Ok(change) => ToolOutput { text: format!("Moved {} to {}", from.show(), to.show()), failed: false, changes: vec![change] },
                Err(e) => ToolOutput::err(e),
            },
        }
    }
}

pub struct MoveFiles;

impl Tool for MoveFiles {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "move_files".into(),
            description: "Move or rename many files at once; use this instead of many move_file calls. Nothing moves if any entry has a problem. With `folder`, paths in `moves` are relative to that folder.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "moves": {
                        "type": "array",
                        "description": "Each file and where it goes",
                        "items": {"type": "object", "properties": {"from": {"type": "string"}, "to": {"type": "string"}}, "required": ["from", "to"]}
                    },
                    "folder": {"type": "string", "description": "Optional folder the paths are relative to"}
                },
                "required": ["moves"]
            }),
        }
    }

    fn title(&self, args: &Value) -> String {
        match moves_of(args) {
            Ok(m) if m.len() == 1 => "Moved 1 file".into(),
            Ok(m) => format!("Moved {} files", m.len()),
            Err(_) => "Moved files".into(),
        }
    }

    fn pending_title(&self, args: &Value) -> String {
        match moves_of(args) {
            Ok(m) if m.len() == 1 => "Move 1 file".into(),
            Ok(m) => format!("Move {} files", m.len()),
            Err(_) => "Move files".into(),
        }
    }

    fn preview(&self, args: &Value, workspace: &Path) -> Option<Preview> {
        let Ok(list) = moves_of(args) else { return Some(preview::args(args)) };
        let moves = list.iter().take(MAX_PREVIEW_LINES).map(|(f, t)| shown_pair(workspace, f, t)).collect();
        Some(Preview::Move { moves, more: list.len().saturating_sub(MAX_PREVIEW_LINES) })
    }

    fn access(&self, _args: &Value) -> Access {
        Access::Write
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        match run_batch(args, ctx) {
            Ok(out) => out,
            Err(e) => ToolOutput::err(e),
        }
    }
}

/// A move as the user should read it: workspace-relative where the paths resolve, and
/// into a folder when `to` is one.
fn shown_pair(ws: &Path, from: &str, to: &str) -> (String, String) {
    match plan(ws, from, to) {
        Ok(Plan::Move { from, to }) => (from.show(), to.show()),
        Ok(Plan::Done { to }) => (preview::clip_line(from), to.show()),
        Err(_) => (preview::clip_line(from), preview::clip_line(to)),
    }
}

enum Plan {
    /// `from` is gone and `to` exists: a repeated call. Say so plainly so the model moves on.
    Done {
        to: Resolved,
    },
    Move {
        from: Resolved,
        to: Resolved,
    },
}

fn plan(ws: &Path, from: &str, to: &str) -> Result<Plan, String> {
    let from = resolve_mut(ws, from)?;
    let Some(name) = from.abs.file_name().map(ToOwned::to_owned) else {
        return Err(format!("can't move {}", from.show()));
    };
    let mut to = sandbox::resolve(ws, to)?;
    // into an existing folder (not through a link to one)
    if std::fs::symlink_metadata(&to.abs).is_ok_and(|m| m.is_dir()) && to.abs != from.abs {
        to = Resolved { abs: to.abs.join(name), root: to.root };
    }
    if to.is_root() || to.is_reserved() {
        return Err(format!("can't move to {}", to.show()));
    }
    if !lexists(&from.abs) {
        if lexists(&to.abs) {
            return Ok(Plan::Done { to });
        }
        return Err(format!("{} doesn't exist; use list_files to see what's there", from.show()));
    }
    if to.abs == from.abs {
        return Ok(Plan::Done { to });
    }
    if lexists(&to.abs) {
        return Err(format!("{} already exists; pick another name", to.show()));
    }
    if to.abs.starts_with(&from.abs) {
        return Err(format!("can't move {} into itself", from.show()));
    }
    Ok(Plan::Move { from, to })
}

fn apply(ctx: &ToolContext<'_>, from: &Resolved, to: &Resolved) -> Result<FileChange, String> {
    if let Some(parent) = to.abs.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("can't create the folder for {}: {e}", to.show()))?;
    }
    // rename doesn't follow a link at `from`: the link itself moves
    std::fs::rename(&from.abs, &to.abs).map_err(|e| format!("can't move {} to {}: {e}", from.show(), to.show()))?;
    ctx.record(UndoOp::Moved { from: from.abs.clone(), to: to.abs.clone() });
    Ok(FileChange::Moved { from: from.abs.clone(), to: to.abs.clone() })
}

/// `(from, to)` pairs from the arguments. Models sometimes send the list as a JSON string.
fn moves_of(args: &Value) -> Result<Vec<(String, String)>, String> {
    let parsed;
    let list = match args.get("moves") {
        Some(Value::Array(a)) => a,
        Some(Value::String(s)) => {
            parsed = serde_json::from_str::<Value>(s).map_err(|_| "invalid arguments: `moves` must be a list of {\"from\", \"to\"}".to_string())?;
            parsed.as_array().ok_or("invalid arguments: `moves` must be a list of {\"from\", \"to\"}")?
        }
        _ => return Err("invalid arguments: `moves` must be a list of {\"from\", \"to\"}".into()),
    };
    if list.len() > MAX_MOVES {
        return Err(format!("too many at once ({}); do at most {MAX_MOVES} per call", list.len()));
    }
    let folder = opt_string(args, "folder")?.map(|f| f.trim().to_string()).filter(|f| !f.is_empty());
    let within = |p: &str| match &folder {
        // Joined as text with `/` (accepted everywhere, and what the model wrote).
        Some(dir) if !Path::new(p.trim()).has_root() => format!("{}/{}", dir.trim_end_matches(['/', '\\']), p.trim()),
        _ => p.to_string(),
    };
    list.iter()
        .enumerate()
        .map(|(i, m)| match (m.get("from").and_then(Value::as_str), m.get("to").and_then(Value::as_str)) {
            (Some(f), Some(t)) => Ok((within(f), within(t))),
            _ => Err(format!("invalid arguments: moves[{i}] needs string `from` and `to`")),
        })
        .collect()
}

fn run_batch(args: &Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, String> {
    let moves = moves_of(args)?;
    if moves.is_empty() {
        return Err("no files given: pass `moves` as a list of {\"from\", \"to\"} paths".into());
    }
    // check everything first, so a bad name doesn't leave the folder half renamed
    let mut problems = Vec::new();
    let mut todo = Vec::new();
    let mut done = 0usize;
    let mut targets = std::collections::HashSet::new();
    let mut done_targets = Vec::new();
    for (from, to) in &moves {
        match plan(ctx.workspace, from, to) {
            Err(e) => problems.push(e),
            Ok(Plan::Done { to }) => {
                done = done.saturating_add(1);
                done_targets.push(to);
            }
            Ok(Plan::Move { from, to }) => {
                if !targets.insert(to.abs.clone()) {
                    problems.push(format!("two files would both be {}", to.show()));
                }
                todo.push((from, to));
            }
        }
    }
    // "already done" only holds if no other entry moves that file away again
    for to in done_targets {
        if todo.iter().any(|(from, _)| to.abs.starts_with(&from.abs)) {
            problems.push(format!("{} is both a destination and moved away", to.show()));
        }
    }
    if !problems.is_empty() {
        let n = problems.len();
        let mut text =
            format!("Nothing was moved. Fix these and call move_files again:\n{}", problems.into_iter().take(MAX_PROBLEMS).collect::<Vec<_>>().join("\n"));
        if n > MAX_PROBLEMS {
            text.push_str(&format!("\n… and {} more", n.saturating_sub(MAX_PROBLEMS)));
        }
        return Ok(ToolOutput::err(text));
    }
    let mut changes = Vec::new();
    for (from, to) in &todo {
        if ctx.cancelled() {
            return Ok(ToolOutput { text: format!("Stopped after moving {} of {}.", changes.len(), todo.len()), failed: true, changes });
        }
        match apply(ctx, from, to) {
            Ok(c) => changes.push(c),
            Err(e) => return Ok(ToolOutput { text: format!("Moved {} of {}, then stopped: {e}", changes.len(), todo.len()), failed: true, changes }),
        }
    }
    let n = changes.len();
    let mut text = format!("Moved {n} {}.", if n == 1 { "file" } else { "files" });
    if done > 0 {
        text.push_str(&format!(" ({done} already done earlier.)"));
    }
    Ok(ToolOutput { text, failed: false, changes })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::fs::testutil::run;

    fn ws() -> (tempfile::TempDir, std::path::PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().canonicalize().unwrap();
        fs::write(root.join("a.txt"), "a").unwrap();
        fs::write(root.join("b.txt"), "b").unwrap();
        fs::create_dir(root.join("docs")).unwrap();
        (d, root)
    }

    #[test]
    fn rename_and_into_folder() {
        let (d, root) = ws();
        let out = run(&MoveFile, d.path(), json!({"from": "a.txt", "to": "new/c.txt"}));
        assert!(!out.failed, "{}", out.text);
        assert_eq!(out.changes, vec![FileChange::Moved { from: root.join("a.txt"), to: root.join("new/c.txt") }]);
        let out = run(&MoveFile, d.path(), json!({"from": "b.txt", "to": "docs"}));
        assert_eq!(out.text, "Moved b.txt to docs/b.txt");
        // repeated call
        let out = run(&MoveFile, d.path(), json!({"from": "b.txt", "to": "docs"}));
        assert!(!out.failed && out.changes.is_empty() && out.text.contains("already there"), "{}", out.text);
        // back to the root
        let out = run(&MoveFile, d.path(), json!({"from": "docs/b.txt", "to": "."}));
        assert!(!out.failed, "{}", out.text);
        assert!(root.join("b.txt").exists());
    }

    #[test]
    fn refusals() {
        let (d, root) = ws();
        fs::create_dir(root.join("docs/sub")).unwrap();
        for (from, to) in [
            ("a.txt", "b.txt"),
            ("nope", "x"),
            ("a.txt", "../a.txt"),
            ("../x", "a"),
            (".", "x"),
            ("docs", "docs/sub/inner"),
            ("a.txt", ".byob/x"),
            (".byob", "y"),
            ("a.txt", "/etc/a.txt"),
        ] {
            let out = run(&MoveFile, d.path(), json!({"from": from, "to": to}));
            assert!(out.failed && out.changes.is_empty(), "{from} -> {to}: {}", out.text);
        }
        assert!(run(&MoveFile, d.path(), json!({"from": "a.txt"})).failed);
        assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "b");
    }

    #[cfg(unix)]
    #[test]
    fn links() {
        let outside = tempfile::tempdir().unwrap();
        let (d, root) = ws();
        std::os::unix::fs::symlink(outside.path(), root.join("out")).unwrap();
        // can't move into a linked-out folder
        assert!(run(&MoveFile, d.path(), json!({"from": "a.txt", "to": "out/a.txt"})).failed);
        // moving the link itself is fine: it stays inside
        let out = run(&MoveFile, d.path(), json!({"from": "out", "to": "docs/out"}));
        assert!(!out.failed, "{}", out.text);
        assert!(fs::symlink_metadata(root.join("docs/out")).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn batch_all_or_nothing() {
        let (d, root) = ws();
        let out = run(&MoveFiles, d.path(), json!({"moves": [{"from": "a.txt", "to": "docs/a.txt"}, {"from": "missing", "to": "x"}]}));
        assert!(out.failed && out.text.starts_with("Nothing was moved"), "{}", out.text);
        assert!(root.join("a.txt").exists());
        let out = run(&MoveFiles, d.path(), json!({"moves": [{"from": "a.txt", "to": "x"}, {"from": "b.txt", "to": "x"}]}));
        assert!(out.failed && out.text.contains("both"), "{}", out.text);
    }

    #[test]
    fn batch_moves_with_folder_and_string_list() {
        let (d, root) = ws();
        let out = run(&MoveFiles, d.path(), json!({"moves": "[{\"from\":\"a.txt\",\"to\":\"1.txt\"},{\"from\":\"b.txt\",\"to\":\"2.txt\"}]"}));
        assert!(!out.failed, "{}", out.text);
        assert_eq!(out.text, "Moved 2 files.");
        assert_eq!(out.changes.len(), 2);
        fs::write(root.join("docs/x"), "").unwrap();
        let out = run(&MoveFiles, d.path(), json!({"folder": "docs", "moves": [{"from": "x", "to": "y"}, {"from": "zz", "to": "x"}]}));
        assert!(out.failed, "{}", out.text);
        let out = run(&MoveFiles, d.path(), json!({"folder": "docs", "moves": [{"from": "x", "to": "y"}]}));
        assert_eq!(out.changes, vec![FileChange::Moved { from: root.join("docs/x"), to: root.join("docs/y") }]);
        let out = run(&MoveFiles, d.path(), json!({"folder": "docs", "moves": [{"from": "x", "to": "y"}]}));
        assert!(!out.failed && out.text.contains("already done"), "{}", out.text);
    }

    #[test]
    fn batch_bad_args() {
        let (d, _root) = ws();
        for args in [
            json!({}),
            json!({"moves": []}),
            json!({"moves": "nope"}),
            json!({"moves": [1]}),
            json!({"moves": [{"from": "a.txt"}]}),
            json!({"folder": 3, "moves": []}),
        ] {
            assert!(run(&MoveFiles, d.path(), args.clone()).failed, "{args}");
        }
        let many: Vec<Value> = (0..=MAX_MOVES).map(|i| json!({"from": format!("{i}"), "to": "x"})).collect();
        assert!(run(&MoveFiles, d.path(), json!({ "moves": many })).text.contains("too many"));
    }

    #[test]
    fn titles() {
        assert_eq!(MoveFile.title(&json!({"from": "a/b.txt", "to": "c/b.txt"})), "Moved b.txt → c/b.txt");
        assert_eq!(MoveFiles.title(&json!({"moves": [{"from": "a", "to": "b"}, {"from": "c", "to": "d"}]})), "Moved 2 files");
        assert_eq!(MoveFiles.title(&json!(null)), "Moved files");
        assert_eq!(MoveFile.pending_title(&json!({"from": "a/b.txt", "to": "c/b.txt"})), "Move b.txt → c/b.txt");
        assert_eq!(MoveFiles.pending_title(&json!({"moves": [{"from": "a", "to": "b"}]})), "Move 1 file");
    }

    #[test]
    fn previews() {
        let (_d, root) = ws();
        fs::create_dir(root.join("dest")).unwrap();
        fs::write(root.join("a.txt"), "").unwrap();
        let p = MoveFile.preview(&json!({"from": "./a.txt", "to": "dest"}), &root).unwrap();
        assert_eq!(p, Preview::Move { moves: vec![("a.txt".into(), "dest/a.txt".into())], more: 0 });
        let list: Vec<Value> = (0..50).map(|i| json!({"from": format!("f{i}"), "to": format!("g{i}")})).collect();
        let Some(Preview::Move { moves, more }) = MoveFiles.preview(&json!({"moves": list, "folder": "x"}), &root) else { panic!() };
        assert_eq!((moves.len(), more), (MAX_PREVIEW_LINES, 10));
        assert_eq!(moves[0], ("x/f0".into(), "x/g0".into()));
    }
}
