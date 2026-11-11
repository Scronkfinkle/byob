//! Engine tests of plugin backends: the plugin channel through a real Python backend, a
//! backend-served tool in a turn, engine events, reloads and status for the model.
//! Skipped without `python3`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use byob_protocol::{ClientMsg, Connection, Entry, Extensions, PluginInfo, ServerMsg, StepStatus};
use byob_store::Paths;
use byob_types::InferenceConfig;
use serde_json::{Value, json};

use crate::extensions::PluginsConfig;
use crate::loop_tests::{Reply, Requests, call, fake_llm};
use crate::{EngineConfig, EngineHandle, LocalConnection, Policy};

pub(crate) fn have_python() -> bool {
    std::process::Command::new(&byob_plugins::default_python()[0]).args(["-c", "pass"]).status().is_ok_and(|s| s.success())
}

pub(crate) struct H {
    pub engine: EngineHandle,
    pub requests: Requests,
    pub home: tempfile::TempDir,
    pub _ws: tempfile::TempDir,
}

pub(crate) fn harness(script: Vec<Reply>, setup: impl FnOnce(&Path)) -> H {
    let (url, requests) = fake_llm(script, "final answer");
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    setup(&home.path().join("plugins"));
    let mut config = EngineConfig::new(InferenceConfig { base_url: url, ..Default::default() });
    config.workspace = ws.path().canonicalize().unwrap();
    config.register_workspace = true;
    config.policy = Some(Policy::ALLOW_ALL);
    config.paths = Some(Paths::at(home.path()));
    config.auto_title = false;
    config.plugins = Some(PluginsConfig {
        global_roots: vec![home.path().join("plugins")],
        scratch_root: Some(home.path().join("chats")),
        python_root: Some(home.path().join("python")),
        python: byob_plugins::default_python(),
        ..Default::default()
    });
    H { engine: EngineHandle::spawn_with(config).unwrap(), requests, home, _ws: ws }
}

