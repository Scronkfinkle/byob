//! Workspace containers against a real Docker daemon: the default image, a devcontainer.json
//! (image, containerEnv, postCreateCommand, ignored features) and a build.dockerfile.
//! Every test is skipped unless `docker info` works (CI runners on macOS and Windows have
//! no daemon). They pull only small images (python:3-slim, debian:stable-slim) and remove
//! the containers they create, also when they fail.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use byob_exec::env::container::{Runtime, RuntimeKind, container_name};
use byob_exec::env::{Envs, Found, ProjectMode};
use byob_exec::{Exec, Mode, register};
use byob_protocol::EnvAction;
use byob_tools::{Answer, Asker, EnvControl, Question, Registry, ToolContext, ToolOutput};
use serde_json::json;

fn docker() -> Option<Runtime> {
    let program = byob_sandbox::which("docker".as_ref())?;
    let ok = Command::new(&program).args(["info", "--format", "{{.ServerVersion}}"]).output().is_ok_and(|o| o.status.success());
    if !ok {
        println!("skipped: no working docker daemon");
        return None;
    }
    Some(Runtime { kind: RuntimeKind::Docker, program })
}

struct Yes(AtomicUsize);

impl Asker for Yes {
    fn ask(&self, q: &Question) -> Answer {
        println!("  asked: {}", q.title);
        self.0.fetch_add(1, Ordering::SeqCst);
        Answer::Yes
    }
}

/// A workspace whose commands run in a container; removes the container when dropped.
struct World {
    data: tempfile::TempDir,
    ws: PathBuf,
    _ws_dir: tempfile::TempDir,
    rt: Runtime,
    envs: Arc<Envs>,
    reg: Registry,
    asker: Yes,
}

impl World {
    fn new(rt: Runtime, image: &str) -> Self {
        let data = tempfile::tempdir().unwrap();
        let ws_dir = tempfile::tempdir().unwrap();
        let ws = ws_dir.path().canonicalize().unwrap();
        let (envs, reg) = Self::exec(data.path(), &rt, image);
        Self { data, ws, _ws_dir: ws_dir, rt, envs, reg, asker: Yes(AtomicUsize::new(0)) }
    }

    fn exec(data: &Path, rt: &Runtime, image: &str) -> (Arc<Envs>, Registry) {
        let found = Found { containers: Some(rt.clone()), container_image: Some(image.into()), ..Default::default() };
        let envs = Arc::new(Envs::new(Some(data), found, ProjectMode::Ask));
        let mut reg = Registry::default();
        register(&mut reg, Exec { mode: Mode::Unsandboxed, data_dir: Some(data.to_path_buf()), envs: Arc::clone(&envs) });
        (envs, reg)
    }

    /// Like a byob restart: new engine state, same data folder.
    fn restart(&mut self, image: &str) {
        (self.envs, self.reg) = Self::exec(self.data.path(), &self.rt, image);
    }

    fn call(&self, tool: &str, args: serde_json::Value) -> ToolOutput {
        let cancel = AtomicBool::new(false);
        let ctx = ToolContext::new(&self.ws, &cancel).with_asker(&self.asker);
        let out = self.reg.get(tool).unwrap_or_else(|| panic!("no tool {tool}")).run(&args, &ctx);
        println!("  {tool} {args}\n    {}", out.text.replace('\n', "\n    "));
        out
    }

    fn container_id(&self) -> String {
        let out = Command::new(&self.rt.program).args(["container", "inspect", "--format", "{{.Id}}", &container_name(&self.ws)]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = Command::new(&self.rt.program).args(["rm", "-f", &container_name(&self.ws)]).output();
    }
}

#[cfg(unix)]
fn my_uid(p: &Path) -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).unwrap().uid()
}

