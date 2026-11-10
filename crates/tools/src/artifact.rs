//! Artifacts: versioned files the model makes beside the conversation (see
//! [`byob_protocol::artifact`]). `create_artifact`, `update_artifact` and `read_artifact`.
//!
//! Layout, in the chat's workspace:
//!
//! ```text
//! .byob/artifacts/<id>/artifact.json   the index: chat, kind, title, file name, versions
//! .byob/artifacts/<id>/v1.html         one file per version, never overwritten
//! ```
//!
//! Charts and images other tools make (`charts/…chart.json`, a PNG from `run_python`) are
//! *adopted*: they get an index whose single version points at the file where it is.
//!
//! Access: [`Access::Read`], like `make_chart`: these tools only ever create new files
//! inside `.byob/artifacts/` (exclusively, never through links) and rewrite their own index.
//! A version a per-turn undo removed simply drops out of the list.

use std::fs::OpenOptions;
use std::io::{ErrorKind, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use byob_protocol::{Artifact, ArtifactKind, ArtifactRef, ArtifactVersion, MediaKind, MediaRef, artifact::valid_id};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::sandbox;
use crate::{Access, FileChange, Tool, ToolContext, ToolOutput, ToolSpec, parse_args};

/// Workspace folder artifacts live in (inside byob's reserved `.byob`).
pub const ARTIFACT_DIR: &str = ".byob/artifacts";
const META_FILE: &str = "artifact.json";
/// Largest artifact content the model may write (per version).
pub const MAX_CONTENT_BYTES: usize = 1024 * 1024;
/// Largest version file read back (adopted images may be bigger than what tools write).
pub const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
/// Find/replace edits in one `update_artifact` call.
pub const MAX_EDITS: usize = 50;
/// Versions one artifact may have.
pub const MAX_VERSIONS: usize = 500;
const MAX_TITLE_CHARS: usize = 80;
const MAX_NOTE_CHARS: usize = 200;
const MAX_META_BYTES: u64 = 512 * 1024;
/// Artifact folders looked at when listing a workspace.
const MAX_LISTED: usize = 2000;
/// Charts/images adopted from one tool call.
const MAX_ADOPTED: usize = 4;
/// Text returned by `read_artifact`.
const MAX_READ_CHARS: usize = 200_000;

/// What the system prompt says about artifacts (appended where the tools are registered).
pub const PROMPT: &str = "Artifacts: when the user wants something to keep, reuse or export — a report, summary or document, an interactive tool, calculator, game or app, a data file — \
make it an artifact with create_artifact rather than pasting it into the chat; it shows in a pane beside the conversation and can be exported. Pick the format that fits: \
summary/report/notes → .md; interactive tool or app → a single-file .html app; data → .csv or .json; a chart of numbers → make_chart (or .html for a custom visualisation). \
To change an artifact, call update_artifact with its id and small find/replace edits (read_artifact shows its current text) — never create a new one for a change. \
HTML apps: one self-contained file with inline <style> and <script>; nothing from the network (no CDNs, web fonts, fetch or images by URL — all blocked); \
style with the theme's CSS variables var(--byob-bg), var(--byob-surface), var(--byob-fg), var(--byob-muted), var(--byob-accent), var(--byob-on-accent), var(--byob-border), var(--byob-danger), var(--byob-success), var(--byob-warning), var(--byob-link), var(--byob-code-bg), var(--byob-code-fg), var(--byob-radius) \
and var(--byob-chart-a) … var(--byob-chart-h) for data series (give fallbacks); \
fit any container width (fluid layout, no fixed page widths). After making one, say in a sentence or two what it is; don't repeat its content.";

/// The index file: an artifact and the chat it belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub chat: String,
    #[serde(flatten)]
    pub artifact: Artifact,
}

/// Milliseconds since the epoch, strictly increasing within the process (so artifacts made
/// in the same millisecond still list in order).
fn now_ms() -> i64 {
    static LAST: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
    let prev = LAST.try_update(Ordering::AcqRel, Ordering::Acquire, |last| Some(now.max(last.saturating_add(1)))).unwrap_or(now);
    now.max(prev.saturating_add(1))
}

/// `.byob/artifacts`, created if needed; refuses links anywhere on the way.
fn root_dir(workspace: &Path) -> Result<(PathBuf, PathBuf), String> {
    let r = sandbox::resolve(workspace, ARTIFACT_DIR)?;
    for dir in [r.root.join(sandbox::RESERVED_DIR), r.abs.clone()] {
        match std::fs::symlink_metadata(&dir) {
            Ok(m) if m.file_type().is_symlink() || !m.is_dir() => return Err(format!("{} is not a plain folder; can't keep artifacts there", dir.display())),
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => std::fs::create_dir(&dir)
                .or_else(|e| if e.kind() == ErrorKind::AlreadyExists { Ok(()) } else { Err(e) })
                .map_err(|e| format!("can't create {}: {e}", dir.display()))?,
            Err(e) => return Err(format!("can't access {}: {e}", dir.display())),
        }
    }
    Ok((r.root, r.abs))
}

