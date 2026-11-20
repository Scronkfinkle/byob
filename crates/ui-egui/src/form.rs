//! The settings' spacing rhythm and form helpers, shared by the settings screen, Settings →
//! Models, and the workspace and chat settings (which draw the same fields as the defaults).
//!
//! Every gap here is designed at Comfortable density and goes through [`Tokens::pad`]; text
//! columns grow with the text size. Use these instead of ad hoc numbers so a page reads as one
//! grid.
//!
//! A setting is a [`setting_row`]: its name and a one-line description on the left (wrapping
//! at a readable measure), its control on the right, every control's right edge on one line;
//! below [`STACK_BELOW`] the control goes under the text. Rows of a section are split by
//! hairlines; sections by a rule and a heading ([`section`]).

use egui::text::{LayoutJob, TextFormat};
use egui::{Align, Color32, FontId, Id, Layout, Rect, Response, RichText, Sense, Stroke, Ui, UiBuilder, pos2, vec2};

use crate::theme::{self, Tokens};

/// Padding around a settings page's content (wide windows).
pub const PAGE_PAD: f32 = 40.0;
/// Padding around a settings page's content on phones.
pub const PAGE_PAD_NARROW: f32 = 16.0;
/// Space above a page's heading.
pub const PAGE_TOP: f32 = 28.0;
/// The widest a settings page's content gets: past this the rows' name and control drift too
/// far apart to read as one line.
pub const MAX_W: f32 = 880.0;
/// The settings navigation column (at least; it follows the chat list's width).
pub const NAV_W: f32 = 240.0;
pub const NAV_ROW_H: f32 = 32.0;
/// The longest a setting's description line gets (about 85 characters).
pub const TEXT_MAX: f32 = 540.0;
/// The narrowest a setting's text column gets beside its control.
pub const TEXT_MIN: f32 = 200.0;
/// Below this content width a setting stacks its control under its name.
pub const STACK_BELOW: f32 = 560.0;
/// Above and below a setting's name and control.
pub const ROW_PAD: f32 = 12.0;
/// A form's label column (`grid`, `row`: the Models forms and the workspace and chat settings).
pub const LABEL_W: f32 = 156.0;
/// The label column where space is short (a narrow window or popover).
pub const LABEL_W_NARROW: f32 = 104.0;
/// Between a label and its control, and between controls.
pub const COL_GAP: f32 = 16.0;
/// Between two rows of a form.
pub const ROW_GAP: f32 = 10.0;
/// Between a heading (and its description) and what it heads.
pub const HEAD_GAP: f32 = 16.0;
/// Before a section (a card) within a page.
pub const SECTION_GAP: f32 = 32.0;
/// Buttons, menus and fields in the settings.
pub const CONTROL_H: f32 = 28.0;

/// The label column's width for a form `avail` wide.
pub fn label_w(t: &Tokens, avail: f32) -> f32 {
    let base = if avail < 3.0 * LABEL_W { LABEL_W_NARROW } else { LABEL_W };
    base * t.knobs.font_scale.clamp(0.8, 1.6)
}

/// The height of a control row.
pub fn control_h(t: &Tokens) -> f32 {
    t.pad(CONTROL_H).max(22.0)
}

