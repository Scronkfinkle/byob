//! The `grid` node: a spreadsheet. Column letters and row numbers, frozen rows and columns,
//! and only the cells on screen are drawn (rows and columns both), so a 1M × 16k sheet costs
//! what a small one does. The owner sends blocks of cells; when what shows isn't loaded the
//! grid asks for it (a `window` event). Selection with the mouse and the keyboard, a formula
//! bar, in-cell editing (typing, F2, double-click), copy as TSV, paste and clear events.
//!
//! Positions are kept in `f64` from the sheet's origin and turned into screen points only
//! near the screen, so a million rows down is as exact as the first.

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};

use byob_protocol::tree::{CellAlign, CellType, GridBlock, GridCell, GridEvent, GridRange, cell_name, column_name};
use egui::text::{LayoutJob, TextWrapping};
use egui::{Align2, Color32, EventFilter, FontId, Key, Modifiers, Pos2, Rect, Sense, Stroke, StrokeKind, TextFormat, pos2, vec2};
use serde_json::Value;

use super::{Cx, canvas};
use crate::theme::Tokens;

/// Height of a grid without its own when the site gives none.
const FALLBACK_HEIGHT: f32 = 360.0;
/// Scrollbar thickness.
const BAR: f32 = 10.0;
/// Rows and columns asked for beyond the screen (at least; one screen each way if more).
const OVERSCAN_ROWS: u32 = 50;
const OVERSCAN_COLS: u32 = 10;
/// Most cells one copy writes.
const MAX_COPY_CELLS: u64 = 200_000;
/// Most cells a clear shows as cleared before the reply.
const MAX_PENDING: u64 = 10_000;
/// Keys the grid (and its cell editor) keeps instead of egui moving focus.
const KEEP_KEYS: EventFilter = EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true };

/// What the node says.
pub(super) struct Grid<'a> {
    pub name: &'a str,
    pub rows: u32,
    pub cols: u32,
    pub blocks: &'a [GridBlock],
    pub frozen_rows: u32,
    pub frozen_cols: u32,
    pub widths: &'a BTreeMap<u32, f32>,
    pub default_width: f32,
    pub editable: bool,
    pub headers: bool,
    pub height: Option<f32>,
    pub selection: Option<GridRange>,
}

/// One grid's client state (in the [`super::TreeState`], by grid name, so it survives
/// replies and a new tree with the same grid).
#[derive(Default)]
pub(super) struct GridState {
    /// The tree pass it was last drawn in.
    pub(super) pass: u64,
    /// Scroll offsets of the scrolling part (points from its first row and column).
    scroll: (f64, f64),
    editing: Option<Editor>,
    /// Edits started (the editor's id).
    edits: u64,
    /// Formula bar text while it has focus, and the cell it is for.
    bar: Option<(u32, u32, String)>,
    /// Cells edited here and not answered: shown until a reply (or the cell changes).
    pending: HashMap<(u32, u32), Pending>,
    /// The window last asked for (row, col, rows, cols), until the tree changes.
    asked: Option<(u32, u32, u32, u32)>,
    /// What the blocks covered when last drawn.
    shape: u64,
    drag: Option<Drag>,
    /// The selection changed and no `select` event went yet.
    unsent: bool,
    /// Scroll the moving corner of the selection into view.
    reveal: bool,
}

impl GridState {
    pub(super) fn replied(&mut self) {
        self.pending.clear();
        self.asked = None;
    }
}

#[cfg(test)]
impl super::TreeState {
    /// Scroll a grid to points from its first scrolling row (as its scrollbar would).
    pub(super) fn scroll_grid(&mut self, name: &str, x: f64, y: f64) {
        if let Some(g) = self.grids.get_mut(name) {
            g.scroll = (x, y);
        }
    }
}

struct Editor {
    row: u32,
    col: u32,
    text: String,
    focused: bool,
}

struct Pending {
    text: String,
    /// The cell's text when edited: a tree with another one has answered.
    was: String,
}

#[derive(Clone, Copy)]
enum Drag {
    Cells,
    Cols,
    Rows,
    /// A scrollbar thumb, grabbed this far from its start.
    VBar(f32),
    HBar(f32),
}

/// Column positions with sparse widths: no per-column allocation.
struct Cols<'a> {
    widths: &'a BTreeMap<u32, f32>,
    dw: f64,
    n: u32,
}

