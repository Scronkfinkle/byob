use std::time::{Duration, Instant};

use serde_json::json;

use super::*;

/// The demo trees (also drawn by the screenshot example and the renderer tests).
pub(crate) const DASHBOARD: &str = include_str!("../../tests/trees/dashboard.json");
pub(crate) const SETTINGS: &str = include_str!("../../tests/trees/settings.json");
pub(crate) const CANVAS: &str = include_str!("../../tests/trees/canvas.json");

fn ok(s: &str) -> Node {
    let (n, faults) = decode_str(s);
    assert!(faults.is_empty(), "{faults:?}");
    n
}

fn text(t: &str) -> Node {
    Node::new(Kind::Text { text: t.into(), style: TextStyle::Body, tone: Tone::Normal })
}

/// A table of `n` rows like the prototype's benchmark grid.
pub(crate) fn grid(n: usize) -> Node {
    let rows = (0..n)
        .map(|i| {
            let q = (i % 17 + 1) as f64;
            let p = (150 + (i % 50) * 37) as f64 / 100.0;
            vec![format!("SKU-{i:05}").into(), format!("Widget {i}").into(), q.into(), p.into(), ((q * p * 100.0).round() / 100.0).into()]
        })
        .collect();
    let columns = serde_json::from_value(json!([
        {"key": "sku", "width": 90}, {"key": "name", "width": 140}, {"key": "qty", "width": 60, "editable": true},
        {"key": "price", "width": 70, "editable": true}, {"key": "total", "width": 80}]))
    .unwrap();
    Node::new(Kind::Column { children: vec![Node::keyed("grid", Kind::Table { name: "grid".into(), columns, rows, height: Some(700.0), select: None })] })
}

#[test]
fn the_demo_trees_decode_cleanly_and_round_trip() {
    for src in [DASHBOARD, SETTINGS, CANVAS] {
        let n = ok(src);
        let again = ok(&serde_json::to_string(&n).unwrap());
        assert_eq!(n, again);
    }
    let d = ok(DASHBOARD);
    assert!(d.count() > 10 && d.find("fails").is_some());
    // Defaults aren't written out.
    let s = serde_json::to_string(&text("hi")).unwrap();
    assert_eq!(s, r#"{"type":"text","text":"hi"}"#);
}

#[test]
fn every_node_kind_decodes() {
    let n = ok(r##"{"type":"column","key":"root","children":[
        {"type":"row","children":[{"type":"card","title":"T","children":[]}]},
        {"type":"text","text":"a","style":"heading","tone":"danger"},
        {"type":"markdown","text":"**b**"},
        {"type":"chip","text":"c","tone":"success"},
        {"type":"button","label":"Go","action":"go","primary":true},
        {"type":"input","name":"q","value":"x","placeholder":"p","submit":"search"},
        {"type":"toggle","name":"t","label":"On","value":true,"action":"flip"},
        {"type":"select","name":"s","options":["a","b"],"value":"b"},
        {"type":"progress","value":0.5,"label":"half"},
        {"type":"table","name":"g","columns":[{"key":"a"}],"rows":[[1],["x"],[null]]},
        {"type":"chart","spec":{"version":1,"type":"bar","title":"C","x":"k","series":[{"key":"v","label":"V"}],"data":[{"k":"a","v":1}]}},
        {"type":"separator"},
        {"type":"image","media":"plots/a.png","alt":"a plot"},
        {"type":"canvas","width":100,"height":50,"action":"hit","shapes":[
            {"type":"rect","x":0,"y":0,"w":10,"h":10,"fill":"accent"},{"type":"circle","x":5,"y":5,"r":3,"stroke":"#ff0000"},
            {"type":"line","points":[[0,0],[10,10]]},{"type":"text","x":1,"y":1,"text":"hi","color":"chart2"}]}]}"##);
    let names: Vec<&str> = n.children().iter().map(Node::type_name).collect();
    assert_eq!(names, ["row", "text", "markdown", "chip", "button", "input", "toggle", "select", "progress", "table", "chart", "separator", "image", "canvas"]);
    assert_eq!(n.key.as_deref(), Some("root"));
}

