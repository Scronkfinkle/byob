//! Design tokens. Every widget reads colours and sizes from [`Tokens`] so themes apply
//! everywhere.
//!
//! A theme is a [`presets::Palette`] (the built-in presets, or the base of a theme file)
//! plus optional colour overrides ([`file::ThemeDef`]). [`Tokens::build`] derives every role
//! from it, then applies the per-device [`Prefs`]: accent, knobs, high contrast, and the
//! user's own per-theme overrides. Prefs are per device — a phone can be light while the
//! desktop is dark — so the apps persist them with eframe storage.

pub mod file;
pub mod prefs;
pub mod presets;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Shadow, Stroke, TextStyle, Visuals};

pub use file::{ThemeDef, ThemeFile};
pub use prefs::{Accent, Align, BubbleStyle, ChartPalette, Density, Knobs, MonoFont, Prefs, ThemeKind, UiFont};
use presets::Palette;

/// Font family for bold text and headings (Inter SemiBold). egui's default fonts have no bold.
pub fn bold_family() -> FontFamily {
    FontFamily::Name("bold".into())
}

/// The bold family if installed (fonts set with `install_fonts` only apply from the next
/// frame, and egui panics on unknown families), else the regular one.
pub fn bold_or_regular(ctx: &egui::Context) -> FontFamily {
    let bold = bold_family();
    if ctx.fonts(|f| f.definitions().families.contains_key(&bold)) { bold } else { FontFamily::Proportional }
}

/// Install the UI fonts: Inter (OFL) or egui's Ubuntu for text, Inter SemiBold for bold, Hack
/// or JetBrains Mono (OFL) for code, egui's defaults (emoji, symbols) as fallbacks.
pub fn install_fonts(ctx: &egui::Context, ui_font: UiFont, mono_font: MonoFont) {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert("inter".into(), Arc::new(FontData::from_static(include_bytes!("../../assets/fonts/Inter-Regular.ttf"))));
    fonts.font_data.insert("inter-semibold".into(), Arc::new(FontData::from_static(include_bytes!("../../assets/fonts/Inter-SemiBold.ttf"))));
    fonts.font_data.insert("jetbrains-mono".into(), Arc::new(FontData::from_static(include_bytes!("../../assets/fonts/JetBrainsMono-Regular.ttf"))));
    let fallbacks = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    if ui_font == UiFont::Inter {
        fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "inter".into());
    }
    if mono_font == MonoFont::JetBrainsMono {
        fonts.families.entry(FontFamily::Monospace).or_default().insert(0, "jetbrains-mono".into());
    }
    fonts.families.insert(bold_family(), std::iter::once("inter-semibold".to_owned()).chain(fallbacks).collect());
    ctx.set_fonts(fonts);
}

/// A colour role: a field of [`Tokens`] users can override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Role {
    /// Field name, also the key in theme files and overrides (`app_bg`).
    pub name: &'static str,
    pub label: &'static str,
    pub group: &'static str,
}

