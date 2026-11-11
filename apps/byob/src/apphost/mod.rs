//! HTML app artifacts inline on the desktop: [`ServoView`], the desktop [`AppView`].
//!
//! Each live app runs in its own `byob-artifact-host` process (Servo, offscreen, software GL;
//! `host/artifact-host/`), so Servo's MPL/tokio dependencies never enter byob and a runaway
//! script can be killed. Frames come back as RGBA and become an egui texture; input goes the
//! other way while the app has focus (click it; Esc gives focus back).
//!
//! Lifecycle per app version (one process each):
//! - started lazily when its card or the pane draws it on screen;
//! - the pane owns it when both show it (the card then mirrors the last frame);
//! - not drawn on screen for [`life::HIDE_AFTER`]: the last frame stays as a snapshot and the
//!   process exits; drawing it again starts a fresh one (page state is lost);
//! - minimized, in a background tab or behind the collapsed pane ([`AppView::hold`]): the
//!   process keeps running but is told to stop painting (`Request::Visible(false)`, for
//!   helpers that announce it) and no frames are uploaded; restoring within
//!   [`life::BACKGROUND_PAUSE_AFTER`] picks up where it was, later it is paused as above
//!   (see [`life`]);
//! - no pong for [`helper::PONG_TIMEOUT`], the process exiting, or Stop: killed, shown as
//!   "Stopped" with Reload.

mod helper;
mod life;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use byob_artifact_wire::{self as wire, Input, Request, Viewport};
use byob_ui_egui::appview::{AppDoc, AppStatus, AppView, AppViewResponse};
use byob_ui_egui::theme::Tokens;
use egui::{RichText, Stroke, vec2};

/// On screen this long before a card's app starts.
const SETTLE: Duration = Duration::from_millis(200);
/// Card height before the app reports its own, and the range it may pick.
const CARD_DEFAULT: f32 = 300.0;
const CARD_MIN: f32 = 120.0;
const CARD_MAX: f32 = 520.0;
/// Errors kept per app.
const MAX_ERRORS: usize = 20;
/// App versions remembered (snapshots); the least recently seen beyond this are dropped.
const MAX_APPS: usize = 24;

type Key = (String, u32);

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    /// Running, or about to start the next time it is on screen.
    Live,
    /// Killed or crashed; waits for Reload.
    Stopped(String),
}

struct App {
    helper: Option<helper::Helper>,
    /// The document as sent (theme defaults and helper script injected).
    html: String,
    state: State,
    texture: Option<egui::TextureHandle>,
    /// CSS pixels, from the app.
    content_height: Option<f32>,
    sent: Option<Viewport>,
    errors: Vec<String>,
    cursor: wire::Cursor,
    focused: bool,
    hovered: bool,
    life: life::Life,
    /// In the background: frames stay in the helper's slot (only the newest is kept).
    hidden: bool,
    /// `Visible(false)` went to the helper.
    hidden_sent: bool,
    /// On screen continuously since.
    visible_since: Option<Instant>,
}

impl App {
    /// `web`: a web surface's document, run as it is (it carries its own bridge script);
    /// else an HTML app artifact, wrapped with the theme defaults and helper script.
    fn new(source: &str, web: bool) -> Self {
        let html = if web { source.to_owned() } else { byob_tools::artifact_types::html_app::wrap_html(source) };
        let state = if html.len() > wire::MAX_HTML { State::Stopped("It's too large to run here.".into()) } else { State::Live };
        Self {
            helper: None,
            html,
            state,
            texture: None,
            content_height: None,
            sent: None,
            errors: Vec::new(),
            cursor: wire::Cursor::Default,
            focused: false,
            hovered: false,
            life: life::Life::new(Instant::now()),
            hidden: false,
            hidden_sent: false,
            visible_since: None,
        }
    }
}

/// Which surface draws an app this frame.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Surface {
    Pane,
    Card,
}

pub struct ServoView {
    path: PathBuf,
    ctx: egui::Context,
    apps: HashMap<Key, App>,
    theme: wire::Theme,
    /// Shown in the pane last frame / this frame.
    pane: Option<Key>,
    pane_now: Option<Key>,
    /// Drawn live by a card this frame (a second card for the same version mirrors it).
    cards_now: HashSet<Key>,
    /// Drawn on screen by their owner / held in the background, this frame.
    drawn_now: HashSet<Key>,
    held_now: HashSet<Key>,
    /// Bridge messages pages posted, not taken yet.
    inbox: Vec<(String, u32, String)>,
}

