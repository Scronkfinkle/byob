//! Native plugin backends (`"backend": {"native": id}`, `byob_plugins::native`): hosted like
//! process backends (init, requests, views, a panic is a crash and a restart, switch-off
//! drops them), and the bundled `sheets` plugin, whose backend is native (`byob-sheets`).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use byob_plugins::Embedded;
use byob_plugins::native::{NativeBackend, NativeFactory, Outbox};
use byob_protocol::sites::artifact_key;
use byob_protocol::{ApprovalAnswer, ClientMsg, ConfigScope, Connection, PluginInfo, ServerMsg, StepStatus, ViewRef};
use byob_store::Paths;
use byob_types::InferenceConfig;
use serde_json::{Value, json};

use crate::extensions::PluginsConfig;
use crate::extensions_tests::{recv, step, tool_names, turn, wait_ext};
use crate::loop_tests::{Reply, Requests, call, fake_llm};
use crate::natives::{Natives, bundled_plugins};
use crate::{EngineConfig, EngineHandle, LocalConnection, Policy};

/// A native backend that counts pokes, remembers its init, and panics when asked.
#[derive(Default)]
struct Echo {
    restart: i64,
    plugin: String,
    pokes: u32,
}

impl NativeBackend for Echo {
    fn init(&mut self, init: &Value, out: &Outbox) {
        self.restart = init["restart"].as_i64().unwrap_or(-1);
        self.plugin = init["plugin"].as_str().unwrap_or_default().to_owned();
        out.log("echo is up");
    }

    fn request(&mut self, method: &str, params: &Value, out: &Outbox) -> Result<Value, String> {
        match method {
            "tool" if params["args"]["panic"] == true => panic!("boom on purpose"),
            "tool" => {
                self.pokes += 1;
                out.view("p", "", json!({"type": "text", "text": format!("pokes: {}", self.pokes)}));
                Ok(json!(format!("poked {} times, restart {}, plugin {}", self.pokes, self.restart, self.plugin)))
            }
            "view" => Ok(json!({"type": "text", "text": format!("pokes: {}", self.pokes)})),
            other => Err(format!("no {other}")),
        }
    }
}

fn echo_factory() -> NativeFactory {
    Arc::new(|| Box::new(Echo::default()) as Box<dyn NativeBackend>)
}

fn echo_plugin(id: &str, native: &str) -> Embedded {
    let manifest = json!({
        "id": id, "name": "Echo", "backend": {"native": native},
        "views": [{"site": "pane", "id": "p", "title": "Pokes"}],
        "tools": [{"name": "poke", "description": "Poke it", "run": "backend",
                   "params": {"type": "object", "properties": {"panic": {"type": "boolean"}}}}]
    });
    Embedded { id: id.into(), files: [("byob-plugin.json".to_owned(), manifest.to_string())].into_iter().collect::<BTreeMap<_, _>>() }
}

struct H {
    engine: EngineHandle,
    requests: Requests,
    home: tempfile::TempDir,
    _ws: tempfile::TempDir,
}

fn harness(script: Vec<Reply>, bundled: Vec<Embedded>, natives: Natives) -> H {
    harness_with(script, bundled, natives, Policy::ALLOW_ALL)
}

fn harness_with(script: Vec<Reply>, bundled: Vec<Embedded>, natives: Natives, policy: Policy) -> H {
    let (url, requests) = fake_llm(script, "final answer");
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let mut config = EngineConfig::new(InferenceConfig { base_url: url, ..Default::default() });
    config.workspace = ws.path().canonicalize().unwrap();
    config.register_workspace = true;
    config.tools = byob_tools::builtin();
    config.policy = Some(policy);
    config.paths = Some(Paths::at(home.path()));
    config.auto_title = false;
    config.plugins = Some(PluginsConfig {
        global_roots: vec![home.path().join("plugins")],
        scratch_root: Some(home.path().join("chats")),
        python_root: Some(home.path().join("python")),
        python: byob_plugins::default_python(),
        bundled,
        bundled_cache: Some(home.path().join("bundled-plugins")),
        natives,
        ..Default::default()
    });
    H { engine: EngineHandle::spawn_with(config).unwrap(), requests, home, _ws: ws }
}

fn connect(h: &H) -> LocalConnection {
    let mut c = h.engine.connect(Arc::new(|| {}));
    c.send(ClientMsg::hello("test"));
    c
}

/// The plugin list after a turn: the last one it sent, or the next one.
fn ext_after(c: &mut LocalConnection, seen: Vec<ServerMsg>, ok: impl Fn(&byob_protocol::Extensions) -> bool) -> byob_protocol::Extensions {
    let last = seen.into_iter().rev().find_map(|m| if let ServerMsg::Extensions(e) = m { Some(*e) } else { None });
    match last {
        Some(e) if ok(&e) => e,
        _ => wait_ext(c, ok),
    }
}

fn info<'a>(e: &'a byob_protocol::Extensions, id: &str) -> &'a PluginInfo {
    e.plugins.iter().find(|p| p.id == id).unwrap_or_else(|| panic!("no plugin {id}"))
}

