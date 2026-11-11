//! Long-lived plugin backends against real Python processes (skipped without `python3`).

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use byob_plugins::backend::{Backend, Launch, MAX_LINE, Out, Phase, Sink};
use byob_plugins::{PluginPython, PyEnvs};
use serde_json::{Value, json};

fn python() -> Option<Vec<OsString>> {
    let p = byob_plugins::default_python();
    std::process::Command::new(&p[0]).arg("-c").arg("pass").status().ok().filter(|s| s.success()).map(|_| p)
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    envs: Arc<PyEnvs>,
}

fn fixture(script: &str) -> Option<Fixture> {
    let base = python()?;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    fs::write(root.join("backend.py"), script).unwrap();
    let envs = Arc::new(PyEnvs::new(root.join("py"), base).with_uv(None));
    Some(Fixture { _dir: dir, root, envs })
}

fn start(f: &Fixture, events: &[&str]) -> (Backend, Receiver<Out>) {
    let (tx, rx) = channel();
    let tx = Mutex::new(tx);
    let sink: Sink = Arc::new(move |o| {
        let _ = tx.lock().unwrap().send(o);
    });
    let py = PluginPython::new(&f.envs, &[], &f.root);
    let cwd = f.root.clone();
    let prepare = Box::new(move || {
        let p = py.python()?;
        let mut argv = p.program;
        argv.push("backend.py".into());
        Ok(Launch { argv, cwd: cwd.clone(), env: p.env, remove_env: vec!["BYOB_API_KEY".into()] }.into())
    });
    let events: Vec<String> = events.iter().map(|s| (*s).to_owned()).collect();
    (Backend::new("demo", json!({"type": "init", "v": 1, "plugin": "demo", "settings": {"CITY": "Oslo"}}), &events, prepare, sink), rx)
}

fn next_msg(rx: &Receiver<Out>, kind: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(Out::Msg(m)) if m["type"] == kind => return m,
            Ok(_) => {}
            Err(_) => panic!("no {kind} message"),
        }
    }
}

fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

const ECHO: &str = r#"
import os, sys
import byob_plugin as byob

state = {"n": 0}

@byob.tool
def add(n: int = 1):
    state["n"] += n
    print("added", n)  # goes to stderr
    return f"n={state['n']}"

@byob.tool
def crash():
    sys.stderr.write("about to crash: boom\n")
    sys.stderr.flush()
    os._exit(3)

@byob.tool
def city():
    return {"text": byob.settings.get("CITY", "?") + " " + str(byob.init.get("restart"))}

@byob.tool
def big():
    byob._send({"type": "notify", "msg": "x" * (2 * 1024 * 1024)})
    sys.__stdout__.write("this is not json\n")
    sys.__stdout__.flush()
    return "ok"

@byob.on_message
def message(msg, client):
    byob.send({"echo": msg}, client=client)

@byob.on("turn_finished")
def finished(event):
    byob.send_view({"type": "text", "text": "turn " + str(event.get("entry"))}, key="last")

@byob.tool
def oops():
    raise ValueError("bad value here")

byob.run()
"#;

#[test]
fn requests_events_and_state_live_in_one_process() {
    let Some(f) = fixture(ECHO) else { return };
    let (b, rx) = start(&f, &[]);
    assert_eq!(b.status().phase, Phase::Idle, "starts lazily");
    let r = b.request("tool", json!({"name": "add", "args": {"n": 2}}), Duration::from_secs(20), &|| false).unwrap();
    assert_eq!(r, json!("n=2"));
    let r = b.request("tool", json!({"name": "add", "args": {"n": 3}}), Duration::from_secs(20), &|| false).unwrap();
    assert_eq!(r, json!("n=5"), "state is kept between calls");
    assert_eq!(b.request("tool", json!({"name": "city"}), Duration::from_secs(20), &|| false).unwrap()["text"], "Oslo 0");
    // the helper subscribes for its @on handlers
    wait_for("the subscription", || b.wants("turn_finished"));
    assert!(b.event(json!({"type": "event", "event": "turn_finished", "chat": "c", "entry": 7})));
    let v = next_msg(&rx, "view");
    assert_eq!((v["key"].as_str(), v["view"]["text"].as_str()), (Some("last"), Some("turn 7")));
    assert!(b.event(json!({"type": "event", "event": "client", "client": 4, "msg": {"click": "go"}})));
    let n = next_msg(&rx, "notify");
    assert_eq!(n["client"], 4);
    assert_eq!(n["msg"]["echo"]["click"], "go");
    // a handler's exception comes back as the reply's error, with the traceback
    let e = b.request("tool", json!({"name": "oops"}), Duration::from_secs(20), &|| false).unwrap_err();
    assert!(e.contains("ValueError: bad value here"), "{e}");
    let e = b.request("tool", json!({"name": "nope"}), Duration::from_secs(20), &|| false).unwrap_err();
    assert!(e.contains("no tool 'nope'"), "{e}");
    // print() went to stderr, which is kept
    wait_for("stderr", || b.status().stderr.contains("added 2"));
    assert_eq!(b.status().starts, 1);
}

