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
const SHOW_USAGE_LABEL: f32 = 1000.0;
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
    /// "~3.4k / 262k" next to the context bar.
    pub usage_label: bool,
    /// "Settings" next to the gear.
    pub settings_label: bool,
    /// How many plugins are on, next to the puzzle chip.
    pub plugin_count: bool,
}

impl Layout {
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
            usage_label: width >= SHOW_USAGE_LABEL,
            settings_label: !phone,
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
    pub settings: Rect,
    pub more: Rect,
    /// The plugins chip (`Rect::NOTHING` while it isn't shown).
    pub plugins: Rect,
    /// The app settings' Close button (`Rect::NOTHING` while they are shut).
    pub settings_close: Rect,
    /// Scripted runs: open the "⋯" menu on the next frame.
    pub open_menu: bool,
}

impl Default for Rects {
    fn default() -> Self {
        Self {
            chats: Rect::NOTHING,
            chat_settings: Rect::NOTHING,
            settings: Rect::NOTHING,
            more: Rect::NOTHING,
            plugins: Rect::NOTHING,
            settings_close: Rect::NOTHING,
            open_menu: false,
        }
    }
}

/// "Ctrl+," (egui spells the key out: "Ctrl+Comma").
pub fn settings_shortcut(ctx: &egui::Context) -> String {
    ctx.format_shortcut(&SETTINGS_SHORTCUT).replace("Comma", ",")
}

/// The app settings button's tooltip.
pub fn settings_tip(ctx: &egui::Context) -> String {
    format!("App settings: connection, defaults for every chat, appearance ({})", settings_shortcut(ctx))
}

impl ByobApp {
    /// Keyboard shortcuts the header offers (before anything else takes the keys).
    pub(crate) fn header_shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.input_mut(|i| i.consume_shortcut(&SETTINGS_SHORTCUT)) {
            self.show_settings = !self.show_settings;
        }
    }

    pub(crate) fn header(&mut self, ui: &mut egui::Ui) {
        let t = self.tokens;
        let layout = Layout::for_width(ui.available_width());
        egui::Panel::top("header").show(ui, |ui| {
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
        });
    }

    /// Right to left: the "⋯" menu, the app settings, the plugins chip, the artifact pane toggle.
    fn header_actions(&mut self, ui: &mut egui::Ui, t: &theme::Tokens, layout: Layout) {
        let more = icons::icon_button(ui, t, icons::more, None, false).on_hover_text("More: chat, workspace and connection settings, artifacts");
        self.header_rects.more = more.rect;
        if std::mem::take(&mut self.header_rects.open_menu) {
            egui::Popup::open_id(ui.ctx(), egui::Popup::default_response_id(&more));
        }
        egui::Popup::menu(&more).show(|ui| self.more_menu(ui));
        let settings =
            icons::icon_button(ui, t, icons::gear, layout.settings_label.then_some("Settings"), self.show_settings).on_hover_text(settings_tip(ui.ctx()));
        self.header_rects.settings = settings.rect;
        if settings.clicked() {
            self.show_settings = !self.show_settings;
        }
        self.header_rects.plugins = self.plugins.chip(ui, t, layout.plugin_count, false).map_or(Rect::NOTHING, |r| r.rect);
        self.artifacts.header_toggle(ui);
        self.plugins.sites.pane_toggles(ui, t);
        self.plugins.sites.status_items(ui, t);
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
        if ui.button("Connection…").on_hover_text("The model server byob talks to").clicked() {
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
        if ui.add(egui::Button::new("App settings").shortcut_text(settings_shortcut(ui.ctx()))).clicked() {
            self.show_settings = true;
            ui.close();
        }
    }

    /// Left to right: the chat list toggle, wordmark, activity, the chat's workspace and
    /// title with its settings, then the connection and context meter in what is left.
    fn header_left(&mut self, ui: &mut egui::Ui, t: &theme::Tokens, layout: Layout) {
        let chats = icons::icon_button(ui, t, icons::sidebar, None, false).on_hover_text("Show or hide the chat list");
        self.header_rects.chats = chats.rect;
        if chats.clicked() {
            self.sidebar.toggle();
        }
        if layout.wordmark {
            ui.label(egui::RichText::new("byob").strong().size(t.text_size(18.0)).color(t.accent));
        }
        if steps::awaiting_approval(&self.entries) {
            steps::waiting_badge(ui, t);
        } else if self.busy() {
            theme::spinner(ui, t, None);
        }
        self.header_context(ui, t, layout);
        ui.add_space(4.0);
        // The connection: its status dot, model and server; a click opens its settings.
        let room = ui.available_width();
        let clicked = ui
            .scope(|ui| {
                ui.set_max_width((room * 0.6).max(16.0));
                match &self.state {
                    Some(s) => connect::chip(ui, t, s, layout.chip),
                    None => {
                        ui.label(egui::RichText::new("connecting…").color(t.text_dim));
                        None
                    }
                }
            })
            .inner;
        match clicked {
            Some(connect::ChipClick::Connection) => self.open_connection(),
            Some(connect::ChipClick::ChatSettings) if self.workspaces.chat.is_none() => self.workspaces.toggle_chat(),
            Some(connect::ChipClick::ChatSettings) | None => {}
        }
        if ui.available_width() > 40.0
            && let Some(msg) = self.usage.meter(ui, t, !layout.usage_label)
        {
            self.conn.send(msg);
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
        assert_eq!(full, Layout { wordmark: true, workspace_name: true, chip: ChipDetail::Full, usage_label: true, settings_label: true, plugin_count: true });
        let laptop = Layout::for_width(1100.0);
        assert_eq!(
            laptop,
            Layout { wordmark: false, workspace_name: true, chip: ChipDetail::Model, usage_label: true, settings_label: true, plugin_count: true }
        );
        let small = Layout::for_width(800.0);
        assert_eq!(
            small,
            Layout { wordmark: false, workspace_name: true, chip: ChipDetail::Model, usage_label: false, settings_label: true, plugin_count: true }
        );
        let phone = Layout::for_width(390.0);
        assert_eq!(
            phone,
            Layout { wordmark: false, workspace_name: false, chip: ChipDetail::Dot, usage_label: false, settings_label: false, plugin_count: false }
        );
        // Narrowing never brings something back.
        let mut last = full;
        for w in (320..=1600).rev().step_by(10) {
            #[allow(clippy::cast_precision_loss)] // small
            let l = Layout::for_width(w as f32);
            assert!(l.wordmark <= last.wordmark && l.usage_label <= last.usage_label && l.settings_label <= last.settings_label, "{w}");
            assert!(l.plugin_count <= last.plugin_count, "{w}");
            assert!(l.workspace_name <= last.workspace_name && (l.chip as u8) <= (last.chip as u8), "{w}");
            last = l;
        }
    }
}
