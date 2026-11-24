//! How each setting of the [registry](super::registry) is drawn: one exhaustive match on
//! [`Key`], and the appearance pieces it uses (the theme gallery, accent swatches, the color
//! roles table).

use byob_protocol::profiles::{ROLE_SUBAGENTS, ROLE_UTILITY, ROLE_VISION};
use byob_protocol::workspace::Field;
use egui::{Color32, RichText, Stroke, vec2};

use super::registry::Key;
use crate::ByobApp;
use crate::form;
use crate::icons;
use crate::theme::file::{self, ThemeDef};
use crate::theme::{self, Accent, Align, BubbleStyle, ChartPalette, Density, Knobs, MonoFont, Prefs, ROLES, ThemeKind, Tokens, UiFont, contrast, hex};

impl ByobApp {
    /// The backdrop is the model's (or a file's): on or off here, made by asking.
    fn backdrop_picker(&mut self, ui: &mut egui::Ui, t: &Tokens, c: &mut crate::cockpit::Cockpit) {
        if !self.stage.available() {
            ui.label(RichText::new("This window can't draw shaders").color(t.text_faint));
            return;
        }
        if c.backdrop.is_empty() {
            ui.label(RichText::new("None: ask byob for one (\"make it a starship's bridge\")").color(t.text_dim));
            return;
        }
        let lines = c.backdrop.lines().count();
        ui.label(RichText::new(format!("On ({lines} lines)")).color(t.text_dim));
        if ui.button("Remove").on_hover_text("Switch the backdrop off (byob can undo it: \"undo that\")").clicked() {
            c.set_backdrop("");
        }
    }
}

/// What the settings drawn in one frame share: the appearance being edited (applied once at
/// the end), the current theme and every theme, and a theme to remove.
pub(crate) struct Draw {
    pub prefs: Prefs,
    pub current: ThemeDef,
    pub themes: Vec<ThemeDef>,
    pub remove: Option<ThemeDef>,
}

impl ByobApp {
    pub(crate) fn begin_draw(&self) -> Draw {
        let prefs = self.prefs.clone();
        let current = file::resolve(prefs.theme_id(self.tokens.dark), self.tokens.dark, &self.user_themes);
        Draw { prefs, current, themes: file::all(&self.user_themes), remove: None }
    }

    /// Apply what the frame's settings changed.
    pub(crate) fn end_draw(&mut self, d: Draw) {
        if let Some(def) = d.remove {
            self.remove_theme(&def);
        } else if d.prefs != self.prefs {
            // Removing the backdrop says byob can undo it: then it can.
            if d.prefs.cockpit.backdrop != self.prefs.cockpit.backdrop {
                crate::look::remember(&mut self.look_history, self.prefs.clone());
            }
            self.set_prefs(d.prefs);
        }
    }

    /// The setting is there to draw (roles need saved models).
    pub(crate) fn setting_shown(&self, key: Key) -> bool {
        match key {
            Key::VisionRole | Key::SubagentRole | Key::UtilityRole => self.models.list.as_ref().is_some_and(|l| !l.items.is_empty()),
            // The engine keeps the layout: only with one that does.
            Key::ShownPreset | Key::ShownList => self.engine_has(byob_protocol::layout::CAP_LAYOUT),
            // The desktop app's own (a browser has none).
            Key::KeepRunning => self.desktop.is_some() && self.keep_running().is_some(),
            Key::StartAtLogin => self.desktop.as_ref().is_some_and(|d| d.start_at_login().is_some()),
            // The engine keeps it: only with one that does.
            Key::Phone => self.engine_has(byob_protocol::phone::CAP_PHONE),
            _ => true,
        }
    }

