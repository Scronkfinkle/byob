//! Client for any OpenAI-compatible `/chat/completions` endpoint
//! (llama-server, vLLM, Ollama, LM Studio, hosted APIs, ...).
//!
//! Blocking by design: a streaming call runs on the caller's thread and reports deltas
//! through a callback. Everything the server sends is treated as untrusted input.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

pub mod budget;
mod probe;
mod sse;
mod think;
pub mod toolprompt;
mod wire;

use std::io::{BufRead, BufReader, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub use budget::{estimate_history_tokens, estimate_message_tokens, estimate_tokens, fit_history};
use byob_types::InferenceConfig;
pub use probe::{Capabilities, ProbeOptions, ServerKind, ToolSupport};
pub use think::ThinkSplitter;
pub use toolprompt::{ParsedToolCalls, ToolCallFilter, parse_tool_calls, tool_prompt};
pub use wire::{ChatMessage, ChatRequest, ToolCall, ToolSpec, Usage};

/// Longest single SSE line we accept.
const MAX_LINE: usize = 4 * 1024 * 1024;
/// Most bytes of an error body we read for the message.
const MAX_ERROR_BODY: u64 = 64 * 1024;
/// Most tool calls accepted in one response.
const MAX_TOOL_CALLS: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("invalid inference URL: {0}")]
    BadUrl(#[from] url::ParseError),
    #[error("could not reach {url}: {source}")]
    Transport { url: String, source: Box<ureq::Error> },
    #[error("server returned HTTP {status}: {message}")]
    Http { status: u16, message: String },
    #[error("stream error: {0}")]
    Stream(String),
}

/// One incremental piece of a streamed reply.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// Visible answer text.
    Content(String),
    /// Model "thinking" (`reasoning_content`, or text inside `<think>` tags).
    Reasoning(String),
}

/// The fully assembled result of a streamed call.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChatOutcome {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
    /// True if the caller cancelled before the server finished.
    pub cancelled: bool,
}

/// Handle to a remote inference endpoint. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Client {
    config: InferenceConfig,
    base: url::Url,
    endpoint: url::Url,
    agent: ureq::Agent,
}

impl Client {
    pub fn new(config: InferenceConfig) -> Result<Self, LlmError> {
        let mut base = config.base_url.trim().to_owned();
        if !base.ends_with('/') {
            base.push('/');
        }
        let base = url::Url::parse(&base)?;
        if !matches!(base.scheme(), "http" | "https") {
            return Err(LlmError::BadUrl(url::ParseError::RelativeUrlWithoutBase));
        }
        let endpoint = base.join("chat/completions")?;
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(10)))
            // Long prompts can take a while before the first byte (prompt processing).
            .timeout_recv_response(Some(Duration::from_secs(600)))
            .build()
            .new_agent();
        Ok(Self { config, base, endpoint, agent })
    }

    pub fn endpoint(&self) -> &url::Url {
        &self.endpoint
    }

    pub fn config(&self) -> &InferenceConfig {
        &self.config
    }

    /// `GET {base}/models`: model ids the server advertises.
    pub fn list_models(&self) -> Result<Vec<String>, LlmError> {
        let url = self.base.join("models")?;
        let mut req = self.agent.get(url.as_str());
        if let Some(key) = &self.config.api_key {
            req = req.header("Authorization", &format!("Bearer {key}"));
        }
        let resp = req.call().map_err(|e| LlmError::Transport { url: url.to_string(), source: Box::new(e) })?;
        let status = resp.status().as_u16();
        let body = resp.into_body().with_config().limit(1024 * 1024).read_to_string().map_err(|e| LlmError::Stream(e.to_string()))?;
        if status >= 400 {
            return Err(LlmError::Http { status, message: wire::error_message(&body) });
        }
        Ok(wire::model_ids(&body))
    }

    /// Stream a chat completion. `on_event` sees each delta as it arrives; the assembled
    /// outcome is returned at the end. Checks `cancel` between chunks.
    pub fn stream_chat(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        cancel: &AtomicBool,
        mut on_event: impl FnMut(StreamEvent),
    ) -> Result<ChatOutcome, LlmError> {
        let request = ChatRequest::new(&self.config.model, messages, tools);
        let body = serde_json::to_vec(&request).map_err(|e| LlmError::Stream(format!("encoding request: {e}")))?;
        let mut req = self.agent.post(self.endpoint.as_str()).header("Accept", "text/event-stream").header("Content-Type", "application/json");
        if let Some(key) = &self.config.api_key {
            req = req.header("Authorization", &format!("Bearer {key}"));
        }
        let resp = req.send(&body[..]).map_err(|e| LlmError::Transport { url: self.endpoint.to_string(), source: Box::new(e) })?;
        let status = resp.status().as_u16();
        if status >= 400 {
            let mut body = String::new();
            let _ = resp.into_body().into_reader().take(MAX_ERROR_BODY).read_to_string(&mut body);
            return Err(LlmError::Http { status, message: wire::error_message(&body) });
        }
        let reader = BufReader::new(resp.into_body().into_reader());
        read_stream(reader, cancel, &mut on_event)
    }
}

fn read_stream(mut reader: impl BufRead, cancel: &AtomicBool, on_event: &mut impl FnMut(StreamEvent)) -> Result<ChatOutcome, LlmError> {
    let mut out = ChatOutcome::default();
    let mut think = ThinkSplitter::default();
    let mut emit = |out: &mut ChatOutcome, ev: StreamEvent| {
        match &ev {
            StreamEvent::Content(t) => out.content.push_str(t),
            StreamEvent::Reasoning(t) => out.reasoning.push_str(t),
        }
        on_event(ev);
    };
    let mut line = Vec::new();
    // Whether the server sent anything at all (a data chunk or `[DONE]`).
    let mut answered = false;
    loop {
        if cancel.load(Ordering::Relaxed) {
            out.cancelled = true;
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
            sse::Line::Done => {
                answered = true;
                break;
            }
            sse::Line::Skip => {}
            sse::Line::Data(json) => {
                answered = true;
                let chunk = wire::Chunk::parse(json).map_err(LlmError::Stream)?;
                if let Some(reasoning) = chunk.reasoning {
                    emit(&mut out, StreamEvent::Reasoning(reasoning));
                }
                if let Some(content) = chunk.content {
                    for ev in think.push(&content) {
                        emit(&mut out, ev);
                    }
                }
                for delta in chunk.tool_calls {
                    wire::merge_tool_call(&mut out.tool_calls, delta, MAX_TOOL_CALLS);
                }
                if chunk.finish_reason.is_some() {
                    out.finish_reason = chunk.finish_reason;
                }
                if chunk.usage.is_some() {
                    out.usage = chunk.usage;
                }
            }
        }
    }
    // A stream that ends with neither data nor `[DONE]` is a dropped connection, not an
    // empty answer: say so instead of returning a silent blank reply.
    if !answered && !out.cancelled {
        return Err(LlmError::Stream("the server closed the connection without replying".into()));
    }
    for ev in think.finish() {
        emit(&mut out, ev);
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
