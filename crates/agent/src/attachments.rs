//! Files in the conversation, engine side:
//!
//! - **Uploads**: chunks from [`ClientMsg::Upload`](byob_protocol::ClientMsg::Upload) are
//!   staged in `.byob/incoming/` and moved to `uploads/<safe, unique name>` in the workspace
//!   when complete. Only paths that came out of an upload may be attached to a message.
//! - **Telling the model**: an attached file is named in the user message with its path and
//!   size; a CSV gets its header and first rows; a PNG/JPEG goes along as an `image_url`
//!   part (downscaled) for vision models.
//! - **Media**: charts and images a tool created become [`MediaRef`]s on its step; clients
//!   fetch them with `GetMedia`, answered only for paths the transcript references.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{Cursor, ErrorKind, Read as _, Write as _};
use std::path::{Path, PathBuf};

use byob_llm::ChatMessage;
use byob_protocol::media::{self, MAX_ATTACHMENTS, MAX_IMAGE_BYTES, MAX_UPLOAD_BYTES, UPLOAD_CHUNK_BYTES};
use byob_protocol::{Entry, MediaContent, MediaKind, MediaRef, ServerMsg};
use byob_tools::sandbox;

/// Workspace folder uploads are saved in.
pub const UPLOAD_DIR: &str = "uploads";
/// Staging folder for unfinished uploads (inside byob's reserved `.byob`).
const INCOMING_DIR: &str = ".byob/incoming";
/// Unfinished uploads one client may have at a time, and all clients together.
const MAX_OPEN_PER_CLIENT: usize = 8;
const MAX_OPEN_TOTAL: usize = 32;
/// Longest file name kept (characters, extension included).
const MAX_NAME_CHARS: usize = 100;
/// Chart files larger than this are not served.
const MAX_CHART_FILE: u64 = 8 * 1024 * 1024;
/// Images larger than this on disk are not decoded.
const MAX_IMAGE_FILE: u64 = 64 * 1024 * 1024;
/// Longest side of an image sent to clients, and to the model.
const CLIENT_IMAGE_SIDE: u32 = 2048;
const MODEL_IMAGE_SIDE: u32 = 1024;
/// Decoder limits: refuse decompression bombs.
const MAX_DECODE_SIDE: u32 = 16_384;
const MAX_DECODE_ALLOC: u64 = 512 * 1024 * 1024;
/// CSV preview given to the model.
const PREVIEW_LINES: usize = 8;
const PREVIEW_CHARS: usize = 2_000;

type ClientId = u64;

struct Partial {
    file: File,
    staged: PathBuf,
    name: String,
    size: u64,
    written: u64,
}

/// Unfinished uploads and the paths finished ones were saved to.
#[derive(Default)]
pub struct Uploads {
    open: BTreeMap<(ClientId, u64), Partial>,
    /// Uploads that failed; their remaining chunks are dropped silently.
    failed: BTreeSet<(ClientId, u64)>,
    /// Workspace-relative paths of finished uploads (may be attached to messages).
    saved: BTreeMap<String, u64>,
}

/// One upload chunk, as received.
pub struct Chunk<'a> {
    pub upload: u64,
    pub name: &'a str,
    pub size: u64,
    pub offset: u64,
    pub data: &'a str,
    pub done: bool,
}

impl Uploads {
    /// Take one chunk. Returns the reply for the uploader, if this chunk ends the upload.
    pub fn chunk(&mut self, workspace: &Path, client: ClientId, c: &Chunk<'_>) -> Option<ServerMsg> {
        let key = (client, c.upload);
        if self.failed.contains(&key) {
            return None;
        }
        let result = self.take(workspace, key, c);
        let reply = |attachment: Option<MediaRef>, error: Option<String>| ServerMsg::Uploaded { upload: c.upload, attachment, error };
        match result {
            Ok(None) => None,
            Ok(Some(r)) => {
                self.saved.insert(r.path.clone(), r.size);
                Some(reply(Some(r), None))
            }
            Err(e) => {
                if let Some(p) = self.open.remove(&key) {
                    let _ = std::fs::remove_file(&p.staged);
                }
                if !c.done {
                    self.failed.insert(key);
                }
                Some(reply(None, Some(e)))
            }
        }
    }

