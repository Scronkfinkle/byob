//! Text out of documents, and PDF pages as pictures, in pure Rust.
//!
//! - **PDF** ([`pdf`]): text per page (hayro's interpreter, a text-collecting device), page
//!   count, and a page rendered to PNG (hayro's rasteriser) for a vision model.
//! - **Word and PowerPoint** ([`ooxml`]): `.docx` and `.pptx` are zip files of XML; their text
//!   comes out as rough Markdown (headings, lists, tables), slides one by one.
//!
//! Every input is hostile: sizes are capped (zip entries are read with a limit, so a zip bomb
//! stops early), parser panics are caught, and errors are short sentences a model can act on.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

#[doc(hidden)]
pub mod fixtures;
pub mod ooxml;
pub mod pdf;

use std::panic::{AssertUnwindSafe, catch_unwind};

/// Largest document file read (bytes).
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// What a file is, by its name and first bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Pdf,
    Docx,
    Pptx,
    /// .xlsx, .xls, .ods: the spreadsheet tools read these.
    Spreadsheet,
    /// .doc, .ppt, .odt, .odp, .rtf: known but not read here.
    OtherDocument,
}

impl Kind {
    /// The kind of `name` (its extension), checked against the first bytes where they tell.
    pub fn of(name: &str, head: &[u8]) -> Option<Self> {
        let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
        let zip = head.starts_with(b"PK\x03\x04");
        match ext.as_str() {
            "pdf" if find(head, b"%PDF-").is_some() => Some(Self::Pdf),
            "docx" | "docm" if zip => Some(Self::Docx),
            "pptx" | "pptm" if zip => Some(Self::Pptx),
            "xlsx" | "xlsm" | "xls" | "ods" => Some(Self::Spreadsheet),
            "doc" | "ppt" | "odt" | "odp" | "rtf" => Some(Self::OtherDocument),
            _ if head.starts_with(b"%PDF-") => Some(Self::Pdf),
            _ => None,
        }
    }
}

/// Where `needle` first occurs in `hay`.
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Run parser code that might panic on a malformed file.
fn guarded<T>(what: &str, f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| Err(format!("{what} is damaged or uses something byob can't read")))
}

/// Parse a page list like `"3"`, `"1-3,5"`, `"4-"` or `"all"` against `total` pages.
/// Pages count from 1; the result is sorted, without duplicates, at most `max` long.
pub fn parse_pages(spec: &str, total: usize, max: usize) -> Result<Vec<usize>, String> {
    let bad = || format!("pages must look like \"3\", \"1-3,5\" or \"4-\" (the document has {total})");
    let spec = spec.trim();
    if spec.is_empty() || spec.eq_ignore_ascii_case("all") {
        return Ok((1..=total.min(max)).collect());
    }
    let mut out = Vec::new();
    for part in spec.split([',', ' ', ';']).map(str::trim).filter(|p| !p.is_empty()) {
        // en and em dashes too: models copy them from "pages 1–3 of 9".
        let part = part.replace(['–', '—'], "-");
        let (a, b) = match part.split_once('-') {
            Some((a, b)) => {
                let a = if a.trim().is_empty() { 1 } else { a.trim().parse::<usize>().map_err(|_| bad())? };
                let b = if b.trim().is_empty() { total } else { b.trim().parse::<usize>().map_err(|_| bad())? };
                (a, b)
            }
            None => {
                let n = part.parse::<usize>().map_err(|_| bad())?;
                (n, n)
            }
        };
        if a == 0 || a > b {
            return Err(bad());
        }
        if a > total {
            return Err(format!("page {a} is past the end: the document has {total}"));
        }
        for n in a..=b.min(total) {
            out.push(n);
            if out.len() > max.saturating_mul(4) {
                break;
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out.truncate(max);
    if out.is_empty() {
        return Err(bad());
    }
    Ok(out)
}

/// Pages as compact ranges: `[1,2,3,5]` → `"1-3,5"`.
pub fn page_ranges(pages: &[usize]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while let Some(&start) = pages.get(i) {
        let mut end = start;
        while pages.get(i.saturating_add(1)) == Some(&end.saturating_add(1)) {
            end = end.saturating_add(1);
            i = i.saturating_add(1);
        }
        out.push(if start == end { start.to_string() } else { format!("{start}-{end}") });
        i = i.saturating_add(1);
    }
    out.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        assert_eq!(Kind::of("a/report.PDF", b"%PDF-1.7\n"), Some(Kind::Pdf));
        assert_eq!(Kind::of("noext", b"%PDF-1.4"), Some(Kind::Pdf));
        assert_eq!(Kind::of("fake.pdf", b"hello"), None);
        assert_eq!(Kind::of("x.docx", b"PK\x03\x04rest"), Some(Kind::Docx));
        assert_eq!(Kind::of("x.docx", b"not a zip"), None);
        assert_eq!(Kind::of("deck.pptx", b"PK\x03\x04"), Some(Kind::Pptx));
        assert_eq!(Kind::of("b.xlsx", b"PK\x03\x04"), Some(Kind::Spreadsheet));
        assert_eq!(Kind::of("old.doc", b"\xd0\xcf"), Some(Kind::OtherDocument));
        assert_eq!(Kind::of("notes.txt", b"hi"), None);
    }

    #[test]
    fn page_lists() {
        assert_eq!(parse_pages("", 3, 50).unwrap(), [1, 2, 3]);
        assert_eq!(parse_pages("all", 100, 5).unwrap(), [1, 2, 3, 4, 5]);
        assert_eq!(parse_pages("2", 3, 50).unwrap(), [2]);
        assert_eq!(parse_pages("5,1-3, 2", 9, 50).unwrap(), [1, 2, 3, 5]);
        assert_eq!(parse_pages("7-", 9, 50).unwrap(), [7, 8, 9]);
        assert_eq!(parse_pages("2–4", 9, 50).unwrap(), [2, 3, 4]);
        assert_eq!(parse_pages("8-20", 9, 50).unwrap(), [8, 9]);
        assert_eq!(parse_pages("1-1000000000000", 3, 50).unwrap(), [1, 2, 3]);
        for bad in ["0", "x", "3-1", "10", "1-2-3"] {
            assert!(parse_pages(bad, 9, 50).is_err(), "{bad}");
        }
    }

    #[test]
    fn ranges() {
        assert_eq!(page_ranges(&[1, 2, 3, 5, 7, 8]), "1-3,5,7-8");
        assert_eq!(page_ranges(&[4]), "4");
        assert_eq!(page_ranges(&[]), "");
    }
}