/// The folder of artifact `id` (which must exist and be a plain folder).
fn artifact_dir(workspace: &Path, id: &str) -> Result<(PathBuf, PathBuf), String> {
    if !valid_id(id) {
        return Err(format!("there is no artifact {id:?}"));
    }
    let r = sandbox::resolve(workspace, &format!("{ARTIFACT_DIR}/{id}"))?;
    match std::fs::symlink_metadata(&r.abs) {
        Ok(m) if m.is_dir() && !m.file_type().is_symlink() => Ok((r.root, r.abs)),
        _ => Err(format!("there is no artifact {id:?}")),
    }
}

fn read_capped(path: &Path, max: u64) -> Result<Vec<u8>, String> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let meta = std::fs::metadata(path).map_err(|e| format!("{name}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{name} is not a file"));
    }
    if meta.len() > max {
        return Err(format!("{name} is too big ({} bytes)", meta.len()));
    }
    let mut out = Vec::new();
    std::fs::File::open(path).and_then(|f| f.take(max).read_to_end(&mut out)).map_err(|e| format!("{name}: {e}"))?;
    Ok(out)
}

/// The file behind a version path, if it is still there (inside the workspace).
fn version_file(workspace: &Path, path: &str) -> Option<PathBuf> {
    let r = sandbox::resolve_follow(workspace, path).ok()?;
    std::fs::metadata(&r.abs).ok().filter(std::fs::Metadata::is_file).map(|_| r.abs)
}

/// Read an artifact's index. Versions whose file is gone (e.g. undone) are left out; an
/// artifact with none left is gone.
pub fn load(workspace: &Path, id: &str) -> Result<Meta, String> {
    let (_, dir) = artifact_dir(workspace, id)?;
    let bytes = read_capped(&dir.join(META_FILE), MAX_META_BYTES).map_err(|_| format!("there is no artifact {id:?}"))?;
    let mut meta: Meta = serde_json::from_slice(&bytes).map_err(|e| format!("artifact {id:?} has a broken index: {e}"))?;
    if meta.artifact.id != id {
        return Err(format!("artifact {id:?} has a broken index"));
    }
    meta.artifact.versions.retain(|v| version_file(workspace, &v.path).is_some());
    if meta.artifact.versions.is_empty() {
        return Err(format!("artifact {id:?} no longer exists"));
    }
    Ok(meta)
}

/// Like [`load`], but only an artifact of `chat`.
pub fn load_in_chat(workspace: &Path, chat: &str, id: &str) -> Result<Meta, String> {
    let meta = load(workspace, id)?;
    if meta.chat != chat {
        return Err(format!("there is no artifact {id:?} in this chat"));
    }
    Ok(meta)
}

/// Write the index (via a temporary file, so a crash never leaves half of one).
fn save(dir: &Path, meta: &Meta) -> Result<(), String> {
    let json = serde_json::to_vec_pretty(meta).map_err(|e| e.to_string())?;
    let tmp = dir.join(format!("{META_FILE}.tmp"));
    let _ = std::fs::remove_file(&tmp);
    let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp).map_err(|e| format!("can't save the artifact index: {e}"))?;
    f.write_all(&json).map_err(|e| format!("can't save the artifact index: {e}"))?;
    drop(f);
    std::fs::rename(&tmp, dir.join(META_FILE)).map_err(|e| format!("can't save the artifact index: {e}"))
}

/// Every artifact of `chat` in `workspace`, oldest first.
pub fn list(workspace: &Path, chat: &str) -> Vec<Artifact> {
    let Ok(r) = sandbox::resolve(workspace, ARTIFACT_DIR) else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(&r.abs) else { return Vec::new() };
    let mut out: Vec<Artifact> = entries
        .take(MAX_LISTED)
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter(|id| valid_id(id))
        .filter_map(|id| load(workspace, &id).ok())
        .filter(|m| m.chat == chat)
        .map(|m| m.artifact)
        .collect();
    out.sort_by_key(|a| (a.versions.first().map_or(0, |v| v.created_at), a.id.clone()));
    out
}

/// Delete every artifact of `chat` in `workspace` for good (when the chat is deleted).
/// Returns how many were removed; ones whose index can't be read are left alone.
pub fn remove_chat(workspace: &Path, chat: &str) -> usize {
    let Ok(r) = sandbox::resolve(workspace, ARTIFACT_DIR) else { return 0 };
    let Ok(entries) = std::fs::read_dir(&r.abs) else { return 0 };
    let mut removed = 0;
    for id in entries.filter_map(Result::ok).filter_map(|e| e.file_name().to_str().map(str::to_owned)) {
        let Ok((_, dir)) = artifact_dir(workspace, &id) else { continue };
        let Ok(bytes) = read_capped(&dir.join(META_FILE), MAX_META_BYTES) else { continue };
        let Ok(meta) = serde_json::from_slice::<Meta>(&bytes) else { continue };
        if meta.chat != chat {
            continue;
        }
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => removed += 1,
            Err(e) => log::warn!("couldn't delete artifact {id} of chat {chat}: {e}"),
        }
    }
    removed
}

/// Bytes of version `version` of artifact `id`, which must belong to `chat`.
pub fn read_version(workspace: &Path, chat: &str, id: &str, version: u32, max: u64) -> Result<(Artifact, Vec<u8>), String> {
    let meta = load_in_chat(workspace, chat, id)?;
    let v = meta.artifact.version(version).ok_or_else(|| format!("artifact {id:?} has no version {version}"))?;
    let file = version_file(workspace, &v.path).ok_or_else(|| format!("version {version} of {id:?} is gone"))?;
    let bytes = read_capped(&file, max)?;
    Ok((meta.artifact, bytes))
}

