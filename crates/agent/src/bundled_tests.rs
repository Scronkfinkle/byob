//! Bundled plugins: byob's own features (artifacts, HTML apps, charts, markdown, images, the
//! file and code-running tools) as plugins inside the binary, with native bindings.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use byob_protocol::{ApprovalAnswer, ClientMsg, ConfigScope, Connection, Extensions, PluginOrigin, PluginScope, ServerMsg, StepStatus};
use byob_store::Paths;
use byob_tools::ArtifactTypes;
use byob_types::InferenceConfig;
use byob_types::config::{Forced, Keys, Layer, Levels, resolve};
use serde_json::json;

use crate::extensions::{ComposeIn, Composed, Extensions as Ext, PluginsConfig};
use crate::extensions_tests::{recv, step, system, tool_names, turn, wait_ext};
use crate::loop_tests::{Reply, Requests, call, fake_llm};
use crate::natives::{Natives, bundled_plugins};
use crate::{EngineConfig, EngineHandle, LocalConnection, Policy};

/// What the app's code-running section would be.
const EXEC: &str = "Running code: run_command and run_python run in a sandbox.";

fn natives() -> Natives {
    Natives::builtin().prompt("exec", |_| EXEC.to_owned())
}

fn config(home: &Path, bundled: bool) -> PluginsConfig {
    PluginsConfig {
        global_roots: vec![home.join("plugins")],
        global_skills: Some(home.join("skills")),
        scratch_root: Some(home.join("chats")),
        python: byob_plugins::default_python(),
        bundled: if bundled { bundled_plugins() } else { Vec::new() },
        natives: natives(),
        ..Default::default()
    }
}

fn effective(switches: &[(&str, bool)]) -> byob_types::config::Effective {
    let global = Layer { plugins: Some(switches.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect()), ..Default::default() };
    resolve(Levels { global: Some(&global), workspace: None, chat: None }, Keys::default(), Forced::default())
}

/// Compose a new chat's plugins in `ws` with the built-in tools as the base.
fn compose(ext: &mut Ext, ws: &Path, switches: &[(&str, bool)]) -> Composed {
    ext.refresh(None, ws);
    let base = byob_tools::builtin();
    let trust = BTreeMap::new();
    let values = BTreeMap::new();
    let eff = effective(switches);
    ext.compose(&ComposeIn { chat: None, workspace: ws, effective: &eff, trust: &trust, values: &values, base: &base, types: &ArtifactTypes::builtin() })
}

#[test]
fn bundled_plugins_load_with_no_files_on_disk() {
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let mut ext = Ext::new(config(home.path(), true));
    let c = compose(&mut ext, ws.path(), &[]);
    assert!(c.errors.is_empty(), "{:?}", c.errors);
    let ids: Vec<&str> = c.infos.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, ["artifacts", "charts", "core-tools", "html-apps", "images", "markdown"]);
    for p in &c.infos {
        assert_eq!((p.scope, p.origin, p.enabled), (PluginScope::Bundled, PluginOrigin::Bundled, true), "{}", p.id);
        assert!(p.problems.is_empty(), "{}: {:?}", p.id, p.problems);
        assert!(!Path::new(&p.dir).exists(), "{} is in the binary, not at {}", p.id, p.dir);
    }
    let locked: Vec<&str> = c.infos.iter().filter(|p| p.locked).map(|p| p.id.as_str()).collect();
    assert_eq!(locked, ["artifacts", "core-tools"]);
    let html = c.infos.iter().find(|p| p.id == "html-apps").unwrap();
    assert_eq!((html.renderers[0].content_type.as_str(), html.renderers[0].renderer.as_str()), ("text/html", "web"));
    assert!(!html.starters.is_empty());
    let charts = c.infos.iter().find(|p| p.id == "charts").unwrap();
    assert_eq!(charts.tools, ["make_chart"]);
    // Every tool of the base is offered (run_command & co. are optional: not in this base).
    let mut have: Vec<String> = c.tools.specs().into_iter().map(|s| s.name).collect();
    have.retain(|n| n != "use_skill");
    let base: Vec<String> = byob_tools::builtin().specs().into_iter().map(|s| s.name).collect();
    assert_eq!(have, base);
}

