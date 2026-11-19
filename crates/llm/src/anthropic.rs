//! Anthropic's Messages API (`POST {base}/messages`), for Claude: the request built from
//! byob's OpenAI-shaped transcript, and the streamed reply folded into a [`ChatOutcome`].
//!
//! - System messages at the start become the top-level `system`; tools become Anthropic
//!   tool definitions; assistant tool calls become `tool_use` blocks and tool results
//!   `tool_result` blocks in a user message; images become base64 (or URL) image blocks.
//! - Prompt caching is on: a breakpoint after the system prompt (tools + system, stable
//!   across a chat) and automatic caching of the whole request, so each tool step reads the
//!   previous one's prefix from the cache.
//! - Models with adaptive thinking get `display: "summarized"`, shown as byob's reasoning.
//!   The reply's blocks (thinking with signatures, text, `tool_use`) come back in
//!   [`ChatOutcome::native`]; the engine keeps them on the assistant message
//!   ([`ChatMessage::native`]) and they are sent back unchanged within the tool loop, as the
//!   API requires. `prefix_mismatch_behavior: "drop_block"` makes the API drop, rather than
//!   reject, thinking blocks whose earlier conversation byob changed (compaction, budget
//!   fitting, old images left out).
//! - No `temperature`/`top_p` unless the profile sets them (current models reject them);
//!   `max_tokens`, `output_config.effort` and extra fields from the profile
//!   ([`crate::params`]). No forced `tool_choice`, no assistant prefill. A `refusal` stop
//!   is an error, [`LlmError::Refused`].
//!
//! Everything the server sends is untrusted: sizes are capped and unknown events and block
//! types are ignored.

use std::collections::BTreeMap;
use std::io::{BufRead, Read};
use std::sync::atomic::{AtomicBool, Ordering};

use byob_types::profile::Params;
use serde_json::{Map, Value, json};

use crate::{ChatMessage, ChatOutcome, LlmError, MAX_LINE, MAX_TOOL_CALLS, StreamEvent, ToolCall, ToolSpec, Usage, sse};

/// The `anthropic-version` header.
pub const VERSION: &str = "2026-10-21";
/// `max_tokens` of every request whose profile sets none: room for thinking as well as the reply (thinking counts
/// toward it). Every current model allows at least this much output.
pub const MAX_TOKENS: u64 = 64_000;
/// Beta for `thinking.block_binding`.
const BETA_BINDING: &str = "thinking-binding-controls-2026-08-01";
/// Beta for `fallbacks: "default"` (server-side retry of a refused request on another model).
const BETA_FALLBACK: &str = "server-side-fallback-2026-07-01";
/// Most content blocks accepted in one reply.
const MAX_BLOCKS: usize = 512;
/// Largest single block (text, thinking, signature or tool input) accepted.
const MAX_BLOCK_BYTES: usize = 16 * 1024 * 1024;

/// Image types the API takes.
const IMAGE_TYPES: &[&str] = &["image/jpeg", "image/png", "image/gif", "image/webp"];

/// What a model supports, judged from its id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Traits {
    /// Adaptive thinking (`thinking: {type: "adaptive"}`): Claude 4.6 and later.
    adaptive: bool,
    /// Server-side `fallbacks: "default"`.
    fallbacks: bool,
}

fn traits(model: &str) -> Traits {
    let m = model.trim().to_ascii_lowercase();
    // Older Claude models only have budgeted thinking (or none): they get no `thinking`.
    const OLDER: &[&str] = &[
        "claude-2",
        "claude-instant",
        "claude-3",
        "claude-opus-4-0",
        "claude-opus-4-1",
        "claude-opus-4-2025",
        "claude-opus-4-5",
        "claude-sonnet-4-0",
        "claude-sonnet-4-2025",
        "claude-sonnet-4-5",
        "claude-haiku-4-5",
    ];
    // Models documented to take `fallbacks: "default"` (Haiku has no fallback).
    const FALLBACKS: &[&str] = &["claude-opus-5", "claude-sonnet-5-5", "claude-fable-5-1"];
    // Another model behind an Anthropic-style endpoint (llama-server, a gateway) gets a
    // plain request.
    let claude = m.starts_with("claude-");
    Traits { adaptive: claude && !OLDER.iter().any(|p| m.starts_with(p)), fallbacks: FALLBACKS.iter().any(|p| m.starts_with(p)) }
}

/// A request body and its `anthropic-beta` values.
pub struct Request {
    pub body: Vec<u8>,
    pub betas: Vec<&'static str>,
}