impl ServoView {
    /// The view, if the helper binary can be found.
    pub fn new(ctx: &egui::Context) -> Option<Self> {
        let path = helper::locate()?;
        log::info!("HTML apps render inline with {}", path.display());
        Some(Self::with_helper(ctx, path))
    }

    fn with_helper(ctx: &egui::Context, path: PathBuf) -> Self {
        Self {
            path,
            ctx: ctx.clone(),
            apps: HashMap::new(),
            theme: wire::Theme::default(),
            pane: None,
            pane_now: None,
            cards_now: HashSet::new(),
            drawn_now: HashSet::new(),
            held_now: HashSet::new(),
            inbox: Vec::new(),
        }
    }

    fn launch(&mut self, key: &Key, viewport: Viewport) {
        let Some(app) = self.apps.get_mut(key) else { return };
        let ctx = self.ctx.clone();
        match helper::Helper::spawn(&self.path, move || ctx.request_repaint()) {
            Ok(h) => {
                log::info!("artifact {}/v{}: renderer started (pid {})", key.0, key.1, h.pid().unwrap_or(0));
                h.send(Request::Load(Box::new(wire::Load { html: app.html.clone(), theme: self.theme.clone(), viewport })));
                h.send(Request::Focus(app.focused));
                app.sent = Some(viewport);
                app.errors.clear();
                app.hidden = false;
                app.hidden_sent = false;
                app.helper = Some(h);
            }
            Err(e) => app.state = State::Stopped(format!("Couldn't start the renderer: {e}")),
        }
    }

    /// Draw `doc` into `rect` from `surface`; the owner runs and gets input, others mirror.
    fn surface(&mut self, ui: &mut egui::Ui, t: &Tokens, doc: &AppDoc<'_>, surface: Surface, size: egui::Vec2) -> egui::Rect {
        let key: Key = (doc.id.to_owned(), doc.version);
        let owner = match surface {
            Surface::Pane => true,
            Surface::Card => self.pane.as_ref() != Some(&key) && self.cards_now.insert(key.clone()),
        };
        if surface == Surface::Pane {
            self.pane_now = Some(key.clone());
        }
        if !self.apps.contains_key(&key) {
            let Some(source) = doc.source else {
                let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
                ui.put(rect, egui::Spinner::new());
                return rect;
            };
            self.apps.insert(key.clone(), App::new(source, doc.web));
        }
        let (rect, resp) = ui.allocate_exact_size(size, if owner { egui::Sense::click_and_drag() } else { egui::Sense::click() });
        let ppp = ui.ctx().pixels_per_point();
        let viewport = Viewport { width: (rect.width() * ppp).round().max(1.0) as u32, height: (rect.height() * ppp).round().max(1.0) as u32, scale: ppp };
        let visible = ui.is_rect_visible(rect);
        let start = {
            let Some(app) = self.apps.get_mut(&key) else { return rect };
            let now = Instant::now();
            if visible {
                app.visible_since.get_or_insert(now);
            } else {
                app.visible_since = None;
            }
            // Only start apps that stay on screen a moment (not ones a scroll passes over,
            // or every card before the chat has scrolled to the bottom).
            let settled = surface == Surface::Pane || app.visible_since.is_some_and(|t| now.saturating_duration_since(t) >= SETTLE);
            let wanted = owner && visible && app.helper.is_none() && app.state == State::Live;
            if wanted && !settled {
                ui.ctx().request_repaint_after(SETTLE);
            }
            wanted && settled
        };
        if start {
            self.launch(&key, viewport);
        }
        if owner && visible {
            self.drawn_now.insert(key.clone());
        }
        let Some(app) = self.apps.get_mut(&key) else { return rect };
        if owner
            && let Some(h) = &app.helper
            && app.sent != Some(viewport)
        {
            h.send(Request::Resize(viewport));
            app.sent = Some(viewport);
        }

        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 6.0, t.app_bg);
        match &app.texture {
            Some(tex) => {
                // Owner: 1:1 in device pixels (it may lag a resize by a frame). Mirror: the
                // top of it, scaled to the width.
                let natural = tex.size_vec2() / ppp;
                let drawn = if owner { natural } else { natural * (rect.width() / natural.x.max(1.0)) };
                let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                painter.image(tex.id(), egui::Rect::from_min_size(rect.min, drawn), uv, egui::Color32::WHITE);
            }
            None if app.state == State::Live => {
                painter.text(rect.center(), egui::Align2::CENTER_CENTER, "Starting the app…", egui::FontId::proportional(13.0), t.text_dim);
            }
            None => {}
        }
        if let State::Stopped(why) = &app.state {
            painter.rect_filled(rect, 6.0, t.app_bg.gamma_multiply(0.8));
            painter.text(rect.center() - vec2(0.0, 10.0), egui::Align2::CENTER_CENTER, "Stopped", egui::FontId::proportional(16.0), t.text_strong);
            painter.text(rect.center() + vec2(0.0, 12.0), egui::Align2::CENTER_CENTER, why, egui::FontId::proportional(12.0), t.text_dim);
        }

