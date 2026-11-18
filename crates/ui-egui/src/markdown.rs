//! Markdown for assistant messages.
//!
//! [`parse`] turns CommonMark (+ GFM tables, strikethrough, task lists) into a small block
//! tree using `pulldown-cmark`; [`show`] draws it with the theme [`Tokens`]. Model output is
//! hostile, so nesting is capped (deeper containers are flattened) and nothing recurses
//! unboundedly.
//!
//! Streaming: [`Doc`] caches the parsed blocks of one entry. While the reply streams, text
//! before the last "safe" blank line (outside code fences, followed by an unindented line) is
//! parsed once and kept; only the tail after it is re-parsed per delta, so long replies don't
//! cost O(n) per frame. When the entry finishes the whole text is parsed once more so the
//! final rendering is exact.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use byob_protocol::layout::Show;
use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontFamily, FontId, Stroke, TextStyle};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::theme::Tokens;

/// Containers nested deeper than this are flattened into their parent.
const MAX_DEPTH: usize = 10;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Style {
    pub strong: bool,
    pub emphasis: bool,
    pub strike: bool,
    pub code: bool,
    /// LaTeX source of a formula (`$…$`, or `$$…$$` when display), drawn by [`crate::math`].
    pub math: Math,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Math {
    #[default]
    None,
    Inline,
    Display,
}

/// A run of uniformly styled text ending at byte `end` of [`Inline::text`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub end: usize,
    pub style: Style,
    /// Index into [`Inline::links`].
    pub link: Option<usize>,
}

/// Styled inline text (a paragraph, heading or table cell).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inline {
    pub text: String,
    pub runs: Vec<Run>,
    pub links: Vec<String>,
    /// GFM task list marker (`[ ]` / `[x]`).
    pub task: Option<bool>,
}

impl Inline {
    fn push(&mut self, s: &str, style: Style, link: Option<usize>) {
        if s.is_empty() {
            return;
        }
        self.text.push_str(s);
        let end = self.text.len();
        match self.runs.last_mut() {
            // Each formula is a run of its own.
            Some(r) if r.style == style && r.link == link && style.math == Math::None => r.end = end,
            _ => self.runs.push(Run { end, style, link }),
        }
    }

    fn plain(text: &str) -> Self {
        let mut i = Inline::default();
        i.push(text, Style::default(), None);
        i
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Paragraph(Inline),
    Heading { level: u8, text: Inline },
    Code { lang: String, code: String },
    Quote(Vec<Block>),
    List { start: Option<u64>, items: Vec<Vec<Block>> },
    Table { head: Vec<Inline>, rows: Vec<Vec<Inline>> },
    Rule,
}

fn options() -> Options {
    Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS | Options::ENABLE_MATH
}

/// `\(…\)` as `$…$` and `\[…\]` as `$$…$$`: the other way models write math (Markdown
/// would read `\(` as an escaped bracket). Code fences, inline code and `\\(` are left alone.
pub fn tex_delimiters(src: &str) -> std::borrow::Cow<'_, str> {
    if !src.contains("\\(") && !src.contains("\\[") {
        return std::borrow::Cow::Borrowed(src);
    }
    let mut out = String::with_capacity(src.len());
    let mut fence: Option<(u8, usize)> = None;
    let mut display = false;
    for line in src.split_inclusive('\n') {
        let after = next_fence(fence, line);
        if fence.is_some() || after.is_some() {
            fence = after;
            out.push_str(line);
            continue;
        }
        let b = line.as_bytes();
        let mut i = 0;
        let mut code: Option<usize> = None;
        while i < b.len() {
            let c = b[i];
            if c == b'`' {
                let run = b[i..].iter().take_while(|&&x| x == b'`').count();
                match code {
                    None => code = Some(run),
                    Some(open) if open == run => code = None,
                    _ => {}
                }
                out.push_str(line.get(i..i + run).unwrap_or_default());
                i += run;
                continue;
            }
            if c == b'\\' && code.is_none() {
                match b.get(i + 1) {
                    Some(b'\\') => {
                        out.push_str("\\\\");
                        i += 2;
                        continue;
                    }
                    Some(b'[') if !display => {
                        out.push_str("$$");
                        display = true;
                        i += 2;
                        continue;
                    }
                    Some(b']') if display => {
                        out.push_str("$$");
                        display = false;
                        i += 2;
                        continue;
                    }
                    Some(b'(') if !display => {
                        // Inline: only with its `\)` on the same line.
                        if let Some(end) = line.get(i + 2..).and_then(|r| r.find("\\)")) {
                            let inner = line.get(i + 2..i + 2 + end).unwrap_or_default().trim();
                            out.push('$');
                            out.push_str(inner);
                            out.push('$');
                            i += 2 + end + 2;
                            continue;
                        }
                    }
                    _ => {}
                }
            }
            // Copy the char whole (multi-byte text stays intact).
            let ch_len = line.get(i..).and_then(|r| r.chars().next()).map_or(1, char::len_utf8);
            out.push_str(line.get(i..i + ch_len).unwrap_or_default());
            i += ch_len;
        }
    }
    std::borrow::Cow::Owned(out)
}

/// Parse markdown into blocks.
pub fn parse(src: &str) -> Vec<Block> {
    let src = tex_delimiters(src);
    let mut it = Parser::new_ext(&src, options()).peekable();
    let mut out = Vec::new();
    // The root has no End event: `blocks` returns at EOF; loop in case of a stray End.
    while it.peek().is_some() {
        blocks(&mut it, 0, &mut out);
    }
    out
}

type Events<'a> = std::iter::Peekable<Parser<'a>>;

fn is_inline_tag(tag: &Tag<'_>) -> bool {
    matches!(tag, Tag::Emphasis | Tag::Strong | Tag::Strikethrough | Tag::Superscript | Tag::Subscript | Tag::Link { .. } | Tag::Image { .. })
}

fn is_inline_end(tag: TagEnd) -> bool {
    matches!(tag, TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Superscript | TagEnd::Subscript | TagEnd::Link | TagEnd::Image)
}

