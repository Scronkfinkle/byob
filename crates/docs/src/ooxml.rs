//! Word (`.docx`) and PowerPoint (`.pptx`) text as rough Markdown.
//!
//! Both are zip files of XML (Office Open XML). We read the parts that hold the text:
//! `word/document.xml`, and the slides of `ppt/presentation.xml` in their order. Headings
//! (`Heading1`… and `Title` styles, slide titles) become `#` lines, list paragraphs `- `
//! items, tables Markdown tables. Formatting, images and comments are dropped.

use std::io::{Cursor, Read};

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

use crate::{MAX_FILE_BYTES, guarded};

/// Most bytes read out of one part of the zip (a zip bomb stops here).
const MAX_PART_BYTES: u64 = 32 * 1024 * 1024;
/// Most characters of text kept from a document.
pub const MAX_TEXT_CHARS: usize = 2_000_000;
/// Most slides read.
const MAX_SLIDES: usize = 1000;

type Zip = zip::ZipArchive<Cursor<Vec<u8>>>;

fn open(bytes: Vec<u8>, what: &str) -> Result<Zip, String> {
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(format!("the {what} is too big (over {} MB)", MAX_FILE_BYTES / (1024 * 1024)));
    }
    zip::ZipArchive::new(Cursor::new(bytes)).map_err(|_| format!("the {what} is damaged (not a readable zip file)"))
}

