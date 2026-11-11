//! Web surfaces and their bridge (13.5+, cap [`CAP_WEB_BRIDGE`]).
//!
//! A web surface is HTML byob runs sandboxed (the Servo helper on the desktop, an iframe in
//! the browser) that talks to byob over a typed bridge: JSON-RPC 2.0 messages, the
//! [MCP Apps](https://github.com/modelcontextprotocol/ext-apps) dialect (`ui/initialize`,
//! `tools/call`, `ui/message`, `ui/update-model-context`, `ui/open-link`, …). Who answers a
//! surface's calls is its [`WebOwner`]: an MCP server (an MCP App: a tool that declares
//! `_meta.ui.resourceUri`), or a plugin (a view declared with `html`, or an artifact
//! renderer `web:<file>`).
//!
//! The client is the host the page talks to. It answers what is about the client itself
//! (`ui/initialize` with the theme as CSS variables, `ui/open-link`, size changes) and sends
//! the rest to the engine as a [`crate::ClientMsg::WebCall`], answered by one
//! [`crate::ServerMsg::WebReply`] with the same `call` number. The engine routes a call to
//! the owner (`tools/call` to the same MCP server through the permission gate, or to the
//! plugin's backend tools; `byob/send` to the plugin's backend), or handles it itself
//! (`byob/document`: the surface's HTML; `ui/message`: a user message in the chat;
//! `ui/update-model-context`: text for the model's next request). Owners push messages to
//! their surfaces with [`crate::ServerMsg::WebPush`].

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Cap: the peer handles [`crate::ClientMsg::WebCall`], [`crate::ServerMsg::WebReply`],
/// [`crate::ServerMsg::WebPush`] and the `byob.mcp_app` / `byob.web` step views.
pub const CAP_WEB_BRIDGE: &str = "web_bridge";

/// The MCP Apps extension id (in `initialize` capabilities).
pub const MCP_UI_EXTENSION: &str = "io.modelcontextprotocol/ui";
/// The MIME type of an MCP App's HTML resource.
pub const MCP_APP_MIME: &str = "text/html;profile=mcp-app";
/// The MCP Apps protocol version byob speaks with views.
pub const MCP_APPS_VERSION: &str = "2026-01-26";

/// Largest call or push (the JSON of `params` / `msg`).
pub const MAX_CALL_BYTES: usize = 256 * 1024;
/// Largest HTML document the engine sends for a surface.
pub const MAX_DOCUMENT_BYTES: usize = 3 * 1024 * 1024;
/// Longest method name.
pub const MAX_METHOD_CHARS: usize = 64;
/// Longest resource URI.
pub const MAX_URI_CHARS: usize = 2048;

/// Who answers a web surface's calls.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WebOwner {
    /// An MCP App: the `ui://` resource `uri` of server `server` of plugin `plugin`.
    Mcp { plugin: String, server: String, uri: String },
    /// A plugin's web view (`views: [{site, id, html}]`; `key` as for tree views) or a
    /// `web:<file>` artifact renderer (`view` is `artifact:<file>`, `key` the artifact id).
    Plugin {
        plugin: String,
        view: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        key: String,
    },
    /// An owner a newer peer knows (its calls are refused).
    #[serde(other)]
    Other,
}

impl WebOwner {
    /// The plugin behind the surface.
    pub fn plugin(&self) -> Option<&str> {
        match self {
            Self::Mcp { plugin, .. } | Self::Plugin { plugin, .. } => Some(plugin),
            Self::Other => None,
        }
    }

    /// Check an untrusted owner's ids and sizes.
    pub fn check(&self) -> Result<(), String> {
        let id = |s: &str| {
            if byob_types::config::plugin_id_ok(s) { Ok(()) } else { Err(format!("{:?} isn't a plugin id", s.chars().take(60).collect::<String>())) }
        };
        match self {
            Self::Mcp { plugin, server, uri } => {
                id(plugin)?;
                if server.is_empty() || server.chars().count() > 128 {
                    return Err("bad server name".into());
                }
                if !uri.starts_with("ui://") || uri.chars().count() > MAX_URI_CHARS {
                    return Err("an MCP App's resource must be a ui:// URI".into());
                }
                Ok(())
            }
            Self::Plugin { plugin, view, key } => {
                id(plugin)?;
                if view.is_empty() || view.chars().count() > 300 {
                    return Err("bad view id".into());
                }
                if key.chars().count() > crate::sites::MAX_VIEW_KEY_CHARS {
                    return Err("bad view key".into());
                }
                Ok(())
            }
            Self::Other => Err("unknown web surface owner".into()),
        }
    }
}

