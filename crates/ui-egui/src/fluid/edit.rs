//! Edit mode: point at anything to see what it is, click to select it, drag to move it,
//! and change it in the inspector (or ask byob to). F2 (or Ctrl+Shift+E) turns it on and off.

use std::sync::Arc;

use egui::{Color32, Context, FontId, Key, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Vec2};

use super::doc::{FluidDoc, Insert, Rule};
use super::{Element, inspector_id, overlay_id, with};
use crate::theme::Tokens;

/// The editor's own state (the app keeps it).
#[derive(Default)]
pub struct Editor {
    pub selected: Option<String>,
    ask: String,
    add: String,
    find: String,
    scene: String,
    history: Vec<FluidDoc>,
    last_push: f64,
    drag: Option<Drag>,
}

/// A drag in progress: moving an element, or scaling it by its corner.
#[derive(Clone)]
enum Drag {
    Move { path: String, start: Vec2 },
    Scale { path: String, start: f32, center: Pos2, from: f32 },
}

/// What the editor asks of the app.
#[derive(Default)]
pub struct Out {
    /// Send this to the model (the user asked byob to change something).
    pub ask: Option<String>,
}

/// Turn edit mode on or off with its keys; whether it's on.
pub fn toggle_keys(ctx: &Context) -> bool {
    let pressed = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::F2) || i.consume_key(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, Key::E));
    with(ctx, |f| {
        if pressed {
            f.editing = !f.editing;
            f.dragging = None;
        }
        f.editing
    })
    .unwrap_or(false)
}

fn describe(e: &Element) -> String {
    let label: String = e.label.chars().take(40).collect::<String>().replace('\n', " ");
    if label.is_empty() || e.is_region() { format!("{} [{}]", e.path, e.role) } else { format!("{} [{} {label:?}]", e.path, e.role) }
}

fn dashed(p: &egui::Painter, r: Rect, c: Color32) {
    let s = Stroke::new(1.0, c);
    for seg in [[r.left_top(), r.right_top()], [r.right_top(), r.right_bottom()], [r.right_bottom(), r.left_bottom()], [r.left_bottom(), r.left_top()]] {
        p.extend(egui::Shape::dashed_line(&seg, s, 4.0, 3.0));
    }
}

impl Editor {
    fn push(&mut self, doc: &FluidDoc, now: f64, force: bool) {
        if force || now - self.last_push > 0.6 {
            self.history.push(doc.clone());
            if self.history.len() > 60 {
                self.history.remove(0);
            }
        }
        self.last_push = now;
    }

