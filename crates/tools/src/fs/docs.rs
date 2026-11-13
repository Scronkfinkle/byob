//! `read_file` for documents: PDF pages, Word and PowerPoint as Markdown (via `byob-docs`),
//! and short pointers for spreadsheets, images and formats byob can't read.
//!
//! One call returns at most [`DOC_BUDGET`] bytes (small models have small contexts, and the
//! turn caps a result at 16 KB anyway), cut at page or line boundaries, with a marker saying
//! what was shown and how to ask for the rest.

use std::fmt::Write as _;
use std::path::Path;

use byob_docs::{Kind, ooxml, page_ranges, parse_pages, pdf};
use serde_json::Value;

use super::bytes;
use crate::ToolContext;

/// Most bytes of document text one call returns.
pub const DOC_BUDGET: usize = 12_000;
/// Most pages (or slides) one call looks at.
const MAX_PAGES_PER_CALL: usize = 50;
/// A page with this many drawn shapes probably has a chart or diagram.
const DRAWING_SHAPES: usize = 30;

/// The `pages` argument: a string like "1-3,5", or a bare number.
pub(super) fn pages_arg(args: &Value) -> Result<Option<String>, String> {
    match args.get("pages") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(Value::Number(n)) => Ok(Some(n.to_string())),
        Some(_) => Err("invalid arguments: `pages` must be a string like \"1-3,5\"".into()),
    }
}

/// Read a document file (already known to be `kind`).
pub(super) fn read(kind: Kind, abs: &Path, name: &str, size: u64, args: &Value, ctx: &ToolContext<'_>) -> Result<String, String> {
    let head = |what: &str| format!("File: {name} ({what}, {})", bytes(size));
    match kind {
        Kind::Spreadsheet => Ok(format!(
            "{}\nThis is a spreadsheet. Look at it with the spreadsheet tools (describe_sheet with sheet=\"{name}\") if you have them, or with run_python.",
            head("spreadsheet")
        )),
        Kind::OtherDocument => Ok(format!(
            "{}\nThis is an older or less common document format that byob can't read. byob reads PDF, .docx and .pptx: convert it to one of those (for example with LibreOffice) or ask the user for one.",
            head("document")
        )),
        Kind::Pdf => read_pdf(abs, name, size, args, ctx),
        Kind::Docx => {
            let md = ooxml::docx_text(load(abs, name)?).map_err(|e| format!("can't read {name}: {e}"))?;
            Ok(lines(&md, &head("Word document"), name, args))
        }
        Kind::Pptx => {
            let slides = ooxml::pptx_slides(load(abs, name)?).map_err(|e| format!("can't read {name}: {e}"))?;
            let pages: Vec<(String, Option<String>)> = slides.into_iter().map(|s| (s, None)).collect();
            paged(&pages, "Slide", &head("PowerPoint"), name, args, ctx)
        }
    }
}

fn load(abs: &Path, name: &str) -> Result<Vec<u8>, String> {
    let size = std::fs::metadata(abs).map_err(|e| format!("can't open {name}: {e}"))?.len();
    if size > byob_docs::MAX_FILE_BYTES {
        return Err(format!("{name} is too big to read ({}; the limit is {})", bytes(size), bytes(byob_docs::MAX_FILE_BYTES)));
    }
    std::fs::read(abs).map_err(|e| format!("can't read {name}: {e}"))
}

