use super::*;
use crate::settings::Tab;
use crate::settings::registry::{self, Key};

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

/// Text only each page draws (its description).
const MARKERS: [(Tab, &str); 6] = [
    (Tab::Models, "The model server byob talks to"),
    (Tab::Defaults, "Defaults for every chat"),
    (Tab::Plugins, "What the chat in view can use"),
    (Tab::Appearance, "How byob looks"),
    (Tab::Text, "Sizes, fonts and how the window"),
    (Tab::Colours, "Every colour of"),
];

/// [`crowded_app`] with saved models and the global settings the engine would send, so every
/// setting has something to draw.
fn full_app() -> ByobApp {
    let fake = Fake::default();
    let view = |id: &str| byob_protocol::profiles::ProfileView {
        profile: byob_protocol::Profile {
            id: id.into(),
            name: format!("Model {id}"),
            base_url: "http://h:1/v1".into(),
            model: "m".into(),
            ..Default::default()
        },
        has_key: false,
        link: Link::Unknown,
        info: None,
    };
    let list = byob_protocol::profiles::ProfileList { items: vec![view("local"), view("cloud")], roles: [("main".to_owned(), "local".to_owned())].into() };
    let config = crate::workspace::tests::view(byob_protocol::ConfigScope::Global, byob_protocol::workspace::Layer::default(), None);
    let app = crowded_with(fake.clone());
    fake.inbox.lock().unwrap().extend([ServerMsg::Profiles(Box::new(list)), ServerMsg::Config(Box::new(config))]);
    app
}

/// A few frames (the engine's answers arrive, rows settle their sizes).
fn settle(ctx: &egui::Context, app: &mut ByobApp, width: f32) {
    for _ in 0..4 {
        frame(ctx, app, width);
    }
}

fn press_key(ctx: &egui::Context, app: &mut ByobApp, width: f32, key: egui::Key) {
    let event = egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE };
    let input =
        egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 700.0))), events: vec![event], ..Default::default() };
    let mut out = ctx.run_ui(input, |ui| app.ui(ui));
    out.textures_delta.clear();
}

/// Where the last frame drew `key`'s setting (each once).
fn drawn(app: &ByobApp, key: Key) -> Vec<egui::Rect> {
    app.appearance.drawn.iter().filter(|(k, _)| *k == key).map(|(_, r)| *r).collect()
}

#[test]
fn only_the_selected_page_draws() {
    let ctx = egui::Context::default();
    let mut app = crowded_app();
    for width in [1000.0, 390.0] {
        for (tab, _) in MARKERS {
            app.open_settings_at(tab);
            drawn_text(&ctx, &mut app, width);
            let text = drawn_text(&ctx, &mut app, width);
            for (other, marker) in MARKERS {
                assert_eq!(text.contains(marker), other == tab, "width {width}, tab {tab:?}: {marker:?} drawn {}", text.contains(marker));
            }
        }
    }
}

#[test]
fn the_settings_replace_the_chat_and_its_list_below_the_header() {
    let ctx = egui::Context::default();
    let mut app = crowded_app();
    frame(&ctx, &mut app, 1000.0);
    frame(&ctx, &mut app, 1000.0);
    assert!(app.composer_rects.field.is_positive(), "the composer shows without the settings");
    for (width, list) in [(1000.0, true), (1000.0, false), (390.0, true), (390.0, false)] {
        app.close_settings();
        frame(&ctx, &mut app, width);
        app.sidebar.set_open(list);
        app.open_settings_at(Tab::Appearance);
        app.composer_rects.field = egui::Rect::NOTHING;
        app.composer_rects.attach = egui::Rect::NOTHING;
        drawn_text(&ctx, &mut app, width);
        let text = drawn_text(&ctx, &mut app, width);
        let (page, back, header) = (app.header_rects.settings_page, app.header_rects.settings_back, app.header_rects.more);
        // Right below the header, down to the bottom and to the right edge.
        assert!((page.top() - header.bottom()).abs() < 12.0, "width {width}, list {list}: page {page:?} under header {header:?}");
        assert!((page.bottom() - 700.0).abs() < 1.0 && (page.right() - width).abs() < 1.0, "width {width}, list {list}: {page:?}");
        if width > sidebar::NARROW {
            // The settings' own navigation where the chat list was, open or not.
            assert!((200.0..=430.0).contains(&page.left()), "width {width}, list {list}: {page:?}");
            assert!(back.is_positive() && back.right() <= page.left(), "width {width}, list {list}: {back:?}");
        } else {
            assert!(page.left() < 1.0, "width {width}, list {list}: {page:?}");
        }
        // Nothing of the chat: no composer, no chat list, no chat title in the header.
        assert_eq!((app.composer_rects.field, app.composer_rects.attach), (egui::Rect::NOTHING, egui::Rect::NOTHING), "width {width}, list {list}");
        for chat in ["Message byob", "Search chats", "remarkably long title"] {
            assert!(!text.contains(chat), "width {width}, list {list}: {chat:?} drew");
        }
        assert!(text.contains("Settings"), "the header says where you are");
    }
}

