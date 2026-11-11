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
    /// Modifier keys held (shift-clicks).
    mods: Modifiers,
    /// What the last frame copied to the clipboard.
    copied: Option<String>,
}

impl Win {
    fn new(w: f32) -> Self {
        let ctx = egui::Context::default();
        ctx.global_style_mut(|s| s.animation_time = 0.0);
        Self { ctx, time: 0.0, size: vec2(w, 1400.0), mods: Modifiers::NONE, copied: None }
    }

    fn frame(&mut self, mut events: Vec<In>, draw: &mut dyn FnMut(&mut egui::Ui)) -> Texts {
        self.time += 0.05;
        events.insert(0, In::ModifiersChanged(self.mods));
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, self.size)), events, time: Some(self.time), ..Default::default() };
        let mut out = self.ctx.run_ui(input, |ui| draw(ui));
        self.copied = out.platform_output.commands.iter().find_map(|c| match c {
            egui::OutputCommand::CopyText(t) => Some(t.clone()),
            _ => None,
        });
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
        let mods = self.mods;
        let press = |pressed| In::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: mods };
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

/// Two unnamed tables (or a grid named like an input) don't share one selection.
#[test]
fn unnamed_tables_and_clashing_names_keep_their_own_state() {
    let col = TableColumn { key: "a".into(), label: String::new(), width: Some(80.0), editable: false };
    let table = || Node::new(Kind::Table { name: String::new(), columns: vec![col.clone()], rows: vec![vec![json!(1)]], height: Some(100.0), select: None });
    let tree = Node::new(Kind::Column { children: vec![table(), table()] });
    let (t, mut st, mut w) = (Tokens::dark(), TreeState::default(), Win::new(800.0));
    w.frame(vec![], &mut |ui| drop(show_tree(ui, &t, &tree, &mut st)));
    w.frame(vec![], &mut |ui| drop(show_tree(ui, &t, &tree, &mut st)));
    let mut names: Vec<&String> = st.values().keys().collect();
    names.sort();
    assert_eq!(names, ["table", "table-2"]);
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

const GRID: &str = include_str!("../../../protocol/tests/trees/grid.json");

/// A `rows` × `cols` sheet named "s" with the cells `r{row}c{col}` of a block at the top left.
fn sheet(rows: u32, cols: u32, block: (u32, u32, usize, usize), frozen_rows: u32, editable: bool) -> Node {
    let (r0, c0, h, w) = block;
    let cells: Vec<Vec<Value>> = (0..h).map(|r| (0..w).map(|c| json!(format!("r{}c{}", r0 as usize + r, c0 as usize + c))).collect()).collect();
    parse(
        &json!({"type": "grid", "name": "s", "rows": rows, "cols": cols, "frozen_rows": frozen_rows, "editable": editable,
            "blocks": [{"row": r0, "col": c0, "cells": cells}]})
        .to_string(),
    )
}

/// A tree, its state and what it sent, drawn by [`Sheet::draw`].
struct Sheet {
    t: Tokens,
    tree: Node,
    st: TreeState,
    out: Vec<Event>,
}

impl Sheet {
    fn new(tree: Node) -> Self {
        Self { t: Tokens::dark(), tree, st: TreeState::default(), out: Vec::new() }
    }

    fn draw(&mut self) -> impl FnMut(&mut egui::Ui) + '_ {
        move |ui| {
            let events = show_tree(ui, &self.t, &self.tree, &mut self.st);
            self.out.extend(events);
        }
    }

    /// Grid events since the last call: (op, row, col, the rest).
    fn take(&mut self) -> Vec<GridEvent> {
        std::mem::take(&mut self.out).into_iter().filter_map(|e| e.grid).collect()
    }
}

fn mods(k: egui::Key, m: Modifiers) -> In {
    In::Key { key: k, physical_key: None, pressed: true, repeat: false, modifiers: m }
}

