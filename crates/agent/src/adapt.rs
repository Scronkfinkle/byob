//! Adapting turns to the inference endpoint.
//!
//! The engine probes each endpoint ([`byob_llm::Client::probe`]) on a background thread,
//! caches what it found per URL + model, and turns that into a [`Plan`] for every turn:
//! native or prompt-based tool calling, a token budget for the history, and whether image
//! parts may be sent. A turn that finds out the server rejects native tools reports it back
//! ([`Event::ToolsRejected`]) so later turns go straight to prompt mode.

use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::Sender;
use std::thread;

use byob_llm::{Capabilities, ChatMessage, LlmError, ServerKind, ToolSpec, ToolSupport, budget, toolprompt};
use byob_protocol::{EndpointInfo, ToolsMode};
use byob_types::InferenceConfig;

use crate::Input;

/// Replaces image parts for endpoints that say they cannot see.
const NO_VISION_NOTE: &str = "[an image was attached here, but this model cannot see images]";

/// How turns call tools, regardless of the probe (for testing and troubleshooting).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolMode {
    /// Follow the probe; try native first when it is inconclusive.
    #[default]
    Auto,
    Native,
    Prompt,
}

impl ToolMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Self::Auto),
            "native" => Some(Self::Native),
            "prompt" => Some(Self::Prompt),
            _ => None,
        }
    }
}

/// Endpoint adaptation settings of an engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdaptOptions {
    pub tool_mode: ToolMode,
    /// Probe endpoints in the background. Off in this crate's unit tests, whose scripted
    /// fake servers expect an exact sequence of requests.
    pub probe: bool,
}

impl Default for AdaptOptions {
    fn default() -> Self {
        Self { tool_mode: ToolMode::Auto, probe: !cfg!(test) }
    }
}

/// How one turn talks to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Cache key of the endpoint (see [`key`]).
    pub key: String,
    /// Describe tools in the system prompt and parse calls out of the reply text.
    pub prompt_tools: bool,
    /// If the server rejects native `tools`, switch to prompt mode and retry once.
    pub fallback: bool,
    /// Context window to budget against, in tokens.
    pub context: usize,
    /// Tokens kept free for the reply.
    pub reserve: usize,
    pub vision: Option<bool>,
}

impl Plan {
    /// The messages for one model request: images dropped if the endpoint can't see, the
    /// tool prompt added in prompt mode, and the whole thing fitted to the context.
    pub fn request(&self, messages: &[ChatMessage], specs: &[ToolSpec]) -> Vec<ChatMessage> {
        let strip = self.vision == Some(false) && messages.iter().any(|m| !m.images.is_empty());
        let prompt = if self.prompt_tools { toolprompt::tool_prompt(specs) } else { String::new() };
        // Native tool definitions take room in the prompt too.
        let reserve = self.reserve.saturating_add(if self.prompt_tools { 0 } else { budget::estimate_tools_tokens(specs) });
        if !strip && prompt.is_empty() {
            return budget::fit_history(messages, self.context, reserve);
        }
        let mut msgs: Vec<ChatMessage> = messages.iter().map(|m| if strip { m.without_images(NO_VISION_NOTE) } else { m.clone() }).collect();
        if !prompt.is_empty() {
            match msgs.first_mut().filter(|m| m.role == "system") {
                Some(sys) => {
                    let mut text = sys.content.take().unwrap_or_default();
                    text.push_str("\n\n");
                    text.push_str(&prompt);
                    sys.content = Some(text);
                }
                None => msgs.insert(0, ChatMessage::system(prompt)),
            }
        }
        budget::fit_history(&msgs, self.context, reserve)
    }

    /// Estimated tokens of a request for `messages` before any fitting: the messages plus
    /// the tool definitions (native) or the tool prompt (prompt mode).
    pub fn raw_tokens(&self, messages: &[ChatMessage], specs: &[ToolSpec]) -> usize {
        let tools = match (self.prompt_tools, specs.is_empty()) {
            (_, true) => 0,
            (true, false) => budget::estimate_tokens(&toolprompt::tool_prompt(specs)),
            (false, false) => budget::estimate_tools_tokens(specs),
        };
        budget::estimate_history_tokens(messages).saturating_add(tools)
    }
}

/// Did the server refuse the request because of the `tools` field? (llama-server without
/// `--jinja`, servers without a tool parser, templates that can't render tools.)
pub fn rejects_tools(err: &LlmError) -> bool {
    let LlmError::Http { status, message } = err else { return false };
    let m = message.to_ascii_lowercase();
    *status >= 400 && ["tool", "jinja", "function"].iter().any(|w| m.contains(w))
}

/// Cache key of an endpoint: base URL and model.
pub fn key(config: &InferenceConfig) -> String {
    format!("{} {}", config.base_url.trim().trim_end_matches('/'), config.model.trim())
}