/// Entries enough that the transcript scrolls.
fn long_chat(fake: &Fake) {
    let entries = (1..=40)
        .map(|i| byob_protocol::Entry {
            id: i,
            role: if i % 2 == 0 { byob_protocol::EntryRole::Assistant } else { byob_protocol::EntryRole::User },
            content: format!("Message number {i}, long enough to take a line or two of the transcript."),
            reasoning: String::new(),
            done: true,
            error: None,
            stats: None,
            steps: vec![],
            undoable: false,
            undone: false,
            attachments: vec![],
        })
        .collect();
    fake.inbox.lock().unwrap().push(ServerMsg::Transcript { entries });
}

#[test]
fn back_and_esc_return_to_the_chat_exactly_as_it_was() {
    let ctx = egui::Context::default();
    let fake = Fake::default();
    let mut app = crowded_with(fake.clone());
    long_chat(&fake);
    settle(&ctx, &mut app, 1000.0);
    // Scroll the transcript up.
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 700.0));
    let wheel = egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: egui::vec2(0.0, 600.0),
        modifiers: egui::Modifiers::NONE,
        phase: egui::TouchPhase::Move,
        source: egui::MouseWheelSource::Wheel,
    };
    for _ in 0..3 {
        let events = vec![egui::Event::PointerMoved(egui::pos2(500.0, 300.0)), wheel.clone()];
        let mut out = ctx.run_ui(egui::RawInput { screen_rect: Some(screen), events, ..Default::default() }, |ui| app.ui(ui));
        out.textures_delta.clear();
    }
    // Until the smooth scroll has finished.
    for _ in 0..60 {
        frame(&ctx, &mut app, 1000.0);
    }
    let before = drawn_text(&ctx, &mut app, 1000.0);
    let composer = app.composer_rects.field;
    assert!(!before.contains("Message number 40"), "scrolled up: {before}");
    // The chat list's Settings button, then Back.
    app.sidebar.request(sidebar::Request::AppSettings);
    settle(&ctx, &mut app, 1000.0);
    assert!(app.show_settings);
    let back = app.header_rects.settings_back;
    click(&ctx, &mut app, 1000.0, back.center());
    assert!(!app.show_settings);
    // The pointer back where it was: Back sits over New chat, which shows its keys on hover.
    let moved = vec![egui::Event::PointerMoved(egui::pos2(500.0, 300.0))];
    ctx.run_ui(egui::RawInput { screen_rect: Some(screen), events: moved, ..Default::default() }, |ui| app.ui(ui)).textures_delta.clear();
    frame(&ctx, &mut app, 1000.0);
    assert_eq!(drawn_text(&ctx, &mut app, 1000.0), before, "the same messages, scrolled the same, the same panes");
    assert_eq!(app.composer_rects.field, composer);
    // Esc, when nothing has the keyboard, goes back too; Ctrl+, toggles.
    app.open_settings();
    settle(&ctx, &mut app, 1000.0);
    ctx.memory_mut(|m| m.surrender_focus(m.focused().unwrap_or(egui::Id::NULL)));
    press_key(&ctx, &mut app, 1000.0, egui::Key::Escape);
    assert!(!app.show_settings);
    frame(&ctx, &mut app, 1000.0);
    assert_eq!(drawn_text(&ctx, &mut app, 1000.0), before);
}

#[test]
fn esc_clears_the_search_before_it_closes_the_settings() {
    let ctx = egui::Context::default();
    let mut app = crowded_app();
    app.set_settings_search("theme");
    settle(&ctx, &mut app, 1000.0);
    press_key(&ctx, &mut app, 1000.0, egui::Key::Escape);
    assert!(app.show_settings && app.appearance.search.is_empty());
    press_key(&ctx, &mut app, 1000.0, egui::Key::Escape);
    assert!(!app.show_settings);
}

#[test]
fn every_setting_is_drawn_on_exactly_one_page() {
    let ctx = egui::Context::default();
    let mut app = full_app();
    let mut seen: Vec<(Key, Tab)> = Vec::new();
    for tab in Tab::ALL {
        app.open_settings_at(tab);
        settle(&ctx, &mut app, 1200.0);
        for (key, rect) in &app.appearance.drawn {
            assert!(rect.is_positive() || rect.height() > 0.0, "{key:?} has no room: {rect:?}");
            seen.push((*key, tab));
        }
    }
    for key in Key::ALL {
        let pages: Vec<Tab> = seen.iter().filter(|(k, _)| k == key).map(|(_, t)| *t).collect();
        let want = registry::get(*key).unwrap().section.tab;
        assert_eq!(pages, vec![want], "{key:?} is drawn on {pages:?}, declared on {want:?}");
    }
}

