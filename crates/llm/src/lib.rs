//! Client for any OpenAI-compatible `/chat/completions` endpoint
//! (llama-server, vLLM, Ollama, LM Studio, hosted APIs, ...), and for Anthropic's Messages
//! API ([`byob_types::ApiKind::Anthropic`], see [`anthropic`]) behind the same interface.
//!
//! Blocking by design: a streaming call runs on the caller's thread and reports deltas
//! through a callback. Everything the server sends is treated as untrusted input.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

pub mod agents;
pub mod anthropic;
pub mod budget;
pub mod params;
mod probe;
mod sse;
mod think;
pub mod toolprompt;
mod wire;

use std::io::{BufRead, BufReader, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

pub use budget::{estimate_history_tokens, estimate_message_tokens, estimate_tokens, fit_history};
use byob_types::{ApiKind, InferenceConfig};
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
    /// The server is busy or rate-limiting (Anthropic 429, 529): worth trying again after
    /// a pause. Reads like [`LlmError::Http`].
    #[error("server returned HTTP {status}: {message}")]
    Busy { status: u16, message: String },
    /// The model declined the request (Anthropic `stop_reason: "refusal"`). Trying the same
    /// request again gets the same answer.
    #[error("the model declined to answer: {0}")]
    Refused(String),
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
    /// Anthropic: the reply's content blocks as sent (thinking with signatures, text,
    /// `tool_use`). Put them on the assistant message ([`ChatMessage::with_native`]) so
    /// later requests of the same tool loop send them back unchanged. Empty for OpenAI.
    pub native: Vec<serde_json::Value>,
    /// Prompt tokens read from / written to the server's prompt cache (Anthropic; included
    /// in [`Usage::prompt_tokens`]).
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
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
        let cli = config.api_kind() == ApiKind::Cli;
        if !matches!(base.scheme(), "http" | "https") && !cli {
            return Err(LlmError::BadUrl(url::ParseError::RelativeUrlWithoutBase));
        }
        let endpoint = match config.api_kind() {
            ApiKind::OpenAi => base.join("chat/completions")?,
            ApiKind::Anthropic => base.join("messages")?,
            // Nothing is sent over HTTP: the agent CLI runs as a subprocess.
            ApiKind::Cli => base.clone(),
        };
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(10)))
            // Long prompts can take a while before the first byte (prompt processing).
            .timeout_recv_response(Some(Duration::from_secs(600)))
            .build()
            .new_agent();
        Ok(Self { config, base, endpoint, agent })
    }

    /// The agent of a `cli://<agent>` endpoint ([`agents`]), `None` for HTTP ones.
    pub fn cli_agent(&self) -> Option<&str> {
        if self.config.api_kind() != ApiKind::Cli {
            return None;
        }
        byob_types::cli_agent(&self.config.base_url)
    }

    pub fn endpoint(&self) -> &url::Url {
        &self.endpoint
    }

    pub fn config(&self) -> &InferenceConfig {
        &self.config
    }

    /// This client with `config`'s request options (params, context); same endpoint and
    /// connections.
    pub fn with_options_of(mut self, config: &InferenceConfig) -> Self {
        self.config.params.clone_from(&config.params);
        self.config.context = config.context;
        self
    }

    /// The API this client speaks ([`InferenceConfig::api_kind`]).
    pub fn api(&self) -> ApiKind {
        self.config.api_kind()
    }

    /// Headers that authenticate (and, for Anthropic, version) every request.
    pub(crate) fn auth_headers(&self) -> Vec<(&'static str, String)> {
        let key = self.config.api_key.as_deref();
        match self.api() {
            ApiKind::OpenAi => key.map(|k| ("Authorization", format!("Bearer {k}"))).into_iter().collect(),
            ApiKind::Cli => Vec::new(),
            ApiKind::Anthropic => {
                let mut h = vec![("anthropic-version", anthropic::VERSION.to_owned())];
                h.extend(key.map(|k| ("x-api-key", k.to_owned())));
                h
            }
        }
    }

    /// The model list's path under the base URL (Anthropic pages it: ask for all at once).
    pub(crate) fn models_path(&self) -> &'static str {
        match self.api() {
            ApiKind::OpenAi | ApiKind::Cli => "models",
            ApiKind::Anthropic => "models?limit=1000",
        }
    }

    /// `GET {base}/models`: model ids the server advertises.
    pub fn list_models(&self) -> Result<Vec<String>, LlmError> {
        self.list_models_within(Duration::from_secs(30))
    }

    /// [`Client::list_models`] that gives up after `timeout` (connecting included): a cheap
    /// "is anything there, and does it take our key" check.
    pub fn list_models_within(&self, timeout: Duration) -> Result<Vec<String>, LlmError> {
        if let Some(id) = self.cli_agent() {
            // Detection spawns the CLI (`--version`, `auth status`); it's cached, and its
            // own short timeouts stand in for `timeout`.
            return agents::list_models(id);
        }
        let url = self.base.join(self.models_path())?;
        let mut req = self.agent.get(url.as_str());
        for (name, value) in self.auth_headers() {
            req = req.header(name, &value);
        }
        let resp = req.config().timeout_global(Some(timeout)).build().call().map_err(|e| LlmError::Transport { url: url.to_string(), source: Box::new(e) })?;
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
        on_event: impl FnMut(StreamEvent),
    ) -> Result<ChatOutcome, LlmError> {
        self.stream_chat_until(messages, tools, StopWhen { cancel, also: None, deadline: None }, on_event)
    }

    /// [`Client::stream_chat`] in llama.cpp server slot `slot` (`"id_slot"`, so the slot's
    /// prompt cache is reused); `None` sends no slot. Only pass a slot to a server that
    /// reported `total_slots` ([`Capabilities::slots`]): others may reject the field.
    pub fn stream_chat_in_slot(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        slot: Option<u64>,
        cancel: &AtomicBool,
        on_event: impl FnMut(StreamEvent),
    ) -> Result<ChatOutcome, LlmError> {
        self.stream(messages, tools, slot, StopWhen { cancel, also: None, deadline: None }, on_event)
    }

    /// [`Client::stream_chat`] that also stops on a second flag (e.g. a caller's own size
    /// limit set from `on_event`) or at a deadline. All are checked every 50 ms, so they
    /// work even while the server sends nothing.
    pub fn stream_chat_until(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        stop_when: StopWhen<'_>,
        on_event: impl FnMut(StreamEvent),
    ) -> Result<ChatOutcome, LlmError> {
        self.stream(messages, tools, None, stop_when, on_event)
    }

    fn stream(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        slot: Option<u64>,
        stop_when: StopWhen<'_>,
        mut on_event: impl FnMut(StreamEvent),
    ) -> Result<ChatOutcome, LlmError> {
        if let Some(id) = self.cli_agent() {
            return agents::stream(id, &self.config, messages, tools, &stop_when, &mut on_event);
        }
        let cancel = &stop_when;
        let api = self.api();
        let mut headers = vec![("Accept", "text/event-stream".to_owned()), ("Content-Type", "application/json".to_owned())];
        headers.extend(self.auth_headers());
        // Anthropic: the request with its beta extras, and the plain one to fall back to if
        // the endpoint rejects them.
        let (first, plain) = match api {
            ApiKind::OpenAi => {
                let request = ChatRequest::new(&self.config.model, messages, tools).in_slot(slot);
                let encoding = |e: serde_json::Error| LlmError::Stream(format!("encoding request: {e}"));
                let mut body = serde_json::to_value(&request).map_err(encoding)?;
                params::apply_openai(&mut body, &self.config.params);
                let body = serde_json::to_vec(&body).map_err(encoding)?;
                (Attempt { body, headers }, None)
            }
            ApiKind::Cli => return Err(LlmError::Stream("agent CLIs don't take HTTP requests".into())),
            ApiKind::Anthropic => {
                let p = &self.config.params;
                let full = anthropic::request(&self.config.model, messages, tools, true, p)?;
                let plain = match full.betas.is_empty() {
                    true => None,
                    false => Some(Attempt { body: anthropic::request(&self.config.model, messages, tools, false, p)?.body, headers: headers.clone() }),
                };
                let mut headers = headers;
                if !full.betas.is_empty() {
                    headers.push(("anthropic-beta", full.betas.join(",")));
                }
                (Attempt { body: full.body, headers }, plain)
            }
        };
        if cancel.now() {
            return Ok(ChatOutcome { cancelled: true, ..ChatOutcome::default() });
        }
        // The blocking HTTP exchange runs on its own thread so Stop works even while the
        // server is silent (e.g. processing a long prompt before the first token): we
        // check `cancel` every POLL instead of only when a chunk arrives.
        let (tx, rx) = mpsc::channel::<Progress>();
        let stop = Arc::new(AtomicBool::new(false));
        let (agent, endpoint, worker_stop) = (self.agent.clone(), self.endpoint.clone(), Arc::clone(&stop));
        let spawned = std::thread::Builder::new().name("byob-llm".into()).spawn(move || {
            let events = tx.clone();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut send = |ev| {
                    let _ = events.send(Progress::Event(ev));
                };
                let result = exchange(&agent, &endpoint, api, &first, &worker_stop, &mut send);
                match (result, plain) {
                    // Nothing was streamed yet: the plain request is safe to send.
                    (Err(e), Some(plain)) if anthropic::rejects_extras(&e) => {
                        log::warn!("the endpoint rejected a beta feature ({e}); sending the request without them");
                        exchange(&agent, &endpoint, api, &plain, &worker_stop, &mut send)
                    }
                    (result, _) => result,
                }
            }))
            .unwrap_or_else(|_| Err(LlmError::Stream("internal error while reading the reply".into())));
            let _ = tx.send(Progress::Done(result));
        });
        if let Err(e) = spawned {
            return Err(LlmError::Stream(format!("couldn't start the request: {e}")));
        }
        // What arrived so far, returned if the caller stops us.
        let mut partial = ChatOutcome::default();
        loop {
            if cancel.now() {
                stop.store(true, Ordering::Relaxed);
                partial.cancelled = true;
                return Ok(partial);
            }
            match rx.recv_timeout(POLL) {
                Ok(Progress::Event(ev)) => {
                    match &ev {
                        StreamEvent::Content(t) => partial.content.push_str(t),
                        StreamEvent::Reasoning(t) => partial.reasoning.push_str(t),
                    }
                    on_event(ev);
                }
                Ok(Progress::Done(result)) => return result,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(LlmError::Stream("the request ended unexpectedly".into())),
            }
        }
    }
}

