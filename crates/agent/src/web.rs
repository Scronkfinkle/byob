//! Web surfaces' calls in the engine (`ClientMsg::WebCall`, see `byob_protocol::web`).
//!
//! A client hosts the page and answers what is about itself; everything else comes here
//! and goes to the surface's owner:
//!
//! | method | MCP App (`WebOwner::Mcp`) | plugin web view (`WebOwner::Plugin`) |
//! |---|---|---|
//! | `byob/document` | `resources/read` of its `ui://` resource | the view's HTML file |
//! | `tools/call` | the same server's tool (not one with `visibility` without `"app"`), through the permission gate | one of the plugin's backend tools (its own UI: no gate) |
//! | `resources/read` | the same server | – |
//! | `byob/send` / `byob/call` | – | a `web` event / request to the plugin's backend |
//! | `ui/message` | a user message in the client's chat (queued if a reply runs) | same |
//! | `ui/update-model-context` | text in the chat's next system prompt (one per surface, the last wins) | same |
//! | `notifications/message` | byob's log | same |
//!
//! Anything slow (servers, backends, files) runs on a worker thread that answers with
//! `Input::Reply`, so a stuck server never blocks the engine.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use byob_plugins::{Connector, ConnectorTool};
use byob_protocol::web::{MAX_DOCUMENT_BYTES, WebApproval, WebDocument, WebError, WebOwner};
use byob_protocol::{ServerMsg, ToolAccess};
use serde_json::{Value, json};

use crate::{ClientId, Engine, Input};

/// JSON-RPC error codes.
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const REFUSED: i64 = -32000;

/// Longest model context one surface keeps (characters).
const MAX_CONTEXT_CHARS: usize = 8000;
/// Most surfaces with model context per chat.
const MAX_CONTEXTS: usize = 8;
/// How long an app's `tools/call` may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// Largest `CallToolResult` sent back to a page.
const MAX_RESULT_BYTES: usize = 1024 * 1024;

/// An MCP server of a switched-on plugin that is up (for MCP Apps' calls).
#[derive(Clone)]
pub(crate) struct AppServer {
    pub plugin: String,
    pub server: String,
    pub local: bool,
    pub connector: Arc<dyn Connector>,
    /// Every tool it lists, app-only ones included.
    pub tools: Vec<ConnectorTool>,
    /// The server's tool name → its name for the model (where grants are kept).
    pub names: Vec<(String, String)>,
}

/// A worker's answer to a web call.
fn reply(call: u64, result: Result<Value, WebError>) -> ServerMsg {
    match result {
        Ok(v) => ServerMsg::WebReply { call, result: Some(v), error: None },
        Err(e) => ServerMsg::WebReply { call, result: None, error: Some(e) },
    }
}

/// `ui/message`'s text, for a user message in the client's chat (queued if a reply runs).
fn message_text(params: &Value) -> Result<String, WebError> {
    if params.get("role").and_then(Value::as_str).is_some_and(|r| r != "user") {
        return Err(WebError::new(INVALID_PARAMS, "only user messages can be sent"));
    }
    let text = content_text(params.get("content"));
    let text = text.trim();
    if text.is_empty() {
        return Err(WebError::new(INVALID_PARAMS, "the message has no text"));
    }
    Ok(text.chars().take(32 * 1024).collect())
}

