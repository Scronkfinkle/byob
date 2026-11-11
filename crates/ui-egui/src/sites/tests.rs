//! The client's render sites: open on first draw, live trees and patches, events back,
//! errors and faults at the site, cards over their snapshot, images, resets, the pane.

use byob_protocol::tree::{Patch, Target};
use byob_protocol::{BackendInfo, PluginOrigin, PluginScope};
use egui::{Event as In, Modifiers, Pos2, Rect, Shape, vec2};
use serde_json::json;

use super::*;

fn ext(views: Vec<ViewDecl>) -> ServerMsg {
    let mut p = crate::plugins::tests::info("todo", PluginScope::Global, PluginOrigin::User);
    p.views = views;
    p.backend = Some(BackendInfo { state: "running".into(), ..Default::default() });
    ServerMsg::Extensions(Box::new(Extensions { plugins: vec![p], ..Default::default() }))
}

fn decl(site: Site, id: &str, title: &str) -> ViewDecl {
    ViewDecl { id: id.into(), site, title: title.into(), icon: String::new(), html: String::new() }
}

fn tree(json: serde_json::Value) -> Node {
    tree::decode(json).0
}

fn texts(ctx: &egui::Context, events: Vec<In>, draw: &mut dyn FnMut(&mut egui::Ui)) -> Vec<(String, Rect)> {
    let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(900.0, 700.0))), events, ..Default::default() };
    let mut out = ctx.run_ui(input, |ui| draw(ui));
    out.textures_delta.clear();
    let mut found = Vec::new();
    fn collect(s: &Shape, out: &mut Vec<(String, Rect)>) {
        match s {
            Shape::Text(t) => out.push((t.galley.text().to_owned(), t.visual_bounding_rect())),
            Shape::Vec(v) => v.iter().for_each(|s| collect(s, out)),
            _ => {}
        }
    }
    for s in &out.shapes {
        collect(&s.shape, &mut found);
    }
    found
}

fn has(found: &[(String, Rect)], text: &str) -> bool {
    found.iter().any(|(t, _)| t.contains(text))
}

fn at(found: &[(String, Rect)], text: &str) -> Pos2 {
    found.iter().find(|(t, _)| t == text).unwrap_or_else(|| panic!("no {text:?} in {:?}", found.iter().map(|f| &f.0).collect::<Vec<_>>())).1.center()
}

