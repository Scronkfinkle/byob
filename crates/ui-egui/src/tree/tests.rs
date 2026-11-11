//! Headless renderer tests: every node kind draws (dark, light, narrow), clicks and typing
//! become events, typed text survives replies, 10k-row tables stay cheap.

use std::time::{Duration, Instant};

use byob_protocol::tree::{self, Patch, Reply, TableColumn};
use byob_protocol::{Step, StepStatus, View};
use egui::{Event as In, Modifiers, Pos2, Rect, Shape, vec2};
use serde_json::json;

use super::*;
use crate::views::ViewRegistry;

const DASHBOARD: &str = include_str!("../../../protocol/tests/trees/dashboard.json");
const SETTINGS: &str = include_str!("../../../protocol/tests/trees/settings.json");
const CANVAS: &str = include_str!("../../../protocol/tests/trees/canvas.json");

fn parse(s: &str) -> Node {
    let (n, f) = tree::decode_str(s);
    assert!(f.is_empty(), "{f:?}");
    n
}

type Texts = Vec<(String, Rect)>;

/// Texts that show: clipped away entirely doesn't count.
fn collect(s: &Shape, clip: Rect, out: &mut Texts) {
    match s {
        Shape::Text(t) if clip.contains_rect(t.visual_bounding_rect().shrink(1.0)) => out.push((t.galley.text().to_owned(), t.visual_bounding_rect())),
        Shape::Vec(v) => v.iter().for_each(|s| collect(s, clip, out)),
        _ => {}
    }
}

/// A headless window driving frames with a clock (double-clicks need one).
struct Win {
    ctx: egui::Context,
    time: f64,
    size: egui::Vec2,
}

impl Win {
    fn new(w: f32) -> Self {
        let ctx = egui::Context::default();
        ctx.global_style_mut(|s| s.animation_time = 0.0);
        Self { ctx, time: 0.0, size: vec2(w, 1400.0) }
    }

    fn frame(&mut self, events: Vec<In>, draw: &mut dyn FnMut(&mut egui::Ui)) -> Texts {
        self.time += 0.05;
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, self.size)), events, time: Some(self.time), ..Default::default() };
        let mut out = self.ctx.run_ui(input, |ui| draw(ui));
        out.textures_delta.clear();
        let mut texts = Vec::new();
        for s in &out.shapes {
            collect(&s.shape, s.clip_rect, &mut texts);
        }
        // As a real frame would (the prototype's numbers include it).
        drop(self.ctx.tessellate(out.shapes, out.pixels_per_point));
        texts
    }

    fn click(&mut self, at: Pos2, draw: &mut dyn FnMut(&mut egui::Ui)) -> Texts {
        let press = |pressed| In::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        self.frame(vec![In::PointerMoved(at), press(true)], draw);
        self.frame(vec![press(false)], draw)
    }

    fn double_click(&mut self, at: Pos2, draw: &mut dyn FnMut(&mut egui::Ui)) -> Texts {
        let press = |pressed| In::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        // A while after the last click, or egui counts a triple click.
        self.time += 1.0;
        self.click(at, draw);
        self.frame(vec![press(true)], draw);
        self.frame(vec![press(false)], draw)
    }
}

fn key(k: egui::Key) -> In {
    In::Key { key: k, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE }
}

fn find(texts: &Texts, s: &str) -> Rect {
    texts.iter().find(|(t, _)| t == s).unwrap_or_else(|| panic!("no {s:?} in {:?}", texts.iter().map(|t| &t.0).collect::<Vec<_>>())).1
}

fn has(texts: &Texts, s: &str) -> bool {
    texts.iter().any(|(t, _)| t.contains(s))
}

fn all_demo() -> Node {
    Node::new(Kind::Column { children: vec![parse(DASHBOARD), parse(SETTINGS), parse(CANVAS)] })
}

