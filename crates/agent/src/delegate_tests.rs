//! Engine-level tests of subagents (`delegate`) against a routed fake server: parent and
//! child requests are told apart by their system prompt, so each side can be scripted, and
//! every request is served on its own thread so concurrency is real (and measured).

use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use yah_protocol::{ApprovalAnswer, ClientMsg, Connection, DelegationState, DelegationUpdate, Entry, PROTOCOL_VERSION, ServerMsg, Step, StepStatus};
use yah_tools::{Access, Registry};
use yah_types::InferenceConfig;

use crate::loop_tests::{Fake, Reply, call, read_request, recv, sse};
use crate::{EngineConfig, EngineHandle, LocalConnection, SubagentOptions};

/// What the fake server answers: a reply and how long to wait before sending it.
type Route = Arc<dyn Fn(&Value) -> (Reply, Duration) + Send + Sync>;

#[derive(Default)]
struct Seen {
    requests: Vec<Value>,
    /// Child requests in flight now, and the most at once.
    active: usize,
    max_active: usize,
}

type Shared = Arc<Mutex<Seen>>;

fn is_child(req: &Value) -> bool {
    req["messages"][0]["content"].as_str().is_some_and(|s| s.contains("running as a subagent"))
}

fn task_of(req: &Value) -> String {
    req["messages"][1]["content"].as_str().unwrap_or_default().to_owned()
}

/// Tool results in a request.
fn results(req: &Value) -> Vec<String> {
    req["messages"].as_array().into_iter().flatten().filter(|m| m["role"] == "tool").map(|m| m["content"].as_str().unwrap_or("").to_owned()).collect()
}

fn tool_names(req: &Value) -> Vec<String> {
    req["tools"].as_array().into_iter().flatten().filter_map(|t| t["function"]["name"].as_str().map(str::to_owned)).collect()
}

fn routed(route: Route) -> (String, Shared) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Shared = Arc::default();
    let shared = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let (route, seen) = (Arc::clone(&route), Arc::clone(&shared));
            std::thread::spawn(move || {
                let body = read_request(&mut s);
                let child = is_child(&body);
                let (reply, delay) = route(&body);
                {
                    let mut g = seen.lock().unwrap();
                    g.requests.push(body);
                    if child {
                        g.active += 1;
                        g.max_active = g.max_active.max(g.active);
                    }
                }
                let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n");
                if delay >= Duration::from_secs(1) {
                    // A slow model: a dot every 100 ms (so a stop is noticed between chunks).
                    let started = Instant::now();
                    while started.elapsed() < delay {
                        let dot = "data: {\"choices\":[{\"delta\":{\"content\":\".\"}}]}\n\n";
                        if s.write_all(dot.as_bytes()).and_then(|()| s.flush()).is_err() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                } else {
                    std::thread::sleep(delay);
                }
                let _ = write!(s, "{}", sse(&reply));
                if child {
                    seen.lock().unwrap().active -= 1;
                }
            });
        }
    });
    (format!("http://{addr}/v1"), seen)
}

/// Count of requests matching `f` so far (to script by round).
fn count(seen: &Shared, f: impl Fn(&Value) -> bool) -> usize {
    seen.lock().unwrap().requests.iter().filter(|r| f(r)).count()
}

fn delegate_call(task: &str) -> (String, String) {
    ("delegate".to_owned(), json!({ "task": task }).to_string())
}

struct Setup {
    engine: EngineHandle,
    runs: Arc<AtomicUsize>,
}

fn engine(url: String, workspace: &Path, paths: Option<yah_store::Paths>, tool_access: Access, parallel: Option<usize>) -> Setup {
    let runs = Arc::new(AtomicUsize::new(0));
    let mut tools = Registry::default();
    tools.register(Arc::new(Fake { name: "echo", access: Access::Read, runs: Arc::clone(&runs), reply: None, change: false }));
    tools.register(Arc::new(Fake { name: "write", access: tool_access, runs: Arc::clone(&runs), reply: Some("written".into()), change: false }));
    let mut config = EngineConfig::new(InferenceConfig { base_url: url, ..Default::default() });
    config.workspace = workspace.to_path_buf();
    config.tools = tools;
    config.paths = paths;
    config.auto_title = false;
    config.subagents = SubagentOptions { max_parallel: parallel, ..SubagentOptions::default() };
    Setup { engine: EngineHandle::spawn_with(config).unwrap(), runs }
}

