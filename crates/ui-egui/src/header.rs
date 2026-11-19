//! The header row, in four groups:
//!
//! - navigation: the chat list toggle (and the wordmark when there is room);
//! - context: where the chat runs (the workspace's environment icon), "workspace / chat
//!   title" and, attached to it, the chat's own settings (sliders);
//! - status: the connection dot, model and server (capabilities in its tooltip) and the
//!   context meter;
//! - actions, on the right where they are never pushed off screen: the artifact pane
//!   toggle, the plugins chip (how many are on, a badge when one has trouble), the app
//!   settings (a gear, also at the bottom of the chat list, Ctrl/Cmd+,) and a "⋯" menu with
//!   everything else.
//!
//! What fits depends on the window width ([`Layout::for_width`]): optional details drop
//! into tooltips and the menu rather than being clipped.

use egui::{Key, KeyboardShortcut, Modifiers, Rect};

use crate::connect::{self, ChipDetail};
use crate::{ByobApp, icons, steps, theme};

/// Opens or closes the app settings.
pub const SETTINGS_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Comma);

/// Window widths at which the header spells more out.
const SHOW_SERVER: f32 = 1200.0;
const SHOW_WORDMARK: f32 = 1300.0;

/// What the header shows at a window width; the rest is in tooltips and the "⋯" menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    /// The "byob" wordmark.
    pub wordmark: bool,
    /// "Workspace / " before the chat title (else only its icon, with the name on hover).
    pub workspace_name: bool,
    /// How much of the connection the chip spells out.
    pub chip: ChipDetail,
    /// How many plugins are on, next to the puzzle chip.
    pub plugin_count: bool,
}

impl Layout {
    /// This, less what the user's layout hides (`header.wordmark`, `header.workspace`,
    /// `header.server`).
    #[must_use]
    pub fn shaped(mut self, ctx: &egui::Context) -> Self {
        use crate::layout::in_header;
        self.wordmark &= in_header(ctx, "header.wordmark");
        self.workspace_name &= in_header(ctx, "header.workspace");
        if self.chip == ChipDetail::Full && !in_header(ctx, "header.server") {
            self.chip = ChipDetail::Model;
        }
        self
    }

    pub fn for_width(width: f32) -> Self {
        let phone = width < crate::sidebar::NARROW;
        Self {
            wordmark: width >= SHOW_WORDMARK,
            workspace_name: !phone,
            chip: if phone {
                ChipDetail::Dot
            } else if width >= SHOW_SERVER {
                ChipDetail::Full
            } else {
                ChipDetail::Model
            },
            plugin_count: !phone,
        }
    }
}

/// Where the header's buttons were last drawn (popovers anchor to them; tests check they
/// stay on screen and apart).
#[derive(Debug, Clone, Copy)]
pub struct Rects {
    pub chats: Rect,
    pub chat_settings: Rect,
    pub more: Rect,
    /// The plugins chip (`Rect::NOTHING` while it isn't shown).
    pub plugins: Rect,
    /// The settings' "Back to chat" (phones, on a page: "Settings") row, and its search field
    /// (`Rect::NOTHING` before they were drawn).
    pub settings_back: Rect,
    pub settings_search: Rect,
    /// The settings' page area, its content column and its categories (by
    /// [`crate::settings::Tab`] order); last drawn, `Rect::NOTHING` before.
    pub settings_page: Rect,
    pub settings_body: Rect,
    pub settings_tabs: [Rect; 6],
    /// The "byob" wordmark, which goes back to the chat (`Rect::NOTHING` when it isn't shown).
    pub home: Rect,
    /// The chat's title in the header.
    pub title: Rect,
    /// Scripted runs: open the "⋯" menu on the next frame.
    pub open_menu: bool,
    /// Scripted runs: open the model switch on the next frame.
    pub open_model_menu: bool,
}

impl Default for Rects {
    fn default() -> Self {
        Self {
            chats: Rect::NOTHING,
            chat_settings: Rect::NOTHING,
            more: Rect::NOTHING,
            plugins: Rect::NOTHING,
            settings_back: Rect::NOTHING,
            settings_search: Rect::NOTHING,
            settings_page: Rect::NOTHING,
            settings_body: Rect::NOTHING,
            settings_tabs: [Rect::NOTHING; 6],
            home: Rect::NOTHING,
            title: Rect::NOTHING,
            open_menu: false,
            open_model_menu: false,
        }
    }
}