    /// Draw the overlay and inspector and handle what's done in them.
    pub fn show(&mut self, ctx: &Context, t: &Tokens) -> Out {
        let mut out = Out::default();
        let Some((els, doc)) = with(ctx, |f| (f.elements.clone(), Arc::clone(&f.doc))) else { return out };
        let mut next = (*doc).clone();
        let now = ctx.input(|i| i.time);
        let screen = ctx.content_rect();
        let pointer = ctx.pointer_hover_pos();
        let accent = t.accent;
        let over_inspector = ctx.layer_id_at(pointer.unwrap_or(Pos2::ZERO)).is_some_and(|l| l.id == inspector_id());
        let rank = stacking(ctx);
        let hovered = pointer.filter(|_| !over_inspector).and_then(|p| pick(&els, p, &rank)).cloned();

        // The overlay: takes every click and drag while editing.
        let resp = egui::Area::new(overlay_id())
            .order(egui::Order::Foreground)
            .fixed_pos(screen.min)
            .show(ctx, |ui| {
                let (rect, resp) = ui.allocate_exact_size(screen.size(), Sense::click_and_drag());
                let p = ui.painter();
                p.rect_filled(rect, 0.0, Color32::from_black_alpha(18));
                for e in els.iter().filter(|e| e.rule.as_ref().is_some_and(|r| r.hidden)) {
                    dashed(p, e.shown, t.text_faint);
                }
                for e in els.iter().filter(|e| e.is_region() && !e.hidden) {
                    p.rect_stroke(e.shown, 2.0, Stroke::new(1.0, accent.gamma_multiply(0.18)), StrokeKind::Outside);
                }
                if let Some(e) = &hovered {
                    p.rect_stroke(e.shown, 3.0, Stroke::new(1.5, accent.gamma_multiply(0.8)), StrokeKind::Outside);
                    let tag = describe(e);
                    let font = FontId::monospace(11.0);
                    let galley = p.layout_no_wrap(tag, font, Color32::WHITE);
                    let at = Pos2::new(
                        e.shown.left().clamp(screen.left(), (screen.right() - galley.size().x - 8.0).max(screen.left())),
                        (e.shown.top() - galley.size().y - 6.0).max(screen.top()),
                    );
                    let bg = Rect::from_min_size(at, galley.size() + Vec2::new(8.0, 4.0));
                    p.rect_filled(bg, 3.0, Color32::from_black_alpha(210));
                    p.galley(at + Vec2::new(4.0, 2.0), galley, Color32::WHITE);
                }
                if let Some(e) = self.selected.as_deref().and_then(|s| els.iter().find(|e| e.path == s)) {
                    p.rect_stroke(e.shown, 3.0, Stroke::new(2.5, accent), StrokeKind::Outside);
                    for c in [e.shown.left_top(), e.shown.right_top(), e.shown.left_bottom(), e.shown.right_bottom()] {
                        p.rect_filled(Rect::from_center_size(c, Vec2::splat(6.0)), 1.0, accent);
                    }
                }
                resp
            })
            .inner;

        // Select, and drag to move.
        if resp.clicked()
            && let Some(e) = &hovered
        {
            self.selected = Some(e.path.clone());
        }
        // Dragging the selection's corner handle scales it; dragging anything else moves it.
        let selected_el = self.selected.as_deref().and_then(|s| els.iter().find(|e| e.path == s));
        let near_corner = |p: Pos2| selected_el.is_some_and(|e| e.shown.right_bottom().distance(p) < 10.0);
        let on_corner = pointer.is_some_and(near_corner);
        // What a drag takes is decided where the button went down (it starts a few points on).
        let origin = ctx.input(|i| i.pointer.press_origin()).or(pointer);
        let at_origin = origin.and_then(|p| pick(&els, p, &rank)).cloned();
        if resp.drag_started() {
            let on_corner = origin.is_some_and(near_corner);
            if let (true, Some(e)) = (on_corner, selected_el) {
                let start = next.rules.get(&e.path).and_then(|r| r.scale).unwrap_or(1.0);
                let center = e.shown.center();
                let from = pointer.map_or(1.0, |p| p.distance(center)).max(4.0);
                self.push(&next, now, true);
                self.drag = Some(Drag::Scale { path: e.path.clone(), start, center, from });
                let path = e.path.clone();
                with(ctx, |f| f.dragging = Some(path));
            } else if let Some(e) = &at_origin {
                // The selection, when the drag starts on it; else what's under the pointer,
                // and a part of an inserted element (its button, its text) moves all of it.
                let path = if selected_el.zip(origin).is_some_and(|(s, p)| s.shown.contains(p)) {
                    self.selected.clone().unwrap_or_default()
                } else if e.path.starts_with('+') {
                    e.path.split('/').next().unwrap_or(&e.path).to_owned()
                } else {
                    e.path.clone()
                };
                self.selected = Some(path.clone());
                let start = next.rules.get(&path).map_or(Vec2::ZERO, Rule::offset);
                self.push(&next, now, true);
                self.drag = Some(Drag::Move { path: path.clone(), start });
                with(ctx, |f| f.dragging = Some(path));
            }
        }
        if let Some(drag) = self.drag.clone() {
            if resp.dragged() {
                let snap = if ctx.input(|i| i.modifiers.shift) { 8.0 } else { 1.0 };
                match drag.clone() {
                    Drag::Move { path, start } => {
                        let total = ctx.input(|i| i.pointer.press_origin().zip(i.pointer.latest_pos()).map(|(a, b)| b - a)).unwrap_or_default();
                        // Snap to whole points (and to 8 with Shift).
                        let v = start + total;
                        let r = next.rule_mut(&path);
                        r.dx = Some((v.x / snap).round() * snap);
                        r.dy = Some((v.y / snap).round() * snap);
                    }
                    Drag::Scale { path, start, center, from } => {
                        if let Some(p) = pointer {
                            let k = (start * p.distance(center) / from).clamp(0.2, 6.0);
                            next.rule_mut(&path).scale = Some((k * 100.0).round() / 100.0);
                        }
                    }
                }
            }
            // With Alt, dropped on another element it goes by that one (wherever it goes).
            let alt = ctx.input(|i| i.modifiers.alt);
            let target = match (&drag, alt, pointer) {
                (Drag::Move { path, .. }, true, Some(p)) => els
                    .iter()
                    .filter(|e| e.shown.contains(p) && !e.path.starts_with(path.as_str()) && !path.starts_with(&format!("{}/", e.path)) && !e.hidden)
                    .max_by(|a, b| over(a, b, &rank))
                    .map(|e| (e.path.clone(), side_of(e.shown, p), e.shown)),
                _ => None,
            };
            if let Some((_, side, r)) = &target {
                let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, overlay_id()));
                painter.rect_stroke(r.expand(2.0), 3.0, Stroke::new(2.0, Color32::from_rgb(250, 200, 80)), StrokeKind::Outside);
                let edge = match *side {
                    "after" => [r.right_top(), r.right_bottom()],
                    "before" => [r.left_top(), r.left_bottom()],
                    "above" => [r.left_top(), r.right_top()],
                    _ => [r.left_bottom(), r.right_bottom()],
                };
                painter.line_segment(edge, Stroke::new(4.0, Color32::from_rgb(250, 200, 80)));
            }
            if resp.drag_stopped() || !ctx.input(|i| i.pointer.any_down()) {
                if let (Drag::Move { path, .. }, Some((to, side, _))) = (&drag, target) {
                    let r = next.rule_mut(path);
                    r.place = Some(format!("{side}:{to}"));
                    r.dx = None;
                    r.dy = None;
                }
                self.drag = None;
                with(ctx, |f| f.dragging = None);
            }
        }
        if on_corner {
            ctx.set_cursor_icon(egui::CursorIcon::ResizeNwSe);
        }

        // Keys (not while typing in the inspector).
        let typing = ctx.egui_wants_keyboard_input();
        if !typing {
            let (shift, cmd) = ctx.input(|i| (i.modifiers.shift, i.modifiers.command));
            let step = if shift { 10.0 } else { 1.0 };
            let keys = ctx.input_mut(|i| {
                let mut k = Vec::new();
                for key in [
                    Key::ArrowLeft,
                    Key::ArrowRight,
                    Key::ArrowUp,
                    Key::ArrowDown,
                    Key::Delete,
                    Key::Backspace,
                    Key::H,
                    Key::R,
                    Key::Escape,
                    Key::Z,
                    Key::Plus,
                    Key::Equals,
                    Key::Minus,
                    Key::OpenBracket,
                ] {
                    if i.key_pressed(key) {
                        k.push(key);
                    }
                }
                k
            });
            for key in keys {
                if key == Key::Z && cmd {
                    if let Some(prev) = self.history.pop() {
                        next = prev;
                    }
                    continue;
                }
                if key == Key::Escape {
                    if self.selected.take().is_none() {
                        with(ctx, |f| f.editing = false);
                    }
                    continue;
                }
                let Some(path) = self.selected.clone() else { continue };
                if key == Key::OpenBracket {
                    self.selected = path.rsplit_once('/').map(|(p, _)| p.to_owned()).filter(|p| els.iter().any(|e| &e.path == p)).or(Some(path));
                    continue;
                }
                self.push(&next, now, false);
                let r = next.rule_mut(&path);
                match key {
                    Key::ArrowLeft => r.dx = Some(r.dx.unwrap_or(0.0) - step),
                    Key::ArrowRight => r.dx = Some(r.dx.unwrap_or(0.0) + step),
                    Key::ArrowUp => r.dy = Some(r.dy.unwrap_or(0.0) - step),
                    Key::ArrowDown => r.dy = Some(r.dy.unwrap_or(0.0) + step),
                    Key::Delete | Key::Backspace | Key::H => r.hidden = !r.hidden,
                    Key::R => *r = Rule::default(),
                    Key::Plus | Key::Equals => r.scale = Some((r.scale.unwrap_or(1.0) * 1.1).min(6.0)),
                    Key::Minus => r.scale = Some((r.scale.unwrap_or(1.0) / 1.1).max(0.2)),
                    _ => {}
                }
            }
        }

        self.inspector(ctx, t, &els, &mut next, &mut out, now);

        next.tidy();
        if next != *doc {
            with(ctx, |f| {
                f.doc = Arc::new(next);
                f.edited += 1;
            });
        }
        out
    }

    #[allow(clippy::too_many_lines)]
    fn inspector(&mut self, ctx: &Context, t: &Tokens, els: &[Element], doc: &mut FluidDoc, out: &mut Out, now: f64) {
        let screen = ctx.content_rect();
        egui::Area::new(inspector_id())
            .order(egui::Order::Tooltip)
            .pivot(egui::Align2::RIGHT_TOP)
            .default_pos(Pos2::new(screen.right() - 12.0, screen.top() + 52.0))
            .movable(true)
            .constrain(true)
            .show(ctx, |ui| {
                crate::menu::frame(t).inner_margin(egui::Margin::same(12)).show(ui, |ui| {
                    ui.set_width(300.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Fluid").strong().size(15.0).color(t.text_strong));
                        ui.label(RichText::new("editing").small().color(t.accent));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Done").on_hover_text("Leave edit mode (F2)").clicked() {
                                with(ctx, |f| f.editing = false);
                            }
                            if ui.add_enabled(!self.history.is_empty(), egui::Button::new("Undo").small()).on_hover_text("Ctrl+Z").clicked()
                                && let Some(prev) = self.history.pop()
                            {
                                *doc = prev;
                            }
                            if ui
                                .add_enabled(!doc.is_empty(), egui::Button::new("Reset all").small())
                                .on_hover_text("Every element back as byob draws it")
                                .clicked()
                            {
                                self.push(doc, now, true);
                                *doc = FluidDoc::default();
                            }
                        });
                    });
                    ui.label(
                        RichText::new(
                            "Click anything to select it, drag to move it (with Alt, drop it on another to keep it beside that one). Arrows nudge (Shift: 10), H hides, R resets, +/− scale, [ selects its parent.",
                        )
                        .small()
                        .color(t.text_faint),
                    );
                    ui.separator();
                    self.scenes(ui, t, doc, now);
                    ui.separator();
                    self.palette(ui, t, doc, now);
                    let Some(path) = self.selected.clone() else {
                        ui.label(RichText::new("Nothing selected: click anything").color(t.text_dim));
                        self.ask_box(ui, None, out);
                        self.finder(ui, t, els);
                        return;
                    };
                    let el = els.iter().find(|e| e.path == path);
                    // Where it sits: each ancestor, clickable.
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        let parts: Vec<&str> = path.split('/').collect();
                        for i in 0..parts.len() {
                            let p = parts[..=i].join("/");
                            let last = i + 1 == parts.len();
                            let text = RichText::new(parts[i]).monospace().size(11.0).color(if last { t.text_strong } else { t.text_dim });
                            if ui.add(egui::Button::new(text).frame(false)).clicked() && els.iter().any(|e| e.path == p) {
                                self.selected = Some(p);
                            }
                            if !last {
                                ui.label(RichText::new("/").monospace().size(11.0).color(t.text_faint));
                            }
                        }
                    });
                    match el {
                        Some(e) => {
                            let label: String = e.label.chars().take(80).collect();
                            ui.label(
                                RichText::new(if label.is_empty() || e.is_region() { e.role.clone() } else { format!("{} · {label:?}", e.role) })
                                    .small()
                                    .color(t.text_dim),
                            );
                        }
                        None => {
                            ui.label(RichText::new("Not on screen now").small().color(t.text_faint));
                        }
                    }
                    let before = doc.clone();
                    let r = doc.rule_mut(&path);
                    egui::Grid::new("fluid-fields").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
                        let f = |ui: &mut egui::Ui, name: &str, v: &mut Option<f32>, def: f32, speed: f64, range: std::ops::RangeInclusive<f32>| {
                            ui.label(name);
                            let mut x = v.unwrap_or(def);
                            ui.horizontal(|ui| {
                                if ui.add(egui::DragValue::new(&mut x).speed(speed).range(range)).changed() {
                                    *v = Some(x);
                                }
                                if v.is_some() && ui.small_button("×").on_hover_text("As byob draws it").clicked() {
                                    *v = None;
                                }
                            });
                            ui.end_row();
                        };
                        f(ui, "Right", &mut r.dx, 0.0, 1.0, -4000.0..=4000.0);
                        f(ui, "Down", &mut r.dy, 0.0, 1.0, -4000.0..=4000.0);
                        f(ui, "Scale", &mut r.scale, 1.0, 0.01, 0.2..=6.0);
                        f(ui, "Opacity", &mut r.opacity, 1.0, 0.01, 0.0..=1.0);
                        f(ui, "Text size", &mut r.size, 1.0, 0.01, 0.3..=6.0);
                        f(ui, "Corners", &mut r.radius, 6.0, 0.2, 0.0..=200.0);
                        let s = |ui: &mut egui::Ui, name: &str, v: &mut Option<String>, hint: &str| {
                            ui.label(name);
                            let mut text = v.clone().unwrap_or_default();
                            if ui.add(egui::TextEdit::singleline(&mut text).hint_text(hint).desired_width(170.0)).changed() {
                                *v = (!text.is_empty()).then_some(text);
                            }
                            ui.end_row();
                        };
                        s(ui, "Text", &mut r.text, "as byob writes it");
                        s(ui, "Color", &mut r.color, "#rrggbb or a name");
                        s(ui, "Background", &mut r.fill, "#rrggbb(aa)");
                        s(ui, "Outline", &mut r.outline, "#rrggbb");
                        s(ui, "Beside", &mut r.place, "after:PATH, below:PATH, swap:PATH…");
                        ui.label("Hidden");
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut r.hidden, "");
                            let mut hover = r.reveal.as_deref() == Some("hover");
                            if ui.checkbox(&mut hover, "only on hover").changed() {
                                r.reveal = hover.then(|| "hover".to_owned());
                            }
                        });
                        ui.end_row();
                    });
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new("Motion").small().color(t.text_dim));
                        for m in ["none", "float", "bounce", "pulse", "shake", "glow"] {
                            let on = r.motion.as_deref().unwrap_or("none") == m;
                            if ui.selectable_label(on, m).clicked() {
                                r.motion = (m != "none").then(|| m.to_owned());
                            }
                        }
                    });
                    let mut spread = false;
                    ui.horizontal(|ui| {
                        if ui.button("Reset this").clicked() {
                            *r = Rule::default();
                        }
                        spread = ui.button("Same for all like it").on_hover_text("A rule for every element with this kind of name here (a glob)").clicked();
                    });
                    if spread && let Some(rule) = doc.rules.remove(&path) {
                        doc.rule_mut(&generalize(&path)).merge(&rule);
                    }
                    if *doc != before {
                        let now_doc = doc.clone();
                        *doc = before;
                        self.push(doc, now, false);
                        *doc = now_doc;
                    }
                    ui.separator();
                    // Something new beside it.
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.add).hint_text("Add text beside it…").desired_width(170.0));
                        let mut add = |at: &str| {
                            if self.add.trim().is_empty() {
                                return;
                            }
                            self.push(doc, now, true);
                            let id = format!("note{}", doc.inserts.len() + 1);
                            let tree = serde_json::json!({"type": "text", "text": self.add.trim()});
                            let ins = serde_json::json!({"id": id, "anchor": path, "at": at, "card": true, "tree": tree, "by": "you"});
                            if let Ok(i) = serde_json::from_value::<Insert>(ins) {
                                doc.inserts.push(i);
                            }
                            self.add.clear();
                        };
                        if ui.small_button("→").on_hover_text("After it").clicked() {
                            add("after");
                        }
                        if ui.small_button("↓").on_hover_text("Below it").clicked() {
                            add("below");
                        }
                    });
                    // Or ask byob.
                    self.ask_box(ui, Some((path.as_str(), el)), out);
                    ui.separator();
                    self.finder(ui, t, els);
                });
            });
    }

    /// Ask byob for a change: to the selected element (`what`), or to the screen.
    fn ask_box(&mut self, ui: &mut egui::Ui, what: Option<(&str, Option<&Element>)>, out: &mut Out) {
        let hint = if what.is_some() {
            "Ask byob to change this… (\"move it over a bit\", \"put a timer next to it\")"
        } else {
            "Ask byob to change the screen… (\"a calmer layout\", \"a notes pad on the right\")"
        };
        let ask = ui.add(egui::TextEdit::multiline(&mut self.ask).hint_text(hint).desired_rows(2).desired_width(f32::INFINITY));
        let enter = ask.has_focus() && ui.input(|i| i.key_pressed(Key::Enter) && !i.modifiers.shift);
        if (ui.button("Ask byob").clicked() || enter) && !self.ask.trim().is_empty() {
            let context = match what {
                Some((_, Some(e))) => format!(
                    "\n\n(Selected on screen in edit mode: {} at {:.0},{:.0} {:.0}×{:.0}. Change it with ui_fluid; inspect under its parent first if you need its neighbors.)",
                    describe(e),
                    e.shown.min.x,
                    e.shown.min.y,
                    e.shown.width(),
                    e.shown.height()
                ),
                Some((path, None)) => format!("\n\n(Selected in edit mode: {path}, not on screen now. Change it with ui_fluid.)"),
                None => "\n\n(Asked from fluid's edit mode: change the screen with ui_fluid.)".to_owned(),
            };
            out.ask = Some(format!("{}{context}", self.ask.trim()));
            self.ask.clear();
        }
    }

    /// New elements, dropped near the middle of the window to be dragged where they go.
    fn palette(&mut self, ui: &mut egui::Ui, t: &Tokens, doc: &mut FluidDoc, now: f64) {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Add").small().color(t.text_dim));
            let kinds: [(&str, serde_json::Value, serde_json::Value); 4] = [
                ("Note", serde_json::json!({"type": "column", "children": [{"type": "text", "text": "{state.text}"}, {"type": "input", "name": "t", "placeholder": "Write a note…", "submit": "set:text={t}"}]}), serde_json::json!({"text": "A note"})),
                ("Clock", serde_json::json!({"type": "column", "children": [{"type": "text", "text": "{time}", "style": "title"}, {"type": "text", "text": "{date}", "tone": "muted"}]}), serde_json::json!({})),
                (
                    "Todo",
                    serde_json::json!({"type": "column", "children": [{"type": "text", "text": "Todo ({state.items})", "style": "title"}, {"type": "input", "name": "task", "placeholder": "Add a task…", "submit": "add:items={task}"}, {"type": "column", "each": "items", "item": {"type": "row", "children": [{"type": "text", "text": "{item}"}, {"type": "button", "label": "Done", "action": "drop:items={index}"}]}, "empty": {"type": "text", "text": "Nothing to do", "tone": "muted"}}]}),
                    serde_json::json!({"items": []}),
                ),
                ("Counter", serde_json::json!({"type": "row", "children": [{"type": "button", "label": "−", "action": "inc:n=-1"}, {"type": "text", "text": "{state.n}", "style": "title"}, {"type": "button", "label": "+", "action": "inc:n=1"}]}), serde_json::json!({"n": 0})),
            ];
            for (name, tree, state) in kinds {
                if ui.small_button(name).clicked() {
                    self.push(doc, now, true);
                    let n = doc.inserts.len() + 1;
                    let id = format!("{}{n}", name.to_lowercase());
                    let ins = serde_json::json!({"id": id, "at": "free", "x": 40, "y": 35, "tree": tree, "state": state, "by": "you"});
                    if let Ok(i) = serde_json::from_value::<Insert>(ins) {
                        doc.inserts.push(i);
                        self.selected = Some(format!("+{id}"));
                    }
                }
            }
        });
    }

    /// Saved scenes: click one to put it on screen, save this one under a name.
    fn scenes(&mut self, ui: &mut egui::Ui, t: &Tokens, doc: &mut FluidDoc, now: f64) {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Scenes").small().color(t.text_dim));
            let names: Vec<String> = doc.scenes.keys().cloned().collect();
            for name in names {
                let r = ui.small_button(&name).on_hover_text("Put this scene on screen (right-click: forget it)");
                if r.clicked() {
                    self.push(doc, now, true);
                    doc.load(&name);
                }
                if r.secondary_clicked() {
                    doc.scenes.remove(&name);
                }
            }
            ui.add(egui::TextEdit::singleline(&mut self.scene).hint_text("name").desired_width(70.0));
            if ui
                .add_enabled(!self.scene.trim().is_empty(), egui::Button::new("Save").small())
                .on_hover_text("Keep the screen as it is now under this name")
                .clicked()
            {
                doc.save(self.scene.trim());
                self.scene.clear();
            }
        });
    }

    /// Find an element by name or text (also hidden ones) and select it.
    fn finder(&mut self, ui: &mut egui::Ui, t: &Tokens, els: &[Element]) {
        ui.add(egui::TextEdit::singleline(&mut self.find).hint_text("Find an element…").desired_width(f32::INFINITY));
        let q = self.find.trim().to_lowercase();
        if q.is_empty() {
            return;
        }
        egui::ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
            for e in els.iter().filter(|e| e.path.to_lowercase().contains(&q) || e.label.to_lowercase().contains(&q)).take(40) {
                let text = RichText::new(describe(e)).monospace().size(10.5).color(if e.hidden { t.text_faint } else { t.text });
                if ui.add(egui::Button::new(text).frame(false).wrap_mode(egui::TextWrapMode::Truncate)).clicked() {
                    self.selected = Some(e.path.clone());
                }
            }
        });
    }
}