fn ops(events: &[GridEvent]) -> Vec<&str> {
    events.iter().map(|e| e.op.as_str()).collect()
}

#[test]
fn a_grid_draws_with_letters_numbers_and_a_formula_bar() {
    let tree = parse(GRID);
    for (t, width) in [(Tokens::dark(), 900.0), (Tokens::light(), 900.0), (Tokens::dark(), 320.0)] {
        let mut w = Win::new(width);
        let mut st = TreeState::default();
        let mut draw = |ui: &mut egui::Ui| drop(show_tree(ui, &t, &tree, &mut st));
        w.frame(vec![], &mut draw);
        let texts = w.frame(vec![], &mut draw);
        let wants: &[&str] = if width < 400.0 {
            &["Budget", "A", "B", "1", "2", "5", "Item", "Rent"]
        } else {
            &["Budget", "A", "B", "C", "1", "2", "5", "Item", "Rent", "TRUE", "#DIV/0!"]
        };
        for &want in wants {
            assert!(texts.iter().any(|(s, _)| s == want), "{want:?} missing at width {width}: {:?}", texts.iter().map(|t| &t.0).collect::<Vec<_>>());
        }
        // The formula bar: B2 (the tree's selection) and its input.
        assert!(texts.iter().any(|(s, _)| s == "B2"), "the cell's name");
        let n = texts.iter().filter(|(s, _)| s == "1200").count();
        assert!(n >= if width < 400.0 { 2 } else { 3 }, "the cells and the formula bar: {n}");
        // Numbers right, text left: Rent's 1200 ends near its column's right edge.
        let b = find(&texts, "B");
        let rent = find(&texts, "Rent");
        let n = texts.iter().find(|(s, r)| s == "1200" && (r.center().y - rent.center().y).abs() < 2.0).unwrap().1;
        assert!(n.right() > b.center().x && n.left() > rent.right(), "{n:?} {b:?}");
    }
}

#[test]
fn the_formula_bar_shows_the_input_and_compact_grids_have_none() {
    let mut s = Sheet::new(parse(GRID));
    let mut w = Win::new(900.0);
    w.frame(vec![], &mut s.draw());
    let texts = w.frame(vec![], &mut s.draw());
    w.click(find(&texts, "2150.5").center(), &mut s.draw());
    let texts = w.frame(vec![], &mut s.draw());
    assert!(has(&texts, "=SUM(B2:B4)") && texts.iter().any(|(t, _)| t == "B5"), "{texts:?}");
    assert_eq!(s.st.grid_selection("sheet"), Some((4, 1, 4, 1)));
    s.st.set_compact(true);
    let texts = w.frame(vec![], &mut s.draw());
    assert!(!has(&texts, "=SUM") && !texts.iter().any(|(t, _)| t == "B5"), "no formula bar");
    assert!(has(&texts, "2150.5"));
}