impl Cols<'_> {
    fn w(&self, c: u32) -> f64 {
        self.widths.get(&c).map_or(self.dw, |w| f64::from(*w))
    }

    /// Left edge of column `c` from column 0.
    fn x(&self, c: u32) -> f64 {
        f64::from(c) * self.dw + self.widths.range(..c).map(|(_, w)| f64::from(*w) - self.dw).sum::<f64>()
    }

    /// The column at `x` from column 0 (clamped to the sheet).
    fn at(&self, x: f64) -> u32 {
        let (mut lo, mut hi) = (0u32, self.n);
        while lo + 1 < hi {
            let mid = lo + (hi - lo) / 2;
            if self.x(mid) <= x {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        lo
    }
}

/// Where things are this frame.
struct Geo<'a> {
    cols: Cols<'a>,
    rows: u32,
    fr: u32,
    fc: u32,
    row_h: f64,
    /// The grid's rect without the scrollbars.
    rect: Rect,
    /// Left of the frozen columns, of the scrolling ones; top of the frozen rows, of the
    /// scrolling ones.
    fx: f32,
    sx: f32,
    fy: f32,
    sy: f32,
    /// Width of the frozen columns.
    fw: f64,
    content_w: f64,
    content_h: f64,
    scroll: (f64, f64),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hit {
    Header,
    At(u32),
}

impl Geo<'_> {
    fn view_w(&self) -> f64 {
        f64::from((self.rect.right() - self.sx).max(0.0))
    }

    fn view_h(&self) -> f64 {
        f64::from((self.rect.bottom() - self.sy).max(0.0))
    }

    fn max_scroll(&self) -> (f64, f64) {
        ((self.content_w - self.view_w()).max(0.0), (self.content_h - self.view_h()).max(0.0))
    }

    fn clamp(&mut self) {
        let (mx, my) = self.max_scroll();
        self.scroll = (self.scroll.0.clamp(0.0, mx), self.scroll.1.clamp(0.0, my));
    }

    fn col_x(&self, c: u32) -> f32 {
        if c < self.fc { self.fx + self.cols.x(c) as f32 } else { self.sx + (self.cols.x(c) - self.fw - self.scroll.0) as f32 }
    }

    fn row_y(&self, r: u32) -> f32 {
        if r < self.fr { self.fy + (f64::from(r) * self.row_h) as f32 } else { self.sy + (f64::from(r - self.fr) * self.row_h - self.scroll.1) as f32 }
    }

    fn cell_rect(&self, r: u32, c: u32) -> Rect {
        Rect::from_min_size(pos2(self.col_x(c), self.row_y(r)), vec2(self.cols.w(c) as f32, self.row_h as f32))
    }

    fn row_at(&self, y: f32) -> Hit {
        if y < self.fy {
            return Hit::Header;
        }
        let last = self.rows.saturating_sub(1);
        if y < self.sy && self.fr > 0 {
            return Hit::At(((f64::from(y - self.fy) / self.row_h) as u32).min(self.fr - 1));
        }
        let off = (f64::from(y - self.sy) + self.scroll.1).max(0.0);
        Hit::At(self.fr.saturating_add((off / self.row_h) as u32).min(last))
    }

    fn col_at(&self, x: f32) -> Hit {
        if x < self.fx {
            return Hit::Header;
        }
        if x < self.sx && self.fc > 0 {
            return Hit::At(self.cols.at(f64::from(x - self.fx)).min(self.fc - 1));
        }
        let off = (f64::from(x - self.sx) + self.scroll.0).max(0.0) + self.fw;
        Hit::At(self.cols.at(off).max(self.fc).min(self.cols.n.saturating_sub(1)))
    }

    /// Rows on screen: the frozen ones, then the scrolling ones.
    fn visible_rows(&self) -> (std::ops::Range<u32>, std::ops::Range<u32>) {
        let first = self.fr.saturating_add((self.scroll.1 / self.row_h) as u32).min(self.rows);
        let n = (self.view_h() / self.row_h).ceil() as u32 + 1;
        (0..self.fr, first..first.saturating_add(n).min(self.rows))
    }

    fn visible_cols(&self) -> (std::ops::Range<u32>, std::ops::Range<u32>) {
        let first = self.cols.at(self.fw + self.scroll.0).max(self.fc);
        let mut end = first;
        let mut x = self.cols.x(first) - self.fw - self.scroll.0;
        while end < self.cols.n && x < self.view_w() {
            x += self.cols.w(end);
            end += 1;
        }
        (0..self.fc, first..end)
    }

    /// Scroll so the cell shows (frozen rows and columns always do).
    fn reveal(&mut self, r: u32, c: u32) {
        if r >= self.fr {
            let y = f64::from(r - self.fr) * self.row_h;
            if y < self.scroll.1 {
                self.scroll.1 = y;
            } else if y + self.row_h > self.scroll.1 + self.view_h() {
                self.scroll.1 = y + self.row_h - self.view_h();
            }
        }
        if c >= self.fc {
            let x = self.cols.x(c) - self.fw;
            let w = self.cols.w(c);
            if x < self.scroll.0 {
                self.scroll.0 = x;
            } else if x + w > self.scroll.0 + self.view_w() {
                self.scroll.0 = x + w - self.view_w();
            }
        }
        self.clamp();
    }
}

/// The cell at (`r`, `c`): `None` if not loaded, `Some(None)` if empty.
fn lookup(blocks: &[GridBlock], r: u32, c: u32) -> Option<Option<&GridCell>> {
    blocks.iter().find_map(|b| b.get(r, c))
}

fn covered(blocks: &[GridBlock], rows: &[std::ops::Range<u32>], cols: &[std::ops::Range<u32>]) -> bool {
    rows.iter().cloned().flatten().all(|r| cols.iter().cloned().flatten().all(|c| blocks.iter().any(|b| b.covers(r, c))))
}

fn shape_of(g: &Grid<'_>) -> u64 {
    let mut h = std::hash::DefaultHasher::new();
    (g.rows, g.cols).hash(&mut h);
    for b in g.blocks {
        (b.row, b.col, b.height(), b.width()).hash(&mut h);
    }
    h.finish()
}

fn range_json(s: GridRange) -> Value {
    serde_json::json!({"row": s.row, "col": s.col, "to_row": s.to_row, "to_col": s.to_col})
}

/// A selection from a value (a reply's, say): `row` and `col`, the other corner optional.
fn range_of(v: &Value) -> Option<GridRange> {
    let n = |k: &str| v.get(k).and_then(Value::as_u64).map(|n| u32::try_from(n).unwrap_or(u32::MAX));
    let (row, col) = (n("row")?, n("col")?);
    Some(GridRange { row, col, to_row: n("to_row").unwrap_or(row), to_col: n("to_col").unwrap_or(col) })
}

/// `B3`, `B3:D7`, `B:D` (whole columns), `3:7` (whole rows).
fn range_name(s: GridRange, rows: u32, cols: u32) -> String {
    let (t, l, b, r) = s.bounds();
    let all_rows = t == 0 && b + 1 >= rows && rows > 1;
    let all_cols = l == 0 && r + 1 >= cols && cols > 1;
    if all_rows && !all_cols {
        return format!("{}:{}", column_name(l), column_name(r));
    }
    if all_cols && !all_rows {
        return format!("{}:{}", u64::from(t) + 1, u64::from(b) + 1);
    }
    if (t, l) == (b, r) { cell_name(t, l) } else { format!("{}:{}", cell_name(t, l), cell_name(b, r)) }
}

/// What to show for a cell: the pending edit, else its value.
fn shown<'c>(gs: &'c GridState, cell: Option<&'c GridCell>, r: u32, c: u32) -> &'c str {
    let v = cell.map_or("", |c| c.v.as_str());
    match gs.pending.get(&(r, c)) {
        Some(p) if p.was == v => &p.text,
        _ => v,
    }
}

