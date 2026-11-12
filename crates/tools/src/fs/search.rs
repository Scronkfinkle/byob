//! `search_files`: find text in the workspace's files (and files whose name matches),
//! skipping what `.gitignore` and hidden folders hide. Output is capped for small models.

use std::fmt::Write as _;
use std::path::Path;

use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use regex::{Regex, RegexBuilder};
use serde_json::{Value, json};

use super::{arg_str, clip, opt_string, req_string};
use crate::{Access, Tool, ToolContext, ToolOutput, ToolSpec, sandbox};

/// Matching lines shown in all.
pub const MAX_LINES: usize = 50;
/// Matching lines shown per file.
const MAX_PER_FILE: usize = 8;
/// File-name matches shown.
const MAX_NAMES: usize = 20;
/// Characters of a matching line shown.
const MAX_LINE_CHARS: usize = 160;
/// Output text, in bytes, besides the closing notes.
const MAX_OUTPUT: usize = 12 * 1024;
/// Bigger files are skipped.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// Files looked at before giving up.
const MAX_FILES: usize = 20_000;
/// Bytes read before giving up.
const MAX_SCAN_BYTES: u64 = 256 * 1024 * 1024;
/// Bytes sniffed to tell text from binary.
const SNIFF: usize = 8192;
/// Compiled regex size limit (hostile patterns).
const REGEX_LIMIT: usize = 1 << 20;

pub struct SearchFiles;

impl Tool for SearchFiles {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "search_files".into(),
            description:
                "Search the workspace's files for text; lists file:line: matching lines and files whose name matches. Skips .gitignore'd and hidden files."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Text to find (case-insensitive unless it has capitals)"},
                    "path": {"type": "string", "description": "Folder or file to search; default the whole workspace"},
                    "glob": {"type": "string", "description": "Only files like this, e.g. *.py or src/**/*.ts"},
                    "regex": {"type": "boolean", "description": "pattern is a regular expression"}
                },
                "required": ["pattern"]
            }),
        }
    }

    fn title(&self, args: &Value) -> String {
        format!("Searched for \u{201c}{}\u{201d}", clip(arg_str(args, "pattern").unwrap_or(""), 40))
    }

    fn pending_title(&self, args: &Value) -> String {
        format!("Search for \u{201c}{}\u{201d}", clip(arg_str(args, "pattern").unwrap_or(""), 40))
    }

    /// The title says it all.
    fn preview(&self, _args: &Value, _workspace: &Path) -> Option<crate::Preview> {
        None
    }

    fn access(&self, _args: &Value) -> Access {
        Access::Read
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        match search(args, ctx) {
            Ok(text) => ToolOutput::ok(text),
            Err(e) => ToolOutput::err(e),
        }
    }
}

fn opt_bool(args: &Value, key: &str) -> Result<bool, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(Value::String(s)) if s.eq_ignore_ascii_case("true") => Ok(true),
        Some(Value::String(s)) if s.eq_ignore_ascii_case("false") || s.is_empty() => Ok(false),
        Some(_) => Err(format!("invalid arguments: `{key}` must be true or false")),
    }
}

/// Upper case anywhere but right after a backslash (`\S` is a class, not a capital).
fn has_capitals(pattern: &str, regex: bool) -> bool {
    let mut escaped = false;
    for c in pattern.chars() {
        if c.is_uppercase() && !(regex && escaped) {
            return true;
        }
        escaped = regex && c == '\\' && !escaped;
    }
    false
}

fn build(pattern: &str, regex: bool) -> Result<Regex, regex::Error> {
    let src = if regex { pattern.to_owned() } else { regex::escape(pattern) };
    RegexBuilder::new(&src).case_insensitive(!has_capitals(pattern, regex)).size_limit(REGEX_LIMIT).dfa_size_limit(REGEX_LIMIT).build()
}

