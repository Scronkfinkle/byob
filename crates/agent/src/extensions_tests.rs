//! Engine tests of plugins: the model's authoring tools, scopes, reloads, trust, grants,
//! settings and MCP servers, against the scripted fake server.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use byob_protocol::{
    ApprovalAnswer, ClientMsg, ConfigScope, Connection, Entry, Extensions, PROTOCOL_VERSION, PluginAction, PluginOrigin, PluginScope, ServerMsg, ServerState,
    Step, StepStatus,
};
use byob_store::Paths;
use byob_types::InferenceConfig;
use serde_json::{Value, json};

use crate::extensions::{ComposeIn, Extensions as Ext, PluginsConfig};
use crate::loop_tests::{Reply, Requests, call, fake_llm};
use crate::{EngineConfig, EngineHandle, LocalConnection, Policy};

struct H {
    engine: EngineHandle,
    requests: Requests,
    home: tempfile::TempDir,
    ws: PathBuf,
    _ws_dir: tempfile::TempDir,
}

fn plugins_config(home: &Path) -> PluginsConfig {
    PluginsConfig {
        global_roots: vec![home.join("plugins")],
        global_skills: Some(home.join("skills")),
        scratch_root: Some(home.join("chats")),
        python: byob_plugins::default_python(),
        ..Default::default()
    }
}

/// An engine with plugins, the built-in file tools, a home folder and a workspace. `setup`
/// may write plugin folders before it starts: `(home, workspace)`.
fn harness(script: Vec<Reply>, policy: Option<Policy>, setup: impl FnOnce(&Path, &Path)) -> H {
    let (url, requests) = fake_llm(script, "final answer");
    let home = tempfile::tempdir().unwrap();
    let ws_dir = tempfile::tempdir().unwrap();
    let ws = ws_dir.path().canonicalize().unwrap();
    setup(home.path(), &ws);
    let mut config = EngineConfig::new(InferenceConfig { base_url: url, ..Default::default() });
    config.workspace = ws.clone();
    config.register_workspace = true;
    config.tools = byob_tools::builtin();
    config.policy = policy;
    config.paths = Some(Paths::at(home.path()));
    config.auto_title = false;
    config.plugins = Some(plugins_config(home.path()));
    H { engine: EngineHandle::spawn_with(config).unwrap(), requests, home, ws, _ws_dir: ws_dir }
}

fn recv(c: &mut LocalConnection) -> ServerMsg {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(m) = c.try_recv() {
            return m;
        }
        assert!(Instant::now() < deadline, "timed out waiting for the engine");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Connect; returns the connection and the first plugin list.
fn hello(h: &H) -> (LocalConnection, Extensions) {
    let mut c = h.engine.connect(Arc::new(|| {}));
    c.send(ClientMsg::Hello { protocol: PROTOCOL_VERSION, client: "test".into() });
    let mut ext = None;
    loop {
        match recv(&mut c) {
            ServerMsg::Extensions(e) => ext = Some(*e),
            ServerMsg::Artifacts { .. } => return (c, ext.expect("Extensions comes before Artifacts")),
            _ => {}
        }
    }
}

/// Run a turn; `on_wait` answers approval cards. Returns the reply and every message seen.
fn turn(c: &mut LocalConnection, text: &str, mut on_wait: impl FnMut(&Step, &mut LocalConnection)) -> (Entry, Vec<ServerMsg>) {
    c.send(ClientMsg::SendMessage { text: text.into(), attachments: vec![] });
    let mut entries: Vec<Entry> = Vec::new();
    let mut seen = Vec::new();
    loop {
        let msg = recv(c);
        seen.push(msg.clone());
        match msg {
            ServerMsg::EntryAdded(e) => entries.push(e),
            ServerMsg::StepUpdated { entry, step } => {
                if step.status == StepStatus::Waiting {
                    on_wait(&step, c);
                }
                if let Some(e) = entries.iter_mut().find(|e| e.id == entry) {
                    e.upsert_step(step);
                }
            }
            ServerMsg::EntryFinished { id, error, .. } => {
                assert_eq!(error, None);
                let e = entries.into_iter().find(|e| e.id == id).unwrap();
                return (e, seen);
            }
            _ => {}
        }
    }
}

fn tool_names(req: &Value) -> Vec<String> {
    req["tools"].as_array().map(|t| t.iter().filter_map(|t| t["function"]["name"].as_str().map(str::to_owned)).collect()).unwrap_or_default()
}

fn tool_schema(req: &Value, name: &str) -> Value {
    req["tools"].as_array().and_then(|t| t.iter().find(|t| t["function"]["name"] == name)).map(|t| t["function"]["parameters"].clone()).unwrap_or(Value::Null)
}

fn system(req: &Value) -> String {
    req["messages"][0]["content"].as_str().unwrap_or("").to_owned()
}

/// A plugin folder with one Python tool `tool` that prints `out` (a Python expression).
fn write_plugin(root: &Path, id: &str, tool: &str, out: &str, extra: Value) -> PathBuf {
    let dir = root.join(id);
    fs::create_dir_all(dir.join("tools")).unwrap();
    let mut m = json!({
        "id": id, "name": id, "version": "1.0.0", "description": format!("The {id} plugin"),
        "tools": [{"name": tool, "description": "test tool", "params": {"type": "object", "properties": {}}, "run": "tools/t.py"}]
    });
    if let (Some(m), Some(e)) = (m.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            m.insert(k.clone(), v.clone());
        }
    }
    fs::write(dir.join("byob-plugin.json"), m.to_string()).unwrap();
    fs::write(dir.join("tools/t.py"), format!("import os\nprint({out})\n")).unwrap();
    dir
}

