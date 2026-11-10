//! Engine tests of the endpoint link: checks, connection tests, keys following (or not) a
//! URL change, and retrying a reply that couldn't reach the server.

use std::io::{BufRead, BufReader, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use byob_protocol::{ClientMsg, Connection, EngineState, EntryRole, InferenceTest, Link, PROTOCOL_VERSION, Problem, ServerMsg};
use byob_store::{Paths, Secrets, Settings};
use byob_types::InferenceConfig;

use crate::{AdaptOptions, EngineConfig, EngineHandle, LocalConnection};

/// What a fake server answers: (status, body). `n` counts chat requests.
type Handler = dyn Fn(&str, usize) -> (u16, String) + Send + Sync;

/// An HTTP server answering every request with `handler(path, n)`; chat requests (`POST
/// /v1/chat/completions`) are streamed as SSE when the status is 200.
fn server(handler: impl Fn(&str, usize) -> (u16, String) + Send + Sync + 'static) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handler: Arc<Handler> = Arc::new(handler);
    let chats = Arc::new(AtomicUsize::new(0));
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let (handler, chats) = (Arc::clone(&handler), Arc::clone(&chats));
            thread::spawn(move || {
                let path = read_path(&mut s);
                let chat = path.ends_with("/chat/completions");
                let n = if chat { chats.fetch_add(1, Ordering::SeqCst) } else { 0 };
                let (status, body) = handler(&path, n);
                let ctype = if chat && status == 200 { "text/event-stream" } else { "application/json" };
                let _ = write!(s, "HTTP/1.1 {status} X\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            });
        }
    });
    format!("http://{addr}/v1")
}

/// Read a request (head and body); returns its path.
fn read_path(s: &mut std::net::TcpStream) -> String {
    let mut r = BufReader::new(s);
    let mut first = String::new();
    let _ = r.read_line(&mut first);
    let path = first.split_whitespace().nth(1).unwrap_or_default().to_owned();
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            len = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; len];
    let _ = r.read_exact(&mut body);
    path
}

fn models(ids: &[&str]) -> String {
    serde_json::json!({"object": "list", "data": ids.iter().map(|id| serde_json::json!({"id": id, "object": "model"})).collect::<Vec<_>>()}).to_string()
}

fn sse(text: &str) -> String {
    format!("data: {}\n\ndata: [DONE]\n\n", serde_json::json!({"choices":[{"delta":{"content": text}}]}))
}

/// A server listing `ids` that streams "hi" to chat requests.
fn ok_server(ids: &'static [&'static str]) -> String {
    server(move |path, _| {
        if path.ends_with("/models") {
            (200, models(ids))
        } else if path.ends_with("/chat/completions") {
            (200, sse("hi"))
        } else {
            (404, "{}".into())
        }
    })
}

fn engine(url: &str, checks: bool, paths: Option<Paths>) -> EngineHandle {
    let mut c = EngineConfig::new(InferenceConfig { base_url: url.into(), model: "m".into(), api_key: Some("sk-old".into()) });
    c.paths = paths;
    c.auto_title = false;
    c.adapt = AdaptOptions { probe: checks, ..Default::default() };
    EngineHandle::spawn_with(c).unwrap()
}

fn recv(conn: &mut LocalConnection) -> ServerMsg {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(m) = conn.try_recv() {
            return m;
        }
        assert!(Instant::now() < deadline, "timed out waiting for engine");
        thread::sleep(Duration::from_millis(2));
    }
}

fn connect(engine: &EngineHandle) -> LocalConnection {
    let mut c = engine.connect(Arc::new(|| {}));
    c.send(ClientMsg::Hello { protocol: PROTOCOL_VERSION, client: "test".into() });
    c
}

/// The first state matching `f`.
fn state_where(c: &mut LocalConnection, f: impl Fn(&EngineState) -> bool) -> EngineState {
    loop {
        if let ServerMsg::State(s) = recv(c)
            && f(&s)
        {
            return s;
        }
    }
}

fn tested(c: &mut LocalConnection) -> InferenceTest {
    loop {
        if let ServerMsg::InferenceTested(t) = recv(c) {
            return t;
        }
    }
}

fn finished(c: &mut LocalConnection) -> Option<String> {
    loop {
        if let ServerMsg::EntryFinished { error, .. } = recv(c) {
            return error;
        }
    }
}

#[test]
fn a_working_endpoint_is_checked_and_remembered() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::at(dir.path());
    let url = ok_server(&["m"]);
    let e = engine(&url, true, Some(paths.clone()));
    let mut c = connect(&e);
    let s = state_where(&mut c, |s| s.link.is_ok());
    assert!(s.onboarded);
    assert_eq!(Settings::load(&paths.settings()).value.inference.last_good.as_deref(), Some(url.as_str()));
}

#[test]
fn a_dead_endpoint_fails_the_check_in_plain_words() {
    let e = engine("http://127.0.0.1:9/v1", true, None);
    let mut c = connect(&e);
    let s = state_where(&mut c, |s| matches!(s.link, Link::Failed { .. }));
    let Link::Failed { problem, detail } = s.link else { unreachable!() };
    assert_eq!(problem, Some(Problem::Unreachable));
    assert!(detail.contains("127.0.0.1:9"));
    assert!(!s.onboarded);
}

