//! Engine-level tests of the tool loop and approvals against a scripted fake server.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use byob_protocol::{ApprovalAnswer, ClientMsg, Connection, Entry, FileChange, PROTOCOL_VERSION, ServerMsg, Step, StepStatus, ToolAccess};
use byob_tools::{Access, Registry, Tool, ToolContext, ToolOutput, ToolSpec};
use byob_types::InferenceConfig;

use crate::{EngineConfig, EngineHandle, LocalConnection, Policy};

#[derive(Clone)]
pub(crate) enum Reply {
    Text(&'static str),
    /// (name, raw arguments)
    Calls(Vec<(String, String)>),
    /// Text, then one call.
    Mixed(&'static str, &'static str, &'static str),
}

pub(crate) fn call(name: &str, args: &str) -> Reply {
    Reply::Calls(vec![(name.into(), args.into())])
}

pub(crate) fn sse(reply: &Reply) -> String {
    let frame = |v: Value| format!("data: {v}\n\n");
    let mut out = String::new();
    match reply {
        Reply::Text(t) => {
            for w in t.split_inclusive(' ') {
                out += &frame(json!({"choices":[{"delta":{"content": w}}]}));
            }
            out += &frame(json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5}}));
        }
        Reply::Calls(calls) => {
            for (i, (name, args)) in calls.iter().enumerate() {
                // Split the arguments over two chunks like real servers do.
                let mid = args.char_indices().nth(args.chars().count() / 2).map_or(0, |(i, _)| i);
                let (a, b) = args.split_at(mid);
                out += &frame(
                    json!({"choices":[{"delta":{"tool_calls":[{"index":i,"id":format!("c{i}"),"type":"function","function":{"name":name,"arguments":a}}]}}]}),
                );
                out += &frame(json!({"choices":[{"delta":{"tool_calls":[{"index":i,"function":{"arguments":b}}]}}]}));
            }
            out += &frame(json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}));
        }
        Reply::Mixed(text, name, args) => {
            out += &frame(json!({"choices":[{"delta":{"content": text}}]}));
            out += &frame(json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"m0","type":"function","function":{"name":name,"arguments":args}}]}}]}));
        }
    }
    out + "data: [DONE]\n\n"
}

/// Requests the fake server received, as JSON.
pub(crate) type Requests = Arc<Mutex<Vec<Value>>>;

/// Replies with `script` in order (repeating the last one); requests without `tools`
/// always get `no_tools`.
fn fake_llm(script: Vec<Reply>, no_tools: &'static str) -> (String, Requests) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let requests: Requests = Arc::default();
    let seen = Arc::clone(&requests);
    std::thread::spawn(move || {
        let mut n = 0usize;
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let body = read_request(&mut s);
            let has_tools = body.get("tools").is_some();
            seen.lock().unwrap().push(body);
            let reply = if has_tools { script.get(n).or(script.last()).cloned().unwrap_or(Reply::Text("ok")) } else { Reply::Text(no_tools) };
            n += 1;
            let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{}", sse(&reply));
        }
    });
    (format!("http://{addr}/v1"), requests)
}

pub(crate) fn read_request(s: &mut std::net::TcpStream) -> Value {
    let mut r = BufReader::new(s);
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':')
            && k.eq_ignore_ascii_case("content-length")
        {
            len = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0; len];
    let _ = r.read_exact(&mut body);
    serde_json::from_slice(&body).unwrap_or(Value::Null)
}

/// A test tool: returns `text` (or `reply`), counts runs.
pub(crate) struct Fake {
    pub(crate) name: &'static str,
    pub(crate) access: Access,
    pub(crate) runs: Arc<AtomicUsize>,
    pub(crate) reply: Option<String>,
    pub(crate) change: bool,
}

