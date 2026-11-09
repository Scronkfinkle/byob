use super::*;
use std::io::Write as _;
use std::net::TcpListener;
use std::sync::mpsc;

/// A one-shot fake OpenAI server: replies to the first request with `status` and `body`,
/// and hands the raw request back through the returned channel.
fn fake_server(status: u16, content_type: &str, body: String) -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    let content_type = content_type.to_owned();
    std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        // Read headers, then Content-Length bytes of body.
        loop {
            let n = s.read(&mut chunk).unwrap();
            buf.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&buf).to_string();
            if let Some(end) = text.find("\r\n\r\n") {
                let len =
                    text.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap())).unwrap_or(0);
                let chunked = text.to_ascii_lowercase().contains("transfer-encoding: chunked");
                if (chunked && text.ends_with("0\r\n\r\n")) || (!chunked && buf.len() >= end + 4 + len) {
                    break;
                }
            }
            if n == 0 {
                break;
            }
        }
        tx.send(String::from_utf8_lossy(&buf).to_string()).unwrap();
        write!(s, "HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\nConnection: close\r\n\r\n{body}").unwrap();
    });
    (format!("http://{addr}/v1"), rx)
}

fn sse(chunks: &[&str]) -> String {
    chunks.iter().map(|c| format!("data: {c}\n\n")).collect::<String>() + "data: [DONE]\n\n"
}

fn client(url: String) -> Client {
    Client::new(InferenceConfig { base_url: url, model: "m".into(), api_key: Some("sk-test".into()) }).unwrap()
}

#[test]
fn joins_chat_completions_path() {
    let c = Client::new(InferenceConfig { base_url: "http://localhost:8080/v1".into(), ..Default::default() }).unwrap();
    assert_eq!(c.endpoint().as_str(), "http://localhost:8080/v1/chat/completions");
}

#[test]
fn rejects_garbage_and_non_http_urls() {
    assert!(Client::new(InferenceConfig { base_url: "not a url".into(), ..Default::default() }).is_err());
    assert!(Client::new(InferenceConfig { base_url: "file:///etc/passwd".into(), ..Default::default() }).is_err());
}

#[test]
fn streams_content_reasoning_and_usage() {
    let body = sse(&[
        r#"{"choices":[{"delta":{"reasoning_content":"let me think"}}]}"#,
        r#"{"choices":[{"delta":{"content":"Hel"}}]}"#,
        r#"{"choices":[{"delta":{"content":"lo"},"finish_reason":"stop"}]}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":2}}"#,
    ]);
    let (url, req) = fake_server(200, "text/event-stream", body);
    let mut events = Vec::new();
    let out = client(url).stream_chat(&[ChatMessage::user("hi")], &[], &AtomicBool::new(false), |e| events.push(e)).unwrap();
    assert_eq!(out.content, "Hello");
    assert_eq!(out.reasoning, "let me think");
    assert_eq!(out.finish_reason.as_deref(), Some("stop"));
    assert_eq!(out.usage, Some(Usage { prompt_tokens: 5, completion_tokens: 2 }));
    assert_eq!(events.len(), 3);

    let req = req.recv().unwrap();
    assert!(req.starts_with("POST /v1/chat/completions"));
    assert!(req.to_ascii_lowercase().contains("authorization: bearer sk-test"));
    assert!(req.contains(r#""stream":true"#));
}

#[test]
fn assembles_streamed_tool_calls() {
    let body = sse(&[
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read_file","arguments":""}}]}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.txt\"}"}}]}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
    ]);
    let (url, req) = fake_server(200, "text/event-stream", body);
    let tools = [ToolSpec { name: "read_file".into(), description: "Read".into(), parameters: serde_json::json!({"type":"object"}) }];
    let out = client(url).stream_chat(&[ChatMessage::user("read a.txt")], &tools, &AtomicBool::new(false), |_| {}).unwrap();
    assert_eq!(out.tool_calls, vec![ToolCall { id: "call_1".into(), name: "read_file".into(), arguments: r#"{"path":"a.txt"}"#.into() }]);
    assert!(req.recv().unwrap().contains(r#""tools":[{"function":{"#));
}

#[test]
fn inline_think_tags_become_reasoning() {
    let body = sse(&[r#"{"choices":[{"delta":{"content":"<think>hmm</think>Answer"}}]}"#]);
    let (url, _req) = fake_server(200, "text/event-stream", body);
    let out = client(url).stream_chat(&[ChatMessage::user("q")], &[], &AtomicBool::new(false), |_| {}).unwrap();
    assert_eq!((out.content.as_str(), out.reasoning.as_str()), ("Answer", "hmm"));
}

#[test]
fn http_errors_carry_server_message() {
    let (url, _req) = fake_server(400, "application/json", r#"{"error":{"message":"context too long"}}"#.into());
    let err = client(url).stream_chat(&[ChatMessage::user("q")], &[], &AtomicBool::new(false), |_| {}).unwrap_err();
    assert_eq!(err.to_string(), "server returned HTTP 400: context too long");
}

#[test]
fn in_stream_error_is_reported() {
    let (url, _req) = fake_server(200, "text/event-stream", sse(&[r#"{"error":{"message":"slot unavailable"}}"#]));
    let err = client(url).stream_chat(&[ChatMessage::user("q")], &[], &AtomicBool::new(false), |_| {}).unwrap_err();
    assert!(err.to_string().contains("slot unavailable"));
}

#[test]
fn cancel_stops_early() {
    let (url, _req) = fake_server(200, "text/event-stream", sse(&[r#"{"choices":[{"delta":{"content":"x"}}]}"#]));
    let out = client(url).stream_chat(&[ChatMessage::user("q")], &[], &AtomicBool::new(true), |_| {}).unwrap();
    assert!(out.cancelled);
    assert!(out.content.is_empty());
}

#[test]
fn hostile_chunks_do_not_panic() {
    for junk in [
        "{}",
        "[]",
        "null",
        r#"{"choices":"x"}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":99999999999}]}}]}"#,
        r#"{"choices":[{"delta":{"content":5}}]}"#,
    ] {
        let _ = wire::Chunk::parse(junk);
        let mut calls = Vec::new();
        if let Ok(c) = wire::Chunk::parse(junk) {
            for d in c.tool_calls {
                wire::merge_tool_call(&mut calls, d, MAX_TOOL_CALLS);
            }
        }
        assert!(calls.len() <= MAX_TOOL_CALLS);
    }
}

#[test]
fn lists_models() {
    let (url, _req) = fake_server(200, "application/json", r#"{"data":[{"id":"local"},{"id":"other"}]}"#.into());
    assert_eq!(client(url).list_models().unwrap(), vec!["local", "other"]);
}

/// Live check against a real server: `YAH_LIVE_URL=http://silicon:9931/v1 cargo test -p yah-llm -- --ignored`
#[test]
#[ignore = "needs a live inference server (YAH_LIVE_URL)"]
fn live_server_streams() {
    let url = std::env::var("YAH_LIVE_URL").expect("set YAH_LIVE_URL");
    let c = Client::new(InferenceConfig { base_url: url, model: "local".into(), api_key: None }).unwrap();
    let mut n = 0;
    let out = c.stream_chat(&[ChatMessage::user("Reply with exactly: pong")], &[], &AtomicBool::new(false), |_| n += 1).unwrap();
    eprintln!("content={:?} reasoning_len={} usage={:?} events={n}", out.content, out.reasoning.len(), out.usage);
    assert!(out.content.to_lowercase().contains("pong"));
}
