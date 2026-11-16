//! The tool loop against a scripted fake Anthropic Messages API: thinking blocks go back
//! with their tool calls, 529 is tried again, a 400 is not (and doesn't switch tool modes),
//! and a refusal ends the turn with a clear error.

use std::io::{BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use byob_protocol::{ClientMsg, Connection, ServerMsg};
use byob_tools::{Access, Registry};
use byob_types::{ApiKind, InferenceConfig};
use serde_json::{Value, json};

use crate::loop_tests::{Fake, recv};
use crate::{EngineConfig, EngineHandle, LocalConnection};

enum Reply {
    /// SSE events of a 200 reply.
    Events(Vec<Value>),
    Http(u16, &'static str),
}

/// A streamed reply: an optional thinking block, then text or one call to `echo`.
fn reply(thinking: Option<(&str, &str)>, text: Option<&str>, call: Option<(&str, &str)>) -> Reply {
    let mut ev = vec![
        json!({"type": "message_start", "message": {"id": "msg_1", "usage": {"input_tokens": 20, "cache_read_input_tokens": 100, "cache_creation_input_tokens": 5}}}),
    ];
    let mut i = 0;
    if let Some((summary, signature)) = thinking {
        ev.push(json!({"type": "content_block_start", "index": i, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}));
        ev.push(json!({"type": "content_block_delta", "index": i, "delta": {"type": "thinking_delta", "thinking": summary}}));
        ev.push(json!({"type": "content_block_delta", "index": i, "delta": {"type": "signature_delta", "signature": signature}}));
        ev.push(json!({"type": "content_block_stop", "index": i}));
        i += 1;
    }
    if let Some(t) = text {
        ev.push(json!({"type": "content_block_start", "index": i, "content_block": {"type": "text", "text": ""}}));
        ev.push(json!({"type": "content_block_delta", "index": i, "delta": {"type": "text_delta", "text": t}}));
        ev.push(json!({"type": "content_block_stop", "index": i}));
        i += 1;
    }
    let stop = if let Some((id, args)) = call {
        ev.push(json!({"type": "content_block_start", "index": i, "content_block": {"type": "tool_use", "id": id, "name": "echo", "input": {}}}));
        let (a, b) = args.split_at(args.len() / 2);
        for part in [a, b] {
            ev.push(json!({"type": "content_block_delta", "index": i, "delta": {"type": "input_json_delta", "partial_json": part}}));
        }
        ev.push(json!({"type": "content_block_stop", "index": i}));
        "tool_use"
    } else {
        "end_turn"
    };
    ev.push(json!({"type": "message_delta", "delta": {"stop_reason": stop}, "usage": {"output_tokens": 7}}));
    ev.push(json!({"type": "message_stop"}));
    Reply::Events(ev)
}

type Requests = Arc<Mutex<Vec<Value>>>;

/// Answers `POST /v1/messages` with `script` in order (repeating the last one).
fn fake_anthropic(script: Vec<Reply>) -> (String, Requests) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let requests: Requests = Arc::default();
    let seen = Arc::clone(&requests);
    std::thread::spawn(move || {
        let mut n = 0usize;
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut r = BufReader::new(&mut s);
            let mut first = String::new();
            let _ = r.read_line(&mut first);
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
            if !first.starts_with("POST /v1/messages ") {
                let _ = write!(s, "HTTP/1.1 404 X\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}");
                continue;
            }
            seen.lock().unwrap().push(serde_json::from_slice(&body).unwrap_or(Value::Null));
            match script.get(n).or(script.last()) {
                Some(Reply::Http(status, m)) => {
                    let body = json!({"type": "error", "error": {"type": "api_error", "message": m}}).to_string();
                    let _ =
                        write!(s, "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                }
                Some(Reply::Events(ev)) => {
                    let sse: String = ev.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap_or_default())).collect();
                    let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{sse}");
                }
                None => {}
            }
            n += 1;
        }
    });
    (format!("http://{addr}/v1"), requests)
}

struct H {
    engine: EngineHandle,
    requests: Requests,
    runs: Arc<AtomicUsize>,
    _ws: tempfile::TempDir,
}

fn harness(script: Vec<Reply>) -> H {
    let (url, requests) = fake_anthropic(script);
    let ws = tempfile::tempdir().unwrap();
    let runs = Arc::new(AtomicUsize::new(0));
    let mut tools = Registry::default();
    tools.register(Arc::new(Fake { name: "echo", access: Access::Read, runs: Arc::clone(&runs), reply: None, change: false }));
    let inference = InferenceConfig { base_url: url, model: "claude-opus-5-5".into(), api_key: Some("sk-ant-test".into()), api: Some(ApiKind::Anthropic) };
    let mut config = EngineConfig::new(inference);
    config.workspace = ws.path().to_path_buf();
    config.tools = tools;
    config.auto_title = false;
    H { engine: EngineHandle::spawn_with(config).unwrap(), requests, runs, _ws: ws }
}

