//! Engine-level tests of context compaction and the usage meter against a scripted fake
//! server: automatic compaction before a request and inside a tool loop, manual `/compact`,
//! history rebuilt from a stored compaction after a restart, and failed summaries.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use byob_protocol::{ChatUsage, ClientMsg, Compaction, Connection, Entry, NoticeLevel, PROTOCOL_VERSION, ServerMsg};
use byob_store::Paths;
use byob_tools::{Access, Registry, Tool, ToolContext, ToolOutput, ToolSpec};
use byob_types::InferenceConfig;
use serde_json::{Value, json};

use crate::compact::{SUMMARY_ACK, SUMMARY_HEADER, SYSTEM};
use crate::{CompactOptions, EngineConfig, EngineHandle, LocalConnection, Policy};

#[derive(Clone)]
enum Reply {
    Text(String),
    Call(&'static str, String),
}

/// What the fake server does with summary requests.
#[derive(Clone, Copy, PartialEq)]
enum Summaries {
    Answer,
    Fail,
    /// A summary far longer than the conversation.
    Long,
}

type Seen = Arc<Mutex<Vec<Value>>>;

fn is_summary_request(body: &Value) -> bool {
    body["messages"][0]["content"].as_str().is_some_and(|s| s == SYSTEM)
}

/// Streams `script` in order to normal requests (the last repeats); summary requests get
/// "SUMMARY-<n>" (or HTTP 500). Every reply reports usage 100 in / 7 out.
fn server(script: Vec<Reply>, summaries: Summaries) -> (String, Seen) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Seen = Arc::default();
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        let (mut n, mut s_n) = (0usize, 0usize);
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let body = read_body(&mut s);
            let summary = is_summary_request(&body);
            log.lock().unwrap().push(body);
            let frame = |v: Value| format!("data: {v}\n\n");
            let usage = json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":7}});
            let out = if summary {
                s_n += 1;
                if summaries == Summaries::Fail {
                    let body = r#"{"error":{"message":"out of memory"}}"#;
                    let _ = write!(s, "HTTP/1.1 500 X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    continue;
                }
                let text = if summaries == Summaries::Long { "padding ".repeat(4000) } else { String::new() };
                frame(json!({"choices":[{"delta":{"content": format!("## User goals\nSUMMARY-{s_n} {text}")}}]})) + &frame(usage)
            } else {
                let reply = script.get(n).or(script.last()).cloned().unwrap_or(Reply::Text("ok".into()));
                n += 1;
                match reply {
                    Reply::Text(t) => frame(json!({"choices":[{"delta":{"content": t}}]})) + &frame(usage),
                    Reply::Call(name, args) => {
                        frame(json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c0","type":"function","function":{"name":name,"arguments":args}}]}}]}))
                            + &frame(usage)
                    }
                }
            };
            let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{out}data: [DONE]\n\n");
        }
    });
    (format!("http://{addr}/v1"), seen)
}