pub(crate) fn recv(c: &mut LocalConnection) -> ServerMsg {
    let start = Instant::now();
    loop {
        if let Some(m) = c.try_recv() {
            return m;
        }
        assert!(start.elapsed() < Duration::from_secs(30), "timed out waiting for the engine");
        // spin briefly so round-trip timings measure the engine, not this loop
        if start.elapsed() < Duration::from_millis(5) {
            std::thread::yield_now();
        } else {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

pub(crate) fn hello(h: &H) -> (LocalConnection, Extensions) {
    let mut c = h.engine.connect(Arc::new(|| {}));
    c.send(ClientMsg::hello("test"));
    let mut ext = None;
    loop {
        match recv(&mut c) {
            ServerMsg::Extensions(e) => ext = Some(*e),
            ServerMsg::Artifacts { .. } => return (c, ext.expect("Extensions comes before Artifacts")),
            _ => {}
        }
    }
}

fn plugin_msg(c: &mut LocalConnection) -> (String, Value) {
    loop {
        match recv(c) {
            ServerMsg::Plugin { plugin, msg } => return (plugin, msg),
            ServerMsg::Notice { text, .. } => panic!("notice: {text}"),
            _ => {}
        }
    }
}

pub(crate) fn turn(c: &mut LocalConnection, text: &str) -> Entry {
    c.send(ClientMsg::SendMessage { text: text.into(), attachments: vec![] });
    let mut entries: Vec<Entry> = Vec::new();
    loop {
        match recv(c) {
            ServerMsg::EntryAdded(e) => entries.push(e),
            ServerMsg::StepUpdated { entry, step } => {
                if let Some(e) = entries.iter_mut().find(|e| e.id == entry) {
                    e.upsert_step(step);
                }
            }
            ServerMsg::EntryFinished { id, error, .. } => {
                assert_eq!(error, None);
                return entries.into_iter().find(|e| e.id == id).unwrap();
            }
            _ => {}
        }
    }
}

pub(crate) fn find<'a>(e: &'a Extensions, id: &str) -> &'a PluginInfo {
    e.plugins.iter().find(|p| p.id == id).unwrap_or_else(|| panic!("no plugin {id}: {:?}", e.plugins))
}

const BACKEND: &str = r#"
import os
import byob_plugin as byob

clicks = {"n": 0}

@byob.on_message
def message(msg, client):
    if msg.get("crash"):
        print("crashing on purpose")
        os._exit(7)
    clicks["n"] += 1
    byob.send({"clicks": clicks["n"], "pid": os.getpid(), "token": os.environ.get("TOKEN")}, client=client)

@byob.tool
def count_clicks(add: int = 0):
    clicks["n"] += add
    byob.send({"count": f"{clicks['n']} clicks"})
    return f"{clicks['n']} clicks so far"

@byob.on("turn_finished")
def finished(event):
    byob.send({"turn_finished": event["entry"], "chat": event["chat"]})

byob.run()
"#;

fn write_backend_plugin(root: &Path) -> PathBuf {
    let dir = root.join("clicker");
    fs::create_dir_all(&dir).unwrap();
    let m = json!({
        "id": "clicker", "name": "Clicker", "backend": "backend.py",
        "settings": [{"key": "TOKEN", "label": "Token", "secret": true, "default": "s3cret"}],
        "tools": [{"name": "count_clicks", "description": "How many clicks", "run": "backend",
                   "params": {"type": "object", "properties": {"add": {"type": "integer"}}}}]
    });
    fs::write(dir.join("byob-plugin.json"), m.to_string()).unwrap();
    fs::write(dir.join("backend.py"), BACKEND).unwrap();
    dir
}

#[test]
fn the_plugin_channel_reaches_a_python_backend_and_back() {
    if !have_python() {
        return;
    }
    let h = harness(vec![Reply::Text("hi")], |root| {
        write_backend_plugin(root);
    });
    let (mut c, ext) = hello(&h);
    let p = find(&ext, "clicker");
    assert_eq!(p.backend.as_ref().map(|b| b.state.as_str()), Some("idle"), "it starts on first use");
    assert_eq!(p.tools, ["count_clicks"]);
    c.send(ClientMsg::Plugin { plugin: "clicker".into(), msg: json!({"click": 1}) });
    let (plugin, msg) = plugin_msg(&mut c);
    assert_eq!(plugin, "clicker");
    assert_eq!(msg["clicks"], 1);
    assert_eq!(msg["token"], "s3cret", "settings (secret ones too) are its environment");
    let pid = msg["pid"].clone();
    // the same process keeps its state; measure the round trip while at it
    let n = 50;
    let t = Instant::now();
    for i in 0..n {
        c.send(ClientMsg::Plugin { plugin: "clicker".into(), msg: json!({"click": 1}) });
        let (_, msg) = plugin_msg(&mut c);
        assert_eq!(msg["clicks"], i + 2);
        assert_eq!(msg["pid"], pid);
    }
    eprintln!("plugin channel round trip through the engine: {:?}", t.elapsed() / n);
    // a client without the cap gets nothing
    let mut old = h.engine.connect(Arc::new(|| {}));
    old.send(ClientMsg::Hello { protocol: byob_protocol::PROTOCOL_MAJOR, minor: 0, client: "old".into(), caps: vec![] });
    c.send(ClientMsg::Plugin { plugin: "clicker".into(), msg: json!({"click": 1}) });
    plugin_msg(&mut c);
    std::thread::sleep(Duration::from_millis(100));
    while let Some(m) = old.try_recv() {
        assert!(!matches!(m, ServerMsg::Plugin { .. }), "{m:?}");
    }
}

#[test]
fn a_crashed_backend_restarts_and_the_model_sees_its_stderr() {
    if !have_python() {
        return;
    }
    let status = json!({"plugin": "clicker"}).to_string();
    let h = harness(vec![call("plugin_status", &status), Reply::Text("ok")], |root| {
        write_backend_plugin(root);
    });
    let (mut c, _) = hello(&h);
    c.send(ClientMsg::Plugin { plugin: "clicker".into(), msg: json!({"click": 1}) });
    let (_, first) = plugin_msg(&mut c);
    c.send(ClientMsg::Plugin { plugin: "clicker".into(), msg: json!({"crash": true}) });
    // clients hear that it crashed
    loop {
        if let ServerMsg::Extensions(e) = recv(&mut c)
            && let Some(b) = &find(&e, "clicker").backend
            && b.state == "crashed"
        {
            assert!(b.error.as_deref().unwrap_or_default().contains("exit code 7"), "{b:?}");
            assert!(b.stderr.contains("crashing on purpose"));
            break;
        }
    }
    let reply = turn(&mut c, "why did it crash?");
    let s = reply.steps.iter().find(|s| s.tool == "plugin_status").unwrap();
    assert!(s.detail.contains("backend:") && s.detail.contains("crashing on purpose"), "{}", s.detail);
    // it came back (a new process, state reset)
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        c.send(ClientMsg::Plugin { plugin: "clicker".into(), msg: json!({"click": 1}) });
        let (_, msg) = plugin_msg(&mut c);
        if msg.get("pid").is_none() {
            continue; // turn_finished, from the turn above
        }
        if msg["pid"] != first["pid"] {
            assert_eq!(msg["clicks"], 1);
            break;
        }
        assert!(Instant::now() < deadline);
    }
}