/// Defines [`Tokens`] (colour roles plus the non-colour fields) and the role table.
macro_rules! tokens {
    ($( $group:literal => { $( $(#[$doc:meta])* $field:ident : $label:literal ),* $(,)? } )*) => {
        #[derive(Debug, Clone, Copy, PartialEq)]
        pub struct Tokens {
            pub dark: bool,
            $($( $(#[$doc])* pub $field: Color32, )*)*
            /// Chart series colours, in order (roles `chart1`…`chart8`).
            pub chart: [Color32; 8],
            /// Base corner radius in points.
            pub radius: f32,
            pub knobs: Knobs,
        }

        /// Every overridable colour, grouped for the Advanced settings.
        pub const ROLES: &[Role] = &[
            $($( Role { name: stringify!($field), label: $label, group: $group }, )*)*
            Role { name: "chart1", label: "Series 1", group: "Charts" },
            Role { name: "chart2", label: "Series 2", group: "Charts" },
            Role { name: "chart3", label: "Series 3", group: "Charts" },
            Role { name: "chart4", label: "Series 4", group: "Charts" },
            Role { name: "chart5", label: "Series 5", group: "Charts" },
            Role { name: "chart6", label: "Series 6", group: "Charts" },
            Role { name: "chart7", label: "Series 7", group: "Charts" },
            Role { name: "chart8", label: "Series 8", group: "Charts" },
        ];

        impl Tokens {
            /// A role's colour by name.
            pub fn get(&self, role: &str) -> Option<Color32> {
                match role {
                    $($( stringify!($field) => Some(self.$field), )*)*
                    _ => chart_slot(role).and_then(|i| self.chart.get(i).copied()),
                }
            }

            /// Set a role by name; `false` if there is no such role.
            pub fn set(&mut self, role: &str, c: Color32) -> bool {
                match role {
                    $($( stringify!($field) => self.$field = c, )*)*
                    _ => match chart_slot(role).and_then(|i| self.chart.get_mut(i)) {
                        Some(slot) => *slot = c,
                        None => return false,
                    },
                }
                true
            }
        }
    };
}

/// `chart3` → 2.
fn chart_slot(role: &str) -> Option<usize> {
    let n: usize = role.strip_prefix("chart")?.parse().ok()?;
    (1..=8).contains(&n).then(|| n - 1)
}

tokens! {
    "Surfaces" => {
        /// Window background behind everything (the transcript).
        app_bg: "Window background",
        /// Header, composer and settings panels.
        panel_bg: "Panels",
        sidebar_bg: "Chat list",
        /// Text inputs.
        field_bg: "Text fields",
        field_border: "Text field border",
        /// Outlines of frames, tables, buttons.
        border: "Borders",
        separator: "Separators",
        button_bg: "Buttons",
        hover: "Hovered",
        pressed: "Pressed",
        selection: "Selection",
        shadow: "Shadows",
    }
    "Messages" => {
        user_bubble: "Your messages",
        assistant_bubble: "Replies",
    }
    "Text" => {
        text: "Body text",
        /// Bold text and headings.
        text_strong: "Headings and bold",
        text_dim: "Secondary text",
        text_faint: "Faint text",
        link: "Links",
        accent: "Accent",
        accent_hover: "Accent, hovered",
        /// Text drawn on an `accent` fill.
        on_accent: "Text on accent",
    }
    "Status" => {
        danger: "Errors",
        warning: "Warnings",
        success: "Success",
    }
    "Code" => {
        /// Fenced code blocks.
        code_bg: "Code background",
        code_border: "Code border",
        code_text: "Code text",
        /// `inline code` spans.
        inline_code_bg: "Inline code background",
        inline_code_text: "Inline code text",
        syntax_keyword: "Keywords",
        syntax_string: "Strings",
        syntax_number: "Numbers",
        syntax_comment: "Comments",
        syntax_function: "Functions",
        syntax_type: "Types",
    }
    "Markdown" => {
        quote_bar: "Quote bar",
        table_header_bg: "Table header",
        table_stripe: "Table stripes",
    }
}

/// Linear blend in sRGB space: `t = 0` → `a`, `t = 1` → `b` (opaque result).
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = if t.is_finite() { t.clamp(0.0, 1.0) } else { 0.0 };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // clamped to 0..=255
    let ch = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round().clamp(0.0, 255.0) as u8;
    Color32::from_rgb(ch(a.r(), b.r()), ch(a.g(), b.g()), ch(a.b(), b.b()))
}

/// `0xRRGGBB` → colour.
pub const fn rgb(v: u32) -> Color32 {
    Color32::from_rgb(((v >> 16) & 0xff) as u8, ((v >> 8) & 0xff) as u8, (v & 0xff) as u8)
}

/// `#rrggbb`, or `#rrggbbaa` when not opaque (unmultiplied).
pub fn hex(c: Color32) -> String {
    let [r, g, b, a] = c.to_srgba_unmultiplied();
    if a == 255 { format!("#{r:02x}{g:02x}{b:02x}") } else { format!("#{r:02x}{g:02x}{b:02x}{a:02x}") }
}

/// Parse `#rgb`, `#rrggbb` or `#rrggbbaa` (the `#` is optional). Anything else → `None`.
pub fn parse_color(s: &str) -> Option<Color32> {
    let h = s.trim();
    let h = h.strip_prefix('#').unwrap_or(h);
    if !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| h.get(i..i + 2).and_then(|x| u8::from_str_radix(x, 16).ok());
    match h.len() {
        3 => {
            let n = |i: usize| h.get(i..=i).and_then(|x| u8::from_str_radix(x, 16).ok()).map(|v| v * 17);
            Some(Color32::from_rgb(n(0)?, n(1)?, n(2)?))
        }
        6 => Some(Color32::from_rgb(byte(0)?, byte(2)?, byte(4)?)),
        8 => Some(Color32::from_rgba_unmultiplied(byte(0)?, byte(2)?, byte(4)?, byte(6)?)),
        _ => None,
    }
}

/// WCAG relative luminance of an sRGB colour (alpha ignored).
pub fn luminance(c: Color32) -> f32 {
    let lin = |x: u8| {
        let s = f32::from(x) / 255.0;
        if s <= 0.040_45 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
}

/// WCAG contrast ratio (1–21).
pub fn contrast(a: Color32, b: Color32) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// `fg`, nudged toward white (dark backgrounds) or black (light ones) just until it reaches
/// `min` contrast against every background. Unchanged if it already does.
pub fn ensure_contrast(fg: Color32, bgs: &[Color32], min: f32) -> Color32 {
    let ok = |c: Color32| bgs.iter().all(|bg| contrast(c, *bg) >= min);
    if ok(fg) {
        return fg;
    }
    let dark_bg = bgs.iter().map(|c| luminance(*c)).fold(0.0f32, f32::max) < 0.18;
    let target = if dark_bg { Color32::WHITE } else { Color32::BLACK };
    (1..=20).map(|i| mix(fg, target, i as f32 / 20.0)).find(|c| ok(*c)).unwrap_or(target)
}

impl Tokens {
    pub fn dark() -> Self {
        Self::derive(&presets::BYOB_DARK, rgb(presets::BYOB_DARK.default_accent()), &Knobs::default())
    }

    pub fn light() -> Self {
        Self::derive(&presets::BYOB_LIGHT, rgb(presets::BYOB_LIGHT.default_accent()), &Knobs::default())
    }

    pub fn has_role(role: &str) -> bool {
        ROLES.iter().any(|r| r.name == role)
    }

    /// Tokens for a set of preferences, given the OS/browser theme if known and the user's
    /// own themes (theme ids not found fall back to byob's pair).
    pub fn for_prefs(prefs: &Prefs, system: Option<egui::Theme>, user: &[ThemeDef]) -> Self {
        let dark = prefs.theme.is_dark(system);
        let def = file::resolve(prefs.theme_id(dark), dark, user);
        Self::build(&def, prefs)
    }

    /// A theme with the user's accent, knobs, contrast setting and overrides applied.
    pub fn build(def: &ThemeDef, prefs: &Prefs) -> Self {
        let mut palette = *def.base;
        palette.dark = def.dark;
        let accent = match &prefs.accent {
            Accent::Theme => def.accent.unwrap_or_else(|| rgb(palette.default_accent())),
            Accent::Named(name) => palette.accent(name).map(rgb).or(def.accent).unwrap_or_else(|| rgb(palette.default_accent())),
            Accent::Custom(c) => parse_color(c).unwrap_or_else(|| rgb(palette.default_accent())),
            fixed => fixed.fixed_color(palette.dark).map_or_else(|| rgb(palette.default_accent()), rgb),
        };
        let mut t = Self::derive(&palette, accent, &prefs.knobs);
        for (role, c) in &def.colors {
            t.set(role, *c);
        }
        if prefs.knobs.high_contrast {
            t.raise_contrast();
        }
        for (role, c) in prefs.overrides_for(&def.id).into_iter().flatten() {
            if let Some(c) = parse_color(c) {
                t.set(role, c);
            }
        }
        t
    }

    /// Every role from a palette. Text-like colours are nudged until readable on the
    /// surfaces they're drawn on (accents and status ≥ 4.5:1, faint text ≥ 3:1). Body text
    /// keeps the scheme's own colour unless it falls below 4.5:1 somewhere (it aims for 7:1;
    /// see the contrast tests for the presets documented as AA-only).
    pub fn derive(p: &Palette, accent: Color32, knobs: &Knobs) -> Self {
        let dark = p.dark;
        let (bg, panel, field) = (rgb(p.bg), rgb(p.panel), rgb(p.field));
        let [s0, s1, s2] = p.surface.map(rgb);
        let extreme = if dark { Color32::WHITE } else { Color32::BLACK };
        let assistant = if dark { mix(bg, s0, 0.45) } else { mix(bg, field, 0.7) };
        let base_surfaces = [bg, panel, field, assistant];
        let accent0 = ensure_contrast(accent, &base_surfaces, 4.5);
        let user = mix(assistant, accent0, if dark { 0.13 } else { 0.10 });
        let surfaces = [bg, panel, field, assistant, user];
        let accent = ensure_contrast(accent0, &surfaces, 4.5);
        let on_accent = [field, bg, Color32::WHITE, Color32::BLACK].into_iter().find(|c| contrast(*c, accent) >= 4.5).unwrap_or(if luminance(accent) > 0.18 {
            Color32::BLACK
        } else {
            Color32::WHITE
        });
        let code_bg = if dark { mix(bg, field, 0.5) } else { mix(bg, s0, 0.35) };
        let text = ensure_contrast(rgb(p.text), &[bg, panel, field, assistant, user, code_bg], 4.5);
        let inline_code_bg = if dark { s0 } else { mix(s0, bg, 0.4) };
        let syntax = p.syntax.map(|c| ensure_contrast(rgb(c), &[code_bg], 4.5));
        let chart_src = match knobs.chart {
            ChartPalette::Theme => p.chart,
            ChartPalette::Accessible if dark => presets::ACCESSIBLE_CHART_DARK,
            ChartPalette::Accessible => presets::ACCESSIBLE_CHART_LIGHT,
        };
        // Series are graphics: 3:1 against the chart's background (WCAG non-text contrast).
        let chart = chart_src.map(|c| ensure_contrast(rgb(c), &[assistant], 3.0));
        Tokens {
            dark,
            app_bg: bg,
            panel_bg: panel,
            sidebar_bg: panel,
            field_bg: field,
            field_border: mix(s1, s2, 0.6),
            border: s1,
            separator: s0,
            button_bg: s0,
            hover: s1,
            pressed: mix(s1, s2, 0.5),
            selection: accent.gamma_multiply(if dark { 0.4 } else { 0.28 }),
            shadow: Color32::from_black_alpha(if dark { 96 } else { 40 }),
            user_bubble: user,
            assistant_bubble: assistant,
            text,
            text_strong: p.strong.map_or_else(|| mix(text, extreme, 0.45), rgb),
            text_dim: ensure_contrast(rgb(p.subtext), &surfaces, 4.5),
            text_faint: ensure_contrast(rgb(p.overlay), &[bg, panel, assistant], 3.0),
            link: accent,
            accent,
            accent_hover: mix(accent, extreme, 0.15),
            on_accent,
            danger: ensure_contrast(rgb(p.red), &surfaces, 4.5),
            warning: ensure_contrast(rgb(p.yellow), &surfaces, 4.5),
            success: ensure_contrast(rgb(p.green), &surfaces, 4.5),
            code_bg,
            code_border: if dark { s0 } else { s1 },
            code_text: text,
            inline_code_bg,
            inline_code_text: ensure_contrast(rgb(p.inline_code), &[inline_code_bg], 4.5),
            syntax_keyword: syntax[0],
            syntax_string: syntax[1],
            syntax_number: syntax[2],
            syntax_comment: syntax[3],
            syntax_function: syntax[4],
            syntax_type: syntax[5],
            quote_bar: s2,
            table_header_bg: if dark { s0 } else { mix(bg, s0, 0.5) },
            table_stripe: mix(assistant, s0, 0.35),
            chart,
            radius: knobs.radius,
            knobs: *knobs,
        }
    }

    /// High contrast: body text ≥ 7:1 everywhere, brighter secondary text and borders,
    /// deeper backgrounds.
    fn raise_contrast(&mut self) {
        let (fg, bg) = if self.dark { (Color32::WHITE, Color32::BLACK) } else { (Color32::BLACK, Color32::WHITE) };
        let t = self;
        t.app_bg = mix(t.app_bg, bg, 0.5);
        t.field_bg = mix(t.field_bg, bg, 0.5);
        t.code_bg = mix(t.code_bg, bg, 0.4);
        let surfaces = [t.app_bg, t.panel_bg, t.field_bg, t.assistant_bubble, t.user_bubble];
        t.text = ensure_contrast(mix(t.text, fg, 0.6), &surfaces, 7.0);
        t.text_strong = ensure_contrast(t.text_strong, &surfaces, 7.0);
        t.text_dim = ensure_contrast(mix(t.text_dim, fg, 0.45), &surfaces, 4.5);
        t.text_faint = ensure_contrast(mix(t.text_faint, fg, 0.4), &surfaces, 4.5);
        t.code_text = ensure_contrast(mix(t.code_text, fg, 0.6), &[t.code_bg], 7.0);
        t.border = mix(t.border, fg, 0.35);
        t.field_border = mix(t.field_border, fg, 0.4);
        t.code_border = mix(t.code_border, fg, 0.35);
        t.quote_bar = mix(t.quote_bar, fg, 0.35);
        for c in [&mut t.syntax_keyword, &mut t.syntax_string, &mut t.syntax_number, &mut t.syntax_comment, &mut t.syntax_function, &mut t.syntax_type] {
            *c = ensure_contrast(*c, &[t.code_bg], 7.0);
        }
    }

    /// A corner radius designed at the default radius (6), scaled to the user's.
    pub fn round(&self, at_default: f32) -> f32 {
        (at_default * self.radius / 6.0).max(0.0)
    }

    /// A gap or padding designed for Comfortable density, scaled to the user's.
    pub fn pad(&self, at_comfortable: f32) -> f32 {
        at_comfortable * self.knobs.density.factor()
    }

    /// A text size designed at 100 %, scaled to the user's UI text size.
    pub fn text_size(&self, at_default: f32) -> f32 {
        at_default * self.knobs.font_scale
    }

    /// Push the tokens into egui's visuals, text styles and spacing (both egui themes, so
    /// nothing depends on which one egui thinks is active).
    pub fn apply(&self, ctx: &egui::Context) {
        let visuals = self.visuals();
        let k = self.knobs;
        let (ui, code) = (k.font_scale.clamp(0.5, 3.0), k.code_scale.clamp(0.5, 3.0));
        let d = k.density.factor();
        ctx.all_styles_mut(|style| {
            style.visuals = visuals.clone();
            style.text_styles = [
                (TextStyle::Small, FontId::new(11.0 * ui, FontFamily::Proportional)),
                (TextStyle::Body, FontId::new(14.0 * ui, FontFamily::Proportional)),
                (TextStyle::Button, FontId::new(14.0 * ui, FontFamily::Proportional)),
                (TextStyle::Monospace, FontId::new(13.0 * code, FontFamily::Monospace)),
                (TextStyle::Heading, FontId::new(20.0 * ui, FontFamily::Proportional)),
            ]
            .into();
            style.spacing.button_padding = egui::vec2(8.0 * d, 4.0 * d);
            style.spacing.item_spacing = egui::vec2(8.0 * d.max(0.8), 5.0 * d);
            style.spacing.interact_size.y = (18.0 * d.max(0.9)).max(14.0 * ui);
            style.spacing.window_margin = egui::Margin::same((8.0 * d).round().clamp(2.0, 20.0) as i8);
            style.spacing.menu_margin = egui::Margin::same((6.0 * d).round().clamp(2.0, 16.0) as i8);
            style.animation_time = if k.reduce_motion { 0.0 } else { 1.0 / 12.0 };
            style.scroll_animation = if k.reduce_motion { egui::style::ScrollAnimation::none() } else { egui::style::ScrollAnimation::default() };
        });
        ctx.set_theme(if self.dark { egui::ThemePreference::Dark } else { egui::ThemePreference::Light });
    }

    pub fn visuals(&self) -> Visuals {
        let mut v = if self.dark { Visuals::dark() } else { Visuals::light() };
        let cr = |x: f32| CornerRadius::same(self.round(x).round().clamp(0.0, 40.0) as u8);
        let r = cr(5.0);
        v.dark_mode = self.dark;
        v.override_text_color = None;
        v.weak_text_color = Some(self.text_dim);
        v.hyperlink_color = self.link;
        v.faint_bg_color = self.table_stripe;
        v.extreme_bg_color = self.field_bg;
        v.text_edit_bg_color = Some(self.field_bg);
        v.code_bg_color = self.inline_code_bg;
        v.warn_fg_color = self.warning;
        v.error_fg_color = self.danger;
        v.panel_fill = self.panel_bg;
        v.window_fill = self.panel_bg;
        v.window_stroke = Stroke::new(1.0, self.border);
        v.window_corner_radius = cr(8.0);
        v.menu_corner_radius = cr(6.0);
        v.window_shadow = Shadow { offset: [0, 6], blur: 18, spread: 0, color: self.shadow };
        v.popup_shadow = Shadow { offset: [0, 3], blur: 10, spread: 0, color: self.shadow };
        v.selection.bg_fill = self.selection;
        v.selection.stroke = Stroke::new(1.0, self.text_strong);
        v.text_cursor.stroke = Stroke::new(2.0, self.accent);
        v.text_cursor.blink = !self.knobs.reduce_motion;
        v.image_loading_spinners = !self.knobs.reduce_motion;
        v.slider_trailing_fill = true;

        let w = &mut v.widgets;
        w.noninteractive.bg_fill = self.panel_bg;
        w.noninteractive.weak_bg_fill = self.panel_bg;
        w.noninteractive.bg_stroke = Stroke::new(1.0, self.separator);
        w.noninteractive.fg_stroke = Stroke::new(1.0, self.text);
        w.noninteractive.corner_radius = r;

        w.inactive.bg_fill = self.button_bg;
        w.inactive.weak_bg_fill = self.button_bg;
        w.inactive.bg_stroke = Stroke::new(1.0, self.border);
        w.inactive.fg_stroke = Stroke::new(1.0, self.text);
        w.inactive.corner_radius = r;

        w.hovered.bg_fill = self.hover;
        w.hovered.weak_bg_fill = self.hover;
        w.hovered.bg_stroke = Stroke::new(1.0, self.accent);
        w.hovered.fg_stroke = Stroke::new(1.5, self.text_strong);
        w.hovered.corner_radius = r;

        w.active.bg_fill = self.pressed;
        w.active.weak_bg_fill = self.pressed;
        w.active.bg_stroke = Stroke::new(1.0, self.accent);
        w.active.fg_stroke = Stroke::new(2.0, self.text_strong);
        w.active.corner_radius = r;

        w.open.bg_fill = self.hover;
        w.open.weak_bg_fill = self.hover;
        w.open.bg_stroke = Stroke::new(1.0, self.border);
        w.open.fg_stroke = Stroke::new(1.0, self.text);
        w.open.corner_radius = r;
        v
    }

    /// Categorical colours for chart series (and pie slices), in a fixed order: series `i`
    /// always gets slot `i`. Text never uses these; labels stay in the text tokens.
    pub fn chart_palette(&self) -> [Color32; 8] {
        self.chart
    }
}

/// A busy indicator: egui's spinner, or a still ellipsis when motion is reduced.
pub fn spinner(ui: &mut egui::Ui, t: &Tokens, size: Option<f32>) -> egui::Response {
    if t.knobs.reduce_motion {
        let text = egui::RichText::new("…").color(t.text_dim);
        ui.label(match size {
            Some(s) => text.size(s),
            None => text,
        })
    } else {
        match size {
            Some(s) => ui.add(egui::Spinner::new().size(s)),
            None => ui.spinner(),
        }
    }
}
