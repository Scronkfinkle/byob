//! Engine tests of workspaces and layered settings: chats grouped by folder, workspace and
//! chat settings reaching the model, the environment choice reaching `run_command`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

use byob_protocol::workspace::{Approvals, EnvKind, Layer, Level};
use byob_protocol::{ClientMsg, ConfigScope, Connection, Entry, NewFolder, PROTOCOL_VERSION, ServerMsg, StepStatus, WorkspaceSummary};
use byob_store::Paths;
use byob_tools::{Access, Registry};
use byob_types::InferenceConfig;

use crate::loop_tests::{Fake, Reply, call, fake_llm};
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

fn wait<T>(conn: &mut LocalConnection, f: impl Fn(ServerMsg) -> Option<T>) -> T {
    loop {
        if let Some(v) = f(recv(conn)) {
            return v;
        }
    }
}

fn hello(engine: &EngineHandle) -> (LocalConnection, Vec<WorkspaceSummary>) {
    let mut c = engine.connect(Arc::new(|| {}));
    c.send(ClientMsg::Hello { protocol: PROTOCOL_VERSION, client: "test".into() });
    let items = wait(&mut c, |m| if let ServerMsg::Workspaces { items, .. } = m { Some(items) } else { None });
    wait(&mut c, |m| matches!(m, ServerMsg::Artifacts { .. }).then_some(()));
    (c, items)
}

