//! OpenAI chat-completions wire types (only what we use), parsed defensively.

use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;

/// A message in the request transcript.
///
/// `images` (URLs or `data:` URLs) turn `content` into OpenAI content parts on the wire:
/// `[{"type":"text",...}, {"type":"image_url",...}]`. Without images `content` is a plain
/// string, which every server accepts.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<WireToolCall>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    /// Image inputs, sent as `image_url` parts after the text. Not deserialized.
    #[serde(skip)]
    pub images: Vec<String>,
    /// An assistant reply's content blocks exactly as the Anthropic Messages API sent them
    /// (thinking with its signature, text, `tool_use`; see [`crate::ChatOutcome::native`]),
    /// sent back as they are so the model keeps its reasoning within a tool loop. Ignored
    /// by the OpenAI wire format; never serialized or stored.
    #[serde(skip)]
    pub native: Vec<Value>,
    /// An assistant reply's reasoning, sent back within a tool loop so the model keeps its
    /// train of thought between tool calls (Qwen, GLM, Kimi, MiniMax, gpt-oss and DeepSeek
    /// expect it; their chat templates keep it after the last user message). It goes in
    /// the field the server sent it in ([`ChatMessage::reasoning_field`]). Never stored.
    #[serde(skip)]
    pub reasoning: String,
    /// Where `reasoning` goes: `reasoning_content` or `reasoning` (as the server streamed
    /// it), or `None` for reasoning the model wrote in `<think>` tags, which goes back the
    /// same way, in front of the content.
    #[serde(skip)]
    pub reasoning_field: Option<&'static str>,
}

/// The delta fields servers stream reasoning in: llama-server, vLLM and DeepSeek use
/// `reasoning_content`; OpenRouter and newer vLLM use `reasoning`.
pub const REASONING_FIELDS: [&str; 2] = ["reasoning_content", "reasoning"];

impl Serialize for ChatMessage {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum Part<'a> {
            Text { text: &'a str },
            ImageUrl { image_url: Url<'a> },
        }
        #[derive(Serialize)]
        struct Url<'a> {
            url: &'a str,
        }
        let mut map = s.serialize_map(None)?;
        map.serialize_entry("role", &self.role)?;
        let reasoning = Some(self.reasoning.trim()).filter(|r| !r.is_empty() && self.role == "assistant");
        // Reasoning from `<think>` tags goes back as the model wrote it.
        let tagged = match (reasoning, self.reasoning_field) {
            (Some(r), None) => Some(format!("<think>\n{r}\n</think>\n\n{}", self.content.as_deref().unwrap_or(""))),
            _ => None,
        };
        let content = tagged.as_ref().or(self.content.as_ref());
        if self.images.is_empty() {
            if let Some(c) = content {
                map.serialize_entry("content", c)?;
            }
        } else {
            let text = content.map(String::as_str).filter(|t| !t.is_empty()).map(|text| Part::Text { text });
            let images = self.images.iter().map(|url| Part::ImageUrl { image_url: Url { url } });
            let parts: Vec<Part<'_>> = text.into_iter().chain(images).collect();
            map.serialize_entry("content", &parts)?;
        }
        if !self.tool_calls.is_empty() {
            map.serialize_entry("tool_calls", &self.tool_calls)?;
        }
        if let Some(id) = &self.tool_call_id {
            map.serialize_entry("tool_call_id", id)?;
        }
        if let (Some(r), Some(field)) = (reasoning, self.reasoning_field) {
            map.serialize_entry(field, r)?;
        }
        map.end()
    }
}

impl ChatMessage {
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
        Self {
            role: role.into(),
            content: Some(text.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
            images: Vec::new(),
            native: Vec::new(),
            reasoning: String::new(),
            reasoning_field: None,
        }
    }
    /// A user message with images (URLs or `data:image/...;base64,` URLs) after the text.
    pub fn user_with_images(text: impl Into<String>, images: Vec<String>) -> Self {
        Self { images, ..Self::user(text) }
    }
    /// This message without its images; each dropped image leaves `note` in the text.
    pub fn without_images(&self, note: &str) -> Self {
        let mut m = Self { images: Vec::new(), ..self.clone() };
        if !self.images.is_empty() {
            let mut text = m.content.take().unwrap_or_default();
            for _ in &self.images {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(note);
            }
            m.content = Some(text);
        }
        m
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
        Self { tool_calls, content: text, ..Self::text("assistant", "") }
    }
    /// The result of a tool call.
    pub fn tool_result(call_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: Some(text.into()),
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
            images: Vec::new(),
            native: Vec::new(),
            reasoning: String::new(),
            reasoning_field: None,
        }
    }
    /// This message with the provider-native blocks of the reply it came from (see
    /// [`ChatMessage::native`]).
    pub fn with_native(self, native: Vec<Value>) -> Self {
        Self { native, ..self }
    }
    /// This assistant message with the reasoning of the reply it came from, to send back
    /// within the tool loop (see [`ChatMessage::reasoning`]).
    pub fn with_reasoning(self, reasoning: String, field: Option<&'static str>) -> Self {
        Self { reasoning, reasoning_field: field, ..self }
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
    /// llama.cpp: run in this server slot (keeps that slot's prompt cache). Only for servers
    /// that reported `total_slots`; others may reject unknown fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    id_slot: Option<u64>,
}

impl<'a> ChatRequest<'a> {
    pub fn new(model: &'a str, messages: &'a [ChatMessage], tools: &[ToolSpec]) -> Self {
        let tools = tools
            .iter()
            .map(|t| serde_json::json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.parameters}}))
            .collect();
        Self { model, messages, tools, stream: true, stream_options: serde_json::json!({"include_usage": true}), id_slot: None }
    }

    /// Ask a llama.cpp server for slot `slot` (`None`: let the server pick).
    pub fn in_slot(self, slot: Option<u64>) -> Self {
        Self { id_slot: slot, ..self }
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
    /// The delta field `reasoning` came in (one of [`REASONING_FIELDS`]).
    pub reasoning_field: Option<&'static str>,
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
        if let Some((field, text)) = REASONING_FIELDS.iter().find_map(|f| str_at(delta, f).map(|t| (*f, t))) {
            chunk.reasoning = Some(text);
            chunk.reasoning_field = Some(field);
        }
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
