//! This window's look for the engine (`byob_protocol::look`): every appearance setting with
//! its value and what it may be ([`report`]), and changes the model asks for, checked and
//! applied ([`apply`]). The window stays the authority: it says what actually changed.

use byob_protocol::look::{LookChanges, LookField, LookReport, ThemeChoice};
use serde_json::{Value, json};

use crate::cockpit;
use crate::count::SizeUnit;
use crate::theme::file::{self, ThemeDef};
use crate::theme::{self, Accent, Align, BubbleStyle, ChartPalette, Density, Knobs, MonoFont, Prefs, ROLES, ThemeKind, Tokens, UiFont};

fn field(key: &str, value: Value, describe: &str) -> LookField {
    LookField { key: key.into(), value, describe: describe.into() }
}

fn lower(s: &str) -> String {
    s.to_ascii_lowercase()
}

fn accent_name(a: &Accent) -> String {
    match a {
        Accent::Theme => "theme".into(),
        Accent::Named(n) => n.clone(),
        Accent::Custom(c) => c.clone(),
        other => lower(&other.label()),
    }
}

/// The accent names of the theme `prefs` draws with now (`dark`: in dark mode).
fn theme_accents(prefs: &Prefs, dark: bool, user_themes: &[ThemeDef]) -> Vec<String> {
    file::resolve(prefs.theme_id(dark), dark, user_themes).base.accents.iter().map(|(n, _)| lower(n)).collect()
}

fn bubble(s: BubbleStyle) -> String {
    lower(s.label())
}

fn round2(v: f32) -> Value {
    json!((f64::from(v) * 100.0).round() / 100.0)
}

/// The free arrangement's settings: which, and each module's place, bezel and label.
fn arrangement_fields(a: &cockpit::arrange::Arrangement, fields: &mut Vec<LookField>) {
    use cockpit::arrange::{Bezel, Module};
    let presets: Vec<String> = cockpit::arrange::Arrangement::PRESETS.iter().map(|(n, d)| format!("{n} ({d})")).collect();
    fields.push(field(
        "arrangement",
        json!(if a.free { "free" } else { "docked" }),
        &format!(
            "docked (byob's usual panels), free (each part where its place.* says, with open space between for a backdrop), or one of: {}",
            presets.join(", ")
        ),
    ));
    let bezels: Vec<&str> = Bezel::ALL.iter().map(|b| b.name()).collect();
    for m in Module::ALL {
        let id = m.id();
        fields.push(field(
            &format!("place.{id}"),
            json!(a.spot(m).to_string()),
            &format!("where {} is when free: left top width height in % of the window (\"22 36 56 48\"), or hidden", m.describe()),
        ));
        fields.push(field(
            &format!("bezel.{id}"),
            json!(a.bezel(m).name()),
            &format!("its frame when free: {} (metal: a metal plate with screws, a stamped label and a lit screen)", bezels.join(", ")),
        ));
        fields.push(field(&format!("label.{id}"), json!(a.label(m)), "text stamped on its bezel when free (\"COMMS\"), up to 24 characters, \"\" for none"));
    }
}

/// What `backdrop` is now, for the model: off, or a scene (its code is a note).
fn backdrop_value(source: &str) -> String {
    if source.is_empty() {
        "off".into()
    } else {
        format!("on ({} lines of WGSL, in the note backdrop_source: read ui_look with notes: true)", source.lines().count())
    }
}

