//! Endpoint capability probe: what kind of server is behind the URL, how much context it
//! has, whether it takes images, and whether native tool calling actually works.
//!
//! Every step is optional and failure-tolerant; each request has its own short timeout and
//! the whole probe stops starting new steps once its budget is spent.

use std::time::{Duration, Instant};

use serde_json::Value;

use crate::{Client, LlmError, ToolSpec, toolprompt, wire};

/// Most bytes read from any metadata response.
const MAX_META_BODY: u64 = 4 * 1024 * 1024;
/// Steps are skipped when less than this is left of the budget.
const MIN_STEP: Duration = Duration::from_millis(250);

/// Best guess at the server software behind the URL.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ServerKind {
    LlamaCpp,
    Vllm,
    Ollama,
    LmStudio,
    OpenAi,
    #[default]
    Unknown,
}

/// How the endpoint handles the `tools` request field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolSupport {
    /// Structured `tool_calls` came back.
    Native,
    /// The model wrote a tool call as text: use [`crate::toolprompt`] parsing.
    InContent,
    /// The server rejected tools, or the model ignored them.
    Unsupported,
    /// Not tested, or the test was inconclusive (timeout, transport error, truncated reply).
    #[default]
    Unknown,
}

/// What [`Client::probe`] found. Fields are `None`/`Unknown` when not discoverable.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Capabilities {
    pub server: ServerKind,
    /// Model ids from `GET /models`.
    pub models: Vec<String>,
    /// Context window the server will actually use for a request, in tokens.
    pub context_length: Option<u64>,
    /// The model's trained/maximum context, when the server reports it separately
    /// (Ollama, LM Studio). Not necessarily usable: the runtime window may be smaller.
    pub max_context_length: Option<u64>,
    /// `Some(true)` when the server advertises image input, `Some(false)` when it says no.
    pub vision: Option<bool>,
    pub tools: ToolSupport,
    /// llama.cpp `/tokenize` works, so [`Client::count_tokens`] gives exact counts.
    pub tokenize: bool,
    /// Requests the server runs in parallel (llama.cpp `/props` `total_slots`).
    pub slots: Option<u64>,
    /// Human-readable log of what each step saw, for diagnostics.
    pub notes: Vec<String>,
}

impl Capabilities {
    /// Context to budget against: the probed one, or [`crate::budget::DEFAULT_CONTEXT_TOKENS`].
    pub fn effective_context(&self) -> usize {
        self.context_length.and_then(|n| usize::try_from(n).ok()).filter(|&n| n > 0).unwrap_or(crate::budget::DEFAULT_CONTEXT_TOKENS)
    }
}

/// Tuning for [`Client::probe_with`].
#[derive(Debug, Clone)]
pub struct ProbeOptions {
    /// No new step starts after this much time.
    pub budget: Duration,
    /// Per-request cap for metadata requests.
    pub step_timeout: Duration,
    /// Per-request cap for the tool-calling test (a real generation).
    pub tool_timeout: Duration,
    /// Run the tool-calling test (costs one short generation).
    pub test_tools: bool,
}

impl Default for ProbeOptions {
    fn default() -> Self {
        Self { budget: Duration::from_secs(20), step_timeout: Duration::from_secs(4), tool_timeout: Duration::from_secs(15), test_tools: true }
    }
}

/// A finished metadata request.
struct Reply {
    status: u16,
    server: Option<String>,
    body: Value,
}

struct Prober<'a> {
    client: &'a Client,
    opts: ProbeOptions,
    deadline: Instant,
    caps: Capabilities,
    root: url::Url,
}

impl Client {
    /// Probe the endpoint with default options (≈20 s budget). Never fails: anything that
    /// cannot be determined is left as `None`/`Unknown` and explained in `notes`.
    pub fn probe(&self) -> Capabilities {
        self.probe_with(ProbeOptions::default())
    }

    /// Probe with explicit options.
    pub fn probe_with(&self, opts: ProbeOptions) -> Capabilities {
        let deadline = Instant::now() + opts.budget;
        let mut p = Prober { client: self, root: self.root(), opts, deadline, caps: Capabilities::default() };
        p.run();
        p.caps
    }

