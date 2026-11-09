//! Engine-level tests of endpoint adaptation: prompt-based tool calling, the native → prompt
//! fallback, history budgeting inside tool loops, and the background probe.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use yah_llm::ChatMessage;
use yah_protocol::{ClientMsg, Connection, EngineState, Entry, PROTOCOL_VERSION, ServerMsg, StepStatus, ToolsMode};
use yah_tools::{Access, Registry, Tool, ToolContext, ToolOutput, ToolSpec};
use yah_types::InferenceConfig;

use crate::{AdaptOptions, EngineConfig, EngineHandle, LocalConnection, Policy, ToolMode};

#[derive(Clone)]
enum Reply {
    /// Streamed as content in small chunks (so tags get split).
    Text(String),
    /// A native tool call (name, arguments).
    Native(&'static str, String),
}

/// A call written as text, in the given tag ("function_call" as instructed, or "tool_call").
fn text_call(tag: &str, name: &str, args: Value) -> String {
    format!("<{tag}>\n{}\n</{tag}>", json!({"name": name, "arguments": args}))
}

/// A recorded request: method, path, JSON body.
type Seen = Arc<Mutex<Vec<(String, String, Value)>>>;

#[derive(Clone, Default)]
struct Server {
    /// `GET /v1/models` body (404 if `None`).
    models: Option<Value>,
    /// Delay before answering any metadata request.
    meta_delay: Duration,
    /// Non-streamed `POST /v1/chat/completions` (the probe's tool test) reply.
    tool_test: Option<Value>,
    /// Streamed requests carrying `tools` get HTTP 500 like llama-server without `--jinja`.
    reject_tools: bool,
    /// Streamed replies in order; the last one repeats.
    script: Vec<Reply>,
}

impl Server {
    fn start(self) -> (String, Seen) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen: Seen = Arc::default();
        let log = Arc::clone(&seen);
        let n = Arc::new(AtomicUsize::new(0));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(s) = stream else { continue };
                let (server, log, n) = (self.clone(), Arc::clone(&log), Arc::clone(&n));
                std::thread::spawn(move || server.serve(s, &log, &n));
            }
        });
        (format!("http://{addr}/v1"), seen)
    }

    fn serve(&self, mut s: std::net::TcpStream, log: &Seen, n: &AtomicUsize) {
        let (method, path, body) = read_request(&mut s);
        log.lock().unwrap().push((method.clone(), path.clone(), body.clone()));
        let respond = |s: &mut std::net::TcpStream, status: u16, ctype: &str, body: String| {
            let _ = write!(s, "HTTP/1.1 {status} X\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        };
        if path != "/v1/chat/completions" {
            std::thread::sleep(self.meta_delay);
            match (&path[..], &self.models) {
                ("/v1/models", Some(m)) => respond(&mut s, 200, "application/json", m.to_string()),
                _ => respond(&mut s, 404, "application/json", r#"{"error":{"message":"not found"}}"#.into()),
            }
            return;
        }
        if body["stream"] != true {
            match &self.tool_test {
                Some(v) => respond(&mut s, 200, "application/json", v.to_string()),
                None => respond(&mut s, 404, "application/json", "{}".into()),
            }
            return;
        }
        if self.reject_tools && body.get("tools").is_some() {
            respond(&mut s, 500, "application/json", r#"{"error":{"code":500,"message":"tools param requires --jinja flag"}}"#.into());
            return;
        }
        let i = n.fetch_add(1, Ordering::SeqCst);
        let reply = self.script.get(i).or(self.script.last()).cloned().unwrap_or(Reply::Text("ok".into()));
        respond(&mut s, 200, "text/event-stream", sse(&reply));
    }
}

fn sse(reply: &Reply) -> String {
    let frame = |v: Value| format!("data: {v}\n\n");
    let mut out = String::new();
    match reply {
        Reply::Text(t) => {
            let chars: Vec<char> = t.chars().collect();
            for chunk in chars.chunks(5) {
                out += &frame(json!({"choices":[{"delta":{"content": chunk.iter().collect::<String>()}}]}));
            }
            out += &frame(json!({"choices":[{"delta":{},"finish_reason":"stop"}]}));
        }
        Reply::Native(name, args) => {
            out += &frame(json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"n0","type":"function","function":{"name":name,"arguments":args}}]}}]}));
            out += &frame(json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}));
        }
    }
    out + "data: [DONE]\n\n"
}