/// The text of `content` blocks (`{type: "text", text}`; one block or a list).
fn content_text(content: Option<&Value>) -> String {
    let blocks: Vec<&Value> = match content {
        Some(Value::Array(a)) => a.iter().collect(),
        Some(v @ Value::Object(_)) => vec![v],
        _ => Vec::new(),
    };
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// An MCP App's HTML from a `resources/read` result.
fn mcp_document(read: &Value, uri: &str) -> Result<WebDocument, String> {
    let contents = read.get("contents").and_then(Value::as_array).ok_or("the server sent no contents")?;
    let item = contents.iter().find(|c| c.get("uri").and_then(Value::as_str) == Some(uri)).or_else(|| contents.first()).ok_or("the resource is empty")?;
    let mime = item.get("mimeType").and_then(Value::as_str).unwrap_or_default();
    if !mime.starts_with("text/html") {
        return Err(format!("the resource is {mime:?}, not text/html;profile=mcp-app"));
    }
    let html = match (item.get("text").and_then(Value::as_str), item.get("blob").and_then(Value::as_str)) {
        (Some(t), _) => t.to_owned(),
        (None, Some(b)) => {
            String::from_utf8(byob_plugins::base64_decode(b).ok_or("the resource's blob isn't base64")?).map_err(|_| "the resource isn't UTF-8")?
        }
        (None, None) => return Err("the resource has neither text nor blob".into()),
    };
    if html.len() > MAX_DOCUMENT_BYTES {
        return Err(format!("the app is too large ({} KB, at most {} KB)", html.len() / 1024, MAX_DOCUMENT_BYTES / 1024));
    }
    let meta = item.pointer("/_meta/ui").cloned().unwrap_or(Value::Null);
    let csp = meta.get("csp").cloned().unwrap_or(Value::Null);
    if !csp.is_null() {
        // Audit trail (the spec asks hosts to log it); byob keeps its own strict policy.
        log::info!("MCP App {uri}: declares CSP {csp} (not granted: byob's sandbox stays closed)");
    }
    Ok(WebDocument {
        html,
        csp,
        permissions: meta.get("permissions").cloned().unwrap_or(Value::Null),
        prefers_border: meta.get("prefersBorder").and_then(Value::as_bool),
    })
}

/// Run `work` on a worker thread; its result goes back to `client` as the reply to `call`.
fn spawn_reply(inbox: &std::sync::mpsc::Sender<Input>, client: ClientId, call: u64, work: impl FnOnce() -> Result<Value, WebError> + Send + 'static) {
    let inbox = inbox.clone();
    let spawned = std::thread::Builder::new().name("byob-web-call".into()).spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).unwrap_or_else(|_| Err(WebError::new(REFUSED, "that failed unexpectedly")));
        let _ = inbox.send(Input::Reply { client, msg: Box::new(reply(call, result)) });
    });
    if let Err(e) = spawned {
        log::error!("could not start a web call: {e}");
    }
}

impl Engine {
    /// `ClientMsg::WebCall`.
    pub(crate) fn web_call(&mut self, client: ClientId, call: u64, owner: WebOwner, method: &str, params: Value, approved: bool) {
        let result = match method {
            "ui/message" => {
                // Answered first: the page needn't wait for the reply to start.
                match message_text(&params) {
                    Ok(text) => {
                        self.send_to(client, reply(call, Ok(json!({}))));
                        self.start_turn(client, text, &[]);
                    }
                    Err(e) => self.send_to(client, reply(call, Err(e))),
                }
                return;
            }
            "ui/update-model-context" => self.web_context(client, &owner, &params),
            "notifications/message" => {
                log::info!("web surface {}: {}", self.web_label(&owner), params.to_string().chars().take(500).collect::<String>());
                Ok(json!({}))
            }
            "ping" => Ok(json!({})),
            _ => match &owner {
                WebOwner::Mcp { .. } => return self.mcp_call(client, call, &owner, method, params, approved),
                WebOwner::Plugin { .. } => return self.plugin_web_call(client, call, &owner, method, params),
                WebOwner::Other => Err(WebError::new(METHOD_NOT_FOUND, "unknown surface")),
            },
        };
        self.send_to(client, reply(call, result));
    }

    fn web_label(&self, owner: &WebOwner) -> String {
        match owner {
            WebOwner::Mcp { plugin, server, uri } => format!("{plugin}/{server} app {uri}"),
            WebOwner::Plugin { plugin, view, key } if key.is_empty() => format!("{plugin} view {view}"),
            WebOwner::Plugin { plugin, view, key } => format!("{plugin} view {view} ({key})"),
            WebOwner::Other => "a web surface".into(),
        }
    }

