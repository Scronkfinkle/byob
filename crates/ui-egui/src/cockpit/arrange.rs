//! Free arrangement: the window's parts as modules placed anywhere, each in a bezel, with
//! open space between them for the backdrop. The parts are the same ones the docked
//! layout draws (the header, chat list, chat, message box, artifact pane and plugin pane);
//! only where they are and what frames them changes.
//!
//! Places are in % of the window, so an arrangement keeps its shape when the window is
//! resized; a module is never drawn smaller than its parts need ([`Module::min_size`]).

use std::collections::BTreeMap;

use egui::{Color32, CornerRadius, Pos2, Rect, Stroke, StrokeKind, Vec2, pos2, vec2};
use serde::{Deserialize, Serialize};

use crate::theme::{Tokens, mix};

/// A part of the window that can be placed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Module {
    Chats,
    Chat,
    Composer,
    Pane,
    Plugins,
    Header,
}

impl Module {
    /// In drawing order: later ones are on top where they overlap.
    pub const ALL: [Self; 6] = [Self::Chats, Self::Chat, Self::Composer, Self::Pane, Self::Plugins, Self::Header];

    pub fn id(self) -> &'static str {
        match self {
            Self::Header => "header",
            Self::Chats => "chats",
            Self::Chat => "chat",
            Self::Composer => "composer",
            Self::Pane => "pane",
            Self::Plugins => "plugin_pane",
        }
    }

    pub fn parse(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.id() == id)
    }

    /// What it is, for people and the model.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Header => "the header (chat title, model chip, context meter, the ⋯ menu)",
            Self::Chats => "the chat list (New chat, search, workspaces, Settings)",
            Self::Chat => "the conversation",
            Self::Composer => "the message box",
            Self::Pane => "the artifact pane (shown while an artifact is open)",
            Self::Plugins => "plugins' pane (shown while one is open)",
        }
    }

    /// The name on its handle while arranging.
    pub fn label(self) -> &'static str {
        match self {
            Self::Header => "Header",
            Self::Chats => "Chat list",
            Self::Chat => "Chat",
            Self::Composer => "Message box",
            Self::Pane => "Artifacts",
            Self::Plugins => "Plugin pane",
        }
    }

    /// The least it's drawn at, in points (its parts need that much).
    pub fn min_size(self) -> Vec2 {
        match self {
            Self::Header => vec2(360.0, 40.0),
            Self::Composer => vec2(260.0, 92.0),
            Self::Chats => vec2(180.0, 160.0),
            _ => vec2(220.0, 140.0),
        }
    }
}

/// Where a module is: a rectangle in % of the window, or not drawn.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Spot {
    /// Left, top, width, height in % of the window.
    At([f32; 4]),
    Hidden,
}

impl Spot {
    /// `"23.5 42 53 44"` (left top width height, in %; commas and % signs are fine) or `hidden`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let t = text.trim().to_ascii_lowercase();
        if matches!(t.as_str(), "hidden" | "hide" | "off" | "none") {
            return Ok(Self::Hidden);
        }
        let nums: Vec<f32> = t
            .split(|c: char| c.is_whitespace() || c == ',' || c == '%')
            .filter(|s| !s.is_empty())
            .map(str::parse)
            .collect::<Result<_, _>>()
            .map_err(|_| format!("{text:?} isn't four numbers"))?;
        let [x, y, w, h] = nums[..] else {
            return Err(format!("{text:?}: four numbers, left top width height in % of the window"));
        };
        if [x, y, w, h].iter().any(|v| !v.is_finite()) || !(0.0..=95.0).contains(&x) || !(0.0..=95.0).contains(&y) || w < 5.0 || h < 3.0 {
            return Err(format!("{text:?}: left and top from 0 to 95, width at least 5 and height at least 3"));
        }
        Ok(Self::At([x, y, w.min(100.0 - x), h.min(100.0 - y)]))
    }
}

impl std::fmt::Display for Spot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Hidden => f.write_str("hidden"),
            Self::At(v) => {
                let n = |x: f32| {
                    let r = (x * 10.0).round() / 10.0;
                    if r.fract() == 0.0 { format!("{r:.0}") } else { format!("{r:.1}") }
                };
                write!(f, "{} {} {} {}", n(v[0]), n(v[1]), n(v[2]), n(v[3]))
            }
        }
    }
}

