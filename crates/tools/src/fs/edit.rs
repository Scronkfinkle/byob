//! `edit_file`: find/replace edits to a text file (see [`crate::edits`]), instead of
//! rewriting it whole.

use std::fs::OpenOptions;
use std::io::{ErrorKind, Write as _};
use std::path::Path;

use serde::Deserialize;
use serde_json::{Value, json};

use super::write::MAX_WRITE_BYTES;
use super::{arg_str, bytes, req_string, resolve_mut, short_name};
use crate::edits::{self, Opts};
use crate::preview::{self, Preview};
use crate::undo::{Fingerprint, UndoOp};
use crate::{Access, FileChange, Tool, ToolContext, ToolOutput, ToolSpec, sandbox};

/// Largest file edited.
pub const MAX_EDIT_BYTES: u64 = 8 * 1024 * 1024;
/// Files up to this size get a diff on the approval card.
const MAX_DIFF_BYTES: u64 = 1024 * 1024;

pub struct EditFile;

#[derive(Deserialize)]
struct Edit {
    #[serde(alias = "old_string", alias = "old_str", alias = "search", alias = "old")]
    find: String,
    #[serde(default, alias = "new_string", alias = "new_str", alias = "replacement", alias = "replace_with", alias = "new")]
    replace: String,
}

impl Tool for EditFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit_file".into(),
            description: "Change part of a text file: each edit replaces `find` (copied from the file, occurring once) with `replace`. Edits apply in order."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, relative to the workspace"},
                    "edits": {"type": "array", "items": {"type": "object", "properties": {"find": {"type": "string"}, "replace": {"type": "string"}}, "required": ["find", "replace"]}},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence of each find"}
                },
                "required": ["path", "edits"]
            }),
        }
    }

    fn title(&self, args: &Value) -> String {
        format!("Edited {}", short_name(arg_str(args, "path").unwrap_or("a file")))
    }

    fn pending_title(&self, args: &Value) -> String {
        format!("Edit {}", short_name(arg_str(args, "path").unwrap_or("a file")))
    }

    /// A diff of what the edits do; the arguments if they don't apply.
    fn preview(&self, args: &Value, workspace: &Path) -> Option<Preview> {
        let planned = || -> Option<Preview> {
            let path = arg_str(args, "path")?;
            let file = sandbox::resolve(workspace, path).ok()?;
            let meta = std::fs::symlink_metadata(&file.abs).ok()?;
            if !meta.is_file() || meta.len() > MAX_DIFF_BYTES {
                return None;
            }
            let old = String::from_utf8(std::fs::read(&file.abs).ok()?).ok()?;
            let new = plan(args, &old, &file.show()).ok()?.text;
            let (lines, more) = preview::diff(&old, &new);
            Some(Preview::Write { path: file.show(), bytes: new.len() as u64, replaces: true, lines, more })
        };
        planned().or_else(|| Some(preview::args(args)))
    }

    fn access(&self, _args: &Value) -> Access {
        Access::Write
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        match edit(args, ctx) {
            Ok(out) => out,
            Err(e) => ToolOutput::err(e),
        }
    }
}

/// `true`/`false`, or the strings small models sometimes send.
fn opt_bool(args: &Value, key: &str) -> Result<bool, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(Value::String(s)) if s.eq_ignore_ascii_case("true") => Ok(true),
        Some(Value::String(s)) if s.eq_ignore_ascii_case("false") || s.is_empty() => Ok(false),
        Some(_) => Err(format!("invalid arguments: `{key}` must be true or false")),
    }
}