fn is_inline_event(ev: &Event<'_>) -> bool {
    match ev {
        Event::Start(tag) => is_inline_tag(tag),
        Event::End(tag) => is_inline_end(*tag),
        Event::Text(_)
        | Event::Code(_)
        | Event::InlineMath(_)
        | Event::DisplayMath(_)
        | Event::InlineHtml(_)
        | Event::FootnoteReference(_)
        | Event::SoftBreak
        | Event::HardBreak
        | Event::TaskListMarker(_) => true,
        Event::Html(_) | Event::Rule => false,
    }
}

/// Read blocks into `out` until the End of the enclosing container (consumed) or EOF.
fn blocks(it: &mut Events<'_>, depth: usize, out: &mut Vec<Block>) {
    // Containers flattened because of MAX_DEPTH: their End events must be skipped.
    let mut transparent = 0usize;
    while let Some(ev) = it.peek() {
        if is_inline_event(ev) {
            // Tight list items hold inline content without a Paragraph.
            let inline = inline(it, false);
            if !inline.text.trim().is_empty() || inline.task.is_some() {
                out.push(Block::Paragraph(inline));
            }
            continue;
        }
        let Some(ev) = it.next() else { break };
        match ev {
            Event::End(_) => {
                if transparent == 0 {
                    return;
                }
                transparent -= 1;
            }
            Event::Rule => out.push(Block::Rule),
            Event::Html(html) => out.push(Block::Paragraph(Inline::plain(html.trim_end()))),
            Event::Start(tag) => block_start(tag, it, depth, out, &mut transparent),
            // Inline events were handled above.
            _ => {}
        }
    }
}

fn block_start(tag: Tag<'_>, it: &mut Events<'_>, depth: usize, out: &mut Vec<Block>, transparent: &mut usize) {
    match tag {
        Tag::Paragraph => out.push(Block::Paragraph(inline(it, true))),
        Tag::Heading { level, .. } => {
            let level = match level {
                HeadingLevel::H1 => 1,
                HeadingLevel::H2 => 2,
                HeadingLevel::H3 => 3,
                HeadingLevel::H4 => 4,
                HeadingLevel::H5 => 5,
                HeadingLevel::H6 => 6,
            };
            out.push(Block::Heading { level, text: inline(it, true) });
        }
        Tag::CodeBlock(kind) => {
            let lang = match kind {
                CodeBlockKind::Fenced(info) => info.split_whitespace().next().unwrap_or_default().to_owned(),
                CodeBlockKind::Indented => String::new(),
            };
            let mut code = String::new();
            for ev in it.by_ref() {
                match ev {
                    Event::Text(t) | Event::Html(t) => code.push_str(&t),
                    Event::End(_) => break,
                    _ => {}
                }
            }
            let trimmed = code.trim_end_matches('\n').len();
            code.truncate(trimmed);
            out.push(Block::Code { lang, code });
        }
        Tag::HtmlBlock => {
            let mut html = String::new();
            for ev in it.by_ref() {
                match ev {
                    Event::Html(t) | Event::Text(t) => html.push_str(&t),
                    Event::End(_) => break,
                    _ => {}
                }
            }
            if !html.trim().is_empty() {
                out.push(Block::Paragraph(Inline::plain(html.trim_end())));
            }
        }
        _ if depth >= MAX_DEPTH => *transparent += 1,
        Tag::BlockQuote(_) => {
            let mut inner = Vec::new();
            blocks(it, depth + 1, &mut inner);
            out.push(Block::Quote(inner));
        }
        Tag::List(start) => {
            let mut items = Vec::new();
            loop {
                match it.peek() {
                    None => break,
                    Some(Event::End(_)) => {
                        it.next();
                        break;
                    }
                    Some(Event::Start(Tag::Item)) => {
                        it.next();
                        let mut item = Vec::new();
                        blocks(it, depth + 1, &mut item);
                        items.push(item);
                    }
                    // Not produced by pulldown-cmark; absorb it (and the list's End) as an item.
                    Some(_) => {
                        let mut item = Vec::new();
                        blocks(it, depth + 1, &mut item);
                        items.push(item);
                        break;
                    }
                }
            }
            out.push(Block::List { start, items });
        }
        Tag::Table(_) => out.push(table(it)),
        // Footnote definitions, definition lists, metadata, …: show their contents in place.
        _ => *transparent += 1,
    }
}

fn table(it: &mut Events<'_>) -> Block {
    let (mut head, mut rows, mut row) = (Vec::new(), Vec::new(), Vec::new());
    while let Some(ev) = it.next() {
        match ev {
            Event::Start(Tag::TableHead | Tag::TableRow) => row = Vec::new(),
            Event::End(TagEnd::TableHead) => head = std::mem::take(&mut row),
            Event::End(TagEnd::TableRow) => rows.push(std::mem::take(&mut row)),
            Event::Start(Tag::TableCell) => row.push(inline(it, true)),
            Event::End(TagEnd::Table) => break,
            _ => {}
        }
    }
    Block::Table { head, rows }
}

