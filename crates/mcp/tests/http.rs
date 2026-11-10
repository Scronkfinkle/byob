//! byob-mcp's Streamable HTTP transport against an in-process fake server.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use byob_mcp::{HttpConfig, McpClient, McpError, TokenProvider};
use serde_json::{Value, json};

const SECS: Duration = Duration::from_secs(10);
const SESSION: &str = "sess-1";

#[derive(Debug, Clone)]
struct Seen {
    method: String,
    headers: Vec<(String, String)>,
    body: Value,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    fn rpc_method(&self) -> &str {
        self.body["method"].as_str().unwrap_or_default()
    }
}

struct Fake {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Fake {
    fn start(token: Option<&'static str>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let log = Arc::clone(&log);
                thread::spawn(move || handle(stream, token, &log));
            }
        });
        Self { url, seen }
    }

    fn config(&self) -> HttpConfig {
        let mut cfg = HttpConfig::new(&self.url);
        cfg.init_timeout = SECS;
        cfg.request_timeout = SECS;
        cfg
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

fn respond(s: &mut TcpStream, status: &str, headers: &[(&str, &str)], body: &str) {
    let mut out = format!("HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    out.push_str(body);
    let _ = s.write_all(out.as_bytes());
}

/// Start an SSE response; the caller writes events and closes (or holds) the stream.
fn sse_head(s: &mut TcpStream) {
    let _ = s.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: text/event-stream\r\n\r\n");
}

fn event(s: &mut TcpStream, data: &Value) {
    let _ = s.write_all(format!("event: message\ndata: {data}\n\n").as_bytes());
    let _ = s.flush();
}

fn result(id: &Value, r: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": r })
}

fn text(t: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": t }] })
}

fn handle(mut stream: TcpStream, token: Option<&str>, log: &Mutex<Vec<Seen>>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut first = String::new();
    reader.read_line(&mut first).unwrap();
    let method = first.split(' ').next().unwrap_or_default().to_owned();
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_owned(), v.trim().to_owned()));
        }
    }
    let len = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    let mut body = vec![0; len];
    reader.read_exact(&mut body).unwrap();
    let seen = Seen { method, headers, body: serde_json::from_slice(&body).unwrap_or(Value::Null) };
    log.lock().unwrap().push(seen.clone());
    let s = &mut stream;

    if let Some(t) = token
        && seen.header("authorization") != Some(&format!("Bearer {t}"))
    {
        return respond(s, "401 Unauthorized", &[("WWW-Authenticate", r#"Bearer resource_metadata="x""#)], "");
    }
    if seen.method == "DELETE" {
        return respond(s, "200 OK", &[], "");
    }
    let id = seen.body.get("id").cloned();
    let rpc = seen.rpc_method().to_owned();
    let Some(id) = id.filter(|_| !rpc.is_empty()) else {
        // Notifications and our replies to server requests.
        return respond(s, "202 Accepted", &[], "");
    };
    if rpc == "initialize" {
        let r =
            result(&id, json!({ "protocolVersion": "2025-03-26", "capabilities": { "tools": {} }, "serverInfo": { "name": "fake-http", "version": "0.9" } }));
        return respond(s, "200 OK", &[("Content-Type", "application/json"), ("Mcp-Session-Id", SESSION)], &r.to_string());
    }
    if seen.header("mcp-session-id") != Some(SESSION) {
        return respond(s, "400 Bad Request", &[], "missing session");
    }
    match rpc.as_str() {
        "tools/list" if seen.body["params"]["cursor"].is_null() => {
            sse_head(s);
            let _ = s.write_all(b": keep-alive comment\n\nevent: message\ndata: not json\n\n");
            event(s, &json!({ "jsonrpc": "2.0", "method": "notifications/message", "params": {} }));
            event(s, &json!({ "jsonrpc": "2.0", "id": "srv-ping", "method": "ping" }));
            event(s, &result(&id, json!({ "tools": [{ "name": "echo" }, { "name": "slow" }], "nextCursor": "c2" })));
            // Hold the stream open: the client must stop reading once it has its answer.
            thread::sleep(Duration::from_secs(5));
        }
        "tools/list" => {
            let r = result(&id, json!({ "tools": [{ "name": "broken", "annotations": { "readOnlyHint": true } }] }));
            respond(s, "200 OK", &[("Content-Type", "application/json")], &r.to_string());
        }
        "tools/call" => match seen.body["params"]["name"].as_str().unwrap_or_default() {
            "echo" => {
                let r = result(&id, text(&format!("echo: {}", seen.body["params"]["arguments"]["text"].as_str().unwrap_or_default())));
                respond(s, "200 OK", &[("Content-Type", "application/json; charset=utf-8")], &r.to_string());
            }
            "sse_echo" => {
                sse_head(s);
                event(s, &result(&id, json!({ "content": [{ "type": "text", "text": "streamed" }], "isError": true })));
            }
            "hangup" => {
                sse_head(s);
                event(s, &json!({ "jsonrpc": "2.0", "method": "notifications/progress", "params": {} }));
            }
            "slow" => {
                thread::sleep(Duration::from_secs(3));
                respond(s, "200 OK", &[("Content-Type", "application/json")], &result(&id, text("late")).to_string());
            }
            "broken" => respond(s, "500 Internal Server Error", &[], "kaput"),
            "expire" => respond(s, "404 Not Found", &[], ""),
            _ => respond(s, "200 OK", &[("Content-Type", "application/json")], "{garbage"),
        },
        _ => respond(
            s,
            "200 OK",
            &[("Content-Type", "application/json")],
            &json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "nope" } }).to_string(),
        ),
    }
}

