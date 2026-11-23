//! Menus in one style: a glass popup ([`crate::glass`]) of full-width rows, each an icon,
//! a label and an optional one-key shortcut (P, R, D…: [`key`] acts on it while the menu is
//! open), with a light pill under the pointer.
//!
//! Where it opens matters: a menu from a row's "⋯" is anchored to that button, right edges
//! lined up, so it opens over the list it belongs to and not over the chat; it opens below
//! the button, or above it when there's no room below. Every menu is exactly as wide as its
//! longest row ([`fit`]), measured before it opens so it is placed right from its first
//! frame (egui places a popup by its last size). Destructive items are red, sit last after a
//! separator, and ask in a [`Confirm`] dialog before they act. Labels are short words with
//! no "…" ("Rename", "Pin", "Delete").
//!
//! egui draws it all, so it looks the same on macOS, Windows, Linux and in the browser.

use egui::{Color32, CornerRadius, Frame, Margin, Popup, RectAlign, Response, Sense, Stroke, Ui, pos2, vec2};

use crate::glass;
use crate::icons::Paint;
use crate::theme::Tokens;

/// A row's height.
const ROW_H: f32 = 30.0;
/// The icon's square and the space around it.
const ICON: f32 = 15.0;
const PAD: f32 = 9.0;
/// Narrowest and widest a menu gets ([`fit`]).
const MIN_WIDTH: f32 = 150.0;
const MAX_WIDTH: f32 = 320.0;

/// The popup's frame: a glass card with a soft shadow.
pub fn frame(t: &Tokens) -> Frame {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // a few points
    let r = t.round(11.0).round().clamp(0.0, 255.0) as u8;
    Frame::NONE
        .fill(if t.dark { crate::theme::mix(glass::fill(t), Color32::WHITE, 0.03) } else { Color32::WHITE })
        .stroke(Stroke::new(1.0, if t.dark { Color32::from_white_alpha(22) } else { Color32::from_black_alpha(24) }))
        .corner_radius(CornerRadius::same(r))
        .inner_margin(Margin::same(5))
        .shadow(egui::epaint::Shadow { offset: [0, 8], blur: 28, spread: 0, color: Color32::from_black_alpha(if t.dark { 110 } else { 34 }) })
}

/// The width a menu of `items` (label, shortcut) needs: its longest row, no wider. Menus get
/// a fixed width (see the module docs), so it is measured before the menu opens.
pub fn fit(ui: &Ui, items: &[(&str, Option<&str>)]) -> f32 {
    fit_with(ui.ctx(), ui.style(), items)
}

/// [`fit`] where only the context is at hand (a menu opened from a button's response).
pub fn fit_ctx(ctx: &egui::Context, items: &[(&str, Option<&str>)]) -> f32 {
    fit_with(ctx, &ctx.global_style(), items)
}

fn fit_with(ctx: &egui::Context, style: &egui::Style, items: &[(&str, Option<&str>)]) -> f32 {
    let body = egui::TextStyle::Body.resolve(style);
    let small = egui::TextStyle::Small.resolve(style);
    let widest = items
        .iter()
        .map(|(label, keys)| {
            let text = ctx.fonts_mut(|f| f.layout_no_wrap((*label).to_owned(), body.clone(), Color32::WHITE).size().x);
            let keys = keys.map_or(0.0, |k| ctx.fonts_mut(|f| f.layout_no_wrap(k.to_owned(), small.clone(), Color32::WHITE).size().x) + 24.0);
            PAD + ICON + PAD + text + keys + PAD
        })
        .fold(0.0, f32::max);
    // The frame's inner margin on both sides.
    (widest + 10.0).clamp(MIN_WIDTH, MAX_WIDTH).ceil()
}

/// What a destructive action asks before it runs: a dialog with a title, what will happen,
/// an optional checkbox, Cancel and the action in red. Esc or a click outside cancels;
/// Enter confirms.
pub struct Confirm<'a> {
    pub title: &'a str,
    pub body: &'a str,
    pub action: &'a str,
    pub check: Option<(&'a str, &'a mut bool)>,
}

/// What the person chose in a [`Confirm`] dialog this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Waiting,
    Cancel,
    Confirm,
}

