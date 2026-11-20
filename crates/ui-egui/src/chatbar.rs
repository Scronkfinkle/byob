//! The row under the message box: where the chat works on the left; how the next answer is
//! made on the right, in the order people reach for them: the model with its effort, the
//! mode (what byob may do without asking), and how full the context is.
//!
//! Inside the box stays only what acts on the message itself (attach, the text, send). These
//! are chosen while writing, so they sit next to it, not in the header.

use byob_protocol::profiles::{Effort, Profile};
use byob_protocol::workspace::{Approvals, Level};
use byob_protocol::{ClientMsg, ConfigScope, ConfigView, Link};
use egui::{Rect, Response, Sense, Ui, pos2, vec2};

use crate::icons::{self, Paint};
use crate::theme::Tokens;
use crate::{ByobApp, glass, menu};

/// The modes, mildest first: each says what happens, not "auto" or "manual". They are the
/// chat's approvals setting, so this menu and the chat's settings never disagree.
pub const MODES: [(Approvals, &str, &str); 4] = [
    (Approvals::ReadOnly, "Read only", "Reads files, changes nothing"),
    (Approvals::Ask, "Ask before changes", "Reads freely, asks before changes"),
    (Approvals::AllowEdits, "Edit freely", "Edits files, asks before commands"),
    (Approvals::AllowAll, "Work freely", "Runs everything, asks what you confirm"),
];

/// The mode's short name on the chip.
pub fn mode_name(a: Approvals) -> &'static str {
    match a {
        Approvals::Yolo => "Never ask",
        other => MODES.iter().find(|m| m.0 == other).map_or("Ask before changes", |m| m.1),
    }
}

/// An effort as people say it.
pub fn effort_name(e: Effort) -> &'static str {
    match e {
        Effort::Low => "Low",
        Effort::Medium => "Medium",
        Effort::High => "High",
        Effort::Xhigh => "Extra",
        Effort::Max => "Max",
    }
}

/// Where a model runs, for the menu's groups.
pub fn place(p: &Profile) -> &'static str {
    if byob_types::cli_agent(&p.base_url).is_some() {
        return "Your plan";
    }
    let rest = p.base_url.split_once("://").map_or(p.base_url.as_str(), |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().map_or(authority, |h| if h == "::1" { "[::1]" } else { h }),
        None => authority.split(':').next().unwrap_or(authority),
    };
    if matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "0.0.0.0") || host.ends_with(".localhost") { "On this computer" } else { "Online" }
}

/// The chat's own settings, as the engine last said (for the mode chip).
#[derive(Default)]
pub struct ChatConfig {
    view: Option<ConfigView>,
    /// The chat the view was asked for (`Some(None)`: a new chat), so a switch asks again.
    asked: Option<Option<String>>,
}

impl ChatConfig {
    /// Keep the engine's answer for the chat scope (others still get the message).
    pub fn received(&mut self, v: &ConfigView) {
        if v.scope == ConfigScope::Chat {
            self.view = Some(v.clone());
        }
    }

    /// Ask again after a reconnect.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// A message asking for the chat's settings when the chat in view changed.
    pub fn ask(&mut self, chat: Option<&str>) -> Option<ClientMsg> {
        let now = chat.map(str::to_owned);
        if self.asked.as_ref() == Some(&now) {
            return None;
        }
        self.asked = Some(now);
        self.view = None;
        Some(ClientMsg::GetConfig { scope: ConfigScope::Chat })
    }

    pub fn view(&self) -> Option<&ConfigView> {
        self.view.as_ref()
    }
}

/// A quiet button: an optional icon, a label with a dimmer second part, and a chevron when it
/// opens a menu. Background only under the pointer, like the sidebar's rows.
/// What leads a chip's label.
#[derive(Clone, Copy)]
enum Lead {
    Nothing,
    Icon(Paint),
    /// A status dot in this colour.
    Dot(egui::Color32),
}