        if owner && app.state == State::Live && app.helper.is_some() {
            input(ui, app, rect, &resp, surface);
        } else if app.focused {
            app.focused = false;
        }
        let stroke = if app.focused { Stroke::new(1.5, t.accent) } else { Stroke::new(1.0, t.border) };
        painter.rect_stroke(rect, 6.0, stroke, egui::StrokeKind::Inside);
        rect
    }

    /// The Stop / Reload row under (card) or over (pane) the app.
    fn controls(&mut self, ui: &mut egui::Ui, t: &Tokens, doc: &AppDoc<'_>, mirrored: bool) -> AppViewResponse {
        let mut resp = AppViewResponse::default();
        let key: Key = (doc.id.to_owned(), doc.version);
        let Some(app) = self.apps.get_mut(&key) else { return resp };
        ui.horizontal(|ui| match &app.state {
            State::Live if mirrored => {
                ui.label(RichText::new("Running beside the chat").small().color(t.text_dim));
            }
            State::Live => {
                let status = if app.helper.is_some() { "Live" } else { "Snapshot" };
                ui.label(RichText::new(status).small().color(t.text_dim));
                if app.helper.is_some() && ui.small_button("Stop").on_hover_text("Stop the app (its state is lost)").clicked() {
                    if let Some(mut h) = app.helper.take() {
                        h.kill();
                    }
                    app.focused = false;
                    app.state = State::Stopped("You stopped it.".into());
                }
                if app.focused {
                    ui.label(RichText::new("Esc to leave the app").small().color(t.text_faint));
                }
            }
            State::Stopped(_) => {
                if ui.small_button("Reload").on_hover_text("Start the app again").clicked() {
                    app.state = State::Live;
                    app.texture = None;
                    app.sent = None;
                }
                if ui.small_button("Open in browser").clicked() {
                    resp.open = true;
                }
            }
        });
        resp
    }

    /// Per frame, after drawing: frames and events in, liveness, background or snapshot
    /// what went out of view.
    fn pump(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        let mut any_live = false;
        let (drawn, held) = (std::mem::take(&mut self.drawn_now), std::mem::take(&mut self.held_now));
        for (key, app) in &mut self.apps {
            let verdict = app.life.frame(now, drawn.contains(key), held.contains(key));
            let Some(h) = &mut app.helper else { continue };
            for e in h.poll() {
                match e {
                    wire::Event::Error { message, line } => {
                        let text = if line > 0 { format!("{message} (line {line})") } else { message };
                        if !app.errors.contains(&text) && app.errors.len() < MAX_ERRORS {
                            app.errors.push(text);
                        }
                    }
                    wire::Event::ContentHeight(height) => app.content_height = Some(height),
                    wire::Event::Cursor(c) => app.cursor = c,
                    wire::Event::Message(m) => {
                        // Bounded like everything else from the helper; a flood is dropped.
                        if self.inbox.len() < 1024 {
                            self.inbox.push((key.0.clone(), key.1, m));
                        }
                    }
                    wire::Event::Title(_) | wire::Event::Ready | wire::Event::Pong(_) | wire::Event::Frame(_) | wire::Event::Features(_) => {}
                }
            }
            // Background: no painting in the helper (if it can), no uploads here; the newest
            // frame waits in its slot for the restore.
            match verdict {
                life::Verdict::Background if !app.hidden => {
                    log::info!("artifact {}/v{}: in the background; rendering paused", key.0, key.1);
                    app.hidden = true;
                    app.focused = false;
                }
                life::Verdict::Show if app.hidden => {
                    log::info!("artifact {}/v{}: back in view", key.0, key.1);
                    app.hidden = false;
                    if app.hidden_sent {
                        h.send(Request::Visible(true));
                        app.hidden_sent = false;
                    }
                }
                _ => {}
            }
            if app.hidden && !app.hidden_sent && h.supports(wire::Features::VISIBILITY) {
                h.send(Request::Visible(false));
                app.hidden_sent = true;
            }
            if !app.hidden
                && let Some(f) = h.take_frame()
            {
                let started = Instant::now();
                let (w, hgt) = (usize::try_from(f.width).unwrap_or(0), usize::try_from(f.height).unwrap_or(0));
                let image = egui::ColorImage::from_rgba_premultiplied([w, hgt], &f.rgba);
                if !h.first_frame_seen() {
                    log::info!("artifact {}/v{}: first frame {}x{} {:?} after start", key.0, key.1, f.width, f.height, h.age());
                }
                match &mut app.texture {
                    Some(tex) => tex.set(image, egui::TextureOptions::NEAREST),
                    None => app.texture = Some(ctx.load_texture(format!("app-{}-{}", key.0, key.1), image, egui::TextureOptions::NEAREST)),
                }
                log::debug!("artifact {}/v{}: frame {}x{} uploaded in {:?}", key.0, key.1, f.width, f.height, started.elapsed());
            }
            match h.check(now) {
                Some(helper::Gone::Unresponsive) => {
                    log::warn!("artifact {}/v{}: no answer for {:?}; renderer killed", key.0, key.1, helper::PONG_TIMEOUT);
                    app.state = State::Stopped("It stopped responding, so byob stopped it.".into());
                    app.helper = None;
                    app.focused = false;
                }
                Some(helper::Gone::Exited(how)) => {
                    log::warn!("artifact {}/v{}: renderer exited ({how})", key.0, key.1);
                    app.state = State::Stopped(format!("Its renderer exited ({how})."));
                    app.helper = None;
                    app.focused = false;
                }
                None if verdict == life::Verdict::Pause => {
                    let why = if app.life.in_background() { "in the background too long" } else { "out of view" };
                    log::info!("artifact {}/v{}: {why}; keeping a snapshot and stopping the renderer", key.0, key.1);
                    if let Some(h) = app.helper.take() {
                        h.send(Request::Shutdown);
                    }
                    app.focused = false;
                    app.sent = None;
                    app.hidden = false;
                    app.hidden_sent = false;
                }
                None => any_live = true,
            }
        }
        if any_live {
            // Keep pinging and noticing hidden apps even when nothing else repaints.
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        if self.apps.len() > MAX_APPS {
            let mut idle: Vec<(Instant, Key)> = self.apps.iter().filter(|(_, a)| a.helper.is_none()).map(|(k, a)| (a.life.last_active(), k.clone())).collect();
            idle.sort();
            for (_, k) in idle.into_iter().take(self.apps.len().saturating_sub(MAX_APPS)) {
                self.apps.remove(&k);
            }
        }
    }
}