/// What one search found.
#[derive(Default)]
struct Found {
    lines: Vec<String>,
    /// Bytes in `lines`.
    size: usize,
    /// Matching lines past the total cap.
    hidden: usize,
    matched_files: usize,
    names: Vec<String>,
    hidden_names: usize,
    files: usize,
    skipped_big: usize,
    /// Stopped early: limits or cancel.
    stopped: Option<&'static str>,
    scanned: u64,
}

fn search(args: &Value, ctx: &ToolContext<'_>) -> Result<String, String> {
    let pattern = match req_string(args, "pattern") {
        Ok(p) => p,
        Err(e) => opt_string(args, "query")?.ok_or(e)?,
    };
    if pattern.is_empty() {
        return Err("pattern is empty; give the text to look for".into());
    }
    if pattern.contains('\n') {
        return Err("pattern must be on one line; search for one line of the text".into());
    }
    let regex = opt_bool(args, "regex")?;
    let glob = opt_string(args, "glob")?.map(|g| g.trim().to_owned()).filter(|g| !g.is_empty() && g != "*" && g != "**");
    let path = opt_string(args, "path")?.unwrap_or_default();
    let target = sandbox::resolve_follow(ctx.workspace, &path)?;
    let meta = std::fs::metadata(&target.abs).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!("{} doesn't exist; use list_files to see what's there", target.show()),
        _ => format!("can't open {}: {e}", target.show()),
    })?;
    let re = build(&pattern, regex).map_err(|e| format!("invalid regex: {e}. Set regex to false to search for the plain text"))?;
    let run = |re: &Regex| -> Result<Found, String> {
        let mut found = Found::default();
        if meta.is_file() {
            let name = target.show();
            scan_file(&target.abs, &name, re, &mut found);
        } else {
            walk(&target.abs, &target.root, glob.as_deref(), re, ctx, &mut found)?;
        }
        Ok(found)
    };
    let mut found = run(&re)?;
    let mut note = "";
    // small models often send a regex without saying so
    if !regex
        && found.matched_files == 0
        && found.names.is_empty()
        && pattern.contains(['.', '*', '|', '+', '?', '[', '(', '\\', '^', '$'])
        && let Ok(re) = build(&pattern, true)
    {
        let again = run(&re)?;
        if again.matched_files > 0 || !again.names.is_empty() {
            found = again;
            note = "No plain-text matches; searched it as a regex.\n";
        }
    }
    Ok(report(&pattern, &target.show(), meta.is_file(), glob.as_deref(), note, &found))
}

fn walk(dir: &Path, root: &Path, glob: Option<&str>, re: &Regex, ctx: &ToolContext<'_>, found: &mut Found) -> Result<(), String> {
    let mut b = WalkBuilder::new(dir);
    b.hidden(true).git_ignore(true).git_exclude(true).git_global(true).ignore(true).parents(true).require_git(false).follow_links(false);
    b.sort_by_file_name(std::ffi::OsStr::cmp);
    if let Some(g) = glob {
        let mut o = OverrideBuilder::new(dir);
        for part in split_globs(g) {
            o.add(part).map_err(|e| format!("invalid glob {part:?}: {e}"))?;
        }
        b.overrides(o.build().map_err(|e| format!("invalid glob {g:?}: {e}"))?);
    }
    for entry in b.build() {
        if ctx.cancelled() {
            found.stopped = Some("cancelled");
            break;
        }
        if found.files >= MAX_FILES || found.scanned >= MAX_SCAN_BYTES {
            found.stopped = Some("too many files");
            break;
        }
        let Ok(entry) = entry else { continue };
        // links are skipped: they may lead out of the workspace
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let p = entry.path();
        let shown = p.strip_prefix(root).map(sandbox::slash).unwrap_or_else(|_| p.display().to_string());
        if shown.split('/').next() == Some(sandbox::RESERVED_DIR) && !sandbox::EDITABLE_DIRS.iter().any(|d| shown.split('/').nth(1) == Some(*d)) {
            continue;
        }
        if p.file_name().is_some_and(|n| re.is_match(&n.to_string_lossy())) {
            if found.names.len() < MAX_NAMES {
                found.names.push(shown.clone());
            } else {
                found.hidden_names = found.hidden_names.saturating_add(1);
            }
        }
        scan_file(p, &shown, re, found);
    }
    Ok(())
}