fn hello(engine: &EngineHandle) -> (LocalConnection, Vec<Entry>) {
    let mut c = engine.connect(Arc::new(|| {}));
    c.send(ClientMsg::Hello { protocol: PROTOCOL_VERSION, client: "test".into() });
    loop {
        if let ServerMsg::Transcript { entries } = recv(&mut c) {
            return (c, entries);
        }
    }
}

/// A client's view, delegations included.
#[derive(Default)]
struct View {
    entries: Vec<Entry>,
    finished: Option<Option<String>>,
    finished_at: Option<Instant>,
    usage: Option<yah_protocol::ChatUsage>,
}

impl View {
    fn apply(&mut self, msg: &ServerMsg) {
        match msg.clone() {
            ServerMsg::EntryAdded(e) => self.entries.push(e),
            ServerMsg::EntryDelta { id, content, reasoning } => {
                if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
                    e.append(&content, &reasoning);
                }
            }
            ServerMsg::StepUpdated { entry, step } => {
                if let Some(e) = self.entries.iter_mut().find(|e| e.id == entry) {
                    e.upsert_step(step);
                }
            }
            ServerMsg::Delegation { entry, step, update } => {
                if let Some(e) = self.entries.iter_mut().find(|e| e.id == entry) {
                    e.apply_delegation(step, &update);
                }
            }
            ServerMsg::EntryFinished { error, .. } => {
                self.finished = Some(error);
                self.finished_at = Some(Instant::now());
            }
            ServerMsg::Usage(u) => self.usage = Some(u),
            _ => {}
        }
    }

    fn reply(&self) -> &Entry {
        self.entries.last().unwrap()
    }
}

/// Send `text` and run to the end; `on_card` sees each subagent approval card.
fn run(c: &mut LocalConnection, text: &str, mut on_card: impl FnMut(&Step, &mut LocalConnection)) -> View {
    let mut view = View::default();
    c.send(ClientMsg::SendMessage { text: text.into(), attachments: vec![] });
    while view.finished.is_none() {
        let msg = recv(c);
        view.apply(&msg);
        if let ServerMsg::Delegation { update: DelegationUpdate::Step(step), .. } = &msg
            && step.status == StepStatus::Waiting
        {
            on_card(step, c);
        }
    }
    // The usage update after the end.
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        match c.try_recv() {
            Some(m) => view.apply(&m),
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    view
}

/// Parent: delegate `tasks` in one round, then answer. Child: `child` decides by task and round.
fn script(tasks: &'static [&'static str], child: impl Fn(&str, usize) -> Reply + Send + Sync + 'static, child_delay: Duration) -> Route {
    Arc::new(move |req: &Value| {
        if is_child(req) {
            return (child(&task_of(req), results(req).len()), child_delay);
        }
        if results(req).is_empty() {
            (Reply::Calls(tasks.iter().map(|t| delegate_call(t)).collect()), Duration::ZERO)
        } else {
            (Reply::Text("All done."), Duration::ZERO)
        }
    })
}