    /// Draw `key`'s control (a block: the whole list).
    pub(crate) fn draw_control(&mut self, ui: &mut egui::Ui, t: &Tokens, key: Key, d: &mut Draw) {
        let layer = |app: &mut ByobApp, ui: &mut egui::Ui, f: Field| {
            if let Some(m) = app.workspaces.defaults.control(ui, t, f) {
                app.conn.send(m);
            }
        };
        let k = &mut d.prefs.knobs;
        match key {
            Key::ModelList => self.model_list(ui, t),
            Key::VisionRole => self.models.role_control(ui, ROLE_VISION, "None"),
            Key::SubagentRole => self.models.role_control(ui, ROLE_SUBAGENTS, "Same as the chat"),
            Key::UtilityRole => self.models.role_control(ui, ROLE_UTILITY, "Same as the chat"),
            Key::Instructions => layer(self, ui, Field::Instructions),
            Key::Approvals => layer(self, ui, Field::Approvals),
            Key::ToolCalls => layer(self, ui, Field::ToolCalls),
            Key::Subagents => layer(self, ui, Field::Subagents),
            Key::Compaction => layer(self, ui, Field::Compaction),
            Key::ModelImages => layer(self, ui, Field::ModelImages),
            Key::ToolCalling => layer(self, ui, Field::ToolMode),
            Key::WorkspaceSettings => match self.sidebar.current_workspace().map(|w| (w.id.clone(), w.name.clone())) {
                Some((id, name)) => {
                    let label = format!("{}'s settings…", truncate(&name, 28));
                    if form::button(ui, t, &label, true).on_hover_text(format!("The settings of {name}, the project of the chat in view")).clicked() {
                        self.workspaces.open_settings(&id, false);
                    }
                }
                None => {
                    ui.label(RichText::new("The chat in view isn't in a project").color(t.text_faint));
                }
            },
            Key::Phone => {
                let msgs = self.phone.ui(ui, t);
                self.send_all(msgs);
            }
            Key::PluginList => self.plugins.panel(ui, t, &crate::plugins::Filter::All),
            Key::Mode => {
                // Each mode drawn in the colors it gives: the light and dark themes as picked.
                let light = theme::Tokens::build(&file::resolve(&d.prefs.light_theme, false, &self.user_themes), &d.prefs);
                let dark = theme::Tokens::build(&file::resolve(&d.prefs.dark_theme, true, &self.user_themes), &d.prefs);
                mode_previews(ui, t, &mut d.prefs.theme, &light, &dark);
            }
            Key::ThemePair => {
                // Import a theme file and copy this look as one, then the theme for this mode.
                if icons::icon_button(ui, t, icons::download, None, false).on_hover_text("Import a theme file (.json)").clicked() {
                    crate::upload::pick_files(ui.ctx(), self.appearance.import_tx.clone());
                }
                if icons::icon_button(ui, t, icons::copy, None, false).on_hover_text("Copy this look as a theme file").clicked() {
                    let text = file::export(&d.current, &d.prefs, &format!("{} (mine)", d.current.name));
                    ui.ctx().copy_text(text);
                }
                let dark = t.dark;
                let slot = if dark { &mut d.prefs.dark_theme } else { &mut d.prefs.light_theme };
                theme_menu(ui, t, slot, &d.themes, dark, &mut d.remove);
            }
            Key::Background => color_chip(ui, t, "bg", &d.current, &mut d.prefs, "app_bg", t.app_bg),
            Key::Foreground => color_chip(ui, t, "fg", &d.current, &mut d.prefs, "text", t.text),
            Key::Accent => accent_menu(ui, t, &d.current, &mut d.prefs.accent),
            Key::Radius => {
                form::slider(ui, t, &mut k.radius, Knobs::RADIUS_RANGE, 1.0, |v| format!("{v:.0} pt"));
            }
            Key::ThinkingSize => {
                let choices: Vec<(crate::count::SizeUnit, &str)> = crate::count::SizeUnit::ALL.iter().map(|u| (*u, u.label())).collect();
                form::select(ui, t, &mut d.prefs.thinking_size, &choices, None);
            }
            Key::ChartColors => {
                let choices: Vec<(ChartPalette, &str)> = ChartPalette::ALL.iter().map(|c| (*c, c.label())).collect();
                form::select(ui, t, &mut k.chart, &choices, None);
            }
            Key::MessageLayout => {
                let choices: Vec<(Align, &str)> = Align::ALL.iter().map(|a| (*a, a.label())).collect();
                form::select(ui, t, &mut k.align, &choices, None);
            }
            Key::UserBubble => {
                let choices: Vec<(BubbleStyle, &str)> = BubbleStyle::ALL.iter().map(|s| (*s, s.label())).collect();
                form::select(ui, t, &mut k.user_bubble, &choices, None);
            }
            Key::ReplyBubble => {
                let choices: Vec<(BubbleStyle, &str)> = BubbleStyle::ALL.iter().map(|s| (*s, s.label())).collect();
                form::select(ui, t, &mut k.assistant_bubble, &choices, None);
            }
            Key::BubblePadding => {
                form::slider(ui, t, &mut k.bubble_padding, Knobs::PADDING_RANGE, 1.0, |v| format!("{v:.0} pt"));
            }
            Key::LineSpacing => {
                form::slider(ui, t, &mut k.line_spacing, Knobs::LINE_SPACING_RANGE, 0.05, |v| format!("{v:.2}×"));
            }
            Key::ShownPreset => self.layout_presets(ui, t),
            Key::ShownList => self.layout_list(ui, t),
            Key::Controls => {
                for c in theme::Controls::ALL {
                    ui.selectable_value(&mut k.controls, c, c.label());
                }
            }
            Key::Backdrop => self.backdrop_picker(ui, t, &mut d.prefs.cockpit),
            Key::Glass => {
                let c = &mut d.prefs.cockpit;
                ui.add_enabled(!c.backdrop.is_empty(), egui::Slider::new(&mut c.glass, crate::cockpit::Cockpit::GLASS_RANGE).step_by(0.02).fixed_decimals(2));
            }
            Key::Frost => {
                let composited = self.stage.composited();
                let c = &mut d.prefs.cockpit;
                let r = ui.add_enabled(composited && !c.backdrop.is_empty(), egui::Slider::new(&mut c.frost, 0.0..=1.0).step_by(0.05).fixed_decimals(2));
                if !composited {
                    r.on_disabled_hover_text("Restart byob with a backdrop on to frost the glass");
                }
            }
            Key::PanelGap => {
                let c = &mut d.prefs.cockpit;
                ui.add_enabled(!c.backdrop.is_empty(), egui::Slider::new(&mut c.gap, crate::cockpit::Cockpit::GAP_RANGE).step_by(1.0).suffix(" pt"));
            }
            Key::Arrangement => {
                let a = &mut d.prefs.cockpit.arrangement;
                if ui.selectable_label(!a.free, "Docked").on_hover_text("byob's usual panels").clicked() {
                    a.free = false;
                }
                for (name, describe) in crate::cockpit::arrange::Arrangement::PRESETS {
                    let label = format!("{}{}", name[..1].to_uppercase(), &name[1..]);
                    if ui.button(label).on_hover_text(describe).clicked()
                        && let Some(p) = crate::cockpit::arrange::Arrangement::preset(name)
                    {
                        *a = p;
                    }
                }
                if a.free && ui.button("Arrange…").on_hover_text("Drag the parts where you want them").clicked() {
                    self.arranging.on = true;
                    self.close_settings();
                }
            }
            Key::ImportTheme => self.import_buttons(ui),
            Key::ExportTheme => self.export_controls(ui, t, d),
            Key::ResetLook => {
                if form::button(ui, t, "Reset", true).on_hover_text("Accent, corners, bubbles and chart colors back to the theme's").clicked() {
                    reset_advanced(&mut d.prefs, &d.current.id);
                }
            }
            Key::ResetAppearance => {
                // The one destructive action on the page: in the error color, outlined.
                if form::danger_button(ui, t, "Reset everything")
                    .on_hover_text("Themes, accent, text, layout and color overrides (keeps imported themes and saved scenes)")
                    .clicked()
                {
                    d.prefs = crate::look::reset(&mut d.prefs);
                }
            }
            Key::Zoom => {
                if let Some(z) = crate::header::zoom_controls(ui, d.prefs.zoom) {
                    d.prefs.zoom = z;
                }
            }
            Key::TextSize => scale_buttons(ui, &mut k.font_scale),
            Key::CodeSize => scale_buttons(ui, &mut k.code_scale),
            Key::Font => font_menu(ui, t, &mut k.ui_font),
            Key::CodeFont => {
                let choices: Vec<(MonoFont, &str)> = MonoFont::ALL.iter().map(|f| (*f, f.label())).collect();
                form::select(ui, t, &mut k.mono_font, &choices, None);
            }
            Key::Density => {
                let choices: Vec<(Density, &str)> = Density::ALL.iter().map(|d| (*d, d.label())).collect();
                form::select(ui, t, &mut k.density, &choices, None);
            }
            Key::ColumnWidth => {
                for (w, name) in Knobs::WIDTHS {
                    let tip = if w > 0.0 { format!("At most {w} pt wide") } else { "The whole window".into() };
                    if ui.selectable_label((k.max_width - w).abs() < 1.0, name).on_hover_text(tip).clicked() {
                        k.max_width = w;
                    }
                }
            }
            Key::ChatListWidth => {
                form::slider(ui, t, &mut k.sidebar_width, Knobs::SIDEBAR_RANGE, 10.0, |v| format!("{v:.0} pt"));
            }
            Key::ArtifactPane => {
                let mut pct = k.pane_share * 100.0;
                if form::slider(ui, t, &mut pct, 30.0..=75.0, 1.0, |v| format!("{v:.0}%")) {
                    k.pane_share = pct / 100.0;
                }
            }
            Key::HighContrast => {
                form::switch(ui, t, &mut k.high_contrast);
            }
            Key::ReduceMotion => {
                form::switch(ui, t, &mut k.reduce_motion);
            }
            Key::ResetText => {
                if form::button(ui, t, "Reset", true).on_hover_text("Zoom, sizes, fonts, spacing and layout back to defaults").clicked() {
                    let dk = Knobs::default();
                    d.prefs.zoom = 1.0;
                    (k.font_scale, k.code_scale, k.ui_font, k.mono_font, k.density) = (dk.font_scale, dk.code_scale, dk.ui_font, dk.mono_font, dk.density);
                    (k.align, k.max_width, k.sidebar_width, k.pane_share) = (dk.align, dk.max_width, dk.sidebar_width, dk.pane_share);
                    (k.high_contrast, k.reduce_motion) = (dk.high_contrast, dk.reduce_motion);
                }
            }
            Key::ColorRoles => advanced(ui, t, &d.current, &mut d.prefs, &mut self.appearance.filter),
            Key::KeepRunning => self.keep_running_control(ui, t),
            Key::StartAtLogin => self.start_at_login_control(ui, t),
        }
    }

