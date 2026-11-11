//! A fake byob around the native sheets backend, in-process: it hands the backend the wire
//! messages byob sends (`byob_plugins::native::dispatch`), keeps every message it sends back,
//! and emulates byob's artifact store (`.byob/artifacts/<id>/artifact.json` + `v<n>` files
//! in a temp workspace), saving the versions tool results and grid drafts carry.
#![allow(dead_code)]

use std::path::Path;
use std::sync::{Arc, Mutex};

use byob_plugins::native::{NativeBackend, Outbox, dispatch};
use serde_json::{Value, json};

pub const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

fn content_type(suffix: &str) -> &'static str {
    match suffix {
        ".xlsx" => XLSX,
        ".csv" => "text/csv",
        ".tsv" => "text/tab-separated-values",
        _ => "application/octet-stream",
    }
}

pub struct Byob {
    b: Box<dyn NativeBackend>,
    out: Outbox,
    inbox: Arc<Mutex<Vec<Value>>>,
    /// Every message that wasn't a reply, in order.
    pub msgs: Vec<Value>,
    pub ws: tempfile::TempDir,
    pub chat: String,
    ids: u32,
    next_id: u64,
    /// The artifact the last tool call saved a version of.
    pub last_id: String,
}

impl Default for Byob {
    fn default() -> Self {
        Self::new()
    }
}

impl Byob {
    pub fn new() -> Self {
        let inbox = Arc::new(Mutex::new(Vec::new()));
        let i = Arc::clone(&inbox);
        let out = Outbox::new(move |m| i.lock().unwrap().push(m));
        let mut b = Self {
            b: (byob_sheets::factory())(),
            out,
            inbox,
            msgs: Vec::new(),
            ws: tempfile::tempdir().unwrap(),
            chat: "chat-1".into(),
            ids: 0,
            next_id: 0,
            last_id: String::new(),
        };
        b.send(json!({"type": "init", "v": 1, "plugin": "sheets", "name": "Spreadsheets", "version": "1.0.0", "byob": "test", "caps": [],
                      "events": [], "settings": {}, "dir": "", "scope": "global", "chat": null, "workspace": null, "restart": 0,
                      "views": [{"id": "sheet", "site": "artifact", "title": "Spreadsheet"}]}));
        b
    }

    pub fn ws(&self) -> String {
        self.ws.path().display().to_string()
    }

    /// Hand `msg` to the backend; returns its reply (if it was a request). Artifact
    /// messages (grid drafts) are saved like byob saves them.
    pub fn send(&mut self, msg: Value) -> Option<Value> {
        dispatch(self.b.as_mut(), &msg, &self.out);
        let got: Vec<Value> = std::mem::take(&mut *self.inbox.lock().unwrap());
        let mut reply = None;
        for m in got {
            if m["type"] == "reply" {
                reply = Some(m);
                continue;
            }
            if m["type"] == "artifact" {
                let draft = m["draft"].as_bool().unwrap_or(false);
                self.store(&m, draft);
            }
            self.msgs.push(m);
        }
        reply
    }