/// How much wider text columns get with the text size.
fn text_scale(t: &Tokens) -> f32 {
    t.knobs.font_scale.clamp(0.8, 1.6)
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

/// A page's heading and what it is for.
pub fn heading(ui: &mut Ui, t: &Tokens, title: &str, blurb: &str) {
    ui.label(RichText::new(title).size(t.text_size(22.0)).family(theme::bold_or_regular(ui.ctx())).color(t.text_strong));
    if !blurb.is_empty() {
        ui.add_space(t.pad(4.0));
        let w = ui.available_width().min(TEXT_MAX * 1.4 * text_scale(t));
        ui.allocate_ui(vec2(w, 0.0), |ui| ui.label(RichText::new(blurb).color(t.text_dim)));
    }
    ui.add_space(t.pad(HEAD_GAP + 8.0));
}

/// A section of a page: a rule (unless it is the page's first), its heading and, if given, a
/// line on what it is for. An empty title draws only the rule.
pub fn section(ui: &mut Ui, t: &Tokens, title: &str, blurb: &str, first: bool) {
    // Sections are cards (see [`card`]): space between them, no rule.
    ui.add_space(t.pad(if first { 8.0 } else { SECTION_GAP }));
    if title.is_empty() {
        return;
    }
    ui.label(RichText::new(title).size(t.text_size(14.0)).family(theme::bold_or_regular(ui.ctx())).color(t.text_strong));
    if !blurb.is_empty() {
        ui.add_space(t.pad(2.0));
        let w = ui.available_width().min(TEXT_MAX * 1.4 * text_scale(t));
        ui.allocate_ui(vec2(w, 0.0), |ui| ui.label(RichText::new(blurb).font(desc_font(ui)).color(t.text_dim)));
    }
    ui.add_space(t.pad(10.0));
}

/// The width of a settings select, so a column of them lines up on both edges.
pub const SELECT_W: f32 = 240.0;

/// egui's drop-down in byob's look: the app's chevron instead of a filled triangle, and
/// [`SELECT_W`] wide unless the caller says otherwise.
pub fn combo<'a>(id_salt: impl egui::AsIdSalt) -> egui::ComboBox<'a> {
    egui::ComboBox::from_id_salt(id_salt).width(SELECT_W).icon(|ui, rect, visuals, _open| {
        let r = Rect::from_center_size(rect.center(), vec2(11.0, 11.0));
        crate::icons::chevron_down(ui.painter(), r, visuals.fg_stroke.color);
    })
}

/// egui's own widgets inside `ui` (drop-downs, buttons, drag values) in byob's outlined
/// look: transparent with a hairline, a soft fill on hover or open, control corners.
pub fn outlined_controls(ui: &mut Ui, t: &Tokens) {
    let r = egui::CornerRadius::same(t.round(CONTROL_R).round().clamp(0.0, 255.0) as u8);
    let w = &mut ui.style_mut().visuals.widgets;
    for (state, fill) in [(&mut w.inactive, Color32::TRANSPARENT), (&mut w.hovered, soft(t)), (&mut w.active, soft_hover(t)), (&mut w.open, soft(t))] {
        state.weak_bg_fill = fill;
        state.bg_stroke = Stroke::new(1.0, t.field_border);
        state.corner_radius = r;
    }
}

/// A select with nothing to choose: its box and text, faded, so it lines up with the
/// selects around it.
pub fn static_select(ui: &mut Ui, t: &Tokens, text: &str, width: f32) -> Response {
    let h = ui.spacing().interact_size.y.max(control_h(t));
    let (rect, resp) = ui.allocate_exact_size(vec2(width, h), Sense::hover());
    if ui.is_rect_visible(rect) {
        ui.painter().rect_stroke(rect, t.round(CONTROL_R), Stroke::new(1.0, t.field_border.gamma_multiply(0.6)), egui::StrokeKind::Inside);
        let g = ui.painter().layout_no_wrap(text.to_owned(), egui::TextStyle::Body.resolve(ui.style()), t.text_faint);
        ui.painter().galley(pos2(rect.left() + 10.0, rect.center().y - g.size().y / 2.0), g, t.text_faint);
    }
    resp
}

/// Corners of controls: selects, buttons, fields and colour chips.
pub const CONTROL_R: f32 = 8.0;
/// Corners of cards (a step rounder than what sits inside them).
pub const CARD_R: f32 = 12.0;

/// A card's surface: white on light, a step lighter than the page on dark.
pub fn card_fill(t: &Tokens) -> Color32 {
    if t.dark { theme::mix(t.app_bg, Color32::WHITE, 0.035) } else { Color32::WHITE }
}

/// A card's soft edge: the separator eased toward the card, so it reads by its fill first.
pub fn card_edge(t: &Tokens) -> Color32 {
    theme::mix(t.separator, card_fill(t), 0.3)
}

/// A section's card: a quiet surface a step off the page (white on light, lifted on dark),
/// a hairline border and 10-point corners; its rows have thin lines between them.
pub fn card(t: &Tokens) -> egui::Frame {
    let (fill, edge) = (card_fill(t), card_edge(t));
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // a few points
    let r = t.round(CARD_R).round().clamp(0.0, 255.0) as u8;
    egui::Frame::new().fill(fill).stroke(Stroke::new(1.0, edge)).corner_radius(egui::CornerRadius::same(r)).inner_margin(egui::Margin::symmetric(18, 2))
}

/// A number on a slider: a thin track filled up to the value in the accent, a round knob,
/// and the value beside it in plain text. Dragging or clicking the track sets it (snapped to
/// `step`). Returns whether it changed.
pub fn slider(ui: &mut Ui, t: &Tokens, value: &mut f32, range: std::ops::RangeInclusive<f32>, step: f32, show: impl Fn(f32) -> String) -> bool {
    let (lo, hi) = (*range.start(), *range.end());
    let w = 160.0;
    let h = control_h(t).max(28.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(w, h), Sense::click_and_drag());
    let knob_r = 8.0;
    let track = Rect::from_min_max(pos2(rect.left() + knob_r, rect.center().y - 2.0), pos2(rect.right() - knob_r, rect.center().y + 2.0));
    let mut changed = false;
    if let Some(p) = resp.interact_pointer_pos()
        && (resp.dragged() || resp.clicked())
        && hi > lo
    {
        let f = ((p.x - track.left()) / track.width()).clamp(0.0, 1.0);
        let mut v = lo + f * (hi - lo);
        if step > 0.0 {
            v = lo + ((v - lo) / step).round() * step;
        }
        let v = v.clamp(lo, hi);
        if (v - *value).abs() > f32::EPSILON {
            *value = v;
            changed = true;
        }
    }
    let f = if hi > lo { ((*value - lo) / (hi - lo)).clamp(0.0, 1.0) } else { 0.0 };
    let x = track.left() + f * track.width();
    let p = ui.painter();
    p.rect_filled(track, 2.0, soft_hover(t));
    p.rect_filled(Rect::from_min_max(track.min, pos2(x, track.max.y)), 2.0, t.accent);
    let knob = pos2(x, rect.center().y);
    p.circle_filled(knob + vec2(0.0, 1.0), knob_r, Color32::from_black_alpha(if t.dark { 70 } else { 28 }));
    p.circle_filled(knob, knob_r, t.field_bg);
    p.circle_stroke(knob, knob_r, Stroke::new(1.0, if resp.hovered() || resp.dragged() { t.text_faint } else { t.field_border }));
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let _ = resp;
    ui.add_space(10.0);
    let label = RichText::new(show(*value)).color(t.text_dim);
    ui.add_sized(vec2(56.0, h), egui::Label::new(label));
    changed
}

/// A control's fill that reads on any surface (a page, a card, a dialog, light or dark): a
/// light tint of the text colour, not a fixed grey that can match what's behind it.
pub(crate) fn soft(t: &Tokens) -> Color32 {
    t.text_strong.gamma_multiply(if t.dark { 0.075 } else { 0.055 })
}

/// A whole row or card under the pointer: lighter than [`soft`], since it covers a wide
/// area (a button's hover across a full row reads as a heavy grey bar).
pub(crate) fn row_hover(t: &Tokens) -> Color32 {
    t.text_strong.gamma_multiply(if t.dark { 0.045 } else { 0.03 })
}

/// [`soft`], hovered or open.
pub(crate) fn soft_hover(t: &Tokens) -> Color32 {
    t.text_strong.gamma_multiply(if t.dark { 0.12 } else { 0.09 })
}

/// A button in byob's style (outlined, quiet), for actions in settings rows. Greyed when
/// `enabled` is false.
pub(crate) fn button(ui: &mut Ui, t: &Tokens, label: &str, enabled: bool) -> Response {
    crate::workspace::dialog_button(ui, t, label, false, enabled, control_h(t).max(32.0))
}

/// [`button`] for the one action that can't be undone: the error colour, outlined.
pub(crate) fn danger_button(ui: &mut Ui, t: &Tokens, label: &str) -> Response {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let g = ui.painter().layout_no_wrap(label.to_owned(), font, t.danger);
    let h = control_h(t).max(32.0);
    let (rect, resp) = ui.allocate_exact_size(vec2((g.size().x + 32.0).max(88.0), h), Sense::click());
    if ui.is_rect_visible(rect) {
        let fill = if resp.hovered() { t.danger.gamma_multiply(0.1) } else { Color32::TRANSPARENT };
        ui.painter().rect(rect, t.round(CONTROL_R), fill, Stroke::new(1.0, t.danger.gamma_multiply(0.5)), egui::StrokeKind::Inside);
        ui.painter().galley(rect.center() - g.size() / 2.0, g, t.danger);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// The button a select opens from: an optional mark, the value, a caret. Outlined (a hairline
/// border, no fill until hovered), 6-point corners, one height.
pub(crate) fn pill(ui: &mut Ui, t: &Tokens, min_w: f32, mark: Option<&dyn Fn(&egui::Painter, Rect)>, value: &str) -> Response {
    let h = control_h(t).max(32.0);
    let font = egui::TextStyle::Body.resolve(ui.style());
    let g = ui.painter().layout_no_wrap(value.to_owned(), font, t.text);
    let lead = if mark.is_some() { 34.0 } else { 12.0 };
    let w = (lead + g.size().x + 34.0).max(min_w);
    let (rect, resp) = ui.allocate_exact_size(vec2(w, h), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    if ui.is_rect_visible(rect) {
        let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
        // Outlined: it sits cleanly on a page, a card or a dialog; a soft fill only while
        // hovered or open.
        let fill = if resp.hovered() || open { soft(t) } else { Color32::TRANSPARENT };
        ui.painter().rect(rect, t.round(CONTROL_R), fill, Stroke::new(1.0, t.field_border), egui::StrokeKind::Inside);
        if let Some(m) = mark {
            m(ui.painter(), Rect::from_center_size(pos2(rect.left() + 18.0, rect.center().y), vec2(18.0, 18.0)));
        }
        let gy = rect.center().y - g.size().y / 2.0;
        ui.painter().galley(pos2(rect.left() + lead, gy), g, t.text);
        crate::icons::chevron_down(ui.painter(), Rect::from_center_size(pos2(rect.right() - 15.0, rect.center().y), vec2(11.0, 11.0)), t.text_dim);
    }
    resp
}

/// One of a few choices as a select: the pill shows the current one; byob's menu lists them
/// all, the current one ticked (with an icon each when `icon` gives one). Returns whether it
/// changed.
pub fn select<T: PartialEq + Copy>(ui: &mut Ui, t: &Tokens, value: &mut T, choices: &[(T, &str)], icon: Option<fn(T) -> crate::icons::Paint>) -> bool {
    let current = choices.iter().find(|(v, _)| *v == *value).map_or("", |(_, l)| *l);
    let paint = icon.map(|f| f(*value));
    let mark = paint.map(|p| move |pt: &egui::Painter, r: Rect| p(pt, r.shrink(1.5), t.text_dim));
    let b = pill(ui, t, 150.0, mark.as_ref().map(|m| m as &dyn Fn(&egui::Painter, Rect)), current);
    let mut changed = false;
    crate::menu::below(&b, t, b.rect.width().max(190.0)).show(|ui| {
        crate::menu::tighten(ui);
        for (v, label) in choices {
            let on = *v == *value;
            if crate::menu::item(ui, t, icon.map(|f| f(*v)), label, on.then_some("✓")).clicked() {
                if !on {
                    *value = *v;
                    changed = true;
                }
                ui.close();
            }
        }
    });
    changed
}

/// A setting's description: a step below its name, still easy to read.
fn desc_font(ui: &Ui) -> FontId {
    FontId::proportional((egui::TextStyle::Body.resolve(ui.style()).size * 0.88).max(10.0))
}

/// How a [`setting_row`] is drawn.
#[derive(Debug, Clone, Copy, Default)]
pub struct RowStyle<'a> {
    /// Words to highlight in the name and description (search results).
    pub needles: &'a [String],
    /// The first row of its section: no hairline above.
    pub first: bool,
    /// The control goes under the text however wide the page (long text fields).
    pub wide: bool,
    /// Briefly lit (0–1) after a search result or deep link jumped here.
    pub flash: f32,
    /// The name is a link (search results: "show it in its page").
    pub link: bool,
}

/// What a [`setting_row`] drew.
pub struct RowOut<R> {
    pub inner: R,
    pub rect: Rect,
    /// The setting's name (clickable when [`RowStyle::link`]).
    pub name: Response,
}

/// One setting: `name` and `desc` on the left, the control `add` draws on the right (its
/// right edge on the page's), centered on the text; stacked on narrow pages.
pub fn setting_row<R>(ui: &mut Ui, t: &Tokens, id: Id, name: &str, desc: &str, style: RowStyle<'_>, add: impl FnOnce(&mut Ui) -> R) -> RowOut<R> {
    let w = ui.available_width();
    let left = ui.cursor().left();
    let top = ui.cursor().top();
    let right = left + w;
    let pad_y = t.pad(ROW_PAD);
    let bg = ui.painter().add(egui::Shape::Noop);
    if !style.first {
        ui.painter().hline(left..=right, top, Stroke::new(1.0, t.separator.gamma_multiply(0.7)));
    }
    let name_font = egui::TextStyle::Body.resolve(ui.style());
    let desc_font = desc_font(ui);
    let hl = (t.selection, t.text_strong);
    let stacked = style.wide || w < STACK_BELOW;
    let gap = t.pad(COL_GAP * 2.0);
    let size_id = id.with("control-size");
    let text_gap = t.pad(3.0);
    let (text_w, control_rect) = if stacked {
        (w.min(TEXT_MAX * 1.4 * text_scale(t)), None)
    } else {
        let prev: Option<egui::Vec2> = ui.data(|d| d.get_temp(size_id));
        let text_min = (TEXT_MIN * text_scale(t)).min(w * 0.5);
        let cmax = (w - gap - text_min).max(w * 0.35);
        let cw = prev.map_or(cmax, |s| s.x.min(cmax));
        let tw = (w - gap - cw).clamp(text_min, TEXT_MAX * text_scale(t));
        (tw, Some((cw, cmax, prev.map_or(control_h(t), |s| s.y))))
    };
    let name_galley = ui.painter().layout_job(highlight(name, style.needles, name_font, if style.link { t.text_strong } else { t.text }, hl, text_w));
    let desc_galley = (!desc.is_empty()).then(|| ui.painter().layout_job(highlight(desc, style.needles, desc_font, t.text_dim, hl, text_w)));
    let text_h = name_galley.size().y + desc_galley.as_ref().map_or(0.0, |g| text_gap + g.size().y);
    let layout = Layout::left_to_right(Align::Center).with_main_wrap(true);
    let (text_top, inner, bottom) = match control_rect {
        None => {
            let y = top + pad_y + text_h + t.pad(8.0);
            // One control row tall: the layout centers across it and grows down as it wraps.
            let max = Rect::from_min_max(pos2(left, y), pos2(right, y + control_h(t)));
            let mut child = ui.new_child(UiBuilder::new().id_salt(id).max_rect(max).layout(layout));
            let inner = add(&mut child);
            (top + pad_y, inner, child.min_rect().bottom().max(y) + pad_y)
        }
        Some((cw, cmax, ch)) => {
            let row_h = text_h.max(ch);
            let text_top = top + pad_y + (row_h - text_h) / 2.0;
            let control_top = top + pad_y + (row_h - ch) / 2.0;
            let max = Rect::from_min_size(pos2(right - cw, control_top), vec2(cmax, ch.max(control_h(t))));
            let mut child = ui.new_child(UiBuilder::new().id_salt(id).max_rect(max).layout(layout));
            let inner = add(&mut child);
            let used = child.min_rect();
            let size = vec2(used.width(), used.height());
            if ((size - vec2(cw, ch)).abs().max_elem()) > 0.5 {
                // Laid out against last frame's size: right-align again with this one.
                ui.data_mut(|d| d.insert_temp(size_id, size));
                ui.ctx().request_discard("a setting's control changed size");
            }
            (text_top, inner, (top + pad_y + row_h).max(used.bottom()) + pad_y)
        }
    };
    let name_rect = Rect::from_min_size(pos2(left, text_top), name_galley.size());
    let desc_top = name_rect.bottom() + text_gap;
    ui.painter().galley(name_rect.min, name_galley, t.text);
    if let Some(g) = desc_galley {
        ui.painter().galley(pos2(left, desc_top), g, t.text_dim);
    }
    let rect = Rect::from_min_max(pos2(left, top), pos2(right, bottom));
    if style.flash > 0.0 {
        let fill = t.text_strong.gamma_multiply(0.08 * style.flash.clamp(0.0, 1.0));
        ui.painter().set(bg, egui::Shape::rect_filled(rect.expand2(vec2(t.pad(8.0), 0.0)), t.round(6.0), fill));
    }
    let sense = if style.link { Sense::click() } else { Sense::hover() };
    let name = ui.interact(name_rect, id.with("name"), sense);
    let name = if style.link { name.on_hover_cursor(egui::CursorIcon::PointingHand) } else { name };
    if style.link && name.hovered() {
        ui.painter().hline(name_rect.x_range(), name_rect.bottom(), Stroke::new(1.0, t.text_dim));
    }
    ui.advance_cursor_after_rect(rect);
    RowOut { inner, rect, name }
}

/// `text` laid out at most `wrap` wide, with every occurrence of `needles` (ASCII case
/// ignored) marked by `hl` (background, text color).
pub fn highlight(text: &str, needles: &[String], font: FontId, color: Color32, hl: (Color32, Color32), wrap: f32) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap.max(10.0);
    let lower = text.to_ascii_lowercase();
    let mut marks = vec![false; text.len()];
    for n in needles.iter().filter(|n| !n.is_empty()) {
        for (i, m) in lower.match_indices(n.as_str()) {
            for slot in marks.iter_mut().skip(i).take(m.len()) {
                *slot = true;
            }
        }
    }
    let plain = TextFormat::simple(font.clone(), color);
    let marked = TextFormat { background: hl.0, color: hl.1, ..TextFormat::simple(font, hl.1) };
    let mut start = 0;
    while start < text.len() {
        let on = marks.get(start).copied().unwrap_or(false);
        let mut end = start + 1;
        while end < text.len() && (!text.is_char_boundary(end) || marks.get(end).copied().unwrap_or(false) == on) {
            end += 1;
        }
        if let Some(piece) = text.get(start..end) {
            job.append(piece, 0.0, if on { marked.clone() } else { plain.clone() });
        }
        start = end;
    }
    job
}

/// An on/off switch; returns its response (`changed()` when flipped).
pub fn switch(ui: &mut Ui, t: &Tokens, on: &mut bool) -> Response {
    let h = (control_h(t) * 0.72).round().max(14.0);
    let size = vec2(h * 1.8, h);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::Switch, ui.is_enabled(), *on, ""));
    if ui.is_rect_visible(rect) {
        let how_on = if t.knobs.reduce_motion { f32::from(u8::from(*on)) } else { ui.ctx().animate_bool_responsive(resp.id, *on) };
        let off_fill = if resp.hovered() { t.pressed } else { t.button_bg };
        let fill = theme::mix(off_fill, t.accent, how_on);
        let r = rect.height() / 2.0;
        ui.painter().rect(rect, r, fill, Stroke::new(1.0, theme::mix(t.border, t.accent, how_on)), egui::StrokeKind::Inside);
        let x = egui::lerp((rect.left() + r)..=(rect.right() - r), how_on);
        let knob = if *on { t.on_accent } else { t.text_strong };
        ui.painter().circle_filled(pos2(x, rect.center().y), r - 3.0, knob);
    }
    resp
}

