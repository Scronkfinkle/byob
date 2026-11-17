//! Small icons drawn with shapes, for glyphs the UI fonts don't have (the app settings
//! gear, chat settings sliders, the chat list and overflow buttons, a disclosure triangle,
//! where a workspace runs its commands, the plugins puzzle piece, the chat list's plus,
//! search, clear and folders, the settings tabs, and the composer's paperclip, send arrow and
//! stop square).

use byob_protocol::workspace::EnvKind;
use egui::{Color32, Pos2, Rect, Response, Sense, Shape, Stroke, Ui, pos2, vec2};

use crate::theme::Tokens;

/// A shape drawn into a square: (painter, rect, colour).
pub type Paint = fn(&egui::Painter, Rect, Color32);

/// "Chat settings": three slider lines with knobs.
pub fn sliders(painter: &egui::Painter, r: Rect, color: Color32) {
    let stroke = Stroke::new(1.4, color);
    let (l, w) = (r.left() + r.width() * 0.15, r.width() * 0.7);
    for (i, knob) in [0.7_f32, 0.3, 0.55].into_iter().enumerate() {
        #[allow(clippy::cast_precision_loss)] // 0..3
        let y = r.top() + r.height() * (0.25 + 0.25 * i as f32);
        painter.line_segment([pos2(l, y), pos2(l + w, y)], stroke);
        painter.circle(pos2(l + w * knob, y), 2.2, color, Stroke::NONE);
    }
}

/// "App settings": a cog with eight teeth.
pub fn gear(painter: &egui::Painter, r: Rect, color: Color32) {
    let s = r.width().min(r.height());
    let c = r.center();
    let stroke = Stroke::new((s * 0.12).max(1.3), color);
    for k in 0..8 {
        #[allow(clippy::cast_precision_loss)] // 0..8
        let a = k as f32 * std::f32::consts::TAU / 8.0;
        let dir = vec2(a.cos(), a.sin());
        painter.line_segment([c + dir * s * 0.3, c + dir * s * 0.47], Stroke::new((s * 0.17).max(1.8), color));
    }
    painter.circle_stroke(c, s * 0.3, stroke);
    painter.circle_stroke(c, s * 0.1, Stroke::new((s * 0.09).max(1.0), color));
}

/// "Chat list": a window with a sidebar.
pub fn sidebar(painter: &egui::Painter, r: Rect, color: Color32) {
    let s = r.width().min(r.height());
    let stroke = Stroke::new((s * 0.09).max(1.2), color);
    let b = Rect::from_center_size(r.center(), vec2(s * 0.92, s * 0.76));
    painter.rect_stroke(b, s * 0.12, stroke, egui::StrokeKind::Inside);
    let x = b.left() + b.width() * 0.36;
    painter.line_segment([pos2(x, b.top()), pos2(x, b.bottom())], stroke);
    for i in 0..3 {
        #[allow(clippy::cast_precision_loss)] // 0..3
        let y = b.top() + b.height() * (0.3 + 0.18 * i as f32);
        painter.line_segment([pos2(b.left() + s * 0.12, y), pos2(x - s * 0.1, y)], Stroke::new(stroke.width * 0.8, color));
    }
}

/// "More": three dots in a row.
pub fn more(painter: &egui::Painter, r: Rect, color: Color32) {
    let s = r.width().min(r.height());
    let c = r.center();
    for dx in [-0.32_f32, 0.0, 0.32] {
        painter.circle_filled(c + vec2(dx * s, 0.0), (s * 0.085).max(1.4), color);
    }
}

