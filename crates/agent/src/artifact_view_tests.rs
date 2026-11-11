//! Engine tests of artifacts drawn by a plugin's view (`view:` renderers, `Site::Artifact`),
//! artifact versions a backend saves (tool results and drafts), and bundled plugins whose
//! backend runs from their materialized folder. Through a real Python backend; skipped
//! without `python3`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use byob_plugins::Embedded;
use byob_protocol::sites::{LiveView, artifact_key};
use byob_protocol::tree::Event;
use byob_protocol::{Artifact, ClientMsg, Connection, ServerMsg, Site, ViewRef};
use byob_store::Paths;
use byob_types::InferenceConfig;
use serde_json::json;

use crate::backend_tests::{find, have_python, recv, turn};
use crate::extensions::PluginsConfig;
use crate::loop_tests::{Reply, Requests, call, fake_llm};
use crate::{EngineConfig, EngineHandle, LocalConnection, Policy};

const BACKEND: &str = r#"
import os, tempfile
import byob_plugin as byob

def bytes_file(text):
    f = os.path.join(tempfile.mkdtemp(), "x.note")
    with open(f, "w") as out:
        out.write(text)
    return f

@byob.tool
def make_note(text: str = "hello"):
    ctx = byob.context()
    return {"text": f"made it in {ctx.get('chat')} from {os.getcwd()}",
            "artifact": {"title": "Note", "filename": "note.note", "file": bytes_file(text), "note": "first"}}

@byob.view("show")
def show(key):
    a = byob.artifact(key, "show") or {}
    with open(a["path"]) as f:
        body = f.read()
    return {"type": "column", "children": [
        {"type": "text", "text": f"{key}: v{a['version']} of {a['latest']} ({a['content_type']}): {body}"},
        {"type": "input", "name": "t"}]}

@byob.on_event("show")
def typed(e):
    a = e["artifact"]
    byob.save_artifact(bytes_file(e["values"].get("t", "")), a["chat"], id=a["id"], note="edited here", draft=True)

byob.run()
"#;

fn plugin_files() -> BTreeMap<String, String> {
    let manifest = json!({
        "id": "notes", "name": "Notes", "backend": "backend.py",
        "artifact_types": [{"type": "application/x-note", "suffixes": [".note"], "label": "note", "binary": true}],
        "renderers": [{"type": "application/x-note", "renderer": "view:show"}],
        "views": [{"site": "artifact", "id": "show", "title": "Note"}],
        "tools": [
            {"name": "make_note", "description": "Make a note", "run": "backend",
             "params": {"type": "object", "properties": {"text": {"type": "string"}}}}
        ]
    });
    [("byob-plugin.json".to_owned(), manifest.to_string()), ("backend.py".to_owned(), BACKEND.to_owned())].into_iter().collect()
}

struct H {
    engine: EngineHandle,
    _requests: Requests,
    home: tempfile::TempDir,
    ws: tempfile::TempDir,
}

/// An engine whose only bundled plugin is `notes` (inside the "binary": `Embedded`).
fn harness(script: Vec<Reply>) -> H {
    let (url, requests) = fake_llm(script, "final answer");
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
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
        bundled: vec![Embedded { id: "notes".into(), files: plugin_files() }],
        bundled_cache: Some(home.path().join("bundled-plugins")),
        ..Default::default()
    });
    H { engine: EngineHandle::spawn_with(config).unwrap(), _requests: requests, home, ws }
}

fn hello(h: &H) -> LocalConnection {
    let mut c = h.engine.connect(Arc::new(|| {}));
    c.send(ClientMsg::hello("test"));
    c
}

fn live(c: &mut LocalConnection, r: &ViewRef) -> LiveView {
    loop {
        match recv(c) {
            ServerMsg::LiveView(lv) if lv.view == *r => return lv,
            _ => {}
        }
    }
}

fn updated(c: &mut LocalConnection) -> Artifact {
    loop {
        if let ServerMsg::ArtifactUpdated(a) = recv(c) {
            return a;
        }
    }
}

fn shown(lv: &LiveView) -> String {
    assert_eq!(lv.error, None, "{lv:?}");
    serde_json::to_string(lv.tree.as_ref().expect("a tree")).unwrap()
}

fn typed(text: &str) -> Event {
    Event { action: "save".into(), values: json!({"t": text}).as_object().cloned().unwrap_or_default(), ..Default::default() }
}

fn materialized(home: &Path) -> PathBuf {
    let root = home.join("bundled-plugins");
    let dirs: Vec<PathBuf> =
        std::fs::read_dir(&root).unwrap().flatten().map(|e| e.path()).filter(|p| p.join(byob_plugins::materialize::MARKER).is_file()).collect();
    assert_eq!(dirs.len(), 1, "{dirs:?}");
    dirs[0].clone()
}