fn wait_for(mut cond: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + SECS;
    while Instant::now() < end {
        if cond() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn handshake_session_and_protocol_headers() {
    let fake = Fake::start(None);
    let c = McpClient::connect_http(&fake.config()).unwrap();
    assert_eq!(c.server_info().name, "fake-http");
    assert_eq!(c.call_tool("echo", json!({ "text": "hi" }), SECS).unwrap().text, "echo: hi");
    let seen = fake.seen();
    let init = &seen[0];
    assert_eq!(init.rpc_method(), "initialize");
    assert_eq!(init.body["params"]["protocolVersion"], byob_mcp::PROTOCOL_VERSION);
    assert!(init.header("accept").unwrap().contains("text/event-stream"));
    assert_eq!(init.header("mcp-session-id"), None);
    let initialized = &seen[1];
    assert_eq!(initialized.rpc_method(), "notifications/initialized");
    assert_eq!(initialized.header("mcp-session-id"), Some(SESSION));
    // The version the server chose is what later requests announce.
    assert_eq!(seen[2].header("mcp-protocol-version"), Some("2025-03-26"));
}

#[test]
fn lists_tools_over_sse_and_json_pages() {
    let fake = Fake::start(None);
    let c = McpClient::connect_http(&fake.config()).unwrap();
    let start = Instant::now();
    let tools = c.list_tools().unwrap();
    // The first page's stream stays open for 5 s; we must not wait for it to end.
    assert!(start.elapsed() < Duration::from_secs(3));
    assert_eq!(tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["echo", "slow", "broken"]);
    assert_eq!(tools[2].annotations.read_only_hint, Some(true));
    // The server's ping (sent mid-stream) was answered.
    assert!(wait_for(|| fake.seen().iter().any(|s| s.body["id"] == "srv-ping" && s.body["result"] == json!({}))));
}

#[test]
fn sse_results_errors_and_hangups() {
    let fake = Fake::start(None);
    let c = McpClient::connect_http(&fake.config()).unwrap();
    let out = c.call_tool("sse_echo", json!({}), SECS).unwrap();
    assert_eq!(out.text, "streamed");
    assert!(out.is_error);
    assert!(matches!(c.call_tool("hangup", json!({}), SECS), Err(McpError::Protocol(_))));
    let err = c.call_tool("broken", json!({}), SECS).unwrap_err();
    assert!(matches!(&err, McpError::Http { status: 500, message, .. } if message == "kaput"), "{err:?}");
    assert!(matches!(c.call_tool("garbage", json!({}), SECS), Err(McpError::Protocol(_))));
    assert_eq!(c.call_tool("echo", json!({ "text": "alive" }), SECS).unwrap().text, "echo: alive");
}

#[test]
fn timeout_sends_cancelled() {
    let fake = Fake::start(None);
    let c = McpClient::connect_http(&fake.config()).unwrap();
    let err = c.call_tool("slow", json!({}), Duration::from_millis(300)).unwrap_err();
    assert_eq!(err, McpError::Timeout(Duration::from_millis(300)));
    let cancelled = fake.seen().into_iter().find(|s| s.rpc_method() == "notifications/cancelled").unwrap();
    assert_eq!(cancelled.header("mcp-session-id"), Some(SESSION));
}

#[test]
fn expired_session_is_reported() {
    let fake = Fake::start(None);
    let c = McpClient::connect_http(&fake.config()).unwrap();
    assert_eq!(c.call_tool("expire", json!({}), SECS).unwrap_err(), McpError::SessionExpired);
    assert!(!c.is_alive());
}

#[test]
fn close_deletes_the_session() {
    let fake = Fake::start(None);
    let c = McpClient::connect_http(&fake.config()).unwrap();
    drop(c);
    assert!(wait_for(|| fake.seen().iter().any(|s| s.method == "DELETE" && s.header("mcp-session-id") == Some(SESSION))));
}

#[test]
fn bearer_token_and_auth_required() {
    let fake = Fake::start(Some("s3cret"));
    let err = McpClient::connect_http(&fake.config()).unwrap_err();
    assert!(matches!(&err, McpError::AuthRequired { www_authenticate } if www_authenticate.contains("resource_metadata")), "{err:?}");
    let c = McpClient::connect_http(&fake.config().header("Authorization", "Bearer wrong").bearer("s3cret")).unwrap();
    assert_eq!(c.call_tool("echo", json!({ "text": "authed" }), SECS).unwrap().text, "echo: authed");
}

#[test]
fn token_provider_is_refreshed_once_on_401() {
    struct Provider(Mutex<&'static str>);
    impl TokenProvider for Provider {
        fn token(&self) -> Option<String> {
            Some(self.0.lock().unwrap().to_string())
        }
        fn refresh(&self) -> bool {
            *self.0.lock().unwrap() = "s3cret";
            true
        }
    }
    let fake = Fake::start(Some("s3cret"));
    let mut cfg = fake.config();
    cfg.auth = Some(Arc::new(Provider(Mutex::new("stale"))));
    let c = McpClient::connect_http(&cfg).unwrap();
    assert_eq!(c.server_info().name, "fake-http");
}

#[test]
fn unreachable_server_is_a_transport_error() {
    // Bind then drop to get a port nobody listens on.
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut cfg = HttpConfig::new(format!("http://127.0.0.1:{port}/mcp"));
    cfg.init_timeout = SECS;
    let err = McpClient::connect_http(&cfg).unwrap_err();
    assert!(matches!(err, McpError::Transport { .. }), "{err:?}");
    assert!(err.is_disconnect());
}