#[test]
fn every_node_kind_draws_in_both_themes_and_narrow() {
    let tree = all_demo();
    for (t, width) in [(Tokens::dark(), 900.0), (Tokens::light(), 900.0), (Tokens::dark(), 320.0)] {
        let mut w = Win::new(width);
        let mut st = TreeState::default();
        let mut draw = |ui: &mut egui::Ui| {
            egui::ScrollArea::vertical().show(ui, |ui| show_tree(ui, &t, &tree, &mut st));
        };
        w.frame(vec![], &mut draw);
        let texts = w.frame(vec![], &mut draw);
        for want in [
            "Build health",
            "3 failing",
            "Builds today",
            "62% of budget",
            "Build minutes",
            "Job",
            "linux-x64",
            "Re-run failed",
            "Weather plugin",
            "open-meteo",
            "Lisbon",
            "metric",
            "Show severe-weather alerts",
            "Forecast plot",
            "68% CPU",
        ] {
            assert!(has(&texts, want), "{want:?} missing at width {width}");
        }
    }
}

#[test]
fn clicks_typing_and_edits_become_events_with_every_field() {
    let t = Tokens::dark();
    let tree = all_demo();
    let mut st = TreeState::default();
    let mut out = Vec::new();
    let mut w = Win::new(900.0);
    let mut draw = |ui: &mut egui::Ui| out.extend(show_tree(ui, &t, &tree, &mut st));
    w.frame(vec![], &mut draw);
    let texts = w.frame(vec![], &mut draw);
    w.click(find(&texts, "Re-run failed").center(), &mut draw);
    // Type into the city field and press Enter: its submit action, with every value.
    let city = find(&texts, "Lisbon");
    w.click(Pos2::new(city.right() + 2.0, city.center().y), &mut draw);
    w.frame(vec![In::Text(" Norte".into())], &mut draw);
    w.frame(vec![key(egui::Key::Enter)], &mut draw);
    // Flip the toggle (no action: nothing sent, but the next event carries it).
    w.click(find(&texts, "Show severe-weather alerts").center(), &mut draw);
    // Select a table row, edit a cell, cancel another edit, click the canvas.
    w.click(find(&texts, "mac-arm").center(), &mut draw);
    // Cells of the Tries column (charts have numbers too).
    let header = find(&texts, "Tries");
    let cell = |v: &str| texts.iter().find(|(s, r)| s == v && r.min.y > header.max.y && r.right() > header.left()).unwrap().1;
    let tries = cell("2");
    w.double_click(tries.center(), &mut draw);
    w.frame(vec![], &mut draw);
    w.frame(vec![key(egui::Key::Backspace), In::Text("5".into())], &mut draw);
    let edited = w.frame(vec![key(egui::Key::Enter)], &mut draw);
    let other = cell("3");
    w.double_click(other.center(), &mut draw);
    w.frame(vec![In::Text("9".into())], &mut draw);
    w.frame(vec![key(egui::Key::Escape)], &mut draw);
    let cpu = find(&texts, "68% CPU");
    w.click(cpu.left_top() + vec2(5.0, 5.0), &mut draw);
    let after = w.frame(vec![], &mut draw);

    let actions: Vec<&str> = out.iter().map(|e| e.action.as_str()).collect();
    assert_eq!(actions, ["rerun", "save", "edit", "click"], "{out:#?}");
    assert_eq!(out[0].values.get("city"), Some(&json!("Lisbon")));
    assert_eq!(out[0].values.get("alerts"), Some(&json!(true)));
    assert_eq!(out[0].values.get("fails"), Some(&Value::Null));
    assert_eq!(out[1].values.get("city"), Some(&json!("Lisbon Norte")));
    assert_eq!(out[1].values.get("units"), Some(&json!("metric")));
    assert_eq!(out[2].values.get("alerts"), Some(&json!(false)));
    assert_eq!(out[2].values.get("fails"), Some(&json!(0)), "the row clicked last is selected");
    let edit = out[2].edit.as_ref().unwrap();
    assert_eq!((edit.table.as_str(), edit.row, edit.column.as_str(), edit.value.as_str()), ("fails", 0, "tries", "5"));
    assert!(has(&edited, "5") && has(&after, "5"), "the edit shows until the owner answers");
    let (x, y) = (out[3].values["_x"].as_f64().unwrap(), out[3].values["_y"].as_f64().unwrap());
    assert!((20.0..30.0).contains(&x) && (92.0..102.0).contains(&y), "canvas units: {x}, {y}");
    assert!(!out[2].values.contains_key("_x"), "click points are for that event only");
}