/// Messages from probe threads and turns.
pub enum Event {
    Probed {
        key: String,
        caps: Box<Capabilities>,
    },
    /// A turn's native tool request was rejected; it switched to prompt mode.
    ToolsRejected {
        key: String,
    },
}

/// What the engine knows about the endpoints it has used.
#[derive(Default)]
pub struct Endpoints {
    opts: AdaptOptions,
    cache: BTreeMap<String, Capabilities>,
    probing: BTreeSet<String>,
    /// Endpoints whose server rejected native tools during a turn.
    rejected: BTreeSet<String>,
}

impl Endpoints {
    pub fn new(opts: AdaptOptions) -> Self {
        Self { opts, ..Default::default() }
    }

    /// Probe `client`'s endpoint on a background thread unless it is cached or already being
    /// probed. The result arrives in the engine inbox as [`Event::Probed`].
    pub fn probe(&mut self, client: &byob_llm::Client, inbox: &Sender<Input>) {
        let key = key(client.config());
        if !self.opts.probe || self.probing.contains(&key) || self.cache.get(&key).is_some_and(conclusive) {
            return;
        }
        let (client, inbox, k) = (client.clone(), inbox.clone(), key.clone());
        let spawned = thread::Builder::new().name("byob-probe".into()).spawn(move || {
            let caps = catch_unwind(AssertUnwindSafe(|| client.probe()))
                .unwrap_or_else(|_| Capabilities { notes: vec!["the probe crashed".into()], ..Default::default() });
            // The engine may be gone; nothing to do then.
            let _ = inbox.send(Input::Adapt(Event::Probed { key: k, caps: Box::new(caps) }));
        });
        match spawned {
            Ok(_) => {
                self.probing.insert(key);
            }
            Err(e) => log::warn!("could not start the endpoint probe: {e}"),
        }
    }

    /// Record an event. Always worth re-broadcasting the engine state afterwards.
    pub fn handle(&mut self, event: Event) {
        match event {
            Event::Probed { key, caps } => {
                log::info!("endpoint {key}: {:?}, context {:?}, vision {:?}, tools {:?}", caps.server, caps.context_length, caps.vision, caps.tools);
                for note in &caps.notes {
                    log::debug!("probe {key}: {note}");
                }
                self.probing.remove(&key);
                self.cache.insert(key, *caps);
            }
            Event::ToolsRejected { key } => {
                log::warn!("endpoint {key} rejected native tools; using prompt-based tool calling");
                self.rejected.insert(key);
            }
        }
    }

    pub fn plan(&self, config: &InferenceConfig) -> Plan {
        let key = key(config);
        let caps = self.cache.get(&key);
        let context = caps.map_or(budget::DEFAULT_CONTEXT_TOKENS, Capabilities::effective_context);
        let reserve = budget::default_reply_reserve(context);
        let (prompt_tools, fallback) = match self.opts.tool_mode {
            ToolMode::Native => (false, false),
            ToolMode::Prompt => (true, false),
            ToolMode::Auto if self.rejected.contains(&key) => (true, false),
            ToolMode::Auto => match caps.map(|c| c.tools) {
                Some(ToolSupport::InContent | ToolSupport::Unsupported) => (true, false),
                _ => (false, true),
            },
        };
        let vision = caps.and_then(|c| c.vision);
        Plan { key, prompt_tools, fallback, context, reserve, vision }
    }

    /// How many requests the endpoint runs in parallel, if its probe found out.
    pub fn slots(&self, config: &InferenceConfig) -> Option<u64> {
        self.cache.get(&key(config)).and_then(|c| c.slots)
    }

    pub fn info(&self, config: &InferenceConfig) -> EndpointInfo {
        let plan = self.plan(config);
        let caps = self.cache.get(&plan.key);
        let tools = if plan.prompt_tools {
            ToolsMode::Prompt
        } else if !plan.fallback || caps.is_some_and(|c| c.tools == ToolSupport::Native) {
            ToolsMode::Native
        } else {
            ToolsMode::Unknown
        };
        EndpointInfo {
            probing: self.probing.contains(&plan.key),
            server: caps.map(|c| server_name(c.server)).unwrap_or_default().into(),
            context_tokens: caps.and_then(|c| c.context_length),
            tools,
            vision: plan.vision,
        }
    }
}

/// A probe that learned something is kept; one that found nothing (server down, wrong URL)
/// is retried the next time the settings are applied.
fn conclusive(c: &Capabilities) -> bool {
    !c.models.is_empty() || c.context_length.is_some() || c.tools != ToolSupport::Unknown || c.server != ServerKind::Unknown
}

