//! byob-mcp against a scripted stdio server (`tests/support/fake_server.rs`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;
use byob_mcp::{Content, McpClient, McpError, StdioConfig};

const SECS: Duration = Duration::from_secs(10);

fn config(mode: &str) -> StdioConfig {
    let mut cfg = StdioConfig::new(env!("CARGO_BIN_EXE_yah-mcp-fake-server")).env("FAKE_MCP_MODE", mode);
    cfg.init_timeout = SECS;
    cfg.request_timeout = SECS;
    cfg
}

fn connect(mode: &str) -> McpClient {
    McpClient::connect_stdio(&config(mode)).unwrap()
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
fn handshake_reports_server_info() {
    let c = connect("normal");
    let info = c.server_info();
    assert_eq!((info.name.as_str(), info.version.as_str()), ("fake", "1.2.3"));
    assert_eq!(info.protocol_version, byob_mcp::PROTOCOL_VERSION);
    assert_eq!(info.instructions.as_deref(), Some("be nice"));
    assert!(info.capabilities.get("tools").is_some());
    assert!(wait_for(|| c.stderr_tail().contains("notification notifications/initialized")));
}

#[test]
fn lists_tools_across_pages() {
    let c = connect("normal");
    let names: Vec<_> = c.list_tools().unwrap().into_iter().map(|t| t.name).collect();
    // Three pages; the nameless entry and the duplicate `echo` are dropped.
    assert_eq!(names, ["echo", "fail", "slow", "crash", "media", "big", "last_reply"]);
}

#[test]
fn calls_tools_and_declines_server_requests() {
    let c = connect("normal");
    let out = c.call_tool("echo", json!({ "text": "hello" }), SECS).unwrap();
    assert_eq!(out.text, "echo: hello");
    assert!(!out.is_error);
    // The server's `roots/list` request got a JSON-RPC error back.
    let last = c.call_tool("last_reply", json!(null), SECS).unwrap();
    let reply: serde_json::Value = serde_json::from_str(&last.text).unwrap();
    assert_eq!(reply["id"], "srv-1");
    assert_eq!(reply["error"]["code"], -32601);
}

#[test]
fn tool_errors_and_rpc_errors() {
    let c = connect("normal");
    let out = c.call_tool("fail", json!({}), SECS).unwrap();
    assert!(out.is_error);
    assert_eq!(out.text, "bad input");
    let err = c.call_tool("nope", json!({}), SECS).unwrap_err();
    assert_eq!(err, McpError::Rpc { code: -32602, message: "Unknown tool: nope".into() });
    // The connection survives both.
    assert!(c.is_alive());
    assert_eq!(c.call_tool("echo", json!({ "text": "still" }), SECS).unwrap().text, "echo: still");
}

#[test]
fn mixed_content_and_big_results() {
    let c = connect("normal");
    let out = c.call_tool("media", json!({}), SECS).unwrap();
    assert_eq!(out.text, "chart:\n[image image/png, 1 KiB]\nnotes");
    assert!(matches!(&out.content[1], Content::Image { mime_type, .. } if mime_type == "image/png"));
    let big = c.call_tool("big", json!({}), SECS).unwrap();
    assert!(big.text.len() <= byob_mcp::MAX_RESULT_TEXT + 32);
    assert!(big.text.ends_with("[output truncated]"));
}

#[test]
fn timeout_sends_cancelled_and_connection_survives() {
    let c = connect("normal");
    let start = Instant::now();
    let err = c.call_tool("slow", json!({ "ms": 3000 }), Duration::from_millis(300)).unwrap_err();
    assert_eq!(err, McpError::Timeout(Duration::from_millis(300)));
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(wait_for(|| c.stderr_tail().contains("cancelled")), "{}", c.stderr_tail());
    // Other calls still work while the slow one finishes in the background.
    assert_eq!(c.call_tool("echo", json!({ "text": "x" }), SECS).unwrap().text, "echo: x");
}

#[test]
fn cancel_flag_stops_a_call() {
    let c = connect("normal");
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancel);
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        flag.store(true, Ordering::Relaxed);
    });
    let start = Instant::now();
    let err = c.call_tool_cancellable("slow", json!({ "ms": 5000 }), SECS, &cancel).unwrap_err();
    assert_eq!(err, McpError::Cancelled);
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(wait_for(|| c.stderr_tail().contains("cancelled")));
}

#[test]
fn server_crash_fails_pending_and_later_calls() {
    let c = connect("normal");
    let err = c.call_tool("crash", json!({}), SECS).unwrap_err();
    let McpError::Closed(why) = &err else { panic!("{err:?}") };
    assert!(why.contains("code 3"), "{why}");
    assert!(why.contains("segfault (pretend)"), "{why}");
    assert!(err.is_disconnect());
    assert!(!c.is_alive());
    assert!(matches!(c.list_tools(), Err(McpError::Closed(_))));
}

#[test]
fn concurrent_calls_from_many_threads() {
    let c = connect("normal");
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let c = c.clone();
            thread::spawn(move || c.call_tool("echo", json!({ "text": format!("t{i}") }), SECS).unwrap().text)
        })
        .collect();
    for (i, h) in handles.into_iter().enumerate() {
        assert_eq!(h.join().unwrap(), format!("echo: t{i}"));
    }
}

#[test]
fn garbage_output_is_tolerated() {
    let c = connect("garbage");
    assert_eq!(c.server_info().name, "fake");
    assert_eq!(c.list_tools().unwrap().len(), 7);
    assert_eq!(c.call_tool("echo", json!({ "text": "ok" }), SECS).unwrap().text, "echo: ok");
}

#[test]
fn exit_during_handshake_reports_stderr() {
    let err = McpClient::connect_stdio(&config("exit-on-init")).unwrap_err();
    let McpError::Closed(why) = &err else { panic!("{err:?}") };
    assert!(why.contains("code 7") && why.contains("missing API key"), "{why}");
}

#[test]
fn silent_server_times_out_handshake() {
    let mut cfg = config("silent");
    cfg.init_timeout = Duration::from_millis(300);
    assert_eq!(McpClient::connect_stdio(&cfg).unwrap_err(), McpError::Timeout(Duration::from_millis(300)));
}

#[test]
fn close_stops_the_server() {
    let c = connect("normal");
    c.close();
    assert!(!c.is_alive());
    assert!(matches!(c.call_tool("echo", json!({}), SECS), Err(McpError::Closed(_))));
}