#[test]
fn connection_tests_explain_what_is_wrong() {
    let e = engine("http://127.0.0.1:9/v1", false, None);
    let mut c = connect(&e);
    // The key stays with its server: a test elsewhere goes without it and gets a 401.
    let locked = server(|path, _| if path.ends_with("/models") { (401, r#"{"error":{"message":"Incorrect API key"}}"#.into()) } else { (404, "{}".into()) });
    c.send(ClientMsg::TestInference { base_url: locked.clone(), model: "m".into(), api_key: None });
    let t = tested(&mut c);
    assert_eq!((t.ok, t.problem, t.base_url.as_str()), (false, Some(Problem::Unauthorized), locked.as_str()));

    let ollama_like = ok_server(&["llama3", "qwen3"]);
    c.send(ClientMsg::TestInference { base_url: ollama_like.clone(), model: "default".into(), api_key: None });
    let t = tested(&mut c);
    assert_eq!((t.ok, t.problem), (false, Some(Problem::ModelNotFound)));
    assert_eq!(t.models, ["llama3", "qwen3"]);
    c.send(ClientMsg::TestInference { base_url: ollama_like, model: "qwen3".into(), api_key: None });
    let t = tested(&mut c);
    assert!(t.ok, "{t:?}");
    assert_eq!(t.detail, "2 models");

    c.send(ClientMsg::TestInference { base_url: "http://127.0.0.1:9/v1".into(), model: "m".into(), api_key: None });
    assert_eq!(tested(&mut c).problem, Some(Problem::Unreachable));
    c.send(ClientMsg::TestInference { base_url: "ftp://nope".into(), model: "m".into(), api_key: None });
    assert_eq!(tested(&mut c).problem, Some(Problem::BadUrl));
    c.send(ClientMsg::TestInference { base_url: "x".repeat(5000), model: "m".into(), api_key: None });
    assert!(tested(&mut c).detail.contains("too long"));
}

#[test]
fn the_key_follows_only_to_the_same_server() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::at(dir.path());
    let e = engine("http://127.0.0.1:9/v1", false, Some(paths.clone()));
    let mut c = connect(&e);
    let key = || Secrets::load(&paths.secrets()).value.api_key;
    // Same server, another path: the key stays.
    c.send(ClientMsg::SetInference { base_url: "http://127.0.0.1:9/api/v1".into(), model: "m".into(), api_key: None });
    assert!(state_where(&mut c, |s| s.inference_base_url.ends_with("/api/v1")).has_api_key);
    // Another server: dropped (and removed from secrets.json).
    c.send(ClientMsg::SetInference { base_url: "http://other:8000/v1".into(), model: "m".into(), api_key: None });
    assert!(!state_where(&mut c, |s| s.inference_base_url.contains("other")).has_api_key);
    assert_eq!(key(), None);
    c.send(ClientMsg::SetInference { base_url: "https://api.example.com/v1".into(), model: "m".into(), api_key: Some(" sk-new ".into()) });
    assert!(state_where(&mut c, |s| s.inference_base_url.contains("example")).has_api_key);
    assert_eq!(key().as_deref(), Some("sk-new"));
    // An empty key removes it.
    c.send(ClientMsg::SetInference { base_url: "https://api.example.com/v1".into(), model: "m2".into(), api_key: Some(String::new()) });
    assert!(!state_where(&mut c, |s| s.inference_model == "m2").has_api_key);
    assert_eq!(key(), None);
}

#[test]
fn a_failed_reply_breaks_the_link_and_retry_runs_it_again() {
    // Down for the first chat request, fine afterwards.
    let url = server(|path, n| match (path.ends_with("/chat/completions"), n) {
        (true, 0) => (503, r#"{"error":{"message":"Loading model"}}"#.into()),
        (true, _) => (200, sse("back again")),
        _ => (404, "{}".into()),
    });
    let e = engine(&url, false, None);
    let mut c = connect(&e);
    c.send(ClientMsg::SendMessage { text: "hello?".into(), attachments: vec![] });
    let error = finished(&mut c).unwrap();
    assert_eq!(Problem::classify(&error), Some(Problem::ServerError));
    let s = state_where(&mut c, |s| matches!(s.link, Link::Failed { .. }));
    assert!(matches!(s.link, Link::Failed { problem: Some(Problem::ServerError), .. }));

    c.send(ClientMsg::Retry);
    // The failed pair goes; the message is sent again.
    loop {
        if let ServerMsg::Transcript { entries } = recv(&mut c) {
            assert!(entries.is_empty());
            break;
        }
    }
    assert_eq!(finished(&mut c), None);
    state_where(&mut c, |s| s.link.is_ok());
    // A late client sees one clean exchange.
    let mut late = connect(&e);
    let entries = loop {
        if let ServerMsg::Transcript { entries } = recv(&mut late) {
            break entries;
        }
    };
    let shape: Vec<_> = entries.iter().map(|e| (e.role, e.content.as_str(), e.error.is_some())).collect();
    assert_eq!(shape, [(EntryRole::User, "hello?", false), (EntryRole::Assistant, "back again", false)]);
    // Nothing failed now: nothing to retry.
    c.send(ClientMsg::Retry);
    loop {
        if let ServerMsg::Notice { text, .. } = recv(&mut c) {
            assert!(text.contains("only a reply that failed"));
            break;
        }
    }
}