/// Read inline content. `explicit`: started by a Start(Paragraph/Heading/TableCell) whose End
/// we consume; otherwise stop (without consuming) at the first non-inline event.
fn inline(it: &mut Events<'_>, explicit: bool) -> Inline {
    let mut out = Inline::default();
    let (mut strong, mut emphasis, mut strike) = (0usize, 0usize, 0usize);
    let mut links: Vec<usize> = Vec::new();
    while let Some(ev) = it.peek() {
        if !is_inline_event(ev) {
            if explicit && matches!(ev, Event::End(_)) {
                it.next();
            }
            break;
        }
        let Some(ev) = it.next() else { break };
        let style = Style { strong: strong > 0, emphasis: emphasis > 0, strike: strike > 0, ..Style::default() };
        let link = links.last().copied();
        match ev {
            Event::Start(Tag::Strong) => strong += 1,
            Event::Start(Tag::Emphasis) => emphasis += 1,
            Event::Start(Tag::Strikethrough) => strike += 1,
            Event::End(TagEnd::Strong) => strong = strong.saturating_sub(1),
            Event::End(TagEnd::Emphasis) => emphasis = emphasis.saturating_sub(1),
            Event::End(TagEnd::Strikethrough) => strike = strike.saturating_sub(1),
            Event::Start(Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. }) => {
                out.links.push(dest_url.to_string());
                links.push(out.links.len() - 1);
            }
            Event::End(TagEnd::Link | TagEnd::Image) => {
                links.pop();
            }
            Event::Text(t) | Event::InlineHtml(t) => out.push(&t, style, link),
            Event::Code(t) => out.push(&t, Style { code: true, ..style }, link),
            Event::InlineMath(t) => out.push(&t, Style { math: Math::Inline, ..style }, link),
            Event::DisplayMath(t) => out.push(&t, Style { math: Math::Display, ..style }, link),
            Event::FootnoteReference(name) => out.push(&format!("[{name}]"), style, link),
            Event::SoftBreak => out.push(" ", style, link),
            Event::HardBreak => out.push("\n", style, link),
            Event::TaskListMarker(done) => out.task = Some(done),
            _ => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------------------
// Streaming cache

/// Parsed blocks of one message, updated incrementally while it streams.
#[derive(Debug, Default)]
pub struct Doc {
    /// The text the blocks were built from.
    src: String,
    done: bool,
    /// `src[..stable_end]` is parsed into `stable` and never re-parsed while streaming.
    stable_end: usize,
    stable: Vec<Block>,
    tail: Vec<Block>,
    /// Scanner state: next unscanned line start, open code fence, previous line blank.
    scan_pos: usize,
    fence: Option<(u8, usize)>,
    prev_blank: bool,
}

impl Doc {
    pub fn blocks(&self) -> impl Iterator<Item = &Block> {
        self.stable.iter().chain(&self.tail)
    }

    /// Bring the blocks up to date with `src`. Cheap when nothing changed.
    pub fn update(&mut self, src: &str, done: bool) {
        if !src.starts_with(self.src.as_str()) {
            // Replaced rather than appended to (e.g. a new transcript): start over.
            *self = Doc::default();
        } else if src.len() == self.src.len() && done == self.done {
            return;
        }
        self.src.push_str(src.get(self.src.len()..).unwrap_or_default());
        self.done = done;
        if done {
            self.stable = parse(src);
            self.stable_end = src.len();
            self.scan_pos = src.len();
            self.tail.clear();
            return;
        }
        if let Some(boundary) = self.scan(src).filter(|&b| b > self.stable_end) {
            let chunk = src.get(self.stable_end..boundary).unwrap_or_default();
            self.stable.extend(parse(chunk));
            self.stable_end = boundary;
        }
        self.tail = parse(src.get(self.stable_end..).unwrap_or_default());
    }

    /// Scan complete lines not yet seen; return the last safe split point found.
    fn scan(&mut self, src: &str) -> Option<usize> {
        let mut boundary = None;
        let mut pos = self.scan_pos;
        while let Some(nl) = src.get(pos..).and_then(|rest| rest.find('\n')) {
            let line = src.get(pos..pos + nl).unwrap_or_default();
            let starts_flush = line.chars().next().is_some_and(|c| !c.is_whitespace());
            if self.fence.is_none() && self.prev_blank && starts_flush {
                boundary = Some(pos);
            }
            self.fence = next_fence(self.fence, line);
            self.prev_blank = self.fence.is_none() && line.trim().is_empty();
            pos += nl + 1;
        }
        self.scan_pos = pos;
        boundary
    }
}

/// A fence line: (`` ` `` or `~`, run length, rest of line).
fn fence_marker(line: &str) -> Option<(u8, usize, &str)> {
    let t = line.trim_start();
    let ch = *t.as_bytes().first()?;
    if ch != b'`' && ch != b'~' {
        return None;
    }
    let n = t.bytes().take_while(|&b| b == ch).count();
    (n >= 3).then(|| (ch, n, t.get(n..).unwrap_or_default()))
}

/// Code-fence state after `line`. Over-detecting fences only delays splitting, never
/// changes the rendering.
fn next_fence(state: Option<(u8, usize)>, line: &str) -> Option<(u8, usize)> {
    match (state, fence_marker(line)) {
        (None, Some((ch, n, rest))) if !(ch == b'`' && rest.contains('`')) => Some((ch, n)),
        (Some((ch, n)), Some((c2, n2, rest))) if c2 == ch && n2 >= n && rest.trim().is_empty() => None,
        (state, _) => state,
    }
}

/// Per-entry [`Doc`]s. An entry's text may be split into parts (text between tool steps),
/// each with its own document.
#[derive(Debug, Default)]
pub struct Cache {
    docs: HashMap<u64, Vec<Doc>>,
}

impl Cache {
    /// The up-to-date document for entry `id`.
    pub fn doc(&mut self, id: u64, src: &str, done: bool) -> &Doc {
        self.part(id, 0, src, done)
    }

    /// The up-to-date document for part `part` of entry `id`.
    pub fn part(&mut self, id: u64, part: usize, src: &str, done: bool) -> &Doc {
        let parts = self.docs.entry(id).or_default();
        if parts.len() <= part {
            parts.resize_with(part + 1, Doc::default);
        }
        // in bounds: just resized to cover `part`
        let doc = &mut parts[part];
        doc.update(src, done);
        doc
    }

    /// Drop documents of entries that no longer exist.
    pub fn retain(&mut self, live: impl Fn(u64) -> bool) {
        self.docs.retain(|id, _| live(*id));
    }

    pub fn len(&self) -> usize {
        self.docs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }
}

// ---------------------------------------------------------------------------------------
// Rendering

/// Only these schemes are opened when a link is clicked (model output is untrusted).
pub fn is_safe_url(url: &str) -> bool {
    let lower = url.trim_start().to_ascii_lowercase();
    ["http://", "https://", "mailto:"].iter().any(|p| lower.starts_with(p))
}

/// Draw a document. `id` must be unique per message (it seeds code-block and table ids).
pub fn show(ui: &mut egui::Ui, t: &Tokens, doc: &Doc, id: egui::Id) {
    let blocks: Vec<&Block> = doc.blocks().collect();
    show_blocks(ui, t, &blocks, id, t.text, 0);
}

fn body_font(ui: &egui::Ui) -> FontId {
    TextStyle::Body.resolve(ui.style())
}

fn show_blocks(ui: &mut egui::Ui, t: &Tokens, blocks: &[&Block], id: egui::Id, color: Color32, depth: usize) {
    for (i, block) in blocks.iter().enumerate() {
        if i > 0 {
            ui.add_space(4.0);
        }
        show_block(ui, t, block, id.with(i), color, depth);
    }
}

fn show_block(ui: &mut egui::Ui, t: &Tokens, block: &Block, id: egui::Id, color: Color32, depth: usize) {
    let body = body_font(ui);
    match block {
        Block::Paragraph(inline) => {
            if let Some(f) = lone_display_math(ui.ctx(), inline) {
                crate::math::block(ui, &f, body.size * crate::math::SCALE, color, id.with("math"));
            } else {
                inline_label(ui, t, inline, &body, color, ui.available_width());
            }
        }
        Block::Heading { level, text } => {
            let scale = match level {
                1 => 1.45,
                2 => 1.3,
                3 => 1.15,
                _ => 1.0,
            };
            if *level <= 2 {
                ui.add_space(4.0);
            }
            let font = FontId::new(body.size * scale, crate::theme::bold_or_regular(ui.ctx()));
            inline_label(ui, t, text, &font, t.text_strong, ui.available_width());
        }
        Block::Code { lang, code } => code_block(ui, t, lang, code, id),
        Block::Quote(inner) => {
            let inner: Vec<&Block> = inner.iter().collect();
            let resp = egui::Frame::new().inner_margin(egui::Margin { left: 12, right: 0, top: 2, bottom: 2 }).show(ui, |ui| {
                show_blocks(ui, t, &inner, id, t.text_dim, depth + 1);
            });
            let r = resp.response.rect;
            ui.painter().rect_filled(egui::Rect::from_min_max(r.min, egui::pos2(r.min.x + 3.0, r.max.y)), 1.5, t.quote_bar);
        }
        Block::List { start, items } => {
            for (i, item) in items.iter().enumerate() {
                let item: Vec<&Block> = item.iter().collect();
                ui.horizontal_top(|ui| {
                    let row = ui.fonts_mut(|f| f.row_height(&body));
                    let width = if start.is_some() { body.size * 1.6 } else { body.size * 1.1 };
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, row), egui::Sense::hover());
                    let task = match item.first() {
                        Some(Block::Paragraph(p)) => p.task,
                        _ => None,
                    };
                    if let Some(done) = task {
                        checkbox(ui, t, rect, done);
                    } else if let Some(n) = start {
                        let marker = format!("{}.", n.saturating_add(i as u64));
                        ui.painter().text(rect.right_top(), egui::Align2::RIGHT_TOP, marker, body.clone(), t.text_dim);
                    } else {
                        // Painted, not glyphs: the default fonts lack ◦ and ▪.
                        let r = (body.size * 0.17).max(2.0);
                        let c = egui::pos2(rect.right() - r - 3.0, rect.center().y);
                        match depth % 3 {
                            0 => ui.painter().circle_filled(c, r, t.text_dim),
                            1 => ui.painter().circle_stroke(c, r - 0.5, Stroke::new(1.0, t.text_dim)),
                            _ => ui.painter().rect_filled(egui::Rect::from_center_size(c, egui::vec2(r * 1.6, r * 1.6)), 0.0, t.text_dim),
                        };
                    }
                    ui.vertical(|ui| show_blocks(ui, t, &item, id.with(i), color, depth + 1));
                });
            }
        }
        Block::Table { head, rows } => table_view(ui, t, head, rows, id),
        Block::Rule => {
            ui.separator();
        }
    }
}

