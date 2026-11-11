//! Hooks in the turn pipeline through the engine: native hooks (ordering, modify, allow,
//! stop), a Python backend plugin's hooks (deny with a message, a prompt line), backend
//! timeouts and faults (fail open / fail closed, reported in the plugin's status), and the
//! built-ins on the pipeline (subagents forked, environment providers from a manifest).

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use byob_protocol::{ApprovalAnswer, ClientMsg, Connection, Entry, Extensions, PluginInfo, PluginScope, ServerMsg, StepStatus};
use byob_store::Paths;
use byob_types::InferenceConfig;
use serde_json::{Value, json};

use crate::backend_tests::have_python;
use crate::extensions::PluginsConfig;
use crate::extensions_tests::system;
use crate::hooks::{Decision, Event, Flow, NativeHook, RequestCtx, ToolCallCtx};
use crate::loop_tests::{Reply, Requests, call, fake_llm};
use crate::natives::{Natives, bundled_plugins};
use crate::{EngineConfig, EngineHandle, LocalConnection, Policy};

struct H {
    engine: EngineHandle,
    requests: Requests,
    home: tempfile::TempDir,
    ws: tempfile::TempDir,
}

/// An engine over a fake model with byob's bundled plugins, `natives` and the global
/// plugins `setup` writes (into `<home>/plugins`).
fn harness(script: Vec<Reply>, policy: Option<Policy>, natives: Natives, setup: impl FnOnce(&Path)) -> H {
    let (url, requests) = fake_llm(script, "final answer");
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    fs::create_dir_all(home.path().join("plugins")).unwrap();
    setup(&home.path().join("plugins"));
    let mut config = EngineConfig::new(InferenceConfig { base_url: url, ..Default::default() });
    config.workspace = ws.path().canonicalize().unwrap();
    config.register_workspace = true;
    config.tools = byob_tools::builtin();
    config.policy = policy;
    config.paths = Some(Paths::at(home.path()));
    config.auto_title = false;
    config.plugins = Some(PluginsConfig {
        global_roots: vec![home.path().join("plugins")],
        scratch_root: Some(home.path().join("chats")),
        python_root: Some(home.path().join("python")),
        python: byob_plugins::default_python(),
        bundled: bundled_plugins(),
        natives,
        ..Default::default()
    });
    H { engine: EngineHandle::spawn_with(config).unwrap(), requests, home, ws }
}

fn plugin(root: &Path, id: &str, manifest: Value, files: &[(&str, &str)]) {
    let dir = root.join(id);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("byob-plugin.json"), manifest.to_string()).unwrap();
    for (name, text) in files {
        fs::write(dir.join(name), text).unwrap();
    }
}

