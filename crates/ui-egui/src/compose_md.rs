//! Live Markdown styling for the composer.
//!
//! The composer stays a plain `TextEdit` over the raw Markdown (that is what gets sent); this
//! module only decides how each byte of it looks. [`style`] turns the text into a
//! [`LayoutJob`] whose sections cover the text exactly, in order, with nothing inserted or
//! removed, so the galley's characters are the text's characters and the cursor, selection,
//! copy/paste and IME work as in an unstyled field. Markers (`**`, `#`, backticks, `>`, link
//! targets) stay visible in a faint colour; what they mark gets the style.
//!
//! Parsing is `pulldown-cmark` with source offsets (the same CommonMark the transcript renders
//! with, so `snake_case` and `2*3*4` are not emphasis): every event's byte range sets flags on
//! the bytes it covers, and bytes no text event covers are syntax. Bare `http(s)://` URLs are
//! found by a scan. Fenced code gets the transcript's syntax colours and a background painted
//! behind its whole lines ([`paint_code_backgrounds`]).
//!
//! egui calls the layouter every frame, so [`cached`] keeps the last job per field, keyed by
//! the text's hash, the fonts and the theme. Past [`MAX_STYLED`] bytes the text is laid out
//! plain.

use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::sync::Arc;

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontFamily, FontId, Stroke};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag};

use crate::theme::Tokens;

/// Longer text is laid out plain (pasted logs and files needn't cost a parse per keystroke).
pub const MAX_STYLED: usize = 100_000;

const TEXT: u16 = 1;
const STRONG: u16 = 1 << 1;
const EM: u16 = 1 << 2;
const STRIKE: u16 = 1 << 3;
const CODE: u16 = 1 << 4;
const BLOCK: u16 = 1 << 5;
const LINK: u16 = 1 << 6;
const QUOTE: u16 = 1 << 7;
const LIST: u16 = 1 << 8;
/// Shown as written: HTML-ish tags (`<context>`) are common in prompts.
const VERBATIM: u16 = 1 << 9;

/// How one byte looks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Cell {
    flags: u16,
    /// Heading level 1–6, 0 outside headings.
    heading: u8,
    /// A syntax colour inside a fenced block (index into [`syntax_colors`], 0 = none).
    syntax: u8,
}

/// The fonts the composer text uses.
#[derive(Debug, Clone, PartialEq)]
pub struct Fonts {
    pub body: FontId,
    pub bold: FontFamily,
    pub mono: FontId,
}

impl Fonts {
    pub fn new(ui: &egui::Ui, t: &Tokens) -> Self {
        let body = egui::TextStyle::Body.resolve(ui.style());
        // Inline code follows the code size knob relative to the UI text size, as in the transcript.
        let mono = FontId::new(body.size * 0.92 * (t.knobs.code_scale / t.knobs.font_scale.max(0.1)), FontFamily::Monospace);
        Self { body, bold: crate::theme::bold_or_regular(ui.ctx()), mono }
    }
}

/// The styled text: the job (no wrap width set) and the char ranges of fenced/indented code
/// blocks, whose lines get a background.
#[derive(Debug, Clone, Default)]
pub struct Styled {
    pub job: Arc<LayoutJob>,
    pub code_blocks: Arc<Vec<Range<usize>>>,
}

fn syntax_colors(t: &Tokens) -> [Color32; 7] {
    [t.code_text, t.syntax_keyword, t.syntax_string, t.syntax_number, t.syntax_comment, t.syntax_function, t.syntax_type]
}

/// Style `text` as Markdown. The job's text is exactly `text`.
pub fn style(t: &Tokens, fonts: &Fonts, text: &str) -> Styled {
    if text.len() > MAX_STYLED {
        let mut job = LayoutJob::default();
        job.append(text, 0.0, TextFormat::simple(fonts.body.clone(), t.text));
        return Styled { job: Arc::new(job), code_blocks: Arc::default() };
    }
    let (cells, blocks) = cells(t, fonts, text);
    let mut job = LayoutJob::default();
    let mut start = 0usize;
    let bytes = text.len();
    for i in 1..=bytes {
        let cut = i == bytes || (text.is_char_boundary(i) && cells.get(i) != cells.get(start));
        if !cut {
            continue;
        }
        if let (Some(s), Some(cell)) = (text.get(start..i), cells.get(start)) {
            job.append(s, 0.0, format(t, fonts, *cell));
        }
        start = i;
    }
    if job.sections.is_empty() {
        // Empty text: one empty section in the body font keeps the empty field one body line tall.
        job.append("", 0.0, TextFormat::simple(fonts.body.clone(), t.text));
    }
    let code_blocks = char_ranges(text, &blocks);
    Styled { job: Arc::new(job), code_blocks: Arc::new(code_blocks) }
}