/// `[.../].byob/artifacts/<id>/v<n>.<ext>` → the version it is.
pub fn parse_version_path(path: &str) -> Option<ArtifactRef> {
    let norm = path.replace('\\', "/");
    let rest = norm.rsplit_once(&format!("{ARTIFACT_DIR}/")).map(|(_, r)| r)?;
    let (id, file) = rest.split_once('/')?;
    let n: u32 = file.strip_prefix('v')?.split_once('.')?.0.parse().ok()?;
    (valid_id(id) && n > 0 && !file.contains('/')).then(|| ArtifactRef { id: id.to_owned(), version: n })
}

/// A new artifact id from a title: `loan-calculator-3f2a`.
fn new_id(title: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut slug = String::new();
    for c in title.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
        if slug.len() >= 40 {
            break;
        }
    }
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "artifact" } else { slug };
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| u64::from(d.subsec_nanos()));
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mix = nanos ^ n.wrapping_mul(0x9e37_79b9) ^ u64::from(std::process::id()).rotate_left(16);
    format!("{slug}-{:04x}", mix & 0xffff)
}

/// Create a fresh, empty folder for a new artifact. Returns `(id, folder)`.
fn new_artifact_dir(workspace: &Path, title: &str) -> Result<(String, PathBuf), String> {
    let (_, root) = root_dir(workspace)?;
    for _ in 0..16 {
        let id = new_id(title);
        let dir = root.join(&id);
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok((id, dir)),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("can't create the artifact: {e}")),
        }
    }
    Err("can't find a free artifact id; try again".into())
}

fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/")
}

fn clip(s: &str, max: usize) -> String {
    let clean: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let clean = clean.trim();
    let mut out: String = clean.chars().take(max).collect();
    if clean.chars().count() > max {
        out.push('…');
    }
    out
}

/// A model-given file name made safe, and the extension its versions are saved with.
fn clean_filename(raw: &str, title: &str) -> Result<(String, String), String> {
    let last = raw.trim().rsplit(['/', '\\']).next().unwrap_or_default();
    let name: String = last.chars().filter(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ')).take(100).collect();
    let name = name.trim().trim_start_matches('.').to_owned();
    let lower = name.to_ascii_lowercase();
    let ext = if lower.ends_with(".chart.json") {
        "chart.json".to_owned()
    } else {
        match lower.rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() && (1..=10).contains(&ext.len()) && ext.bytes().all(|b| b.is_ascii_alphanumeric()) => ext.to_owned(),
            _ => {
                return Err(format!(
                    "filename {raw:?} needs an extension that says what it is: .md for a document or summary, .html for an interactive app, .csv or .json for data, .chart.json for a chart"
                ));
            }
        }
    };
    let name = if name.is_empty() { format!("{}.{ext}", new_id(title)) } else { name };
    Ok((name, ext))
}

/// Write a version file exclusively (never through a link, never over another file).
fn write_version(dir: &Path, n: u32, ext: &str, content: &str) -> Result<PathBuf, String> {
    let path = dir.join(format!("v{n}.{ext}"));
    let mut f = OpenOptions::new().write(true).create_new(true).open(&path).map_err(|e| format!("can't write version {n}: {e}"))?;
    f.write_all(content.as_bytes()).map_err(|e| format!("can't write version {n}: {e}"))?;
    Ok(path)
}

/// Check (and for charts normalise) content for `kind`. Returns the text to save and notes
/// for the model.
fn check_content(kind: ArtifactKind, content: &str) -> Result<(String, String), String> {
    if content.len() > MAX_CONTENT_BYTES {
        return Err(format!("the content is too big ({} bytes; the limit is {MAX_CONTENT_BYTES})", content.len()));
    }
    if content.trim().is_empty() {
        return Err("the content is empty".into());
    }
    match kind {
        ArtifactKind::Image => {
            Err("create_artifact writes text files; images that tools make (make_chart, run_python) show up as artifacts on their own".into())
        }
        ArtifactKind::Chart => {
            let value: Value = serde_json::from_str(content).map_err(|e| format!("a .chart.json artifact must be a chart spec (JSON): {e}"))?;
            let spec = byob_types::chart::validate(&value)?;
            Ok((spec.to_json(), String::new()))
        }
        ArtifactKind::Html => {
            let lower = content.to_ascii_lowercase();
            let external = ["src=\"http", "src='http", "href=\"http", "href='http", "src=\"//", "@import url(", "url(http", "fetch(", "xmlhttprequest"]
                .iter()
                .any(|p| lower.contains(p));
            let note = if external {
                " Warning: it seems to load things from the network (a URL, fetch or a CDN). That is blocked in artifacts; inline everything instead."
            } else {
                ""
            };
            Ok((content.to_owned(), note.to_owned()))
        }
        ArtifactKind::Markdown | ArtifactKind::Text => Ok((content.to_owned(), String::new())),
    }
}

fn chat_of<'a>(ctx: &ToolContext<'a>) -> Result<&'a str, String> {
    ctx.chat().filter(|c| !c.is_empty()).ok_or_else(|| "artifacts are only available in a chat".to_owned())
}