impl TryFrom<String> for Spot {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        Self::parse(&s)
    }
}

impl From<Spot> for String {
    fn from(s: Spot) -> Self {
        s.to_string()
    }
}

/// How a module is framed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Bezel {
    /// A rounded pane of the panel color (glass over a backdrop).
    #[default]
    Glass,
    /// A beveled metal plate with screws, its label stamped on it, and the part on a lit
    /// screen inside. ("instrument" in arrangements saved by 0.3.10.)
    #[serde(alias = "instrument")]
    Metal,
    /// Nothing: the part straight on the backdrop.
    /// Its label, if it has one, is still shown: over the part, on the backdrop.
    Bare,
}

impl Bezel {
    pub const ALL: [Self; 3] = [Self::Glass, Self::Metal, Self::Bare];

    pub fn name(self) -> &'static str {
        match self {
            Self::Glass => "glass",
            Self::Metal => "metal",
            Self::Bare => "bare",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        let s = if s == "instrument" { "metal".to_owned() } else { s };
        Self::ALL.into_iter().find(|b| b.name() == s)
    }

    /// The room it takes around the part (labeled or not), as [`paint`] draws it.
    pub fn overhead(self, labeled: bool) -> Vec2 {
        let label = if labeled { LABEL_HEIGHT } else { 0.0 };
        match self {
            Self::Bare => vec2(0.0, label),
            Self::Glass => vec2(12.0, 12.0 + label),
            Self::Metal => vec2(32.0, 34.0 + if labeled { LABEL_HEIGHT - 12.0 + 2.0 } else { 0.0 }),
        }
    }
}

/// About how tall a stamped label is (its text and margin).
const LABEL_HEIGHT: f32 = 17.0;

/// Longest label on a bezel.
pub const MAX_LABEL: usize = 24;

/// Where the modules are, when free.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Arrangement {
    /// Free (modules where `places` says) rather than docked (byob's usual panels).
    pub free: bool,
    /// By module id; one not here is where [`Arrangement::FLOATING`] has it.
    pub places: BTreeMap<String, Spot>,
    pub bezels: BTreeMap<String, Bezel>,
    pub labels: BTreeMap<String, String>,
}

