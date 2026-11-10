//! Engine tests of message queueing against a fake server whose replies are steered by the
//! user's text: "SLOW" waits for the test to release it, "FAILONCE" fails the first time,
//! summary requests are answered at once.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use byob_protocol::queue::{MAX_QUEUED, MAX_QUEUED_TEXT_BYTES};
use byob_protocol::{ChatQueue, ClientMsg, Connection, Entry, EntryRole, NoticeLevel, QueuePause, ServerMsg};
use byob_types::InferenceConfig;
use serde_json::{Value, json};

use crate::compact::SYSTEM;
use crate::{CompactOptions, EngineConfig, EngineHandle, LocalConnection};

/// Per request: the last user message, or "SUMMARY".
type Seen = Arc<Mutex<Vec<String>>>;

fn last_user(body: &Value) -> String {
    let msgs = body["messages"].as_array().cloned().unwrap_or_default();
    let Some(m) = msgs.iter().rev().find(|m| m["role"] == "user") else { return String::new() };
    match &m["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join(" "),
        _ => String::new(),
    }
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

fn server() -> (String, Seen, Sender<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Seen = Arc::default();
    let log = Arc::clone(&seen);
    let (release, gate): (Sender<()>, Receiver<()>) = channel();
    let gate = Arc::new(Mutex::new(gate));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let (log, gate) = (Arc::clone(&log), Arc::clone(&gate));
            std::thread::spawn(move || {
                let body = read_body(&mut s);
                let frame = |v: Value| format!("data: {v}\n\n");
                let usage = frame(json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":7}}));
                if body["messages"][0]["content"].as_str() == Some(SYSTEM) {
                    log.lock().unwrap().push("SUMMARY".into());
                    let out = frame(json!({"choices":[{"delta":{"content": "## User goals\nSUMMARY-1"}}]})) + &usage;
                    let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{out}data: [DONE]\n\n");
                    return;
                }
                let text = last_user(&body);
                let first_time = {
                    let mut l = log.lock().unwrap();
                    let first = !l.contains(&text);
                    l.push(text.clone());
                    first
                };
                if text.contains("FAILONCE") && first_time {
                    let body = r#"{"error":{"message":"bad request"}}"#;
                    let _ = write!(s, "HTTP/1.1 400 X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    return;
                }
                let short: String = text.chars().take(20).collect();
                let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n");
                let _ = write!(s, "{}", frame(json!({"choices":[{"delta":{"content": format!("re: {short}")}}]})));
                let _ = s.flush();
                if text.contains("SLOW") {
                    let _ = gate.lock().unwrap().recv_timeout(Duration::from_secs(10));
                }
                let _ = write!(s, "{usage}data: [DONE]\n\n");
            });
        }
    });
    (format!("http://{addr}/v1"), seen, release)
}

fn engine(url: String, compaction: CompactOptions) -> EngineHandle {
    let mut config = EngineConfig::new(InferenceConfig { base_url: url, model: "m".into(), api_key: None });
    config.workspace = PathBuf::from("/ws");
    config.system_prompt = "You are a test.".into();
    config.auto_title = false;
    config.compaction = compaction;
    EngineHandle::spawn_with(config).unwrap()
}

/// What a UI would keep.
#[derive(Default)]
struct View {
    entries: Vec<Entry>,
    queue: ChatQueue,
    notices: Vec<(NoticeLevel, String)>,
    busy: bool,
    compactions: usize,
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
            ServerMsg::Queue(q) => self.queue = q,
            ServerMsg::State(s) => self.busy = s.busy,
            ServerMsg::Notice { level, text } => self.notices.push((level, text)),
            ServerMsg::Compacted(_) => self.compactions += 1,
            _ => {}
        }
    }

    fn queued(&self) -> Vec<&str> {
        self.queue.items.iter().map(|i| i.text.as_str()).collect()
    }

    fn id(&self, text: &str) -> u64 {
        self.queue.items.iter().find(|i| i.text == text).map(|i| i.id).unwrap()
    }

    fn users(&self) -> Vec<&str> {
        self.entries.iter().filter(|e| e.role == EntryRole::User).map(|e| e.content.as_str()).collect()
    }

    /// Nothing runs, every reply finished, the queue is empty or paused.
    fn idle(&self) -> bool {
        !self.busy && self.entries.iter().all(|e| e.done) && (self.queue.items.is_empty() || self.queue.paused.is_some())
    }
}

fn connect(engine: &EngineHandle) -> (LocalConnection, View) {
    let mut c = engine.connect(Arc::new(|| {}));
    c.send(ClientMsg::hello("test"));
    let mut v = View::default();
    pump_until(&mut c, &mut v, |_| false, true);
    (c, v)
}

