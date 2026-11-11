//! The `table` node: a sticky header and only the rows on screen (`show_rows`), one
//! allocation per row and painted cells, so 10k rows cost what 30 do. Click selects a row
//! (`values[name]`, and the `select` action); double-click edits an editable cell, Enter or
//! clicking away sends an `edit` event, Escape cancels.

use byob_protocol::tree::{CellEdit, TableColumn, cell_text};
use egui::text::{LayoutJob, TextWrapping};
use egui::{Align2, Rect, Sense, Stroke, TextFormat, pos2, vec2};
use serde_json::{Map, Value};

use super::{Cx, Editing};

/// Height of a table without its own.
const DEFAULT_HEIGHT: f32 = 300.0;
/// Narrowest a column without a width gets.
const MIN_AUTO_WIDTH: f32 = 60.0;

pub(super) struct Table<'a> {
    pub name: &'a str,
    pub columns: &'a [TableColumn],
    pub rows: &'a [Vec<Value>],
    pub height: Option<f32>,
    pub select: Option<&'a str>,
}

fn widths(columns: &[TableColumn], avail: f32) -> Vec<f32> {
    let fixed: f32 = columns.iter().filter_map(|c| c.width).sum();
    let autos = columns.iter().filter(|c| c.width.is_none()).count();
    let auto = if autos == 0 { 0.0 } else { ((avail - fixed) / autos as f32).max(MIN_AUTO_WIDTH) };
    columns.iter().map(|c| c.width.unwrap_or(auto)).collect()
}