/// Each layer's place in the stacking, back to front (the background panels first).
fn stacking(ctx: &Context) -> std::collections::HashMap<egui::LayerId, usize> {
    ctx.memory(|m| m.layer_ids().enumerate().map(|(i, l)| (l, (l.order as usize) * 100_000 + i)).collect())
}

/// How high `e` is drawn: moved ones over the panels (on fluid's own layer).
fn height(e: &Element, rank: &std::collections::HashMap<egui::LayerId, usize>) -> usize {
    let own = rank.get(&e.layer).copied().unwrap_or((e.layer.order as usize) * 100_000);
    if e.lifted { own.max(egui::Order::Middle as usize * 100_000) } else { own }
}

/// Which of two elements under the pointer is the one pointed at: a drawn one before a
/// hidden one, then the one drawn on top (an inserted element over the chat under it),
/// then the innermost.
fn over(a: &Element, b: &Element, rank: &std::collections::HashMap<egui::LayerId, usize>) -> std::cmp::Ordering {
    (!a.hidden).cmp(&!b.hidden).then(height(a, rank).cmp(&height(b, rank))).then(b.shown.area().total_cmp(&a.shown.area())).then(a.depth.cmp(&b.depth))
}

/// The element pointed at, at `p`.
fn pick<'a>(els: &'a [Element], p: Pos2, rank: &std::collections::HashMap<egui::LayerId, usize>) -> Option<&'a Element> {
    els.iter().filter(|e| e.shown.contains(p)).max_by(|a, b| over(a, b, rank))
}