#[test]
fn typed_text_survives_replies_unless_the_owner_changes_the_field() {
    let t = Tokens::dark();
    let mut view = TreeView::new(parse(SETTINGS));
    let mut w = Win::new(800.0);
    let texts = w.frame(vec![], &mut |ui| drop(view.show(ui, &t)));
    let city = find(&texts, "Lisbon");
    w.click(Pos2::new(city.right() + 2.0, city.center().y), &mut |ui| drop(view.show(ui, &t)));
    w.frame(vec![In::Text("!".into())], &mut |ui| drop(view.show(ui, &t)));
    assert_eq!(view.state.values()["city"], json!("Lisbon!"));
    // A reply that changes other things (even the whole view, same field value): kept.
    let p = |v: Value| Reply { patch: serde_json::from_value::<Vec<Patch>>(v).unwrap(), ..Reply::default() };
    assert!(view.reply(p(json!([{"set": {"key": "saved", "fields": {"text": "saving…"}}}]))).is_empty());
    assert!(view.reply(Reply { view: Some(parse(SETTINGS)), ..Reply::default() }).is_empty());
    let texts = w.frame(vec![In::Text("?".into())], &mut |ui| drop(view.show(ui, &t)));
    assert_eq!(view.state.values()["city"], json!("Lisbon!?"), "still typing");
    assert!(has(&texts, "Lisbon!"));
    // The owner changes the field's value: it wins.
    assert!(view.reply(p(json!([{"put": {"path": "/children/1/children/1/value", "value": "Porto"}}]))).is_empty());
    w.frame(vec![], &mut |ui| drop(view.show(ui, &t)));
    assert_eq!(view.state.values()["city"], json!("Porto"));
    // A reply's values force a field (clearing an input after Add).
    let mut clear = Reply::default();
    clear.values.insert("city".into(), json!(""));
    view.reply(clear);
    let texts = w.frame(vec![], &mut |ui| drop(view.show(ui, &t)));
    assert_eq!(view.state.values()["city"], json!(""));
    assert!(has(&texts, "City name"), "the placeholder shows again");
    // Fields that leave the tree leave the values.
    view.reply(Reply { view: Some(parse(CANVAS)), ..Reply::default() });
    w.frame(vec![], &mut |ui| drop(view.show(ui, &t)));
    assert!(view.state.values().is_empty());
}

fn grid(n: usize) -> Node {
    let rows =
        (0..n).map(|i| vec![json!(format!("SKU-{i:05}")), json!(format!("Widget {i}")), json!(i % 17 + 1), json!(1.5 + (i % 50) as f64 * 0.37)]).collect();
    let col = |k: &str, w: f32, editable: bool| TableColumn { key: k.into(), label: String::new(), width: Some(w), editable };
    let columns = vec![col("sku", 90.0, false), col("name", 140.0, false), col("qty", 60.0, true), col("price", 70.0, true)];
    Node::keyed("grid", Kind::Table { name: "grid".into(), columns, rows, height: Some(700.0), select: None })
}

