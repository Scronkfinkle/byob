//! Per-device appearance preferences and their storage format.
//!
//! Stored by the apps with eframe storage (RON in a file natively, `localStorage` in the
//! browser). Every field has a default, so older or hand-edited storage never breaks
//! startup; [`Prefs::sanitized`] clamps hostile values and migrates the version-0 format
//! (`{theme, accent, font_scale, high_contrast}`) without losing anything.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::presets;

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

/// The accent colour. `Theme` uses the theme's own; `Named` one of the theme's named
/// accents (`mauve`); `Custom` any `#rrggbb`. The six fixed colours are byob's original
/// accents, kept so version-0 preferences still mean the same colour.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Accent {
    #[default]
    Theme,
    Blue,
    Violet,
    Green,
    Orange,
    Pink,
    Teal,
    Named(String),
    Custom(String),
}

impl Accent {
    /// byob's six fixed accents (dark and light shades, ≥ 4.5:1 on byob's surfaces).
    pub const FIXED: [Accent; 6] = [Accent::Blue, Accent::Violet, Accent::Green, Accent::Orange, Accent::Pink, Accent::Teal];

    pub fn label(&self) -> String {
        match self {
            Accent::Theme => "Theme".into(),
            Accent::Blue => "Blue".into(),
            Accent::Violet => "Violet".into(),
            Accent::Green => "Green".into(),
            Accent::Orange => "Orange".into(),
            Accent::Pink => "Pink".into(),
            Accent::Teal => "Teal".into(),
            Accent::Named(n) => n.clone(),
            Accent::Custom(c) => c.clone(),
        }
    }

    /// The colour of a fixed accent on a dark or light theme.
    pub fn fixed_color(&self, dark: bool) -> Option<u32> {
        let p = if dark { &presets::BYOB_DARK } else { &presets::BYOB_LIGHT };
        let name = match self {
            Accent::Blue => "blue",
            Accent::Violet => "violet",
            Accent::Green => "green",
            Accent::Orange => "orange",
            Accent::Pink => "pink",
            Accent::Teal => "teal",
            _ => return None,
        };
        p.accent(name)
    }
}

/// How much room widgets get.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Density {
    Compact,
    #[default]
    Comfortable,
    Spacious,
}

impl Density {
    pub const ALL: [Density; 3] = [Density::Compact, Density::Comfortable, Density::Spacious];

    pub fn label(self) -> &'static str {
        match self {
            Density::Compact => "Compact",
            Density::Comfortable => "Comfortable",
            Density::Spacious => "Spacious",
        }
    }

    /// 0.7 / 1 / 1.4: multiplies spacing and padding.
    pub fn factor(self) -> f32 {
        match self {
            Density::Compact => 0.7,
            Density::Comfortable => 1.0,
            Density::Spacious => 1.4,
        }
    }
}

/// How a message is set off from the transcript.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum BubbleStyle {
    #[default]
    Filled,
    Outlined,
    /// No box: the name and the text only.
    Minimal,
}

impl BubbleStyle {
    pub const ALL: [BubbleStyle; 3] = [BubbleStyle::Filled, BubbleStyle::Outlined, BubbleStyle::Minimal];

    pub fn label(self) -> &'static str {
        match self {
            BubbleStyle::Filled => "Filled",
            BubbleStyle::Outlined => "Outlined",
            BubbleStyle::Minimal => "Minimal",
        }
    }
}

/// Where messages sit in the transcript column.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Align {
    /// Every message full width.
    #[default]
    Stacked,
    /// Chat style: your messages on the right, narrower.
    Chat,
}

impl Align {
    pub const ALL: [Align; 2] = [Align::Stacked, Align::Chat];

    pub fn label(self) -> &'static str {
        match self {
            Align::Stacked => "Stacked",
            Align::Chat => "Chat (yours right)",
        }
    }
}

/// UI typeface.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum UiFont {
    /// Inter (bundled).
    #[default]
    Inter,
    /// egui's built-in Ubuntu Light.
    Ubuntu,
}

impl UiFont {
    pub const ALL: [UiFont; 2] = [UiFont::Inter, UiFont::Ubuntu];

    pub fn label(self) -> &'static str {
        match self {
            UiFont::Inter => "Inter",
            UiFont::Ubuntu => "Ubuntu Light",
        }
    }
}