/// The request for `messages` and `tools`. `extras` adds the beta features (thinking-block
/// binding, server-side fallbacks); without them it is the plainest request the model takes.
/// The profile's `params` go on either ([`crate::params`]).
pub fn request(model: &str, messages: &[ChatMessage], tools: &[ToolSpec], extras: bool, params: &Params) -> Result<Request, LlmError> {
    let (mut body, betas) = request_body(model, messages, tools, extras);
    crate::params::apply_anthropic(&mut body, params);
    let body = serde_json::to_vec(&body).map_err(|e| LlmError::Stream(format!("encoding request: {e}")))?;
    Ok(Request { body, betas })
}

pub(crate) fn request_body(model: &str, messages: &[ChatMessage], tools: &[ToolSpec], extras: bool) -> (Value, Vec<&'static str>) {
    let traits = traits(model);
    let (mut system, turns) = convert(messages);
    let mut body = Map::new();
    let mut betas = Vec::new();
    body.insert("model".into(), json!(model));
    body.insert("max_tokens".into(), json!(MAX_TOKENS));
    body.insert("stream".into(), json!(true));
    // Automatic caching: the breakpoint sits on the last block, so the next request of the
    // tool loop (this one plus the reply and results) reads all of this from the cache.
    body.insert("cache_control".into(), json!({"type": "ephemeral"}));
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools.iter().map(tool).collect()));
    }
    // A second breakpoint after tools + system, the part that stays the same across turns.
    if let Some(Value::Object(last)) = system.last_mut() {
        last.insert("cache_control".into(), json!({"type": "ephemeral"}));
    }
    if !system.is_empty() {
        body.insert("system".into(), Value::Array(system));
    }
    body.insert("messages".into(), Value::Array(turns.into_iter().map(|(role, content)| json!({"role": role, "content": content})).collect()));
    if traits.adaptive {
        let mut thinking = json!({"type": "adaptive", "display": "summarized"});
        if extras {
            thinking["block_binding"] = json!({"prefix_mismatch_behavior": "drop_block"});
            betas.push(BETA_BINDING);
        }
        body.insert("thinking".into(), thinking);
    }
    if extras && traits.fallbacks {
        body.insert("fallbacks".into(), json!("default"));
        betas.push(BETA_FALLBACK);
    }
    (Value::Object(body), betas)
}

/// Did a 400 reject one of the beta extras (an endpoint without them)? Then the plain
/// request is tried once.
pub fn rejects_extras(err: &LlmError) -> bool {
    let LlmError::Http { status: 400, message } = err else { return false };
    let m = message.to_ascii_lowercase();
    ["fallback", "block_binding", "anthropic-beta", "beta"].iter().any(|w| m.contains(w))
}

/// An Anthropic tool definition. `input_schema` must be an object schema.
fn tool(t: &ToolSpec) -> Value {
    let mut schema = match &t.parameters {
        Value::Object(o) => o.clone(),
        _ => Map::new(),
    };
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        schema.insert("type".into(), json!("object"));
    }
    let mut out = json!({"name": t.name, "input_schema": schema});
    if !t.description.trim().is_empty() {
        out["description"] = json!(t.description);
    }
    out
}