fn chip(ui: &mut Ui, t: &Tokens, lead: Lead, main: &str, dim: Option<&str>, menu: bool, max_text: f32) -> Response {
    let font = egui::TextStyle::Body.resolve(ui.style());
    let mut job = egui::text::LayoutJob::default();
    let fmt = |color| egui::TextFormat { font_id: font.clone(), color, ..Default::default() };
    job.append(main, 0.0, fmt(t.text_dim));
    if let Some(d) = dim {
        job.append(&format!(" {d}"), 0.0, fmt(t.text_faint));
    }
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_text.max(24.0));
    let galley = ui.painter().layout_job(job);
    let h = icons::button_height(ui);
    let (pad, chev) = (7.0, if menu { 14.0 } else { 0.0 });
    let icon_w = if matches!(lead, Lead::Nothing) { 0.0 } else { 19.0 };
    let w = pad + icon_w + galley.size().x + chev + pad;
    let sense = if menu { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(vec2(w, h), sense);
    if ui.is_rect_visible(rect) {
        let state = if menu && resp.is_pointer_button_down_on() {
            glass::RowState::Pressed
        } else if menu && (resp.hovered() || egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp))) {
            glass::RowState::Hovered
        } else {
            glass::RowState::Idle
        };
        glass::row(ui.painter(), t, rect, state);
        let color = if state == glass::RowState::Idle { t.text_dim } else { t.text_strong };
        let mut x = rect.left() + pad;
        match lead {
            Lead::Icon(paint) => {
                paint(ui.painter(), Rect::from_center_size(pos2(x + 7.0, rect.center().y), vec2(14.0, 14.0)), color);
                x += icon_w;
            }
            Lead::Dot(c) => {
                ui.painter().circle_filled(pos2(x + 5.0, rect.center().y), 3.5, c);
                x += icon_w - 4.0;
            }
            Lead::Nothing => {}
        }
        ui.painter().galley(pos2(x, rect.center().y - galley.size().y / 2.0), galley.clone(), color);
        if menu {
            let at = pos2(x + galley.size().x + 8.0, rect.center().y);
            icons::chevron_down(ui.painter(), Rect::from_center_size(at, vec2(10.0, 10.0)), t.text_faint);
        }
    }
    if menu { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}

/// The place picker's button (a menu).
pub fn place_chip(ui: &mut Ui, t: &Tokens, paint: Paint, label: &str, room: f32) -> Response {
    chip(ui, t, Lead::Icon(paint), label, None, true, room)
}