/// `*.rs,*.toml` or `*.rs *.toml` (but not inside `{a,b}`).
fn split_globs(g: &str) -> Vec<&str> {
    if g.contains('{') {
        return vec![g];
    }
    g.split([',', ' ']).map(str::trim).filter(|s| !s.is_empty()).collect()
}

fn scan_file(p: &Path, shown: &str, re: &Regex, found: &mut Found) {
    found.files = found.files.saturating_add(1);
    let Ok(meta) = std::fs::metadata(p) else { return };
    if meta.len() > MAX_FILE_BYTES {
        found.skipped_big = found.skipped_big.saturating_add(1);
        return;
    }
    let Ok(data) = std::fs::read(p) else { return };
    found.scanned = found.scanned.saturating_add(data.len() as u64);
    if data.get(..SNIFF.min(data.len())).is_some_and(|h| h.contains(&0)) {
        return;
    }
    let text = String::from_utf8_lossy(&data);
    let mut in_file = 0usize;
    let mut over = 0usize;
    for (i, line) in text.split('\n').enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let Some(m) = re.find(line) else { continue };
        if in_file == 0 {
            found.matched_files = found.matched_files.saturating_add(1);
        }
        in_file = in_file.saturating_add(1);
        if found.lines.len() >= MAX_LINES || found.size >= MAX_OUTPUT {
            found.hidden = found.hidden.saturating_add(1);
        } else if in_file > MAX_PER_FILE {
            over = over.saturating_add(1);
        } else {
            let row = format!("{shown}:{}: {}", i.saturating_add(1), excerpt(line, m.start()));
            found.size = found.size.saturating_add(row.len().saturating_add(1));
            found.lines.push(row);
        }
    }
    if over > 0 {
        found.lines.push(format!("{shown}: … {over} more in this file"));
    }
}

/// The line without its indentation, at most [`MAX_LINE_CHARS`], around the match at byte `at`.
fn excerpt(line: &str, at: usize) -> String {
    let trimmed = line.trim_start();
    let at = at.saturating_sub(line.len().saturating_sub(trimmed.len()));
    let trimmed = trimmed.trim_end();
    let count = trimmed.chars().count();
    let clean = |s: &str| s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>();
    if count <= MAX_LINE_CHARS {
        return clean(trimmed);
    }
    let at_char = trimmed.get(..at).map_or(0, |s| s.chars().count());
    let start = at_char.saturating_sub(MAX_LINE_CHARS / 4);
    let start = start.min(count.saturating_sub(MAX_LINE_CHARS));
    let body: String = trimmed.chars().skip(start).take(MAX_LINE_CHARS).collect();
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.push_str(&clean(&body));
    if start.saturating_add(MAX_LINE_CHARS) < count {
        out.push('…');
    }
    out
}