/// Adopt the charts and images a tool call made as artifacts of `chat` (one version each,
/// pointing at the file where it is). Returns refs for the step.
fn adopt(workspace: &Path, chat: &str, media: &[MediaRef]) -> Vec<ArtifactRef> {
    let mut out = Vec::new();
    for m in media.iter().filter(|m| m.kind != MediaKind::File).take(MAX_ADOPTED) {
        let kind = if m.kind == MediaKind::Chart { ArtifactKind::Chart } else { ArtifactKind::Image };
        let name = m.name().to_owned();
        let title = name.strip_suffix(".chart.json").or_else(|| name.rsplit_once('.').map(|(s, _)| s)).unwrap_or(&name).to_owned();
        let size = version_file(workspace, &m.path).and_then(|p| std::fs::metadata(p).ok()).map_or(m.size, |md| md.len());
        let result = new_artifact_dir(workspace, &title).and_then(|(id, dir)| {
            let version = ArtifactVersion { n: 1, path: m.path.clone(), created_at: now_ms(), note: String::new(), size };
            let artifact = Artifact { id: id.clone(), kind, title: clip(&title, MAX_TITLE_CHARS), filename: name.clone(), versions: vec![version] };
            save(&dir, &Meta { chat: chat.to_owned(), artifact }).map(|()| id)
        });
        match result {
            Ok(id) => out.push(ArtifactRef { id, version: 1 }),
            Err(e) => log::warn!("adopting {} as an artifact: {e}", m.path),
        }
    }
    out
}

/// The artifact versions a finished tool call made: versions written by the artifact tools
/// (by path), else the charts and images it made, adopted.
pub fn for_call(workspace: &Path, chat: &str, changes: &[FileChange], media: &[MediaRef]) -> Vec<ArtifactRef> {
    let made: Vec<ArtifactRef> = changes
        .iter()
        .filter_map(|c| match c {
            FileChange::Created(p) => parse_version_path(&p.to_string_lossy()),
            _ => None,
        })
        .collect();
    if !made.is_empty() {
        return made;
    }
    adopt(workspace, chat, media)
}

/// Lines for the system prompt naming the chat's artifacts, so later turns can update them.
pub fn prompt_list(artifacts: &[Artifact]) -> String {
    if artifacts.is_empty() {
        return String::new();
    }
    let mut out = String::from("Artifacts in this chat (update them by id):");
    for a in artifacts.iter().rev().take(30) {
        let v = a.latest().map_or(1, |v| v.n);
        out.push_str(&format!("\n- {}: \"{}\" ({}, v{v}, {})", a.id, a.title, a.kind.label(), a.filename));
    }
    out
}

// ---------------------------------------------------------------------------------------
// Tools

pub struct CreateArtifact;
pub struct UpdateArtifact;
pub struct ReadArtifact;

#[derive(Deserialize)]
struct CreateArgs {
    title: String,
    #[serde(default, alias = "name", alias = "file")]
    filename: String,
    content: String,
    #[serde(default)]
    note: String,
}

#[derive(Deserialize)]
struct Edit {
    find: String,
    #[serde(default, alias = "replacement", alias = "replace_with")]
    replace: String,
}

#[derive(Deserialize)]
struct UpdateArgs {
    id: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    edits: Option<Vec<Edit>>,
    #[serde(default)]
    note: String,
}

#[derive(Deserialize)]
struct ReadArgs {
    id: String,
    #[serde(default)]
    version: Option<u32>,
}

fn arg_str<'a>(args: &'a Value, key: &str) -> &'a str {
    args.get(key).and_then(Value::as_str).unwrap_or_default()
}

impl Tool for CreateArtifact {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "create_artifact".into(),
            description: "Make an artifact: a file shown beside the chat that the user can open, keep and export, with versions. \
                The file name's extension decides what it is: .md (document, report, summary), .html (interactive single-file app: HTML+CSS+JS in one file, no network), \
                .csv/.json/.py/… (data or code, shown as source), .chart.json (a chart spec). Returns the artifact's id for update_artifact."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string", "description": "Short title, e.g. \"Loan calculator\""},
                    "filename": {"type": "string", "description": "File name with extension, e.g. loan-calculator.html, q3-summary.md, prices.csv"},
                    "content": {"type": "string", "description": "The whole file"},
                    "note": {"type": "string", "description": "Optional: what this first version is"}
                },
                "required": ["title", "filename", "content"]
            }),
        }
    }

    fn title(&self, args: &Value) -> String {
        format!("Made {}", clip(arg_str(args, "title"), 60))
    }

    fn pending_title(&self, args: &Value) -> String {
        format!("Make {}", clip(arg_str(args, "title"), 60))
    }

    fn access(&self, _args: &Value) -> Access {
        Access::Read
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        match create(args, ctx) {
            Ok(out) => out,
            Err(e) => ToolOutput::err(e),
        }
    }
}