/// What sits at the right end of a menu row.
#[derive(Clone, Copy)]
enum Trail<'a> {
    Nothing,
    /// A key that picks it ("1").
    Key(&'a str),
    /// The current choice: a check, then its key.
    Check(&'a str),
    /// A value and an arrow (it opens a submenu).
    Value(&'a str),
}

/// The keys 1…9, by position.
const NUMBER_KEYS: [(egui::Key, &str); 9] = [
    (egui::Key::Num1, "1"),
    (egui::Key::Num2, "2"),
    (egui::Key::Num3, "3"),
    (egui::Key::Num4, "4"),
    (egui::Key::Num5, "5"),
    (egui::Key::Num6, "6"),
    (egui::Key::Num7, "7"),
    (egui::Key::Num8, "8"),
    (egui::Key::Num9, "9"),
];

/// A menu row: a label (and a dimmer note after it), an optional line under it saying what
/// it does, and a trail at the right end.
fn pick_row(ui: &mut Ui, t: &Tokens, label: &str, note: Option<&str>, what: Option<&str>, trail: Trail, warn: bool) -> Response {
    let width = ui.available_width();
    let body = egui::TextStyle::Body.resolve(ui.style());
    let small = egui::TextStyle::Small.resolve(ui.style());
    let line_h = ui.text_style_height(&egui::TextStyle::Body);
    let h = if what.is_some() { (line_h * 2.0 + 16.0).max(44.0) } else { (line_h + 12.0).max(30.0) };
    let (rect, resp) = ui.allocate_exact_size(vec2(width, h.round()), Sense::click());
    if ui.is_rect_visible(rect) {
        let state = if resp.is_pointer_button_down_on() {
            glass::RowState::Pressed
        } else if resp.hovered() {
            glass::RowState::Hovered
        } else {
            glass::RowState::Idle
        };
        glass::row(ui.painter(), t, rect, state);
        let strong = if warn {
            t.warning
        } else if state == glass::RowState::Idle {
            t.text
        } else {
            t.text_strong
        };
        let p = ui.painter();
        // The trail first: the text takes what is left.
        let mut right = rect.right() - 10.0;
        let faint = |s: &str| p.layout_no_wrap(s.to_owned(), body.clone(), t.text_faint);
        match trail {
            Trail::Nothing => {}
            Trail::Key(k) => {
                let g = faint(k);
                right -= g.size().x;
                p.galley(pos2(right, rect.center().y - g.size().y / 2.0), g, t.text_faint);
                right -= 10.0;
            }
            Trail::Check(k) => {
                let g = faint(k);
                if !k.is_empty() {
                    right -= g.size().x;
                    p.galley(pos2(right, rect.center().y - g.size().y / 2.0), g, t.text_faint);
                    right -= 10.0;
                }
                icons::check(p, Rect::from_center_size(pos2(right - 7.0, rect.center().y), vec2(14.0, 14.0)), t.accent);
                right -= 24.0;
            }
            Trail::Value(v) => {
                icons::chevron_right(p, Rect::from_center_size(pos2(right - 5.0, rect.center().y), vec2(10.0, 10.0)), t.text_faint);
                right -= 16.0;
                let g = p.layout_no_wrap(v.to_owned(), body.clone(), t.text_dim);
                right -= g.size().x;
                p.galley(pos2(right, rect.center().y - g.size().y / 2.0), g, t.text_dim);
                right -= 10.0;
            }
        }
        let x = rect.left() + 10.0;
        let room = (right - x).max(1.0);
        let mut job = egui::text::LayoutJob::default();
        job.append(label, 0.0, egui::TextFormat { font_id: body.clone(), color: strong, ..Default::default() });
        if let Some(n) = note {
            job.append(&format!("  {n}"), 0.0, egui::TextFormat { font_id: body.clone(), color: t.text_faint, ..Default::default() });
        }
        job.wrap = egui::text::TextWrapping::truncate_at_width(room);
        let a = p.layout_job(job);
        let b = what.map(|w| {
            let mut job = egui::text::LayoutJob::simple_singleline(w.to_owned(), small.clone(), t.text_dim);
            job.wrap = egui::text::TextWrapping::truncate_at_width(room);
            p.layout_job(job)
        });
        let total = a.size().y + b.as_ref().map_or(0.0, |b| b.size().y + 2.0);
        let top = rect.center().y - total / 2.0;
        let a_h = a.size().y;
        p.galley(pos2(x, top), a, strong);
        if let Some(b) = b {
            p.galley(pos2(x, top + a_h + 2.0), b, t.text_dim);
        }
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A menu row with a title and a line under it, checked when it's the current choice.
pub fn row_with_note(ui: &mut Ui, t: &Tokens, title: &str, note: &str, on: bool) -> Response {
    pick_row(ui, t, title, None, Some(note), if on { Trail::Check("") } else { Trail::Nothing }, false)
}

/// A menu row showing a setting's current value and an arrow; hovering it opens its submenu
/// ([`sub_menu_config`]).
pub fn value_row(ui: &mut Ui, t: &Tokens, label: &str, value: &str) -> Response {
    pick_row(ui, t, label, None, None, Trail::Value(value), false)
}

/// A plain choice in a submenu, checked when it's the current one.
pub fn choice_row(ui: &mut Ui, t: &Tokens, label: &str, on: bool) -> Response {
    pick_row(ui, t, label, None, None, if on { Trail::Check("") } else { Trail::Nothing }, false)
}

/// A submenu in the same glass look as [`menu::frame`].
pub fn sub_menu_config(t: &Tokens) -> egui::containers::menu::MenuConfig {
    let frame = menu::frame(t);
    egui::containers::menu::MenuConfig::new().style(move |style: &mut egui::Style| {
        style.visuals.window_fill = frame.fill;
        style.visuals.window_stroke = frame.stroke;
        style.visuals.window_corner_radius = frame.corner_radius;
        style.visuals.popup_shadow = frame.shadow;
        style.spacing.menu_margin = frame.inner_margin;
        style.spacing.item_spacing.y = 1.0;
    })
}

/// Cmd/Ctrl+U: add files.
pub const ADD_FILES: egui::KeyboardShortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::U);

/// The mic in the message box: quiet; while listening, accent on a soft tinted square.
pub fn mic_button(ui: &mut Ui, t: &Tokens, d: f32, radius: f32, listening: bool) -> Response {
    if listening {
        let r = radius.min(t.round(8.0));
        let b = crate::composer::round_button(ui, t, d, r, t.accent.gamma_multiply(0.2), t.accent, icons::mic, true);
        crate::tip::keys(b, t, &[("Done", &["Click"]), ("Drop it", &["Esc"])])
    } else {
        crate::tip::note(
            crate::composer::ghost_icon(ui, t, d, radius, icons::mic, true),
            t,
            "Speak instead of typing",
            "Click again when done · Esc to drop it",
        )
    }
}

/// Send as a quiet "↵" (Enter), no filled circle: strong when there is something to send.
pub fn send_glyph(ui: &mut Ui, t: &Tokens, d: f32, radius: f32, ready: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(d, d), if ready { Sense::click() } else { Sense::hover() });
    if ui.is_rect_visible(rect) {
        if ready && resp.is_pointer_button_down_on() {
            ui.painter().rect_filled(rect, radius, t.pressed);
        } else if ready && resp.hovered() {
            ui.painter().rect_filled(rect, radius, t.hover);
        }
        let color = if ready { t.text_strong } else { t.text_faint };
        icons::enter(ui.painter(), Rect::from_center_size(rect.center(), vec2(d * 0.6, d * 0.6)), color);
    }
    if ready { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}

/// The voice while listening, inside the box: a row of dots that stretch into bars as it
/// hears you, newest on the right. A preview of the design: the levels are drawn, not measured.
pub fn wave(ui: &Ui, t: &Tokens, rect: Rect, secs: f64) {
    let p = ui.painter();
    #[allow(clippy::cast_possible_truncation)] // a clock for display
    let ts = secs as f32;
    let step = 7.0;
    let half = rect.height() * 0.3;
    let mut x = rect.left() + 4.0;
    let mut i = 0.0_f32;
    while x < rect.right() - 2.0 {
        let along = ((x - rect.left()) / rect.width().max(1.0)).clamp(0.0, 1.0);
        let level = (0.5 + 0.5 * (i * 0.9 - ts * 7.0).sin()) * (0.5 + 0.5 * (i * 0.37 + ts * 3.1).sin()) * (0.4 + 0.6 * (ts * 1.7 - i * 0.13).sin().abs());
        let h = half * level * along * along;
        let c = rect.center().y;
        if h < 1.0 {
            p.circle_filled(pos2(x, c), 1.3, t.text_faint);
        } else {
            p.line_segment([pos2(x, c - h), pos2(x, c + h)], egui::Stroke::new(2.4, t.text_dim));
        }
        x += step;
        i += 1.0;
    }
}

impl ByobApp {
    /// Start listening, or end it. Speech to text isn't wired yet: ending says so for a moment.
    pub(crate) fn toggle_voice(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        if self.voice.take().is_some() {
            self.voice_note = Some(now + 3.0);
        } else {
            self.voice = Some(now);
            self.voice_note = None;
        }
    }

    /// The "+" in the message box: a menu of what can go into the message.
    pub(crate) fn add_menu(&mut self, ui: &mut Ui, t: &Tokens, d: f32, radius: f32) -> Response {
        let b = crate::composer::ghost_icon(ui, t, d, radius, icons::plus, true);
        let b = {
            let c = crate::tip::cmd(ui.ctx());
            crate::tip::keys(b, t, &[("Add files or photos", &[c, "U"]), ("Commands", &["/"])])
        };
        if ui.input_mut(|i| i.consume_shortcut(&ADD_FILES)) {
            self.uploads.pick(ui.ctx());
        }
        let keys = ui.ctx().format_shortcut(&ADD_FILES);
        let empty = self.input.trim().is_empty();
        let mut files = false;
        let mut slash = false;
        let mut plugins = false;
        menu::from_left(&b, t, 250.0).show(|ui| {
            menu::tighten(ui);
            if menu::item(ui, t, Some(icons::paperclip), "Add files or photos", Some(&keys))
                .on_hover_text("Or drop them onto the window, or paste an image")
                .clicked()
            {
                files = true;
                ui.close();
            }
            let r = ui.add_enabled_ui(empty, |ui| menu::item(ui, t, Some(icons::slash_box), "Slash commands", Some("/"))).inner;
            let r = if empty {
                r.on_hover_text("Commands, skills and plugin actions")
            } else {
                r.on_disabled_hover_text("Type / at the start of an empty message")
            };
            if r.clicked() {
                slash = true;
                ui.close();
            }
            menu::separator(ui, t);
            if menu::item(ui, t, Some(icons::puzzle), "Plugins", None).on_hover_text("Tools, skills and servers byob can use").clicked() {
                plugins = true;
                ui.close();
            }
        });
        if files {
            self.uploads.pick(ui.ctx());
        }
        if slash {
            self.input = "/".to_owned();
            self.focus_composer = true;
        }
        if plugins {
            self.plugins.open(None);
        }
        b
    }

    /// The row under the message box.
    pub(crate) fn chat_bar(&mut self, ui: &mut Ui, t: &Tokens) {
        let chat = self.sidebar.current().map(|c| c.id.clone());
        if let Some(msg) = self.chat_config.ask(chat.as_deref()) {
            self.conn.send(msg);
        }
        ui.add_space(t.pad(4.0));
        let h = icons::button_height(ui);
        let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::hover());
        let narrow = row.width() < 460.0;
        // The right side first: it keeps its size, the place on the left takes what's left.
        let left_edge = ui
            .scope_builder(egui::UiBuilder::new().max_rect(row).layout(egui::Layout::right_to_left(egui::Align::Center)), |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                if let Some(msg) = self.usage.ring(ui, t) {
                    self.conn.send(msg);
                }
                self.mode_chip(ui, t, narrow);
                self.model_chip(ui, t, narrow);
                ui.min_rect().left()
            })
            .inner;
        let left = Rect::from_min_max(row.min, pos2((left_edge - 8.0).max(row.left()), row.bottom()));
        // Listening (drawn inside the box): Esc drops it.
        if self.voice.is_some() && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            self.voice = None;
        }
        if let Some(until) = self.voice_note {
            let now = ui.input(|i| i.time);
            if now < until {
                let font = egui::TextStyle::Body.resolve(ui.style());
                ui.painter().text(pos2(left.left() + 8.0, left.center().y), egui::Align2::LEFT_CENTER, "Voice to text isn't connected yet", font, t.text_dim);
                ui.ctx().request_repaint_after(std::time::Duration::from_secs_f64(until - now));
                return;
            }
            self.voice_note = None;
        }
        ui.scope_builder(egui::UiBuilder::new().max_rect(left).layout(egui::Layout::left_to_right(egui::Align::Center)), |ui| {
            let room = ui.available_width() - 40.0;
            if chat.is_none() && self.queue.editing.is_none() {
                self.where_picker(ui, t, room);
            } else {
                // Fixed by the first message: a plain label.
                let ws = self.sidebar.current_workspace().map(|w| (w.name.clone(), w.folder.clone()));
                let (paint, name): (Paint, String) = match &ws {
                    Some((name, _)) => (icons::folder, name.clone()),
                    None => (icons::compose, "No folder".to_owned()),
                };
                let tip = match &ws {
                    Some((_, path)) => format!("Works in {path}"),
                    None => "Just chat: nothing on your disk is touched. Turn it into a project from the chat's menu".to_owned(),
                };
                let sub = if ws.is_some() { tip.as_str() } else { "Nothing on your disk is touched" };
                crate::tip::note(
                    chip(ui, t, Lead::Icon(paint), &name, None, false, room),
                    t,
                    if ws.is_some() { "Works in this project" } else { "No folder" },
                    sub,
                );
            }
        });
    }

    /// The model and its effort ("Sonnet Medium"); one menu changes either.
    fn model_chip(&mut self, ui: &mut Ui, t: &Tokens, narrow: bool) {
        let Some(state) = self.state.clone() else {
            chip(ui, t, Lead::Nothing, "connecting", None, false, 120.0);
            return;
        };
        let current = self.chat_profile();
        let switch = self.models.list.as_ref().is_some_and(|l| !l.items.is_empty());
        let (link, model) = match &state.chat_endpoint {
            Some(e) => (e.link.clone(), e.model.clone()),
            None => (state.link.clone(), state.inference_model.clone()),
        };
        let name = current.as_ref().map_or(model, |p| p.label().to_owned());
        let effort = current.as_ref().and_then(|p| p.params.effort).filter(|_| !narrow).map(effort_name);
        // A dot only when something is wrong with the connection.
        let dot = match link {
            Link::Failed { .. } => Some(t.danger),
            Link::Checking => Some(t.warning),
            Link::Ok | Link::Unknown => None,
        };
        let r = chip(ui, t, dot.map_or(Lead::Nothing, Lead::Dot), &name, effort, true, if narrow { 110.0 } else { 190.0 });
        let r = match &link {
            Link::Failed { .. } => crate::tip::note(r, t, "Can't reach the model", "Click to pick another or check the connection"),
            _ => crate::tip::note(r, t, "Model and effort", "For this chat · 1 to 9 pick while the menu is open"),
        };
        if !switch {
            if r.clicked() {
                self.open_connection();
            }
            return;
        }
        if std::mem::take(&mut self.header_rects.open_model_menu) {
            egui::Popup::open_id(ui.ctx(), egui::Popup::default_response_id(&r));
        }
        let Some(list) = self.models.list.clone() else { return };
        let chat_sets = state.chat_endpoint.as_ref().is_some_and(|e| e.from == Level::Chat);
        let current_id = current.as_ref().map(|p| p.id.clone());
        let inherit = if self.sidebar.current_workspace().is_some() { "Use the project's model" } else { "Use the default model" };
        let levels = current.as_ref().and_then(crate::models::effort_levels);
        // Grouped by where they run; numbered in that order (1…9 pick while it's open).
        let mut ordered: Vec<&Profile> = Vec::new();
        for group in ["Your plan", "On this computer", "Online"] {
            ordered.extend(list.items.iter().map(|v| &v.profile).filter(|p| place(p) == group));
        }
        let twin = |p: &Profile| list.items.iter().filter(|v| v.profile.label() == p.label()).count() > 1;
        let main = list.role(byob_protocol::profiles::ROLE_MAIN).map(|v| v.profile.id.clone());
        let mut picked: Option<Option<String>> = None;
        let mut effort: Option<Option<Effort>> = None;
        let mut manage = false;
        menu::below(&r, t, 270.0).show(|ui| {
            menu::tighten(ui);
            let mut group = "";
            for (i, p) in ordered.iter().enumerate() {
                if place(p) != group {
                    group = place(p);
                    if i > 0 {
                        menu::separator(ui, t);
                    }
                    menu::heading(ui, t, group);
                }
                let on = current_id.as_deref() == Some(p.id.as_str());
                let key = NUMBER_KEYS.get(i);
                let k = key.map_or("", |k| k.1);
                let trail = if on { Trail::Check(k) } else { Trail::Key(k) };
                // Two with the same name: the second says which it is.
                let note = twin(p).then(|| if main.as_deref() == Some(p.id.as_str()) { "default" } else { p.id.as_str() });
                let tip = if p.description.is_empty() { format!("{} · {}", p.model, crate::connect::short_url(&p.base_url)) } else { p.description.clone() };
                let pressed = key.is_some_and(|k| menu::key(ui, k.0));
                if pick_row(ui, t, p.label(), note, None, trail, false).on_hover_text(tip).clicked() || pressed {
                    picked = Some(Some(p.id.clone()));
                    ui.close();
                }
            }
            if let (Some((levels, default, tip)), Some(p)) = (levels, current.as_ref()) {
                menu::separator(ui, t);
                let shown = p.params.effort.map_or("Default", effort_name);
                let row = pick_row(ui, t, "Effort", None, None, Trail::Value(shown), false)
                    .on_hover_text(format!("{tip}. It is saved with {}, so it applies to every chat that uses it", p.label()));
                egui::containers::menu::SubMenu::new().config(sub_menu_config(t)).show(ui, &row, |ui| {
                    ui.set_width(180.0);
                    let default_row = pick_row(ui, t, "Default", None, None, if p.params.effort.is_none() { Trail::Check("") } else { Trail::Nothing }, false);
                    if default_row.on_hover_text(default).clicked() {
                        effort = Some(None);
                        ui.close();
                    }
                    for &e in levels {
                        let trail = if p.params.effort == Some(e) { Trail::Check("") } else { Trail::Nothing };
                        if pick_row(ui, t, effort_name(e), None, None, trail, false).clicked() {
                            effort = Some(Some(e));
                            ui.close();
                        }
                    }
                });
            }
            menu::separator(ui, t);
            if chat_sets && menu::item(ui, t, Some(icons::folder), inherit, None).on_hover_text("Stop choosing a model for this chat").clicked() {
                picked = Some(None);
                ui.close();
            }
            if menu::item(ui, t, Some(icons::chip), "Manage models", None).clicked() {
                manage = true;
                ui.close();
            }
        });
        if let Some(p) = picked {
            self.conn.send(ClientMsg::SetLevelProfile { scope: ConfigScope::Chat, profile: p });
        }
        if let (Some(e), Some(mut p)) = (effort, current) {
            p.params.effort = e;
            self.conn.send(ClientMsg::UpdateProfile { profile: p, api_key: None });
        }
        if manage {
            self.open_connection();
        }
    }

    /// What byob may do without asking, said as what happens.
    fn mode_chip(&mut self, ui: &mut Ui, t: &Tokens, narrow: bool) {
        let Some(view) = self.chat_config.view().cloned() else { return };
        let now = view.effective.approvals.value;
        let own = view.own.approvals;
        let paint: Paint = match now {
            Approvals::ReadOnly => icons::eye,
            Approvals::Ask => icons::shield,
            Approvals::AllowEdits => icons::pencil,
            Approvals::AllowAll | Approvals::Yolo => icons::bolt,
        };
        let label = mode_name(now);
        let r = chip(ui, t, Lead::Icon(paint), if narrow { "" } else { label }, None, true, 160.0);
        let r = crate::tip::note(r, t, label, "What byob may do without asking · 1 to 5 pick");
        self.composer_rects.mode = r.rect;
        let inherit = if self.sidebar.current_workspace().is_some() { "Use the project's mode" } else { "Use the default mode" };
        let mut set: Option<Option<Approvals>> = None;
        menu::below(&r, t, 300.0).show(|ui| {
            menu::tighten(ui);
            menu::heading(ui, t, "Mode");
            let all = MODES.iter().copied().chain([(Approvals::Yolo, "Never ask", "Every call runs, even ones to confirm")]);
            for (i, (a, title, what)) in all.enumerate() {
                if a == Approvals::Yolo {
                    menu::separator(ui, t);
                }
                let key = NUMBER_KEYS.get(i);
                let k = key.map_or("", |k| k.1);
                let trail = if now == a { Trail::Check(k) } else { Trail::Key(k) };
                let pressed = key.is_some_and(|k| menu::key(ui, k.0));
                if pick_row(ui, t, title, None, Some(what), trail, a == Approvals::Yolo).clicked() || pressed {
                    set = Some(Some(a));
                    ui.close();
                }
            }
            if own.is_some() {
                menu::separator(ui, t);
                if menu::item(ui, t, Some(icons::folder), inherit, None).clicked() {
                    set = Some(None);
                    ui.close();
                }
            }
        });
        if let Some(a) = set {
            let mut layer = view.own.clone();
            layer.approvals = a;
            self.conn.send(ClientMsg::SetConfig { scope: ConfigScope::Chat, layer, api_key: None });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(url: &str) -> Profile {
        Profile { base_url: url.into(), ..Profile::default() }
    }

    #[test]
    fn models_group_by_where_they_run() {
        assert_eq!(place(&profile("cli://claude")), "Your plan");
        assert_eq!(place(&profile("http://127.0.0.1:8080/v1")), "On this computer");
        assert_eq!(place(&profile("http://localhost:11434")), "On this computer");
        assert_eq!(place(&profile("http://[::1]:8080/v1")), "On this computer");
        assert_eq!(place(&profile("https://api.anthropic.com")), "Online");
    }

    #[test]
    fn every_mode_has_a_plain_name() {
        for a in Approvals::ALL {
            assert!(!mode_name(a).is_empty());
        }
        assert_eq!(mode_name(Approvals::Ask), "Ask before changes");
    }

    #[test]
    fn the_chat_config_is_asked_again_when_the_chat_changes() {
        let mut c = ChatConfig::default();
        assert!(c.ask(None).is_some());
        assert!(c.ask(None).is_none());
        assert!(c.ask(Some("a")).is_some());
        assert!(c.ask(Some("a")).is_none());
        c.reset();
        assert!(c.ask(Some("a")).is_some());
    }
}