impl Confirm<'_> {
    pub fn show(self, ctx: &egui::Context, t: &Tokens, id: egui::Id) -> Choice {
        let Confirm { title, body, action, check } = self;
        let mut choice = Choice::Waiting;
        let resp = egui::Modal::new(id).frame(frame(t).inner_margin(Margin::same(18))).show(ctx, |ui| {
            ui.set_width(340.0);
            ui.label(egui::RichText::new(title).strong().size(t.text_size(16.0)).color(t.text_strong));
            ui.add_space(6.0);
            ui.add(egui::Label::new(egui::RichText::new(body).color(t.text_dim)).wrap());
            if let Some((label, value)) = check {
                ui.add_space(8.0);
                ui.checkbox(value, egui::RichText::new(label).color(t.text));
            }
            ui.add_space(14.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let danger =
                    egui::Button::new(egui::RichText::new(action).color(t.on_accent)).fill(t.danger).corner_radius(t.round(8.0)).min_size(vec2(84.0, 30.0));
                if ui.add(danger).clicked() || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    choice = Choice::Confirm;
                }
                let cancel =
                    egui::Button::new(egui::RichText::new("Cancel").color(t.text)).fill(t.button_bg).corner_radius(t.round(8.0)).min_size(vec2(84.0, 30.0));
                if ui.add(cancel).clicked() {
                    choice = Choice::Cancel;
                }
            });
        });
        if choice == Choice::Waiting && resp.should_close() {
            choice = Choice::Cancel;
        }
        choice
    }
}

/// Below the button, right edges aligned; above it when there's no room below.
const ANCHORS: &[RectAlign] = &[RectAlign::BOTTOM_END, RectAlign::TOP_END];

/// [`Popup::menu`] for `button` in this style and `width`, anchored as [`ANCHORS`] say.
pub fn below<'a>(button: &'a Response, t: &Tokens, width: f32) -> Popup<'a> {
    Popup::menu(button).align(RectAlign::BOTTOM_END).align_alternatives(ANCHORS).gap(4.0).width(width).frame(frame(t))
}

/// Below the button, left edges aligned (a title's menu); above it when there's no room below.
const ANCHORS_BELOW_START: &[RectAlign] = &[RectAlign::BOTTOM_START, RectAlign::TOP_START];

/// [`Popup::menu`] below a button on the left of a bar, left edges aligned.
pub fn below_left<'a>(button: &'a Response, t: &Tokens, width: f32) -> Popup<'a> {
    Popup::menu(button).align(RectAlign::BOTTOM_START).align_alternatives(ANCHORS_BELOW_START).gap(4.0).width(width).frame(frame(t))
}

/// Left edges aligned with the button, above it (a control near the window's bottom, like
/// the composer's), or below when there's no room above.
const ANCHORS_START: &[RectAlign] = &[RectAlign::TOP_START, RectAlign::BOTTOM_START];

/// [`Popup::menu`] for a button on the left of something: left edges aligned, opening
/// above it when there's room (see [`ANCHORS_START`]).
pub fn from_left<'a>(button: &'a Response, t: &Tokens, width: f32) -> Popup<'a> {
    Popup::menu(button).align(RectAlign::TOP_START).align_alternatives(ANCHORS_START).gap(4.0).width(width).frame(frame(t))
}

/// [`Popup::context_menu`] (right click, long press: at the pointer) in this style and `width`.
pub fn context<'a>(target: &'a Response, t: &Tokens, width: f32) -> Popup<'a> {
    Popup::context_menu(target).width(width).frame(frame(t))
}

/// Was `k` pressed (alone) while the menu is open? Consumes it. Menus show the letter as the
/// item's shortcut and act on it like a click.
pub fn key(ui: &Ui, k: egui::Key) -> bool {
    ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, k))
}

/// One item. Returns its response (`clicked()`): the caller acts, then calls `ui.close()`.
pub fn item(ui: &mut Ui, t: &Tokens, icon: Option<Paint>, label: &str, shortcut: Option<&str>) -> Response {
    row(ui, t, icon.as_ref().map(|f| f as &dyn Fn(&egui::Painter, egui::Rect, egui::Color32)), label, shortcut, false)
}

/// An item whose mark isn't one of the icons (a color square, a status dot): `mark` draws
/// it in the icon's place, given that square and the icon color.
pub fn item_marked(ui: &mut Ui, t: &Tokens, mark: &dyn Fn(&egui::Painter, egui::Rect, egui::Color32), label: &str, shortcut: Option<&str>) -> Response {
    row(ui, t, Some(mark), label, shortcut, false)
}

