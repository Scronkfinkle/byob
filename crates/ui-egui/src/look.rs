//! This window's look for the engine (`byob_protocol::look`): every appearance setting with
//! its value and what it may be ([`report`]), and changes the model asks for, checked and
//! applied ([`apply`]). The window stays the authority: it says what actually changed.

use byob_protocol::look::{LookChanges, LookField, LookReport, ThemeChoice};
use serde_json::{Value, json};

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

fn bubble(s: BubbleStyle) -> String {
    lower(s.label())
}

fn round2(v: f32) -> Value {
    json!((f64::from(v) * 100.0).round() / 100.0)
}

/// The look as `prefs` sets it, drawn with `tokens`, with `themes` to choose from.
pub fn report(prefs: &Prefs, tokens: &Tokens, user_themes: &[ThemeDef]) -> LookReport {
    let k = &prefs.knobs;
    let mut fields = vec![
        field("mode", json!(lower(prefs.theme.label())), "dark, light, or system (follow the computer)"),
        field("dark_theme", json!(prefs.dark_theme), "the theme used when dark: a theme id from the list"),
        field("light_theme", json!(prefs.light_theme), "the theme used when light: a theme id from the list"),
        field(
            "accent",
            json!(accent_name(&prefs.accent)),
            "the highlight colour: theme (the theme's own), blue, violet, green, orange, pink, teal, or any #rrggbb",
        ),
        field("corners", round2(k.radius), "how round corners are, 0 (square) to 16"),
        field("text_size", round2(k.font_scale), "text size, 0.75 to 2 (1 is normal)"),
        field("code_size", round2(k.code_scale), "code text size, 0.75 to 2"),
        field("zoom", round2(prefs.zoom), "everything bigger or smaller (text, icons, spacing), 0.5 to 3"),
        field("density", json!(lower(k.density.label())), "spacing: compact, comfortable or spacious"),
        field("your_messages", json!(bubble(k.user_bubble)), "how your messages are drawn: filled, outlined or minimal"),
        field("replies", json!(bubble(k.assistant_bubble)), "how replies are drawn: filled, outlined or minimal"),
        field("message_layout", json!(if k.align == Align::Chat { "chat" } else { "stacked" }), "stacked (one column) or chat (your messages on the right)"),
        field("column_width", round2(k.max_width), "widest the conversation gets in points, 480 to 2000, or 0 for the whole window"),
        field("font", json!(if k.ui_font == UiFont::Ubuntu { "ubuntu" } else { "inter" }), "the interface font: inter or ubuntu"),
        field("code_font", json!(if k.mono_font == MonoFont::JetBrainsMono { "jetbrains_mono" } else { "hack" }), "the code font: hack or jetbrains_mono"),
        field("high_contrast", json!(k.high_contrast), "stronger text and borders: true or false"),
        field("reduce_motion", json!(k.reduce_motion), "no animations: true or false"),
        field("chart_colours", json!(if k.chart == ChartPalette::Accessible { "accessible" } else { "theme" }), "theme, or accessible (colour-blind safe)"),
    ];
    for r in ROLES {
        if let Some(c) = tokens.get(r.name) {
            let overridden = prefs.overrides.get(prefs.theme_id(tokens.dark)).is_some_and(|o| o.contains_key(r.name));
            let describe = format!("{} ({}){}: #rrggbb, or \"\" for the theme's", r.label, r.group, if overridden { ", changed from the theme's" } else { "" });
            fields.push(LookField { key: format!("colour.{}", r.name), value: json!(theme::hex(c)), describe });
        }
    }
    let themes = file::all(user_themes).into_iter().map(|d| ThemeChoice { id: d.id, name: d.name, dark: d.dark }).collect();
    LookReport { fields, themes, answered: None, problems: Vec::new() }.sanitized()
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

/// Apply `changes` to `prefs` (`dark`: the window draws dark; `user_themes`: theme files).
/// What couldn't be applied, and why.
pub fn apply(prefs: &mut Prefs, changes: &LookChanges, dark: bool, user_themes: &[ThemeDef]) -> Vec<String> {
    let mut problems = Vec::new();
    let themes = file::all(user_themes);
    for (key, v) in changes {
        let k: &mut Knobs = &mut prefs.knobs;
        let r: Result<(), String> = match key.as_str() {
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
                match (s.as_str(), fixed, v.as_str().and_then(theme::parse_color)) {
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
                    _ => Err(format!("accent: theme, blue, violet, green, orange, pink, teal or #rrggbb, not {v}")),
                }
            }
            "corners" => number(v, Knobs::RADIUS_RANGE, key).map(|n| k.radius = n.round()),
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
            "reduce_motion" => boolean(v, key).map(|b| k.reduce_motion = b),
            "chart_colours" => match text(v).as_str() {
                "theme" => {
                    k.chart = ChartPalette::Theme;
                    Ok(())
                }
                "accessible" | "colour_blind" | "colorblind" => {
                    k.chart = ChartPalette::Accessible;
                    Ok(())
                }
                _ => Err(format!("chart_colours: theme or accessible, not {v}")),
            },
            other => match other.strip_prefix("colour.").filter(|r| ROLES.iter().any(|x| x.name == *r)) {
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
        let r = report(&prefs, &tokens, &[]);
        assert!(r.fields.len() > 20 && r.fields.len() <= byob_protocol::look::MAX_FIELDS, "{}", r.fields.len());
        assert!(r.themes.iter().any(|t| t.id == prefs.dark_theme));
        let all: LookChanges = r.fields.iter().filter(|f| !f.key.starts_with("colour.")).map(|f| (f.key.clone(), f.value.clone())).collect();
        let mut p = prefs.clone();
        assert_eq!(apply(&mut p, &all, true, &[]), Vec::<String>::new());
        assert_eq!(p, prefs, "setting each to what it is changes nothing");
    }

    #[test]
    fn changes_apply_and_bad_values_are_explained() {
        let mut p = Prefs::default();
        let problems = apply(
            &mut p,
            &changes(
                json!({"accent": "Orange", "corners": 12, "text_size": "120%", "mode": "light", "replies": "outlined", "colour.app_bg": "#202124", "zoom": 9, "sparkle": 1, "light_theme": "byob-dark"}),
            ),
            true,
            &[],
        );
        assert_eq!(p.accent, Accent::Orange);
        assert_eq!((p.knobs.radius, p.theme, p.knobs.assistant_bubble), (12.0, ThemeKind::Light, BubbleStyle::Outlined));
        assert_eq!(p.overrides[p.theme_id(true)]["app_bg"], "#202124");
        assert_eq!(problems.len(), 4, "{problems:?}");
        assert!(problems.iter().any(|e| e.starts_with("zoom:")) && problems.iter().any(|e| e.contains("sparkle")), "{problems:?}");
        assert!(problems.iter().any(|e| e.starts_with("text_size:")), "120% isn't a scale: {problems:?}");
        assert!(problems.iter().any(|e| e.contains("is a dark theme")), "{problems:?}");
        // A custom accent, and a colour back to the theme's.
        assert!(apply(&mut p, &changes(json!({"accent": "#ff8800", "colour.app_bg": ""})), true, &[]).is_empty());
        assert_eq!(p.accent, Accent::Custom("#ff8800".into()));
        assert!(p.overrides[p.theme_id(true)].is_empty());
    }
}
