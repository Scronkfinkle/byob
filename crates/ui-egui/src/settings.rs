//! The settings panel: inference endpoint (engine-wide) and appearance (this device only):
//! a theme gallery, the knobs, theme files, and every colour role under Advanced.

use std::sync::mpsc::{Receiver, Sender, channel};

use egui::{Color32, RichText, Stroke, vec2};

use crate::ByobApp;
use crate::theme::file::{self, Origin, ThemeDef};
use crate::theme::{self, Accent, Align, BubbleStyle, ChartPalette, Density, Knobs, MonoFont, Prefs, ROLES, ThemeKind, Tokens, UiFont, contrast, hex};
use crate::upload::Picked;

/// Below this width the appearance settings stack labels above their controls.
const NARROW: f32 = 560.0;

/// State of the appearance settings between frames.
pub struct AppearanceUi {
    import_tx: Sender<Picked>,
    import_rx: Receiver<Picked>,
    exports: crate::export::Exports,
    export_name: String,
    /// What the last import/export said (ok, text).
    status: Vec<(bool, String)>,
    filter: String,
    /// Scripted runs: keep this section ("knobs", "advanced") scrolled into view.
    pub(crate) reveal: Option<String>,
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
            reveal: None,
        }
    }
}

impl ByobApp {
    pub(crate) fn settings(&mut self, ui: &mut egui::Ui) {
        let t = self.tokens;
        ui.add_space(4.0);
        if std::mem::take(&mut self.reveal_connection) {
            ui.scroll_to_cursor(Some(egui::Align::Min));
        }
        ui.label(RichText::new("Connection").family(theme::bold_or_regular(ui.ctx())).color(t.text_strong))
            .on_hover_text("The model server byob talks to; saved for every window and browser tab");
        match &self.state {
            Some(state) => self.connect.form(ui, &t, state, "settings"),
            None => {
                ui.label(RichText::new("connecting…").color(t.text_dim));
            }
        }
        ui.add_space(10.0);
        ui.label(RichText::new("Appearance").family(theme::bold_or_regular(ui.ctx())).color(t.text_strong)).on_hover_text("Saved on this device only");
        self.appearance_settings(ui);
        ui.add_space(6.0);
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

    fn appearance_settings(&mut self, ui: &mut egui::Ui) {
        let t = self.tokens;
        let narrow = ui.available_width() < NARROW;
        let mut prefs = self.prefs.clone();
        let dark_now = t.dark;
        let themes = file::all(&self.user_themes);
        let current = file::resolve(prefs.theme_id(dark_now), dark_now, &self.user_themes);

        row(ui, narrow, "Mode", |ui| {
            for kind in ThemeKind::ALL {
                ui.selectable_value(&mut prefs.theme, kind, kind.label());
            }
        });
        if prefs.theme == ThemeKind::System {
            row(ui, narrow, "Follows system", |ui| {
                theme_combo(ui, "light-theme", "Light", &mut prefs.light_theme, &themes, false);
                theme_combo(ui, "dark-theme", "Dark", &mut prefs.dark_theme, &themes, true);
            });
        }

        ui.add_space(4.0);
        let mut remove = None;
        gallery(ui, &t, &themes, &mut prefs, dark_now, &mut remove);
        if let Some(def) = remove {
            self.remove_theme(&def);
            return;
        }
        ui.add_space(4.0);
        self.theme_files(ui, &t, narrow, &current, &prefs);

        ui.add_space(6.0);
        if self.appearance.reveal.as_deref() == Some("knobs") {
            ui.scroll_to_cursor(Some(egui::Align::Min));
        }
        ui.label(RichText::new("Knobs").family(theme::bold_or_regular(ui.ctx())).color(t.text_strong));
        knobs(ui, &t, narrow, &current, &mut prefs);
        ui.horizontal_wrapped(|ui| {
            if ui.button("Reset knobs").on_hover_text("Sizes, fonts, spacing and layout back to defaults").clicked() {
                prefs.knobs = Knobs::default();
                prefs.accent = Accent::Theme;
            }
            if ui.button("Reset all appearance").on_hover_text("Themes, accent, knobs and colour overrides (keeps imported themes)").clicked() {
                prefs = Prefs { imported: std::mem::take(&mut prefs.imported), ..Prefs::default() };
            }
        });

        ui.add_space(4.0);
        let reveal = self.appearance.reveal.as_deref() == Some("advanced");
        if reveal {
            ui.scroll_to_cursor(Some(egui::Align::Min));
        }
        let header = egui::CollapsingHeader::new(RichText::new("Advanced: every colour").color(t.text_strong)).id_salt("advanced-colours");
        header.open(reveal.then_some(true)).show(ui, |ui| {
            advanced(ui, &t, &current, &mut prefs, &mut self.appearance.filter);
        });

        if prefs != self.prefs {
            self.set_prefs(prefs);
        }
    }

    fn theme_files(&mut self, ui: &mut egui::Ui, t: &Tokens, narrow: bool, current: &ThemeDef, prefs: &Prefs) {
        row(ui, narrow, "Theme files", |ui| {
            if ui.button("Import…").on_hover_text("Add a theme from a .json theme file").clicked() {
                crate::upload::pick_files(ui.ctx(), self.appearance.import_tx.clone());
            }
            if self.appearance.export_name.is_empty() {
                self.appearance.export_name = if current.is_builtin() { format!("{} (mine)", current.name) } else { current.name.clone() };
            }
            ui.add(egui::TextEdit::singleline(&mut self.appearance.export_name).desired_width(150.0).hint_text("Theme name"));
            let text = file::export(current, prefs, &self.appearance.export_name);
            let filename = format!("{}.json", file::slug(&self.appearance.export_name));
            if ui.button("Export…").on_hover_text("Save the current look (colours and knobs) as a theme file").clicked() {
                self.appearance.exports.save(ui.ctx(), &filename, "application/json", text.clone().into_bytes());
            }
            #[cfg(not(target_arch = "wasm32"))]
            if let Some(dir) = self.themes_dir.clone() {
                if ui.button("Save to themes").on_hover_text(format!("Save into {} and list it in the gallery", dir.display())).clicked() {
                    let mut p = prefs.clone();
                    let def = file::parse(&text, Origin::Imported(0)).map(|f| f.def);
                    match def.map_err(|e| e.to_string()).and_then(|d| self.store_theme(&mut p, &d, &text).map(|w| (d, w))) {
                        Ok((d, w)) => {
                            self.appearance.status.push((true, format!("Saved \"{}\" {w}", d.name)));
                            self.reload_themes();
                            if let Some(saved) = self.user_themes.iter().find(|u| u.id == d.id).cloned() {
                                // The saved theme already carries the overrides.
                                p.overrides.remove(&current.id);
                                pick_theme(&mut p, &saved, t.dark);
                                self.set_prefs(p);
                            }
                        }
                        Err(e) => self.appearance.status.push((false, e)),
                    }
                }
                if ui.button("Reload").on_hover_text(format!("Re-read {}", dir.display())).clicked() {
                    self.reload_themes();
                }
            }
        });
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

/// A label and its controls: side by side, or stacked on narrow screens.
fn row(ui: &mut egui::Ui, narrow: bool, label: &str, add: impl FnOnce(&mut egui::Ui)) {
    if narrow {
        ui.label(label);
        ui.horizontal_wrapped(add);
    } else {
        ui.horizontal_wrapped(|ui| {
            let h = ui.spacing().interact_size.y;
            ui.allocate_ui_with_layout(vec2(130.0, h), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.set_min_size(vec2(130.0, h));
                ui.label(label);
            });
            add(ui);
        });
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
    // Two columns on a phone, else ~150 pt cards.
    let w = if avail < NARROW { ((avail - gap) / 2.0).floor().max(100.0) } else { 150.0 };
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

fn knobs(ui: &mut egui::Ui, t: &Tokens, narrow: bool, current: &ThemeDef, prefs: &mut Prefs) {
    row(ui, narrow, "Accent", |ui| accent_picker(ui, t, current, &mut prefs.accent));
    let k = &mut prefs.knobs;
    row(ui, narrow, "Text size", |ui| scale_buttons(ui, &mut k.font_scale));
    row(ui, narrow, "Code size", |ui| scale_buttons(ui, &mut k.code_scale));
    row(ui, narrow, "Font", |ui| {
        for f in UiFont::ALL {
            ui.selectable_value(&mut k.ui_font, f, f.label());
        }
    });
    row(ui, narrow, "Code font", |ui| {
        for f in MonoFont::ALL {
            ui.selectable_value(&mut k.mono_font, f, f.label());
        }
    });
    row(ui, narrow, "Density", |ui| {
        for d in Density::ALL {
            ui.selectable_value(&mut k.density, d, d.label());
        }
    });
    row(ui, narrow, "Corner radius", |ui| {
        ui.add(egui::Slider::new(&mut k.radius, Knobs::RADIUS_RANGE).step_by(1.0).suffix(" pt"));
    });
    row(ui, narrow, "Your messages", |ui| {
        for s in BubbleStyle::ALL {
            ui.selectable_value(&mut k.user_bubble, s, s.label());
        }
    });
    row(ui, narrow, "Replies", |ui| {
        for s in BubbleStyle::ALL {
            ui.selectable_value(&mut k.assistant_bubble, s, s.label());
        }
    });
    row(ui, narrow, "Layout", |ui| {
        for a in Align::ALL {
            ui.selectable_value(&mut k.align, a, a.label());
        }
    });
    row(ui, narrow, "Column width", |ui| {
        for (w, name) in Knobs::WIDTHS {
            if ui
                .selectable_label((k.max_width - w).abs() < 1.0, name)
                .on_hover_text(if w > 0.0 { format!("At most {w} pt wide") } else { "The whole window".into() })
                .clicked()
            {
                k.max_width = w;
            }
        }
    });
    row(ui, narrow, "Chart colours", |ui| {
        for c in ChartPalette::ALL {
            ui.selectable_value(&mut k.chart, c, c.label());
        }
    });
    row(ui, narrow, "Chat list width", |ui| {
        ui.add(egui::Slider::new(&mut k.sidebar_width, Knobs::SIDEBAR_RANGE).step_by(10.0).suffix(" pt"));
    });
    row(ui, narrow, "Artifact pane", |ui| {
        let mut pct = k.pane_share * 100.0;
        if ui.add(egui::Slider::new(&mut pct, 30.0..=75.0).step_by(1.0).suffix(" %")).changed() {
            k.pane_share = pct / 100.0;
        }
    });
    row(ui, narrow, "Accessibility", |ui| {
        ui.checkbox(&mut k.high_contrast, "High contrast");
        ui.checkbox(&mut k.reduce_motion, "Reduce motion");
    });
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

/// Every colour role with a picker; changes are overrides for the current theme only.
fn advanced(ui: &mut egui::Ui, t: &Tokens, current: &ThemeDef, prefs: &mut Prefs, filter: &mut String) {
    ui.label(RichText::new(format!("Changes apply to {} on this device. Export to keep them as a theme file.", current.name)).small().color(t.text_dim));
    ui.horizontal_wrapped(|ui| {
        ui.add(egui::TextEdit::singleline(filter).hint_text("Filter roles").desired_width(160.0));
        let has = prefs.overrides.get(&current.id).is_some_and(|o| !o.is_empty());
        if ui.add_enabled(has, egui::Button::new("Reset all colours")).clicked() {
            prefs.overrides.remove(&current.id);
        }
    });
    let needle = filter.trim().to_lowercase();
    let mut group = "";
    for role in ROLES {
        if !needle.is_empty() && !role.label.to_lowercase().contains(&needle) && !role.name.contains(&needle) && !role.group.to_lowercase().contains(&needle) {
            continue;
        }
        if role.group != group {
            group = role.group;
            ui.add_space(4.0);
            ui.label(RichText::new(group).small().color(t.text_faint));
        }
        let Some(mut c) = t.get(role.name) else { continue };
        let overridden = prefs.overrides.get(&current.id).is_some_and(|o| o.contains_key(role.name));
        ui.horizontal_wrapped(|ui| {
            let alpha = if c.a() < 255 { egui::color_picker::Alpha::OnlyBlend } else { egui::color_picker::Alpha::Opaque };
            if egui::color_picker::color_edit_button_srgba(ui, &mut c, alpha).changed() {
                if role.name == "accent" {
                    prefs.accent = Accent::Custom(hex(c));
                } else {
                    prefs.overrides.entry(current.id.clone()).or_default().insert(role.name.into(), hex(c));
                }
            }
            ui.label(RichText::new(role.label).color(if overridden { t.text_strong } else { t.text }));
            ui.label(RichText::new(hex(c)).small().monospace().color(t.text_faint));
            if let Some(bg) = read_on(role.name).and_then(|b| t.get(b)) {
                let ratio = contrast(c, bg);
                let color = if ratio >= 4.5 { t.text_dim } else { t.danger };
                ui.label(RichText::new(format!("{ratio:.1}:1")).small().color(color))
                    .on_hover_text("Contrast against what it's drawn on (WCAG: 4.5:1 text, 7:1 enhanced)");
            }
            if overridden
                && ui.small_button("Reset").clicked()
                && let Some(o) = prefs.overrides.get_mut(&current.id)
            {
                o.remove(role.name);
            }
        });
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::presets;

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
                    egui::CollapsingHeader::new("x").default_open(true).show(ui, |ui| app.appearance_settings(ui));
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
