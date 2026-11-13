//! byob's memory: the bundled `memory` plugin's backend, in-process. Memory is plain Markdown
//! the user can read and edit in any editor:
//!
//! - `<config>/memory.md`: notes for every chat (byob's config folder);
//! - `<workspace>/.byob/memory.md`: notes for one workspace.
//!
//! Each `- ` line is one note; anything else in the files (headings, prose) is kept as it is.
//! A `before_prompt` hook puts the start of both files ([`PROMPT_CAP`] bytes each) into the
//! system prompt, `remember(text, scope?)` adds a note and `forget(text, scope?)` removes one
//! (by its text, a unique part of it, or its number), and a pane shows and edits both.
//!
//! The plugin's manifest (`crates/bundled/plugins/memory`) names it with `"backend":
//! {"native": "memory"}`; the app registers [`factory`] under [`ID`].
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use byob_plugins::native::{NativeBackend, NativeFactory, Outbox};
use serde_json::{Value, json};

/// The id manifests name (`"backend": {"native": "memory"}`).
pub const ID: &str = "memory";
/// The tools it serves, in the manifest's order.
pub const TOOLS: [&str; 2] = ["remember", "forget"];
/// The pane it draws.
pub const VIEW: &str = "memory";
/// Most bytes of each file that go into the system prompt.
pub const PROMPT_CAP: usize = 4096;
/// Largest memory file `remember` still adds to.
pub const MAX_FILE: usize = 64 * 1024;
/// Longest note (characters).
pub const MAX_NOTE_CHARS: usize = 500;
/// The workspace file, relative to the workspace.
pub const WORKSPACE_FILE: &str = ".byob/memory.md";

const HEADING: &str = "# Memory";
const INTRO: &str = "Notes byob keeps across chats. Edit freely: each \"- \" line is one note.";

/// Which file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Every chat (`<config>/memory.md`).
    Global,
    /// One workspace (`<workspace>/.byob/memory.md`).
    Workspace,
}

impl Scope {
    fn parse(s: Option<&str>) -> Result<Option<Self>, String> {
        match s.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            None | Some("") => Ok(None),
            Some("global" | "user" | "me" | "everywhere" | "all") => Ok(Some(Self::Global)),
            Some("workspace" | "project" | "local" | "here") => Ok(Some(Self::Workspace)),
            Some(other) => Err(format!("scope must be \"global\" or \"workspace\", not {other:?}")),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Workspace => "workspace",
        }
    }
}

/// Where the files are.
#[derive(Debug, Clone, Default)]
pub struct Files {
    /// byob's config folder (`None`: there is no global memory).
    pub config: Option<PathBuf>,
    /// The workspace in question (`None`: no workspace memory).
    pub workspace: Option<PathBuf>,
}

impl Files {
    pub fn path(&self, scope: Scope) -> Option<PathBuf> {
        match scope {
            Scope::Global => self.config.as_ref().map(|c| c.join("memory.md")),
            Scope::Workspace => self.workspace.as_ref().map(|w| w.join(WORKSPACE_FILE)),
        }
    }

    fn need(&self, scope: Scope) -> Result<PathBuf, String> {
        self.path(scope).ok_or_else(|| match scope {
            Scope::Global => "there is no config folder for global memory; use scope \"workspace\"".to_owned(),
            Scope::Workspace => "this chat has no workspace; use scope \"global\"".to_owned(),
        })
    }
}

/// The notes of a file: (line index, text) of each `- ` / `* ` line.
fn notes(text: &str) -> Vec<(usize, String)> {
    text.lines()
        .enumerate()
        .filter_map(|(i, l)| {
            let t = l.trim_start();
            let rest = t.strip_prefix("- ").or_else(|| t.strip_prefix("* "))?;
            let rest = rest.trim();
            (!rest.is_empty()).then(|| (i, rest.to_owned()))
        })
        .collect()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Write `text` to `path` (via a temporary file, so a crash never leaves half a file).
fn write(path: &Path, text: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("can't make {}: {e}", dir.display()))?;
    }
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("can't write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("can't write {}: {e}", path.display()))
}

