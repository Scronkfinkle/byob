//! Engine tests of model profiles: saving them (keys only in secrets.json, never to
//! clients), levels picking one, deleting one in use, roles, the old connection form, and
//! workspaces and chats with their own endpoint becoming profiles at startup.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use byob_protocol::profiles::{Profile, ProfileList};
use byob_protocol::workspace::{Layer, Level};
use byob_protocol::{ClientMsg, ConfigScope, ConfigView, Connection, NewFolder, ServerMsg};
use byob_store::{Paths, Secrets, Settings, Workspaces};
use byob_types::InferenceConfig;

use crate::loop_tests::{Reply, fake_llm};
use crate::{EngineConfig, EngineHandle, LocalConnection};

fn recv(conn: &mut LocalConnection) -> ServerMsg {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(m) = conn.try_recv() {
            return m;
        }
        assert!(Instant::now() < deadline, "timed out waiting for engine");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn wait<T>(conn: &mut LocalConnection, mut f: impl FnMut(ServerMsg) -> Option<T>) -> T {
    loop {
        if let Some(v) = f(recv(conn)) {
            return v;
        }
    }
}

fn profiles_where(c: &mut LocalConnection, f: impl Fn(&ProfileList) -> bool) -> ProfileList {
    wait(c, |m| match m {
        ServerMsg::Profiles(l) if f(&l) => Some(*l),
        ServerMsg::Notice { text, .. } => panic!("notice: {text}"),
        _ => None,
    })
}

fn config_view(c: &mut LocalConnection) -> ConfigView {
    wait(c, |m| if let ServerMsg::Config(v) = m { Some(*v) } else { None })
}

fn engine(url: &str, paths: &Paths, workspace: &Path) -> EngineHandle {
    let mut config = EngineConfig::new(InferenceConfig { base_url: url.into(), model: "global-model".into(), api_key: None, api: None, ..Default::default() });
    config.workspace = workspace.to_path_buf();
    config.paths = Some(paths.clone());
    config.auto_title = false;
    config.detect_local = false;
    let s = Settings::load(&paths.settings()).value;
    let k = Secrets::load(&paths.secrets()).value;
    config.profiles = s.profiles;
    config.roles = s.roles;
    config.profile_keys = k.profile_keys;
    config.scoped_keys = k.scoped_keys;
    EngineHandle::spawn_with(config).unwrap()
}

fn hello(engine: &EngineHandle) -> (LocalConnection, ProfileList) {
    let mut c = engine.connect(Arc::new(|| {}));
    c.send(ClientMsg::hello("test"));
    let list = profiles_where(&mut c, |_| true);
    wait(&mut c, |m| matches!(m, ServerMsg::Artifacts { .. }).then_some(()));
    (c, list)
}

fn new_profile(name: &str, url: &str, model: &str) -> Profile {
    Profile { name: name.into(), base_url: url.into(), model: model.into(), description: "for tests".into(), ..Profile::default() }
}

fn create(c: &mut LocalConnection, p: Profile, key: Option<&str>) -> (String, ProfileList) {
    let name = p.name.clone();
    c.send(ClientMsg::CreateProfile { profile: p, api_key: key.map(Into::into), assign: None, make_main: false });
    let list = profiles_where(c, |l| l.items.iter().any(|v| v.profile.name == name));
    (list.items.iter().find(|v| v.profile.name == name).map(|v| v.profile.id.clone()).unwrap(), list)
}

#[test]
fn profiles_are_saved_without_their_keys_reaching_settings_or_clients() {
    let home = tempfile::tempdir().unwrap();
    let paths = Paths::at(home.path());
    let e = engine("http://127.0.0.1:9/v1", &paths, home.path());
    let (mut c, first) = hello(&e);
    assert!(first.items.is_empty());
    let (id, list) = create(&mut c, new_profile("Big one", "https://api.example.com/v1", "big"), Some("sk-secret-1"));
    assert_eq!(id, "big-one");
    let v = list.get(&id).unwrap();
    assert!(v.has_key);
    assert_eq!(v.profile.description, "for tests");
    // The first model becomes the default one: the global endpoint follows it.
    assert_eq!(list.roles.get("main"), Some(&id));
    let s = Settings::load(&paths.settings()).value;
    assert_eq!((s.inference.base_url.as_deref(), s.inference.model.as_deref()), (Some("https://api.example.com/v1"), Some("big")));
    assert_eq!(s.profiles[&id].name, "Big one");
    let k = Secrets::load(&paths.secrets()).value;
    assert_eq!((k.profile_keys.get(&id).map(String::as_str), k.api_key.as_deref()), (Some("sk-secret-1"), Some("sk-secret-1")));
    assert!(!std::fs::read_to_string(paths.settings()).unwrap().contains("sk-secret"));

    // Duplicate, rename the copy, make it a role; every message stays free of keys.
    c.send(ClientMsg::DuplicateProfile { id: id.clone() });
    let list = profiles_where(&mut c, |l| l.items.len() == 2);
    let copy = list.items.iter().find(|v| v.profile.id != id).unwrap().clone();
    assert_eq!((copy.profile.name.as_str(), copy.has_key), ("Big one (copy)", true));
    let mut renamed = copy.profile.clone();
    renamed.name = "Vision".into();
    renamed.base_url = "http://elsewhere:1/v1".into();
    c.send(ClientMsg::UpdateProfile { profile: renamed, api_key: None });
    let list = profiles_where(&mut c, |l| l.get(&copy.profile.id).is_some_and(|v| v.profile.name == "Vision"));
    assert!(!list.get(&copy.profile.id).unwrap().has_key, "a key doesn't follow its profile to another server");
    c.send(ClientMsg::SetRole { role: "vision".into(), profile: Some(copy.profile.id.clone()) });
    let mut all = Vec::new();
    let list = wait(&mut c, |m| {
        all.push(m.to_json());
        match m {
            ServerMsg::Profiles(l) if l.roles.contains_key("vision") => Some(*l),
            _ => None,
        }
    });
    assert_eq!(list.roles_of(&copy.profile.id), ["vision"]);
    assert!(all.iter().all(|m| !m.contains("sk-secret")), "keys never reach clients");
    let s = Settings::load(&paths.settings()).value;
    assert_eq!(s.models["vision"].base_url.as_deref(), Some("http://elsewhere:1/v1"), "older builds see the role");
    // The default model can't be deleted; another one can.
    c.send(ClientMsg::DeleteProfile { id: id.clone() });
    assert!(wait(&mut c, |m| if let ServerMsg::Notice { text, .. } = m { Some(text) } else { None }).contains("default"));
    c.send(ClientMsg::DeleteProfile { id: copy.profile.id.clone() });
    let list = profiles_where(&mut c, |l| l.items.len() == 1);
    assert!(!list.roles.contains_key("vision"));
    let s = Settings::load(&paths.settings()).value;
    assert!(!s.models.contains_key("vision") && !s.roles.contains_key("vision"));
}

#[test]
fn a_level_picks_a_profile_and_a_deleted_one_falls_back() {
    let home = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let folder = folder.path().canonicalize().unwrap();
    let paths = Paths::at(home.path());
    let (global_url, global_reqs) = fake_llm(vec![Reply::Text("from global")], "x");
    let (other_url, other_reqs) = fake_llm(vec![Reply::Text("from the profile")], "x");
    let e = engine(&global_url, &paths, home.path());
    let (mut c, _) = hello(&e);
    // The first model saved becomes the default (the global endpoint is it already).
    let (main, _) = create(&mut c, new_profile("Main", &global_url, "global-model"), None);
    let (id, list) = create(&mut c, new_profile("Other", &other_url, "small"), Some("sk-other"));
    assert_eq!(list.role("main").map(|v| v.profile.id.clone()), Some(main));
    c.send(ClientMsg::CreateWorkspace { name: "Data".into(), folder: NewFolder::Existing { path: folder.display().to_string() }, config: Layer::default() });
    let ws = wait(&mut c, |m| if let ServerMsg::WorkspaceCreated { id, .. } = m { id } else { None });
    c.send(ClientMsg::SetLevelProfile { scope: ConfigScope::Workspace { id: ws.clone() }, profile: Some(id.clone()) });
    let view = config_view(&mut c);
    assert_eq!(view.own.profile.as_deref(), Some(id.as_str()));
    // The level keeps a copy of the endpoint, and its key, for older builds.
    assert_eq!((view.own.base_url.as_deref(), view.own.model.as_deref()), (Some(other_url.as_str()), Some("small")));
    assert!(view.has_key);
    assert_eq!((view.effective.base_url.value.as_str(), view.effective.base_url.from), (other_url.as_str(), Level::Workspace));
    assert_eq!(Secrets::load(&paths.secrets()).value.scoped_keys.get(&format!("workspace:{ws}")).map(String::as_str), Some("sk-other"));

    // A chat in the workspace talks to the profile's server.
    c.send(ClientMsg::NewChat { workspace: Some(ws.clone()) });
    let st = wait(&mut c, |m| if let ServerMsg::State(s) = m { s.chat_endpoint.filter(|e| e.profile.is_some()) } else { None });
    assert_eq!(st.profile.as_deref(), Some(id.as_str()));
    c.send(ClientMsg::SendMessage { text: "hi".into(), attachments: vec![] });
    wait(&mut c, |m| matches!(m, ServerMsg::EntryFinished { .. }).then_some(()));
    assert_eq!(other_reqs.lock().unwrap().len(), 1);
    assert_eq!(other_reqs.lock().unwrap()[0]["model"], "small");
    assert!(global_reqs.lock().unwrap().is_empty());

    // Editing the profile moves the workspace's copy along.
    let mut edited = new_profile("Other", &other_url, "smaller");
    edited.id.clone_from(&id);
    c.send(ClientMsg::UpdateProfile { profile: edited, api_key: None });
    profiles_where(&mut c, |l| l.get(&id).is_some_and(|v| v.profile.model == "smaller"));
    let mut rows = Workspaces::open(Some(&paths)).value;
    assert_eq!(rows.get(&ws).unwrap().config.model.as_deref(), Some("smaller"));

    // Deleted: the workspace inherits the global model again, its copies and key go.
    c.send(ClientMsg::DeleteProfile { id: id.clone() });
    profiles_where(&mut c, |l| l.get(&id).is_none());
    c.send(ClientMsg::GetConfig { scope: ConfigScope::Workspace { id: ws.clone() } });
    let view = config_view(&mut c);
    assert_eq!((view.own.profile.clone(), view.own.base_url.clone(), view.has_key), (None, None, false));
    assert_eq!(view.effective.base_url.from, Level::Global);
    rows = Workspaces::open(Some(&paths)).value;
    assert!(rows.get(&ws).unwrap().config.profile.is_none());
}

#[test]
fn a_profile_deleted_behind_the_engines_back_never_crashes() {
    let home = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let folder = folder.path().canonicalize().unwrap();
    let paths = Paths::at(home.path());
    paths.ensure().unwrap();
    // A workspace (written by hand, or by a newer build) names a profile that isn't there.
    let mut ws = Workspaces::open(Some(&paths)).value;
    let layer = Layer { profile: Some("gone".into()), base_url: Some("http://stale:1/v1".into()), model: Some("stale".into()), ..Layer::default() };
    let row = ws.create("Data", &folder, layer).unwrap();
    drop(ws);
    let e = engine("http://127.0.0.1:9/v1", &paths, &folder);
    let (mut c, _) = hello(&e);
    c.send(ClientMsg::GetConfig { scope: ConfigScope::Workspace { id: row.id.clone() } });
    let view = config_view(&mut c);
    assert_eq!((view.effective.base_url.value.as_str(), view.effective.base_url.from), ("http://127.0.0.1:9/v1", Level::Global));
    assert_eq!(view.effective.model.value, "global-model");
    // Picking it again is refused with a notice.
    c.send(ClientMsg::SetLevelProfile { scope: ConfigScope::Chat, profile: Some("gone".into()) });
    assert!(wait(&mut c, |m| if let ServerMsg::Notice { text, .. } = m { Some(text) } else { None }).contains("no longer exists"));
}

#[test]
fn own_endpoints_of_workspaces_and_chats_become_profiles_once() {
    let home = tempfile::tempdir().unwrap();
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let paths = Paths::at(home.path());
    paths.ensure().unwrap();
    let own = |url: &str, model: &str| Layer { base_url: Some(url.into()), model: Some(model.into()), ..Layer::default() };
    let mut ws = Workspaces::open(Some(&paths)).value;
    let wa = ws.create("A", &a.path().canonicalize().unwrap(), own("https://api.example.com/v1", "gpt")).unwrap();
    let wb = ws.create("B", &b.path().canonicalize().unwrap(), own("https://api.example.com/v1", "gpt")).unwrap();
    ws.set_chat_config("c1", own("http://box:8000/v1", "qwen")).unwrap();
    // Only a model: still inherits its URL, stays as it is.
    ws.set_chat_config("c2", Layer { model: Some("tiny".into()), ..Layer::default() }).unwrap();
    drop(ws);
    let secrets = Secrets {
        scoped_keys: [(format!("workspace:{}", wa.id), "sk-a".to_owned()), (format!("workspace:{}", wb.id), "sk-a".to_owned())].into(),
        ..Secrets::default()
    };
    secrets.save(&paths.secrets()).unwrap();
    let e = engine("http://127.0.0.1:9/v1", &paths, home.path());
    let (_c, list) = hello(&e);
    // Two workspaces with the same endpoint and key share one profile; the chat has its own.
    assert_eq!(list.items.len(), 2, "{list:?}");
    let ws = Workspaces::open(Some(&paths)).value;
    let (pa, pb) = (ws.get(&wa.id).unwrap().config.profile.clone(), ws.get(&wb.id).unwrap().config.profile.clone());
    assert!(pa.is_some() && pa == pb);
    let pa = pa.unwrap();
    assert_eq!(Secrets::load(&paths.secrets()).value.profile_keys.get(&pa).map(String::as_str), Some("sk-a"));
    assert!(ws.chat_config("c1").unwrap().profile.is_some());
    assert!(ws.chat_config("c2").unwrap().profile.is_none());
    assert_eq!(ws.get(&wa.id).unwrap().config.base_url.as_deref(), Some("https://api.example.com/v1"), "the copy stays for older builds");
    drop(e);
    // A second start changes nothing.
    let before = (std::fs::read_to_string(paths.settings()).unwrap(), std::fs::read_to_string(paths.workspaces()).unwrap());
    let e = engine("http://127.0.0.1:9/v1", &paths, home.path());
    let (_c, list) = hello(&e);
    assert_eq!(list.items.len(), 2);
    assert_eq!(before, (std::fs::read_to_string(paths.settings()).unwrap(), std::fs::read_to_string(paths.workspaces()).unwrap()));
}

#[test]
fn the_connection_form_edits_the_main_profile() {
    let home = tempfile::tempdir().unwrap();
    let paths = Paths::at(home.path());
    let e = engine("http://127.0.0.1:9/v1", &paths, home.path());
    let (mut c, _) = hello(&e);
    c.send(ClientMsg::SetInference { base_url: "http://gpu:8000/v1".into(), model: "qwen".into(), api_key: Some("sk-1".into()) });
    let list = profiles_where(&mut c, |l| l.roles.contains_key("main"));
    let main = list.role("main").unwrap().clone();
    assert_eq!((main.profile.model.as_str(), main.has_key), ("qwen", true));
    // Removing the key edits that profile rather than making another.
    c.send(ClientMsg::SetInference { base_url: "http://gpu:8000/v1".into(), model: "qwen".into(), api_key: Some(String::new()) });
    let list = profiles_where(&mut c, |l| l.role("main").is_some_and(|v| !v.has_key));
    assert_eq!(list.items.len(), 1);
    // Another model: another profile, now the default; the old one stays.
    c.send(ClientMsg::SetInference { base_url: "http://gpu:8000/v1".into(), model: "llama".into(), api_key: None });
    let list = profiles_where(&mut c, |l| l.role("main").is_some_and(|v| v.profile.model == "llama"));
    assert_eq!(list.items.len(), 2);
    // Making the first one the default again switches the engine to it.
    c.send(ClientMsg::SetRole { role: "main".into(), profile: Some(main.profile.id.clone()) });
    wait(&mut c, |m| if let ServerMsg::State(s) = m { (s.inference_model == "qwen").then_some(()) } else { None });
    assert_eq!(Settings::load(&paths.settings()).value.inference.model.as_deref(), Some("qwen"));
}
