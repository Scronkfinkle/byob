//! The header row, over the chat only (the chat list, when open, runs the full height
//! beside it). It sits on the chat's background with no line under it, in four groups:
//!
//! - window controls, only while the chat list is closed: the list toggle, Back and
//!   Forward, where the list shows them when open (so the toggle never moves);
//! - context: "workspace / chat title" (a container or Nix workspace adds a small badge)
//!   and, attached to it, the chat's own settings (sliders);
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

use crate::connect::ChipDetail;
use crate::{ByobApp, icons, steps, theme};

/// Opens or closes the app settings.
pub const SETTINGS_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Comma);

/// Window widths at which the header spells more out.
const SHOW_SERVER: f32 = 1200.0;

/// What the header shows at a window width; the rest is in tooltips and the "⋯" menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    /// "Workspace / " before the chat title (else only its icon, with the name on hover).
    pub workspace_name: bool,
    /// How much of the connection the chip spells out.
    pub chip: ChipDetail,
    /// How many plugins are on, next to the puzzle chip.
    pub plugin_count: bool,
    /// The plugins' pane toggles in the header (else in the "⋯" menu: on a phone their
    /// names ran over the window controls).
    pub pane_toggles: bool,
}

impl Layout {
    /// This, less what the user's layout hides (`header.workspace`, `header.server`).
    #[must_use]
    pub fn shaped(mut self, ctx: &egui::Context) -> Self {
        use crate::layout::in_header;
        self.workspace_name &= in_header(ctx, "header.workspace");
        if self.chip == ChipDetail::Full && !in_header(ctx, "header.server") {
            self.chip = ChipDetail::Model;
        }
        self
    }

    pub fn for_width(width: f32) -> Self {
        let phone = width < crate::sidebar::NARROW;
        Self {
            workspace_name: !phone,
            chip: if phone {
                ChipDetail::Dot
            } else if width >= SHOW_SERVER {
                ChipDetail::Full
            } else {
                ChipDetail::Model
            },
            plugin_count: !phone,
            pane_toggles: !phone,
        }
    }
}

/// What the title's menu asked for.
#[derive(Clone, Copy)]
enum ChatAction {
    Rename,
    Pin,
    Project,
    Settings,
    Archive,
    Delete,
}

