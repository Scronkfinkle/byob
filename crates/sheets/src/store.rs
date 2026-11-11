//! Artifacts on disk (`.byob/artifacts/<id>/artifact.json` + version files, as byob writes
//! them), what a tool's `sheet` argument names, and the live workbooks in memory.

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::SheetError;
use crate::book::{Book, file_format};
use crate::text::{pystr, repr_str};

pub const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
pub const CSV: &str = "text/csv";
pub const TSV: &str = "text/tab-separated-values";
const SHEET_SUFFIXES: [&str; 4] = [".xlsx", ".csv", ".tsv", ".tab"];

fn id_ok(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty() && b.len() <= 128 && b[0].is_ascii_alphanumeric() && b.iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

fn art_dir(ws: &str, aid: &str) -> PathBuf {
    Path::new(ws).join(".byob").join("artifacts").join(aid)
}

pub fn read_meta(ws: &str, aid: &str) -> Option<Value> {
    if !id_ok(aid) || aid.contains("..") {
        return None;
    }
    let text = std::fs::read(art_dir(ws, aid).join("artifact.json")).ok()?;
    let v: Value = serde_json::from_slice(&text).ok()?;
    v.is_object().then_some(v)
}

pub fn metas(ws: &str) -> Vec<Value> {
    let root = Path::new(ws).join(".byob").join("artifacts");
    let Ok(rd) = std::fs::read_dir(&root) else { return Vec::new() };
    let mut ids: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    ids.sort();
    let mut out = Vec::new();
    for aid in ids {
        if let Some(mut m) = read_meta(ws, &aid) {
            if let Some(o) = m.as_object_mut() {
                o.entry("id").or_insert(Value::String(aid));
            }
            out.push(m);
        }
    }
    out
}

pub fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

/// Python's truthiness of a JSON value.
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(t)) => !t.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `meta.get(k) or fallback` as text.
fn or_text(v: &Value, k: &str, fallback: &str) -> String {
    match v.get(k) {
        Some(x) if truthy(Some(x)) => pystr(x),
        _ => fallback.to_owned(),
    }
}

/// (n, absolute path) of the newest version whose file exists.
fn latest(ws: &str, meta: &Value) -> (Option<i64>, Option<PathBuf>) {
    let mut vs: Vec<&Value> = meta.get("versions").and_then(Value::as_array).into_iter().flatten().filter(|v| v.is_object()).collect();
    let key = |v: &Value| v.get("n").and_then(Value::as_i64).filter(|_| v.get("n").is_some_and(Value::is_i64)).unwrap_or(-1);
    vs.sort_by_key(|v| std::cmp::Reverse(key(v)));
    for v in vs {
        if let Some(p) = s(v, "path").filter(|p| !p.is_empty()) {
            let ap = if Path::new(p).is_absolute() { PathBuf::from(p) } else { Path::new(ws).join(p) };
            if ap.is_file() {
                return (v.get("n").and_then(Value::as_i64), Some(ap));
            }
        }
    }
    (Some(0), None)
}

pub fn sheet_format(content_type: Option<&str>, path: &str) -> Option<&'static str> {
    let ct = content_type.unwrap_or_default().split(';').next().unwrap_or_default().trim();
    match ct {
        XLSX => return Some("xlsx"),
        CSV => return Some("csv"),
        TSV => return Some("tsv"),
        _ => {}
    }
    let low = path.to_lowercase();
    if !path.is_empty() && SHEET_SUFFIXES.iter().any(|x| low.ends_with(x)) {
        return Some(file_format(path));
    }
    None
}

fn is_sheet_meta(m: &Value) -> bool {
    sheet_format(s(m, "content_type"), &or_text(m, "filename", "")).is_some()
}

/// `stem.xlsx` of a file name.
pub fn xlsx_name(filename: &str, fallback: &str) -> String {
    let base = Path::new(filename).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
    let stem = splitext(&base).0;
    format!("{}.xlsx", if stem.is_empty() { fallback } else { stem })
}

/// Python's `os.path.splitext` of a base name.
pub fn splitext(base: &str) -> (&str, &str) {
    let lead = base.len() - base.trim_start_matches('.').len();
    match base[lead..].rfind('.') {
        Some(i) => base.split_at(lead + i),
        None => (base, ""),
    }
}

/// What a tool's `sheet` argument named: an artifact or a workspace file.
#[derive(Debug, Clone)]
pub struct Source {
    pub artifact: bool,
    pub path: PathBuf,
    pub fmt: &'static str,
    pub aid: String,
    pub meta: Value,
    pub version: Option<i64>,
    pub rel: String,
    pub note: String,
}

fn version_text(v: Option<i64>) -> String {
    v.map_or_else(|| "None".into(), |n| n.to_string())
}