/// Code typeface.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum MonoFont {
    /// egui's built-in Hack.
    #[default]
    Hack,
    /// JetBrains Mono (bundled).
    JetBrainsMono,
}

impl MonoFont {
    pub const ALL: [MonoFont; 2] = [MonoFont::Hack, MonoFont::JetBrainsMono];

    pub fn label(self) -> &'static str {
        match self {
            MonoFont::Hack => "Hack",
            MonoFont::JetBrainsMono => "JetBrains Mono",
        }
    }
}

/// Where chart series colours come from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChartPalette {
    /// The theme's own colours.
    #[default]
    Theme,
    /// byob's colour-vision-deficiency-checked palette.
    Accessible,
}

impl ChartPalette {
    pub const ALL: [ChartPalette; 2] = [ChartPalette::Theme, ChartPalette::Accessible];

    pub fn label(self) -> &'static str {
        match self {
            ChartPalette::Theme => "Theme",
            ChartPalette::Accessible => "Colour-blind safe",
        }
    }
}

/// Everything about the look that isn't a colour. Also what a theme file's `knobs` sets.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Knobs {
    /// UI text size multiplier, see [`Knobs::FONT_SCALES`].
    pub font_scale: f32,
    /// Code text size multiplier (code blocks, inline code, tool output).
    pub code_scale: f32,
    pub ui_font: UiFont,
    pub mono_font: MonoFont,
    pub density: Density,
    /// Base corner radius in points (0 = square).
    pub radius: f32,
    pub user_bubble: BubbleStyle,
    pub assistant_bubble: BubbleStyle,
    pub align: Align,
    /// Widest the transcript column gets, in points; 0 = the whole window.
    pub max_width: f32,
    /// Brighter secondary text and borders.
    pub high_contrast: bool,
    /// No spinners, animations or smooth scrolling.
    pub reduce_motion: bool,
    pub chart: ChartPalette,
    /// Starting width of the chat list, in points.
    pub sidebar_width: f32,
    /// Starting share of the window the artifact pane takes.
    pub pane_share: f32,
}

impl Default for Knobs {
    fn default() -> Self {
        Self {
            font_scale: 1.0,
            code_scale: 1.0,
            ui_font: UiFont::Inter,
            mono_font: MonoFont::Hack,
            density: Density::Comfortable,
            radius: 6.0,
            user_bubble: BubbleStyle::Filled,
            assistant_bubble: BubbleStyle::Filled,
            align: Align::Stacked,
            max_width: 0.0,
            high_contrast: false,
            reduce_motion: false,
            chart: ChartPalette::Theme,
            sidebar_width: 240.0,
            pane_share: 0.48,
        }
    }
}

