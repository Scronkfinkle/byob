use super::file::{self, Origin};
use super::presets::{self, PRESETS};
use super::*;

/// Presets whose own body-text colour doesn't reach 7:1 (WCAG AAA) on every surface — the
/// raised ones (replies, your accent-tinted messages, code) are where they fall short — and
/// the floor they do reach, with every accent. All are at least AA (4.5:1). The scheme's
/// colours are kept as designed; High contrast lifts every one of them to 7:1 (tested).
/// Solarized's text sits right at 4.5:1 by design and is lifted to it where needed.
const BODY_TEXT_EXCEPTIONS: &[(&str, f32)] = &[
    ("catppuccin-latte", 6.2),
    ("catppuccin-frappe", 5.1),
    ("catppuccin-macchiato", 6.2),
    ("nord", 6.2),
    ("rose-pine-dawn", 5.8),
    ("solarized-dark", 4.5),
    ("solarized-light", 4.5),
];

fn body_floor(id: &str) -> f32 {
    BODY_TEXT_EXCEPTIONS.iter().find(|(e, _)| *e == id).map_or(7.0, |(_, f)| *f)
}

/// Every accent a user can pick on a preset.
fn accents(p: &Palette) -> Vec<Accent> {
    let mut out = vec![Accent::Theme];
    out.extend(p.accents.iter().map(|(n, _)| Accent::Named((*n).into())));
    out.extend(Accent::FIXED);
    out.push(Accent::Custom("#808080".into()));
    out
}

/// Every contrast requirement `t` misses.
fn check(name: &str, t: &Tokens, body: f32) -> Vec<String> {
    let mut bad = Vec::new();
    let mut need = |what: &str, fg: Color32, bg: Color32, min: f32| {
        let r = contrast(fg, bg);
        if r < min {
            bad.push(format!("{name}: {what} {r:.2} < {min}"));
        }
    };
    let surfaces = [("app", t.app_bg), ("panel", t.panel_bg), ("user", t.user_bubble), ("assistant", t.assistant_bubble), ("field", t.field_bg)];
    for (surface, bg) in surfaces {
        need(&format!("text on {surface}"), t.text, bg, body);
        need(&format!("dim text on {surface}"), t.text_dim, bg, 4.5);
        need(&format!("accent on {surface}"), t.accent, bg, 4.5);
        need(&format!("danger on {surface}"), t.danger, bg, 4.5);
        need(&format!("warning on {surface}"), t.warning, bg, 4.5);
        need(&format!("success on {surface}"), t.success, bg, 4.5);
    }
    need("code", t.code_text, t.code_bg, body);
    for (role, c) in [("keyword", t.syntax_keyword), ("string", t.syntax_string), ("number", t.syntax_number), ("comment", t.syntax_comment)] {
        need(&format!("syntax {role}"), c, t.code_bg, 4.5);
    }
    need("inline code", t.inline_code_text, t.inline_code_bg, 4.5);
    need("on accent", t.on_accent, t.accent, 4.5);
    for c in t.chart {
        need("chart series", c, t.assistant_bubble, 3.0);
    }
    bad
}

#[test]
fn every_preset_is_readable_with_every_accent() {
    let mut bad = Vec::new();
    for p in PRESETS {
        for accent in accents(p) {
            for hc in [false, true] {
                let prefs = Prefs { accent: accent.clone(), knobs: Knobs { high_contrast: hc, ..Knobs::default() }, ..Prefs::default() };
                let t = Tokens::build(&ThemeDef::builtin(p), &prefs);
                let body = if hc { 7.0 } else { body_floor(p.id) };
                bad.extend(check(&format!("{} {accent:?} hc={hc}", p.id), &t, body));
            }
        }
    }
    assert!(bad.is_empty(), "{} failures:\n{}", bad.len(), bad.join("\n"));
}

#[test]
fn exceptions_are_real_and_still_meet_aa() {
    for (id, floor) in BODY_TEXT_EXCEPTIONS {
        assert!(*floor >= 4.5, "{id}");
        let p = presets::find(id).unwrap();
        let t = Tokens::build(&ThemeDef::builtin(p), &Prefs::default());
        let worst =
            [t.app_bg, t.panel_bg, t.user_bubble, t.assistant_bubble, t.field_bg, t.code_bg].iter().map(|bg| contrast(t.text, *bg)).fold(21.0, f32::min);
        assert!(worst < 7.0, "{id} reaches 7:1 now ({worst:.2}); drop the exception");
    }
}