fn click(ctx: &egui::Context, p: Pos2, draw: &mut dyn FnMut(&mut egui::Ui)) {
    let press = |pressed| In::PointerButton { pos: p, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
    texts(ctx, vec![In::PointerMoved(p), press(true)], draw);
    texts(ctx, vec![press(false)], draw);
}

#[test]
fn a_pane_opens_once_shows_the_live_tree_and_sends_events() {
    let mut s = Sites::default();
    s.set_enabled(true);
    s.apply(&ext(vec![decl(Site::Pane, "list", "To-do"), decl(Site::Status, "count", "Count")]));
    assert!(s.open_pane(""));
    let ctx = egui::Context::default();
    let t = Tokens::dark();
    let r = ViewRef::new("todo", "list", "");
    let found = texts(&ctx, vec![], &mut |ui| s.pane(ui, &t, false, &mut crate::web::Web::new(Box::new(crate::appview::Placeholder))));
    assert!(has(&found, "Loading…") && has(&found, "To-do"));
    assert_eq!(s.take(), vec![ClientMsg::OpenView { view: r.clone() }]);
    texts(&ctx, vec![], &mut |ui| s.pane(ui, &t, false, &mut crate::web::Web::new(Box::new(crate::appview::Placeholder))));
    assert!(s.take().is_empty(), "opened once");
    let t0 = tree(json!({"type": "column", "children": [
        {"type": "text", "key": "n", "text": "0 left"},
        {"type": "button", "label": "Add", "action": "add"}]}));
    assert!(s.apply(&ServerMsg::LiveView(LiveView { view: r.clone(), tree: Some(t0), ..Default::default() })));
    let found = texts(&ctx, vec![], &mut |ui| s.pane(ui, &t, false, &mut crate::web::Web::new(Box::new(crate::appview::Placeholder))));
    assert!(has(&found, "0 left"));
    let add = at(&found, "Add");
    click(&ctx, add, &mut |ui| s.pane(ui, &t, false, &mut crate::web::Web::new(Box::new(crate::appview::Placeholder))));
    let sent = s.take();
    assert!(matches!(&sent[..], [ClientMsg::ViewEvent { view, event }] if *view == r && event.action == "add"), "{sent:?}");
    // a patch from the engine
    let patch = vec![Patch::Set { at: Target::key("n"), fields: json!({"text": "1 left"}).as_object().cloned().unwrap_or_default() }];
    s.apply(&ServerMsg::LiveView(LiveView { view: r.clone(), patch, faults: vec!["/children/3: unknown node type \"slider\"".into()], ..Default::default() }));
    let found = texts(&ctx, vec![], &mut |ui| s.pane(ui, &t, false, &mut crate::web::Web::new(Box::new(crate::appview::Placeholder))));
    assert!(has(&found, "1 left") && !has(&found, "0 left"));
    assert!(has(&found, "1 problem in this view"), "faults show at the site");
    // reset: forgotten, opened again on the next draw
    s.apply(&ServerMsg::LiveView(LiveView { view: r.clone(), reset: true, ..Default::default() }));
    texts(&ctx, vec![], &mut |ui| s.pane(ui, &t, false, &mut crate::web::Web::new(Box::new(crate::appview::Placeholder))));
    assert_eq!(s.take(), vec![ClientMsg::OpenView { view: r.clone() }]);
    // an error at the site
    s.apply(&ServerMsg::LiveView(LiveView { view: r.clone(), error: Some("Todo can't show To-do: its backend is failed".into()), ..Default::default() }));
    let found = texts(&ctx, vec![], &mut |ui| s.pane(ui, &t, false, &mut crate::web::Web::new(Box::new(crate::appview::Placeholder))));
    assert!(has(&found, "its backend is failed"));
    // another chat: everything resolves anew
    s.peek(&ServerMsg::ChatOpened { chat: None, workspace: None });
    texts(&ctx, vec![], &mut |ui| s.pane(ui, &t, false, &mut crate::web::Web::new(Box::new(crate::appview::Placeholder))));
    assert_eq!(s.take(), vec![ClientMsg::OpenView { view: r }]);
    // the plugin switched off: its views go, the pane closes
    let mut off = ext(vec![decl(Site::Pane, "list", "To-do")]);
    if let ServerMsg::Extensions(e) = &mut off {
        e.plugins[0].enabled = false;
    }
    s.apply(&off);
    assert!(!s.pane_is_open() && s.declared().is_empty());
    assert_eq!(s.take(), vec![ClientMsg::CloseView { view: ViewRef::new("todo", "list", "") }]);
}

#[test]
fn cards_show_their_snapshot_until_the_live_one_comes_and_old_engines_open_nothing() {
    let owner = ViewRef::new("todo", "item", "7");
    let snap = tree(json!({"type": "text", "text": "buy milk (open)"}));
    let mut view = snap.to_view();
    view.owner = Some(owner.clone());
    let ctx = egui::Context::default();
    let t = Tokens::light();
    // an engine without view_sites: the snapshot, nothing sent
    let mut old = Sites::default();
    let found = texts(&ctx, vec![], &mut |ui| assert!(old.card(ui, &t, &view)));
    assert!(has(&found, "buy milk (open)"));
    assert!(old.take().is_empty());
    let mut s = Sites::default();
    s.set_enabled(true);
    texts(&ctx, vec![], &mut |ui| assert!(s.card(ui, &t, &view)));
    assert_eq!(s.take(), vec![ClientMsg::OpenView { view: owner.clone() }]);
    s.apply(&ServerMsg::LiveView(LiveView { view: owner, tree: Some(tree(json!({"type": "text", "text": "buy milk (done)"}))), ..Default::default() }));
    let found = texts(&ctx, vec![], &mut |ui| assert!(s.card(ui, &t, &view)));
    assert!(has(&found, "buy milk (done)") && !has(&found, "(open)"));
    // not a plugin's card: not ours
    let plain = snap.to_view();
    texts(&ctx, vec![], &mut |ui| assert!(!s.card(ui, &t, &plain)));
}

#[test]
fn status_items_and_settings_sections_draw_their_views() {
    let mut s = Sites::default();
    s.set_enabled(true);
    s.apply(&ext(vec![decl(Site::Status, "count", "Count"), decl(Site::Settings, "prefs", "To-do settings")]));
    let ctx = egui::Context::default();
    let t = Tokens::dark();
    let count = ViewRef::new("todo", "count", "");
    let prefs = ViewRef::new("todo", "prefs", "");
    s.apply(&ServerMsg::LiveView(LiveView { view: count.clone(), tree: Some(tree(json!({"type": "chip", "text": "3 to do"}))), ..Default::default() }));
    s.apply(&ServerMsg::LiveView(LiveView {
        view: prefs.clone(),
        tree: Some(tree(json!({"type": "toggle", "name": "hide", "label": "Hide done"}))),
        ..Default::default()
    }));
    let found = texts(&ctx, vec![], &mut |ui| {
        ui.horizontal(|ui| s.status_items(ui, &t));
        s.settings_sections(ui, &t, "todo");
        s.settings_sections(ui, &t, "other");
    });
    assert!(has(&found, "3 to do") && has(&found, "Hide done") && has(&found, "To-do settings"));
    let sent = s.take();
    assert!(sent.contains(&ClientMsg::OpenView { view: count }) && sent.contains(&ClientMsg::OpenView { view: prefs }), "{sent:?}");
    assert!(!s.open_pane(""), "no pane views");
}

#[test]
fn image_nodes_load_through_media_and_artifacts() {
    let mut s = Sites::default();
    s.set_enabled(true);
    s.apply(&ext(vec![decl(Site::Pane, "pics", "Pictures")]));
    s.open_pane("");
    let r = ViewRef::new("todo", "pics", "");
    let t0 = tree(json!({"type": "column", "children": [
        {"type": "image", "media": "plots/a.png", "alt": "plot A"},
        {"type": "image", "artifact": "logo", "alt": "the logo"},
        {"type": "image", "artifact": "nope", "alt": "missing"}]}));
    let art: byob_protocol::Artifact = serde_json::from_value(
        json!({"id": "logo", "kind": "image", "title": "Logo", "filename": "logo.png", "versions": [{"n": 2, "path": "logo.png", "created_at": 0}]}),
    )
    .unwrap();
    s.peek(&ServerMsg::Artifacts { chat: None, artifacts: vec![art] });
    s.apply(&ServerMsg::LiveView(LiveView { view: r.clone(), tree: Some(t0), ..Default::default() }));
    let ctx = egui::Context::default();
    let t = Tokens::dark();
    let found = texts(&ctx, vec![], &mut |ui| s.pane(ui, &t, false, &mut crate::web::Web::new(Box::new(crate::appview::Placeholder))));
    assert!(has(&found, "Loading…") && has(&found, "no artifact \"nope\""), "{:?}", found.iter().map(|f| &f.0).collect::<Vec<_>>());
    let sent = s.take();
    assert!(sent.contains(&ClientMsg::GetMedia { path: "plots/a.png".into() }), "{sent:?}");
    assert!(sent.contains(&ClientMsg::GetArtifact { id: "logo".into(), version: 2 }), "{sent:?}");
    assert!(s.loading());
    let mut png = Vec::new();
    image::RgbaImage::new(4, 3).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
    let data = byob_protocol::media::encode(&png);
    // others see these too (not consumed)
    s.peek(&ServerMsg::Media { path: "plots/a.png".into(), content: MediaContent::Image { mime: "image/png".into(), data: data.clone() } });
    s.peek(&ServerMsg::ArtifactFile { id: "logo".into(), version: 2, data: Some(data), error: None });
    let found = texts(&ctx, vec![], &mut |ui| s.pane(ui, &t, false, &mut crate::web::Web::new(Box::new(crate::appview::Placeholder))));
    assert!(!has(&found, "Loading…"));
    assert!(!s.loading());
    assert!(s.take().is_empty(), "fetched once");
}