type Turn = (&'static str, Vec<Value>);

/// The system blocks and the alternating user/assistant turns for `messages`.
fn convert(messages: &[ChatMessage]) -> (Vec<Value>, Vec<Turn>) {
    let mut system = Vec::new();
    let mut turns: Vec<Turn> = Vec::new();
    for m in messages {
        match m.role.as_str() {
            "system" if turns.is_empty() => system.extend(text_block(m.content.as_deref())),
            "assistant" => push(&mut turns, "assistant", assistant_blocks(m)),
            "tool" => push(&mut turns, "user", vec![tool_result(m)]),
            // A user message, or a system note later on (the API has no system role there).
            _ => push(&mut turns, "user", user_blocks(m)),
        }
    }
    (system, fix_turns(turns))
}

/// Append `blocks` as a `role` turn, joining it to the previous turn of the same role.
fn push(turns: &mut Vec<Turn>, role: &'static str, blocks: Vec<Value>) {
    match turns.last_mut() {
        Some((r, content)) if *r == role => content.extend(blocks),
        _ => turns.push((role, blocks)),
    }
}

/// A text block, if there is any text (the API rejects empty and blank ones).
fn text_block(text: Option<&str>) -> Option<Value> {
    text.filter(|t| !t.trim().is_empty()).map(|t| json!({"type": "text", "text": t}))
}

fn text(t: &str) -> Value {
    json!({"type": "text", "text": t})
}

fn user_blocks(m: &ChatMessage) -> Vec<Value> {
    let mut out: Vec<Value> = text_block(m.content.as_deref()).into_iter().collect();
    out.extend(m.images.iter().map(|u| image(u)));
    out
}

/// An image block from a `data:image/...;base64,` URL or an http(s) URL.
pub(crate) fn image(url: &str) -> Value {
    if let Some(rest) = url.strip_prefix("data:") {
        let parsed = rest.split_once(',').and_then(|(meta, data)| Some((meta.strip_suffix(";base64")?, data)));
        return match parsed {
            Some((mime, data)) if IMAGE_TYPES.contains(&mime.to_ascii_lowercase().as_str()) => {
                json!({"type": "image", "source": {"type": "base64", "media_type": mime.to_ascii_lowercase(), "data": data}})
            }
            _ => text("[an image in a format this model can't take was left out]"),
        };
    }
    if url.starts_with("https://") || url.starts_with("http://") {
        return json!({"type": "image", "source": {"type": "url", "url": url}});
    }
    text("[an image that isn't a data or http URL was left out]")
}

/// Tool-use ids must match `[a-zA-Z0-9_-]+`; ids from other servers may not.
fn tool_id(id: &str) -> String {
    let s: String = id.trim().chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
    if s.is_empty() { "toolu_missing".into() } else { s }
}

/// Tool arguments as the `input` object; `{}` when they aren't a JSON object.
fn tool_input(arguments: &str) -> Value {
    match serde_json::from_str::<Value>(arguments) {
        Ok(v @ Value::Object(_)) => v,
        _ => json!({}),
    }
}

fn block_type(b: &Value) -> &str {
    b.get("type").and_then(Value::as_str).unwrap_or_default()
}

fn is_thinking(b: &Value) -> bool {
    matches!(block_type(b), "thinking" | "redacted_thinking")
}

/// An assistant turn: the reply's own blocks when they still match its tool calls, else
/// rebuilt (thinking blocks kept first) from the text and calls.
fn assistant_blocks(m: &ChatMessage) -> Vec<Value> {
    let calls: Vec<&str> = m.tool_calls.iter().map(|c| c.id.as_str()).collect();
    let native_calls: Vec<&str> = m.native.iter().filter(|b| block_type(b) == "tool_use").filter_map(|b| b.get("id").and_then(Value::as_str)).collect();
    if !m.native.is_empty() && native_calls == calls {
        return m.native.clone();
    }
    let mut out: Vec<Value> = m.native.iter().filter(|b| is_thinking(b)).cloned().collect();
    out.extend(text_block(m.content.as_deref()));
    for c in &m.tool_calls {
        out.push(json!({"type": "tool_use", "id": tool_id(&c.id), "name": c.function.name, "input": tool_input(&c.function.arguments)}));
    }
    out
}

fn tool_result(m: &ChatMessage) -> Value {
    let id = tool_id(m.tool_call_id.as_deref().unwrap_or_default());
    let body = m.content.as_deref().filter(|t| !t.trim().is_empty()).unwrap_or("(no output)");
    let content = if m.images.is_empty() {
        json!(body)
    } else {
        let mut parts = vec![text(body)];
        parts.extend(m.images.iter().map(|u| image(u)));
        Value::Array(parts)
    };
    json!({"type": "tool_result", "tool_use_id": id, "content": content})
}

/// Make the turns valid: every `tool_use` answered by a `tool_result` at the start of the
/// next user turn (missing ones say so; results without their call become text), the
/// first turn a user one, the last one too (no prefill), and no turn empty.
fn fix_turns(turns: Vec<Turn>) -> Vec<Turn> {
    let mut out: Vec<Turn> = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    for (role, blocks) in turns {
        if role == "assistant" {
            if !pending.is_empty() {
                out.push(("user", answer(std::mem::take(&mut pending), Vec::new())));
            }
            pending = blocks.iter().filter(|b| block_type(b) == "tool_use").filter_map(|b| b.get("id").and_then(Value::as_str)).map(str::to_owned).collect();
            let blocks = if blocks.is_empty() { vec![text("(no reply)")] } else { blocks };
            out.push((role, blocks));
        } else {
            out.push((role, answer(std::mem::take(&mut pending), blocks)));
        }
    }
    if !pending.is_empty() {
        out.push(("user", answer(pending, Vec::new())));
    }
    if out.first().is_none_or(|(r, _)| *r != "user") {
        out.insert(0, ("user", vec![text("(the conversation so far continues below)")]));
    }
    if out.last().is_some_and(|(r, _)| *r == "assistant") {
        out.push(("user", vec![text("Continue.")]));
    }
    out
}

/// A user turn's blocks answering the calls `pending`: their results first, in the order
/// called, then everything else.
fn answer(pending: Vec<String>, blocks: Vec<Value>) -> Vec<Value> {
    let mut results: BTreeMap<String, Value> = BTreeMap::new();
    let mut rest = Vec::new();
    for b in blocks {
        let id = b.get("tool_use_id").and_then(Value::as_str).map(str::to_owned);
        match id {
            Some(id) if block_type(&b) == "tool_result" && pending.contains(&id) && !results.contains_key(&id) => {
                results.insert(id, b);
            }
            Some(_) if block_type(&b) == "tool_result" => rest.push(orphan(&b)),
            _ => rest.push(b),
        }
    }
    let mut out: Vec<Value> = pending
        .iter()
        .map(|id| results.remove(id).unwrap_or_else(|| json!({"type": "tool_result", "tool_use_id": id, "content": "(no result: the call didn't finish)"})))
        .collect();
    out.extend(rest);
    if out.is_empty() {
        out.push(text("(empty message)"));
    }
    out
}

/// A tool result whose call isn't in the conversation any more, as text.
fn orphan(b: &Value) -> Value {
    let content = match b.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    };
    text(&format!("[result of an earlier tool call]\n{content}"))
}

