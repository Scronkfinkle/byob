//! The settings screen: a destination of its own that replaces the chat while open. On a
//! desktop the left column becomes the settings' navigation (Back, a search field, the
//! categories) and the selected category's page takes the rest; on phones a list of
//! categories opens each page (with a way back). The header stays.
//!
//! What the pages show comes from the [registry](registry): every setting declared once
//! (name, description, keywords, tab and section), drawn by [`controls`]. The search matches
//! the same entries, plus the items of lists (saved models, plugins, themes, color roles).
//! Models are engine-wide; chat defaults are the global layer of the layered settings;
//! appearance, text & layout and colors are per device.

mod controls;
pub(crate) mod registry;
mod shown;

use std::sync::mpsc::{Receiver, Sender, channel};

use egui::{Id, RichText, Stroke, pos2, vec2};

pub(crate) use controls::pick_theme;
use registry::{Key, Kind, SECTIONS, SETTINGS, Section, Setting};

use crate::ByobApp;
use crate::form::{self, RowStyle};
use crate::icons;
use crate::theme::file::{self, Origin, ThemeDef};
use crate::theme::{self, Prefs, ROLES, Tokens};
use crate::upload::Picked;

/// The settings' categories, one page each.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Tab {
    /// Saved models and their roles (the connection form on engines without profiles).
    #[default]
    Models,
    /// Defaults for every chat (the global level of the layered settings).
    Defaults,
    Plugins,
    /// Light/dark, the theme gallery, the accent and shape, messages, theme files.
    Appearance,
    /// Zoom, text sizes, fonts, density, widths, accessibility.
    Text,
    /// Every color role of the current theme.
    Colors,
    /// The desktop app on this computer: keep running in the background, start at login.
    System,
}

impl Tab {
    pub const ALL: [Tab; 7] = [Tab::Models, Tab::Defaults, Tab::Plugins, Tab::Appearance, Tab::Text, Tab::Colors, Tab::System];

    /// The name scripts use (`--settings-section`).
    pub fn id(self) -> &'static str {
        match self {
            Tab::Models => "models",
            Tab::Defaults => "defaults",
            Tab::Plugins => "plugins",
            Tab::Appearance => "appearance",
            Tab::Text => "text",
            Tab::Colors => "colors",
            Tab::System => "system",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Tab::Models => "Models",
            Tab::Defaults => "Chats",
            Tab::Plugins => "Plugins",
            Tab::Appearance => "Appearance",
            Tab::Text => "Text & layout",
            Tab::Colors => "Colors",
            Tab::System => "System",
        }
    }

    /// What is in it, in a few words (the phone list).
    fn summary(self) -> &'static str {
        match self {
            Tab::Models => "Saved models and their roles",
            Tab::Defaults => "Instructions, approvals, context",
            Tab::Plugins => "Tools, skills and servers",
            Tab::Appearance => "Theme, accent, messages",
            Tab::Text => "Zoom, fonts, density, accessibility",
            Tab::Colors => "All the theme's colors, one by one",
            Tab::System => "In the background, at login",
        }
    }

    fn icon(self) -> icons::Paint {
        match self {
            Tab::Models => icons::chip,
            Tab::Defaults => icons::sliders,
            Tab::Plugins => icons::puzzle,
            Tab::Appearance => icons::contrast,
            Tab::Text => icons::text_size,
            Tab::Colors => icons::colors,
            Tab::System => icons::app_window,
        }
    }

    /// A tab by its [`Tab::id`], or by the older section names: `knobs` (text & layout),
    /// `advanced` / `colors` (colors), `connection`, `chats`, `model:<id>` (models).
    pub fn find(name: &str) -> Option<Tab> {
        let name = name.trim();
        if name.starts_with("model:") {
            return Some(Tab::Models);
        }
        Tab::ALL.into_iter().find(|t| t.id() == name).or(match name {
            "knobs" | "text-and-layout" | "layout" => Some(Tab::Text),
            "advanced" | "colours" => Some(Tab::Colors),
            "connection" => Some(Tab::Models),
            "chat-defaults" | "chats" => Some(Tab::Defaults),
            _ => None,
        })
    }
}

/// A search result.
#[derive(Debug, Clone)]
enum Hit {
    Setting(&'static Setting),
    /// A saved model: (id, name, what it is).
    Model(String, String, String),
    /// A plugin: (id, name, description).
    Plugin(String, String, String),
    Theme(ThemeDef),
    /// Color roles: how many match, and the first few names.
    Colors(usize, String),
}

impl Hit {
    fn key(&self) -> Option<Key> {
        match self {
            Hit::Setting(s) => Some(s.key),
            _ => None,
        }
    }
}

/// The settings screen between frames: the category in view (kept for the session), the
/// search, where a deep link or result should land, and the appearance pages' import/export
/// state.
pub struct AppearanceUi {
    import_tx: Sender<Picked>,
    import_rx: Receiver<Picked>,
    exports: crate::export::Exports,
    export_name: String,
    /// What the last import/export said (ok, text).
    status: Vec<(bool, String)>,
    filter: String,
    /// The category in view.
    pub(crate) tab: Tab,
    /// What is typed in the search field.
    pub(crate) search: String,
    /// Put the keyboard in the search field next frame (Ctrl+F in the settings).
    pub(crate) focus_search: bool,
    /// Phones: a category's page is open (else the list of categories).
    pub(crate) detail: bool,
    /// Scroll to this setting when it is next drawn, then light it up for a moment.
    pub(crate) jump: Option<Key>,
    /// Frames left to keep scrolling to [`Self::jump`] (a frame may be laid out twice, and
    /// rows settle their size in the first ones).
    jump_frames: u8,
    flash: Option<(Key, f64)>,
    /// What the last frame drew: each setting and where.
    pub(crate) drawn: Vec<(Key, egui::Rect)>,
    /// Appearance's Advanced is open (its groups shown on the page).
    pub(crate) advanced: bool,
}

impl Default for AppearanceUi {
    fn default() -> Self {
        let (import_tx, import_rx) = channel();
        Self {
            import_tx,
            import_rx,
            exports: crate::export::Exports::default(),
            export_name: String::new(),
            status: Vec::new(),
            filter: String::new(),
            tab: Tab::default(),
            search: String::new(),
            focus_search: false,
            detail: false,
            jump: None,
            jump_frames: 0,
            flash: None,
            drawn: Vec::new(),
            advanced: false,
        }
    }
}

/// How long a setting stays lit after a jump to it, in seconds.
const FLASH_S: f64 = 1.8;

impl ByobApp {
    /// Open the settings where they were (phones: on the list of categories).
    pub(crate) fn open_settings(&mut self) {
        self.show_settings = true;
        self.appearance.detail = false;
    }

