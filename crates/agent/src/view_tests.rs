//! Engine tests of live views at render sites, through the demo plugin's real Python
//! backend (`examples/plugins/todo-demo`). Skipped without `python3`.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use byob_protocol::sites::LiveView;
use byob_protocol::tree::{self, Event, Node};
use byob_protocol::{ClientMsg, Connection, ServerMsg, Site, ViewRef};
use serde_json::{Value, json};

use crate::LocalConnection;
use crate::backend_tests::{H, find, harness, have_python, hello, recv, turn};
use crate::loop_tests::{Reply, call};

const MANIFEST: &str = include_str!("../../../examples/plugins/todo-demo/byob-plugin.json");
const BACKEND: &str = include_str!("../../../examples/plugins/todo-demo/backend.py");

fn write_demo(root: &Path) {
    let dir = root.join("todo-demo");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("byob-plugin.json"), MANIFEST).unwrap();
    fs::write(dir.join("backend.py"), BACKEND).unwrap();
}

fn demo(view: &str, key: &str) -> ViewRef {
    ViewRef::new("todo-demo", view, key)
}

/// The next `LiveView` for `r`.
fn live(c: &mut LocalConnection, r: &ViewRef) -> LiveView {
    loop {
        match recv(c) {
            ServerMsg::LiveView(lv) if lv.view == *r => return lv,
            ServerMsg::Notice { text, .. } => panic!("notice: {text}"),
            _ => {}
        }
    }
}

fn event(action: &str, values: Value) -> Event {
    Event { action: action.into(), values: values.as_object().cloned().unwrap_or_default(), edit: None }
}

/// What a client does with a `LiveView`.
fn apply(tree: &mut Option<Node>, lv: LiveView) {
    assert_eq!(lv.error, None);
    if let Some(t) = lv.tree {
        *tree = Some(t);
    }
    if !lv.patch.is_empty() {
        let faults = tree::apply(tree.as_mut().expect("patches come after a tree"), lv.patch);
        assert!(faults.is_empty(), "{faults:?}");
    }
}

fn text(tree: &Option<Node>) -> String {
    serde_json::to_string(tree).unwrap()
}

fn open(c: &mut LocalConnection, r: &ViewRef) -> Option<Node> {
    c.send(ClientMsg::OpenView { view: r.clone() });
    let mut tree = None;
    apply(&mut tree, live(c, r));
    tree
}

fn second(h: &H) -> LocalConnection {
    hello(h).0
}

#[test]
fn a_pane_view_is_owned_by_the_engine_and_reaches_every_client() {
    if !have_python() {
        return;
    }
    let h = harness(vec![Reply::Text("hi")], write_demo);
    let (mut a, ext) = hello(&h);
    let sites: Vec<Site> = find(&ext, "todo-demo").views.iter().map(|v| v.site).collect();
    assert_eq!(sites, [Site::Pane, Site::Status, Site::Settings, Site::Card]);
    let mut b = second(&h);
    let list = demo("list", "");
    let mut ta = open(&mut a, &list);
    assert!(text(&ta).contains("Nothing to do."), "{}", text(&ta));
    let mut tb = open(&mut b, &list);
    assert_eq!(ta, tb);
    let count = demo("count", "");
    let mut chip = open(&mut b, &count);
    assert!(text(&chip).contains("No to-dos"));

    a.send(ClientMsg::ViewEvent { view: list.clone(), event: event("add", json!({"new": "buy milk"})) });
    let la = live(&mut a, &list);
    assert!(!la.patch.is_empty() && la.tree.is_none(), "a diff, not the whole view: {la:?}");
    assert_eq!(la.values.get("new"), Some(&json!("")), "the reply's values go to the client that acted");
    apply(&mut ta, la);
    let lb = live(&mut b, &list);
    assert!(lb.values.is_empty(), "…and only to it");
    apply(&mut tb, lb);
    assert!(text(&ta).contains("buy milk"));
    assert_eq!(ta, tb, "both clients show the engine's view");
    // the status chip follows the same state
    apply(&mut chip, live(&mut b, &count));
    assert!(text(&chip).contains("1 to do"), "{}", text(&chip));

    // a client that opens it later gets the cached tree at once
    let mut c = second(&h);
    let tc = open(&mut c, &list);
    assert_eq!(tc, ta);

    // a client that closed it hears no more
    b.send(ClientMsg::CloseView { view: list.clone() });
    a.send(ClientMsg::ViewEvent { view: list.clone(), event: event("toggle:1", json!({})) });
    apply(&mut ta, live(&mut a, &list));
    apply(&mut chip, live(&mut b, &count));
    assert!(text(&chip).contains("0 to do"), "{}", text(&chip));
    std::thread::sleep(Duration::from_millis(100));
    while let Some(m) = b.try_recv() {
        assert!(!matches!(&m, ServerMsg::LiveView(lv) if lv.view == list), "{m:?}");
    }
}