/// What editing a cell starts with (and the formula bar shows).
fn input_of(gs: &GridState, g: &Grid<'_>, r: u32, c: u32) -> String {
    let cell = lookup(g.blocks, r, c).flatten();
    let v = cell.map_or("", |c| c.v.as_str());
    match gs.pending.get(&(r, c)) {
        Some(p) if p.was == v => p.text.clone(),
        _ => cell.map(|c| c.input().to_owned()).unwrap_or_default(),
    }
}

pub(super) fn show(ui: &mut egui::Ui, cx: &mut Cx<'_, '_>, g: Grid<'_>, id: egui::Id) {
    let mut gs = cx.st.grids.remove(g.name).unwrap_or_default();
    gs.pass = cx.st.pass;
    let shape = shape_of(&g);
    if shape != gs.shape {
        gs.shape = shape;
        gs.asked = None;
    }
    // The selection, a field like any other: the tree's only when the tree changes it.
    let tree_sel = range_json(g.selection.unwrap_or_default());
    let v = cx.st.field(g.name, || tree_sel.clone(), |v| *v == tree_sel);
    let mut sel = clamp_range(range_of(v).unwrap_or_default(), g.rows, g.cols);
    let before = sel;
    let mut run = Run { g: &g, gs: &mut gs, sel: &mut sel, id, events: Vec::new() };
    run.frame(ui, cx);
    let events = std::mem::take(&mut run.events);
    if sel != before {
        cx.st.values.insert(g.name.to_owned(), range_json(sel));
        gs.unsent = true;
    } else if let Some(v) = cx.st.values.get(g.name)
        && range_of(v) != Some(sel)
    {
        cx.st.values.insert(g.name.to_owned(), range_json(sel));
    }
    for e in events {
        cx.st.fire_grid(e);
    }
    if gs.unsent && gs.drag.is_none() {
        gs.unsent = false;
        let (t, l, b, r) = (sel.row, sel.col, sel.to_row, sel.to_col);
        cx.st.fire_grid(GridEvent { name: g.name.to_owned(), op: "select".into(), row: t, col: l, to_row: Some(b), to_col: Some(r), ..GridEvent::default() });
    }
    cx.st.grids.insert(g.name.to_owned(), gs);
}

fn clamp_range(s: GridRange, rows: u32, cols: u32) -> GridRange {
    let (lr, lc) = (rows.saturating_sub(1), cols.saturating_sub(1));
    GridRange { row: s.row.min(lr), col: s.col.min(lc), to_row: s.to_row.min(lr), to_col: s.to_col.min(lc) }
}

/// One frame of one grid.
struct Run<'a, 'g> {
    g: &'a Grid<'g>,
    gs: &'a mut GridState,
    sel: &'a mut GridRange,
    id: egui::Id,
    events: Vec<GridEvent>,
}

impl Run<'_, '_> {
    fn event(&self, op: &str, row: u32, col: u32) -> GridEvent {
        GridEvent { name: self.g.name.to_owned(), op: op.to_owned(), row, col, ..GridEvent::default() }
    }

    fn frame(&mut self, ui: &mut egui::Ui, cx: &mut Cx<'_, '_>) {
        let t = cx.t;
        let before = *self.sel;
        let font = egui::TextStyle::Body.resolve(ui.style());
        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 2.0 * t.pad(3.0);
        let bar_h = if cx.st.compact { 0.0 } else { self.formula_bar(ui, t, &font) };
        let g = self.g;
        let cols = Cols { widths: g.widths, dw: f64::from(g.default_width), n: g.cols };
        // Row numbers as wide as the biggest.
        let hx = if g.headers {
            let digits = "8".repeat(u64::from(g.rows).to_string().len().max(2));
            ui.painter().layout_no_wrap(digits, font.clone(), t.text).size().x + 2.0 * t.pad(6.0)
        } else {
            0.0
        };
        let hy = if g.headers { row_h } else { 0.0 };
        let fw = cols.x(g.frozen_cols);
        let content_w = cols.x(g.cols) - fw;
        let content_h = f64::from(g.rows - g.frozen_rows.min(g.rows)) * f64::from(row_h);
        let fh = g.frozen_rows as f32 * row_h;
        // As tall as asked (or as the site gives), never taller than the sheet.
        let want_h = g.height.or(cx.st.fill_height.map(|h| h - bar_h)).unwrap_or(FALLBACK_HEIGHT).max(3.0 * row_h);
        let natural_h = hy as f64 + fh as f64 + content_h + f64::from(BAR) + 2.0;
        let natural_w = f64::from(hx) + fw + content_w + f64::from(BAR) + 2.0;
        let size = vec2(ui.available_width().min(natural_w as f32).max(60.0), want_h.min(natural_h as f32));
        let (outer, _) = ui.allocate_exact_size(size, Sense::hover());
        let inner = outer.shrink(1.0);
        let view_w = f64::from(inner.width() - hx) - fw;
        let view_h = f64::from(inner.height() - hy - fh);
        let vbar = content_h > view_h;
        let hbar = content_w > view_w - if vbar { f64::from(BAR) } else { 0.0 };
        let vbar = vbar || (hbar && content_h > view_h - f64::from(BAR));
        let rect = Rect::from_min_max(inner.min, pos2(inner.right() - if vbar { BAR } else { 0.0 }, inner.bottom() - if hbar { BAR } else { 0.0 }));
        let fx = rect.left() + hx;
        let fy = rect.top() + hy;
        let mut geo = Geo {
            cols,
            rows: g.rows,
            fr: g.frozen_rows,
            fc: g.frozen_cols,
            row_h: f64::from(row_h),
            rect,
            fx,
            sx: fx + fw as f32,
            fy,
            sy: fy + fh,
            fw,
            content_w,
            content_h,
            scroll: self.gs.scroll,
        };
        geo.clamp();

        let resp = ui.interact(rect, self.id, Sense::click_and_drag());
        let focused = ui.memory(|m| m.has_focus(self.id));
        if focused {
            ui.memory_mut(|m| m.set_focus_lock_filter(self.id, KEEP_KEYS));
        }
        if self.gs.editing.is_some() {
            self.editor_keys(ui);
        } else if focused {
            self.keys(ui, &geo);
        }
        self.pointer(ui, &resp, &mut geo);
        self.scrollbars(ui, &mut geo, vbar, hbar, t);
        if resp.contains_pointer() || ui.rect_contains_pointer(outer) {
            ui.input_mut(|i| {
                let d = i.smooth_scroll_delta;
                let (mx, my) = geo.max_scroll();
                if my > 0.0 && d.y != 0.0 {
                    geo.scroll.1 -= f64::from(d.y);
                    i.smooth_scroll_delta.y = 0.0;
                }
                if mx > 0.0 && d.x != 0.0 {
                    geo.scroll.0 -= f64::from(d.x);
                    i.smooth_scroll_delta.x = 0.0;
                }
            });
        }
        if std::mem::take(&mut self.gs.reveal) {
            geo.reveal(self.sel.to_row, self.sel.to_col);
        }
        geo.clamp();
        // The formula bar (drawn first) shows a moved selection on the next frame.
        if geo.scroll != self.gs.scroll || *self.sel != before {
            ui.ctx().request_repaint();
        }
        self.gs.scroll = geo.scroll;

        let p = ui.painter_at(outer);
        p.rect_filled(inner, 0.0, t.field_bg);
        self.paint(ui, cx, &geo, &font);
        p.rect_stroke(outer, t.round(4.0), Stroke::new(1.0, t.border), StrokeKind::Inside);
        self.cell_editor(ui, &geo, &font, t);
        self.ask_window(&geo);
    }