fn format(t: &Tokens, fonts: &Fonts, cell: Cell) -> TextFormat {
    let f = cell.flags;
    let text = f & (TEXT | VERBATIM) != 0;
    let mut fmt = TextFormat::simple(fonts.body.clone(), t.text);
    if cell.heading > 0 {
        let scale = match cell.heading {
            1 => 1.25,
            2 => 1.15,
            3 => 1.07,
            _ => 1.0,
        };
        fmt.font_id = FontId::new(fonts.body.size * scale, fonts.bold.clone());
        fmt.color = t.text_strong;
    }
    if f & QUOTE != 0 {
        fmt.color = t.text_dim;
    }
    if f & STRONG != 0 {
        fmt.font_id.family = fonts.bold.clone();
        fmt.color = t.text_strong;
    }
    fmt.italics = f & EM != 0;
    if f & LINK != 0 && text {
        fmt.color = t.link;
        fmt.underline = Stroke::new(1.0, t.link.gamma_multiply(0.6));
    }
    if f & STRIKE != 0 && text {
        fmt.strikethrough = Stroke::new(1.0, fmt.color);
    }
    if f & CODE != 0 {
        fmt.font_id = fonts.mono.clone();
        fmt.color = t.inline_code_text;
        fmt.background = t.inline_code_bg;
        fmt.italics = false;
    }
    if f & BLOCK != 0 {
        fmt.font_id = fonts.mono.clone();
        fmt.color = syntax_colors(t).get(usize::from(cell.syntax)).copied().unwrap_or(t.code_text);
        fmt.italics = false;
    }
    if f & LIST != 0 {
        fmt.color = t.accent;
        fmt.font_id.family = fonts.bold.clone();
    } else if !text {
        // Syntax: the markers, link targets, fences.
        fmt.color = t.text_faint;
        fmt.underline = Stroke::NONE;
    }
    fmt
}

/// Per-byte styles, and the byte ranges of code blocks.
fn cells(t: &Tokens, fonts: &Fonts, text: &str) -> (Vec<Cell>, Vec<Range<usize>>) {
    let n = text.len();
    let mut cells = vec![Cell::default(); n];
    let mut blocks = Vec::new();
    let set = |cells: &mut [Cell], r: &Range<usize>, flag: u16| {
        for c in cells.get_mut(r.clone()).into_iter().flatten() {
            c.flags |= flag;
        }
    };
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    // The fenced block being read: its language and the source ranges of its text.
    let mut fence: Option<(String, Vec<Range<usize>>)> = None;
    for (ev, r) in Parser::new_ext(text, options).into_offset_iter() {
        let r = r.start.min(n)..r.end.min(n);
        match ev {
            Event::Start(tag) => match tag {
                Tag::Strong | Tag::Emphasis if arithmetic(text, &r) => {
                    // `2*3*4`: CommonMark allows `*` emphasis inside words, but between digits it is maths.
                    let s = text.get(r.clone()).unwrap_or_default();
                    let open = s.len() - s.trim_start_matches('*').len();
                    let close = s.len() - s.trim_end_matches('*').len();
                    set(&mut cells, &(r.start..r.start + open), TEXT);
                    set(&mut cells, &(r.end - close..r.end), TEXT);
                }
                Tag::Strong => set(&mut cells, &r, STRONG),
                Tag::Emphasis => set(&mut cells, &r, EM),
                Tag::Strikethrough => set(&mut cells, &r, STRIKE),
                Tag::Link { .. } | Tag::Image { .. } => set(&mut cells, &r, LINK),
                Tag::BlockQuote(_) => set(&mut cells, &r, QUOTE),
                Tag::Heading { level, .. } => {
                    #[allow(clippy::cast_possible_truncation)] // 1..=6
                    let level = level as u8;
                    for c in cells.get_mut(r.clone()).into_iter().flatten() {
                        c.heading = level;
                    }
                }
                Tag::CodeBlock(kind) => {
                    set(&mut cells, &r, BLOCK);
                    blocks.push(r.clone());
                    let lang = match kind {
                        CodeBlockKind::Fenced(info) => info.split(|c: char| c.is_whitespace() || c == ',' || c == '{').next().unwrap_or_default().to_owned(),
                        CodeBlockKind::Indented => String::new(),
                    };
                    fence = Some((lang, Vec::new()));
                }
                Tag::Item => {
                    // The marker: `-`, `*`, `+`, `1.` or `1)` at the start of the item.
                    let marker = text.get(r.clone()).map_or(0, list_marker_len);
                    set(&mut cells, &(r.start..r.start.saturating_add(marker).min(r.end)), LIST);
                }
                _ => {}
            },
            Event::End(pulldown_cmark::TagEnd::CodeBlock) => {
                if let Some((lang, pieces)) = fence.take() {
                    highlight(t, fonts, text, &lang, &pieces, &mut cells);
                }
            }
            Event::Text(_) => {
                set(&mut cells, &r, TEXT);
                if let Some((_, pieces)) = fence.as_mut() {
                    pieces.push(r);
                }
            }
            Event::Code(_) => {
                set(&mut cells, &r, CODE);
                // The content between the backtick runs is text.
                let s = text.get(r.clone()).unwrap_or_default();
                let open = s.len() - s.trim_start_matches('`').len();
                let close = s.len() - s.trim_end_matches('`').len();
                if open + close < s.len() {
                    set(&mut cells, &(r.start + open..r.end - close), TEXT);
                }
            }
            Event::Html(_) | Event::InlineHtml(_) | Event::InlineMath(_) | Event::DisplayMath(_) | Event::FootnoteReference(_) => {
                set(&mut cells, &r, VERBATIM);
            }
            Event::SoftBreak | Event::HardBreak => set(&mut cells, &r, TEXT),
            Event::TaskListMarker(_) => set(&mut cells, &r, LIST),
            Event::End(_) | Event::Rule => {}
        }
    }
    bare_urls(text, &mut cells);
    (cells, blocks)
}