#[test]
fn search_finds_settings_by_name_description_and_keyword() {
    let ctx = egui::Context::default();
    let mut app = full_app();
    for (query, key, not) in [
        ("accent", Key::Accent, Key::Zoom),
        ("colour blindness", Key::ChartColours, Key::Accent),
        ("dark mode", Key::Mode, Key::Font),
        ("SYSTEM PROMPT", Key::Instructions, Key::Approvals),
        ("sidebar", Key::ChatListWidth, Key::ColumnWidth),
    ] {
        app.set_settings_search(query);
        settle(&ctx, &mut app, 1200.0);
        assert_eq!(drawn(&app, key).len(), 1, "{query:?} finds {key:?}");
        assert!(drawn(&app, not).is_empty(), "{query:?} doesn't find {not:?}");
    }
    // Lists' items: a saved model, a plugin, a theme, colour roles.
    for (query, shown) in [("cloud", "Model cloud"), ("weather", "The weather plugin"), ("nord", "Nord"), ("syntax", "colour roles")] {
        app.set_settings_search(query);
        settle(&ctx, &mut app, 1200.0);
        let text = drawn_text(&ctx, &mut app, 1200.0);
        assert!(text.contains(shown), "{query:?}: {shown:?} not in {text}");
    }
    // Nothing found.
    app.set_settings_search("xyzzy");
    let text = drawn_text(&ctx, &mut app, 1200.0);
    assert!(text.contains("No settings match “xyzzy”"), "{text}");
    assert!(app.appearance.drawn.is_empty());
}

#[test]
fn a_result_jumps_to_its_page_scrolled_to_it() {
    let ctx = egui::Context::default();
    let mut app = full_app();
    app.set_settings_search("reduce motion");
    settle(&ctx, &mut app, 1000.0);
    // Enter in the search field: the first result's page, at the setting, lit.
    ctx.memory_mut(|m| m.request_focus(egui::Id::unique("settings-search")));
    frame(&ctx, &mut app, 1000.0);
    press_key(&ctx, &mut app, 1000.0, egui::Key::Enter);
    assert_eq!((app.appearance.tab, app.appearance.search.as_str()), (Tab::Text, ""));
    settle(&ctx, &mut app, 1000.0);
    let row = drawn(&app, Key::ReduceMotion)[0];
    let page = app.header_rects.settings_page;
    assert!(page.contains_rect(row), "scrolled into view: {row:?} in {page:?}");
    // A result's name goes to its page too; its category heading opens the category.
    app.set_settings_search("accent");
    settle(&ctx, &mut app, 1000.0);
    let row = drawn(&app, Key::Accent)[0];
    click(&ctx, &mut app, 1000.0, row.left_top() + egui::vec2(12.0, form::ROW_PAD + 6.0));
    assert_eq!((app.appearance.tab, app.appearance.search.as_str()), (Tab::Appearance, ""));
    // A model found by name opens its form.
    app.set_settings_search("cloud");
    settle(&ctx, &mut app, 1000.0);
    let text = drawn_text(&ctx, &mut app, 1000.0);
    assert!(text.contains("Model cloud"), "{text}");
}

#[test]
fn every_way_into_the_settings_selects_its_page() {
    let ctx = egui::Context::default();
    let mut app = crowded_app();
    frame(&ctx, &mut app, 1000.0);
    let mut check = |name: &str, open: &dyn Fn(&mut ByobApp), want: Tab| {
        app.close_settings();
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
        ("chats", Tab::Defaults),
        ("plugins", Tab::Plugins),
        ("appearance", Tab::Appearance),
        ("text", Tab::Text),
        ("colours", Tab::Colours),
        ("knobs", Tab::Text),
        ("advanced", Tab::Colours),
        ("model:local", Tab::Models),
        // One setting, by its id.
        ("accent", Tab::Appearance),
        ("approvals", Tab::Defaults),
        ("reduce-motion", Tab::Text),
    ] {
        check(section, &|a| assert!(a.reveal_settings(section), "{section}"), want);
    }
    assert!(!app.reveal_settings("nope"));
    assert_eq!(app.models.reveal.as_deref(), Some("local"), "model:<id> opens that model's form once listed");
}

#[test]
fn a_setting_deep_link_scrolls_to_it() {
    let ctx = egui::Context::default();
    let mut app = full_app();
    for width in [1000.0, 390.0] {
        app.close_settings();
        assert!(app.reveal_settings("reset-text"));
        settle(&ctx, &mut app, width);
        let row = drawn(&app, Key::ResetText)[0];
        let page = app.header_rects.settings_page;
        assert!(page.contains_rect(row), "width {width}: {row:?} in {page:?}");
        assert!(drawn(&app, Key::Zoom).iter().all(|r| !page.contains_rect(*r)), "width {width}: scrolled past the top");
    }
}

