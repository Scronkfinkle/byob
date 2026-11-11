//! JSON-RPC 2.0 plumbing shared by both transports: request ids, waiting callers, and
//! routing of whatever the server sends.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, PoisonError};

use serde_json::{Value, json};

use crate::McpError;
use crate::text::truncate;

/// Longest error message we keep from a server.
const MAX_ERROR_MESSAGE: usize = 2000;

pub(crate) type Reply = Result<Value, McpError>;

#[derive(Default)]
struct State {
    pending: HashMap<u64, Sender<Reply>>,
    /// Set once the connection is gone; no new requests are accepted after that.
    gone: Option<String>,
}

/// Matches answers to waiting callers.
#[derive(Default)]
pub(crate) struct Router {
    state: Mutex<State>,
}

impl Router {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Start waiting for the answer to `id`. Fails if the connection is already gone.
    pub fn register(&self, id: u64) -> Result<Receiver<Reply>, McpError> {
        let mut st = self.state();
        if let Some(why) = &st.gone {
            return Err(McpError::Closed(why.clone()));
        }
        let (tx, rx) = mpsc::channel();
        st.pending.insert(id, tx);
        Ok(rx)
    }

    /// Stop waiting for `id`. Returns whether it was still pending.
    pub fn forget(&self, id: u64) -> bool {
        self.state().pending.remove(&id).is_some()
    }

    /// Hand `reply` to whoever waits for `id` (no-op if nobody does any more).
    pub fn resolve(&self, id: u64, reply: Reply) {
        let tx = self.state().pending.remove(&id);
        if let Some(tx) = tx {
            let _ = tx.send(reply);
        }
    }

    /// The connection is gone: fail every waiting caller and refuse new requests.
    pub fn shut(&self, why: &str) {
        self.shut_answering(why, None);
    }

    /// [`shut`](Self::shut), but caller `answer.0` gets `answer.1` instead of `Closed`. The
    /// connection is marked gone before anyone is woken, so a caller that checks
    /// [`gone`](Self::gone) right after its reply sees it.
    pub fn shut_answering(&self, why: &str, answer: Option<(u64, Reply)>) {
        let mut pending = {
            let mut st = self.state();
            if st.gone.is_none() {
                st.gone = Some(why.to_owned());
            }
            std::mem::take(&mut st.pending)
        };
        if let Some((id, reply)) = answer
            && let Some(tx) = pending.remove(&id)
        {
            let _ = tx.send(reply);
        }
        for tx in pending.into_values() {
            let _ = tx.send(Err(McpError::Closed(why.to_owned())));
        }
    }

    pub fn gone(&self) -> Option<String> {
        self.state().gone.clone()
    }

    /// Route one parsed message (or batch) from the server. Returns the replies the
    /// transport must send back (answers to server→client requests).
    pub fn deliver(&self, msg: Value) -> Vec<Value> {
        match msg {
            Value::Array(items) => items.into_iter().take(256).filter_map(|m| self.deliver_one(m)).collect(),
            other => self.deliver_one(other).into_iter().collect(),
        }
    }

    fn deliver_one(&self, msg: Value) -> Option<Value> {
        let Value::Object(mut obj) = msg else {
            log::debug!("mcp: ignoring non-object message");
            return None;
        };
        if let Some(method) = obj.get("method").and_then(Value::as_str) {
            let id = obj.get("id").filter(|id| id.is_string() || id.is_number())?;
            // A request from the server (ping, roots, sampling, elicitation): answer ping,
            // decline the rest.
            let reply = if method == "ping" {
                json!({ "jsonrpc": "2.0", "id": id, "result": {} })
            } else {
                log::debug!("mcp: declining server request `{}`", truncate(method, 100));
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "Method not supported by byob" } })
            };
            return Some(reply);
        }
        let Some(id) = obj.get("id").and_then(response_id) else {
            log::debug!("mcp: ignoring message without a usable id");
            return None;
        };
        let reply = if let Some(err) = obj.get("error") {
            let code = err.get("code").and_then(Value::as_i64).unwrap_or(-32603);
            let message = err.get("message").and_then(Value::as_str).unwrap_or("the connector returned an error");
            Err(McpError::Rpc { code, message: truncate(message, MAX_ERROR_MESSAGE) })
        } else if let Some(result) = obj.remove("result") {
            Ok(result)
        } else {
            Err(McpError::Protocol("response has neither result nor error".into()))
        };
        self.resolve(id, reply);
        None
    }
}

/// Our ids are numbers; tolerate servers that echo them back as strings.
pub(crate) fn response_id(id: &Value) -> Option<u64> {
    match id {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// The id of `msg` if it is a response (not a request/notification).
pub(crate) fn answer_id(msg: &Value) -> Option<u64> {
    if msg.get("method").is_some() {
        return None;
    }
    msg.get("id").and_then(response_id)
}

pub(crate) fn request(id: u64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

pub(crate) fn notification(method: &str, params: Option<Value>) -> Value {
    match params {
        Some(p) => json!({ "jsonrpc": "2.0", "method": method, "params": p }),
        None => json!({ "jsonrpc": "2.0", "method": method }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_results_and_errors_by_id() {
        let r = Router::default();
        let a = r.register(1).unwrap();
        let b = r.register(2).unwrap();
        assert!(r.deliver(json!({"jsonrpc":"2.0","id":1,"result":{"ok":true}})).is_empty());
        assert!(r.deliver(json!({"jsonrpc":"2.0","id":"2","error":{"code":-32602,"message":"nope"}})).is_empty());
        assert_eq!(a.recv().unwrap(), Ok(json!({"ok":true})));
        assert_eq!(b.recv().unwrap(), Err(McpError::Rpc { code: -32602, message: "nope".into() }));
    }

    #[test]
    fn answers_server_requests_and_ignores_garbage() {
        let r = Router::default();
        let replies = r.deliver(json!([
            {"jsonrpc":"2.0","id":"s1","method":"ping"},
            {"jsonrpc":"2.0","id":7,"method":"sampling/createMessage","params":{}},
            {"jsonrpc":"2.0","method":"notifications/message","params":{}},
            {"jsonrpc":"2.0","id":99,"result":{}},
            {"id":null},
            42,
            "str",
        ]));
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0], json!({"jsonrpc":"2.0","id":"s1","result":{}}));
        assert_eq!(replies[1]["id"], json!(7));
        assert_eq!(replies[1]["error"]["code"], json!(-32601));
    }

    #[test]
    fn the_answered_caller_wakes_to_a_connection_already_gone() {
        let r = Router::default();
        let (a, b) = (r.register(1).unwrap(), r.register(2).unwrap());
        r.shut_answering("expired", Some((1, Err(McpError::SessionExpired))));
        assert_eq!(a.recv().unwrap(), Err(McpError::SessionExpired));
        assert!(r.gone().is_some());
        assert_eq!(b.recv().unwrap(), Err(McpError::Closed("expired".into())));
    }

    #[test]
    fn shut_fails_pending_and_refuses_new() {
        let r = Router::default();
        let a = r.register(1).unwrap();
        r.shut("exited");
        assert_eq!(a.recv().unwrap(), Err(McpError::Closed("exited".into())));
        assert_eq!(r.register(2).unwrap_err(), McpError::Closed("exited".into()));
    }
}