/// `text` on one line, at most [`MAX_NOTE_CHARS`].
fn one_line(text: &str) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let joined = joined.trim_start_matches(['-', '*', ' ']).to_owned();
    match joined.char_indices().nth(MAX_NOTE_CHARS) {
        Some((i, _)) => joined[..i].to_owned(),
        None => joined,
    }
}

fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase().trim_end_matches(['.', '!']).to_owned()
}

/// Add note `text` to the file of `scope`.
pub fn remember(files: &Files, scope: Scope, text: &str) -> Result<String, String> {
    let note = one_line(text);
    if note.is_empty() {
        return Err("`text` is empty: say what to remember".into());
    }
    let path = files.need(scope)?;
    let mut body = read(&path);
    if notes(&body).iter().any(|(_, n)| norm(n) == norm(&note)) {
        return Ok(format!("Already remembered ({}).", scope.label()));
    }
    if body.len() > MAX_FILE {
        return Err(format!("{} memory is full ({} KB); forget old notes first", scope.label(), MAX_FILE / 1024));
    }
    if body.trim().is_empty() {
        body = format!("{HEADING}\n\n{INTRO}\n\n");
    } else if !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str("- ");
    body.push_str(&note);
    body.push('\n');
    write(&path, &body)?;
    let mut msg = format!("Remembered ({}).", scope.label());
    if body.len() > PROMPT_CAP {
        msg.push_str(&format!(" That memory is over {} KB now, so only its start reaches the prompt: forget notes that no longer matter.", PROMPT_CAP / 1024));
    }
    Ok(msg)
}

/// Remove the notes at `lines` (line indices) from `path`.
fn drop_lines(path: &Path, lines: &[usize]) -> Result<(), String> {
    let body = read(path);
    let mut out: String = body.lines().enumerate().filter(|(i, _)| !lines.contains(i)).map(|(_, l)| format!("{l}\n")).collect();
    if !body.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }
    write(path, &out)
}

/// Remove a note: `query` is its text, a unique part of it, or its number (`3`, `#3`) in the
/// file of `scope`. Without a scope both files are searched.
pub fn forget(files: &Files, scope: Option<Scope>, query: &str) -> Result<String, String> {
    let q = query.trim();
    if q.is_empty() {
        return Err("`text` is empty: say which note to forget".into());
    }
    let scopes: Vec<Scope> = match scope {
        Some(s) => {
            files.need(s)?;
            vec![s]
        }
        None => [Scope::Global, Scope::Workspace].into_iter().filter(|s| files.path(*s).is_some()).collect(),
    };
    if let Ok(n) = q.trim_start_matches('#').parse::<usize>() {
        let [s] = scopes[..] else { return Err("give the scope with a note's number, or forget it by its text".into()) };
        let path = files.need(s)?;
        let all = notes(&read(&path));
        let Some((line, text)) = n.checked_sub(1).and_then(|i| all.get(i)) else {
            return Err(format!("{} memory has {} notes; there is no note {n}", s.label(), all.len()));
        };
        drop_lines(&path, &[*line])?;
        return Ok(format!("Forgot ({}): {text}", s.label()));
    }
    let want = norm(q);
    let mut exact: Vec<(Scope, usize, String)> = Vec::new();
    let mut part: Vec<(Scope, usize, String)> = Vec::new();
    for s in &scopes {
        let Some(path) = files.path(*s) else { continue };
        for (line, text) in notes(&read(&path)) {
            let n = norm(&text);
            if n == want {
                exact.push((*s, line, text));
            } else if n.contains(&want) {
                part.push((*s, line, text));
            }
        }
    }
    let hits = if exact.is_empty() { part } else { exact };
    match hits.len() {
        0 => Err(format!("no note matches {q:?}")),
        1 => {
            let (s, line, text) = &hits[0];
            drop_lines(&files.need(*s)?, &[*line])?;
            Ok(format!("Forgot ({}): {text}", s.label()))
        }
        _ if hits.iter().all(|h| norm(&h.2) == norm(&hits[0].2)) => {
            for s in [Scope::Global, Scope::Workspace] {
                let lines: Vec<usize> = hits.iter().filter(|h| h.0 == s).map(|h| h.1).collect();
                if !lines.is_empty() {
                    drop_lines(&files.need(s)?, &lines)?;
                }
            }
            Ok(format!("Forgot: {}", hits[0].2))
        }
        n => {
            let list: Vec<String> = hits.iter().take(5).map(|h| format!("- {} ({})", h.2, h.0.label())).collect();
            Err(format!("{q:?} matches {n} notes; give more of the text:\n{}", list.join("\n")))
        }
    }
}