/// What the engine says when a call needs the user's go-ahead first (a `tools/call` the
/// permission gate would ask about): the client asks and sends the call again with
/// `approved`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebApproval {
    /// What would run, e.g. "Weather: refresh_dashboard".
    pub title: String,
    pub access: crate::ToolAccess,
}

/// Why a call failed (a JSON-RPC error for the page).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebError {
    /// JSON-RPC error code (-32601 method not found, -32602 bad params, -32000 refused).
    pub code: i64,
    pub message: String,
    /// Set when the user has to allow it first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<WebApproval>,
}

impl WebError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), approval: None }
    }
}

/// The answer to `byob/document`: the surface's HTML and what it asks of the sandbox.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WebDocument {
    pub html: String,
    /// The resource's `_meta.ui.csp` as declared (byob keeps its own strict policy and only
    /// reports these; see the module docs of the client).
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub csp: Value,
    /// `_meta.ui.permissions` as declared (none are granted).
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub permissions: Value,
    /// `_meta.ui.prefersBorder`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefers_border: Option<bool>,
}

/// An MCP App on a tool's step (a [`crate::View`] of kind [`McpApp::KIND`]): the tool
/// declared `_meta.ui.resourceUri`, so clients render that resource with the call's input
/// and result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpApp {
    pub plugin: String,
    pub server: String,
    /// The server's name for the tool.
    pub tool: String,
    /// The `ui://` resource.
    pub uri: String,
    /// What the step is called (the tool's title).
    #[serde(default)]
    pub title: String,
    /// The call's arguments.
    #[serde(default)]
    pub input: Value,
    /// The `CallToolResult` (content, structuredContent, isError), possibly shortened.
    #[serde(default)]
    pub result: Value,
    /// The tool's definition as the server listed it (for `hostContext.toolInfo`).
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub tool_def: Value,
}

impl McpApp {
    pub fn owner(&self) -> WebOwner {
        WebOwner::Mcp { plugin: self.plugin.clone(), server: self.server.clone(), uri: self.uri.clone() }
    }
}

impl crate::ViewData for McpApp {
    const KIND: &'static str = "byob.mcp_app";
    const V: u32 = 1;
    fn fallback(&self) -> String {
        let text: String = self
            .result
            .get("content")
            .and_then(Value::as_array)
            .map(|c| c.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"))
            .unwrap_or_default();
        let title = if self.title.is_empty() { &self.tool } else { &self.title };
        let text: String = text.chars().take(4000).collect();
        format!("**{title}** (an app; this version of byob can't show it)\n\n{text}")
    }
}

/// A plugin's web view on a step (a [`crate::View`] of kind [`WebCard::KIND`]): a backend
/// tool returned `card_view` naming a view with `html`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebCard {
    pub plugin: String,
    pub view: String,
    #[serde(default)]
    pub key: String,
    /// What the tool gave the card (sent to the page as `byob/card`).
    #[serde(default)]
    pub data: Value,
    #[serde(default)]
    pub text: String,
}

impl WebCard {
    pub fn owner(&self) -> WebOwner {
        WebOwner::Plugin { plugin: self.plugin.clone(), view: self.view.clone(), key: self.key.clone() }
    }
}

impl crate::ViewData for WebCard {
    const KIND: &'static str = "byob.web";
    const V: u32 = 1;
    fn fallback(&self) -> String {
        self.text.chars().take(4000).collect()
    }
}

