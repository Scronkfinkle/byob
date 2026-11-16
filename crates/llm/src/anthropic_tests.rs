//! The Anthropic Messages client against in-process fake servers.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::time::Duration;

use byob_types::{ApiKind, InferenceConfig};
use serde_json::{Value, json};

use crate::{Capabilities, ChatMessage, Client, LlmError, ProbeOptions, ServerKind, StreamEvent, ToolCall, ToolSpec, ToolSupport, Usage};

/// A request the fake server saw.
struct Seen {
    line: String,
    headers: Vec<(String, String)>,
    body: Value,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// Answers each request with the next of `replies` (status, body); a body starting with
/// `event:` is sent as an event stream. Requests come back through the channel.
fn server(replies: Vec<(u16, String)>) -> (String, mpsc::Receiver<Seen>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut replies = replies.into_iter();
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut r = BufReader::new(&mut s);
            let mut line = String::new();
            let _ = r.read_line(&mut line);
            let mut headers = Vec::new();
            loop {
                let mut h = String::new();
                if r.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                    break;
                }
                if let Some((k, v)) = h.split_once(':') {
                    headers.push((k.trim().to_owned(), v.trim().to_owned()));
                }
            }
            let len = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.parse().ok()).unwrap_or(0usize);
            let mut body = vec![0; len];
            let _ = r.read_exact(&mut body);
            let _ = tx.send(Seen { line: line.trim().to_owned(), headers, body: serde_json::from_slice(&body).unwrap_or(Value::Null) });
            let (status, body) = replies.next().unwrap_or((404, "{}".into()));
            let kind = if body.starts_with("event:") { "text/event-stream" } else { "application/json" };
            let _ = write!(s, "HTTP/1.1 {status} X\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        }
    });
    (format!("http://{addr}/v1"), rx)
}

fn events(list: &[Value]) -> String {
    list.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap_or_default())).collect()
}

fn claude(url: String, model: &str) -> Client {
    Client::new(InferenceConfig { base_url: url, model: model.into(), api_key: Some("sk-ant-test".into()), api: Some(ApiKind::Anthropic) }).unwrap()
}

fn start(input: u64, read: u64, write: u64) -> Value {
    json!({"type": "message_start", "message": {"id": "msg_1", "model": "claude-opus-5-5", "usage": {"input_tokens": input, "cache_read_input_tokens": read, "cache_creation_input_tokens": write, "output_tokens": 1}}})
}

fn end(stop: &str, output: u64) -> [Value; 2] {
    [json!({"type": "message_delta", "delta": {"stop_reason": stop}, "usage": {"output_tokens": output}}), json!({"type": "message_stop"})]
}

#[test]
fn picks_the_messages_endpoint_from_the_host_or_the_setting() {
    let at = |url: &str, api| Client::new(InferenceConfig { base_url: url.into(), api, ..Default::default() }).unwrap().endpoint().to_string();
    assert_eq!(at("https://api.anthropic.com/v1", None), "https://api.anthropic.com/v1/messages");
    assert_eq!(at("http://gw:8080/v1", Some(ApiKind::Anthropic)), "http://gw:8080/v1/messages");
    assert_eq!(at("http://gw:8080/v1", None), "http://gw:8080/v1/chat/completions");
}

