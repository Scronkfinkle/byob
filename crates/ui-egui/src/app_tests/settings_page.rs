use super::*;
use crate::settings::Tab;

/// One frame `width` × 700; every piece of text it drew.
fn drawn_text(ctx: &egui::Context, app: &mut ByobApp, width: f32) -> String {
    fn walk(shape: &egui::Shape, out: &mut String) {
        match shape {
            egui::Shape::Text(t) => {
                out.push_str(t.galley.text());
                out.push('\n');
            }
            egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, out)),
            _ => {}
        }
    }
    let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 700.0))), ..Default::default() };
    let mut out = ctx.run_ui(input, |ui| app.ui(ui));
    out.textures_delta.clear();
    let mut text = String::new();
    for s in &out.shapes {
        walk(&s.shape, &mut text);
    }
    text
}

/// Text only each tab draws (its description).
const MARKERS: [(Tab, &str); 6] = [
    (Tab::Models, "The model server byob talks to"),
    (Tab::Defaults, "Saved in settings.json"),
    (Tab::Plugins, "What the chat in view can use"),
    (Tab::Appearance, "How byob looks"),
    (Tab::Text, "Sizes, fonts and how the window"),
    (Tab::Colours, "Every colour of"),
];

#[test]
fn only_the_selected_tab_draws() {
    let ctx = egui::Context::default();
    let mut app = crowded_app();
    app.show_settings = true;
    for width in [1000.0, 390.0] {
        for (tab, _) in MARKERS {
            app.appearance.tab = tab;
            drawn_text(&ctx, &mut app, width);
            let text = drawn_text(&ctx, &mut app, width);
            for (other, marker) in MARKERS {
                assert_eq!(text.contains(marker), other == tab, "width {width}, tab {tab:?}: {marker:?} drawn {}", text.contains(marker));
            }
        }
    }
}

#[test]
fn the_settings_cover_everything_beside_the_chat_list_and_the_chat_is_not_drawn_under_them() {
    let ctx = egui::Context::default();
    let mut app = crowded_app();
    frame(&ctx, &mut app, 1000.0);
    frame(&ctx, &mut app, 1000.0);
    assert!(app.header_rects.field.is_positive(), "the composer shows without the settings");
    app.show_settings = true;
    for (width, list) in [(1000.0, true), (1000.0, false), (390.0, true), (390.0, false)] {
        app.sidebar.set_open(list);
        app.header_rects.field = egui::Rect::NOTHING;
        app.header_rects.attach = egui::Rect::NOTHING;
        let text = drawn_text(&ctx, &mut app, width);
        drawn_text(&ctx, &mut app, width);
        let page = app.header_rects.settings_page;
        let header = app.header_rects.chats;
        // Right below the header, down to the bottom and to the right edge.
        assert!((page.top() - header.bottom()).abs() < 12.0, "width {width}, list {list}: page {page:?} under header {header:?}");
        assert!((page.bottom() - 700.0).abs() < 1.0 && (page.right() - width).abs() < 1.0, "width {width}, list {list}: {page:?}");
        // Beside the chat list on a desktop, the whole width on a phone or with the list shut.
        let left = if list && width > sidebar::NARROW { 180.0..=430.0 } else { 0.0..=1.0 };
        assert!(left.contains(&page.left()), "width {width}, list {list}: {page:?}");
        // Nothing of the chat pane: no composer, no starters.
        assert_eq!((app.header_rects.field, app.header_rects.attach), (egui::Rect::NOTHING, egui::Rect::NOTHING), "width {width}, list {list}");
        assert!(!text.contains("Message byob"), "width {width}, list {list}: the composer drew");
    }
}