    /// The formula bar: the selection's name and the active cell's input, editable. Its
    /// height.
    fn formula_bar(&mut self, ui: &mut egui::Ui, t: &Tokens, font: &FontId) -> f32 {
        let g = self.g;
        let bar_id = self.id.with("bar");
        let (r, c) = (self.sel.row, self.sel.col);
        let input = input_of(self.gs, g, r, c);
        let focused = ui.memory(|m| m.has_focus(bar_id));
        // Some(commit?) when editing in the bar ends.
        let mut done = None;
        if focused {
            ui.memory_mut(|m| m.set_focus_lock_filter(bar_id, KEEP_KEYS));
            let (enter, esc) = ui.input_mut(|i| (i.consume_key(Modifiers::NONE, Key::Enter), i.consume_key(Modifiers::NONE, Key::Escape)));
            done = if enter {
                Some(true)
            } else if esc {
                Some(false)
            } else {
                None
            };
            if self.gs.bar.is_none() {
                self.gs.bar = Some((r, c, input.clone()));
            }
        }
        let label = range_name(*self.sel, g.rows, g.cols);
        let look = Look { font: FontId::monospace(font.size * 0.9), color: t.text_dim, align: Align2::CENTER_CENTER, italic: false };
        let mut shown = input;
        let resp = ui
            .horizontal(|ui| {
                let (lr, _) = ui.allocate_exact_size(vec2(84.0, ui.spacing().interact_size.y), Sense::hover());
                ui.painter().rect(lr, t.round(4.0), t.code_bg, Stroke::new(1.0, t.border), StrokeKind::Inside);
                paint_text(ui.painter(), lr, &label, look, 4.0);
                let text = match &mut self.gs.bar {
                    Some((_, _, text)) if focused => text,
                    _ => &mut shown,
                };
                let edit = egui::TextEdit::singleline(text).id(bar_id).font(font.clone()).desired_width(ui.available_width()).interactive(g.editable);
                ui.add(edit)
            })
            .inner;
        if done.is_none() && resp.lost_focus() {
            // Clicked away: what was typed counts.
            done = Some(true);
        }
        match done {
            Some(commit) => {
                if let Some((br, bc, text)) = self.gs.bar.take()
                    && commit
                {
                    self.commit(br, bc, text);
                }
                if focused {
                    ui.memory_mut(|m| m.request_focus(self.id));
                }
            }
            None if !focused => self.gs.bar = None,
            None => {}
        }
        resp.rect.height() + ui.spacing().item_spacing.y
    }

    /// An edit typed into (`r`, `c`): shown until the reply, sent if it changed something.
    fn commit(&mut self, r: u32, c: u32, text: String) {
        if !self.g.editable {
            return;
        }
        let was_input = input_of(self.gs, self.g, r, c);
        if text == was_input {
            return;
        }
        let was = lookup(self.g.blocks, r, c).flatten().map(|c| c.v.clone()).unwrap_or_default();
        self.gs.pending.insert((r, c), Pending { text: text.clone(), was });
        let e = GridEvent { value: Some(text), ..self.event("edit", r, c) };
        self.events.push(e);
    }

    fn start_edit(&mut self, text: String) {
        if self.g.editable {
            self.gs.editing = Some(Editor { row: self.sel.row, col: self.sel.col, text, focused: false });
            self.gs.edits = self.gs.edits.wrapping_add(1);
            self.gs.reveal = true;
            *self.sel = GridRange::cell(self.sel.row, self.sel.col);
        }
    }

    /// Move the selection: the active cell (collapsing it) or, with `extend`, its other corner.
    fn go(&mut self, dr: i64, dc: i64, extend: bool) {
        let clamp = |v: u32, d: i64, n: u32| (i64::from(v) + d).clamp(0, i64::from(n.saturating_sub(1))) as u32;
        let (rows, cols) = (self.g.rows, self.g.cols);
        if extend {
            self.sel.to_row = clamp(self.sel.to_row, dr, rows);
            self.sel.to_col = clamp(self.sel.to_col, dc, cols);
        } else {
            *self.sel = GridRange::cell(clamp(self.sel.row, dr, rows), clamp(self.sel.col, dc, cols));
        }
        self.gs.reveal = true;
    }

    fn go_to(&mut self, r: u32, c: u32, extend: bool) {
        let (dr, dc) = if extend { (self.sel.to_row, self.sel.to_col) } else { (self.sel.row, self.sel.col) };
        self.go(i64::from(r) - i64::from(dr), i64::from(c) - i64::from(dc), extend);
    }