    pub fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next_id += 1;
        let r = self.send(json!({"type": "request", "id": self.next_id, "method": method, "params": params})).expect("a reply");
        assert_eq!(r["id"], self.next_id);
        match r.get("error") {
            Some(e) => Err(e.as_str().unwrap_or_default().to_owned()),
            None => Ok(r["result"].clone()),
        }
    }

    // -- the artifact store
    pub fn meta(&self, aid: &str) -> Value {
        serde_json::from_slice(&std::fs::read(self.ws.path().join(".byob/artifacts").join(aid).join("artifact.json")).unwrap()).unwrap()
    }

    pub fn latest(&self, aid: &str) -> u64 {
        self.meta(aid)["versions"].as_array().unwrap().iter().map(|v| v["n"].as_u64().unwrap()).max().unwrap()
    }

    /// Save a version from a tool result's `artifact` (or an artifact message): (id, n).
    pub fn store(&mut self, art: &Value, draft: bool) -> (String, u64) {
        let (aid, mut meta) = match art["id"].as_str() {
            Some(id) => (id.to_owned(), self.meta(id)),
            None => {
                self.ids += 1;
                let filename = art["filename"].as_str().unwrap_or("sheet");
                let stem = Path::new(filename).file_stem().unwrap().to_string_lossy().into_owned();
                let id = format!("{stem}-{:04x}", self.ids);
                let title = art["title"].as_str().filter(|t| !t.is_empty()).map_or(stem.clone(), str::to_owned);
                (
                    id.clone(),
                    json!({"chat": self.chat, "id": id, "kind": "file", "content_type": "", "title": title, "filename": art["filename"], "versions": []}),
                )
            }
        };
        if let Some(f) = art["filename"].as_str().filter(|f| !f.is_empty()) {
            meta["filename"] = json!(f);
        }
        if let Some(t) = art["title"].as_str().filter(|t| !t.is_empty()) {
            meta["title"] = json!(t);
        }
        let suffix = Path::new(meta["filename"].as_str().unwrap_or_default())
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
            .unwrap_or_default();
        meta["content_type"] = json!(content_type(&suffix));
        let dir = self.ws.path().join(".byob/artifacts").join(&aid);
        std::fs::create_dir_all(&dir).unwrap();
        let vs = meta["versions"].as_array_mut().unwrap();
        let reuse = draft && vs.last().is_some_and(|v| v["draft"] == true);
        if !reuse {
            let n = vs.len() + 1;
            vs.push(json!({"n": n, "note": art["note"].as_str().unwrap_or_default(), "draft": draft}));
        }
        let v = vs.last_mut().unwrap();
        let n = v["n"].as_u64().unwrap();
        let rel = format!(".byob/artifacts/{aid}/v{n}{suffix}");
        std::fs::copy(art["file"].as_str().unwrap(), self.ws.path().join(&rel)).unwrap();
        v["size"] = json!(std::fs::metadata(self.ws.path().join(&rel)).unwrap().len());
        v["path"] = json!(rel);
        std::fs::write(dir.join("artifact.json"), meta.to_string()).unwrap();
        (aid, n)
    }

    /// An artifact uploaded by the user.
    pub fn add_artifact(&mut self, filename: &str, data: &[u8]) -> String {
        let path = self.ws.path().join(format!("upload-{filename}"));
        std::fs::write(&path, data).unwrap();
        let (aid, _) = self.store(&json!({"filename": filename, "title": filename, "file": path}), false);
        std::fs::remove_file(path).unwrap();
        aid
    }

    /// A workspace file.
    pub fn file(&self, name: &str, data: &[u8]) {
        let p = self.ws.path().join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    }

    /// Call a tool: its text with byob's "Saved: …" line, as the model sees it; an error
    /// is "Error: …".
    pub fn tool(&mut self, name: &str, args: Value) -> String {
        let params = json!({"name": name, "args": args, "workspace": self.ws(), "chat": self.chat});
        match self.request("tool", params) {
            Err(e) => format!("Error: {e}"),
            Ok(r) if r["artifact"].is_object() => {
                let (aid, n) = self.store(&r["artifact"], false);
                self.last_id = aid.clone();
                format!("{}\nSaved: artifact {aid}, version {n}.", r["text"].as_str().unwrap_or_default())
            }
            Ok(Value::String(s)) => s,
            Ok(other) => other.to_string(),
        }
    }

    pub fn new_sheet(&mut self, title: &str) -> (String, String) {
        let text = self.tool("create_sheet", json!({"title": title}));
        (self.last_id.clone(), text)
    }

    /// What byob says a view key shows (`stale`: what it said when the client opened this
    /// key, before newer versions).
    pub fn artifact(&self, aid: &str, version: Option<u64>, stale: bool) -> Value {
        let meta = self.meta(aid);
        let latest = self.latest(aid);
        let n = version.unwrap_or(latest);
        let v = meta["versions"].as_array().unwrap().iter().find(|v| v["n"] == n).unwrap().clone();
        json!({"id": aid, "version": n, "latest": if stale { n } else { latest },
               "path": self.ws.path().join(v["path"].as_str().unwrap()), "filename": meta["filename"],
               "content_type": meta["content_type"], "title": meta["title"], "workspace": self.ws(), "chat": self.chat})
    }

    pub fn view_at(&mut self, aid: &str, version: Option<u64>, stale: bool) -> Value {
        let a = self.artifact(aid, version, stale);
        let key = format!("{aid}@{}", a["version"]);
        let chat = self.chat.clone();
        self.request("view", json!({"site": "artifact", "view": "sheet", "key": key, "client": "c1", "chat": chat, "artifact": a})).unwrap()
    }

    pub fn view(&mut self, aid: &str) -> Value {
        self.view_at(aid, None, false)
    }

    /// A grid event on the view of `aid` (newest version unless `version`); returns the
    /// messages it caused (redraws, drafts). `grid` holds the event's grid fields.
    pub fn event_at(&mut self, aid: &str, op: &str, version: Option<u64>, action: Option<&str>, stale: bool, grid: Value) -> Vec<Value> {
        let a = self.artifact(aid, version, stale);
        let key = format!("{aid}@{}", a["version"]);
        let start = self.msgs.len();
        let mut g = grid;
        g["name"] = json!("sheet");
        g["op"] = json!(op);
        let chat = self.chat.clone();
        self.send(json!({"type": "event", "event": "view", "view": "sheet", "site": "artifact", "key": key, "client": "c1", "chat": chat,
                         "action": action.unwrap_or(op), "values": {}, "artifact": a, "grid": g}));
        self.view_at(aid, version, stale); // as byob's client would, after the redraw
        self.msgs[start..].to_vec()
    }

    pub fn event(&mut self, aid: &str, op: &str, grid: Value) -> Vec<Value> {
        self.event_at(aid, op, None, None, false, grid)
    }
}