fn report(pattern: &str, where_: &str, is_file: bool, glob: Option<&str>, note: &str, f: &Found) -> String {
    let mut out = String::from(note);
    if f.lines.is_empty() && f.names.is_empty() {
        let scope = if is_file || where_ == "." { String::new() } else { format!(" in {where_}") };
        let globbed = glob.map(|g| format!(" ({g} files)")).unwrap_or_default();
        let _ = write!(out, "No matches for {pattern:?}{scope}{globbed}; searched {} file{}", f.files, if f.files == 1 { "" } else { "s" });
        if !is_file {
            out.push_str(", skipping hidden and .gitignore'd ones");
        }
        out.push('.');
    } else {
        out.push_str(&f.lines.join("\n"));
        if !f.names.is_empty() {
            if !f.lines.is_empty() {
                out.push('\n');
            }
            let _ = write!(out, "Files whose name matches: {}", f.names.join(", "));
            if f.hidden_names > 0 {
                let _ = write!(out, " and {} more", f.hidden_names);
            }
        }
        if f.hidden > 0 {
            let _ = write!(out, "\n… {} more matching lines ({} files match in all). Narrow it with path or glob.", f.hidden, f.matched_files);
        }
    }
    if f.skipped_big > 0 {
        let _ = write!(out, "\n{} files over {} MB were skipped.", f.skipped_big, MAX_FILE_BYTES / (1024 * 1024));
    }
    if let Some(why) = f.stopped {
        let _ = write!(out, "\nStopped early ({why}) after {} files; narrow it with path or glob.", f.files);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::fs::testutil::run;

    fn ws() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        fs::create_dir_all(p.join("src/deep")).unwrap();
        fs::create_dir_all(p.join("target/debug")).unwrap();
        fs::create_dir_all(p.join(".git")).unwrap();
        fs::create_dir_all(p.join(".byob/undo")).unwrap();
        fs::create_dir_all(p.join(".byob/skills/s")).unwrap();
        fs::write(p.join(".gitignore"), "target/\n*.log\n").unwrap();
        fs::write(p.join("src/main.rs"), "fn main() {\n    let total = add(1, 2);\n    println!(\"{total}\");\n}\n").unwrap();
        fs::write(p.join("src/deep/add.rs"), "pub fn add(a: i32, b: i32) -> i32 {\r\n    a + b\r\n}\r\n").unwrap();
        fs::write(p.join("README.md"), "# Adder\nCall add to add.\n").unwrap();
        fs::write(p.join("target/debug/out.rs"), "fn add() {}\n").unwrap();
        fs::write(p.join("run.log"), "add add add\n").unwrap();
        fs::write(p.join(".git/config"), "add\n").unwrap();
        fs::write(p.join(".byob/undo/x"), "add\n").unwrap();
        fs::write(p.join(".byob/skills/s/SKILL.md"), "add skill\n").unwrap();
        fs::write(p.join("blob.bin"), b"add\0\x01\x02").unwrap();
        fs::write(p.join("notes.txt"), "Ünïcödé line: grüße add\nnaïve\n").unwrap();
        d
    }

    fn search(ws: &Path, args: Value) -> String {
        let out = run(&SearchFiles, ws, args.clone());
        assert!(!out.failed, "{args}: {}", out.text);
        out.text
    }

    #[test]
    fn finds_lines_and_names_and_respects_ignores() {
        let d = ws();
        let t = search(d.path(), json!({"pattern": "add"}));
        assert!(t.contains("src/main.rs:2: let total = add(1, 2);"), "{t}");
        assert!(t.contains("src/deep/add.rs:1: pub fn add(a: i32, b: i32) -> i32 {"), "CRLF stripped: {t}");
        assert!(t.contains("README.md:1: # Adder") && t.contains("README.md:2: Call add to add."), "smart case: {t}");
        assert!(t.contains("Files whose name matches: src/deep/add.rs"), "{t}");
        assert!(t.contains("notes.txt:1: Ünïcödé line: grüße add"), "{t}");
        for hidden in ["target/", "run.log", ".git", ".byob/undo", "blob.bin"] {
            assert!(!t.contains(hidden), "{hidden}: {t}");
        }
    }

    #[test]
    fn case_regex_glob_and_path() {
        let d = ws();
        let t = search(d.path(), json!({"pattern": "Adder"}));
        assert_eq!(t, "README.md:1: # Adder");
        let t = search(d.path(), json!({"pattern": r"fn \w+\(", "regex": true}));
        assert!(t.contains("src/main.rs:1:") && t.contains("src/deep/add.rs:1:"), "{t}");
        let t = search(d.path(), json!({"pattern": "add", "glob": "*.md"}));
        assert_eq!(t, "README.md:1: # Adder\nREADME.md:2: Call add to add.");
        let t = search(d.path(), json!({"pattern": "add", "glob": "*.md, *.txt"}));
        assert!(t.contains("README.md") && t.contains("notes.txt") && !t.contains(".rs"), "{t}");
        let t = search(d.path(), json!({"pattern": "add", "path": "src/deep"}));
        assert!(t.starts_with("src/deep/add.rs:1:"), "{t}");
        let t = search(d.path(), json!({"pattern": "a + b", "path": "src/deep/add.rs"}));
        assert_eq!(t, "src/deep/add.rs:2: a + b");
        // an unsaid regex is retried as one
        let t = search(d.path(), json!({"pattern": "let .* = add"}));
        assert!(t.contains("as a regex") && t.contains("src/main.rs:2:"), "{t}");
        // the workspace's own skills are searchable when asked for
        let t = search(d.path(), json!({"pattern": "skill", "path": ".byob/skills"}));
        assert!(t.contains("SKILL.md:1: add skill"), "{t}");
    }

    #[test]
    fn unicode_matches_case_insensitively() {
        let d = ws();
        let t = search(d.path(), json!({"pattern": "grüße"}));
        assert_eq!(t, "notes.txt:1: Ünïcödé line: grüße add");
        let t = search(d.path(), json!({"pattern": "ünïcödé"}));
        assert!(t.contains("notes.txt:1:"), "{t}");
    }

    #[test]
    fn no_matches_and_errors() {
        let d = ws();
        let t = search(d.path(), json!({"pattern": "zzz_nothing"}));
        assert!(t.starts_with("No matches for \"zzz_nothing\"") && t.contains("gitignore"), "{t}");
        for args in [
            json!({}),
            json!({"pattern": ""}),
            json!({"pattern": "a\nb"}),
            json!({"pattern": "(", "regex": true}),
            json!({"pattern": "x", "path": "nope"}),
            json!({"pattern": "x", "path": ".."}),
            json!({"pattern": "x", "glob": "["}),
            json!({"pattern": "x", "regex": 5}),
        ] {
            assert!(run(&SearchFiles, d.path(), args.clone()).failed, "{args}");
        }
    }

    #[test]
    fn output_is_capped() {
        let d = tempfile::tempdir().unwrap();
        for f in 0..20 {
            let body: String = (0..30).map(|i| format!("needle {f} {i}\n")).collect();
            fs::write(d.path().join(format!("f{f:02}.txt")), body).unwrap();
        }
        fs::write(d.path().join("long.txt"), format!("{}needle{}\n", "x".repeat(500), "y".repeat(500))).unwrap();
        let t = search(d.path(), json!({"pattern": "needle"}));
        let rows = t.lines().filter(|l| l.contains(": needle")).count();
        assert!(rows <= MAX_LINES, "{rows}: {t}");
        assert!(t.contains("f00.txt: … 22 more in this file"), "{t}");
        assert!(t.contains("more matching lines (21 files match in all)") && t.contains("Narrow it"), "{t}");
        let t = search(d.path(), json!({"pattern": "needle", "path": "long.txt"}));
        assert!(t.starts_with("long.txt:1: …x") && t.contains("needle") && t.ends_with('…'), "{t}");
        assert!(t.chars().count() < MAX_LINE_CHARS + 20, "{t}");
    }

    #[test]
    fn gitignore_applies_without_a_git_folder() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join(".gitignore"), "build/\n").unwrap();
        fs::create_dir(d.path().join("build")).unwrap();
        fs::write(d.path().join("build/x.txt"), "hello\n").unwrap();
        fs::write(d.path().join("y.txt"), "hello\n").unwrap();
        assert_eq!(search(d.path(), json!({"pattern": "hello"})), "y.txt:1: hello");
    }

    #[cfg(unix)]
    #[test]
    fn never_follows_links_out() {
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), "SECRET\n").unwrap();
        let d = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), d.path().join("out")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), d.path().join("s.txt")).unwrap();
        let t = search(d.path(), json!({"pattern": "SECRET"}));
        assert!(t.starts_with("No matches"), "{t}");
        assert!(run(&SearchFiles, d.path(), json!({"pattern": "SECRET", "path": "out"})).failed);
    }

    #[test]
    fn titles() {
        assert_eq!(SearchFiles.title(&json!({"pattern": "foo"})), "Searched for \u{201c}foo\u{201d}");
        assert_eq!(SearchFiles.access(&json!({})), Access::Read);
    }
}
