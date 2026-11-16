//! The settings' spacing rhythm and form helpers, shared by the app settings tabs, Settings →
//! Models, and the workspace and chat settings (which draw the same fields as the defaults).
//!
//! Every gap here is designed at Comfortable density and goes through [`Tokens::pad`]; label
//! columns grow with the text size. Use these instead of ad hoc numbers so a tab reads as one
//! grid: a fixed label column, controls of one height, the same gap between rows, and more
//! between groups.

use egui::{Align, Layout, RichText, Sense, Ui, vec2};

use crate::theme::{self, Tokens};

/// Padding around a settings page's content (wide windows).
pub const PAGE_PAD: f32 = 32.0;
/// Padding around a settings page's content on phones.
pub const PAGE_PAD_NARROW: f32 = 16.0;
/// Space above a page's heading.
pub const PAGE_TOP: f32 = 24.0;
/// The widest a settings page's content gets: form fields don't stretch across a wide window.
pub const MAX_W: f32 = 760.0;
/// The settings' tab list.
pub const NAV_W: f32 = 208.0;
pub const NAV_ROW_H: f32 = 32.0;
/// A form's label column.
pub const LABEL_W: f32 = 156.0;
/// The label column where space is short (a narrow window or popover).
pub const LABEL_W_NARROW: f32 = 104.0;
/// Between a label and its control, and between controls.
pub const COL_GAP: f32 = 16.0;
/// Between two rows of a form.
pub const ROW_GAP: f32 = 10.0;
/// Between a heading (and its description) and what it heads.
pub const HEAD_GAP: f32 = 16.0;
/// Before a group of rows within a page.
pub const SECTION_GAP: f32 = 32.0;
/// Buttons, menus and fields in the settings.
pub const CONTROL_H: f32 = 28.0;
/// Below this width a form stacks each label above its controls.
pub const NARROW: f32 = 520.0;

/// The label column's width for a form `avail` wide.
pub fn label_w(t: &Tokens, avail: f32) -> f32 {
    let base = if avail < 3.0 * LABEL_W { LABEL_W_NARROW } else { LABEL_W };
    base * t.knobs.font_scale.clamp(0.8, 1.6)
}

/// The height of a control row.
pub fn control_h(t: &Tokens) -> f32 {
    t.pad(CONTROL_H).max(22.0)
}

/// Controls in `ui` one height: buttons, menus, fields and toggles line up row to row.
pub fn controls(ui: &mut Ui, t: &Tokens) {
    let h = control_h(t);
    let s = ui.spacing_mut();
    s.interact_size.y = h;
    s.button_padding = vec2(t.pad(10.0), t.pad(4.0));
    s.item_spacing = vec2(t.pad(8.0), t.pad(6.0));
}

/// A one-line text field as tall as the buttons beside it.
pub fn field<'a>(text: &'a mut String, t: &Tokens) -> egui::TextEdit<'a> {
    egui::TextEdit::singleline(text).min_size(vec2(0.0, control_h(t))).align(egui::Align2::LEFT_CENTER)
}

/// One form row: the label in a fixed column, the controls after it (wrapping within their
/// column); stacked on narrow screens.
pub fn row<R>(ui: &mut Ui, t: &Tokens, label: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    let h = control_h(t);
    let avail = ui.available_width();
    let r = if avail < NARROW {
        ui.label(RichText::new(label).color(t.text));
        ui.horizontal_wrapped(add).inner
    } else {
        let lw = label_w(t, avail);
        ui.horizontal_top(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(lw, h), Sense::hover());
            let font = egui::TextStyle::Body.resolve(ui.style());
            let galley = ui.painter().layout(label.to_owned(), font, t.text, lw - t.pad(8.0));
            ui.painter().galley(egui::pos2(rect.left(), rect.center().y - galley.size().y / 2.0), galley, t.text);
            let w = ui.available_width();
            ui.allocate_ui_with_layout(vec2(w, h), Layout::left_to_right(Align::Center).with_main_wrap(true), add).inner
        })
        .inner
    };
    ui.add_space(t.pad(ROW_GAP));
    r
}

/// A page's heading and what it is for.
pub fn heading(ui: &mut Ui, t: &Tokens, title: &str, blurb: &str) {
    ui.label(RichText::new(title).size(t.text_size(20.0)).family(theme::bold_or_regular(ui.ctx())).color(t.text_strong));
    if !blurb.is_empty() {
        ui.add_space(t.pad(2.0));
        ui.label(RichText::new(blurb).color(t.text_dim));
    }
    ui.add_space(t.pad(HEAD_GAP + 4.0));
}

/// A group of rows within a page: a rule, its heading and, if given, a line on what it is for.
pub fn section(ui: &mut Ui, t: &Tokens, title: &str, blurb: &str) {
    ui.add_space(t.pad(SECTION_GAP / 2.0));
    let y = ui.cursor().top();
    ui.painter().hline(ui.max_rect().x_range(), y, egui::Stroke::new(1.0, t.separator));
    ui.add_space(t.pad(SECTION_GAP / 2.0));
    ui.label(RichText::new(title).size(t.text_size(15.0)).family(theme::bold_or_regular(ui.ctx())).color(t.text_strong));
    if !blurb.is_empty() {
        ui.label(RichText::new(blurb).small().color(t.text_dim));
    }
    ui.add_space(t.pad(HEAD_GAP - 4.0));
}

/// A grid of label + control rows with the settings' columns and gaps (`egui::Grid` for forms
/// whose cells are built row by row).
pub fn grid(id: impl egui::AsIdSalt, t: &Tokens, avail: f32) -> egui::Grid {
    egui::Grid::new(id).num_columns(2).spacing([t.pad(COL_GAP), t.pad(ROW_GAP)]).min_col_width(label_w(t, avail)).min_row_height(control_h(t))
}
