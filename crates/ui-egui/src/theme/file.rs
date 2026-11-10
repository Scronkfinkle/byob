//! Theme files: a small JSON document naming a base preset plus whatever it changes.
//!
//! ```json
//! {
//!   "byob_theme": 1,
//!   "name": "Mocha, warmer",
//!   "base": "catppuccin-mocha",
//!   "dark": true,
//!   "accent": "#fab387",
//!   "colors": { "app_bg": "#1b1b29", "syntax_keyword": "#f5c2e7" },
//!   "knobs": { "radius": 10, "density": "Compact" }
//! }
//! ```
//!
//! Everything but `name` is optional. Roles are the [`super::ROLES`] names; missing roles
//! come from the base preset. Parsing is forgiving — a bad colour, an unknown role or
//! knob becomes a warning and is skipped — and only a file that isn't a JSON object at all
//! is rejected. Desktop keeps user themes as `<config>/themes/*.json`; the browser keeps
//! imported ones in its preferences.

use egui::Color32;
use serde_json::{Map, Value};

use super::presets::{self, Palette};
use super::{Knobs, Prefs, Tokens, hex, parse_color};

/// Where a theme came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    Builtin,
    /// A file in the desktop themes folder (its file name).
    Folder(String),
    /// Imported in the browser: index into [`Prefs::imported`].
    Imported(usize),
}

/// A theme ready to build tokens from: a base palette plus a file's changes.
#[derive(Debug, Clone, PartialEq)]
pub struct ThemeDef {
    /// `catppuccin-mocha` for presets, `user:<slug>` for theme files.
    pub id: String,
    pub name: String,
    pub dark: bool,
    pub base: &'static Palette,
    /// The theme's own accent (what `Accent::Theme` means).
    pub accent: Option<Color32>,
    pub colors: Vec<(String, Color32)>,
    /// Knobs the file sets, applied when the theme is picked in the gallery.
    pub knobs: Option<Map<String, Value>>,
    pub origin: Origin,
}

impl ThemeDef {
    pub fn builtin(p: &'static Palette) -> Self {
        Self { id: p.id.into(), name: p.name.into(), dark: p.dark, base: p, accent: None, colors: Vec::new(), knobs: None, origin: Origin::Builtin }
    }

    pub fn is_builtin(&self) -> bool {
        self.origin == Origin::Builtin
    }

    /// Who made it, for the gallery tooltip.
    pub fn credit(&self) -> String {
        if self.is_builtin() { self.base.credit.to_owned() } else { format!("Your theme, based on {}", self.base.name) }
    }
}

/// The theme with this id among the user's themes and the presets; unknown ids fall back
/// to byob's own dark or light theme.
pub fn resolve(id: &str, dark: bool, user: &[ThemeDef]) -> ThemeDef {
    user.iter()
        .find(|d| d.id == id)
        .cloned()
        .or_else(|| presets::find(id).map(ThemeDef::builtin))
        .unwrap_or_else(|| ThemeDef::builtin(if dark { &presets::BYOB_DARK } else { &presets::BYOB_LIGHT }))
}

/// Use a theme by CLI-style name: `system`/`dark`/`light` set the mode; a preset id
/// (`catppuccin-mocha`) or user theme id/name selects that theme (with its knobs) and shows
/// it. `false` if nothing has that name.
pub fn select(prefs: &mut Prefs, name: &str, user: &[ThemeDef]) -> bool {
    if let Some(kind) = super::ThemeKind::from_name(name) {
        prefs.theme = kind;
        return true;
    }
    let wanted = name.trim();
    let found = user
        .iter()
        .find(|d| d.id == wanted || d.name.eq_ignore_ascii_case(wanted) || d.id == format!("user:{}", slug(wanted)))
        .cloned()
        .or_else(|| presets::find(wanted).map(ThemeDef::builtin));
    let Some(def) = found else { return false };
    if def.dark {
        prefs.dark_theme.clone_from(&def.id);
    } else {
        prefs.light_theme.clone_from(&def.id);
    }
    prefs.theme = if def.dark { super::ThemeKind::Dark } else { super::ThemeKind::Light };
    if let Some(k) = &def.knobs {
        apply_knobs(k, &mut prefs.knobs, &mut Vec::new());
    }
    true
}