fn read_body(s: &mut std::net::TcpStream) -> Value {
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

/// Returns ~3000 characters per call.
struct Big;

impl Tool for Big {
    fn spec(&self) -> ToolSpec {
        ToolSpec { name: "big".into(), description: "Big output.".into(), parameters: json!({"type":"object"}) }
    }
    fn title(&self, _args: &Value) -> String {
        "big".into()
    }
    fn access(&self, _args: &Value) -> Access {
        Access::Read
    }
    fn run(&self, args: &Value, _ctx: &ToolContext<'_>) -> ToolOutput {
        ToolOutput::ok(format!("RESULT {} {}", args, "data ".repeat(600)))
    }
}

fn engine(url: String, compaction: CompactOptions, tools: bool, paths: Option<Paths>) -> EngineHandle {
    let mut registry = Registry::default();
    if tools {
        registry.register(Arc::new(Big));
    }
    let config = EngineConfig {
        inference: InferenceConfig { base_url: url, model: "m".into(), api_key: None },
        workspace: PathBuf::from("/ws"),
        tools: registry,
        policy: Policy::default(),
        system_prompt: "You are a test.".into(),
        adapt: crate::AdaptOptions { probe: false, ..Default::default() },
        paths,
        auto_title: false,
        storage: byob_store::StorageKind::default(),
        chats_folder: None,
        compaction,
    };
    EngineHandle::spawn_with(config).unwrap()
}

fn at(tokens: usize) -> CompactOptions {
    CompactOptions { at_tokens: Some(tokens), keep_turns: 1, ..Default::default() }
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

/// What a UI would keep.
#[derive(Default)]
struct View {
    entries: Vec<Entry>,
    compactions: Vec<Compaction>,
    usage: Option<ChatUsage>,
    notices: Vec<(NoticeLevel, String)>,
}

impl View {
    fn apply(&mut self, m: ServerMsg) {
        match m {
            ServerMsg::Transcript { entries } => self.entries = entries,
            ServerMsg::EntryAdded(e) => self.entries.push(e),
            ServerMsg::EntryDelta { id, content, reasoning } => {
                if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
                    e.append(&content, &reasoning);
                }
            }
            ServerMsg::EntryFinished { id, error, stats } => {
                if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
                    (e.done, e.error, e.stats) = (true, error, stats);
                }
            }
            ServerMsg::Compactions { items, .. } => self.compactions = items,
            ServerMsg::Compacted(c) => self.compactions.push(c),
            ServerMsg::Usage(u) => self.usage = Some(u),
            ServerMsg::Notice { level, text } => self.notices.push((level, text)),
            _ => {}
        }
    }
}

fn connect(engine: &EngineHandle) -> (LocalConnection, View) {
    let mut c = engine.connect(Arc::new(|| {}));
    c.send(ClientMsg::Hello { protocol: PROTOCOL_VERSION, client: "test".into() });
    let mut v = View::default();
    loop {
        let m = recv(&mut c);
        let end = matches!(m, ServerMsg::Artifacts { .. });
        v.apply(m);
        if end {
            return (c, v);
        }
    }
}

/// Send `text`, wait for the reply to finish (no error) and the chat to be idle.
fn turn(c: &mut LocalConnection, v: &mut View, text: &str) {
    c.send(ClientMsg::SendMessage { text: text.into(), attachments: vec![] });
    loop {
        let m = recv(c);
        let done = matches!(&m, ServerMsg::EntryFinished { .. });
        if let ServerMsg::EntryFinished { error, .. } = &m {
            assert_eq!(error, &None);
        }
        v.apply(m);
        if done {
            break;
        }
    }
    settle(c, v);
}

/// Let the engine send what follows (usage, state).
fn settle(c: &mut LocalConnection, v: &mut View) {
    std::thread::sleep(Duration::from_millis(60));
    while let Some(m) = c.try_recv() {
        v.apply(m);
    }
}

/// ~150 estimated tokens.
fn question(i: usize) -> String {
    format!("question {i} {}", "word ".repeat(100))
}

fn texts(req: &Value) -> Vec<String> {
    req["messages"].as_array().unwrap().iter().map(|m| m["content"].as_str().unwrap_or_default().to_owned()).collect()
}

fn normal(seen: &Seen) -> Vec<Value> {
    seen.lock().unwrap().iter().filter(|b| !is_summary_request(b)).cloned().collect()
}

fn summaries(seen: &Seen) -> Vec<Value> {
    seen.lock().unwrap().iter().filter(|b| is_summary_request(b)).cloned().collect()
}