/// Whether the `*` emphasis at `r` sits inside a word and next to digits on both ends
/// (`2*3*4`, `a*2*b` is still emphasis).
fn arithmetic(text: &str, r: &Range<usize>) -> bool {
    let Some(s) = text.get(r.clone()) else { return false };
    if !s.starts_with('*') {
        return false;
    }
    let inner = s.trim_matches('*');
    let before = text.get(..r.start).and_then(|b| b.chars().next_back());
    let after = text.get(r.end..).and_then(|a| a.chars().next());
    let digit = |c: Option<char>| c.is_some_and(|c| c.is_ascii_digit());
    let alnum = |c: Option<char>| c.is_some_and(char::is_alphanumeric);
    alnum(before) && alnum(after) && (digit(before) || digit(after)) && (digit(inner.chars().next()) || digit(inner.chars().next_back()))
}

/// Bytes of a list item's marker at the start of `item` (0 if there is none).
fn list_marker_len(item: &str) -> usize {
    let b = item.as_bytes();
    match b.first() {
        Some(b'-' | b'*' | b'+') => 1,
        Some(c) if c.is_ascii_digit() => {
            let digits = b.iter().take_while(|c| c.is_ascii_digit()).count();
            match b.get(digits) {
                Some(b'.' | b')') => digits + 1,
                _ => 0,
            }
        }
        _ => 0,
    }
}

/// Syntax colours for a code block's text (`pieces` are its source ranges, in order).
fn highlight(t: &Tokens, fonts: &Fonts, text: &str, lang: &str, pieces: &[Range<usize>], cells: &mut [Cell]) {
    if lang.is_empty() || pieces.is_empty() {
        return;
    }
    let code: String = pieces.iter().filter_map(|r| text.get(r.clone())).collect();
    let job = crate::highlight::job(t, lang, &code, &fonts.mono);
    let colors = syntax_colors(t);
    // Walk the job's sections and the pieces together: offset `at` in `code` is offset `off`
    // into piece `p`.
    let mut p = 0usize;
    let mut piece_start = 0usize;
    for section in &job.sections {
        #[allow(clippy::cast_possible_truncation)] // 7 colours
        let idx = colors.iter().position(|c| *c == section.format.color).unwrap_or(0) as u8;
        if idx == 0 {
            continue;
        }
        for at in section.byte_range.start.0..section.byte_range.end.0 {
            while let Some(r) = pieces.get(p) {
                if at < piece_start + r.len() {
                    break;
                }
                piece_start += r.len();
                p += 1;
            }
            let Some(r) = pieces.get(p) else { return };
            if let Some(c) = cells.get_mut(r.start + (at - piece_start)) {
                c.syntax = idx;
            }
        }
    }
}

