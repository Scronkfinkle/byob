//! Drawing a free arrangement ([`super::arrange`]) and arranging it by hand.

use egui::{Color32, Id, Rect, Sense, Stroke, StrokeKind, vec2};

use super::arrange::{Arrangement, Module, Spot};
use crate::ByobApp;
use crate::theme::{Tokens, mix};

/// Arranging by hand: each module shows a handle to drag it by, a corner to resize it and a
/// button to hide it, and a bar at the top offers arrangements, hidden parts and Done.
#[derive(Default)]
pub struct Arranging {
    pub on: bool,
    /// The module being dragged, how, and where it was when the drag began.
    drag: Option<(Module, Grip, Rect)>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Grip {
    Move,
    Resize,
}

/// The chat and message box make room for open panes over them, as the docked layout
/// does: they end where a pane on their right begins (or begin where one on their left
/// ends), unless that would leave them too narrow. And nothing starts under the header,
/// nor the chat under the message box: a part grown to fit its bezel and label (a metal
/// header is taller than its place) would cover the top of what's below it, label and all.
fn make_room(mut open: Vec<(Module, Rect)>) -> Vec<(Module, Rect)> {
    const GAP: f32 = 10.0;
    let panes: Vec<Rect> = open.iter().filter(|(m, _)| matches!(m, Module::Pane | Module::Plugins)).map(|(_, r)| *r).collect();
    for (m, r) in &mut open {
        if !matches!(m, Module::Chat | Module::Composer) {
            continue;
        }
        for p in &panes {
            if !r.intersects(*p) {
                continue;
            }
            let mut room = *r;
            if p.center().x >= r.center().x {
                room.max.x = room.max.x.min(p.min.x - GAP);
            } else {
                room.min.x = room.min.x.max(p.max.x + GAP);
            }
            if room.width() >= m.min_size().x {
                *r = room;
            }
        }
    }
    let find = |open: &[(Module, Rect)], want: Module| open.iter().find(|(m, _)| *m == want).map(|(_, r)| *r);
    let (header, composer) = (find(&open, Module::Header), find(&open, Module::Composer));
    for (m, r) in &mut open {
        let mut room = *r;
        if let Some(h) = header
            && *m != Module::Header
            && r.intersect(h).is_positive()
            && r.top() > h.top()
        {
            room.min.y = room.min.y.max(h.bottom() + GAP);
        }
        if let Some(c) = composer
            && *m == Module::Chat
            && r.intersect(c).is_positive()
            && c.top() > r.top()
        {
            room.max.y = room.max.y.min(c.top() - GAP);
        }
        if room.height() >= m.min_size().y {
            *r = room;
        }
    }
    open
}

impl ByobApp {
    /// The chat screen is drawn free this frame (never on phones, in Settings or with a
    /// maximized artifact: those keep their own screens).
    pub(crate) fn free_now(&mut self, narrow: bool) -> bool {
        self.prefs.cockpit.arrangement.free
            && !narrow
            && !self.show_settings
            && !self.artifacts.is_maximized()
            && self.artifacts.web_mut().maximized().is_none()
    }

    /// Start arranging by hand: free, from byob's floating arrangement if there was none.
    pub(crate) fn start_arranging(&mut self) {
        let a = &mut self.prefs.cockpit.arrangement;
        if a.places.is_empty()
            && let Some(p) = Arrangement::preset("floating")
        {
            *a = p;
        }
        a.free = true;
        self.arranging.on = true;
        self.prefs_gen = self.prefs_gen.wrapping_add(1);
    }

    /// Back to byob's usual docked panels (the free places are kept for next time).
    pub(crate) fn dock(&mut self) {
        self.prefs.cockpit.arrangement.free = false;
        self.arranging = Arranging::default();
        self.prefs_gen = self.prefs_gen.wrapping_add(1);
    }