/// Every theme, built-ins first.
pub fn all(user: &[ThemeDef]) -> Vec<ThemeDef> {
    presets::PRESETS.iter().map(|p| ThemeDef::builtin(p)).chain(user.iter().cloned()).collect()
}

/// A parsed theme file and what was wrong with it.
#[derive(Debug, Clone, PartialEq)]
pub struct ThemeFile {
    pub def: ThemeDef,
    pub warnings: Vec<String>,
}

/// `Mocha, warmer!` → `mocha-warmer`.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
        if out.len() >= 60 {
            break;
        }
    }
    let out = out.trim_end_matches('-').to_owned();
    if out.is_empty() { "theme".into() } else { out }
}

/// Parse a theme file. `Err` only when it isn't a JSON object; everything else that's
/// wrong is a warning and falls back to the base preset.
pub fn parse(text: &str, origin: Origin) -> Result<ThemeFile, String> {
    if text.len() > Prefs::MAX_THEME_BYTES {
        return Err(format!("theme file is too large ({} KiB, at most {} KiB)", text.len() / 1024, Prefs::MAX_THEME_BYTES / 1024));
    }
    let value: Value = serde_json::from_str(text).map_err(|e| format!("not a theme file (JSON): {e}"))?;
    let Value::Object(obj) = value else { return Err("not a theme file: expected a JSON object".into()) };
    let mut warnings = Vec::new();
    let name = match obj.get("name") {
        Some(Value::String(s)) if !s.trim().is_empty() => s.trim().chars().take(80).collect(),
        _ => {
            warnings.push("no \"name\"; called it \"Untitled theme\"".into());
            "Untitled theme".to_owned()
        }
    };
    let dark_flag = match obj.get("dark") {
        None => None,
        Some(Value::Bool(b)) => Some(*b),
        Some(other) => {
            warnings.push(format!("\"dark\" should be true or false, not {other}"));
            None
        }
    };
    let base = match obj.get("base") {
        None => None,
        Some(Value::String(id)) => {
            let found = presets::find(id);
            if found.is_none() {
                warnings.push(format!("unknown base theme {id:?}; using byob's own"));
            }
            found
        }
        Some(other) => {
            warnings.push(format!("\"base\" should be a theme id, not {other}"));
            None
        }
    };
    let base = base.unwrap_or(if dark_flag == Some(false) { &presets::BYOB_LIGHT } else { &presets::BYOB_DARK });
    let mut accent = match obj.get("accent") {
        None => None,
        Some(v) => color_value(v).or_else(|| {
            warnings.push(format!("\"accent\": {v} is not a colour (#rrggbb)"));
            None
        }),
    };
    let mut colors = Vec::new();
    match obj.get("colors") {
        None => {}
        Some(Value::Object(map)) => {
            for (role, v) in map {
                let Some(c) = color_value(v) else {
                    warnings.push(format!("colors.{role}: {v} is not a colour (#rrggbb)"));
                    continue;
                };
                if role == "accent" {
                    // The accent drives other roles (hover, selection, your bubbles).
                    accent = Some(c);
                } else if Tokens::has_role(role) {
                    colors.push((role.clone(), c));
                } else {
                    warnings.push(format!("colors.{role}: no such colour role"));
                }
            }
        }
        Some(other) => warnings.push(format!("\"colors\" should be an object of role → colour, not {other}")),
    }
    let knobs = match obj.get("knobs") {
        None => None,
        Some(Value::Object(map)) => {
            let mut probe = Knobs::default();
            Some(apply_knobs(map, &mut probe, &mut warnings))
        }
        Some(other) => {
            warnings.push(format!("\"knobs\" should be an object, not {other}"));
            None
        }
    };
    for key in obj.keys() {
        if !matches!(key.as_str(), "byob_theme" | "yah_theme" | "name" | "base" | "dark" | "accent" | "colors" | "knobs") {
            warnings.push(format!("ignored unknown field {key:?}"));
        }
    }
    let def = ThemeDef {
        id: format!("user:{}", slug(&name)),
        name,
        dark: dark_flag.unwrap_or(base.dark),
        base,
        accent,
        colors,
        knobs: knobs.filter(|k| !k.is_empty()),
        origin,
    };
    Ok(ThemeFile { def, warnings })
}

fn color_value(v: &Value) -> Option<Color32> {
    v.as_str().and_then(parse_color)
}