    fn take(&mut self, workspace: &Path, key: (ClientId, u64), c: &Chunk<'_>) -> Result<Option<MediaRef>, String> {
        let bytes = media::decode(c.data)?;
        if bytes.len() > UPLOAD_CHUNK_BYTES {
            return Err("upload chunk is too big".into());
        }
        if !self.open.contains_key(&key) {
            if c.offset != 0 {
                return Err("upload chunk out of order".into());
            }
            self.start(workspace, key, c)?;
        }
        let p = self.open.get_mut(&key).ok_or("upload vanished")?;
        if c.offset != p.written || c.size != p.size {
            return Err("upload chunk out of order".into());
        }
        let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        let written = p.written.checked_add(len).filter(|w| *w <= p.size).ok_or_else(|| format!("{} is bigger than announced", p.name))?;
        p.file.write_all(&bytes).map_err(|e| format!("can't save {}: {e}", p.name))?;
        p.written = written;
        if !c.done {
            return Ok(None);
        }
        if p.written != p.size {
            return Err(format!("{} arrived incomplete", p.name));
        }
        let p = self.open.remove(&key).ok_or("upload vanished")?;
        finish(workspace, p).map(Some)
    }

    fn start(&mut self, workspace: &Path, key: (ClientId, u64), c: &Chunk<'_>) -> Result<(), String> {
        let name = safe_name(c.name);
        if c.size > MAX_UPLOAD_BYTES {
            return Err(format!("{name} is too big ({}; the limit is {})", media::size_label(c.size), media::size_label(MAX_UPLOAD_BYTES)));
        }
        if self.open.len() >= MAX_OPEN_TOTAL || self.open.keys().filter(|(cl, _)| *cl == key.0).count() >= MAX_OPEN_PER_CLIENT {
            return Err("too many uploads at once; wait for the others to finish".into());
        }
        let dir = sandbox::resolve(workspace, INCOMING_DIR)?;
        std::fs::create_dir_all(&dir.abs).map_err(|e| format!("can't stage the upload: {e}"))?;
        let staged = dir.abs.join(format!("{}-{}.part", key.0, key.1));
        let file = OpenOptions::new().write(true).create(true).truncate(true).open(&staged).map_err(|e| format!("can't stage the upload: {e}"))?;
        self.open.insert(key, Partial { file, staged, name, size: c.size, written: 0 });
        Ok(())
    }

    /// A client went away: drop its unfinished uploads.
    pub fn forget(&mut self, client: ClientId) {
        let keys: Vec<_> = self.open.keys().filter(|(c, _)| *c == client).copied().collect();
        for k in keys {
            if let Some(p) = self.open.remove(&k) {
                let _ = std::fs::remove_file(&p.staged);
            }
        }
        self.failed.retain(|(c, _)| *c != client);
    }

    /// The attachments for a message: only paths that came out of an upload, at most
    /// [`MAX_ATTACHMENTS`], no duplicates.
    pub fn claim(&self, paths: &[String]) -> Result<Vec<MediaRef>, String> {
        let mut out: Vec<MediaRef> = Vec::new();
        for p in paths {
            let size = *self.saved.get(p).ok_or_else(|| format!("{p} isn't an uploaded file; attach it again"))?;
            if !out.iter().any(|r| &r.path == p) {
                out.push(MediaRef::new(p.clone(), size));
            }
        }
        if out.len() > MAX_ATTACHMENTS {
            return Err(format!("at most {MAX_ATTACHMENTS} files per message"));
        }
        Ok(out)
    }
}

/// Move a complete upload into `uploads/` under a name nothing else uses.
fn finish(workspace: &Path, p: Partial) -> Result<MediaRef, String> {
    drop(p.file);
    let result = (|| {
        let dir = sandbox::resolve(workspace, UPLOAD_DIR)?;
        match std::fs::symlink_metadata(&dir.abs) {
            Ok(m) if m.file_type().is_symlink() || !m.is_dir() => return Err(format!("{UPLOAD_DIR} in the workspace is not a folder")),
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => std::fs::create_dir_all(&dir.abs).map_err(|e| format!("can't create {UPLOAD_DIR}: {e}"))?,
            Err(e) => return Err(format!("can't access {UPLOAD_DIR}: {e}")),
        }
        let target = unique_path(&dir.abs, &p.name).ok_or_else(|| format!("too many files called {}", p.name))?;
        // Copy into a file created exclusively (never through a link), then drop the stage.
        let mut out = OpenOptions::new().write(true).create_new(true).open(&target).map_err(|e| format!("can't save {}: {e}", p.name))?;
        let mut src = File::open(&p.staged).map_err(|e| format!("can't save {}: {e}", p.name))?;
        std::io::copy(&mut src, &mut out).map_err(|e| format!("can't save {}: {e}", p.name))?;
        let rel = target.strip_prefix(&dir.root).map(Path::to_path_buf).unwrap_or_else(|_| target.clone());
        Ok(MediaRef::new(rel.to_string_lossy().replace('\\', "/"), p.size))
    })();
    let _ = std::fs::remove_file(&p.staged);
    result
}