    fn keep_running_control(&mut self, ui: &mut egui::Ui, t: &Tokens) {
        use crate::desktop::KeepRunning;
        let Some(mut value) = self.keep_running() else { return };
        let choices: Vec<(KeepRunning, &str)> = KeepRunning::ALL.iter().map(|k| (*k, k.label())).collect();
        if form::select(ui, t, &mut value, &choices, None) {
            self.set_keep_running(value);
        }
    }

    fn start_at_login_control(&mut self, ui: &mut egui::Ui, t: &Tokens) {
        let Some(desktop) = self.desktop.as_mut() else { return };
        let Some(mut on) = desktop.start_at_login() else { return };
        if form::switch(ui, t, &mut on).changed()
            && let Err(e) = desktop.set_start_at_login(on)
        {
            self.notices.push(byob_protocol::NoticeLevel::Error, format!("Couldn't change Start byob when you log in: {e}"));
        }
    }

    /// Saved models (or, on engines without them, the connection form).
    fn model_list(&mut self, ui: &mut egui::Ui, t: &Tokens) {
        if self.models.available() {
            self.models.show(ui, t, self.state.as_ref());
            return;
        }
        match &self.state {
            Some(state) => self.connect.form(ui, t, state, "settings"),
            None => {
                ui.label(RichText::new("connecting…").color(t.text_dim));
            }
        }
    }