#[test]
fn a_native_backend_is_hosted_like_a_process_one() {
    let script = vec![
        call("poke", "{}"),
        Reply::Text("one"),
        call("poke", r#"{"panic": true}"#),
        Reply::Text("two"),
        call("poke", "{}"),
        Reply::Text("three"),
        call("poke", "{}"),
        Reply::Text("four"),
    ];
    let h = harness(script, vec![echo_plugin("echo", "echo"), echo_plugin("ghost", "nobody")], Natives::builtin().backend("echo", echo_factory()));
    let mut c = connect(&h);
    let e = wait_ext(&mut c, |e| !e.plugins.is_empty());
    let p = info(&e, "echo");
    assert!(p.problems.is_empty(), "{:?}", p.problems);
    assert_eq!(p.backend.as_ref().map(|b| b.state.as_str()), Some("idle"));
    assert!(!Path::new(&p.dir).exists(), "in-tree code needs no files on disk: {}", p.dir);
    assert!(!h.home.path().join("bundled-plugins").exists() || std::fs::read_dir(h.home.path().join("bundled-plugins")).unwrap().next().is_none());
    // An id nobody registered is the plugin's problem, not a crash.
    assert!(info(&e, "ghost").problems.iter().any(|m| m.contains("\"nobody\" isn't a backend built into this byob")), "{:?}", info(&e, "ghost").problems);

    // Started on first use; init first; its tool served on its thread.
    let (entry, seen) = turn(&mut c, "poke it", |_, _| {});
    let s = step(&entry, "poke");
    assert_eq!(s.status, StepStatus::Done, "{}", s.detail);
    assert!(s.detail.contains("poked 1 times, restart 0, plugin echo"), "{}", s.detail);
    // Its log line comes in its own update, after the state changes.
    let e = ext_after(&mut c, seen, |e| info(e, "echo").backend.as_ref().is_some_and(|b| b.stderr.contains("(log) echo is up")));
    assert!(info(&e, "echo").backend.as_ref().is_some_and(|b| b.starts == 1), "started once");

    // A panic is a crash: the call fails, a fault is kept, the next use starts a fresh one.
    let (entry, seen) = turn(&mut c, "panic", |_, _| {});
    let s = step(&entry, "poke");
    assert_eq!(s.status, StepStatus::Failed, "{}", s.detail);
    assert!(s.detail.contains("panicked: boom on purpose"), "{}", s.detail);
    let e = ext_after(&mut c, seen, |e| {
        info(e, "echo").backend.as_ref().is_some_and(|b| b.faults.iter().any(|f| f.contains("the native backend panicked: boom on purpose")))
    });
    assert_eq!(info(&e, "echo").backend.as_ref().unwrap().starts, 1);
    let (entry, _) = turn(&mut c, "again", |_, _| {});
    assert!(step(&entry, "poke").detail.contains("poked 1 times, restart 1"), "state was dropped: {}", step(&entry, "poke").detail);

    // Its pane view: drawn on request, updated by its messages.
    let v = ViewRef::new("echo", "p", "");
    c.send(ClientMsg::OpenView { view: v.clone() });
    loop {
        if let ServerMsg::LiveView(lv) = recv(&mut c)
            && lv.view == v
        {
            assert!(serde_json::to_string(&lv.tree).unwrap().contains("pokes: 1"), "{lv:?}");
            break;
        }
    }

    // Switched off: dropped (its view resets); on again: a new backend from scratch.
    c.send(ClientMsg::SetPluginEnabled { scope: ConfigScope::Global, plugin: "echo".into(), enabled: Some(false) });
    loop {
        if let ServerMsg::LiveView(lv) = recv(&mut c)
            && lv.view == v
        {
            assert!(lv.reset, "{lv:?}");
            break;
        }
    }
    c.send(ClientMsg::SetPluginEnabled { scope: ConfigScope::Global, plugin: "echo".into(), enabled: Some(true) });
    wait_ext(&mut c, |e| info(e, "echo").enabled);
    let (entry, _) = turn(&mut c, "after the switch", |_, _| {});
    assert!(step(&entry, "poke").detail.contains("poked 1 times, restart 0"), "{}", step(&entry, "poke").detail);
    assert_eq!(h.requests.lock().unwrap().len(), 8);
}

/// Says what its `init` carried.
#[derive(Default)]
struct Peek {
    init: Value,
}

impl NativeBackend for Peek {
    fn init(&mut self, init: &Value, _: &Outbox) {
        self.init = init.clone();
    }

    fn request(&mut self, _: &str, _: &Value, _: &Outbox) -> Result<Value, String> {
        let token = self.init["secrets"]["TOKEN"].as_str().unwrap_or_default();
        Ok(json!(format!(
            "token {} chars, city {}, token in settings: {}",
            token.len(),
            self.init["settings"]["CITY"],
            self.init["settings"].get("TOKEN").is_some()
        )))
    }
}

#[test]
fn a_native_backend_gets_its_secrets_in_init_and_its_read_tools_run_unasked() {
    let manifest = json!({
        "id": "keyed", "name": "Keyed", "backend": {"native": "peek"},
        "settings": [{"key": "TOKEN", "label": "Token", "secret": true}, {"key": "CITY", "label": "City", "default": "Paris"}],
        "tools": [{"name": "peek", "description": "Peek", "run": "backend", "access": "read"}]
    });
    let plugin = Embedded { id: "keyed".into(), files: [("byob-plugin.json".to_owned(), manifest.to_string())].into_iter().collect() };
    let factory: NativeFactory = Arc::new(|| Box::new(Peek::default()) as Box<dyn NativeBackend>);
    // The default policy asks before `execute` tools: this one says it only reads.
    let h = harness_with(vec![call("peek", "{}"), Reply::Text("done")], vec![plugin], Natives::builtin().backend("peek", factory), Policy::default());
    let mut c = connect(&h);
    wait_ext(&mut c, |e| e.plugins.iter().any(|p| p.id == "keyed"));
    let secret = "s3cret-token";
    c.send(ClientMsg::SetPluginSetting { plugin: "keyed".into(), key: "TOKEN".into(), value: Some(secret.into()) });
    wait_ext(&mut c, |e| e.plugins.iter().any(|p| p.id == "keyed" && p.settings.iter().all(|s| s.set)));
    let (entry, seen) = turn(&mut c, "peek", |s, _| panic!("a read tool was asked about: {}", s.title));
    let s = step(&entry, "peek");
    assert_eq!(s.status, StepStatus::Done, "{}", s.detail);
    assert_eq!(s.detail, format!("token {} chars, city \"Paris\", token in settings: false", secret.len()));
    for m in &seen {
        assert!(!m.to_json().contains(secret), "a secret went to a client: {}", m.to_json());
    }
}

fn sheets_natives() -> Natives {
    Natives::builtin().backend(byob_sheets::ID, byob_sheets::factory())
}

#[test]
fn the_sheets_plugin_is_native_and_on_by_default() {
    let script = vec![
        call("create_sheet", r#"{"title": "Budget"}"#),
        call("write_cells", r#"{"sheet": "budget.xlsx", "rows": [["Rent", 1200], ["Food", "=B1/4"]]}"#),
        Reply::Text("made"),
    ];
    let h = harness(script, bundled_plugins(), sheets_natives());
    let mut c = connect(&h);
    // Shipped on, in use, nothing on disk.
    let e = wait_ext(&mut c, |e| !e.plugins.is_empty());
    let p = info(&e, "sheets");
    assert!(p.enabled && p.in_use() && p.tools.contains(&"create_sheet".to_owned()), "{p:?}");
    assert!(p.problems.is_empty() && p.superseded_by.is_none(), "{p:?}");
    assert!(!Path::new(&p.dir).exists(), "in the binary, not at {}", p.dir);

    // The model gets the sheet tools, served in-process.
    let (entry, _) = turn(&mut c, "make a budget", |_, _| {});
    let made = step(&entry, "create_sheet");
    assert_eq!(made.status, StepStatus::Done, "{}", made.detail);
    assert!(made.detail.contains("New spreadsheet \"Budget\" (budget.xlsx).") && made.detail.contains("Saved: artifact"), "{}", made.detail);
    let wrote = step(&entry, "write_cells");
    assert!(wrote.detail.contains("Wrote 4 cells.") && wrote.detail.contains("Food 300"), "{}", wrote.detail);
    let tools = tool_names(&h.requests.lock().unwrap()[0]);
    assert_eq!(tools.iter().filter(|t| t.contains("create_sheet")).count(), 1, "{tools:?}");
    let id = wrote.artifacts[0].id.clone();
    let meta = byob_tools::artifact::load(h._ws.path(), &id).unwrap();
    assert_eq!(meta.artifact.versions.len(), 1, "made and written in one request: one version");

    // Its grid draws the artifact.
    let v = ViewRef::new("sheets", "sheet", artifact_key(&id, 1));
    c.send(ClientMsg::OpenView { view: v.clone() });
    loop {
        if let ServerMsg::LiveView(lv) = recv(&mut c)
            && lv.view == v
        {
            assert_eq!(lv.error, None);
            let tree = serde_json::to_string(&lv.tree).unwrap();
            assert!(tree.contains("\"grid\"") && tree.contains("\"300\"") && tree.contains("=B1/4"), "{tree}");
            break;
        }
    }

    // Switched off: its view resets, its tools go, and the view can't be opened now.
    c.send(ClientMsg::SetPluginEnabled { scope: ConfigScope::Global, plugin: "sheets".into(), enabled: Some(false) });
    let (mut reset, mut ext) = (false, None);
    while !reset || ext.is_none() {
        match recv(&mut c) {
            ServerMsg::LiveView(lv) if lv.view == v => {
                assert!(lv.reset || lv.error.is_some(), "{lv:?}");
                reset = true;
            }
            ServerMsg::Extensions(e) if !info(&e, "sheets").enabled => ext = Some(*e),
            _ => {}
        }
    }
    assert!(info(ext.as_ref().unwrap(), "sheets").tools.is_empty());
    c.send(ClientMsg::OpenView { view: v.clone() });
    loop {
        if let ServerMsg::LiveView(lv) = recv(&mut c)
            && lv.view == v
        {
            assert!(lv.error.is_some(), "{lv:?}");
            break;
        }
    }
}

/// Charts drawn from a sheet stay in step with it: chart_sheet links a chart to the sheet's
/// columns (formula results included), and every later save of the sheet draws it again,
/// within the same request (one version) and in the next (a new one).
#[test]
fn a_chart_drawn_from_a_sheet_follows_it() {
    let script = vec![
        call("create_sheet", r#"{"title": "Profit"}"#),
        call("write_cells", r#"{"sheet": "profit.xlsx", "rows": [["Month", "Sales", "Costs", "Profit"], ["Jan", 10, 4, "=B2-C2"], ["Feb", 12, 5, "=B3-C3"]]}"#),
        call("chart_sheet", r#"{"sheet": "profit.xlsx", "type": "bar", "x": "Month", "series": ["Profit"], "title": "Profit by month"}"#),
        call("write_cells", r#"{"sheet": "profit.xlsx", "cells": {"B2": 20}}"#),
        Reply::Text("done"),
        call("write_cells", r#"{"sheet": "profit.xlsx", "cells": {"B3": 30}}"#),
        Reply::Text("again"),
    ];
    let h = harness(script, bundled_plugins(), sheets_natives());
    let mut c = connect(&h);
    wait_ext(&mut c, |e| !e.plugins.is_empty());
    let (entry, _) = turn(&mut c, "chart my profit", |_, _| {});
    let charted = step(&entry, "chart_sheet");
    assert_eq!(charted.status, StepStatus::Done, "{}", charted.detail);
    assert!(charted.detail.contains("linked"), "{}", charted.detail);
    let chart = charted.artifacts[0].id.clone();
    let profit = |ws: &Path| {
        let meta = byob_tools::artifact::load(ws, &chart).unwrap();
        let latest = meta.artifact.latest().unwrap().clone();
        let spec: serde_json::Value = serde_json::from_slice(&std::fs::read(ws.join(&latest.path)).unwrap()).unwrap();
        let rows: Vec<(String, f64)> =
            spec["data"].as_array().unwrap().iter().map(|r| (r["Month"].as_str().unwrap().to_owned(), r["Profit"].as_f64().unwrap())).collect();
        (meta.artifact.versions.len(), rows, spec["source"]["x"].clone())
    };
    let (versions, rows, x) = profit(h._ws.path());
    assert_eq!(rows, [("Jan".to_owned(), 16.0), ("Feb".to_owned(), 7.0)], "the later edit in the same request is in it");
    assert_eq!((versions, x), (1, serde_json::json!("A")), "one version for the request");
    assert!(step(&entry, "write_cells").detail.contains("Redrew the linked chart") || entry.steps.iter().any(|s| s.detail.contains("Redrew the linked chart")));
    // The next request edits the sheet: the chart follows, as a new version.
    let (entry, _) = turn(&mut c, "raise February", |_, _| {});
    assert!(step(&entry, "write_cells").detail.contains("Redrew the linked chart \"Profit by month\""), "{}", step(&entry, "write_cells").detail);
    let (versions, rows, _) = profit(h._ws.path());
    assert_eq!((versions, rows[1].clone()), (2, ("Feb".to_owned(), 25.0)));
}

/// The bundled memory, plan and ask-user plugins, on by default: remember writes the global
/// memory.md, the next turn's system prompt holds it, the pane shows it; update_plan's plan
/// is restated after later tool calls of the turn.
#[test]
fn memory_and_plan_are_bundled_and_on_by_default() {
    let script = vec![
        call("remember", r#"{"text": "Prefers metric units"}"#),
        call("update_plan", r#"{"steps": [{"text": "Look around", "status": "doing"}, {"text": "Report", "status": "pending"}]}"#),
        call("list_files", "{}"),
        Reply::Text("noted"),
        Reply::Text("second"),
    ];
    let (url, requests) = fake_llm(script, "final answer");
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let config_dir = home.path().join("config");
    let mut config = EngineConfig::new(InferenceConfig { base_url: url, ..Default::default() });
    config.workspace = ws.path().canonicalize().unwrap();
    config.register_workspace = true;
    config.tools = byob_tools::builtin();
    config.policy = Some(Policy::ALLOW_ALL);
    config.paths = Some(Paths::at(home.path()));
    config.auto_title = false;
    config.plugins = Some(PluginsConfig {
        scratch_root: Some(home.path().join("chats")),
        python: byob_plugins::default_python(),
        bundled: bundled_plugins(),
        bundled_cache: Some(home.path().join("bundled-plugins")),
        natives: Natives::builtin().backend(byob_memory::ID, byob_memory::factory(Some(config_dir.clone()))),
        ..Default::default()
    });
    let engine = EngineHandle::spawn_with(config).unwrap();
    let mut c = engine.connect(Arc::new(|| {}));
    c.send(ClientMsg::hello("test"));
    let e = wait_ext(&mut c, |e| !e.plugins.is_empty());
    for id in ["memory", "plan", "ask-user"] {
        let p = info(&e, id);
        assert!(p.enabled && p.in_use() && p.problems.is_empty(), "{p:?}");
    }
    let (entry, _) = turn(&mut c, "remember that I prefer metric, then plan", |_, _| {});
    let r = step(&entry, "remember");
    assert_eq!((r.status, r.detail.as_str(), r.title.as_str()), (StepStatus::Done, "Remembered (global).", "Remembered: Prefers metric units"));
    assert!(std::fs::read_to_string(config_dir.join("memory.md")).unwrap().contains("- Prefers metric units\n"));
    let plan = step(&entry, "update_plan");
    assert_eq!(plan.title, "Plan: 0/2 done");
    assert_eq!(plan.views.first().map(|v| v.kind.as_str()), Some("byob.tree"));
    let reqs = requests.lock().unwrap().clone();
    let names = tool_names(&reqs[0]);
    for t in ["remember", "forget", "update_plan", "ask_user"] {
        assert!(names.iter().any(|n| n == t), "{t} in {names:?}");
    }
    let first_system = reqs[0]["messages"][0]["content"].as_str().unwrap().to_owned();
    assert!(first_system.contains("Memory: notes kept across chats") && first_system.contains("Nothing is saved yet."), "{first_system}");
    // After update_plan: its result is the plan (not restated); after list_files it is.
    let last = |req: &Value| req["messages"].as_array().unwrap().last().unwrap()["content"].as_str().unwrap_or_default().to_owned();
    assert!(last(&reqs[2]).starts_with("Plan (0/2 done):") && !last(&reqs[2]).contains("[Current plan]"), "{}", last(&reqs[2]));
    assert!(last(&reqs[3]).ends_with("[Current plan]\nPlan (0/2 done):\n[>] Look around\n[ ] Report"), "{}", last(&reqs[3]));
    // The next turn: the note is in the system prompt, the open plan on the user message.
    turn(&mut c, "and now?", |_, _| {});
    let reqs = requests.lock().unwrap().clone();
    let req = reqs.last().unwrap();
    let system = req["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("Everywhere:\n- Prefers metric units"), "{system}");
    assert!(last(req).starts_with("and now?\n\n(Your plan in this chat so far"), "{}", last(req));
    // The pane shows it.
    let v = ViewRef::new("memory", "memory", "");
    c.send(ClientMsg::OpenView { view: v.clone() });
    loop {
        if let ServerMsg::LiveView(lv) = recv(&mut c)
            && lv.view == v
        {
            assert_eq!(lv.error, None);
            assert!(serde_json::to_string(&lv.tree).unwrap().contains("Prefers metric units"), "{lv:?}");
            break;
        }
    }
}

#[test]
fn without_its_backend_built_in_sheets_reports_a_problem() {
    // A byob built without the `native-sheets` feature: the plugin is listed, with a problem.
    let h = harness(vec![], bundled_plugins(), Natives::builtin());
    let mut c = connect(&h);
    let e = wait_ext(&mut c, |e| !e.plugins.is_empty());
    let p = info(&e, "sheets");
    assert!(p.problems.iter().any(|m| m.contains("\"sheets\" isn't a backend built into this byob")), "{:?}", p.problems);
}

#[test]
fn the_sheets_manifest_names_the_native_backend() {
    let text = byob_bundled::file("sheets", "byob-plugin.json").unwrap();
    let m = byob_plugins::manifest::parse_manifest_str(text, byob_plugins::manifest::ParseOptions { builtin: true, file: None }).unwrap();
    assert_eq!(m.id, "sheets");
    // `provides` lets a third-party spreadsheets plugin replace it.
    assert_eq!(m.provides.as_deref(), Some("spreadsheets"));
    assert!(m.on_by_default);
    assert_eq!(m.backend.unwrap().run, byob_plugins::manifest::BackendRun::Native(byob_sheets::ID.into()));
    assert!(m.requirements.is_empty());
    let names: Vec<&str> = m.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, byob_sheets::TOOLS);
    assert_eq!(m.views.len(), 1);
}

/// A one-page web server on loopback (no live network in tests).
fn page_server(body: &'static str) -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for mut s in listener.incoming().flatten() {
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let head = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            let _ = s.write_all(head.as_bytes()).and_then(|()| s.write_all(body.as_bytes()));
        }
    });
    base
}

#[test]
fn the_web_plugin_is_on_by_default_and_reads_pages_without_asking() {
    let page = "<html><head><title>Release notes</title></head><body><nav>Home | Blog</nav><main><h1>v2.0</h1>\
        <p>This release adds a brand new parser, many fixes, and a much faster startup for everyone who uses it daily.</p>\
        <p>Upgrading is easy: replace the binary, keep your settings, and read the migration notes linked below.</p></main></body></html>";
    let base = page_server(page);
    let script = vec![call("web_fetch", &json!({"url": format!("{base}/notes")}).to_string()), Reply::Text("read it")];
    let natives = Natives::builtin().backend(byob_web_tools::ID, byob_web_tools::factory());
    // The default policy: web_fetch only reads, so nobody is asked.
    let h = harness_with(script, bundled_plugins(), natives, Policy::default());
    let mut c = connect(&h);
    let e = wait_ext(&mut c, |e| e.plugins.iter().any(|p| p.id == "web"));
    let web = info(&e, "web");
    assert!(web.enabled && web.problems.is_empty(), "{:?}", web.problems);
    assert_eq!(web.tools, ["web_search", "web_fetch"]);
    let (entry, _) = turn(&mut c, "read the notes", |s, _| panic!("web_fetch was asked about: {}", s.title));
    let s = step(&entry, "web_fetch");
    assert_eq!(s.status, StepStatus::Done, "{}", s.detail);
    assert_eq!(s.title, format!("Read {base}/notes"));
    let reqs = h.requests.lock().unwrap();
    let result = reqs[1]["messages"].as_array().unwrap().iter().rev().find(|m| m["role"] == "tool").unwrap()["content"].as_str().unwrap().to_owned();
    assert!(result.contains("Title: Release notes\n") && result.contains("# v2.0\n\nThis release adds") && !result.contains("Home | Blog"), "{result}");
    let tools = tool_names(&reqs[0]);
    assert!(tools.iter().any(|t| t == "web_search") && tools.iter().any(|t| t == "web_fetch"), "{tools:?}");
}

#[test]
fn the_help_plugin_is_on_by_default_and_answers_from_byobs_docs() {
    let script = vec![call("byob_help", &json!({"query": "gmail app password"}).to_string()), Reply::Text("use an app password")];
    let natives = Natives::builtin().backend(byob_help::ID, byob_help::factory());
    let h = harness_with(script, bundled_plugins(), natives, Policy::default());
    let mut c = connect(&h);
    let e = wait_ext(&mut c, |e| e.plugins.iter().any(|p| p.id == "help"));
    let help = info(&e, "help");
    assert!(help.enabled && help.problems.is_empty(), "{:?}", help.problems);
    assert_eq!(help.tools, ["byob_help"]);
    let (entry, _) = turn(&mut c, "how do I connect my Gmail?", |s, _| panic!("byob_help was asked about: {}", s.title));
    assert_eq!(step(&entry, "byob_help").status, StepStatus::Done);
    let reqs = h.requests.lock().unwrap();
    assert!(crate::extensions_tests::system(&reqs[0]).contains("look it up with byob_help"));
    let result = reqs[1]["messages"].as_array().unwrap().iter().rev().find(|m| m["role"] == "tool").unwrap()["content"].as_str().unwrap().to_owned();
    assert!(result.contains("section \"Gmail\"") && result.contains("2-Step Verification"), "{result}");
}

/// The bundled calendar plugin on a fake CalDAV server under "Allow everything", making five
/// calls: (the must-confirm cards it showed: tool, text, warning), the server, what clients saw.
fn calendar_turn(natives: Natives) -> (Vec<(String, String, String)>, byob_calendar::fake::Server, Vec<ServerMsg>) {
    use byob_calendar::fake::WORK;
    let script = vec![
        call("calendar_create", r#"{"title": "Focus", "start": "2026-11-19T14:00"}"#),
        call("calendar_create", r#"{"title": "Sync", "start": "2026-11-19T15:00", "attendees": ["ann@example.com"]}"#),
        call("calendar_update", r#"{"id": "work/standup.ics", "title": "Daily"}"#),
        call("calendar_update", r#"{"id": "work/lunch.ics", "title": "Long lunch"}"#),
        call("calendar_delete", r#"{"id": "work/lunch.ics"}"#),
        Reply::Text("done"),
    ];
    let (server, mut c, _h) = calendar_engine(script, natives);
    let mut asked = Vec::new();
    let (entry, seen) = turn(&mut c, "book things", |s, c| {
        let ask = s.ask.as_ref().expect("a must-confirm card");
        assert!(ask.always, "{ask:?}");
        asked.push((s.tool.clone(), ask.text.clone(), ask.warning.clone().unwrap_or_default()));
        c.send(ClientMsg::Approve { step: s.id, answer: ApprovalAnswer::Allow });
    });
    let steps: Vec<(String, StepStatus)> = entry.steps.iter().map(|s| (s.tool.clone(), s.status)).collect();
    assert!(steps.len() == 5 && steps.iter().all(|(_, st)| *st == StepStatus::Done), "{steps:?}");
    assert!(server.object(&format!("{WORK}lunch.ics")).is_none());
    assert!(server.object(&format!("{WORK}standup.ics")).unwrap().contains("SUMMARY:Daily"));
    (asked, server, seen)
}

/// An engine under "Allow everything" with the calendar plugin on, pointed at a fake CalDAV
/// server holding a lunch (no attendees) and a standup (one attendee).
fn calendar_engine(script: Vec<Reply>, natives: Natives) -> (byob_calendar::fake::Server, LocalConnection, H) {
    use byob_calendar::fake::{PASS, Server, USER, WORK};
    let server = Server::start();
    server.put(&format!("{WORK}lunch.ics"), "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:lunch\r\nSUMMARY:Lunch\r\nDTSTART:20261119T110000Z\r\nDTEND:20261119T120000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n");
    server.put(
        &format!("{WORK}standup.ics"),
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:standup\r\nSUMMARY:Standup\r\nDTSTART:20261119T083000Z\r\nDTEND:20261119T084500Z\r\nATTENDEE:mailto:ann@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
    );
    let natives = natives.backend(byob_calendar::ID, byob_calendar::factory());
    // "Allow everything": nothing would ask but the must-confirm calls.
    let h = harness_with(script, bundled_plugins(), natives, Policy::ALLOW_ALL);
    let mut c = connect(&h);
    let e = wait_ext(&mut c, |e| e.plugins.iter().any(|p| p.id == "calendar"));
    let p = info(&e, "calendar");
    assert!(!p.enabled, "shipped off: it needs an account");
    c.send(ClientMsg::SetPluginEnabled { scope: ConfigScope::Global, plugin: "calendar".into(), enabled: Some(true) });
    for (k, v) in [("CALDAV_URL", server.url("/")), ("USERNAME", USER.into()), ("PASSWORD", PASS.into()), ("TIME_ZONE", "Europe/Berlin".into())] {
        c.send(ClientMsg::SetPluginSetting { plugin: "calendar".into(), key: k.into(), value: Some(v) });
    }
    let e = wait_ext(&mut c, |e| {
        e.plugins.iter().any(|p| p.id == "calendar" && p.enabled && p.problems.is_empty() && p.settings.iter().filter(|s| s.set).count() == 4)
    });
    assert_eq!(info(&e, "calendar").tools, byob_calendar::TOOLS);
    (server, c, h)
}

#[test]
fn must_confirm_cards_show_the_whole_text_and_refuse_what_cant_be_shown() {
    // A description far past the ordinary preview's cut, and one past what a card shows.
    let long: String = (0..200).map(|i| format!("Line {i} of the agenda, which the user must be able to read.\n")).collect();
    let huge = "x".repeat(byob_tools::preview::MAX_CONFIRM_BYTES + 10);
    let args = |d: &str| json!({"title": "Sync", "start": "2026-11-19T15:00", "attendees": ["ann@example.com"], "description": d}).to_string();
    let script = vec![call("calendar_create", &args(&long)), call("calendar_create", &args(&huge)), Reply::Text("done")];
    let (server, mut c, _h) = calendar_engine(script, Natives::builtin());
    let mut cards = Vec::new();
    let (entry, _) = turn(&mut c, "book it", |s, c| {
        cards.push(s.clone());
        c.send(ClientMsg::Approve { step: s.id, answer: ApprovalAnswer::Allow });
    });
    assert_eq!(cards.len(), 1, "the oversized call never reached a person");
    let Some(byob_protocol::Preview::Args { code, fields, more }) = &cards[0].preview else { panic!("{:?}", cards[0].preview) };
    assert_eq!((code.as_deref(), *more), (Some(long.as_str()), 0), "the whole description, not the first 40 lines");
    assert!(fields.iter().any(|(k, v)| k == "attendees" && v.contains("ann@example.com")), "{fields:?}");
    let refused = &entry.steps[1];
    assert_eq!(refused.status, StepStatus::Denied);
    assert!(refused.detail.contains("too long to show in full") && refused.detail.contains("limit is 100 KB"), "{}", refused.detail);
    let stored: Vec<String> = server.state.lock().unwrap().objects.values().map(|(_, t)| t.replace("\r\n ", "")).collect();
    assert!(stored.iter().any(|t| t.contains("Line 199 of the agenda")) && !stored.iter().any(|t| t.contains("xxxxxxxxxx")), "only the shown one was saved");
}

#[test]
fn calendar_calls_that_reach_people_ask_even_under_allow_everything() {
    let (asked, _server, seen) = calendar_turn(Natives::builtin());
    // Not the plain create or the change to an event without attendees.
    let tools: Vec<&str> = asked.iter().map(|(t, _, _)| t.as_str()).collect();
    assert_eq!(tools, ["calendar_create", "calendar_update", "calendar_delete"], "{asked:?}");
    assert!(asked[0].1.contains("1 attendee (ann@example.com)") && asked[0].2.contains("may email them"), "{asked:?}");
    assert!(asked[1].1.contains("\"Standup\"") && asked[1].1.contains("It has attendees"), "{asked:?}");
    assert!(asked[2].1.starts_with("Delete \"Long lunch\"") && asked[2].2.contains("Deletes the event"), "{asked:?}");
    for m in &seen {
        assert!(!m.to_json().contains(byob_calendar::fake::PASS), "a secret went to a client: {}", m.to_json());
    }
}

/// Allows every call, from the first place in `before_tool_call`.
struct AllowAll;
impl crate::hooks::NativeHook for AllowAll {
    fn before_tool_call(&self, _: &mut crate::hooks::ToolCallCtx<'_>) -> crate::hooks::Decision {
        crate::hooks::Decision::Allow
    }
}

#[test]
fn another_plugins_allow_doesnt_skip_the_calendars_confirm_rules() {
    // A hook that allows everything runs before the calendar's own: the manifest's `confirm`
    // rules (attendees on create, every delete) still ask; the calendar's hook (changes to
    // events that already have attendees) doesn't get its turn.
    let natives = Natives::builtin().hook("allow_all", Arc::new(AllowAll), &[(crate::hooks::Event::BeforeToolCall, 0)]);
    let (asked, _, _) = calendar_turn(natives);
    let tools: Vec<&str> = asked.iter().map(|(t, _, _)| t.as_str()).collect();
    assert_eq!(tools, ["calendar_create", "calendar_delete"], "{asked:?}");
    assert_eq!(asked[0].2, "Has attendees: your calendar server may email them an invitation.");
}

/// The bundled mail plugin's `mail_send` is `"confirm"`: every call waits for a person,
/// even under "Allow everything" and after an "always allow everywhere" answer; a no
/// sends nothing. Its card shows the recipients, subject and body; the password (a secret
/// setting) never reaches a client.
#[test]
fn mail_send_always_waits_for_a_person_even_when_everything_is_allowed() {
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};
    // An SMTP server that counts connections and turns every one away.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let connections = Arc::new(AtomicUsize::new(0));
    let seen_conn = Arc::clone(&connections);
    std::thread::spawn(move || {
        for mut s in listener.incoming().flatten() {
            seen_conn.fetch_add(1, Ordering::SeqCst);
            let _ = s.write_all(b"554 not today\r\n");
        }
    });
    let send = |subject: &str| json!({"to": "Ada <ada@example.org>", "subject": subject, "body": "Hi Ada,\nsee you Friday."}).to_string();
    let script = vec![call("mail_send", &send("Engine notes")), call("mail_send", &send("Engine notes, again")), Reply::Text("done")];
    let natives = Natives::builtin().backend(byob_mail::ID, byob_mail::factory());
    let h = harness_with(script, bundled_plugins(), natives, Policy::ALLOW_ALL);
    let mut c = connect(&h);
    let e = wait_ext(&mut c, |e| e.plugins.iter().any(|p| p.id == "mail"));
    assert!(!info(&e, "mail").enabled, "mail ships switched off");
    c.send(ClientMsg::SetPluginEnabled { scope: ConfigScope::Global, plugin: "mail".into(), enabled: Some(true) });
    let password = "hunter2-mail-pass";
    let smtp_port = port.to_string();
    for (k, v) in [
        ("USERNAME", "me@example.com"),
        ("PASSWORD", password),
        ("SMTP_HOST", "127.0.0.1"),
        ("SMTP_PORT", smtp_port.as_str()),
        ("SMTP_SECURITY", "none"),
        ("SAVE_SENT", "never"),
    ] {
        c.send(ClientMsg::SetPluginSetting { plugin: "mail".into(), key: k.into(), value: Some(v.into()) });
    }
    let e = wait_ext(&mut c, |e| {
        e.plugins
            .iter()
            .any(|p| p.id == "mail" && p.enabled && p.tools.iter().any(|t| t == "mail_send") && p.settings.iter().any(|s| s.key == "PASSWORD" && s.set))
    });
    assert_eq!(info(&e, "mail").tools, byob_mail::TOOLS);
    let mut cards = Vec::new();
    let (entry, seen) = turn(&mut c, "email Ada", |s, c| {
        cards.push(s.clone());
        // "Always, everywhere" the first time: it must not stick.
        let answer = if cards.len() == 1 { ApprovalAnswer::AlwaysEverywhere } else { ApprovalAnswer::Deny };
        c.send(ClientMsg::Approve { step: s.id, answer });
    });
    assert_eq!(cards.len(), 2, "both sends asked: {cards:?}");
    for (card, subject) in cards.iter().zip(["Engine notes", "Engine notes, again"]) {
        let ask = card.ask.as_ref().expect("a must-confirm card");
        assert!(ask.always && ask.warning.as_deref() == Some("Sends this email from your account. It can't be unsent."), "{ask:?}");
        let Some(byob_protocol::Preview::Args { fields, code, .. }) = &card.preview else { panic!("no preview: {card:?}") };
        assert!(
            fields.contains(&("to".to_owned(), "Ada <ada@example.org>".to_owned())) && fields.contains(&("subject".to_owned(), subject.to_owned())),
            "{fields:?}"
        );
        assert_eq!(code.as_deref(), Some("Hi Ada,\nsee you Friday."));
    }
    let steps: Vec<_> = entry.steps.iter().filter(|s| s.tool == "mail_send").collect();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0].status, StepStatus::Failed, "allowed: it ran (and the server refused): {}", steps[0].detail);
    assert!(steps[0].detail.contains("Not sent"), "{}", steps[0].detail);
    assert_eq!(steps[1].status, StepStatus::Denied);
    assert_eq!(connections.load(Ordering::SeqCst), 1, "the denied send never connected");
    for m in &seen {
        assert!(!m.to_json().contains(password), "the password went to a client: {}", m.to_json());
    }
}
