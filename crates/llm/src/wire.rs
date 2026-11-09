//! OpenAI chat-completions wire types (only what we use), parsed defensively.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A message in the request transcript.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<WireToolCall>,
    pub tool_call_id: Option<String>,
    /// Images (`data:` or `http(s)` URLs) sent with the text as OpenAI `image_url` content
    /// parts, for vision models. Empty for plain text messages.
    #[serde(skip)]
    pub images: Vec<String>,
}

impl Serialize for ChatMessage {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut m = s.serialize_map(None)?;
        m.serialize_entry("role", &self.role)?;
        if self.images.is_empty() {
            if let Some(c) = &self.content {
                m.serialize_entry("content", c)?;
            }
        } else {
            let mut parts = Vec::with_capacity(self.images.len().saturating_add(1));
            if let Some(c) = self.content.as_ref().filter(|c| !c.is_empty()) {
                parts.push(serde_json::json!({"type": "text", "text": c}));
            }
            parts.extend(self.images.iter().map(|url| serde_json::json!({"type": "image_url", "image_url": {"url": url}})));
            m.serialize_entry("content", &parts)?;
        }
        if !self.tool_calls.is_empty() {
            m.serialize_entry("tool_calls", &self.tool_calls)?;
        }
        if let Some(id) = &self.tool_call_id {
            m.serialize_entry("tool_call_id", id)?;
        }
        m.end()
    }
}

impl ChatMessage {
    /// A user message with images (see [`ChatMessage::images`]).
    pub fn user_with_images(text: impl Into<String>, images: Vec<String>) -> Self {
        Self { images, ..Self::user(text) }
    }
    pub fn system(text: impl Into<String>) -> Self {
        Self::text("system", text)
    }
    pub fn user(text: impl Into<String>) -> Self {
        Self::text("user", text)
    }
    pub fn assistant(text: impl Into<String>) -> Self {
        Self::text("assistant", text)
    }
    fn text(role: &str, text: impl Into<String>) -> Self {
        Self { role: role.into(), content: Some(text.into()), tool_calls: Vec::new(), tool_call_id: None, images: Vec::new() }
    }
    /// An assistant turn that requested tool calls.
    pub fn assistant_tool_calls(text: Option<String>, calls: &[ToolCall]) -> Self {
        let tool_calls = calls
            .iter()
            .map(|c| WireToolCall {
                id: c.id.clone(),
                kind: "function".into(),
                function: WireFunction { name: c.name.clone(), arguments: c.arguments.clone() },
            })
            .collect();
        Self { role: "assistant".into(), content: text, tool_calls, tool_call_id: None, images: Vec::new() }
    }
    /// The result of a tool call.
    pub fn tool_result(call_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self { role: "tool".into(), content: Some(text.into()), tool_calls: Vec::new(), tool_call_id: Some(call_id.into()), images: Vec::new() }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: WireFunction,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireFunction {
    pub name: String,
    pub arguments: String,
}

/// A tool the model may call. `parameters` is a JSON Schema object.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// A tool call assembled from stream deltas. `arguments` is raw (possibly invalid) JSON.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
}

#[derive(Serialize)]
pub struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<Value>,
    stream: bool,
    stream_options: Value,
}

impl<'a> ChatRequest<'a> {
    pub fn new(model: &'a str, messages: &'a [ChatMessage], tools: &[ToolSpec]) -> Self {
        let tools = tools
            .iter()
            .map(|t| serde_json::json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.parameters}}))
            .collect();
        Self { model, messages, tools, stream: true, stream_options: serde_json::json!({"include_usage": true}) }
    }
}

/// One tool-call fragment from a stream chunk.
#[derive(Debug, Default)]
pub struct ToolCallDelta {
    pub index: usize,
    pub id: Option<String>,
    pub name: Option<String>,
    pub arguments: Option<String>,
}

/// The parts of one `chat.completion.chunk` we care about.
#[derive(Debug, Default)]
pub struct Chunk {
    pub content: Option<String>,
    pub reasoning: Option<String>,
    pub tool_calls: Vec<ToolCallDelta>,
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
}

fn str_at(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned)
}

impl Chunk {
    pub fn parse(json: &str) -> Result<Self, String> {
        let v: Value = serde_json::from_str(json).map_err(|e| format!("bad chunk JSON: {e}"))?;
        if let Some(err) = v.get("error") {
            return Err(error_from_value(err));
        }
        let mut chunk = Chunk { usage: v.get("usage").and_then(|u| serde_json::from_value(u.clone()).ok()), ..Default::default() };
        let Some(choice) = v.get("choices").and_then(|c| c.get(0)) else { return Ok(chunk) };
        chunk.finish_reason = str_at(choice, "finish_reason");
        let Some(delta) = choice.get("delta") else { return Ok(chunk) };
        chunk.content = str_at(delta, "content");
        // llama-server/DeepSeek use `reasoning_content`; some servers use `reasoning`.
        chunk.reasoning = str_at(delta, "reasoning_content").or_else(|| str_at(delta, "reasoning"));
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for (i, call) in calls.iter().enumerate() {
                let f = call.get("function");
                chunk.tool_calls.push(ToolCallDelta {
                    index: call.get("index").and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()).unwrap_or(i),
                    id: str_at(call, "id"),
                    name: f.and_then(|f| str_at(f, "name")),
                    arguments: f.and_then(|f| f.get("arguments")).and_then(Value::as_str).map(str::to_owned),
                });
            }
        }
        Ok(chunk)
    }
}

/// Fold a delta into the assembled calls. Indices beyond `max` are dropped.
pub fn merge_tool_call(calls: &mut Vec<ToolCall>, delta: ToolCallDelta, max: usize) {
    if delta.index >= max {
        return;
    }
    if calls.len() <= delta.index {
        calls.resize_with(delta.index + 1, ToolCall::default);
    }
    let Some(call) = calls.get_mut(delta.index) else { return };
    if let Some(id) = delta.id {
        call.id = id;
    }
    if let Some(name) = delta.name {
        call.name.push_str(&name);
    }
    if let Some(args) = delta.arguments {
        call.arguments.push_str(&args);
    }
}

fn error_from_value(err: &Value) -> String {
    err.get("message").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| err.to_string())
}

/// Best-effort human message from an error body.
pub fn error_message(body: &str) -> String {
    match serde_json::from_str::<Value>(body) {
        Ok(v) => v.get("error").map(error_from_value).unwrap_or_else(|| v.to_string()),
        Err(_) => body.chars().take(500).collect(),
    }
}

pub fn model_ids(body: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<Value>(body) else { return Vec::new() };
    v.get("data").and_then(Value::as_array).map(|a| a.iter().filter_map(|m| str_at(m, "id")).collect()).unwrap_or_default()
}