pub(super) fn show(ui: &mut egui::Ui, cx: &mut Cx<'_, '_>, tb: Table<'_>, id: egui::Id) {
    let t = cx.t;
    let pad = t.pad(4.0);
    let row_h = ui.text_style_height(&egui::TextStyle::Body) + 2.0 * t.pad(4.0);
    let font = egui::TextStyle::Body.resolve(ui.style());
    // The selected row, a field like any other (the tree has no say in it).
    let selected = cx.st.field(tb.name, || Value::Null, |_| true).as_u64().and_then(|r| usize::try_from(r).ok());
    let frame = egui::Frame::new().stroke(Stroke::new(1.0, t.border)).corner_radius(t.round(6.0)).inner_margin(1.0);
    frame.show(ui, |ui| {
        let ws = widths(tb.columns, ui.available_width());
        let total: f32 = ws.iter().sum();
        let mut clicked = None;
        let mut start_edit = None;
        let mut done: Option<(usize, usize, Option<String>)> = None;
        egui::ScrollArea::horizontal().id_salt(id.with("h")).auto_shrink([false, true]).show(ui, |ui| {
            ui.set_min_width(total);
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            // Header.
            let (hr, _) = ui.allocate_exact_size(vec2(total, row_h), Sense::hover());
            ui.painter().rect_filled(hr, 0.0, t.table_header_bg);
            let mut x = hr.left();
            for (ci, (c, w)) in tb.columns.iter().zip(&ws).enumerate() {
                let cell = Rect::from_min_size(pos2(x, hr.top()), vec2(*w, row_h));
                paint(ui, cell, c.header().to_owned(), font.clone(), t.text_strong, numeric_column(tb.rows, ci), pad);
                x += w;
            }
            let max_h = tb.height.unwrap_or(DEFAULT_HEIGHT);
            egui::ScrollArea::vertical().id_salt(id.with("v")).max_height(max_h).auto_shrink([false, true]).show_rows(ui, row_h, tb.rows.len(), |ui, range| {
                ui.set_min_width(total);
                for r in range {
                    let row = tb.rows.get(r).map(Vec::as_slice).unwrap_or_default();
                    let (rect, resp) = ui.allocate_exact_size(vec2(total, row_h), Sense::click());
                    let bg = if selected == Some(r) {
                        Some(t.selection)
                    } else if resp.hovered() {
                        Some(t.hover)
                    } else {
                        (r % 2 == 1).then_some(t.table_stripe)
                    };
                    if let Some(bg) = bg {
                        ui.painter().rect_filled(rect, 0.0, bg);
                    }
                    let mut x = rect.left();
                    for (ci, (c, w)) in tb.columns.iter().zip(&ws).enumerate() {
                        let cell = Rect::from_min_size(pos2(x, rect.top()), vec2(*w, row_h));
                        x += w;
                        let editing = cx.st.editing.as_mut().filter(|e| e.table == tb.name && e.row == r && e.col == ci);
                        if let Some(e) = editing {
                            let edit = egui::TextEdit::singleline(&mut e.text).id(id.with(("edit", r, ci))).font(font.clone()).margin(vec2(pad - 2.0, 2.0));
                            let er = ui.put(cell.shrink(1.0), edit);
                            if !e.focused {
                                er.request_focus();
                                e.focused = true;
                            } else if er.lost_focus() {
                                let cancel = ui.input(|i| i.key_pressed(egui::Key::Escape));
                                done = Some((r, ci, (!cancel).then(|| e.text.clone())));
                            }
                            continue;
                        }
                        let pending = cx.st.pending.get(&(tb.name.to_owned(), r, ci));
                        let value = row.get(ci);
                        let numeric = pending.is_none() && value.is_some_and(Value::is_number);
                        let text = pending.cloned().unwrap_or_else(|| value.map(cell_text).unwrap_or_default());
                        paint(ui, cell, text, font.clone(), t.text, numeric, pad);
                        if c.editable && resp.double_clicked() && resp.interact_pointer_pos().is_some_and(|p| cell.contains(p)) {
                            start_edit = Some((r, ci, value.map(cell_text).unwrap_or_default()));
                        }
                    }
                    if resp.clicked() {
                        clicked = Some(r);
                    }
                }
            });
        });
        if let Some(r) = clicked {
            cx.st.values.insert(tb.name.to_owned(), Value::from(r));
            if let Some(a) = tb.select {
                cx.st.fire(a, None, Map::new());
            }
        }
        if let Some((row, col, text)) = start_edit {
            cx.st.editing = Some(Editing { table: tb.name.to_owned(), row, col, text, focused: false });
        }
        if let Some((row, col, text)) = done {
            cx.st.editing = None;
            if let (Some(value), Some(c)) = (text, tb.columns.get(col)) {
                let unchanged = tb.rows.get(row).and_then(|r| r.get(col)).map(cell_text).unwrap_or_default() == value;
                if !unchanged {
                    // Shown until the owner answers (its reply has the real value).
                    cx.st.pending.insert((tb.name.to_owned(), row, col), value.clone());
                    let edit = CellEdit { table: tb.name.to_owned(), row, column: c.key.clone(), value };
                    cx.st.fire("edit", Some(edit), Map::new());
                }
            }
        }
    });
}

/// A column of numbers (by its first filled cells): its header is right-aligned like them.
fn numeric_column(rows: &[Vec<Value>], col: usize) -> bool {
    rows.iter().take(20).filter_map(|r| r.get(col)).find(|v| !v.is_null()).is_some_and(Value::is_number)
}

/// One cell's text, cut with "…" to the cell, numbers right-aligned.
fn paint(ui: &egui::Ui, cell: Rect, text: String, font: egui::FontId, color: egui::Color32, right: bool, pad: f32) {
    let mut job = LayoutJob::single_section(text.replace('\n', " "), TextFormat::simple(font, color));
    job.wrap = TextWrapping::truncate_at_width((cell.width() - 2.0 * pad).max(1.0));
    let galley = ui.painter().layout_job(job);
    let (x, align) = if right { (cell.right() - pad, Align2::RIGHT_CENTER) } else { (cell.left() + pad, Align2::LEFT_CENTER) };
    let at = align.anchor_size(pos2(x, cell.center().y), galley.size()).min;
    ui.painter().with_clip_rect(cell.intersect(ui.clip_rect())).galley(at, galley, color);
}