#[test]
fn defaults_compose_the_same_prompt_and_tools_as_before() {
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let c = compose(&mut Ext::new(config(home.path(), true)), ws.path(), &[]);
    // What the app used to append to the system prompt itself.
    let old = format!("{}\n\n{EXEC}", ArtifactTypes::builtin().prompt());
    assert_eq!(c.sections, old);
    let base = byob_tools::builtin();
    for spec in base.specs() {
        assert_eq!(c.tools.get(&spec.name).map(|t| t.spec()), Some(spec.clone()), "{}", spec.name);
    }
}

/// An engine with plugins over a fake model; `bundled: false` is how byob was set up before
/// bundled plugins (the app appended the artifact and code-running prompts itself).
fn engine(script: Vec<Reply>, bundled: bool, home: &Path, ws: &Path) -> (EngineHandle, Requests) {
    let (url, requests) = fake_llm(script, "final answer");
    let mut config = EngineConfig::new(InferenceConfig { base_url: url, ..Default::default() });
    config.workspace = ws.to_path_buf();
    config.register_workspace = true;
    config.tools = byob_tools::builtin();
    config.policy = Some(Policy::ALLOW_ALL);
    config.paths = Some(Paths::at(home));
    config.auto_title = false;
    if !bundled {
        config.system_prompt = format!("{}\n\n{}", config.system_prompt, format!("{}\n\n{EXEC}", config.artifact_types.prompt()).trim());
    }
    config.plugins = Some(self::config(home, bundled));
    (EngineHandle::spawn_with(config).unwrap(), requests)
}

fn hello(engine: &EngineHandle) -> (LocalConnection, Extensions) {
    let mut c = engine.connect(Arc::new(|| {}));
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

#[test]
fn the_system_prompt_is_byte_identical_with_the_defaults() {
    let mut prompts = Vec::new();
    let mut tools = Vec::new();
    for bundled in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        let (engine, requests) = engine(vec![Reply::Text("hi")], bundled, home.path(), &ws);
        let (mut c, _) = hello(&engine);
        turn(&mut c, "hello", |_, _| {});
        let reqs = requests.lock().unwrap().clone();
        prompts.push(system(&reqs[0]));
        tools.push(serde_json::to_string(&reqs[0]["tools"]).unwrap());
    }
    assert!(prompts[1].contains("Pick the format that fits") && prompts[1].contains(EXEC));
    assert_eq!(prompts[0], prompts[1]);
    assert_eq!(tools[0], tools[1]);
}

#[test]
fn a_copy_anywhere_beats_the_bundled_one() {
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let mut ext = Ext::new(config(home.path(), true));
    let files = ext.bundled_files("charts").unwrap();
    crate::authoring::fork_files(&files, &home.path().join("plugins"), "charts").unwrap();
    let c = compose(&mut ext, ws.path(), &[]);
    let copies: Vec<(PluginScope, Option<PluginScope>)> = c.infos.iter().filter(|p| p.id == "charts").map(|p| (p.scope, p.shadowed_by)).collect();
    assert_eq!(copies, [(PluginScope::Global, None), (PluginScope::Bundled, Some(PluginScope::Global))]);
    // The fork keeps the native parts: same tool, same type.
    let global = c.infos.iter().find(|p| p.scope == PluginScope::Global).unwrap();
    assert_eq!((global.tools.as_slice(), global.problems.as_slice(), global.origin), (&["make_chart".to_owned()][..], &[][..], PluginOrigin::User));
    assert!(c.artifact_types.get(byob_protocol::artifact::TYPE_CHART).is_some());
    // A workspace copy beats both.
    crate::authoring::fork_files(&files, &ws.path().join(".byob/plugins"), "charts").unwrap();
    let c = compose(&mut ext, ws.path(), &[]);
    let copies: Vec<(PluginScope, Option<PluginScope>)> = c.infos.iter().filter(|p| p.id == "charts").map(|p| (p.scope, p.shadowed_by)).collect();
    assert_eq!(
        copies,
        [(PluginScope::Workspace, None), (PluginScope::Global, Some(PluginScope::Workspace)), (PluginScope::Bundled, Some(PluginScope::Workspace))]
    );
}

