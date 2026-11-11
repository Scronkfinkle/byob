//! What servers tell us, parsed defensively from untrusted JSON.

use serde_json::{Map, Value, json};

use crate::text::truncate;

/// Longest tool name we accept (longer ones are skipped).
const MAX_NAME: usize = 256;
/// Longest tool title/description we keep.
const MAX_DESCRIPTION: usize = 8 * 1024;
/// Most result text handed to the caller.
pub const MAX_RESULT_TEXT: usize = 1024 * 1024;
/// Longest server instructions we keep.
const MAX_INSTRUCTIONS: usize = 16 * 1024;

/// Who answered `initialize`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
    /// The protocol version the server chose.
    pub protocol_version: String,
    /// Optional usage hints for the model.
    pub instructions: Option<String>,
    /// The server's raw `capabilities` object.
    pub capabilities: Value,
}

impl ServerInfo {
    pub(crate) fn from_result(v: &Value) -> Self {
        let info = v.get("serverInfo");
        let s = |o: Option<&Value>, k: &str| o.and_then(|o| o.get(k)).and_then(Value::as_str).map(|s| truncate(s, MAX_NAME)).unwrap_or_default();
        Self {
            name: s(info, "name"),
            version: s(info, "version"),
            protocol_version: s(Some(v), "protocolVersion"),
            instructions: v.get("instructions").and_then(Value::as_str).map(|t| truncate(t, MAX_INSTRUCTIONS)),
            capabilities: v.get("capabilities").filter(|c| c.is_object()).cloned().unwrap_or_else(|| json!({})),
        }
    }
}

/// Behaviour hints a server may give for a tool. Untrusted: hints, not guarantees.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolAnnotations {
    pub title: Option<String>,
    pub read_only_hint: Option<bool>,
    pub destructive_hint: Option<bool>,
    pub idempotent_hint: Option<bool>,
    pub open_world_hint: Option<bool>,
}

/// One tool a server offers.
#[derive(Debug, Clone, PartialEq)]
pub struct McpTool {
    pub name: String,
    pub title: Option<String>,
    /// Empty if the server gave none.
    pub description: String,
    /// JSON Schema of the arguments; `{"type":"object"}` if missing or not an object.
    pub input_schema: Value,
    pub annotations: ToolAnnotations,
    /// MCP Apps: `_meta.ui` (or the deprecated `_meta["ui/resourceUri"]`), if the tool has a UI.
    pub ui: Option<ToolUi>,
    /// The tool's entry as listed (for an MCP App's `hostContext.toolInfo`), capped.
    pub raw: Value,
}

/// An MCP Apps tool's UI (`_meta.ui`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolUi {
    /// The `ui://` resource that renders its results; `None` for app-only helpers without one.
    pub resource_uri: Option<String>,
    /// The model may see and call it (`visibility` has `"model"`; the default).
    pub model: bool,
    /// Its server's apps may call it (`visibility` has `"app"`; the default).
    pub app: bool,
}

impl ToolUi {
    fn from_meta(meta: Option<&Value>) -> Option<Self> {
        let meta = meta?;
        let ui = meta.get("ui");
        let uri = ui
            .and_then(|u| u.get("resourceUri"))
            .or_else(|| meta.get("ui/resourceUri"))
            .and_then(Value::as_str)
            .filter(|u| u.starts_with("ui://") && u.len() <= 2048)
            .map(str::to_owned);
        let visibility = ui.and_then(|u| u.get("visibility")).and_then(Value::as_array);
        if uri.is_none() && visibility.is_none() {
            return None;
        }
        let has = |w: &str| visibility.is_none_or(|v| v.iter().any(|x| x.as_str() == Some(w)));
        Some(Self { resource_uri: uri, model: has("model"), app: has("app") })
    }
}

impl McpTool {
    /// `None` for entries without a usable name.
    pub(crate) fn from_value(v: &Value) -> Option<Self> {
        let name = v.get("name").and_then(Value::as_str).filter(|n| !n.trim().is_empty() && n.len() <= MAX_NAME)?;
        let text = |o: &Value, k: &str| o.get(k).and_then(Value::as_str).map(|s| truncate(s, MAX_DESCRIPTION));
        let input_schema = match v.get("inputSchema") {
            Some(Value::Object(o)) => {
                let mut o = o.clone();
                if !o.contains_key("type") {
                    o.insert("type".into(), json!("object"));
                }
                Value::Object(o)
            }
            _ => json!({ "type": "object" }),
        };
        let a = v.get("annotations").filter(|a| a.is_object()).cloned().unwrap_or_else(|| json!({}));
        let flag = |k: &str| a.get(k).and_then(Value::as_bool);
        let raw = if v.to_string().len() <= 64 * 1024 { v.clone() } else { json!({"name": name}) };
        Some(Self {
            ui: ToolUi::from_meta(v.get("_meta")),
            raw,
            name: name.to_owned(),
            title: text(v, "title"),
            description: text(v, "description").unwrap_or_default(),
            input_schema,
            annotations: ToolAnnotations {
                title: text(&a, "title"),
                read_only_hint: flag("readOnlyHint"),
                destructive_hint: flag("destructiveHint"),
                idempotent_hint: flag("idempotentHint"),
                open_world_hint: flag("openWorldHint"),
            },
        })
    }
}