/// One content block of the reply as it streams in.
enum Block {
    Text(String),
    Thinking {
        thinking: String,
        signature: String,
    },
    /// Opaque; sent back as it came.
    Redacted(Value),
    ToolUse {
        id: String,
        name: String,
        json: String,
        input: Value,
    },
    /// A block we don't keep (unknown type, over a limit, or before a fallback).
    Skip,
}

impl Block {
    fn native(&self) -> Option<Value> {
        match self {
            Self::Text(t) => text_block(Some(t)),
            Self::Thinking { thinking, signature } => Some(json!({"type": "thinking", "thinking": thinking, "signature": signature})),
            Self::Redacted(v) => Some(v.clone()),
            Self::ToolUse { id, name, json, input } => {
                let input = if json.trim().is_empty() { input.clone() } else { tool_input(json) };
                Some(json!({"type": "tool_use", "id": id, "name": name, "input": input}))
            }
            Self::Skip => None,
        }
    }
}

/// Fold of the reply stream.
#[derive(Default)]
struct Reply {
    out: ChatOutcome,
    blocks: Vec<Block>,
    /// Server block index → position in `blocks`.
    index: BTreeMap<u64, usize>,
    started: bool,
    stopped: bool,
    stop_reason: Option<String>,
    refusal: Option<String>,
    input: u64,
    cache_write: u64,
    cache_read: u64,
    output: u64,
    usage_seen: bool,
}

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// Append `piece` to `buf`, within [`MAX_BLOCK_BYTES`].
fn append(buf: &mut String, piece: &str) -> Result<(), LlmError> {
    if buf.len().saturating_add(piece.len()) > MAX_BLOCK_BYTES {
        return Err(LlmError::Stream("a block of the reply is too large".into()));
    }
    buf.push_str(piece);
    Ok(())
}

impl Reply {
    fn emit(&mut self, ev: StreamEvent, on_event: &mut impl FnMut(StreamEvent)) {
        match &ev {
            StreamEvent::Content(t) => self.out.content.push_str(t),
            StreamEvent::Reasoning(t) => self.out.reasoning.push_str(t),
        }
        on_event(ev);
    }

    fn usage(&mut self, u: Option<&Value>) {
        let Some(u) = u.filter(|u| u.is_object()) else { return };
        self.usage_seen = true;
        let field = |k: &str| u.get(k).and_then(Value::as_u64);
        // Later events repeat or update the counts; take what they say.
        self.input = field("input_tokens").unwrap_or(self.input);
        self.cache_write = field("cache_creation_input_tokens").unwrap_or(self.cache_write);
        self.cache_read = field("cache_read_input_tokens").unwrap_or(self.cache_read);
        self.output = field("output_tokens").unwrap_or(self.output);
    }