impl Source {
    pub fn label(&self) -> String {
        if self.artifact {
            format!("\"{}\" ({}, version {})", or_text(&self.meta, "title", &self.aid), self.aid, version_text(self.version))
        } else {
            self.rel.clone()
        }
    }

    pub fn title(&self) -> String {
        if self.artifact {
            return or_text(&self.meta, "title", &self.aid);
        }
        let base = Path::new(&self.rel).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        splitext(&base).0.to_owned()
    }
}

fn artifact_source(ws: &str, meta: &Value, note: String) -> Result<Source, SheetError> {
    let aid = or_text(meta, "id", "");
    let (n, path) = latest(ws, meta);
    let Some(path) = path else { return Err(SheetError::Msg(format!("Artifact {aid} has no saved version on disk."))) };
    let Some(fmt) = sheet_format(s(meta, "content_type"), &path.to_string_lossy()) else {
        return Err(SheetError::Msg(format!("{aid} is a {} artifact, not a spreadsheet.", or_text(meta, "content_type", "non-spreadsheet"))));
    };
    Ok(Source { artifact: true, path, fmt, aid, meta: meta.clone(), version: n, rel: String::new(), note })
}

/// The artifact a write (or create_sheet from=) made from workspace file `rel`, if any.
fn derived(ws: &str, chat: Option<&str>, rel: &str) -> Option<Value> {
    let want = format!("From {rel}");
    let mut best = None;
    for m in metas(ws) {
        let first = m.get("versions").and_then(Value::as_array).and_then(|v| v.first()).filter(|v| v.is_object());
        if first.is_some_and(|v| s(v, "note") == Some(&want)) && is_sheet_meta(&m) {
            if s(&m, "chat") == chat {
                return Some(m);
            }
            if best.is_none() {
                best = Some(m);
            }
        }
    }
    best
}

fn sheet_artifacts(ws: &str, chat: Option<&str>) -> Vec<Value> {
    metas(ws).into_iter().filter(|m| is_sheet_meta(m) && (chat.is_none() || s(m, "chat") == chat)).collect()
}

/// A lexically normalized absolute path (Python's `os.path.abspath`).
fn normalize(p: &Path) -> PathBuf {
    let abs = if p.is_absolute() { p.to_path_buf() } else { std::env::current_dir().unwrap_or_default().join(p) };
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Python's `os.path.relpath(path, start)`, with `/` between components on every platform:
/// it is shown to the model and kept in artifact notes, so it reads the same everywhere. A
/// path with nothing in common with `start` (another drive) comes back whole, `/`-separated.
pub fn relpath(path: &Path, start: &Path) -> String {
    let (p, s) = (normalize(path), normalize(start));
    let pc: Vec<Component> = p.components().collect();
    let sc: Vec<Component> = s.components().collect();
    let common = pc.iter().zip(&sc).take_while(|(a, b)| a == b).count();
    if common == 0 {
        return p.to_string_lossy().replace('\\', "/");
    }
    let mut out: Vec<String> = vec!["..".to_owned(); sc.len() - common];
    out.extend(pc[common..].iter().map(|c| c.as_os_str().to_string_lossy().into_owned()));
    if out.is_empty() { ".".into() } else { out.join("/") }
}

/// The [`Source`] for a tool's `sheet` argument (an artifact id, a workspace path, or an
/// artifact's filename or title).
pub fn resolve(ws: &str, chat: Option<&str>, arg: Option<&Value>, follow: bool) -> Result<Source, SheetError> {
    let Some(arg) = arg.and_then(Value::as_str).filter(|a| !a.trim().is_empty()) else {
        return Err(SheetError::Msg("Give `sheet`: an artifact id (like budget-3f2a) or a workspace path of an .xlsx, .csv or .tsv file.".into()));
    };
    let mut s0 = arg.trim().trim_matches(|c| c == '"' || c == '\'' || c == '`').to_owned();
    if let Some(i) = s0.rfind('@') {
        let (head, tail) = (&s0[..i], &s0[i + 1..]);
        if !head.is_empty() && !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) {
            s0 = head.to_owned();
        }
    }
    if let Some(rest) = s0.strip_prefix("artifact:") {
        s0 = rest.to_owned();
    }
    let s1 = s0.as_str();
    if id_ok(s1)
        && let Some(mut meta) = read_meta(ws, s1)
    {
        if let Some(o) = meta.as_object_mut() {
            o.entry("id").or_insert(Value::String(s1.to_owned()));
        }
        return artifact_source(ws, &meta, String::new());
    }
    let path = if Path::new(s1).is_absolute() { PathBuf::from(s1) } else { Path::new(ws).join(s1) };
    if path.is_file() {
        let low = path.to_string_lossy().to_lowercase();
        if !SHEET_SUFFIXES.iter().any(|x| low.ends_with(x)) {
            return Err(SheetError::Msg(format!("{s1} isn't a spreadsheet file; use an .xlsx, .csv or .tsv file.")));
        }
        let rel = relpath(&path, Path::new(ws));
        if follow && let Some(d) = derived(ws, chat, &rel) {
            let id = or_text(&d, "id", "");
            return artifact_source(ws, &d, format!("({rel} was saved as artifact {id} earlier; using that. Pass {id} as `sheet` from now on.)"));
        }
        let fmt = file_format(&path.to_string_lossy());
        return Ok(Source { artifact: false, path, fmt, aid: String::new(), meta: Value::Null, version: None, rel, note: String::new() });
    }
    let low = s1.to_lowercase();
    let mut found: Vec<Value> = metas(ws)
        .into_iter()
        .filter(|m| {
            let f = or_text(m, "filename", "");
            is_sheet_meta(m) && [f.to_lowercase(), or_text(m, "title", "").to_lowercase(), splitext(&f).0.to_lowercase()].contains(&low)
        })
        .collect();
    found.sort_by_key(|m| s(m, "chat") != chat);
    if let Some(m) = found.first() {
        return artifact_source(ws, m, String::new());
    }
    let have = sheet_artifacts(ws, chat);
    let mut hint = String::new();
    if !have.is_empty() {
        let list: Vec<String> = have
            .iter()
            .take(10)
            .map(|m| {
                let f = m.get("filename").filter(|f| truthy(Some(f))).or_else(|| m.get("title")).map_or_else(|| "None".into(), pystr);
                format!("{} ({f})", m.get("id").map_or_else(|| "None".into(), pystr))
            })
            .collect();
        hint = format!(" Spreadsheets in this chat: {}.", list.join(", "));
    }
    Err(SheetError::Msg(format!("No spreadsheet {}: it isn't an artifact id or a file in the workspace.{hint} To make one, call create_sheet.", repr_str(s1))))
}

