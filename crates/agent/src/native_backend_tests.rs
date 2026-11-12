//! Native plugin backends (`"backend": {"native": id}`, `byob_plugins::native`): hosted like
//! process backends (init, requests, views, a panic is a crash and a restart, switch-off
//! drops them), and the bundled `sheets` plugin, whose backend is native (`byob-sheets`).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use byob_plugins::Embedded;
use byob_plugins::native::{NativeBackend, NativeFactory, Outbox};
use byob_protocol::sites::artifact_key;
use byob_protocol::{ClientMsg, ConfigScope, Connection, PluginInfo, ServerMsg, StepStatus, ViewRef};
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
    let e = ext_after(&mut c, seen, |e| info(e, "echo").backend.as_ref().is_some_and(|b| b.state == "running"));
    assert!(info(&e, "echo").backend.as_ref().unwrap().stderr.contains("(log) echo is up"));

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
    assert_eq!(meta.artifact.versions.len(), 2);

    // Its grid draws the artifact.
    let v = ViewRef::new("sheets", "sheet", artifact_key(&id, 2));
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