/// A painted task-list checkbox, right-aligned in `rect` (the default fonts lack ☐/☑).
fn checkbox(ui: &egui::Ui, t: &Tokens, rect: egui::Rect, done: bool) {
    let side = (rect.height() * 0.62).min(rect.width());
    let r = egui::Rect::from_center_size(egui::pos2(rect.right() - side / 2.0 - 2.0, rect.center().y), egui::vec2(side, side));
    let painter = ui.painter();
    if done {
        painter.rect_filled(r, 3.0, t.accent);
        let p = |x: f32, y: f32| egui::pos2(r.left() + r.width() * x, r.top() + r.height() * y);
        painter.line(vec![p(0.22, 0.52), p(0.42, 0.72), p(0.78, 0.3)], Stroke::new(1.8, t.on_accent));
    } else {
        painter.rect_stroke(r, 3.0, Stroke::new(1.2, t.text_dim), egui::StrokeKind::Inside);
    }
}

fn code_block(ui: &mut egui::Ui, t: &Tokens, lang: &str, code: &str, id: egui::Id) {
    egui::Frame::new()
        .fill(t.code_bg)
        .stroke(Stroke::new(1.0, t.code_border))
        .corner_radius(t.radius)
        .inner_margin(egui::Margin { left: 10, right: 6, top: 4, bottom: 8 })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let shape = crate::layout::get(ui.ctx());
            let hovered = ui.ui_contains_pointer();
            let (show_lang, show_copy) = (shape.visible("code.language", hovered), shape.visible("code.copy", hovered));
            // Both hidden: no row. Else the row keeps its height, so hovering doesn't move the code.
            let row = [shape.show("code.language"), shape.show("code.copy")].iter().any(|s| *s != Show::Hidden);
            ui.horizontal(|ui| {
                if !row {
                    return;
                }
                ui.set_min_height(ui.text_style_height(&egui::TextStyle::Small).max(ui.spacing().interact_size.y * 0.75));
                if show_lang {
                    let label = if lang.is_empty() { "code" } else { lang };
                    ui.label(egui::RichText::new(label).small().color(t.text_faint));
                }
                if !show_copy {
                    return;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let copied_at: Option<f64> = ui.data(|d| d.get_temp(id));
                    let now = ui.input(|i| i.time);
                    let recent = copied_at.is_some_and(|at| now - at < 1.5);
                    let text = if recent { "Copied" } else { "Copy" };
                    let button = egui::Button::new(egui::RichText::new(text).small().color(t.text_dim)).small().frame(false);
                    if ui.add(button).on_hover_text("Copy code").clicked() {
                        ui.ctx().copy_text(code.to_owned());
                        ui.data_mut(|d| d.insert_temp(id, now));
                    }
                    if recent {
                        ui.ctx().request_repaint_after(std::time::Duration::from_millis(300));
                    }
                });
            });
            egui::ScrollArea::horizontal().id_salt(id.with("scroll")).auto_shrink([false, true]).show(ui, |ui| {
                let font = egui::TextStyle::Monospace.resolve(ui.style());
                let job = crate::highlight::job(t, lang, code, &font);
                ui.add(egui::Label::new(job).extend().selectable(true));
            });
        });
}