/// "Plugins": a jigsaw piece with a knob on top and one on the right.
pub fn puzzle(painter: &egui::Painter, r: Rect, color: Color32) {
    use std::f32::consts::PI;
    let s = r.width().min(r.height());
    let stroke = Stroke::new((s * 0.1).max(1.2), color);
    let o = r.center() - vec2(s * 0.47, s * 0.53);
    let at = |x: f32, y: f32| o + vec2(x * s, y * s);
    let (left, top, right, bottom, knob) = (0.1_f32, 0.32_f32, 0.72_f32, 0.92_f32, 0.11_f32);
    let mut points = vec![at(left, bottom), at(left, top), at(0.41 - knob, top)];
    for k in 0..=8 {
        #[allow(clippy::cast_precision_loss)] // 0..=8
        let a = PI + PI * k as f32 / 8.0;
        points.push(at(0.41 + knob * a.cos(), top + knob * 1.3 * a.sin()));
    }
    points.extend([at(right, top), at(right, 0.62 - knob)]);
    for k in 0..=8 {
        #[allow(clippy::cast_precision_loss)] // 0..=8
        let a = -PI / 2.0 + PI * k as f32 / 8.0;
        points.push(at(right + knob * 1.3 * a.cos(), 0.62 + knob * a.sin()));
    }
    points.push(at(right, bottom));
    painter.add(Shape::closed_line(points, stroke));
}

/// "New": a plus.
pub fn plus(painter: &egui::Painter, r: Rect, color: Color32) {
    let s = r.width().min(r.height());
    let c = r.center();
    let (h, stroke) = (s * 0.36, Stroke::new((s * 0.11).max(1.3), color));
    painter.line_segment([c - vec2(h, 0.0), c + vec2(h, 0.0)], stroke);
    painter.line_segment([c - vec2(0.0, h), c + vec2(0.0, h)], stroke);
}

/// "Search": a magnifying glass.
pub fn search(painter: &egui::Painter, r: Rect, color: Color32) {
    let s = r.width().min(r.height());
    let stroke = Stroke::new((s * 0.1).max(1.3), color);
    let c = r.center() - vec2(s * 0.08, s * 0.08);
    let radius = s * 0.28;
    painter.circle_stroke(c, radius, stroke);
    let d = vec2(1.0, 1.0).normalized();
    painter.line_segment([c + d * radius, c + d * s * 0.5], Stroke::new(stroke.width * 1.15, color));
}

/// "Clear" / "close": a small ×.
pub fn cross(painter: &egui::Painter, r: Rect, color: Color32) {
    let s = r.width().min(r.height());
    let c = r.center();
    let (h, stroke) = (s * 0.26, Stroke::new((s * 0.1).max(1.3), color));
    painter.line_segment([c + vec2(-h, -h), c + vec2(h, h)], stroke);
    painter.line_segment([c + vec2(-h, h), c + vec2(h, -h)], stroke);
}

/// "Folder": a folder with a tab.
pub fn folder(painter: &egui::Painter, r: Rect, color: Color32) {
    let s = r.width().min(r.height());
    let stroke = Stroke::new((s * 0.08).max(1.3), color);
    let o = r.center() - vec2(s * 0.5, s * 0.5);
    let at = |x: f32, y: f32| o + vec2(x * s, y * s);
    let outline = vec![at(0.08, 0.2), at(0.38, 0.2), at(0.48, 0.32), at(0.92, 0.32), at(0.92, 0.82), at(0.08, 0.82)];
    painter.add(Shape::closed_line(outline, stroke));
    painter.line_segment([at(0.08, 0.42), at(0.92, 0.42)], stroke);
}

/// "New folder": a folder with a plus in it.
pub fn folder_plus(painter: &egui::Painter, r: Rect, color: Color32) {
    folder(painter, r, color);
    let s = r.width().min(r.height());
    let c = r.center() + vec2(0.0, s * 0.12);
    let (h, stroke) = (s * 0.13, Stroke::new((s * 0.08).max(1.3), color));
    painter.line_segment([c - vec2(h, 0.0), c + vec2(h, 0.0)], stroke);
    painter.line_segment([c - vec2(0.0, h), c + vec2(0.0, h)], stroke);
}