fn median_frame(tree: &Node, frames: usize) -> (Duration, usize) {
    let t = Tokens::dark();
    let mut st = TreeState::default();
    let mut w = Win::new(1200.0);
    let mut times = Vec::new();
    let mut texts = 0;
    for i in 0..frames {
        let start = Instant::now();
        texts = w.frame(vec![], &mut |ui| drop(egui::ScrollArea::vertical().show(ui, |ui| show_tree(ui, &t, tree, &mut st)))).len();
        if i >= 3 {
            times.push(start.elapsed());
        }
    }
    times.sort();
    (times.get(times.len() / 2).copied().unwrap_or_default(), texts)
}

/// The prototype's frame benchmark with a generous limit: a 10k-row table draws only what
/// is on screen.
#[test]
fn a_10k_row_table_draws_only_what_shows() {
    let (table, texts) = median_frame(&grid(10_000), 30);
    let small = median_frame(&parse(DASHBOARD), 30).0;
    // What an extension would send without a table: 10k row nodes of text nodes.
    let naive_rows = (0..10_000)
        .map(|i| {
            Node::new(Kind::Row {
                children: (0..4).map(|c| Node::new(Kind::Text { text: format!("{i}:{c}"), style: TextStyle::Body, tone: Tone::Normal })).collect(),
            })
        })
        .collect();
    let (naive, _) = median_frame(&Node::new(Kind::Column { children: naive_rows }), 6);
    eprintln!("frame (layout + tessellation): 10k-row table {table:?} ({texts} texts), dashboard {small:?}, 10k row nodes {naive:?}");
    assert!(texts < 400, "virtualized: {texts} texts drawn");
    assert!(table < Duration::from_millis(30), "{table:?}");
}

fn step_with(id: u64, views: Vec<View>) -> Step {
    Step {
        id,
        tool: "plugin".into(),
        title: "A view".into(),
        access: byob_protocol::ToolAccess::Read,
        status: StepStatus::Done,
        detail: String::new(),
        changes: vec![],
        at: 0,
        media: vec![],
        preview: None,
        artifacts: vec![],
        delegation: None,
        ask: None,
        views,
    }
}

#[test]
fn a_step_view_can_be_a_tree_and_newer_trees_show_their_fallback() {
    let t = Tokens::dark();
    let mut reg = ViewRegistry::default();
    let mut plugins = crate::plugins::PluginsUi::default();
    let latest = Default::default();
    let tree = parse(SETTINGS);
    let step = step_with(4, vec![tree.to_view()]);
    let mut w = Win::new(800.0);
    w.frame(vec![], &mut |ui| reg.show_step(ui, &t, &step, &latest, &mut plugins));
    let texts = w.frame(vec![], &mut |ui| reg.show_step(ui, &t, &step, &latest, &mut plugins));
    assert!(has(&texts, "Weather plugin") && has(&texts, "Lisbon"), "drawn natively");
    w.click(find(&texts, "Save").center(), &mut |ui| reg.show_step(ui, &t, &step, &latest, &mut plugins));
    let events = reg.take_tree_events();
    assert_eq!(events.len(), 1);
    assert_eq!((events[0].step, events[0].view, events[0].event.action.as_str()), (4, 0, "save"));
    assert!(reg.take_tree_events().is_empty());
    // A tree schema from a newer byob: the markdown fallback.
    let newer = step_with(5, vec![View { v: tree::VERSION + 1, ..tree.to_view() }]);
    let texts = w.frame(vec![], &mut |ui| reg.show_step(ui, &t, &newer, &latest, &mut plugins));
    assert!(has(&texts, "Weather plugin") && !has(&texts, "Lisbon"), "{texts:?}");
    // Broken data still draws what it can.
    let broken =
        step_with(6, vec![View { data: json!({"type": "column", "children": [{"type": "nope"}, {"type": "text", "text": "kept"}]}), ..tree.to_view() }]);
    let texts = w.frame(vec![], &mut |ui| reg.show_step(ui, &t, &broken, &latest, &mut plugins));
    assert!(has(&texts, "kept"));
}