#[test]
fn streams_text_thinking_and_cached_usage() {
    let body = events(&[
        start(12, 3000, 40),
        json!({"type": "ping"}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Weighing it up."}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "EqQB"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Hel"}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "lo"}}),
        json!({"type": "content_block_stop", "index": 1}),
        end("end_turn", 9)[0].clone(),
        end("end_turn", 9)[1].clone(),
    ]);
    let (url, seen) = server(vec![(200, body)]);
    let mut got = Vec::new();
    let msgs = [ChatMessage::system("be kind"), ChatMessage::user("hi")];
    let out = claude(url, "claude-opus-5-5").stream_chat(&msgs, &[], &AtomicBool::new(false), |e| got.push(e)).unwrap();
    assert_eq!(out.content, "Hello");
    assert_eq!(out.reasoning, "Weighing it up.");
    assert_eq!(got, vec![StreamEvent::Reasoning("Weighing it up.".into()), StreamEvent::Content("Hel".into()), StreamEvent::Content("lo".into())]);
    assert_eq!(out.finish_reason.as_deref(), Some("stop"));
    // Prompt tokens include the cached ones.
    assert_eq!(out.usage, Some(Usage { prompt_tokens: 3052, completion_tokens: 9 }));
    assert_eq!((out.cache_read_tokens, out.cache_write_tokens), (3000, 40));
    assert!(out.tool_calls.is_empty());
    assert_eq!(out.native, vec![json!({"type": "thinking", "thinking": "Weighing it up.", "signature": "EqQB"}), json!({"type": "text", "text": "Hello"})]);

    let req = seen.recv().unwrap();
    assert_eq!(req.line, "POST /v1/messages HTTP/1.1");
    assert_eq!(req.header("x-api-key"), Some("sk-ant-test"));
    assert_eq!(req.header("anthropic-version"), Some(crate::anthropic::VERSION));
    assert_eq!(req.header("anthropic-beta"), Some("thinking-binding-controls-2026-08-01,server-side-fallback-2026-07-01"));
    assert_eq!(req.header("authorization"), None, "the key goes in x-api-key only");
    let b = &req.body;
    assert_eq!(b["model"], "claude-opus-5-5");
    assert_eq!(b["stream"], true);
    assert_eq!(b["max_tokens"], crate::anthropic::MAX_TOKENS);
    assert_eq!(b["cache_control"], json!({"type": "ephemeral"}));
    assert_eq!(b["system"], json!([{"type": "text", "text": "be kind", "cache_control": {"type": "ephemeral"}}]));
    assert_eq!(b["messages"], json!([{"role": "user", "content": [{"type": "text", "text": "hi"}]}]));
    assert_eq!(b["thinking"]["display"], "summarized");
    assert_eq!(b["fallbacks"], "default");
    for banned in ["temperature", "top_p", "top_k", "tool_choice", "id_slot", "stream_options"] {
        assert!(b.get(banned).is_none(), "{banned} must not be sent");
    }
}

#[test]
fn tool_round_trip_replays_thinking_and_sends_images() {
    let first = events(&[
        start(50, 0, 50),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Need the file."}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig-A"}}),
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Reading it."}}),
        json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "toolu_01", "name": "read_file", "input": {}}}),
        json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"path\":"}}),
        json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": " \"a.txt\"}"}}),
        json!({"type": "content_block_start", "index": 3, "content_block": {"type": "tool_use", "id": "toolu_02", "name": "list", "input": {}}}),
        end("tool_use", 30)[0].clone(),
        end("tool_use", 30)[1].clone(),
    ]);
    let second = events(&[start(10, 90, 20), end("end_turn", 2)[0].clone(), end("end_turn", 2)[1].clone()]);
    let (url, seen) = server(vec![(200, first), (200, second)]);
    let c = claude(url, "claude-opus-5-5");
    let tools = [ToolSpec {
        name: "read_file".into(),
        description: "Read a file".into(),
        parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
    }];
    let mut msgs = vec![ChatMessage::system("sys"), ChatMessage::user_with_images("what's in it?", vec!["data:image/jpeg;base64,/9j/AA==".into()])];
    let out = c.stream_chat(&msgs, &tools, &AtomicBool::new(false), |_| {}).unwrap();
    assert_eq!(out.finish_reason.as_deref(), Some("tool_calls"));
    assert_eq!(
        out.tool_calls,
        vec![
            ToolCall { id: "toolu_01".into(), name: "read_file".into(), arguments: r#"{"path":"a.txt"}"#.into() },
            ToolCall { id: "toolu_02".into(), name: "list".into(), arguments: String::new() },
        ]
    );
    let req1 = seen.recv().unwrap();
    assert_eq!(
        req1.body["tools"],
        json!([{"name": "read_file", "description": "Read a file", "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}}])
    );
    assert_eq!(
        req1.body["messages"][0]["content"],
        json!([{"type": "text", "text": "what's in it?"}, {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "/9j/AA=="}}])
    );

    // What the engine does: the calls with the reply's blocks, then the results.
    msgs.push(ChatMessage::assistant_tool_calls(Some(out.content.clone()), &out.tool_calls).with_native(out.native.clone()));
    msgs.push(ChatMessage::tool_result("toolu_01", "hello"));
    msgs.push(ChatMessage::tool_result("toolu_02", "a.txt"));
    let done = c.stream_chat(&msgs, &tools, &AtomicBool::new(false), |_| {}).unwrap();
    assert_eq!(done.usage, Some(Usage { prompt_tokens: 120, completion_tokens: 2 }));
    assert_eq!(done.cache_read_tokens, 90);
    let req2 = seen.recv().unwrap();
    let turns = req2.body["messages"].as_array().cloned().unwrap_or_default();
    assert_eq!(turns.len(), 3);
    assert_eq!(turns[0], req1.body["messages"][0], "earlier turns are sent the same way (cache hits)");
    assert_eq!(
        turns[1],
        json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "Need the file.", "signature": "sig-A"},
            {"type": "text", "text": "Reading it."},
            {"type": "tool_use", "id": "toolu_01", "name": "read_file", "input": {"path": "a.txt"}},
            {"type": "tool_use", "id": "toolu_02", "name": "list", "input": {}},
        ]})
    );
    assert_eq!(
        turns[2],
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_01", "content": "hello"},
            {"type": "tool_result", "tool_use_id": "toolu_02", "content": "a.txt"},
        ]})
    );
    assert_eq!(req2.body["system"], req1.body["system"]);
    assert_eq!(req2.body["cache_control"], json!({"type": "ephemeral"}));
}