impl Tool for Fake {
    fn spec(&self) -> ToolSpec {
        ToolSpec { name: self.name.into(), description: "test".into(), parameters: json!({"type":"object"}) }
    }
    fn title(&self, args: &Value) -> String {
        format!("{} {}", self.name, args.get("text").and_then(Value::as_str).unwrap_or(""))
    }
    fn access(&self, _args: &Value) -> Access {
        self.access
    }
    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        self.runs.fetch_add(1, Ordering::SeqCst);
        if self.name == "boom" {
            panic!("tool bug");
        }
        let mut out = ToolOutput::ok(self.reply.clone().unwrap_or_else(|| args.get("text").and_then(Value::as_str).unwrap_or("").to_owned()));
        if self.change {
            out.changes.push(byob_tools::FileChange::Created(ctx.workspace.join("a.txt")));
        }
        out
    }
}

struct Harness {
    engine: EngineHandle,
    requests: Requests,
    pub(crate) runs: Arc<AtomicUsize>,
}

fn harness(script: Vec<Reply>, tool: &'static str, access: Access) -> Harness {
    harness_with(script, tool, access, None)
}

fn harness_with(script: Vec<Reply>, tool: &'static str, access: Access, reply: Option<String>) -> Harness {
    harness_at(script, tool, access, reply, None)
}

fn harness_at(script: Vec<Reply>, tool: &'static str, access: Access, reply: Option<String>, paths: Option<byob_store::Paths>) -> Harness {
    let (url, requests) = fake_llm(script, "final answer");
    let runs = Arc::new(AtomicUsize::new(0));
    let mut tools = Registry::default();
    tools.register(Arc::new(Fake { name: tool, access, runs: Arc::clone(&runs), reply, change: access == Access::Write }));
    let config = EngineConfig {
        inference: InferenceConfig { base_url: url, ..Default::default() },
        workspace: PathBuf::from("/ws"),
        tools,
        policy: Policy::default(),
        system_prompt: crate::turn::DEFAULT_SYSTEM_PROMPT.into(),
        paths,
        auto_title: false,
        storage: byob_store::StorageKind::default(),
        chats_folder: None,
        adapt: crate::AdaptOptions::default(),
        compaction: crate::CompactOptions::default(),
        agents: Vec::new(),
        models: Default::default(),
        subagents: Default::default(),
    };
    Harness { engine: EngineHandle::spawn_with(config).unwrap(), requests, runs }
}

pub(crate) fn recv(conn: &mut LocalConnection) -> ServerMsg {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(m) = conn.try_recv() {
            return m;
        }
        assert!(Instant::now() < deadline, "timed out waiting for engine");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn client(h: &Harness) -> (LocalConnection, Vec<Entry>) {
    let mut c = h.engine.connect(Arc::new(|| {}));
    c.send(ClientMsg::Hello { protocol: PROTOCOL_VERSION, client: "test".into() });
    // Hello is answered with the chat list, State, ChatOpened and the Transcript.
    loop {
        if let ServerMsg::Transcript { entries } = recv(&mut c) {
            return (c, entries);
        }
    }
}

/// Mirror of what a UI keeps.
#[derive(Default)]
struct View {
    entries: Vec<Entry>,
    error: Option<Option<String>>,
}

impl View {
    fn apply(&mut self, msg: ServerMsg) {
        match msg {
            ServerMsg::Transcript { entries } => self.entries = entries,
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
            ServerMsg::EntryFinished { id, error, .. } => {
                if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
                    e.done = true;
                }
                self.error = Some(error);
            }
            _ => {}
        }
    }

    fn reply(&self) -> &Entry {
        self.entries.last().unwrap()
    }
}

/// Run a turn to the end; `on_wait` answers each approval card as it appears.
fn run_turn(c: &mut LocalConnection, text: &str, mut on_wait: impl FnMut(&Step, &mut LocalConnection)) -> View {
    let mut view = View::default();
    c.send(ClientMsg::SendMessage { text: text.into(), attachments: vec![] });
    while view.error.is_none() {
        let msg = recv(c);
        if let ServerMsg::StepUpdated { step, .. } = &msg
            && step.status == StepStatus::Waiting
        {
            let step = step.clone();
            view.apply(msg);
            on_wait(&step, c);
            continue;
        }
        view.apply(msg);
    }
    view
}