fn create(args: &Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, String> {
    let chat = chat_of(ctx)?;
    let a: CreateArgs = parse_args(args)?;
    let title = clip(&a.title, MAX_TITLE_CHARS);
    if title.is_empty() {
        return Err("give the artifact a title".into());
    }
    let (filename, ext) = clean_filename(&a.filename, &title)?;
    let kind = ArtifactKind::for_filename(&filename);
    let (content, warning) = check_content(kind, &a.content)?;
    let (id, dir) = new_artifact_dir(ctx.workspace, &title)?;
    let result = (|| {
        let file = write_version(&dir, 1, &ext, &content)?;
        let root = sandbox::root(ctx.workspace)?;
        let version = ArtifactVersion { n: 1, path: rel(&root, &file), created_at: now_ms(), note: clip(&a.note, MAX_NOTE_CHARS), size: content.len() as u64 };
        let artifact = Artifact { id: id.clone(), kind, title: title.clone(), filename: filename.clone(), versions: vec![version] };
        save(&dir, &Meta { chat: chat.to_owned(), artifact })?;
        Ok::<_, String>(file)
    })();
    let file = match result {
        Ok(f) => f,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e);
        }
    };
    let text = format!(
        "Created the {} artifact \"{title}\" (id {id}, version 1, {filename}). It shows beside the chat. To change it later, call update_artifact with id {id}.{warning}",
        kind.label()
    );
    Ok(ToolOutput { text, failed: false, changes: vec![FileChange::Created(file)] })
}

impl Tool for UpdateArtifact {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "update_artifact".into(),
            description: "Save a new version of an artifact. Prefer `edits`: find/replace pairs applied in order, where each `find` is exact text that occurs exactly once \
                in the current version (include enough surrounding text to make it unique). Send `content` (the whole file) only for a rewrite. \
                Use read_artifact first if you don't have the current text."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "The artifact's id"},
                    "edits": {"type": "array", "items": {"type": "object", "properties": {"find": {"type": "string"}, "replace": {"type": "string"}}, "required": ["find", "replace"]}},
                    "content": {"type": "string", "description": "Or: the whole new file"},
                    "note": {"type": "string", "description": "What changed, in a few words"}
                },
                "required": ["id"]
            }),
        }
    }

    fn title(&self, args: &Value) -> String {
        format!("Updated {}", clip(arg_str(args, "id"), 60))
    }

    fn pending_title(&self, args: &Value) -> String {
        format!("Update {}", clip(arg_str(args, "id"), 60))
    }

    fn access(&self, _args: &Value) -> Access {
        Access::Read
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        match update(args, ctx) {
            Ok(out) => out,
            Err(e) => ToolOutput::err(e),
        }
    }
}

/// Apply find/replace edits in order. Each `find` must occur exactly once in the text as
/// it is at that point.
pub fn apply_edits(text: &str, edits: &[(String, String)]) -> Result<String, String> {
    if edits.is_empty() {
        return Err("edits is empty; give at least one {find, replace}".into());
    }
    if edits.len() > MAX_EDITS {
        return Err(format!("too many edits ({}; at most {MAX_EDITS}); send the whole file as content instead", edits.len()));
    }
    let mut out = text.to_owned();
    for (i, (find, replace)) in edits.iter().enumerate() {
        let k = i.saturating_add(1);
        if find.is_empty() {
            return Err(format!("edit {k}: `find` is empty"));
        }
        let count = out.matches(find.as_str()).count();
        match count {
            1 => out = out.replacen(find.as_str(), replace, 1),
            0 => {
                return Err(format!(
                    "edit {k}: this `find` text is not in the current version: {:?}. It must match exactly (whitespace too); call read_artifact to see the current text. No edits were applied.",
                    clip(find, 120)
                ));
            }
            n => {
                return Err(format!(
                    "edit {k}: this `find` text occurs {n} times: {:?}. Include more surrounding text so it occurs exactly once. No edits were applied.",
                    clip(find, 120)
                ));
            }
        }
        if out.len() > MAX_CONTENT_BYTES {
            return Err(format!("the edits make the file too big (over {MAX_CONTENT_BYTES} bytes)"));
        }
    }
    Ok(out)
}