#[test]
fn a_refusal_is_an_error_not_an_answer() {
    let body = events(&[
        start(5, 0, 0),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Sure, here"}}),
        json!({"type": "message_delta", "delta": {"stop_reason": "refusal", "stop_details": {"type": "refusal", "category": "bio\n<b>"}}, "usage": {"output_tokens": 3}}),
        json!({"type": "message_stop"}),
    ]);
    let (url, _seen) = server(vec![(200, body)]);
    let err = claude(url, "claude-opus-5-5").stream_chat(&[ChatMessage::user("x")], &[], &AtomicBool::new(false), |_| {}).unwrap_err();
    let LlmError::Refused(why) = &err else { panic!("{err:?}") };
    assert!(why.contains("category: biob"), "the category is cleaned: {why}");
    assert!(err.to_string().starts_with("the model declined to answer"));
}

#[test]
fn busy_is_its_own_error_and_bad_requests_stay_http() {
    let overloaded = r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
    let limited = r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#;
    let bad = r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: too big"}}"#;
    let (url, _seen) = server(vec![(529, overloaded.into()), (429, limited.into()), (400, bad.into())]);
    let c = claude(url, "claude-haiku-4-5");
    let call = || c.stream_chat(&[ChatMessage::user("x")], &[], &AtomicBool::new(false), |_| {}).unwrap_err();
    let e = call();
    assert!(matches!(&e, LlmError::Busy { status: 529, message } if message == "Overloaded"), "{e:?}");
    assert_eq!(e.to_string(), "server returned HTTP 529: Overloaded");
    assert!(matches!(call(), LlmError::Busy { status: 429, .. }));
    assert!(matches!(call(), LlmError::Http { status: 400, message } if message == "max_tokens: too big"));
}