impl Drop for Byob {
    fn drop(&mut self) {
        self.send(json!({"type": "shutdown"}));
    }
}

/// The first `grid` node of a view tree.
pub fn grid_of(tree: &Value) -> Option<&Value> {
    if tree["type"] == "grid" {
        return Some(tree);
    }
    tree["children"].as_array()?.iter().find_map(grid_of)
}

/// Every `text` node's text.
pub fn texts_of(tree: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if tree["type"] == "text" {
        out.push(tree["text"].as_str().unwrap_or_default().to_owned());
    }
    for ch in tree["children"].as_array().into_iter().flatten() {
        out.extend(texts_of(ch));
    }
    out
}

/// The cell at 0-based (row, col) from the grid's blocks (`"missing"` if none has it).
pub fn cell(g: &Value, row: i64, col: i64) -> Value {
    for b in g["blocks"].as_array().unwrap() {
        let (r, c) = (row - b["row"].as_i64().unwrap(), col - b["col"].as_i64().unwrap());
        let cells = b["cells"].as_array().unwrap();
        if r >= 0 && (r as usize) < cells.len() {
            let line = cells[r as usize].as_array().unwrap();
            if c >= 0 && (c as usize) < line.len() {
                return line[c as usize].clone();
            }
        }
    }
    json!("missing")
}

/// A row of a TSV reply (the line starting with the row number), without the number.
pub fn tsv_row(text: &str, row: u32) -> Option<Vec<String>> {
    let want = row.to_string();
    text.lines().find_map(|l| {
        let mut parts = l.split('\t');
        (parts.next() == Some(want.as_str())).then(|| parts.map(str::to_owned).collect())
    })
}

/// The views among `msgs`.
pub fn views(msgs: &[Value]) -> Vec<&Value> {
    msgs.iter().filter(|m| m["type"] == "view").collect()
}

/// A small deterministic random source (xorshift) for generated test data.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// In `lo..=hi`.
    pub fn int(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next() % (hi - lo + 1)
    }

    pub fn pick<'a>(&mut self, of: &[&'a str]) -> &'a str {
        of[(self.next() % of.len() as u64) as usize]
    }
}