/// A dialog the title's menu opened.
pub enum ChatEdit {
    Rename { id: String, title: String },
    Delete { id: String, title: String },
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
    pub settings_tabs: [Rect; crate::settings::Tab::ALL.len()],
    /// Over the settings, the back chevron that goes back to the chat (`Rect::NOTHING` elsewhere).
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
            settings_tabs: [Rect::NOTHING; crate::settings::Tab::ALL.len()],
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
        // Switch Space: Ctrl+1 … Ctrl+9.
        let digits = [
            egui::Key::Num1,
            egui::Key::Num2,
            egui::Key::Num3,
            egui::Key::Num4,
            egui::Key::Num5,
            egui::Key::Num6,
            egui::Key::Num7,
            egui::Key::Num8,
            egui::Key::Num9,
        ];
        for (n, key) in digits.into_iter().enumerate() {
            if self.sidebar.spaces.len() > n && ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, key)) {
                self.sidebar.switch_space(n);
            }
        }
        // Hide or show the sidebar: Cmd/Ctrl+B, unless typing (there it makes text bold).
        if !ctx.egui_wants_keyboard_input() && ctx.input_mut(|i| i.consume_shortcut(&crate::sidebar::TOGGLE_SHORTCUT)) {
            self.sidebar.toggle();
        }
        // Back from a page (Projects, Apps, Library, Plugins) to the chat.
        if self.page != crate::Page::Chat && ctx.input_mut(|i| i.consume_shortcut(&crate::sidebar::BACK_SHORTCUT)) {
            self.page = crate::Page::Chat;
        }
        // Back / Forward through the chats opened: Cmd+[ / Cmd+] (Ctrl elsewhere).
        for (shortcut, forward) in [(crate::sidebar::BACK_SHORTCUT, false), (crate::sidebar::FORWARD_SHORTCUT, true)] {
            if ctx.input_mut(|i| i.consume_shortcut(&shortcut))
                && let Some(msg) = self.sidebar.go(forward)
            {
                self.close_settings();
                self.conn.send(msg);
            }
        }
        let (find, palette) = ctx.input_mut(|i| {
            let find = !i.modifiers.shift && i.consume_shortcut(&crate::sidebar::SEARCH_SHORTCUT);
            (find, !find && !i.modifiers.shift && i.consume_shortcut(&crate::sidebar::PALETTE_SHORTCUT))
        });
        if find || palette {
            if self.show_settings {
                // In the settings, Find searches them.
                self.appearance.focus_search = true;
            } else if find && self.page == crate::Page::Projects {
                // On the Projects page, Find searches the projects (its field shows the key).
                self.projects.find();
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
        // On the chat's background, no line under it; as tall as the chat list's top bar so
        // their controls line up. With the app's own title bar (macOS) it is the title bar:
        // room for the window buttons while the list is closed, and its empty space moves
        // the window.
        let titlebar = self.sidebar.titlebar;
        let bar = crate::sidebar::top_bar_height(ui, titlebar);
        let button = icons::button_height(ui);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // a few points
        let pad = ((bar - button) / 2.0).max(0.0).round() as i8;
        #[allow(clippy::cast_possible_truncation)] // a few points
        let left = match (self.sidebar_docked, titlebar) {
            (true, _) => 14,
            (false, Some(tb)) => (tb.left - 2.0) as i8,
            (false, None) => 10,
        };
        let margin = egui::Margin { left, right: 12, top: pad, bottom: pad };
        let t = self.tokens;
        let frame = egui::Frame::NONE.fill(if self.show_settings { t.panel_bg } else { t.app_bg }).inner_margin(margin);
        let frame = crate::cockpit::framed(ui.ctx(), frame);
        let separator = self.show_settings && crate::cockpit::gap(ui.ctx()) <= 0.0;
        let shown = egui::Panel::top("header").show_separator_line(separator).frame(frame).show(ui, |ui| {
            let full = ui.max_rect() + margin;
            if titlebar.is_some() {
                // Registered before the row, so its buttons stay on top of it.
                crate::sidebar::window_drag(ui, full, egui::Id::unique("header-drag"));
            }
            self.header_row(ui, layout);
        });
        crate::layout::set_header_rect(ui.ctx(), shown.response.rect);
        crate::cockpit::mark(ui.ctx(), shown.response.rect);
    }

    /// The header's row (in its panel, or in a free arrangement's module).
    pub(crate) fn header_row(&mut self, ui: &mut egui::Ui, layout: Layout) {
        crate::fluid::region(ui, "header", |ui| self.header_row_inner(ui, layout))
    }

    fn header_row_inner(&mut self, ui: &mut egui::Ui, layout: Layout) {
        let t = self.tokens;
        ui.horizontal(|ui| {
            // The row's full height up front: items line up on its center whatever order
            // they are drawn in.
            ui.set_min_height(icons::button_height(ui));
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
        let more = icons::icon_button(ui, t, icons::more, None, false).on_hover_text("More: chat, project and connection settings, artifacts");
        self.header_rects.more = more.rect;
        if std::mem::take(&mut self.header_rects.open_menu) {
            egui::Popup::open_id(ui.ctx(), egui::Popup::default_response_id(&more));
        }
        let ws_label = self.sidebar.current_workspace().map(|w| format!("{} settings", w.name));
        let shortcut = settings_shortcut(ui.ctx());
        let mut items: Vec<(&str, Option<&str>)> =
            vec![("Chat settings", None), ("Connection", None), ("Plugins", None), ("Collapse all artifacts", None), ("App settings", Some(shortcut.as_str()))];
        if let Some(w) = &ws_label {
            items.push((w.as_str(), None));
        }
        let width = crate::menu::fit(ui, &items).max(200.0);
        crate::menu::below(&more, t, width).show(|ui| self.more_menu(ui, t, layout));
        if self.show_settings {
            // The chat's panes aren't there to toggle.
            self.header_rects.plugins = Rect::NOTHING;
            return;
        }
        // This chat's settings: at the right, with the other things about the chat; the
        // sheet opens under it.
        let open = self.workspaces.chat.is_some();
        let gear = icons::icon_button(ui, t, icons::sliders, None, open);
        let gear = crate::tip::note(gear, t, "Chat settings", "Model, mode, instructions, where it works");
        self.header_rects.chat_settings = gear.rect;
        if gear.clicked() {
            self.workspaces.toggle_chat();
        }
        self.header_rects.plugins = Rect::NOTHING;
        // The toolbar in the layout's order, drawn right to left.
        let shape = crate::layout::get(ui.ctx());
        let items = shape.arrange(byob_protocol::layout::TOOLBAR, &self.plugins.sites.status_ids());
        let ctx = ui.ctx().clone();
        for id in items.iter().rev().filter(|id| crate::layout::in_header(&ctx, id)) {
            match id.as_str() {
                "toolbar.plugins" => self.header_rects.plugins = self.plugins.chip(ui, t, layout.plugin_count, false).map_or(Rect::NOTHING, |r| r.rect),
                "toolbar.artifacts" => self.artifacts.header_toggle(ui, t),
                "toolbar.panes" if layout.pane_toggles => self.plugins.sites.pane_toggles(ui, t),
                "toolbar.panes" => {}
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

    /// The "⋯" menu: every settings entry point by name, and the artifact actions.
    fn more_menu(&mut self, ui: &mut egui::Ui, t: &theme::Tokens, layout: Layout) {
        use crate::menu;
        menu::tighten(ui);
        if menu::item(ui, t, Some(icons::sliders), "Chat settings", None).on_hover_text("This chat's model, approvals, where commands run…").clicked() {
            if self.workspaces.chat.is_none() {
                self.workspaces.toggle_chat();
            }
            ui.close();
        }
        if let Some(ws) = self.sidebar.current_workspace().map(|w| (w.id.clone(), w.name.clone()))
            && menu::item(ui, t, Some(icons::folder), &format!("{} settings", ws.1), None)
                .on_hover_text("Defaults for its chats, and where its commands run")
                .clicked()
        {
            self.workspaces.open_settings(&ws.0, false);
            ui.close();
        }
        if menu::item(ui, t, Some(icons::chip), if self.models.available() { "Models" } else { "Connection" }, None)
            .on_hover_text("The models and servers byob talks to")
            .clicked()
        {
            self.open_connection();
            ui.close();
        }
        if menu::item(ui, t, Some(icons::puzzle), "Plugins", None)
            .on_hover_text("Tools, skills and servers byob can use; their settings and switches")
            .clicked()
        {
            self.plugins.open(None);
            ui.close();
        }
        if !layout.pane_toggles && !self.show_settings && self.plugins.sites.pane_menu_items(ui, t) {
            ui.close();
        }
        if !self.artifacts.is_empty() {
            menu::separator(ui, t);
            self.artifacts.menu_items(ui, t);
        }
        menu::separator(ui, t);
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
        if ui.button("Edit anything… (F2)").on_hover_text("Select any element on screen to move, restyle, hide or add to it, or ask byob to").clicked() {
            crate::fluid::with(ui.ctx(), |f| f.editing = true);
            ui.close();
        }
        // Zoom, where people look for it in an app menu.
        ui.horizontal(|ui| {
            ui.add_space(9.0);
            ui.label(egui::RichText::new("Zoom").color(t.text));
            if let Some(z) = zoom_controls(ui, self.prefs.zoom) {
                self.set_zoom(z);
            }
        });
        menu::separator(ui, t);
        if menu::item(ui, t, Some(icons::gear), "App settings", Some(&settings_shortcut(ui.ctx()))).clicked() {
            self.open_settings();
            ui.close();
        }
    }

    /// Left to right: the window controls (while the list is closed), activity, the chat's workspace and
    /// title with its settings, then the connection and context meter in what is left.
    fn header_left(&mut self, ui: &mut egui::Ui, t: &theme::Tokens, layout: Layout) {
        if self.show_settings {
            self.settings_header_left(ui, t, layout);
            return;
        }
        self.header_rects.chats = Rect::NOTHING;
        self.header_rects.home = Rect::NOTHING;
        if !self.sidebar_docked {
            self.window_controls(ui, t);
            ui.add_space(6.0);
        }
        if steps::awaiting_approval(&self.entries) {
            steps::waiting_badge(ui, t);
        } else if self.busy() {
            theme::spinner(ui, t, None);
        }
        // The model and the context meter moved under the message box (chatbar.rs).
        self.header_context(ui, t, layout);
    }

    /// Over the settings: the chevron that goes back, where you are, and whether a reply is
    /// running or waiting on you (the chat goes on meanwhile).
    fn settings_header_left(&mut self, ui: &mut egui::Ui, t: &theme::Tokens, _layout: Layout) {
        self.header_rects.chats = Rect::NOTHING;
        self.header_rects.chat_settings = Rect::NOTHING;
        // Back to the chat: a chevron where the window controls are.
        let size = icons::button_height(ui);
        let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
        let home = crate::sidebar::window_button(ui, t, rect, egui::Id::unique("header-home"), icons::back, true).on_hover_text("Back to the chat (Esc)");
        self.header_rects.home = home.rect;
        if home.clicked() {
            self.go_home();
        }
        ui.add_space(4.0);
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

    /// The list toggle, Back and Forward, while the chat list is closed.
    fn window_controls(&mut self, ui: &mut egui::Ui, t: &theme::Tokens) {
        let size = icons::button_height(ui);
        let button = |ui: &mut egui::Ui, name: &str, paint: icons::Paint, enabled: bool| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
            crate::sidebar::window_button(ui, t, rect, egui::Id::unique(("header-window", name)), paint, enabled)
        };
        let chats = button(ui, "toggle", icons::sidebar, true).on_hover_text("Show the chat list");
        let (can_back, can_forward) = (self.sidebar.can_go(false), self.sidebar.can_go(true));
        let back = button(ui, "back", icons::back, can_back).on_hover_text(format!("Back ({})", ui.ctx().format_shortcut(&crate::sidebar::BACK_SHORTCUT)));
        let forward = button(ui, "forward", icons::forward, can_forward)
            .on_hover_text(format!("Forward ({})", ui.ctx().format_shortcut(&crate::sidebar::FORWARD_SHORTCUT)));
        self.header_rects.chats = chats.rect;
        if chats.clicked() {
            self.sidebar.toggle();
        }
        for (resp, fwd) in [(back, false), (forward, true)] {
            if resp.clicked()
                && let Some(msg) = self.sidebar.go(fwd)
            {
                self.conn.send(msg);
            }
        }
    }

    /// Where the chat runs and what it is: the workspace's environment icon, "workspace /
    /// title" (a click opens the chat's settings) and the chat settings button.
    fn header_context(&mut self, ui: &mut egui::Ui, t: &theme::Tokens, layout: Layout) {
        let title = self.sidebar.current().map_or("New chat", |c| c.title.as_str()).to_owned();
        let ws = self.sidebar.current_workspace().map(|w| (w.name.clone(), w.env));
        // Where commands run, only when it isn't this machine (a container, a Nix shell).
        if let Some((name, env)) = ws.as_ref().filter(|(_, e)| !matches!(e, byob_protocol::workspace::EnvKind::Auto | byob_protocol::workspace::EnvKind::Host))
        {
            let size = t.text_size(14.0);
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
            icons::env(ui.painter(), rect, *env, icons::env_color(t, *env));
            resp.on_hover_text(format!("Project {name}\n{}", icons::env_tip(*env)));
        }
        // Projects has its own big title on the page: nothing to repeat up here.
        if self.page == crate::Page::Projects {
            self.header_rects.title = Rect::NOTHING;
            return;
        }
        let page = match self.page {
            crate::Page::Projects => None,
            crate::Page::Apps => Some("Apps"),
            crate::Page::Library => Some("Library"),
            crate::Page::Plugins => Some("Plugins"),
            crate::Page::Chat => None,
        };
        if let Some(name) = page {
            ui.add_space(8.0);
            ui.label(egui::RichText::new(name).color(t.text_strong));
            self.header_rects.title = Rect::NOTHING;
            return;
        }
        let room = ui.available_width();
        let chat = self.sidebar.current().cloned();
        // The title, and a chevron when there is a chat to act on: its menu. All the room
        // the actions left (nothing follows it since the model moved under the message box;
        // half of it made "New chat" "Ne…" on a phone), never more (it ran over the chat
        // settings); with no room at all, no title (its menu is also the chat list's).
        let max = room - 4.0;
        if max < 44.0 {
            self.header_rects.title = Rect::NOTHING;
            return;
        }
        let title_resp = ui
            .scope(|ui| {
                ui.set_max_width(max);
                let mut job = egui::text::LayoutJob::default();
                let font = egui::TextStyle::Body.resolve(ui.style());
                if let Some((name, _)) = ws.as_ref().filter(|_| layout.workspace_name) {
                    job.append(&format!("{name} / "), 0.0, egui::TextFormat::simple(font.clone(), t.text_dim));
                }
                job.append(&title, 0.0, egui::TextFormat::simple(font, t.text_strong));
                job.wrap = egui::text::TextWrapping::truncate_at_width((max - 30.0).max(14.0));
                let galley = ui.painter().layout_job(job);
                let h = icons::button_height(ui);
                let menu = chat.is_some();
                let w = 8.0 + galley.size().x + if menu { 22.0 } else { 8.0 };
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, h), if menu { egui::Sense::click() } else { egui::Sense::hover() });
                let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
                if menu && (resp.hovered() || open) {
                    let state = if resp.is_pointer_button_down_on() { crate::glass::RowState::Pressed } else { crate::glass::RowState::Hovered };
                    crate::glass::row(ui.painter(), t, rect, state);
                }
                let at = egui::pos2(rect.left() + 8.0, rect.center().y - galley.size().y / 2.0);
                let gw = galley.size().x;
                ui.painter().galley(at, galley, t.text_strong);
                if menu {
                    let c = egui::pos2(rect.left() + 8.0 + gw + 11.0, rect.center().y);
                    icons::chevron_down(ui.painter(), Rect::from_center_size(c, egui::vec2(10.0, 10.0)), t.text_dim);
                }
                resp
            })
            .inner;
        self.header_rects.title = title_resp.rect;
        let Some(chat) = chat else { return };
        let title_resp = crate::tip::note(title_resp.on_hover_cursor(egui::CursorIcon::PointingHand), t, &title, "Rename, pin, archive or delete this chat");
        let pinned = chat.pinned_at.is_some();
        let can_project = self.engine_has(byob_protocol::version::CAP_UNATTACHED) && chat.workspace.is_none();
        let can_archive = self.engine_has(byob_protocol::version::CAP_ARCHIVE);
        let archived = chat.archived_at.is_some();
        let mut chosen = None;
        crate::menu::below_left(&title_resp, t, 230.0).show(|ui| {
            use crate::menu;
            menu::tighten(ui);
            if menu::item(ui, t, Some(icons::pencil), "Rename", Some("R")).clicked() || menu::key(ui, egui::Key::R) {
                chosen = Some(ChatAction::Rename);
                ui.close();
            }
            if menu::item(ui, t, Some(icons::pin), if pinned { "Unpin" } else { "Pin" }, Some("P")).clicked() || menu::key(ui, egui::Key::P) {
                chosen = Some(ChatAction::Pin);
                ui.close();
            }
            if can_project
                && (menu::item(ui, t, Some(icons::folder_plus), "Turn into project", Some("T"))
                    .on_hover_text("Its files become a project named after the chat")
                    .clicked()
                    || menu::key(ui, egui::Key::T))
            {
                chosen = Some(ChatAction::Project);
                ui.close();
            }
            menu::separator(ui, t);
            if menu::item(ui, t, Some(icons::sliders), "Chat settings", Some("S")).clicked() || menu::key(ui, egui::Key::S) {
                chosen = Some(ChatAction::Settings);
                ui.close();
            }
            if can_archive
                && (menu::item(ui, t, Some(icons::archive), if archived { "Unarchive" } else { "Archive" }, Some("A")).clicked() || menu::key(ui, egui::Key::A))
            {
                chosen = Some(ChatAction::Archive);
                ui.close();
            }
            menu::separator(ui, t);
            if menu::danger(ui, t, Some(icons::trash), "Delete", Some("D")).clicked() || menu::key(ui, egui::Key::D) {
                chosen = Some(ChatAction::Delete);
                ui.close();
            }
        });
        match chosen {
            Some(ChatAction::Rename) => self.chat_edit = Some(ChatEdit::Rename { id: chat.id.clone(), title: chat.title.clone() }),
            Some(ChatAction::Pin) => self.conn.send(byob_protocol::ClientMsg::PinChat { id: chat.id.clone(), pinned: !pinned }),
            Some(ChatAction::Project) => self.conn.send(byob_protocol::ClientMsg::ChatToProject { chat: chat.id.clone() }),
            Some(ChatAction::Settings) if self.workspaces.chat.is_none() => self.workspaces.toggle_chat(),
            Some(ChatAction::Archive) => self.conn.send(byob_protocol::ClientMsg::ArchiveChat { id: chat.id.clone(), archived: !archived }),
            Some(ChatAction::Delete) => self.chat_edit = Some(ChatEdit::Delete { id: chat.id.clone(), title: chat.title.clone() }),
            Some(ChatAction::Settings) | None => {}
        }
    }

    /// The title menu's dialogs: renaming the chat, or confirming its deletion.
    pub(crate) fn chat_edit_dialog(&mut self, ctx: &egui::Context) {
        let t = self.tokens;
        let Some(edit) = self.chat_edit.as_mut() else { return };
        match edit {
            ChatEdit::Rename { id, title } => {
                let mut done = None;
                egui::Modal::new(egui::Id::unique("rename-chat")).frame(crate::menu::frame(&t).inner_margin(16)).show(ctx, |ui| {
                    ui.set_width(320.0);
                    ui.label(egui::RichText::new("Rename chat").strong().color(t.text_strong));
                    ui.add_space(8.0);
                    let field = ui.add(egui::TextEdit::singleline(title).desired_width(f32::INFINITY));
                    field.request_focus();
                    ui.add_space(12.0);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if ui.button("Save").clicked() || enter {
                            done = Some(true);
                        }
                        if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                            done = Some(false);
                        }
                    });
                });
                match done {
                    Some(true) => {
                        let (id, title) = (id.clone(), title.trim().to_owned());
                        if !title.is_empty() {
                            self.conn.send(byob_protocol::ClientMsg::RenameChat { id, title });
                        }
                        self.chat_edit = None;
                    }
                    Some(false) => self.chat_edit = None,
                    None => {}
                }
            }
            ChatEdit::Delete { id, title } => {
                let body = format!("“{title}” and its messages are deleted; what was made in it stays in your Library. This can't be undone.");
                let choice = crate::menu::Confirm { title: "Delete this chat?", body: &body, action: "Delete", check: None }.show(
                    ctx,
                    &t,
                    egui::Id::unique("delete-chat"),
                );
                match choice {
                    crate::menu::Choice::Confirm => {
                        let id = id.clone();
                        self.conn.send(byob_protocol::ClientMsg::DeleteChat { id });
                        self.chat_edit = None;
                    }
                    crate::menu::Choice::Cancel => self.chat_edit = None,
                    crate::menu::Choice::Waiting => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn details_drop_as_the_window_narrows() {
        let full = Layout::for_width(1600.0);
        assert_eq!(full, Layout { workspace_name: true, chip: ChipDetail::Full, plugin_count: true, pane_toggles: true });
        let laptop = Layout::for_width(1100.0);
        assert_eq!(laptop, Layout { workspace_name: true, chip: ChipDetail::Model, plugin_count: true, pane_toggles: true });
        let small = Layout::for_width(800.0);
        assert_eq!(small, Layout { workspace_name: true, chip: ChipDetail::Model, plugin_count: true, pane_toggles: true });
        let phone = Layout::for_width(390.0);
        assert_eq!(phone, Layout { workspace_name: false, chip: ChipDetail::Dot, plugin_count: false, pane_toggles: false });
        // Narrowing never brings something back.
        let mut last = full;
        for w in (320..=1600).rev().step_by(10) {
            #[allow(clippy::cast_precision_loss)] // small
            let l = Layout::for_width(w as f32);
            assert!(l.plugin_count <= last.plugin_count, "{w}");
            assert!(l.workspace_name <= last.workspace_name && (l.chip as u8) <= (last.chip as u8), "{w}");
            last = l;
        }
    }
}