    /// The server root: the base URL without a trailing `v1/` segment.
    pub(crate) fn root(&self) -> url::Url {
        if self.base.path().ends_with("/v1/")
            && let Ok(u) = self.base.join("../")
        {
            return u;
        }
        self.base.clone()
    }

    /// Exact token count via llama.cpp's `POST /tokenize`.
    pub fn count_tokens(&self, text: &str) -> Result<usize, LlmError> {
        let url = self.root().join("tokenize")?;
        let body = serde_json::json!({"content": text});
        let reply = self.request(&url, Some(&body), Duration::from_secs(10))?;
        if reply.status >= 400 {
            return Err(LlmError::Http { status: reply.status, message: wire::error_message(&reply.body.to_string()) });
        }
        reply.body.get("tokens").and_then(Value::as_array).map(Vec::len).ok_or_else(|| LlmError::Stream("tokenize reply has no tokens".into()))
    }

    /// GET (or POST `body`) returning parsed JSON (`Null` if the body is not JSON).
    fn request(&self, url: &url::Url, body: Option<&Value>, timeout: Duration) -> Result<Reply, LlmError> {
        let transport = |e| LlmError::Transport { url: url.to_string(), source: Box::new(e) };
        let auth = self.config.api_key.as_ref().map(|k| format!("Bearer {k}"));
        let resp = match body {
            Some(b) => {
                let mut req = self.agent.post(url.as_str()).header("Content-Type", "application/json");
                if let Some(a) = &auth {
                    req = req.header("Authorization", a);
                }
                req.config().timeout_global(Some(timeout)).build().send(b.to_string().as_bytes()).map_err(transport)?
            }
            None => {
                let mut req = self.agent.get(url.as_str());
                if let Some(a) = &auth {
                    req = req.header("Authorization", a);
                }
                req.config().timeout_global(Some(timeout)).build().call().map_err(transport)?
            }
        };
        let status = resp.status().as_u16();
        let server = resp.headers().get("server").and_then(|v| v.to_str().ok()).map(str::to_owned);
        let text = resp.into_body().with_config().limit(MAX_META_BODY).read_to_string().map_err(|e| LlmError::Stream(e.to_string()))?;
        let body = serde_json::from_str(&text).unwrap_or(Value::Null);
        Ok(Reply { status, server, body })
    }
}

