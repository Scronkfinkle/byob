//! `read_file`: bounded text preview of a file, by line range.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};

use serde_json::{Value, json};

use super::{arg_str, bytes, opt_u64, req_string, short_name};
use crate::{Access, Tool, ToolContext, ToolOutput, ToolSpec, sandbox};

/// Most bytes of file text returned by one call.
pub const MAX_READ_BYTES: usize = 256 * 1024;
/// Default and maximum number of lines returned by one call.
pub const DEFAULT_LINES: u64 = 2000;
/// Longer lines are cut.
pub const MAX_LINE_BYTES: usize = 4000;
/// Bytes sniffed to tell text from binary.
const SNIFF: usize = 8192;
/// Past this size we stop counting lines after the requested range.
const COUNT_LIMIT: u64 = 32 * 1024 * 1024;

pub struct ReadFile;

impl Tool for ReadFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".into(),
            description:
                "Read a text file in the workspace, or look at a PNG/JPEG image. Long files come back in parts: pass offset (first line, from 1) and limit (number of lines) to read more."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, relative to the workspace"},
                    "offset": {"type": "integer", "description": "First line to read, starting at 1 (default 1)"},
                    "limit": {"type": "integer", "description": "How many lines to read (default and max 2000)"}
                },
                "required": ["path"]
            }),
        }
    }

    fn title(&self, args: &Value) -> String {
        format!("Read {}", short_name(arg_str(args, "path").unwrap_or("a file")))
    }

    fn pending_title(&self, args: &Value) -> String {
        self.title(args)
    }

    /// The title says it all.
    fn preview(&self, _args: &Value, _workspace: &std::path::Path) -> Option<crate::Preview> {
        None
    }

    fn access(&self, _args: &Value) -> Access {
        Access::Read
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        match read(args, ctx) {
            Ok(out) => out,
            Err(e) => ToolOutput::err(e),
        }
    }
}

fn read(args: &Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, String> {
    let path = req_string(args, "path")?;
    let offset = opt_u64(args, "offset")?.unwrap_or(1).max(1);
    let limit = opt_u64(args, "limit")?.unwrap_or(DEFAULT_LINES).clamp(1, DEFAULT_LINES);
    let file = sandbox::resolve_follow(ctx.workspace, &path)?;
    let name = file.show();
    let meta = std::fs::metadata(&file.abs).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!("{name} doesn't exist; use list_files to see what's there"),
        _ => format!("can't open {name}: {e}"),
    })?;
    if meta.is_dir() {
        return Err(format!("{name} is a folder; use list_files to see what's in it"));
    }
    if !meta.is_file() {
        return Err(format!("{name} is not a regular file"));
    }
    let size = meta.len();
    let mut f = File::open(&file.abs).map_err(|e| format!("can't open {name}: {e}"))?;

    let mut head = Vec::with_capacity(SNIFF);
    (&mut f).take(SNIFF as u64).read_to_end(&mut head).map_err(|e| format!("can't read {name}: {e}"))?;
    if size == 0 || head.is_empty() {
        return Ok(ToolOutput::ok(format!("File: {name} (empty)")));
    }
    if crate::images::sniff(&head).is_some() {
        // A picture: the model gets to look at it (vision models; others are told they can't).
        return Ok(match crate::images::for_model(&file.abs) {
            Ok(url) => ToolOutput {
                images: vec![url],
                ..ToolOutput::ok(format!("File: {name} ({}, an image; it is shown to you after the tool results)", bytes(size)))
            },
            Err(e) => ToolOutput::ok(format!("File: {name} ({}, an image)\nIt can't be shown: {e}", bytes(size))),
        });
    }
    if looks_binary(&head) {
        return Ok(ToolOutput::ok(format!("File: {name} ({})\nThis is a binary file; it can't be shown as text.", bytes(size))));
    }

    let mut reader = BufReader::new(std::io::Cursor::new(head).chain(f));
    let mut out = String::new();
    let mut line = Vec::new();
    let mut line_no: u64 = 0;
    let mut first_shown = 0;
    let mut last_shown = 0;
    let mut cut_lines = false;
    let mut lossy = false;
    let mut full = false;
    let mut total_known = true;
    loop {
        if line_no.is_multiple_of(4096) && ctx.cancelled() {
            total_known = false;
            break;
        }
        let Some(cut) = next_line(&mut reader, &mut line, MAX_LINE_BYTES).map_err(|e| format!("can't read {name}: {e}"))? else {
            break;
        };
        line_no = line_no.saturating_add(1);
        if line_no < offset {
            continue;
        }
        if full || line_no.saturating_sub(offset) >= limit {
            full = true;
            if size > COUNT_LIMIT {
                total_known = false;
                break;
            }
            continue;
        }
        let text = String::from_utf8_lossy(&line);
        lossy |= matches!(text, std::borrow::Cow::Owned(_));
        if !out.is_empty() && out.len().saturating_add(text.len()).saturating_add(1) > MAX_READ_BYTES {
            full = true;
            continue;
        }
        if !out.is_empty() || first_shown != 0 {
            out.push('\n');
        }
        out.push_str(&text);
        if cut {
            cut_lines = true;
            out.push_str(" … (line cut)");
        }
        if first_shown == 0 {
            first_shown = line_no;
        }
        last_shown = line_no;
    }

    let total = if total_known { format!("{line_no} lines") } else { "many lines".into() };
    let mut header = format!("File: {name} ({}, {total})", bytes(size));
    if first_shown == 0 {
        let _ = write!(header, "\nOffset {offset} is past the end of the file.");
        return Ok(ToolOutput::ok(header));
    }
    let whole = first_shown == 1 && !full;
    if !whole {
        let _ = write!(header, "\nShowing lines {first_shown}-{last_shown}.");
    }
    if lossy {
        header.push_str("\nNot valid UTF-8; unreadable bytes are shown as �.");
    }
    if cut_lines {
        let _ = write!(header, "\nLines longer than {MAX_LINE_BYTES} bytes are cut.");
    }
    let mut text = format!("{header}\n---\n{out}");
    if full {
        let _ = write!(text, "\n--- more follows: call read_file with offset={}", last_shown.saturating_add(1));
    }
    Ok(ToolOutput::ok(text))
}