/// Apply messages until `done` holds (or, with `hello`, until the end of opening a chat).
fn pump_until(c: &mut LocalConnection, v: &mut View, done: impl Fn(&View) -> bool, hello: bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if !hello && done(v) {
            return;
        }
        match c.try_recv() {
            Some(m) => {
                let end = hello && matches!(m, ServerMsg::Artifacts { .. });
                v.apply(m);
                if end {
                    return;
                }
            }
            None => {
                assert!(Instant::now() < deadline, "timed out; queue {:?}, busy {}, users {:?}", v.queued(), v.busy, v.users());
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
}

fn until(c: &mut LocalConnection, v: &mut View, done: impl Fn(&View) -> bool) {
    pump_until(c, v, done, false);
}

fn wait_seen(seen: &Seen, n: usize) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while seen.lock().unwrap().len() < n {
        assert!(Instant::now() < deadline, "the server saw {:?}", seen.lock().unwrap());
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn send(c: &mut LocalConnection, text: &str) {
    c.send(ClientMsg::SendMessage { text: text.into(), attachments: vec![] });
}

/// Start a slow reply and wait until its request is at the server.
fn start_slow(c: &mut LocalConnection, v: &mut View, seen: &Seen, text: &str) {
    let before = seen.lock().unwrap().len();
    send(c, text);
    wait_seen(seen, before + 1);
    until(c, v, |v| v.busy);
}

fn seen_list(seen: &Seen) -> Vec<String> {
    seen.lock().unwrap().clone()
}

#[test]
fn messages_sent_while_busy_queue_and_run_in_order() {
    let (url, seen, gate) = server();
    let engine = engine(url, CompactOptions::default());
    let (mut c, mut v) = connect(&engine);
    start_slow(&mut c, &mut v, &seen, "SLOW one");
    send(&mut c, "two");
    send(&mut c, "SLOW three");
    until(&mut c, &mut v, |v| v.queue.items.len() == 2);
    assert_eq!(v.queued(), ["two", "SLOW three"]);
    assert_eq!(v.queue.paused, None);
    // Queued messages are not part of the transcript.
    assert_eq!(v.users(), ["SLOW one"]);
    gate.send(()).unwrap();
    wait_seen(&seen, 3);
    until(&mut c, &mut v, |v| v.queue.items.is_empty() && v.users().len() == 3);
    gate.send(()).unwrap();
    until(&mut c, &mut v, |v| v.idle() && v.entries.len() == 6);
    assert_eq!(seen_list(&seen), ["SLOW one", "two", "SLOW three"]);
    assert_eq!(v.users(), ["SLOW one", "two", "SLOW three"]);
    assert!(v.entries.iter().all(|e| e.error.is_none()));
    assert_eq!(v.entries[3].content, "re: two");
}

#[test]
fn stop_pauses_the_queue_until_resumed() {
    let (url, seen, _gate) = server();
    let engine = engine(url, CompactOptions::default());
    let (mut c, mut v) = connect(&engine);
    start_slow(&mut c, &mut v, &seen, "SLOW one");
    send(&mut c, "two");
    until(&mut c, &mut v, |v| v.queue.items.len() == 1);
    c.send(ClientMsg::Cancel);
    until(&mut c, &mut v, |v| v.queue.paused == Some(QueuePause::Stopped) && v.idle());
    assert_eq!(v.entries[1].error.as_deref(), Some("stopped"));
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(seen_list(&seen), ["SLOW one"], "a paused queue sends nothing");
    // A message typed into the free chat goes at once; the queue stays paused.
    send(&mut c, "three");
    until(&mut c, &mut v, |v| v.idle() && v.entries.len() == 4);
    assert_eq!((v.queued(), v.queue.paused), (vec!["two"], Some(QueuePause::Stopped)));
    c.send(ClientMsg::ResumeQueue);
    until(&mut c, &mut v, |v| v.idle() && v.entries.len() == 6);
    assert_eq!(seen_list(&seen), ["SLOW one", "three", "two"]);
    assert!(v.queue.items.is_empty() && v.queue.paused.is_none());
}

#[test]
fn a_failed_reply_pauses_the_queue_and_retry_lets_it_go_on() {
    let (url, seen, gate) = server();
    let engine = engine(url, CompactOptions::default());
    let (mut c, mut v) = connect(&engine);
    start_slow(&mut c, &mut v, &seen, "SLOW one");
    send(&mut c, "FAILONCE two");
    send(&mut c, "three");
    until(&mut c, &mut v, |v| v.queue.items.len() == 2);
    gate.send(()).unwrap();
    until(&mut c, &mut v, |v| v.queue.paused == Some(QueuePause::Failed) && v.idle());
    assert_eq!(v.queued(), ["three"]);
    assert!(v.entries[3].error.is_some());
    c.send(ClientMsg::Retry);
    until(&mut c, &mut v, |v| v.idle() && v.queue.items.is_empty() && v.entries.len() == 6);
    assert_eq!(seen_list(&seen), ["SLOW one", "FAILONCE two", "FAILONCE two", "three"]);
    assert_eq!(v.users(), ["SLOW one", "FAILONCE two", "three"]);
}

#[test]
fn queued_messages_can_be_removed_edited_and_reordered() {
    let (url, seen, gate) = server();
    let engine = engine(url, CompactOptions::default());
    let (mut c, mut v) = connect(&engine);
    start_slow(&mut c, &mut v, &seen, "SLOW one");
    for t in ["a", "b", "c"] {
        send(&mut c, t);
    }
    until(&mut c, &mut v, |v| v.queue.items.len() == 3);
    let (a, b, cc) = (v.id("a"), v.id("b"), v.id("c"));
    c.send(ClientMsg::Unqueue { id: b });
    c.send(ClientMsg::EditQueued { id: cc, text: "  c2 ".into(), attachments: vec![] });
    c.send(ClientMsg::MoveQueued { id: cc, to: 0 });
    until(&mut c, &mut v, |v| v.queued() == ["c2", "a"]);
    assert_eq!(v.id("c2"), cc, "an edit keeps the message's id and place");
    // Moving past the end makes it last.
    c.send(ClientMsg::MoveQueued { id: cc, to: 99 });
    until(&mut c, &mut v, |v| v.queued() == ["a", "c2"]);
    // An empty edit removes; unknown ids are answered, not obeyed.
    c.send(ClientMsg::EditQueued { id: a, text: " ".into(), attachments: vec![] });
    until(&mut c, &mut v, |v| v.queued() == ["c2"]);
    c.send(ClientMsg::Unqueue { id: b });
    until(&mut c, &mut v, |v| v.notices.iter().any(|(_, t)| t.contains("isn't queued")));
    gate.send(()).unwrap();
    until(&mut c, &mut v, |v| v.idle() && v.entries.len() == 4);
    assert_eq!(seen_list(&seen), ["SLOW one", "c2"]);
}

#[test]
fn send_now_stops_the_reply_and_runs_that_message_next() {
    let (url, seen, _gate) = server();
    let engine = engine(url, CompactOptions::default());
    let (mut c, mut v) = connect(&engine);
    start_slow(&mut c, &mut v, &seen, "SLOW one");
    send(&mut c, "a");
    send(&mut c, "b");
    until(&mut c, &mut v, |v| v.queue.items.len() == 2);
    c.send(ClientMsg::SendQueuedNow { id: v.id("b") });
    until(&mut c, &mut v, |v| v.idle() && v.entries.len() == 6);
    assert_eq!(v.entries[1].error.as_deref(), Some("stopped"));
    assert_eq!(seen_list(&seen), ["SLOW one", "b", "a"], "the rest of the queue goes on after it");
    assert!(v.queue.items.is_empty() && v.queue.paused.is_none());
    // On a free chat with a paused queue it just sends.
    start_slow(&mut c, &mut v, &seen, "SLOW two");
    send(&mut c, "x");
    send(&mut c, "y");
    until(&mut c, &mut v, |v| v.queue.items.len() == 2);
    c.send(ClientMsg::Cancel);
    until(&mut c, &mut v, |v| v.queue.paused.is_some() && v.idle());
    c.send(ClientMsg::SendQueuedNow { id: v.id("y") });
    until(&mut c, &mut v, |v| v.idle() && v.queue.items.is_empty() && v.entries.len() == 12);
    assert_eq!(seen_list(&seen)[3..], ["SLOW two", "y", "x"]);
}

#[test]
fn the_queue_is_capped_and_validated_and_goes_with_its_chat() {
    let (url, seen, _gate) = server();
    let engine = engine(url, CompactOptions::default());
    let (mut c, mut v) = connect(&engine);
    start_slow(&mut c, &mut v, &seen, "SLOW one");
    for i in 0..MAX_QUEUED {
        send(&mut c, &format!("q{i}"));
    }
    until(&mut c, &mut v, |v| v.queue.items.len() == MAX_QUEUED);
    send(&mut c, "one too many");
    until(&mut c, &mut v, |v| v.notices.iter().any(|(_, t)| t.contains("queue is full")));
    c.send(ClientMsg::Unqueue { id: v.id("q0") });
    until(&mut c, &mut v, |v| v.queue.items.len() == MAX_QUEUED - 1);
    send(&mut c, &"x".repeat(MAX_QUEUED_TEXT_BYTES + 1));
    until(&mut c, &mut v, |v| v.notices.iter().any(|(_, t)| t.contains("too long to queue")));
    // Attachments must be uploads, queued or edited.
    c.send(ClientMsg::SendMessage { text: "see file".into(), attachments: vec!["uploads/nope.csv".into()] });
    c.send(ClientMsg::EditQueued { id: v.id("q1"), text: "q1".into(), attachments: vec!["../etc/passwd".into()] });
    until(&mut c, &mut v, |v| v.notices.iter().filter(|(_, t)| t.contains("isn't an uploaded file")).count() == 2);
    c.send(ClientMsg::EditQueued { id: v.id("q1"), text: "x".repeat(MAX_QUEUED_TEXT_BYTES + 1), attachments: vec![] });
    until(&mut c, &mut v, |v| v.notices.iter().any(|(_, t)| t.contains("at most 256 KB")));
    std::thread::sleep(Duration::from_millis(100));
    until(&mut c, &mut v, |_| true);
    assert_eq!(v.queue.items.len(), MAX_QUEUED - 1);
    assert_eq!(v.queue.items[0].text, "q1");
    // Deleting the (stopped) chat drops its queue.
    c.send(ClientMsg::Cancel);
    until(&mut c, &mut v, |v| v.queue.paused.is_some() && v.idle());
    let chat = v.queue.chat.clone().unwrap();
    c.send(ClientMsg::DeleteChat { id: chat.clone() });
    until(&mut c, &mut v, |v| v.queue.chat.is_none());
    assert!(v.queue.items.is_empty());
    c.send(ClientMsg::OpenChat { id: chat });
    until(&mut c, &mut v, |v| v.notices.iter().any(|(_, t)| t.contains("no longer exists")));
    assert_eq!(seen_list(&seen), ["SLOW one"]);
}

#[test]
fn every_client_of_the_chat_sees_and_edits_the_same_queue() {
    let (url, seen, gate) = server();
    let engine = engine(url, CompactOptions::default());
    let (mut a, mut va) = connect(&engine);
    start_slow(&mut a, &mut va, &seen, "SLOW one");
    send(&mut a, "two");
    until(&mut a, &mut va, |v| v.queue.items.len() == 1);
    // A client that connects later opens the chat with its queue.
    let (mut b, mut vb) = connect(&engine);
    assert_eq!(vb.queued(), ["two"]);
    assert!(vb.busy);
    send(&mut b, "three");
    until(&mut a, &mut va, |v| v.queued() == ["two", "three"]);
    b.send(ClientMsg::Unqueue { id: vb.id("two") });
    until(&mut a, &mut va, |v| v.queued() == ["three"]);
    // The queue outlives the window that filled it.
    drop(a);
    gate.send(()).unwrap();
    until(&mut b, &mut vb, |v| v.idle() && v.queue.items.is_empty() && v.entries.len() == 4);
    assert_eq!(seen_list(&seen), ["SLOW one", "three"]);
}

/// ~150 estimated tokens.
fn question(i: usize) -> String {
    format!("question {i} {}", "word ".repeat(100))
}

#[test]
fn a_queued_message_is_compacted_for_before_it_is_sent() {
    let (url, seen, gate) = server();
    let engine = engine(url, CompactOptions { at_tokens: Some(700), keep_turns: 1, ..Default::default() });
    let (mut c, mut v) = connect(&engine);
    for i in 1..=3 {
        send(&mut c, &question(i));
        until(&mut c, &mut v, |v| v.idle() && v.entries.len() == 2 * i);
    }
    start_slow(&mut c, &mut v, &seen, &format!("SLOW {}", question(4)));
    send(&mut c, &question(5));
    until(&mut c, &mut v, |v| v.queue.items.len() == 1);
    assert_eq!(v.compactions, 0);
    gate.send(()).unwrap();
    until(&mut c, &mut v, |v| v.idle() && v.entries.len() == 10);
    let s = seen_list(&seen);
    assert_eq!(s.len(), 6, "{s:?}");
    assert!(s[3].starts_with("SLOW question 4") && s[4] == "SUMMARY" && s[5].starts_with("question 5"), "{s:?}");
    assert_eq!(v.compactions, 1);
    assert!(v.entries.iter().all(|e| e.error.is_none()));
}