fn table_view(ui: &mut egui::Ui, t: &Tokens, head: &[Inline], rows: &[Vec<Inline>], id: egui::Id) {
    let cols = rows.iter().map(Vec::len).chain([head.len()]).max().unwrap_or(0).max(1);
    let body = body_font(ui);
    #[allow(clippy::cast_precision_loss)] // column counts are small
    let col_width = ((ui.available_width() - 24.0) / cols as f32 - 16.0).max(80.0);
    // The grid's stripes reach half a row gap (3) past their row: 3 more at the bottom keeps
    // the last stripe as far from the border as the sides, so its square corners sit evenly
    // inside the rounded ones (radius − inset = 0, see `theme::inner_radius`).
    let margin = egui::Margin { left: 6, right: 6, top: 6, bottom: 9 };
    egui::Frame::new().stroke(Stroke::new(1.0, t.border)).corner_radius(t.radius).inner_margin(margin).show(ui, |ui| {
        egui::ScrollArea::horizontal().id_salt(id.with("scroll")).auto_shrink([true, true]).show(ui, |ui| {
            egui::Grid::new(id.with("grid")).striped(true).spacing([16.0, 6.0]).show(ui, |ui| {
                for cell in head {
                    let strong =
                        Inline { runs: cell.runs.iter().map(|r| Run { style: Style { strong: true, ..r.style }, ..r.clone() }).collect(), ..cell.clone() };
                    inline_label(ui, t, &strong, &body, t.text_strong, col_width);
                }
                ui.end_row();
                for row in rows {
                    for cell in row {
                        inline_label(ui, t, cell, &body, t.text, col_width);
                    }
                    ui.end_row();
                }
            });
        });
    });
}

/// A formula placed in a line of text: the first of the two placeholder chars standing in
/// for it, and the formula drawn from there.
pub struct Placed {
    pub char_index: usize,
    pub formula: Arc<crate::math::Formula>,
    pub size: f32,
}

/// Stands in for a formula in the text layout, twice: not whitespace (lines don't break at
/// it or between the two), invisible, and harmless when copied. The formula's width is the
/// second one's letter spacing, which egui puts between the two wherever the pair lands
/// (spacing on a lone char vanishes at the start of a line).
pub const PLACEHOLDER: char = '\u{2060}';

/// The layout job for inline text, the char ranges of links (indices into `inline.links`)
/// and where formulas go. `math`: typeset formulas, with the width [`PLACEHOLDER`] takes in
/// `font` (`None`: show their `$…$` source).
pub fn layout_job(
    t: &Tokens,
    inline: &Inline,
    font: &FontId,
    bold: &FontFamily,
    color: Color32,
    wrap_width: f32,
    math: Option<f32>,
) -> (LayoutJob, Vec<(Range<usize>, usize)>, Vec<Placed>) {
    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap_width;
    let mut chars = 0usize;
    let mut links = Vec::new();
    let mut placed = Vec::new();
    let mut start = 0usize;
    // Inline code follows the code size knob relative to the UI text size.
    let mono = FontId::new(font.size * 0.92 * (t.knobs.code_scale / t.knobs.font_scale.max(0.1)), FontFamily::Monospace);
    let line_height = crate::theme::line_height(t, font.size);
    let math_size = font.size * crate::math::SCALE;
    for run in &inline.runs {
        let Some(text) = inline.text.get(start..run.end) else { break };
        start = run.end;
        let s = run.style;
        let mut fmt = TextFormat { font_id: font.clone(), color, italics: s.emphasis, line_height, ..TextFormat::default() };
        if s.math != Math::None {
            let display = s.math == Math::Display;
            if let Some((space, f)) = math.zip(crate::math::typeset(text, display)) {
                // An invisible stand-in as wide as the formula, its line tall enough for it
                // (centred, so there is room below the baseline too).
                let w = f.width() * math_size;
                let reach = f.height().max(f.depth()) * math_size;
                fmt.color = Color32::TRANSPARENT;
                fmt.line_height = Some((2.0 * reach + font.size * 0.5).max(line_height.unwrap_or(0.0)));
                fmt.valign = egui::Align::Center;
                placed.push(Placed { char_index: chars, formula: f, size: math_size });
                if let Some(link) = run.link {
                    links.push((chars..chars + 2, link));
                }
                chars += 2;
                job.append(&PLACEHOLDER.to_string(), 0.0, fmt.clone());
                job.append(&PLACEHOLDER.to_string(), 0.0, TextFormat { extra_letter_spacing: (w - space).max(0.0), ..fmt });
                continue;
            }
            // Not typeset: its source, as written.
            let fence = if display { "$$" } else { "$" };
            let source = format!("{fence}{text}{fence}");
            let n = source.chars().count();
            if let Some(link) = run.link {
                fmt.color = t.link;
                links.push((chars..chars + n, link));
            }
            chars += n;
            job.append(&source, 0.0, fmt);
            continue;
        }
        if s.strong {
            fmt.font_id.family = bold.clone();
            fmt.color = t.text_strong;
        }
        if s.code {
            fmt.font_id = mono.clone();
            fmt.color = t.inline_code_text;
            fmt.background = t.inline_code_bg;
        }
        if s.strike {
            fmt.strikethrough = Stroke::new(1.0, fmt.color);
        }
        let n = text.chars().count();
        if let Some(link) = run.link {
            fmt.color = t.link;
            fmt.underline = Stroke::new(1.0, t.link.gamma_multiply(0.6));
            links.push((chars..chars + n, link));
        }
        chars += n;
        job.append(text, 0.0, fmt);
    }
    // Text beside a formula sits mid-line with it.
    if !placed.is_empty() {
        for section in &mut job.sections {
            section.format.valign = egui::Align::Center;
        }
    }
    (job, links, placed)
}