#[test]
fn compacts_before_a_request_over_the_threshold_and_keeps_the_newest_turn() {
    let (url, seen) = server(vec![Reply::Text("answer".into())], Summaries::Answer);
    let engine = engine(url, at(700), false, None);
    let (mut c, mut v) = connect(&engine);
    for i in 1..=4 {
        turn(&mut c, &mut v, &question(i));
    }
    assert!(summaries(&seen).is_empty(), "under the threshold");
    turn(&mut c, &mut v, &question(5));
    let sums = summaries(&seen);
    assert_eq!(sums.len(), 1);
    // The summarizer saw the old turns but not the kept one or the new message.
    let asked = texts(&sums[0])[1].clone();
    assert!(asked.contains("question 1") && asked.contains("question 3") && !asked.contains("question 4") && !asked.contains("question 5"));
    // The request went on from the summary, the newest turn and the new message.
    let reqs = normal(&seen);
    let last = texts(reqs.last().unwrap());
    assert_eq!(last.len(), 6, "{last:?}");
    assert!(last[1].starts_with(SUMMARY_HEADER) && last[1].ends_with("SUMMARY-1"));
    assert_eq!(last[2], SUMMARY_ACK);
    assert!(last[3].starts_with("question 4") && last[5].starts_with("question 5"));
    // Clients got the compaction, anchored after the last covered entry (turn 3's reply).
    assert_eq!(v.compactions.len(), 1);
    let comp = &v.compactions[0];
    assert_eq!(comp.after_entry, v.entries[5].id);
    assert!(comp.tokens_before > 700 && comp.tokens_after < comp.tokens_before && !comp.manual);
    assert!(comp.summary.contains("SUMMARY-1"));
    // The transcript still shows everything.
    assert_eq!(v.entries.len(), 10);
    // Usage: 5 replies + 1 summary, each 100 in / 7 out; the summary counts in turn 5's stats.
    let u = v.usage.clone().unwrap();
    assert_eq!((u.model_calls, u.prompt_tokens, u.completion_tokens, u.compactions), (6, 600, 42, 1));
    assert_eq!((u.last_prompt_tokens, u.context_used, u.estimated, u.compact_at), (100, 107, false, 700));
    assert_eq!(v.entries[9].stats.unwrap().model_calls, 2);
    // The next turn builds on the summary too.
    turn(&mut c, &mut v, "and then?");
    let last = texts(normal(&seen).last().unwrap());
    assert!(last[1].ends_with("SUMMARY-1") && last[3].starts_with("question 4"));
}