    /// `ui/update-model-context`: kept per chat and surface, for the next system prompt.
    fn web_context(&mut self, client: ClientId, owner: &WebOwner, params: &Value) -> Result<Value, WebError> {
        let Some(chat) = self.client_chat(client) else { return Err(WebError::new(REFUSED, "open a chat first")) };
        let mut text = content_text(params.get("content"));
        if let Some(s) = params.get("structuredContent").filter(|s| !s.is_null()) {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&s.to_string());
        }
        let text: String = text.chars().take(MAX_CONTEXT_CHARS).collect();
        let label = self.web_label(owner);
        let Ok(live) = self.live(&chat) else { return Err(WebError::new(REFUSED, "the chat can't be opened")) };
        if text.trim().is_empty() {
            live.app_context.remove(&label);
        } else {
            if !live.app_context.contains_key(&label) && live.app_context.len() >= MAX_CONTEXTS {
                return Err(WebError::new(REFUSED, format!("at most {MAX_CONTEXTS} apps can give the model context in one chat")));
            }
            live.app_context.insert(label, text);
        }
        Ok(json!({}))
    }

    /// The MCP server behind an MCP App, in the client's chat.
    fn app_server(&mut self, client: ClientId, plugin: &str, server: &str) -> Result<(AppServer, PathBuf), WebError> {
        let chat = self.client_chat(client);
        let ws = self.ext_workspace(chat.as_deref(), Some(client));
        let Some(c) = self.compose_for(chat.as_deref(), Some(client), false) else { return Err(WebError::new(REFUSED, "plugins are turned off")) };
        let found = c.apps.into_iter().find(|a| a.plugin == plugin && a.server == server);
        found.map(|a| (a, ws)).ok_or_else(|| WebError::new(REFUSED, format!("{plugin}'s server {server} isn't running (is the plugin switched on?)")))
    }

    fn mcp_call(&mut self, client: ClientId, call: u64, owner: &WebOwner, method: &str, params: Value, approved: bool) {
        let WebOwner::Mcp { plugin, server, uri } = owner else { return };
        let (app, ws) = match self.app_server(client, plugin, server) {
            Ok(a) => a,
            Err(e) => return self.send_to(client, reply(call, Err(e))),
        };
        match method {
            "byob/document" => {
                let uri = uri.clone();
                spawn_reply(&self.inbox, client, call, move || {
                    let read = app.connector.read_resource(&uri).map_err(|e| WebError::new(REFUSED, e))?;
                    let doc = mcp_document(&read, &uri).map_err(|e| WebError::new(REFUSED, e))?;
                    serde_json::to_value(doc).map_err(|e| WebError::new(REFUSED, e.to_string()))
                });
            }
            "resources/read" => {
                let Some(want) = params.get("uri").and_then(Value::as_str).map(str::to_owned) else {
                    return self.send_to(client, reply(call, Err(WebError::new(INVALID_PARAMS, "resources/read needs a uri"))));
                };
                spawn_reply(&self.inbox, client, call, move || app.connector.read_resource(&want).map_err(|e| WebError::new(REFUSED, e)));
            }
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
                let Some(tool) = app.tools.iter().find(|t| t.name == name).cloned() else {
                    return self.send_to(client, reply(call, Err(WebError::new(INVALID_PARAMS, format!("{server} has no tool {name:?}")))));
                };
                if tool.model_only {
                    return self.send_to(client, reply(call, Err(WebError::new(REFUSED, format!("{name} can't be called from an app")))));
                }
                let access = if tool.read_only() {
                    ToolAccess::Read
                } else if app.local {
                    ToolAccess::Execute
                } else {
                    ToolAccess::Network
                };
                if let Err(e) = self.web_gate(client, &app, &tool, access, approved) {
                    return self.send_to(client, reply(call, Err(e)));
                }
                let args = match params.get("arguments") {
                    Some(a @ Value::Object(_)) => a.clone(),
                    _ => json!({}),
                };
                log::info!("MCP App {uri}: calls {plugin}/{server} {name}");
                spawn_reply(&self.inbox, client, call, move || {
                    let cancel = AtomicBool::new(false);
                    let ctx = byob_tools::ToolContext::new(ws.as_path(), &cancel);
                    let r = app.connector.call_tool(&name, &args, &ctx).map_err(|e| WebError::new(REFUSED, e))?;
                    Ok(r.for_app(MAX_RESULT_BYTES))
                });
            }
            _ => self.send_to(client, reply(call, Err(WebError::new(METHOD_NOT_FOUND, format!("byob doesn't handle {method}"))))),
        }
    }

    /// The permission gate for a call an app makes: the chat's rules and grants, as for
    /// the model's calls. `Err` with `approval` when the user has to say yes first.
    fn web_gate(&mut self, client: ClientId, app: &AppServer, tool: &ConnectorTool, access: ToolAccess, approved: bool) -> Result<(), WebError> {
        let chat = self.client_chat(client);
        let settings = self.chat_settings(chat.as_deref(), Some(client));
        let policy = self.policy_for(&settings.effective);
        let model_name = app.names.iter().find(|(n, _)| *n == tool.name).map(|(_, m)| m.clone()).unwrap_or_else(|| tool.name.clone());
        let granted = chat.as_deref().is_some_and(|c| self.store.is_allowed(c, &model_name).unwrap_or(false));
        match crate::permissions::decide(&policy, granted, access) {
            crate::permissions::Gate::Allow => Ok(()),
            crate::permissions::Gate::Deny(why) => Err(WebError::new(REFUSED, format!("Not run: {why}"))),
            crate::permissions::Gate::Ask if approved => Ok(()),
            crate::permissions::Gate::Ask => Err(WebError {
                code: REFUSED,
                message: "this call needs your OK".into(),
                approval: Some(WebApproval { title: format!("{}: {}", app.plugin, tool.label()), access }),
            }),
        }
    }

    fn plugin_web_call(&mut self, client: ClientId, call: u64, owner: &WebOwner, method: &str, params: Value) {
        let WebOwner::Plugin { plugin, view, key } = owner else { return };
        let chat = self.client_chat(client);
        let Some(c) = self.compose_for(chat.as_deref(), Some(client), false) else {
            return self.send_to(client, reply(call, Err(WebError::new(REFUSED, "plugins are turned off"))));
        };
        let Some(info) = c.infos.iter().find(|p| p.id == *plugin && p.in_use() && p.enabled && p.trusted) else {
            return self.send_to(client, reply(call, Err(WebError::new(REFUSED, format!("plugin {plugin} isn't switched on here")))));
        };
        let dir = PathBuf::from(&info.dir);
        match method {
            "byob/document" => {
                let file = match view.strip_prefix("artifact:") {
                    Some(f) => info.renderers.iter().find_map(|r| r.renderer.strip_prefix("web:").filter(|w| w.trim() == f).map(|w| w.trim().to_owned())),
                    None => info.views.iter().find(|v| v.id == *view && !v.html.is_empty()).map(|v| v.html.clone()),
                };
                let Some(file) = file.filter(|f| byob_plugins::manifest::path_inside(f)) else {
                    return self.send_to(client, reply(call, Err(WebError::new(REFUSED, format!("{plugin} declares no web view {view}")))));
                };
                spawn_reply(&self.inbox, client, call, move || {
                    let path = dir.join(&file);
                    let meta = std::fs::metadata(&path).map_err(|e| WebError::new(REFUSED, format!("{file}: {e}")))?;
                    if meta.len() > MAX_DOCUMENT_BYTES as u64 {
                        return Err(WebError::new(REFUSED, format!("{file} is too large")));
                    }
                    let html = std::fs::read_to_string(&path).map_err(|e| WebError::new(REFUSED, format!("{file}: {e}")))?;
                    serde_json::to_value(WebDocument { html, ..Default::default() }).map_err(|e| WebError::new(REFUSED, e.to_string()))
                });
            }
            "tools/call" => {
                // The plugin's own page calling the plugin's own backend tools: like a click in
                // its tree view, no approval card.
                let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
                let found = c.owners.iter().find(|(_, o)| o.plugin == *plugin && o.dir == dir && o.declared == name).map(|(n, _)| n.clone());
                let Some(tool) = found.and_then(|n| c.tools.get(&n).cloned()) else {
                    return self.send_to(client, reply(call, Err(WebError::new(INVALID_PARAMS, format!("{plugin} has no tool {name:?}")))));
                };
                let args = match params.get("arguments") {
                    Some(a @ Value::Object(_)) => a.clone(),
                    _ => json!({}),
                };
                let ws = self.ext_workspace(chat.as_deref(), Some(client));
                spawn_reply(&self.inbox, client, call, move || {
                    let cancel = AtomicBool::new(false);
                    let mut ctx = byob_tools::ToolContext::new(ws.as_path(), &cancel);
                    if let Some(c) = chat.as_deref() {
                        ctx = ctx.with_chat(c);
                    }
                    let out = tool.run(&args, &ctx);
                    Ok(json!({"content": [{"type": "text", "text": out.text}], "isError": out.failed}))
                });
            }
            "byob/send" | "byob/call" => {
                let backend = self.ext.as_ref().and_then(|e| e.backends.get(&dir)).map(|e| Arc::clone(&e.backend));
                let Some(backend) = backend else {
                    return self.send_to(client, reply(call, Err(WebError::new(REFUSED, format!("{plugin} has no backend running")))));
                };
                let event = json!({"view": view, "key": key, "client": client, "chat": chat, "msg": params});
                if method == "byob/send" {
                    let mut e = event;
                    if let Some(o) = e.as_object_mut() {
                        o.insert("type".into(), json!("event"));
                        o.insert("event".into(), json!("web"));
                    }
                    let r = if backend.event(e) { Ok(json!({})) } else { Err(WebError::new(REFUSED, format!("{plugin}'s backend isn't running"))) };
                    return self.send_to(client, reply(call, r));
                }
                spawn_reply(&self.inbox, client, call, move || backend.request("web", event, CALL_TIMEOUT, &|| false).map_err(|e| WebError::new(REFUSED, e)));
            }
            _ => self.send_to(client, reply(call, Err(WebError::new(METHOD_NOT_FOUND, format!("byob doesn't handle {method}"))))),
        }
    }

    /// A plugin backend's `{"type": "web", "view", "key"?, "msg", "client"?}`: to the pages
    /// of that view (only clients that listed the `web_bridge` cap).
    pub(crate) fn backend_web(&mut self, plugin: &str, chat: Option<&str>, msg: &Value) {
        let Some(view) = msg.get("view").and_then(Value::as_str) else { return };
        let key = msg.get("key").and_then(Value::as_str).unwrap_or_default();
        let inner = msg.get("msg").cloned().unwrap_or(Value::Null);
        if serde_json::to_string(&inner).map_or(usize::MAX, |s| s.len()) > byob_protocol::web::MAX_CALL_BYTES {
            log::warn!("plugin {plugin}: web message too large; dropped");
            return;
        }
        let owner = WebOwner::Plugin { plugin: plugin.to_owned(), view: view.to_owned(), key: key.to_owned() };
        let to = msg.get("client").and_then(Value::as_u64);
        let out = ServerMsg::WebPush { owner, msg: inner };
        let ids: Vec<ClientId> = self
            .clients
            .iter()
            .filter(|(i, c)| to.is_none_or(|t| t == **i) && c.attached && c.caps.iter().any(|c| c == byob_protocol::web::CAP_WEB_BRIDGE))
            .filter(|(_, c)| to.is_some() || chat.is_none_or(|ch| c.chat.as_deref() == Some(ch)))
            .map(|(i, _)| *i)
            .collect();
        for i in ids {
            self.send_to(i, out.clone());
        }
    }
}