fn read_pdf(abs: &Path, name: &str, size: u64, args: &Value, ctx: &ToolContext<'_>) -> Result<String, String> {
    let doc = pdf::Document::open(load(abs, name)?).map_err(|e| format!("can't read {name}: {e}"))?;
    let total = doc.page_count();
    let head = format!("File: {name} (PDF, {}, {total} page{})", bytes(size), if total == 1 { "" } else { "s" });
    if total == 0 {
        return Ok(format!("{head}\nThe PDF has no pages."));
    }
    let want = parse_pages(&pages_arg(args)?.unwrap_or_default(), total, MAX_PAGES_PER_CALL)?;
    // Text is extracted lazily, page by page, so a 500-page PDF costs only what is shown.
    let mut pages: Vec<(usize, String, Option<String>)> = Vec::new();
    let mut used = 0usize;
    for &n in &want {
        if ctx.cancelled() || used >= DOC_BUDGET {
            break;
        }
        let (text, note) = match doc.page_text(n) {
            Ok(p) => {
                let note = if p.text.trim().is_empty() {
                    Some(format!("(no text on this page: it is probably a scan or a picture; look_at_image with page={n} shows it)"))
                } else if p.images > 0 {
                    Some(format!("[this page also has {} picture(s); look_at_image with page={n} shows them]", p.images))
                } else if p.shapes >= DRAWING_SHAPES {
                    Some(format!("[this page has drawings, maybe a chart or diagram; look_at_image with page={n} shows them]"))
                } else {
                    None
                };
                (p.text, note)
            }
            Err(e) => (String::new(), Some(format!("(can't read this page: {e})"))),
        };
        used = used.saturating_add(text.len()).saturating_add(note.as_ref().map_or(0, String::len));
        pages.push((n, text, note));
    }
    let shown: Vec<(String, Option<String>)> = pages.iter().map(|(_, t, n)| (t.clone(), n.clone())).collect();
    let numbers: Vec<usize> = pages.iter().map(|(n, _, _)| *n).collect();
    Ok(render_pages(&shown, &numbers, &want, total, "Page", &head, name))
}

/// Slides (or other pre-split pages) with the `pages` argument applied.
fn paged(all: &[(String, Option<String>)], unit: &str, head: &str, name: &str, args: &Value, ctx: &ToolContext<'_>) -> Result<String, String> {
    let total = all.len();
    if total == 0 {
        return Ok(format!("{head}\nThere is no text in it."));
    }
    let want = parse_pages(&pages_arg(args)?.unwrap_or_default(), total, MAX_PAGES_PER_CALL)?;
    let mut shown = Vec::new();
    let mut numbers = Vec::new();
    for &n in &want {
        if ctx.cancelled() {
            break;
        }
        if let Some(p) = n.checked_sub(1).and_then(|i| all.get(i)) {
            shown.push(p.clone());
            numbers.push(n);
        }
    }
    let head = format!("{head}, {total} {}{}", unit.to_lowercase(), if total == 1 { "" } else { "s" });
    Ok(render_pages(&shown, &numbers, &want, total, unit, &head, name))
}

/// Pages under `--- Page n of T ---` markers, within [`DOC_BUDGET`]: whole pages while they
/// fit (the first one cut if it alone is too long), then a pointer to the rest of `want`.
fn render_pages(pages: &[(String, Option<String>)], numbers: &[usize], want: &[usize], total: usize, unit: &str, head: &str, name: &str) -> String {
    let mut body = String::new();
    let mut done: Vec<usize> = Vec::new();
    let mut cut_page = None;
    for ((text, note), &n) in pages.iter().zip(numbers) {
        let mut part = format!("--- {unit} {n} of {total} ---\n");
        if !text.trim().is_empty() {
            part.push_str(text.trim());
            part.push('\n');
        }
        if let Some(note) = note {
            part.push_str(note);
            part.push('\n');
        }
        if body.len().saturating_add(part.len()) > DOC_BUDGET {
            if done.is_empty() {
                let mut cut = super::prefix(&part, DOC_BUDGET).to_owned();
                cut.push_str(&format!("\n… ({} {n} is cut here; it is too long to show whole)\n", unit.to_lowercase()));
                body.push_str(&cut);
                done.push(n);
                cut_page = Some(n);
            }
            break;
        }
        body.push_str(&part);
        done.push(n);
    }
    let rest: Vec<usize> = want.iter().copied().filter(|n| !done.contains(n)).collect();
    let mut out = head.to_owned();
    let all: Vec<usize> = (1..=total).collect();
    if done != all {
        let _ = write!(
            out,
            "\nShowing {} {} of {total}.",
            if done.len() == 1 { unit.to_lowercase() } else { format!("{}s", unit.to_lowercase()) },
            page_ranges(&done)
        );
    }
    if let Some(n) = cut_page {
        let _ = write!(out, " ({} {n} is long and was cut.)", unit.to_lowercase());
    }
    out.push('\n');
    out.push_str(body.trim_end());
    if !rest.is_empty() {
        let _ = write!(out, "\n--- more follows: call read_file with path=\"{name}\" and pages=\"{}\"", page_ranges(&rest));
    }
    out
}