fn update(args: &Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, String> {
    let chat = chat_of(ctx)?;
    let a: UpdateArgs = parse_args(args)?;
    let mut meta = load_in_chat(ctx.workspace, chat, a.id.trim())?;
    let id = meta.artifact.id.clone();
    let kind = meta.artifact.kind;
    if kind == ArtifactKind::Image {
        return Err(format!("{id} is an image; images can't be edited as text. Make a new one instead"));
    }
    if meta.artifact.versions.len() >= MAX_VERSIONS {
        return Err(format!("{id} has too many versions; create a new artifact"));
    }
    let latest = meta.artifact.latest().cloned().ok_or_else(|| format!("artifact {id:?} has no versions"))?;
    let new_text = match (a.content, a.edits) {
        (Some(_), Some(_)) => return Err("give either content or edits, not both".into()),
        (None, None) => return Err("give edits (find/replace pairs) or the whole new content".into()),
        (Some(content), None) => content,
        (None, Some(edits)) => {
            let file = version_file(ctx.workspace, &latest.path).ok_or_else(|| format!("the latest version of {id} is gone"))?;
            let bytes = read_capped(&file, MAX_FILE_BYTES)?;
            let current = String::from_utf8(bytes).map_err(|_| format!("{id} is not a text file"))?;
            let pairs: Vec<(String, String)> = edits.into_iter().map(|e| (e.find, e.replace)).collect();
            let out = apply_edits(&current, &pairs)?;
            if out == current {
                return Err("the edits change nothing".into());
            }
            out
        }
    };
    let (content, warning) = check_content(kind, &new_text)?;
    let (root, dir) = artifact_dir(ctx.workspace, &id)?;
    // Version numbers are never reused, even if a version was undone.
    let ext = latest.path.rsplit_once("/v").and_then(|(_, f)| f.split_once('.')).map(|(_, e)| e.to_owned()).unwrap_or_else(|| {
        let lower = meta.artifact.filename.to_ascii_lowercase();
        if lower.ends_with(".chart.json") { "chart.json".to_owned() } else { lower.rsplit_once('.').map_or("txt".to_owned(), |(_, e)| e.to_owned()) }
    });
    let all = serde_json::from_slice::<Meta>(&read_capped(&dir.join(META_FILE), MAX_META_BYTES)?).map_err(|e| e.to_string())?;
    let n = all.artifact.versions.iter().map(|v| v.n).max().unwrap_or(0).saturating_add(1);
    let file = write_version(&dir, n, &ext, &content)?;
    let note = clip(&a.note, MAX_NOTE_CHARS);
    meta.artifact.versions = all.artifact.versions;
    meta.artifact.versions.push(ArtifactVersion { n, path: rel(&root, &file), created_at: now_ms(), note: note.clone(), size: content.len() as u64 });
    if let Err(e) = save(&dir, &meta) {
        let _ = std::fs::remove_file(&file);
        return Err(e);
    }
    let text =
        format!("Saved version {n} of \"{}\" (id {id}){}.{warning}", meta.artifact.title, if note.is_empty() { String::new() } else { format!(": {note}") });
    Ok(ToolOutput { text, failed: false, changes: vec![FileChange::Created(file)] })
}

impl Tool for ReadArtifact {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_artifact".into(),
            description: "Read the current text of an artifact (or of an older version), e.g. before changing it with update_artifact.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "version": {"type": "integer", "description": "Optional; default the latest"}
                },
                "required": ["id"]
            }),
        }
    }

    fn title(&self, args: &Value) -> String {
        format!("Read {}", clip(arg_str(args, "id"), 60))
    }

    fn pending_title(&self, args: &Value) -> String {
        self.title(args)
    }

    fn access(&self, _args: &Value) -> Access {
        Access::Read
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        let result = (|| {
            let chat = chat_of(ctx)?;
            let a: ReadArgs = parse_args(args)?;
            let meta = load_in_chat(ctx.workspace, chat, a.id.trim())?;
            let v = match a.version {
                Some(n) => meta.artifact.version(n).cloned().ok_or_else(|| format!("{} has no version {n}", meta.artifact.id))?,
                None => meta.artifact.latest().cloned().ok_or("no versions")?,
            };
            if !meta.artifact.kind.is_text() {
                return Err(format!("{} is an image; it can't be read as text", meta.artifact.id));
            }
            let (_, bytes) = read_version(ctx.workspace, chat, &meta.artifact.id, v.n, MAX_FILE_BYTES)?;
            let text = String::from_utf8_lossy(&bytes);
            let shown: String = text.chars().take(MAX_READ_CHARS).collect();
            let more = if shown.len() < text.len() { "\n… [cut off]" } else { "" };
            Ok(format!(
                "{} \"{}\", version {} of {} ({}):\n{shown}{more}",
                meta.artifact.id,
                meta.artifact.title,
                v.n,
                meta.artifact.versions.len(),
                meta.artifact.filename
            ))
        })();
        match result {
            Ok(t) => ToolOutput::ok(t),
            Err(e) => ToolOutput::err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn run_in(tool: &dyn Tool, ws: &Path, chat: &str, args: Value) -> ToolOutput {
        let cancel = AtomicBool::new(false);
        tool.run(&args, &ToolContext::new(ws, &cancel).with_chat(chat))
    }

    fn id_of(out: &ToolOutput) -> String {
        let p = match &out.changes[..] {
            [FileChange::Created(p)] => p.to_string_lossy().into_owned(),
            other => panic!("{other:?} / {}", out.text),
        };
        parse_version_path(&p).unwrap().id
    }

    const HTML: &str = "<!doctype html><button style=\"background:red\">Go</button><button>Stop</button>";

    #[test]
    fn create_update_read_and_versions() {
        let d = tempfile::tempdir().unwrap();
        let ws = d.path();
        let out = run_in(&CreateArtifact, ws, "c1", json!({"title": "Loan calculator", "filename": "loan.html", "content": HTML}));
        assert!(!out.failed, "{}", out.text);
        let id = id_of(&out);
        assert!(id.starts_with("loan-calculator-"), "{id}");
        assert!(out.text.contains(&id));
        let a = &list(ws, "c1")[0];
        assert_eq!((a.kind, a.filename.as_str(), a.versions.len()), (ArtifactKind::Html, "loan.html", 1));
        assert_eq!(a.versions[0].path, format!(".byob/artifacts/{id}/v1.html"));
        assert!(list(ws, "other").is_empty(), "artifacts are per chat");

        let out = run_in(&UpdateArtifact, ws, "c1", json!({"id": id, "edits": [{"find": "background:red", "replace": "background:blue"}], "note": "blue"}));
        assert!(!out.failed, "{}", out.text);
        assert_eq!(parse_version_path(&id_of_path(&out)).unwrap().version, 2);
        let (a, v2) = read_version(ws, "c1", &id, 2, MAX_FILE_BYTES).unwrap();
        assert_eq!(String::from_utf8(v2).unwrap(), HTML.replace("red", "blue"));
        assert_eq!(a.latest().unwrap().note, "blue");
        // v1 is untouched
        assert_eq!(read_version(ws, "c1", &id, 1, MAX_FILE_BYTES).unwrap().1, HTML.as_bytes());

        let read = run_in(&ReadArtifact, ws, "c1", json!({"id": id}));
        assert!(read.text.contains("version 2") && read.text.contains("background:blue"), "{}", read.text);
        let other_chat = run_in(&ReadArtifact, ws, "c2", json!({"id": id}));
        assert!(other_chat.failed);
        assert!(run_in(&UpdateArtifact, ws, "c2", json!({"id": id, "content": "x"})).failed, "can't touch another chat's artifact");
        assert!(read_version(ws, "c2", &id, 1, MAX_FILE_BYTES).is_err());

        // a whole-content rewrite
        let out = run_in(&UpdateArtifact, ws, "c1", json!({"id": id, "content": "<p>new</p>"}));
        assert!(!out.failed, "{}", out.text);
        assert_eq!(load(ws, &id).unwrap().artifact.latest().unwrap().n, 3);
        // an undone version (file removed) drops out; numbers are never reused
        std::fs::remove_file(ws.join(format!(".byob/artifacts/{id}/v3.html"))).unwrap();
        assert_eq!(load(ws, &id).unwrap().artifact.latest().unwrap().n, 2);
        let out = run_in(&UpdateArtifact, ws, "c1", json!({"id": id, "content": "<p>again</p>"}));
        assert!(!out.failed, "{}", out.text);
        assert_eq!(load(ws, &id).unwrap().artifact.versions.iter().map(|v| v.n).collect::<Vec<_>>(), vec![1, 2, 4]);
    }

    /// Deleting a chat deletes its artifacts for good (they were left orphaned before).
    #[test]
    fn removing_a_chat_removes_only_its_artifacts() {
        let d = tempfile::tempdir().unwrap();
        let ws = d.path();
        for (chat, title) in [("c1", "One"), ("c1", "Two"), ("c2", "Other")] {
            assert!(!run_in(&CreateArtifact, ws, chat, json!({"title": title, "filename": "a.md", "content": "# hi"})).failed);
        }
        assert_eq!(remove_chat(ws, "c1"), 2);
        assert!(list(ws, "c1").is_empty());
        assert_eq!(list(ws, "c2").len(), 1);
        assert_eq!(std::fs::read_dir(ws.join(ARTIFACT_DIR)).unwrap().count(), 1, "nothing left behind on disk");
        assert_eq!(remove_chat(ws, "c1"), 0);
        assert_eq!(remove_chat(&ws.join("missing"), "c2"), 0);
    }

    fn id_of_path(out: &ToolOutput) -> String {
        match &out.changes[..] {
            [FileChange::Created(p)] => p.to_string_lossy().into_owned(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn edits_must_match_exactly_once() {
        let t = "a b a c";
        assert_eq!(apply_edits(t, &[("b".into(), "B".into()), ("c".into(), "C".into())]).unwrap(), "a B a C");
        let e = apply_edits(t, &[("a".into(), "x".into())]).unwrap_err();
        assert!(e.contains("occurs 2 times") && e.contains("edit 1"), "{e}");
        let e = apply_edits(t, &[("b".into(), "B".into()), ("zz".into(), "x".into())]).unwrap_err();
        assert!(e.contains("edit 2") && e.contains("not in the current version") && e.contains("read_artifact"), "{e}");
        // edits see the text as earlier edits left it
        assert_eq!(apply_edits(t, &[("b".into(), "q".into()), ("q a".into(), "r".into())]).unwrap(), "a r c");
        assert!(apply_edits(t, &[]).is_err());
        assert!(apply_edits(t, &[(String::new(), "x".into())]).is_err());
        let many: Vec<(String, String)> = (0..=MAX_EDITS).map(|i| (format!("{i}"), String::new())).collect();
        assert!(apply_edits(t, &many).unwrap_err().contains("too many"));
        let big = "x".repeat(MAX_CONTENT_BYTES);
        assert!(apply_edits("ab", &[("a".into(), big)]).unwrap_err().contains("too big"));
    }

    #[test]
    fn failed_edits_write_nothing() {
        let d = tempfile::tempdir().unwrap();
        let ws = d.path();
        let id = id_of(&run_in(&CreateArtifact, ws, "c", json!({"title": "T", "filename": "t.html", "content": HTML})));
        let out = run_in(&UpdateArtifact, ws, "c", json!({"id": id, "edits": [{"find": "button", "replace": "x"}]}));
        assert!(out.failed && out.text.contains("occurs 4 times"), "{}", out.text);
        assert!(out.changes.is_empty());
        assert_eq!(load(ws, &id).unwrap().artifact.versions.len(), 1);
        let both = run_in(&UpdateArtifact, ws, "c", json!({"id": id, "content": "x", "edits": [{"find": "Go", "replace": "x"}]}));
        assert!(both.failed && both.text.contains("not both"));
        assert!(run_in(&UpdateArtifact, ws, "c", json!({"id": id})).failed);
        assert!(run_in(&UpdateArtifact, ws, "c", json!({"id": "../../etc", "content": "x"})).failed);
        let same = run_in(&UpdateArtifact, ws, "c", json!({"id": id, "edits": [{"find": "Go", "replace": "Go"}]}));
        assert!(same.failed && same.text.contains("change nothing"));
    }

    #[test]
    fn create_validates_names_kinds_and_sizes() {
        let d = tempfile::tempdir().unwrap();
        let ws = d.path();
        let c = |args: Value| run_in(&CreateArtifact, ws, "c", args);
        assert!(c(json!({"title": "T", "filename": "noext", "content": "x"})).text.contains("extension"));
        assert!(c(json!({"title": "T", "filename": "p.png", "content": "x"})).text.contains("images"));
        assert!(c(json!({"title": "", "filename": "a.md", "content": "x"})).failed);
        assert!(c(json!({"title": "T", "filename": "a.md", "content": "  "})).failed);
        let big = "x".repeat(MAX_CONTENT_BYTES + 1);
        assert!(c(json!({"title": "T", "filename": "a.md", "content": big})).text.contains("too big"));
        assert!(c(json!({"title": "T", "filename": "a.chart.json", "content": "{nope"})).failed);
        // nothing half-made is left behind
        assert!(list(ws, "c").is_empty());
        let root = ws.join(ARTIFACT_DIR);
        assert_eq!(std::fs::read_dir(&root).map(|r| r.count()).unwrap_or(0), 0);

        let md = c(json!({"title": "Q3 summary", "filename": "../../q3.md", "content": "# Q3\n"}));
        assert!(!md.failed, "{}", md.text);
        let chart = c(json!({"title": "Sales", "filename": "sales.chart.json",
            "content": r#"{"type":"bar","x":"m","series":["v"],"data":[{"m":"a","v":1}]}"#}));
        assert!(!chart.failed, "{}", chart.text);
        let csv = c(json!({"title": "Prices", "filename": "prices.csv", "content": "a,b\n1,2\n"}));
        let net = c(json!({"title": "Net", "filename": "n.html", "content": "<script src=\"https://cdn.example/x.js\"></script>"}));
        assert!(net.text.contains("blocked"), "{}", net.text);
        let kinds: Vec<(ArtifactKind, String)> = list(ws, "c").into_iter().map(|a| (a.kind, a.filename)).collect();
        assert_eq!(
            kinds,
            vec![
                (ArtifactKind::Markdown, "q3.md".into()),
                (ArtifactKind::Chart, "sales.chart.json".into()),
                (ArtifactKind::Text, "prices.csv".into()),
                (ArtifactKind::Html, "n.html".into())
            ]
        );
        assert!(!csv.failed);
        assert!(Path::new(&id_of_path(&chart)).ends_with("v1.chart.json"));
        // no chat, no artifacts
        let cancel = AtomicBool::new(false);
        assert!(CreateArtifact.run(&json!({"title": "T", "filename": "a.md", "content": "x"}), &ToolContext::new(ws, &cancel)).failed);
    }

    #[cfg(unix)]
    #[test]
    fn never_writes_through_a_linked_artifact_folder() {
        let outside = tempfile::tempdir().unwrap();
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join(".byob")).unwrap();
        std::os::unix::fs::symlink(outside.path(), d.path().join(ARTIFACT_DIR)).unwrap();
        let out = run_in(&CreateArtifact, d.path(), "c", json!({"title": "T", "filename": "a.md", "content": "x"}));
        assert!(out.failed, "{}", out.text);
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn charts_and_images_are_adopted() {
        let d = tempfile::tempdir().unwrap();
        let ws = d.path();
        std::fs::create_dir(ws.join("charts")).unwrap();
        std::fs::write(ws.join("charts/Sales.chart.json"), "{}").unwrap();
        std::fs::write(ws.join("plot.png"), "png").unwrap();
        let media = vec![MediaRef::new("charts/Sales.chart.json", 2), MediaRef::new("plot.png", 3), MediaRef::new("notes.txt", 1)];
        let refs = for_call(ws, "c", &[], &media);
        assert_eq!(refs.len(), 2);
        let list = list(ws, "c");
        assert_eq!(list.iter().map(|a| (a.kind, a.title.as_str())).collect::<Vec<_>>(), vec![(ArtifactKind::Chart, "Sales"), (ArtifactKind::Image, "plot")]);
        assert_eq!(list[1].versions[0].path, "plot.png");
        // an artifact tool's own version is recognised, not adopted again
        let p = ws.join(".byob/artifacts/x-1/v3.md");
        assert_eq!(for_call(ws, "c", &[FileChange::Created(p)], &media), vec![ArtifactRef { id: "x-1".into(), version: 3 }]);
    }

    #[test]
    fn version_paths() {
        assert_eq!(parse_version_path(".byob/artifacts/a-1/v12.chart.json"), Some(ArtifactRef { id: "a-1".into(), version: 12 }));
        assert_eq!(parse_version_path("/w/.byob/artifacts/a/v1.html"), Some(ArtifactRef { id: "a".into(), version: 1 }));
        for bad in [".byob/artifacts/a/artifact.json", ".byob/artifacts/A/v1.md", ".byob/artifacts/a/v0.md", "charts/x.chart.json", ".byob/artifacts/a/b/v1.md"]
        {
            assert_eq!(parse_version_path(bad), None, "{bad}");
        }
    }

    #[test]
    fn prompt_lists_latest_versions() {
        let d = tempfile::tempdir().unwrap();
        let id = id_of(&run_in(&CreateArtifact, d.path(), "c", json!({"title": "Calc", "filename": "calc.html", "content": HTML})));
        let p = prompt_list(&list(d.path(), "c"));
        assert!(p.contains(&format!("{id}: \"Calc\" (app, v1, calc.html)")), "{p}");
        assert_eq!(prompt_list(&[]), "");
    }
}