/// A paragraph that is one display formula (`$$…$$` on its own), typeset, while math is on.
fn lone_display_math(ctx: &egui::Context, inline: &Inline) -> Option<Arc<crate::math::Formula>> {
    if !crate::math::enabled(ctx) {
        return None;
    }
    let mut start = 0;
    let mut formula = None;
    for run in &inline.runs {
        let text = inline.text.get(start..run.end)?;
        start = run.end;
        match run.style.math {
            Math::Display if formula.is_none() => formula = Some(text),
            Math::None if text.trim().is_empty() => {}
            _ => return None,
        }
    }
    crate::math::typeset(formula?, true)
}

/// Where each formula's baseline starts in `galley` (relative to its top left).
fn baselines(galley: &egui::Galley, placed: &[Placed]) -> Vec<egui::Pos2> {
    let mut out = Vec::with_capacity(placed.len());
    let mut index = 0usize;
    let mut want = placed.iter().map(|p| p.char_index).peekable();
    for row in &galley.rows {
        for g in &row.glyphs {
            if want.peek() == Some(&index) {
                want.next();
                out.push(row.pos + g.pos.to_vec2());
            }
            index += 1;
        }
        if row.ends_with_newline {
            index += 1;
        }
    }
    out
}

fn inline_label(ui: &mut egui::Ui, t: &Tokens, inline: &Inline, font: &FontId, color: Color32, wrap_width: f32) {
    let bold = crate::theme::bold_or_regular(ui.ctx());
    let math = crate::math::enabled(ui.ctx()).then(|| ui.fonts_mut(|f| f.glyph_width(font, PLACEHOLDER)));
    let (job, links, placed) = layout_job(t, inline, font, &bold, color, wrap_width.max(20.0), math);
    let galley = ui.fonts_mut(|f| f.layout_job(job));
    let resp = ui.add(egui::Label::new(Arc::clone(&galley)).selectable(true));
    for (p, at) in placed.iter().zip(baselines(&galley, &placed)) {
        crate::math::paint(ui.painter(), &p.formula, resp.rect.min + at.to_vec2(), p.size, color);
    }
    if links.is_empty() {
        return;
    }
    let Some(pos) = resp.hover_pos() else { return };
    let Some(&(_, link)) = link_at(&galley, pos - resp.rect.min, &links) else { return };
    let Some(url) = inline.links.get(link) else { return };
    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    let resp = resp.on_hover_text_at_pointer(url.as_str());
    if resp.clicked() && is_safe_url(url) {
        ui.ctx().open_url(egui::OpenUrl::new_tab(url));
    }
}

