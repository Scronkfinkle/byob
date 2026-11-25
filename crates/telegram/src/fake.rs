//! A scripted Telegram Bot API server for tests, on a local TcpListener (never the real
//! Telegram): `getMe`, `getUpdates` answering what the test queued (or nothing, after a short
//! long-poll wait), `sendMessage` (with errors the test queued), `answerCallbackQuery` and
//! `deleteWebhook`. Every call is recorded. A wrong token gets 401. Test code: it panics on
//! what it can't do.
#![allow(clippy::unwrap_used)]

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

pub const TOKEN: &str = "123456:fake-T0KEN-s3cret";
pub const BOT: &str = "loom_test_bot";

/// What getUpdates answers next.
pub enum Next {
    Updates(Vec<Value>),
    /// An error answer: (HTTP status, Telegram's JSON).
    Error(u16, Value),
}

#[derive(Default)]
pub struct State {
    pub updates: VecDeque<Next>,
    pub send_errors: VecDeque<(u16, Value)>,
    /// (method, body) of every call.
    pub calls: Vec<(String, Value)>,
    next_message: i64,
}

pub struct Server {
    pub base: String,
    pub state: Arc<Mutex<State>>,
}

fn error(code: u16, description: &str) -> Value {
    json!({"ok": false, "error_code": code, "description": description})
}

impl Server {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State { next_message: 100, ..State::default() }));
        let st = Arc::clone(&state);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let st = Arc::clone(&st);
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        return;
                    }
                    let path = line.split_whitespace().nth(1).unwrap_or_default().to_owned();
                    let mut len = 0usize;
                    loop {
                        let mut h = String::new();
                        if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
                            break;
                        }
                        if let Some((k, v)) = h.split_once(':')
                            && k.eq_ignore_ascii_case("content-length")
                        {
                            len = v.trim().parse().unwrap_or(0);
                        }
                    }
                    let mut body = vec![0; len];
                    let _ = reader.read_exact(&mut body);
                    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    let (status, answer) = answer(&st, &path, body);
                    let text = answer.to_string();
                    let mut stream = stream;
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                        text.len()
                    );
                });
            }
        });
        Self { base, state }
    }

    pub fn push(&self, next: Next) {
        self.state.lock().unwrap().updates.push_back(next);
    }

    /// Bodies of every call of `method`.
    pub fn calls(&self, method: &str) -> Vec<Value> {
        self.state.lock().unwrap().calls.iter().filter(|(m, _)| m == method).map(|(_, b)| b.clone()).collect()
    }
}

fn answer(st: &Arc<Mutex<State>>, path: &str, body: Value) -> (u16, Value) {
    let Some(rest) = path.strip_prefix("/bot") else { return (404, error(404, "Not Found")) };
    let Some((token, method)) = rest.rsplit_once('/') else { return (404, error(404, "Not Found")) };
    st.lock().unwrap().calls.push((method.to_owned(), body.clone()));
    if token != TOKEN {
        return (401, error(401, "Unauthorized"));
    }
    match method {
        "getMe" => (200, json!({"ok": true, "result": {"id": 42, "is_bot": true, "first_name": "Loom", "username": BOT}})),
        "getUpdates" => {
            let wait = Duration::from_secs(body["timeout"].as_u64().unwrap_or(0)).min(Duration::from_millis(500));
            let end = Instant::now() + wait;
            loop {
                if let Some(next) = st.lock().unwrap().updates.pop_front() {
                    return match next {
                        Next::Updates(u) => (200, json!({"ok": true, "result": u})),
                        Next::Error(status, v) => (status, v),
                    };
                }
                if Instant::now() >= end {
                    return (200, json!({"ok": true, "result": []}));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        "sendMessage" => {
            let mut s = st.lock().unwrap();
            if let Some((status, v)) = s.send_errors.pop_front() {
                return (status, v);
            }
            s.next_message += 1;
            (200, json!({"ok": true, "result": {"message_id": s.next_message, "chat": {"id": body["chat_id"]}, "text": body["text"]}}))
        }
        "answerCallbackQuery" | "deleteWebhook" | "setMyCommands" => (200, json!({"ok": true, "result": true})),
        _ => (404, error(404, "Not Found: method not found")),
    }
}

/// A private-chat text message from user `from`.
pub fn text(update: i64, from: i64, text: &str) -> Value {
    json!({"update_id": update, "message": {"message_id": update, "from": {"id": from, "is_bot": false, "first_name": "Ada", "username": "ada"},
        "chat": {"id": from, "type": "private"}, "date": 0, "text": text}})
}
