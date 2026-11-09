//! A scripted MCP server on stdio for yah-mcp's integration tests. Not shipped.
//!
//! `FAKE_MCP_MODE` picks misbehaviour: `normal` (default), `garbage` (junk lines before
//! every reply), `exit-on-init` (dies during the handshake), `silent` (never answers).

use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

type Out = Arc<Mutex<std::io::Stdout>>;

fn send(out: &Out, mode: &str, msg: &Value) {
    let mut o = out.lock().unwrap_or_else(PoisonError::into_inner);
    if mode == "garbage" {
        let _ = writeln!(o, "Server starting... not JSON");
        let _ = writeln!(o, "{{\"jsonrpc\":\"2.0\",\"id\":\"weird\",\"result\":1}}");
        let _ = writeln!(o, "[1,2,3]");
        let _ = writeln!(o, "{{\"jsonrpc\":\"2.0\",\"id\":424242,\"result\":{{}}}}");
        let _ = writeln!(o, "{{truncated");
        let _ = writeln!(o, "{}", "x".repeat(17 * 1024 * 1024));
        let _ = writeln!(o);
    }
    let _ = writeln!(o, "{msg}");
    let _ = o.flush();
}

fn tool(name: &str, desc: &str) -> Value {
    json!({ "name": name, "description": desc, "inputSchema": { "type": "object", "properties": {} } })
}

fn text(t: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": t }] })
}

fn main() {
    let mode = std::env::var("FAKE_MCP_MODE").unwrap_or_else(|_| "normal".into());
    let out: Out = Arc::new(Mutex::new(std::io::stdout()));
    let last_reply = Arc::new(Mutex::new(Value::Null));
    eprintln!("fake server up (mode {mode})");
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or_default().to_owned();
        let id = msg.get("id").cloned();
        if method.is_empty() {
            // A reply to one of our requests.
            *last_reply.lock().unwrap_or_else(PoisonError::into_inner) = msg;
            continue;
        }
        if mode == "silent" {
            continue;
        }
        let Some(id) = id else {
            if method == "notifications/cancelled" {
                eprintln!("cancelled {}", msg["params"]["requestId"]);
            } else {
                eprintln!("notification {method}");
            }
            continue;
        };
        let rid = id.clone();
        let reply = move |result: Value| json!({ "jsonrpc": "2.0", "id": rid, "result": result });
        match method.as_str() {
            "initialize" => {
                if mode == "exit-on-init" {
                    eprintln!("fatal: missing API key");
                    std::process::exit(7);
                }
                send(
                    &out,
                    &mode,
                    &reply(json!({
                        "protocolVersion": msg["params"]["protocolVersion"],
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "fake", "version": "1.2.3" },
                        "instructions": "be nice",
                    })),
                );
            }
            "tools/list" => {
                let page = match msg["params"]["cursor"].as_str() {
                    None => json!({ "tools": [tool("echo", "Echo text"), tool("fail", "Always fails"), { "description": "nameless" }], "nextCursor": "p2" }),
                    Some("p2") => json!({ "tools": [tool("slow", "Sleeps"), tool("crash", "Exits"), tool("echo", "duplicate")], "nextCursor": "p3" }),
                    Some(_) => json!({ "tools": [tool("media", "Mixed content"), tool("big", "Huge"), tool("last_reply", "What the client last answered")] }),
                };
                send(&out, &mode, &reply(page));
            }
            "tools/call" => {
                let args = msg["params"]["arguments"].clone();
                match msg["params"]["name"].as_str().unwrap_or_default() {
                    "echo" => {
                        // Chatter first: a log notification and a request yah must decline.
                        send(&out, &mode, &json!({ "jsonrpc": "2.0", "method": "notifications/message", "params": { "level": "info", "data": "hi" } }));
                        send(&out, &mode, &json!({ "jsonrpc": "2.0", "id": "srv-1", "method": "roots/list" }));
                        send(&out, &mode, &reply(text(&format!("echo: {}", args["text"].as_str().unwrap_or_default()))));
                    }
                    "fail" => send(&out, &mode, &reply(json!({ "content": [{ "type": "text", "text": "bad input" }], "isError": true }))),
                    "slow" => {
                        let out = Arc::clone(&out);
                        let mode = mode.clone();
                        let ms = args["ms"].as_u64().unwrap_or(2000);
                        thread::spawn(move || {
                            thread::sleep(Duration::from_millis(ms));
                            send(&out, &mode, &reply(text("finally")));
                        });
                    }
                    "crash" => {
                        eprintln!("segfault (pretend)");
                        std::process::exit(3);
                    }
                    "media" => send(
                        &out,
                        &mode,
                        &reply(json!({ "content": [
                            { "type": "text", "text": "chart:" },
                            { "type": "image", "mimeType": "image/png", "data": "iVBORw0KGgo=" },
                            { "type": "resource", "resource": { "uri": "file:///notes.txt", "text": "notes" } },
                        ]})),
                    ),
                    "big" => send(&out, &mode, &reply(text(&"y".repeat(3 * 1024 * 1024)))),
                    "last_reply" => {
                        let last = last_reply.lock().unwrap_or_else(PoisonError::into_inner).to_string();
                        send(&out, &mode, &reply(text(&last)));
                    }
                    other => {
                        send(&out, &mode, &json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32602, "message": format!("Unknown tool: {other}") } }))
                    }
                }
            }
            _ => send(&out, &mode, &json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "Method not found" } })),
        }
    }
    eprintln!("stdin closed, bye");
}