#[test]
fn unknown_and_broken_nodes_degrade_instead_of_failing() {
    let (n, faults) = decode(json!({"type": "column", "children": [
        {"type": "slider", "fallback": "Volume: 7"},
        {"type": "gauge"},
        {"type": "button", "label": "no action"},
        "not a node",
        {"text": "no type"},
        {"type": "chart", "spec": {"type": "donut"}, "fallback": "a chart"},
        {"type": "text", "text": "fine", "tone": "sparkly", "style": "huge"},
        {"type": "table", "columns": [{"key": "a"}, {"key": "b"}], "rows": [{"b": 2, "a": 1}, 5, [[1, 2], {"x": 1}]]},
    ]}));
    let kinds: Vec<&str> = n.children().iter().map(Node::type_name).collect();
    assert_eq!(kinds, ["text", "text", "text", "table"], "{n:#?}");
    assert_eq!(n.children()[0].kind, Kind::Text { text: "Volume: 7".into(), style: TextStyle::Body, tone: Tone::Dim });
    // Unknown tones and styles are the defaults, silently (they may be newer).
    assert_eq!(n.children()[2].kind, Kind::Text { text: "fine".into(), style: TextStyle::Body, tone: Tone::Normal });
    let Kind::Table { rows, .. } = &n.children()[3].kind else { panic!() };
    assert_eq!(rows, &vec![vec![json!(1), json!(2)], vec![], vec![json!("[1,2]"), json!("{\"x\":1}")]]);
    let report = Fault::report(&faults);
    for want in [
        "/children/0: unknown node type \"slider\"; showing its fallback",
        "/children/1: unknown node type \"gauge\"; left out",
        "/children/2: button: missing field `action`",
        "/children/3: a node must be an object",
        "/children/4: a node needs a \"type\"",
        "/children/5/spec: ",
        "/children/7/rows/1: a row must be a list",
    ] {
        assert!(report.contains(want), "{want:?} not in\n{report}");
    }
    assert_eq!(faults.len(), 7, "{report}");
    // Not JSON, or not a node at the root: an empty column and a fault, never an error.
    let (n, f) = decode_str("{nope");
    assert_eq!((n.count(), f.len()), (1, 1));
    let (n, f) = decode(json!([1, 2]));
    assert_eq!((n.type_name(), f.len()), ("column", 1));
}

#[test]
fn limits_are_applied_and_reported() {
    // Depth: 40 nested columns keep 32 levels.
    let mut v = json!({"type": "text", "text": "deep"});
    for _ in 0..40 {
        v = json!({"type": "column", "children": [v]});
    }
    let (n, f) = decode(v);
    assert_eq!(n.depth(), MAX_DEPTH);
    assert!(f.iter().any(|f| f.message.contains("too deep")), "{f:?}");
    // Node count.
    let many: Vec<Value> = (0..MAX_NODES + 50).map(|i| json!({"type": "text", "text": i.to_string()})).collect();
    let (n, f) = decode(json!({"type": "column", "children": many}));
    assert_eq!(n.count(), MAX_NODES);
    assert_eq!(f.len(), 1, "one fault, not one per node: {f:?}");
    // Long text, too many options, rows and columns, silly numbers.
    let long = "é".repeat(MAX_TEXT_CHARS + 10);
    let (n, f) = decode(json!({"type": "column", "children": [
        {"type": "text", "text": long},
        {"type": "select", "name": "s", "options": vec!["o"; MAX_OPTIONS + 1]},
        {"type": "table", "columns": vec![json!({"key": "k"}); MAX_TABLE_COLUMNS + 1], "rows": vec![json!([1]); MAX_TABLE_ROWS + 1]},
        {"type": "progress", "value": 7},
        {"type": "canvas", "width": 1e30, "height": -5, "shapes": [{"type": "circle", "x": 1e39, "y": 0, "r": 1}]},
    ]}));
    let c = n.children();
    assert!(matches!(&c[0].kind, Kind::Text { text, .. } if text.chars().count() == MAX_TEXT_CHARS));
    assert!(matches!(&c[1].kind, Kind::Select { options, .. } if options.len() == MAX_OPTIONS));
    assert!(matches!(&c[2].kind, Kind::Table { columns, rows, .. } if columns.len() == MAX_TABLE_COLUMNS && rows.len() == MAX_TABLE_ROWS));
    assert!(matches!(c[3].kind, Kind::Progress { value, .. } if value == 1.0));
    let Kind::Canvas { width, height, shapes, .. } = &c[4].kind else { panic!() };
    assert_eq!((*width, *height), (4000.0, 1.0));
    assert!(matches!(shapes[0], Shape::Circle { x, .. } if x.is_finite()));
    assert_eq!(f.len(), 4, "{f:?}");
    // Faults are capped.
    let bad: Vec<Value> = (0..200).map(|_| json!({"type": "nope"})).collect();
    let (_, f) = decode(json!({"type": "column", "children": bad}));
    assert_eq!(f.len(), MAX_FAULTS + 1);
    assert!(f.last().unwrap().message.contains("150 more"));
}