    /// Keys while the grid has focus (and no cell is being edited).
    fn keys(&mut self, ui: &egui::Ui, geo: &Geo<'_>) {
        let events = ui.input(|i| i.events.clone());
        let page = ((geo.view_h() / geo.row_h) as i64 - 1).max(1);
        let (last_r, last_c) = (self.g.rows.saturating_sub(1), self.g.cols.saturating_sub(1));
        for e in events {
            match e {
                egui::Event::Key { key, pressed: true, modifiers: m, .. } => {
                    let cmd = m.command || m.ctrl;
                    let (s, sr, sc) = (m.shift, self.sel.to_row, self.sel.to_col);
                    match key {
                        Key::ArrowUp if cmd => self.go_to(0, if s { sc } else { self.sel.col }, s),
                        Key::ArrowDown if cmd => self.go_to(last_r, if s { sc } else { self.sel.col }, s),
                        Key::ArrowLeft if cmd => self.go_to(if s { sr } else { self.sel.row }, 0, s),
                        Key::ArrowRight if cmd => self.go_to(if s { sr } else { self.sel.row }, last_c, s),
                        Key::ArrowUp => self.go(-1, 0, s),
                        Key::ArrowDown => self.go(1, 0, s),
                        Key::ArrowLeft => self.go(0, -1, s),
                        Key::ArrowRight => self.go(0, 1, s),
                        Key::Tab => self.go(0, if s { -1 } else { 1 }, false),
                        Key::Enter => self.go(if s { -1 } else { 1 }, 0, false),
                        Key::PageUp => self.go(-page, 0, s),
                        Key::PageDown => self.go(page, 0, s),
                        Key::Home if cmd => self.go_to(0, 0, s),
                        Key::End if cmd => self.go_to(last_r, last_c, s),
                        Key::Home => self.go_to(if s { sr } else { self.sel.row }, 0, s),
                        Key::End => self.go_to(if s { sr } else { self.sel.row }, last_c, s),
                        Key::F2 => {
                            let input = input_of(self.gs, self.g, self.sel.row, self.sel.col);
                            self.start_edit(input);
                        }
                        Key::Delete | Key::Backspace => self.clear(),
                        _ => {}
                    }
                }
                egui::Event::Text(s) if !s.is_empty() && !s.chars().any(char::is_control) => {
                    if !ui.input(|i| i.modifiers.command || i.modifiers.ctrl) {
                        self.start_edit(s);
                    }
                }
                egui::Event::Copy => ui.ctx().copy_text(self.tsv()),
                egui::Event::Cut => {
                    ui.ctx().copy_text(self.tsv());
                    self.clear();
                }
                egui::Event::Paste(text) if self.g.editable => {
                    let (t, l, _, _) = self.sel.bounds();
                    let e = GridEvent { text: Some(text), ..self.event("paste", t, l) };
                    self.events.push(e);
                }
                _ => {}
            }
            if self.gs.editing.is_some() {
                break;
            }
        }
    }

    fn clear(&mut self) {
        if !self.g.editable {
            return;
        }
        let (t, l, b, r) = self.sel.bounds();
        let e = GridEvent { to_row: Some(b), to_col: Some(r), ..self.event("clear", t, l) };
        self.events.push(e);
        // Shown empty until the reply, if not too many.
        if u64::from(b - t + 1) * u64::from(r - l + 1) <= MAX_PENDING {
            for row in t..=b {
                for col in l..=r {
                    if let Some(Some(cell)) = lookup(self.g.blocks, row, col) {
                        self.gs.pending.insert((row, col), Pending { text: String::new(), was: cell.v.clone() });
                    }
                }
            }
        }
    }

    /// The selection as tab-separated shown values (cells not loaded are empty), cut to
    /// what is loaded and to [`MAX_COPY_CELLS`].
    fn tsv(&self) -> String {
        let (t, l, b, r) = self.sel.bounds();
        let max_r = self.g.blocks.iter().map(|k| k.row.saturating_add(k.height())).max().unwrap_or(0);
        let max_c = self.g.blocks.iter().map(|k| k.col.saturating_add(k.width())).max().unwrap_or(0);
        let b = b.min(max_r.saturating_sub(1)).max(t);
        let r = r.min(max_c.saturating_sub(1)).max(l);
        let width = u64::from(r - l + 1);
        let b = b.min(t.saturating_add(u32::try_from(MAX_COPY_CELLS / width).unwrap_or(u32::MAX).max(1) - 1));
        let mut out = String::new();
        for row in t..=b {
            if row > t {
                out.push('\n');
            }
            for col in l..=r {
                if col > l {
                    out.push('\t');
                }
                let cell = lookup(self.g.blocks, row, col).flatten();
                out.push_str(&shown(self.gs, cell, row, col).replace(['\t', '\n', '\r'], " "));
            }
        }
        out
    }

    /// Enter, Tab and Escape in the cell editor.
    /// The cell editor's id: a new one for each edit, so its cursor starts afresh (at the end).
    fn edit_id(&self) -> egui::Id {
        self.id.with(("edit", self.gs.edits))
    }