/// A grid of label + control rows with the settings' columns and gaps (`egui::Grid` for forms
/// whose cells are built row by row).
pub fn grid(id: impl egui::AsIdSalt, t: &Tokens, avail: f32) -> egui::Grid {
    egui::Grid::new(id).num_columns(2).spacing([t.pad(COL_GAP), t.pad(ROW_GAP)]).min_col_width(label_w(t, avail)).min_row_height(control_h(t))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row `width` wide with a button; (the row, the button) once the row has settled.
    fn row_at(width: f32) -> (Rect, Rect) {
        let ctx = egui::Context::default();
        let t = Tokens::dark();
        let mut got = (Rect::NOTHING, Rect::NOTHING);
        for _ in 0..3 {
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(width, 600.0))), ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| {
                egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
                    let mut button = Rect::NOTHING;
                    let out = setting_row(ui, &t, Id::unique("row"), "Density", "How much space around and between things.", RowStyle::default(), |ui| {
                        button = ui.button("Comfortable").rect;
                    });
                    got = (out.rect, button);
                });
            });
            out.textures_delta.clear();
        }
        got
    }

    #[test]
    fn a_setting_puts_its_control_at_the_right_edge_or_under_it_when_narrow() {
        let (row, button) = row_at(800.0);
        assert!((button.right() - row.right()).abs() < 1.0, "right-aligned: {button:?} in {row:?}");
        assert!(button.left() > row.left() + TEXT_MIN, "beside the text: {button:?}");
        assert!(row.height() < 80.0, "one line of controls: {row:?}");
        let (row, button) = row_at(400.0);
        assert!((button.left() - row.left()).abs() < 1.0, "under the text, at its left: {button:?} in {row:?}");
        assert!(button.top() > row.top() + 30.0, "below the name and description: {button:?} in {row:?}");
    }

    #[test]
    fn highlight_marks_every_match_whatever_the_case() {
        let job = highlight("Code font and Font", &["font".to_owned()], FontId::proportional(12.0), Color32::WHITE, (Color32::RED, Color32::BLACK), 300.0);
        let marked: Vec<&str> = job
            .sections
            .iter()
            .filter(|s| s.format.background == Color32::RED)
            .filter_map(|s| job.text.get(s.byte_range.start.0..s.byte_range.end.0))
            .collect();
        assert_eq!(marked, ["font", "Font"]);
        // Multi-byte text stays whole.
        let job = highlight("Rosé Pine", &["pine".to_owned()], FontId::proportional(12.0), Color32::WHITE, (Color32::RED, Color32::BLACK), 300.0);
        assert_eq!(job.text, "Rosé Pine");
    }

    /// The slider: a click on its track sets the value there, snapped to the step and kept
    /// in the range.
    #[test]
    fn the_slider_snaps_and_stays_in_range() {
        let ctx = egui::Context::default();
        let mut v = 6.0_f32;
        let frame = |v: &mut f32, events: Vec<egui::Event>| {
            let mut out = ctx.run_ui(egui::RawInput { events, ..Default::default() }, |ui| {
                slider(ui, &Tokens::light(), v, 0.0..=16.0, 1.0, |x| format!("{x:.0} pt"));
            });
            out.textures_delta.clear();
        };
        frame(&mut v, vec![]);
        // The track runs from x = 8 to x = 152 (knob radius in from the 160-point control).
        let click = |x: f32| {
            let at = pos2(x, 14.0);
            vec![
                egui::Event::PointerMoved(at),
                egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE },
                egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE },
            ]
        };
        frame(&mut v, click(152.0));
        assert!((v - 16.0).abs() < f32::EPSILON, "the end: {v}");
        frame(&mut v, click(8.0 + 144.0 * 0.52));
        assert!((v - 8.0).abs() < f32::EPSILON, "snapped to a whole point: {v}");
        frame(&mut v, click(500.0));
        assert!(v <= 16.0, "never past the range: {v}");
    }
}