    /// The chat screen, free: each module in its bezel where the arrangement puts it.
    pub(crate) fn free_frame(&mut self, ui: &mut egui::Ui) {
        let t = self.tokens;
        let ctx = ui.ctx().clone();
        let screen = ui.max_rect();
        if !self.stage.active(&self.prefs.cockpit) {
            ui.painter().rect_filled(screen, 0.0, mix(t.sidebar_bg.to_opaque(), Color32::BLACK, 0.35));
        }
        crate::upload::drop_overlay(&ctx, &t);
        let arrangement = self.prefs.cockpit.arrangement.clone();
        let mut open: Vec<(Module, Rect)> = Vec::new();
        for m in Module::ALL {
            if let Some(rect) = arrangement.rect(m, screen)
                && self.module_open(m)
            {
                open.push((m, rect));
            }
        }
        let open = if self.arranging.on { open } else { make_room(open) };
        // The chat list's module carries the window controls (its top bar) while it is
        // shown, as when docked: the header shows them only while it isn't.
        self.sidebar_docked = open.iter().any(|(m, _)| *m == Module::Chats);
        for (m, rect) in open {
            let inner = super::arrange::paint(ui.painter(), rect, arrangement.bezel(m), arrangement.label(m), &t);
            super::mark_exact(&ctx, rect);
            let builder = egui::UiBuilder::new().max_rect(inner).id_salt(("byob-module", m.id())).layout(egui::Layout::top_down(egui::Align::Min));
            ui.scope_builder(builder, |ui| {
                ui.set_clip_rect(inner.intersect(screen));
                self.module(ui, m, rect);
            });
        }
        if self.arranging.on {
            self.arranging_handles(&ctx, screen, &t);
        }
    }

    /// Whether `m` has something to show now (the panes only while open, the chat list
    /// while it isn't collapsed).
    fn module_open(&mut self, m: Module) -> bool {
        match m {
            Module::Chats => self.sidebar.is_open(false),
            Module::Pane => self.artifacts.is_open(),
            Module::Plugins => self.plugins.sites.pane_is_open(),
            Module::Header | Module::Chat | Module::Composer => true,
        }
    }

    fn module(&mut self, ui: &mut egui::Ui, m: Module, rect: Rect) {
        let t = self.tokens;
        match m {
            Module::Header => {
                let layout = crate::header::Layout::for_width(ui.available_width()).shaped(ui.ctx());
                ui.add_space(((ui.available_height() - crate::icons::button_height(ui)) / 2.0).max(0.0));
                self.header_row(ui, layout);
                crate::layout::set_header_rect(ui.ctx(), rect);
            }
            Module::Chats => {
                let sent = self.sidebar.show(ui, &t, false);
                self.send_all(sent);
            }
            Module::Chat => {
                self.plugins.trust_card(ui, &t, &self.entries);
                self.transcript(ui);
            }
            Module::Composer => self.composer(ui),
            Module::Pane => self.artifacts.pane(ui, &t, false),
            Module::Plugins => self.plugins.sites.pane(ui, &t, false, self.artifacts.web_mut()),
        }
    }