/// The look as `prefs` sets it, drawn with `tokens`, with `themes` to choose from
/// (`backdrops`: this window can draw backdrop shaders).
pub fn report(prefs: &Prefs, tokens: &Tokens, user_themes: &[ThemeDef], backdrops: bool) -> LookReport {
    let k = &prefs.knobs;
    let mut fields = vec![
        field("mode", json!(lower(prefs.theme.label())), "dark, light, or system (follow the computer)"),
        field("dark_theme", json!(prefs.dark_theme), "the theme used when dark: a theme id from the list"),
        field("light_theme", json!(prefs.light_theme), "the theme used when light: a theme id from the list"),
        field(
            "accent",
            json!(accent_name(&prefs.accent)),
            &format!(
                "the highlight color: theme (the theme's own), blue, violet, green, orange, pink, teal, one of this theme's colors ({}), or any #rrggbb",
                theme_accents(prefs, tokens.dark, user_themes).join(", ")
            ),
        ),
        field("corners", round2(k.radius), "how round corners are, 0 (square) to 16"),
        field("text_size", round2(k.font_scale), "text size, 0.75 to 2 (1 is normal)"),
        field("code_size", round2(k.code_scale), "code text size, 0.75 to 2"),
        field("zoom", round2(prefs.zoom), "everything bigger or smaller (text, icons, spacing), 0.5 to 3"),
        field("density", json!(lower(k.density.label())), "spacing: compact, comfortable or spacious"),
        field("your_messages", json!(bubble(k.user_bubble)), "how your messages are drawn: filled, outlined or minimal"),
        field("replies", json!(bubble(k.assistant_bubble)), "how replies are drawn: filled, outlined or minimal"),
        field("message_layout", json!(if k.align == Align::Chat { "chat" } else { "stacked" }), "stacked (one column) or chat (your messages on the right)"),
        field("bubble_padding", round2(k.bubble_padding), "space inside message bubbles between their edge and the text, 0 to 32 (10 is normal)"),
        field("line_spacing", round2(k.line_spacing), "space between lines of message text, 0.9 to 2 (1 is normal)"),
        field("column_width", round2(k.max_width), "widest the conversation gets in points, 480 to 2000, or 0 for the whole window"),
        field("font", json!(if k.ui_font == UiFont::Ubuntu { "ubuntu" } else { "inter" }), "the interface font: inter or ubuntu"),
        field("code_font", json!(if k.mono_font == MonoFont::JetBrainsMono { "jetbrains_mono" } else { "hack" }), "the code font: hack or jetbrains_mono"),
        field("high_contrast", json!(k.high_contrast), "stronger text and borders: true or false"),
        field(
            "controls",
            json!(k.controls.name()),
            "how buttons and other controls look: standard, hud (a heads-up display: square, outlined in the accent, lit when hovered), plate (raised metal plates that press in) or pill (fully rounded)",
        ),
        field("reduce_motion", json!(k.reduce_motion), "no animations: true or false"),
        field(
            "thinking_size",
            json!(if prefs.thinking_size == SizeUnit::Characters { "characters" } else { "tokens" }),
            "what a reply's thinking is measured in: tokens (about) or characters",
        ),
        field("chart_colors", json!(if k.chart == ChartPalette::Accessible { "accessible" } else { "theme" }), "theme, or accessible (color-blind safe)"),
    ];
    let mut notes = std::collections::BTreeMap::new();
    if backdrops {
        let c = &prefs.cockpit;
        fields.push(field(
            "backdrop",
            json!(backdrop_value(&c.backdrop)),
            "a scene drawn behind the panels, which turn to glass over it: off, or WGSL shader code you write (read ui_look with notes: true first: the guide says how, and how to re-theme the whole window around it)",
        ));
        fields.push(field("glass", round2(c.glass), "how opaque the panels are over a backdrop, 0.2 (mostly see-through) to 1 (opaque)"));
        fields.push(field("frost", round2(c.frost), "how frosted the glass is over a backdrop, 0 (clear) to 1 (blurs what's behind the panels); only where the window can (a backdrop on when byob started)"));
        fields.push(field("panel_gap", round2(c.gap), "space between the panels over a backdrop, where it shows, 0 to 32 points"));
        notes.insert("backdrop_guide".to_owned(), cockpit::wgsl::GUIDE.to_owned());
        if !c.backdrop.is_empty() {
            notes.insert("backdrop_source".to_owned(), c.backdrop.clone());
        }
    }
    arrangement_fields(&prefs.cockpit.arrangement, &mut fields);
    for r in ROLES {
        if let Some(c) = tokens.get(r.name) {
            let overridden = prefs.overrides.get(prefs.theme_id(tokens.dark)).is_some_and(|o| o.contains_key(r.name));
            let describe = format!("{} ({}){}: #rrggbb, or \"\" for the theme's", r.label, r.group, if overridden { ", changed from the theme's" } else { "" });
            fields.push(LookField { key: format!("color.{}", r.name), value: json!(theme::hex(c)), describe });
        }
    }
    let themes = file::all(user_themes).into_iter().map(|d| ThemeChoice { id: d.id, name: d.name, dark: d.dark }).collect();
    LookReport { fields, themes, answered: None, problems: Vec::new(), notes }.sanitized()
}