impl AppView for ServoView {
    fn show(&mut self, ui: &mut egui::Ui, t: &Tokens, doc: &AppDoc<'_>) -> AppViewResponse {
        let resp = self.controls(ui, t, doc, false);
        let size = ui.available_size_before_wrap().max(vec2(1.0, 1.0));
        self.surface(ui, t, doc, Surface::Pane, size);
        resp
    }

    fn card(&mut self, ui: &mut egui::Ui, t: &Tokens, doc: &AppDoc<'_>) -> Option<AppViewResponse> {
        let key: Key = (doc.id.to_owned(), doc.version);
        let mirrored = self.pane.as_ref() == Some(&key) || self.cards_now.contains(&key);
        // A web surface's page says its height over the bridge; else it is measured.
        let height = doc.height.or_else(|| self.apps.get(&key).and_then(|a| a.content_height)).map_or(CARD_DEFAULT, |h| h.clamp(CARD_MIN, CARD_MAX));
        self.surface(ui, t, doc, Surface::Card, vec2(ui.available_width(), height));
        Some(self.controls(ui, t, doc, mirrored))
    }

    fn live_cards(&self) -> bool {
        true
    }

    fn end_frame(&mut self, ctx: &egui::Context) {
        self.pane = self.pane_now.take();
        self.cards_now.clear();
        self.pump(ctx);
    }

    fn errors(&self) -> Vec<String> {
        self.pane.as_ref().and_then(|k| self.apps.get(k)).map(|a| a.errors.clone()).unwrap_or_default()
    }

