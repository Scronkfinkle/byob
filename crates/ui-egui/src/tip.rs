//! Tooltips in two shapes, short and scannable:
//! - [`keys`]: what a control does and its keys, one row each ("Send  ↵", "New line  ⇧ ↵"),
//!   the keys drawn as small keycaps on the right;
//! - [`note`]: a plain title and a dimmer line under it with the second way to use it
//!   ("Speak instead of typing" / "Click again when done · Esc to drop").
//!
//! Key names follow the platform: ⌘ ⌥ ⇧ on macOS, Ctrl Alt Shift elsewhere.

use egui::{Response, RichText, Sense, Stroke, Ui, vec2};

use crate::theme::Tokens;

fn mac(ctx: &egui::Context) -> bool {
    matches!(ctx.os(), egui::os::OperatingSystem::Mac | egui::os::OperatingSystem::IOS)
}

/// The command key: "⌘" on macOS, "Ctrl" elsewhere.
pub fn cmd(ctx: &egui::Context) -> &'static str {
    if mac(ctx) { "⌘" } else { "Ctrl" }
}

/// Shift: "⇧" on macOS, "Shift" elsewhere.
pub fn shift(ctx: &egui::Context) -> &'static str {
    if mac(ctx) { "⇧" } else { "Shift" }
}

/// Enter.
pub const ENTER: &str = "↵";

/// One key as a small keycap.
pub fn keycap(ui: &mut Ui, t: &Tokens, key: &str) {
    let font = egui::TextStyle::Small.resolve(ui.style());
    let g = ui.painter().layout_no_wrap(key.to_owned(), font, t.text_dim);
    let size = vec2((g.size().x + 10.0).max(20.0), (g.size().y + 4.0).max(18.0));
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let p = ui.painter();
    p.rect_stroke(rect, t.round(4.0), Stroke::new(1.0, t.border), egui::StrokeKind::Inside);
    p.galley(rect.center() - g.size() / 2.0, g, t.text_dim);
}

/// Rows of what it does and its keys.
pub fn keys(resp: Response, t: &Tokens, rows: &[(&str, &[&str])]) -> Response {
    let t = *t;
    let rows: Vec<(String, Vec<String>)> = rows.iter().map(|(l, k)| ((*l).to_owned(), k.iter().map(|s| (*s).to_owned()).collect())).collect();
    resp.on_hover_ui(move |ui| {
        // As wide as its longest row (a tooltip from a narrow rail would get squeezed).
        let font = egui::TextStyle::Body.resolve(ui.style());
        let w = rows
            .iter()
            .map(|(l, ks)| ui.painter().layout_no_wrap(l.clone(), font.clone(), t.text).size().x + 20.0 + 24.0 * ks.len() as f32)
            .fold(0.0, f32::max);
        ui.set_min_width(w);
        egui::Grid::new("tip-keys").num_columns(2).spacing([20.0, 6.0]).show(ui, |ui| {
            for (label, ks) in &rows {
                ui.label(RichText::new(label).color(t.text_strong));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 3.0;
                    for k in ks.iter().rev() {
                        keycap(ui, &t, k);
                    }
                });
                ui.end_row();
            }
        });
    })
}

/// A title and a dimmer line under it.
pub fn note(resp: Response, t: &Tokens, title: &str, sub: &str) -> Response {
    let t = *t;
    let (title, sub) = (title.to_owned(), sub.to_owned());
    resp.on_hover_ui(move |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.label(RichText::new(&title).color(t.text_strong));
        if !sub.is_empty() {
            ui.label(RichText::new(&sub).small().color(t.text_dim));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_names_follow_the_platform() {
        let ctx = egui::Context::default();
        ctx.set_os(egui::os::OperatingSystem::Mac);
        assert_eq!((cmd(&ctx), shift(&ctx)), ("⌘", "⇧"));
        ctx.set_os(egui::os::OperatingSystem::Windows);
        assert_eq!((cmd(&ctx), shift(&ctx)), ("Ctrl", "Shift"));
        ctx.set_os(egui::os::OperatingSystem::Nix);
        assert_eq!((cmd(&ctx), shift(&ctx)), ("Ctrl", "Shift"));
    }
}