/// Send `text` and wait for the reply: its content, reasoning, and error.
fn run(h: &H, text: &str) -> (String, String, Option<String>) {
    let mut c: LocalConnection = h.engine.connect(Arc::new(|| {}));
    c.send(ClientMsg::hello("test"));
    while !matches!(recv(&mut c), ServerMsg::Transcript { .. }) {}
    c.send(ClientMsg::SendMessage { text: text.into(), attachments: vec![] });
    let (mut content, mut reasoning, mut id) = (String::new(), String::new(), None);
    loop {
        match recv(&mut c) {
            ServerMsg::EntryAdded(e) if e.role == byob_protocol::EntryRole::Assistant => id = Some(e.id),
            ServerMsg::EntryDelta { id: d, content: t, reasoning: r } if Some(d) == id => {
                content.push_str(&t);
                reasoning.push_str(&r);
            }
            ServerMsg::EntryFinished { id: d, error, .. } if Some(d) == id => return (content, reasoning, error),
            _ => {}
        }
    }
}

#[test]
fn tool_loop_sends_thinking_back_with_its_call() {
    let h =
        harness(vec![reply(Some(("I should echo.", "sig-123")), None, Some(("toolu_1", r#"{"text":"pong"}"#))), reply(None, Some("The answer is pong"), None)]);
    let (content, reasoning, error) = run(&h, "ping?");
    assert_eq!(error, None);
    assert_eq!(content, "The answer is pong");
    assert!(reasoning.contains("I should echo."), "{reasoning:?}");
    assert_eq!(h.runs.load(Ordering::SeqCst), 1);
    let reqs = h.requests.lock().unwrap();
    assert_eq!(reqs.len(), 2);
    let first = &reqs[0];
    assert_eq!(first["model"], "claude-opus-5-5");
    assert_eq!(first["tools"][0]["name"], "echo");
    assert_eq!(first["tools"][0]["input_schema"]["type"], "object");
    assert_eq!(first["cache_control"], json!({"type": "ephemeral"}));
    assert!(first["system"][0]["text"].as_str().is_some_and(|s| !s.is_empty()), "the system prompt is the top-level system");
    assert_eq!(first["system"].as_array().and_then(|s| s.last()).map(|b| b["cache_control"].clone()), Some(json!({"type": "ephemeral"})));
    assert!(first.get("temperature").is_none() && first.get("top_p").is_none());
    assert_eq!(first["thinking"]["type"], "adaptive");
    let turns = reqs[1]["messages"].as_array().cloned().unwrap_or_default();
    let n = turns.len();
    assert!(n >= 3);
    // The thinking block, signature and all, goes back ahead of its tool call.
    assert_eq!(turns[n - 2]["role"], "assistant");
    assert_eq!(
        turns[n - 2]["content"],
        json!([
            {"type": "thinking", "thinking": "I should echo.", "signature": "sig-123"},
            {"type": "tool_use", "id": "toolu_1", "name": "echo", "input": {"text": "pong"}},
        ])
    );
    assert_eq!(turns[n - 1]["role"], "user");
    assert_eq!(turns[n - 1]["content"][0], json!({"type": "tool_result", "tool_use_id": "toolu_1", "content": "pong"}));
    assert_eq!(turns.iter().filter(|t| t["role"] == "system").count(), 0);
}

#[test]
fn overloaded_is_tried_again() {
    let h = harness(vec![Reply::Http(529, "Overloaded"), reply(None, Some("ok"), None)]);
    let (content, _, error) = run(&h, "hi");
    assert_eq!((content.as_str(), error), ("ok", None));
    assert_eq!(h.requests.lock().unwrap().len(), 2);
}

#[test]
fn a_bad_request_is_not_tried_again_nor_switches_to_prompt_tools() {
    let h = harness(vec![Reply::Http(400, "messages.2: tool_use ids were found without tool_result blocks")]);
    let (_, _, error) = run(&h, "hi");
    assert!(error.unwrap_or_default().contains("tool_result"));
    let reqs = h.requests.lock().unwrap();
    assert_eq!(reqs.len(), 1, "not retried, and no second request in prompt mode");
}

#[test]
fn a_refusal_ends_the_turn_with_a_clear_error() {
    let refusal = Reply::Events(vec![
        json!({"type": "message_start", "message": {"usage": {"input_tokens": 9}}}),
        json!({"type": "message_delta", "delta": {"stop_reason": "refusal", "stop_details": {"type": "refusal", "category": "cyber"}}, "usage": {"output_tokens": 0}}),
        json!({"type": "message_stop"}),
    ]);
    let h = harness(vec![refusal]);
    let (_, _, error) = run(&h, "hi");
    let error = error.unwrap_or_default();
    assert!(error.contains("declined") && error.contains("cyber"), "{error}");
    assert_eq!(h.requests.lock().unwrap().len(), 1, "a refusal isn't retried");
}