/// "−  110 %  +  Reset": the zoom's controls (Settings and the ⋯ menu); the new zoom when one
/// is pressed.
pub fn zoom_controls(ui: &mut egui::Ui, zoom: f32) -> Option<f32> {
    use egui::gui_zoom::kb_shortcuts as keys;
    let ctx = ui.ctx().clone();
    let mut out = None;
    let (lo, hi) = (*theme::Prefs::ZOOM_RANGE.start(), *theme::Prefs::ZOOM_RANGE.end());
    if ui.add_enabled(zoom > lo + 1e-3, egui::Button::new(" − ")).on_hover_text(format!("Zoom out ({})", ctx.format_shortcut(&keys::ZOOM_OUT))).clicked() {
        out = Some(theme::Prefs::zoom_step(zoom, -1));
    }
    #[allow(clippy::cast_possible_truncation)] // 50..=300
    let pct = (zoom * 100.0).round() as i32;
    ui.label(format!("{pct} %")).on_hover_text("Everything bigger or smaller: text, icons and spacing");
    if ui.add_enabled(zoom < hi - 1e-3, egui::Button::new(" + ")).on_hover_text(format!("Zoom in ({})", ctx.format_shortcut(&keys::ZOOM_IN))).clicked() {
        out = Some(theme::Prefs::zoom_step(zoom, 1));
    }
    if (zoom - 1.0).abs() > 1e-3 && ui.button("Reset").on_hover_text(format!("Back to 100 % ({})", ctx.format_shortcut(&keys::ZOOM_RESET))).clicked() {
        out = Some(1.0);
    }
    out
}

/// "Ctrl+," (egui spells the key out: "Ctrl+Comma").
pub fn settings_shortcut(ctx: &egui::Context) -> String {
    ctx.format_shortcut(&SETTINGS_SHORTCUT).replace("Comma", ",")
}

/// The app settings button's tooltip.
pub fn settings_tip(ctx: &egui::Context) -> String {
    format!("Settings: models, chats, plugins, appearance ({})", settings_shortcut(ctx))
}

impl ByobApp {
    /// Zoom the whole interface (stored with the appearance preferences; applied next frame).
    pub(crate) fn set_zoom(&mut self, zoom: f32) {
        self.prefs.zoom = zoom;
        self.prefs = std::mem::take(&mut self.prefs).sanitized();
    }

    /// Back to the chat: close the app settings and the chat settings popover.
    pub(crate) fn go_home(&mut self) {
        self.close_settings();
        if self.workspaces.chat.is_some() {
            self.workspaces.toggle_chat();
        }
    }