#[test]
fn compacts_inside_a_long_tool_loop() {
    let script = vec![Reply::Call("big", r#"{"n":1}"#.into()), Reply::Call("big", r#"{"n":2}"#.into()), Reply::Text("done".into())];
    let (url, seen) = server(script, Summaries::Answer);
    let engine = engine(url, at(1500), true, None);
    let (mut c, mut v) = connect(&engine);
    turn(&mut c, &mut v, "go");
    let reqs = normal(&seen);
    assert_eq!(reqs.len(), 3);
    assert_eq!(summaries(&seen).len(), 1);
    // Round 2 (two ~860-token results) passed the threshold: it went on from the summary
    // and a copy of the request; the summarizer had seen both results.
    let round2 = texts(&reqs[2]);
    assert_eq!(round2.len(), 4, "{round2:?}");
    assert!(round2[1].ends_with("SUMMARY-1"));
    assert!(round2[3].starts_with("go\n\n[Note: the context was compacted"));
    assert!(reqs[2].get("tools").is_some(), "tools stay on");
    let asked = texts(&summaries(&seen)[0])[1].clone();
    assert!(asked.contains(r#"RESULT {"n":1}"#) && asked.contains(r#"RESULT {"n":2}"#));
    assert_eq!(v.entries[1].content, "done");
    assert_eq!(v.compactions.len(), 1);
    assert_eq!(v.compactions[0].after_entry, 0, "nothing stored before this turn");
    let u = v.usage.unwrap();
    assert_eq!((u.model_calls, u.compactions), (4, 1));
}

#[test]
fn history_is_rebuilt_from_the_compaction_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::at(dir.path());
    let (url, seen) = server(vec![Reply::Text("answer".into())], Summaries::Answer);
    {
        let engine = engine(url.clone(), at(700), false, Some(paths.clone()));
        let (mut c, mut v) = connect(&engine);
        for i in 1..=5 {
            turn(&mut c, &mut v, &question(i));
        }
        assert_eq!(v.compactions.len(), 1);
    }
    // No automatic compaction this time: what is sent comes from the stored one.
    let engine = engine(url, CompactOptions { auto: false, ..Default::default() }, false, Some(paths));
    let (mut c, mut v) = connect(&engine);
    assert_eq!(v.entries.len(), 10, "the transcript keeps every message");
    assert_eq!(v.compactions.len(), 1);
    assert_eq!(v.compactions[0].after_entry, v.entries[5].id);
    let u = v.usage.clone().unwrap();
    assert_eq!((u.model_calls, u.compactions, u.compact_at, u.estimated), (6, 1, 0, true));
    assert!(u.context_used < 700, "estimated from the compacted history: {}", u.context_used);
    turn(&mut c, &mut v, "after restart");
    let last = texts(normal(&seen).last().unwrap());
    assert!(last[1].ends_with("SUMMARY-1"), "{last:?}");
    assert!(last[3].starts_with("question 4") && last[5].starts_with("question 5") && last[7] == "after restart");
    assert!(!last.iter().any(|t| t.starts_with("question 1")));
}

#[test]
fn manual_compaction_with_focus() {
    let (url, seen) = server(vec![Reply::Text("answer".into())], Summaries::Answer);
    let engine = engine(url, CompactOptions::default(), false, None);
    let (mut c, mut v) = connect(&engine);
    // Nothing to compact in a new chat.
    c.send(ClientMsg::Compact { focus: None });
    let ServerMsg::Notice { text, .. } = recv(&mut c) else { panic!("expected a notice") };
    assert!(text.contains("Nothing to compact"));
    turn(&mut c, &mut v, &question(1));
    turn(&mut c, &mut v, &question(2));
    c.send(ClientMsg::SendMessage { text: "/compact keep the CSV analysis details".into(), attachments: vec![] });
    loop {
        let m = recv(&mut c);
        let done = matches!(m, ServerMsg::Compacted(_));
        v.apply(m);
        if done {
            break;
        }
    }
    settle(&mut c, &mut v);
    let sums = summaries(&seen);
    assert_eq!(sums.len(), 1);
    let asked = texts(&sums[0])[1].clone();
    assert!(asked.contains("focus on: keep the CSV analysis details") && asked.contains("### User\nquestion 2"));
    let comp = v.compactions.last().unwrap();
    assert!(comp.manual && comp.focus.as_deref() == Some("keep the CSV analysis details"));
    assert_eq!(comp.after_entry, v.entries.last().unwrap().id, "covers everything");
    assert_eq!(v.entries.len(), 4, "/compact is not a message");
    let u = v.usage.clone().unwrap();
    assert!(!u.compacting && u.estimated);
    assert_eq!((u.model_calls, u.compactions), (3, 1));
    // The next request starts from the summary alone.
    turn(&mut c, &mut v, "third");
    let last = texts(normal(&seen).last().unwrap());
    assert_eq!(last.len(), 4, "{last:?}");
    assert!(last[1].ends_with("SUMMARY-1") && last[3] == "third");
    // Compacting again right away has nothing new except that turn; after it, nothing.
    c.send(ClientMsg::Compact { focus: None });
    while !matches!(recv(&mut c), ServerMsg::Compacted(_)) {}
    c.send(ClientMsg::Compact { focus: None });
    let (_, text) = loop {
        if let ServerMsg::Notice { level, text } = recv(&mut c) {
            break (level, text);
        }
    };
    assert!(text.contains("Nothing new"), "{text}");
}

#[test]
fn failed_summary_falls_back_to_fit_history_and_the_turn_answers() {
    let (url, seen) = server(vec![Reply::Text("answer".into())], Summaries::Fail);
    let engine = engine(url, at(700), false, None);
    let (mut c, mut v) = connect(&engine);
    for i in 1..=5 {
        turn(&mut c, &mut v, &question(i));
    }
    assert_eq!(summaries(&seen).len(), 1);
    assert!(v.compactions.is_empty());
    assert!(v.notices.iter().any(|(l, t)| *l == NoticeLevel::Info && t.contains("Couldn't summarise") && t.contains("out of memory")), "{:?}", v.notices);
    // The request went out with the full history (it fits the 32k default window).
    let last = texts(normal(&seen).last().unwrap());
    assert!(last[1].starts_with("question 1") && last.len() == 10);
    assert_eq!(v.entries[9].content, "answer");
    // A manual compaction that fails says so and changes nothing.
    c.send(ClientMsg::Compact { focus: None });
    let text = loop {
        if let ServerMsg::Notice { level: NoticeLevel::Error, text } = recv(&mut c) {
            break text;
        }
    };
    assert!(text.contains("couldn't compact") && text.contains("out of memory"));
    settle(&mut c, &mut v);
    assert!(!v.usage.as_ref().unwrap().compacting);
    turn(&mut c, &mut v, "still works");
}

#[test]
fn a_summary_longer_than_the_conversation_is_not_used() {
    let (url, seen) = server(vec![Reply::Text("answer".into())], Summaries::Long);
    let engine = engine(url, at(700), false, None);
    let (mut c, mut v) = connect(&engine);
    for i in 1..=5 {
        turn(&mut c, &mut v, &question(i));
    }
    assert_eq!(summaries(&seen).len(), 1);
    assert!(v.compactions.is_empty());
    assert!(v.notices.iter().any(|(_, t)| t.contains("came out longer")), "{:?}", v.notices);
    assert_eq!(texts(normal(&seen).last().unwrap()).len(), 10, "the full history went out");
    c.send(ClientMsg::Compact { focus: None });
    let text = loop {
        if let ServerMsg::Notice { text, .. } = recv(&mut c) {
            break text;
        }
    };
    assert!(text.contains("came out longer"), "{text}");
    settle(&mut c, &mut v);
    assert!(v.compactions.is_empty() && !v.usage.as_ref().unwrap().compacting);
}

#[test]
fn stopping_a_manual_compaction() {
    // A server that never answers summary requests.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let held = Arc::new(AtomicUsize::new(0));
    let h = Arc::clone(&held);
    std::thread::spawn(move || {
        let mut keep = Vec::new();
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let body = read_body(&mut s);
            if is_summary_request(&body) {
                // Start the stream, then go quiet (the client checks its flag between chunks).
                let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n");
                for _ in 0..200 {
                    let _ = write!(s, "data: {}\n\n", json!({"choices":[{"delta":{"reasoning_content":"hmm "}}]}));
                    let _ = s.flush();
                    std::thread::sleep(Duration::from_millis(20));
                }
                h.fetch_add(1, Ordering::SeqCst);
                keep.push(s);
                continue;
            }
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"delta":{"content":"hi"}}]})
            );
        }
    });
    let engine = engine(format!("http://{addr}/v1"), CompactOptions::default(), false, None);
    let (mut c, mut v) = connect(&engine);
    turn(&mut c, &mut v, "first");
    c.send(ClientMsg::Compact { focus: None });
    loop {
        if let ServerMsg::State(s) = recv(&mut c)
            && s.busy
        {
            break;
        }
    }
    c.send(ClientMsg::Cancel);
    let text = loop {
        if let ServerMsg::Notice { text, .. } = recv(&mut c) {
            break text;
        }
    };
    assert_eq!(text, "Compaction stopped.");
    settle(&mut c, &mut v);
    assert!(v.compactions.is_empty());
    turn(&mut c, &mut v, "second");
}

// The same tests on the folder backend.
crate::test_store::folder_too!(
    compacts_before_a_request_over_the_threshold_and_keeps_the_newest_turn,
    compacts_inside_a_long_tool_loop,
    history_is_rebuilt_from_the_compaction_after_a_restart,
    manual_compaction_with_focus,
    failed_summary_falls_back_to_fit_history_and_the_turn_answers,
    a_summary_longer_than_the_conversation_is_not_used,
    stopping_a_manual_compaction,
);