/// A bundled arrangement: (module, place, bezel, label) for each module.
type Preset = [(Module, Spot, Bezel, &'static str); 6];

impl Arrangement {
    /// The docked layout's shape, floating: everything where it usually is, apart.
    const FLOATING: Preset = [
        (Module::Header, Spot::At([1.0, 1.0, 98.0, 6.0]), Bezel::Glass, ""),
        (Module::Chats, Spot::At([1.0, 8.5, 20.0, 90.5]), Bezel::Glass, ""),
        (Module::Chat, Spot::At([22.0, 8.5, 77.0, 77.0]), Bezel::Glass, ""),
        (Module::Composer, Spot::At([22.0, 86.5, 77.0, 12.5]), Bezel::Glass, ""),
        (Module::Pane, Spot::At([60.0, 8.5, 39.0, 90.5]), Bezel::Glass, ""),
        (Module::Plugins, Spot::At([66.0, 8.5, 33.0, 90.5]), Bezel::Glass, ""),
    ];

    /// One column in the middle, nothing else but the header.
    const FOCUS: Preset = [
        (Module::Header, Spot::At([20.0, 2.0, 60.0, 6.0]), Bezel::Glass, ""),
        (Module::Chats, Spot::Hidden, Bezel::Glass, ""),
        (Module::Chat, Spot::At([20.0, 10.0, 60.0, 72.0]), Bezel::Bare, ""),
        (Module::Composer, Spot::At([20.0, 84.0, 60.0, 13.0]), Bezel::Glass, ""),
        (Module::Pane, Spot::At([62.0, 10.0, 36.0, 87.0]), Bezel::Glass, ""),
        (Module::Plugins, Spot::At([62.0, 10.0, 36.0, 87.0]), Bezel::Glass, ""),
    ];

    pub const PRESETS: [(&'static str, &'static str); 2] =
        [("floating", "byob's usual layout with space between the parts"), ("focus", "the chat alone in the middle")];

    /// A bundled arrangement by name (free).
    pub fn preset(name: &str) -> Option<Self> {
        let p = match name.trim().to_ascii_lowercase().as_str() {
            "floating" | "free" => Self::FLOATING,
            "focus" => Self::FOCUS,
            _ => return None,
        };
        Some(Self {
            free: true,
            places: p.iter().map(|(m, s, _, _)| (m.id().to_owned(), *s)).collect(),
            bezels: p.iter().map(|(m, _, b, _)| (m.id().to_owned(), *b)).collect(),
            labels: p.iter().filter(|(_, _, _, l)| !l.is_empty()).map(|(m, _, _, l)| (m.id().to_owned(), (*l).to_owned())).collect(),
        })
    }

    pub fn spot(&self, m: Module) -> Spot {
        self.places.get(m.id()).copied().unwrap_or_else(|| Self::FLOATING.iter().find(|p| p.0 == m).map_or(Spot::Hidden, |p| p.1))
    }

    pub fn bezel(&self, m: Module) -> Bezel {
        self.bezels.get(m.id()).copied().unwrap_or_default()
    }

    pub fn label(&self, m: Module) -> &str {
        self.labels.get(m.id()).map_or("", String::as_str)
    }

    /// Where `m` is drawn in `screen`: its place, grown to its least size, kept on screen.
    pub fn rect(&self, m: Module, screen: Rect) -> Option<Rect> {
        let Spot::At([x, y, w, h]) = self.spot(m) else {
            return None;
        };
        let at = |fx: f32, fy: f32| pos2(screen.left() + screen.width() * fx / 100.0, screen.top() + screen.height() * fy / 100.0);
        let r = Rect::from_min_max(at(x, y), at(x + w, y + h));
        let least = m.min_size() + self.bezel(m).overhead(!self.label(m).is_empty());
        let size = r.size().max(least).min(screen.size());
        let min = r.min.min(screen.max - size).max(screen.min);
        Some(Rect::from_min_size(min, size))
    }

    /// Set `m`'s place from a rectangle drawn in `screen` (dragging), snapped to 0.5 %.
    pub fn place(&mut self, m: Module, rect: Rect, screen: Rect) {
        let pct = |v: f32, from: f32, span: f32| (((v - from) / span.max(1.0)) * 200.0).round() / 2.0;
        let x = pct(rect.left(), screen.left(), screen.width()).clamp(0.0, 95.0);
        let y = pct(rect.top(), screen.top(), screen.height()).clamp(0.0, 95.0);
        let w = pct(rect.right(), screen.left(), screen.width()) - x;
        let h = pct(rect.bottom(), screen.top(), screen.height()) - y;
        self.places.insert(m.id().to_owned(), Spot::At([x, y, w.clamp(5.0, 100.0 - x), h.clamp(3.0, 100.0 - y)]));
    }

    /// Clamp what may come from storage.
    #[must_use]
    pub fn sanitized(mut self) -> Self {
        self.places.retain(|k, _| Module::parse(k).is_some());
        self.bezels.retain(|k, _| Module::parse(k).is_some());
        self.labels.retain(|k, l| Module::parse(k).is_some() && !l.trim().is_empty());
        for l in self.labels.values_mut() {
            *l = l.chars().filter(|c| !c.is_control()).take(MAX_LABEL).collect();
        }
        self
    }
}

/// Draw `bezel` around `rect` (labeled `label`), returning where the part goes inside.
pub fn paint(painter: &egui::Painter, rect: Rect, bezel: Bezel, label: &str, t: &Tokens) -> Rect {
    let r = t.round(10.0).round().clamp(0.0, 24.0);
    match bezel {
        // No frame, but a label asked for is still shown (it used to vanish silently).
        Bezel::Bare if label.is_empty() => rect,
        Bezel::Bare => stamp(painter, rect.translate(vec2(-12.0, -2.0)), label, t.text_dim, t).1.translate(vec2(12.0, 2.0)).intersect(rect),
        Bezel::Glass => {
            painter.rect(rect, CornerRadius::same(r as u8), t.panel_bg, Stroke::new(1.0, t.border.gamma_multiply(0.7)), StrokeKind::Inside);
            let inner = rect.shrink(6.0);
            if label.is_empty() { inner } else { stamp(painter, rect, label, t.text_dim, t).1.intersect(inner) }
        }
        Bezel::Metal => metal(painter, rect, label, t),
    }
}

/// The label in small capitals along the top edge; returns its height and what's below it.
fn stamp(painter: &egui::Painter, rect: Rect, label: &str, color: Color32, t: &Tokens) -> (f32, Rect) {
    let font = egui::FontId::monospace(t.text_size(9.5));
    let galley = painter.layout_no_wrap(label.to_uppercase(), font, color);
    let h = galley.size().y + 6.0;
    painter.galley(pos2(rect.left() + 14.0, rect.top() + 4.0), galley, color);
    (h, Rect::from_min_max(pos2(rect.left(), rect.top() + h), rect.max))
}

/// A metal plate: bevel, screws, stamped label, lit screen.
fn metal(painter: &egui::Painter, rect: Rect, label: &str, t: &Tokens) -> Rect {
    let plate = mix(t.panel_bg.to_opaque(), Color32::from_rgb(40, 42, 46), 0.55);
    let r = CornerRadius::same(t.round(6.0).round().clamp(2.0, 12.0) as u8);
    painter.rect_filled(rect, r, plate);
    // Bevel: light along the top and left, shade along the bottom and right.
    let light = Stroke::new(1.5, Color32::from_white_alpha(28));
    let shade = Stroke::new(1.5, Color32::from_black_alpha(120));
    painter.line_segment([rect.left_top() + vec2(4.0, 1.0), rect.right_top() + vec2(-4.0, 1.0)], light);
    painter.line_segment([rect.left_top() + vec2(1.0, 4.0), rect.left_bottom() + vec2(1.0, -4.0)], light);
    painter.line_segment([rect.left_bottom() + vec2(4.0, -1.0), rect.right_bottom() + vec2(-4.0, -1.0)], shade);
    painter.line_segment([rect.right_top() + vec2(-1.0, 4.0), rect.right_bottom() + vec2(-1.0, -4.0)], shade);
    for c in
        [rect.left_top() + vec2(6.0, 6.0), rect.right_top() + vec2(-6.0, 6.0), rect.left_bottom() + vec2(6.0, -6.0), rect.right_bottom() + vec2(-6.0, -6.0)]
    {
        screw(painter, c);
    }
    let (top, below) = if label.is_empty() { (12.0, rect) } else { stamp(painter, rect.shrink2(vec2(6.0, 0.0)), label, t.accent.gamma_multiply(0.85), t) };
    let screen = Rect::from_min_max(pos2(rect.left() + 12.0, below.top().max(rect.top() + top) + 2.0), rect.max - vec2(12.0, 12.0));
    // The screen: sunk into the plate, lit at its rim.
    painter.rect(screen.expand(2.0), r, Color32::from_black_alpha(160), Stroke::NONE, StrokeKind::Outside);
    painter.rect(screen, r, t.app_bg, Stroke::new(1.0, t.accent.gamma_multiply(0.35)), StrokeKind::Outside);
    screen.shrink(4.0)
}

fn screw(painter: &egui::Painter, at: Pos2) {
    painter.circle(at, 2.6, Color32::from_gray(70), Stroke::new(0.8, Color32::from_black_alpha(140)));
    painter.line_segment([at + vec2(-1.6, 1.0), at + vec2(1.6, -1.0)], Stroke::new(0.8, Color32::from_black_alpha(160)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places_read_and_write_as_four_percentages() {
        assert_eq!(Spot::parse("23.5 42 53 44"), Ok(Spot::At([23.5, 42.0, 53.0, 44.0])));
        assert_eq!(Spot::parse("10%, 10%, 95%, 50%"), Ok(Spot::At([10.0, 10.0, 90.0, 50.0])), "kept inside the window");
        assert_eq!(Spot::parse(" Hidden "), Ok(Spot::Hidden));
        for bad in ["1 2 3", "a b c d", "-1 0 50 50", "0 0 2 50", "nan 0 50 50"] {
            assert!(Spot::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(Spot::At([23.5, 42.0, 53.0, 44.0]).to_string(), "23.5 42 53 44");
        let json = serde_json::to_string(&Spot::At([1.0, 2.0, 3.5, 40.0])).unwrap();
        assert_eq!(json, "\"1 2 3.5 40\"");
        assert!(serde_json::from_str::<Spot>("\"1 2\"").is_err());
    }

    #[test]
    fn the_metal_bezel_reads_its_old_name() {
        // 0.3.10 called it "instrument" (a cockpit instrument); saved arrangements keep working.
        assert_eq!(serde_json::from_str::<Bezel>("\"instrument\"").unwrap(), Bezel::Metal);
        assert_eq!(Bezel::parse(" Instrument "), Some(Bezel::Metal));
        assert_eq!(serde_json::to_string(&Bezel::Metal).unwrap(), "\"metal\"");
        assert_eq!(Bezel::parse("metal"), Some(Bezel::Metal));
    }

    #[test]
    fn modules_keep_their_least_size_and_stay_on_screen() {
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 800.0));
        let mut a = Arrangement::preset("floating").unwrap();
        a.places.insert("chat".into(), Spot::At([22.0, 36.0, 56.0, 48.0]));
        assert_eq!(a.rect(Module::Chat, screen), Some(Rect::from_min_size(pos2(220.0, 288.0), vec2(560.0, 384.0))));
        a.places.insert("header".into(), Spot::At([90.0, 95.0, 5.0, 3.0]));
        a.bezels.insert("header".into(), Bezel::Metal);
        a.labels.insert("header".into(), "DECK".into());
        let h = a.rect(Module::Header, screen).unwrap();
        assert!(screen.contains_rect(h) && h.width() >= 360.0 + 32.0 && h.height() >= 40.0 + 34.0, "room for the header and its metal bezel: {h:?}");
        assert_eq!(Arrangement::preset("focus").unwrap().rect(Module::Chats, screen), None);
    }

    #[test]
    fn dragging_snaps_to_half_percents() {
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 800.0));
        let mut a = Arrangement::default();
        a.place(Module::Chat, Rect::from_min_size(pos2(101.0, 83.0), vec2(503.0, 401.0)), screen);
        assert_eq!(a.spot(Module::Chat), Spot::At([10.0, 10.5, 50.5, 50.0]));
        assert_eq!(a.spot(Module::Composer), Arrangement::FLOATING[3].1, "the rest float where byob has them");
    }

    /// Regression: a label on a bare bezel (the focus preset's chat, labeled) was never drawn.
    #[test]
    fn a_bare_bezel_still_shows_its_label() {
        let ctx = egui::Context::default();
        let t = Tokens::dark();
        let rect = Rect::from_min_size(pos2(100.0, 100.0), vec2(400.0, 300.0));
        let draw = |label: &'static str| {
            let mut inner = Rect::NOTHING;
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| inner = paint(ui.painter(), rect, Bezel::Bare, label, &t));
            out.textures_delta.clear();
            (inner, out.shapes.len())
        };
        let ((plain, base), (labeled, with_label)) = (draw(""), draw("COMMS"));
        let drawn_labeled = with_label.saturating_sub(base);
        assert_eq!(plain, rect, "bare and unlabeled: the part fills it");
        assert!(drawn_labeled > 0, "the label is drawn");
        assert!(labeled.top() > rect.top() && labeled.bottom() == rect.bottom() && labeled.left() == rect.left(), "the part below its label: {labeled:?}");
        assert!(Bezel::Bare.overhead(true).y >= labeled.top() - rect.top(), "room is made for it");
    }

    #[test]
    fn stored_arrangements_drop_unknown_modules_and_cut_labels() {
        let mut a = Arrangement::preset("floating").unwrap();
        a.places.insert("warp_drive".into(), Spot::Hidden);
        a.labels.insert("chat".into(), "x".repeat(100));
        a.labels.insert("composer".into(), "  ".into());
        let a = a.sanitized();
        assert!(!a.places.contains_key("warp_drive"));
        assert_eq!(a.label(Module::Chat).len(), MAX_LABEL);
        assert_eq!(a.label(Module::Composer), "");
    }
}