    fn runs_inline(&self) -> bool {
        true
    }

    fn hold(&mut self, id: &str, version: u32) {
        let key: Key = (id.to_owned(), version);
        if self.apps.contains_key(&key) {
            self.held_now.insert(key);
        }
    }

    fn status(&self, id: &str, version: u32) -> AppStatus {
        match self.apps.get(&(id.to_owned(), version)) {
            None => AppStatus::Unknown,
            Some(a) => match (&a.state, &a.helper) {
                (State::Stopped(_), _) => AppStatus::Stopped,
                (State::Live, Some(_)) => AppStatus::Running,
                (State::Live, None) if a.texture.is_some() => AppStatus::Paused,
                (State::Live, None) => AppStatus::Unknown,
            },
        }
    }

    fn post(&mut self, id: &str, version: u32, msg: &str) {
        let Some(h) = self.apps.get(&(id.to_owned(), version)).and_then(|a| a.helper.as_ref()) else { return };
        if h.supports(wire::Features::BRIDGE) && msg.len() <= wire::MAX_MESSAGE {
            h.send(Request::Message(msg.to_owned()));
        }
    }

    fn take_messages(&mut self) -> Vec<(String, u32, String)> {
        std::mem::take(&mut self.inbox)
    }

    fn bridged(&self, id: &str, version: u32) -> Option<bool> {
        let h = self.apps.get(&(id.to_owned(), version))?.helper.as_ref()?;
        h.ready().then(|| h.supports(wire::Features::BRIDGE))
    }

    fn close(&mut self, id: &str, version: u32) {
        if let Some(mut app) = self.apps.remove(&(id.to_owned(), version))
            && let Some(mut h) = app.helper.take()
        {
            h.send(Request::Shutdown);
            h.kill();
        }
    }