    fn import_buttons(&mut self, ui: &mut egui::Ui) {
        if form::button(ui, &self.tokens, "Import…", true).on_hover_text("Add a theme from a .json theme file").clicked() {
            crate::upload::pick_files(ui.ctx(), self.appearance.import_tx.clone());
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(dir) = &self.themes_dir
            && icons::icon_button(ui, &self.tokens, icons::refresh, None, false)
                .on_hover_text(format!("Read the theme files in {} again", dir.display()))
                .clicked()
        {
            self.reload_themes();
        }
    }

    /// "Save…": a small panel with the look's name, then Save to my themes (where there's a
    /// themes folder) or Export as file. One button on the row, the choices when asked.
    fn export_controls(&mut self, ui: &mut egui::Ui, t: &Tokens, d: &mut Draw) {
        let current = d.current.clone();
        if self.appearance.export_name.is_empty() {
            self.appearance.export_name = if current.is_builtin() { format!("{} (mine)", current.name) } else { current.name.clone() };
        }
        let b = form::button(ui, t, "Save…", true).on_hover_text("Save this look as a theme");
        #[cfg_attr(target_arch = "wasm32", allow(unused_mut))]
        let mut save_to_themes = false;
        crate::menu::below(&b, t, 280.0).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            ui.label(egui::RichText::new("Name").size(t.text_size(12.5)).color(t.text_dim));
            let name_id = egui::Id::unique("export-theme-name");
            crate::workspace::form_field(ui, t, name_id, |frame| {
                egui::TextEdit::singleline(&mut self.appearance.export_name)
                    .id(name_id)
                    .hint_text("Theme name")
                    .char_limit(80)
                    .desired_width(240.0)
                    .frame(frame)
            });
            let named = !self.appearance.export_name.trim().is_empty();
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                #[cfg(not(target_arch = "wasm32"))]
                if self.themes_dir.is_some()
                    && crate::workspace::dialog_button(ui, t, "Save to my themes", true, named, 32.0)
                        .on_hover_text("List it with your themes, ready to pick")
                        .clicked()
                {
                    save_to_themes = true;
                    ui.close();
                }
                if form::button(ui, t, "Export as file…", named).on_hover_text("Save it as a .json file to share").clicked() {
                    let text = file::export(&current, &d.prefs, &self.appearance.export_name);
                    let filename = format!("{}.json", file::slug(&self.appearance.export_name));
                    self.appearance.exports.save(ui.ctx(), &filename, "application/json", text.into_bytes());
                    ui.close();
                }
            });
        });
        let text = file::export(&current, &d.prefs, &self.appearance.export_name);
        #[cfg(not(target_arch = "wasm32"))]
        if save_to_themes && self.themes_dir.is_some() {
            // Saving picks the saved theme: what this frame changed goes in first.
            if d.prefs != self.prefs {
                self.set_prefs(d.prefs.clone());
            }
            let mut p = self.prefs.clone();
            let def = file::parse(&text, file::Origin::Imported(0)).map(|f| f.def);
            match def.map_err(|e| e.to_string()).and_then(|df| self.store_theme(&mut p, &df, &text).map(|w| (df, w))) {
                Ok((df, w)) => {
                    self.appearance.status.push((true, format!("Saved \"{}\" {w}", df.name)));
                    self.reload_themes();
                    if let Some(saved) = self.user_themes.iter().find(|u| u.id == df.id).cloned() {
                        // The saved theme already carries the overrides.
                        p.overrides.remove(&current.id);
                        pick_theme(&mut p, &saved, t.dark);
                        self.set_prefs(p);
                    }
                }
                Err(e) => self.appearance.status.push((false, e)),
            }
            *d = self.begin_draw();
        }
        #[cfg(target_arch = "wasm32")]
        let _ = (save_to_themes, text);
    }

    /// Below Theme files: what the last imports and exports said, and theme file warnings.
    pub(crate) fn theme_file_status(&mut self, ui: &mut egui::Ui, t: &Tokens) {
        for (ok, text) in &self.appearance.status {
            ui.label(RichText::new(text).small().color(if *ok { t.text_dim } else { t.warning }));
        }
        if !self.theme_warnings.is_empty() {
            egui::CollapsingHeader::new(RichText::new(format!("{} theme file warnings", self.theme_warnings.len())).small().color(t.warning))
                .id_salt("theme-warnings")
                .show(ui, |ui| {
                    for w in &self.theme_warnings {
                        ui.label(RichText::new(w).small().color(t.text_dim));
                    }
                });
        }
    }
}

/// At most `n` characters of `s`, with "…" when cut.
fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_owned() } else { format!("{}…", s.chars().take(n.saturating_sub(1)).collect::<String>()) }
}

/// Picking a theme in the gallery: it becomes the dark or light theme (by its kind), the
/// window switches to it unless following the system, the accent goes back to the
/// theme's own, and any knobs the theme file sets apply.
pub(crate) fn pick_theme(prefs: &mut Prefs, def: &ThemeDef, dark_now: bool) {
    if def.dark {
        prefs.dark_theme.clone_from(&def.id);
    } else {
        prefs.light_theme.clone_from(&def.id);
    }
    if prefs.theme != ThemeKind::System || def.dark != dark_now {
        // Following the system would hide the pick until the OS flips; show it now.
        prefs.theme = if def.dark { ThemeKind::Dark } else { ThemeKind::Light };
    }
    prefs.accent = Accent::Theme;
    if let Some(k) = &def.knobs {
        file::apply_knobs(k, &mut prefs.knobs, &mut Vec::new());
    }
}

fn scale_buttons(ui: &mut egui::Ui, value: &mut f32) {
    for (scale, name) in Knobs::FONT_SCALES {
        if ui.selectable_label((*value - scale).abs() < 0.01, name).clicked() {
            *value = scale;
        }
    }
}