#[test]
fn replies_decode_tolerantly() {
    let (r, f) = Reply::parse(
        r#"{"view": {"type": "text", "text": "hi"}, "patch": [
            {"set": {"key": "count", "fields": {"text": "3 left"}}},
            {"replace": {"key": "x", "node": {"type": "chip", "text": "c"}}},
            {"append": {"key": "list", "nodes": [{"type": "separator"}]}},
            {"remove": {"key": "old"}},
            {"rows": {"key": "grid", "start": 2, "rows": [[1, 2]]}},
            {"put": {"path": "/children/0/text", "value": "x"}},
            {"frobnicate": {"key": "a"}},
            {"set": {"fields": {}}},
            {"set": {"key": "a"}, "remove": {"key": "b"}}
        ], "values": {"new": ""}}"#,
    )
    .unwrap();
    assert!(r.view.is_some());
    assert_eq!(r.patch.len(), 6);
    assert_eq!(r.values.get("new"), Some(&json!("")));
    assert_eq!(f.len(), 3, "{f:?}");
    assert_eq!(f[0].path, "/patch/6/frobnicate");
    // Patches survive a serde round trip in the documented shape.
    let s = serde_json::to_string(&r.patch).unwrap();
    assert!(s.contains(r#"{"set":{"key":"count","fields":{"text":"3 left"}}}"#), "{s}");
    let back: Vec<Patch> = serde_json::from_str(&s).unwrap();
    assert_eq!(back, r.patch);
    assert!(Reply::parse("[1]").is_err() && Reply::parse("print('hi')").is_err());
    // An event as a plugin reads it.
    let e = Event { action: "edit".into(), values: Map::new(), edit: Some(CellEdit { table: "g".into(), row: 3, column: "qty".into(), value: "5".into() }) };
    assert_eq!(serde_json::to_string(&e).unwrap(), r#"{"action":"edit","values":{},"edit":{"table":"g","row":3,"column":"qty","value":"5"}}"#);
}

fn todo(items: &[(&str, bool)]) -> Node {
    let item = |i: usize, (t, done): (&str, bool)| {
        json!({"type": "row", "key": format!("item-{i}"), "children": [
            {"type": "toggle", "name": format!("done-{i}"), "label": t, "value": done, "action": format!("toggle:{i}")},
            {"type": "button", "label": "Remove", "action": format!("remove:{i}"), "danger": true}]})
    };
    let left = items.iter().filter(|(_, d)| !d).count();
    let v = json!({"type": "card", "key": "root", "title": "Todos", "children": [
        {"type": "row", "children": [
            {"type": "input", "name": "new", "placeholder": "What needs doing?", "submit": "add"},
            {"type": "button", "label": "Add", "action": "add", "primary": true},
            {"type": "chip", "key": "count", "text": format!("{left} left"), "tone": if left > 0 { "accent" } else { "success" }}]},
        {"type": "column", "key": "list", "children": items.iter().enumerate().map(|(i, it)| item(i, *it)).collect::<Vec<_>>()}]});
    decode(v).0
}

#[test]
fn keyed_patches_apply_and_bad_ones_are_reported() {
    let mut t = todo(&[("milk", false), ("eggs", true)]);
    let patch: Vec<Patch> = serde_json::from_value(json!([
        {"set": {"key": "count", "fields": {"text": "0 left", "tone": "success"}}},
        {"append": {"key": "list", "nodes": [{"type": "text", "text": "new"}]}},
        {"remove": {"key": "item-0"}},
        {"replace": {"key": "item-1", "node": {"type": "text", "key": "item-1", "text": "replaced"}}},
    ]))
    .unwrap();
    let faults = apply(&mut t, patch);
    assert!(faults.is_empty(), "{faults:?}");
    assert_eq!(t.find("count").unwrap().kind, Kind::Chip { text: "0 left".into(), tone: Tone::Success });
    let list = t.find("list").unwrap().children();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].kind, Kind::Text { text: "replaced".into(), style: TextStyle::Body, tone: Tone::Normal });
    let before = t.clone();
    let bad: Vec<Patch> = serde_json::from_value(json!([
        {"set": {"key": "nope", "fields": {"text": "x"}}},
        {"set": {"key": "count", "fields": {"type": "button"}}},
        {"set": {"key": "count", "fields": {"text": 5}}},
        {"append": {"key": "count", "nodes": [{"type": "separator"}]}},
        {"remove": {"path": ""}},
        {"rows": {"key": "list", "rows": []}},
        {"put": {"path": "/children/9/text", "value": "x"}},
        {"put": {"path": "nope", "value": 1}},
    ]))
    .unwrap();
    let faults = apply(&mut t, bad);
    assert_eq!(t, before, "nothing changed");
    assert_eq!(faults.len(), 8, "{faults:#?}");
    assert!(faults[0].to_string().starts_with("/patch/0: no node has key \"nope\""), "{}", faults[0]);
    // Limits hold across patches.
    let mut t = todo(&[]);
    let big: Vec<Value> = (0..MAX_NODES).map(|_| json!({"type": "separator"})).collect();
    let p: Vec<Patch> = serde_json::from_value(json!([{"append": {"key": "list", "nodes": big}}])).unwrap();
    let f = apply(&mut t, p);
    assert!(f.iter().any(|f| f.message.contains("more than 20000 nodes")), "{f:?}");
    assert_eq!(t.find("list").unwrap().children().len(), 0);
}