pub(crate) fn server_name(k: ServerKind) -> &'static str {
    match k {
        ServerKind::LlamaCpp => "llama.cpp",
        ServerKind::Vllm => "vLLM",
        ServerKind::Ollama => "Ollama",
        ServerKind::LmStudio => "LM Studio",
        ServerKind::OpenAi => "OpenAI",
        ServerKind::Unknown => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> InferenceConfig {
        InferenceConfig { base_url: "http://h/v1/".into(), model: "m".into(), api_key: None }
    }

    fn with(tools: ToolSupport, mode: ToolMode) -> Endpoints {
        let mut e = Endpoints::new(AdaptOptions { tool_mode: mode, probe: false });
        let caps = Capabilities { tools, context_length: Some(8192), vision: Some(false), ..Default::default() };
        e.handle(Event::Probed { key: key(&config()), caps: Box::new(caps) });
        e
    }

    #[test]
    fn plan_follows_probe_and_overrides() {
        let p = with(ToolSupport::Native, ToolMode::Auto).plan(&config());
        assert!(!p.prompt_tools && p.fallback);
        assert_eq!((p.context, p.reserve), (8192, 1024));
        assert!(with(ToolSupport::InContent, ToolMode::Auto).plan(&config()).prompt_tools);
        assert!(with(ToolSupport::Unsupported, ToolMode::Auto).plan(&config()).prompt_tools);
        let unknown = with(ToolSupport::Unknown, ToolMode::Auto);
        assert!(!unknown.plan(&config()).prompt_tools && unknown.plan(&config()).fallback);
        assert_eq!(unknown.info(&config()).tools, ToolsMode::Unknown);
        let forced = with(ToolSupport::Unsupported, ToolMode::Native).plan(&config());
        assert!(!forced.prompt_tools && !forced.fallback);
        assert!(with(ToolSupport::Native, ToolMode::Prompt).plan(&config()).prompt_tools);
        // Nothing probed: default budget, native first with fallback.
        let none = Endpoints::default().plan(&config());
        assert_eq!(none.context, budget::DEFAULT_CONTEXT_TOKENS);
        assert!(none.fallback && !none.prompt_tools);
    }

    #[test]
    fn rejection_is_remembered_per_endpoint() {
        let mut e = with(ToolSupport::Unknown, ToolMode::Auto);
        e.handle(Event::ToolsRejected { key: key(&config()) });
        assert!(e.plan(&config()).prompt_tools);
        assert_eq!(e.info(&config()).tools, ToolsMode::Prompt);
        let other = InferenceConfig { model: "other".into(), ..config() };
        assert!(!e.plan(&other).prompt_tools);
    }

    #[test]
    fn recognises_tool_rejections() {
        let http = |status, message: &str| LlmError::Http { status, message: message.into() };
        assert!(rejects_tools(&http(500, "tools param requires --jinja flag")));
        assert!(rejects_tools(&http(400, "This model does not support Tools")));
        assert!(!rejects_tools(&http(400, "context length exceeded")));
        assert!(!rejects_tools(&http(200, "tools")));
        assert!(!rejects_tools(&LlmError::Stream("tool".into())));
    }

    #[test]
    fn request_adds_tool_prompt_and_strips_images() {
        let mut plan = with(ToolSupport::InContent, ToolMode::Auto).plan(&config());
        let spec = ToolSpec { name: "echo".into(), description: "d".into(), parameters: serde_json::json!({"type": "object"}) };
        let msgs = vec![ChatMessage::system("sys"), ChatMessage::user_with_images("see", vec!["data:image/png;base64,AA".into()])];
        let out = plan.request(&msgs, std::slice::from_ref(&spec));
        let sys = out[0].content.as_deref().unwrap();
        assert!(sys.starts_with("sys\n\n# Tools") && sys.contains("\"name\":\"echo\""));
        assert!(out[1].images.is_empty() && out[1].content.as_deref().unwrap().contains("cannot see images"));
        // With vision (or unknown vision), images go through untouched.
        plan.vision = None;
        plan.prompt_tools = false;
        assert_eq!(plan.request(&msgs, &[spec]), msgs);
    }

    #[test]
    fn info_summarises_probe() {
        let mut e = Endpoints::default();
        let caps =
            Capabilities { server: ServerKind::LlamaCpp, context_length: Some(262_144), vision: Some(true), tools: ToolSupport::Native, ..Default::default() };
        e.handle(Event::Probed { key: key(&config()), caps: Box::new(caps) });
        let info = e.info(&config());
        assert_eq!(info.summary(), "llama.cpp · 262k ctx · native tools · vision");
        assert_eq!(e.plan(&config()).reserve, 16_384);
        assert_eq!(e.slots(&config()), None);
        let caps = Capabilities { slots: Some(4), ..Default::default() };
        e.handle(Event::Probed { key: key(&config()), caps: Box::new(caps) });
        assert_eq!(e.slots(&config()), Some(4));
    }
}