/// The edits of a call: `edits`, or a single top-level `find`/`replace`.
fn edits_of(args: &Value) -> Result<Vec<(String, String)>, String> {
    let list = match args.get("edits") {
        Some(Value::Array(a)) => a.clone(),
        // some models send the array as a JSON string, or a single edit object
        Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
            Ok(Value::Array(a)) => a,
            Ok(v @ Value::Object(_)) => vec![v],
            _ => return Err("invalid arguments: `edits` must be a list of {find, replace}".into()),
        },
        Some(v @ Value::Object(_)) => vec![v.clone()],
        None | Some(Value::Null) if args.get("find").is_some() || args.get("old_string").is_some() => vec![args.clone()],
        None | Some(Value::Null) => return Err("invalid arguments: missing `edits` (a list of {find, replace})".into()),
        Some(_) => return Err("invalid arguments: `edits` must be a list of {find, replace}".into()),
    };
    list.into_iter()
        .enumerate()
        .map(|(i, v)| {
            serde_json::from_value::<Edit>(v).map(|e| (e.find, e.replace)).map_err(|e| format!("invalid arguments: edit {}: {e}", i.saturating_add(1)))
        })
        .collect()
}

/// What the edits make of `old`.
fn plan(args: &Value, old: &str, name: &str) -> Result<edits::Applied, String> {
    let list = edits_of(args)?;
    let opts = Opts { replace_all: opt_bool(args, "replace_all")?, offer_replace_all: true, max_bytes: MAX_WRITE_BYTES, what: name, reread: "read_file" };
    let applied = edits::apply(old, &list, &opts)?;
    if applied.text == old {
        return Err("the edits change nothing (each replace is the same as its find)".into());
    }
    Ok(applied)
}