fn number(v: &Value, range: std::ops::RangeInclusive<f32>, key: &str) -> Result<f32, String> {
    #[allow(clippy::cast_possible_truncation)] // a setting, checked against a small range
    let n = v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().trim_end_matches('%').parse().ok())).map(|n| n as f32);
    match n {
        Some(n) if range.contains(&n) => Ok(n),
        _ => Err(format!("{key}: {v} isn't a number from {} to {}", range.start(), range.end())),
    }
}

fn text(v: &Value) -> String {
    v.as_str().map(|s| s.trim().to_ascii_lowercase().replace([' ', '-'], "_")).unwrap_or_default()
}

fn boolean(v: &Value, key: &str) -> Result<bool, String> {
    match v {
        Value::Bool(b) => Ok(*b),
        Value::String(s) if matches!(s.as_str(), "true" | "on" | "yes") => Ok(true),
        Value::String(s) if matches!(s.as_str(), "false" | "off" | "no") => Ok(false),
        _ => Err(format!("{key}: true or false, not {v}")),
    }
}

fn bubble_style(v: &Value, key: &str) -> Result<BubbleStyle, String> {
    BubbleStyle::ALL.into_iter().find(|b| lower(b.label()) == text(v)).ok_or_else(|| format!("{key}: filled, outlined or minimal, not {v}"))
}

/// Most looks kept to undo to.
pub const HISTORY: usize = 20;

/// A change the model asked for (`ServerMsg::SetLook`), with undo: `{"undo": true}` puts back
/// the look before the last change (`history`, newest last); `{"reset": true}` goes back to
/// the theme's own colors and byob's defaults (keeping the themes, mode and zoom). Anything
/// else is [`apply`]'d. What couldn't be done, and why.
pub fn change(prefs: &mut Prefs, history: &mut Vec<Prefs>, changes: &LookChanges, dark: bool, user_themes: &[ThemeDef], backdrops: bool) -> Vec<String> {
    let flag = |k: &str| changes.get(k).is_some_and(|v| v.as_bool().unwrap_or(true));
    if flag("undo") {
        return match history.pop() {
            Some(before) => {
                *prefs = before;
                Vec::new()
            }
            None => vec!["undo: there's no earlier look to go back to (only changes made by asking are kept)".into()],
        };
    }
    let before = prefs.clone();
    let mut problems = Vec::new();
    if flag("reset") {
        *prefs = Prefs {
            imported: std::mem::take(&mut prefs.imported),
            theme: prefs.theme,
            dark_theme: prefs.dark_theme.clone(),
            light_theme: prefs.light_theme.clone(),
            zoom: prefs.zoom,
            ..Prefs::default()
        };
    }
    let rest: LookChanges = changes.iter().filter(|(k, _)| *k != "reset" && *k != "undo").map(|(k, v)| (k.clone(), v.clone())).collect();
    problems.extend(apply(prefs, &rest, dark, user_themes, backdrops));
    if *prefs != before {
        history.push(before);
        if history.len() > HISTORY {
            history.remove(0);
        }
    }
    problems
}

/// A new backdrop: off, or WGSL that has to compile.
fn backdrop(c: &mut cockpit::Cockpit, v: &Value, backdrops: bool) -> Result<(), String> {
    let Some(text) = v.as_str() else {
        return Err(format!("backdrop: off, or WGSL code, not {v}"));
    };
    let name = text.trim().to_ascii_lowercase();
    if matches!(name.as_str(), "" | "off" | "none" | "no") {
        c.set_backdrop("");
        return Ok(());
    }
    if !backdrops {
        return Err("backdrop: this window can't draw shaders (it isn't drawing with wgpu)".into());
    }
    cockpit::wgsl::check(text).map_err(|e| format!("backdrop: {e}"))?;
    c.set_backdrop(text);
    Ok(())
}