#[test]
fn a_crash_fails_the_call_with_its_stderr_and_it_comes_back() {
    let Some(f) = fixture(ECHO) else { return };
    let (b, rx) = start(&f, &[]);
    b.request("tool", json!({"name": "add"}), Duration::from_secs(20), &|| false).unwrap();
    let first = b.status().pid.unwrap();
    let e = b.request("tool", json!({"name": "crash"}), Duration::from_secs(20), &|| false).unwrap_err();
    assert!(e.contains("exit code 3") && e.contains("about to crash: boom"), "{e}");
    let mut exited = false;
    while let Ok(o) = rx.recv_timeout(Duration::from_secs(5)) {
        if matches!(o, Out::Exited(_)) {
            exited = true;
            break;
        }
    }
    assert!(exited);
    // restarted after the backoff, without anyone asking
    wait_for("the restart", || b.status().phase == Phase::Running);
    let s = b.status();
    assert_eq!(s.starts, 2);
    assert_ne!(s.pid, Some(first));
    assert_eq!(b.request("tool", json!({"name": "add"}), Duration::from_secs(20), &|| false).unwrap(), json!("n=1"), "a new process");
    assert_eq!(b.request("tool", json!({"name": "city"}), Duration::from_secs(20), &|| false).unwrap()["text"], "Oslo 1", "init says it's a restart");
}

#[test]
fn it_gives_up_after_crashing_again_and_again() {
    let Some(f) = fixture("import sys\nsys.stderr.write('no luck\\n')\nsys.exit(1)\n") else { return };
    let (b, _rx) = start(&f, &[]);
    for _ in 0..byob_plugins::backend::MAX_CRASHES {
        let _ = b.request("tool", json!({}), Duration::from_secs(20), &|| false);
    }
    wait_for("giving up", || b.status().phase == Phase::Failed);
    let e = b.request("tool", json!({}), Duration::from_secs(5), &|| false).unwrap_err();
    assert!(e.contains("stopped starting it") && e.contains("no luck"), "{e}");
}

#[test]
fn stop_ends_the_process_and_later_calls_say_so() {
    let Some(f) = fixture(ECHO) else { return };
    let (b, _rx) = start(&f, &[]);
    b.request("tool", json!({"name": "add"}), Duration::from_secs(20), &|| false).unwrap();
    let pid = b.status().pid.unwrap();
    b.stop("reloaded");
    assert_eq!(b.status().phase, Phase::Stopped);
    #[cfg(target_os = "linux")]
    wait_for("the process to go", || !Path::new(&format!("/proc/{pid}")).exists() || is_zombie(pid));
    let _ = pid;
    let e = b.request("tool", json!({"name": "add"}), Duration::from_secs(5), &|| false).unwrap_err();
    assert!(e.contains("stopped"), "{e}");
    assert!(!b.event(json!({"type": "event", "event": "client"})));
}

#[cfg(target_os = "linux")]
fn is_zombie(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| s.split_whitespace().nth(2) == Some("Z"))
}

#[test]
fn a_backend_that_ignores_stdin_closing_is_killed() {
    let Some(f) = fixture("import time, sys\nfor line in sys.stdin:\n    pass\ntime.sleep(60)\n") else { return };
    let (b, _rx) = start(&f, &[]);
    b.start();
    wait_for("start", || b.status().phase == Phase::Running);
    let pid = b.status().pid.unwrap();
    let t = Instant::now();
    drop(b);
    #[cfg(target_os = "linux")]
    wait_for("the kill", || !Path::new(&format!("/proc/{pid}")).exists());
    let _ = pid;
    assert!(t.elapsed() < Duration::from_secs(10));
}

#[test]
fn oversized_and_garbled_output_are_faults_not_failures() {
    let Some(f) = fixture(ECHO) else { return };
    let (b, rx) = start(&f, &[]);
    assert_eq!(b.request("tool", json!({"name": "big"}), Duration::from_secs(20), &|| false).unwrap(), json!("ok"));
    let a = next_msg(&rx, "fault");
    assert!(a["text"].as_str().unwrap().contains(&format!("over {MAX_LINE} bytes")), "{a}");
    let g = next_msg(&rx, "fault");
    assert!(g["text"].as_str().unwrap().contains("this is not json"), "{g}");
    let s = b.status();
    assert_eq!(s.faults.len(), 2);
    assert!(s.stderr.contains("(stdout) this is not json"));
    // still serving
    assert_eq!(b.request("tool", json!({"name": "add"}), Duration::from_secs(20), &|| false).unwrap(), json!("n=1"));
}