impl Prober<'_> {
    fn run(&mut self) {
        self.models();
        // Only ask the endpoints that make sense for what we know so far.
        let maybe = |p: &Self, k: ServerKind| matches!(p.caps.server, ServerKind::Unknown) || p.caps.server == k;
        if maybe(self, ServerKind::LlamaCpp) {
            self.props();
        }
        if maybe(self, ServerKind::Ollama) {
            self.ollama();
        }
        if maybe(self, ServerKind::LmStudio) {
            self.lm_studio();
        }
        if maybe(self, ServerKind::LlamaCpp) {
            self.tokenize();
        }
        if self.opts.test_tools {
            self.tool_test();
        }
    }

    fn note(&mut self, s: impl Into<String>) {
        self.caps.notes.push(s.into());
    }

    fn left(&self) -> Option<Duration> {
        Some(self.deadline.checked_duration_since(Instant::now())?).filter(|d| *d >= MIN_STEP)
    }

    /// One bounded request; `None` (with a note) if out of budget or it failed.
    fn fetch(&mut self, url: &url::Url, body: Option<&Value>, cap: Duration) -> Option<Reply> {
        let Some(left) = self.left() else {
            self.note(format!("{}: skipped, probe budget spent", url.path()));
            return None;
        };
        match self.client.request(url, body, cap.min(left)) {
            Ok(r) => Some(r),
            Err(e) => {
                self.note(format!("{}: {e}", url.path()));
                None
            }
        }
    }

    fn fetch_path(&mut self, root: bool, path: &str, body: Option<&Value>) -> Option<Reply> {
        let base = if root { self.root.clone() } else { self.client.base.clone() };
        let url = base.join(path).ok()?;
        let reply = self.fetch(&url, body, self.opts.step_timeout)?;
        if reply.status >= 400 {
            self.note(format!("{}: HTTP {}", url.path(), reply.status));
            return None;
        }
        Some(reply)
    }

    fn set_kind(&mut self, kind: ServerKind, why: &str) {
        if self.caps.server == ServerKind::Unknown && kind != ServerKind::Unknown {
            self.caps.server = kind;
            self.note(format!("server looks like {kind:?} ({why})"));
        }
    }

    fn set_context(&mut self, n: Option<u64>, why: &str) {
        if let (None, Some(n)) = (self.caps.context_length, n.filter(|&n| n > 0)) {
            self.caps.context_length = Some(n);
            self.note(format!("context {n} tokens from {why}"));
        }
    }

    fn set_vision(&mut self, v: Option<bool>) {
        if self.caps.vision != Some(true) {
            self.caps.vision = v.or(self.caps.vision);
        }
    }

    fn wanted_model(&self) -> Option<String> {
        let m = self.client.config.model.trim();
        if !m.is_empty() && (self.caps.models.is_empty() || self.caps.models.iter().any(|x| x == m)) {
            return Some(m.to_owned());
        }
        self.caps.models.first().cloned().or_else(|| Some(m.to_owned()).filter(|m| !m.is_empty()))
    }

    /// `GET {base}/models`: ids, plus llama.cpp `meta.n_ctx`, vLLM `max_model_len`, OpenRouter-style
    /// `context_length` and `architecture.input_modalities`.
    fn models(&mut self) {
        let host = self.client.base.host_str().unwrap_or_default().to_ascii_lowercase();
        if host == "api.openai.com" {
            self.set_kind(ServerKind::OpenAi, "host");
        }
        let Some(reply) = self.fetch_path(false, "models", None) else { return };
        if let Some(s) = reply.server.as_deref().filter(|s| s.to_ascii_lowercase().contains("llama")) {
            let why = format!("Server: {s}");
            self.set_kind(ServerKind::LlamaCpp, &why);
        }
        self.caps.models = wire::model_ids(&reply.body.to_string());
        let entries = reply.body.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
        let model = self.wanted_model();
        let entry = entries.iter().find(|e| e.get("id").and_then(Value::as_str) == model.as_deref()).or(entries.first()).cloned().unwrap_or(Value::Null);
        let owned_by = entry.get("owned_by").and_then(Value::as_str).unwrap_or_default().to_ascii_lowercase();
        let kind = match owned_by.as_str() {
            "llamacpp" | "llama.cpp" => ServerKind::LlamaCpp,
            "vllm" => ServerKind::Vllm,
            "library" | "ollama" => ServerKind::Ollama,
            "organization_owner" => ServerKind::LmStudio,
            "openai" | "openai-internal" | "system" => ServerKind::OpenAi,
            _ if entry.get("max_model_len").is_some() => ServerKind::Vllm,
            _ => ServerKind::Unknown,
        };
        self.set_kind(kind, "/models metadata");
        self.set_context(entry.pointer("/meta/n_ctx").and_then(Value::as_u64), "/models meta.n_ctx");
        self.set_context(entry.get("max_model_len").and_then(Value::as_u64), "/models max_model_len");
        self.set_context(entry.get("context_length").and_then(Value::as_u64), "/models context_length");
        if let Some(mods) = entry.pointer("/architecture/input_modalities").and_then(Value::as_array) {
            self.set_vision(Some(mods.iter().any(|m| m.as_str() == Some("image"))));
        }
    }

    /// llama.cpp `GET /props`: runtime `n_ctx`, `modalities.vision` and `total_slots`.
    fn props(&mut self) {
        let Some(reply) = self.fetch_path(true, "props", None) else { return };
        let Some(settings) = reply.body.get("default_generation_settings") else { return };
        // Hostile or silly values are ignored; nobody runs thousands of slots.
        if let Some(n) = reply.body.get("total_slots").and_then(Value::as_u64).filter(|n| (1..=1024).contains(n)) {
            self.caps.slots = Some(n);
            self.note(format!("{n} parallel slots from /props total_slots"));
        }
        self.set_kind(ServerKind::LlamaCpp, "/props");
        self.set_context(settings.get("n_ctx").and_then(Value::as_u64), "/props n_ctx");
        self.set_vision(reply.body.pointer("/modalities/vision").and_then(Value::as_bool));
    }

    /// Ollama `GET /api/version` then `POST /api/show` for context and capabilities.
    fn ollama(&mut self) {
        if self.caps.server != ServerKind::Ollama {
            let Some(reply) = self.fetch_path(true, "api/version", None) else { return };
            if reply.body.get("version").and_then(Value::as_str).is_none() {
                return;
            }
            self.set_kind(ServerKind::Ollama, "/api/version");
        }
        let Some(model) = self.wanted_model() else { return };
        let Some(show) = self.fetch_path(true, "api/show", Some(&serde_json::json!({"model": model}))) else { return };
        let trained = show
            .body
            .get("model_info")
            .and_then(Value::as_object)
            .and_then(|info| info.iter().find(|(k, _)| k.ends_with(".context_length")).and_then(|(_, v)| v.as_u64()));
        self.caps.max_context_length = trained;
        // The runtime window is `num_ctx` if the model sets one; otherwise Ollama's server
        // default applies (often much smaller than the trained length), which we can't see.
        let num_ctx = show
            .body
            .get("parameters")
            .and_then(Value::as_str)
            .and_then(|p| p.lines().find_map(|l| l.trim().strip_prefix("num_ctx").and_then(|v| v.trim().parse::<u64>().ok())));
        match num_ctx {
            Some(n) => self.set_context(Some(n), "Ollama num_ctx"),
            None => self.note(format!("Ollama: trained context {trained:?}, runtime num_ctx not reported (server default applies)")),
        }
        if let Some(caps) = show.body.get("capabilities").and_then(Value::as_array) {
            self.set_vision(Some(caps.iter().any(|c| c.as_str() == Some("vision"))));
        }
    }

    /// LM Studio `GET /api/v0/models`: `type` (llm/vlm) and loaded/max context.
    fn lm_studio(&mut self) {
        let Some(reply) = self.fetch_path(true, "api/v0/models", None) else { return };
        let entries = reply.body.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
        let model = self.wanted_model();
        let Some(entry) = entries.iter().find(|e| e.get("id").and_then(Value::as_str) == model.as_deref()).or(entries.first()).cloned() else {
            return;
        };
        if entry.get("max_context_length").is_none() && entry.get("type").is_none() {
            return;
        }
        self.set_kind(ServerKind::LmStudio, "/api/v0/models");
        self.caps.max_context_length = entry.get("max_context_length").and_then(Value::as_u64);
        self.set_context(entry.get("loaded_context_length").and_then(Value::as_u64), "LM Studio loaded_context_length");
        self.set_context(self.caps.max_context_length, "LM Studio max_context_length");
        if let Some(t) = entry.get("type").and_then(Value::as_str) {
            self.set_vision(Some(t == "vlm"));
        }
    }

    /// llama.cpp `POST /tokenize`.
    fn tokenize(&mut self) {
        let Some(reply) = self.fetch_path(true, "tokenize", Some(&serde_json::json!({"content": "hello world"}))) else { return };
        self.caps.tokenize = reply.body.get("tokens").and_then(Value::as_array).is_some_and(|t| !t.is_empty());
        if self.caps.tokenize {
            self.note("/tokenize available");
        }
    }

    /// A tiny real request with one trivial tool, classified by what comes back.
    fn tool_test(&mut self) {
        let Ok(url) = self.client.base.join("chat/completions") else { return };
        let tool = probe_tool();
        let body = serde_json::json!({
            "model": self.wanted_model().unwrap_or_default(),
            "messages": [{"role": "user", "content": "Call the get_magic_number tool now. Do not reply with text."}],
            "tools": [{"type": "function", "function": {"name": tool.name, "description": tool.description, "parameters": tool.parameters}}],
            "max_tokens": 512,
            "temperature": 0,
            "stream": false,
        });
        let Some(reply) = self.fetch(&url, Some(&body), self.opts.tool_timeout) else { return };
        let (tools, why) = classify_tool_reply(reply.status, &reply.body, &tool);
        self.caps.tools = tools;
        self.note(format!("tool test: {tools:?} ({why})"));
    }
}