#[test]
fn clicks_keys_and_edits_become_grid_events() {
    let mut s = Sheet::new(sheet(1000, 30, (0, 0, 40, 10), 0, true));
    let mut w = Win::new(1000.0);
    w.frame(vec![], &mut s.draw());
    let texts = w.frame(vec![], &mut s.draw());
    // Click selects: a select event, the selection in values.
    w.click(find(&texts, "r2c1").center(), &mut s.draw());
    let e = s.out.last().cloned().unwrap();
    assert_eq!(e.action, "select");
    assert_eq!(e.values["s"], json!({"row": 2, "col": 1, "to_row": 2, "to_col": 1}));
    s.take();
    // Arrows move, Shift+arrows extend.
    w.frame(vec![key(egui::Key::ArrowDown), key(egui::Key::ArrowRight)], &mut s.draw());
    assert_eq!(s.st.grid_selection("s"), Some((3, 2, 3, 2)));
    w.frame(vec![mods(egui::Key::ArrowDown, Modifiers::SHIFT), mods(egui::Key::ArrowRight, Modifiers::SHIFT)], &mut s.draw());
    assert_eq!(s.st.grid_selection("s"), Some((3, 2, 4, 3)));
    let sel = s.take();
    assert_eq!(ops(&sel), ["select", "select"], "one per frame that changed it");
    assert_eq!((sel[1].row, sel[1].col, sel[1].to_row, sel[1].to_col), (3, 2, Some(4), Some(3)));
    // Copy: the range as TSV.
    w.frame(vec![In::Copy], &mut s.draw());
    assert_eq!(w.copied.as_deref(), Some("r3c2\tr3c3\nr4c2\tr4c3"));
    // Delete clears the range; paste goes to its top left.
    w.frame(vec![key(egui::Key::Delete)], &mut s.draw());
    w.frame(vec![In::Paste("a\tb\nc\td".into())], &mut s.draw());
    let ev = s.take();
    assert_eq!(ops(&ev), ["clear", "paste"]);
    assert_eq!((ev[0].row, ev[0].col, ev[0].to_row, ev[0].to_col), (3, 2, Some(4), Some(3)));
    assert_eq!((ev[1].row, ev[1].col, ev[1].text.as_deref()), (3, 2, Some("a\tb\nc\td")));
    // Typing starts an edit (replacing the cell); Enter commits and moves down.
    w.frame(vec![key(egui::Key::ArrowUp)], &mut s.draw());
    w.frame(vec![In::Text("4".into())], &mut s.draw());
    w.frame(vec![In::Text("2".into())], &mut s.draw());
    let editing = w.frame(vec![], &mut s.draw());
    assert!(has(&editing, "42"));
    w.frame(vec![key(egui::Key::Enter)], &mut s.draw());
    let after = w.frame(vec![], &mut s.draw());
    let ev: Vec<GridEvent> = s.take().into_iter().filter(|e| e.op != "select").collect();
    assert_eq!(ev.len(), 1, "{ev:?}");
    assert_eq!((ev[0].op.as_str(), ev[0].row, ev[0].col, ev[0].value.as_deref()), ("edit", 2, 2, Some("42")));
    assert_eq!(s.st.grid_selection("s"), Some((3, 2, 3, 2)), "moved down");
    // The edit shows until the owner answers.
    assert!(has(&after, "42") && !has(&after, "r2c2"), "{:?}", after.iter().map(|t| &t.0).collect::<Vec<_>>());
    s.st.replied(&Map::new());
    let texts = w.frame(vec![], &mut s.draw());
    assert!(has(&texts, "r2c2") && !has(&texts, "42"));
    // F2 edits the input; Escape cancels.
    w.frame(vec![key(egui::Key::F2)], &mut s.draw());
    w.frame(vec![In::Text("zz".into())], &mut s.draw());
    let editing = w.frame(vec![], &mut s.draw());
    assert!(has(&editing, "r3c2zz"), "{:?}", editing.iter().map(|t| &t.0).filter(|t| t.contains("zz") || t.contains("r3c")).collect::<Vec<_>>());
    w.frame(vec![key(egui::Key::Escape)], &mut s.draw());
    let texts = w.frame(vec![], &mut s.draw());
    assert!(!has(&texts, "zz") && has(&texts, "r3c2"));
    assert!(s.take().iter().all(|e| e.op != "edit"));
    // Tab commits and moves right; the grid keeps the keyboard.
    w.frame(vec![In::Text("7".into())], &mut s.draw());
    w.frame(vec![], &mut s.draw());
    w.frame(vec![key(egui::Key::Tab)], &mut s.draw());
    w.frame(vec![key(egui::Key::ArrowDown)], &mut s.draw());
    let ev: Vec<GridEvent> = s.take().into_iter().filter(|e| e.op == "edit").collect();
    assert_eq!((ev[0].row, ev[0].col, ev[0].value.as_deref()), (3, 2, Some("7")));
    assert_eq!(s.st.grid_selection("s"), Some((4, 3, 4, 3)));
    // Double-click edits with the input; clicking a column letter selects the column.
    let texts = w.frame(vec![], &mut s.draw());
    w.double_click(find(&texts, "r6c4").center(), &mut s.draw());
    w.frame(vec![], &mut s.draw());
    w.frame(vec![In::Text("!".into())], &mut s.draw());
    w.frame(vec![key(egui::Key::Enter)], &mut s.draw());
    let ev: Vec<GridEvent> = s.take().into_iter().filter(|e| e.op == "edit").collect();
    assert_eq!((ev[0].row, ev[0].col, ev[0].value.as_deref()), (6, 4, Some("r6c4!")));
    w.click(find(&texts, "C").center(), &mut s.draw());
    assert_eq!(s.st.grid_selection("s"), Some((0, 2, 999, 2)));
    // Shift+click extends.
    w.click(find(&texts, "r1c1").center(), &mut s.draw());
    w.mods = Modifiers::SHIFT;
    w.click(find(&texts, "r3c4").center(), &mut s.draw());
    w.mods = Modifiers::NONE;
    assert_eq!(s.st.grid_selection("s"), Some((1, 1, 3, 4)));
}