    fn editor_keys(&mut self, ui: &mut egui::Ui) {
        let edit_id = self.edit_id();
        if !ui.memory(|m| m.has_focus(edit_id)) {
            return;
        }
        ui.memory_mut(|m| m.set_focus_lock_filter(edit_id, KEEP_KEYS));
        let (enter, up, tab, back, esc) = ui.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::Enter),
                i.consume_key(Modifiers::SHIFT, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::Tab),
                i.consume_key(Modifiers::SHIFT, Key::Tab),
                i.consume_key(Modifiers::NONE, Key::Escape),
            )
        });
        if !(enter || up || tab || back || esc) {
            return;
        }
        if let Some(e) = self.gs.editing.take()
            && !esc
        {
            self.commit(e.row, e.col, e.text);
            let (dr, dc) = match (enter, up, tab) {
                (true, _, _) => (1, 0),
                (_, true, _) => (-1, 0),
                (_, _, true) => (0, 1),
                _ => (0, -1),
            };
            self.go(dr, dc, false);
        }
        ui.memory_mut(|m| m.request_focus(self.id));
    }

    /// Clicks and drags on the cells and headers.
    fn pointer(&mut self, ui: &egui::Ui, resp: &egui::Response, geo: &mut Geo<'_>) {
        let (pressed, down, pos) = ui.input(|i| (i.pointer.primary_pressed(), i.pointer.primary_down(), i.pointer.interact_pos()));
        let shift = ui.input(|i| i.modifiers.shift);
        let editor_rect = self.gs.editing.as_ref().map(|e| geo.cell_rect(e.row, e.col));
        if pressed
            && let Some(p) = pos
            && resp.contains_pointer()
            && !editor_rect.is_some_and(|r| r.contains(p))
        {
            if let Some(e) = self.gs.editing.take() {
                self.commit(e.row, e.col, e.text);
            }
            ui.memory_mut(|m| m.request_focus(self.id));
            let (last_r, last_c) = (self.g.rows.saturating_sub(1), self.g.cols.saturating_sub(1));
            match (geo.row_at(p.y), geo.col_at(p.x)) {
                (Hit::Header, Hit::Header) => *self.sel = GridRange { row: 0, col: 0, to_row: last_r, to_col: last_c },
                (Hit::Header, Hit::At(c)) => {
                    let from = if shift { self.sel.col } else { c };
                    *self.sel = GridRange { row: 0, col: from, to_row: last_r, to_col: c };
                    self.gs.drag = Some(Drag::Cols);
                }
                (Hit::At(r), Hit::Header) => {
                    let from = if shift { self.sel.row } else { r };
                    *self.sel = GridRange { row: from, col: 0, to_row: r, to_col: last_c };
                    self.gs.drag = Some(Drag::Rows);
                }
                (Hit::At(r), Hit::At(c)) => {
                    if shift {
                        self.sel.to_row = r;
                        self.sel.to_col = c;
                    } else {
                        *self.sel = GridRange::cell(r, c);
                    }
                    self.gs.drag = Some(Drag::Cells);
                }
            }
        }
        if let (Some(d @ (Drag::Cells | Drag::Cols | Drag::Rows)), true, Some(p)) = (self.gs.drag, down, pos) {
            // Past an edge: scroll that way.
            let r = geo.rect;
            if p.y > r.bottom() {
                geo.scroll.1 += geo.row_h;
            } else if p.y < geo.fy {
                geo.scroll.1 -= geo.row_h;
            }
            if p.x > r.right() {
                geo.scroll.0 += 40.0;
            } else if p.x < geo.fx {
                geo.scroll.0 -= 40.0;
            }
            geo.clamp();
            let q = pos2(p.x.clamp(geo.fx, r.right() - 1.0), p.y.clamp(geo.fy, r.bottom() - 1.0));
            if let (Hit::At(row), Hit::At(col)) = (geo.row_at(q.y), geo.col_at(q.x)) {
                match d {
                    Drag::Cells => {
                        self.sel.to_row = row;
                        self.sel.to_col = col;
                    }
                    Drag::Cols => self.sel.to_col = col,
                    _ => self.sel.to_row = row,
                }
            }
        }
        if !down && matches!(self.gs.drag, Some(Drag::Cells | Drag::Cols | Drag::Rows)) {
            self.gs.drag = None;
        }
        if resp.double_clicked()
            && let Some(p) = pos
            && let (Hit::At(r), Hit::At(c)) = (geo.row_at(p.y), geo.col_at(p.x))
        {
            *self.sel = GridRange::cell(r, c);
            let input = input_of(self.gs, self.g, r, c);
            self.start_edit(input);
        }
    }

    fn scrollbars(&mut self, ui: &egui::Ui, geo: &mut Geo<'_>, vbar: bool, hbar: bool, t: &Tokens) {
        let r = geo.rect;
        let (mx, my) = geo.max_scroll();
        let (down, pos) = ui.input(|i| (i.pointer.primary_down(), i.pointer.interact_pos()));
        let thumb_color = t.text_faint.gamma_multiply(0.6);
        if vbar {
            let track = Rect::from_min_max(pos2(r.right(), geo.sy), pos2(r.right() + BAR, r.bottom()));
            let len = track.height();
            let view = geo.view_h();
            let th = ((view / (geo.content_h.max(1.0))) as f32 * len).clamp(20.0_f32.min(len), len);
            let at = if my > 0.0 { (geo.scroll.1 / my) as f32 * (len - th) } else { 0.0 };
            let thumb = Rect::from_min_size(pos2(track.left() + 2.0, track.top() + at), vec2(BAR - 4.0, th));
            let resp = ui.interact(track, self.id.with("vbar"), Sense::click_and_drag());
            if resp.is_pointer_button_down_on()
                && self.gs.drag.is_none()
                && let Some(p) = pos
            {
                let grab = if thumb.contains(p) { p.y - thumb.top() } else { th / 2.0 };
                self.gs.drag = Some(Drag::VBar(grab));
            }
            if let (Some(Drag::VBar(grab)), true, Some(p)) = (self.gs.drag, down, pos)
                && len > th
            {
                geo.scroll.1 = f64::from(((p.y - track.top() - grab) / (len - th)).clamp(0.0, 1.0)) * my;
            }
            ui.painter().rect_filled(thumb, 3.0, thumb_color);
        }
        if hbar {
            let track = Rect::from_min_max(pos2(geo.sx, r.bottom()), pos2(r.right(), r.bottom() + BAR));
            let len = track.width();
            let view = geo.view_w();
            let tw = ((view / (geo.content_w.max(1.0))) as f32 * len).clamp(20.0_f32.min(len), len);
            let at = if mx > 0.0 { (geo.scroll.0 / mx) as f32 * (len - tw) } else { 0.0 };
            let thumb = Rect::from_min_size(pos2(track.left() + at, track.top() + 2.0), vec2(tw, BAR - 4.0));
            let resp = ui.interact(track, self.id.with("hbar"), Sense::click_and_drag());
            if resp.is_pointer_button_down_on()
                && self.gs.drag.is_none()
                && let Some(p) = pos
            {
                let grab = if thumb.contains(p) { p.x - thumb.left() } else { tw / 2.0 };
                self.gs.drag = Some(Drag::HBar(grab));
            }
            if let (Some(Drag::HBar(grab)), true, Some(p)) = (self.gs.drag, down, pos)
                && len > tw
            {
                geo.scroll.0 = f64::from(((p.x - track.left() - grab) / (len - tw)).clamp(0.0, 1.0)) * mx;
            }
            ui.painter().rect_filled(thumb, 3.0, thumb_color);
        }
        if !down && matches!(self.gs.drag, Some(Drag::VBar(_) | Drag::HBar(_))) {
            self.gs.drag = None;
        }
    }

    fn paint(&self, ui: &egui::Ui, cx: &Cx<'_, '_>, geo: &Geo<'_>, font: &FontId) {
        let t = cx.t;
        let (fr, sr) = geo.visible_rows();
        let (fc, sc) = geo.visible_cols();
        let r = geo.rect;
        let main = Rect::from_min_max(pos2(geo.sx, geo.sy), r.max);
        let top = Rect::from_min_max(pos2(geo.sx, geo.fy), pos2(r.right(), geo.sy));
        let left = Rect::from_min_max(pos2(geo.fx, geo.sy), pos2(geo.sx, r.bottom()));
        let corner = Rect::from_min_max(pos2(geo.fx, geo.fy), pos2(geo.sx, geo.sy));
        let cells = Cells { t, geo, font, bold: &cx.bold, gs: self.gs, g: self.g, sel: *self.sel };
        for (clip, rows, cols) in [(main, &sr, &sc), (top, &fr, &sc), (left, &sr, &fc), (corner, &fr, &fc)] {
            if clip.width() > 0.0 && clip.height() > 0.0 {
                cells.region(&ui.painter_at(clip), rows.clone(), cols.clone());
            }
        }
        // The lines under the frozen parts.
        let edge = Stroke::new(1.0, t.border);
        if geo.fr > 0 {
            ui.painter_at(r).hline(r.left()..=r.right(), geo.sy - 0.5, edge);
        }
        if geo.fc > 0 {
            ui.painter_at(r).vline(geo.sx - 0.5, r.top()..=r.bottom(), edge);
        }
        if self.g.headers {
            cells.headers(ui, [fc, sc], [fr, sr]);
        }
    }

    /// The in-cell editor, over its cell.
    fn cell_editor(&mut self, ui: &mut egui::Ui, geo: &Geo<'_>, font: &FontId, t: &Tokens) {
        let edit_id = self.edit_id();
        let Some(e) = self.gs.editing.as_mut() else { return };
        let cell = geo.cell_rect(e.row, e.col);
        let clip = Rect::from_min_max(pos2(if e.col < geo.fc { geo.fx } else { geo.sx }, if e.row < geo.fr { geo.fy } else { geo.sy }), geo.rect.max);
        let at = cell.intersect(clip);
        if at.width() < 4.0 || at.height() < 4.0 {
            return;
        }
        let edit = egui::TextEdit::singleline(&mut e.text)
            .id(edit_id)
            .font(font.clone())
            .frame(egui::Frame::new())
            .background_color(t.field_bg)
            .margin(vec2(t.pad(4.0) - 1.0, 1.0))
            .event_filter(KEEP_KEYS);
        let resp = ui.put(at, edit);
        ui.painter().rect_stroke(at, 0.0, Stroke::new(2.0, t.accent), StrokeKind::Inside);
        if !e.focused {
            resp.request_focus();
            e.focused = true;
        } else if resp.lost_focus() {
            // Focus went elsewhere (a click outside the grid): commit.
            if let Some(e) = self.gs.editing.take() {
                self.commit(e.row, e.col, e.text);
            }
        }
    }

    /// Ask for the cells that show but aren't loaded (once per range, until the tree changes).
    fn ask_window(&mut self, geo: &Geo<'_>) {
        let (fr, sr) = geo.visible_rows();
        let (fc, sc) = geo.visible_cols();
        if covered(self.g.blocks, &[fr.clone(), sr.clone()], &[fc.clone(), sc.clone()]) {
            return;
        }
        // The scrolling part (the owner adds the frozen strips itself).
        let in_asked = |a: (u32, u32, u32, u32)| {
            let rows_in = sr.is_empty() || (sr.start >= a.0 && sr.end <= a.0.saturating_add(a.2));
            let cols_in = sc.is_empty() || (sc.start >= a.1 && sc.end <= a.1.saturating_add(a.3));
            rows_in && cols_in
        };
        if self.gs.asked.is_some_and(in_asked) {
            return;
        }
        let (or, oc) = (u32::try_from(sr.len()).unwrap_or(0).max(OVERSCAN_ROWS), u32::try_from(sc.len()).unwrap_or(0).max(OVERSCAN_COLS));
        let r0 = if sr.is_empty() { 0 } else { sr.start.saturating_sub(or) };
        let r1 = sr.end.max(fr.end).saturating_add(or).min(self.g.rows);
        let c0 = if sc.is_empty() { 0 } else { sc.start.saturating_sub(oc) };
        let c1 = sc.end.max(fc.end).saturating_add(oc).min(self.g.cols);
        let w = (r0, c0, r1.saturating_sub(r0), c1.saturating_sub(c0));
        self.gs.asked = Some(w);
        let e = GridEvent { rows: Some(w.2), cols: Some(w.3), ..self.event("window", w.0, w.1) };
        self.events.push(e);
    }
}