#[test]
fn the_page_is_remembered_and_clicking_a_category_selects_it() {
    let ctx = egui::Context::default();
    let mut app = crowded_app();
    app.open_settings_at(Tab::Models);
    settle(&ctx, &mut app, 1000.0);
    let plugins = app.header_rects.settings_tabs[Tab::Plugins as usize];
    assert!(plugins.is_positive(), "{plugins:?}");
    click(&ctx, &mut app, 1000.0, plugins.center());
    assert_eq!(app.appearance.tab, Tab::Plugins);
    // The page sits right of the navigation.
    let (body, tab) = (app.header_rects.settings_body, app.header_rects.settings_tabs[Tab::Plugins as usize]);
    assert!(body.left() > tab.right(), "{body:?} {tab:?}");
    // Closed and opened again (Ctrl+, / App settings): the same page.
    app.close_settings();
    frame(&ctx, &mut app, 1000.0);
    app.set_settings_open(true);
    frame(&ctx, &mut app, 1000.0);
    assert_eq!(app.appearance.tab, Tab::Plugins);
}

#[test]
fn phones_list_the_categories_then_open_one() {
    let ctx = egui::Context::default();
    let mut app = crowded_app();
    app.set_settings_open(true);
    drawn_text(&ctx, &mut app, 390.0);
    let text = drawn_text(&ctx, &mut app, 390.0);
    assert!(text.contains("Instructions, approvals, context") && text.contains("Back to chat"), "{text}");
    assert!(MARKERS.iter().all(|(_, m)| !text.contains(m)), "no page yet: {text}");
    let rows: Vec<egui::Rect> = app.header_rects.settings_tabs.to_vec();
    assert!(rows.windows(2).all(|w| w[1].top() >= w[0].bottom() - 0.5 && w[1].right() <= 390.5), "a list down the screen: {rows:?}");
    // A category opens its page, with a way back to the list.
    click(&ctx, &mut app, 390.0, rows[Tab::Appearance as usize].center());
    let text = drawn_text(&ctx, &mut app, 390.0);
    assert!(app.appearance.detail && text.contains("How byob looks"), "{text}");
    let back = app.header_rects.settings_back;
    click(&ctx, &mut app, 390.0, back.center());
    assert!(app.show_settings && !app.appearance.detail);
    // Esc: from a page to the list, from the list to the chat.
    app.open_settings_at(Tab::Text);
    frame(&ctx, &mut app, 390.0);
    press_key(&ctx, &mut app, 390.0, egui::Key::Escape);
    assert!(app.show_settings && !app.appearance.detail);
    press_key(&ctx, &mut app, 390.0, egui::Key::Escape);
    assert!(!app.show_settings);
    // A deep link goes straight to its page; search sits on the list.
    assert!(app.reveal_settings("colours"));
    let text = drawn_text(&ctx, &mut app, 390.0);
    assert!(text.contains("Every colour of"), "{text}");
    app.set_settings_search("density");
    settle(&ctx, &mut app, 390.0);
    assert_eq!(drawn(&app, Key::Density).len(), 1);
    assert!(app.header_rects.settings_search.is_positive());
}

#[test]
fn the_content_column_stays_readable_and_centred_in_a_wide_window() {
    let ctx = egui::Context::default();
    let mut app = crowded_app();
    app.open_settings_at(Tab::Defaults);
    settle(&ctx, &mut app, 2000.0);
    let (body, page) = (app.header_rects.settings_body, app.header_rects.settings_page);
    assert!(body.width() <= form::MAX_W + 0.5, "{body:?}");
    assert!(((body.left() - page.left()) - (page.right() - body.right())).abs() < 24.0, "centred: {body:?} in {page:?}");
    // In a laptop window it takes what there is, less the padding.
    settle(&ctx, &mut app, 1100.0);
    let (body, page) = (app.header_rects.settings_body, app.header_rects.settings_page);
    assert!(body.width() > page.width() - 2.0 * form::PAGE_PAD - 24.0, "{body:?} in {page:?}");
}

#[test]
fn ctrl_f_in_the_settings_searches_them() {
    let ctx = egui::Context::default();
    let mut app = crowded_app();
    app.open_settings_at(Tab::Text);
    settle(&ctx, &mut app, 1000.0);
    shortcut(&ctx, &mut app, egui::Key::F, egui::Modifiers::COMMAND);
    frame(&ctx, &mut app, 1000.0);
    assert!(app.show_settings);
    assert_eq!(ctx.memory(|m| m.focused()), Some(egui::Id::unique("settings-search")));
}
