//! Small icons drawn with shapes, for glyphs the UI fonts don't have (a settings gear, a
//! disclosure triangle).

use egui::{Color32, Pos2, Rect, Response, Sense, Shape, Stroke, Ui, pos2, vec2};

use crate::theme::Tokens;

/// "Settings": three slider lines with knobs.
fn sliders(painter: &egui::Painter, r: Rect, color: Color32) {
    let stroke = Stroke::new(1.4, color);
    let (l, w) = (r.left() + r.width() * 0.15, r.width() * 0.7);
    for (i, knob) in [0.7_f32, 0.3, 0.55].into_iter().enumerate() {
        #[allow(clippy::cast_precision_loss)] // 0..3
        let y = r.top() + r.height() * (0.25 + 0.25 * i as f32);
        painter.line_segment([pos2(l, y), pos2(l + w, y)], stroke);
        painter.circle(pos2(l + w * knob, y), 2.2, color, Stroke::NONE);
    }
}

/// A frameless button showing the settings icon.
pub fn settings_button(ui: &mut Ui, t: &Tokens) -> Response {
    let size = vec2(20.0, 20.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let color = if resp.hovered() { t.text_strong } else { t.text_dim };
        if resp.hovered() {
            ui.painter().rect_filled(rect, t.round(4.0), t.hover);
        }
        sliders(ui.painter(), rect.shrink(3.0), color);
    }
    resp
}

/// A small triangle pointing right (`open` = false) or down, centred in `rect`.
pub fn disclosure(painter: &egui::Painter, rect: Rect, open: bool, color: Color32) {
    let c = rect.center();
    let s = rect.width().min(rect.height()) * 0.28;
    let points: Vec<Pos2> = if open {
        vec![pos2(c.x - s, c.y - s * 0.6), pos2(c.x + s, c.y - s * 0.6), pos2(c.x, c.y + s * 0.8)]
    } else {
        vec![pos2(c.x - s * 0.6, c.y - s), pos2(c.x + s * 0.8, c.y), pos2(c.x - s * 0.6, c.y + s)]
    };
    painter.add(Shape::convex_polygon(points, color, Stroke::NONE));
}