#[test]
fn rows_and_pointer_puts_apply() {
    let mut t = grid(10);
    let p: Vec<Patch> = serde_json::from_value(json!([
        {"rows": {"key": "grid", "start": 8, "rows": [["a"], ["b"], ["c"]]}},
        {"put": {"path": "/children/0/rows/0/2", "value": 99}},
        {"put": {"path": "/children/0/rows/1", "value": {"sku": "obj", "qty": 1}}},
        {"put": {"path": "/children/0/columns/2/label", "value": "Qty"}},
        {"put": {"path": "/children/0/height", "value": 200}},
    ]))
    .unwrap();
    let f = apply(&mut t, p);
    assert!(f.is_empty(), "{f:?}");
    let Kind::Table { rows, columns, height, .. } = &t.children()[0].kind else { panic!() };
    assert_eq!(rows.len(), 11);
    assert_eq!(rows[10], vec![json!("c")]);
    assert_eq!(rows[0][2], json!(99));
    assert_eq!(rows[1], vec![json!("obj"), Value::Null, json!(1), Value::Null, Value::Null]);
    assert_eq!((columns[2].label.as_str(), *height), ("Qty", Some(200.0)));
    let p: Vec<Patch> = serde_json::from_value(json!([{"rows": {"key": "grid", "len": 3}}])).unwrap();
    assert!(apply(&mut t, p).is_empty());
    assert!(matches!(&t.children()[0].kind, Kind::Table { rows, .. } if rows.len() == 3));
}

/// Apply `diff(a, b)` to `a` (through JSON, as it travels) and check it gives `b`.
fn round_trip(a: &Node, b: &Node) -> Vec<Patch> {
    let patch = diff(a, b);
    let wire = serde_json::to_string(&patch).unwrap();
    let decoded: Vec<Patch> = serde_json::from_str(&wire).unwrap();
    let mut c = a.clone();
    let f = apply(&mut c, decoded);
    assert!(f.is_empty(), "{f:?}\n{wire}");
    assert_eq!(&c, b, "patch: {wire}");
    patch
}

#[test]
fn diff_then_apply_gives_the_new_tree() {
    let a = todo(&[("milk", false), ("eggs", true), ("jam", false)]);
    assert!(diff(&a, &a).is_empty());
    // A toggle: two field puts (the toggle's value and the count chip).
    let b = todo(&[("milk", true), ("eggs", true), ("jam", false)]);
    let p = round_trip(&a, &b);
    assert!(p.iter().all(|p| matches!(p, Patch::Put { .. })) && p.len() <= 3, "{p:?}");
    // Added at the end: an append; one removed: a remove (plus renumbered actions).
    let c = todo(&[("milk", false), ("eggs", true), ("jam", false), ("tea", false)]);
    let p = round_trip(&a, &c);
    assert!(p.iter().any(|p| matches!(p, Patch::Append { .. })), "{p:?}");
    round_trip(&a, &todo(&[("milk", false), ("jam", false)]));
    round_trip(&a, &todo(&[]));
    round_trip(&todo(&[]), &a);
    // Type changes, keys, root replacement, every node kind.
    let d = ok(DASHBOARD);
    let s = ok(SETTINGS);
    round_trip(&d, &s);
    round_trip(&s, &d);
    round_trip(&ok(CANVAS), &d);
    let mut d2 = serde_json::to_value(&d).unwrap();
    d2["children"][1]["children"][0]["title"] = json!("Builds this week");
    d2["children"][2]["spec"]["title"] = json!("Minutes");
    d2["children"][3]["rows"][1][1] = json!("linker OOM");
    d2["children"][3]["columns"][0]["width"] = json!(200);
    d2["children"][0]["children"][1]["key"] = json!("status2");
    round_trip(&d, &decode(d2).0);
    let mut c2 = serde_json::to_value(ok(CANVAS)).unwrap();
    c2["children"][0]["shapes"][2]["w"] = json!(10);
    c2["children"][0]["shapes"].as_array_mut().unwrap().pop();
    round_trip(&ok(CANVAS), &decode(c2).0);
}