#[test]
fn core_plugins_stay_on_and_the_others_switch_off() {
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let mut ext = Ext::new(config(home.path(), true));
    let c = compose(&mut ext, ws.path(), &[("artifacts", false), ("core-tools", false), ("charts", false)]);
    let on = |id: &str| c.infos.iter().find(|p| p.id == id).unwrap().enabled;
    assert!(on("artifacts") && on("core-tools") && !on("charts"));
    for kept in ["create_artifact", "read_file", "write_file"] {
        assert!(c.tools.get(kept).is_some(), "{kept}");
    }
    // Charts off: no make_chart, no chart type, nothing about them in the prompt.
    assert!(c.tools.get("make_chart").is_none());
    assert!(c.artifact_types.get(byob_protocol::artifact::TYPE_CHART).is_none());
    assert_eq!(c.artifact_types.for_filename("a.chart.json").content_type, byob_protocol::artifact::TYPE_TEXT);
    assert!(!c.sections.contains("make_chart") && !c.sections.contains(".chart.json"), "{}", c.sections);
    let create = c.tools.get("create_artifact").unwrap().spec().description;
    assert!(!create.contains(".chart.json") && create.contains(".html"), "{create}");
    // HTML apps off: its guidance goes with it.
    let c = compose(&mut ext, ws.path(), &[("html-apps", false)]);
    assert!(!c.sections.contains("HTML apps:") && c.sections.contains("make_chart"));
}

#[test]
fn unknown_native_ids_are_problems_not_crashes() {
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let dir = home.path().join("plugins/odd");
    fs::create_dir_all(&dir).unwrap();
    let manifest = json!({"id": "odd", "name": "Odd",
        "tools": [{"native": "teleport"}, {"native": "warp_drive", "optional": true}],
        "artifact_types": [{"type": "text/x-odd", "native": "nope"}, {"type": "text/csv", "suffixes": [".csv"], "label": "table", "pick": "a table → .csv", "guidance": "missing.md"}],
        "renderers": [{"type": "text/csv", "renderer": "native:spreadsheet"}],
        "system_prompt": {"native": "nada"}});
    fs::write(dir.join("byob-plugin.json"), manifest.to_string()).unwrap();
    let mut ext = Ext::new(config(home.path(), true));
    let c = compose(&mut ext, ws.path(), &[]);
    let odd = c.infos.iter().find(|p| p.id == "odd").unwrap();
    assert_eq!(odd.problems.len(), 4, "the optional one is fine: {:?}", odd.problems);
    assert_eq!(odd.problems[0], "artifact_types: \"nope\" isn't an artifact type built into this byob");
    assert!(odd.problems[1].starts_with("artifact_types: guidance: ") && odd.problems[1].contains("missing.md"), "{}", odd.problems[1]);
    assert_eq!(odd.problems[2], "system_prompt: \"nada\" isn't a prompt section built into this byob");
    assert_eq!(odd.problems[3], "tools: \"teleport\" isn't a tool built into this byob");
    // The declarative type still works: a table type in the prompt and in create_artifact.
    assert_eq!(c.artifact_types.for_filename("prices.csv").label, "table");
    assert!(c.sections.contains("; a table → .csv."), "{}", c.sections);
}

#[test]
fn a_model_forks_html_apps_and_its_guidance_edit_changes_the_prompt() {
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let ws = ws.path().canonicalize().unwrap();
    let read = json!({"plugin": "html-apps", "file": "guidance.md"}).to_string();
    let edit_bundled = json!({"plugin": "html-apps", "file": "guidance.md", "find": "one self-contained file", "replace": "one file"}).to_string();
    let edit_fork = json!({"plugin": "html-apps", "file": "guidance.md", "find": "one self-contained file", "replace": "one SINGLE file"}).to_string();
    let fork = json!({"plugin": "html-apps", "scope": "global"}).to_string();
    let script = vec![
        call("plugin_status", &read),
        call("edit_plugin", &edit_bundled),
        call("keep_plugin", &fork),
        call("edit_plugin", &edit_fork),
        Reply::Text("done"),
        Reply::Text("next turn"),
    ];
    let (engine, requests) = engine(script, true, home.path(), &ws);
    let (mut c, ext) = hello(&engine);
    assert!(ext.plugins.iter().any(|p| p.id == "html-apps" && p.scope == PluginScope::Bundled));
    let mut asked = Vec::new();
    let (r, _) = turn(&mut c, "make html apps use one single file", |s, c| {
        asked.push(s.tool.clone());
        c.send(ClientMsg::Approve { step: s.id, answer: ApprovalAnswer::Allow });
    });
    let status = step(&r, "plugin_status");
    assert!(status.detail.contains("HTML apps: one self-contained file"), "bundled files are readable: {}", status.detail);
    let edits: Vec<_> = r.steps.iter().filter(|s| s.tool == "edit_plugin").collect();
    assert_eq!(edits[0].status, StepStatus::Failed);
    assert!(edits[0].detail.contains("Fork it first"), "{}", edits[0].detail);
    assert_eq!(step(&r, "keep_plugin").status, StepStatus::Done, "{}", step(&r, "keep_plugin").detail);
    assert_eq!(edits[1].status, StepStatus::Done, "{}", edits[1].detail);
    assert_eq!(asked, ["keep_plugin"], "forking everywhere needs a person; editing the fork follows the approvals");
    let fork_dir = home.path().join("plugins/html-apps");
    assert!(fs::read_to_string(fork_dir.join("guidance.md")).unwrap().contains("one SINGLE file"));
    // The next turn's prompt has the edited guidance; the native type (and its check) stay.
    turn(&mut c, "again", |_, _| {});
    let reqs = requests.lock().unwrap().clone();
    let first = system(&reqs[0]);
    let last = system(reqs.last().unwrap());
    assert!(first.contains("HTML apps: one self-contained file") && !first.contains("SINGLE"));
    assert!(last.contains("HTML apps: one SINGLE file") && !last.contains("one self-contained file"), "{last}");
    assert!(tool_names(reqs.last().unwrap()).contains(&"create_artifact".to_owned()));
    c.send(ClientMsg::GetExtensions);
    let e = wait_ext(&mut c, |e| e.plugins.iter().any(|p| p.id == "html-apps" && p.scope == PluginScope::Global));
    let copies: Vec<(PluginScope, Option<PluginScope>)> = e.plugins.iter().filter(|p| p.id == "html-apps").map(|p| (p.scope, p.shadowed_by)).collect();
    assert_eq!(copies, [(PluginScope::Global, None), (PluginScope::Bundled, Some(PluginScope::Global))]);
}