/// The part of a memory file for the prompt: without byob's heading and intro, at most
/// [`PROMPT_CAP`] bytes.
fn prompt_body(text: &str) -> String {
    let mut body: Vec<&str> = text.lines().collect();
    if body.first().is_some_and(|l| l.trim() == HEADING) {
        body.remove(0);
    }
    body.retain(|l| l.trim() != INTRO);
    let body = body.join("\n");
    let body = body.trim();
    if body.len() <= PROMPT_CAP {
        return body.to_owned();
    }
    let mut cut = PROMPT_CAP;
    while !body.is_char_boundary(cut) {
        cut -= 1;
    }
    let head = &body[..cut];
    let head = head.rfind('\n').map_or(head, |i| &head[..i]);
    format!("{head}\n(… more notes not shown)")
}

/// The system-prompt section.
pub fn prompt(files: &Files) -> String {
    let mut out = String::from(
        "Memory: notes kept across chats (the user can edit them too). When the user states a lasting preference or fact, save it with remember (scope \"workspace\" if it's only about this project); not task details or secrets. forget removes a wrong or outdated note.",
    );
    let global = files.path(Scope::Global).map(|p| prompt_body(&read(&p))).unwrap_or_default();
    let ws = files.path(Scope::Workspace).map(|p| prompt_body(&read(&p))).unwrap_or_default();
    if global.is_empty() && ws.is_empty() {
        out.push_str(" Nothing is saved yet.");
    }
    if !global.is_empty() {
        out.push_str("\nEverywhere:\n");
        out.push_str(&global);
    }
    if !ws.is_empty() {
        out.push_str("\nThis workspace:\n");
        out.push_str(&ws);
    }
    out
}

/// One file's card in the pane.
fn card(files: &Files, scope: Scope) -> Value {
    let (title, field) = match scope {
        Scope::Global => ("Everywhere", "new_global"),
        Scope::Workspace => ("This workspace", "new_workspace"),
    };
    let Some(path) = files.path(scope) else {
        let why = match scope {
            Scope::Global => "There is no config folder, so no global memory.",
            Scope::Workspace => "Open a chat in a workspace to see its memory.",
        };
        return json!({"type": "card", "title": title, "children": [{"type": "text", "text": why, "tone": "dim"}]});
    };
    let text = read(&path);
    let all = notes(&text);
    let mut children = vec![json!({"type": "text", "text": path.display().to_string(), "style": "mono", "tone": "dim"})];
    if all.is_empty() {
        children.push(json!({"type": "text", "text": "Nothing saved yet.", "tone": "dim"}));
    }
    for (i, (_, note)) in all.iter().enumerate() {
        children.push(json!({"type": "row", "children": [
            {"type": "text", "text": note},
            {"type": "button", "label": "Forget", "action": format!("forget:{}:{i}", scope.label())},
        ]}));
    }
    let others = text.lines().filter(|l| !l.trim().is_empty()).count().saturating_sub(all.len());
    if others > 2 {
        children.push(json!({"type": "text", "text": "The file has other text too; it goes into the prompt as written.", "style": "small", "tone": "dim"}));
    }
    children.push(json!({"type": "row", "children": [
        {"type": "input", "name": field, "placeholder": "Remember…", "submit": format!("add:{}", scope.label())},
        {"type": "button", "label": "Add", "action": format!("add:{}", scope.label())},
    ]}));
    json!({"type": "card", "title": title, "children": children})
}