/// Text unless it has NUL bytes or is mostly control characters.
fn looks_binary(head: &[u8]) -> bool {
    if head.contains(&0) {
        return true;
    }
    let ctrl = head.iter().filter(|&&b| b < 0x20 && !matches!(b, b'\t' | b'\n' | b'\r' | 0x0c | 0x1b)).count();
    ctrl.saturating_mul(10) > head.len()
}

/// Read one line into `buf` (without its line ending), keeping at most `cap` bytes.
/// Returns `None` at end of file, else whether the line was cut.
fn next_line(r: &mut impl BufRead, buf: &mut Vec<u8>, cap: usize) -> std::io::Result<Option<bool>> {
    buf.clear();
    let mut any = false;
    let mut cut = false;
    loop {
        let chunk = r.fill_buf()?;
        if chunk.is_empty() {
            break;
        }
        any = true;
        let (take, done) = match chunk.iter().position(|&b| b == b'\n') {
            Some(i) => (i, true),
            None => (chunk.len(), false),
        };
        let room = cap.saturating_sub(buf.len());
        let part = chunk.get(..take.min(room)).unwrap_or_default();
        buf.extend_from_slice(part);
        cut |= take > room;
        r.consume(if done { take.saturating_add(1) } else { take });
        if done {
            break;
        }
    }
    if !any {
        return Ok(None);
    }
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    if cut {
        // don't leave half a UTF-8 character at the cut
        while buf.last().is_some_and(|b| b & 0xC0 == 0x80) {
            buf.pop();
        }
        if buf.last().is_some_and(|b| *b >= 0xC0) {
            buf.pop();
        }
    }
    Ok(Some(cut))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::fs::testutil::run;

    fn ws_with(name: &str, content: &[u8]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join(name), content).unwrap();
        d
    }

    #[test]
    fn images_are_shown_to_the_model() {
        let mut png = Vec::new();
        image::RgbImage::from_pixel(3000, 1500, image::Rgb([200, 30, 30])).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        let d = ws_with("sheet.png", &png);
        let out = run(&ReadFile, d.path(), json!({"path": "sheet.png"}));
        assert!(!out.failed && out.text.contains("an image; it is shown to you"), "{}", out.text);
        assert_eq!(out.images.len(), 1);
        let url = &out.images[0];
        let (mime, data) = url.strip_prefix("data:").and_then(|u| u.split_once(";base64,")).unwrap();
        let bytes = byob_protocol::media::decode(data).unwrap();
        let img = image::load_from_memory(&bytes).unwrap();
        // Downscaled to fit the model's side.
        assert_eq!((mime, img.width(), img.height()), ("image/jpeg", 1024, 512));

        // Not an image after all: said so, no picture.
        let d = ws_with("bad.png", b"\x89PNG\r\n\x1a\nnot really");
        let out = run(&ReadFile, d.path(), json!({"path": "bad.png"}));
        assert!(!out.failed && out.text.contains("can't be shown") && out.images.is_empty(), "{}", out.text);
    }

    #[test]
    fn reads_small_file_whole() {
        let d = ws_with("notes.md", b"one\ntwo\r\nthree\n");
        let out = run(&ReadFile, d.path(), json!({"path": "notes.md"}));
        assert!(!out.failed);
        assert_eq!(out.text, "File: notes.md (15 B, 3 lines)\n---\none\ntwo\nthree");
    }

    #[test]
    fn line_ranges() {
        let body: String = (1..=10).map(|i| format!("l{i}\n")).collect();
        let d = ws_with("a.txt", body.as_bytes());
        let out = run(&ReadFile, d.path(), json!({"path": "a.txt", "offset": 3, "limit": "2"}));
        assert!(out.text.contains("10 lines"), "{}", out.text);
        assert!(out.text.contains("Showing lines 3-4."));
        assert!(out.text.ends_with("---\nl3\nl4\n--- more follows: call read_file with offset=5"), "{}", out.text);
        let out = run(&ReadFile, d.path(), json!({"path": "a.txt", "offset": 9}));
        assert!(out.text.ends_with("---\nl9\nl10"), "{}", out.text);
        let out = run(&ReadFile, d.path(), json!({"path": "a.txt", "offset": 50}));
        assert!(out.text.contains("past the end"));
        assert!(!out.failed);
    }

    #[test]
    fn empty_lines_kept() {
        let d = ws_with("a.txt", b"\n\nx\n");
        let out = run(&ReadFile, d.path(), json!({"path": "a.txt"}));
        assert!(out.text.ends_with("---\n\n\nx"), "{:?}", out.text);
        let out = run(&ReadFile, d.path(), json!({"path": "a.txt", "offset": 2}));
        assert!(out.text.ends_with("---\n\nx"), "{:?}", out.text);
    }

    #[test]
    fn byte_cap() {
        let line = "y".repeat(1000);
        let body = format!("{line}\n").repeat(1000);
        let d = ws_with("big.txt", body.as_bytes());
        let out = run(&ReadFile, d.path(), json!({"path": "big.txt"}));
        assert!(out.text.len() < MAX_READ_BYTES + 500);
        assert!(out.text.contains("more follows: call read_file with offset="));
    }

    #[test]
    fn long_line_is_cut_without_splitting_utf8() {
        let body = "é".repeat(5000);
        let d = ws_with("a.txt", body.as_bytes());
        let out = run(&ReadFile, d.path(), json!({"path": "a.txt"}));
        assert!(out.text.contains("(line cut)"));
        assert!(!out.text.contains('�'));
    }

    #[test]
    fn binary_and_non_utf8() {
        let d = ws_with("x.png", &[0x89, b'P', b'N', b'G', 0, 0, 1, 2]);
        let out = run(&ReadFile, d.path(), json!({"path": "x.png"}));
        assert!(out.text.contains("binary file"), "{}", out.text);
        assert!(!out.failed);
        let d = ws_with("latin1.txt", b"caf\xe9\n");
        let out = run(&ReadFile, d.path(), json!({"path": "latin1.txt"}));
        assert!(out.text.contains("caf�"));
        assert!(out.text.contains("Not valid UTF-8"));
        let d = ws_with("e.txt", b"");
        assert_eq!(run(&ReadFile, d.path(), json!({"path": "e.txt"})).text, "File: e.txt (empty)");
    }

    #[test]
    fn refusals() {
        let d = ws_with("a.txt", b"x");
        fs::create_dir(d.path().join("dir")).unwrap();
        for args in
            [json!({}), json!({"path": 1}), json!({"path": "../a.txt"}), json!({"path": "/etc/passwd"}), json!({"path": "dir"}), json!({"path": "nope"})]
        {
            assert!(run(&ReadFile, d.path(), args.clone()).failed, "{args}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_out_refused_in_ok() {
        let outside = ws_with("secret", b"s3cret");
        let d = ws_with("a.txt", b"x");
        std::os::unix::fs::symlink(outside.path().join("secret"), d.path().join("leak")).unwrap();
        std::os::unix::fs::symlink(d.path().join("a.txt"), d.path().join("alias")).unwrap();
        let out = run(&ReadFile, d.path(), json!({"path": "leak"}));
        assert!(out.failed && !out.text.contains("s3cret"));
        let out = run(&ReadFile, d.path(), json!({"path": "alias"}));
        assert!(!out.failed && out.text.ends_with("---\nx"), "{}", out.text);
    }

    #[test]
    fn title() {
        assert_eq!(ReadFile.title(&json!({"path": "docs/notes.md"})), "Read notes.md");
        assert_eq!(ReadFile.title(&json!(42)), "Read a file");
        assert_eq!(ReadFile.pending_title(&json!({"path": "docs/notes.md"})), "Read notes.md");
    }
}