// ---- workbooks in memory -----------------------------------------------------------------

fn sha(data: &[u8]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    data.hash(&mut h);
    h.finish()
}

fn file_sha(path: &Path) -> Option<u64> {
    std::fs::read(path).ok().map(|d| sha(&d))
}

type Stat = (PathBuf, u64, i128);

fn stat(path: &Path) -> Option<Stat> {
    let m = std::fs::metadata(path).ok()?;
    let t = m.modified().ok()?.duration_since(UNIX_EPOCH).map(|d| i128::try_from(d.as_nanos()).unwrap_or(0)).unwrap_or(0);
    Some((path.to_path_buf(), m.len(), t))
}

/// The live workbook of one artifact: tool writes and grid edits go into it.
pub struct Entry {
    pub book: Book,
    sha: Option<u64>,
    stat: Option<Stat>,
    /// The version our state builds on.
    base: i64,
    /// Versions we wrote that byob may not have stored yet: (sha, when).
    ours: Vec<(u64, Instant)>,
    /// Cells the user changed in the grid since the model's last write.
    pub user_edits: Vec<String>,
}

impl Entry {
    pub fn wrote(&mut self, data: &[u8], base: Option<i64>) {
        let h = sha(data);
        self.sha = Some(h);
        self.ours.retain(|(x, _)| *x != h);
        self.ours.push((h, Instant::now()));
        if self.ours.len() > 32 {
            let drop = self.ours.len() - 32;
            self.ours.drain(..drop);
        }
        self.stat = None;
        if let Some(b) = base {
            self.base = self.base.max(b);
        }
    }

    pub fn user_edited(&mut self, label: String) {
        self.user_edits.retain(|l| *l != label);
        self.user_edits.push(label);
        if self.user_edits.len() > 200 {
            let drop = self.user_edits.len() - 200;
            self.user_edits.drain(..drop);
        }
    }
}

/// Workbooks kept between calls.
#[derive(Default)]
pub struct Store {
    pub entries: BTreeMap<String, Entry>,
    old: Vec<(PathBuf, Book)>,
    files: Vec<(PathBuf, Option<Stat>, Book)>,
    tmp: Option<PathBuf>,
    tmp_files: Vec<PathBuf>,
    tmp_n: u64,
}

fn load(path: &Path, fmt: &str) -> Result<Book, SheetError> {
    Book::load(path, Some(fmt))
}