    /// Open the settings on `tab`'s page.
    pub(crate) fn open_settings_at(&mut self, tab: Tab) {
        self.show_settings = true;
        self.appearance.tab = tab;
        self.appearance.detail = true;
        self.appearance.search.clear();
    }

    /// Back to the chat as it was: nothing of it changed while the settings were open.
    pub(crate) fn close_settings(&mut self) {
        self.show_settings = false;
        self.appearance.search.clear();
        self.appearance.jump = None;
        self.appearance.flash = None;
    }

    /// Ctrl+,.
    pub(crate) fn toggle_settings(&mut self) {
        if self.show_settings {
            self.close_settings();
        } else {
            self.open_settings();
        }
    }

    /// Scripted runs: open the settings with `text` in the search field.
    pub fn set_settings_search(&mut self, text: &str) {
        self.open_settings();
        text.clone_into(&mut self.appearance.search);
    }

    /// Open the page of `key`'s setting, scrolled to it and lit for a moment.
    pub(crate) fn jump_to(&mut self, key: Key) {
        if let Some(s) = registry::get(key) {
            self.open_settings_at(s.section.tab);
            // A setting of Appearance's Advanced: open it (it stays open for a page setting).
            self.appearance.advanced |= registry::is_advanced(s.section);
            self.appearance.jump = Some(key);
            self.appearance.jump_frames = 4;
        }
    }

    /// Keys while the settings are open (before anything is drawn): Esc clears the search,
    /// goes back to the list of categories on a phone, or back to the chat. Not while typing
    /// or with a menu open: those take Esc themselves.
    pub(crate) fn settings_keys(&mut self, ctx: &egui::Context, narrow: bool) {
        if !ctx.input(|i| i.key_pressed(egui::Key::Escape)) || ctx.text_edit_focused() || egui::Popup::is_any_open(ctx) {
            return;
        }
        if !self.appearance.search.is_empty() {
            self.appearance.search.clear();
        } else if narrow && self.appearance.detail {
            self.appearance.detail = false;
        } else {
            self.close_settings();
        }
    }

    /// The settings screen, below the header: the navigation column and the page beside it,
    /// or on phones the list of categories or one page.
    pub(crate) fn settings_screen(&mut self, ui: &mut egui::Ui, narrow: bool) {
        crate::fluid::region(ui, "settings", |ui| self.settings_inner(ui, narrow))
    }

    fn settings_inner(&mut self, ui: &mut egui::Ui, narrow: bool) {
        let t = self.tokens;
        if std::mem::take(&mut self.plugins.reveal) {
            self.open_settings_at(Tab::Plugins);
        }
        self.appearance.drawn.clear();
        let page = egui::Frame::central_panel(ui.style()).inner_margin(0.0).fill(t.app_bg);
        if narrow {
            egui::CentralPanel::default().frame(page).show(ui, |ui| {
                self.header_rects.settings_page = ui.max_rect();
                form::controls(ui, &t);
                self.phone_settings(ui, &t);
            });
            return;
        }
        let nav_w = t.knobs.sidebar_width.max(form::NAV_W * t.knobs.font_scale.clamp(0.9, 1.4)).min(ui.available_width() * 0.36);
        egui::Panel::left("settings-nav")
            .resizable(false)
            .exact_size(nav_w)
            .frame(egui::Frame::side_top_panel(ui.style()).fill(t.sidebar_bg).inner_margin(egui::Margin::symmetric(10, 0)))
            .show(ui, |ui| {
                form::controls(ui, &t);
                self.settings_nav(ui, &t);
            });
        egui::CentralPanel::default().frame(page).show(ui, |ui| {
            self.header_rects.settings_page = ui.max_rect();
            form::controls(ui, &t);
            self.settings_content(ui, &t, t.pad(form::PAGE_PAD));
        });
    }