#[test]
fn requests_time_out_and_can_be_cancelled() {
    let Some(f) = fixture("import sys\nfor line in sys.stdin:\n    pass\n") else { return };
    let (b, _rx) = start(&f, &[]);
    let e = b.request("tool", json!({}), Duration::from_millis(300), &|| false).unwrap_err();
    assert!(e.contains("didn't answer"), "{e}");
    let e = b.request("tool", json!({}), Duration::from_secs(20), &|| true).unwrap_err();
    assert_eq!(e, "Stopped by the user.");
}

#[test]
fn round_trips_are_fast() {
    let Some(f) = fixture(ECHO) else { return };
    let (b, _rx) = start(&f, &[]);
    b.request("tool", json!({"name": "add"}), Duration::from_secs(20), &|| false).unwrap();
    let n = 300;
    let t = Instant::now();
    for _ in 0..n {
        b.request("tool", json!({"name": "city"}), Duration::from_secs(20), &|| false).unwrap();
    }
    let each = t.elapsed() / n;
    eprintln!("backend round trip: {each:?} per request (mean of {n})");
    assert!(each < Duration::from_millis(50), "{each:?}");
}

/// A wheel with one module, made by hand (a zip with dist-info), so pip installs it offline.
fn tiny_wheel(dir: &Path, py: &[OsString]) -> PathBuf {
    let whl = dir.join("tinypkg-0.1-py3-none-any.whl");
    let code = r#"
import sys, zipfile
with zipfile.ZipFile(sys.argv[1], "w") as z:
    z.writestr("tinypkg/__init__.py", "ANSWER = 42\n")
    z.writestr("tinypkg-0.1.dist-info/METADATA", "Metadata-Version: 2.1\nName: tinypkg\nVersion: 0.1\n")
    z.writestr("tinypkg-0.1.dist-info/WHEEL", "Wheel-Version: 1.0\nGenerator: byob-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n")
    z.writestr("tinypkg-0.1.dist-info/RECORD", "")
"#;
    let ok = std::process::Command::new(&py[0]).arg("-c").arg(code).arg(&whl).status().unwrap().success();
    assert!(ok);
    whl
}

#[test]
fn requirements_get_a_cached_venv_used_by_the_backend() {
    let Some(base) = python() else { return };
    let venv_ok = std::process::Command::new(&base[0]).args(["-c", "import venv, ensurepip"]).status().is_ok_and(|s| s.success());
    if !venv_ok {
        eprintln!("skipped: this Python has no venv/ensurepip");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("wheels")).unwrap();
    tiny_wheel(&root.join("wheels"), &base);
    fs::write(root.join("backend.py"), "import byob_plugin as byob, tinypkg\n\n@byob.tool\ndef answer():\n    return str(tinypkg.ANSWER)\n\nbyob.run()\n")
        .unwrap();
    let envs = Arc::new(PyEnvs::new(root.join("py"), base.clone()).with_uv(None).offline());
    let reqs = vec!["./wheels/tinypkg-0.1-py3-none-any.whl".to_owned()];
    let py = PluginPython::new(&envs, &reqs, root);
    let t = Instant::now();
    let p = py.python().unwrap();
    eprintln!("venv with one local wheel built in {:?}", t.elapsed());
    assert!(py.env.as_ref().unwrap().ready());
    let cwd = root.to_path_buf();
    let prepare = Box::new(move || {
        let mut argv = p.program.clone();
        argv.push("backend.py".into());
        Ok(Launch { argv, cwd: cwd.clone(), env: p.env.clone(), remove_env: vec![] }.into())
    });
    let b = Backend::new("venv", json!({"type": "init"}), &[], prepare, Arc::new(|_| {}));
    assert_eq!(b.request("tool", json!({"name": "answer"}), Duration::from_secs(20), &|| false).unwrap(), json!("42"));
    // a second engine run finds it built
    let again = PyEnvs::new(root.join("py"), base).with_uv(None).offline();
    let t = Instant::now();
    assert!(again.for_plugin(&reqs, root).ensure().is_ok());
    assert!(t.elapsed() < Duration::from_secs(2));
}

#[test]
fn a_failed_install_is_a_problem_with_the_installer_output() {
    let Some(base) = python() else { return };
    let dir = tempfile::tempdir().unwrap();
    let envs = Arc::new(PyEnvs::new(dir.path().join("py"), base).with_uv(None).offline());
    let py = PluginPython::new(&envs, &["./missing-0.1-py3-none-any.whl".into()], dir.path());
    let e = py.python().unwrap_err();
    assert!(e.starts_with("python.requirements couldn't be installed"), "{e}");
    assert_eq!(py.failure().as_deref(), Some(e.as_str()));
    // kept, not retried on every call, until the plugin reloads
    assert_eq!(py.python().unwrap_err(), e);
    envs.forget_failed();
    let fresh = PluginPython::new(&envs, &["./missing-0.1-py3-none-any.whl".into()], dir.path());
    assert!(fresh.failure().is_none());
}