    /// Handles over every placed module (the closed ones too, so they can be placed), and the
    /// bar of arrangements and hidden parts.
    fn arranging_handles(&mut self, ctx: &egui::Context, screen: Rect, t: &Tokens) {
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.arranging.on = false;
            return;
        }
        let mut changed = false;
        let area = egui::Area::new(Id::unique("byob-arranging")).order(egui::Order::Foreground).fixed_pos(screen.min).interactable(true);
        area.show(ctx, |ui| {
            ui.set_min_size(screen.size());
            let a = self.prefs.cockpit.arrangement.clone();
            for m in Module::ALL {
                let Some(rect) = a.rect(m, screen) else {
                    continue;
                };
                changed |= self.handle(ui, m, rect, screen, t);
            }
        });
        let bar_at = egui::pos2(screen.center().x - 330.0, screen.top() + screen.height() * 0.22);
        egui::Area::new(Id::unique("byob-arranging-bar")).order(egui::Order::Tooltip).default_pos(bar_at).movable(true).show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Arranging").strong()).on_hover_text("Drag this bar by its edge to move it");
                    ui.label(egui::RichText::new("drag a part to move it, its corner to resize it").color(t.text_dim));
                    ui.separator();
                    for (name, describe) in Arrangement::PRESETS {
                        if ui.small_button(name).on_hover_text(describe).clicked()
                            && let Some(p) = Arrangement::preset(name)
                        {
                            self.prefs.cockpit.arrangement = p;
                            changed = true;
                        }
                    }
                    let hidden: Vec<Module> = Module::ALL.into_iter().filter(|m| self.prefs.cockpit.arrangement.spot(*m) == Spot::Hidden).collect();
                    if !hidden.is_empty() {
                        ui.separator();
                        for m in hidden {
                            if ui.small_button(format!("Show {}", m.label().to_lowercase())).clicked() {
                                self.prefs.cockpit.arrangement.places.remove(m.id());
                                changed = true;
                            }
                        }
                    }
                    ui.separator();
                    if ui.button("Dock everything").on_hover_text("byob's usual panels (this arrangement is kept for next time)").clicked() {
                        self.dock();
                    }
                    if ui.button("Done").clicked() {
                        self.arranging.on = false;
                    }
                });
            });
        });
        if changed {
            self.prefs_gen = self.prefs_gen.wrapping_add(1);
        }
    }

    /// One module's handle; whether its place changed for good (a drag ended, it was hidden).
    fn handle(&mut self, ui: &mut egui::Ui, m: Module, rect: Rect, screen: Rect, t: &Tokens) -> bool {
        let id = Id::unique(("byob-arrange", m.id()));
        let grip = Rect::from_min_size(rect.max - vec2(20.0, 20.0), vec2(20.0, 20.0));
        let body = ui.interact(rect, id.with("move"), Sense::drag());
        let corner = ui.interact(grip, id.with("size"), Sense::drag());
        let close = Rect::from_min_size(rect.right_top() + vec2(-26.0, 6.0), vec2(20.0, 20.0));
        let hide = ui.interact(close, id.with("hide"), Sense::click());
        let active = self.arranging.drag.is_some_and(|(d, _, _)| d == m);
        let p = ui.painter();
        let tint = if active || body.hovered() { 0.22 } else { 0.1 };
        p.rect(rect, 6.0, t.accent.gamma_multiply(tint), Stroke::new(if active { 2.0 } else { 1.0 }, t.accent), StrokeKind::Inside);
        let open = if self.module_open(m) { "" } else { " (closed now)" };
        let title = p.layout_no_wrap(format!("{}{open}", m.label()), egui::FontId::proportional(t.text_size(13.0)), t.text_strong);
        let chip = Rect::from_min_size(rect.min + vec2(8.0, 8.0), title.size() + vec2(24.0, 6.0));
        p.rect_filled(chip, 4.0, mix(t.panel_bg.to_opaque(), t.accent, 0.25));
        // A grip of six dots, then the name.
        for (col, row) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0), (0.0, 2.0), (1.0, 2.0)] {
            p.circle_filled(chip.left_center() + vec2(6.0 + col * 4.0, (row - 1.0) * 4.0), 1.2, t.text_dim);
        }
        p.galley(chip.min + vec2(18.0, 3.0), title, t.text_strong);
        // The corner: three diagonal lines.
        for k in 0..3 {
            #[allow(clippy::cast_precision_loss)] // 0..3
            let o = 5.0 + 4.0 * k as f32;
            p.line_segment([grip.right_bottom() - vec2(o, 3.0), grip.right_bottom() - vec2(3.0, o)], Stroke::new(1.5, t.accent));
        }
        p.text(close.center(), egui::Align2::CENTER_CENTER, "×", egui::FontId::proportional(16.0), if hide.hovered() { t.danger } else { t.text_dim });
        if hide.clicked() {
            self.prefs.cockpit.arrangement.places.insert(m.id().to_owned(), Spot::Hidden);
            return true;
        }
        if corner.drag_started() {
            self.arranging.drag = Some((m, Grip::Resize, rect));
        } else if body.drag_started() {
            self.arranging.drag = Some((m, Grip::Move, rect));
        }
        let Some((d, how, start)) = self.arranging.drag.filter(|(d, _, _)| *d == m) else {
            return false;
        };
        let delta = ui.input(|i| i.pointer.press_origin().zip(i.pointer.interact_pos()).map(|(a, b)| b - a)).unwrap_or_default();
        let moved = match how {
            Grip::Move => start.translate(delta),
            Grip::Resize => Rect::from_min_max(start.min, (start.max + delta).max(start.min + d.min_size())),
        };
        self.prefs.cockpit.arrangement.place(d, moved, screen);
        if body.drag_stopped() || corner.drag_stopped() || !ui.input(|i| i.pointer.any_down()) {
            self.arranging.drag = None;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::pos2;

    fn r(x0: f32, y0: f32, x1: f32, y1: f32) -> Rect {
        Rect::from_min_max(pos2(x0, y0), pos2(x1, y1))
    }

    /// An open pane over the chat's right side: the chat and message box end before it; the
    /// chat list, on the left and not over them, stays; a pane leaving the chat too narrow
    /// doesn't squeeze it.
    #[test]
    fn the_chat_makes_room_for_open_panes() {
        let open = vec![
            (Module::Chats, r(0.0, 0.0, 200.0, 800.0)),
            (Module::Chat, r(220.0, 0.0, 1000.0, 700.0)),
            (Module::Composer, r(220.0, 700.0, 1000.0, 800.0)),
            (Module::Pane, r(600.0, 0.0, 1000.0, 800.0)),
        ];
        let out = make_room(open);
        assert_eq!(out[1].1, r(220.0, 0.0, 590.0, 700.0));
        assert_eq!(out[2].1, r(220.0, 700.0, 590.0, 800.0));
        assert_eq!(out[0].1, r(0.0, 0.0, 200.0, 800.0));
        let squeezed = make_room(vec![(Module::Chat, r(220.0, 0.0, 1000.0, 700.0)), (Module::Plugins, r(300.0, 0.0, 1000.0, 800.0))]);
        assert_eq!(squeezed[0].1, r(220.0, 0.0, 1000.0, 700.0), "too narrow to make room: overlapped instead");
    }
    /// Regression: a metal, labeled header (taller than its place) covered the top of the
    /// chat list and the chat, and with it their labels.
    #[test]
    fn nothing_starts_under_the_header_or_the_message_box() {
        let r = |x0: f32, y0: f32, x1: f32, y1: f32| Rect::from_min_max(egui::pos2(x0, y0), egui::pos2(x1, y1));
        let out = make_room(vec![
            (Module::Header, r(14.0, 8.0, 1426.0, 96.0)),
            (Module::Chats, r(14.0, 69.0, 302.0, 802.0)),
            (Module::Chat, r(317.0, 69.0, 1426.0, 720.0)),
            (Module::Composer, r(317.0, 700.0, 1426.0, 802.0)),
        ]);
        let get = |m: Module| out.iter().find(|(o, _)| *o == m).map(|(_, r)| *r).unwrap_or(Rect::NOTHING);
        assert_eq!(get(Module::Chats).top(), 106.0);
        assert_eq!(get(Module::Chat).top(), 106.0);
        assert_eq!(get(Module::Chat).bottom(), 690.0);
        for (i, (_, a)) in out.iter().enumerate() {
            for (_, b) in out.iter().skip(i + 1) {
                assert!(!a.intersects(*b), "{a:?} and {b:?} overlap");
            }
        }
    }
}