    /// The desktop's left column: Back, the search field, the categories (with how many
    /// results each has while searching).
    fn settings_nav(&mut self, ui: &mut egui::Ui, t: &Tokens) {
        let w = ui.available_width();
        ui.add_space(t.pad(10.0));
        ui.spacing_mut().item_spacing.y = t.pad(2.0);
        let back = nav_item(ui, t, icons::arrow_left, "Back to chat", NavLook { width: w, hint: Some("Esc"), ..NavLook::default() })
            .on_hover_text("Close the settings and go back to the chat as you left it");
        self.header_rects.settings_back = back.rect;
        if back.clicked() {
            self.close_settings();
        }
        ui.add_space(t.pad(10.0));
        self.search_field(ui, t, w);
        ui.add_space(t.pad(14.0));
        let words = registry::words(&self.appearance.search);
        let counts: Option<Vec<usize>> = (!words.is_empty()).then(|| self.search_hits(&words).iter().map(|(_, h)| h.len()).collect());
        for (i, tab) in Tab::ALL.into_iter().enumerate() {
            if !self.tab_shown(tab) {
                continue;
            }
            let count = counts.as_ref().map(|c| c.get(i).copied().unwrap_or(0));
            let hint = count.filter(|n| *n > 0).map(|n| n.to_string());
            let look =
                NavLook { width: w, selected: count.is_none() && self.appearance.tab == tab, hint: hint.as_deref(), dim: count == Some(0), chevron: false };
            let r = nav_item(ui, t, tab.icon(), self.tab_label(tab), look);
            self.header_rects.settings_tabs[i] = r.rect;
            if r.clicked() {
                self.open_settings_at(tab);
            }
        }
    }

    /// The search field: Enter goes to the first result, ↓ to the results, Esc clears it.
    fn search_field(&mut self, ui: &mut egui::Ui, t: &Tokens, width: f32) {
        let id = Id::unique("settings-search");
        if std::mem::take(&mut self.appearance.focus_search) {
            ui.memory_mut(|m| m.request_focus(id));
        }
        let h = form::control_h(t).max(t.pad(30.0));
        let icon = t.text_size(14.0);
        #[allow(clippy::cast_possible_truncation)] // a few points
        let inset = (icon + 14.0) as i8;
        let edit = egui::TextEdit::singleline(&mut self.appearance.search)
            .id(id)
            .hint_text(RichText::new("Search settings").color(t.text_faint))
            .margin(egui::Margin { left: inset, right: inset, top: 0, bottom: 0 })
            .align(egui::Align2::LEFT_CENTER)
            .min_size(vec2(width, h))
            .desired_width(width);
        let resp = ui.add(edit);
        self.header_rects.settings_search = resp.rect;
        let (esc, enter, down) = ui.input(|i| (i.key_pressed(egui::Key::Escape), i.key_pressed(egui::Key::Enter), i.key_pressed(egui::Key::ArrowDown)));
        if resp.lost_focus() && esc {
            self.appearance.search.clear();
        }
        if resp.lost_focus() && enter {
            let words = registry::words(&self.appearance.search);
            if let Some(hit) = self.search_hits(&words).into_iter().find_map(|(_, h)| h.into_iter().next()) {
                self.open_hit(&hit);
            }
        }
        if resp.has_focus() && down && !self.appearance.search.is_empty() {
            ui.memory_mut(|m| m.move_focus(egui::FocusDirection::Next));
        }
        let p = ui.painter();
        let c = resp.rect.left_center() + vec2(8.0 + icon / 2.0, 0.0);
        icons::search(p, egui::Rect::from_center_size(c, vec2(icon, icon)), if resp.has_focus() { t.text } else { t.text_faint });
        if !self.appearance.search.is_empty() {
            let r = egui::Rect::from_center_size(resp.rect.right_center() - vec2(6.0 + icon / 2.0, 0.0), vec2(icon + 4.0, icon + 4.0));
            let x = ui.interact(r, id.with("clear"), egui::Sense::click()).on_hover_text("Clear the search (Esc)");
            if x.hovered() {
                ui.painter().rect_filled(r, t.round(4.0), t.hover);
            }
            icons::cross(ui.painter(), r, if x.hovered() { t.text_strong } else { t.text_dim });
            if x.clicked() {
                self.appearance.search.clear();
            }
        }
    }

    /// Phones: the list of categories (Back, search, categories) or one category's page.
    fn phone_settings(&mut self, ui: &mut egui::Ui, t: &Tokens) {
        let pad = t.pad(form::PAGE_PAD_NARROW);
        let searching = !self.appearance.search.trim().is_empty();
        let inner = ui.max_rect().shrink2(vec2(pad, 0.0));
        let w = inner.width();
        ui.add_space(t.pad(8.0));
        let (label, to_list) = if self.appearance.detail && !searching { ("Settings", true) } else { ("Back to chat", false) };
        let back = ui
            .horizontal(|ui| {
                ui.add_space(pad);
                nav_item(ui, t, icons::arrow_left, label, NavLook { width: w, ..NavLook::default() })
            })
            .inner;
        self.header_rects.settings_back = back.rect;
        if back.clicked() {
            if to_list {
                self.appearance.detail = false;
            } else {
                self.close_settings();
            }
        }
        if !to_list {
            ui.add_space(t.pad(8.0));
            ui.horizontal(|ui| {
                ui.add_space(pad);
                self.search_field(ui, t, w);
            });
        }
        ui.add_space(t.pad(8.0));
        let y = ui.cursor().top();
        ui.painter().hline(ui.max_rect().x_range(), y, Stroke::new(1.0, t.separator));
        ui.add_space(1.0);
        if to_list || searching {
            self.settings_content(ui, t, pad);
            return;
        }
        egui::ScrollArea::vertical().id_salt("settings-categories").auto_shrink([false, false]).show(ui, |ui| {
            ui.add_space(t.pad(8.0));
            ui.spacing_mut().item_spacing.y = t.pad(2.0);
            for (i, tab) in Tab::ALL.into_iter().enumerate() {
                if !self.tab_shown(tab) {
                    continue;
                }
                let r = ui
                    .horizontal(|ui| {
                        ui.add_space(pad);
                        category_row(ui, t, tab, self.tab_label(tab), tab.summary(), w)
                    })
                    .inner;
                self.header_rects.settings_tabs[i] = r.rect;
                if r.clicked() {
                    self.open_settings_at(tab);
                }
            }
        });
    }