/// A file name from a client, made safe: last component only, no control or reserved
/// characters, no leading dots, bounded length (keeping the extension).
pub fn safe_name(raw: &str) -> String {
    let last = raw.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned: String = last.chars().map(|c| if c.is_control() || matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|') { ' ' } else { c }).collect();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let cleaned = cleaned.trim_start_matches(['.', ' ']).trim_end_matches(['.', ' ']).to_owned();
    let name = if cleaned.is_empty() { "file".to_owned() } else { cleaned };
    if name.chars().count() <= MAX_NAME_CHARS {
        return name;
    }
    let (stem, ext) = split_ext(&name);
    let ext: String = ext.chars().take(16).collect();
    let stem: String = stem.chars().take(MAX_NAME_CHARS.saturating_sub(ext.chars().count())).collect();
    format!("{stem}{ext}")
}

/// "a.tar.gz" → ("a.tar", ".gz"); "notes" → ("notes", "").
fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => (name.get(..i).unwrap_or(name), name.get(i..).unwrap_or("")),
        _ => (name, ""),
    }
}

/// `dir/name`, or `dir/name (2).ext`, … — the first that doesn't exist yet.
fn unique_path(dir: &Path, name: &str) -> Option<PathBuf> {
    let (stem, ext) = split_ext(name);
    (1..10_000u32).map(|n| if n == 1 { dir.join(name) } else { dir.join(format!("{stem} ({n}){ext}")) }).find(|p| std::fs::symlink_metadata(p).is_err())
}

/// Charts and images among a tool call's file changes, for its step.
pub fn media_for_changes(changes: &[byob_tools::FileChange], workspace: &Path) -> Vec<MediaRef> {
    let root = workspace.canonicalize().unwrap_or_else(|_| workspace.to_path_buf());
    changes
        .iter()
        .filter_map(|c| match c {
            byob_tools::FileChange::Created(p) | byob_tools::FileChange::Modified(p) | byob_tools::FileChange::Moved { to: p, .. } => Some(p),
            byob_tools::FileChange::Deleted(_) => None,
        })
        .filter_map(|p| {
            let rel = p.strip_prefix(&root).or_else(|_| p.strip_prefix(workspace)).ok()?;
            let r = MediaRef::new(rel.to_string_lossy().replace('\\', "/"), std::fs::metadata(p).map(|m| m.len()).unwrap_or(0));
            (r.kind != MediaKind::File).then_some(r)
        })
        .collect()
}

/// Whether the transcript shows `path` (so a client may fetch it).
pub fn referenced(transcript: &[Entry], path: &str) -> bool {
    transcript.iter().any(|e| e.attachments.iter().any(|a| a.path == path) || e.steps.iter().any(|s| s.media.iter().any(|m| m.path == path)))
}

/// The content behind a media path, for a client. Runs off the engine thread.
pub fn load(workspace: &Path, path: &str) -> MediaContent {
    let error = |message: String| MediaContent::Error { message };
    let file = match sandbox::resolve_follow(workspace, path) {
        Ok(f) => f.abs,
        Err(e) => return error(e),
    };
    match MediaKind::for_path(path) {
        MediaKind::Chart => match read_capped(&file, MAX_CHART_FILE) {
            Ok(bytes) => match byob_protocol::media::ChartSpec::from_json(&String::from_utf8_lossy(&bytes)) {
                Ok(spec) => MediaContent::Chart { spec },
                Err(e) => error(e),
            },
            Err(e) => error(e),
        },
        MediaKind::Image => match read_capped(&file, MAX_IMAGE_FILE).and_then(|b| fit_image(b, CLIENT_IMAGE_SIDE, MAX_IMAGE_BYTES)) {
            Ok((mime, data)) => MediaContent::Image { mime: mime.into(), data: media::encode(&data) },
            Err(e) => error(e),
        },
        MediaKind::File => error("not a chart or image".into()),
    }
}