/// The pane: both files, their notes with Forget buttons, a field to add one.
pub fn pane(files: &Files) -> Value {
    json!({"type": "column", "children": [
        {"type": "text", "text": "What byob remembers across chats. The files are plain Markdown: edit them in any editor too.", "style": "small", "tone": "dim"},
        card(files, Scope::Global),
        card(files, Scope::Workspace),
        {"type": "button", "label": "Reload", "action": "reload"},
    ]})
}

/// The memory backend's state.
pub struct Memory {
    config: Option<PathBuf>,
    /// The workspace the pane shows (the last one a client looked at).
    workspace: Option<PathBuf>,
    /// The pane was asked for (so redraws are worth sending).
    drawn: bool,
}

/// Makes a fresh backend (registered as [`ID`]); `config` is byob's config folder.
pub fn factory(config: Option<PathBuf>) -> NativeFactory {
    Arc::new(move || Box::new(Memory::new(config.clone())) as Box<dyn NativeBackend>)
}

fn workspace_of(v: &Value) -> Option<PathBuf> {
    v.get("workspace").and_then(Value::as_str).filter(|w| !w.is_empty()).map(PathBuf::from)
}

impl Memory {
    pub fn new(config: Option<PathBuf>) -> Self {
        Self { config, workspace: None, drawn: false }
    }

    fn files(&self, workspace: Option<PathBuf>) -> Files {
        Files { config: self.config.clone(), workspace }
    }

    fn pane_files(&self) -> Files {
        self.files(self.workspace.clone())
    }

    fn refresh(&self, out: &Outbox) {
        if self.drawn {
            out.view(VIEW, "", pane(&self.pane_files()));
        }
    }

    fn tool(&mut self, name: &str, args: &Value, workspace: Option<PathBuf>) -> Result<Value, String> {
        let files = self.files(workspace);
        let text = args.get("text").and_then(Value::as_str).unwrap_or_default();
        let scope = Scope::parse(args.get("scope").and_then(Value::as_str))?;
        let r = match name {
            "remember" => remember(&files, scope.unwrap_or(Scope::Global), text),
            "forget" => forget(&files, scope, text),
            other => return Err(format!("this backend has no tool {other:?} (known: {})", TOOLS.join(", "))),
        };
        r.map(Value::String)
    }

    fn view_event(&mut self, ev: &Value, out: &Outbox) {
        let action = ev.get("action").and_then(Value::as_str).unwrap_or_default();
        let values = ev.get("values").cloned().unwrap_or(Value::Null);
        let files = self.pane_files();
        let scope_of = |s: &str| if s == "workspace" { Scope::Workspace } else { Scope::Global };
        let mut clear = None;
        let result = if let Some(s) = action.strip_prefix("add:") {
            let field = if s == "workspace" { "new_workspace" } else { "new_global" };
            let text = values.get(field).and_then(Value::as_str).unwrap_or_default();
            clear = Some(field);
            if text.trim().is_empty() { Ok(String::new()) } else { remember(&files, scope_of(s), text) }
        } else if let Some(rest) = action.strip_prefix("forget:") {
            let (s, i) = rest.split_once(':').unwrap_or((rest, ""));
            match i.parse::<usize>() {
                Ok(i) => forget(&files, Some(scope_of(s)), &(i.saturating_add(1)).to_string()),
                Err(_) => Err(format!("bad action {action:?}")),
            }
        } else {
            Ok(String::new())
        };
        if let Err(e) = result {
            out.notice(&e, "error");
        }
        let tree = pane(&files);
        match clear {
            Some(field) => out.send(json!({"type": "view", "id": VIEW, "key": "", "view": tree, "values": {field: ""}, "client": ev.get("client")})),
            None => out.view(VIEW, "", tree),
        }
    }
}

impl NativeBackend for Memory {
    fn init(&mut self, _init: &Value, out: &Outbox) {
        out.subscribe(&["chat_opened"]);
    }

    fn event(&mut self, ev: &Value, out: &Outbox) {
        match ev.get("event").and_then(Value::as_str).unwrap_or_default() {
            "view" => {
                if let Some(w) = workspace_of(ev) {
                    self.workspace = Some(w);
                }
                self.drawn = true;
                self.view_event(ev, out);
            }
            "chat_opened" => {
                let w = workspace_of(ev);
                if w.is_some() && w != self.workspace {
                    self.workspace = w;
                    self.refresh(out);
                }
            }
            _ => {}
        }
    }