fn messages(req: &Value) -> Vec<Value> {
    req["messages"].as_array().cloned().unwrap_or_default()
}

fn tool_results(req: &Value) -> Vec<String> {
    messages(req).iter().filter(|m| m["role"] == "tool").map(|m| m["content"].as_str().unwrap_or("").to_owned()).collect()
}

#[test]
fn tool_call_then_answer() {
    let h = harness(vec![call("echo", r#"{"text":"pong"}"#), Reply::Text("The answer is pong")], "echo", Access::Read);
    let (mut c, _) = client(&h);
    let view = run_turn(&mut c, "ping?", |_, _| panic!("reads must not ask"));
    assert_eq!(view.error, Some(None));
    let reply = view.reply();
    assert_eq!(reply.content, "The answer is pong");
    assert_eq!(reply.steps.len(), 1);
    let step = &reply.steps[0];
    assert_eq!((step.status, step.access, step.title.as_str(), step.detail.as_str()), (StepStatus::Done, ToolAccess::Read, "echo pong", "pong"));
    assert_eq!(h.runs.load(Ordering::SeqCst), 1);

    let reqs = h.requests.lock().unwrap();
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[0]["tools"][0]["function"]["name"], "echo");
    let second = messages(&reqs[1]);
    let assistant = &second[second.len() - 2];
    assert_eq!(assistant["tool_calls"][0]["function"]["arguments"], r#"{"text":"pong"}"#);
    assert_eq!(assistant["tool_calls"][0]["id"], "c0");
    let tool = &second[second.len() - 1];
    assert_eq!((tool["role"].as_str(), tool["tool_call_id"].as_str(), tool["content"].as_str()), (Some("tool"), Some("c0"), Some("pong")));
}

#[test]
fn approval_allow_runs_the_tool_and_reports_changes() {
    let h = harness(vec![call("write", r#"{"text":"x"}"#), Reply::Text("written")], "write", Access::Write);
    let (mut c, _) = client(&h);
    let mut asked = 0;
    let view = run_turn(&mut c, "write it", |step, c| {
        asked += 1;
        assert_eq!(h.runs.load(Ordering::SeqCst), 0, "must not run before approval");
        c.send(ClientMsg::Approve { step: step.id, answer: ApprovalAnswer::Allow });
    });
    assert_eq!(asked, 1);
    assert_eq!(h.runs.load(Ordering::SeqCst), 1);
    let step = &view.reply().steps[0];
    assert_eq!(step.status, StepStatus::Done);
    assert_eq!(step.changes, vec![FileChange::Created { path: "a.txt".into() }]);
    assert_eq!(view.reply().content, "written");
}

#[test]
fn approval_deny_tells_the_model() {
    let h = harness(vec![call("write", r#"{"text":"x"}"#), Reply::Text("ok, I won't")], "write", Access::Write);
    let (mut c, _) = client(&h);
    let view = run_turn(&mut c, "write it", |step, c| c.send(ClientMsg::Approve { step: step.id, answer: ApprovalAnswer::Deny }));
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
    assert_eq!(view.reply().steps[0].status, StepStatus::Denied);
    assert_eq!(view.error, Some(None));
    let reqs = h.requests.lock().unwrap();
    assert!(tool_results(&reqs[1])[0].contains("Not run: the user said no"));
}

#[test]
fn always_stops_asking_for_that_tool() {
    let h = harness(vec![call("write", r#"{"text":"1"}"#), call("write", r#"{"text":"2"}"#), Reply::Text("both done")], "write", Access::Write);
    let (mut c, _) = client(&h);
    let mut asked = 0;
    let view = run_turn(&mut c, "write twice", |step, c| {
        asked += 1;
        c.send(ClientMsg::Approve { step: step.id, answer: ApprovalAnswer::Always });
    });
    assert_eq!(asked, 1);
    assert_eq!(h.runs.load(Ordering::SeqCst), 2);
    assert!(view.reply().steps.iter().all(|s| s.status == StepStatus::Done));
}

#[test]
fn cancel_while_waiting_denies_and_finishes() {
    let h = harness(vec![call("write", r#"{"text":"x"}"#)], "write", Access::Write);
    let (mut c, _) = client(&h);
    let view = run_turn(&mut c, "write it", |_, c| c.send(ClientMsg::Cancel));
    assert_eq!(view.error, Some(Some("stopped".into())));
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
    assert_eq!(view.reply().steps[0].status, StepStatus::Denied);
    // The engine is free again.
    let (mut c2, entries) = client(&h);
    assert!(entries.last().unwrap().steps.iter().all(|s| s.status.finished()));
    c2.send(ClientMsg::Cancel);
}

#[test]
fn late_client_sees_pending_approval_and_can_answer_it() {
    let h = harness(vec![call("write", r#"{"text":"x"}"#), Reply::Text("done from phone")], "write", Access::Write);
    let (mut desktop, _) = client(&h);
    let view = run_turn(&mut desktop, "write it", |step, _| {
        // A phone connects now: the transcript carries the waiting step.
        let (mut phone, entries) = client(&h);
        let pending: Vec<u64> = entries.last().unwrap().pending_approvals().map(|s| s.id).collect();
        assert_eq!(pending, vec![step.id]);
        phone.send(ClientMsg::Approve { step: step.id, answer: ApprovalAnswer::Allow });
        // A second, late answer is ignored.
        phone.send(ClientMsg::Approve { step: step.id, answer: ApprovalAnswer::Deny });
    });
    assert_eq!(h.runs.load(Ordering::SeqCst), 1);
    assert_eq!(view.reply().content, "done from phone");
    assert_eq!(view.reply().steps[0].status, StepStatus::Done);
}

#[test]
fn repeated_identical_calls_are_not_rerun_and_end_the_loop() {
    // The model asks for the same call forever.
    let h = harness(vec![call("echo", r#"{"text":"again"}"#)], "echo", Access::Read);
    let (mut c, _) = client(&h);
    let view = run_turn(&mut c, "loop", |_, _| {});
    assert_eq!(view.error, Some(None));
    assert_eq!(h.runs.load(Ordering::SeqCst), 1);
    assert_eq!(view.reply().steps.len(), 1);
    assert_eq!(view.reply().content, "final answer");
    let reqs = h.requests.lock().unwrap();
    assert_eq!(reqs.len(), 1 + crate::limits::MAX_REPEATS + 1);
    assert!(reqs.last().unwrap().get("tools").is_none(), "tools are switched off after repeats");
    assert!(tool_results(&reqs[2]).last().unwrap().contains("already called echo"));
}

#[test]
fn tool_calls_are_capped_per_turn() {
    let n = crate::limits::MAX_CALLS + 3;
    let calls = (0..n).map(|i| ("echo".to_owned(), format!(r#"{{"text":"{i}"}}"#))).collect();
    let h = harness(vec![Reply::Calls(calls)], "echo", Access::Read);
    let (mut c, _) = client(&h);
    let view = run_turn(&mut c, "go", |_, _| {});
    assert_eq!(h.runs.load(Ordering::SeqCst), crate::limits::MAX_CALLS);
    assert_eq!(view.reply().steps.len(), crate::limits::MAX_CALLS);
    let reqs = h.requests.lock().unwrap();
    assert_eq!(reqs.len(), 2);
    let results = tool_results(&reqs[1]);
    assert_eq!(results.len(), n, "every call gets a result");
    assert!(results.last().unwrap().contains("limit of 30 tool calls"));
    assert!(reqs[1].get("tools").is_none());
    assert_eq!(view.reply().content, "final answer");
}

#[test]
fn malformed_arguments_and_unknown_tools_fail_visibly() {
    let calls = vec![("echo".to_owned(), r#"{"text": "#.to_owned()), ("nope".to_owned(), "{}".to_owned()), ("echo".to_owned(), "[1,2]".to_owned())];
    let h = harness(vec![Reply::Calls(calls), Reply::Text("sorry")], "echo", Access::Read);
    let (mut c, _) = client(&h);
    let view = run_turn(&mut c, "go", |_, _| {});
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
    let steps = &view.reply().steps;
    assert_eq!(steps.len(), 3);
    assert!(steps.iter().all(|s| s.status == StepStatus::Failed));
    let reqs = h.requests.lock().unwrap();
    let results = tool_results(&reqs[1]);
    assert!(results[0].contains("not valid JSON"), "{}", results[0]);
    assert!(results[1].contains("no tool called \"nope\""));
    assert!(results[2].contains("JSON object"));
    assert_eq!(view.reply().content, "sorry");
}

#[test]
fn big_output_is_capped() {
    let h = harness_with(vec![call("big", "{}"), Reply::Text("ok")], "big", Access::Read, Some("x".repeat(100_000)));
    let (mut c, _) = client(&h);
    let view = run_turn(&mut c, "go", |_, _| {});
    assert_eq!(view.reply().steps[0].status, StepStatus::Done);
    let reqs = h.requests.lock().unwrap();
    let result = &tool_results(&reqs[1])[0];
    assert!(result.len() < crate::limits::MAX_OUTPUT + 200);
    assert!(result.contains("cut: showing 16384 of 100000 bytes"));
}

#[test]
fn turn_output_budget_is_shared_across_calls() {
    let calls = (0..6).map(|i| ("big".to_owned(), format!(r#"{{"n":{i}}}"#))).collect();
    let h = harness_with(vec![Reply::Calls(calls), Reply::Text("ok")], "big", Access::Read, Some("y".repeat(15_000)));
    let (mut c, _) = client(&h);
    run_turn(&mut c, "go", |_, _| {});
    let reqs = h.requests.lock().unwrap();
    let results = tool_results(&reqs[1]);
    let total: usize = results.iter().map(String::len).sum();
    assert!(total < crate::limits::TURN_OUTPUT_BUDGET + 6 * 200, "{total}");
    assert!(results[4].contains("budget is used up"));
}

#[test]
fn panicking_tool_fails_the_call_not_the_turn() {
    let h = harness(vec![call("boom", "{}"), Reply::Text("it broke")], "boom", Access::Read);
    let (mut c, _) = client(&h);
    let view = run_turn(&mut c, "go", |_, _| {});
    assert_eq!(view.error, Some(None));
    assert_eq!(view.reply().steps[0].status, StepStatus::Failed);
    assert_eq!(view.reply().content, "it broke");
    assert!(tool_results(&h.requests.lock().unwrap()[1])[0].starts_with("Error: the tool crashed"));
}

#[test]
fn text_before_tool_calls_is_kept_and_separated() {
    let h = harness(vec![Reply::Mixed("Let me check.", "echo", r#"{"text":"a"}"#), Reply::Text("Done.")], "echo", Access::Read);
    let (mut c, _) = client(&h);
    let view = run_turn(&mut c, "go", |_, _| {});
    assert_eq!(view.reply().content, "Let me check.\n\nDone.");
    assert_eq!(view.reply().steps[0].at, "Let me check.".len());
    let reqs = h.requests.lock().unwrap();
    let second = messages(&reqs[1]);
    assert_eq!(second[second.len() - 2]["content"], "Let me check.");
}

#[test]
fn later_turns_send_only_final_text() {
    let h = harness(vec![call("echo", r#"{"text":"a"}"#), Reply::Text("First.")], "echo", Access::Read);
    let (mut c, _) = client(&h);
    run_turn(&mut c, "one", |_, _| {});
    run_turn(&mut c, "two", |_, _| {});
    let reqs = h.requests.lock().unwrap();
    let roles: Vec<String> = messages(reqs.last().unwrap()).iter().map(|m| m["role"].as_str().unwrap_or("").to_owned()).collect();
    assert_eq!(roles, ["system", "user", "assistant", "user"]);
}

#[test]
fn approval_card_then_undo_the_turn() {
    use byob_protocol::{NoticeLevel, Preview};
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(ws.path().join("old.txt"), "one\ntwo\n").unwrap();
    let (url, _) = fake_llm(
        vec![
            call("write_file", r#"{"path":"todo.md","content":"- [ ] eggs\n"}"#),
            call("write_file", r#"{"path":"old.txt","content":"one\nTWO\n"}"#),
            Reply::Text("done"),
        ],
        "done",
    );
    let config = EngineConfig {
        inference: InferenceConfig { base_url: url, ..Default::default() },
        workspace: ws.path().to_path_buf(),
        tools: byob_tools::builtin(),
        policy: Policy::default(),
        system_prompt: crate::turn::DEFAULT_SYSTEM_PROMPT.into(),
        paths: None,
        auto_title: false,
        storage: byob_store::StorageKind::default(),
        chats_folder: None,
        adapt: crate::AdaptOptions { probe: false, ..Default::default() },
        compaction: crate::CompactOptions::default(),
        agents: Vec::new(),
        models: Default::default(),
        subagents: Default::default(),
    };
    let h = Harness { engine: EngineHandle::spawn_with(config).unwrap(), requests: Arc::default(), runs: Arc::default() };
    let (mut c, _) = client(&h);
    let mut cards = Vec::new();
    let view = run_turn(&mut c, "make a todo", |step, c| {
        cards.push(step.clone());
        c.send(ClientMsg::Approve { step: step.id, answer: ApprovalAnswer::Allow });
    });
    // cards ask in the present tense and carry a readable preview
    assert_eq!(cards[0].title, "Write todo.md");
    assert!(matches!(&cards[0].preview, Some(Preview::Write { path, replaces: false, .. }) if path == "todo.md"), "{:?}", cards[0].preview);
    assert!(matches!(&cards[1].preview, Some(Preview::Write { replaces: true, lines, .. }) if lines.len() == 3), "{:?}", cards[1].preview);
    // finished rows say what happened
    let titles: Vec<_> = view.reply().steps.iter().map(|s| s.title.as_str()).collect();
    assert_eq!(titles, ["Wrote todo.md", "Wrote old.txt"]);
    let entry = view.reply().id;

    // a late client sees the reply as undoable
    let (mut late, entries) = client(&h);
    assert!(entries.last().unwrap().undoable);
    late.send(ClientMsg::Undo { entry });
    let mut notice = None;
    let mut flags = None;
    while notice.is_none() || flags.is_none() {
        match recv(&mut late) {
            ServerMsg::Notice { level, text } => notice = Some((level, text)),
            ServerMsg::EntryUndo { id, undoable, undone } => flags = Some((id, undoable, undone)),
            _ => {}
        }
    }
    assert_eq!(notice.as_ref().map(|n| n.0), Some(NoticeLevel::Info), "{notice:?}");
    assert_eq!(flags, Some((entry, false, true)));
    assert!(!ws.path().join("todo.md").exists());
    assert_eq!(std::fs::read_to_string(ws.path().join("old.txt")).unwrap(), "one\ntwo\n");
    // twice is refused
    late.send(ClientMsg::Undo { entry });
    assert!(matches!(recv(&mut late), ServerMsg::Notice { level: NoticeLevel::Error, .. }));
}

/// Switch `c` to a new, empty chat.
fn new_chat(c: &mut LocalConnection) {
    c.send(ClientMsg::NewChat);
    while !matches!(recv(c), ServerMsg::Transcript { .. }) {}
}

fn answer_with(answer: ApprovalAnswer) -> impl FnMut(&Step, &mut LocalConnection) {
    move |step, c| c.send(ClientMsg::Approve { step: step.id, answer })
}

#[test]
fn always_allow_is_per_chat_or_global_and_survives_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let paths = byob_store::Paths::at(dir.path());
    let w = |n: &str| call("write", &format!(r#"{{"text":"{n}"}}"#));
    let script = vec![w("1"), Reply::Text("ok1"), w("2"), Reply::Text("ok2"), w("3"), Reply::Text("ok3")];
    let h = harness_at(script, "write", Access::Write, None, Some(paths.clone()));
    let (mut c, _) = client(&h);
    run_turn(&mut c, "one", answer_with(ApprovalAnswer::Always));
    // Same chat: no question.
    let view = run_turn(&mut c, "two", |_, _| panic!("granted for this chat"));
    assert_eq!(view.reply().content, "ok2");
    // Another chat: asked again; this time for every chat.
    new_chat(&mut c);
    let mut asked = 0;
    let mut everywhere = answer_with(ApprovalAnswer::AlwaysEverywhere);
    run_turn(&mut c, "three", |s, c| {
        asked += 1;
        everywhere(s, c);
    });
    assert_eq!(asked, 1);
    assert_eq!(h.runs.load(Ordering::SeqCst), 3);

    // After a restart the global grant still holds, in a brand-new chat.
    let h2 = harness_at(vec![w("4"), Reply::Text("ok4")], "write", Access::Write, None, Some(paths));
    let (mut c2, entries) = client(&h2);
    // The most recent chat is restored with its steps and file changes.
    assert_eq!(entries.len(), 2);
    let step = &entries[1].steps[0];
    assert_eq!((step.status, step.changes.len()), (StepStatus::Done, 1));
    new_chat(&mut c2);
    let view = run_turn(&mut c2, "four", |_, _| panic!("granted everywhere"));
    assert_eq!(view.reply().content, "ok4");
}

fn artifact_engine(script: Vec<Reply>, workspace: &std::path::Path, paths: byob_store::Paths) -> (EngineHandle, Requests) {
    let (url, requests) = fake_llm(script, "final answer");
    let mut tools = Registry::default();
    tools.register(Arc::new(byob_tools::CreateArtifact));
    tools.register(Arc::new(byob_tools::UpdateArtifact));
    let config = EngineConfig {
        inference: InferenceConfig { base_url: url, ..Default::default() },
        workspace: workspace.to_path_buf(),
        tools,
        policy: Policy::default(),
        system_prompt: crate::turn::DEFAULT_SYSTEM_PROMPT.into(),
        paths: Some(paths),
        auto_title: false,
        storage: byob_store::StorageKind::default(),
        chats_folder: None,
        adapt: crate::AdaptOptions::default(),
        compaction: crate::CompactOptions::default(),
        agents: Vec::new(),
        models: Default::default(),
        subagents: Default::default(),
    };
    (EngineHandle::spawn_with(config).unwrap(), requests)
}

/// Run a turn, also collecting the artifact updates that came with it.
fn artifact_turn(c: &mut LocalConnection, text: &str) -> (View, Vec<byob_protocol::Artifact>) {
    let mut view = View::default();
    let mut updated = Vec::new();
    c.send(ClientMsg::SendMessage { text: text.into(), attachments: vec![] });
    while view.error.is_none() {
        match recv(c) {
            ServerMsg::ArtifactUpdated(a) => updated.push(a),
            other => view.apply(other),
        }
    }
    (view, updated)
}

fn artifact_file(c: &mut LocalConnection) -> ServerMsg {
    loop {
        let m = recv(c);
        if matches!(m, ServerMsg::ArtifactFile { .. }) {
            return m;
        }
    }
}

#[test]
fn artifacts_are_created_versioned_served_and_restored() {
    let dir = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let paths = byob_store::Paths::at(dir.path());
    let html = r#"{"title":"Calc","filename":"calc.html","content":"<button style=\"color:red\">Go</button>"}"#;
    let (engine, _) = artifact_engine(vec![call("create_artifact", html), Reply::Text("Made it")], ws.path(), paths.clone());
    let mut c = engine.connect(Arc::new(|| {}));
    c.send(ClientMsg::Hello { protocol: PROTOCOL_VERSION, client: "test".into() });
    while !matches!(recv(&mut c), ServerMsg::Artifacts { .. }) {}
    // Auto-allowed: no approval card.
    let (view, updated) = artifact_turn(&mut c, "make a calculator");
    let step = &view.reply().steps[0];
    assert_eq!(step.status, StepStatus::Done, "{}", step.detail);
    assert_eq!(updated.len(), 1);
    let a = &updated[0];
    assert_eq!((a.kind, a.versions.len()), (byob_protocol::ArtifactKind::Html, 1));
    assert_eq!(step.artifacts, vec![byob_protocol::ArtifactRef { id: a.id.clone(), version: 1 }]);
    // The client can fetch it; unknown ids are refused.
    c.send(ClientMsg::GetArtifact { id: a.id.clone(), version: 1 });
    let ServerMsg::ArtifactFile { data: Some(data), .. } = artifact_file(&mut c) else { panic!("no file") };
    assert_eq!(byob_protocol::media::decode(&data).unwrap(), br#"<button style="color:red">Go</button>"#);
    c.send(ClientMsg::GetArtifact { id: "nope".into(), version: 1 });
    assert!(matches!(artifact_file(&mut c), ServerMsg::ArtifactFile { data: None, error: Some(_), .. }));
    drop(c);
    drop(engine);

    // After a restart: the step still points at the artifact, the list comes with the
    // chat, the model is told about it, and an edit makes version 2.
    let edit = format!(r#"{{"id":"{}","edits":[{{"find":"color:red","replace":"color:blue"}}],"note":"blue"}}"#, a.id);
    let (engine, requests) = artifact_engine(vec![Reply::Calls(vec![("update_artifact".into(), edit)]), Reply::Text("Done")], ws.path(), paths);
    let mut c = engine.connect(Arc::new(|| {}));
    c.send(ClientMsg::Hello { protocol: PROTOCOL_VERSION, client: "test".into() });
    let mut entries = Vec::new();
    let listed = loop {
        match recv(&mut c) {
            ServerMsg::Transcript { entries: e } => entries = e,
            ServerMsg::Artifacts { artifacts, .. } => break artifacts,
            _ => {}
        }
    };
    assert_eq!(listed, vec![a.clone()]);
    assert_eq!(entries[1].steps[0].artifacts, vec![byob_protocol::ArtifactRef { id: a.id.clone(), version: 1 }]);
    let (view, updated) = artifact_turn(&mut c, "make the button blue");
    assert_eq!(view.reply().steps[0].status, StepStatus::Done, "{}", view.reply().steps[0].detail);
    assert_eq!(updated[0].versions.iter().map(|v| v.n).collect::<Vec<_>>(), vec![1, 2]);
    let system = requests.lock().unwrap()[0]["messages"][0]["content"].as_str().unwrap_or_default().to_owned();
    assert!(system.contains(&format!("{}: \"Calc\" (app, v1, calc.html)", a.id)), "{system}");
    c.send(ClientMsg::GetArtifact { id: a.id.clone(), version: 2 });
    let ServerMsg::ArtifactFile { data: Some(data), .. } = artifact_file(&mut c) else { panic!("no file") };
    assert_eq!(byob_protocol::media::decode(&data).unwrap(), br#"<button style="color:blue">Go</button>"#);
}

// The same tests on the folder backend.
crate::test_store::folder_too!(
    tool_call_then_answer,
    approval_allow_runs_the_tool_and_reports_changes,
    approval_deny_tells_the_model,
    always_stops_asking_for_that_tool,
    cancel_while_waiting_denies_and_finishes,
    late_client_sees_pending_approval_and_can_answer_it,
    repeated_identical_calls_are_not_rerun_and_end_the_loop,
    tool_calls_are_capped_per_turn,
    malformed_arguments_and_unknown_tools_fail_visibly,
    big_output_is_capped,
    turn_output_budget_is_shared_across_calls,
    panicking_tool_fails_the_call_not_the_turn,
    text_before_tool_calls_is_kept_and_separated,
    later_turns_send_only_final_text,
    approval_card_then_undo_the_turn,
    always_allow_is_per_chat_or_global_and_survives_restarts,
    artifacts_are_created_versioned_served_and_restored,
);
