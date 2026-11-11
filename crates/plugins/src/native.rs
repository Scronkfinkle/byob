//! Native plugin backends: in-tree Rust that plays a plugin backend's part inside byob's own
//! process, with the same message vocabulary as the JSON-lines wire format
//! (`docs/plugins-backend.md`).
//!
//! A manifest names one with `"backend": {"native": "<id>"}`; the app registers a
//! [`NativeFactory`] under that id (`byob_agent::Natives::backend`). The engine's backend host
//! ([`crate::backend::Backend`]) runs it exactly like a process backend: made on first use, on
//! a thread of its own (so tool calls and view requests never block the engine thread), sent
//! the same `init`, `event`, `request`, `cancel` and `shutdown` messages, answering with the
//! same `reply`, `view`, `notify`, `notice`, `log`, `fault`, `subscribe`, `artifact` and `web`
//! messages through an [`Outbox`]. A panic is a crash: the host catches it, records a fault,
//! drops the backend's state and makes a fresh one from the factory (with the process
//! backends' backoff and crash limit). Reload, switch-off and removal drop it.
//!
//! [`serve_stdio`] runs the same backend as a JSON-lines process instead, so one
//! implementation can be tested (or benchmarked) against a process backend's harness.

use std::io::{BufRead, Write};
use std::sync::Arc;

use serde_json::{Value, json};

/// A plugin backend in byob's process (see the module docs). Calls come one at a time, in
/// order, on the backend's own thread.
pub trait NativeBackend: Send {
    /// The `init` message: first after every (re)start (`restart` counts earlier starts).
    fn init(&mut self, init: &Value, out: &Outbox) {
        let _ = (init, out);
    }

    /// An `event` (`event`: `client`, `view`, `web`, or a subscribed engine event).
    fn event(&mut self, event: &Value, out: &Outbox) {
        let _ = (event, out);
    }

    /// A `request` (`method`: `tool`, `view`, `hook`, `web`): its `reply`'s result, or the
    /// error text. Messages sent through `out` meanwhile reach byob before the reply.
    fn request(&mut self, method: &str, params: &Value, out: &Outbox) -> Result<Value, String>;

    /// After the reply to a request went out (e.g. draw views again after a tool call).
    fn after_request(&mut self, method: &str, out: &Outbox) {
        let _ = (method, out);
    }

    /// `shutdown`: byob stops it (reload, switched off, removed); nothing more comes.
    fn shutdown(&mut self) {}
}

/// Makes a fresh backend (on every start and restart).
pub type NativeFactory = Arc<dyn Fn() -> Box<dyn NativeBackend> + Send + Sync>;

/// Where a native backend's messages go (byob's side of the wire).
#[derive(Clone)]
pub struct Outbox(Arc<dyn Fn(Value) + Send + Sync>);

impl Outbox {
    pub fn new(send: impl Fn(Value) + Send + Sync + 'static) -> Self {
        Self(Arc::new(send))
    }

    /// Any message (`{"type": …, …}`).
    pub fn send(&self, msg: Value) {
        (self.0)(msg);
    }

    /// A view's whole new tree (`values` and `client`: fields set for one client).
    pub fn view(&self, id: &str, key: &str, tree: Value) {
        self.send(json!({"type": "view", "id": id, "key": key, "view": tree}));
    }

    /// Forwarded to clients over the plugin channel (all of them without `client`).
    pub fn notify(&self, msg: Value, client: Option<u64>) {
        self.send(json!({"type": "notify", "msg": msg, "client": client}));
    }

    /// A notice in clients (`level`: `info` or `error`).
    pub fn notice(&self, text: &str, level: &str) {
        self.send(json!({"type": "notice", "text": text, "level": level}));
    }

    /// A line in its log (the stderr ring `plugin_status` shows).
    pub fn log(&self, text: &str) {
        self.send(json!({"type": "log", "text": text}));
    }

    /// A problem for the model and the user (kept in its status).
    pub fn fault(&self, text: &str) {
        self.send(json!({"type": "fault", "text": text}));
    }

    /// Engine events to get from now on.
    pub fn subscribe(&self, events: &[&str]) {
        self.send(json!({"type": "subscribe", "events": events}));
    }
}

/// Hand one message from byob to `backend` (the wire format's dispatch, shared by the
/// in-process host and [`serve_stdio`]). `false` after `shutdown`.
pub fn dispatch(backend: &mut dyn NativeBackend, msg: &Value, out: &Outbox) -> bool {
    match msg.get("type").and_then(Value::as_str).unwrap_or_default() {
        "init" => backend.init(msg, out),
        "event" => backend.event(msg, out),
        "request" => {
            let id = msg.get("id").cloned().unwrap_or(Value::Null);
            let method = msg.get("method").and_then(Value::as_str).unwrap_or_default();
            let params = msg.get("params").unwrap_or(&Value::Null);
            let reply = match backend.request(method, params, out) {
                Ok(result) => json!({"type": "reply", "id": id, "result": result}),
                Err(error) => json!({"type": "reply", "id": id, "error": error}),
            };
            out.send(reply);
            backend.after_request(method, out);
        }
        "shutdown" => {
            backend.shutdown();
            return false;
        }
        // `cancel` (a call can't be interrupted; its late reply is dropped) and newer types
        _ => {}
    }
    true
}

/// Serve `backend` as a JSON-lines process on stdin/stdout until stdin closes or byob sends
/// `shutdown`.
pub fn serve_stdio(mut backend: Box<dyn NativeBackend>) -> std::io::Result<()> {
    let stdout = std::sync::Mutex::new(std::io::stdout());
    let out = Outbox::new(move |msg| {
        if let Ok(mut line) = serde_json::to_vec(&msg) {
            line.push(b'\n');
            let mut w = stdout.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let _ = w.write_all(&line).and_then(|()| w.flush());
        }
    });
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(msg) = serde_json::from_str::<Value>(line.trim()) else { continue };
        if !msg.is_object() {
            continue;
        }
        if !dispatch(backend.as_mut(), &msg, &out) {
            break;
        }
    }
    Ok(())
}

/// A panic's message.
pub fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload.downcast_ref::<&str>().map(|s| (*s).to_owned()).or_else(|| payload.downcast_ref::<String>().cloned()).unwrap_or_else(|| "(no message)".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Echo;
    impl NativeBackend for Echo {
        fn request(&mut self, method: &str, params: &Value, out: &Outbox) -> Result<Value, String> {
            out.log("working");
            match method {
                "tool" => Ok(params.get("args").cloned().unwrap_or(Value::Null)),
                _ => Err(format!("no {method}")),
            }
        }
        fn after_request(&mut self, _: &str, out: &Outbox) {
            out.view("v", "", json!({"type": "text", "text": "after"}));
        }
    }

    #[test]
    fn dispatch_replies_in_order() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = Arc::clone(&seen);
        let out = Outbox::new(move |m| s.lock().unwrap().push(m));
        let mut b = Echo;
        assert!(dispatch(&mut b, &json!({"type": "request", "id": 3, "method": "tool", "params": {"args": {"x": 1}}}), &out));
        assert!(dispatch(&mut b, &json!({"type": "request", "id": 4, "method": "nope"}), &out));
        assert!(!dispatch(&mut b, &json!({"type": "shutdown"}), &out));
        let seen = seen.lock().unwrap();
        let types: Vec<&str> = seen.iter().map(|m| m["type"].as_str().unwrap()).collect();
        assert_eq!(types, ["log", "reply", "view", "log", "reply", "view"]);
        assert_eq!(seen[1]["result"], json!({"x": 1}));
        assert_eq!(seen[4]["error"], "no nope");
    }
}