impl Store {
    /// The live workbook of artifact `aid`, whose latest version (as far as the caller knows)
    /// is `version` at `path`. The one in memory wins while that file is what we last wrote,
    /// or older than what our state builds on, or one of ours byob is still storing;
    /// otherwise (a restore, a file changed elsewhere) it is loaded from the file.
    pub fn artifact_entry(&mut self, aid: &str, path: &Path, fmt: &str, version: Option<i64>) -> Result<&mut Entry, SheetError> {
        let sig = stat(path);
        let keep = match self.entries.get_mut(aid) {
            Some(e) if sig.is_some() && e.stat == sig => true,
            Some(e) => {
                let v = version.unwrap_or(0);
                let h = file_sha(path);
                if h == e.sha {
                    e.stat = sig.clone();
                    e.base = e.base.max(v);
                    true
                } else {
                    v <= e.base || h.is_some_and(|h| e.ours.iter().any(|(x, t)| *x == h && t.elapsed().as_secs_f64() < 10.0))
                }
            }
            None => false,
        };
        if !keep {
            let book = match load(path, fmt) {
                Ok(b) => b,
                Err(SheetError::Io(e)) => return Err(SheetError::Msg(format!("Artifact {aid}'s file couldn't be read ({e})."))),
                Err(e) => return Err(e),
            };
            let e = Entry { book, sha: file_sha(path), stat: sig, base: version.unwrap_or(0), ours: Vec::new(), user_edits: Vec::new() };
            self.entries.insert(aid.to_owned(), e);
        }
        self.entries.get_mut(aid).ok_or_else(|| SheetError::Msg(format!("Artifact {aid} isn't loaded.")))
    }

    /// An older version's workbook (read-only views), cached (6).
    pub fn version_book(&mut self, path: &Path, fmt: &str) -> Result<&mut Book, SheetError> {
        let book = match self.old.iter().position(|(p, _)| p == path) {
            Some(i) => self.old.remove(i).1,
            None => load(path, fmt)?,
        };
        self.old.push((path.to_path_buf(), book));
        while self.old.len() > 6 {
            self.old.remove(0);
        }
        self.old.last_mut().map(|(_, b)| b).ok_or_else(|| SheetError::Msg("no workbook".into()))
    }

    /// A workspace file's workbook, cached while the file is unchanged (6).
    pub fn file_book(&mut self, path: &Path, fmt: &str) -> Result<&mut Book, SheetError> {
        let sig = stat(path);
        let i = self.files.iter().position(|(p, st, _)| p == path && *st == sig);
        let i = match i {
            Some(i) => i,
            None => {
                let b = load(path, fmt)?;
                self.files.retain(|(p, ..)| p != path);
                self.files.push((path.to_path_buf(), sig, b));
                while self.files.len() > 6 {
                    self.files.remove(0);
                }
                self.files.len() - 1
            }
        };
        self.files.get_mut(i).map(|(.., b)| b).ok_or_else(|| SheetError::Msg("no workbook".into()))
    }

    pub fn read_book(&mut self, src: &Source) -> Result<&mut Book, SheetError> {
        if src.artifact {
            return Ok(&mut self.artifact_entry(&src.aid, &src.path, src.fmt, src.version)?.book);
        }
        self.file_book(&src.path, src.fmt)
    }

    /// Write the bytes for byob to copy (it copies them right after our reply).
    pub fn save_tmp(&mut self, data: &[u8], filename: &str) -> Result<String, SheetError> {
        if self.tmp.as_ref().is_none_or(|d| !d.is_dir()) {
            let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
            let dir = std::env::temp_dir().join(format!("byob-sheets-{}-{nanos:x}", std::process::id()));
            std::fs::create_dir_all(&dir).map_err(|e| SheetError::Io(e.to_string()))?;
            self.tmp = Some(dir);
        }
        let dir = self.tmp.clone().unwrap_or_else(std::env::temp_dir);
        self.tmp_n += 1;
        let base =
            Path::new(if filename.is_empty() { "sheet.xlsx" } else { filename }).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        let mut name = String::new();
        let mut bad = false;
        for ch in base.chars() {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                name.push(ch);
                bad = false;
            } else if !bad {
                name.push('_');
                bad = true;
            }
        }
        let path = dir.join(format!("{}-{name}", self.tmp_n));
        std::fs::write(&path, data).map_err(|e| SheetError::Io(e.to_string()))?;
        self.tmp_files.push(path.clone());
        while self.tmp_files.len() > 16 {
            let old = self.tmp_files.remove(0);
            let _ = std::fs::remove_file(old);
        }
        Ok(path.to_string_lossy().into_owned())
    }

    /// Remove the temporary folder (on shutdown).
    pub fn cleanup(&mut self) {
        if let Some(d) = self.tmp.take() {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relpath_uses_forward_slashes_everywhere() {
        let ws = std::env::temp_dir().join("ws");
        assert_eq!(relpath(&ws.join("data").join("sales.csv"), &ws), "data/sales.csv");
        assert_eq!(relpath(&ws.join("a").join("b").join("c.tsv"), &ws), "a/b/c.tsv");
        assert_eq!(relpath(&ws.join("x.csv"), &ws.join("sub").join("dir")), "../../x.csv");
        assert_eq!(relpath(&ws, &ws), ".");
    }
}