fn edit(args: &Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, String> {
    let path = req_string(args, "path")?;
    let file = resolve_mut(ctx.workspace, &path)?;
    let name = file.show();
    match std::fs::symlink_metadata(&file.abs) {
        Ok(m) if m.file_type().is_symlink() => return Err(format!("{name} is a link; edit the file it points to instead")),
        Ok(m) if m.is_dir() => return Err(format!("{name} is a folder; give a file name")),
        Ok(m) if !m.is_file() => return Err(format!("{name} is not a regular file")),
        Ok(m) if m.len() > MAX_EDIT_BYTES => return Err(format!("{name} is too big to edit ({}; max {})", bytes(m.len()), bytes(MAX_EDIT_BYTES))),
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => return Err(format!("{name} doesn't exist; use write_file to create it")),
        Err(e) => return Err(format!("can't access {name}: {e}")),
    }
    let raw = std::fs::read(&file.abs).map_err(|e| format!("can't read {name}: {e}"))?;
    if raw.contains(&0) {
        return Err(format!("{name} is a binary file; edit_file only changes text"));
    }
    let old =
        String::from_utf8(raw).map_err(|_| format!("{name} is not UTF-8 text; edit_file can't change it safely. Rewrite it with write_file if you must"))?;
    let applied = plan(args, &old, &name)?;
    // keep the old content so the change can be undone
    let backup = match ctx.journal() {
        Some(j) => Some(j.backup(ctx.workspace, &file.abs).map_err(|e| format!("can't keep a copy of {name} for undo ({e}); not changing it"))?),
        None => None,
    };
    let mut f = OpenOptions::new().write(true).truncate(true).open(&file.abs).map_err(|e| format!("can't write {name}: {e}"))?;
    f.write_all(applied.text.as_bytes()).map_err(|e| format!("can't write {name}: {e}"))?;
    let fingerprint = Some(Fingerprint::of_bytes(applied.text.as_bytes()));
    if let Some(backup) = backup {
        ctx.record(UndoOp::Replaced { path: file.abs.clone(), backup, fingerprint });
    }
    Ok(ToolOutput { text: summary(&name, &old, &applied), failed: false, changes: vec![FileChange::Modified(file.abs.clone())] })
}

/// "Edited notes.md: 2 replacements, 3 lines → 4 (edit 2 matched ignoring whitespace)."
fn summary(name: &str, old: &str, applied: &edits::Applied) -> String {
    let n = applied.replaced;
    let mut s = format!("Edited {name}: {n} replacement{}", if n == 1 { "" } else { "s" });
    let (before, after) = (old.lines().count(), applied.text.lines().count());
    if before != after {
        s.push_str(&format!(", {before} lines → {after}"));
    }
    match applied.loose.as_slice() {
        [] => {}
        [k] => s.push_str(&format!(" (edit {k} matched ignoring whitespace)")),
        ks => s.push_str(&format!(" (edits {} matched ignoring whitespace)", ks.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "))),
    }
    s.push('.');
    s
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::fs::testutil::{run, run_journaled};

    #[test]
    fn edits_in_place() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().canonicalize().unwrap();
        fs::write(root.join("a.py"), "def f():\n    return 1\n\nprint(f())\n").unwrap();
        let out = run(
            &EditFile,
            d.path(),
            json!({"path": "a.py", "edits": [{"find": "return 1", "replace": "return 2"}, {"find": "print(f())", "replace": "print(f())\nprint('done')"}]}),
        );
        assert!(!out.failed, "{}", out.text);
        assert_eq!(out.text, "Edited a.py: 2 replacements, 4 lines → 5.");
        assert_eq!(out.changes, vec![FileChange::Modified(root.join("a.py"))]);
        assert_eq!(fs::read_to_string(root.join("a.py")).unwrap(), "def f():\n    return 2\n\nprint(f())\nprint('done')\n");
    }

    #[test]
    fn forgiving_argument_shapes() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("t.txt"), "one two three").unwrap();
        for args in [
            json!({"path": "t.txt", "find": "one", "replace": "1"}),
            json!({"path": "t.txt", "old_string": "two", "new_string": "2"}),
            json!({"path": "t.txt", "edits": "[{\"find\": \"three\", \"replace\": \"3\"}]"}),
            json!({"path": "t.txt", "edits": {"find": "1 2", "replace": "x"}}),
        ] {
            let out = run(&EditFile, d.path(), args.clone());
            assert!(!out.failed, "{args}: {}", out.text);
        }
        assert_eq!(fs::read_to_string(d.path().join("t.txt")).unwrap(), "x 3");
        let out = run(&EditFile, d.path(), json!({"path": "t.txt", "edits": [{"find": "x", "replace": "y"}], "replace_all": "true"}));
        assert!(!out.failed, "{}", out.text);
    }

    #[test]
    fn replace_all_and_ambiguity() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("t.txt"), "a\nfoo\nfoo\n").unwrap();
        let out = run(&EditFile, d.path(), json!({"path": "t.txt", "edits": [{"find": "foo", "replace": "bar"}]}));
        assert!(out.failed && out.text.contains("occurs 2 times in t.txt (lines 2, 3)") && out.text.contains("replace_all"), "{}", out.text);
        let out = run(&EditFile, d.path(), json!({"path": "t.txt", "edits": [{"find": "foo", "replace": "bar"}], "replace_all": true}));
        assert_eq!(out.text, "Edited t.txt: 2 replacements.");
        assert_eq!(fs::read_to_string(d.path().join("t.txt")).unwrap(), "a\nbar\nbar\n");
    }

    #[test]
    fn crlf_and_loose_matches_are_reported() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("w.txt"), "[a]\r\n  key = 1\r\n").unwrap();
        let out = run(&EditFile, d.path(), json!({"path": "w.txt", "edits": [{"find": "[a]\nkey = 1", "replace": "[a]\nkey = 2\nother = 3"}]}));
        assert!(!out.failed, "{}", out.text);
        assert!(out.text.contains("edit 1 matched ignoring whitespace"), "{}", out.text);
        assert_eq!(fs::read_to_string(d.path().join("w.txt")).unwrap(), "[a]\r\n  key = 2\r\n  other = 3\r\n");
    }

    #[test]
    fn failures_change_nothing() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("t.txt"), "alpha\nbeta\n").unwrap();
        fs::write(d.path().join("bin"), b"a\0b").unwrap();
        fs::write(d.path().join("latin1"), b"caf\xe9").unwrap();
        fs::create_dir(d.path().join("dir")).unwrap();
        for (args, says) in [
            (json!({"path": "t.txt", "edits": [{"find": "alpha", "replace": "A"}, {"find": "gamma", "replace": "G"}]}), "edit 2"),
            (json!({"path": "t.txt", "edits": [{"find": "alpha", "replace": "alpha"}]}), "change nothing"),
            (json!({"path": "t.txt", "edits": []}), "empty"),
            (json!({"path": "t.txt"}), "missing `edits`"),
            (json!({"path": "t.txt", "edits": [{"replace": "x"}]}), "edit 1"),
            (json!({"path": "t.txt", "edits": 3}), "list"),
            (json!({"path": "t.txt", "edits": [{"find": "a", "replace": "b"}], "replace_all": 7}), "true or false"),
            (json!({"path": "nope.txt", "edits": [{"find": "a", "replace": "b"}]}), "write_file"),
            (json!({"path": "bin", "edits": [{"find": "a", "replace": "b"}]}), "binary"),
            (json!({"path": "latin1", "edits": [{"find": "caf", "replace": "b"}]}), "UTF-8"),
            (json!({"path": "dir", "edits": [{"find": "a", "replace": "b"}]}), "folder"),
            (json!({"path": "../t.txt", "edits": [{"find": "a", "replace": "b"}]}), ""),
            (json!({"path": ".byob/x", "edits": [{"find": "a", "replace": "b"}]}), "reserved"),
        ] {
            let out = run(&EditFile, d.path(), args.clone());
            assert!(out.failed && out.changes.is_empty() && out.text.contains(says), "{args}: {}", out.text);
        }
        assert_eq!(fs::read_to_string(d.path().join("t.txt")).unwrap(), "alpha\nbeta\n");
    }

    #[cfg(unix)]
    #[test]
    fn never_edits_through_links() {
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("victim"), "safe").unwrap();
        let d = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path().join("victim"), d.path().join("link")).unwrap();
        std::os::unix::fs::symlink(outside.path(), d.path().join("outdir")).unwrap();
        for p in ["link", "outdir/victim"] {
            let out = run(&EditFile, d.path(), json!({"path": p, "edits": [{"find": "safe", "replace": "pwned"}]}));
            assert!(out.failed, "{p}");
        }
        assert_eq!(fs::read_to_string(outside.path().join("victim")).unwrap(), "safe");
    }

    #[test]
    fn undo_restores_the_file() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().canonicalize().unwrap();
        fs::write(root.join("n.md"), "# Title\nbody\n").unwrap();
        let journal = crate::Journal::new(&root, "turn1");
        let out = run_journaled(&EditFile, &root, &journal, json!({"path": "n.md", "edits": [{"find": "body", "replace": "new body"}]}));
        assert!(!out.failed, "{}", out.text);
        let ops = journal.take();
        assert!(matches!(ops.as_slice(), [UndoOp::Replaced { .. }]), "{ops:?}");
        crate::undo::undo(&ops, &root).unwrap();
        assert_eq!(fs::read_to_string(root.join("n.md")).unwrap(), "# Title\nbody\n");
    }

    #[test]
    fn previews_a_diff() {
        use crate::preview::{DiffLine, LineKind};
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("old.md"), "one\ntwo\nthree\n").unwrap();
        let p = EditFile.preview(&json!({"path": "old.md", "edits": [{"find": "two", "replace": "TWO"}]}), d.path()).unwrap();
        let Preview::Write { path, replaces: true, lines, .. } = p else { panic!("{p:?}") };
        assert_eq!(path, "old.md");
        assert!(lines.contains(&DiffLine { kind: LineKind::Removed, text: "two".into() }), "{lines:?}");
        assert!(lines.contains(&DiffLine { kind: LineKind::Added, text: "TWO".into() }), "{lines:?}");
        // edits that don't apply: the arguments
        let p = EditFile.preview(&json!({"path": "old.md", "edits": [{"find": "four", "replace": "x"}]}), d.path());
        assert!(matches!(p, Some(Preview::Args { .. })), "{p:?}");
        assert!(matches!(EditFile.preview(&json!({"path": "/etc/passwd", "edits": []}), d.path()), Some(Preview::Args { .. })));
    }

    #[test]
    fn titles() {
        assert_eq!(EditFile.title(&json!({"path": "a/notes.md"})), "Edited notes.md");
        assert_eq!(EditFile.pending_title(&json!({"path": "a/notes.md"})), "Edit notes.md");
        assert_eq!(EditFile.access(&json!({})), Access::Write);
    }
}