/// Text documents (Word) by lines, honouring `offset`/`limit` like plain text files.
fn lines(text: &str, head: &str, name: &str, args: &Value) -> String {
    let offset = super::opt_u64(args, "offset").ok().flatten().unwrap_or(1).max(1);
    let limit = super::opt_u64(args, "limit").ok().flatten().unwrap_or(super::read::DEFAULT_LINES).clamp(1, super::read::DEFAULT_LINES);
    let all: Vec<&str> = text.lines().collect();
    let total = all.len();
    if total == 0 {
        return format!("{head}\nThere is no text in it.");
    }
    let start = usize::try_from(offset).unwrap_or(usize::MAX).saturating_sub(1);
    if start >= total {
        return format!("{head}, {total} lines as text\nOffset {offset} is past the end.");
    }
    let mut body = String::new();
    let mut last = start;
    let max = usize::try_from(limit).unwrap_or(usize::MAX);
    for (i, line) in all.iter().enumerate().skip(start).take(max) {
        if !body.is_empty() && body.len().saturating_add(line.len()).saturating_add(1) > DOC_BUDGET {
            break;
        }
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(super::prefix(line, DOC_BUDGET));
        last = i;
    }
    let shown_to = last.saturating_add(1);
    let mut out = format!("{head}, {total} lines as text");
    if start > 0 || shown_to < total {
        let _ = write!(out, "\nShowing lines {}-{shown_to} of {total}.", start.saturating_add(1));
    }
    let _ = write!(out, "\n---\n{body}");
    if shown_to < total {
        let _ = write!(out, "\n--- more follows: call read_file with path=\"{name}\" and offset={}", shown_to.saturating_add(1));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ReadFile;
    use crate::fs::testutil::run;
    use byob_docs::fixtures;

    fn ws(files: &[(&str, Vec<u8>)]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for (name, data) in files {
            std::fs::write(d.path().join(name), data).unwrap();
        }
        d
    }

    #[test]
    fn read_file_reads_pdf_pages() {
        let many: Vec<Vec<(u32, u32, u32, &str)>> = (0..3).map(|_| vec![(72, 700, 12, "Some text")]).collect();
        let refs: Vec<&[(u32, u32, u32, &str)]> = many.iter().map(Vec::as_slice).collect();
        let d = ws(&[("report.pdf", fixtures::sample_pdf()), ("three.pdf", fixtures::make_pdf(&refs, 0))]);
        let out = run(&ReadFile, d.path(), serde_json::json!({"path": "report.pdf"}));
        assert!(!out.failed, "{}", out.text);
        assert!(out.text.starts_with("File: report.pdf (PDF, "), "{}", out.text);
        assert!(
            out.text.contains(", 2 pages)\n--- Page 1 of 2 ---\nQuarterly report\nSales grew by 12 percent.\n--- Page 2 of 2 ---\nChart: revenue by region"),
            "{}",
            out.text
        );
        let out = run(&ReadFile, d.path(), serde_json::json!({"path": "three.pdf", "pages": 2}));
        assert!(out.text.contains("Showing page 2 of 3.\n--- Page 2 of 3 ---\nSome text"), "{}", out.text);
        let out = run(&ReadFile, d.path(), serde_json::json!({"path": "three.pdf", "pages": "9"}));
        assert!(out.failed && out.text.contains("past the end"), "{}", out.text);
    }

    #[test]
    fn pdf_page_without_text_points_to_look_at_image() {
        let d = ws(&[("scan.pdf", fixtures::make_pdf(&[&[]], 40))]);
        let out = run(&ReadFile, d.path(), serde_json::json!({"path": "scan.pdf"}));
        assert!(
            out.text.contains("--- Page 1 of 1 ---\n(no text on this page: it is probably a scan or a picture; look_at_image with page=1 shows it)"),
            "{}",
            out.text
        );
    }

    #[test]
    fn read_file_reads_word_and_powerpoint() {
        let d = ws(&[("a.docx", fixtures::sample_docx()), ("deck.pptx", fixtures::sample_pptx())]);
        let out = run(&ReadFile, d.path(), serde_json::json!({"path": "a.docx"}));
        assert!(out.text.contains("(Word document, ") && out.text.contains("---\n# Annual report\n"), "{}", out.text);
        let out = run(&ReadFile, d.path(), serde_json::json!({"path": "deck.pptx", "pages": "2"}));
        assert!(out.text.contains("2 slides\nShowing slide 2 of 2.\n--- Slide 2 of 2 ---\n## Second\n\n- Costs fell"), "{}", out.text);
    }

    #[test]
    fn pointers_for_spreadsheets_images_and_old_formats() {
        let png = {
            let mut out = Vec::new();
            image::DynamicImage::ImageRgb8(image::RgbImage::new(2, 2)).write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
            out
        };
        let d = ws(&[("b.xlsx", b"PK\x03\x04".to_vec()), ("old.doc", vec![0xd0, 0xcf, 0x11, 0xe0, 0]), ("c.png", png), ("fake.docx", b"plain text".to_vec())]);
        let text = |p: &str| run(&ReadFile, d.path(), serde_json::json!({"path": p})).text;
        assert!(text("b.xlsx").contains("describe_sheet"));
        assert!(text("old.doc").contains("can't read"));
        // Images are shown, not pointed at.
        let img = run(&ReadFile, d.path(), serde_json::json!({"path": "c.png"}));
        assert!(img.text.contains("an image") && img.images.len() == 1, "{}", img.text);
        // Not really a .docx: read as the text it is.
        assert!(text("fake.docx").ends_with("---\nplain text"), "{}", text("fake.docx"));
    }

    #[test]
    fn page_markers_and_budget() {
        let pages: Vec<(String, Option<String>)> = (1..=5).map(|i| (format!("page {i} ") + &"x".repeat(5000), None)).collect();
        let out = render_pages(&pages, &[1, 2, 3, 4, 5], &[1, 2, 3, 4, 5], 9, "Page", "File: a.pdf", "a.pdf");
        assert!(out.starts_with("File: a.pdf\nShowing pages 1-2 of 9.\n--- Page 1 of 9 ---\npage 1"), "{out}");
        assert!(out.contains("--- Page 2 of 9 ---"));
        assert!(!out.contains("Page 3 of 9"));
        assert!(out.ends_with("--- more follows: call read_file with path=\"a.pdf\" and pages=\"3-5\""), "{out}");
        assert!(out.len() < DOC_BUDGET + 300);
    }

    #[test]
    fn one_huge_page_is_cut() {
        let pages = vec![("y".repeat(50_000), None)];
        let out = render_pages(&pages, &[3], &[3], 3, "Page", "File: a.pdf", "a.pdf");
        assert!(out.contains("Showing page 3 of 3. (page 3 is long and was cut.)"), "{}", &out[..200]);
        assert!(out.len() < DOC_BUDGET + 300);
    }

    #[test]
    fn whole_document_has_no_range_line() {
        let pages = vec![("a".into(), None), ("b".into(), Some("[note]".into()))];
        let out = render_pages(&pages, &[1, 2], &[1, 2], 2, "Slide", "File: d.pptx, 2 slides", "d.pptx");
        assert_eq!(out, "File: d.pptx, 2 slides\n--- Slide 1 of 2 ---\na\n--- Slide 2 of 2 ---\nb\n[note]");
    }

    #[test]
    fn word_lines() {
        let text: String = (1..=10).map(|i| format!("line {i}\n")).collect();
        let out = lines(&text, "File: a.docx (Word document, 1 KB)", "a.docx", &serde_json::json!({"offset": 4, "limit": 2}));
        assert_eq!(
            out,
            "File: a.docx (Word document, 1 KB), 10 lines as text\nShowing lines 4-5 of 10.\n---\nline 4\nline 5\n--- more follows: call read_file with path=\"a.docx\" and offset=6"
        );
    }
}