#[cfg(unix)]
#[test]
fn default_image_runs_as_the_user_with_network_per_call_and_survives_restarts() {
    let Some(rt) = docker() else { return };
    let mut w = World::new(rt, "python:3-slim");
    // No devcontainer.json: only the setting turns it on.
    let out = w.call("run_command", json!({"command": "echo on-the-host"}));
    assert!(!out.text.contains("container"), "{}", out.text);
    assert_eq!(w.envs.describe(&w.ws, &byob_tools::EnvPlan::default()).container.state, None);
    w.envs.apply(&w.ws, &EnvAction::Container { on: Some(true) }).unwrap();

    let out = w.call("run_command", json!({"command": "id -u; pwd; touch made.txt; python3 --version", "timeout_s": 600}));
    assert!(!out.failed, "{}", out.text);
    assert!(out.text.contains("Commands run inside this workspace's docker container"), "{}", out.text);
    let uid = my_uid(w.data.path());
    assert!(out.text.contains(&format!("\n{uid}\n{}\n", w.ws.display())), "runs as the user in the same path: {}", out.text);
    assert!(out.text.contains("Python 3") && out.text.contains("Files created: made.txt"), "{}", out.text);
    assert_eq!(my_uid(&w.ws.join("made.txt")), uid, "not owned by root");
    let id = w.container_id();
    assert_eq!(w.envs.describe(&w.ws, &byob_tools::EnvPlan::default()).container.state.as_deref(), Some("running"));

    // Reused, and quiet the second time.
    let out = w.call("run_python", json!({"code": "import sys; print('py', sys.version_info[0])"}));
    assert!(!out.failed && out.text.contains("py 3") && !out.text.contains("Commands run inside"), "{}", out.text);
    assert_eq!(w.container_id(), id);

    // No network unless the call asks.
    let probe = "import socket\ntry:\n    socket.create_connection(('1.1.1.1', 53), timeout=5); print('online')\nexcept OSError as e:\n    print('offline', e)";
    let out = w.call("run_python", json!({"code": probe}));
    assert!(out.text.contains("offline"), "{}", out.text);
    let out = w.call("run_python", json!({"code": probe, "network": true}));
    assert!(out.text.contains("online"), "{}", out.text);
    let out = w.call("run_python", json!({"code": probe}));
    assert!(out.text.contains("offline"), "disconnected again: {}", out.text);

    // A timeout stops the command inside the container too.
    let out = w.call("run_command", json!({"command": "sleep 300 & sleep 301", "timeout_s": 2}));
    assert!(out.failed && out.text.contains("Timed out"), "{}", out.text);
    let out = w.call("run_command", json!({"command": "cat /proc/[0-9]*/cmdline | tr '\\0' ' ' | grep -c 'sleep 30[01]' || true"}));
    assert!(out.text.contains("\n0"), "nothing left running: {}", out.text);

    // install_packages uses apt-get there; a missing program gets the hint first.
    let out = w.call("run_command", json!({"command": "jq --version"}));
    assert!(out.failed && out.text.contains("install_packages"), "{}", out.text);
    let out = w.call("install_packages", json!({"packages": ["jq", "nosuchpkg-xyz"]}));
    assert!(out.text.contains("Installed in the container with apt-get: jq"), "{}", out.text);
    assert!(out.failed && out.text.contains("\"nosuchpkg-xyz\""), "{}", out.text);
    let out = w.call("run_command", json!({"command": "echo '{\"a\": 2}' | jq .a"}));
    assert!(!out.failed && out.text.contains("\n2"), "{}", out.text);

    // After a restart: the same container, jq still installed.
    w.restart("python:3-slim");
    let out = w.call("run_command", json!({"command": "jq --version"}));
    assert!(!out.failed && out.text.contains("jq-"), "{}", out.text);
    assert_eq!(w.container_id(), id, "reused across restarts");

    // Stop and remove from the settings.
    w.envs.apply(&w.ws, &EnvAction::StopContainer).unwrap();
    assert_eq!(w.envs.describe(&w.ws, &byob_tools::EnvPlan::default()).container.state.as_deref(), Some("exited"));
    let out = w.call("run_command", json!({"command": "echo back"}));
    assert!(!out.failed && out.text.contains("back"), "started again: {}", out.text);
    w.envs.apply(&w.ws, &EnvAction::RemoveContainer).unwrap();
    assert_eq!(w.envs.describe(&w.ws, &byob_tools::EnvPlan::default()).container.state, None);
}