fn temp() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn delegate_runs_a_child_turn_and_returns_its_report() {
    let ws = temp();
    let data = temp();
    let paths = yah_store::Paths::at(data.path());
    let child = |_: &str, round: usize| if round == 0 { call("echo", r#"{"text":"3 lines"}"#) } else { Reply::Text("notes.md has 3 lines.") };
    let (url, seen) = routed(script(&["Count the lines of notes.md"], child, Duration::ZERO));
    let s = engine(url.clone(), ws.path(), Some(paths.clone()), Access::Write, Some(2));
    let (mut c, _) = hello(&s.engine);
    let view = run(&mut c, "please count the lines", |_, _| panic!("reads don't ask"));
    assert_eq!(view.finished, Some(None));
    assert_eq!(view.reply().content, "All done.");
    assert_eq!(s.runs.load(Ordering::SeqCst), 1);

    // The parent step carries the child's whole transcript.
    let step = &view.reply().steps[0];
    assert_eq!((step.tool.as_str(), step.status), ("delegate", StepStatus::Done));
    let d = step.delegation.as_deref().unwrap();
    assert_eq!((d.agent.as_str(), d.task.as_str(), d.state), ("general", "Count the lines of notes.md", DelegationState::Done));
    assert!(d.content.contains("notes.md has 3 lines."), "{}", d.content);
    assert_eq!(d.steps.len(), 1);
    assert_eq!((d.steps[0].tool.as_str(), d.steps[0].status), ("echo", StepStatus::Done));
    // Only text replies report usage in this fake: one child request of 10 + 5.
    assert_eq!((d.prompt_tokens, d.completion_tokens), (10, 5));

    let reqs = seen.lock().unwrap().requests.clone();
    // The child starts fresh: its system prompt and the task, not the chat.
    let first_child = reqs.iter().find(|r| is_child(r)).unwrap();
    let msgs = first_child["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2);
    assert!(!first_child.to_string().contains("please count the lines"));
    // It gets the tools but can't delegate (depth 1); the parent can.
    assert_eq!(tool_names(first_child), ["echo", "write"]);
    let parent_first = reqs.iter().find(|r| !is_child(r)).unwrap();
    assert!(tool_names(parent_first).contains(&"delegate".to_owned()));
    assert!(parent_first["messages"][0]["content"].as_str().unwrap().contains("## Subagents"));
    // The parent's context grows only by the report.
    let parent_last = reqs.iter().rfind(|r| !is_child(r)).unwrap();
    let report = &results(parent_last)[0];
    assert!(report.starts_with("[subagent general · 1 tool call: echo"), "{report}");
    assert!(report.contains("notes.md has 3 lines."), "{report}");
    assert!(!parent_last.to_string().contains("3 lines\"}"), "the child's own tool call stays out of the parent context");

    // Subagent tokens are counted apart from the chat's.
    let u = view.usage.unwrap();
    assert_eq!((u.agent_prompt_tokens, u.agent_completion_tokens, u.agent_calls), (10, 5, 2));
    assert_eq!(u.prompt_tokens, 10, "the parent's own answer");
    drop(c);
    drop(s);

    // After a restart the child thread is back in its step.
    let s = engine(url, ws.path(), Some(paths), Access::Write, Some(2));
    let (_c, entries) = hello(&s.engine);
    let d = entries[1].steps[0].delegation.as_deref().expect("the run is restored");
    assert_eq!((d.state, d.steps.len()), (DelegationState::Done, 1));
    assert!(d.content.contains("notes.md has 3 lines."));
    assert_eq!(d.steps[0].status, StepStatus::Done);
}

#[test]
fn delegations_in_one_round_run_in_parallel_up_to_the_slot_cap() {
    let ws = temp();
    let child = |task: &str, _: usize| {
        Reply::Text(if task.contains("one") {
            "report one"
        } else if task.contains("two") {
            "report two"
        } else {
            "report three"
        })
    };
    let delay = Duration::from_millis(400);
    let (url, seen) = routed(script(&["task one", "task two", "task three"], child, delay));
    let s = engine(url, ws.path(), None, Access::Write, Some(2));
    let (mut c, _) = hello(&s.engine);
    let started = Instant::now();
    let view = run(&mut c, "do three things", |_, _| {});
    let took = view.finished_at.unwrap() - started;
    assert_eq!(view.finished, Some(None));
    let max = seen.lock().unwrap().max_active;
    assert_eq!(max, 2, "two at once (the cap), not three and not one");
    // Three children of 400 ms with two slots: about 800 ms, not 1200.
    assert!(took < Duration::from_millis(1150), "{took:?}");
    let steps = &view.reply().steps;
    assert_eq!(steps.len(), 3);
    assert!(steps.iter().all(|s| s.delegation.as_ref().is_some_and(|d| d.state == DelegationState::Done)));
    // Results come back in call order.
    let reqs = seen.lock().unwrap().requests.clone();
    let last = reqs.iter().rfind(|r| !is_child(r)).unwrap();
    let r = results(last);
    assert!(r[0].contains("report one") && r[1].contains("report two") && r[2].contains("report three"), "{r:?}");
}

#[test]
fn one_slot_runs_delegations_one_after_another() {
    let ws = temp();
    let (url, seen) = routed(script(&["a", "b"], |_, _| Reply::Text("ok"), Duration::from_millis(150)));
    let s = engine(url, ws.path(), None, Access::Write, Some(1));
    let (mut c, _) = hello(&s.engine);
    let view = run(&mut c, "go", |_, _| {});
    assert_eq!(view.finished, Some(None));
    assert_eq!(seen.lock().unwrap().max_active, 1);
    assert_eq!(count(&seen, is_child), 2);
}

#[test]
fn a_subagents_approval_card_is_answered_from_any_client() {
    let ws = temp();
    let child = |_: &str, round: usize| if round == 0 { call("write", r#"{"text":"x"}"#) } else { Reply::Text("wrote it") };
    let (url, _) = routed(script(&["write x"], child, Duration::ZERO));
    let s = engine(url, ws.path(), None, Access::Write, Some(2));
    let (mut desktop, _) = hello(&s.engine);
    let mut cards = 0;
    let view = run(&mut desktop, "write x via a subagent", |card, _| {
        cards += 1;
        assert_eq!(s.runs.load(Ordering::SeqCst), 0, "not before approval");
        // A phone connecting now sees the card in the transcript and answers it.
        let (mut phone, entries) = hello(&s.engine);
        let pending: Vec<u64> = entries.last().unwrap().pending_approvals().map(|s| s.id).collect();
        assert_eq!(pending, vec![card.id]);
        phone.send(ClientMsg::Approve { step: card.id, answer: ApprovalAnswer::Allow });
    });
    assert_eq!(cards, 1);
    assert_eq!(s.runs.load(Ordering::SeqCst), 1);
    let d = view.reply().steps[0].delegation.as_deref().unwrap();
    assert_eq!((d.steps[0].status, d.state), (StepStatus::Done, DelegationState::Done));
}

#[test]
fn stop_cancels_the_whole_tree() {
    let ws = temp();
    // The child thinks for a long time (a slow model request).
    let route: Route = Arc::new(|req: &Value| {
        if is_child(req) {
            (Reply::Text("never"), Duration::from_secs(30))
        } else if results(req).is_empty() {
            (Reply::Calls(vec![delegate_call("slow one"), delegate_call("slow two")]), Duration::ZERO)
        } else {
            (Reply::Text("unreachable"), Duration::ZERO)
        }
    });
    let (url, seen) = routed(route);
    let s = engine(url, ws.path(), None, Access::Write, Some(2));
    let (mut c, _) = hello(&s.engine);
    c.send(ClientMsg::SendMessage { text: "go".into(), attachments: vec![] });
    let mut view = View::default();
    // Wait until both children are talking to the model, then stop.
    while count(&seen, is_child) < 2 {
        if let Some(m) = c.try_recv() {
            view.apply(&m);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let stopped = Instant::now();
    c.send(ClientMsg::Cancel);
    while view.finished.is_none() {
        view.apply(&recv(&mut c));
    }
    assert!(stopped.elapsed() < Duration::from_secs(5), "{:?}", stopped.elapsed());
    assert_eq!(view.finished, Some(Some("stopped".into())));
    for step in &view.reply().steps {
        let d = step.delegation.as_deref().unwrap();
        assert!(d.state.finished() && d.state != DelegationState::Done, "{:?}", d.state);
    }
    // The parent never got to a second request.
    assert_eq!(count(&seen, |r| !is_child(r)), 1);
}

#[test]
fn a_waiting_subagent_card_is_denied_by_stop() {
    let ws = temp();
    let (url, _) = routed(script(&["write"], |_, _| call("write", r#"{"text":"x"}"#), Duration::ZERO));
    let s = engine(url, ws.path(), None, Access::Write, Some(2));
    let (mut c, _) = hello(&s.engine);
    let view = run(&mut c, "go", |_, c| c.send(ClientMsg::Cancel));
    assert_eq!(view.finished, Some(Some("stopped".into())));
    assert_eq!(s.runs.load(Ordering::SeqCst), 0);
    let d = view.reply().steps[0].delegation.as_deref().unwrap();
    assert_eq!(d.steps[0].status, StepStatus::Denied);
    assert_eq!(d.state, DelegationState::Stopped);
}

#[test]
fn subagents_cannot_delegate() {
    let ws = temp();
    let child = |_: &str, round: usize| if round == 0 { Reply::Calls(vec![delegate_call("deeper")]) } else { Reply::Text("can't go deeper") };
    let (url, seen) = routed(script(&["try to nest"], child, Duration::ZERO));
    let s = engine(url, ws.path(), None, Access::Write, Some(2));
    let (mut c, _) = hello(&s.engine);
    let view = run(&mut c, "go", |_, _| {});
    assert_eq!(view.finished, Some(None));
    let reqs = seen.lock().unwrap().requests.clone();
    let second_child = reqs.iter().filter(|r| is_child(r)).nth(1).unwrap();
    assert!(results(second_child)[0].contains("no tool called \"delegate\""), "{:?}", results(second_child));
    assert_eq!(count(&seen, |r| is_child(r) && task_of(r) == "deeper"), 0);
    let d = view.reply().steps[0].delegation.as_deref().unwrap();
    assert_eq!(d.steps[0].status, StepStatus::Failed);
}

#[test]
fn workspace_agent_definitions_pick_prompt_and_tools() {
    let ws = temp();
    let dir = ws.path().join(".claude").join("agents");
    std::fs::create_dir_all(&dir).unwrap();
    // A Claude Code agent file, unchanged (unknown keys, a Claude model alias, Claude tool names).
    std::fs::write(
        dir.join("counter.md"),
        "---\nname: counter\ndescription: Counts things in files. Use for any counting.\ntools: Read, Bash\nmodel: sonnet\ncolor: green\n---\n\nYou count things precisely.\n",
    )
    .unwrap();
    let route: Route = Arc::new(|req: &Value| {
        if is_child(req) {
            return (Reply::Text("42"), Duration::ZERO);
        }
        if results(req).is_empty() {
            (Reply::Calls(vec![("delegate".into(), json!({"agent": "counter", "task": "count"}).to_string())]), Duration::ZERO)
        } else {
            (Reply::Text("It is 42."), Duration::ZERO)
        }
    });
    let (url, seen) = routed(route);
    let runs = Arc::new(AtomicUsize::new(0));
    let mut tools = Registry::default();
    for name in ["read_file", "write_file", "run_command"] {
        tools.register(Arc::new(Fake { name, access: Access::Read, runs: Arc::clone(&runs), reply: None, change: false }));
    }
    let mut config = EngineConfig::new(InferenceConfig { base_url: url, ..Default::default() });
    config.workspace = ws.path().to_path_buf();
    config.tools = tools;
    config.auto_title = false;
    let engine = EngineHandle::spawn_with(config).unwrap();
    let (mut c, _) = hello(&engine);
    let view = run(&mut c, "count it", |_, _| {});
    assert_eq!(view.finished, Some(None));
    let reqs = seen.lock().unwrap().requests.clone();
    let parent = reqs.iter().find(|r| !is_child(r)).unwrap();
    assert!(parent["messages"][0]["content"].as_str().unwrap().contains("- counter: Counts things in files."));
    let spec = parent["tools"].as_array().unwrap().iter().find(|t| t["function"]["name"] == "delegate").unwrap();
    assert_eq!(spec["function"]["parameters"]["properties"]["agent"]["enum"], json!(["general", "counter"]));
    let child = reqs.iter().find(|r| is_child(r)).unwrap();
    assert!(child["messages"][0]["content"].as_str().unwrap().starts_with("You count things precisely."));
    // Read → read_file, Bash → run_command; write_file is not for this agent.
    assert_eq!(tool_names(child), ["read_file", "run_command"]);
    assert_eq!(view.reply().steps[0].delegation.as_deref().unwrap().agent, "counter");
}

#[test]
fn an_agents_model_role_picks_its_endpoint() {
    let ws = temp();
    let dir = ws.path().join(".yah").join("agents");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("quick.md"), "---\nname: quick\ndescription: Fast lookups\nmodel: fast\n---\nBe quick.").unwrap();
    std::fs::write(dir.join("other.md"), "---\nname: other\ndescription: On another model id\nmodel: tiny-model\n---\nBe small.").unwrap();
    let route: Route = Arc::new(|req: &Value| {
        if is_child(req) {
            return (Reply::Text("main side"), Duration::ZERO);
        }
        if results(req).is_empty() {
            let calls =
                ["quick", "general", "other"].iter().map(|a| ("delegate".to_owned(), json!({"agent": a, "task": format!("as {a}")}).to_string())).collect();
            (Reply::Calls(calls), Duration::ZERO)
        } else {
            (Reply::Text("done"), Duration::ZERO)
        }
    });
    let (url, main) = routed(route);
    let (role_url, role) = routed(Arc::new(|_: &Value| (Reply::Text("role side"), Duration::ZERO)));
    let runs = Arc::new(AtomicUsize::new(0));
    let mut tools = Registry::default();
    tools.register(Arc::new(Fake { name: "echo", access: Access::Read, runs, reply: None, change: false }));
    let mut config = EngineConfig::new(InferenceConfig { base_url: url, model: "main-model".into(), api_key: None });
    config.subagents = SubagentOptions { max_parallel: Some(3), ..SubagentOptions::default() };
    config.workspace = ws.path().to_path_buf();
    config.tools = tools;
    config.auto_title = false;
    config.models.insert("fast".into(), InferenceConfig { base_url: role_url, model: "small".into(), api_key: None });
    let engine = EngineHandle::spawn_with(config).unwrap();
    let (mut c, _) = hello(&engine);
    let view = run(&mut c, "go", |_, _| {});
    assert_eq!(view.finished, Some(None));
    let on_role = role.lock().unwrap().requests.clone();
    assert_eq!(on_role.len(), 1);
    assert_eq!((on_role[0]["model"].as_str(), task_of(&on_role[0]).as_str()), (Some("small"), "as quick"));
    let on_main: Vec<(String, String)> =
        main.lock().unwrap().requests.iter().filter(|r| is_child(r)).map(|r| (task_of(r), r["model"].as_str().unwrap_or("").to_owned())).collect();
    assert!(on_main.contains(&("as general".to_owned(), "main-model".to_owned())), "{on_main:?}");
    assert!(on_main.contains(&("as other".to_owned(), "tiny-model".to_owned())), "{on_main:?}");
    let models: Vec<&str> = view.reply().steps.iter().filter_map(|s| s.delegation.as_deref()).map(|d| d.model.as_str()).collect();
    assert_eq!(models, ["small", "main-model", "tiny-model"]);
}

#[test]
fn bad_delegations_are_explained_to_the_model() {
    let ws = temp();
    let route: Route = Arc::new(|req: &Value| {
        if results(req).is_empty() {
            let calls = vec![
                ("delegate".to_owned(), json!({"agent": "nobody", "task": "x"}).to_string()),
                ("delegate".to_owned(), json!({"task": "  "}).to_string()),
                ("delegate".to_owned(), json!({"task": "x", "tools": ["nope"]}).to_string()),
            ];
            (Reply::Calls(calls), Duration::ZERO)
        } else {
            (Reply::Text("sorry"), Duration::ZERO)
        }
    });
    let (url, seen) = routed(route);
    let s = engine(url, ws.path(), None, Access::Write, Some(2));
    let (mut c, _) = hello(&s.engine);
    let view = run(&mut c, "go", |_, _| {});
    assert_eq!(view.finished, Some(None));
    let reqs = seen.lock().unwrap().requests.clone();
    assert_eq!(count(&seen, is_child), 0);
    let r = results(reqs.last().unwrap());
    assert!(r[0].contains("no agent called \"nobody\"") && r[0].contains("general"), "{}", r[0]);
    assert!(r[1].contains("needs a `task`"), "{}", r[1]);
    assert!(r[2].contains("none of the tools asked for"), "{}", r[2]);
    assert!(view.reply().steps.iter().all(|s| s.status == StepStatus::Failed));
}

crate::test_store::folder_too!(
    delegate_runs_a_child_turn_and_returns_its_report,
    delegations_in_one_round_run_in_parallel_up_to_the_slot_cap,
    a_subagents_approval_card_is_answered_from_any_client,
    stop_cancels_the_whole_tree,
);