/// Answer a `GetMedia`: only for paths the transcript shows.
pub fn answer(transcript: &[Entry], workspace: &Path, path: &str) -> MediaContent {
    if referenced(transcript, path) { load(workspace, path) } else { MediaContent::Error { message: "not part of this conversation".into() } }
}

/// One artifact version's bytes for a client: as saved, except an image too big for one
/// message, which is downscaled. Runs off the engine thread.
pub fn artifact_for_client(workspace: &Path, chat: &str, id: &str, version: u32) -> Result<Vec<u8>, String> {
    let (artifact, bytes) = byob_tools::artifact::read_version(workspace, chat, id, version, byob_tools::artifact::MAX_FILE_BYTES)?;
    if bytes.len() <= byob_protocol::artifact::MAX_ARTIFACT_SEND_BYTES {
        return Ok(bytes);
    }
    if artifact.kind == byob_protocol::ArtifactKind::Image {
        return fit_image(bytes, CLIENT_IMAGE_SIDE, byob_protocol::artifact::MAX_ARTIFACT_SEND_BYTES).map(|(_, b)| b);
    }
    Err(format!("{} is too big to show here ({}); it's in the workspace", artifact.filename, media::size_label(bytes.len() as u64)))
}

fn read_capped(path: &Path, max: u64) -> Result<Vec<u8>, String> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let meta = std::fs::metadata(path).map_err(|e| format!("{name}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{name} is not a file"));
    }
    if meta.len() > max {
        return Err(format!("{name} is too big to show ({})", media::size_label(meta.len())));
    }
    let mut out = Vec::new();
    File::open(path).and_then(|f| f.take(max).read_to_end(&mut out)).map_err(|e| format!("{name}: {e}"))?;
    Ok(out)
}

fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else {
        None
    }
}

/// A PNG/JPEG no larger than `max_side` px and `max_bytes`: passed through when it already
/// fits, else decoded (with limits), downscaled and re-encoded.
fn fit_image(bytes: Vec<u8>, max_side: u32, max_bytes: usize) -> Result<(&'static str, Vec<u8>), String> {
    let mime = sniff(&bytes).ok_or("not a PNG or JPEG image")?;
    let mut reader = image::ImageReader::new(Cursor::new(&bytes));
    reader.set_format(if mime == "image/png" { image::ImageFormat::Png } else { image::ImageFormat::Jpeg });
    let (w, h) = reader.into_dimensions().map_err(|e| format!("unreadable image: {e}"))?;
    if bytes.len() <= max_bytes && w <= max_side && h <= max_side {
        return Ok((mime, bytes));
    }
    let mut reader = image::ImageReader::new(Cursor::new(&bytes));
    reader.set_format(if mime == "image/png" { image::ImageFormat::Png } else { image::ImageFormat::Jpeg });
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DECODE_SIDE);
    limits.max_image_height = Some(MAX_DECODE_SIDE);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    let img = reader.decode().map_err(|e| format!("unreadable image: {e}"))?;
    let mut side = max_side;
    loop {
        let small = if img.width() > side || img.height() > side { img.thumbnail(side, side) } else { img.clone() };
        let mut out = Vec::new();
        let (fmt, mime) = if small.color().has_alpha() { (image::ImageFormat::Png, "image/png") } else { (image::ImageFormat::Jpeg, "image/jpeg") };
        let small = if fmt == image::ImageFormat::Jpeg { image::DynamicImage::ImageRgb8(small.to_rgb8()) } else { small };
        small.write_to(&mut Cursor::new(&mut out), fmt).map_err(|e| format!("can't re-encode image: {e}"))?;
        if out.len() <= max_bytes || side <= 256 {
            return Ok((mime, out));
        }
        side = side.saturating_mul(3) / 4;
    }
}

/// An earlier user message as the model sees it again: the text plus which files came with it.
pub fn with_names(text: &str, attachments: &[MediaRef]) -> String {
    if attachments.is_empty() {
        return text.to_owned();
    }
    let names: Vec<&str> = attachments.iter().map(|a| a.path.as_str()).collect();
    format!("{text}\n\n[Attached: {}]", names.join(", ")).trim_start().to_owned()
}

