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
fn messages_serialize_text_or_image_parts() {
    let plain = serde_json::to_value(ChatMessage::user("hi")).unwrap();
    assert_eq!(plain, serde_json::json!({"role": "user", "content": "hi"}));
    let call = ToolCall { id: "c".into(), name: "f".into(), arguments: "{}".into() };
    let calls = serde_json::to_value(ChatMessage::assistant_tool_calls(None, &[call])).unwrap();
    assert_eq!(calls, serde_json::json!({"role": "assistant", "tool_calls": [{"id": "c", "type": "function", "function": {"name": "f", "arguments": "{}"}}]}));
    let m = ChatMessage::user_with_images("what is this?", vec!["data:image/png;base64,AAAA".into()]);
    assert_eq!(
        serde_json::to_value(&m).unwrap(),
        serde_json::json!({"role": "user", "content": [
            {"type": "text", "text": "what is this?"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
        ]})
    );
    let stripped = m.without_images("[image omitted]");
    assert!(stripped.images.is_empty());
    assert_eq!(stripped.content.as_deref(), Some("what is this?\n[image omitted]"));
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

/// Route reply that makes the fake server stall instead of answering.
const STALL: &str = "<stall>";

/// A multi-request fake server. `routes` maps "METHOD /path" to (status, body); anything else
/// is a 404. Every request line is reported through the channel.
fn routed_server(server_header: &str, routes: &[(&str, u16, &str)]) -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    let routes: Vec<(String, u16, String)> = routes.iter().map(|(r, s, b)| (r.to_string(), *s, b.to_string())).collect();
    let server_header = server_header.to_owned();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let (routes, tx, server_header) = (routes.clone(), tx.clone(), server_header.clone());
            std::thread::spawn(move || {
                let req = read_request(&mut s);
                let line: String = req.lines().next().unwrap_or_default().rsplit_once(' ').map(|(a, _)| a.to_owned()).unwrap_or_default();
                let _ = tx.send(req.clone());
                let (status, body) = routes.iter().find(|(r, _, _)| *r == line).map(|(_, s, b)| (*s, b.clone())).unwrap_or((404, "{}".into()));
                if body == STALL {
                    std::thread::sleep(Duration::from_secs(5));
                    return;
                }
                let hdr = if server_header.is_empty() { String::new() } else { format!("Server: {server_header}\r\n") };
                let _ = write!(
                    s,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n{hdr}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            });
        }
    });
    (format!("http://{addr}/v1"), rx)
}

fn read_request(s: &mut std::net::TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while let Ok(n) = s.read(&mut chunk) {
        buf.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&buf).to_string();
        if let Some(end) = text.find("\r\n\r\n") {
            let len =
                text.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap())).unwrap_or(0);
            if buf.len() >= end + 4 + len {
                break;
            }
        }
        if n == 0 {
            break;
        }
    }
    String::from_utf8_lossy(&buf).to_string()
}

const NATIVE_TOOL_REPLY: &str = r#"{"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"c","type":"function","function":{"name":"get_magic_number","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#;

fn quick() -> ProbeOptions {
    ProbeOptions { budget: Duration::from_secs(10), step_timeout: Duration::from_secs(2), tool_timeout: Duration::from_secs(2), test_tools: true }
}