/// A destructive item, in the error color (it should ask with [`Confirm`] before acting).
pub fn danger(ui: &mut Ui, t: &Tokens, icon: Option<Paint>, label: &str, shortcut: Option<&str>) -> Response {
    row(ui, t, icon.as_ref().map(|f| f as &dyn Fn(&egui::Painter, egui::Rect, egui::Color32)), label, shortcut, true)
}

fn row(
    ui: &mut Ui,
    t: &Tokens,
    icon: Option<&dyn Fn(&egui::Painter, egui::Rect, egui::Color32)>,
    label: &str,
    shortcut: Option<&str>,
    danger: bool,
) -> Response {
    let width = ui.available_width();
    let h = (ui.text_style_height(&egui::TextStyle::Body) + 12.0).max(ROW_H).round();
    let (rect, resp) = ui.allocate_exact_size(vec2(width, h), Sense::click());
    if ui.is_rect_visible(rect) {
        let over = resp.hovered();
        let state = if resp.is_pointer_button_down_on() {
            glass::RowState::Pressed
        } else if over {
            glass::RowState::Hovered
        } else {
            glass::RowState::Idle
        };
        glass::row(ui.painter(), t, rect, state);
        let color = match (danger, over) {
            (true, _) => t.danger,
            (false, true) => t.text_strong,
            (false, false) => t.text,
        };
        let icon_color = if danger {
            t.danger
        } else if over {
            t.text_strong
        } else {
            t.text_dim
        };
        let mut x = rect.left() + PAD;
        if let Some(paint) = icon {
            paint(ui.painter(), egui::Rect::from_center_size(pos2(x + ICON / 2.0, rect.center().y), vec2(ICON, ICON)), icon_color);
            x += ICON + PAD;
        }
        let keys = shortcut.map(|k| ui.painter().layout_no_wrap(k.to_owned(), egui::TextStyle::Small.resolve(ui.style()), t.text_faint));
        let keys_w = keys.as_ref().map_or(0.0, |k| k.size().x + 16.0);
        let mut job = egui::text::LayoutJob::simple_singleline(label.to_owned(), egui::TextStyle::Body.resolve(ui.style()), color);
        job.wrap = egui::text::TextWrapping::truncate_at_width((rect.right() - PAD - keys_w - x).max(1.0));
        let galley = ui.painter().layout_job(job);
        ui.painter().galley(pos2(x, rect.center().y - galley.size().y / 2.0), galley, color);
        if let Some(k) = keys {
            ui.painter().galley(pos2(rect.right() - PAD - k.size().x, rect.center().y - k.size().y / 2.0), k, t.text_faint);
        }
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A thin line between groups of items.
pub fn separator(ui: &mut Ui, t: &Tokens) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 9.0), Sense::hover());
    glass::hairline(ui.painter(), t, rect.center().y, rect.left() + 4.0, rect.right() - 4.0);
}

/// A small heading over a group ("New chat in").
pub fn heading(ui: &mut Ui, t: &Tokens, text: &str) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::hover());
    let g = ui.painter().layout_no_wrap(text.to_owned(), egui::TextStyle::Small.resolve(ui.style()), t.text_faint);
    ui.painter().galley(pos2(rect.left() + PAD, rect.bottom() - g.size().y - 3.0), g, t.text_faint);
}

/// Rows sit flush in a menu.
pub fn tighten(ui: &mut Ui) {
    ui.spacing_mut().item_spacing.y = 1.0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_click_and_draw_on_both_themes() {
        for t in [Tokens::dark(), Tokens::light()] {
            let ctx = egui::Context::default();
            let mut clicked = false;
            let mut frame_at = |events: Vec<egui::Event>| {
                let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 300.0))), events, ..Default::default() };
                let mut out = ctx.run_ui(input, |ui| {
                    tighten(ui);
                    ui.set_width(200.0);
                    item(ui, &t, Some(crate::icons::pencil), "Rename", Some("F2"));
                    separator(ui, &t);
                    heading(ui, &t, "Group");
                    clicked |= danger(ui, &t, Some(crate::icons::trash), "Delete", Some("D")).clicked();
                });
                out.textures_delta.clear();
            };
            frame_at(vec![]);
            // Rename, separator (9), heading (24), then Delete: click the middle of Delete.
            let at = pos2(50.0, 30.0 + 1.0 + 9.0 + 1.0 + 24.0 + 1.0 + 15.0);
            let press = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
            frame_at(vec![egui::Event::PointerMoved(at)]);
            frame_at(vec![press(true)]);
            frame_at(vec![press(false)]);
            assert!(clicked, "dark={}", t.dark);
        }
    }
}