#[test]
fn read_only_grids_select_and_copy_but_dont_edit() {
    let mut s = Sheet::new(sheet(20, 5, (0, 0, 20, 5), 0, false));
    let mut w = Win::new(900.0);
    w.frame(vec![], &mut s.draw());
    let texts = w.frame(vec![], &mut s.draw());
    w.click(find(&texts, "r1c1").center(), &mut s.draw());
    w.frame(vec![In::Text("x".into())], &mut s.draw());
    w.frame(vec![key(egui::Key::Enter)], &mut s.draw());
    w.frame(vec![key(egui::Key::Delete), In::Paste("p".into())], &mut s.draw());
    w.frame(vec![In::Copy], &mut s.draw());
    assert_eq!(w.copied.as_deref(), Some("r2c1"));
    assert_eq!(ops(&s.take()), ["select", "select"]);
}

#[test]
fn scrolling_asks_for_the_window_once_and_frozen_rows_stay() {
    let mut s = Sheet::new(sheet(1_000_000, 200, (0, 0, 60, 20), 1, true));
    let mut w = Win::new(1000.0);
    w.frame(vec![], &mut s.draw());
    let texts = w.frame(vec![], &mut s.draw());
    assert!(has(&texts, "r5c0"));
    assert!(s.take().is_empty(), "what shows is loaded: nothing asked");
    // Far down (as the scrollbar would): the rows there aren't loaded.
    s.st.scroll_grid("s", 0.0, 500_000.0 * 22.0);
    let texts = w.frame(vec![], &mut s.draw());
    let ev = s.take();
    assert_eq!(ops(&ev), ["window"], "{ev:?}");
    let (row, rows) = (ev[0].row, ev[0].rows.unwrap());
    let first_shown: u32 = texts.iter().filter_map(|(t, r)| t.parse::<u32>().ok().filter(|_| r.left() < 60.0)).filter(|n| *n > 1000).min().unwrap() - 1;
    assert!(row + 50 <= first_shown && first_shown + 60 < row + rows, "{row}+{rows} covers {first_shown} with overscan");
    assert_eq!((ev[0].col, ev[0].cols), (0, Some(ev[0].cols.unwrap())));
    assert!(ev[0].cols.unwrap() >= 10);
    // The frozen row and the column letters stay at the top.
    let a = find(&texts, "A");
    assert!(texts.iter().any(|(t, r)| t == "r0c0" && r.top() > a.bottom() && r.top() < a.bottom() + 30.0), "the frozen row under the letters");
    assert!(texts.iter().any(|(t, _)| t == "1"), "row 1's number");
    // Not again while it is outstanding, even moving a little.
    w.frame(vec![], &mut s.draw());
    // A reply that brings no blocks there (the owner can't, or answered something else)
    // doesn't make it ask again every frame.
    s.st.replied(&Map::new());
    w.frame(vec![], &mut s.draw());
    s.st.replied(&Map::new());
    w.frame(vec![], &mut s.draw());
    let again = s.take();
    assert!(again.is_empty(), "asked again after a reply: {again:?}");
    s.st.scroll_grid("s", 0.0, 500_010.0 * 22.0);
    w.frame(vec![], &mut s.draw());
    let again = s.take();
    assert!(again.is_empty(), "{ev:?} then {again:?}");
    // The owner answers with the window: drawn, nothing more asked.
    let mut answered = sheet(1_000_000, 200, (row, 0, rows as usize, 20), 1, true);
    let Kind::Grid { blocks, .. } = &mut answered.kind else { panic!() };
    let head = (0..20).map(|c| byob_protocol::tree::GridCell::text(if c == 0 { "head".to_owned() } else { format!("h{c}") })).collect();
    blocks.insert(0, byob_protocol::tree::GridBlock { row: 0, col: 0, cells: vec![head] });
    s.tree = answered;
    let texts = w.frame(vec![], &mut s.draw());
    assert!(has(&texts, &format!("r{}c0", first_shown + 20)) && has(&texts, "head"));
    assert!(s.take().is_empty());
    // Scrolling far past it asks again.
    s.st.scroll_grid("s", 0.0, 900_000.0 * 22.0);
    w.frame(vec![], &mut s.draw());
    assert_eq!(ops(&s.take()), ["window"]);
}