/// "Attach": a paperclip, leaning right.
pub fn paperclip(painter: &egui::Painter, r: Rect, color: Color32) {
    use std::f32::consts::PI;
    let s = r.width().min(r.height());
    let stroke = Stroke::new((s * 0.085).max(1.3), color);
    // An upright clip in a unit square (three straight runs joined by U-turns), turned 40°.
    let mut points: Vec<Pos2> = vec![pos2(0.44, 0.34)];
    let mut arc = |c: Pos2, radius: f32, from: f32, to: f32| {
        for k in 0..=10 {
            #[allow(clippy::cast_precision_loss)] // 0..=10
            let a = from + (to - from) * k as f32 / 10.0;
            points.push(c + vec2(a.cos(), a.sin()) * radius);
        }
    };
    arc(pos2(0.52, 0.66), 0.08, PI, 0.0);
    arc(pos2(0.47, 0.24), 0.13, 0.0, -PI);
    arc(pos2(0.5, 0.74), 0.16, PI, 0.0);
    points.push(pos2(0.66, 0.4));
    let (sin, cos) = 0.7_f32.sin_cos();
    let c = r.center();
    let points = points
        .into_iter()
        .map(|p| {
            let d = (p - pos2(0.5, 0.5)) * s * 1.08;
            c + vec2(d.x * cos - d.y * sin, d.x * sin + d.y * cos)
        })
        .collect();
    painter.add(Shape::line(points, stroke));
}

/// "Send": an arrow pointing up.
pub fn arrow_up(painter: &egui::Painter, r: Rect, color: Color32) {
    let s = r.width().min(r.height());
    // A whole number of pixels wide, on the pixel grid: an odd width centred on a pixel, an
    // even one between two, so the stem is crisp instead of smeared over three columns.
    let ppp = painter.pixels_per_point();
    let px = (s * 0.11 * ppp).round().max(2.0);
    let stroke = Stroke::new(px / ppp, color);
    let cx = if px % 2.0 == 1.0 { painter.round_to_pixel_center(r.center().x) } else { (r.center().x * ppp).round() / ppp };
    let snap = |y: f32| (y * ppp).round() / ppp;
    let (top, bottom, wing) = (snap(r.center().y - s * 0.3), snap(r.center().y + s * 0.3), snap(s * 0.27));
    painter.line_segment([pos2(cx, bottom), pos2(cx, top + stroke.width * 0.5)], stroke);
    painter.add(egui::epaint::PathShape::line(vec![pos2(cx - wing, top + wing), pos2(cx, top), pos2(cx + wing, top + wing)], stroke));
}

/// "Stop": a filled square with softened corners.
pub fn stop(painter: &egui::Painter, r: Rect, color: Color32) {
    let s = r.width().min(r.height());
    painter.rect_filled(Rect::from_center_size(r.center(), vec2(s * 0.56, s * 0.56)), s * 0.1, color);
}

/// "Models": a chip with two pins on each side.
pub fn chip(painter: &egui::Painter, r: Rect, color: Color32) {
    let s = r.width().min(r.height());
    let stroke = Stroke::new((s * 0.09).max(1.2), color);
    let body = Rect::from_center_size(r.center(), vec2(s * 0.58, s * 0.58));
    painter.rect_stroke(body, s * 0.08, stroke, egui::StrokeKind::Middle);
    painter.rect_filled(Rect::from_center_size(r.center(), vec2(s * 0.2, s * 0.2)), s * 0.03, color);
    for f in [0.36_f32, 0.64] {
        let (x, y) = (r.left() + s * f + (r.width() - s) / 2.0, r.top() + s * f + (r.height() - s) / 2.0);
        painter.line_segment([pos2(x, body.top()), pos2(x, body.top() - s * 0.14)], stroke);
        painter.line_segment([pos2(x, body.bottom()), pos2(x, body.bottom() + s * 0.14)], stroke);
        painter.line_segment([pos2(body.left(), y), pos2(body.left() - s * 0.14, y)], stroke);
        painter.line_segment([pos2(body.right(), y), pos2(body.right() + s * 0.14, y)], stroke);
    }
}

/// "Appearance": a circle, half filled (light and dark).
pub fn contrast(painter: &egui::Painter, r: Rect, color: Color32) {
    let s = r.width().min(r.height());
    let (c, radius) = (r.center(), s * 0.38);
    painter.circle_stroke(c, radius, Stroke::new((s * 0.09).max(1.2), color));
    let half: Vec<Pos2> = (0..=16)
        .map(|k| {
            #[allow(clippy::cast_precision_loss)] // 0..=16
            let a = -std::f32::consts::FRAC_PI_2 + std::f32::consts::PI * k as f32 / 16.0;
            c + vec2(a.cos(), a.sin()) * radius
        })
        .collect();
    painter.add(Shape::convex_polygon(half, color, Stroke::NONE));
}

