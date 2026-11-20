//! A glass material for the chat list ([`panel`], edge to edge), drawn from the theme's
//! tokens so every theme and custom colour gets it. Menus use the same fill
//! ([`crate::menu::frame`]).
//!
//! egui can't blur what is behind a window or panel, so the look is built from the cues that
//! read as glass on macOS: a surface a shade lighter than the content beside it, a soft sheen
//! fading down from the top edge, and a hairline where it meets the content. Rows on it are
//! tinted with translucent overlays of the text colour (not the accent), so selection stays
//! calm on every theme.

use egui::{Color32, Painter, Rect, Shape, Stroke, StrokeKind, pos2};

use crate::theme::{Tokens, mix};

/// Corner radius of a row on glass at the default roundness.
pub const ROW_RADIUS: f32 = 8.0;
/// How far down the sheen fades.
const SHEEN_H: f32 = 160.0;

/// The glass fill: the panel colour lifted toward white.
pub fn fill(t: &Tokens) -> Color32 {
    if t.dark { mix(t.panel_bg, Color32::WHITE, 0.045) } else { mix(t.panel_bg, Color32::WHITE, 0.62) }
}

/// A translucent overlay of the text colour: rows' hover, pressed and selected tints.
fn tint(t: &Tokens, alpha: f32) -> Color32 {
    t.text_strong.gamma_multiply(alpha)
}

/// How a row looks right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowState {
    Idle,
    Hovered,
    Pressed,
    Selected,
}

/// Paint a row's background on glass for `state` (nothing when idle).
pub fn row(painter: &Painter, t: &Tokens, rect: Rect, state: RowState) {
    let r = t.round(ROW_RADIUS);
    let (fill, edge) = match state {
        RowState::Idle => return,
        RowState::Hovered => (tint(t, if t.dark { 0.055 } else { 0.045 }), None),
        RowState::Pressed => (tint(t, if t.dark { 0.09 } else { 0.075 }), None),
        // Selected: a calm pill with a faint glassy rim.
        RowState::Selected => (tint(t, if t.dark { 0.10 } else { 0.075 }), Some(tint(t, if t.dark { 0.07 } else { 0.05 }))),
    };
    painter.rect_filled(rect, r, fill);
    if let Some(edge) = edge {
        painter.rect_stroke(rect, r, Stroke::new(1.0, edge), StrokeKind::Inside);
    }
}

/// The ring of a focused field on glass.
pub fn focus_ring(painter: &Painter, t: &Tokens, rect: Rect) {
    let r = t.round(ROW_RADIUS);
    painter.rect_filled(rect, r, if t.dark { Color32::from_black_alpha(46) } else { Color32::from_white_alpha(200) });
    painter.rect_stroke(rect, r, Stroke::new(1.0, t.accent.gamma_multiply(0.55)), StrokeKind::Inside);
}

/// A hairline separator on glass.
pub fn hairline(painter: &Painter, t: &Tokens, y: f32, left: f32, right: f32) {
    painter.line_segment([pos2(left, y), pos2(right, y)], Stroke::new(1.0, tint(t, if t.dark { 0.07 } else { 0.08 })));
}

/// Paint glass filling `rect` edge to edge (the chat list): the fill, the sheen fading
/// down from the top, and a hairline on its right where it meets the chat.
pub fn panel(painter: &Painter, t: &Tokens, rect: Rect) {
    painter.rect_filled(rect, 0, fill(t));
    let sheen = if t.dark { Color32::from_white_alpha(10) } else { Color32::from_white_alpha(130) };
    vertical_gradient(painter, Rect::from_min_max(rect.min, pos2(rect.right(), (rect.top() + SHEEN_H).min(rect.bottom()))), sheen, Color32::TRANSPARENT);
    let edge = if t.dark { Color32::from_black_alpha(120) } else { Color32::from_black_alpha(22) };
    painter.line_segment([pos2(rect.right() - 0.5, rect.top()), pos2(rect.right() - 0.5, rect.bottom())], Stroke::new(1.0, edge));
}

/// A vertical gradient over `rect`, `top` to `bottom`.
fn vertical_gradient(painter: &Painter, rect: Rect, top: Color32, bottom: Color32) {
    if rect.height() <= 0.0 || rect.width() <= 0.0 {
        return;
    }
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(rect.left_top(), top);
    mesh.colored_vertex(rect.right_top(), top);
    mesh.colored_vertex(rect.right_bottom(), bottom);
    mesh.colored_vertex(rect.left_bottom(), bottom);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    painter.add(Shape::mesh(mesh));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::contrast;

    #[test]
    fn glass_stands_apart_from_the_chat_and_text_still_reads() {
        for t in [Tokens::dark(), Tokens::light()] {
            let glass = fill(&t);
            assert_ne!(glass, t.app_bg, "dark={}", t.dark);
            assert!(contrast(t.text, glass) >= 4.5, "dark={}: body text on glass", t.dark);
            assert!(contrast(t.text_faint, glass) >= 2.0, "dark={}: faint text on glass", t.dark);
        }
    }

    #[test]
    fn paints_without_panicking_on_tiny_or_empty_rects() {
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            let t = Tokens::dark();
            for r in [Rect::NOTHING, Rect::from_min_size(pos2(0.0, 0.0), egui::vec2(1.0, 1.0)), Rect::from_min_size(pos2(0.0, 0.0), egui::vec2(240.0, 600.0))] {
                panel(ui.painter(), &t, r);
                row(ui.painter(), &t, r, RowState::Selected);
            }
        });
        out.textures_delta.clear();
    }
}