    /// The tab is listed: System only where there is a desktop app (not in a browser).
    fn tab_shown(&self, tab: Tab) -> bool {
        tab != Tab::System || self.desktop.is_some()
    }

    fn tab_label(&self, tab: Tab) -> &'static str {
        if tab == Tab::Models && !self.models.available() { "Connection" } else { tab.label() }
    }

    fn tab_blurb(&self, tab: Tab) -> String {
        match tab {
            Tab::Models if self.models.available() => "The models byob can use, saved for every window and browser tab.".into(),
            Tab::Models => "The model server byob talks to; saved for every window and browser tab.".into(),
            Tab::Defaults => "Defaults for every chat, saved in settings.json. A project or a chat can change each of them for itself.".into(),
            Tab::Plugins => "What the chat in view can use: plugins of this chat, its project and every chat.".into(),
            Tab::Appearance => "How byob looks on this device: its theme, colors and messages.".into(),
            Tab::Text => "Sizes, fonts and how the window is laid out. Saved on this device only.".into(),
            Tab::Colors => {
                let current = file::resolve(self.prefs.theme_id(self.tokens.dark), self.tokens.dark, &self.user_themes);
                format!("Every color of {}, changed on this device only. Export the theme under Appearance → Theme files to keep them.", current.name)
            }
            Tab::System => "How byob runs on this computer.".into(),
        }
    }