/// `place.<module>`, `bezel.<module>` or `label.<module>`. Placing or framing a module makes
/// the arrangement free (from byob's floating one, if it was docked with none).
fn module_setting(a: &mut cockpit::arrange::Arrangement, key: &str, v: &Value) -> Result<(), String> {
    use cockpit::arrange::{Arrangement, Bezel, MAX_LABEL, Module, Spot};
    let (kind, id) = key.split_once('.').unwrap_or_default();
    let Some(m) = Module::parse(id) else {
        let ids: Vec<&str> = Module::ALL.iter().map(|m| m.id()).collect();
        return Err(format!("{key}: there's no part {id:?} (they are {})", ids.join(", ")));
    };
    let text_value = v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string());
    // Set as it is, nothing changes (docked stays docked); a change makes it free.
    let go_free = |a: &mut Arrangement| {
        if !a.free {
            if a.places.is_empty()
                && let Some(p) = Arrangement::preset("floating")
            {
                *a = p;
            }
            a.free = true;
        }
    };
    match kind {
        "place" => Spot::parse(&text_value).map(|s| {
            if s != a.spot(m) {
                go_free(a);
                a.places.insert(m.id().to_owned(), s);
            }
        }),
        "bezel" => Bezel::parse(&text_value)
            .map(|b| {
                if b != a.bezel(m) {
                    go_free(a);
                    a.bezels.insert(m.id().to_owned(), b);
                }
            })
            .ok_or_else(|| format!("{key}: glass, metal or bare, not {v}")),
        _ => {
            let label: String = text_value.trim().chars().filter(|c| !c.is_control()).take(MAX_LABEL).collect();
            if label.is_empty() {
                a.labels.remove(m.id());
            } else {
                a.labels.insert(m.id().to_owned(), label);
            }
            Ok(())
        }
    }
    .map_err(|e| if e.starts_with(key) { e } else { format!("{key}: {e}") })
}

