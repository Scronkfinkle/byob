//! The real window drawn across arrangements, themes and sizes (and a few other states:
//! Settings, the Projects page, an artifact open, edit mode), each checked for structural
//! invariants, so a layout regression fails here without anyone looking at it.
//!
//! What is checked comes from fluid's element list (AccessKit's widgets, byob's named
//! regions, clickable rects and painted text) and the shapes painted this frame.

use egui::{Pos2, Rect, Shape, Vec2};

use super::*;
use crate::cockpit::arrange::{Arrangement, Bezel};
use crate::fluid::{self, Element};

/// What the window shows besides the chat.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Extra {
    None,
    Settings,
    Projects,
    Artifact,
    Edit,
}

#[derive(Clone, Copy, Debug)]
struct Case {
    /// `docked`, or a free preset.
    arrangement: &'static str,
    theme: theme::ThemeKind,
    size: Vec2,
    extra: Extra,
}

impl std::fmt::Display for Case {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{} {:?} {}x{} {:?}]", self.arrangement, self.theme, self.size.x, self.size.y, self.extra)
    }
}

const SIZES: [Vec2; 2] = [Vec2::new(800.0, 600.0), Vec2::new(1440.0, 900.0)];
const ARRANGEMENTS: [&str; 3] = ["docked", "floating", "focus"];

fn cases() -> Vec<Case> {
    let mut out = Vec::new();
    for arrangement in ARRANGEMENTS {
        for theme in [theme::ThemeKind::Light, theme::ThemeKind::Dark] {
            for size in SIZES {
                out.push(Case { arrangement, theme, size, extra: Extra::None });
            }
        }
    }
    for extra in [Extra::Settings, Extra::Projects, Extra::Artifact, Extra::Edit] {
        for arrangement in ["docked", "floating"] {
            out.push(Case { arrangement, theme: theme::ThemeKind::Dark, size: SIZES[1], extra });
        }
    }
    out
}

/// `n` frames; what the last one painted, in paint order.
fn run(ctx: &egui::Context, app: &mut ByobApp, size: Vec2, n: usize) -> Vec<egui::epaint::ClippedShape> {
    let mut shapes = Vec::new();
    for _ in 0..n {
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)), ..Default::default() };
        let mut out = ctx.run_ui(input, |ui| app.ui(ui));
        out.textures_delta.clear();
        shapes = out.shapes;
    }
    shapes
}

/// A window drawn in a case: its context, app, fluid's elements and the last frame's shapes.
struct Drawn {
    ctx: egui::Context,
    app: ByobApp,
    els: Vec<Element>,
    shapes: Vec<egui::epaint::ClippedShape>,
}

/// The crowded chat in `case`, drawn until fluid has listed its elements; `edit` changes
/// the arrangement before the first frame.
fn render(case: Case, edit: impl FnOnce(&mut Arrangement)) -> Drawn {
    let ctx = egui::Context::default();
    let mut app = crowded_app();
    let mut prefs = app.prefs().clone();
    prefs.theme = case.theme;
    prefs.cockpit.arrangement = Arrangement::preset(case.arrangement).unwrap_or_default();
    edit(&mut prefs.cockpit.arrangement);
    app.set_prefs(prefs);
    run(&ctx, &mut app, case.size, 1);
    match case.extra {
        Extra::None => {}
        Extra::Settings => app.open_settings_at(settings::Tab::Text),
        Extra::Projects => app.page = Page::Projects,
        Extra::Artifact => assert!(app.artifacts.open("a"), "{case}: the artifact opens"),
        Extra::Edit => {
            fluid::with(&ctx, |f| f.editing = true);
        }
    }
    fluid::with(&ctx, fluid::Fluid::wake);
    let shapes = run(&ctx, &mut app, case.size, 6);
    let (fresh, els) = fluid::with(&ctx, |f| (f.fresh(), f.elements.clone())).unwrap_or_default();
    assert!(fresh && !els.is_empty(), "{case}: fluid listed the window");
    Drawn { ctx, app, els, shapes }
}

/// A run of text painted: what, where, clipped to what, how big, and its place in the
/// paint order.
struct Painted {
    text: String,
    rect: Rect,
    clip: Rect,
    size: f32,
    index: usize,
}