/// Painting cells and headers.
struct Cells<'a> {
    t: &'a Tokens,
    geo: &'a Geo<'a>,
    font: &'a FontId,
    bold: &'a egui::FontFamily,
    gs: &'a GridState,
    g: &'a Grid<'a>,
    sel: GridRange,
}

impl Cells<'_> {
    fn region(&self, p: &egui::Painter, rows: std::ops::Range<u32>, cols: std::ops::Range<u32>) {
        let t = self.t;
        let pad = t.pad(4.0);
        let grid_line = Stroke::new(1.0, t.separator);
        let not_loaded = t.table_stripe.gamma_multiply(0.7);
        let sel_fill = t.selection.gamma_multiply(0.55);
        let editing = self.gs.editing.as_ref().map(|e| (e.row, e.col));
        for r in rows.clone() {
            for c in cols.clone() {
                let rect = self.geo.cell_rect(r, c);
                let got = lookup(self.g.blocks, r, c);
                let cell = got.flatten();
                match (got, cell.and_then(|c| c.fill.as_deref())) {
                    (None, _) => {
                        p.rect_filled(rect, 0.0, not_loaded);
                    }
                    (_, Some(fill)) => {
                        p.rect_filled(rect, 0.0, canvas::color(t, Some(fill), Color32::TRANSPARENT));
                    }
                    _ => {}
                }
                if self.sel.contains(r, c) && (self.sel.row, self.sel.col) != (r, c) {
                    p.rect_filled(rect, 0.0, sel_fill);
                }
                if got.is_none() || editing == Some((r, c)) {
                    continue;
                }
                let text = shown(self.gs, cell, r, c);
                if text.is_empty() {
                    continue;
                }
                let ty = cell.map_or(CellType::Text, |c| c.t);
                let align = match (cell.and_then(|c| c.align), ty) {
                    (Some(CellAlign::Left), _) => Align2::LEFT_CENTER,
                    (Some(CellAlign::Center), _) | (None, CellType::Bool) => Align2::CENTER_CENTER,
                    (Some(CellAlign::Right), _) | (None, CellType::Number) => Align2::RIGHT_CENTER,
                    _ => Align2::LEFT_CENTER,
                };
                let default = if ty == CellType::Error { t.danger } else { t.text };
                let color = canvas::color(t, cell.and_then(|c| c.color.as_deref()), default);
                let bold = cell.is_some_and(|c| c.bold);
                let font = if bold { FontId::new(self.font.size, self.bold.clone()) } else { self.font.clone() };
                paint_text(p, rect, text, Look { font, color, align, italic: cell.is_some_and(|c| c.italic) }, pad);
            }
        }
        // Grid lines.
        if let (Some(first), Some(last)) = (rows.clone().next(), rows.clone().last()) {
            let (x0, x1) = (cols.clone().next().map(|c| self.geo.col_x(c)), cols.clone().last().map(|c| self.geo.col_x(c) + self.geo.cols.w(c) as f32));
            if let (Some(x0), Some(x1)) = (x0, x1) {
                for r in first..=last {
                    let y = self.geo.row_y(r) + self.geo.row_h as f32 - 0.5;
                    p.hline(x0..=x1, y, grid_line);
                }
                let (y0, y1) = (self.geo.row_y(first), self.geo.row_y(last) + self.geo.row_h as f32);
                for c in cols.clone() {
                    let x = self.geo.col_x(c) + self.geo.cols.w(c) as f32 - 0.5;
                    p.vline(x, y0..=y1, grid_line);
                }
            }
        }
        // The active cell.
        let (ar, ac) = (self.sel.row, self.sel.col);
        if rows.contains(&ar) && cols.contains(&ac) {
            p.rect_stroke(self.geo.cell_rect(ar, ac), 0.0, Stroke::new(2.0, t.accent), StrokeKind::Inside);
        }
    }

    /// Column letters along the top and row numbers down the left.
    fn headers(&self, ui: &egui::Ui, cols: [std::ops::Range<u32>; 2], rows: [std::ops::Range<u32>; 2]) {
        let (t, geo) = (self.t, self.geo);
        let r = geo.rect;
        let (st, sl, sb, sr) = self.sel.bounds();
        let font = FontId::new(self.font.size * 0.85, self.font.family.clone());
        let line = Stroke::new(1.0, t.border);
        let head = |p: &egui::Painter, rect: Rect, text: &str, on: bool| {
            p.rect_filled(rect, 0.0, if on { t.selection } else { t.table_header_bg });
            p.rect_stroke(rect, 0.0, Stroke::new(1.0, t.separator), StrokeKind::Inside);
            let look = Look { font: font.clone(), color: if on { t.text_strong } else { t.text_dim }, align: Align2::CENTER_CENTER, italic: false };
            paint_text(p, rect, text, look, 2.0);
        };
        let top = Rect::from_min_max(pos2(geo.fx, r.top()), pos2(r.right(), geo.fy));
        for (i, range) in cols.into_iter().enumerate() {
            let clip = if i == 0 { Rect::from_min_max(top.min, pos2(geo.sx, top.bottom())) } else { Rect::from_min_max(pos2(geo.sx, top.top()), top.max) };
            let p = ui.painter_at(clip);
            for c in range {
                let rect = Rect::from_min_size(pos2(geo.col_x(c), r.top()), vec2(geo.cols.w(c) as f32, geo.fy - r.top()));
                head(&p, rect, &column_name(c), (sl..=sr).contains(&c));
            }
        }
        let left = Rect::from_min_max(pos2(r.left(), geo.fy), pos2(geo.fx, r.bottom()));
        for (i, range) in rows.into_iter().enumerate() {
            let clip = if i == 0 { Rect::from_min_max(left.min, pos2(left.right(), geo.sy)) } else { Rect::from_min_max(pos2(left.left(), geo.sy), left.max) };
            let p = ui.painter_at(clip);
            for row in range {
                let rect = Rect::from_min_size(pos2(r.left(), geo.row_y(row)), vec2(geo.fx - r.left(), geo.row_h as f32));
                head(&p, rect, &(u64::from(row) + 1).to_string(), (st..=sb).contains(&row));
            }
        }
        let corner = Rect::from_min_max(r.min, pos2(geo.fx, geo.fy));
        let p = ui.painter_at(r);
        p.rect_filled(corner, 0.0, t.table_header_bg);
        p.hline(r.left()..=r.right(), geo.fy - 0.5, line);
        p.vline(geo.fx - 0.5, r.top()..=r.bottom(), line);
    }
}

/// How a cell's text looks.
struct Look {
    font: FontId,
    color: Color32,
    align: Align2,
    italic: bool,
}

/// One line of text in a cell, cut with "…" to it.
fn paint_text(p: &egui::Painter, cell: Rect, text: &str, look: Look, pad: f32) {
    let Look { font, color, align, italic } = look;
    let text = if text.contains('\n') { text.replace('\n', " ") } else { text.to_owned() };
    let mut fmt = TextFormat::simple(font, color);
    fmt.italics = italic;
    let mut job = LayoutJob::single_section(text, fmt);
    job.wrap = TextWrapping::truncate_at_width((cell.width() - 2.0 * pad).max(1.0));
    let galley = p.layout_job(job);
    let x = match align.x() {
        egui::Align::Min => cell.left() + pad,
        egui::Align::Center => cell.center().x,
        egui::Align::Max => cell.right() - pad,
    };
    let at = align.anchor_size(Pos2::new(x, cell.center().y), galley.size()).min;
    p.with_clip_rect(cell.intersect(p.clip_rect())).galley(at, galley, color);
}