/// One piece of a tool result.
#[derive(Debug, Clone, PartialEq)]
pub enum Content {
    Text(String),
    /// Base64 `data`.
    Image {
        mime_type: String,
        data: String,
    },
    /// Base64 `data`.
    Audio {
        mime_type: String,
        data: String,
    },
    /// An embedded resource; `text` is set for text resources.
    Resource {
        uri: String,
        mime_type: Option<String>,
        text: Option<String>,
    },
    ResourceLink {
        uri: String,
        name: String,
    },
    /// A content type byob doesn't know.
    Other {
        kind: String,
    },
}

impl Content {
    fn from_value(v: &Value) -> Option<Self> {
        let s = |o: &Value, k: &str| o.get(k).and_then(Value::as_str).map(str::to_owned);
        let kind = s(v, "type")?;
        Some(match kind.as_str() {
            "text" => Self::Text(s(v, "text")?),
            "image" => Self::Image { mime_type: s(v, "mimeType").unwrap_or_default(), data: s(v, "data")? },
            "audio" => Self::Audio { mime_type: s(v, "mimeType").unwrap_or_default(), data: s(v, "data")? },
            "resource" => {
                let r = v.get("resource")?;
                Self::Resource { uri: s(r, "uri").unwrap_or_default(), mime_type: s(r, "mimeType"), text: s(r, "text") }
            }
            "resource_link" => Self::ResourceLink { uri: s(v, "uri").unwrap_or_default(), name: s(v, "name").unwrap_or_default() },
            _ => Self::Other { kind: truncate(&kind, 64) },
        })
    }

    /// Text for the model; non-text parts become short placeholders.
    pub fn to_text(&self) -> String {
        let kib = |b64: &str| (b64.len() / 4 * 3).div_ceil(1024);
        match self {
            Self::Text(t) => t.clone(),
            Self::Image { mime_type, data } => format!("[image {mime_type}, {} KiB]", kib(data)),
            Self::Audio { mime_type, data } => format!("[audio {mime_type}, {} KiB]", kib(data)),
            Self::Resource { text: Some(t), .. } => t.clone(),
            Self::Resource { uri, .. } => format!("[resource {uri}]"),
            Self::ResourceLink { uri, name } => format!("[resource link {name}: {uri}]"),
            Self::Other { kind } => format!("[unsupported content: {kind}]"),
        }
    }
}

/// The outcome of `tools/call`.
#[derive(Debug, Clone, PartialEq)]
pub struct CallResult {
    /// All parts as text for the model (placeholders for non-text parts), capped at
    /// [`MAX_RESULT_TEXT`]; `"(no result)"` if the server returned nothing.
    pub text: String,
    /// The tool reported a failure (`isError`). The text says what went wrong.
    pub is_error: bool,
    /// The parts as sent (images keep their base64 data, e.g. to save to a file).
    pub content: Vec<Content>,
    /// `structuredContent`, if any.
    pub structured: Option<Value>,
    /// The result as the server sent it (for MCP Apps' `ui/notifications/tool-result`).
    pub raw: Value,
}

impl CallResult {
    pub(crate) fn from_value(v: &Value) -> Self {
        let content: Vec<Content> =
            v.get("content").and_then(Value::as_array).map(|a| a.iter().take(1000).filter_map(Content::from_value).collect()).unwrap_or_default();
        let structured = v.get("structuredContent").filter(|s| !s.is_null()).cloned();
        let mut text = String::new();
        let mut truncated = false;
        for part in &content {
            let t = part.to_text();
            let sep = usize::from(!text.is_empty());
            if text.len().saturating_add(sep).saturating_add(t.len()) > MAX_RESULT_TEXT {
                if sep == 1 {
                    text.push('\n');
                }
                text.push_str(&truncate(&t, MAX_RESULT_TEXT.saturating_sub(text.len())));
                truncated = true;
                break;
            }
            if sep == 1 {
                text.push('\n');
            }
            text.push_str(&t);
        }
        if text.is_empty()
            && let Some(s) = &structured
        {
            let json = s.to_string();
            truncated = json.len() > MAX_RESULT_TEXT;
            text = truncate(&json, MAX_RESULT_TEXT);
        }
        if truncated {
            text.push_str("\n[output truncated]");
        }
        if text.is_empty() {
            text = "(no result)".into();
        }
        let is_error = v.get("isError").and_then(Value::as_bool).unwrap_or(false);
        Self { text, is_error, content, structured, raw: if v.is_object() { v.clone() } else { json!({}) } }
    }
}