fn read_request(s: &mut std::net::TcpStream) -> (String, String, Value) {
    let mut r = BufReader::new(s);
    let mut first = String::new();
    let _ = r.read_line(&mut first);
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
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
    (method, path, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// Echoes `text`, or returns `big` characters.
struct Echo {
    big: usize,
    runs: Arc<AtomicUsize>,
}

impl Tool for Echo {
    fn spec(&self) -> ToolSpec {
        ToolSpec { name: "echo".into(), description: "Echo text back.".into(), parameters: json!({"type":"object","properties":{"text":{"type":"string"}}}) }
    }
    fn title(&self, _args: &Value) -> String {
        "echo".into()
    }
    fn access(&self, _args: &Value) -> Access {
        Access::Read
    }
    fn run(&self, args: &Value, _ctx: &ToolContext<'_>) -> ToolOutput {
        let n = self.runs.fetch_add(1, Ordering::SeqCst);
        if self.big > 0 {
            return ToolOutput::ok(format!("START{n} {} END{n}", "x".repeat(self.big)));
        }
        ToolOutput::ok(args.get("text").and_then(Value::as_str).unwrap_or("").to_owned())
    }
}

struct Harness {
    engine: EngineHandle,
    seen: Seen,
    runs: Arc<AtomicUsize>,
}

fn harness(server: Server, adapt: AdaptOptions, big: usize) -> Harness {
    let (url, seen) = server.start();
    let runs = Arc::new(AtomicUsize::new(0));
    let mut tools = Registry::default();
    tools.register(Arc::new(Echo { big, runs: Arc::clone(&runs) }));
    let config = EngineConfig {
        inference: InferenceConfig { base_url: url, model: "m".into(), api_key: None },
        workspace: PathBuf::from("/ws"),
        tools,
        policy: Policy::default(),
        system_prompt: "You are a test.".into(),
        adapt,
        paths: None,
        auto_title: false,
        // These tests are about fit_history; compaction has its own (compact_tests).
        compaction: crate::CompactOptions { auto: false, ..Default::default() },
    };
    Harness { engine: EngineHandle::spawn_with(config).unwrap(), seen, runs }
}

fn recv(conn: &mut LocalConnection) -> ServerMsg {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(m) = conn.try_recv() {
            return m;
        }
        assert!(Instant::now() < deadline, "timed out waiting for engine");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn connect(h: &Harness) -> (LocalConnection, EngineState) {
    let mut c = h.engine.connect(Arc::new(|| {}));
    c.send(ClientMsg::Hello { protocol: PROTOCOL_VERSION, client: "test".into() });
    let ServerMsg::State(state) = recv(&mut c) else { panic!("expected state") };
    // Then Chats, ChatOpened, and finally the Transcript.
    while !matches!(recv(&mut c), ServerMsg::Transcript { .. }) {}
    (c, state)
}

/// Wait for a State broadcast matching `want`.
fn wait_state(c: &mut LocalConnection, want: impl Fn(&EngineState) -> bool) -> EngineState {
    loop {
        if let ServerMsg::State(s) = recv(c)
            && want(&s)
        {
            return s;
        }
    }
}

/// Send `text` and collect the reply entry; also returns the last State seen.
fn turn(c: &mut LocalConnection, text: &str) -> (Entry, Option<EngineState>) {
    c.send(ClientMsg::SendMessage { text: text.into(), attachments: vec![] });
    let mut entries: Vec<Entry> = Vec::new();
    let mut state = None;
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
            ServerMsg::State(s) => state = Some(s),
            ServerMsg::EntryFinished { id, error, .. } => {
                assert_eq!(error, None);
                let mut e = entries.into_iter().find(|e| e.id == id).unwrap();
                e.done = true;
                return (e, state);
            }
            _ => {}
        }
    }
}

/// Streamed chat requests (not the probe's tool test), in order.
fn chats(seen: &Seen) -> Vec<Value> {
    seen.lock().unwrap().iter().filter(|(_, p, b)| p == "/v1/chat/completions" && b["stream"] == true).map(|(_, _, b)| b.clone()).collect()
}

fn messages(req: &Value) -> Vec<Value> {
    req["messages"].as_array().cloned().unwrap_or_default()
}

fn no_probe(tool_mode: ToolMode) -> AdaptOptions {
    AdaptOptions { tool_mode, probe: false }
}

#[test]
fn prompt_mode_runs_tools_from_reply_text() {
    let script =
        vec![Reply::Text(format!("Let me check.\n{}", text_call("function_call", "echo", json!({"text": "pong"})))), Reply::Text("The answer is pong".into())];
    let h = harness(Server { script, ..Default::default() }, no_probe(ToolMode::Prompt), 0);
    let (mut c, state) = connect(&h);
    assert_eq!(state.endpoint_info.unwrap().tools, ToolsMode::Prompt);
    let (reply, _) = turn(&mut c, "ping?");
    // Markup never reaches clients; the step ran.
    assert_eq!(reply.content, "Let me check.\n\nThe answer is pong");
    assert_eq!(reply.steps.len(), 1);
    assert_eq!((reply.steps[0].status, reply.steps[0].detail.as_str()), (StepStatus::Done, "pong"));
    assert_eq!(reply.steps[0].at, "Let me check.\n".len());
    assert_eq!(h.runs.load(Ordering::SeqCst), 1);

    let reqs = chats(&h.seen);
    assert_eq!(reqs.len(), 2);
    assert!(reqs.iter().all(|r| r.get("tools").is_none()), "no native tools in prompt mode");
    let sys = messages(&reqs[0])[0]["content"].as_str().unwrap().to_owned();
    assert!(sys.starts_with("You are a test.") && sys.contains("<tools>") && sys.contains("\"name\":\"echo\""));
    let second = messages(&reqs[1]);
    let n = second.len();
    assert_eq!(second[n - 2]["role"], "assistant");
    assert!(second[n - 2]["content"].as_str().unwrap().contains("<function_call>"));
    assert_eq!(second[n - 1]["role"], "user");
    assert_eq!(second[n - 1]["content"], "<tool_response name=\"echo\">\npong\n</tool_response>");
    assert!(second.iter().all(|m| m["role"] != "tool" && m.get("tool_calls").is_none()));
}

#[test]
fn prompt_mode_accepts_calls_the_server_parsed_itself() {
    // Like llama-server behind a parsing proxy: call text comes back as structured tool_calls.
    let script = vec![Reply::Native("echo", r#"{"text":"pong"}"#.into()), Reply::Text("got pong".into())];
    let h = harness(Server { script, ..Default::default() }, no_probe(ToolMode::Prompt), 0);
    let (mut c, _) = connect(&h);
    let (reply, _) = turn(&mut c, "ping?");
    assert_eq!(reply.content, "got pong");
    assert_eq!(h.runs.load(Ordering::SeqCst), 1);
    let reqs = chats(&h.seen);
    let last = messages(&reqs[1]).pop().unwrap();
    assert_eq!(last["content"], "<tool_response name=\"echo\">\npong\n</tool_response>");
}

#[test]
fn rejected_native_tools_fall_back_to_prompt_mode_and_stick() {
    let script = vec![Reply::Text(text_call("tool_call", "echo", json!({"text": "pong"}))), Reply::Text("pong it is".into())];
    let h = harness(Server { script, reject_tools: true, ..Default::default() }, no_probe(ToolMode::Auto), 0);
    let (mut c, state) = connect(&h);
    assert_eq!(state.endpoint_info.unwrap().tools, ToolsMode::Unknown);
    let (reply, state) = turn(&mut c, "ping?");
    assert_eq!(reply.content, "pong it is");
    assert_eq!(reply.steps[0].status, StepStatus::Done);
    assert_eq!(state.unwrap().endpoint_info.unwrap().tools, ToolsMode::Prompt);
    {
        let all = h.seen.lock().unwrap();
        let tried: Vec<bool> = all.iter().filter(|(_, _, b)| b["stream"] == true).map(|(_, _, b)| b.get("tools").is_some()).collect();
        assert_eq!(tried, [true, false, false], "native once, then the retry and the next round in prompt mode");
    }
    // The next turn goes straight to prompt mode.
    let (reply, _) = turn(&mut c, "again");
    assert_eq!(reply.content, "pong it is");
    let all = h.seen.lock().unwrap();
    assert_eq!(all.iter().filter(|(_, _, b)| b.get("tools").is_some()).count(), 1);
}

#[test]
fn forced_native_mode_does_not_fall_back() {
    let h = harness(Server { reject_tools: true, ..Default::default() }, no_probe(ToolMode::Native), 0);
    let (mut c, _) = connect(&h);
    c.send(ClientMsg::SendMessage { text: "hi".into(), attachments: vec![] });
    loop {
        if let ServerMsg::EntryFinished { error, .. } = recv(&mut c) {
            assert!(error.unwrap().contains("jinja"));
            break;
        }
    }
    assert_eq!(chats(&h.seen).len(), 1);
}

#[test]
fn long_tool_loop_stays_within_probed_context() {
    // The probe finds a 4096-token llama.cpp server with working native tools.
    let models = json!({"data": [{"id": "m", "owned_by": "llamacpp", "meta": {"n_ctx": 4096}}]});
    let tool_test = json!({"choices": [{"message": {"tool_calls": [{"id": "1", "type": "function", "function": {"name": "get_magic_number", "arguments": "{}"}}]}, "finish_reason": "tool_calls"}]});
    let mut script: Vec<Reply> = (0..6).map(|i| Reply::Native("echo", format!(r#"{{"text":"{i}"}}"#))).collect();
    script.push(Reply::Text("done".into()));
    let server = Server { models: Some(models), tool_test: Some(tool_test), script, ..Default::default() };
    // Each result is ~9k characters (~2.6k estimated tokens): two don't fit together.
    let h = harness(server, AdaptOptions { tool_mode: ToolMode::Auto, probe: true }, 9_000);
    let (mut c, _) = connect(&h);
    let state = wait_state(&mut c, |s| s.endpoint_info.as_ref().is_some_and(|i| !i.probing));
    let info = state.endpoint_info.unwrap();
    assert_eq!((info.context_tokens, info.tools, info.server.as_str()), (Some(4096), ToolsMode::Native, "llama.cpp"));

    let (reply, _) = turn(&mut c, "go");
    assert_eq!(reply.content, "done");
    assert_eq!(h.runs.load(Ordering::SeqCst), 6);
    let reqs = chats(&h.seen);
    assert_eq!(reqs.len(), 7);
    let tools_cost = yah_llm::budget::estimate_tools_tokens(&[yah_llm::ToolSpec {
        name: "echo".into(),
        description: "Echo text back.".into(),
        parameters: json!({"type":"object","properties":{"text":{"type":"string"}}}),
    }]);
    for (round, req) in reqs.iter().enumerate() {
        let msgs: Vec<ChatMessage> = serde_json::from_value(req["messages"].clone()).unwrap();
        let used = yah_llm::estimate_history_tokens(&msgs) + if req.get("tools").is_some() { tools_cost } else { 0 };
        assert!(used <= 4096 - 1024, "round {round}: {used} tokens");
        // Every round's call and result are still there; the newest result keeps head and tail.
        assert_eq!(msgs.iter().filter(|m| m.role == "tool").count(), round);
        if round > 0 {
            let last = msgs.last().unwrap().content.clone().unwrap();
            let k = round - 1;
            assert!(last.starts_with(&format!("START{k}")) && last.ends_with(&format!("END{k}")), "{}", &last[..40]);
        }
    }
}

#[test]
fn probe_runs_in_the_background() {
    // Metadata requests hang for a while; the engine must answer and run turns meanwhile.
    let models = json!({"data": [{"id": "m", "owned_by": "llamacpp", "meta": {"n_ctx": 8192}}]});
    let server = Server { models: Some(models), meta_delay: Duration::from_millis(1500), script: vec![Reply::Text("hello".into())], ..Default::default() };
    let h = harness(server, AdaptOptions { tool_mode: ToolMode::Auto, probe: true }, 0);
    let started = Instant::now();
    let (mut c, state) = connect(&h);
    assert!(state.endpoint_info.unwrap().probing);
    let (reply, _) = turn(&mut c, "hi");
    assert_eq!(reply.content, "hello");
    assert!(started.elapsed() < Duration::from_millis(1200), "blocked on the probe: {:?}", started.elapsed());
    let state = wait_state(&mut c, |s| s.endpoint_info.as_ref().is_some_and(|i| !i.probing));
    assert_eq!(state.endpoint_info.unwrap().context_tokens, Some(8192));
    // Re-applying the same settings uses the cache instead of probing again.
    let probes = || h.seen.lock().unwrap().iter().filter(|(_, p, _)| p == "/v1/models").count();
    assert_eq!(probes(), 1);
    c.send(ClientMsg::SetInference { base_url: state.inference_base_url.clone(), model: "m".into(), api_key: None });
    let state = wait_state(&mut c, |_| true);
    assert!(!state.endpoint_info.unwrap().probing);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(probes(), 1);
}

#[test]
fn probe_results_only_reach_clients_that_said_hello() {
    let models = json!({"data": [{"id": "m", "owned_by": "llamacpp", "meta": {"n_ctx": 8192}}]});
    let h = harness(Server { models: Some(models), ..Default::default() }, AdaptOptions { tool_mode: ToolMode::Auto, probe: true }, 0);
    let mut quiet = h.engine.connect(Arc::new(|| {}));
    let (mut c, _) = connect(&h);
    wait_state(&mut c, |s| s.endpoint_info.as_ref().is_some_and(|i| !i.probing));
    assert!(quiet.try_recv().is_none(), "a client must say Hello before it gets broadcasts");
}