fn probe_tool() -> ToolSpec {
    ToolSpec {
        name: "get_magic_number".into(),
        description: "Returns the magic number.".into(),
        parameters: serde_json::json!({"type": "object", "properties": {}, "required": []}),
    }
}

/// Classify a non-streamed reply to the tool test.
fn classify_tool_reply(status: u16, body: &Value, tool: &ToolSpec) -> (ToolSupport, String) {
    if status >= 400 {
        let msg = body.get("error").map(|e| e.get("message").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| e.to_string()));
        let msg: String = msg.unwrap_or_else(|| body.to_string()).chars().take(200).collect();
        // 5xx/429 are about the server's state, not about tools.
        let kind = if status >= 500 || status == 429 { ToolSupport::Unknown } else { ToolSupport::Unsupported };
        return (kind, format!("HTTP {status}: {msg}"));
    }
    let Some(choice) = body.pointer("/choices/0").filter(|c| c.is_object()) else { return (ToolSupport::Unknown, "no choices in reply".into()) };
    let message = choice.get("message").cloned().unwrap_or(Value::Null);
    let native = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .is_some_and(|calls| calls.iter().any(|c| c.pointer("/function/name").and_then(Value::as_str).is_some_and(|n| !n.is_empty())));
    if native {
        return (ToolSupport::Native, "structured tool_calls".into());
    }
    let content = message.get("content").and_then(Value::as_str).unwrap_or_default();
    let parsed = toolprompt::parse_tool_calls(content, std::slice::from_ref(tool));
    if !parsed.calls.is_empty() || content.contains(tool.name.as_str()) && content.contains('{') {
        return (ToolSupport::InContent, "tool call written into content".into());
    }
    let finish = choice.get("finish_reason").and_then(Value::as_str).unwrap_or_default();
    if finish == "length" && content.trim().is_empty() {
        return (ToolSupport::Unknown, "reply truncated before any answer".into());
    }
    (ToolSupport::Unsupported, format!("model answered in text (finish_reason {finish:?})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(status: u16, body: &str) -> ToolSupport {
        classify_tool_reply(status, &serde_json::from_str(body).unwrap_or(Value::Null), &probe_tool()).0
    }

    #[test]
    fn classifies_tool_replies() {
        let native = r#"{"choices":[{"message":{"content":null,"tool_calls":[{"id":"1","type":"function","function":{"name":"get_magic_number","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#;
        assert_eq!(classify(200, native), ToolSupport::Native);
        let leaked =
            r#"{"choices":[{"message":{"content":"<tool_call>\n{\"name\": \"get_magic_number\", \"arguments\": {}}\n</tool_call>"},"finish_reason":"stop"}]}"#;
        assert_eq!(classify(200, leaked), ToolSupport::InContent);
        let text = r#"{"choices":[{"message":{"content":"I cannot do that."},"finish_reason":"stop"}]}"#;
        assert_eq!(classify(200, text), ToolSupport::Unsupported);
        let cut = r#"{"choices":[{"message":{"content":"","reasoning_content":"hmm"},"finish_reason":"length"}]}"#;
        assert_eq!(classify(200, cut), ToolSupport::Unknown);
        assert_eq!(classify(400, r#"{"error":{"message":"tools param requires --jinja flag"}}"#), ToolSupport::Unsupported);
        assert_eq!(classify(503, r#"{"error":{"message":"loading model"}}"#), ToolSupport::Unknown);
        for junk in ["null", "[]", "{}", r#"{"choices":"x"}"#, r#"{"choices":[{"message":{"tool_calls":[{"function":5}]}}]}"#] {
            assert_ne!(classify(200, junk), ToolSupport::Native);
        }
    }
}
