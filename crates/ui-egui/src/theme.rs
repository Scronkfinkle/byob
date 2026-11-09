//! Design tokens. Every widget reads colours from [`Tokens`] so themes apply everywhere.
//!
//! The appearance a user picks ([`Prefs`]: theme, accent, font size, contrast) is per device —
//! a phone can be light while the desktop is dark — so it lives client-side: the apps persist
//! it with eframe storage (a file natively, `localStorage` in the browser).

use std::sync::Arc;

use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Shadow, Stroke, TextStyle, Visuals};
use serde::{Deserialize, Serialize};

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

/// Install the UI fonts: Inter (OFL) for text and bold, egui's defaults (Hack mono, emoji)
/// as fallbacks. Call once per context.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert("inter".into(), Arc::new(FontData::from_static(include_bytes!("../assets/fonts/Inter-Regular.ttf"))));
    fonts.font_data.insert("inter-semibold".into(), Arc::new(FontData::from_static(include_bytes!("../assets/fonts/Inter-SemiBold.ttf"))));
    let fallbacks = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "inter".into());
    fonts.families.insert(bold_family(), std::iter::once("inter-semibold".to_owned()).chain(fallbacks).collect());
    ctx.set_fonts(fonts);
}

/// Light/dark choice. `System` follows the OS (desktop) or `prefers-color-scheme` (browser).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeKind {
    #[default]
    System,
    Dark,
    Light,
}

impl ThemeKind {
    pub const ALL: [ThemeKind; 3] = [ThemeKind::System, ThemeKind::Dark, ThemeKind::Light];

    pub fn label(self) -> &'static str {
        match self {
            ThemeKind::System => "System",
            ThemeKind::Dark => "Dark",
            ThemeKind::Light => "Light",
        }
    }

    /// Parse a CLI/config name (`system`, `dark`, `light`).
    pub fn from_name(s: &str) -> Option<ThemeKind> {
        match s.to_ascii_lowercase().as_str() {
            "system" | "auto" => Some(ThemeKind::System),
            "dark" => Some(ThemeKind::Dark),
            "light" => Some(ThemeKind::Light),
            _ => None,
        }
    }

    /// Whether to draw dark, given the OS/browser theme if known (unknown → dark).
    pub fn is_dark(self, system: Option<egui::Theme>) -> bool {
        match self {
            ThemeKind::Dark => true,
            ThemeKind::Light => false,
            ThemeKind::System => system != Some(egui::Theme::Light),
        }
    }
}

/// Accent colour presets. Each has a dark- and a light-theme shade, both readable as text on
/// that theme's surfaces (≥ 4.5:1, checked by the tests below).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Accent {
    #[default]
    Blue,
    Violet,
    Green,
    Orange,
    Pink,
    Teal,
}

impl Accent {
    pub const ALL: [Accent; 6] = [Accent::Blue, Accent::Violet, Accent::Green, Accent::Orange, Accent::Pink, Accent::Teal];

    pub fn label(self) -> &'static str {
        match self {
            Accent::Blue => "Blue",
            Accent::Violet => "Violet",
            Accent::Green => "Green",
            Accent::Orange => "Orange",
            Accent::Pink => "Pink",
            Accent::Teal => "Teal",
        }
    }

    pub fn color(self, dark: bool) -> Color32 {
        let (d, l) = match self {
            Accent::Blue => ([0x6c, 0x9e, 0xf8], [0x1f, 0x56, 0xc4]),
            Accent::Violet => ([0xa7, 0x8b, 0xfa], [0x65, 0x3f, 0xcc]),
            Accent::Green => ([0x4f, 0xc2, 0x8a], [0x16, 0x73, 0x47]),
            Accent::Orange => ([0xf5, 0x9e, 0x4c], [0xa3, 0x4a, 0x05]),
            Accent::Pink => ([0xf4, 0x72, 0xb6], [0xb0, 0x25, 0x6c]),
            Accent::Teal => ([0x2d, 0xd4, 0xbf], [0x0d, 0x6b, 0x63]),
        };
        let [r, g, b] = if dark { d } else { l };
        Color32::from_rgb(r, g, b)
    }
}