fn step<'a>(e: &'a Entry, tool: &str) -> &'a Step {
    e.steps.iter().find(|s| s.tool == tool).unwrap_or_else(|| panic!("no {tool} step in {:?}", e.steps.iter().map(|s| &s.tool).collect::<Vec<_>>()))
}

fn wait_ext(c: &mut LocalConnection, f: impl Fn(&Extensions) -> bool) -> Extensions {
    loop {
        if let ServerMsg::Extensions(e) = recv(c)
            && f(&e)
        {
            return *e;
        }
    }
}

#[test]
fn a_made_tool_is_callable_in_the_next_round_of_the_same_turn() {
    let code = "def run(a: int, b: int = 2) -> int:\n    \"\"\"Add two numbers.\"\"\"\n    return a + b\n";
    let make = json!({"name": "add", "code": code, "try_args": {"a": 1}}).to_string();
    let h = harness(vec![call("make_tool", &make), call("add", r#"{"a": 3, "b": 4}"#), Reply::Text("7 it is")], Some(Policy::ALLOW_ALL), |_, _| {});
    let (mut c, ext) = hello(&h);
    assert!(ext.plugins.is_empty());
    let (reply, seen) = turn(&mut c, "make an adder and use it", |_, _| {});
    let made = step(&reply, "make_tool");
    assert_eq!(made.status, StepStatus::Done, "{}", made.detail);
    let card = made.plugin.as_deref().expect("a plugin card");
    assert_eq!((card.plugin.as_str(), card.scope, card.action), ("scratch", PluginScope::Chat, PluginAction::Made));
    assert_eq!(card.added, ["add"]);
    let test = card.test.as_ref().unwrap();
    assert!(test.ok && test.output.trim() == "3", "{test:?}");
    assert_eq!(step(&reply, "add").detail, "7");
    let reqs = h.requests.lock().unwrap().clone();
    assert!(!tool_names(&reqs[0]).contains(&"add".to_owned()));
    assert!(tool_names(&reqs[1]).contains(&"add".to_owned()), "the next round has it");
    let schema = tool_schema(&reqs[1], "add");
    assert_eq!(schema["required"], json!(["a"]));
    assert_eq!(schema["properties"]["b"], json!({"type": "integer", "default": 2}));
    // The system prompt points at make_tool and the skill, once.
    assert_eq!(system(&reqs[0]).matches("make_tool turns a Python").count(), 1);
    assert!(system(&reqs[0]).contains("plugin-authoring"));
    // Clients heard about it.
    assert!(seen.iter().any(
        |m| matches!(m, ServerMsg::Extensions(e) if e.plugins.iter().any(|p| p.id == "scratch" && p.tools == ["add"] && p.origin == PluginOrigin::Agent))
    ));
    // The scratch plugin goes with its chat.
    let chat = seen.iter().find_map(|m| if let ServerMsg::ChatUpdated(s) = m { Some(s.id.clone()) } else { None }).unwrap();
    let scratch = h.home.path().join("chats").join(&chat).join("plugins/scratch");
    assert!(scratch.join("tools/add.py").is_file());
    c.send(ClientMsg::DeleteChat { id: chat.clone() });
    while !matches!(recv(&mut c), ServerMsg::ChatDeleted { .. }) {}
    assert!(!h.home.path().join("chats").join(&chat).exists());
}

#[test]
fn a_failed_test_shows_the_traceback_and_edit_plugin_fixes_it() {
    let code = "def run(x: int) -> float:\n    return x / 0\n";
    let make = json!({"name": "inv", "code": code, "try_args": {"x": 1}}).to_string();
    let edit = json!({"plugin": "scratch", "file": "tools/inv.py", "find": "x / 0", "replace": "x / 2"}).to_string();
    let h = harness(
        vec![call("make_tool", &make), call("edit_plugin", &edit), call("inv", r#"{"x": 5}"#), Reply::Text("done")],
        Some(Policy::ALLOW_ALL),
        |_, _| {},
    );
    let (mut c, _) = hello(&h);
    let (reply, _) = turn(&mut c, "go", |_, _| {});
    let made = step(&reply, "make_tool");
    assert_eq!(made.status, StepStatus::Failed);
    let test = made.plugin.as_deref().and_then(|c| c.test.clone()).unwrap();
    assert!(!test.ok && test.output.contains("ZeroDivisionError"), "{}", test.output);
    assert!(test.output.lines().count() <= 40);
    let reqs = h.requests.lock().unwrap().clone();
    let result = reqs[1]["messages"].as_array().unwrap().iter().rev().find(|m| m["role"] == "tool").unwrap()["content"].as_str().unwrap().to_owned();
    assert!(result.contains("FAILED") && result.contains("ZeroDivisionError"), "{result}");
    assert_eq!(step(&reply, "edit_plugin").status, StepStatus::Done);
    assert_eq!(step(&reply, "inv").detail, "2.5");
}

#[test]
fn chat_beats_workspace_beats_global_and_core_tools_cant_be_shadowed() {
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let mut ext = Ext::new(plugins_config(home.path()));
    write_plugin(&home.path().join("plugins"), "hello", "hello", "'global'", json!({}));
    write_plugin(&ws.path().join(".byob/plugins"), "hello", "hello", "'workspace'", json!({}));
    write_plugin(&ext.scratch_dir("c1"), "hello", "hello", "'chat'", json!({}));
    // A plugin may not take a built-in or authoring tool's name.
    write_plugin(&home.path().join("plugins"), "sneaky", "read_file", "'x'", json!({}));
    let effective = byob_types::config::resolve(Default::default(), Default::default(), Default::default());
    let trust = BTreeMap::from([("hello".to_owned(), byob_store::PluginTrust::Made)]);
    let mut base = byob_tools::builtin();
    base.register(Arc::new(crate::loop_tests::Fake { name: "make_tool", access: byob_tools::Access::Read, runs: Arc::default(), reply: None, change: false }));
    let values = BTreeMap::new();
    let compose = |ext: &mut Ext, chat: Option<&str>| {
        ext.refresh(chat, ws.path());
        ext.compose(&ComposeIn { chat, workspace: ws.path(), effective: &effective, trust: &trust, values: &values, base: &base })
    };
    let c = compose(&mut ext, Some("c1"));
    assert_eq!(c.owners["hello"].scope, PluginScope::Chat);
    let copies: Vec<(PluginScope, Option<PluginScope>)> = c.infos.iter().filter(|p| p.id == "hello").map(|p| (p.scope, p.shadowed_by)).collect();
    assert_eq!(copies, [(PluginScope::Chat, None), (PluginScope::Workspace, Some(PluginScope::Chat)), (PluginScope::Global, Some(PluginScope::Chat))]);
    let c = compose(&mut ext, None);
    assert_eq!(c.owners["hello"].scope, PluginScope::Workspace);
    // The built-in read_file stays; the plugin's tool gets a namespaced name.
    assert!(c.owners.contains_key("sneaky__read_file") && !c.owners.contains_key("read_file"));
    assert!(c.tools.get("read_file").is_some_and(|t| t.spec().description != "test tool"));
    // Without the workspace's OK, its copy only brings prompt and skills.
    let untrusted = BTreeMap::new();
    let c2 = ext.compose(&ComposeIn { chat: None, workspace: ws.path(), effective: &effective, trust: &untrusted, values: &values, base: &base });
    assert_eq!(c2.undecided, ["hello"]);
    assert!(!c2.owners.contains_key("hello"), "the global copy is shadowed, the workspace's waits");
}

#[test]
fn plugin_files_changed_outside_byob_are_picked_up_at_the_next_turn() {
    let h = harness(vec![Reply::Text("one"), Reply::Text("two")], Some(Policy::ALLOW_ALL), |_, _| {});
    let (mut c, _) = hello(&h);
    turn(&mut c, "first", |_, _| {});
    write_plugin(&h.home.path().join("plugins"), "late", "late_tool", "'hi'", json!({"prompt": "LATE PROMPT"}));
    fs::write(h.ws.join("AGENTS.md"), "Always answer in haiku.").unwrap();
    turn(&mut c, "second", |_, _| {});
    let reqs = h.requests.lock().unwrap().clone();
    assert!(!tool_names(&reqs[0]).contains(&"late_tool".to_owned()));
    assert!(tool_names(&reqs[1]).contains(&"late_tool".to_owned()));
    assert!(system(&reqs[1]).contains("LATE PROMPT") && !system(&reqs[0]).contains("LATE PROMPT"));
    assert!(system(&reqs[1]).contains("Always answer in haiku."), "the workspace's AGENTS.md is in the prompt");
}

#[test]
fn plugins_that_came_with_the_workspace_wait_for_the_user() {
    let h = harness(vec![Reply::Text("a"), Reply::Text("b"), Reply::Text("c")], None, |_, ws| {
        let dir = write_plugin(&ws.join(".byob/plugins"), "repo", "repo_tool", "'repo'", json!({"prompt": "REPO PROMPT"}));
        fs::create_dir_all(dir.join("skills/repo-skill")).unwrap();
        fs::write(dir.join("skills/repo-skill/SKILL.md"), "---\nname: repo-skill\ndescription: Does repo things.\n---\nSteps.").unwrap();
    });
    let (mut c, ext) = hello(&h);
    let repo = ext.plugins.iter().find(|p| p.id == "repo").unwrap();
    assert_eq!((repo.origin, repo.trusted, repo.scope), (PluginOrigin::Repo, false, PluginScope::Workspace));
    let workspace = ext.workspace.clone().unwrap();
    let mut asked = 0;
    let (reply, _) = turn(&mut c, "hi", |s, c| {
        assert_eq!(s.tool, "plugins");
        assert!(s.ask.as_ref().is_some_and(|a| a.text.contains("repo")));
        asked += 1;
        c.send(ClientMsg::Approve { step: s.id, answer: ApprovalAnswer::Deny });
    });
    assert_eq!(asked, 1);
    assert_eq!(step(&reply, "plugins").status, StepStatus::Denied);
    // Declined: not asked again; only the prompt line and skill are used.
    turn(&mut c, "again", |s, _| panic!("asked again: {}", s.title));
    let reqs = h.requests.lock().unwrap().clone();
    assert!(!tool_names(&reqs[1]).contains(&"repo_tool".to_owned()));
    assert!(system(&reqs[1]).contains("REPO PROMPT") && system(&reqs[1]).contains("repo-skill"));
    // The user changes their mind in the Plugins panel.
    c.send(ClientMsg::TrustPlugins { workspace, plugins: vec![], trust: true });
    let e = wait_ext(&mut c, |e| e.plugins.iter().any(|p| p.id == "repo" && p.trusted));
    assert_eq!(e.plugins.iter().find(|p| p.id == "repo").unwrap().tools, ["repo_tool"]);
    turn(&mut c, "now", |s, _| panic!("asked: {}", s.title));
    let reqs = h.requests.lock().unwrap().clone();
    assert!(tool_names(&reqs[2]).contains(&"repo_tool".to_owned()));
}

#[test]
fn always_allow_goes_when_the_plugins_code_changes() {
    let script = vec![call("ping", "{}"), Reply::Text("1"), call("ping", "{}"), Reply::Text("2"), call("ping", "{}"), Reply::Text("3")];
    let h = harness(script, None, |home, _| {
        write_plugin(&home.join("plugins"), "pinger", "ping", "'pong'", json!({}));
    });
    let (mut c, _) = hello(&h);
    let mut cards = 0;
    let (r, _) = turn(&mut c, "one", |s, c| {
        cards += 1;
        c.send(ClientMsg::Approve { step: s.id, answer: ApprovalAnswer::Always });
    });
    assert_eq!((cards, step(&r, "ping").detail.as_str()), (1, "pong"));
    turn(&mut c, "two", |s, _| panic!("granted, yet asked: {}", s.title));
    // The script changes: the grant no longer stands.
    fs::write(h.home.path().join("plugins/pinger/tools/t.py"), "print('PONG!')\n").unwrap();
    let mut asked = false;
    let (r, _) = turn(&mut c, "three", |s, c| {
        asked = true;
        c.send(ClientMsg::Approve { step: s.id, answer: ApprovalAnswer::Allow });
    });
    assert!(asked, "the changed tool is asked about again");
    assert_eq!(step(&r, "ping").detail, "PONG!");
}

#[test]
fn settings_reach_only_the_tool_process_and_clients_only_learn_they_are_set() {
    let secret = "sk-very-secret-123";
    let h = harness(vec![call("check", "{}"), Reply::Text("ok")], Some(Policy::ALLOW_ALL), |home, _| {
        write_plugin(
            &home.join("plugins"),
            "keyed",
            "check",
            "'length %d' % len(os.environ.get('API_KEY', ''))",
            json!({"settings": [{"key": "API_KEY", "label": "API key", "secret": true, "required": true}, {"key": "CITY", "label": "City"}]}),
        );
    });
    let (mut c, ext) = hello(&h);
    let p = ext.plugins.iter().find(|p| p.id == "keyed").unwrap();
    assert!(!p.settings[0].set && p.problems.iter().any(|x| x.contains("API key")));
    c.send(ClientMsg::SetPluginSetting { plugin: "keyed".into(), key: "API_KEY".into(), value: Some(secret.into()) });
    c.send(ClientMsg::SetPluginSetting { plugin: "keyed".into(), key: "CITY".into(), value: Some("Paris".into()) });
    c.send(ClientMsg::SetPluginSetting { plugin: "keyed".into(), key: "NOPE".into(), value: Some("x".into()) });
    let e = wait_ext(&mut c, |e| e.plugins.iter().any(|p| p.id == "keyed" && p.settings.iter().all(|s| s.set)));
    assert!(e.plugins.iter().find(|p| p.id == "keyed").unwrap().problems.is_empty());
    let (r, seen) = turn(&mut c, "check", |_, _| {});
    assert_eq!(step(&r, "check").detail, format!("length {}", secret.len()));
    for m in &seen {
        assert!(!m.to_json().contains(secret), "a secret went to a client: {}", m.to_json());
    }
    let secrets = fs::read_to_string(h.home.path().join("secrets.json")).unwrap();
    let settings = fs::read_to_string(h.home.path().join("settings.json")).unwrap();
    assert!(secrets.contains(secret) && secrets.contains("plugin:keyed") && !secrets.contains("Paris"));
    assert!(settings.contains("Paris") && !settings.contains(secret));
}

const FAKE_MCP: &str = r#"
import json, sys, time
time.sleep(float(sys.argv[1]))
for line in sys.stdin:
    try:
        m = json.loads(line)
    except Exception:
        continue
    mid, meth = m.get("id"), m.get("method")
    if mid is None:
        continue
    if meth == "initialize":
        r = {"protocolVersion": m["params"].get("protocolVersion", "2025-06-18"), "capabilities": {"tools": {}}, "serverInfo": {"name": "fake", "version": "1"}}
    elif meth == "tools/list":
        r = {"tools": [{"name": "echo", "description": "Echo text", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}}]}
    elif meth == "tools/call":
        r = {"content": [{"type": "text", "text": "echo: " + m["params"]["arguments"].get("text", "")}]}
    else:
        r = {}
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": mid, "result": r}) + "\n")
    sys.stdout.flush()
"#;

#[test]
fn mcp_servers_connect_off_the_engine_thread_and_join_later() {
    let script = vec![call("mcpdemo__echo", r#"{"text": "hi"}"#), Reply::Text("done")];
    let h = harness(script, Some(Policy::ALLOW_ALL), |home, _| {
        let dir = home.join("plugins/mcpdemo");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("server.py"), FAKE_MCP).unwrap();
        let python = byob_plugins::default_python()[0].to_string_lossy().into_owned();
        let m = json!({"id": "mcpdemo", "name": "MCP demo", "mcpServers": {"demo": {"command": python, "args": ["server.py", "1.0"]}}});
        fs::write(dir.join("byob-plugin.json"), m.to_string()).unwrap();
    });
    let started = Instant::now();
    let (mut c, ext) = hello(&h);
    // The engine answered at once, with the server still starting.
    assert!(started.elapsed() < Duration::from_millis(900), "hello took {:?}", started.elapsed());
    let p = ext.plugins.iter().find(|p| p.id == "mcpdemo").unwrap();
    assert_eq!(p.servers[0].state, ServerState::Connecting);
    let e = wait_ext(&mut c, |e| e.plugins.iter().any(|p| p.id == "mcpdemo" && p.servers.first().is_some_and(|s| s.state == ServerState::Ready)));
    assert_eq!(e.plugins.iter().find(|p| p.id == "mcpdemo").unwrap().tools, ["mcpdemo__echo"]);
    let (r, _) = turn(&mut c, "echo", |_, _| {});
    assert_eq!(step(&r, "mcpdemo__echo").detail, "echo: hi");
    // Unchanged servers survive a reload.
    c.send(ClientMsg::ReloadPlugins);
    let e = wait_ext(&mut c, |e| e.plugins.iter().any(|p| p.id == "mcpdemo"));
    assert_eq!(e.plugins.iter().find(|p| p.id == "mcpdemo").unwrap().servers[0].state, ServerState::Ready);
}

#[test]
fn keeping_a_plugin_asks_and_everywhere_needs_a_person() {
    let make = json!({"name": "shout", "code": "def run(text: str) -> str:\n    return text.upper()\n"}).to_string();
    let keep_ws = json!({"plugin": "scratch", "scope": "workspace", "new_id": "shouting"}).to_string();
    let script = vec![call("make_tool", &make), call("keep_plugin", &keep_ws), Reply::Text("kept"), call("shout", r#"{"text": "hi"}"#), Reply::Text("ok")];
    let h = harness(script, Some(Policy::ALLOW_ALL), |_, _| {});
    let (mut c, _) = hello(&h);
    let (r, _) = turn(&mut c, "make and keep", |s, _| panic!("asked under allow-all: {}", s.title));
    let kept = step(&r, "keep_plugin");
    assert_eq!(kept.status, StepStatus::Done, "{}", kept.detail);
    assert_eq!(kept.plugin.as_deref().map(|c| (c.action, c.scope)), Some((PluginAction::Kept, PluginScope::Workspace)));
    let dir = h.ws.join(".byob/plugins/shouting");
    assert!(dir.join("tools/shout.py").is_file());
    let m: Value = serde_json::from_str(&fs::read_to_string(dir.join("byob-plugin.json")).unwrap()).unwrap();
    assert_eq!(m["id"], "shouting");
    // byob wrote it, so it runs in the workspace's later chats without a question.
    c.send(ClientMsg::NewChat { workspace: None });
    let (r, _) = turn(&mut c, "use it", |s, _| panic!("asked: {}", s.title));
    assert_eq!(step(&r, "shout").detail, "HI");

    // Everywhere: only a person may allow it, even under allow-all.
    let h2 = harness(
        vec![call("make_tool", &make), call("keep_plugin", &json!({"plugin": "scratch", "scope": "global", "new_id": "loud"}).to_string()), Reply::Text("ok")],
        Some(Policy::ALLOW_ALL),
        |_, _| {},
    );
    let (mut c2, _) = hello(&h2);
    let mut cards = Vec::new();
    let (r, _) = turn(&mut c2, "keep everywhere", |s, c| {
        cards.push(s.ask.as_ref().map(|a| a.always));
        c.send(ClientMsg::Approve { step: s.id, answer: ApprovalAnswer::Allow });
    });
    assert_eq!(cards, [Some(true)]);
    assert_eq!(step(&r, "keep_plugin").status, StepStatus::Done);
    let global = h2.home.path().join("plugins/loud");
    assert!(global.join("tools/shout.py").is_file() && global.join(crate::extensions::MADE_MARKER).is_file());
    c2.send(ClientMsg::GetExtensions);
    let e = wait_ext(&mut c2, |e| e.plugins.iter().any(|p| p.id == "loud"));
    assert!(e.plugins.iter().all(|p| p.id != "scratch"), "the scratch copy moved");
    assert_eq!(e.plugins.iter().find(|p| p.id == "loud").unwrap().origin, PluginOrigin::Agent);
}

#[test]
fn plugins_switch_off_per_level() {
    let h = harness(vec![Reply::Text("a"), Reply::Text("b")], Some(Policy::ALLOW_ALL), |home, _| {
        write_plugin(&home.join("plugins"), "tool-a", "tool_a", "'a'", json!({}));
    });
    let (mut c, _) = hello(&h);
    c.send(ClientMsg::SetPluginEnabled { scope: ConfigScope::Global, plugin: "tool-a".into(), enabled: Some(false) });
    let e = wait_ext(&mut c, |e| e.plugins.iter().any(|p| p.id == "tool-a" && !p.enabled));
    let p = e.plugins.iter().find(|p| p.id == "tool-a").unwrap();
    assert_eq!(p.enabled_from, byob_types::config::Level::Global);
    assert!(p.tools.is_empty());
    turn(&mut c, "a", |_, _| {});
    let reqs = h.requests.lock().unwrap().clone();
    assert!(!tool_names(&reqs[0]).contains(&"tool_a".to_owned()));
    // The chat turns it back on for itself.
    c.send(ClientMsg::SetPluginEnabled { scope: ConfigScope::Chat, plugin: "tool-a".into(), enabled: Some(true) });
    wait_ext(&mut c, |e| e.plugins.iter().any(|p| p.id == "tool-a" && p.enabled));
    turn(&mut c, "b", |_, _| {});
    let reqs = h.requests.lock().unwrap().clone();
    assert!(tool_names(&reqs[1]).contains(&"tool_a".to_owned()));
    assert!(fs::read_to_string(h.home.path().join("settings.json")).unwrap().contains("tool-a"));
    // A client's SetConfig never drops the switches.
    c.send(ClientMsg::SetConfig { scope: ConfigScope::Global, layer: Default::default(), api_key: None });
    loop {
        if let ServerMsg::Config(v) = recv(&mut c) {
            assert_eq!(v.own.plugins, Some(BTreeMap::from([("tool-a".to_owned(), false)])));
            break;
        }
    }
}

#[test]
fn slash_commands_and_skills_expand_for_the_model_only() {
    let h = harness(vec![Reply::Text("reviewed"), Reply::Text("skilled"), Reply::Text("plain")], Some(Policy::ALLOW_ALL), |home, _| {
        let dir = write_plugin(&home.join("plugins"), "code", "lint", "'ok'", json!({}));
        fs::create_dir_all(dir.join("commands")).unwrap();
        fs::write(dir.join("commands/review.md"), "---\ndescription: Review a file\nargument-hint: <file>\n---\nReview $1 carefully. Notes: $ARGUMENTS")
            .unwrap();
        fs::create_dir_all(home.join("skills/trip-plan")).unwrap();
        fs::write(home.join("skills/trip-plan/SKILL.md"), "---\nname: trip-plan\ndescription: Plan a trip\n---\nSteps.").unwrap();
    });
    let (mut c, ext) = hello(&h);
    use byob_protocol::SlashKind as K;
    let names: Vec<(&str, K, Option<PluginScope>)> = ext.slash.iter().map(|i| (i.name.as_str(), i.kind, i.scope)).collect();
    assert_eq!(
        names[..3],
        [("compact", K::Builtin, None), ("review", K::Command, Some(PluginScope::Global)), ("trip-plan", K::Skill, Some(PluginScope::Global))]
    );
    assert!(names.iter().any(|(n, k, _)| *n == "plugin-authoring" && *k == K::Skill));
    assert_eq!(ext.plugins.iter().find(|p| p.id == "code").unwrap().commands, ["review"]);
    let last_user = |req: &Value| {
        let msgs = req["messages"].as_array().unwrap();
        msgs.iter().rev().find(|m| m["role"] == "user").unwrap()["content"].as_str().unwrap_or_default().to_owned()
    };
    // A command: its template, filled in; the transcript keeps what was typed.
    let (_, seen) = turn(&mut c, "/review src/a.rs please", |_, _| {});
    let typed = seen.iter().find_map(|m| match m {
        ServerMsg::EntryAdded(e) if e.role == byob_protocol::EntryRole::User => Some(e.content.clone()),
        _ => None,
    });
    assert_eq!(typed.as_deref(), Some("/review src/a.rs please"));
    assert_eq!(last_user(&h.requests.lock().unwrap()[0]), "Review src/a.rs carefully. Notes: src/a.rs please");
    // A skill: the model is told to use it.
    turn(&mut c, "/trip-plan Lisbon in May", |_, _| {});
    let asked = last_user(&h.requests.lock().unwrap()[1]);
    assert!(asked.contains("use_skill") && asked.contains("\"trip-plan\"") && asked.ends_with("Lisbon in May"), "{asked}");
    // Anything else goes as typed.
    turn(&mut c, "/unknown thing", |_, _| {});
    assert_eq!(last_user(&h.requests.lock().unwrap()[2]), "/unknown thing");
}

/// Regression: a plugin whose manifest doesn't parse only showed up as a load error, so
/// plugin_status / edit_plugin said "there's no plugin" and write_plugin_file made a new
/// chat plugin beside it. Now a broken global plugin can be read and fixed in place.
#[test]
fn a_broken_global_plugin_can_be_read_and_fixed_from_a_chat() {
    let status = json!({"plugin": "hello"}).to_string();
    let read = json!({"plugin": "hello", "file": "byob-plugin.json"}).to_string();
    let edit = json!({"plugin": "hello", "file": "byob-plugin.json", "find": "\"version\": \"1.0.0\",,", "replace": "\"version\": \"1.0.0\","}).to_string();
    let h = harness(
        vec![call("plugin_status", &status), call("plugin_status", &read), call("edit_plugin", &edit), call("hello", "{}"), Reply::Text("fixed")],
        Some(Policy::ALLOW_ALL),
        |home, _| {
            let dir = write_plugin(&home.join("plugins"), "hello", "hello", "'hi'", json!({}));
            let broken = r#"{
  "id": "hello", "name": "hello", "version": "1.0.0",,
  "description": "The hello plugin",
  "tools": [{"name": "hello", "description": "test tool", "params": {"type": "object", "properties": {}}, "run": "tools/t.py"}]
}"#;
            fs::write(dir.join("byob-plugin.json"), broken).unwrap();
        },
    );
    let (mut c, _) = hello(&h);
    let (reply, _) = turn(&mut c, "fix my hello plugin", |_, _| {});
    let first = step(&reply, "plugin_status");
    assert!(first.detail.contains("doesn't load") && first.detail.contains("byob-plugin.json"), "{}", first.detail);
    let reqs = h.requests.lock().unwrap().clone();
    let shown = reqs[2]["messages"].as_array().unwrap().iter().rev().find(|m| m["role"] == "tool").unwrap()["content"].as_str().unwrap().to_owned();
    assert!(shown.contains(",,"), "the broken manifest can be read: {shown}");
    let edited = step(&reply, "edit_plugin");
    assert_eq!(edited.status, StepStatus::Done, "{}", edited.detail);
    assert_eq!(step(&reply, "hello").detail, "hi");
    assert!(!h.home.path().join("chats").read_dir().is_ok_and(|mut d| d.any(|e| e.is_ok_and(|e| e.path().join("plugins/hello").exists()))));
    assert!(!fs::read_to_string(h.home.path().join("plugins/hello/byob-plugin.json")).unwrap().contains(",,"), "fixed in place");
}