/// A part of the zip as text; `None` if it isn't there.
fn part(zip: &mut Zip, name: &str) -> Result<Option<String>, String> {
    let Ok(file) = zip.by_name(name) else { return Ok(None) };
    let mut buf = Vec::new();
    file.take(MAX_PART_BYTES).read_to_end(&mut buf).map_err(|_| format!("can't unpack {name} from the file"))?;
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

/// The text of a Word document, as Markdown.
pub fn docx_text(bytes: Vec<u8>) -> Result<String, String> {
    guarded("the Word document", || {
        let mut zip = open(bytes, "Word document")?;
        let xml = part(&mut zip, "word/document.xml")?.ok_or("this .docx has no document text (word/document.xml is missing)")?;
        Ok(Body::markdown(&xml, BodyKind::Word))
    })
}

/// The text of each slide of a PowerPoint deck, in order, as Markdown.
pub fn pptx_slides(bytes: Vec<u8>) -> Result<Vec<String>, String> {
    guarded("the PowerPoint file", || {
        let mut zip = open(bytes, "PowerPoint file")?;
        let names = slide_order(&mut zip)?;
        let mut slides = Vec::new();
        for name in names.iter().take(MAX_SLIDES) {
            let xml = part(&mut zip, name)?.unwrap_or_default();
            slides.push(Body::markdown(&xml, BodyKind::Slide));
        }
        Ok(slides)
    })
}

/// Slide part names in presentation order (`presentation.xml`'s list, through its rels),
/// else every `ppt/slides/slideN.xml` by N.
fn slide_order(zip: &mut Zip) -> Result<Vec<String>, String> {
    let ordered = (|| -> Result<Vec<String>, String> {
        let pres = part(zip, "ppt/presentation.xml")?.unwrap_or_default();
        let rels = part(zip, "ppt/_rels/presentation.xml.rels")?.unwrap_or_default();
        let ids = attrs_of(&pres, "sldId", "r:id");
        let targets: Vec<(String, String)> = {
            let ids = attrs_of(&rels, "Relationship", "Id");
            let tgts = attrs_of(&rels, "Relationship", "Target");
            ids.into_iter().zip(tgts).collect()
        };
        Ok(ids
            .iter()
            .filter_map(|id| targets.iter().find(|(i, _)| i == id))
            .map(|(_, t)| {
                let t = t.trim_start_matches('/');
                if t.starts_with("ppt/") { t.to_owned() } else { format!("ppt/{t}") }
            })
            .collect())
    })()?;
    if !ordered.is_empty() {
        return Ok(ordered);
    }
    let mut numbered: Vec<(u32, String)> = zip
        .file_names()
        .filter_map(|n| {
            let n = n.ok()?;
            let num = n.strip_prefix("ppt/slides/slide")?.strip_suffix(".xml")?.parse().ok()?;
            Some((num, n.into_owned()))
        })
        .collect();
    numbered.sort();
    Ok(numbered.into_iter().map(|(_, n)| n).collect())
}

/// The value of attribute `attr` on every element named `elem` (prefix ignored).
fn attrs_of(xml: &str, elem: &str, attr: &str) -> Vec<String> {
    let mut r = Reader::from_str(xml);
    let mut out = Vec::new();
    loop {
        match r.read_event() {
            Ok(Event::Start(e) | Event::Empty(e)) if e.local_name().as_ref() == elem => {
                out.push(attr_value(&e, attr).unwrap_or_default());
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

fn attr_value(e: &BytesStart<'_>, key: &str) -> Option<String> {
    e.attributes().flatten().find(|a| a.key.as_ref() == key).map(|a| a.value.to_string())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BodyKind {
    Word,
    Slide,
}

/// Walks a document body or a slide and writes Markdown.
struct Body {
    kind: BodyKind,
    out: String,
    /// The paragraph being read.
    para: String,
    /// Its heading level (0: none) and list depth (`None`: not a list item).
    heading: usize,
    list: Option<usize>,
    /// Tables being read (nested tables flatten into their cell), each a list of rows.
    tables: Vec<Vec<Vec<String>>>,
    /// Inside a slide's title placeholder.
    title_shape: bool,
    /// Inside a text run (`w:t`, `a:t`): only that text is kept (not field codes, deletions).
    in_text: bool,
    /// Characters so far.
    chars: usize,
}

impl Body {
    fn markdown(xml: &str, kind: BodyKind) -> String {
        let mut b =
            Self { kind, out: String::new(), para: String::new(), heading: 0, list: None, tables: Vec::new(), title_shape: false, in_text: false, chars: 0 };
        let mut r = Reader::from_str(xml);
        loop {
            match r.read_event() {
                Ok(Event::Start(e)) => b.start(&e, false),
                Ok(Event::Empty(e)) => b.start(&e, true),
                Ok(Event::End(e)) => b.end(e.local_name().as_ref()),
                Ok(Event::Text(t)) if b.in_text => b.para.push_str(&t.xml10_content()),
                Ok(Event::GeneralRef(e)) if b.in_text => match e.resolve_char_ref() {
                    Ok(Some(c)) => b.para.push(c),
                    _ => b.para.push_str(quick_xml::escape::resolve_xml_entity(&e.xml10_content()).unwrap_or("")),
                },
                Ok(Event::Eof) | Err(_) => break,
                _ => {}
            }
            if b.chars > MAX_TEXT_CHARS {
                b.out.push_str("\n\n[… the rest of the document is cut]");
                break;
            }
        }
        b.out.trim().to_owned()
    }
}

impl Body {
    fn start(&mut self, e: &BytesStart<'_>, empty: bool) {
        match (self.kind, e.local_name().as_ref()) {
            (_, "t") => self.in_text = !empty,
            (BodyKind::Word, "pStyle") => {
                if let Some(v) = attr_value(e, "w:val") {
                    self.heading = heading_level(&v);
                    if self.list.is_none() && v.to_ascii_lowercase().starts_with("list") {
                        self.list = Some(0);
                    }
                }
            }
            (BodyKind::Word, "ilvl") => {
                let lvl = attr_value(e, "w:val").and_then(|v| v.parse().ok()).unwrap_or(0usize);
                self.list = Some(lvl.min(8));
            }
            (BodyKind::Word, "numPr") => {
                self.list.get_or_insert(0);
            }
            (BodyKind::Word, "tab") if self.tables.is_empty() || empty => self.para.push('\t'),
            (BodyKind::Word, "br" | "cr") => self.para.push('\n'),
            (BodyKind::Slide, "br") => self.para.push('\n'),
            (BodyKind::Slide, "ph") => {
                if matches!(attr_value(e, "type").as_deref(), Some("title" | "ctrTitle")) {
                    self.title_shape = true;
                }
            }
            (BodyKind::Slide, "pPr") => {
                if let Some(l) = attr_value(e, "lvl").and_then(|v| v.parse::<usize>().ok()) {
                    self.list = Some(l.min(8));
                }
            }
            (BodyKind::Slide, "buChar" | "buAutoNum") => {
                self.list.get_or_insert(0);
            }
            (_, "tbl") if !empty => self.tables.push(Vec::new()),
            (_, "tr") if !empty => {
                if let Some(t) = self.tables.last_mut() {
                    t.push(Vec::new());
                }
            }
            (_, "tc") if !empty => {
                if let Some(row) = self.tables.last_mut().and_then(|t| t.last_mut()) {
                    row.push(String::new());
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, name: &str) {
        match name {
            "t" => self.in_text = false,
            "p" => self.end_paragraph(),
            "sp" if self.kind == BodyKind::Slide => self.title_shape = false,
            "tbl" => {
                if let Some(rows) = self.tables.pop() {
                    let md = table_markdown(&rows);
                    match self.tables.last_mut().and_then(|t| t.last_mut()).and_then(|r| r.last_mut()) {
                        // A table inside a cell: its text joins the cell.
                        Some(cell) => {
                            push_cell_text(cell, &rows.iter().flatten().cloned().collect::<Vec<_>>().join(" / "));
                        }
                        None => self.block(&md),
                    }
                }
            }
            _ => {}
        }
    }

    fn end_paragraph(&mut self) {
        let text = std::mem::take(&mut self.para);
        let (heading, list) = (std::mem::take(&mut self.heading), self.list.take());
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        if let Some(cell) = self.tables.last_mut().and_then(|t| t.last_mut()).and_then(|r| r.last_mut()) {
            push_cell_text(cell, text);
            return;
        }
        let line = if self.title_shape {
            format!("## {}", one_line(text))
        } else if heading > 0 {
            format!("{} {}", "#".repeat(heading), one_line(text))
        } else if let Some(depth) = list {
            format!("{}- {}", "  ".repeat(depth), text.replace('\n', " "))
        } else {
            text.to_owned()
        };
        let tight = list.is_some() && self.out.lines().last().is_some_and(|l| l.trim_start().starts_with("- "));
        if tight {
            self.out.push('\n');
            self.out.push_str(&line);
            self.chars = self.chars.saturating_add(line.len());
        } else {
            self.block(&line);
        }
    }

    fn block(&mut self, text: &str) {
        if !self.out.is_empty() {
            self.out.push_str("\n\n");
        }
        self.out.push_str(text);
        self.chars = self.chars.saturating_add(text.len());
    }
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn push_cell_text(cell: &mut String, text: &str) {
    if !cell.is_empty() {
        cell.push_str("<br>");
    }
    cell.push_str(&one_line(text).replace('|', "\\|"));
}

/// `Heading1`/`heading 2`/`Title` → 1/2/1; anything else 0.
fn heading_level(style: &str) -> usize {
    let s = style.to_ascii_lowercase().replace([' ', '-', '_'], "");
    if s == "title" {
        return 1;
    }
    if s == "subtitle" {
        return 2;
    }
    s.strip_prefix("heading").and_then(|n| n.parse::<usize>().ok()).filter(|n| (1..=6).contains(n)).unwrap_or(0)
}

fn table_markdown(rows: &[Vec<String>]) -> String {
    let rows: Vec<&Vec<String>> = rows.iter().filter(|r| r.iter().any(|c| !c.trim().is_empty())).collect();
    let width = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    for (i, row) in rows.iter().enumerate() {
        let cells: Vec<&str> = (0..width).map(|c| row.get(c).map_or("", String::as_str)).collect();
        out.push_str(&format!("| {} |\n", cells.join(" | ")));
        if i == 0 {
            out.push_str(&format!("|{}\n", " --- |".repeat(width)));
        }
    }
    out.trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{make_zip, sample_docx, sample_pptx};

    #[test]
    fn docx_to_markdown() {
        let md = docx_text(sample_docx()).unwrap();
        assert_eq!(
            md,
            "# Annual report\n\nRevenue rose 12 & more%.\n\n- First point\n  - Sub point\n\n| Region | Sales |\n| --- | --- |\n| North | 40 |\n\n## Outlook\n\nLine one\nline two"
        );
    }

    #[test]
    fn pptx_slides_in_presentation_order() {
        let slides = pptx_slides(sample_pptx()).unwrap();
        assert_eq!(slides, ["## Welcome\n\n- Agenda\n- Results", "## Second\n\n- Costs fell"]);
    }

    #[test]
    fn damaged_files() {
        assert!(docx_text(b"PK\x03\x04junk".to_vec()).unwrap_err().contains("damaged"));
        assert!(docx_text(make_zip(&[("x.xml", "<a/>")])).unwrap_err().contains("missing"));
        assert!(pptx_slides(make_zip(&[("x.xml", "<a/>")])).unwrap().is_empty());
        // Broken XML ends the text where it breaks.
        let z = make_zip(&[("word/document.xml", "<w:document><w:body><w:p><w:r><w:t>ok</w:t></w:r></w:p><w:p><<<")]);
        assert_eq!(docx_text(z).unwrap(), "ok");
    }

    #[test]
    fn heading_styles() {
        assert_eq!(heading_level("Heading1"), 1);
        assert_eq!(heading_level("heading 3"), 3);
        assert_eq!(heading_level("Title"), 1);
        assert_eq!(heading_level("Heading9"), 0);
        assert_eq!(heading_level("Normal"), 0);
    }
}