/// The modes as small pictures of byob, each in its own colors, with its name under it.
/// System is split: light on the left, dark on the right. The chosen one has an ink ring.
fn mode_previews(ui: &mut egui::Ui, t: &Tokens, value: &mut ThemeKind, light: &Tokens, dark: &Tokens) {
    let size = vec2(76.0, 50.0);
    let label_font = egui::FontId::proportional(12.0);
    let label_h = 18.0;
    let radius = t.round(form::CONTROL_R + 2.0);
    ui.spacing_mut().item_spacing.x = 12.0;
    for (kind, name) in [(ThemeKind::System, "System"), (ThemeKind::Light, "Light"), (ThemeKind::Dark, "Dark")] {
        let (rect, resp) = ui.allocate_exact_size(vec2(size.x, size.y + label_h), egui::Sense::click());
        let resp = resp.on_hover_text(match kind {
            ThemeKind::System => "Follow this computer: light by day, dark when it is",
            ThemeKind::Light => "Always light",
            ThemeKind::Dark => "Always dark",
        });
        let on = *value == kind;
        resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::RadioButton, true, on, name));
        if resp.clicked() {
            *value = kind;
        }
        if !ui.is_rect_visible(rect) {
            continue;
        }
        let tile = egui::Rect::from_min_size(rect.min, size);
        let p = ui.painter();
        match kind {
            ThemeKind::System => {
                let mid = tile.center().x;
                mini_window(&p.with_clip_rect(egui::Rect::from_min_max(tile.min, egui::pos2(mid, tile.bottom()))), tile, radius, light);
                mini_window(&p.with_clip_rect(egui::Rect::from_min_max(egui::pos2(mid, tile.top()), tile.max)), tile, radius, dark);
            }
            ThemeKind::Light => mini_window(p, tile, radius, light),
            ThemeKind::Dark => mini_window(p, tile, radius, dark),
        }
        // A soft hairline on every tile; the chosen one gets an ink ring just outside it.
        let edge = if resp.hovered() && !on { t.field_border } else { t.separator };
        p.rect_stroke(tile, radius, Stroke::new(1.0, edge), egui::StrokeKind::Inside);
        if on || resp.has_focus() {
            p.rect_stroke(tile.expand(2.5), radius + 2.5, Stroke::new(1.5, t.text_strong), egui::StrokeKind::Middle);
        }
        let color = if on { t.text_strong } else { t.text_dim };
        p.text(egui::pos2(tile.center().x, tile.bottom() + label_h * 0.5 + 3.0), egui::Align2::CENTER_CENTER, name, label_font.clone(), color);
    }
}

/// byob in miniature, in `tk`'s colors: a sidebar with a few chats, a reply, your message
/// in its bubble, and the message box with its send button in the accent.
fn mini_window(p: &egui::Painter, r: egui::Rect, radius: f32, tk: &Tokens) {
    use egui::{Rect, pos2};
    p.rect_filled(r, radius, tk.app_bg);
    // The sidebar.
    let side = Rect::from_min_max(r.min, pos2(r.left() + r.width() * 0.3, r.bottom()));
    p.rect_filled(side, egui::CornerRadius { nw: radius as u8, sw: radius as u8, ne: 0, se: 0 }, tk.panel_bg);
    p.vline(side.right(), side.y_range(), Stroke::new(1.0, tk.separator));
    let line = |x0: f32, x1: f32, y: f32, c: Color32| p.rect_filled(Rect::from_min_max(pos2(x0, y - 1.25), pos2(x1, y + 1.25)), 1.25, c);
    let faint = tk.text_faint.gamma_multiply(0.7);
    p.circle_filled(pos2(side.left() + 6.0, side.top() + 7.0), 2.0, faint);
    for (i, w) in [0.75_f32, 0.55, 0.65].iter().enumerate() {
        #[allow(clippy::cast_precision_loss)] // three rows
        let y = side.top() + 16.0 + i as f32 * 7.0;
        line(side.left() + 4.0, side.left() + 4.0 + (side.width() - 8.0) * w, y, faint);
    }
    // The chat: a reply in two lines, then your message on the right in its bubble.
    let main = Rect::from_min_max(pos2(side.right() + 6.0, r.top() + 7.0), pos2(r.right() - 6.0, r.bottom() - 6.0));
    let text = tk.text_dim.gamma_multiply(0.75);
    line(main.left(), main.left() + main.width() * 0.8, main.top() + 2.0, text);
    line(main.left(), main.left() + main.width() * 0.55, main.top() + 8.0, text);
    let bubble = Rect::from_min_max(pos2(main.right() - main.width() * 0.5, main.top() + 13.0), pos2(main.right(), main.top() + 21.0));
    p.rect_filled(bubble, 3.0, tk.user_bubble);
    // The message box with its send button.
    let input = Rect::from_min_max(pos2(main.left(), main.bottom() - 10.0), main.max);
    p.rect_filled(input, 3.5, tk.field_bg);
    p.rect_stroke(input, 3.5, Stroke::new(1.0, tk.field_border.gamma_multiply(0.8)), egui::StrokeKind::Inside);
    p.circle_filled(pos2(input.right() - 5.0, input.center().y), 2.5, tk.accent);
}

/// A theme's sample: "Aa" in its accent on its background, in a circle.
fn theme_mark(def: &ThemeDef) -> impl Fn(&egui::Painter, egui::Rect) + '_ {
    move |p: &egui::Painter, r: egui::Rect| {
        let bg = theme::rgb(def.base.bg);
        let accent = def.accent.unwrap_or_else(|| theme::rgb(def.base.default_accent()));
        p.circle_filled(r.center(), r.width() / 2.0, bg);
        p.circle_stroke(r.center(), r.width() / 2.0, Stroke::new(1.0, Color32::from_gray(128).gamma_multiply(0.4)));
        p.text(r.center(), egui::Align2::CENTER_CENTER, "Aa", egui::FontId::proportional(r.height() * 0.5), accent);
    }
}