/// "Text and layout": a big and a small A.
pub fn text_size(painter: &egui::Painter, r: Rect, color: Color32) {
    let s = r.width().min(r.height());
    let stroke = Stroke::new((s * 0.09).max(1.2), color);
    let a = |left: f32, w: f32, h: f32| {
        let base = r.center().y + s * 0.34;
        let (l, rt, top) = (pos2(left, base), pos2(left + w, base), pos2(left + w / 2.0, base - h));
        painter.line(vec![l, top, rt], stroke);
        let y = base - h * 0.38;
        let k = 0.38 * 0.5;
        painter.line_segment([pos2(left + w * k, y), pos2(left + w * (1.0 - k), y)], stroke);
    };
    let x0 = r.center().x - s * 0.46;
    a(x0, s * 0.52, s * 0.68);
    a(x0 + s * 0.56, s * 0.36, s * 0.44);
}

/// "Colours": three overlapping circles.
pub fn colours(painter: &egui::Painter, r: Rect, color: Color32) {
    let s = r.width().min(r.height());
    let stroke = Stroke::new((s * 0.08).max(1.1), color);
    let c = r.center();
    for (dx, dy) in [(0.0_f32, -0.14_f32), (-0.15, 0.12), (0.15, 0.12)] {
        painter.circle_stroke(c + vec2(dx * s, dy * s), s * 0.24, stroke);
    }
}

/// A small chevron pointing down (menus, split buttons), centred in `rect`.
pub fn chevron_down(painter: &egui::Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let s = rect.width().min(rect.height()) * 0.22;
    let stroke = Stroke::new(1.4, color);
    painter.line(vec![pos2(c.x - s, c.y - s * 0.5), pos2(c.x, c.y + s * 0.5), pos2(c.x + s, c.y - s * 0.5)], stroke);
}

/// A small chevron pointing right (a row that opens a page), centred in `rect`.
pub fn chevron_right(painter: &egui::Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let s = rect.width().min(rect.height()) * 0.22;
    let stroke = Stroke::new(1.4, color);
    painter.line(vec![pos2(c.x - s * 0.5, c.y - s), pos2(c.x + s * 0.5, c.y), pos2(c.x - s * 0.5, c.y + s)], stroke);
}

/// "Back": an arrow pointing left.
pub fn arrow_left(painter: &egui::Painter, r: Rect, color: Color32) {
    let s = r.width().min(r.height());
    let c = r.center();
    let stroke = Stroke::new((s * 0.1).max(1.3), color);
    let (h, head) = (s * 0.32, s * 0.24);
    painter.line_segment([c + vec2(-h, 0.0), c + vec2(h, 0.0)], stroke);
    painter.line(vec![c + vec2(-h + head, -head), c + vec2(-h, 0.0), c + vec2(-h + head, head)], stroke);
}

/// The height of a text button (what egui gives `ui.button`).
pub fn button_height(ui: &Ui) -> f32 {
    let text = ui.text_style_height(&egui::TextStyle::Button);
    ui.spacing().interact_size.y.max(text + 2.0 * ui.spacing().button_padding.y)
}

/// A header-style button: an icon and, if `label` is given, text after it. Frameless until
/// hovered; filled while `selected`.
pub fn icon_button(ui: &mut Ui, t: &Tokens, paint: Paint, label: Option<&str>, selected: bool) -> Response {
    // Exactly a button's height, so it lines up with the buttons and labels beside it.
    let height = button_height(ui);
    let icon = t.text_size(16.0).min(height - 2.0).max(8.0);
    let pad = 5.0;
    let color = if selected { t.text_strong } else { t.text };
    let galley = label.map(|l| ui.painter().layout_no_wrap(l.to_owned(), egui::TextStyle::Button.resolve(ui.style()), color));
    let text_w = galley.as_ref().map_or(0.0, |g| g.size().x + 6.0);
    let size = vec2(pad + icon + text_w + pad, height);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let fill = if resp.is_pointer_button_down_on() {
            Some(t.pressed)
        } else if resp.hovered() {
            Some(t.hover)
        } else {
            selected.then_some(t.selection)
        };
        if let Some(fill) = fill {
            ui.painter().rect_filled(rect, t.round(4.0), fill);
        }
        let icon_rect = Rect::from_min_size(pos2(rect.left() + pad, rect.center().y - icon / 2.0), vec2(icon, icon));
        let icon_color = if resp.hovered() || selected { t.text_strong } else { t.text_dim };
        paint(ui.painter(), icon_rect, icon_color);
        if let Some(g) = galley {
            let pos = pos2(icon_rect.right() + 6.0, rect.center().y - g.size().y / 2.0);
            ui.painter().galley(pos, g, color);
        }
    }
    resp
}