#[test]
fn probes_llama_server() {
    let (url, reqs) = routed_server(
        "llama.cpp",
        &[
            ("GET /v1/models", 200, r#"{"object":"list","data":[{"id":"m","owned_by":"llamacpp","meta":{"n_ctx_train":131072}}]}"#),
            ("GET /props", 200, r#"{"default_generation_settings":{"n_ctx":65536},"modalities":{"vision":true}}"#),
            ("POST /tokenize", 200, r#"{"tokens":[1,2,3]}"#),
            ("POST /v1/chat/completions", 200, NATIVE_TOOL_REPLY),
        ],
    );
    let caps = client(url.clone()).probe_with(quick());
    assert_eq!(caps.server, ServerKind::LlamaCpp, "{caps:?}");
    assert_eq!(caps.models, ["m"]);
    assert_eq!(caps.context_length, Some(65536));
    assert_eq!(caps.vision, Some(true));
    assert_eq!(caps.tools, ToolSupport::Native);
    assert!(caps.tokenize);
    let seen: Vec<String> = reqs.try_iter().collect();
    let tool_req = seen.iter().find(|r| r.starts_with("POST /v1/chat/completions")).unwrap();
    assert!(tool_req.contains(r#""max_tokens":512"#) && tool_req.contains("get_magic_number"));
    assert!(!seen.iter().any(|r| r.contains("/api/")), "no Ollama/LM Studio probes once identified");
    assert_eq!(client(url).count_tokens("hello").unwrap(), 3);
}

#[test]
fn probes_basalt_style_proxy_like_silicon() {
    // Python proxy in front of llama-server: no Server: llama.cpp, no owned_by, no /tokenize.
    let (url, _reqs) = routed_server(
        "BaseHTTP/0.6 Python/3.14.8",
        &[
            (
                "GET /v1/models",
                200,
                r#"{"object":"list","data":[{"id":"local","meta":{"n_ctx":262144},"architecture":{"input_modalities":["text","image"]}}]}"#,
            ),
            ("GET /props", 200, r#"{"default_generation_settings":{"n_ctx":262144},"total_slots":4}"#),
            ("POST /tokenize", 404, r#"{"error":{"message":"not found"}}"#),
            ("POST /v1/chat/completions", 200, NATIVE_TOOL_REPLY),
        ],
    );
    let caps = client(url).probe_with(quick());
    assert_eq!((caps.server, caps.context_length, caps.vision, caps.tools), (ServerKind::LlamaCpp, Some(262144), Some(true), ToolSupport::Native));
    assert!(!caps.tokenize);
    assert!(caps.notes.iter().any(|n| n.contains("/tokenize: HTTP 404")), "{:?}", caps.notes);
}

#[test]
fn probes_llama_server_without_jinja() {
    let (url, _reqs) = routed_server(
        "llama.cpp",
        &[
            ("GET /v1/models", 200, r#"{"data":[{"id":"m","owned_by":"llamacpp"}]}"#),
            ("POST /v1/chat/completions", 500, r#"{"error":{"code":500,"message":"tools param requires --jinja flag"}}"#),
        ],
    );
    let caps = client(url).probe_with(quick());
    assert_eq!(caps.server, ServerKind::LlamaCpp);
    assert_eq!(caps.context_length, None);
    // llama-server reports this as a 500; we can't distinguish it from overload, so Unknown.
    assert_eq!(caps.tools, ToolSupport::Unknown);
    assert!(caps.notes.iter().any(|n| n.contains("--jinja")));
}

#[test]
fn probes_vllm() {
    let (url, reqs) = routed_server(
        "uvicorn",
        &[
            ("GET /v1/models", 200, r#"{"object":"list","data":[{"id":"m","object":"model","owned_by":"vllm","max_model_len":32768}]}"#),
            (
                "POST /v1/chat/completions",
                400,
                r#"{"object":"error","message":"\"auto\" tool choice requires --enable-auto-tool-choice","type":"BadRequestError","code":400}"#,
            ),
        ],
    );
    let caps = client(url).probe_with(quick());
    assert_eq!((caps.server, caps.context_length, caps.tools), (ServerKind::Vllm, Some(32768), ToolSupport::Unsupported));
    assert_eq!(caps.vision, None);
    assert!(!reqs.try_iter().any(|r| r.contains("/api/")));
}

#[test]
fn probes_ollama() {
    let (url, reqs) = routed_server(
        "",
        &[
            ("GET /v1/models", 200, r#"{"object":"list","data":[{"id":"m","object":"model","owned_by":"library"}]}"#),
            (
                "POST /api/show",
                200,
                r#"{"parameters":"stop \"<|im_end|>\"\nnum_ctx 8192","model_info":{"qwen2.context_length":32768},"capabilities":["completion","tools","vision"]}"#,
            ),
            (
                "POST /v1/chat/completions",
                200,
                r#"{"choices":[{"message":{"content":"<tool_call>{\"name\":\"get_magic_number\",\"arguments\":{}}</tool_call>"},"finish_reason":"stop"}]}"#,
            ),
        ],
    );
    let caps = client(url).probe_with(quick());
    assert_eq!(caps.server, ServerKind::Ollama);
    assert_eq!((caps.context_length, caps.max_context_length), (Some(8192), Some(32768)));
    assert_eq!(caps.vision, Some(true));
    assert_eq!(caps.tools, ToolSupport::InContent);
    let show = reqs.try_iter().find(|r| r.starts_with("POST /api/show")).unwrap();
    assert!(show.contains(r#"{"model":"m"}"#));
}

#[test]
fn probes_ollama_without_num_ctx_by_version_endpoint() {
    let (url, _reqs) = routed_server(
        "",
        &[
            ("GET /v1/models", 200, r#"{"data":[{"id":"m"}]}"#),
            ("GET /api/version", 200, r#"{"version":"0.12.0"}"#),
            ("POST /api/show", 200, r#"{"model_info":{"llama.context_length":131072},"capabilities":["completion"]}"#),
            ("POST /v1/chat/completions", 400, r#"{"error":{"message":"registry.ollama.ai/library/m does not support tools"}}"#),
        ],
    );
    let caps = client(url).probe_with(quick());
    assert_eq!(caps.server, ServerKind::Ollama);
    assert_eq!((caps.context_length, caps.max_context_length), (None, Some(131072)));
    assert_eq!(caps.vision, Some(false));
    assert_eq!(caps.tools, ToolSupport::Unsupported);
    assert_eq!(caps.effective_context(), budget::DEFAULT_CONTEXT_TOKENS);
}

#[test]
fn probes_lm_studio() {
    let (url, _reqs) = routed_server(
        "",
        &[
            ("GET /v1/models", 200, r#"{"data":[{"id":"m","object":"model","owned_by":"organization_owner"}]}"#),
            ("GET /api/v0/models", 200, r#"{"data":[{"id":"m","type":"vlm","max_context_length":131072,"loaded_context_length":16384}]}"#),
            ("POST /v1/chat/completions", 200, r#"{"choices":[{"message":{"content":"The magic number is unknown."},"finish_reason":"stop"}]}"#),
        ],
    );
    let caps = client(url).probe_with(quick());
    assert_eq!(caps.server, ServerKind::LmStudio);
    assert_eq!((caps.context_length, caps.max_context_length, caps.vision), (Some(16384), Some(131072), Some(true)));
    assert_eq!(caps.tools, ToolSupport::Unsupported);

    let (url, _reqs) = routed_server(
        "",
        &[
            ("GET /v1/models", 200, r#"{"data":[{"id":"m"}]}"#),
            ("GET /api/v0/models", 200, r#"{"data":[{"id":"m","type":"vlm","max_context_length":131072,"loaded_context_length":16384}]}"#),
        ],
    );
    let caps = client(url).probe_with(ProbeOptions { test_tools: false, ..quick() });
    assert_eq!(caps.server, ServerKind::LmStudio);
    assert_eq!((caps.context_length, caps.max_context_length, caps.vision), (Some(16384), Some(131072), Some(true)));
    assert_eq!(caps.tools, ToolSupport::Unknown);
}

#[test]
fn probes_openai_style_hosted_api() {
    let (url, reqs) = routed_server(
        "cloudflare",
        &[
            ("GET /v1/models", 200, r#"{"object":"list","data":[{"id":"gpt-x","object":"model","owned_by":"system"}]}"#),
            ("POST /v1/chat/completions", 200, NATIVE_TOOL_REPLY),
        ],
    );
    let caps = client(url).probe_with(quick());
    assert_eq!((caps.server, caps.context_length, caps.tools), (ServerKind::OpenAi, None, ToolSupport::Native));
    let paths: Vec<String> = reqs.try_iter().map(|r| r.lines().next().unwrap_or_default().to_owned()).collect();
    assert_eq!(paths.len(), 2, "no local-server probes against a hosted API: {paths:?}");
}

#[test]
fn probe_survives_dead_and_hostile_servers() {
    // Nothing listening.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let caps = client(format!("http://{addr}/v1")).probe_with(quick());
    assert_eq!(caps, Capabilities { notes: caps.notes.clone(), ..Default::default() });
    assert!(!caps.notes.is_empty());

    // Garbage everywhere.
    let (url, _reqs) = routed_server(
        "",
        &[
            ("GET /v1/models", 200, "not json"),
            ("GET /props", 200, r#"{"default_generation_settings":"x"}"#),
            ("GET /api/version", 200, "[]"),
            ("POST /tokenize", 200, r#"{"tokens":"many"}"#),
            ("POST /v1/chat/completions", 200, r#"{"choices":[null]}"#),
        ],
    );
    let caps = client(url).probe_with(quick());
    assert_eq!(caps.context_length, None);
    assert!(!caps.tokenize);
    assert_eq!(caps.tools, ToolSupport::Unknown);
}

#[test]
fn probe_respects_its_time_budget() {
    let (url, _reqs) = routed_server("", &[("GET /v1/models", 200, STALL), ("GET /props", 200, STALL), ("POST /v1/chat/completions", 200, STALL)]);
    let start = std::time::Instant::now();
    let opts =
        ProbeOptions { budget: Duration::from_millis(1500), step_timeout: Duration::from_millis(600), tool_timeout: Duration::from_secs(10), test_tools: true };
    let caps = client(url).probe_with(opts);
    let took = start.elapsed();
    assert!(took < Duration::from_millis(2500), "probe took {took:?}");
    assert_eq!(caps.tools, ToolSupport::Unknown);
    assert!(caps.notes.iter().any(|n| n.contains("budget spent") || n.contains("imeout") || n.contains("timed out")), "{:?}", caps.notes);
}

#[test]
fn root_strips_v1() {
    assert_eq!(client("http://h:1/v1".into()).root().as_str(), "http://h:1/");
    assert_eq!(client("http://h:1/api/v1/".into()).root().as_str(), "http://h:1/api/");
    assert_eq!(client("http://h:1/openai".into()).root().as_str(), "http://h:1/openai/");
}

/// Live probe: `BYOB_LIVE_URL=http://silicon:9931/v1 cargo test -p byob-llm live_probe -- --ignored --nocapture`
#[test]
#[ignore = "needs a live inference server (BYOB_LIVE_URL)"]
fn live_probe() {
    let url = std::env::var("BYOB_LIVE_URL").expect("set BYOB_LIVE_URL");
    let c = Client::new(InferenceConfig { base_url: url, model: "local".into(), api_key: None }).unwrap();
    let start = std::time::Instant::now();
    let caps = c.probe();
    eprintln!("probe took {:?}\n{caps:#?}", start.elapsed());
    match c.count_tokens("hello world, this is a test") {
        Ok(n) => eprintln!("tokenize: {n} tokens (estimate {})", estimate_tokens("hello world, this is a test")),
        Err(e) => eprintln!("tokenize: {e}"),
    }
    assert!(!caps.models.is_empty());
}

/// Live check against a real server: `BYOB_LIVE_URL=http://silicon:9931/v1 cargo test -p byob-llm -- --ignored`
#[test]
#[ignore = "needs a live inference server (BYOB_LIVE_URL)"]
fn live_server_streams() {
    let url = std::env::var("BYOB_LIVE_URL").expect("set BYOB_LIVE_URL");
    let c = Client::new(InferenceConfig { base_url: url, model: "local".into(), api_key: None }).unwrap();
    let mut n = 0;
    let out = c.stream_chat(&[ChatMessage::user("Reply with exactly: pong")], &[], &AtomicBool::new(false), |_| n += 1).unwrap();
    eprintln!("content={:?} reasoning_len={} usage={:?} events={n}", out.content, out.reasoning.len(), out.usage);
    assert!(out.content.to_lowercase().contains("pong"));
}

#[test]
fn images_become_content_parts_and_text_stays_a_string() {
    let plain = serde_json::to_value(ChatMessage::user("hi")).unwrap();
    assert_eq!(plain, serde_json::json!({"role": "user", "content": "hi"}));
    let m = ChatMessage::user_with_images("what is this?", vec!["data:image/png;base64,AAAA".into()]);
    assert_eq!(
        serde_json::to_value(&m).unwrap(),
        serde_json::json!({"role": "user", "content": [
            {"type": "text", "text": "what is this?"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
        ]})
    );
    assert!(crate::budget::estimate_message_tokens(&m) > crate::budget::estimate_message_tokens(&ChatMessage::user("what is this?")) + 500);
    let tool = serde_json::to_value(ChatMessage::tool_result("c1", "ok")).unwrap();
    assert_eq!(tool, serde_json::json!({"role": "tool", "content": "ok", "tool_call_id": "c1"}));
}