/// The theme for this mode: byob's own first, the others under More themes (a submenu).
fn theme_menu(ui: &mut egui::Ui, t: &Tokens, value: &mut String, themes: &[ThemeDef], dark: bool, remove: &mut Option<ThemeDef>) {
    let Some(current) = themes.iter().find(|d| d.id == *value).or_else(|| themes.iter().find(|d| d.dark == dark)) else { return };
    let mark = theme_mark(current);
    let b = form::pill(ui, t, 200.0, Some(&mark), &current.name);
    // One theme; a theme you added can be removed (right-click).
    let row = |ui: &mut egui::Ui, def: &ThemeDef, value: &mut String, remove: &mut Option<ThemeDef>| {
        let mark = theme_mark(def);
        let on = def.id == *value;
        let r = crate::menu::item_marked(ui, t, &|p, rect, _| mark(p, rect.expand(3.0)), &def.name, on.then_some("✓"));
        let r = if def.is_builtin() { r } else { r.on_hover_text("Right-click to remove it") };
        if !def.is_builtin() && r.secondary_clicked() {
            *remove = Some(def.clone());
            ui.close();
        }
        if r.clicked() {
            value.clone_from(&def.id);
            ui.close();
        }
    };
    crate::menu::below(&b, t, 240.0).show(|ui| {
        crate::menu::tighten(ui);
        let (own, more): (Vec<&ThemeDef>, Vec<&ThemeDef>) = themes.iter().filter(|d| d.dark == dark).partition(|d| d.base.credit == "byob" || !d.is_builtin());
        for def in own {
            row(ui, def, value, remove);
        }
        if !more.is_empty() {
            let r = crate::chatbar::value_row(ui, t, "More themes", "");
            egui::containers::menu::SubMenu::new().config(crate::chatbar::sub_menu_config(t)).show(ui, &r, |ui| {
                ui.set_width(220.0);
                egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                    for def in more {
                        row(ui, def, value, remove);
                    }
                });
            });
        }
    });
}

/// The accent: Default (the theme's own: byob's ink), the theme's colors, or Custom (any
/// color, edited as hex beside it).
fn accent_menu(ui: &mut egui::Ui, t: &Tokens, current: &ThemeDef, accent: &mut Accent) {
    let p = current.base;
    let own = current.accent.unwrap_or_else(|| theme::rgb(p.default_accent()));
    let dot = |c: Color32| {
        move |pt: &egui::Painter, r: egui::Rect| {
            pt.circle_filled(r.center(), 7.0, c);
            pt.circle_stroke(r.center(), 7.0, Stroke::new(1.0, Color32::from_gray(128).gamma_multiply(0.45)));
        }
    };
    let label = match &*accent {
        Accent::Theme => "Default".to_owned(),
        Accent::Custom(_) => "Custom".to_owned(),
        other => capitalize(&other.label()),
    };
    let mark = dot(t.accent);
    let b = form::pill(ui, t, 130.0, Some(&mark), &label);
    crate::menu::below(&b, t, 200.0).show(|ui| {
        crate::menu::tighten(ui);
        let d = dot(own);
        if crate::menu::item_marked(ui, t, &|pt, r, _| d(pt, r), "Default", (*accent == Accent::Theme).then_some("✓")).clicked() {
            *accent = Accent::Theme;
            ui.close();
        }
        // The theme's own colors (its first is the default, already listed).
        for (name, c) in p.accents.iter().skip(1) {
            let on = matches!(accent, Accent::Named(n) if n.eq_ignore_ascii_case(name));
            let d = dot(theme::rgb(*c));
            if crate::menu::item_marked(ui, t, &|pt, r, _| d(pt, r), &capitalize(name), on.then_some("✓")).clicked() {
                *accent = Accent::Named((*name).to_owned());
                ui.close();
            }
        }
        crate::menu::separator(ui, t);
        let on = matches!(accent, Accent::Custom(_));
        if crate::menu::item(ui, t, None, "Custom", on.then_some("✓")).clicked() {
            *accent = Accent::Custom(hex(t.accent));
            ui.close();
        }
    });
    if let Accent::Custom(h) = accent {
        let mut c = theme::parse_color(h).unwrap_or(t.accent);
        if hex_field(ui, t, "accent-hex", &mut c) {
            *accent = Accent::Custom(hex(c));
        }
    }
}

/// Appearance's "Reset advanced": accent, corners, message style and chart colors back to
/// the defaults, and Advanced's own colors (background, text) back to theme `theme`'s.
fn reset_advanced(prefs: &mut Prefs, theme: &str) {
    let (k, dk) = (&mut prefs.knobs, Knobs::default());
    prefs.accent = Accent::Theme;
    (k.radius, k.user_bubble, k.assistant_bubble, k.chart) = (dk.radius, dk.user_bubble, dk.assistant_bubble, dk.chart);
    (k.bubble_padding, k.line_spacing) = (dk.bubble_padding, dk.line_spacing);
    if let Some(o) = prefs.overrides.get_mut(theme) {
        for role in ["app_bg", "text"] {
            o.remove(role);
        }
        if o.is_empty() {
            prefs.overrides.remove(theme);
        }
    }
}