/// Per-device appearance preferences. Serialised by the apps; unknown/missing fields fall
/// back to defaults so old or hand-edited storage never breaks startup.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub theme: ThemeKind,
    pub accent: Accent,
    /// Text size multiplier, see [`Prefs::FONT_SCALES`].
    pub font_scale: f32,
    /// Brighter secondary text and borders.
    pub high_contrast: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self { theme: ThemeKind::System, accent: Accent::Blue, font_scale: 1.0, high_contrast: false }
    }
}

impl Prefs {
    /// Storage key used by the apps.
    pub const STORAGE_KEY: &'static str = "yah.prefs";
    pub const FONT_SCALES: [(f32, &'static str); 5] = [(0.85, "S"), (1.0, "M"), (1.15, "L"), (1.3, "XL"), (1.5, "XXL")];
    const MIN_SCALE: f32 = 0.75;
    const MAX_SCALE: f32 = 2.0;

    /// Clamp values that may come from untrusted storage.
    #[must_use]
    pub fn sanitized(mut self) -> Self {
        self.font_scale = if self.font_scale.is_finite() { self.font_scale.clamp(Self::MIN_SCALE, Self::MAX_SCALE) } else { 1.0 };
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tokens {
    pub dark: bool,
    /// Window background behind everything.
    pub app_bg: Color32,
    /// Header, composer and settings panels.
    pub panel_bg: Color32,
    /// Text inputs.
    pub field_bg: Color32,
    pub field_border: Color32,
    /// Outlines of frames, tables, buttons.
    pub border: Color32,
    pub separator: Color32,
    pub user_bubble: Color32,
    pub assistant_bubble: Color32,
    /// Fenced code blocks.
    pub code_bg: Color32,
    pub code_border: Color32,
    pub code_text: Color32,
    /// `inline code` spans.
    pub inline_code_bg: Color32,
    pub inline_code_text: Color32,
    pub accent: Color32,
    pub accent_hover: Color32,
    /// Text drawn on an `accent` fill.
    pub on_accent: Color32,
    pub link: Color32,
    pub text: Color32,
    /// Bold text and headings (egui's default fonts have no bold face).
    pub text_strong: Color32,
    pub text_dim: Color32,
    pub text_faint: Color32,
    pub danger: Color32,
    pub warning: Color32,
    pub success: Color32,
    pub button_bg: Color32,
    pub hover: Color32,
    pub pressed: Color32,
    pub selection: Color32,
    pub quote_bar: Color32,
    pub table_header_bg: Color32,
    pub table_stripe: Color32,
    pub shadow: Color32,
    pub radius: f32,
}

/// Linear blend in sRGB space: `t = 0` → `a`, `t = 1` → `b`.
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // clamped to 0..=255
    let ch = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round().clamp(0.0, 255.0) as u8;
    Color32::from_rgb(ch(a.r(), b.r()), ch(a.g(), b.g()), ch(a.b(), b.b()))
}

impl Tokens {
    pub fn dark() -> Self {
        Self::new(true, Accent::Blue, false)
    }

    pub fn light() -> Self {
        Self::new(false, Accent::Blue, false)
    }

    /// Tokens for a set of preferences, given the OS/browser theme if known.
    pub fn for_prefs(prefs: &Prefs, system: Option<egui::Theme>) -> Self {
        Self::new(prefs.theme.is_dark(system), prefs.accent, prefs.high_contrast)
    }

    pub fn new(dark: bool, accent: Accent, high_contrast: bool) -> Self {
        let rgb = |r, g, b| Color32::from_rgb(r, g, b);
        let a = accent.color(dark);
        let mut t = if dark {
            let panel = rgb(0x1c, 0x1d, 0x21);
            Tokens {
                dark,
                app_bg: rgb(0x16, 0x17, 0x1a),
                panel_bg: panel,
                field_bg: rgb(0x12, 0x13, 0x16),
                field_border: rgb(0x3a, 0x3c, 0x43),
                border: rgb(0x33, 0x35, 0x3b),
                separator: rgb(0x2c, 0x2e, 0x33),
                user_bubble: mix(rgb(0x1f, 0x21, 0x26), a, 0.13),
                assistant_bubble: rgb(0x22, 0x24, 0x29),
                code_bg: rgb(0x16, 0x17, 0x1b),
                code_border: rgb(0x31, 0x33, 0x3a),
                code_text: rgb(0xdc, 0xde, 0xe3),
                inline_code_bg: rgb(0x31, 0x34, 0x3c),
                inline_code_text: rgb(0xf0, 0xc6, 0x8a),
                accent: a,
                accent_hover: mix(a, Color32::WHITE, 0.15),
                on_accent: rgb(0x10, 0x11, 0x14),
                link: a,
                text: rgb(0xe4, 0xe5, 0xe8),
                text_strong: rgb(0xff, 0xff, 0xff),
                text_dim: rgb(0xa8, 0xab, 0xb2),
                text_faint: rgb(0x75, 0x78, 0x80),
                danger: rgb(0xf7, 0x85, 0x85),
                warning: rgb(0xe8, 0xb7, 0x5a),
                success: rgb(0x5c, 0xc9, 0x8a),
                button_bg: rgb(0x2b, 0x2d, 0x33),
                hover: rgb(0x34, 0x37, 0x3e),
                pressed: rgb(0x3e, 0x41, 0x49),
                selection: a.gamma_multiply(0.4),
                quote_bar: rgb(0x48, 0x4b, 0x54),
                table_header_bg: rgb(0x2a, 0x2c, 0x32),
                table_stripe: rgb(0x26, 0x28, 0x2d),
                shadow: Color32::from_black_alpha(96),
                radius: 6.0,
            }
        } else {
            Tokens {
                dark,
                app_bg: rgb(0xf6, 0xf6, 0xf4),
                panel_bg: rgb(0xef, 0xef, 0xec),
                field_bg: rgb(0xff, 0xff, 0xff),
                field_border: rgb(0xb8, 0xb9, 0xbe),
                border: rgb(0xd0, 0xd1, 0xd5),
                separator: rgb(0xda, 0xdb, 0xde),
                user_bubble: mix(Color32::WHITE, a, 0.10),
                assistant_bubble: rgb(0xff, 0xff, 0xff),
                code_bg: rgb(0xf3, 0xf4, 0xf6),
                code_border: rgb(0xd8, 0xda, 0xde),
                code_text: rgb(0x1f, 0x23, 0x28),
                inline_code_bg: rgb(0xe9, 0xeb, 0xef),
                inline_code_text: rgb(0x9a, 0x2f, 0x1f),
                accent: a,
                accent_hover: mix(a, Color32::BLACK, 0.12),
                on_accent: rgb(0xff, 0xff, 0xff),
                link: a,
                text: rgb(0x1d, 0x1e, 0x21),
                text_strong: rgb(0x00, 0x00, 0x00),
                text_dim: rgb(0x55, 0x58, 0x5f),
                text_faint: rgb(0x80, 0x83, 0x8a),
                danger: rgb(0xb4, 0x23, 0x23),
                warning: rgb(0x8a, 0x56, 0x00),
                success: rgb(0x1a, 0x6e, 0x3e),
                button_bg: rgb(0xe4, 0xe4, 0xe7),
                hover: rgb(0xd9, 0xd9, 0xde),
                pressed: rgb(0xcb, 0xcb, 0xd2),
                selection: a.gamma_multiply(0.28),
                quote_bar: rgb(0xc4, 0xc6, 0xcc),
                table_header_bg: rgb(0xee, 0xef, 0xf2),
                table_stripe: rgb(0xf7, 0xf7, 0xf9),
                shadow: Color32::from_black_alpha(40),
                radius: 6.0,
            }
        };
        if high_contrast {
            let (fg, bg) = if dark { (Color32::WHITE, Color32::BLACK) } else { (Color32::BLACK, Color32::WHITE) };
            t.text = mix(t.text, fg, 0.6);
            t.text_dim = mix(t.text_dim, fg, 0.45);
            t.text_faint = mix(t.text_faint, fg, 0.4);
            t.code_text = mix(t.code_text, fg, 0.6);
            t.border = mix(t.border, fg, 0.35);
            t.field_border = mix(t.field_border, fg, 0.4);
            t.code_border = mix(t.code_border, fg, 0.35);
            t.quote_bar = mix(t.quote_bar, fg, 0.35);
            t.app_bg = mix(t.app_bg, bg, 0.5);
            t.field_bg = mix(t.field_bg, bg, 0.5);
        }
        t
    }

    /// Push the tokens into egui's visuals and text styles (both egui themes, so nothing
    /// depends on which one egui thinks is active).
    pub fn apply(&self, ctx: &egui::Context, font_scale: f32) {
        let visuals = self.visuals();
        let scale = if font_scale.is_finite() { font_scale.clamp(0.5, 3.0) } else { 1.0 };
        ctx.all_styles_mut(|style| {
            style.visuals = visuals.clone();
            style.text_styles = [
                (TextStyle::Small, FontId::new(11.0 * scale, FontFamily::Proportional)),
                (TextStyle::Body, FontId::new(14.0 * scale, FontFamily::Proportional)),
                (TextStyle::Button, FontId::new(14.0 * scale, FontFamily::Proportional)),
                (TextStyle::Monospace, FontId::new(13.0 * scale, FontFamily::Monospace)),
                (TextStyle::Heading, FontId::new(20.0 * scale, FontFamily::Proportional)),
            ]
            .into();
            style.spacing.button_padding = egui::vec2(8.0, 4.0);
            style.spacing.item_spacing = egui::vec2(8.0, 5.0);
        });
        ctx.set_theme(if self.dark { egui::ThemePreference::Dark } else { egui::ThemePreference::Light });
    }

    pub fn visuals(&self) -> Visuals {
        let mut v = if self.dark { Visuals::dark() } else { Visuals::light() };
        let r = CornerRadius::same(5);
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
        v.window_corner_radius = CornerRadius::same(8);
        v.menu_corner_radius = CornerRadius::same(6);
        v.window_shadow = Shadow { offset: [0, 6], blur: 18, spread: 0, color: self.shadow };
        v.popup_shadow = Shadow { offset: [0, 3], blur: 10, spread: 0, color: self.shadow };
        v.selection.bg_fill = self.selection;
        v.selection.stroke = Stroke::new(1.0, self.text_strong);
        v.text_cursor.stroke = Stroke::new(2.0, self.accent);

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
}

impl Tokens {
    /// Categorical colours for chart series (and pie slices), in a fixed order: series `i`
    /// always gets slot `i`. Eight hues, each stepped separately for dark and light
    /// surfaces, ordered so neighbours stay apart under the common colour-vision
    /// deficiencies. Text never uses these; labels stay in the text tokens.
    pub fn chart_palette(&self) -> [Color32; 8] {
        let hex = |v: u32| Color32::from_rgb(((v >> 16) & 0xff) as u8, ((v >> 8) & 0xff) as u8, (v & 0xff) as u8);
        let slots: [u32; 8] = if self.dark {
            [0x3987e5, 0xd95926, 0x199e70, 0xc98500, 0xd55181, 0x008300, 0x9085e9, 0xe66767]
        } else {
            [0x2a78d6, 0xeb6834, 0x1baf7a, 0xeda100, 0xe87ba4, 0x008300, 0x6250d6, 0xe34948]
        };
        slots.map(hex)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn all_tokens() -> Vec<(String, Tokens)> {
        let mut out = Vec::new();
        for dark in [true, false] {
            for accent in Accent::ALL {
                for hc in [false, true] {
                    out.push((format!("dark={dark} {accent:?} hc={hc}"), Tokens::new(dark, accent, hc)));
                }
            }
        }
        out
    }

    #[test]
    fn body_text_and_accents_are_readable_on_every_surface() {
        for (name, t) in all_tokens() {
            for (surface, bg) in [("panel", t.panel_bg), ("user", t.user_bubble), ("assistant", t.assistant_bubble), ("field", t.field_bg)] {
                assert!(contrast(t.text, bg) >= 7.0, "{name}: text on {surface}");
                assert!(contrast(t.text_dim, bg) >= 4.5, "{name}: dim text on {surface}");
                assert!(contrast(t.accent, bg) >= 4.5, "{name}: accent on {surface}");
                assert!(contrast(t.danger, bg) >= 4.5, "{name}: danger on {surface}");
                assert!(contrast(t.warning, bg) >= 4.5, "{name}: warning on {surface}");
                assert!(contrast(t.success, bg) >= 4.5, "{name}: success on {surface}");
            }
            assert!(contrast(t.code_text, t.code_bg) >= 7.0, "{name}: code");
            assert!(contrast(t.inline_code_text, t.inline_code_bg) >= 4.5, "{name}: inline code");
            assert!(contrast(t.on_accent, t.accent) >= 4.5, "{name}: on accent");
        }
    }

    #[test]
    fn high_contrast_raises_secondary_text_contrast() {
        for dark in [true, false] {
            let (n, hc) = (Tokens::new(dark, Accent::Blue, false), Tokens::new(dark, Accent::Blue, true));
            assert!(contrast(hc.text_dim, hc.panel_bg) > contrast(n.text_dim, n.panel_bg));
            assert!(contrast(hc.border, hc.panel_bg) > contrast(n.border, n.panel_bg));
        }
    }

    #[test]
    fn system_theme_follows_the_os_and_defaults_to_dark() {
        assert!(ThemeKind::System.is_dark(None));
        assert!(ThemeKind::System.is_dark(Some(egui::Theme::Dark)));
        assert!(!ThemeKind::System.is_dark(Some(egui::Theme::Light)));
        assert!(!ThemeKind::Light.is_dark(Some(egui::Theme::Dark)));
        assert!(ThemeKind::Dark.is_dark(Some(egui::Theme::Light)));
    }

    #[test]
    fn hostile_font_scale_is_clamped() {
        for bad in [f32::NAN, f32::INFINITY, -3.0, 0.0, 1e9] {
            let p = Prefs { font_scale: bad, ..Prefs::default() }.sanitized();
            assert!(p.font_scale.is_finite() && (0.75..=2.0).contains(&p.font_scale), "{bad}");
        }
    }

    #[test]
    fn prefs_round_trip_and_tolerate_partial_storage() {
        let prefs = Prefs { theme: ThemeKind::Light, accent: Accent::Teal, font_scale: 1.3, high_contrast: true };
        let json = serde_json::to_string(&prefs).unwrap_or_default();
        assert_eq!(serde_json::from_str::<Prefs>(&json).ok(), Some(prefs));
        // Older storage without newer fields keeps defaults for them.
        let partial: Option<Prefs> = serde_json::from_str(r#"{"theme":"Dark"}"#).ok();
        assert_eq!(partial, Some(Prefs { theme: ThemeKind::Dark, ..Prefs::default() }));
    }

    #[test]
    fn apply_sets_the_egui_theme() {
        let ctx = egui::Context::default();
        Tokens::light().apply(&ctx, 1.0);
        assert_eq!(ctx.theme(), egui::Theme::Light);
        assert_eq!(ctx.style_of(egui::Theme::Light).visuals.panel_fill, Tokens::light().panel_bg);
        Tokens::dark().apply(&ctx, 1.2);
        assert_eq!(ctx.theme(), egui::Theme::Dark);
        assert!((ctx.style_of(egui::Theme::Dark).text_styles[&TextStyle::Body].size - 16.8).abs() < 0.01);
    }
}
