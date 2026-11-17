//! How each setting of the [registry](super::registry) is drawn: one exhaustive match on
//! [`Key`], and the appearance pieces it uses (the theme gallery, accent swatches, the colour
//! roles table).

use byob_protocol::profiles::{ROLE_SUBAGENTS, ROLE_UTILITY, ROLE_VISION};
use byob_protocol::workspace::Field;
use egui::{Color32, RichText, Stroke, vec2};

use super::registry::Key;
use crate::ByobApp;
use crate::form;
use crate::theme::file::{self, ThemeDef};
use crate::theme::{self, Accent, Align, BubbleStyle, ChartPalette, Density, Knobs, MonoFont, Prefs, ROLES, ThemeKind, Tokens, UiFont, contrast, hex};

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
            self.set_prefs(d.prefs);
        }
    }

    /// The setting is there to draw (roles need saved models).
    pub(crate) fn setting_shown(&self, key: Key) -> bool {
        match key {
            Key::VisionRole | Key::SubagentRole | Key::UtilityRole => self.models.list.as_ref().is_some_and(|l| !l.items.is_empty()),
            // The engine keeps the layout: only with one that does.
            Key::ShownPreset | Key::ShownList => self.engine_has(byob_protocol::layout::CAP_LAYOUT),
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
                    if ui.button(label).on_hover_text(format!("The settings of {name}, the workspace of the chat in view")).clicked() {
                        self.workspaces.open_settings(&id, false);
                    }
                }
                None => {
                    ui.label(RichText::new("The chat in view isn't in a workspace").color(t.text_faint));
                }
            },
            Key::PluginList => self.plugins.panel(ui, t, &crate::plugins::Filter::All),
            Key::Mode => {
                for kind in ThemeKind::ALL {
                    ui.selectable_value(&mut d.prefs.theme, kind, kind.label());
                }
            }
            Key::ThemePair => {
                theme_combo(ui, "light-theme", "Light", &mut d.prefs.light_theme, &d.themes, false);
                ui.add_space(t.pad(4.0));
                theme_combo(ui, "dark-theme", "Dark", &mut d.prefs.dark_theme, &d.themes, true);
            }
            Key::Gallery => {
                let dark = t.dark;
                gallery(ui, t, &d.themes, &mut d.prefs, dark, &mut d.remove);
            }
            Key::Accent => accent_picker(ui, t, &d.current, &mut d.prefs.accent),
            Key::Radius => {
                ui.add(egui::Slider::new(&mut k.radius, Knobs::RADIUS_RANGE).step_by(1.0).suffix(" pt"));
            }
            Key::ChartColours => {
                for c in ChartPalette::ALL {
                    ui.selectable_value(&mut k.chart, c, c.label());
                }
            }
            Key::MessageLayout => {
                for a in Align::ALL {
                    ui.selectable_value(&mut k.align, a, a.label());
                }
            }
            Key::UserBubble => {
                for s in BubbleStyle::ALL {
                    ui.selectable_value(&mut k.user_bubble, s, s.label());
                }
            }
            Key::ReplyBubble => {
                for s in BubbleStyle::ALL {
                    ui.selectable_value(&mut k.assistant_bubble, s, s.label());
                }
            }
            Key::BubblePadding => {
                ui.add(egui::Slider::new(&mut k.bubble_padding, Knobs::PADDING_RANGE).step_by(1.0).suffix(" pt"));
            }
            Key::LineSpacing => {
                ui.add(egui::Slider::new(&mut k.line_spacing, Knobs::LINE_SPACING_RANGE).step_by(0.05).fixed_decimals(2));
            }
            Key::ShownPreset => self.layout_presets(ui, t),
            Key::ShownList => self.layout_list(ui, t),
            Key::ImportTheme => self.import_buttons(ui),
            Key::ExportTheme => self.export_controls(ui, t, d),
            Key::ResetLook => {
                if ui.button("Reset").on_hover_text("Accent, corners, bubbles and chart colours back to the theme's").clicked() {
                    let dk = Knobs::default();
                    d.prefs.accent = Accent::Theme;
                    (k.radius, k.user_bubble, k.assistant_bubble, k.chart) = (dk.radius, dk.user_bubble, dk.assistant_bubble, dk.chart);
                    (k.bubble_padding, k.line_spacing) = (dk.bubble_padding, dk.line_spacing);
                }
            }
            Key::ResetAppearance => {
                if ui.button("Reset everything").on_hover_text("Themes, accent, text, layout and colour overrides (keeps imported themes)").clicked() {
                    d.prefs = Prefs { imported: std::mem::take(&mut d.prefs.imported), ..Prefs::default() };
                }
            }
            Key::Zoom => {
                if let Some(z) = crate::header::zoom_controls(ui, d.prefs.zoom) {
                    d.prefs.zoom = z;
                }
            }
            Key::TextSize => scale_buttons(ui, &mut k.font_scale),
            Key::CodeSize => scale_buttons(ui, &mut k.code_scale),
            Key::Font => {
                for f in UiFont::ALL {
                    ui.selectable_value(&mut k.ui_font, f, f.label());
                }
            }
            Key::CodeFont => {
                for f in MonoFont::ALL {
                    ui.selectable_value(&mut k.mono_font, f, f.label());
                }
            }
            Key::Density => {
                for dn in Density::ALL {
                    ui.selectable_value(&mut k.density, dn, dn.label());
                }
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
                ui.add(egui::Slider::new(&mut k.sidebar_width, Knobs::SIDEBAR_RANGE).step_by(10.0).suffix(" pt"));
            }
            Key::ArtifactPane => {
                let mut pct = k.pane_share * 100.0;
                if ui.add(egui::Slider::new(&mut pct, 30.0..=75.0).step_by(1.0).suffix(" %")).changed() {
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
                if ui.button("Reset").on_hover_text("Zoom, sizes, fonts, spacing and layout back to defaults").clicked() {
                    let dk = Knobs::default();
                    d.prefs.zoom = 1.0;
                    (k.font_scale, k.code_scale, k.ui_font, k.mono_font, k.density) = (dk.font_scale, dk.code_scale, dk.ui_font, dk.mono_font, dk.density);
                    (k.align, k.max_width, k.sidebar_width, k.pane_share) = (dk.align, dk.max_width, dk.sidebar_width, dk.pane_share);
                    (k.high_contrast, k.reduce_motion) = (dk.high_contrast, dk.reduce_motion);
                }
            }
            Key::ColourRoles => advanced(ui, t, &d.current, &mut d.prefs, &mut self.appearance.filter),
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
        if ui.button("Import…").on_hover_text("Add a theme from a .json theme file").clicked() {
            crate::upload::pick_files(ui.ctx(), self.appearance.import_tx.clone());
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(dir) = &self.themes_dir
            && ui.button("Reload").on_hover_text(format!("Re-read {}", dir.display())).clicked()
        {
            self.reload_themes();
        }
    }

    /// A name for the current look, and Export… / Save to themes.
    fn export_controls(&mut self, ui: &mut egui::Ui, t: &Tokens, d: &mut Draw) {
        let current = d.current.clone();
        if self.appearance.export_name.is_empty() {
            self.appearance.export_name = if current.is_builtin() { format!("{} (mine)", current.name) } else { current.name.clone() };
        }
        ui.add(form::field(&mut self.appearance.export_name, t).desired_width(170.0).hint_text("Theme name"));
        let text = file::export(&current, &d.prefs, &self.appearance.export_name);
        let filename = format!("{}.json", file::slug(&self.appearance.export_name));
        if ui.button("Export…").on_hover_text("Save the current look (colours and knobs) as a theme file").clicked() {
            self.appearance.exports.save(ui.ctx(), &filename, "application/json", text.clone().into_bytes());
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(dir) = self.themes_dir.clone()
            && ui.button("Save to themes").on_hover_text(format!("Save into {} and list it in the gallery", dir.display())).clicked()
        {
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

fn theme_combo(ui: &mut egui::Ui, id: &str, label: &str, value: &mut String, themes: &[ThemeDef], dark: bool) {
    let shown = themes.iter().find(|d| d.id == *value).map_or_else(|| value.clone(), |d| d.name.clone());
    ui.label(label);
    egui::ComboBox::from_id_salt(id).selected_text(shown).show_ui(ui, |ui| {
        // Themes of the matching kind first; the others are allowed too.
        for d in themes.iter().filter(|d| d.dark == dark).chain(themes.iter().filter(|d| d.dark != dark)) {
            ui.selectable_value(value, d.id.clone(), &d.name);
        }
    });
}

/// Swatch cards, one per theme. Click to use; right-click a user theme to remove it.
fn gallery(ui: &mut egui::Ui, t: &Tokens, themes: &[ThemeDef], prefs: &mut Prefs, dark_now: bool, remove: &mut Option<ThemeDef>) {
    let gap = ui.spacing().item_spacing.x;
    let avail = ui.available_width();
    // Two columns on a phone, else as many ~150 pt cards as fit, spread to fill the row.
    let cols = if avail < form::NARROW { 2.0 } else { ((avail + gap) / (150.0 + gap)).floor().max(2.0) };
    let w = ((avail - gap * (cols - 1.0)) / cols).floor().clamp(100.0, 220.0);
    let h = (w * 0.62).round();
    // Cards show each theme's own look (its accent) with this device's knobs and overrides.
    let card_prefs = Prefs { accent: Accent::Theme, ..prefs.clone() };
    let active = prefs.theme_id(dark_now).to_owned();
    let mut picked = None;
    ui.horizontal_wrapped(|ui| {
        for def in themes {
            let ct = Tokens::build(def, &card_prefs);
            let slot = if def.id == prefs.dark_theme && def.id == prefs.light_theme {
                Some("dark + light")
            } else if def.id == prefs.dark_theme {
                Some("dark")
            } else if def.id == prefs.light_theme {
                Some("light")
            } else {
                None
            };
            let resp = card(ui, t, &ct, def, vec2(w, h), def.id == active, slot);
            let resp = resp.on_hover_text(format!("{} · {} · {}", def.name, if def.dark { "dark" } else { "light" }, def.credit()));
            if resp.clicked() {
                picked = Some(def.clone());
            }
            if !def.is_builtin() {
                resp.context_menu(|ui| {
                    if ui.button("Remove theme").clicked() {
                        *remove = Some(def.clone());
                        ui.close();
                    }
                });
            }
        }
    });
    if let Some(def) = picked {
        pick_theme(prefs, &def, dark_now);
    }
}

/// One gallery card: a miniature of the theme (header, a reply, your message, code) and its name.
fn card(ui: &mut egui::Ui, t: &Tokens, ct: &Tokens, def: &ThemeDef, size: egui::Vec2, active: bool, slot: Option<&str>) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let p = ui.painter_at(rect.expand(2.0));
    let r = t.round(8.0);
    p.rect_filled(rect, r, ct.app_bg);
    let name_h = 20.0f32.min(size.y * 0.3);
    let body = egui::Rect::from_min_max(rect.min, egui::pos2(rect.max.x, rect.max.y - name_h));
    // header strip with the accent as the "byob" mark
    let header = egui::Rect::from_min_size(body.min, vec2(body.width(), body.height() * 0.16));
    p.rect_filled(header, egui::CornerRadius { nw: r as u8, ne: r as u8, sw: 0, se: 0 }, ct.panel_bg);
    p.circle_filled(egui::pos2(header.min.x + 8.0, header.center().y), (header.height() * 0.28).max(2.0), ct.accent);
    let inner = body.shrink2(vec2(7.0, 0.0));
    let line = |y: f32, x0: f32, len: f32, c: Color32| {
        p.line_segment([egui::pos2(x0, y), egui::pos2(x0 + len, y)], Stroke::new(2.0, c));
    };
    // a reply bubble with two lines of text and a code chip
    let reply = egui::Rect::from_min_size(egui::pos2(inner.min.x, header.max.y + 5.0), vec2(inner.width() * 0.78, body.height() * 0.5));
    p.rect_filled(reply, r * 0.5, ct.assistant_bubble);
    line(reply.min.y + 6.0, reply.min.x + 6.0, reply.width() * 0.6, ct.text);
    line(reply.min.y + 11.0, reply.min.x + 6.0, reply.width() * 0.4, ct.text_dim);
    let code = egui::Rect::from_min_max(egui::pos2(reply.min.x + 5.0, reply.min.y + 16.0), egui::pos2(reply.max.x - 5.0, reply.max.y - 4.0));
    if code.height() > 4.0 {
        p.rect_filled(code, 2.0, ct.code_bg);
        let y = code.center().y;
        let seg = code.width() / 5.0;
        for (i, c) in [ct.syntax_keyword, ct.syntax_function, ct.syntax_string].into_iter().enumerate() {
            #[allow(clippy::cast_precision_loss)] // three segments
            let x = code.min.x + 3.0 + i as f32 * (seg + 3.0);
            line(y, x, seg, c);
        }
    }
    // your message, on the right
    let user = egui::Rect::from_min_size(egui::pos2(inner.max.x - inner.width() * 0.5, reply.max.y + 4.0), vec2(inner.width() * 0.5, body.height() * 0.16));
    if user.max.y < body.max.y {
        p.rect_filled(user, r * 0.5, ct.user_bubble);
        line(user.center().y, user.min.x + 6.0, user.width() * 0.5, ct.text);
    }
    // name strip
    let name_rect = egui::Rect::from_min_max(egui::pos2(rect.min.x, rect.max.y - name_h), rect.max);
    p.rect_filled(name_rect, egui::CornerRadius { nw: 0, ne: 0, sw: r as u8, se: r as u8 }, ct.panel_bg);
    let font = egui::FontId::proportional(t.text_size(11.5).min(name_h * 0.7));
    let label = p.layout(def.name.clone(), font.clone(), ct.text, name_rect.width() - 10.0);
    p.galley(egui::pos2(name_rect.min.x + 6.0, name_rect.center().y - label.size().y / 2.0), label, ct.text);
    if let Some(slot) = slot {
        let badge = p.layout_no_wrap(slot.to_owned(), egui::FontId::proportional(9.5), ct.on_accent);
        let b = egui::Rect::from_min_size(egui::pos2(rect.max.x - badge.size().x - 10.0, rect.min.y + 3.0), badge.size() + vec2(6.0, 2.0));
        p.rect_filled(b, 3.0, ct.accent);
        p.galley(b.min + vec2(3.0, 1.0), badge, ct.on_accent);
    }
    let stroke = if active {
        Stroke::new(2.5, t.accent)
    } else if resp.hovered() {
        Stroke::new(1.5, t.text_dim)
    } else {
        Stroke::new(1.0, t.border)
    };
    p.rect_stroke(rect, r, stroke, egui::StrokeKind::Outside);
    resp
}

fn scale_buttons(ui: &mut egui::Ui, value: &mut f32) {
    for (scale, name) in Knobs::FONT_SCALES {
        if ui.selectable_label((*value - scale).abs() < 0.01, name).clicked() {
            *value = scale;
        }
    }
}

/// The theme's own accent, its named accents, and a custom colour.
fn accent_picker(ui: &mut egui::Ui, t: &Tokens, current: &ThemeDef, accent: &mut Accent) {
    let p = current.base;
    let own = current.accent.unwrap_or_else(|| theme::rgb(p.default_accent()));
    if swatch(ui, own, *accent == Accent::Theme, t).on_hover_text("The theme's own accent").clicked() {
        *accent = Accent::Theme;
    }
    for (name, c) in p.accents {
        let selected = matches!(accent, Accent::Named(n) if n.eq_ignore_ascii_case(name));
        if swatch(ui, theme::rgb(*c), selected, t).on_hover_text(*name).clicked() {
            *accent = Accent::Named((*name).to_owned());
        }
    }
    let mut c = t.accent;
    let custom = matches!(accent, Accent::Custom(_));
    if egui::color_picker::color_edit_button_srgba(ui, &mut c, egui::color_picker::Alpha::Opaque).on_hover_text("Any colour").changed() {
        *accent = Accent::Custom(hex(c));
    }
    if custom || matches!(accent, Accent::Blue | Accent::Violet | Accent::Green | Accent::Orange | Accent::Pink | Accent::Teal) {
        ui.label(RichText::new(accent.label()).small().color(t.text_dim));
    }
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

/// A round colour button, ringed when selected.
fn swatch(ui: &mut egui::Ui, color: Color32, selected: bool, t: &Tokens) -> egui::Response {
    let size = ui.spacing().interact_size.y;
    let (rect, resp) = ui.allocate_exact_size(vec2(size, size), egui::Sense::click());
    let center = rect.center();
    let r = size * 0.5;
    if selected {
        ui.painter().circle_stroke(center, r - 1.0, Stroke::new(2.0, t.text_strong));
    } else if resp.hovered() {
        ui.painter().circle_stroke(center, r - 1.0, Stroke::new(1.0, t.border));
    }
    ui.painter().circle_filled(center, r - 4.0, color);
    resp
}

/// Every colour role with a picker, in columns; changes are overrides for the current theme only.
fn advanced(ui: &mut egui::Ui, t: &Tokens, current: &ThemeDef, prefs: &mut Prefs, filter: &mut String) {
    ui.horizontal_wrapped(|ui| {
        ui.add(form::field(filter, t).hint_text("Filter colours").desired_width(200.0));
        let has = prefs.overrides.get(&current.id).is_some_and(|o| !o.is_empty());
        if ui.add_enabled(has, egui::Button::new("Reset all colours")).clicked() {
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

/// One colour role: its name, then at the right its contrast, hex and picker (and Reset when
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