/// A color of the theme (its background, its text) as a chip: a dot and its hex, editable.
/// A change is an override of this theme only (the Colors tab lists every one).
fn color_chip(ui: &mut egui::Ui, t: &Tokens, id: &str, current: &ThemeDef, prefs: &mut Prefs, role: &str, now: Color32) {
    let mut c = now;
    if hex_field(ui, t, id, &mut c) {
        prefs.overrides.entry(current.id.clone()).or_default().insert(role.to_owned(), hex(c));
    }
}

/// A hex color in a pill: its dot and `#RRGGBB`, typed over. True when a valid new color
/// was entered (Enter, or leaving the field).
fn hex_field(ui: &mut egui::Ui, t: &Tokens, id: &str, color: &mut Color32) -> bool {
    let key = egui::Id::unique(("hex-field", id));
    let mut text = ui.data(|d| d.get_temp::<String>(key)).unwrap_or_else(|| hex(*color).to_uppercase());
    let (rect, _) = ui.allocate_exact_size(vec2(118.0, 32.0), egui::Sense::hover());
    ui.painter().rect(rect, t.round(form::CONTROL_R), Color32::TRANSPARENT, Stroke::new(1.0, t.field_border), egui::StrokeKind::Inside);
    ui.painter().circle_filled(egui::pos2(rect.left() + 16.0, rect.center().y), 7.0, *color);
    ui.painter().circle_stroke(egui::pos2(rect.left() + 16.0, rect.center().y), 7.0, Stroke::new(1.0, Color32::from_gray(128).gamma_multiply(0.45)));
    let field = egui::Rect::from_min_max(egui::pos2(rect.left() + 30.0, rect.top()), egui::pos2(rect.right() - 8.0, rect.bottom()));
    let r = ui.put(
        field,
        egui::TextEdit::singleline(&mut text).frame(egui::Frame::NONE).font(egui::TextStyle::Monospace).char_limit(7).align(egui::Align2::LEFT_CENTER),
    );
    let mut changed = false;
    if r.has_focus() || r.changed() {
        ui.data_mut(|d| d.insert_temp(key, text.clone()));
    }
    if r.lost_focus() {
        if let Some(c) = theme::parse_color(text.trim()) {
            changed = c != *color;
            *color = c;
        }
        ui.data_mut(|d| d.remove_temp::<String>(key));
    }
    changed
}

/// The interface font, as a menu.
fn font_menu(ui: &mut egui::Ui, t: &Tokens, font: &mut UiFont) {
    let b = form::pill(ui, t, 120.0, None, font.label());
    crate::menu::below(&b, t, 180.0).show(|ui| {
        crate::menu::tighten(ui);
        for f in UiFont::ALL {
            if crate::menu::item(ui, t, None, f.label(), (*font == f).then_some("✓")).clicked() {
                *font = f;
                ui.close();
            }
        }
    });
}

/// "violet" → "Violet".
fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

/// The background a text-like role is read on, for its contrast hint.
fn read_on(role: &str) -> Option<&'static str> {
    Some(match role {
        "text" | "text_strong" | "text_dim" | "text_faint" | "link" | "accent" | "danger" | "warning" | "success" => "assistant_bubble",
        "code_text" | "syntax_keyword" | "syntax_string" | "syntax_number" | "syntax_comment" | "syntax_function" | "syntax_type" => "code_bg",
        "inline_code_text" => "inline_code_bg",
        "on_accent" => "accent",
        _ => return None,
    })
}

/// Every color role with a picker, in columns; changes are overrides for the current theme only.
fn advanced(ui: &mut egui::Ui, t: &Tokens, current: &ThemeDef, prefs: &mut Prefs, filter: &mut String) {
    ui.horizontal_wrapped(|ui| {
        ui.add(form::field(filter, t).hint_text("Filter colors").desired_width(200.0));
        let has = prefs.overrides.get(&current.id).is_some_and(|o| !o.is_empty());
        if form::button(ui, t, "Reset all colors", has).clicked() {
            prefs.overrides.remove(&current.id);
        }
    });
    let needle = filter.trim().to_lowercase();
    let shown = |r: &&theme::Role| {
        needle.is_empty() || r.label.to_lowercase().contains(&needle) || r.name.contains(&needle) || r.group.to_lowercase().contains(&needle)
    };
    // Groups of roles, in two columns on a wide page (each group where there is less so far).
    let mut groups: Vec<(&str, Vec<&theme::Role>)> = Vec::new();
    for role in ROLES.iter().filter(shown) {
        match groups.last_mut() {
            Some((g, list)) if *g == role.group => list.push(role),
            _ => groups.push((role.group, vec![role])),
        }
    }
    let two = ui.available_width() >= 760.0;
    let (mut left, mut right) = (Vec::new(), Vec::new());
    let (mut left_n, mut right_n) = (0, 0);
    for g in groups {
        let n = g.1.len() + 2;
        if two && right_n < left_n {
            right_n += n;
            right.push(g);
        } else {
            left_n += n;
            left.push(g);
        }
    }
    let split = if two { vec![left, right] } else { vec![left] };
    let gap = t.pad(48.0);
    ui.horizontal_top(|ui| {
        let w = if two { (ui.available_width() - gap) / 2.0 } else { ui.available_width() }.max(200.0);
        for (i, col) in split.into_iter().enumerate() {
            if i > 0 {
                ui.add_space(gap - ui.spacing().item_spacing.x);
            }
            ui.vertical(|ui| {
                ui.set_width(w);
                for (group, roles) in col {
                    ui.add_space(t.pad(form::HEAD_GAP + 4.0));
                    ui.label(RichText::new(group).strong().color(t.text_strong));
                    ui.add_space(t.pad(2.0));
                    for role in roles {
                        role_row(ui, t, role, current, prefs, w);
                    }
                }
            });
        }
    });
}