/// The link under `pos` (relative to the galley's top-left), if any.
fn link_at<'a>(galley: &egui::Galley, pos: egui::Vec2, links: &'a [(Range<usize>, usize)]) -> Option<&'a (Range<usize>, usize)> {
    if !galley.rect.expand(2.0).contains(pos.to_pos2()) {
        return None;
    }
    let cursor = galley.cursor_from_pos(pos);
    let rect = galley.pos_from_cursor(cursor);
    if pos.y < rect.min.y || pos.y > rect.max.y {
        return None;
    }
    // `cursor_from_pos` snaps to the nearest boundary: left of it means the previous char.
    let index = if pos.x < rect.min.x { cursor.index.0.checked_sub(1)? } else { cursor.index.0 };
    links.iter().find(|(range, _)| range.contains(&index))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# Title\n\nSome **bold** and *italic* and `code` text with a [link](https://example.com).\n\n\
        ## Code\n\n```rust\nfn main() {\n\n    println!(\"hi\");\n}\n```\n\n\
        - one\n- two\n  - nested\n- [x] done\n\n\
        > quoted\n> text\n\n\
        | a | b |\n|---|:-:|\n| 1 | **2** |\n| 3 | 4 |\n\n---\n\nThe end.\n";

    fn para(text: &str) -> Block {
        Block::Paragraph(Inline::plain(text))
    }

    #[test]
    fn parses_common_constructs() {
        let blocks = parse(SAMPLE);
        assert!(matches!(&blocks[0], Block::Heading { level: 1, text } if text.text == "Title"));
        let Block::Paragraph(p) = &blocks[1] else { panic!("{:?}", blocks[1]) };
        assert_eq!(p.text, "Some bold and italic and code text with a link.");
        assert_eq!(p.links, ["https://example.com"]);
        assert!(p.runs.iter().any(|r| r.style.strong));
        assert!(p.runs.iter().any(|r| r.style.emphasis));
        assert!(p.runs.iter().any(|r| r.style.code));
        assert!(p.runs.iter().any(|r| r.link == Some(0)));
        assert_eq!(blocks[3], Block::Code { lang: "rust".into(), code: "fn main() {\n\n    println!(\"hi\");\n}".into() });
        let Block::List { start: None, items } = &blocks[4] else { panic!("{:?}", blocks[4]) };
        assert_eq!(items.len(), 3);
        assert!(matches!(&items[1][1], Block::List { items, .. } if items[0] == vec![para("nested")]));
        assert!(matches!(&items[2][0], Block::Paragraph(i) if i.task == Some(true) && i.text == "done"));
        assert!(matches!(&blocks[5], Block::Quote(inner) if inner.len() == 1));
        let Block::Table { head, rows } = &blocks[6] else { panic!("{:?}", blocks[6]) };
        assert_eq!(head.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(rows.len(), 2);
        assert!(rows[0][1].runs[0].style.strong);
        assert_eq!(blocks[7], Block::Rule);
        assert_eq!(blocks[8], para("The end."));
        assert_eq!(blocks.len(), 9);
    }

    #[test]
    fn ordered_lists_keep_their_start() {
        let blocks = parse("3. c\n4. d\n");
        assert!(matches!(&blocks[0], Block::List { start: Some(3), items } if items.len() == 2));
    }

    fn stream(src: &str, step: usize) -> Doc {
        let mut doc = Doc::default();
        let mut end = 0;
        while end < src.len() {
            end = (end + step).min(src.len());
            while !src.is_char_boundary(end) {
                end += 1;
            }
            doc.update(&src[..end], false);
        }
        doc
    }

    #[test]
    fn streaming_matches_a_full_parse() {
        for step in [1, 3, 7, 40, 1000] {
            let doc = stream(SAMPLE, step);
            let streamed: Vec<Block> = doc.blocks().cloned().collect();
            assert_eq!(streamed, parse(SAMPLE), "step {step}");
            assert!(doc.stable_end > SAMPLE.len() / 2, "step {step}: stable prefix should advance ({})", doc.stable_end);
        }
    }

    #[test]
    fn blank_lines_inside_fences_are_not_split_points() {
        let src = "```\na\n\nb\n\n~~~\n\nc\n```\n\nafter\n";
        let doc = stream(src, 2);
        assert_eq!(doc.blocks().cloned().collect::<Vec<_>>(), parse(src));
        assert!(matches!(doc.blocks().next(), Some(Block::Code { code, .. }) if code == "a\n\nb\n\n~~~\n\nc"));
    }

    #[test]
    fn finishing_reparses_everything() {
        // A loose list split while streaming renders as one list once done.
        let src = "1. a\n\n2. b\n\n3. c\n";
        let mut doc = stream(src, 1);
        doc.update(src, true);
        assert_eq!(doc.blocks().cloned().collect::<Vec<_>>(), parse(src));
        assert!(matches!(doc.blocks().next(), Some(Block::List { items, .. }) if items.len() == 3));
    }

    #[test]
    fn replaced_text_resets_the_cache() {
        let mut doc = Doc::default();
        doc.update("# one\n\ntwo\n\nthree", false);
        doc.update("# uno\n\ndos\n\ntres\n\ncuatro", false);
        assert_eq!(doc.blocks().cloned().collect::<Vec<_>>(), parse("# uno\n\ndos\n\ntres\n\ncuatro"));
        doc.update("x", false);
        assert_eq!(doc.blocks().cloned().collect::<Vec<_>>(), vec![para("x")]);
    }

    #[test]
    fn streaming_work_is_bounded_by_the_tail() {
        // Many paragraphs: the tail stays small, so the stable prefix covers almost everything.
        let src: String = (0..500).map(|i| format!("Paragraph number {i} with some words.\n\n")).collect();
        let doc = stream(&src, 13);
        assert!(src.len() - doc.stable_end < 100, "tail {}", src.len() - doc.stable_end);
        assert_eq!(doc.blocks().count(), 500);
    }

    #[test]
    fn hostile_nesting_does_not_overflow() {
        for src in [">".repeat(50_000), "- ".repeat(20_000) + "x", "*".repeat(20_000) + "x", "[".repeat(20_000) + "x"] {
            let blocks = parse(&src);
            let mut doc = Doc::default();
            doc.update(&src, true);
            assert_eq!(doc.blocks().count(), blocks.len());
        }
        fn depth(b: &Block) -> usize {
            match b {
                Block::Quote(inner) => 1 + inner.iter().map(depth).max().unwrap_or(0),
                Block::List { items, .. } => 1 + items.iter().flatten().map(depth).max().unwrap_or(0),
                _ => 0,
            }
        }
        let deep = parse(&format!("{} deep", ">".repeat(100)));
        assert!(deep.iter().map(depth).max().unwrap_or(0) <= MAX_DEPTH);
        assert!(matches!(deep.first(), Some(Block::Quote(_))));
    }

    #[test]
    fn unclosed_constructs_mid_stream_parse() {
        for partial in ["```py\nprint(1", "| a | b |\n|---", "**bold", "[link](http://x", "- [", "> "] {
            let mut doc = Doc::default();
            doc.update(partial, false);
            let _ = doc.blocks().count();
        }
    }

    #[test]
    fn renders_before_and_after_fonts_are_installed() {
        // Fonts set with `install_fonts` only apply next frame; the first frame must not ask
        // egui for the not-yet-bound bold family (which would panic).
        let ctx = egui::Context::default();
        let mut doc = Doc::default();
        doc.update(SAMPLE, true);
        crate::theme::install_fonts(&ctx, crate::theme::UiFont::Inter, crate::theme::MonoFont::Hack);
        for _ in 0..2 {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| show(ui, &Tokens::light(), &doc, egui::Id::unique("t")));
            out.textures_delta.clear();
        }
        assert_eq!(crate::theme::bold_or_regular(&ctx), crate::theme::bold_family());
    }

    #[test]
    fn only_web_and_mail_links_open() {
        assert!(is_safe_url("https://example.com"));
        assert!(is_safe_url("HTTP://x"));
        assert!(is_safe_url("mailto:a@b.c"));
        assert!(!is_safe_url("javascript:alert(1)"));
        assert!(!is_safe_url("file:///etc/passwd"));
        assert!(!is_safe_url("/relative"));
    }

    #[test]
    fn link_char_ranges_account_for_multibyte_text() {
        let blocks = parse("- [ ] é [here](https://x.y) z");
        let Some(Block::List { items, .. }) = blocks.first() else { panic!("{blocks:?}") };
        let Block::Paragraph(inline) = &items[0][0] else { panic!("{items:?}") };
        let (job, links, _) = layout_job(&Tokens::dark(), inline, &FontId::proportional(14.0), &FontFamily::Proportional, Color32::WHITE, 500.0, None);
        assert_eq!(links.len(), 1);
        let chars: Vec<char> = job.text.chars().collect();
        let linked: String = chars[links[0].0.clone()].iter().collect();
        assert_eq!(linked, "here");
    }

    #[test]
    fn link_hit_testing_finds_the_link_under_the_pointer() {
        let ctx = egui::Context::default();
        let inline = match parse("go [there](https://x.y) now").first() {
            Some(Block::Paragraph(i)) => i.clone(),
            other => panic!("{other:?}"),
        };
        let (job, links, _) = layout_job(&Tokens::dark(), &inline, &FontId::proportional(14.0), &FontFamily::Proportional, Color32::WHITE, 500.0, None);
        let mut galley = None;
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| galley = Some(ui.fonts_mut(|f| f.layout_job(job.clone()))));
        out.textures_delta.clear();
        let Some(galley) = galley else { panic!("no galley") };
        let y = galley.rect.center().y;
        let at = |index: usize| galley.pos_from_cursor(egui::text::CCursor::new(index)).center().x;
        // Middle of "t" (index 3) and of the space before it (index 2).
        let inside = egui::vec2((at(3) + at(4)) / 2.0, y);
        let before = egui::vec2((at(2) + at(3)) / 2.0, y);
        assert!(link_at(&galley, inside, &links).is_some());
        assert!(link_at(&galley, before, &links).is_none());
        assert!(link_at(&galley, egui::vec2(at(1), y + 100.0), &links).is_none());
    }
}