/// The new user message: text, a note per attached file (CSV: header and first rows), and
/// images as `image_url` parts.
pub fn user_message(workspace: &Path, text: &str, attachments: &[MediaRef]) -> ChatMessage {
    if attachments.is_empty() {
        return ChatMessage::user(text);
    }
    let mut out = text.trim().to_owned();
    let mut images = Vec::new();
    out.push_str("\n\n[The user attached these files; they are saved in the workspace]");
    for a in attachments {
        out.push_str(&format!("\n- {} ({})", a.path, media::size_label(a.size)));
        let Ok(file) = sandbox::resolve_follow(workspace, &a.path) else { continue };
        match a.kind {
            MediaKind::Image => match read_capped(&file.abs, MAX_IMAGE_FILE).and_then(|b| fit_image(b, MODEL_IMAGE_SIDE, MAX_IMAGE_BYTES)) {
                Ok((mime, data)) => {
                    out.push_str(" — image, shown below");
                    images.push(format!("data:{mime};base64,{}", media::encode(&data)));
                }
                Err(e) => out.push_str(&format!(" — can't show it: {e}")),
            },
            _ if is_table(&a.path) => out.push_str(&csv_preview(&file.abs)),
            _ => out.push_str(" — read it with read_file if you need its contents"),
        }
    }
    ChatMessage::user_with_images(out.trim_start(), images)
}

fn is_table(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".csv") || lower.ends_with(".tsv")
}