/// Apply `changes` to `prefs` (`dark`: the window draws dark; `user_themes`: theme files;
/// `backdrops`: it can draw backdrop shaders). What couldn't be applied, and why.
pub fn apply(prefs: &mut Prefs, changes: &LookChanges, dark: bool, user_themes: &[ThemeDef], backdrops: bool) -> Vec<String> {
    let mut problems = Vec::new();
    let themes = file::all(user_themes);
    // Colors last, into the theme the window draws with after the rest: "dark mode, and
    // this background" colors the dark theme.
    let (colors, rest): (Vec<_>, Vec<_>) = changes.iter().partition(|(k, _)| k.starts_with("color."));
    for (key, v) in rest.into_iter().chain(colors) {
        let dark = match prefs.theme {
            ThemeKind::Dark => true,
            ThemeKind::Light => false,
            ThemeKind::System => dark,
        };
        let k: &mut Knobs = &mut prefs.knobs;
        let r: Result<(), String> = match key.as_str() {
            "fluid" => {
                let ops = v.as_str().map_or_else(|| v.clone(), |s| serde_json::from_str(s).unwrap_or(Value::Null));
                let a = crate::fluid::doc::apply(&mut prefs.fluid, &ops, "you");
                if a.problems.is_empty() { Ok(()) } else { Err(a.problems.join("; ")) }
            }
            "mode" => ThemeKind::from_name(&text(v)).map(|m| prefs.theme = m).ok_or_else(|| format!("mode: dark, light or system, not {v}")),
            "dark_theme" | "light_theme" => {
                let want = key == "dark_theme";
                match themes.iter().find(|d| d.id == v.as_str().unwrap_or_default() || lower(&d.name) == lower(v.as_str().unwrap_or_default())) {
                    Some(d) if d.dark == want => {
                        if want {
                            prefs.dark_theme.clone_from(&d.id)
                        } else {
                            prefs.light_theme.clone_from(&d.id)
                        }
                        Ok(())
                    }
                    Some(d) => Err(format!("{key}: {} is a {} theme", d.name, if d.dark { "dark" } else { "light" })),
                    None => Err(format!("{key}: no theme {v}")),
                }
            }
            "accent" => {
                let s = text(v);
                let fixed = Accent::FIXED.into_iter().find(|a| lower(&a.label()) == s);
                // One of the theme's own (the Appearance menu offers them; the report says them).
                let named = || theme_accents(prefs, dark, user_themes).into_iter().find(|n| lower(n) == s);
                match (s.as_str(), fixed.or_else(|| named().map(Accent::Named)), v.as_str().and_then(theme::parse_color)) {
                    ("theme" | "default", _, _) => {
                        prefs.accent = Accent::Theme;
                        Ok(())
                    }
                    (_, Some(a), _) => {
                        prefs.accent = a;
                        Ok(())
                    }
                    (_, None, Some(c)) => {
                        prefs.accent = Accent::Custom(theme::hex(c));
                        Ok(())
                    }
                    _ => Err(format!("accent: theme, blue, violet, green, orange, pink, teal, one of this theme's colors or #rrggbb, not {v}")),
                }
            }
            "corners" => number(v, Knobs::RADIUS_RANGE, key).map(|n| k.radius = n.round()),
            "bubble_padding" => number(v, Knobs::PADDING_RANGE, key).map(|n| k.bubble_padding = n.round()),
            "line_spacing" => number(v, Knobs::LINE_SPACING_RANGE, key).map(|n| k.line_spacing = n),
            "text_size" => number(v, Knobs::SCALE_RANGE, key).map(|n| k.font_scale = n),
            "code_size" => number(v, Knobs::SCALE_RANGE, key).map(|n| k.code_scale = n),
            "zoom" => number(v, Prefs::ZOOM_RANGE, key).map(|n| prefs.zoom = n),
            "density" => Density::ALL
                .into_iter()
                .find(|d| lower(d.label()) == text(v))
                .map(|d| k.density = d)
                .ok_or_else(|| format!("density: compact, comfortable or spacious, not {v}")),
            "your_messages" => bubble_style(v, key).map(|b| k.user_bubble = b),
            "replies" => bubble_style(v, key).map(|b| k.assistant_bubble = b),
            "message_layout" => match text(v).as_str() {
                "stacked" => {
                    k.align = Align::Stacked;
                    Ok(())
                }
                "chat" => {
                    k.align = Align::Chat;
                    Ok(())
                }
                _ => Err(format!("message_layout: stacked or chat, not {v}")),
            },
            "column_width" => number(v, 0.0..=2000.0, key).and_then(|n| {
                if n == 0.0 || n >= 480.0 {
                    k.max_width = n;
                    Ok(())
                } else {
                    Err("column_width: 0 (the whole window) or 480 to 2000".into())
                }
            }),
            "font" => match text(v).as_str() {
                "inter" => {
                    k.ui_font = UiFont::Inter;
                    Ok(())
                }
                "ubuntu" | "ubuntu_light" => {
                    k.ui_font = UiFont::Ubuntu;
                    Ok(())
                }
                _ => Err(format!("font: inter or ubuntu, not {v}")),
            },
            "code_font" => match text(v).as_str() {
                "hack" => {
                    k.mono_font = MonoFont::Hack;
                    Ok(())
                }
                "jetbrains_mono" | "jetbrains" => {
                    k.mono_font = MonoFont::JetBrainsMono;
                    Ok(())
                }
                _ => Err(format!("code_font: hack or jetbrains_mono, not {v}")),
            },
            "high_contrast" => boolean(v, key).map(|b| k.high_contrast = b),
            "controls" => crate::theme::Controls::ALL
                .into_iter()
                .find(|c| c.name() == text(v))
                .map(|c| k.controls = c)
                .ok_or_else(|| format!("controls: standard, hud, plate or pill, not {v}")),
            "backdrop" => backdrop(&mut prefs.cockpit, v, backdrops),
            "arrangement" => {
                let a = &mut prefs.cockpit.arrangement;
                match text(v).as_str() {
                    "docked" | "dock" | "normal" | "default" => {
                        a.free = false;
                        Ok(())
                    }
                    "free" if !a.places.is_empty() => {
                        a.free = true;
                        Ok(())
                    }
                    name => match cockpit::arrange::Arrangement::preset(name) {
                        Some(p) => {
                            *a = p;
                            Ok(())
                        }
                        None => Err(format!("arrangement: docked, free, floating or focus, not {v}")),
                    },
                }
            }
            "glass" => number(v, cockpit::Cockpit::GLASS_RANGE, key).map(|n| prefs.cockpit.glass = n),
            "frost" => number(v, 0.0..=1.0, key).map(|n| prefs.cockpit.frost = n),
            "panel_gap" => number(v, cockpit::Cockpit::GAP_RANGE, key).map(|n| prefs.cockpit.gap = n.round()),
            "reduce_motion" => boolean(v, key).map(|b| k.reduce_motion = b),
            "thinking_size" => match text(v).as_str() {
                "tokens" => {
                    prefs.thinking_size = SizeUnit::Tokens;
                    Ok(())
                }
                "characters" | "chars" => {
                    prefs.thinking_size = SizeUnit::Characters;
                    Ok(())
                }
                _ => Err(format!("thinking_size: tokens or characters, not {v}")),
            },
            "chart_colors" | "chart_colours" => match text(v).as_str() {
                "theme" => {
                    k.chart = ChartPalette::Theme;
                    Ok(())
                }
                "accessible" | "color_blind" | "colour_blind" | "colorblind" => {
                    k.chart = ChartPalette::Accessible;
                    Ok(())
                }
                _ => Err(format!("chart_colors: theme or accessible, not {v}")),
            },
            other if other.starts_with("place.") || other.starts_with("bezel.") || other.starts_with("label.") => {
                module_setting(&mut prefs.cockpit.arrangement, other, v)
            }
            other => match other.strip_prefix("color.").filter(|r| ROLES.iter().any(|x| x.name == *r)) {
                Some(role) => {
                    let id = prefs.theme_id(dark).to_owned();
                    match v.as_str().map(str::trim) {
                        Some("") => {
                            if let Some(o) = prefs.overrides.get_mut(&id) {
                                o.remove(role);
                            }
                            Ok(())
                        }
                        Some(s) => match theme::parse_color(s) {
                            Some(c) => {
                                prefs.overrides.entry(id).or_default().insert(role.to_owned(), theme::hex(c));
                                Ok(())
                            }
                            None => Err(format!("{key}: #rrggbb, or \"\" for the theme's, not {v}")),
                        },
                        None => Err(format!("{key}: #rrggbb, or \"\" for the theme's, not {v}")),
                    }
                }
                None => Err(format!("there's no setting {other:?}")),
            },
        };
        if let Err(e) = r {
            problems.push(e);
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changes(v: Value) -> LookChanges {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn every_setting_reported_can_be_set_to_what_it_is() {
        let prefs = Prefs::default();
        let tokens = Tokens::dark();
        let r = report(&prefs, &tokens, &[], true);
        assert!(r.fields.len() > 20 && r.fields.len() <= byob_protocol::look::MAX_FIELDS, "{}", r.fields.len());
        assert!(r.themes.iter().any(|t| t.id == prefs.dark_theme));
        let all: LookChanges = r.fields.iter().filter(|f| !f.key.starts_with("color.")).map(|f| (f.key.clone(), f.value.clone())).collect();
        let mut p = prefs.clone();
        assert_eq!(apply(&mut p, &all, true, &[], true), Vec::<String>::new());
        assert_eq!(p, prefs, "setting each to what it is changes nothing");
    }

    /// What Appearance offers the model can set too: Thinking size, and the theme's own
    /// accents (byob's ink, Catppuccin's mauve), which the report names when they're picked.
    #[test]
    fn thinking_size_and_the_themes_own_accents_can_be_set() {
        let mut p = Prefs::default();
        assert!(apply(&mut p, &changes(json!({"thinking_size": "characters", "accent": "ink"})), true, &[], true).is_empty());
        assert_eq!((p.thinking_size, &p.accent), (SizeUnit::Characters, &Accent::Named("ink".into())));
        let r = report(&p, &Tokens::dark(), &[], true);
        assert_eq!((r.get("thinking_size"), r.get("accent")), (Some(&json!("characters")), Some(&json!("ink"))));
        assert!(r.fields.iter().any(|f| f.key == "accent" && f.describe.contains("ink")), "the theme's accents are listed");
        // Another theme's: Catppuccin's mauve; still the fixed blue for "blue".
        p.dark_theme = "catppuccin-mocha".into();
        assert!(apply(&mut p, &changes(json!({"accent": "Mauve"})), true, &[], true).is_empty());
        assert_eq!(p.accent, Accent::Named("mauve".into()));
        assert!(apply(&mut p, &changes(json!({"accent": "blue"})), true, &[], true).is_empty());
        assert_eq!(p.accent, Accent::Blue);
        assert_eq!(apply(&mut p, &changes(json!({"accent": "plaid", "thinking_size": "words"})), true, &[], true).len(), 2);
    }

    #[test]
    fn changes_apply_and_bad_values_are_explained() {
        let mut p = Prefs::default();
        let problems = apply(
            &mut p,
            &changes(
                json!({"accent": "Orange", "corners": 12, "text_size": "120%", "mode": "light", "replies": "outlined", "color.app_bg": "#202124", "zoom": 9, "sparkle": 1, "light_theme": "byob-dark"}),
            ),
            true,
            &[],
            true,
        );
        assert_eq!(p.accent, Accent::Orange);
        assert_eq!((p.knobs.radius, p.theme, p.knobs.assistant_bubble), (12.0, ThemeKind::Light, BubbleStyle::Outlined));
        assert_eq!(p.overrides[p.theme_id(false)]["app_bg"], "#202124", "the light theme: the mode asked for with it");
        assert_eq!(problems.len(), 4, "{problems:?}");
        assert!(problems.iter().any(|e| e.starts_with("zoom:")) && problems.iter().any(|e| e.contains("sparkle")), "{problems:?}");
        assert!(problems.iter().any(|e| e.starts_with("text_size:")), "120% isn't a scale: {problems:?}");
        assert!(problems.iter().any(|e| e.contains("is a dark theme")), "{problems:?}");
        // A custom accent, and a color back to the theme's.
        assert!(apply(&mut p, &changes(json!({"accent": "#ff8800", "color.app_bg": ""})), true, &[], true).is_empty());
        assert_eq!(p.accent, Accent::Custom("#ff8800".into()));
        assert!(p.overrides[p.theme_id(false)].is_empty());
        // The old British key still works.
        assert!(apply(&mut p, &changes(json!({"chart_colours": "accessible"})), true, &[], true).is_empty());
        assert_eq!(p.knobs.chart, ChartPalette::Accessible);
    }

    /// A backdrop: a scene by name, WGSL that compiles (on trial, its source in a note), or
    /// off; WGSL that doesn't is refused with its line, and a window without wgpu says so.
    #[test]
    fn backdrops_are_checked_before_they_are_kept() {
        let mut p = Prefs::default();
        let r = report(&p, &Tokens::dark(), &[], true);
        assert_eq!(r.get("backdrop"), Some(&json!("off")));
        assert!(r.notes.contains_key("backdrop_guide") && !r.notes.contains_key("backdrop_source"));
        assert!(!apply(&mut p, &changes(json!({"backdrop": "cockpit"})), true, &[], true).is_empty(), "no scenes by name: only code");
        let own = "fn scene(p: vec2<f32>) -> vec4<f32> {\n    return vec4<f32>(u.accent.rgb * 0.2, 1.0);\n}\n";
        assert!(apply(&mut p, &changes(json!({"backdrop": own, "glass": 0.5})), true, &[], true).is_empty());
        assert!(p.cockpit.trial && p.cockpit.glass == 0.5);
        let r = report(&p, &Tokens::dark(), &[], true);
        assert_eq!(r.notes.get("backdrop_source").map(String::as_str), Some(own));
        assert!(r.get("backdrop").unwrap().as_str().unwrap().starts_with("on (3 lines"));
        let bad = apply(&mut p, &changes(json!({"backdrop": "fn scene(p: vec2<f32>) -> vec4<f32> {\n  return oops;\n}"})), true, &[], true);
        assert!(bad.len() == 1 && bad[0].starts_with("backdrop: line 2:"), "{bad:?}");
        assert_eq!(p.cockpit.backdrop, own, "kept the one that worked");
        assert!(apply(&mut p, &changes(json!({"backdrop": "off"})), true, &[], true).is_empty());
        assert!(p.cockpit.backdrop.is_empty() && !p.cockpit.trial);
        let no_gpu = apply(&mut p, &changes(json!({"backdrop": own})), true, &[], false);
        assert!(no_gpu[0].contains("can't draw shaders"), "{no_gpu:?}");
        assert!(report(&p, &Tokens::dark(), &[], false).get("backdrop").is_none());
    }

    /// Arrangements: a preset by name, then a part moved, framed and labeled on top; a
    /// place given while docked makes it free; mistakes are named.
    #[test]
    fn the_model_arranges_the_window() {
        use crate::cockpit::arrange::{Bezel, Module, Spot};
        let mut p = Prefs::default();
        let r = report(&p, &Tokens::dark(), &[], false);
        assert_eq!(r.get("arrangement"), Some(&json!("docked")));
        assert!(r.fields.len() <= byob_protocol::look::MAX_FIELDS, "{}", r.fields.len());
        let problems = apply(
            &mut p,
            &changes(
                json!({"arrangement": "floating", "place.chat": "25 30 50 55", "bezel.chat": "bare", "bezel.chats": "instrument", "label.chats": "NAV", "label.composer": "Say it"}),
            ),
            true,
            &[],
            false,
        );
        assert!(problems.is_empty(), "{problems:?}");
        let a = &p.cockpit.arrangement;
        assert!(a.free);
        assert_eq!((a.spot(Module::Chat), a.bezel(Module::Chat)), (Spot::At([25.0, 30.0, 50.0, 55.0]), Bezel::Bare));
        assert_eq!(
            (a.label(Module::Composer), a.label(Module::Chats), a.bezel(Module::Chats)),
            ("Say it", "NAV", Bezel::Metal),
            "labels and bezels on top of the floating arrangement"
        );
        let bad =
            apply(&mut p, &changes(json!({"place.warp": "1 2 3 4", "place.chat": "1 2", "bezel.pane": "chrome", "arrangement": "zen"})), true, &[], false);
        assert_eq!(bad.len(), 4, "{bad:?}");
        assert!(bad.iter().any(|e| e.contains("there's no part \"warp\"")), "{bad:?}");
        assert!(apply(&mut p, &changes(json!({"arrangement": "docked"})), true, &[], false).is_empty());
        assert!(!p.cockpit.arrangement.free && p.cockpit.arrangement.label(Module::Chats) == "NAV", "docked, the free places kept");
        let mut q = Prefs::default();
        assert!(apply(&mut q, &changes(json!({"place.chats": "hidden"})), true, &[], false).is_empty());
        assert!(
            q.cockpit.arrangement.free && q.cockpit.arrangement.spot(Module::Chats) == Spot::Hidden && q.cockpit.arrangement.spot(Module::Chat) != Spot::Hidden
        );
    }

    /// Bubble padding and line spacing: asked for in a stress test, now settings.
    #[test]
    fn bubble_padding_and_line_spacing_are_settings() {
        let mut p = Prefs::default();
        let problems = apply(&mut p, &changes(json!({"bubble_padding": 18, "line_spacing": 1.3})), true, &[], true);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!((p.knobs.bubble_padding, p.knobs.line_spacing), (18.0, 1.3));
        assert!(!apply(&mut p, &changes(json!({"line_spacing": 9})), true, &[], true).is_empty());
        let r = report(&p, &crate::theme::Tokens::dark(), &[], true);
        assert_eq!(r.get("bubble_padding"), Some(&json!(18.0)));
    }

    /// Undo puts back the look before the model's last change (exactly: overrides too);
    /// reset goes back to the theme's; colors asked for with a mode go to that mode's theme.
    #[test]
    fn changes_can_be_undone_and_reset() {
        let start = Prefs::default();
        let mut p = start.clone();
        let mut history = Vec::new();
        assert!(change(&mut p, &mut history, &changes(json!({"mode": "dark", "color.app_bg": "#191724", "corners": 16})), false, &[], true).is_empty());
        assert_eq!(p.overrides[&p.dark_theme]["app_bg"], "#191724", "the color went to the dark theme, the one now in use");
        assert!(!p.overrides.contains_key(&p.light_theme));
        assert!(change(&mut p, &mut history, &changes(json!({"accent": "pink"})), true, &[], true).is_empty());
        assert!(change(&mut p, &mut history, &changes(json!({"undo": true})), true, &[], true).is_empty());
        assert_eq!((p.knobs.radius, p.accent.clone()), (16.0, Accent::Theme), "one step back");
        change(&mut p, &mut history, &changes(json!({"undo": true})), true, &[], true);
        assert_eq!(p, start, "back to the start");
        assert!(change(&mut p, &mut history, &changes(json!({"undo": true})), true, &[], true)[0].contains("no earlier look"));
        // Reset keeps the themes and mode, drops the rest.
        change(&mut p, &mut history, &changes(json!({"mode": "dark", "color.text": "#ff0000", "corners": 3})), false, &[], true);
        change(&mut p, &mut history, &changes(json!({"reset": true})), true, &[], true);
        assert_eq!((p.theme, p.knobs.radius), (ThemeKind::Dark, Knobs::default().radius));
        assert!(p.overrides.values().all(std::collections::BTreeMap::is_empty), "{:?}", p.overrides);
    }
}