#[test]
fn presets_are_well_formed() {
    let mut ids: Vec<&str> = PRESETS.iter().map(|p| p.id).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), PRESETS.len(), "duplicate preset ids");
    for p in PRESETS {
        assert!(!p.accents.is_empty(), "{}", p.id);
        assert_eq!(presets::find(&p.id.to_uppercase()).map(|f| f.id), Some(p.id));
        assert_eq!(p.dark, luminance(rgb(p.bg)) < 0.2, "{}: dark flag", p.id);
    }
    for id in ["catppuccin-latte", "catppuccin-frappe", "catppuccin-macchiato", "catppuccin-mocha", "gruvbox-dark", "nord", "dracula", "solarized-light"] {
        assert!(presets::find(id).is_some(), "{id}");
    }
    // Official Catppuccin Mocha values land where they should.
    let m = Tokens::build(&ThemeDef::builtin(&presets::CATPPUCCIN_MOCHA), &Prefs::default());
    assert_eq!((m.app_bg, m.panel_bg, m.text, m.accent), (rgb(0x1e1e2e), rgb(0x181825), rgb(0xcdd6f4), rgb(0xcba6f7)));
}

#[test]
fn high_contrast_raises_secondary_text_contrast() {
    for p in PRESETS {
        let def = ThemeDef::builtin(p);
        let n = Tokens::build(&def, &Prefs::default());
        let hc = Tokens::build(&def, &Prefs { knobs: Knobs { high_contrast: true, ..Knobs::default() }, ..Prefs::default() });
        assert!(contrast(hc.text_dim, hc.panel_bg) > contrast(n.text_dim, n.panel_bg), "{}", p.id);
        assert!(contrast(hc.border, hc.panel_bg) > contrast(n.border, n.panel_bg), "{}", p.id);
    }
}

#[test]
fn system_theme_follows_the_os_and_defaults_to_dark() {
    assert!(ThemeKind::System.is_dark(None));
    assert!(ThemeKind::System.is_dark(Some(egui::Theme::Dark)));
    assert!(!ThemeKind::System.is_dark(Some(egui::Theme::Light)));
    assert!(!ThemeKind::Light.is_dark(Some(egui::Theme::Dark)));
    assert!(ThemeKind::Dark.is_dark(Some(egui::Theme::Light)));
    // System picks the user's light or dark theme.
    let prefs = Prefs { dark_theme: "nord".into(), light_theme: "gruvbox-light".into(), ..Prefs::default() };
    assert_eq!(Tokens::for_prefs(&prefs, Some(egui::Theme::Dark), &[]).app_bg, rgb(0x2e3440));
    assert_eq!(Tokens::for_prefs(&prefs, Some(egui::Theme::Light), &[]).app_bg, rgb(0xfbf1c7));
    // An id that no longer exists (deleted theme file) falls back to yah's own.
    let gone = Prefs { dark_theme: "user:gone".into(), ..Prefs::default() };
    assert_eq!(Tokens::for_prefs(&gone, None, &[]), Tokens::dark());
}

#[test]
fn hostile_knobs_are_clamped() {
    for bad in [f32::NAN, f32::INFINITY, -3.0, 0.0, 1e9] {
        let k = Knobs { font_scale: bad, code_scale: bad, radius: bad, sidebar_width: bad, pane_share: bad, max_width: bad, ..Knobs::default() }.sanitized();
        assert!(k.font_scale.is_finite() && Knobs::SCALE_RANGE.contains(&k.font_scale), "{bad}");
        assert!(Knobs::SCALE_RANGE.contains(&k.code_scale) && Knobs::RADIUS_RANGE.contains(&k.radius), "{bad}");
        assert!(Knobs::SIDEBAR_RANGE.contains(&k.sidebar_width) && Knobs::PANE_RANGE.contains(&k.pane_share), "{bad}");
        assert!(k.max_width == 0.0 || (480.0..=2000.0).contains(&k.max_width), "{bad}");
    }
    let p = Prefs {
        accent: Accent::Custom("not a colour".into()),
        dark_theme: String::new(),
        overrides: [("nord".to_owned(), [("text".to_owned(), "#zzz".to_owned()), ("nope".to_owned(), "#fff".to_owned())].into())].into(),
        imported: vec!["x".repeat(Prefs::MAX_THEME_BYTES + 1)],
        ..Prefs::default()
    }
    .sanitized();
    assert_eq!((p.accent, p.dark_theme.as_str()), (Accent::Theme, "yah-dark"));
    assert!(p.overrides.is_empty() && p.imported.is_empty());
}