/// Mark bare `http://` and `https://` URLs in plain text as links.
fn bare_urls(text: &str, cells: &mut [Cell]) {
    let bytes = text.as_bytes();
    let mut from = 0usize;
    while let Some(found) = text.get(from..).and_then(|s| s.find("http")) {
        let start = from + found;
        from = start + 4;
        let rest = text.get(start..).unwrap_or_default();
        let scheme = if rest.starts_with("https://") {
            8
        } else if rest.starts_with("http://") {
            7
        } else {
            continue;
        };
        // Not inside a word (`xhttp://`), and in plain text.
        if start > 0 && bytes.get(start - 1).is_some_and(u8::is_ascii_alphanumeric) {
            continue;
        }
        let mut end = start + scheme;
        while let Some(&b) = bytes.get(end) {
            if b.is_ascii_whitespace() || matches!(b, b'<' | b'>' | b'"' | b'`') || b >= 0x80 {
                break;
            }
            end += 1;
        }
        while end > start + scheme
            && bytes.get(end - 1).is_some_and(|b| matches!(b, b'.' | b',' | b':' | b';' | b'!' | b'?' | b')' | b'\'' | b'*' | b'_' | b'~'))
        {
            end -= 1;
        }
        from = end.max(from);
        if end == start + scheme {
            continue;
        }
        let Some(span) = cells.get_mut(start..end) else { continue };
        if span.iter().any(|c| c.flags & (CODE | BLOCK | LINK | VERBATIM) != 0 || c.flags & TEXT == 0) {
            continue;
        }
        for c in span {
            c.flags |= LINK;
        }
    }
}

/// Byte ranges → char ranges (both sorted by start).
fn char_ranges(text: &str, ranges: &[Range<usize>]) -> Vec<Range<usize>> {
    if ranges.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(ranges.len());
    for r in ranges {
        let start = text.get(..r.start).map_or(0, |s| s.chars().count());
        let len = text.get(r.clone()).map_or(0, |s| s.chars().count());
        out.push(start..start + len);
    }
    out
}

/// Wrap the chars `sel` of `text` in `marker` (`**` or `*`), or unwrap them if they already
/// are. Surrounding spaces stay outside. Returns the new text and the new selection (chars),
/// still around the same words; an empty selection leaves the cursor between the markers.
pub fn toggle(text: &str, sel: Range<usize>, marker: &str) -> (String, Range<usize>) {
    let byte = |c: usize| text.char_indices().nth(c).map_or(text.len(), |(b, _)| b);
    let (mut a, mut b) = (byte(sel.start.min(sel.end)), byte(sel.end.max(sel.start)));
    let inner = text.get(a..b).unwrap_or_default();
    a += inner.len() - inner.trim_start().len();
    b -= inner.len() - inner.trim_end().len();
    b = b.max(a);
    let chars = |s: &str| s.chars().count();
    let m = marker.len();
    let (before, rest) = text.split_at(a.min(text.len()));
    let (inner, after) = rest.split_at((b - a).min(rest.len()));
    let star = marker.starts_with('*');
    let run = |s: &str, from_end: bool| {
        let c = if star { '*' } else { '_' };
        if from_end { s.len() - s.trim_end_matches(c).len() } else { s.len() - s.trim_start_matches(c).len() }
    };
    // Already wrapped: `*` needs an odd run (`*x*`, `***x***`), `**` a run of two or more.
    let wrapped = |l: usize, r: usize| if m == 1 { l % 2 == 1 && r % 2 == 1 } else { l >= 2 && r >= 2 };
    let start = chars(before);
    if !inner.is_empty() && wrapped(run(before, true), run(after, false)) {
        let out = format!("{}{inner}{}", before.get(..before.len() - m).unwrap_or_default(), after.get(m..).unwrap_or_default());
        let s = start - m;
        return (out, s..s + chars(inner));
    }
    if inner.len() > 2 * m && wrapped(run(inner, false), run(inner, true)) {
        let body = inner.get(m..inner.len() - m).unwrap_or_default();
        return (format!("{before}{body}{after}"), start..start + chars(body));
    }
    let out = format!("{before}{marker}{inner}{marker}{after}");
    let s = start + chars(marker);
    (out, s..s + chars(inner))
}