/// A frameless button showing the chat settings icon.
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

/// Where a workspace runs its commands, as an icon: a box (container), a snowflake (Nix dev
/// shell), a monitor (this machine) or a sparkle (Auto: decided per folder).
pub fn env(painter: &egui::Painter, rect: Rect, kind: EnvKind, color: Color32) {
    let r = rect.shrink(rect.width() * 0.1);
    let s = r.width().min(r.height());
    let stroke = Stroke::new((s * 0.11).max(1.2), color);
    let at = |x: f32, y: f32| pos2(r.left() + x * s, r.top() + y * s);
    match kind {
        EnvKind::Container => {
            // An isometric box: its outline, and the three edges meeting at the front corner.
            let outline = vec![at(0.5, 0.06), at(0.92, 0.28), at(0.92, 0.74), at(0.5, 0.96), at(0.08, 0.74), at(0.08, 0.28)];
            painter.add(Shape::closed_line(outline, stroke));
            painter.line(vec![at(0.08, 0.28), at(0.5, 0.5), at(0.92, 0.28)], stroke);
            painter.line_segment([at(0.5, 0.5), at(0.5, 0.96)], stroke);
        }
        EnvKind::Nix => {
            // A snowflake: three bars through the centre, each end with a small V.
            let c = r.center();
            for k in 0..6 {
                #[allow(clippy::cast_precision_loss)] // 0..6
                let a = k as f32 * std::f32::consts::TAU / 6.0 - std::f32::consts::FRAC_PI_2;
                let dir = vec2(a.cos(), a.sin());
                let tip = c + dir * s * 0.46;
                painter.line_segment([c, tip], stroke);
                let mid = c + dir * s * 0.3;
                for side in [-1.0_f32, 1.0] {
                    let b = a + side * 0.75;
                    painter.line_segment([mid, mid + vec2(b.cos(), b.sin()) * s * 0.14], stroke);
                }
            }
        }
        EnvKind::Host => {
            // A monitor on a stand.
            painter.rect_stroke(Rect::from_min_max(at(0.06, 0.14), at(0.94, 0.7)), s * 0.06, stroke, egui::StrokeKind::Inside);
            painter.line_segment([at(0.5, 0.7), at(0.5, 0.86)], stroke);
            painter.line_segment([at(0.28, 0.88), at(0.72, 0.88)], stroke);
        }
        EnvKind::Auto => {
            // A four-pointed sparkle.
            let c = r.center();
            let (o, i) = (s * 0.46, s * 0.12);
            let points = vec![
                c + vec2(0.0, -o),
                c + vec2(i, -i),
                c + vec2(o, 0.0),
                c + vec2(i, i),
                c + vec2(0.0, o),
                c + vec2(-i, i),
                c + vec2(-o, 0.0),
                c + vec2(-i, -i),
            ];
            painter.add(Shape::closed_line(points, stroke));
        }
    }
}

/// The colour an environment's icon is drawn in.
pub fn env_color(t: &Tokens, kind: EnvKind) -> Color32 {
    match kind {
        EnvKind::Container => t.link,
        EnvKind::Nix => t.accent,
        EnvKind::Host => t.text_dim,
        EnvKind::Auto => t.text_faint,
    }
}

/// One line on where commands run, for the icon's tooltip.
pub fn env_tip(kind: EnvKind) -> &'static str {
    match kind {
        EnvKind::Container => "Container: commands run in this workspace's Docker/Podman container",
        EnvKind::Nix => "Nix dev shell: commands run with the folder's Nix environment loaded",
        EnvKind::Host => "This machine: commands run here, in byob's sandbox",
        EnvKind::Auto => "Auto: byob checks the folder (flake, devcontainer…) and asks once",
    }
}