/// When [`Client::stream_chat_until`] gives up: either flag set, or the deadline passed.
#[derive(Clone, Copy)]
pub struct StopWhen<'a> {
    pub cancel: &'a AtomicBool,
    pub also: Option<&'a AtomicBool>,
    pub deadline: Option<std::time::Instant>,
}

impl StopWhen<'_> {
    pub(crate) fn now(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
            || self.also.is_some_and(|f| f.load(Ordering::Relaxed))
            || self.deadline.is_some_and(|d| std::time::Instant::now() >= d)
    }
}

/// How often a waiting [`Client::stream_chat`] checks its cancel flag.
const POLL: std::time::Duration = std::time::Duration::from_millis(50);

enum Progress {
    Event(StreamEvent),
    Done(Result<ChatOutcome, LlmError>),
}

/// One request body and its headers.
struct Attempt {
    body: Vec<u8>,
    headers: Vec<(&'static str, String)>,
}

/// One streaming request and its reply (on the request thread).
fn exchange(
    agent: &ureq::Agent,
    endpoint: &url::Url,
    api: ApiKind,
    attempt: &Attempt,
    stop: &AtomicBool,
    on_event: &mut impl FnMut(StreamEvent),
) -> Result<ChatOutcome, LlmError> {
    let mut req = agent.post(endpoint.as_str());
    for (name, value) in &attempt.headers {
        req = req.header(*name, value);
    }
    let resp = req.send(&attempt.body[..]).map_err(|e| LlmError::Transport { url: endpoint.to_string(), source: Box::new(e) })?;
    let status = resp.status().as_u16();
    if status >= 400 {
        let mut text = String::new();
        let _ = resp.into_body().into_reader().take(MAX_ERROR_BODY).read_to_string(&mut text);
        let message = wire::error_message(&text);
        // Anthropic: rate-limited (429) or overloaded (529) is worth another try.
        return Err(match (api, status) {
            (ApiKind::Anthropic, 429 | 529) => LlmError::Busy { status, message },
            _ => LlmError::Http { status, message },
        });
    }
    let reader = BufReader::new(resp.into_body().into_reader());
    match api {
        ApiKind::OpenAi | ApiKind::Cli => read_stream(reader, stop, on_event),
        ApiKind::Anthropic => anthropic::read_stream(reader, stop, on_event),
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
mod anthropic_tests;
#[cfg(test)]
mod tests;