    fn set_theme(&mut self, vars: &[(&'static str, String)]) {
        let dark = vars.iter().find(|(k, _)| *k == "--byob-bg").is_none_or(|(_, v)| luminance(v) < 0.5);
        self.theme = wire::Theme { vars: vars.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect(), dark };
        for app in self.apps.values() {
            if let Some(h) = &app.helper {
                h.send(Request::Theme(self.theme.clone()));
            }
        }
    }
}

use byob_ui_egui::appview::luminance;

fn cursor_icon(c: wire::Cursor) -> egui::CursorIcon {
    match c {
        wire::Cursor::Default => egui::CursorIcon::Default,
        wire::Cursor::Pointer => egui::CursorIcon::PointingHand,
        wire::Cursor::Text => egui::CursorIcon::Text,
        wire::Cursor::Crosshair => egui::CursorIcon::Crosshair,
        wire::Cursor::Move => egui::CursorIcon::Move,
        wire::Cursor::Grab => egui::CursorIcon::Grab,
        wire::Cursor::Grabbing => egui::CursorIcon::Grabbing,
        wire::Cursor::NotAllowed => egui::CursorIcon::NotAllowed,
        wire::Cursor::None => egui::CursorIcon::None,
    }
}

fn mods(m: egui::Modifiers) -> wire::Mods {
    let mut bits = 0;
    for (on, bit) in [(m.shift, wire::Mods::SHIFT), (m.ctrl, wire::Mods::CTRL), (m.alt, wire::Mods::ALT), (m.mac_cmd, wire::Mods::META)] {
        if on {
            bits |= bit;
        }
    }
    wire::Mods(bits)
}

/// Keys forwarded by name (text arrives as `Event::Text`).
fn named_key(k: egui::Key) -> Option<&'static str> {
    use egui::Key as K;
    Some(match k {
        K::ArrowUp => "ArrowUp",
        K::ArrowDown => "ArrowDown",
        K::ArrowLeft => "ArrowLeft",
        K::ArrowRight => "ArrowRight",
        K::Backspace => "Backspace",
        K::Delete => "Delete",
        K::Enter => "Enter",
        K::Tab => "Tab",
        K::Home => "Home",
        K::End => "End",
        K::PageUp => "PageUp",
        K::PageDown => "PageDown",
        K::Insert => "Insert",
        _ => return None,
    })
}

/// Click focuses the app (arrows and Tab go to it); Esc or clicking elsewhere gives focus
/// back. Pointer input goes to it while hovered; keys while focused.
fn input(ui: &egui::Ui, app: &mut App, rect: egui::Rect, resp: &egui::Response, surface: Surface) {
    let Some(h) = &app.helper else { return };
    if resp.clicked() || resp.drag_started() {
        resp.request_focus();
    }
    if resp.has_focus() {
        ui.memory_mut(|m| m.set_focus_lock_filter(resp.id, egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: false }));
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            resp.surrender_focus();
        }
    }
    let focused = resp.has_focus();
    if focused != app.focused {
        app.focused = focused;
        h.send(Request::Focus(focused));
    }
    let ppp = ui.ctx().pixels_per_point();
    let at = |p: egui::Pos2| {
        let d = (p - rect.min) * ppp;
        (d.x, d.y)
    };
    // `contains_pointer` respects clipping and egui layers on top (menus, windows).
    let hovered = resp.contains_pointer();
    // In a card the wheel scrolls the chat unless the app has focus.
    let wheel_to_app = hovered && (surface == Surface::Pane || focused);
    let mut scrolled = false;
    for ev in ui.input(|i| i.events.clone()) {
        let input = match ev {
            egui::Event::PointerMoved(p) if hovered => {
                let (x, y) = at(p);
                Input::MouseMove { x, y }
            }
            egui::Event::PointerButton { pos, button, pressed, .. } if hovered || (!pressed && focused) => {
                let button = match button {
                    egui::PointerButton::Primary => wire::Button::Primary,
                    egui::PointerButton::Secondary => wire::Button::Secondary,
                    egui::PointerButton::Middle => wire::Button::Middle,
                    _ => continue,
                };
                let (x, y) = at(pos);
                Input::MouseButton { button, down: pressed, x, y }
            }
            egui::Event::MouseWheel { unit, delta, .. } if wheel_to_app => {
                let d = wheel_pixels(unit, delta, ppp, rect.height());
                let (x, y) = at(ui.input(|i| i.pointer.hover_pos()).unwrap_or(rect.center()));
                scrolled = true;
                Input::Wheel { dx: d.x, dy: d.y, x, y }
            }
            egui::Event::Text(text) if focused => Input::Text(text),
            egui::Event::Paste(text) if focused => Input::Text(text),
            egui::Event::Key { key, pressed, repeat, modifiers, .. } if focused => match named_key(key) {
                Some(name) => Input::Key(wire::KeyEvent { down: pressed, key: name.into(), code: name.into(), mods: mods(modifiers), repeat }),
                None => continue,
            },
            _ => continue,
        };
        h.send(Request::Input(input));
    }
    if scrolled && surface == Surface::Card {
        ui.ctx().input_mut(|i| i.smooth_scroll_delta = egui::Vec2::ZERO);
    }
    if app.hovered && !hovered {
        h.send(Request::Input(Input::MouseLeave));
    }
    app.hovered = hovered;
    if hovered {
        ui.ctx().set_cursor_icon(cursor_icon(app.cursor));
    }
}