    /// Keyboard shortcuts the header offers (before anything else takes the keys).
    pub(crate) fn header_shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.input_mut(|i| i.consume_shortcut(&SETTINGS_SHORTCUT)) {
            self.toggle_settings();
        }
        // New chat: Ctrl/Cmd+N (desktop), Ctrl/Cmd+Shift+O (also in the browser, which keeps
        // Ctrl+N for itself). Search the chats: Ctrl/Cmd+F (not with Shift: that maximizes
        // the artifact pane).
        let narrow = ctx.content_rect().width() < crate::sidebar::NARROW;
        let new_chat = ctx.input_mut(|i| {
            i.consume_shortcut(&crate::sidebar::NEW_CHAT_SHORTCUT_ALT)
                || (cfg!(not(target_arch = "wasm32")) && i.consume_shortcut(&crate::sidebar::NEW_CHAT_SHORTCUT))
        });
        if new_chat {
            let msg = self.sidebar.new_chat(narrow);
            self.close_settings();
            self.conn.send(msg);
        }
        if ctx.input_mut(|i| !i.modifiers.shift && i.consume_shortcut(&crate::sidebar::SEARCH_SHORTCUT)) {
            if self.show_settings {
                // In the settings, Find searches them.
                self.appearance.focus_search = true;
            } else {
                self.sidebar.focus_search();
            }
        }
        // Zoom the whole interface, as in a browser: Ctrl + (or Ctrl =), Ctrl −, Ctrl 0.
        use egui::gui_zoom::kb_shortcuts as zoom;
        if ctx.input_mut(|i| i.consume_shortcut(&zoom::ZOOM_RESET)) {
            self.set_zoom(1.0);
        } else if ctx.input_mut(|i| i.consume_shortcut(&zoom::ZOOM_IN) || i.consume_shortcut(&zoom::ZOOM_IN_SECONDARY)) {
            self.set_zoom(theme::Prefs::zoom_step(self.prefs.zoom, 1));
        } else if ctx.input_mut(|i| i.consume_shortcut(&zoom::ZOOM_OUT)) {
            self.set_zoom(theme::Prefs::zoom_step(self.prefs.zoom, -1));
        }
    }

    pub(crate) fn header(&mut self, ui: &mut egui::Ui) {
        let layout = Layout::for_width(ui.available_width()).shaped(ui.ctx());
        let frame = crate::cockpit::framed(ui.ctx(), egui::Frame::side_top_panel(ui.style()));
        let shown =
            egui::Panel::top("header").frame(frame).show_separator_line(crate::cockpit::gap(ui.ctx()) <= 0.0).show(ui, |ui| self.header_row(ui, layout));
        crate::layout::set_header_rect(ui.ctx(), shown.response.rect);
        crate::cockpit::mark(ui.ctx(), shown.response.rect);
    }

    /// The header's row (in its panel, or in a free arrangement's module).
    pub(crate) fn header_row(&mut self, ui: &mut egui::Ui, layout: Layout) {
        let t = self.tokens;
        ui.horizontal(|ui| {
            // The row's full height up front: items line up on its centre whatever order
            // they are drawn in.
            let mut height = icons::button_height(ui);
            if layout.wordmark {
                height = height.max(ui.ctx().fonts_mut(|f| f.row_height(&egui::FontId::proportional(t.text_size(18.0)))));
            }
            ui.set_min_height(height);
            // The actions first, right to left, so a long title or URL can never push
            // them off screen; the rest shares what is left.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.header_actions(ui, &t, layout);
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| self.header_left(ui, &t, layout));
            });
        });
    }

    /// Right to left: the "⋯" menu (App settings is in it and in the sidebar), the plugins
    /// chip, the artifact pane toggle.
    fn header_actions(&mut self, ui: &mut egui::Ui, t: &theme::Tokens, layout: Layout) {
        let more = icons::icon_button(ui, t, icons::more, None, false).on_hover_text("More: chat, workspace and connection settings, artifacts");
        self.header_rects.more = more.rect;
        if std::mem::take(&mut self.header_rects.open_menu) {
            egui::Popup::open_id(ui.ctx(), egui::Popup::default_response_id(&more));
        }
        egui::Popup::menu(&more).show(|ui| self.more_menu(ui));
        if self.show_settings {
            // The chat's panes aren't there to toggle.
            self.header_rects.plugins = Rect::NOTHING;
            return;
        }
        self.header_rects.plugins = Rect::NOTHING;
        // The toolbar in the layout's order, drawn right to left.
        let shape = crate::layout::get(ui.ctx());
        let items = shape.arrange(byob_protocol::layout::TOOLBAR, &self.plugins.sites.status_ids());
        let ctx = ui.ctx().clone();
        for id in items.iter().rev().filter(|id| crate::layout::in_header(&ctx, id)) {
            match id.as_str() {
                "toolbar.plugins" => self.header_rects.plugins = self.plugins.chip(ui, t, layout.plugin_count, false).map_or(Rect::NOTHING, |r| r.rect),
                "toolbar.artifacts" => self.artifacts.header_toggle(ui),
                "toolbar.panes" => self.plugins.sites.pane_toggles(ui, t),
                view => self.plugins.sites.status_item(ui, t, view),
            }
        }
        // YOLO is on: say so where it can't be missed (nothing asks before a call runs).
        if self.state.as_ref().is_some_and(|s| s.yolo) {
            let badge = egui::Button::new(egui::RichText::new("YOLO").small().strong().color(t.on_accent)).fill(t.danger).corner_radius(t.round(10.0));
            if ui
                .add(badge)
                .on_hover_text("YOLO: nothing asks before a tool call runs, not even what a person is normally asked to confirm. Click to change it.")
                .clicked()
            {
                self.jump_to(crate::settings::registry::Key::Approvals);
            }
        }
    }

    /// The profile the chat in view uses: its own or its workspace's, else the default one
    /// (when the engine runs it).
    pub(crate) fn chat_profile(&self) -> Option<byob_protocol::Profile> {
        let (list, state) = (self.models.list.as_ref()?, self.state.as_ref()?);
        let view = match &state.chat_endpoint {
            Some(e) => list.get(e.profile.as_deref()?)?,
            None => list.role(byob_protocol::profiles::ROLE_MAIN).filter(|v| v.profile.points_at(&state.inference_base_url, &state.inference_model))?,
        };
        Some(view.profile.clone())
    }

    /// The header's model switch: the saved models for this chat, and a way to manage them.
    fn model_menu(&mut self, ui: &mut egui::Ui, t: &theme::Tokens, current: Option<&str>) {
        ui.set_min_width(260.0);
        let Some(list) = self.models.list.clone() else { return };
        ui.label(egui::RichText::new("Model for this chat").small().color(t.text_dim));
        let chat_sets = self.state.as_ref().and_then(|s| s.chat_endpoint.as_ref()).is_some_and(|e| e.from == byob_protocol::workspace::Level::Chat);
        let mut picked = None;
        for v in &list.items {
            let p = &v.profile;
            let on = current == Some(p.id.as_str());
            let mut job = egui::text::LayoutJob::default();
            let font = egui::TextStyle::Button.resolve(ui.style());
            let small = egui::TextStyle::Small.resolve(ui.style());
            let fmt = |font: egui::FontId, color| egui::TextFormat { font_id: font, color, ..Default::default() };
            job.append(if on { "✓ " } else { "    " }, 0.0, fmt(font.clone(), t.accent));
            job.append(p.label(), 0.0, fmt(font, t.text_strong));
            job.append(&format!("   {} · {}", p.model, connect::short_url(&p.base_url)), 0.0, fmt(small, t.text_faint));
            let r = ui.selectable_label(on, job);
            let r = if p.description.is_empty() { r } else { r.on_hover_text(&p.description) };
            if r.clicked() {
                picked = Some(Some(p.id.clone()));
                ui.close();
            }
        }
        if chat_sets {
            ui.separator();
            if ui.button("Use the workspace's model").on_hover_text("Stop choosing a model for this chat").clicked() {
                picked = Some(None);
                ui.close();
            }
        }
        ui.separator();
        if ui.button("Manage models…").clicked() {
            self.open_connection();
            ui.close();
        }
        if let Some(p) = picked {
            self.conn.send(byob_protocol::ClientMsg::SetLevelProfile { scope: byob_protocol::ConfigScope::Chat, profile: p });
        }
    }

    /// The "⋯" menu: every settings entry point by name, and the artifact actions.
    fn more_menu(&mut self, ui: &mut egui::Ui) {
        ui.set_min_width(220.0);
        if ui.button("Chat settings…").on_hover_text("This chat's model, approvals, where commands run…").clicked() {
            if self.workspaces.chat.is_none() {
                self.workspaces.toggle_chat();
            }
            ui.close();
        }
        if let Some(ws) = self.sidebar.current_workspace().map(|w| (w.id.clone(), w.name.clone()))
            && ui.button(format!("Workspace settings: {}…", ws.1)).on_hover_text("Defaults for its chats, and where its commands run").clicked()
        {
            self.workspaces.open_settings(&ws.0, false);
            ui.close();
        }
        if ui.button(if self.models.available() { "Models…" } else { "Connection…" }).on_hover_text("The models and servers byob talks to").clicked() {
            self.open_connection();
            ui.close();
        }
        if ui.button("Plugins…").on_hover_text("Tools, skills and servers byob can use; their settings and switches").clicked() {
            self.plugins.open(None);
            ui.close();
        }
        if !self.artifacts.is_empty() {
            ui.separator();
            self.artifacts.menu_items(ui);
        }
        ui.separator();
        if self.prefs.cockpit.arrangement.free {
            if ui.button("Arrange panels…").on_hover_text("Move, resize and hide the window's parts").clicked() {
                self.arranging.on = true;
                ui.close();
            }
            if ui.button("Dock panels").on_hover_text("Back to byob's usual panels").clicked() {
                self.dock();
                ui.close();
            }
        } else if ui.button("Arrange panels…").on_hover_text("Place the window's parts anywhere: floating, framed in metal, or just the chat").clicked() {
            self.start_arranging();
            ui.close();
        }
        // Zoom, where people look for it in an app menu.
        ui.horizontal(|ui| {
            ui.label("Zoom");
            if let Some(z) = zoom_controls(ui, self.prefs.zoom) {
                self.set_zoom(z);
            }
        });
        if ui.add(egui::Button::new("App settings").shortcut_text(settings_shortcut(ui.ctx()))).clicked() {
            self.open_settings();
            ui.close();
        }
    }

    /// Left to right: the chat list toggle, wordmark, activity, the chat's workspace and
    /// title with its settings, then the connection and context meter in what is left.
    fn header_left(&mut self, ui: &mut egui::Ui, t: &theme::Tokens, layout: Layout) {
        if self.show_settings {
            self.settings_header_left(ui, t, layout);
            return;
        }
        let chats = icons::icon_button(ui, t, icons::sidebar, None, false).on_hover_text("Show or hide the chat list");
        self.header_rects.chats = chats.rect;
        if chats.clicked() {
            self.sidebar.toggle();
        }
        if layout.wordmark {
            let home = ui
                .add(egui::Label::new(egui::RichText::new("byob").strong().size(t.text_size(18.0)).color(t.accent)).sense(egui::Sense::click()))
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text("Back to the chat");
            self.header_rects.home = home.rect;
            if home.clicked() {
                self.go_home();
            }
        }
        if steps::awaiting_approval(&self.entries) {
            steps::waiting_badge(ui, t);
        } else if self.busy() {
            theme::spinner(ui, t, None);
        }
        self.header_context(ui, t, layout);
        ui.add_space(4.0);
        // The model chip and the context meter, in the layout's order.
        let shape = crate::layout::get(ui.ctx());
        for id in shape.arrange(byob_protocol::layout::HEADER, &[]) {
            if !crate::layout::in_header(ui.ctx(), &id) {
                continue;
            }
            match id.as_str() {
                "header.model" => self.header_model(ui, t, layout),
                "header.usage" => {
                    if ui.available_width() > 40.0
                        && let Some(msg) = self.usage.meter(ui, t)
                    {
                        self.conn.send(msg);
                    }
                }
                _ => {}
            }
        }
    }

    /// The connection: its status dot, model and server; a click switches the chat's model
    /// (or opens the connection's settings).
    fn header_model(&mut self, ui: &mut egui::Ui, t: &theme::Tokens, layout: Layout) {
        let room = ui.available_width();
        // With saved models, the chip switches this chat's model.
        let current = self.chat_profile();
        let switch = self.models.list.as_ref().is_some_and(|l| !l.items.is_empty());
        let name = current.as_ref().map(|p| p.label().to_owned());
        let (resp, clicked) = ui
            .scope(|ui| {
                ui.set_max_width((room * 0.6).max(16.0));
                match &self.state {
                    Some(s) => {
                        let (r, c) = connect::chip(ui, t, s, layout.chip, name.as_deref(), switch);
                        (Some(r), c)
                    }
                    None => {
                        ui.label(egui::RichText::new("connecting…").color(t.text_dim));
                        (None, None)
                    }
                }
            })
            .inner;
        if let Some(resp) = resp.filter(|_| switch) {
            if std::mem::take(&mut self.header_rects.open_model_menu) {
                egui::Popup::open_id(ui.ctx(), egui::Popup::default_response_id(&resp));
            }
            let current = current.map(|p| p.id);
            egui::Popup::menu(&resp).show(|ui| self.model_menu(ui, t, current.as_deref()));
        }
        match clicked {
            Some(connect::ChipClick::Connection) => self.open_connection(),
            Some(connect::ChipClick::ChatSettings) if self.workspaces.chat.is_none() => self.workspaces.toggle_chat(),
            Some(connect::ChipClick::ChatSettings) | None => {}
        }
    }

    /// Over the settings: where you are, the wordmark that goes back, and whether a reply is
    /// running or waiting on you (the chat goes on meanwhile).
    fn settings_header_left(&mut self, ui: &mut egui::Ui, t: &theme::Tokens, layout: Layout) {
        self.header_rects.chats = Rect::NOTHING;
        self.header_rects.chat_settings = Rect::NOTHING;
        self.header_rects.home = Rect::NOTHING;
        ui.add_space(6.0);
        if layout.wordmark {
            let home = ui
                .add(egui::Label::new(egui::RichText::new("byob").strong().size(t.text_size(18.0)).color(t.accent)).sense(egui::Sense::click()))
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text("Back to the chat");
            self.header_rects.home = home.rect;
            if home.clicked() {
                self.go_home();
            }
            ui.add_space(6.0);
        }
        let title = ui.label(egui::RichText::new("Settings").strong().color(t.text_strong));
        self.header_rects.title = title.rect;
        if steps::awaiting_approval(&self.entries) {
            ui.add_space(8.0);
            let badge = ui.add(egui::Button::new(egui::RichText::new("Waiting for your approval").color(t.warning)).frame(false));
            if badge.on_hover_text("Go back to the chat to answer it").clicked() {
                self.close_settings();
            }
        } else if self.busy() {
            ui.add_space(8.0);
            theme::spinner(ui, t, None).on_hover_text("A reply is running in the chat");
        }
    }

    /// Where the chat runs and what it is: the workspace's environment icon, "workspace /
    /// title" (a click opens the chat's settings) and the chat settings button.
    fn header_context(&mut self, ui: &mut egui::Ui, t: &theme::Tokens, layout: Layout) {
        let title = self.sidebar.current().map_or("New chat", |c| c.title.as_str()).to_owned();
        let ws = self.sidebar.current_workspace().map(|w| (w.name.clone(), w.env));
        if let Some((name, env)) = &ws {
            let size = t.text_size(15.0);
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
            icons::env(ui.painter(), rect, *env, icons::env_color(t, *env));
            resp.on_hover_text(format!("Workspace {name}\n{}", icons::env_tip(*env)));
        }
        let room = ui.available_width();
        let title_resp = ui
            .scope(|ui| {
                ui.set_max_width((room * 0.4).max(40.0));
                let mut job = egui::text::LayoutJob::default();
                let font = egui::TextStyle::Body.resolve(ui.style());
                if let Some((name, _)) = ws.as_ref().filter(|_| layout.workspace_name) {
                    job.append(&format!("{name} / "), 0.0, egui::TextFormat::simple(font.clone(), t.text_dim));
                }
                job.append(&title, 0.0, egui::TextFormat::simple(font, t.text_strong));
                let tip = ws.as_ref().map_or_else(|| title.clone(), |(w, _)| format!("{title}\nin the workspace {w}"));
                ui.add(egui::Label::new(job).truncate().sense(egui::Sense::click()))
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text(format!("{tip}\n\nClick for this chat's settings"))
            })
            .inner;
        let gear = icons::settings_button(ui, t).on_hover_text("Chat settings: this chat's model, approvals, where commands run…");
        self.header_rects.chat_settings = gear.rect;
        self.header_rects.title = title_resp.rect;
        if gear.clicked() || title_resp.clicked() {
            self.workspaces.toggle_chat();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn details_drop_as_the_window_narrows() {
        let full = Layout::for_width(1600.0);
        assert_eq!(full, Layout { wordmark: true, workspace_name: true, chip: ChipDetail::Full, plugin_count: true });
        let laptop = Layout::for_width(1100.0);
        assert_eq!(laptop, Layout { wordmark: false, workspace_name: true, chip: ChipDetail::Model, plugin_count: true });
        let small = Layout::for_width(800.0);
        assert_eq!(small, Layout { wordmark: false, workspace_name: true, chip: ChipDetail::Model, plugin_count: true });
        let phone = Layout::for_width(390.0);
        assert_eq!(phone, Layout { wordmark: false, workspace_name: false, chip: ChipDetail::Dot, plugin_count: false });
        // Narrowing never brings something back.
        let mut last = full;
        for w in (320..=1600).rev().step_by(10) {
            #[allow(clippy::cast_precision_loss)] // small
            let l = Layout::for_width(w as f32);
            assert!(l.wordmark <= last.wordmark, "{w}");
            assert!(l.plugin_count <= last.plugin_count, "{w}");
            assert!(l.workspace_name <= last.workspace_name && (l.chip as u8) <= (last.chip as u8), "{w}");
            last = l;
        }
    }
}