/// The last styled text of each composer, keyed by what it depends on.
#[derive(Clone, Default)]
struct Cache {
    key: Option<(u64, usize, Tokens)>,
    styled: Styled,
}

/// [`style`], cached per `id` (the text's hash, the fonts and the theme).
pub fn cached(ui: &egui::Ui, t: &Tokens, id: egui::Id, text: &str) -> Styled {
    let fonts = Fonts::new(ui, t);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    fonts.body.size.to_bits().hash(&mut h);
    fonts.mono.size.to_bits().hash(&mut h);
    fonts.bold.hash(&mut h);
    let key = (h.finish(), text.len(), *t);
    let id = id.with("compose-md");
    if let Some(hit) = ui.data(|d| d.get_temp::<Cache>(id)).filter(|c| c.key.as_ref() == Some(&key)) {
        return hit.styled;
    }
    let styled = style(t, &fonts, text);
    ui.data_mut(|d| d.insert_temp(id, Cache { key: Some(key), styled: styled.clone() }));
    styled
}

/// The code-block backgrounds behind the galley's code lines, from `left` to `right`: the inline
/// code colour (the transcript's code background is too close to the field's).
pub fn code_backgrounds(
    t: &Tokens,
    galley: &egui::Galley,
    origin: egui::Pos2,
    left: f32,
    right: f32,
    blocks: &[Range<usize>],
    total: usize,
) -> Vec<egui::Shape> {
    let mut shapes = Vec::new();
    if blocks.is_empty() {
        return shapes;
    }
    let mut char_at = 0usize;
    let mut current: Option<egui::Rect> = None;
    let flush = |current: &mut Option<egui::Rect>, shapes: &mut Vec<egui::Shape>| {
        if let Some(r) = current.take() {
            shapes.push(egui::Shape::rect_filled(r, 4.0, t.inline_code_bg));
        }
    };
    for row in &galley.rows {
        let start = char_at;
        char_at += row.char_count_including_newline().0;
        // The empty last row of an unterminated block still belongs to it (where the cursor goes).
        let inside = blocks.iter().any(|b| b.contains(&start) || (start == total && b.end == total && b.start < total && !row.ends_with_newline));
        if inside {
            let r = row.rect().translate(origin.to_vec2());
            let r = egui::Rect::from_min_max(egui::pos2(left, r.top()), egui::pos2(right, r.bottom()));
            current = Some(current.map_or(r, |c| c.union(r)));
        } else {
            flush(&mut current, &mut shapes);
        }
    }
    flush(&mut current, &mut shapes);
    shapes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fonts() -> Fonts {
        Fonts { body: FontId::proportional(14.0), bold: FontFamily::Name("bold".into()), mono: FontId::monospace(13.0) }
    }

    /// Fonts the test context has (a named bold family would panic in layout).
    fn real_fonts() -> Fonts {
        Fonts { body: FontId::proportional(14.0), bold: FontFamily::Proportional, mono: FontId::monospace(13.0) }
    }

    fn styled(text: &str) -> LayoutJob {
        (*style(&Tokens::dark(), &fonts(), text).job).clone()
    }

    /// The format of the first char of `needle` in `text`.
    fn at(job: &LayoutJob, needle: &str) -> TextFormat {
        let Some(i) = job.text.find(needle) else { panic!("{needle:?} not in {:?}", job.text) };
        let Some(s) = job.sections.iter().find(|s| (s.byte_range.start.0..s.byte_range.end.0).contains(&i)) else { panic!("no section at {i}") };
        s.format.clone()
    }

    fn covers_exactly(text: &str) {
        let job = styled(text);
        assert_eq!(job.text, text);
        let mut end = 0;
        for s in &job.sections {
            assert_eq!(s.byte_range.start.0, end, "{text:?}: {:?}", job.sections);
            assert!(text.is_char_boundary(s.byte_range.end.0), "{text:?}");
            end = s.byte_range.end.0;
        }
        assert_eq!(end, text.len(), "{text:?}");
    }

    #[test]
    fn the_sections_cover_the_text_exactly() {
        let pieces = [
            "",
            "a",
            "**",
            "*",
            "_",
            "__",
            "~~",
            "`",
            "```",
            "~~~",
            "#",
            "# ",
            "> ",
            "- ",
            "1. ",
            "2) ",
            "[",
            "](",
            ")",
            "[ ] ",
            "[x] ",
            "\n",
            "\r\n",
            "  \n",
            "\\",
            "<b>",
            "</b>",
            "&amp;",
            "https://e.com/a_b",
            "snake_case_name",
            "2*3*4",
            "é",
            "日本",
            "👍🏽",
            "👨‍👩‍👧",
            "\t",
            "    ",
            "|",
            "---",
            "***",
            "rust\n",
            "fn main() {}",
            "\"str",
            "// c",
            "=\n",
        ];
        // Every pair and a pseudo-random spread of longer strings.
        for a in pieces {
            for b in pieces {
                covers_exactly(&format!("{a}{b}"));
                covers_exactly(&format!("{a}x{b}y{a}"));
            }
        }
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..2000 {
            let mut s = String::new();
            for _ in 0..(seed % 12) {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                #[allow(clippy::cast_possible_truncation)]
                s.push_str(pieces.get((seed % pieces.len() as u64) as usize).copied().unwrap_or(""));
            }
            seed = seed.wrapping_add(0x9e37_79b9);
            covers_exactly(&s);
        }
    }

    #[test]
    fn the_galley_has_one_char_per_char_of_the_text() {
        let ctx = egui::Context::default();
        ctx.run_ui(egui::RawInput::default(), |_| {}).textures_delta.clear();
        for text in ["", "**bold** and *it*", "# Head\r\n- [ ] task 👍🏽\n```rust\nfn x() {}\n```", "> quote `code` [l](http://x)\n\n日本語 _e_"] {
            let job = (*style(&Tokens::dark(), &real_fonts(), text).job).clone();
            let galley = ctx.fonts_mut(|f| f.layout_job(job));
            assert_eq!(galley.text(), text);
            let rows: usize = galley.rows.iter().map(|r| r.char_count_including_newline().0).sum();
            assert_eq!(rows, text.chars().count(), "{text:?}");
        }
    }

    #[test]
    fn each_construct_gets_its_style() {
        let t = Tokens::dark();
        let f = fonts();
        let job = styled("Some **bold**, __also__, *it*, _em_, ***both*** and ~~gone~~ text");
        assert_eq!(at(&job, "bold").font_id.family, f.bold);
        assert_eq!(at(&job, "bold").color, t.text_strong);
        assert_eq!(at(&job, "**bold").color, t.text_faint, "markers are faint");
        assert_eq!(at(&job, "also").font_id.family, f.bold);
        assert!(at(&job, "it*").italics && at(&job, "em_").italics);
        assert!(at(&job, "both").italics && at(&job, "both").font_id.family == f.bold);
        assert!(at(&job, "gone").strikethrough.width > 0.0);
        assert!(!at(&job, "Some").italics && at(&job, "Some").color == t.text);
    }

    #[test]
    fn headings_and_lists_get_their_style() {
        let t = Tokens::dark();
        let f = fonts();
        let job = styled("# Big\n### Less\n###### Least\nbody");
        assert!(at(&job, "Big").font_id.size > at(&job, "Less").font_id.size);
        assert!(at(&job, "Less").font_id.size > at(&job, "body").font_id.size);
        assert_eq!(at(&job, "Least").font_id.family, f.bold);
        assert_eq!(at(&job, "# Big").color, t.text_faint);

        let job = styled("- one\n* two\n+ three\n1. four\n2) five\n- [ ] task\n- [x] done");
        for m in ["- one", "* two", "+ three", "1. four", "2) five", "[ ]", "[x]"] {
            assert_eq!(at(&job, m).color, t.accent, "{m}");
        }
        assert_eq!(at(&job, "one").color, t.text);
    }

    #[test]
    fn code_gets_its_style() {
        let t = Tokens::dark();
        let f = fonts();
        let job = styled("run `cargo test` now");
        assert_eq!(at(&job, "cargo").font_id, f.mono);
        assert_eq!(at(&job, "cargo").background, t.inline_code_bg);
        assert_eq!(at(&job, "`cargo").color, t.text_faint);

        let src = "intro\n```rust\nfn main() { let s = \"hi\"; }\n```\nafter";
        let s = style(&t, &f, src);
        let job = &s.job;
        assert_eq!(at(job, "fn main").font_id, f.mono);
        assert_eq!(at(job, "fn main").color, t.syntax_keyword);
        assert_eq!(at(job, "\"hi\"").color, t.syntax_string);
        assert_eq!(at(job, "```rust").color, t.text_faint);
        assert_eq!(at(job, "after").font_id, f.body);
        let start = src.find("```").unwrap_or(0);
        assert_eq!(s.code_blocks.first().map(|r| r.start), Some(start));

        // An unterminated fence styles to the end.
        let job = styled("```\nstill code\nand more");
        assert_eq!(at(&job, "and more").font_id, f.mono);
    }

    #[test]
    fn links_and_quotes_get_their_style() {
        let t = Tokens::dark();
        let job = styled("see [the docs](https://example.com/x) or https://byob.chat/a_b_c. ok");
        assert_eq!(at(&job, "the docs").color, t.link);
        assert_eq!(at(&job, "https://example").color, t.text_faint, "the target is dimmed");
        assert_eq!(at(&job, "https://byob").color, t.link);
        assert_eq!(at(&job, "a_b_c").color, t.link);
        assert_eq!(at(&job, ". ok").color, t.text, "trailing punctuation isn't part of the URL");

        let job = styled("> quoted *words*\n\nplain");
        assert_eq!(at(&job, "quoted").color, t.text_dim);
        assert_eq!(at(&job, "> ").color, t.text_faint);
        assert_eq!(at(&job, "plain").color, t.text);

        let job = styled("Use <context> tags");
        assert_eq!(at(&job, "<context>").color, t.text, "HTML-ish tags read as typed");
    }

    #[test]
    fn snake_case_and_arithmetic_are_not_emphasis() {
        let job = styled("call snake_case_names and my_var_ here, 2*3*4 = 24, a * b * c");
        for w in ["case", "names", "var", "3", "b "] {
            assert!(!at(&job, w).italics, "{w}");
        }
        assert!(job.sections.iter().all(|s| !s.format.italics));
    }

    #[test]
    fn bold_and_italic_shortcuts_wrap_and_unwrap_the_selection() {
        let cases = [
            ("say hello now", 4..9, "**", "say **hello** now", 6..11),
            ("say hello now", 4..10, "*", "say *hello* now", 5..10),
            ("say **hello** now", 6..11, "**", "say hello now", 4..9),
            ("say **hello** now", 4..13, "**", "say hello now", 4..9),
            ("say *hello* now", 5..10, "*", "say hello now", 4..9),
            ("say **hello** now", 6..11, "*", "say ***hello*** now", 7..12),
            ("say ***hello*** now", 7..12, "*", "say **hello** now", 6..11),
            ("ab", 1..1, "**", "a****b", 3..3),
            ("日本 語", 3..4, "*", "日本 *語*", 4..5),
            ("", 0..0, "*", "**", 1..1),
        ];
        for (text, sel, marker, want, want_sel) in cases {
            assert_eq!(toggle(text, sel.clone(), marker), (want.to_owned(), want_sel), "{text:?} {sel:?} {marker}");
        }
    }

    #[test]
    fn long_text_is_plain() {
        let big = "**x** ".repeat(MAX_STYLED / 6 + 10);
        let job = styled(&big);
        assert_eq!(job.text, big);
        assert_eq!(job.sections.len(), 1);
    }

    #[test]
    fn code_lines_get_one_background() {
        let ctx = egui::Context::default();
        ctx.run_ui(egui::RawInput::default(), |_| {}).textures_delta.clear();
        let t = Tokens::dark();
        let text = "a\n```\nx\ny\n```\nb";
        let s = style(&t, &real_fonts(), text);
        let galley = ctx.fonts_mut(|f| f.layout_job((*s.job).clone()));
        let shapes = code_backgrounds(&t, &galley, egui::Pos2::ZERO, 0.0, 100.0, &s.code_blocks, text.chars().count());
        assert_eq!(shapes.len(), 1);
        let Some(egui::Shape::Rect(r)) = shapes.first() else { panic!("{shapes:?}") };
        // Fence, two lines, fence: four rows, not the first or last line.
        let row = galley.rows.first().map_or(0.0, |r| r.rect().height());
        assert!(r.rect.top() >= row - 0.5 && r.rect.height() > row * 3.5, "{r:?}");
    }
}