#[test]
fn a_bundled_backend_runs_from_its_materialized_folder_and_draws_artifacts() {
    if !have_python() {
        return;
    }
    let h = harness(vec![call("make_note", r#"{"text": "hello"}"#), Reply::Text("made"), Reply::Text("ok")]);
    let mut c = hello(&h);
    // The plugin list: bundled, its view at the artifact site, its renderer, no problems.
    let ext = loop {
        if let ServerMsg::Extensions(e) = recv(&mut c) {
            break e;
        }
    };
    let p = find(&ext, "notes");
    assert!(p.problems.is_empty(), "{:?}", p.problems);
    assert_eq!(p.views.iter().map(|v| v.site).collect::<Vec<_>>(), [Site::Artifact]);
    assert_eq!(p.renderers[0].renderer, "view:show");
    let dir = materialized(h.home.path());
    assert_eq!(Path::new(&p.dir), dir, "its folder is the materialized one");
    assert!(dir.file_name().unwrap().to_string_lossy().starts_with(&format!("notes-{}-", env!("CARGO_PKG_VERSION"))));
    assert!(std::fs::metadata(dir.join("backend.py")).unwrap().permissions().readonly());

    // The model's tool saves a version from the backend's bytes.
    let entry = turn(&mut c, "make a note");
    let step = entry.steps.iter().find(|s| s.tool == "make_note").unwrap();
    assert!(step.detail.contains("made it in") && step.detail.contains("Saved: artifact"), "{}", step.detail);
    assert!(step.detail.contains(&dir.display().to_string()), "the backend runs in its folder: {}", step.detail);
    assert_eq!(step.artifacts.len(), 1, "{step:?}");
    let id = step.artifacts[0].id.clone();
    let meta = byob_tools::artifact::load(h.ws.path(), &id).unwrap();
    assert_eq!((meta.artifact.content_type.as_str(), meta.artifact.filename.as_str()), ("application/x-note", "note.note"));

    // Its view: the backend gets the version's file.
    let v1 = ViewRef::new("notes", "show", artifact_key(&id, 1));
    c.send(ClientMsg::OpenView { view: v1.clone() });
    let lv = live(&mut c, &v1);
    assert!(shown(&lv).contains(&format!("{id}@1: v1 of 1 (application/x-note): hello")), "{}", shown(&lv));
    // A key that isn't an artifact of this chat never reaches the backend.
    let bad = ViewRef::new("notes", "show", "nope@1");
    c.send(ClientMsg::OpenView { view: bad.clone() });
    assert!(live(&mut c, &bad).error.unwrap().contains("no artifact"));

    // The user's edits: a draft version, replaced while they keep editing.
    c.send(ClientMsg::ViewEvent { view: v1.clone(), event: typed("draft one") });
    let a = updated(&mut c);
    assert_eq!(a.latest().map(|v| v.n), Some(2));
    c.send(ClientMsg::ViewEvent { view: v1.clone(), event: typed("draft two") });
    let a = updated(&mut c);
    assert_eq!(a.versions.iter().map(|v| v.n).collect::<Vec<_>>(), [1, 2], "the draft was replaced");
    let v2 = ViewRef::new("notes", "show", artifact_key(&id, 2));
    c.send(ClientMsg::OpenView { view: v2.clone() });
    assert!(shown(&live(&mut c, &v2)).contains("v2 of 2 (application/x-note): draft two"));
    // A turn freezes it: the next edit is a new version.
    turn(&mut c, "thanks");
    c.send(ClientMsg::ViewEvent { view: v2.clone(), event: typed("after the turn") });
    let a = updated(&mut c);
    assert_eq!(a.versions.iter().map(|v| v.n).collect::<Vec<_>>(), [1, 2, 3]);
    let text = std::fs::read_to_string(h.ws.path().join(&a.version(3).unwrap().path)).unwrap();
    assert_eq!(text, "after the turn");
}

#[test]
fn views_at_the_artifact_site_need_a_matching_renderer_view() {
    let bad = json!({"id": "x", "name": "X", "backend": "b.py", "renderers": [{"type": "text/csv", "renderer": "view:grid"}]});
    let e = byob_plugins::manifest::parse_manifest_str(&bad.to_string(), Default::default()).unwrap_err().to_string();
    assert!(e.contains("needs a view"), "{e}");
    let good = json!({"id": "x", "name": "X", "backend": "b.py", "views": [{"site": "artifact", "id": "grid"}],
        "renderers": [{"type": "text/csv", "renderer": "view:grid"}], "artifact_types": [{"type": "text/csv", "suffixes": [".csv"], "binary": true}]});
    let m = byob_plugins::manifest::parse_manifest_str(&good.to_string(), Default::default()).unwrap();
    assert!(m.artifact_types[0].binary);
}
