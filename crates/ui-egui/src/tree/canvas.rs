//! The `canvas` node: a display list in a fixed box, scaled down to fit the width. Colours
//! are theme roles (`accent`, `text_dim`, `chart3`, …) or `#rrggbb`; a click sends the
//! canvas's action with the point in `_x`/`_y` (canvas units).

use byob_protocol::tree::Shape;
use egui::{Color32, Sense, Stroke, vec2};
use serde_json::{Map, Value};

use super::Cx;
use crate::theme::{self, Tokens};

/// A colour by role name or hex; `default` if none or unknown.
pub(crate) fn color(t: &Tokens, c: Option<&str>, default: Color32) -> Color32 {
    let Some(c) = c else { return default };
    t.get(c).or_else(|| theme::parse_color(c)).unwrap_or(default)
}

pub(super) fn show(ui: &mut egui::Ui, cx: &mut Cx<'_>, width: f32, height: f32, shapes: &[Shape], action: Option<&str>) {
    let t = cx.t;
    let (w, h) = (width.clamp(1.0, 4000.0), height.clamp(1.0, 4000.0));
    let scale = (ui.available_width() / w).clamp(0.05, 1.0);
    let sense = if action.is_some() { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(vec2(w, h) * scale, sense);
    if !ui.is_rect_visible(rect) {
        return;
    }
    let p = ui.painter_at(rect);
    let at = |x: f32, y: f32| rect.min + vec2(x, y) * scale;
    let none = Color32::TRANSPARENT;
    for s in shapes {
        match s {
            Shape::Rect { x, y, w, h, fill, stroke, radius } => {
                let r = egui::Rect::from_min_size(at(*x, *y), vec2(w.max(0.0), h.max(0.0)) * scale);
                let stroke = Stroke::new(1.0, color(t, stroke.as_deref(), none));
                p.rect(r, radius.max(0.0) * scale, color(t, fill.as_deref(), none), stroke, egui::StrokeKind::Inside);
            }
            Shape::Circle { x, y, r, fill, stroke } => {
                p.circle(at(*x, *y), r.max(0.0) * scale, color(t, fill.as_deref(), none), Stroke::new(1.0, color(t, stroke.as_deref(), none)));
            }
            Shape::Line { points, stroke, width } => {
                let pts = points.iter().map(|[x, y]| at(*x, *y)).collect();
                p.add(egui::Shape::line(pts, Stroke::new((width * scale).max(0.5), color(t, stroke.as_deref(), t.text))));
            }
            Shape::Text { x, y, text, color: c, size } => {
                let font = egui::FontId::proportional((size * scale).clamp(4.0, 96.0));
                p.text(at(*x, *y), egui::Align2::LEFT_TOP, text, font, color(t, c.as_deref(), t.text));
            }
        }
    }
    if let (Some(a), true) = (action, resp.clicked()) {
        let mut extra = Map::new();
        if let Some(pos) = resp.interact_pointer_pos() {
            let d = (pos - rect.min) / scale;
            let round = |v: f32| Value::from(((v * 10.0).round() / 10.0) as f64);
            extra.insert("_x".into(), round(d.x));
            extra.insert("_y".into(), round(d.y));
        }
        cx.st.fire(a, None, extra);
    }
}