    fn request(&mut self, method: &str, params: &Value, _out: &Outbox) -> Result<Value, String> {
        match method {
            "tool" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
                self.tool(name, params.get("args").unwrap_or(&Value::Null), workspace_of(params))
            }
            "hook" if params.get("event").and_then(Value::as_str) == Some("before_prompt") => {
                let files = self.files(workspace_of(params));
                Ok(json!({"action": "modify", "add": [{"id": "memory", "text": prompt(&files)}]}))
            }
            "hook" => Ok(json!({"action": "continue"})),
            "view" => {
                if let Some(w) = workspace_of(params) {
                    self.workspace = Some(w);
                }
                self.drawn = true;
                Ok(pane(&self.pane_files()))
            }
            other => Err(format!("unknown request {other:?}")),
        }
    }

    fn after_request(&mut self, method: &str, out: &Outbox) {
        if method == "tool" {
            self.refresh(out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn files(dir: &Path) -> Files {
        Files { config: Some(dir.join("config")), workspace: Some(dir.join("ws")) }
    }

    #[test]
    fn remember_writes_plain_markdown_and_skips_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let f = files(dir.path());
        assert_eq!(remember(&f, Scope::Global, "Prefers  metric\nunits").unwrap(), "Remembered (global).");
        assert_eq!(remember(&f, Scope::Global, "prefers metric units.").unwrap(), "Already remembered (global).");
        remember(&f, Scope::Workspace, "- Uses pnpm").unwrap();
        let g = std::fs::read_to_string(dir.path().join("config/memory.md")).unwrap();
        assert_eq!(g, format!("{HEADING}\n\n{INTRO}\n\n- Prefers metric units\n"));
        let w = std::fs::read_to_string(dir.path().join("ws/.byob/memory.md")).unwrap();
        assert!(w.ends_with("\n- Uses pnpm\n"), "{w}");
        assert!(remember(&f, Scope::Global, "  ").is_err());
        let none = Files { config: None, workspace: None };
        assert!(remember(&none, Scope::Global, "x").unwrap_err().contains("scope \"workspace\""));
        assert!(remember(&none, Scope::Workspace, "x").unwrap_err().contains("no workspace"));
        // A file the user wrote by hand (no trailing newline) keeps its text.
        std::fs::write(dir.path().join("ws/.byob/memory.md"), "My notes\n\n- a").unwrap();
        remember(&f, Scope::Workspace, "b").unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("ws/.byob/memory.md")).unwrap(), "My notes\n\n- a\n- b\n");
    }

    #[test]
    fn forget_by_text_part_or_number_and_refuses_ambiguity() {
        let dir = tempfile::tempdir().unwrap();
        let f = files(dir.path());
        for n in ["likes tea", "likes green tea", "lives in Oslo"] {
            remember(&f, Scope::Global, n).unwrap();
        }
        remember(&f, Scope::Workspace, "likes tea").unwrap();
        // Exact text wins over parts, in both files at once.
        assert_eq!(forget(&f, None, "Likes tea").unwrap(), "Forgot: likes tea");
        assert!(!read(&dir.path().join("ws/.byob/memory.md")).contains("likes tea"));
        remember(&f, Scope::Global, "likes black tea").unwrap();
        let e = forget(&f, None, "tea").unwrap_err();
        assert!(e.contains("matches 2 notes"), "{e}");
        assert_eq!(forget(&f, None, "oslo").unwrap(), "Forgot (global): lives in Oslo");
        assert!(forget(&f, None, "paris").unwrap_err().contains("no note matches"));
        // By number, in one scope.
        assert!(forget(&f, None, "1").is_err());
        assert_eq!(forget(&f, Some(Scope::Global), "#2").unwrap(), "Forgot (global): likes black tea");
        assert!(forget(&f, Some(Scope::Global), "9").unwrap_err().contains("has 1 notes"));
        let g = read(&dir.path().join("config/memory.md"));
        assert!(g.starts_with(HEADING) && g.ends_with("- likes green tea\n"), "{g}");
    }

    #[test]
    fn the_prompt_holds_both_files_capped() {
        let dir = tempfile::tempdir().unwrap();
        let f = files(dir.path());
        assert!(prompt(&f).ends_with("Nothing is saved yet."));
        remember(&f, Scope::Global, "prefers metric units").unwrap();
        remember(&f, Scope::Workspace, "the data is in data/").unwrap();
        let p = prompt(&f);
        assert!(p.ends_with("\nEverywhere:\n- prefers metric units\nThis workspace:\n- the data is in data/"), "{p}");
        assert!(!p.contains(INTRO));
        let big: String = (0..400).map(|i| format!("- note number {i}\n")).collect();
        std::fs::write(dir.path().join("config/memory.md"), big).unwrap();
        let p = prompt(&f);
        assert!(p.len() < 2 * PROMPT_CAP, "{}", p.len());
        assert!(p.contains("(… more notes not shown)"));
        assert!(remember(&f, Scope::Global, "one more").unwrap().contains("only its start"));
    }

    fn collect() -> (Outbox, Arc<Mutex<Vec<Value>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = Arc::clone(&seen);
        (Outbox::new(move |m| s.lock().unwrap().push(m)), seen)
    }

    #[test]
    fn the_backend_serves_tools_hook_and_pane() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        let ws_s = ws.display().to_string();
        let mut m = Memory::new(Some(dir.path().join("config")));
        let (out, seen) = collect();
        let r = m.request("tool", &json!({"name": "remember", "args": {"text": "uses metric", "scope": "workspace"}, "workspace": ws_s}), &out).unwrap();
        assert_eq!(r, "Remembered (workspace).");
        assert!(m.request("tool", &json!({"name": "remember", "args": {"text": "x", "scope": "galaxy"}}), &out).is_err());
        assert!(m.request("tool", &json!({"name": "nope", "args": {}}), &out).is_err());
        let hook = m.request("hook", &json!({"event": "before_prompt", "chat": "c", "workspace": ws_s, "sections": []}), &out).unwrap();
        assert_eq!(hook["action"], "modify");
        assert!(hook["add"][0]["text"].as_str().unwrap().contains("This workspace:\n- uses metric"));
        // The pane: drawn once asked for, redrawn after tools.
        assert!(seen.lock().unwrap().is_empty());
        let tree = m.request("view", &json!({"site": "pane", "view": "memory", "key": "", "workspace": ws_s}), &out).unwrap();
        let (_, faults) = byob_protocol::tree::decode(tree.clone());
        assert!(faults.is_empty(), "{faults:?}");
        assert!(tree.to_string().contains("uses metric"));
        m.request("tool", &json!({"name": "remember", "args": {"text": "likes tea"}, "workspace": ws_s}), &out).unwrap();
        m.after_request("tool", &out);
        assert!(seen.lock().unwrap().last().unwrap()["view"].to_string().contains("likes tea"));
        // Pane events: add, forget.
        m.event(
            &json!({"event": "view", "view": "memory", "action": "add:global", "values": {"new_global": "lives in Oslo"}, "client": 3, "workspace": ws_s}),
            &out,
        );
        let last = seen.lock().unwrap().last().unwrap().clone();
        assert_eq!(last["values"], json!({"new_global": ""}));
        assert!(last["view"].to_string().contains("lives in Oslo"));
        m.event(&json!({"event": "view", "view": "memory", "action": "forget:workspace:0", "values": {}, "workspace": ws_s}), &out);
        assert!(!read(&ws.join(WORKSPACE_FILE)).contains("uses metric"));
        // Opening a chat in another workspace redraws the pane for it.
        let n = seen.lock().unwrap().len();
        m.event(&json!({"event": "chat_opened", "chat": "c2", "client": 3, "workspace": dir.path().join("other").display().to_string()}), &out);
        assert_eq!(seen.lock().unwrap().len(), n + 1);
        assert!(!seen.lock().unwrap().last().unwrap()["view"].to_string().contains("uses metric"));
    }
}