fn recv(c: &mut LocalConnection) -> ServerMsg {
    let start = Instant::now();
    loop {
        if let Some(m) = c.try_recv() {
            return m;
        }
        assert!(start.elapsed() < Duration::from_secs(30), "timed out waiting for the engine");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn hello(h: &H) -> (LocalConnection, Extensions) {
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

/// Send `text` and run the turn to its end; approval cards are answered with `answer`
/// (and counted). Returns the reply, its error and how many cards there were.
fn turn(c: &mut LocalConnection, text: &str, answer: ApprovalAnswer) -> (Entry, Option<String>, usize) {
    c.send(ClientMsg::SendMessage { text: text.into(), attachments: vec![] });
    let mut entries: Vec<Entry> = Vec::new();
    let mut cards = 0;
    loop {
        match recv(c) {
            ServerMsg::EntryAdded(e) => entries.push(e),
            ServerMsg::StepUpdated { entry, step } => {
                if step.status == StepStatus::Waiting {
                    cards += 1;
                    c.send(ClientMsg::Approve { step: step.id, answer });
                }
                if let Some(e) = entries.iter_mut().find(|e| e.id == entry) {
                    e.upsert_step(step);
                }
            }
            ServerMsg::EntryFinished { id, error, .. } => return (entries.into_iter().find(|e| e.id == id).unwrap(), error, cards),
            _ => {}
        }
    }
}

fn find<'a>(e: &'a Extensions, id: &str) -> &'a PluginInfo {
    e.plugins.iter().find(|p| p.id == id).unwrap_or_else(|| panic!("no plugin {id}: {:?}", e.plugins.iter().map(|p| &p.id).collect::<Vec<_>>()))
}

fn extensions(c: &mut LocalConnection) -> Extensions {
    c.send(ClientMsg::GetExtensions);
    loop {
        if let ServerMsg::Extensions(e) = recv(c) {
            return *e;
        }
    }
}

/// The tool results the model got in `req`.
fn results(req: &Value) -> Vec<String> {
    req["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "tool").map(|m| m["content"].as_str().unwrap_or_default().to_owned()).collect()
}

// ---- native hooks -----------------------------------------------------------------------

/// Moves every `write_file` into `safe/`.
struct IntoSafe;
impl NativeHook for IntoSafe {
    fn before_tool_call(&self, ctx: &mut ToolCallCtx<'_>) -> Decision {
        if let Some(p) = ctx.args.get("path").and_then(Value::as_str).filter(|p| !p.starts_with("safe/")) {
            ctx.args["path"] = json!(format!("safe/{p}"));
        }
        Decision::Continue
    }
}

/// Approves writes under `safe/` (what it sees is after IntoSafe, which runs first).
struct ApproveSafe(Arc<Mutex<Vec<String>>>);
impl NativeHook for ApproveSafe {
    fn before_tool_call(&self, ctx: &mut ToolCallCtx<'_>) -> Decision {
        let path = ctx.args.get("path").and_then(Value::as_str).unwrap_or_default().to_owned();
        self.0.lock().unwrap().push(path.clone());
        if path.starts_with("safe/") { Decision::Allow } else { Decision::Continue }
    }
}

#[test]
fn native_hooks_run_in_priority_order_modify_arguments_and_approve() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let natives = Natives::builtin().hook("into_safe", Arc::new(IntoSafe), &[]).hook("approve_safe", Arc::new(ApproveSafe(Arc::clone(&seen))), &[]);
    let script = vec![call("write_file", r#"{"path":"notes.txt","content":"hi"}"#), Reply::Text("done")];
    // Declared in the "wrong" order: priority decides.
    let h = harness(script, None, natives, |root| {
        plugin(
            root,
            "safe-writes",
            json!({"id": "safe-writes", "name": "Safe writes", "hooks": [
                {"event": "before_tool_call", "native": "approve_safe", "priority": 20},
                {"event": "before_tool_call", "native": "into_safe", "priority": 10, "tools": ["write_file"]}
            ]}),
            &[],
        );
    });
    let (mut c, _) = hello(&h);
    let (reply, error, cards) = turn(&mut c, "write a note", ApprovalAnswer::Deny);
    assert_eq!(error, None);
    assert_eq!(cards, 0, "the hook approved it: the default policy (ask for writes) didn't get to ask");
    assert_eq!(*seen.lock().unwrap(), ["safe/notes.txt"], "the later hook saw the earlier one's change");
    assert_eq!(fs::read_to_string(h.ws.path().join("safe/notes.txt")).unwrap(), "hi");
    assert!(!h.ws.path().join("notes.txt").exists());
    assert_eq!(reply.steps[0].status, StepStatus::Done);
}

#[test]
fn without_a_decision_the_permission_gate_still_asks() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let natives = Natives::builtin().hook("approve_safe", Arc::new(ApproveSafe(Arc::clone(&seen))), &[]);
    let script = vec![call("write_file", r#"{"path":"notes.txt","content":"hi"}"#), Reply::Text("done")];
    let h = harness(script, None, natives, |root| {
        plugin(root, "approver", json!({"id": "approver", "name": "Approver", "hooks": [{"event": "before_tool_call", "native": "approve_safe"}]}), &[]);
    });
    let (mut c, _) = hello(&h);
    let (reply, _, cards) = turn(&mut c, "write a note", ApprovalAnswer::Deny);
    assert_eq!(cards, 1, "the hook said nothing about notes.txt: the gate asked");
    assert_eq!(reply.steps[0].status, StepStatus::Denied);
    assert!(!h.ws.path().join("notes.txt").exists());
}

/// Ends the turn before its second request.
struct Budget(Arc<AtomicUsize>);
impl NativeHook for Budget {
    fn before_request(&self, ctx: &mut RequestCtx<'_>) -> Flow {
        self.0.fetch_add(1, Ordering::SeqCst);
        if ctx.round > 0 { Flow::Stop("the request budget is used up".into()) } else { Flow::Continue }
    }
}

#[test]
fn a_before_request_stop_ends_the_turn_with_its_reason() {
    let calls = Arc::new(AtomicUsize::new(0));
    // A native nobody names runs in its default place (here: every request).
    let natives = Natives::builtin().hook("budget", Arc::new(Budget(Arc::clone(&calls))), &[(Event::BeforeRequest, 50)]);
    let h = harness(vec![call("list_files", "{}"), Reply::Text("done")], Some(Policy::ALLOW_ALL), natives, |_| {});
    let (mut c, _) = hello(&h);
    let (_, error, _) = turn(&mut c, "list", ApprovalAnswer::Deny);
    assert_eq!(error.as_deref(), Some("the request budget is used up"));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(h.requests.lock().unwrap().len(), 1, "the second request was never made");
}

// ---- a Python backend plugin's hooks ----------------------------------------------------

const GUARD: &str = r#"
import byob_plugin as byob

@byob.hook("before_tool_call")
def guard(call):
    if call["tool"] == "delete_file":
        return byob.deny("Deleting files is turned off by the guard plugin; move it to old/ instead.")

@byob.hook("before_prompt")
def prompt(p):
    assert any(s["id"] == "base_prompt" for s in p["sections"])
    return byob.modify(add=["GUARD: never delete files."])

byob.run()
"#;

#[test]
fn a_backend_hook_denies_delete_file_with_a_message_and_adds_a_prompt_line() {
    if !have_python() {
        return;
    }
    let script = vec![call("delete_file", r#"{"path":"keep.txt"}"#), Reply::Text("I couldn't delete it.")];
    let h = harness(script, Some(Policy::ALLOW_ALL), Natives::builtin(), |root| {
        plugin(
            root,
            "guard",
            json!({"id": "guard", "name": "Guard", "backend": "backend.py", "hooks": [
                {"event": "before_tool_call", "tools": ["delete_file"], "fail": "closed"},
                {"event": "before_prompt", "priority": 700}
            ]}),
            &[("backend.py", GUARD)],
        );
    });
    fs::write(h.ws.path().join("keep.txt"), "precious").unwrap();
    let (mut c, ext) = hello(&h);
    assert!(find(&ext, "guard").problems.is_empty(), "{:?}", find(&ext, "guard").problems);
    let (reply, error, _) = turn(&mut c, "delete keep.txt", ApprovalAnswer::Deny);
    assert_eq!(error, None);
    assert!(h.ws.path().join("keep.txt").exists(), "the hook kept the file");
    let step = &reply.steps[0];
    assert_eq!(step.status, StepStatus::Denied);
    assert!(step.detail.contains("turned off by the guard plugin"), "{}", step.detail);
    let reqs = h.requests.lock().unwrap().clone();
    let r = results(&reqs[1]);
    assert!(r[0].starts_with("Not run: Deleting files is turned off by the guard plugin; move it to old/ instead."), "{r:?}");
    // The prompt line went last (priority 700, after the artifact list at 600).
    assert!(system(&reqs[0]).ends_with("\n\nGUARD: never delete files."), "{}", system(&reqs[0]));
    assert!(system(&reqs[0]).starts_with(crate::turn::DEFAULT_SYSTEM_PROMPT));
}

const SLOW_AND_BROKEN: &str = r#"
import time
import byob_plugin as byob

@byob.hook("before_tool_call")
def check(call):
    if call["tool"] == "write_file":
        time.sleep(1.5)
        return byob.deny("too late to matter")
    raise RuntimeError("the policy file is missing")

byob.run()
"#;

#[test]
fn backend_hook_faults_fail_open_or_closed_and_reach_the_plugins_status() {
    if !have_python() {
        return;
    }
    let script = vec![
        Reply::Calls(vec![("write_file".into(), r#"{"path":"a.txt","content":"a"}"#.into()), ("delete_file".into(), r#"{"path":"b.txt"}"#.into())]),
        Reply::Text("done"),
    ];
    let h = harness(script, Some(Policy::ALLOW_ALL), Natives::builtin(), |root| {
        plugin(
            root,
            "policy",
            json!({"id": "policy", "name": "Policy", "backend": "backend.py", "hooks": [
                {"event": "before_tool_call", "tools": ["write_file"], "timeout_ms": 300},
                {"event": "before_tool_call", "tools": ["delete_file"], "fail": "closed", "timeout_ms": 10000}
            ]}),
            &[("backend.py", SLOW_AND_BROKEN)],
        );
    });
    fs::write(h.ws.path().join("b.txt"), "b").unwrap();
    let (mut c, _) = hello(&h);
    let (reply, error, _) = turn(&mut c, "write a, delete b", ApprovalAnswer::Deny);
    assert_eq!(error, None);
    // Timed out, fail open: the write went ahead as if the hook said continue.
    assert_eq!(reply.steps[0].status, StepStatus::Done);
    assert!(h.ws.path().join("a.txt").exists());
    // Raised, fail closed: the delete was refused.
    assert_eq!(reply.steps[1].status, StepStatus::Denied);
    assert!(h.ws.path().join("b.txt").exists());
    let reqs = h.requests.lock().unwrap().clone();
    let r = results(&reqs[1]);
    assert!(r[1].contains("the policy plugin's before_tool_call hook failed") && r[1].contains("the policy file is missing"), "{r:?}");
    // Both are faults in the plugin's status (what plugin_status shows the model).
    let ext = extensions(&mut c);
    let faults = &find(&ext, "policy").backend.as_ref().expect("a backend").faults;
    assert!(faults.iter().any(|f| f.contains("before_tool_call hook") && f.contains("didn't answer within 300 ms")), "{faults:?}");
    assert!(faults.iter().any(|f| f.contains("before_tool_call hook") && f.contains("the policy file is missing")), "{faults:?}");
}

const OBSERVER: &str = r#"
import json, os
import byob_plugin as byob

@byob.hook("after_tool_call")
def after(call):
    return byob.modify(result=call["result"] + "\n(checked by observer)", card={"type": "text", "text": "observed " + call["tool"]})

@byob.hook("on_turn_end")
def end(turn):
    with open(os.path.join(os.environ["BYOB_PLUGIN_DIR"], "ended.json"), "w") as f:
        json.dump({"answer": turn["answer"], "error": turn["error"]}, f)

byob.run()
"#;

#[test]
fn after_tool_call_changes_the_result_and_attaches_a_card_and_on_turn_end_observes() {
    if !have_python() {
        return;
    }
    let script = vec![call("list_files", "{}"), Reply::Text("Listed.")];
    let h = harness(script, Some(Policy::ALLOW_ALL), Natives::builtin(), |root| {
        plugin(
            root,
            "observer",
            json!({"id": "observer", "name": "Observer", "backend": "backend.py", "hooks": [
                {"event": "after_tool_call", "tools": ["list_files"]},
                {"event": "on_turn_end"}
            ]}),
            &[("backend.py", OBSERVER)],
        );
    });
    let (mut c, _) = hello(&h);
    let (reply, error, _) = turn(&mut c, "list", ApprovalAnswer::Deny);
    assert_eq!(error, None);
    let reqs = h.requests.lock().unwrap().clone();
    assert!(results(&reqs[1])[0].ends_with("\n(checked by observer)"), "{:?}", results(&reqs[1]));
    let step = &reply.steps[0];
    assert_eq!(step.views.len(), 1);
    assert_eq!(step.views[0].kind, byob_protocol::tree::KIND);
    assert!(step.views[0].fallback.contains("observed list_files"), "{:?}", step.views[0]);
    let ended = h.home.path().join("plugins/observer/ended.json");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ended.exists() {
        assert!(Instant::now() < deadline, "on_turn_end never ran");
        std::thread::sleep(Duration::from_millis(20));
    }
    let v: Value = serde_json::from_str(&fs::read_to_string(ended).unwrap()).unwrap();
    assert_eq!(v, json!({"answer": "Listed.", "error": null}));
}

/// Wants a person to confirm every run_command, whatever the policy.
struct AskFirst;
impl NativeHook for AskFirst {
    fn before_tool_call(&self, _: &mut ToolCallCtx<'_>) -> Decision {
        Decision::Ask("The careful plugin wants you to see this one.".into())
    }
}

#[test]
fn an_ask_decision_shows_a_card_even_with_auto_approve() {
    let natives = Natives::builtin().hook("ask_first", Arc::new(AskFirst), &[(Event::BeforeToolCall, 10)]);
    let script = vec![call("write_file", r#"{"path":"x.txt","content":"x"}"#), Reply::Text("done")];
    let h = harness(script, Some(Policy::ALLOW_ALL), natives, |_| {});
    let (mut c, _) = hello(&h);
    let (reply, _, cards) = turn(&mut c, "write", ApprovalAnswer::Allow);
    assert_eq!(cards, 1, "--auto-approve doesn't answer what a hook wants a person to see");
    let step = &reply.steps[0];
    assert_eq!(step.status, StepStatus::Done);
    assert!(step.ask.as_ref().is_some_and(|a| a.text.contains("careful plugin")), "{:?}", step.ask);
    assert!(h.ws.path().join("x.txt").exists());
}

// ---- built-ins on the pipeline ----------------------------------------------------------

#[test]
fn the_subagents_plugin_offers_delegate_and_a_fork_keeps_it_working() {
    let h = harness(vec![Reply::Text("hi")], Some(Policy::ALLOW_ALL), Natives::builtin(), |_| {});
    let (mut c, ext) = hello(&h);
    let sub = find(&ext, "subagents");
    assert_eq!((sub.scope, sub.tools.as_slice()), (PluginScope::Bundled, ["delegate".to_owned()].as_slice()));
    let _ = turn(&mut c, "hi", ApprovalAnswer::Deny);
    let first = h.requests.lock().unwrap()[0].clone();
    assert!(crate::extensions_tests::tool_names(&first).last().is_some_and(|n| n == "delegate"), "concurrent tools go last");
    assert!(system(&first).contains("## Subagents"));
    // Switched off: no delegate, no section.
    c.send(ClientMsg::SetPluginEnabled { plugin: "subagents".into(), scope: byob_protocol::ConfigScope::Global, enabled: Some(false) });
    let _ = extensions(&mut c);
    c.send(ClientMsg::NewChat { workspace: None });
    let _ = turn(&mut c, "hi again", ApprovalAnswer::Deny);
    let second = h.requests.lock().unwrap().last().unwrap().clone();
    assert!(!crate::extensions_tests::tool_names(&second).contains(&"delegate".to_owned()));
    assert!(!system(&second).contains("## Subagents"));
    // Forked to global (and on again): the copy wins and still delegates.
    c.send(ClientMsg::SetPluginEnabled { plugin: "subagents".into(), scope: byob_protocol::ConfigScope::Global, enabled: None });
    c.send(ClientMsg::KeepPlugin { plugin: "subagents".into(), scope: PluginScope::Global, new_id: None });
    let deadline = Instant::now() + Duration::from_secs(10);
    let forked = loop {
        let e = extensions(&mut c);
        if find(&e, "subagents").scope == PluginScope::Global {
            break e;
        }
        assert!(Instant::now() < deadline, "the fork never showed up");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(h.home.path().join("plugins/subagents/byob-plugin.json").is_file());
    assert_eq!(find(&forked, "subagents").tools, ["delegate"]);
    c.send(ClientMsg::NewChat { workspace: None });
    let _ = turn(&mut c, "and now?", ApprovalAnswer::Deny);
    let third = h.requests.lock().unwrap().last().unwrap().clone();
    assert!(crate::extensions_tests::tool_names(&third).contains(&"delegate".to_owned()));
    assert!(system(&third).contains("## Subagents"));
}

#[test]
fn a_plugin_declares_an_environment_provider() {
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let root = home.path().join("plugins");
    plugin(
        &root,
        "conda-env",
        json!({"id": "conda-env", "name": "Conda", "environments": [
            {"id": "conda", "label": "conda environment", "detect": ["environment.yml"], "command": "python3", "args": ["print_env.py"]}
        ]}),
        &[("print_env.py", "import json; print(json.dumps({'CONDA_DEFAULT_ENV': 'demo'}))")],
    );
    let mut ext = crate::extensions::Extensions::new(PluginsConfig {
        global_roots: vec![root.clone()],
        scratch_root: Some(home.path().join("chats")),
        bundled: bundled_plugins(),
        natives: Natives::builtin(),
        ..Default::default()
    });
    let compose = |ext: &mut crate::extensions::Extensions, switches: &[(&str, bool)]| {
        ext.refresh(None, ws.path());
        let global = byob_types::config::Layer { plugins: Some(switches.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect()), ..Default::default() };
        let eff = byob_types::config::resolve(
            byob_types::config::Levels { global: Some(&global), workspace: None, chat: None },
            byob_types::config::Keys::default(),
            byob_types::config::Forced::default(),
        );
        let (trust, values) = (BTreeMap::new(), BTreeMap::new());
        let base = byob_tools::builtin();
        ext.compose(&crate::extensions::ComposeIn {
            chat: None,
            workspace: ws.path(),
            effective: &eff,
            trust: &trust,
            values: &values,
            base: &base,
            types: &byob_tools::ArtifactTypes::builtin(),
        })
    };
    let c = compose(&mut ext, &[]);
    assert!(c.errors.is_empty() && find_info(&c.infos, "conda-env").problems.is_empty());
    let p = &c.env_providers;
    assert!(p.off.is_empty(), "byob's own stay on: {:?}", p.off);
    assert_eq!(p.external.len(), 1);
    let conda = &p.external[0];
    assert_eq!(
        (conda.id.as_str(), conda.label.as_str(), conda.detect.as_slice()),
        ("conda-env-conda", "conda environment", ["environment.yml".to_owned()].as_slice())
    );
    assert_eq!(conda.dir, root.join("conda-env"));
    // Switching the bundled environments plugin off leaves byob's own out; the plugin's stays.
    let c = compose(&mut ext, &[("environments", false)]);
    assert_eq!(c.env_providers.off, ["container", "mise", "nix", "pixi"]);
    assert_eq!(c.env_providers.external.len(), 1);
    let c = compose(&mut ext, &[("conda-env", false)]);
    assert!(c.env_providers.external.is_empty());
}

fn find_info<'a>(infos: &'a [PluginInfo], id: &str) -> &'a PluginInfo {
    infos.iter().find(|p| p.id == id).unwrap()
}