    fn section_title(&self, sec: &Section) -> &'static str {
        if std::ptr::eq(sec, &registry::SAVED_MODELS) && !self.models.available() { "Connection" } else { sec.title }
    }

    /// The page (or the search's results), scrolling on its own in a column at most
    /// [`form::MAX_W`] wide, centered in what is left of the window.
    fn settings_content(&mut self, ui: &mut egui::Ui, t: &Tokens, pad: f32) {
        let searching = !self.appearance.search.trim().is_empty();
        let tab = self.appearance.tab;
        let salt = if searching { "search" } else { tab.id() };
        egui::ScrollArea::vertical().id_salt(("settings-page", salt)).auto_shrink([false, false]).show(ui, |ui| {
            let avail = ui.available_width();
            let max = form::MAX_W * t.knobs.font_scale.clamp(1.0, 1.3);
            let w = (avail - 2.0 * pad).clamp(120.0_f32.min(avail), max);
            let left = ((avail - w) / 2.0).max(0.0);
            ui.add_space(t.pad(form::PAGE_TOP));
            ui.horizontal_top(|ui| {
                ui.add_space(left);
                ui.vertical(|ui| {
                    ui.set_width(w);
                    self.header_rects.settings_body = ui.max_rect();
                    if searching {
                        self.search_results(ui, t);
                    } else {
                        self.settings_tab_page(ui, t, tab);
                    }
                });
            });
            ui.add_space(pad * 2.0);
        });
    }

    /// One category: its heading, then each section's settings in registry order. On
    /// Appearance, the fine-tuning sits under Advanced, folded, before the last row (Reset).
    fn settings_tab_page(&mut self, ui: &mut egui::Ui, t: &Tokens, tab: Tab) {
        form::heading(ui, t, self.tab_label(tab), &self.tab_blurb(tab));
        let mut d = self.begin_draw();
        let page: Vec<&'static Section> = SECTIONS.iter().copied().filter(|s| s.tab == tab && !registry::is_advanced(s)).collect();
        if tab == Tab::Appearance {
            let (main, last): (Vec<&'static Section>, Vec<&'static Section>) = page.into_iter().partition(|s| !std::ptr::eq(*s, &registry::RESET_LOOK));
            self.section_cards(ui, t, &main, &mut d);
            if advanced_disclosure(ui, t, self.appearance.advanced).clicked() {
                self.appearance.advanced = !self.appearance.advanced;
            }
            if self.appearance.advanced {
                let advanced: Vec<&'static Section> = registry::ADVANCED.to_vec();
                self.section_cards(ui, t, &advanced, &mut d);
            }
            self.section_cards(ui, t, &last, &mut d);
        } else {
            self.section_cards(ui, t, &page, &mut d);
        }
        self.end_draw(d);
    }

    /// Sections as cards: a small heading over each, its rows in a card (a section that is
    /// one long list, like the models or the plugins, is drawn on the page as it is).
    fn section_cards(&mut self, ui: &mut egui::Ui, t: &Tokens, sections: &[&'static Section], d: &mut controls::Draw) {
        let mut first = true;
        for sec in sections.iter().copied() {
            let rows: Vec<&'static Setting> = SETTINGS.iter().filter(|s| std::ptr::eq(s.section, sec) && self.setting_shown(s.key)).collect();
            if rows.is_empty() {
                continue;
            }
            form::section(ui, t, self.section_title(sec), sec.blurb, first);
            first = false;
            // A section of rows is a card (a list draws its own).
            if rows.iter().any(|s| s.kind != Kind::Block) {
                form::card(t).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    for (i, s) in rows.into_iter().enumerate() {
                        self.draw_setting(ui, t, s, d, &[], i == 0, false);
                    }
                });
            } else {
                for (i, s) in rows.into_iter().enumerate() {
                    self.draw_setting(ui, t, s, d, &[], i == 0, false);
                }
            }
            if std::ptr::eq(sec, &registry::THEME_FILES) {
                self.theme_file_status(ui, t);
            }
        }
    }

    /// One setting, where it is on its page or as a search result (`link`: its name goes to
    /// its page; a list becomes a row that opens it).
    #[allow(clippy::too_many_arguments)] // one row, placed
    fn draw_setting(&mut self, ui: &mut egui::Ui, t: &Tokens, s: &'static Setting, d: &mut controls::Draw, needles: &[String], first: bool, link: bool) {
        let flash = self.flash_level(ui.ctx(), s.key);
        let id = Id::unique(("setting", s.id, link));
        let style = RowStyle { needles, first, wide: s.kind == Kind::Wide, flash, link };
        let rect = match s.kind {
            Kind::Block if link => {
                let out = form::setting_row(ui, t, id, s.name, s.desc, style, |ui| {
                    ui.button("Show").on_hover_text(format!("Go to {}", s.section.tab.label())).clicked()
                });
                if out.inner || out.name.clicked() {
                    self.jump_to(s.key);
                }
                out.rect
            }
            Kind::Block => {
                let top = ui.cursor().top();
                ui.add_space(t.pad(6.0));
                self.draw_control(ui, t, s.key, d);
                let r = ui.max_rect();
                egui::Rect::from_min_max(pos2(r.left(), top), pos2(r.right(), ui.cursor().top().max(top + 1.0)))
            }
            Kind::Row | Kind::Wide => {
                let out = form::setting_row(ui, t, id, s.name, s.desc, style, |ui| self.draw_control(ui, t, s.key, d));
                if link && out.name.on_hover_text(format!("Show it in {} → {}", s.section.tab.label(), s.section.title)).clicked() {
                    self.jump_to(s.key);
                }
                out.rect
            }
        };
        self.appearance.drawn.push((s.key, rect));
        if !link && self.appearance.jump == Some(s.key) {
            let align = if s.kind == Kind::Block { egui::Align::Min } else { egui::Align::Center };
            ui.scroll_to_rect_animation(rect, Some(align), egui::style::ScrollAnimation::none());
            if self.appearance.flash.is_none_or(|(k, _)| k != s.key) {
                self.appearance.flash = Some((s.key, ui.ctx().input(|i| i.time)));
            }
            self.appearance.jump_frames = self.appearance.jump_frames.saturating_sub(1);
            if self.appearance.jump_frames == 0 {
                self.appearance.jump = None;
            }
            ui.ctx().request_repaint();
        }
    }

    /// How lit `key`'s row is (1 just after a jump, fading to 0).
    fn flash_level(&mut self, ctx: &egui::Context, key: Key) -> f32 {
        let Some((k, at)) = self.appearance.flash else { return 0.0 };
        if k != key {
            return 0.0;
        }
        let age = ctx.input(|i| i.time) - at;
        if age > FLASH_S {
            self.appearance.flash = None;
            return 0.0;
        }
        if self.tokens.knobs.reduce_motion {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64((FLASH_S - age).max(0.0)));
            return 1.0;
        }
        ctx.request_repaint();
        #[allow(clippy::cast_possible_truncation)] // 0..=1
        let left = (1.0 - age / FLASH_S) as f32;
        left.clamp(0.0, 1.0).powf(0.6)
    }

    /// What `words` find, by category in page order: settings (best match first), then the
    /// items of that category's lists. Every category is listed, perhaps with nothing.
    fn search_hits(&self, words: &[String]) -> Vec<(Tab, Vec<Hit>)> {
        Tab::ALL
            .into_iter()
            .map(|tab| {
                let mut found: Vec<(&'static Setting, u32)> = SETTINGS
                    .iter()
                    .filter(|s| s.section.tab == tab && self.setting_shown(s.key))
                    .filter_map(|s| registry::score(s, words).map(|n| (s, n)))
                    .collect();
                found.sort_by_key(|h| std::cmp::Reverse(h.1));
                let mut hits: Vec<Hit> = found.into_iter().map(|(s, _)| Hit::Setting(s)).collect();
                match tab {
                    Tab::Models => {
                        for v in self.models.list.iter().flat_map(|l| l.items.iter()) {
                            let p = &v.profile;
                            if registry::matches_all(&format!("{} {} {} {}", p.label(), p.model, p.base_url, p.description), words) {
                                let what = format!("{} · {}", p.model, crate::connect::short_url(&p.base_url));
                                hits.push(Hit::Model(p.id.clone(), p.label().to_owned(), what));
                            }
                        }
                    }
                    Tab::Plugins => {
                        for p in self.plugins.extensions().iter().flat_map(|e| e.plugins.iter()).filter(|p| p.shadowed_by.is_none()) {
                            if registry::matches_all(&format!("{} {} {}", p.name, p.id, p.description), words) {
                                hits.push(Hit::Plugin(p.id.clone(), p.name.clone(), p.description.clone()));
                            }
                        }
                    }
                    Tab::Appearance => {
                        for def in file::all(&self.user_themes).into_iter().filter(|d| registry::matches_all(&d.name, words)).take(8) {
                            hits.push(Hit::Theme(def));
                        }
                    }
                    Tab::Colors => {
                        let roles: Vec<&str> =
                            ROLES.iter().filter(|r| registry::matches_all(&format!("{} {} {}", r.label, r.name, r.group), words)).map(|r| r.label).collect();
                        if !roles.is_empty() {
                            let mut names = roles.iter().take(4).copied().collect::<Vec<_>>().join(", ");
                            if roles.len() > 4 {
                                names.push_str(", …");
                            }
                            hits.push(Hit::Colors(roles.len(), names));
                        }
                    }
                    Tab::Defaults | Tab::Text | Tab::System => {}
                }
                (tab, hits)
            })
            .collect()
    }

    /// Go where a result is (Enter in the search field, or its button).
    fn open_hit(&mut self, hit: &Hit) {
        match hit {
            Hit::Setting(s) => self.jump_to(s.key),
            Hit::Model(id, ..) => {
                self.models.start_edit(id);
                self.jump_to(Key::ModelList);
            }
            Hit::Plugin(id, ..) => {
                self.plugins.open(Some(id));
                self.jump_to(Key::PluginList);
            }
            Hit::Theme(def) => {
                let mut p = self.prefs.clone();
                pick_theme(&mut p, def, self.tokens.dark);
                self.set_prefs(p);
                self.jump_to(Key::ThemePair);
            }
            Hit::Colors(..) => {
                self.appearance.filter = self.appearance.search.trim().to_owned();
                self.jump_to(Key::ColorRoles);
            }
        }
    }

    /// The search's results: every matching setting as it is on its page (its control works
    /// here), grouped under its category; lists as rows that open them.
    fn search_results(&mut self, ui: &mut egui::Ui, t: &Tokens) {
        let query = self.appearance.search.trim().to_owned();
        let words = registry::words(&query);
        let hits = self.search_hits(&words);
        let n: usize = hits.iter().map(|(_, h)| h.len()).sum();
        if n == 0 {
            ui.add_space(t.pad(48.0));
            ui.vertical_centered(|ui| {
                let s = t.text_size(36.0);
                let (r, _) = ui.allocate_exact_size(vec2(s, s), egui::Sense::hover());
                icons::search(ui.painter(), r, t.text_faint);
                ui.add_space(t.pad(12.0));
                ui.label(RichText::new(format!("No settings match “{query}”")).size(t.text_size(17.0)).color(t.text_strong));
                ui.add_space(t.pad(4.0));
                ui.label(RichText::new("Try another word, like theme, font, approvals or model.").color(t.text_dim));
            });
            return;
        }
        let blurb = format!("{n} {} for “{query}”. Change them right here, or click a name to see it in its page.", if n == 1 { "result" } else { "results" });
        form::heading(ui, t, "Search", &blurb);
        let mut d = self.begin_draw();
        let mut action = None;
        let mut first = true;
        for (tab, items) in hits.into_iter().filter(|(_, h)| !h.is_empty()) {
            form::section(ui, t, "", "", first);
            first = false;
            // The category: a link to its page.
            let r = ui
                .horizontal(|ui| {
                    let s = t.text_size(15.0);
                    let (ir, _) = ui.allocate_exact_size(vec2(s, s), egui::Sense::hover());
                    let title = RichText::new(self.tab_label(tab)).size(t.text_size(15.0)).family(theme::bold_or_regular(ui.ctx())).color(t.text_strong);
                    let r = ui.add(egui::Label::new(title).sense(egui::Sense::click())).on_hover_cursor(egui::CursorIcon::PointingHand);
                    (tab.icon())(ui.painter(), ir, if r.hovered() { t.accent } else { t.text_dim });
                    let (cr, _) = ui.allocate_exact_size(vec2(s, s), egui::Sense::hover());
                    icons::chevron_right(ui.painter(), cr, if r.hovered() { t.accent } else { t.text_faint });
                    r.on_hover_text(format!("Open {}", tab.label()))
                })
                .inner;
            if r.clicked() {
                self.open_settings_at(tab);
            }
            ui.add_space(t.pad(2.0));
            for (i, hit) in items.into_iter().enumerate() {
                let first = i == 0;
                if let Some(key) = hit.key()
                    && let Some(s) = registry::get(key)
                {
                    self.draw_setting(ui, t, s, &mut d, &words, first, true);
                    continue;
                }
                let style = RowStyle { needles: &words, first, link: true, ..RowStyle::default() };
                let (id, name, desc, button) = match &hit {
                    Hit::Model(id, name, what) => (Id::unique(("hit-model", id)), name.clone(), what.clone(), "Edit"),
                    Hit::Plugin(id, name, what) => (Id::unique(("hit-plugin", id)), name.clone(), what.clone(), "Open"),
                    Hit::Theme(def) => {
                        let what = format!("{} theme · {}", if def.dark { "Dark" } else { "Light" }, def.credit());
                        (Id::unique(("hit-theme", &def.id)), def.name.clone(), what, "Use")
                    }
                    Hit::Colors(count, names) => {
                        (Id::unique("hit-colors"), format!("{count} color role{}", if *count == 1 { "" } else { "s" }), format!("{names}."), "Show")
                    }
                    Hit::Setting(_) => continue,
                };
                let out = form::setting_row(ui, t, id, &name, &desc, style, |ui| ui.button(button).clicked());
                if out.inner || out.name.clicked() {
                    action = Some(hit.clone());
                }
            }
        }
        self.end_draw(d);
        if let Some(hit) = action {
            self.open_hit(&hit);
        }
    }

    /// Imports that finished picking, and export outcomes (every frame).
    pub(crate) fn poll_appearance(&mut self) {
        let picked: Vec<Picked> = std::iter::from_fn(|| self.appearance.import_rx.try_recv().ok()).collect();
        for p in picked {
            match p {
                Ok((name, bytes)) => self.import_theme(&name, &bytes),
                Err(e) if e.is_empty() => {}
                Err(e) => self.appearance.status.push((false, e)),
            }
        }
        for o in self.appearance.exports.poll() {
            self.appearance.status.push(match o {
                Ok(s) => (true, s),
                Err(e) => (false, e),
            });
        }
        let excess = self.appearance.status.len().saturating_sub(4);
        self.appearance.status.drain(..excess);
    }

    /// A picked theme file: into the themes folder (desktop) or browser storage, then use it.
    fn import_theme(&mut self, name: &str, bytes: &[u8]) {
        let Ok(text) = std::str::from_utf8(bytes) else {
            self.appearance.status.push((false, format!("{name}: not a text file")));
            return;
        };
        let parsed = match file::parse(text, Origin::Imported(0)) {
            Ok(p) => p,
            Err(e) => {
                self.appearance.status.push((false, format!("{name}: {e}")));
                return;
            }
        };
        let id = parsed.def.id.clone();
        let mut prefs = self.prefs.clone();
        let stored = self.store_theme(&mut prefs, &parsed.def, text);
        match stored {
            Ok(where_) => {
                let warn =
                    if parsed.warnings.is_empty() { String::new() } else { format!(" ({} warnings: {})", parsed.warnings.len(), parsed.warnings.join("; ")) };
                self.appearance.status.push((parsed.warnings.is_empty(), format!("Imported \"{}\" {where_}{warn}", parsed.def.name)));
            }
            Err(e) => {
                self.appearance.status.push((false, e));
                return;
            }
        }
        self.set_prefs(prefs.clone());
        self.reload_themes();
        if let Some(def) = self.user_themes.iter().find(|d| d.id == id).cloned() {
            pick_theme(&mut prefs, &def, self.tokens.dark);
            self.set_prefs(prefs);
        }
    }

    /// Keep a theme file's text: the desktop themes folder if there is one, else browser storage.
    fn store_theme(&self, prefs: &mut Prefs, def: &ThemeDef, text: &str) -> Result<String, String> {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(dir) = &self.themes_dir {
            let path = dir.join(format!("{}.json", file::slug(&def.name)));
            std::fs::create_dir_all(dir).map_err(|e| format!("Couldn't create {}: {e}", dir.display()))?;
            std::fs::write(&path, text).map_err(|e| format!("Couldn't save {}: {e}", path.display()))?;
            return Ok(format!("to {}", path.display()));
        }
        // Replace an earlier import of the same theme.
        prefs.imported.retain(|old| file::parse(old, Origin::Imported(0)).map_or(true, |f| f.def.id != def.id));
        if prefs.imported.len() >= Prefs::MAX_IMPORTED {
            return Err(format!("At most {} imported themes; remove one first", Prefs::MAX_IMPORTED));
        }
        prefs.imported.push(text.to_owned());
        Ok("into this browser".into())
    }

    /// Delete a user theme (its file, or the browser's copy).
    fn remove_theme(&mut self, def: &ThemeDef) {
        let mut prefs = self.prefs.clone();
        match &def.origin {
            Origin::Builtin => return,
            Origin::Imported(i) => {
                if *i < prefs.imported.len() {
                    prefs.imported.remove(*i);
                }
            }
            #[cfg(not(target_arch = "wasm32"))]
            Origin::Folder(name) => {
                if let Some(dir) = &self.themes_dir
                    && let Err(e) = std::fs::remove_file(dir.join(name))
                {
                    self.appearance.status.push((false, format!("Couldn't remove {name}: {e}")));
                }
            }
            #[cfg(target_arch = "wasm32")]
            Origin::Folder(_) => {}
        }
        prefs.overrides.remove(&def.id);
        self.set_prefs(prefs);
        self.reload_themes();
    }
}