#[cfg(unix)]
#[test]
fn devcontainer_json_is_asked_once_and_honoured() {
    let Some(rt) = docker() else { return };
    let w = World::new(rt, "python:3-slim");
    std::fs::create_dir(w.ws.join(".devcontainer")).unwrap();
    let config = r#"{
        // debian has no python3: the model has to install it
        "image": "debian:stable-slim",
        "containerEnv": {"GREETING": "hello-from-devcontainer"},
        "postCreateCommand": "echo made > /etc/byob-made",
        "features": {"ghcr.io/devcontainers/features/node:1": {}},
    }"#;
    std::fs::write(w.ws.join(".devcontainer/devcontainer.json"), config).unwrap();
    let d = w.envs.describe(&w.ws, &byob_tools::EnvPlan::default());
    assert_eq!(
        (d.container.devcontainer.as_deref(), d.container.use_container, d.container.image.as_str()),
        (Some(".devcontainer/devcontainer.json"), None, "debian:stable-slim")
    );

    let out = w.call("run_command", json!({"command": "echo $GREETING; cat /etc/byob-made; command -v python3 || echo no-python", "timeout_s": 600}));
    assert!(!out.failed, "{}", out.text);
    assert!(out.text.contains("hello-from-devcontainer\nmade\nno-python"), "{}", out.text);
    assert!(out.text.contains("byob ignores: features"), "{}", out.text);
    assert_eq!(w.asker.0.load(Ordering::SeqCst), 1);
    let out = w.call("run_python", json!({"code": "print(1)"}));
    assert!(out.failed && out.text.contains("install_packages") && out.text.contains("python3"), "{}", out.text);
    let out = w.call("install_packages", json!({"packages": ["python3"]}));
    assert!(!out.failed, "{}", out.text);
    let out = w.call("run_python", json!({"code": "print('now', 1 + 1)"}));
    assert!(!out.failed && out.text.contains("now 2"), "{}", out.text);
    assert_eq!(w.asker.0.load(Ordering::SeqCst), 1, "asked once");
    let saved = std::fs::read_to_string(w.data.path().join("envs/workspaces.json")).unwrap();
    assert!(saved.contains("\"container\": true"), "{saved}");
}

#[cfg(unix)]
#[test]
fn devcontainer_dockerfile_is_built_and_rebuilt_when_it_changes() {
    let Some(rt) = docker() else { return };
    let w = World::new(rt, "python:3-slim");
    w.envs.apply(&w.ws, &EnvAction::Container { on: Some(true) }).unwrap();
    std::fs::create_dir(w.ws.join(".devcontainer")).unwrap();
    std::fs::write(w.ws.join(".devcontainer/devcontainer.json"), r#"{"build": {"dockerfile": "Dockerfile", "args": {"V": "one"}}}"#).unwrap();
    std::fs::write(w.ws.join(".devcontainer/Dockerfile"), "FROM debian:stable-slim\nARG V\nRUN echo built-$V > /built\n").unwrap();
    let out = w.call("run_command", json!({"command": "cat /built", "timeout_s": 600}));
    assert!(!out.failed && out.text.contains("built-one"), "{}", out.text);
    let first = w.container_id();
    std::fs::write(w.ws.join(".devcontainer/devcontainer.json"), r#"{"build": {"dockerfile": "Dockerfile", "args": {"V": "two"}}}"#).unwrap();
    let out = w.call("run_command", json!({"command": "cat /built", "timeout_s": 600}));
    assert!(!out.failed && out.text.contains("built-two") && out.text.contains("recreated"), "{}", out.text);
    assert_ne!(w.container_id(), first);
    let tag = container_name(&w.ws).replacen("byob-", "byob-image-", 1);
    let images = Command::new(&w.rt.program).args(["images", "--format", "{{.Repository}}:{{.Tag}}", &tag]).output().unwrap();
    for image in String::from_utf8_lossy(&images.stdout).lines() {
        let _ = Command::new(&w.rt.program).args(["rmi", "-f", image]).output();
    }
}