#[test]
fn prefs_round_trip_in_json_and_ron() {
    let prefs = Prefs {
        theme: ThemeKind::Light,
        light_theme: "catppuccin-latte".into(),
        accent: Accent::Named("mauve".into()),
        knobs: Knobs { font_scale: 1.3, radius: 10.0, user_bubble: BubbleStyle::Outlined, align: Align::Chat, reduce_motion: true, ..Knobs::default() },
        overrides: [("catppuccin-latte".to_owned(), [("text".to_owned(), "#112233".to_owned())].into())].into(),
        imported: vec![r#"{"name":"x"}"#.into()],
        ..Prefs::default()
    };
    let json = serde_json::to_string(&prefs).unwrap();
    assert_eq!(serde_json::from_str::<Prefs>(&json).unwrap().sanitized(), prefs);
    let ron_text = ron::to_string(&prefs).unwrap();
    assert_eq!(ron::from_str::<Prefs>(&ron_text).unwrap().sanitized(), prefs);
    assert!(!ron_text.contains("high_contrast:Some") && !ron_text.contains("font_scale:None"), "{ron_text}");
}

/// Version 0, as eframe stored it (RON): `Prefs { theme, accent, font_scale, high_contrast }`.
#[test]
fn version_0_prefs_migrate_without_losing_anything() {
    let v0 = "(theme:Light,accent:Teal,font_scale:1.3,high_contrast:true)";
    let p = ron::from_str::<Prefs>(v0).unwrap().sanitized();
    assert_eq!((p.version, p.theme, &p.accent), (Prefs::VERSION, ThemeKind::Light, &Accent::Teal));
    assert_eq!((p.knobs.font_scale, p.knobs.high_contrast), (1.3, true));
    assert_eq!((p.dark_theme.as_str(), p.light_theme.as_str()), ("yah-dark", "yah-light"));
    // Same colours as before: yah's light theme, the old teal accent.
    assert!(!Tokens::for_prefs(&p, None, &[]).dark);
    let plain = Tokens::for_prefs(&Prefs { knobs: Knobs { high_contrast: false, ..p.knobs }, ..p.clone() }, None, &[]);
    assert_eq!((plain.app_bg, plain.accent), (rgb(0xf6f6f4), rgb(0x0d6b63)));
    // And it saves in the new shape, which loads back the same.
    let again = ron::from_str::<Prefs>(&ron::to_string(&p).unwrap()).unwrap().sanitized();
    assert_eq!(again, p);
    // Partial and empty storage keep defaults; garbage fields are ignored.
    assert_eq!(ron::from_str::<Prefs>("(theme:Dark)").unwrap().sanitized(), Prefs { theme: ThemeKind::Dark, ..Prefs::default() });
    assert_eq!(ron::from_str::<Prefs>("()").unwrap().sanitized(), Prefs::default());
    assert_eq!(serde_json::from_str::<Prefs>(r#"{"theme":"Dark","future_field":[1,2]}"#).unwrap().sanitized().theme, ThemeKind::Dark);
    for accent in ["Blue", "Violet", "Green", "Orange", "Pink"] {
        assert!(ron::from_str::<Prefs>(&format!("(accent:{accent})")).is_ok(), "{accent}");
    }
}

#[test]
fn apply_sets_the_egui_theme_and_sizes() {
    let ctx = egui::Context::default();
    Tokens::light().apply(&ctx);
    assert_eq!(ctx.theme(), egui::Theme::Light);
    assert_eq!(ctx.style_of(egui::Theme::Light).visuals.panel_fill, Tokens::light().panel_bg);
    let big = Tokens::derive(&presets::YAH_DARK, Color32::BLUE, &Knobs { font_scale: 1.2, code_scale: 1.5, reduce_motion: true, ..Knobs::default() });
    big.apply(&ctx);
    assert_eq!(ctx.theme(), egui::Theme::Dark);
    let style = ctx.style_of(egui::Theme::Dark);
    assert!((style.text_styles[&TextStyle::Body].size - 16.8).abs() < 0.01);
    assert!((style.text_styles[&TextStyle::Monospace].size - 19.5).abs() < 0.01);
    assert_eq!(style.animation_time, 0.0);
}

#[test]
fn every_role_reads_and_writes() {
    let mut names: Vec<&str> = ROLES.iter().map(|r| r.name).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), ROLES.len());
    let mut t = Tokens::dark();
    for r in ROLES {
        assert!(t.get(r.name).is_some(), "{}", r.name);
        assert!(t.set(r.name, Color32::from_rgb(1, 2, 3)), "{}", r.name);
        assert_eq!(t.get(r.name), Some(Color32::from_rgb(1, 2, 3)), "{}", r.name);
    }
    assert!(!t.set("chart9", Color32::RED) && !t.set("chart0", Color32::RED) && t.get("nope").is_none());
}

#[test]
fn colours_parse_strictly() {
    assert_eq!(parse_color("#1e1e2e"), Some(rgb(0x1e1e2e)));
    assert_eq!(parse_color(" CBA6F7 "), Some(rgb(0xcba6f7)));
    assert_eq!(parse_color("#fff"), Some(Color32::WHITE));
    assert_eq!(parse_color("#00000080").map(|c| c.to_srgba_unmultiplied()), Some([0, 0, 0, 128]));
    for bad in ["", "#", "#12", "#12345", "#gggggg", "red", "#1234567", "#ffffff00ff", "＃ffffff", "#ff ff ff"] {
        assert_eq!(parse_color(bad), None, "{bad}");
    }
    for c in [rgb(0x123456), Color32::from_rgba_unmultiplied(10, 20, 30, 40)] {
        assert_eq!(parse_color(&hex(c)).map(|p| p.to_srgba_unmultiplied()), Some(c.to_srgba_unmultiplied()));
    }
}

#[test]
fn theme_files_round_trip() {
    let def = ThemeDef::builtin(&presets::CATPPUCCIN_MOCHA);
    let prefs = Prefs {
        accent: Accent::Named("peach".into()),
        knobs: Knobs { radius: 11.0, density: Density::Spacious, ..Knobs::default() },
        overrides: [("catppuccin-mocha".to_owned(), [("app_bg".to_owned(), "#101018".to_owned()), ("chart3".to_owned(), "#a6e3a1".to_owned())].into())].into(),
        ..Prefs::default()
    };
    let text = file::export(&def, &prefs, "Mocha, darker");
    let f = file::parse(&text, Origin::Imported(0)).unwrap();
    assert!(f.warnings.is_empty(), "{:?}", f.warnings);
    assert_eq!((f.def.id.as_str(), f.def.name.as_str(), f.def.dark, f.def.base.id), ("user:mocha-darker", "Mocha, darker", true, "catppuccin-mocha"));
    // Small: only what differs from the base preset.
    assert_eq!(f.def.colors.len(), 1, "{text}");
    // Using the file with its knobs looks exactly like the original.
    let mut knobs = Knobs::default();
    file::apply_knobs(f.def.knobs.as_ref().unwrap(), &mut knobs, &mut Vec::new());
    let imported = Tokens::build(&f.def, &Prefs { knobs, ..Prefs::default() });
    assert_eq!(imported, Tokens::build(&def, &prefs));
}

#[test]
fn malformed_theme_files_warn_and_fall_back() {
    for bad in ["", "not json", "[1,2]", "\"str\"", "{"] {
        assert!(file::parse(bad, Origin::Imported(0)).is_err(), "{bad}");
    }
    assert!(file::parse(&format!("{{\"name\":\"{}\"}}", "x".repeat(Prefs::MAX_THEME_BYTES)), Origin::Imported(0)).is_err());
    let text = r##"{
        "name": 7, "base": "no-such-theme", "dark": "yes", "accent": "blue",
        "colors": { "text": "#zzzzzz", "bogus_role": "#ffffff", "app_bg": "#010203", "accent": "#ff8800", "chart2": 5 },
        "knobs": { "radius": "round", "density": "Compact", "nope": 1, "font_scale": 99 },
        "extra": true
    }"##;
    let f = file::parse(text, Origin::Imported(0)).unwrap();
    let w = f.warnings.join("\n");
    for expect in ["name", "unknown base", "\"dark\"", "\"accent\"", "colors.text", "colors.bogus_role", "colors.chart2", "knobs.radius", "knobs.nope", "extra"]
    {
        assert!(w.contains(expect), "missing warning about {expect}:\n{w}");
    }
    assert_eq!((f.def.name.as_str(), f.def.base.id, f.def.dark), ("Untitled theme", "yah-dark", true));
    assert_eq!(f.def.colors, vec![("app_bg".to_owned(), rgb(0x010203))]);
    // `colors.accent` sets the theme's accent (it drives hover, selection, your bubbles).
    assert_eq!(f.def.accent, Some(rgb(0xff8800)));
    // Valid knobs survive their bad neighbours (and are clamped).
    let mut k = Knobs::default();
    file::apply_knobs(f.def.knobs.as_ref().unwrap(), &mut k, &mut Vec::new());
    assert_eq!((k.density, k.font_scale, k.radius), (Density::Compact, 2.0, 6.0));
    // Missing roles come from the base preset.
    let t = Tokens::build(&f.def, &Prefs::default());
    assert_eq!((t.app_bg, t.text), (rgb(0x010203), Tokens::dark().text));
    // Light theme with an unknown base falls back to yah's light one.
    let light = file::parse(r#"{"name":"L","base":42,"dark":false}"#, Origin::Imported(0)).unwrap();
    assert_eq!((light.def.base.id, light.def.dark), ("yah-light", false));
}

#[test]
fn user_themes_are_listed_and_resolved() {
    let prefs =
        Prefs { imported: vec![r#"{"name":"Mine","base":"dracula"}"#.into(), "garbage".into(), r#"{"name":"Mine","base":"nord"}"#.into()], ..Prefs::default() };
    let (defs, warnings) = file::imported(&prefs);
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].base.id, "dracula");
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert_eq!(file::all(&defs).len(), PRESETS.len() + 1);
    let p = Prefs { dark_theme: "user:mine".into(), ..Prefs::default() };
    assert_eq!(Tokens::for_prefs(&p, None, &defs).app_bg, rgb(0x282a36));
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn themes_folder_loads_json_files() {
    let dir = std::env::temp_dir().join(format!("yah-themes-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.json"), r#"{"name":"A","base":"nord","colors":{"text":"nope"}}"#).unwrap();
    std::fs::write(dir.join("b.json"), "nope").unwrap();
    std::fs::write(dir.join("c.txt"), r#"{"name":"C"}"#).unwrap();
    let (defs, warnings) = file::load_dir(&dir);
    assert_eq!(defs.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["A"]);
    assert_eq!(defs[0].origin, Origin::Folder("a.json".into()));
    assert!(warnings.iter().any(|w| w.starts_with("a.json: colors.text")) && warnings.iter().any(|w| w.starts_with("b.json")), "{warnings:?}");
    assert!(file::load_dir(&dir.join("missing")).0.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn themes_select_by_cli_name() {
    let mut p = Prefs::default();
    assert!(file::select(&mut p, "catppuccin-latte", &[]));
    assert_eq!((p.theme, p.light_theme.as_str()), (ThemeKind::Light, "catppuccin-latte"));
    assert!(file::select(&mut p, "Dark", &[]) && p.theme == ThemeKind::Dark);
    let (mine, _) = file::imported(&Prefs { imported: vec![r#"{"name":"My Mocha","base":"catppuccin-mocha"}"#.into()], ..Prefs::default() });
    assert!(file::select(&mut p, "my mocha", &mine));
    assert_eq!(p.dark_theme, "user:my-mocha");
    assert!(!file::select(&mut p, "no-such", &mine));
}

#[test]
fn slugs() {
    assert_eq!(file::slug("Mocha, warmer!"), "mocha-warmer");
    assert_eq!(file::slug("  "), "theme");
    assert_eq!(file::slug("Rosé Pine"), "rosé-pine");
    assert!(file::slug(&"x".repeat(500)).len() <= 64);
}