/// How a [`nav_item`] looks.
#[derive(Debug, Clone, Copy, Default)]
struct NavLook<'a> {
    width: f32,
    selected: bool,
    /// Faint text at the right: a key, a count.
    hint: Option<&'a str>,
    /// Nothing there (a category without results).
    dim: bool,
    chevron: bool,
}

/// One row of the settings' navigation: an icon and a label, the selected one filled.
fn nav_item(ui: &mut egui::Ui, t: &Tokens, icon: icons::Paint, label: &str, look: NavLook<'_>) -> egui::Response {
    let h = t.pad(form::NAV_ROW_H).max(form::control_h(t));
    let size = t.text_size(16.0).min(h - 8.0).max(10.0);
    let pad = t.pad(10.0);
    let color = if look.selected {
        t.text_strong
    } else if look.dim {
        t.text_faint
    } else {
        t.text
    };
    let font = egui::TextStyle::Body.resolve(ui.style());
    let galley = ui.painter().layout_no_wrap(label.to_owned(), font, color);
    let (rect, resp) = ui.allocate_exact_size(vec2(look.width, h), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let fill = if look.selected {
            Some(t.selection)
        } else if resp.is_pointer_button_down_on() {
            Some(t.pressed)
        } else if resp.hovered() {
            Some(t.hover)
        } else {
            None
        };
        if let Some(fill) = fill {
            ui.painter().rect_filled(rect, t.round(6.0), fill);
        }
        let icon_rect = egui::Rect::from_min_size(pos2(rect.left() + pad, rect.center().y - size / 2.0), vec2(size, size));
        let icon_color = if look.selected {
            t.accent
        } else if look.dim {
            t.text_faint
        } else {
            t.text_dim
        };
        icon(ui.painter(), icon_rect, icon_color);
        ui.painter().galley(pos2(icon_rect.right() + pad * 0.8, rect.center().y - galley.size().y / 2.0), galley, color);
        let mut right = rect.right() - pad;
        if look.chevron {
            let c = egui::Rect::from_center_size(pos2(right - size / 2.0, rect.center().y), vec2(size, size));
            icons::chevron_right(ui.painter(), c, t.text_faint);
            right -= size + 4.0;
        }
        if let Some(hint) = look.hint {
            let g = ui.painter().layout_no_wrap(hint.to_owned(), egui::TextStyle::Small.resolve(ui.style()), t.text_faint);
            ui.painter().galley(pos2(right - g.size().x, rect.center().y - g.size().y / 2.0), g, t.text_faint);
        }
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Phones: a category in the list: its icon, name, what is in it, and a chevron.
fn category_row(ui: &mut egui::Ui, t: &Tokens, tab: Tab, label: &str, summary: &str, width: f32) -> egui::Response {
    let h = t.pad(56.0).max(form::control_h(t) * 1.8);
    let (rect, resp) = ui.allocate_exact_size(vec2(width, h), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        if resp.is_pointer_button_down_on() {
            ui.painter().rect_filled(rect, t.round(8.0), t.pressed);
        } else if resp.hovered() {
            ui.painter().rect_filled(rect, t.round(8.0), t.hover);
        }
        let pad = t.pad(12.0);
        let size = t.text_size(18.0);
        let icon_rect = egui::Rect::from_min_size(pos2(rect.left() + pad, rect.center().y - size / 2.0), vec2(size, size));
        (tab.icon())(ui.painter(), icon_rect, t.accent);
        let x = icon_rect.right() + pad;
        let text_w = (rect.right() - pad - size - x).max(40.0);
        let name = ui.painter().layout(label.to_owned(), egui::TextStyle::Body.resolve(ui.style()), t.text_strong, text_w);
        let sub = ui.painter().layout(summary.to_owned(), egui::TextStyle::Small.resolve(ui.style()), t.text_dim, text_w);
        let total = name.size().y + 2.0 + sub.size().y;
        let y = rect.center().y - total / 2.0;
        let name_h = name.size().y;
        ui.painter().galley(pos2(x, y), name, t.text_strong);
        ui.painter().galley(pos2(x, y + name_h + 2.0), sub, t.text_dim);
        let c = egui::Rect::from_center_size(pos2(rect.right() - pad - size / 2.0, rect.center().y), vec2(size, size));
        icons::chevron_right(ui.painter(), c, t.text_faint);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// "Advanced" and a caret: opens the fine-tuning below it on the page (no dialog), with
/// what's in it on one quiet line while folded.
fn advanced_disclosure(ui: &mut egui::Ui, t: &Tokens, open: bool) -> egui::Response {
    ui.add_space(t.pad(form::SECTION_GAP));
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 32.0), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let color = if resp.hovered() { t.text_strong } else { t.text_dim };
        let g = ui.painter().layout_no_wrap("Advanced".to_owned(), egui::FontId::new(t.text_size(14.0), crate::theme::bold_or_regular(ui.ctx())), color);
        let gy = rect.center().y - g.size().y / 2.0;
        let gw = g.size().x;
        ui.painter().galley(pos2(rect.left(), gy), g, color);
        let c = egui::Rect::from_center_size(pos2(rect.left() + gw + 14.0, rect.center().y), vec2(12.0, 12.0));
        icons::chevron(ui.painter(), c, open, color);
        if !open {
            let note = ui.painter().layout_no_wrap(
                "Colors, message style, what's shown, corners, theme files".to_owned(),
                egui::FontId::proportional(t.text_size(12.5)),
                t.text_faint,
            );
            let ny = rect.center().y - note.size().y / 2.0;
            ui.painter().galley(pos2(c.right() + 12.0, ny), note, t.text_faint);
        }
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{Accent, Density, ThemeKind, presets};
    use egui::Color32;

    #[test]
    fn picking_a_theme_fills_its_slot_and_shows_it() {
        let mut p = Prefs { theme: ThemeKind::System, accent: Accent::Named("teal".into()), ..Prefs::default() };
        // System is dark now: a dark pick stays in System mode.
        pick_theme(&mut p, &ThemeDef::builtin(&presets::CATPPUCCIN_MOCHA), true);
        assert_eq!((p.dark_theme.as_str(), p.theme, &p.accent), ("catppuccin-mocha", ThemeKind::System, &Accent::Theme));
        // A light pick while dark switches to Light so it's visible.
        pick_theme(&mut p, &ThemeDef::builtin(&presets::CATPPUCCIN_LATTE), true);
        assert_eq!((p.light_theme.as_str(), p.dark_theme.as_str(), p.theme), ("catppuccin-latte", "catppuccin-mocha", ThemeKind::Light));
    }

    #[test]
    fn theme_file_knobs_apply_on_pick() {
        let f = file::parse(r#"{"name":"Round","base":"nord","knobs":{"radius":12,"density":"Compact"}}"#, Origin::Imported(0)).unwrap();
        let mut p = Prefs::default();
        pick_theme(&mut p, &f.def, true);
        assert_eq!((p.knobs.radius, p.knobs.density, p.dark_theme.as_str()), (12.0, Density::Compact, "user:round"));
    }

    #[test]
    fn appearance_settings_render_wide_and_narrow() {
        for width in [1000.0, 360.0] {
            let ctx = egui::Context::default();
            let mut app = crate::ByobApp::new(Box::new(NullConn), "test");
            app.set_prefs(Prefs { overrides: [("byob-dark".to_owned(), [("text".to_owned(), "#ff0000".to_owned())].into())].into(), ..Prefs::default() });
            for _ in 0..2 {
                let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(width, 800.0))), ..Default::default() };
                let mut out = ctx.run_ui(input, |ui| {
                    app.apply_theme(ui.ctx());
                    let t = app.tokens;
                    for tab in [Tab::Appearance, Tab::Text, Tab::Colors] {
                        app.appearance.tab = tab;
                        app.settings_content(ui, &t, 16.0);
                    }
                });
                out.textures_delta.clear();
            }
            assert_eq!(app.tokens.text, Color32::from_rgb(255, 0, 0));
        }
    }

    struct NullConn;
    impl byob_protocol::Connection for NullConn {
        fn send(&mut self, _: byob_protocol::ClientMsg) {}
        fn try_recv(&mut self) -> Option<byob_protocol::ServerMsg> {
            None
        }
    }
}