fn painted_text(shapes: &[egui::epaint::ClippedShape]) -> Vec<Painted> {
    fn walk(shape: &Shape, clip: Rect, index: usize, out: &mut Vec<Painted>) {
        match shape {
            Shape::Text(t) if !t.galley.job.text.trim().is_empty() => {
                let size = t.galley.job.sections.iter().map(|s| s.format.font_id.size).fold(f32::INFINITY, f32::min);
                out.push(Painted { text: t.galley.job.text.clone(), rect: t.visual_bounding_rect(), clip, size, index });
            }
            Shape::Vec(v) => v.iter().for_each(|s| walk(s, clip, index, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    for (i, cs) in shapes.iter().enumerate() {
        walk(&cs.shape, cs.clip_rect, i, &mut out);
    }
    out
}

/// Something opaque painted over `p`'s middle after it (a panel or plate drawn on top).
fn covered(shapes: &[egui::epaint::ClippedShape], p: &Painted) -> Option<Rect> {
    let at = p.rect.center();
    shapes.iter().skip(p.index + 1).find_map(|cs| match &cs.shape {
        Shape::Rect(r) if r.fill.a() >= 200 && r.rect.contains(at) && cs.clip_rect.contains(at) => Some(r.rect),
        _ => None,
    })
}

/// The window controls, by the label fluid lists them with, that are drawn exactly once on
/// the chat screen wherever the arrangement puts them.
const CONTROLS: [&str; 6] = ["sidebar", "back", "forward", "New chat", "Search", "more"];

fn check(case: Case, d: &Drawn) -> Vec<String> {
    let (app, els) = (&d.app, &d.els);
    let mut bad = Vec::new();
    let screen = Rect::from_min_size(Pos2::ZERO, case.size);
    let shown: Vec<&Element> = els.iter().filter(|e| !e.hidden && !e.collapsed && e.shown.is_positive()).collect();
    // Buttons, enabled or not (Back with nothing to go back to is still drawn).
    let buttons: Vec<&Element> = shown.iter().copied().filter(|e| e.role == "button").collect();
    if matches!(case.extra, Extra::None | Extra::Artifact | Extra::Edit) {
        // Each window control once (the list toggle, Back and Forward were drawn both in the
        // header and the chat list in free arrangements). The list's own (New chat, Search)
        // only while the list is shown.
        let list = shown.iter().any(|e| e.path == "sidebar");
        for name in CONTROLS {
            let found: Vec<&str> = buttons.iter().filter(|e| e.label == name).map(|e| e.path.as_str()).collect();
            let want = usize::from(list || !matches!(name, "New chat" | "Search"));
            if found.len() != want {
                bad.push(format!("{case}: {name:?} drawn {} times (want {want}): {found:?}", found.len()));
            }
        }
        // Settings: the gear or status row at the foot of the chat list.
        let settings = app.sidebar.rects.settings;
        if list && !(settings.is_positive() && screen.contains_rect(settings)) {
            bad.push(format!("{case}: no settings button at the foot of the list: {settings:?}"));
        }
    }
    // Interactive elements are in the window: across it, and up and down in the parts that
    // don't scroll (scrolled content is laid out past the edge and clipped).
    let fixed = |e: &Element| ["header/", "composer/"].iter().any(|p| e.path.starts_with(p));
    for e in shown.iter().filter(|e| e.interactive && e.role != "area" && e.role != "widget") {
        let (r, w) = (e.shown, screen.expand(1.0));
        let across = r.min.x < w.min.x || r.max.x > w.max.x;
        let down = r.min.y < w.min.y || r.max.y > w.max.y;
        if across || (down && fixed(e)) {
            bad.push(format!("{case}: {} is outside the window: {r:?}", e.path));
        }
    }
    // Two buttons aren't stacked on one another (same center, neither inside the other).
    for (i, a) in buttons.iter().enumerate() {
        for b in &buttons[i + 1..] {
            let nested = a.shown.expand(1.0).contains_rect(b.shown) || b.shown.expand(1.0).contains_rect(a.shown);
            if a.shown.center().distance(b.shown.center()) < 2.0 && !nested {
                bad.push(format!("{case}: {} and {} drawn on one another at {:?}", a.path, b.path, a.shown));
            }
        }
    }
    // No text drawn with no size, or out of the window where it isn't clipped away.
    for p in painted_text(&d.shapes) {
        let what = format!("{:?}", p.text.chars().take(40).collect::<String>());
        if !(p.size.is_finite() && p.size >= 5.0) || !p.rect.is_finite() || p.rect.width() <= 0.0 || p.rect.height() <= 0.0 {
            bad.push(format!("{case}: text {what} drawn at size {} in {:?}", p.size, p.rect));
            continue;
        }
        let seen = p.rect.intersect(p.clip);
        if seen.is_positive() && !screen.expand(1.0).contains_rect(seen) && seen.intersect(screen).area() < seen.area() * 0.5 {
            bad.push(format!("{case}: text {what} is off screen at {:?}", p.rect));
        }
    }
    // Docked, the panels don't overlap.
    if case.arrangement == "docked" && case.extra != Extra::Settings {
        let panels: Vec<&Element> = shown.iter().copied().filter(|e| e.is_region() && e.depth == 0).collect();
        for (i, a) in panels.iter().enumerate() {
            for b in &panels[i + 1..] {
                if a.shown.intersect(b.shown).area() > 1.0 {
                    bad.push(format!("{case}: panels {} {:?} and {} {:?} overlap", a.path, a.shown, b.path, b.shown));
                }
            }
        }
    }
    bad
}

/// Every case in [`cases`] against every invariant in [`check`]; all failures reported at
/// once, each with its case and the elements' paths.
#[test]
fn the_window_keeps_its_invariants_across_the_matrix() {
    let started = std::time::Instant::now();
    let mut bad = Vec::new();
    let all = cases();
    for case in &all {
        bad.extend(check(*case, &render(*case, |_| {})));
    }
    assert!(bad.is_empty(), "{} problems in {} cases:\n{}", bad.len(), all.len(), bad.join("\n"));
    assert!(started.elapsed() < std::time::Duration::from_secs(20), "the matrix took {:?}", started.elapsed());
}

/// Edit mode's inspector covers part of the window: what is under it is still listed (it
/// used to vanish from the elements, so it couldn't be picked or changed).
#[test]
fn edit_mode_still_lists_what_is_under_the_inspector() {
    // The message box at the top right, under where the inspector opens: its model picker
    // is drawn by byob (no AccessKit node), the kind that went missing.
    let case = Case { arrangement: "floating", theme: theme::ThemeKind::Dark, size: SIZES[1], extra: Extra::None };
    let Drawn { ctx, mut app, els: before, .. } = render(case, |a| {
        a.places.insert("composer".into(), crate::cockpit::arrange::Spot::At([22.0, 8.5, 77.0, 30.0]));
        a.places.insert("chat".into(), crate::cockpit::arrange::Spot::At([22.0, 40.0, 77.0, 58.0]));
    });
    fluid::with(&ctx, |f| f.editing = true);
    run(&ctx, &mut app, case.size, 6);
    let editing = fluid::with(&ctx, |f| f.elements.clone()).unwrap_or_default();
    let inspector = ctx.memory(|m| m.area_rect(fluid::inspector_id())).unwrap();
    let under: Vec<&str> = before.iter().filter(|e| e.interactive && inspector.contains_rect(e.shown)).map(|e| e.path.as_str()).collect();
    assert!(under.len() >= 2, "{case}: byob's buttons under the inspector at {inspector:?}: {under:?}");
    let gone: Vec<&&str> = under.iter().filter(|p| !editing.iter().any(|e| e.path == **p)).collect();
    assert!(gone.is_empty(), "{case}: under the inspector {inspector:?}, no longer listed: {gone:?} (of {under:?})");
}

/// A free arrangement with a label stamped on every bezel: each one shown is drawn, in the
/// window, not clipped away and not painted over (a tall metal header covered the chat
/// list's and the chat's; a bare bezel's wasn't drawn at all), and the header and message
/// box under theirs aren't cut off (a labeled message box lost its bottom row).
#[test]
fn every_bezel_label_set_is_drawn() {
    // Module, its label, the region it's drawn as.
    let labels = [("header", "DSV-9", "header"), ("chats", "LOGBOOK", "sidebar"), ("chat", "COMMS", "chat"), ("composer", "HELM", "composer")];
    let mut bad = Vec::new();
    for arrangement in ["floating", "focus"] {
        for bezel in Bezel::ALL {
            for size in SIZES {
                let case = Case { arrangement, theme: theme::ThemeKind::Dark, size, extra: Extra::None };
                let d = render(case, |a| {
                    for (m, l, _) in labels {
                        a.labels.insert(m.into(), l.into());
                        a.bezels.insert(m.into(), bezel);
                    }
                });
                let screen = Rect::from_min_size(Pos2::ZERO, size);
                let text = painted_text(&d.shapes);
                for (m, label, region) in labels {
                    if !d.els.iter().any(|e| e.path == region) {
                        continue; // not shown (hidden in this arrangement, or the list closed)
                    }
                    let at = format!("{case} {bezel:?}: label.{m} {label:?}");
                    match text.iter().find(|p| p.text == label) {
                        None => bad.push(format!("{at} isn't drawn")),
                        Some(p) if !p.clip.contains_rect(p.rect) || !screen.contains_rect(p.rect) => {
                            bad.push(format!("{at} is clipped or off screen: {:?} in {:?}", p.rect, p.clip));
                        }
                        Some(p) => {
                            if let Some(over) = covered(&d.shapes, p) {
                                bad.push(format!("{at} at {:?} is painted over by {over:?}", p.rect));
                            }
                        }
                    }
                }
                // What the header and the message box hold isn't cut off to make room for a
                // label (they don't scroll: what's clipped can't be reached).
                for region in ["header", "composer"] {
                    let Some(r) = d.els.iter().find(|e| e.path == region).map(|e| e.rect) else { continue };
                    for p in text.iter().filter(|p| r.contains(p.rect.center()) && labels.iter().all(|l| l.1 != p.text)) {
                        if !p.clip.expand(1.0).contains_rect(p.rect) {
                            bad.push(format!("{case} {bezel:?}: {region}'s {:?} is cut off: {:?} in {:?}", p.text, p.rect, p.clip));
                        }
                    }
                }
            }
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}