/// Which side of `r` the point `p` is nearest: after, before, above or below.
fn side_of(r: Rect, p: Pos2) -> &'static str {
    let d = [(p.x - r.right()).abs(), (p.x - r.left()).abs(), (p.y - r.top()).abs(), (p.y - r.bottom()).abs()];
    let i = (0..4).min_by(|&a, &b| d[a].total_cmp(&d[b])).unwrap_or(0);
    ["after", "before", "above", "below"][i]
}

/// `sidebar/row.3/button:Pin` → `sidebar/*/button:Pin`: the same kind of element in every
/// sibling; a numbered part (`text#2`, `row.3`) becomes `*`.
pub fn generalize(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    let n = parts.len();
    parts
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let numbered = p.rsplit_once(['#', '.']).is_some_and(|(_, d)| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()));
            if numbered {
                let (head, _) = p.rsplit_once(['#', '.']).unwrap_or((p, ""));
                if i + 1 == n { format!("{head}*") } else { "*".to_owned() }
            } else {
                (*p).to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_element_on_top_is_picked_not_the_smallest() {
        let layer = |order, name| egui::LayerId::new(order, egui::Id::unique(name));
        let (panel, insert) = (layer(egui::Order::Background, "panel"), layer(egui::Order::Middle, "insert"));
        let rank: std::collections::HashMap<egui::LayerId, usize> = [(panel, 0), (insert, egui::Order::Middle as usize * 100_000)].into_iter().collect();
        let mk = |path: &str, r: Rect, l| super::super::element(path.into(), "region".into(), String::new(), r, l, None, 0, true);
        // A small button in the chat, under a bigger inserted counter.
        let els = vec![
            mk("chat/button:OpenAI", Rect::from_min_size(Pos2::new(546.0, 327.0), Vec2::new(90.0, 25.0)), panel),
            mk("+counter1", Rect::from_min_size(Pos2::new(590.0, 310.0), Vec2::new(97.0, 47.0)), insert),
        ];
        assert_eq!(pick(&els, Pos2::new(600.0, 340.0), &rank).map(|e| e.path.as_str()), Some("+counter1"));
        // Where only the button is, the button.
        assert_eq!(pick(&els, Pos2::new(560.0, 340.0), &rank).map(|e| e.path.as_str()), Some("chat/button:OpenAI"));
    }
}