#[test]
fn every_way_into_the_settings_selects_its_tab() {
    let ctx = egui::Context::default();
    let mut app = crowded_app();
    frame(&ctx, &mut app, 1000.0);
    let mut check = |name: &str, open: &dyn Fn(&mut ByobApp), want: Tab| {
        app.show_settings = false;
        app.appearance.tab = if want == Tab::Colours { Tab::Models } else { Tab::Colours };
        open(&mut app);
        frame(&ctx, &mut app, 1000.0);
        frame(&ctx, &mut app, 1000.0);
        assert!(app.show_settings, "{name}");
        assert_eq!(app.appearance.tab, want, "{name}");
    };
    // The ⋯ menu's Models… / the model chip's Manage models… / a failed reply's card.
    check("open_connection", &|a| a.open_connection(), Tab::Models);
    // The ⋯ menu's Plugins…, the header chip, a card's Files.
    check("plugins.open", &|a| a.plugins.open(Some("weather")), Tab::Plugins);
    for (panel, want) in [("models", Tab::Models), ("edit-model:x", Tab::Models), ("add-models:http://127.0.0.1:1/v1", Tab::Models), ("plugins", Tab::Plugins)]
    {
        check(panel, &|a| assert!(a.open_panel(panel), "{panel}"), want);
    }
    check("plugin:weather", &|a| assert!(a.open_panel("plugin:weather")), Tab::Plugins);
    for (section, want) in [
        ("models", Tab::Models),
        ("defaults", Tab::Defaults),
        ("plugins", Tab::Plugins),
        ("appearance", Tab::Appearance),
        ("text", Tab::Text),
        ("colours", Tab::Colours),
        ("knobs", Tab::Text),
        ("advanced", Tab::Colours),
        ("model:local", Tab::Models),
    ] {
        check(section, &|a| assert!(a.reveal_settings(section), "{section}"), want);
    }
    assert!(!app.reveal_settings("nope"));
    assert_eq!(app.models.reveal.as_deref(), Some("local"), "model:<id> opens that model's form once listed");
}

#[test]
fn the_tab_is_remembered_and_clicking_one_selects_it() {
    let ctx = egui::Context::default();
    let mut app = crowded_app();
    app.show_settings = true;
    for width in [1000.0, 390.0] {
        app.appearance.tab = Tab::Models;
        frame(&ctx, &mut app, width);
        frame(&ctx, &mut app, width);
        let plugins = app.header_rects.settings_tabs[Tab::Plugins as usize];
        assert!(plugins.is_positive() && plugins.right() <= width, "width {width}: {plugins:?}");
        click(&ctx, &mut app, width, plugins.center());
        assert_eq!(app.appearance.tab, Tab::Plugins, "width {width}");
        // The content column starts right of the tab list on a desktop, under the strip on a phone.
        let (body, tab) = (app.header_rects.settings_body, app.header_rects.settings_tabs[Tab::Plugins as usize]);
        if width > sidebar::NARROW {
            assert!(body.left() > tab.right(), "{body:?} {tab:?}");
        } else {
            assert!(body.top() > tab.bottom(), "{body:?} {tab:?}");
        }
    }
    // Closed and opened again (Ctrl+, / App settings): the same tab.
    app.show_settings = false;
    frame(&ctx, &mut app, 1000.0);
    app.set_settings_open(true);
    frame(&ctx, &mut app, 1000.0);
    assert_eq!(app.appearance.tab, Tab::Plugins);
}

#[test]
fn a_deep_link_scrolls_the_phone_strip_to_its_tab() {
    let ctx = egui::Context::default();
    let mut app = crowded_app();
    assert!(app.reveal_settings("colours"));
    for _ in 0..4 {
        frame(&ctx, &mut app, 390.0);
    }
    let r = app.header_rects.settings_tabs[Tab::Colours as usize];
    assert!(r.left() >= 0.0 && r.right() <= 390.5, "{r:?}");
}

#[test]
fn the_content_column_stays_readable_in_a_wide_window() {
    let ctx = egui::Context::default();
    let mut app = crowded_app();
    app.show_settings = true;
    app.appearance.tab = Tab::Defaults;
    frame(&ctx, &mut app, 2000.0);
    frame(&ctx, &mut app, 2000.0);
    let body = app.header_rects.settings_body;
    assert!(body.width() <= form::MAX_W + 0.5, "{body:?}");
}