/// One color role: its name, then at the right its contrast, hex and picker (and Reset when
/// changed on this device).
fn role_row(ui: &mut egui::Ui, t: &Tokens, role: &theme::Role, current: &ThemeDef, prefs: &mut Prefs, w: f32) {
    let h = form::control_h(t);
    let cell = |ui: &mut egui::Ui, w: f32, add: &mut dyn FnMut(&mut egui::Ui)| {
        ui.allocate_ui_with_layout(vec2(w, h), egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.set_min_size(vec2(w, h));
            add(ui);
        });
    };
    let Some(mut c) = t.get(role.name) else { return };
    let overridden = prefs.overrides.get(&current.id).is_some_and(|o| o.contains_key(role.name));
    ui.allocate_ui_with_layout(vec2(w, h), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_min_size(vec2(w, h));
        let name = if overridden { RichText::new(format!("{} •", role.label)).color(t.text_strong) } else { RichText::new(role.label).color(t.text) };
        ui.label(name).on_hover_text(if overridden { "Changed on this device" } else { role.name });
        // Right to left: the picker at the right edge, then its hex, contrast and Reset.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let alpha = if c.a() < 255 { egui::color_picker::Alpha::OnlyBlend } else { egui::color_picker::Alpha::Opaque };
            if egui::color_picker::color_edit_button_srgba(ui, &mut c, alpha).changed() {
                if role.name == "accent" {
                    prefs.accent = Accent::Custom(hex(c));
                } else {
                    prefs.overrides.entry(current.id.clone()).or_default().insert(role.name.into(), hex(c));
                }
            }
            cell(ui, t.pad(80.0), &mut |ui| {
                ui.label(RichText::new(hex(c)).small().monospace().color(t.text_faint));
            });
            cell(ui, t.pad(48.0), &mut |ui| {
                if let Some(bg) = read_on(role.name).and_then(|b| t.get(b)) {
                    let ratio = contrast(c, bg);
                    let color = if ratio >= 4.5 { t.text_dim } else { t.danger };
                    ui.label(RichText::new(format!("{ratio:.1}:1")).small().color(color))
                        .on_hover_text("Contrast against what it's drawn on (WCAG: 4.5:1 text, 7:1 enhanced)");
                }
            });
            if overridden
                && ui.small_button("Reset").clicked()
                && let Some(o) = prefs.overrides.get_mut(&current.id)
            {
                o.remove(role.name);
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reset advanced puts Advanced's Background and Text color back to the theme's (other
    /// themes' colors and other roles stay: the Colors page has its own reset).
    #[test]
    fn reset_advanced_clears_the_background_and_text_colors() {
        let mut prefs = Prefs::default();
        let set = |p: &mut Prefs, theme: &str, role: &str| {
            p.overrides.entry(theme.to_owned()).or_default().insert(role.to_owned(), "#123456".to_owned());
        };
        set(&mut prefs, "byob-dark", "app_bg");
        set(&mut prefs, "byob-dark", "text");
        set(&mut prefs, "byob-dark", "accent");
        set(&mut prefs, "nord", "app_bg");
        prefs.knobs.radius = 2.0;
        reset_advanced(&mut prefs, "byob-dark");
        let dark = prefs.overrides.get("byob-dark");
        assert!(dark.is_some_and(|o| !o.contains_key("app_bg") && !o.contains_key("text") && o.contains_key("accent")), "{:?}", prefs.overrides);
        assert!(prefs.overrides.get("nord").is_some_and(|o| o.contains_key("app_bg")), "another theme's colors stay");
        assert!((prefs.knobs.radius - Knobs::default().radius).abs() < f32::EPSILON);
        prefs.overrides.clear();
        set(&mut prefs, "byob-dark", "app_bg");
        reset_advanced(&mut prefs, "byob-dark");
        assert!(prefs.overrides.is_empty(), "no empty entry left: {:?}", prefs.overrides);
    }

    /// Each picture is its mode: clicking Light then Dark picks them; System stays split.
    #[test]
    fn clicking_a_mode_picture_picks_that_mode() {
        let ctx = egui::Context::default();
        let (light, dark) = (Tokens::light(), Tokens::dark());
        let mut mode = ThemeKind::System;
        let frame = |mode: &mut ThemeKind, events: Vec<egui::Event>| {
            let mut out = ctx.run_ui(egui::RawInput { events, ..Default::default() }, |ui| {
                ui.horizontal(|ui| mode_previews(ui, &light, mode, &light, &dark));
            });
            out.textures_delta.clear();
        };
        let click = |x: f32| {
            let at = egui::pos2(x, 25.0);
            vec![
                egui::Event::PointerMoved(at),
                egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE },
                egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE },
            ]
        };
        frame(&mut mode, vec![]);
        // Tiles are 76 wide, 12 apart: System, then Light at 88, then Dark at 176.
        frame(&mut mode, click(88.0 + 38.0));
        assert_eq!(mode, ThemeKind::Light);
        frame(&mut mode, click(176.0 + 38.0));
        assert_eq!(mode, ThemeKind::Dark);
        frame(&mut mode, click(38.0));
        assert_eq!(mode, ThemeKind::System);
    }
}