/// An egui wheel delta in device pixels. egui and Servo agree on the sign (positive: the
/// view scrolls up / left), so it passes through unflipped.
fn wheel_pixels(unit: egui::MouseWheelUnit, delta: egui::Vec2, ppp: f32, height: f32) -> egui::Vec2 {
    match unit {
        egui::MouseWheelUnit::Point => delta * ppp,
        egui::MouseWheelUnit::Line => delta * 40.0 * ppp,
        egui::MouseWheelUnit::Page => delta * height * ppp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: the wheel was negated on the way to Servo, so apps scrolled backwards.
    #[test]
    fn wheel_down_scrolls_the_app_down() {
        // Wheel down / swipe up: egui reports a negative y, which Servo reads as scrolling down.
        let d = wheel_pixels(egui::MouseWheelUnit::Line, egui::vec2(0.0, -1.0), 2.0, 300.0);
        assert!(d.y < 0.0, "got {d:?}");
        assert_eq!(wheel_pixels(egui::MouseWheelUnit::Point, egui::vec2(3.0, 5.0), 2.0, 300.0), egui::vec2(6.0, 10.0));
        assert_eq!(wheel_pixels(egui::MouseWheelUnit::Page, egui::vec2(0.0, 1.0), 1.0, 300.0), egui::vec2(0.0, 300.0));
    }

    #[test]
    fn dark_follows_the_background() {
        assert!(luminance("#16171a") < 0.5);
        assert!(luminance("#ffffff") > 0.9);
        assert!((luminance("nope") - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn keys_and_mods() {
        assert_eq!(named_key(egui::Key::ArrowLeft), Some("ArrowLeft"));
        assert_eq!(named_key(egui::Key::A), None);
        let m = mods(egui::Modifiers { shift: true, ctrl: true, ..Default::default() });
        assert!(m.has(wire::Mods::SHIFT) && m.has(wire::Mods::CTRL) && !m.has(wire::Mods::ALT));
    }

    /// Minimized (held, not drawn): the helper is told to stop painting, and to start again
    /// on restore; a helper that never announced the feature isn't sent the new message.
    #[cfg(unix)]
    #[test]
    fn background_and_restore_reach_the_helper() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let run = |features: bool| -> (Vec<Request>, Vec<AppStatus>) {
            let log = dir.path().join(format!("in-{features}.bin"));
            let path = dir.path().join(format!("helper-{features}"));
            // Ready, maybe Features(VISIBILITY), then record every request (the shell keeps
            // stdout open meanwhile).
            let hello = if features { r"\001\000\000\000\000\005\000\000\000\007\001\000\000\000" } else { r"\001\000\000\000\000" };
            std::fs::write(&path, format!("#!/bin/sh\nprintf '{hello}'\ncat > '{}'\n", log.display())).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            let ctx = egui::Context::default();
            let mut v = ServoView::with_helper(&ctx, path);
            let key: Key = ("app".into(), 1);
            v.apps.insert(key.clone(), App::new("<p>hi</p>", false));
            v.launch(&key, Viewport { width: 10, height: 10, scale: 1.0 });
            let mut statuses = vec![v.status("app", 1)];
            // On screen until the helper has said hello.
            let start = Instant::now();
            let said_hello = |v: &ServoView| !features || v.apps[&key].helper.as_ref().is_some_and(|h| h.supports(wire::Features::VISIBILITY));
            while (start.elapsed() < Duration::from_millis(300) || !said_hello(&v)) && start.elapsed() < Duration::from_secs(5) {
                v.drawn_now.insert(key.clone());
                v.pump(&ctx);
                std::thread::sleep(Duration::from_millis(10));
            }
            std::thread::sleep(Duration::from_millis(50));
            for drawn in [true, false, false, true] {
                if drawn {
                    v.drawn_now.insert(key.clone());
                } else {
                    v.hold("app", 1);
                }
                v.pump(&ctx);
                statuses.push(v.status("app", 1));
                assert_eq!(v.apps[&key].hidden, !drawn);
            }
            if let Some(mut h) = v.apps.get_mut(&key).and_then(|a| a.helper.take()) {
                h.send(Request::Shutdown);
                std::thread::sleep(Duration::from_millis(100));
                h.kill();
            }
            let bytes = std::fs::read(&log).unwrap();
            let mut r = bytes.as_slice();
            let mut reqs = Vec::new();
            while let Ok(Some(req)) = wire::recv::<Request>(&mut r) {
                if !matches!(req, Request::Ping(_)) {
                    reqs.push(req);
                }
            }
            (reqs, statuses)
        };
        let (reqs, statuses) = run(true);
        let vis: Vec<&Request> = reqs.iter().filter(|r| matches!(r, Request::Visible(_))).collect();
        assert_eq!(vis, [&Request::Visible(false), &Request::Visible(true)], "{reqs:?}");
        assert!(matches!(reqs.first(), Some(Request::Load(_))), "{reqs:?}");
        assert!(statuses.iter().all(|s| *s == AppStatus::Running), "minimized still runs: {statuses:?}");
        let (reqs, _) = run(false);
        assert!(!reqs.iter().any(|r| matches!(r, Request::Visible(_))), "old helpers only get what they know: {reqs:?}");
    }

    /// The real helper, if one is built: `$BYOB_ARTIFACT_HOST`, else the helper workspace's
    /// release build.
    fn built_helper() -> Option<PathBuf> {
        helper::locate().or_else(|| {
            let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../host/artifact-host/target/release/byob-artifact-host");
            p.is_file().then_some(p)
        })
    }

    /// Run `html` (plus [`REPORT_JS`]) in the real helper at 400x300, send `inputs`, then
    /// press F9. Returns what the page reported and the newest frame.
    fn drive(path: &std::path::Path, html: &str, inputs: &[Input]) -> (Vec<String>, wire::Frame) {
        let mut h = helper::Helper::spawn(path, || {}).unwrap();
        h.send(Request::Load(Box::new(wire::Load {
            html: byob_tools::artifact_types::html_app::wrap_html(&format!("{html}{REPORT_JS}")),
            theme: wire::Theme::default(),
            viewport: Viewport { width: 400, height: 300, scale: 1.0 },
        })));
        h.send(Request::Focus(true));
        let mut out = Vec::new();
        let mut frame = None;
        let mut pump_until = |h: &mut helper::Helper, out: &mut Vec<String>, what: &str| {
            let start = Instant::now();
            while !out.iter().any(|m: &String| m.starts_with(what)) && start.elapsed() < Duration::from_secs(30) {
                for e in h.poll() {
                    if let wire::Event::Error { message, .. } = e {
                        out.push(message);
                    }
                }
                if let Some(f) = h.take_frame() {
                    frame = Some(f);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(out.iter().any(|m| m.starts_with(what)), "no {what:?} from the page: {out:?}");
        };
        pump_until(&mut h, &mut out, "ready");
        for i in inputs {
            h.send(Request::Input(i.clone()));
            std::thread::sleep(Duration::from_millis(30));
        }
        std::thread::sleep(Duration::from_millis(300));
        h.send(Request::Input(Input::Key(wire::KeyEvent { down: true, key: "F9".into(), code: "F9".into(), mods: wire::Mods(0), repeat: false })));
        pump_until(&mut h, &mut out, "selection ");
        // Whatever the click changed has been painted by now; keep the newest frame.
        std::thread::sleep(Duration::from_millis(300));
        if let Some(f) = h.take_frame() {
            frame = Some(f);
        }
        h.kill();
        (out, frame.unwrap())
    }

    /// The page says when it has loaded and, on F9, what is selected.
    const REPORT_JS: &str = "<script>function report(m){console.log('\\u0001byob-error 0 '+m)}\
        addEventListener('load',()=>setTimeout(()=>report('ready'),200));\
        addEventListener('keydown',e=>{if(e.key!=='F9')return;report('selection '+JSON.stringify(String(getSelection())));report('caret in '+getSelection().anchorNode?.nodeName)},true);</script>";

    /// RGBA at (x, y).
    fn pixel(f: &wire::Frame, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * f.width + x) * 4) as usize;
        [f.rgba[i], f.rgba[i + 1], f.rgba[i + 2], f.rgba[i + 3]]
    }

    /// Regression: clicking a `<canvas>` (the bead-loom app's grid) painted the whole canvas
    /// with the selection tint. Servo put the caret inside the canvas and painted the
    /// "selected" canvas (only once something is drawn in it); the helper now moves such a
    /// caret out (`SELECTION_FIX` in host/artifact-host). Text selection must keep working.
    /// Needs a built helper; skipped without one.
    #[test]
    fn clicking_a_canvas_does_not_highlight_it() {
        let Some(path) = built_helper() else {
            eprintln!("skipped: no byob-artifact-host built");
            return;
        };
        let page = "<body style='margin:0;background:#fff'><canvas id=c width=400 height=150 style='display:block;touch-action:none'></canvas>\
            <p style='margin:20px 0;font:20px sans-serif;color:#000'>pick some words</p>\
            <script>const g=c.getContext('2d');g.fillStyle='#fff';g.fillRect(0,0,400,150);c.addEventListener('pointerdown',e=>c.setPointerCapture(e.pointerId))</script>";
        let primary = |down, x, y| Input::MouseButton { button: wire::Button::Primary, down, x, y };
        let click = |x, y| [Input::MouseMove { x, y }, primary(true, x, y), primary(false, x, y)];

        let (out, frame) = drive(&path, page, &click(100.0, 75.0));
        assert!(out.contains(&"selection \"\"".to_owned()), "{out:?}");
        for (x, y) in [(100, 75), (10, 10), (390, 140)] {
            assert_eq!(pixel(&frame, x, y), [255, 255, 255, 255], "canvas at ({x}, {y}) after a click on it");
        }

        // A double click on a word still selects it.
        let (x, y) = (20.0, 180.0);
        let (out, _) = drive(&path, page, &[click(x, y).as_slice(), &click(x, y)[1..]].concat());
        assert!(out.contains(&"selection \"pick\"".to_owned()), "{out:?}");
    }

    /// Without a helper on disk the view isn't offered (the placeholder stays).
    #[test]
    fn no_helper_no_view() {
        if helper::locate().is_none() {
            assert!(ServoView::new(&egui::Context::default()).is_none());
        }
    }
}