#[test]
fn the_wheel_scrolls_the_grid() {
    let mut s = Sheet::new(sheet(10_000, 50, (0, 0, 400, 20), 0, true));
    let mut w = Win::new(1000.0);
    w.frame(vec![], &mut s.draw());
    let texts = w.frame(vec![], &mut s.draw());
    let at = find(&texts, "r3c3").center();
    let wheel = In::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: vec2(0.0, -2000.0),
        phase: egui::TouchPhase::Move,
        source: egui::MouseWheelSource::Unknown,
        modifiers: Modifiers::NONE,
    };
    w.frame(vec![In::PointerMoved(at), wheel], &mut s.draw());
    let mut texts = Vec::new();
    for _ in 0..40 {
        texts = w.frame(vec![], &mut s.draw());
    }
    assert!(!has(&texts, "r3c3") && has(&texts, "r100c3"), "{:?}", texts.iter().map(|t| &t.0).take(40).collect::<Vec<_>>());
}

/// The table benchmark for a spreadsheet-sized grid: a 1M × 16k sheet with a 60 × 20 block
/// costs what a small one does.
#[test]
fn a_huge_grid_draws_only_what_shows() {
    let huge = Node::new(Kind::Column { children: vec![sheet(1_048_576, 16_384, (0, 0, 60, 20), 1, true)] });
    let (frame, texts) = median_frame(&huge, 30);
    let mut far = Sheet::new(huge.clone());
    let mut w = Win::new(1200.0);
    w.frame(vec![], &mut far.draw());
    far.st.scroll_grid("s", 1_400_000.0, 1_000_000.0 * 22.0);
    let mut times = Vec::new();
    for _ in 0..20 {
        let start = Instant::now();
        w.frame(vec![], &mut far.draw());
        times.push(start.elapsed());
    }
    times.sort();
    let far_frame = times[times.len() / 2];
    eprintln!("frame (layout + tessellation): 1M × 16k grid {frame:?} ({texts} texts), scrolled to the far corner {far_frame:?}");
    assert!(texts < 1500, "virtualized: {texts} texts drawn");
    assert!(frame < Duration::from_millis(30), "{frame:?}");
    assert!(far_frame < Duration::from_millis(30), "{far_frame:?}");
}