#[test]
fn an_endpoint_without_the_betas_gets_the_plain_request() {
    let rejected = r#"{"type":"error","error":{"type":"invalid_request_error","message":"thinking.block_binding: Extra inputs are not permitted"}}"#;
    let ok = events(&[start(1, 0, 0), end("end_turn", 1)[0].clone(), end("end_turn", 1)[1].clone()]);
    let (url, seen) = server(vec![(400, rejected.into()), (200, ok)]);
    let out = claude(url, "claude-opus-5-5").stream_chat(&[ChatMessage::user("x")], &[], &AtomicBool::new(false), |_| {}).unwrap();
    assert_eq!(out.finish_reason.as_deref(), Some("stop"));
    let first = seen.recv().unwrap();
    let second = seen.recv().unwrap();
    assert!(first.header("anthropic-beta").is_some() && first.body.get("fallbacks").is_some());
    assert_eq!(second.header("anthropic-beta"), None);
    assert!(second.body.get("fallbacks").is_none() && second.body["thinking"].get("block_binding").is_none());
    assert_eq!(second.body["thinking"]["type"], "adaptive");
}

#[test]
fn probe_reads_the_models_api_and_sends_no_tool_test() {
    let list = json!({"data": [
        {"type": "model", "id": "claude-haiku-5-5", "max_input_tokens": 1_000_000, "capabilities": {"image_input": {"supported": true}}},
        {"type": "model", "id": "claude-opus-5-5", "display_name": "Claude Opus 5.5", "max_input_tokens": 1_000_000, "max_tokens": 128_000,
         "capabilities": {"image_input": {"supported": true}, "thinking": {"supported": true}}},
    ], "has_more": false});
    let (url, seen) = server(vec![(200, list.to_string())]);
    let quick = ProbeOptions { budget: Duration::from_secs(5), step_timeout: Duration::from_secs(2), tool_timeout: Duration::from_secs(2), test_tools: true };
    let caps = claude(url.clone(), "claude-opus-5-5").probe_with(quick.clone());
    assert_eq!(
        (caps.server, caps.context_length, caps.vision, caps.tools, caps.slots),
        (ServerKind::Anthropic, Some(1_000_000), Some(true), ToolSupport::Native, None),
        "{caps:?}"
    );
    assert_eq!(caps.models, ["claude-haiku-5-5", "claude-opus-5-5"]);
    let req = seen.recv().unwrap();
    assert_eq!(req.line, "GET /v1/models?limit=1000 HTTP/1.1");
    assert_eq!((req.header("x-api-key"), req.header("anthropic-version")), (Some("sk-ant-test"), Some(crate::anthropic::VERSION)));
    assert!(seen.recv_timeout(Duration::from_millis(300)).is_err(), "nothing else: no tool test, no llama.cpp endpoints");

    // A model the list doesn't show is asked for by name; a dead key leaves things unknown.
    let one = json!({"id": "claude-opus-5-5-latest", "max_input_tokens": 500_000, "capabilities": {"image_input": {"supported": false}}});
    let (url, seen) = server(vec![(200, list.to_string()), (200, one.to_string())]);
    let caps = claude(url, "claude-opus-5-5-latest").probe_with(quick.clone());
    assert_eq!((caps.context_length, caps.vision), (Some(500_000), Some(false)));
    assert_eq!(seen.recv().unwrap().line, "GET /v1/models?limit=1000 HTTP/1.1");
    assert_eq!(seen.recv().unwrap().line, "GET /v1/models/claude-opus-5-5-latest HTTP/1.1");
    let (url, _seen) = server(vec![(401, r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#.into())]);
    let caps = claude(url, "claude-opus-5-5").probe_with(quick);
    assert_eq!(caps, Capabilities { notes: caps.notes.clone(), ..Default::default() }, "inconclusive, so it is probed again later");
}

#[test]
fn list_models_uses_anthropic_headers() {
    let (url, seen) =
        server(vec![(200, r#"{"data":[{"id":"claude-opus-5-5"}]}"#.into()), (401, r#"{"type":"error","error":{"message":"invalid x-api-key"}}"#.into())]);
    let c = claude(url, "claude-opus-5-5");
    assert_eq!(c.list_models().unwrap(), ["claude-opus-5-5"]);
    let req = seen.recv().unwrap();
    assert_eq!(req.line, "GET /v1/models?limit=1000 HTTP/1.1");
    assert_eq!(req.header("x-api-key"), Some("sk-ant-test"));
    assert!(matches!(c.list_models(), Err(LlmError::Http { status: 401, message }) if message == "invalid x-api-key"));
}