/// `arguments` for `tools/call`: MCP wants an object.
pub(crate) fn arguments(args: Value) -> Value {
    match args {
        Value::Object(_) => args,
        _ => Value::Object(Map::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tools_defensively() {
        let t = McpTool::from_value(&json!({
            "name": "get_issue", "description": "Get one", "inputSchema": {"properties": {"id": {"type": "string"}}},
            "annotations": {"readOnlyHint": true, "destructiveHint": "yes"}
        }))
        .unwrap();
        assert_eq!(t.name, "get_issue");
        assert_eq!(t.input_schema["type"], "object");
        assert_eq!(t.annotations.read_only_hint, Some(true));
        assert_eq!(t.annotations.destructive_hint, None);
        assert!(McpTool::from_value(&json!({"name": ""})).is_none());
        assert!(McpTool::from_value(&json!({"name": 5})).is_none());
        assert_eq!(McpTool::from_value(&json!({"name": "x", "inputSchema": "bad"})).unwrap().input_schema, json!({"type":"object"}));
    }

    #[test]
    fn apps_ui_metadata() {
        let t = McpTool::from_value(&json!({"name": "show", "_meta": {"ui": {"resourceUri": "ui://w/dash"}}})).unwrap();
        assert_eq!(t.ui, Some(ToolUi { resource_uri: Some("ui://w/dash".into()), model: true, app: true }));
        let old = McpTool::from_value(&json!({"name": "show", "_meta": {"ui/resourceUri": "ui://w/old"}})).unwrap();
        assert_eq!(old.ui.unwrap().resource_uri.as_deref(), Some("ui://w/old"));
        let app_only = McpTool::from_value(&json!({"name": "refresh", "_meta": {"ui": {"visibility": ["app"]}}})).unwrap();
        assert_eq!(app_only.ui, Some(ToolUi { resource_uri: None, model: false, app: true }));
        assert_eq!(McpTool::from_value(&json!({"name": "x", "_meta": {"ui": {"resourceUri": "https://evil"}}})).unwrap().ui, None);
        assert_eq!(McpTool::from_value(&json!({"name": "x"})).unwrap().ui, None);
        assert_eq!(t.raw["_meta"]["ui"]["resourceUri"], "ui://w/dash");
    }

    #[test]
    fn result_text_joins_parts_with_placeholders() {
        let r = CallResult::from_value(&json!({
            "content": [
                {"type": "text", "text": "hello"},
                {"type": "image", "mimeType": "image/png", "data": "AAAA"},
                {"type": "resource", "resource": {"uri": "file:///a", "text": "inline"}},
                {"type": "resource", "resource": {"uri": "file:///b", "blob": "AA=="}},
                {"type": "resource_link", "uri": "file:///c", "name": "c"},
                {"type": "hologram"},
                {"nope": 1},
            ],
            "isError": true
        }));
        assert!(r.is_error);
        assert_eq!(r.text, "hello\n[image image/png, 1 KiB]\ninline\n[resource file:///b]\n[resource link c: file:///c]\n[unsupported content: hologram]");
        assert_eq!(r.content.len(), 6);
    }

    #[test]
    fn empty_and_structured_results() {
        assert_eq!(CallResult::from_value(&json!({})).text, "(no result)");
        assert_eq!(CallResult::from_value(&json!("garbage")).text, "(no result)");
        let r = CallResult::from_value(&json!({"content": [], "structuredContent": {"n": 1}}));
        assert_eq!(r.text, r#"{"n":1}"#);
    }

    #[test]
    fn result_text_is_capped() {
        let big = "x".repeat(MAX_RESULT_TEXT + 10);
        let r = CallResult::from_value(&json!({"content": [{"type":"text","text":"a"}, {"type":"text","text": big}]}));
        assert!(r.text.len() <= MAX_RESULT_TEXT + 30);
        assert!(r.text.ends_with("[output truncated]"));
    }
}