    fn block(&mut self, v: &Value) -> Option<&mut Block> {
        let i = v.get("index").and_then(Value::as_u64)?;
        let pos = *self.index.get(&i)?;
        self.blocks.get_mut(pos)
    }

    /// One `data:` payload. `Ok(true)` once the message is complete.
    fn event(&mut self, data: &str, on_event: &mut impl FnMut(StreamEvent)) -> Result<bool, LlmError> {
        let v: Value = serde_json::from_str(data).map_err(|e| LlmError::Stream(format!("bad event JSON: {e}")))?;
        match str_of(&v, "type") {
            "message_start" => {
                self.started = true;
                self.usage(v.pointer("/message/usage"));
            }
            "content_block_start" => self.start_block(&v, on_event)?,
            "content_block_delta" => self.delta(&v, on_event)?,
            "message_delta" => {
                self.started = true;
                if let Some(d) = v.get("delta") {
                    if let Some(r) = d.get("stop_reason").and_then(Value::as_str) {
                        self.stop_reason = Some(r.chars().take(64).collect());
                    }
                    if let Some(c) = d.pointer("/stop_details/category").and_then(Value::as_str) {
                        self.refusal = Some(c.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').take(64).collect());
                    }
                }
                self.usage(v.get("usage"));
            }
            "message_stop" => {
                self.stopped = true;
                return Ok(true);
            }
            "error" => {
                let e = v.get("error").unwrap_or(&Value::Null);
                let message: String = e.get("message").and_then(Value::as_str).map_or_else(|| e.to_string(), str::to_owned).chars().take(500).collect();
                return Err(LlmError::Stream(format!("{} ({})", message, str_of(e, "type"))));
            }
            // `ping`, `content_block_stop` and anything newer.
            _ => {}
        }
        Ok(false)
    }

    fn start_block(&mut self, v: &Value, on_event: &mut impl FnMut(StreamEvent)) -> Result<(), LlmError> {
        let Some(i) = v.get("index").and_then(Value::as_u64) else { return Ok(()) };
        if self.blocks.len() >= MAX_BLOCKS {
            return Err(LlmError::Stream("the reply has too many blocks".into()));
        }
        let b = v.get("content_block").unwrap_or(&Value::Null);
        let calls = self.blocks.iter().filter(|b| matches!(b, Block::ToolUse { .. })).count();
        let block = match str_of(b, "type") {
            "text" => {
                let t = str_of(b, "text").to_owned();
                if !t.is_empty() {
                    self.emit(StreamEvent::Content(t.clone()), on_event);
                }
                Block::Text(t)
            }
            "thinking" => {
                // Separate the summaries of successive thinking blocks.
                if !self.out.reasoning.is_empty() && !self.out.reasoning.ends_with('\n') {
                    self.emit(StreamEvent::Reasoning("\n\n".into()), on_event);
                }
                let thinking = str_of(b, "thinking").to_owned();
                if !thinking.is_empty() {
                    self.emit(StreamEvent::Reasoning(thinking.clone()), on_event);
                }
                Block::Thinking { thinking, signature: str_of(b, "signature").to_owned() }
            }
            "redacted_thinking" if b.to_string().len() <= MAX_BLOCK_BYTES => Block::Redacted(b.clone()),
            "tool_use" if calls < MAX_TOOL_CALLS => Block::ToolUse {
                id: str_of(b, "id").chars().take(256).collect(),
                name: str_of(b, "name").chars().take(256).collect(),
                json: String::new(),
                input: b.get("input").filter(|i| i.is_object()).cloned().unwrap_or_else(|| json!({})),
            },
            // A refused request went on on another model: what the first one did before the
            // switch (thinking, tool calls) is not sent back, and its calls don't run.
            "fallback" => {
                log::info!("the request went on on another model ({:?} → {:?})", b.pointer("/from/model"), b.pointer("/to/model"));
                for earlier in &mut self.blocks {
                    if !matches!(earlier, Block::Text(_)) {
                        *earlier = Block::Skip;
                    }
                }
                Block::Skip
            }
            _ => Block::Skip,
        };
        self.index.insert(i, self.blocks.len());
        self.blocks.push(block);
        Ok(())
    }

    fn delta(&mut self, v: &Value, on_event: &mut impl FnMut(StreamEvent)) -> Result<(), LlmError> {
        let d = v.get("delta").unwrap_or(&Value::Null);
        let piece = |k: &str| d.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
        let mut event = None;
        match (str_of(d, "type"), self.block(v)) {
            ("text_delta", Some(Block::Text(t))) => {
                let p = piece("text");
                append(t, &p)?;
                event = Some(StreamEvent::Content(p));
            }
            ("thinking_delta", Some(Block::Thinking { thinking, .. })) => {
                let p = piece("thinking");
                append(thinking, &p)?;
                event = Some(StreamEvent::Reasoning(p));
            }
            ("signature_delta", Some(Block::Thinking { signature, .. })) => append(signature, &piece("signature"))?,
            ("input_json_delta", Some(Block::ToolUse { json, .. })) => append(json, &piece("partial_json"))?,
            _ => {}
        }
        if let Some(ev) = event.filter(|e| !matches!(e, StreamEvent::Content(t) | StreamEvent::Reasoning(t) if t.is_empty())) {
            self.emit(ev, on_event);
        }
        Ok(())
    }

    fn finish(mut self) -> Result<ChatOutcome, LlmError> {
        if self.out.cancelled {
            return Ok(self.out);
        }
        if !self.started {
            return Err(LlmError::Stream("the server closed the connection without replying".into()));
        }
        if !self.stopped {
            return Err(LlmError::Stream("the connection closed before the reply was complete".into()));
        }
        if self.stop_reason.as_deref() == Some("refusal") {
            let why = self.refusal.as_deref().filter(|c| !c.is_empty()).map(|c| format!(" (category: {c})")).unwrap_or_default();
            return Err(LlmError::Refused(format!("the model's safeguards stopped this request{why}. Rephrase it, or try another model.")));
        }
        self.out.native = self.blocks.iter().filter_map(Block::native).collect();
        self.out.tool_calls = self
            .out
            .native
            .iter()
            .filter(|b| block_type(b) == "tool_use")
            .map(|b| ToolCall {
                id: str_of(b, "id").to_owned(),
                name: str_of(b, "name").to_owned(),
                arguments: b.get("input").map(Value::to_string).filter(|a| a != "{}").unwrap_or_default(),
            })
            .collect();
        self.out.finish_reason = self.stop_reason.as_deref().map(|r| {
            match r {
                "end_turn" | "stop_sequence" | "pause_turn" => "stop",
                "tool_use" => "tool_calls",
                "max_tokens" | "model_context_window_exceeded" => "length",
                other => other,
            }
            .to_owned()
        });
        if self.usage_seen {
            let prompt = self.input.saturating_add(self.cache_write).saturating_add(self.cache_read);
            self.out.usage = Some(Usage { prompt_tokens: prompt, completion_tokens: self.output });
            self.out.cache_read_tokens = self.cache_read;
            self.out.cache_write_tokens = self.cache_write;
            log::debug!("anthropic usage: {} input ({} from cache, {} written to it), {} output", prompt, self.cache_read, self.cache_write, self.output);
        }
        Ok(self.out)
    }
}

/// Read a streamed Messages reply.
pub fn read_stream(mut reader: impl BufRead, cancel: &AtomicBool, on_event: &mut impl FnMut(StreamEvent)) -> Result<ChatOutcome, LlmError> {
    let mut reply = Reply::default();
    let mut line = Vec::new();
    loop {
        if cancel.load(Ordering::Relaxed) {
            reply.out.cancelled = true;
            break;
        }
        line.clear();
        let n = (&mut reader).take(MAX_LINE as u64 + 1).read_until(b'\n', &mut line).map_err(|e| LlmError::Stream(e.to_string()))?;
        if n == 0 {
            break;
        }
        if line.len() > MAX_LINE {
            return Err(LlmError::Stream("SSE line too long".into()));
        }
        let text = String::from_utf8_lossy(&line);
        match sse::parse_line(&text) {
            sse::Line::Data(data) => {
                if reply.event(data, on_event)? {
                    break;
                }
            }
            sse::Line::Done => break,
            sse::Line::Skip => {}
        }
    }
    reply.finish()
}

/// `max_input_tokens` and image support of one `GET /v1/models` entry.
pub fn model_limits(entry: &Value) -> (Option<u64>, Option<bool>) {
    let context = entry.get("max_input_tokens").and_then(Value::as_u64).filter(|n| *n > 0);
    let vision = entry.pointer("/capabilities/image_input/supported").and_then(Value::as_bool);
    (context, vision)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traits_follow_the_model_id() {
        let t = |m| traits(m);
        assert_eq!(t("claude-opus-5-5"), Traits { adaptive: true, fallbacks: true });
        assert_eq!(t("claude-opus-5"), Traits { adaptive: true, fallbacks: true });
        assert_eq!(t("claude-sonnet-5-5"), Traits { adaptive: true, fallbacks: true });
        assert_eq!(t("claude-haiku-5-5"), Traits { adaptive: true, fallbacks: false });
        assert_eq!(t("claude-opus-4-8"), Traits { adaptive: true, fallbacks: false });
        assert_eq!(t("claude-haiku-4-5-20251001"), Traits { adaptive: false, fallbacks: false });
        assert_eq!(t("claude-sonnet-4-5"), Traits { adaptive: false, fallbacks: false });
        assert_eq!(t("local"), Traits { adaptive: false, fallbacks: false });
    }

    fn assistant(text: Option<&str>, calls: &[(&str, &str)]) -> ChatMessage {
        let calls: Vec<ToolCall> = calls.iter().map(|(id, args)| ToolCall { id: (*id).into(), name: "read".into(), arguments: (*args).into() }).collect();
        ChatMessage::assistant_tool_calls(text.map(Into::into), &calls)
    }

    #[test]
    fn converts_the_transcript() {
        let msgs = vec![
            ChatMessage::system("be brief"),
            ChatMessage::user("hi"),
            assistant(Some("looking"), &[("call:1", r#"{"path":"a"}"#), ("c2", "not json")]),
            ChatMessage::tool_result("c2", "two"),
            ChatMessage::tool_result("call:1", ""),
            ChatMessage::user_with_images("[images]", vec!["data:image/png;base64,AAAA".into(), "data:text/html;base64,PGI+".into()]),
            ChatMessage::system("a later note"),
        ];
        let (body, betas) = request_body("claude-opus-5-5", &msgs, &[], true);
        assert_eq!(body["system"], json!([{"type": "text", "text": "be brief", "cache_control": {"type": "ephemeral"}}]));
        assert_eq!(body["cache_control"], json!({"type": "ephemeral"}));
        assert_eq!(body["messages"][0], json!({"role": "user", "content": [{"type": "text", "text": "hi"}]}));
        assert_eq!(
            body["messages"][1]["content"],
            json!([
                {"type": "text", "text": "looking"},
                {"type": "tool_use", "id": "call_1", "name": "read", "input": {"path": "a"}},
                {"type": "tool_use", "id": "c2", "name": "read", "input": {}},
            ])
        );
        let user = &body["messages"][2];
        assert_eq!(user["role"], "user");
        // Results first, in call order; then the images and the later system note.
        assert_eq!(user["content"][0], json!({"type": "tool_result", "tool_use_id": "call_1", "content": "(no output)"}));
        assert_eq!(user["content"][1], json!({"type": "tool_result", "tool_use_id": "c2", "content": "two"}));
        assert_eq!(user["content"][3], json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}));
        assert_eq!(user["content"][4]["type"], "text", "an unsupported image becomes a note");
        assert_eq!(user["content"][5], json!({"type": "text", "text": "a later note"}));
        assert_eq!(body["messages"].as_array().map(Vec::len), Some(3));
        assert_eq!(body["thinking"], json!({"type": "adaptive", "display": "summarized", "block_binding": {"prefix_mismatch_behavior": "drop_block"}}));
        assert_eq!(body["fallbacks"], "default");
        assert_eq!(betas, vec![BETA_BINDING, BETA_FALLBACK]);
        for banned in ["temperature", "top_p", "top_k", "tool_choice"] {
            assert!(body.get(banned).is_none(), "{banned}");
        }
        let (plain, betas) = request_body("claude-opus-5-5", &msgs, &[], false);
        assert!(betas.is_empty() && plain.get("fallbacks").is_none());
        assert_eq!(plain["thinking"], json!({"type": "adaptive", "display": "summarized"}));
        let (old, betas) = request_body("claude-haiku-4-5", &msgs, &[], true);
        assert!(betas.is_empty() && old.get("thinking").is_none() && old.get("fallbacks").is_none());
    }

    #[test]
    fn repairs_turns_the_api_would_reject() {
        // A history cut in the middle: it starts with a call whose result is gone, has a
        // result whose call is gone, and ends on the assistant.
        let msgs = vec![assistant(None, &[("a", "{}")]), ChatMessage::user("next"), ChatMessage::tool_result("gone", "old result"), ChatMessage::assistant("")];
        let (body, _) = request_body("claude-opus-5-5", &msgs, &[], true);
        let turns = body["messages"].as_array().cloned().unwrap_or_default();
        let roles: Vec<&str> = turns.iter().map(|t| t["role"].as_str().unwrap_or_default()).collect();
        assert_eq!(roles, ["user", "assistant", "user", "assistant", "user"]);
        assert_eq!(turns[2]["content"][0]["type"], "tool_result");
        assert_eq!(turns[2]["content"][0]["tool_use_id"], "a");
        assert_eq!(turns[2]["content"][1], json!({"type": "text", "text": "next"}));
        assert_eq!(turns[2]["content"][2]["text"], "[result of an earlier tool call]\nold result");
        assert_eq!(turns[3]["content"], json!([{"type": "text", "text": "(no reply)"}]));
    }

    #[test]
    fn replays_native_blocks_only_while_they_match_the_calls() {
        let native = vec![
            json!({"type": "thinking", "thinking": "hmm", "signature": "sig"}),
            json!({"type": "tool_use", "id": "t1", "name": "read", "input": {"b": 1, "a": 2}}),
        ];
        let m = assistant(None, &[("t1", r#"{"b":1,"a":2}"#)]).with_native(native.clone());
        assert_eq!(assistant_blocks(&m), native);
        // The engine changed the calls: rebuilt, with the thinking kept in front.
        let changed = assistant(Some("x"), &[("other", "{}")]).with_native(native);
        let blocks = assistant_blocks(&changed);
        assert_eq!(blocks[0], json!({"type": "thinking", "thinking": "hmm", "signature": "sig"}));
        assert_eq!(blocks[1], json!({"type": "text", "text": "x"}));
        assert_eq!(blocks[2]["id"], "other");
    }

    #[test]
    fn tool_schemas_are_objects() {
        let spec = |parameters| ToolSpec { name: "t".into(), description: "d".into(), parameters };
        assert_eq!(
            tool(&spec(json!({"type": "object", "properties": {"x": {}}}))),
            json!({"name": "t", "description": "d", "input_schema": {"type": "object", "properties": {"x": {}}}})
        );
        assert_eq!(tool(&spec(json!(null)))["input_schema"], json!({"type": "object"}));
    }

    fn read(events: &[Value]) -> (Result<ChatOutcome, LlmError>, Vec<StreamEvent>) {
        let body: String = events.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap_or_default())).collect();
        let mut seen = Vec::new();
        let out = read_stream(std::io::Cursor::new(body), &AtomicBool::new(false), &mut |e| seen.push(e));
        (out, seen)
    }

    #[test]
    fn hostile_streams_fail_cleanly() {
        // Nothing, a cut-off reply, junk, and an error event are errors, not panics.
        assert!(matches!(read(&[]).0, Err(LlmError::Stream(_))));
        let start = json!({"type": "message_start", "message": {"usage": {"input_tokens": 1}}});
        assert!(matches!(read(std::slice::from_ref(&start)).0, Err(LlmError::Stream(_))));
        let junk = vec![
            start.clone(),
            json!({"type": "content_block_delta", "index": 99, "delta": {"type": "text_delta", "text": "x"}}),
            json!({"type": "content_block_start", "content_block": {"type": "text"}}),
            json!({"type": "content_block_start", "index": 0, "content_block": "nope"}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": 5}}),
            json!({"type": "message_delta", "delta": {"stop_reason": 7}, "usage": "x"}),
            json!({"type": "message_stop"}),
        ];
        let (out, _) = read(&junk);
        let out = out.unwrap();
        assert!(out.content.is_empty() && out.tool_calls.is_empty() && out.native.is_empty());
        let err = json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}});
        let (out, _) = read(&[start, err]);
        assert!(matches!(out, Err(LlmError::Stream(m)) if m.contains("Overloaded")));
    }

    #[test]
    fn a_fallback_drops_what_came_before_it() {
        let events = vec![
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 3}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "s"}}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "t1", "name": "run", "input": {}}}),
            json!({"type": "content_block_start", "index": 2, "content_block": {"type": "fallback", "from": {"model": "a"}, "to": {"model": "b"}}}),
            json!({"type": "content_block_start", "index": 3, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 3, "delta": {"type": "text_delta", "text": "hello"}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 4}}),
            json!({"type": "message_stop"}),
        ];
        let out = read(&events).0.unwrap();
        assert!(out.tool_calls.is_empty());
        assert_eq!(out.native, vec![json!({"type": "text", "text": "hello"})]);
        assert_eq!(out.finish_reason.as_deref(), Some("stop"));
    }
}