/// The chat's app context as one system-prompt section.
pub(crate) fn context_section(contexts: &std::collections::BTreeMap<String, String>) -> String {
    if contexts.is_empty() {
        return String::new();
    }
    let mut s = String::from("## What the user's open apps report\nApps shown in this chat keep this up to date (newest state; it may change):");
    for (label, text) in contexts {
        s.push_str(&format!("\n\n### {label}\n{text}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_from_resources() {
        let read = json!({"contents": [{"uri": "ui://w/d", "mimeType": "text/html;profile=mcp-app", "text": "<p>hi</p>",
            "_meta": {"ui": {"csp": {"connectDomains": ["https://api.x"]}, "prefersBorder": true}}}]});
        let d = mcp_document(&read, "ui://w/d").unwrap();
        assert_eq!(d.html, "<p>hi</p>");
        assert_eq!(d.prefers_border, Some(true));
        assert_eq!(d.csp["connectDomains"][0], "https://api.x");
        let blob = json!({"contents": [{"uri": "ui://w/d", "mimeType": "text/html;profile=mcp-app", "blob": "PHA+aGk8L3A+"}]});
        assert_eq!(mcp_document(&blob, "ui://w/d").unwrap().html, "<p>hi</p>");
        assert!(mcp_document(&json!({"contents": [{"mimeType": "image/png", "blob": "AA=="}]}), "ui://w/d").is_err());
        assert!(mcp_document(&json!({"contents": []}), "ui://w/d").is_err());
        assert!(mcp_document(&json!({}), "ui://w/d").is_err());
    }

    #[test]
    fn content_text_and_sections() {
        assert_eq!(content_text(Some(&json!({"type": "text", "text": "a"}))), "a");
        assert_eq!(content_text(Some(&json!([{"type": "text", "text": "a"}, {"type": "image"}, {"type": "text", "text": "b"}]))), "a\nb");
        assert_eq!(content_text(None), "");
        let mut m = std::collections::BTreeMap::new();
        assert_eq!(context_section(&m), "");
        m.insert("w/main app ui://w/d".to_owned(), "city: Lisbon".to_owned());
        let s = context_section(&m);
        assert!(s.contains("### w/main app ui://w/d\ncity: Lisbon"), "{s}");
    }
}