/// Size checks of the bridge messages (see [`crate::ClientMsg::check_sizes`]).
pub(crate) fn check_call(owner: &WebOwner, method: &str, params: &Value) -> Result<(), String> {
    owner.check()?;
    if method.is_empty() || method.chars().count() > MAX_METHOD_CHARS {
        return Err("bad method".into());
    }
    let size = serde_json::to_string(params).map_or(usize::MAX, |s| s.len());
    if size > MAX_CALL_BYTES {
        return Err(format!("web call too large ({size} bytes, at most {MAX_CALL_BYTES})"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClientMsg, ServerMsg, ViewData as _};
    use serde_json::json;

    fn mcp() -> WebOwner {
        WebOwner::Mcp { plugin: "weather".into(), server: "main".into(), uri: "ui://weather/dash".into() }
    }

    #[test]
    fn bridge_messages_round_trip_and_are_checked() {
        let call = ClientMsg::WebCall { call: 7, owner: mcp(), method: "tools/call".into(), params: json!({"name": "refresh"}), approved: false };
        assert_eq!(ClientMsg::from_json(&call.to_json()).unwrap(), call);
        assert!(call.check_sizes().is_ok());
        let j = call.to_json();
        assert!(j.contains(r#""owner":{"kind":"mcp","plugin":"weather","server":"main","uri":"ui://weather/dash"}"#), "{j}");
        let plugin = WebOwner::Plugin { plugin: "sheets".into(), view: "grid".into(), key: String::new() };
        assert_eq!(serde_json::to_string(&plugin).unwrap(), r#"{"kind":"plugin","plugin":"sheets","view":"grid"}"#);
        for bad in [
            WebOwner::Mcp { plugin: "Weather".into(), server: "main".into(), uri: "ui://x".into() },
            WebOwner::Mcp { plugin: "weather".into(), server: "main".into(), uri: "https://x".into() },
            WebOwner::Plugin { plugin: "p".into(), view: String::new(), key: String::new() },
            WebOwner::Other,
        ] {
            assert!(ClientMsg::WebCall { call: 1, owner: bad, method: "x".into(), params: Value::Null, approved: false }.check_sizes().is_err());
        }
        let big = ClientMsg::WebCall { call: 1, owner: mcp(), method: "x".into(), params: json!("x".repeat(MAX_CALL_BYTES)), approved: false };
        assert!(big.check_sizes().is_err());
        // A newer peer's owner kind decodes (and is refused).
        let other: WebOwner = serde_json::from_str(r#"{"kind":"canvas","x":1}"#).unwrap();
        assert_eq!(other, WebOwner::Other);

        for m in [
            ServerMsg::WebReply { call: 7, result: Some(json!({"content": []})), error: None },
            ServerMsg::WebReply {
                call: 8,
                result: None,
                error: Some(WebError {
                    code: -32000,
                    message: "ask first".into(),
                    approval: Some(WebApproval { title: "t".into(), access: crate::ToolAccess::Network }),
                }),
            },
            ServerMsg::WebPush { owner: plugin, msg: json!({"jsonrpc": "2.0", "method": "byob/message", "params": {"n": 1}}) },
        ] {
            assert_eq!(ServerMsg::from_json(&m.to_json()).unwrap(), m);
        }
    }

    #[test]
    fn mcp_app_views_fall_back_to_their_text() {
        let app = McpApp {
            plugin: "weather".into(),
            server: "main".into(),
            tool: "get_weather".into(),
            uri: "ui://weather/dash".into(),
            title: "Weather".into(),
            input: json!({"city": "Lisbon"}),
            result: json!({"content": [{"type": "text", "text": "21°C"}]}),
            tool_def: Value::Null,
        };
        let v = crate::View::of(&app);
        assert_eq!(v.kind, "byob.mcp_app");
        assert!(v.fallback.contains("21°C") && v.fallback.contains("Weather"));
        assert_eq!(v.get::<McpApp>(), Some(app.clone()));
        assert_eq!(app.owner(), mcp());
        assert!(v.check().is_ok());
        assert!(!serde_json::to_string(&app).unwrap().contains("tool_def"));
        assert_eq!(WebCard { plugin: "p".into(), view: "v".into(), key: "k".into(), data: Value::Null, text: "hi".into() }.fallback(), "hi");
    }
}