#[cfg(test)]
mod math_tests {
    use super::*;

    fn runs(src: &str) -> Vec<(String, Math)> {
        let blocks = parse(src);
        let Some(Block::Paragraph(p)) = blocks.first() else { panic!("{blocks:?}") };
        let mut start = 0;
        p.runs
            .iter()
            .map(|r| {
                let s = p.text[start..r.end].to_owned();
                start = r.end;
                (s, r.style.math)
            })
            .collect()
    }

    #[test]
    fn math_is_its_own_runs_and_prices_stay_text() {
        assert_eq!(runs("Area is $\\pi r^2$, done."), [("Area is ".into(), Math::None), ("\\pi r^2".into(), Math::Inline), (", done.".into(), Math::None)]);
        assert_eq!(runs("$$\\int_0^1 x\\,dx$$"), [("\\int_0^1 x\\,dx".into(), Math::Display)]);
        assert_eq!(runs("$a$$b$")[..2], [("a".into(), Math::Inline), ("b".into(), Math::Inline)], "two formulas side by side stay two");
        assert!(runs("It costs $5 and $10.").iter().all(|(_, m)| *m == Math::None), "prices aren't math");
    }

    #[test]
    fn formulas_become_placeholders_when_on_and_source_when_off() {
        let blocks = parse("Euler: $e^{i\\pi} + 1 = 0$!");
        let Some(Block::Paragraph(p)) = blocks.first() else { panic!() };
        let font = FontId::proportional(14.0);
        let (job, _, placed) = layout_job(&Tokens::dark(), p, &font, &FontFamily::Proportional, Color32::WHITE, 500.0, Some(4.0));
        assert_eq!(job.text, format!("Euler: {PLACEHOLDER}{PLACEHOLDER}!"));
        assert_eq!(placed.len(), 1);
        assert_eq!(placed[0].char_index, 7);
        assert!(placed[0].formula.width() > 2.0);
        let (job, _, placed) = layout_job(&Tokens::dark(), p, &font, &FontFamily::Proportional, Color32::WHITE, 500.0, None);
        assert_eq!(job.text, "Euler: $e^{i\\pi} + 1 = 0$!");
        assert!(placed.is_empty());
    }

    /// Regression: the space for an inline formula came before its stand-in mid-line and
    /// vanished at the start of a line, so formulas were drawn over the next words. Wherever
    /// the formula lands, its slot is its width and the text after it starts past it.
    #[test]
    fn an_inline_formula_gets_its_width_wherever_it_lands() {
        let ctx = egui::Context::default();
        for (src, wrap) in [("$x^2 + y^2$ first", 500.0), ("see $x^2 + y^2$ here", 500.0), ("aaaa bbbb cccc $x^2 + y^2$ wrapped", 90.0)] {
            let blocks = parse(src);
            let Some(Block::Paragraph(p)) = blocks.first() else { panic!() };
            let font = FontId::proportional(14.0);
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                let space = ui.fonts_mut(|f| f.glyph_width(&font, PLACEHOLDER));
                let (job, _, placed) = layout_job(&Tokens::dark(), p, &font, &FontFamily::Proportional, Color32::WHITE, wrap, Some(space));
                let g = ui.fonts_mut(|f| f.layout_job(job));
                let w = placed[0].formula.width() * placed[0].size;
                let row = g.rows.iter().find(|r| r.glyphs.iter().any(|gl| gl.chr == PLACEHOLDER)).unwrap();
                let xs: Vec<f32> = row.glyphs.iter().filter(|gl| gl.chr == PLACEHOLDER).map(|gl| gl.pos.x).collect();
                assert!((xs[1] - xs[0] - w).abs() < 1.0, "{src}: slot {} for width {w}", xs[1] - xs[0]);
                let at = baselines(&g, &placed)[0];
                assert!((at.x - (row.pos.x + xs[0])).abs() < 0.01, "{src}: drawn from the first stand-in");
            });
            out.textures_delta.clear();
        }
    }

    /// Regression: deepseek wrote `\\(…\\)` and `\\[…\\]` (as most models do), and Markdown
    /// read `\\(` as an escaped bracket. They're math too; code is left alone.
    #[test]
    fn backslash_delimiters_are_math() {
        assert_eq!(tex_delimiters("For \\(ax^2\\), divide:\n\\[ x = \\frac{1}{2} \\]\n"), "For $ax^2$, divide:\n$$ x = \\frac{1}{2} $$\n");
        assert_eq!(tex_delimiters("\\[\na+b\n\\]"), "$$\na+b\n$$", "across lines");
        let code = "`\\(x\\)` and\n```\n\\[y\\]\n```\n";
        assert_eq!(tex_delimiters(code), code, "code stays as written");
        assert_eq!(tex_delimiters("a \\\\(b) \\(c"), "a \\\\(b) \\(c", "an escaped backslash, and \\( without its \\)");
        assert_eq!(tex_delimiters("naïve \\(é\\)"), "naïve $é$");
        assert_eq!(runs("For \\(ax^2+bx+c=0\\), go")[1], ("ax^2+bx+c=0".into(), Math::Inline));
    }
}