#[test]
fn diff_of_a_10k_row_table_is_small() {
    let a = grid(10_000);
    let mut b = a.clone();
    let Kind::Table { rows, .. } = &mut b.children_mut().unwrap()[0].kind else { panic!() };
    rows[4321][2] = json!(9);
    rows[4321][4] = json!(99);
    let p = round_trip(&a, &b);
    let bytes = serde_json::to_string(&p).unwrap().len();
    assert!(bytes < 150, "{bytes} bytes: {p:?}");
    // Many rows changed: one rows patch; rows added and cut.
    let mut c = a.clone();
    let Kind::Table { rows, .. } = &mut c.children_mut().unwrap()[0].kind else { panic!() };
    rows.iter_mut().skip(100).step_by(3).for_each(|r| r[1] = json!("changed"));
    rows.truncate(9000);
    let p = round_trip(&a, &c);
    assert_eq!(p.len(), 1, "{:?}", p.len());
    let mut d = a.clone();
    let Kind::Table { rows, .. } = &mut d.children_mut().unwrap()[0].kind else { panic!() };
    rows.push(vec![json!("new")]);
    rows[0][0] = json!("first");
    round_trip(&a, &d);
}

fn time(n: u32, mut f: impl FnMut()) -> Duration {
    let start = Instant::now();
    for _ in 0..n {
        f();
    }
    start.elapsed() / n
}

/// The prototype's measurements as a test with generous limits (debug builds are slow):
/// decoding a 10k-row table, diffing a re-sent view, row and `set` patches on it.
#[test]
fn a_10k_row_table_is_fast_enough() {
    let tree = grid(10_000);
    let json = serde_json::to_string(&tree).unwrap();
    let parse = time(3, || {
        let (n, f) = decode_str(&json);
        assert!(f.is_empty() && n.count() == 2);
    });
    let mut b = tree.clone();
    let Kind::Table { rows, .. } = &mut b.children_mut().unwrap()[0].kind else { panic!() };
    rows[4321][2] = json!(9);
    let diff_t = time(3, || assert_eq!(diff(&tree, &b).len(), 1));
    let mut t = tree.clone();
    let mut i = 0;
    let rows_t = time(200, || {
        i += 1;
        let p: Vec<Patch> = serde_json::from_str(&format!(r#"[{{"rows":{{"key":"grid","start":{i},"rows":[["a","b",1,2,2]]}}}}]"#)).unwrap();
        assert!(apply(&mut t, p).is_empty());
    });
    // The prototype's slow path: `set` on the table node round-tripped all 10k rows.
    let set_t = time(50, || {
        let p: Vec<Patch> = serde_json::from_str(r#"[{"set":{"key":"grid","fields":{"height":500}}}]"#).unwrap();
        assert!(apply(&mut t, p).is_empty());
    });
    eprintln!("10k-row table: {} bytes; decode {parse:?}, diff {diff_t:?}, rows patch {rows_t:?}, set {set_t:?}", json.len());
    assert!(parse < Duration::from_millis(500), "{parse:?}");
    assert!(diff_t < Duration::from_millis(200), "{diff_t:?}");
    assert!(rows_t < Duration::from_millis(5), "{rows_t:?}");
    assert!(set_t < Duration::from_millis(5), "set must not touch the rows: {set_t:?}");
}

#[test]
fn trees_travel_as_views_with_a_markdown_fallback() {
    let d = ok(DASHBOARD);
    let v = d.to_view();
    assert_eq!((v.kind.as_str(), v.v), (KIND, VERSION));
    assert!(v.check().is_ok());
    assert_eq!(v.get::<Node>(), Some(d));
    for want in ["**Build health**", "| Job | Error |", "| linux-x64 | test timeout |", "*Chart: Build minutes*", "`3 failing`"] {
        assert!(v.fallback.contains(want), "{want:?} not in\n{}", v.fallback);
    }
    let big = grid(100);
    assert!(big.fallback_markdown().contains("80 more rows"));
}