#[test]
fn clients_before_13_4_see_bundled_plugins_as_global() {
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let ws = ws.path().canonicalize().unwrap();
    let (engine, _) = engine(vec![], true, home.path(), &ws);
    let mut c = engine.connect(Arc::new(|| {}));
    let caps = byob_protocol::version::client_caps().into_iter().filter(|c| c != byob_protocol::version::CAP_BUNDLED_SCOPE).collect();
    c.send(ClientMsg::Hello { protocol: byob_protocol::PROTOCOL_MAJOR, minor: 3, client: "old".into(), caps });
    let e = wait_ext(&mut c, |e| !e.plugins.is_empty());
    assert!(e.plugins.iter().all(|p| p.scope == PluginScope::Global && p.origin == PluginOrigin::Bundled));
    // And as JSON a 13.3 client can read: no "bundled" scope anywhere.
    assert!(!ServerMsg::Extensions(Box::new(e)).to_json().contains("\"scope\":\"bundled\""));
}

#[test]
fn clients_switch_and_fork_bundled_plugins() {
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let ws = ws.path().canonicalize().unwrap();
    let (engine, requests) = engine(vec![Reply::Text("a")], true, home.path(), &ws);
    let (mut c, _) = hello(&engine);
    // A locked plugin can't be switched off.
    c.send(ClientMsg::SetPluginEnabled { scope: ConfigScope::Global, plugin: "artifacts".into(), enabled: Some(false) });
    loop {
        if let ServerMsg::Notice { text, .. } = recv(&mut c) {
            assert!(text.contains("can't be switched off"), "{text}");
            break;
        }
    }
    c.send(ClientMsg::SetPluginEnabled { scope: ConfigScope::Global, plugin: "charts".into(), enabled: Some(false) });
    wait_ext(&mut c, |e| e.plugins.iter().any(|p| p.id == "charts" && !p.enabled));
    turn(&mut c, "hi", |_, _| {});
    let reqs = requests.lock().unwrap().clone();
    assert!(!tool_names(&reqs[0]).contains(&"make_chart".to_owned()) && tool_names(&reqs[0]).contains(&"create_artifact".to_owned()));
    assert!(!system(&reqs[0]).contains("make_chart"));
    // Fork from Settings → Plugins: KeepPlugin on the bundled copy.
    c.send(ClientMsg::KeepPlugin { plugin: "markdown".into(), scope: PluginScope::Global, new_id: None });
    let e = wait_ext(&mut c, |e| e.plugins.iter().any(|p| p.id == "markdown" && p.scope == PluginScope::Global));
    assert!(home.path().join("plugins/markdown/byob-plugin.json").is_file());
    assert!(e.plugins.iter().any(|p| p.id == "markdown" && p.scope == PluginScope::Bundled && p.shadowed_by == Some(PluginScope::Global)));
}