/// Send `text` and wait for the reply entry to finish.
fn turn(c: &mut LocalConnection, text: &str) -> Entry {
    c.send(ClientMsg::SendMessage { text: text.into(), attachments: vec![] });
    let mut entries: Vec<Entry> = Vec::new();
    loop {
        match recv(c) {
            ServerMsg::EntryAdded(e) => entries.push(e),
            ServerMsg::EntryDelta { id, content, reasoning } => {
                if let Some(e) = entries.iter_mut().find(|e| e.id == id) {
                    e.append(&content, &reasoning);
                }
            }
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

fn create(c: &mut LocalConnection, name: &str, folder: &Path, config: Layer) -> String {
    c.send(ClientMsg::CreateWorkspace { name: name.into(), folder: NewFolder::Existing { path: folder.display().to_string() }, config });
    let id = wait(c, |m| if let ServerMsg::WorkspaceCreated { id, error } = m { Some(id.ok_or(error)) } else { None }).unwrap();
    // The creator now looks at a new chat in it.
    let opened = wait(c, |m| if let ServerMsg::ChatOpened { chat: None, workspace } = m { Some(workspace) } else { None });
    assert_eq!(opened.as_deref(), Some(id.as_str()));
    wait(c, |m| matches!(m, ServerMsg::Artifacts { .. }).then_some(()));
    id
}

fn engine(url: String, paths: Option<Paths>, tools: Registry, workspace: &Path) -> EngineHandle {
    let mut config = EngineConfig::new(InferenceConfig { base_url: url, model: "global-model".into(), api_key: Some("sk-global".into()) });
    config.workspace = workspace.to_path_buf();
    config.paths = paths;
    config.tools = tools;
    config.auto_title = false;
    config.detect_local = false;
    config.system_prompt = "You are a test.".into();
    config.defaults = Layer { instructions: Some("GLOBAL-RULE".into()), ..Layer::default() };
    EngineHandle::spawn_with(config).unwrap()
}

fn writer() -> (Registry, Arc<AtomicUsize>) {
    let runs = Arc::new(AtomicUsize::new(0));
    let mut tools = Registry::default();
    tools.register(Arc::new(Fake { name: "writer", access: Access::Write, runs: Arc::clone(&runs), reply: None, change: false }));
    (tools, runs)
}

#[test]
fn chat_and_workspace_settings_reach_the_model() {
    let home = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let folder = folder.path().canonicalize().unwrap();
    let (global_url, global_reqs) = fake_llm(vec![Reply::Text("from global")], "x");
    let (chat_url, chat_reqs) = fake_llm(vec![call("writer", r#"{"text":"hi"}"#), Reply::Text("from the chat's server")], "x");
    let (tools, runs) = writer();
    let engine = engine(global_url, Some(Paths::at(home.path())), tools, home.path());
    let (mut c, _) = hello(&engine);

    let ws_layer = Layer { instructions: Some("WS-RULE".into()), approvals: Some(Approvals::ReadOnly), ..Layer::default() };
    let ws = create(&mut c, "Data", &folder, ws_layer);
    // Settings for the new chat before its first message: its own server and model.
    let chat_layer = Layer { base_url: Some(chat_url.clone()), model: Some("small".into()), instructions: Some("CHAT-RULE".into()), ..Layer::default() };
    c.send(ClientMsg::SetConfig { scope: ConfigScope::Chat, layer: chat_layer, api_key: Some("sk-chat".into()) });
    let view = wait(&mut c, |m| if let ServerMsg::Config(v) = m { Some(v) } else { None });
    assert_eq!((view.effective.model.value.as_str(), view.effective.model.from), ("small", Level::Chat));
    assert_eq!(view.effective.approvals.from, Level::Workspace);
    assert!(view.has_key && view.workspace.as_ref().is_some_and(|w| w.id == ws));

    let reply = turn(&mut c, "write something");
    assert_eq!(reply.content, "from the chat's server");
    // Read only (from the workspace): the write was refused without a card.
    assert_eq!(reply.steps[0].status, StepStatus::Denied);
    assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 0);
    let reqs = chat_reqs.lock().unwrap().clone();
    assert!(global_reqs.lock().unwrap().is_empty(), "the global server wasn't used");
    assert_eq!(reqs[0]["model"], "small");
    let system = reqs[0]["messages"][0]["content"].as_str().unwrap().to_owned();
    let (g, w, ch) = (system.find("GLOBAL-RULE").unwrap(), system.find("WS-RULE").unwrap(), system.find("CHAT-RULE").unwrap());
    assert!(system.starts_with("You are a test.") && g < w && w < ch, "instructions are appended in order: {system}");

    // The chat belongs to the workspace, and its header shows its own endpoint.
    let summary = wait(&mut c, |m| if let ServerMsg::ChatUpdated(s) = m { Some(s) } else { None });
    assert_eq!(summary.workspace.as_deref(), Some(ws.as_str()));
    c.send(ClientMsg::GetConfig { scope: ConfigScope::Chat });
    let view = wait(&mut c, |m| if let ServerMsg::Config(v) = m { Some(v) } else { None });
    assert_eq!(view.own.model.as_deref(), Some("small"));
    let levels: Vec<Level> = view.effective.instructions.iter().map(|(l, _)| *l).collect();
    assert_eq!(levels, [Level::Global, Level::Workspace, Level::Chat]);
    let (c2, _) = hello(&engine);
    drop(c2);

    // Saved where they belong: the key only in secrets.json.
    let ws_file = std::fs::read_to_string(home.path().join("workspaces.json")).unwrap();
    assert!(ws_file.contains("WS-RULE") && ws_file.contains("read_only"), "{ws_file}");
    let chat_file = std::fs::read_to_string(home.path().join("chat-settings.json")).unwrap();
    assert!(chat_file.contains("small") && !chat_file.contains("sk-chat"), "{chat_file}");
    let secrets = std::fs::read_to_string(home.path().join("secrets.json")).unwrap();
    assert!(secrets.contains("sk-chat"));
    assert!(!folder.join(".byob").join("workspaces.json").exists() && std::fs::read_dir(&folder).unwrap().count() == 0, "nothing written into the folder");

    // Resetting the chat's endpoint goes back to the global server (its key stays there).
    c.send(ClientMsg::SetConfig { scope: ConfigScope::Chat, layer: Layer::default(), api_key: None });
    let view = wait(&mut c, |m| if let ServerMsg::Config(v) = m { Some(v) } else { None });
    assert_eq!((view.effective.base_url.from, view.effective.key_from, view.has_key), (Level::Global, Some(Level::Global), false));
    assert!(!std::fs::read_to_string(home.path().join("secrets.json")).unwrap().contains("sk-chat"), "the chat's key went with its URL");
}

#[test]
fn chats_group_by_their_folder_and_workspaces_can_be_renamed_and_removed() {
    let home = tempfile::tempdir().unwrap();
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let paths = Paths::at(home.path());
    {
        // Chats from before workspaces existed, in two folders.
        let store = byob_store::Store::open(paths.db()).unwrap();
        store.create_chat("c1", "one", a.path()).unwrap();
        store.create_chat("c2", "two", a.path()).unwrap();
        store.create_chat("c3", "three", b.path()).unwrap();
    }
    let (url, _) = fake_llm(vec![Reply::Text("ok")], "x");
    let engine = engine(url, Some(paths), Registry::default(), a.path());
    let (mut c, items) = hello(&engine);
    let count = |items: &[WorkspaceSummary], folder: &Path| items.iter().find(|w| Path::new(&w.folder) == folder).map(|w| (w.chats, w.name.clone()));
    let name_a = a.path().file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(count(&items, a.path()), Some((2, name_a)));
    assert_eq!(count(&items, b.path()).map(|(n, _)| n), Some(1));
    let wb = items.iter().find(|w| Path::new(&w.folder) == b.path()).unwrap().id.clone();

    c.send(ClientMsg::RenameWorkspace { id: wb.clone(), name: "Bee".into() });
    let items = wait(&mut c, |m| if let ServerMsg::Workspaces { items, .. } = m { Some(items) } else { None });
    assert!(items.iter().any(|w| w.id == wb && w.name == "Bee"));

    // Problems are refused clearly.
    c.send(ClientMsg::CreateWorkspace { name: "x".into(), folder: NewFolder::Existing { path: "relative".into() }, config: Layer::default() });
    let err = wait(&mut c, |m| if let ServerMsg::WorkspaceCreated { error, .. } = m { Some(error) } else { None });
    assert!(err.is_some_and(|e| e.contains("full path")));
    c.send(ClientMsg::CreateWorkspace { name: "again".into(), folder: NewFolder::Existing { path: b.path().display().to_string() }, config: Layer::default() });
    let err = wait(&mut c, |m| if let ServerMsg::WorkspaceCreated { error, .. } = m { Some(error) } else { None });
    assert!(err.is_some_and(|e| e.contains("already")));
    let bad = Layer { env: Some(EnvKind::Nix), ..Layer::default() };
    c.send(ClientMsg::SetConfig { scope: ConfigScope::Chat, layer: bad, api_key: None });
    let notice = wait(&mut c, |m| if let ServerMsg::Notice { text, .. } = m { Some(text) } else { None });
    assert!(notice.contains("can only run"), "{notice}");
    c.send(ClientMsg::NewChat { workspace: Some("w-nope".into()) });
    let notice = wait(&mut c, |m| if let ServerMsg::Notice { text, .. } = m { Some(text) } else { None });
    assert!(notice.contains("no longer exists"));

    // A new chat in a workspace picked by id.
    c.send(ClientMsg::NewChat { workspace: Some(wb.clone()) });
    let opened = wait(&mut c, |m| if let ServerMsg::ChatOpened { chat: None, workspace } = m { Some(workspace) } else { None });
    assert_eq!(opened.as_deref(), Some(wb.as_str()));

    // Removing it deletes its chats from byob, never the folder.
    std::fs::write(b.path().join("keep.txt"), "mine").unwrap();
    c.send(ClientMsg::RemoveWorkspace { id: wb.clone(), clean: false });
    let deleted = wait(&mut c, |m| if let ServerMsg::ChatDeleted { id } = m { Some(id) } else { None });
    assert_eq!(deleted, "c3");
    let items = wait(&mut c, |m| if let ServerMsg::Workspaces { items, .. } = m { Some(items) } else { None });
    assert!(!items.iter().any(|w| w.id == wb));
    assert_eq!(std::fs::read_to_string(b.path().join("keep.txt")).unwrap(), "mine");

    // Folder browsing on the engine's machine: subfolders only.
    std::fs::create_dir(a.path().join("sub")).unwrap();
    c.send(ClientMsg::ListFolders { path: Some(a.path().display().to_string()) });
    let list = wait(&mut c, |m| if let ServerMsg::Folders(l) = m { Some(l) } else { None });
    assert_eq!(list.dirs, ["sub"]);
}

/// A fake `docker` whose daemon is down; it logs its arguments.
#[cfg(unix)]
fn fake_docker(dir: &Path) -> (byob_exec::env::container::Runtime, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let docker = dir.join("bin/docker");
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::write(&docker, "#!/bin/sh\necho \"$@\" >> \"$0.log\"\necho 'Cannot connect to the Docker daemon at unix:///var/run/docker.sock.' >&2\nexit 1\n")
        .unwrap();
    std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
    (byob_exec::env::container::Runtime { kind: byob_exec::env::container::RuntimeKind::Docker, program: docker.clone() }, docker.with_extension("log"))
}

#[cfg(unix)]
#[test]
fn a_container_workspace_runs_commands_in_its_container_and_a_chat_can_opt_out() {
    use byob_exec::env::{Envs, Found, ProjectMode};
    let home = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let folder = folder.path().canonicalize().unwrap();
    let data = home.path().join("data");
    let (rt, log) = fake_docker(home.path());
    let envs = Arc::new(Envs::new(Some(&data), Found { containers: Some(rt), ..Found::default() }, ProjectMode::Ask));
    let mut tools = Registry::default();
    byob_exec::register(&mut tools, byob_exec::Exec { mode: byob_exec::Mode::Unsandboxed, data_dir: Some(data.clone()), envs: Arc::clone(&envs) });
    let run = || call("run_command", r#"{"command":"echo ran-on-host"}"#);
    let script = vec![run(), Reply::Text("done"), run(), Reply::Text("done")];
    let (url, _) = fake_llm(script, "x");
    let mut config = EngineConfig::new(InferenceConfig { base_url: url, ..Default::default() });
    config.workspace = home.path().to_path_buf();
    config.tools = tools;
    config.envs = Some(envs.clone());
    config.auto_title = false;
    config.detect_local = false;
    let engine = EngineHandle::spawn_with(config).unwrap();
    let (mut c, _) = hello(&engine);

    // Chosen when creating the workspace: Container, and no approval cards.
    let layer = Layer { env: Some(EnvKind::Container), approvals: Some(Approvals::AllowAll), ..Layer::default() };
    let ws = create(&mut c, "Boxed", &folder, layer);
    let reply = turn(&mut c, "run it");
    let step = &reply.steps[0];
    assert_eq!(step.status, StepStatus::Failed, "{step:?}");
    assert!(step.detail.contains("Docker isn't running"), "{}", step.detail);
    assert!(std::fs::read_to_string(&log).unwrap().contains("info"), "run_command went to docker");
    // The choice is the folder's answer too (no ask-once question later).
    c.send(ClientMsg::WorkspaceEnv { action: None, workspace: Some(ws.clone()) });
    let env = wait(&mut c, |m| if let ServerMsg::WorkspaceEnv(e) = m { Some(e) } else { None });
    assert_eq!(env.container.use_container, Some(true));
    let mut answered = false;
    for _ in 0..100 {
        answered = std::fs::read_to_string(data.join("envs/workspaces.json")).is_ok_and(|s| s.contains("\"container\": true"));
        if answered {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(answered, "recorded as the consent");

    // This chat runs on this machine instead.
    std::fs::remove_file(&log).unwrap();
    c.send(ClientMsg::SetConfig { scope: ConfigScope::Chat, layer: Layer { env: Some(EnvKind::Host), ..Layer::default() }, api_key: None });
    let view = wait(&mut c, |m| if let ServerMsg::Config(v) = m { Some(v) } else { None });
    assert_eq!((view.effective.env.value, view.effective.env.from), (EnvKind::Host, Level::Chat));
    let reply = turn(&mut c, "run it here");
    let step = reply.steps.iter().find(|s| s.tool == "run_command").unwrap();
    assert_eq!(step.status, StepStatus::Done, "{step:?}");
    assert!(step.detail.contains("ran-on-host"), "{}", step.detail);
    assert!(!log.exists(), "docker wasn't called");
}