#[test]
fn a_backend_tool_attaches_a_live_card_to_its_step() {
    if !have_python() {
        return;
    }
    let h = harness(vec![call("todo_add", r#"{"text": "call mum"}"#), Reply::Text("added")], write_demo);
    let (mut a, _) = hello(&h);
    let entry = turn(&mut a, "remind me to call mum");
    let step = entry.steps.iter().find(|s| s.tool == "todo_add").unwrap();
    assert!(step.detail.starts_with("Added 'call mum'"), "{}", step.detail);
    let view = step.views.first().expect("the card");
    assert_eq!(view.kind, tree::KIND);
    let owner = view.owner.clone().expect("owned by the plugin");
    assert_eq!(owner, demo("item", "1"));
    assert!(view.fallback.contains("call mum"), "{}", view.fallback);
    // live: the cached card, then events change it
    let mut card = open(&mut a, &owner);
    assert!(text(&card).contains("Mark done"));
    a.send(ClientMsg::ViewEvent { view: owner.clone(), event: event("toggle", json!({})) });
    apply(&mut card, live(&mut a, &owner));
    assert!(text(&card).contains("Mark open"), "{}", text(&card));
}

#[test]
fn reloading_the_plugin_drops_its_views_and_clients_open_them_again() {
    if !have_python() {
        return;
    }
    let h = harness(vec![Reply::Text("hi")], write_demo);
    let (mut a, _) = hello(&h);
    let list = demo("list", "");
    let mut t = open(&mut a, &list);
    a.send(ClientMsg::ViewEvent { view: list.clone(), event: event("add", json!({"new": "x"})) });
    apply(&mut t, live(&mut a, &list));
    assert!(text(&t).contains("\"x\""));
    a.send(ClientMsg::ReloadPlugins);
    let lv = live(&mut a, &list);
    assert!(lv.reset, "{lv:?}");
    let t = open(&mut a, &list);
    assert!(text(&t).contains("Nothing to do."), "a fresh backend, fresh state: {}", text(&t));
}

#[test]
fn tree_faults_go_to_the_plugins_status_and_the_site() {
    if !have_python() {
        return;
    }
    let status = json!({"plugin": "broken"}).to_string();
    let h = harness(vec![call("plugin_status", &status), Reply::Text("ok")], |root| {
        let dir = root.join("broken");
        fs::create_dir_all(&dir).unwrap();
        let m = json!({"id": "broken", "name": "Broken", "backend": "backend.py",
                       "views": [{"site": "pane", "id": "main"}, {"site": "status", "id": "oops"}]});
        fs::write(dir.join("byob-plugin.json"), m.to_string()).unwrap();
        let code = r#"
import byob_plugin as byob

@byob.view("main")
def main():
    return {"type": "column", "children": [{"type": "button", "label": "Go"}, {"type": "slider", "fallback": "Volume 7"}]}

@byob.view("oops")
def oops():
    raise ValueError("no chip today")

byob.run()
"#;
        fs::write(dir.join("backend.py"), code).unwrap();
    });
    let (mut a, _) = hello(&h);
    let main = ViewRef::new("broken", "main", "");
    a.send(ClientMsg::OpenView { view: main.clone() });
    let lv = live(&mut a, &main);
    let faults = lv.faults.join("\n");
    assert!(faults.contains("/children/0") && faults.contains("action"), "{faults}");
    assert!(text(&lv.tree.clone()).contains("Volume 7"), "an unknown node shows its fallback");
    let oops = ViewRef::new("broken", "oops", "");
    a.send(ClientMsg::OpenView { view: oops.clone() });
    let lv = live(&mut a, &oops);
    assert!(lv.error.as_deref().unwrap_or_default().contains("no chip today"), "{lv:?}");
    // an undeclared view is an error at the site
    let nope = ViewRef::new("broken", "nope", "");
    a.send(ClientMsg::OpenView { view: nope.clone() });
    assert!(live(&mut a, &nope).error.unwrap_or_default().contains("has no view \"nope\" (its views: main, oops)"));
    // the model sees both in plugin_status
    turn(&mut a, "what's wrong with it?");
    // (what the model got back from plugin_status, in the next request)
    let detail = serde_json::to_string(&h.requests.lock().unwrap().last().cloned()).unwrap().replace("\\\"", "\"");
    assert!(detail.contains("byob had to fix its tree") && detail.contains("no chip today"), "{detail}");
    assert!(detail.contains("missing field `action`"), "{detail}");
}

#[test]
fn clients_without_the_cap_get_no_live_views() {
    if !have_python() {
        return;
    }
    let h = harness(vec![Reply::Text("hi")], write_demo);
    let mut old = h.engine.connect(Arc::new(|| {}));
    let caps = vec![byob_protocol::version::CAP_VIEWS.to_owned(), byob_protocol::version::CAP_PLUGIN_CHANNEL.to_owned()];
    old.send(ClientMsg::Hello { protocol: byob_protocol::PROTOCOL_MAJOR, minor: 2, client: "13.2".into(), caps });
    old.send(ClientMsg::OpenView { view: demo("list", "") });
    let (mut a, _) = hello(&h);
    let _ = open(&mut a, &demo("list", ""));
    std::thread::sleep(Duration::from_millis(200));
    while let Some(m) = old.try_recv() {
        assert!(!matches!(m, ServerMsg::LiveView(_)), "{m:?}");
    }
}