#[test]
fn a_backend_serves_tools_and_gets_the_events_it_subscribed_to() {
    if !have_python() {
        return;
    }
    let h = harness(vec![call("count_clicks", r#"{"add": 5}"#), Reply::Text("5 clicks")], |root| {
        write_backend_plugin(root);
    });
    let (mut c, _) = hello(&h);
    c.send(ClientMsg::SendMessage { text: "count".into(), attachments: vec![] });
    let mut view = None;
    let mut finished = None;
    let mut step_detail = None;
    while finished.is_none() || view.is_none() {
        match recv(&mut c) {
            ServerMsg::StepUpdated { step, .. } if step.tool == "count_clicks" && step.status.finished() => {
                assert_eq!(step.status, StepStatus::Done, "{}", step.detail);
                step_detail = Some(step.detail.clone());
            }
            ServerMsg::Plugin { msg, .. } if msg.get("count").is_some() => view = Some(msg),
            ServerMsg::Plugin { msg, .. } if msg.get("turn_finished").is_some() => finished = Some(msg),
            ServerMsg::EntryFinished { error, .. } => assert_eq!(error, None),
            _ => {}
        }
    }
    assert_eq!(step_detail.as_deref(), Some("5 clicks so far"));
    assert_eq!(view.unwrap()["count"], "5 clicks");
    assert!(finished.unwrap()["turn_finished"].is_u64());
    let reqs = h.requests.lock().unwrap().clone();
    let tools: Vec<&str> = reqs[0]["tools"].as_array().unwrap().iter().filter_map(|t| t["function"]["name"].as_str()).collect();
    assert!(tools.contains(&"count_clicks"));
}

#[test]
fn editing_a_plugin_stops_its_backend_and_the_next_use_runs_the_new_code() {
    if !have_python() {
        return;
    }
    let mut dir = PathBuf::new();
    let h = harness(vec![Reply::Text("hi")], |root| dir = write_backend_plugin(root));
    let (mut c, _) = hello(&h);
    c.send(ClientMsg::Plugin { plugin: "clicker".into(), msg: json!({"click": 1}) });
    let (_, first) = plugin_msg(&mut c);
    let pid = first["pid"].as_u64().unwrap();
    fs::write(dir.join("backend.py"), BACKEND.replace("\"clicks\": clicks[\"n\"]", "\"clicks\": clicks[\"n\"] * 100")).unwrap();
    c.send(ClientMsg::ReloadPlugins);
    c.send(ClientMsg::Plugin { plugin: "clicker".into(), msg: json!({"click": 1}) });
    let (_, msg) = plugin_msg(&mut c);
    assert_eq!(msg["clicks"], 100, "new code, fresh state");
    assert_ne!(msg["pid"].as_u64(), Some(pid));
    #[cfg(target_os = "linux")]
    {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Path::new(&format!("/proc/{pid}")).exists() {
            assert!(Instant::now() < deadline, "the old backend still runs");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    // switched off: stopped, and messages say why
    c.send(ClientMsg::SetPluginEnabled { scope: byob_protocol::ConfigScope::Global, plugin: "clicker".into(), enabled: Some(false) });
    c.send(ClientMsg::Plugin { plugin: "clicker".into(), msg: json!({"click": 1}) });
    loop {
        if let ServerMsg::Notice { text, .. } = recv(&mut c) {
            assert!(text.contains("switched off"), "{text}");
            break;
        }
    }
    let _ = h.home.path();
}

#[test]
fn subprocess_tools_can_import_the_helper_too() {
    if !have_python() {
        return;
    }
    let h = harness(vec![call("which_helper", "{}"), Reply::Text("ok")], |root| {
        let dir = root.join("helped");
        fs::create_dir_all(dir.join("tools")).unwrap();
        let m = json!({"id": "helped", "name": "Helped", "tools": [{"name": "which_helper", "description": "d", "run": "tools/t.py"}]});
        fs::write(dir.join("byob-plugin.json"), m.to_string()).unwrap();
        fs::write(dir.join("tools/t.py"), "import byob_plugin\nprint('helper', byob_plugin.VERSION)\n").unwrap();
    });
    let (mut c, _) = hello(&h);
    let reply = turn(&mut c, "go");
    let s = reply.steps.iter().find(|s| s.tool == "which_helper").unwrap();
    assert_eq!((s.status, s.detail.as_str()), (StepStatus::Done, "helper 1"));
}

/// The first fenced block of `lang` after `after` in the authoring guide.
fn guide_block(after: &str, lang: &str) -> String {
    let guide = crate::extensions::AUTHORING_SKILL;
    let rest = &guide[guide.find(after).unwrap()..];
    let open = format!("```{lang}\n");
    let start = rest.find(&open).unwrap() + open.len();
    let len = rest[start..].find("```").unwrap();
    rest[start..start + len].to_owned()
}

#[test]
fn the_authoring_guides_backend_example_works() {
    if !have_python() {
        return;
    }
    let h = harness(vec![call("bump", r#"{"by": 3}"#), call("bump", "{}"), Reply::Text("4")], |root| {
        let dir = root.join("counter");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("byob-plugin.json"), guide_block("## 4. A backend", "json")).unwrap();
        fs::write(dir.join("backend.py"), guide_block("## 4. A backend", "python")).unwrap();
    });
    let (mut c, ext) = hello(&h);
    assert!(find(&ext, "counter").problems.is_empty(), "{:?}", find(&ext, "counter").problems);
    let reply = turn(&mut c, "bump it");
    let details: Vec<&str> = reply.steps.iter().filter(|s| s.tool == "bump").map(|s| s.detail.as_str()).collect();
    assert_eq!(details, ["total is 3", "total is 4"]);
    c.send(ClientMsg::Plugin { plugin: "counter".into(), msg: json!({}) });
    loop {
        let (_, msg) = plugin_msg(&mut c);
        if msg.get("total").is_some() {
            assert_eq!(msg["total"], 4);
            break;
        }
    }
}

#[test]
fn a_plugin_without_a_backend_says_so() {
    if !have_python() {
        return;
    }
    let h = harness(vec![Reply::Text("hi")], |root| {
        let dir = root.join("plain");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("byob-plugin.json"), json!({"id": "plain", "name": "Plain"}).to_string()).unwrap();
    });
    let (mut c, _) = hello(&h);
    c.send(ClientMsg::Plugin { plugin: "plain".into(), msg: json!({}) });
    loop {
        if let ServerMsg::Notice { text, .. } = recv(&mut c) {
            assert_eq!(text, "plugin plain can't take messages: it has no backend");
            break;
        }
    }
}