impl Knobs {
    pub const FONT_SCALES: [(f32, &'static str); 5] = [(0.85, "S"), (1.0, "M"), (1.15, "L"), (1.3, "XL"), (1.5, "XXL")];
    pub const SCALE_RANGE: std::ops::RangeInclusive<f32> = 0.75..=2.0;
    pub const RADIUS_RANGE: std::ops::RangeInclusive<f32> = 0.0..=16.0;
    pub const SIDEBAR_RANGE: std::ops::RangeInclusive<f32> = 180.0..=420.0;
    pub const PANE_RANGE: std::ops::RangeInclusive<f32> = 0.3..=0.75;
    /// Readable column widths offered (0 = full width); anything 480..=2000 is accepted.
    pub const WIDTHS: [(f32, &'static str); 4] = [(0.0, "Full"), (720.0, "Narrow"), (880.0, "Medium"), (1080.0, "Wide")];

    /// Clamp values that may come from untrusted storage or a theme file.
    #[must_use]
    pub fn sanitized(mut self) -> Self {
        let clamp = |v: f32, r: &std::ops::RangeInclusive<f32>, d: f32| if v.is_finite() { v.clamp(*r.start(), *r.end()) } else { d };
        let d = Knobs::default();
        self.font_scale = clamp(self.font_scale, &Self::SCALE_RANGE, d.font_scale);
        self.code_scale = clamp(self.code_scale, &Self::SCALE_RANGE, d.code_scale);
        self.radius = clamp(self.radius, &Self::RADIUS_RANGE, d.radius);
        self.sidebar_width = clamp(self.sidebar_width, &Self::SIDEBAR_RANGE, d.sidebar_width);
        self.pane_share = clamp(self.pane_share, &Self::PANE_RANGE, d.pane_share);
        self.max_width = if self.max_width.is_finite() && self.max_width >= 480.0 { self.max_width.min(2000.0) } else { 0.0 };
        self
    }
}

/// Per-device appearance preferences.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// Storage format version; 0 (absent) is the original `{theme, accent, font_scale,
    /// high_contrast}` struct.
    pub version: u32,
    pub theme: ThemeKind,
    /// Theme id used when dark (`Dark`, or `System` with a dark OS).
    pub dark_theme: String,
    /// Theme id used when light.
    pub light_theme: String,
    pub accent: Accent,
    pub knobs: Knobs,
    /// Per-theme colour overrides: theme id → role name → `#rrggbb`.
    pub overrides: BTreeMap<String, BTreeMap<String, String>>,
    /// Theme files imported in the browser (their text; desktop keeps them in the themes folder).
    pub imported: Vec<String>,
    /// Version 0's text size; moved into `knobs` by [`Prefs::sanitized`].
    #[serde(skip_serializing, deserialize_with = "present")]
    pub font_scale: Option<f32>,
    /// Version 0's contrast switch; moved into `knobs` by [`Prefs::sanitized`].
    #[serde(skip_serializing, deserialize_with = "present")]
    pub high_contrast: Option<bool>,
}

/// A field that was written as a plain value (RON would otherwise want `Some(…)`).
fn present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            version: Self::VERSION,
            theme: ThemeKind::System,
            dark_theme: presets::BYOB_DARK.id.into(),
            light_theme: presets::BYOB_LIGHT.id.into(),
            accent: Accent::Theme,
            knobs: Knobs::default(),
            overrides: BTreeMap::new(),
            imported: Vec::new(),
            font_scale: None,
            high_contrast: None,
        }
    }
}

impl Prefs {
    /// Storage key used by the apps.
    pub const STORAGE_KEY: &'static str = "byob.prefs";
    pub const VERSION: u32 = 1;
    /// Most imported themes kept in browser storage, and the largest each may be.
    pub const MAX_IMPORTED: usize = 32;
    pub const MAX_THEME_BYTES: usize = 64 * 1024;
    /// Overrides kept per theme (there are fewer roles than this).
    const MAX_OVERRIDES: usize = 128;

    /// Migrate older formats and clamp values that may come from untrusted storage.
    #[must_use]
    pub fn sanitized(mut self) -> Self {
        // Version 0 kept these at the top level. A version-1 file never has them.
        if let Some(scale) = self.font_scale.take() {
            self.knobs.font_scale = scale;
        }
        if let Some(hc) = self.high_contrast.take() {
            self.knobs.high_contrast = hc;
        }
        if self.version < Self::VERSION {
            // Version 0 had no theme ids: byob's own pair, which is what it drew.
            self.version = Self::VERSION;
        }
        self.knobs = self.knobs.sanitized();
        for id in [&mut self.dark_theme, &mut self.light_theme] {
            if id.is_empty() || id.len() > 120 {
                id.clear();
            }
        }
        if self.dark_theme.is_empty() {
            self.dark_theme = presets::BYOB_DARK.id.into();
        }
        if self.light_theme.is_empty() {
            self.light_theme = presets::BYOB_LIGHT.id.into();
        }
        if let Accent::Custom(hex) = &self.accent
            && super::parse_color(hex).is_none()
        {
            self.accent = Accent::Theme;
        }
        self.overrides.retain(|id, roles| {
            roles.retain(|role, hex| super::Tokens::has_role(role) && super::parse_color(hex).is_some());
            id.len() <= 120 && !roles.is_empty() && roles.len() <= Self::MAX_OVERRIDES
        });
        self.imported.retain(|text| text.len() <= Self::MAX_THEME_BYTES);
        self.imported.truncate(Self::MAX_IMPORTED);
        self
    }

    /// The theme id in use for a dark or light window.
    pub fn theme_id(&self, dark: bool) -> &str {
        if dark { &self.dark_theme } else { &self.light_theme }
    }

    /// Overrides for one theme.
    pub fn overrides_for(&self, id: &str) -> Option<&BTreeMap<String, String>> {
        self.overrides.get(id)
    }
}