/// Set the knobs in `map` that are valid, one by one (a bad one doesn't spoil the rest).
/// Returns the valid ones.
pub fn apply_knobs(map: &Map<String, Value>, knobs: &mut Knobs, warnings: &mut Vec<String>) -> Map<String, Value> {
    let mut good = Map::new();
    for (key, v) in map {
        let Ok(Value::Object(mut current)) = serde_json::to_value(*knobs) else { break };
        if !current.contains_key(key) {
            warnings.push(format!("knobs.{key}: no such knob"));
            continue;
        }
        current.insert(key.clone(), v.clone());
        match serde_json::from_value::<Knobs>(Value::Object(current)) {
            Ok(k) => {
                *knobs = k.sanitized();
                good.insert(key.clone(), v.clone());
            }
            Err(e) => warnings.push(format!("knobs.{key}: {e}")),
        }
    }
    good
}

/// The current look as a theme file: base, accent, the colours that differ from the base
/// preset with that accent (the theme's own changes plus the user's overrides), and knobs.
pub fn export(def: &ThemeDef, prefs: &Prefs, name: &str) -> String {
    let mut plain = prefs.clone();
    plain.knobs.high_contrast = false;
    let current = Tokens::build(def, &plain);
    let mut reference_prefs = plain.clone();
    reference_prefs.accent = super::Accent::Custom(hex(current.accent));
    reference_prefs.overrides.clear();
    let mut base = ThemeDef::builtin(def.base);
    base.dark = def.dark;
    let reference = Tokens::build(&base, &reference_prefs);
    let mut colors = Map::new();
    for role in super::ROLES {
        if let (Some(a), Some(b)) = (current.get(role.name), reference.get(role.name))
            && a != b
        {
            colors.insert(role.name.into(), Value::String(hex(a)));
        }
    }
    let knobs = serde_json::to_value(prefs.knobs.sanitized()).unwrap_or(Value::Null);
    let mut obj = Map::new();
    obj.insert("byob_theme".into(), Value::from(1));
    obj.insert("name".into(), Value::String(name.trim().chars().take(80).collect()));
    obj.insert("base".into(), Value::String(def.base.id.into()));
    obj.insert("dark".into(), Value::Bool(def.dark));
    obj.insert("accent".into(), Value::String(hex(current.accent)));
    obj.insert("colors".into(), Value::Object(colors));
    obj.insert("knobs".into(), knobs);
    serde_json::to_string_pretty(&Value::Object(obj)).unwrap_or_default()
}

/// Themes from the browser's imported list, and warnings naming the bad ones.
pub fn imported(prefs: &Prefs) -> (Vec<ThemeDef>, Vec<String>) {
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    for (i, text) in prefs.imported.iter().enumerate() {
        match parse(text, Origin::Imported(i)) {
            Ok(f) => out.push(f.def),
            Err(e) => warnings.push(format!("imported theme {}: {e}", i + 1)),
        }
    }
    (dedupe(out, &mut warnings), warnings)
}

/// Load `dir/*.json` (desktop). A missing folder is no themes, not an error.
#[cfg(not(target_arch = "wasm32"))]
pub fn load_dir(dir: &std::path::Path) -> (Vec<ThemeDef>, Vec<String>) {
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return (out, warnings) };
    let mut paths: Vec<_> = entries.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
    paths.sort();
    for path in paths.into_iter().take(100) {
        let file = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let read = std::fs::metadata(&path).map_err(|e| e.to_string()).and_then(|m| {
            if m.len() > Prefs::MAX_THEME_BYTES as u64 { Err("too large".into()) } else { std::fs::read_to_string(&path).map_err(|e| e.to_string()) }
        });
        match read.and_then(|text| parse(&text, Origin::Folder(file.clone()))) {
            Ok(f) => {
                warnings.extend(f.warnings.iter().map(|w| format!("{file}: {w}")));
                out.push(f.def);
            }
            Err(e) => warnings.push(format!("{file}: {e}")),
        }
    }
    (dedupe(out, &mut warnings), warnings)
}

/// Two files with the same name: keep the first.
fn dedupe(defs: Vec<ThemeDef>, warnings: &mut Vec<String>) -> Vec<ThemeDef> {
    let mut out: Vec<ThemeDef> = Vec::new();
    for d in defs {
        if out.iter().any(|o| o.id == d.id) {
            warnings.push(format!("two themes are called {:?}; showing the first", d.name));
        } else {
            out.push(d);
        }
    }
    out
}