/// Header, first rows and row count of a CSV, for the model.
fn csv_preview(path: &Path) -> String {
    let Ok(bytes) = read_capped(path, MAX_UPLOAD_BYTES) else { return String::new() };
    let text = String::from_utf8_lossy(&bytes);
    let rows = text.lines().filter(|l| !l.trim().is_empty()).count().saturating_sub(1);
    let mut head = String::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()).take(PREVIEW_LINES) {
        if head.len().saturating_add(line.len()) > PREVIEW_CHARS {
            head.push_str("…\n");
            break;
        }
        head.push_str(line.trim_end_matches('\r'));
        head.push('\n');
    }
    format!(" — table with {rows} data rows. First lines:\n```csv\n{head}```\nTo chart it, call make_chart with file set to this path.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk<'a>(upload: u64, name: &'a str, size: u64, offset: u64, data: &'a str, done: bool) -> Chunk<'a> {
        Chunk { upload, name, size, offset, data, done }
    }

    fn upload_all(u: &mut Uploads, ws: &Path, client: u64, id: u64, name: &str, bytes: &[u8]) -> ServerMsg {
        let size = bytes.len() as u64;
        let pieces: Vec<&[u8]> = if bytes.is_empty() { vec![&[][..]] } else { bytes.chunks(UPLOAD_CHUNK_BYTES).collect() };
        let mut offset = 0;
        let mut last = None;
        for (i, piece) in pieces.iter().enumerate() {
            let data = media::encode(piece);
            last = u.chunk(ws, client, &chunk(id, name, size, offset, &data, i + 1 == pieces.len()));
            offset += piece.len() as u64;
        }
        last.unwrap()
    }

    fn saved(msg: ServerMsg) -> MediaRef {
        match msg {
            ServerMsg::Uploaded { attachment: Some(a), error: None, .. } => a,
            other => panic!("expected success, got {other:?}"),
        }
    }

    fn failed(msg: ServerMsg) -> String {
        match msg {
            ServerMsg::Uploaded { attachment: None, error: Some(e), .. } => e,
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[test]
    fn uploads_land_in_uploads_with_safe_unique_names() {
        let d = tempfile::tempdir().unwrap();
        let mut u = Uploads::default();
        let a = saved(upload_all(&mut u, d.path(), 1, 1, "../../sales.csv", b"month,sales\nJan,1\n"));
        assert_eq!(a.path, "uploads/sales.csv");
        assert_eq!(a.size, 18);
        let b = saved(upload_all(&mut u, d.path(), 2, 1, "sales.csv", b"x"));
        assert_eq!(b.path, "uploads/sales (2).csv");
        let c = saved(upload_all(&mut u, d.path(), 1, 2, "/etc/passwd", b"root"));
        assert_eq!(c.path, "uploads/passwd");
        let e = saved(upload_all(&mut u, d.path(), 1, 3, "..", b""));
        assert_eq!(e.path, "uploads/file");
        assert_eq!(std::fs::read(d.path().join("uploads/sales.csv")).unwrap(), b"month,sales\nJan,1\n");
        // the staging folder is left empty
        assert_eq!(std::fs::read_dir(d.path().join(INCOMING_DIR)).unwrap().count(), 0);
        assert_eq!(u.claim(&[a.path.clone(), a.path.clone()]).unwrap(), vec![a]);
        assert!(u.claim(&["notes.md".into()]).is_err());
        assert!(u.claim(&["uploads/../notes.md".into()]).is_err());
    }

    #[test]
    fn multi_chunk_upload() {
        let d = tempfile::tempdir().unwrap();
        let mut u = Uploads::default();
        let big: Vec<u8> = (0..UPLOAD_CHUNK_BYTES * 2 + 17).map(|i| (i % 251) as u8).collect();
        let a = saved(upload_all(&mut u, d.path(), 1, 9, "big.bin", &big));
        assert_eq!(std::fs::read(d.path().join(&a.path)).unwrap(), big);
    }

    #[test]
    fn hostile_uploads_are_refused() {
        let d = tempfile::tempdir().unwrap();
        let mut u = Uploads::default();
        let ok = media::encode(b"abc");
        assert!(failed(u.chunk(d.path(), 1, &chunk(1, "huge.bin", MAX_UPLOAD_BYTES + 1, 0, &ok, false)).unwrap()).contains("too big"));
        // later chunks of a failed upload are dropped silently
        assert!(u.chunk(d.path(), 1, &chunk(1, "huge.bin", MAX_UPLOAD_BYTES + 1, 3, &ok, false)).is_none());
        assert!(failed(u.chunk(d.path(), 1, &chunk(2, "a", 2, 0, &ok, true)).unwrap()).contains("bigger than announced"));
        assert!(failed(u.chunk(d.path(), 1, &chunk(3, "a", 10, 0, &ok, true)).unwrap()).contains("incomplete"));
        assert!(failed(u.chunk(d.path(), 1, &chunk(4, "a", 10, 5, &ok, true)).unwrap()).contains("out of order"));
        assert!(failed(u.chunk(d.path(), 1, &chunk(5, "a", 10, 0, "!!", true)).unwrap()).contains("base64"));
        let too_big_chunk = media::encode(&vec![0; UPLOAD_CHUNK_BYTES + 1]);
        assert!(failed(u.chunk(d.path(), 1, &chunk(6, "a", MAX_UPLOAD_BYTES, 0, &too_big_chunk, false)).unwrap()).contains("chunk is too big"));
        assert!(!d.path().join(UPLOAD_DIR).exists());
        // too many open at once
        for id in 10..10 + MAX_OPEN_PER_CLIENT as u64 {
            assert!(u.chunk(d.path(), 2, &chunk(id, "p", 100, 0, &ok, false)).is_none());
        }
        assert!(failed(u.chunk(d.path(), 2, &chunk(99, "p", 100, 0, &ok, false)).unwrap()).contains("too many"));
        u.forget(2);
        assert_eq!(std::fs::read_dir(d.path().join(INCOMING_DIR)).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn never_saves_through_a_linked_uploads_folder() {
        let outside = tempfile::tempdir().unwrap();
        let d = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), d.path().join(UPLOAD_DIR)).unwrap();
        let mut u = Uploads::default();
        failed(upload_all(&mut u, d.path(), 1, 1, "x.csv", b"a"));
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn names() {
        assert_eq!(safe_name("a/b\\c.txt"), "c.txt");
        assert_eq!(safe_name("..."), "file");
        assert_eq!(safe_name(".bashrc"), "bashrc");
        assert_eq!(safe_name("a\u{0}b:c?.csv"), "a b c .csv");
        let long = format!("{}.csv", "x".repeat(300));
        let s = safe_name(&long);
        assert_eq!(s.chars().count(), MAX_NAME_CHARS);
        assert!(s.ends_with(".csv"));
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 128]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(img).write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
        out
    }

    #[test]
    fn media_is_served_only_when_referenced() {
        let d = tempfile::tempdir().unwrap();
        let ws = d.path();
        std::fs::create_dir(ws.join("charts")).unwrap();
        let spec = byob_types::chart::validate(&serde_json::json!({"type": "bar", "x": "m", "series": ["v"], "data": [{"m": "a", "v": 1}]})).unwrap();
        std::fs::write(ws.join("charts/c.chart.json"), spec.to_json()).unwrap();
        std::fs::write(ws.join("plot.png"), png(40, 30)).unwrap();
        std::fs::write(ws.join("secret.png"), png(4, 4)).unwrap();
        let changes = vec![
            byob_tools::FileChange::Created(ws.join("charts/c.chart.json")),
            byob_tools::FileChange::Modified(ws.join("plot.png")),
            byob_tools::FileChange::Created(ws.join("notes.md")),
        ];
        let media = media_for_changes(&changes, ws);
        assert_eq!(
            media.iter().map(|m| (m.kind, m.path.as_str())).collect::<Vec<_>>(),
            vec![(MediaKind::Chart, "charts/c.chart.json"), (MediaKind::Image, "plot.png")]
        );
        let step = byob_protocol::Step {
            id: 1,
            tool: "t".into(),
            title: "t".into(),
            access: byob_protocol::ToolAccess::Read,
            status: byob_protocol::StepStatus::Done,
            detail: String::new(),
            changes: vec![],
            at: 0,
            preview: None,
            media,
            artifacts: vec![],
            delegation: None,
            ask: None,
            plugin: None,
        };
        let entry = Entry {
            id: 0,
            role: byob_protocol::EntryRole::Assistant,
            content: String::new(),
            reasoning: String::new(),
            done: true,
            error: None,
            stats: None,
            steps: vec![step],
            undoable: false,
            undone: false,
            attachments: vec![],
        };
        let t = vec![entry];
        assert_eq!(answer(&t, ws, "charts/c.chart.json"), MediaContent::Chart { spec });
        assert!(matches!(answer(&t, ws, "plot.png"), MediaContent::Image { mime, .. } if mime == "image/png"));
        assert!(matches!(answer(&t, ws, "secret.png"), MediaContent::Error { .. }));
        assert!(matches!(answer(&t, ws, "../etc/passwd"), MediaContent::Error { .. }));
    }

    #[test]
    fn big_images_are_downscaled() {
        let (mime, out) = fit_image(png(3000, 1000), 1024, MAX_IMAGE_BYTES).unwrap();
        assert_eq!(mime, "image/jpeg");
        let img = image::load_from_memory(&out).unwrap();
        assert_eq!((img.width(), img.height()), (1024, 341));
        let small = png(10, 10);
        assert_eq!(fit_image(small.clone(), 1024, MAX_IMAGE_BYTES).unwrap(), ("image/png", small));
        assert!(fit_image(b"GIF89a".to_vec(), 1024, MAX_IMAGE_BYTES).is_err());
        assert!(fit_image(b"\x89PNG\r\n\x1a\ngarbage".to_vec(), 1024, MAX_IMAGE_BYTES).is_err());
    }

    #[test]
    fn the_model_hears_about_attachments() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("uploads")).unwrap();
        let csv: String = std::iter::once("month,sales\n".to_owned()).chain((0..50).map(|i| format!("m{i},{i}\n"))).collect();
        std::fs::write(d.path().join("uploads/sales.csv"), &csv).unwrap();
        std::fs::write(d.path().join("uploads/p.png"), png(20, 20)).unwrap();
        let refs = vec![MediaRef::new("uploads/sales.csv", csv.len() as u64), MediaRef::new("uploads/p.png", 10)];
        let m = user_message(d.path(), "chart it", &refs);
        let text = m.content.as_deref().unwrap();
        assert!(text.starts_with("chart it\n\n[The user attached"));
        assert!(text.contains("uploads/sales.csv"));
        assert!(text.contains("table with 50 data rows"));
        assert!(text.contains("month,sales\nm0,0\n"));
        assert!(!text.contains("m20,20"));
        assert_eq!(m.images.len(), 1);
        assert!(m.images[0].starts_with("data:image/png;base64,"));
        assert_eq!(user_message(d.path(), "hi", &[]), ChatMessage::user("hi"));
        assert_eq!(with_names("see", &refs[..1]), "see\n\n[Attached: uploads/sales.csv]");
        assert_eq!(with_names("", &refs[..1]), "[Attached: uploads/sales.csv]");
    }
}
