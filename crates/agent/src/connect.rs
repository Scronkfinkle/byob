//! The engine's link to its inference endpoint: a quick check whenever the endpoint is set,
//! connection tests for the settings form, looking for servers on this machine's usual ports,
//! and running a message again after its reply failed to reach the server.
//!
//! Everything that talks to a server runs on its own short-lived thread and reports back
//! through the engine inbox ([`Input::Link`]).

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use byob_llm::{Capabilities, Client, LlmError, ProbeOptions, ServerKind};
use byob_protocol::connect::{MAX_KEY_CHARS, MAX_MODEL_CHARS, MAX_URL_CHARS};
use byob_protocol::{Detected, EntryRole, InferenceTest, Link, PRESETS, Problem, ServerMsg};
use byob_store::Settings;
use byob_types::InferenceConfig;

use crate::{ClientId, Engine, Input, adapt, chats};

/// The check run whenever the endpoint changes (and at startup).
const CHECK_TIMEOUT: Duration = Duration::from_secs(5);
/// A connection test from the settings form.
const TEST_TIMEOUT: Duration = Duration::from_secs(8);
/// Looking for a local server: loopback answers at once or not at all.
const DETECT_TIMEOUT: Duration = Duration::from_millis(800);

/// Results of the threads started here.
pub enum Event {
    /// The check of endpoint `key` (see [`adapt::key`]) finished.
    Checked {
        key: String,
        link: Link,
    },
    Tested {
        client: ClientId,
        test: Box<InferenceTest>,
    },
    Detected(Vec<Detected>),
}

/// Run `work` on a named thread; its event (if the thread didn't crash) goes to the engine.
fn spawn(name: &str, inbox: &Sender<Input>, work: impl FnOnce() -> Event + Send + 'static) -> bool {
    let inbox = inbox.clone();
    let spawned = thread::Builder::new().name(name.into()).spawn(move || match catch_unwind(AssertUnwindSafe(work)) {
        Ok(event) => {
            let _ = inbox.send(Input::Link(event));
        }
        Err(_) => log::error!("the connection check crashed"),
    });
    match spawned {
        Ok(_) => true,
        Err(e) => {
            log::warn!("could not start a connection check: {e}");
            false
        }
    }
}

/// What a failed `GET /models` means for the user. A 404 there is a wrong path, whatever
/// the body says (it often echoes the path, which mentions "models").
fn problem_of(err: &LlmError) -> Option<Problem> {
    match err {
        LlmError::BadUrl(_) => Some(Problem::BadUrl),
        LlmError::Transport { .. } => Some(Problem::Unreachable),
        LlmError::Http { status: 404 | 405, .. } => Some(Problem::WrongPath),
        other => Problem::classify(&other.to_string()),
    }
}

/// The link after a check: anything that lists models (even none) is there.
pub fn link_of(result: &Result<Vec<String>, LlmError>) -> Link {
    match result {
        Ok(_) => Link::Ok,
        Err(e) => Link::Failed { problem: problem_of(e), detail: e.to_string() },
    }
}

/// Test an endpoint: is it there, does it take the key, does it have the model. Probes
/// briefly (no generation) to say what answered.
pub fn test(client: &Client) -> InferenceTest {
    let config = client.config();
    let base = InferenceTest { base_url: config.base_url.clone(), model: config.model.clone(), ..Default::default() };
    let models = match client.list_models_within(TEST_TIMEOUT) {
        Ok(models) => models,
        Err(e) => return InferenceTest { problem: problem_of(&e), detail: e.to_string(), ..base },
    };
    let caps = client.probe_with(ProbeOptions { budget: TEST_TIMEOUT, step_timeout: Duration::from_secs(3), test_tools: false, ..Default::default() });
    verdict(base, models, &caps)
}

/// Judge a reachable endpoint by the models it lists. llama-server answers whatever the
/// model is called, so only other servers must list it.
fn verdict(base: InferenceTest, models: Vec<String>, caps: &Capabilities) -> InferenceTest {
    let model = base.model.trim().to_owned();
    let lenient = caps.server == ServerKind::LlamaCpp;
    let missing = !lenient && !models.is_empty() && !models.contains(&model);
    if missing {
        let detail = if model.is_empty() {
            "choose one of the models it lists".to_owned()
        } else {
            format!("{model:?} isn't one of the {} model(s) it lists", models.len())
        };
        return InferenceTest { problem: Some(Problem::ModelNotFound), detail, models, ..base };
    }
    let mut parts = Vec::new();
    let server = adapt::server_name(caps.server);
    if !server.is_empty() {
        parts.push(server.to_owned());
    }
    if let Some(n) = caps.context_length {
        parts.push(if n >= 1000 { format!("{}k ctx", n / 1000) } else { format!("{n} ctx") });
    }
    if caps.vision == Some(true) {
        parts.push("vision".into());
    }
    match models.len() {
        0 => {}
        1 => parts.push("1 model".into()),
        n => parts.push(format!("{n} models")),
    }
    InferenceTest { ok: true, detail: parts.join(" · "), models, ..base }
}

/// Servers answering on the usual local ports (llama-server, Ollama, LM Studio, vLLM), in
/// [`PRESETS`] order. Only those that list a model count (a port can be anything).
pub fn detect() -> Vec<Detected> {
    thread::scope(|s| {
        let probes: Vec<_> = PRESETS
            .iter()
            .filter(|p| p.local)
            .map(|p| {
                s.spawn(move || {
                    let client = Client::new(InferenceConfig { base_url: p.base_url.into(), model: String::new(), api_key: None }).ok()?;
                    let models = client.list_models_within(DETECT_TIMEOUT).ok().filter(|m| !m.is_empty())?;
                    Some(Detected { preset: p.id.into(), base_url: p.base_url.into(), models })
                })
            })
            .collect();
        probes.into_iter().filter_map(|h| h.join().ok().flatten()).collect()
    })
}

/// Scheme, host and port of a URL, lowercased ("http://127.0.0.1:8080").
fn origin(url: &str) -> String {
    let u = url.trim().to_ascii_lowercase();
    let (scheme, rest) = u.split_once("://").unwrap_or(("", u.as_str()));
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    format!("{scheme}://{host}")
}

/// A client-supplied endpoint, refused if absurdly long.
pub fn check_lengths(base_url: &str, model: &str, api_key: Option<&str>) -> Result<(), String> {
    if base_url.chars().count() > MAX_URL_CHARS || model.chars().count() > MAX_MODEL_CHARS || api_key.is_some_and(|k| k.chars().count() > MAX_KEY_CHARS) {
        return Err("the URL, model name or API key is too long".into());
    }
    Ok(())
}

impl Engine {
    /// The key to use with `base_url`: a new one if given (`""` means none), else the stored
    /// one, but only for the same server. A key never follows the URL to another host.
    pub(crate) fn key_for(&self, base_url: &str, api_key: Option<String>) -> Option<String> {
        match api_key {
            Some(k) => Some(k.trim().to_owned()).filter(|k| !k.is_empty()),
            None => self.config.api_key.clone().filter(|_| origin(base_url) == origin(&self.config.base_url)),
        }
    }

    /// Check the current endpoint in the background (if checks are on).
    pub(crate) fn check_link(&mut self) {
        let client = match &self.llm {
            Ok(c) => c.clone(),
            Err(e) => {
                let link = Link::Failed { problem: Some(Problem::BadUrl), detail: e.clone() };
                self.set_link(link);
                return;
            }
        };
        if !self.checks {
            return;
        }
        let key = adapt::key(client.config());
        if spawn("byob-check", &self.inbox, move || Event::Checked { key, link: link_of(&client.list_models_within(CHECK_TIMEOUT)) }) {
            self.set_link(Link::Checking);
        }
    }

    pub(crate) fn handle_connect(&mut self, event: Event) {
        match event {
            Event::Checked { key, link } => {
                // A check of an endpoint we've since moved away from says nothing.
                if key == adapt::key(&self.config) {
                    self.set_link(link);
                }
            }
            Event::Tested { client, test } => {
                self.testing.remove(&client);
                self.send_to(client, ServerMsg::InferenceTested(*test));
            }
            Event::Detected(found) => {
                log::info!("local inference servers: {:?}", found.iter().map(|d| d.base_url.as_str()).collect::<Vec<_>>());
                self.detected = found;
                self.detecting = false;
                self.broadcast_state();
            }
        }
    }

    /// Record how the current endpoint is doing; a working one is remembered as onboarded.
    pub(crate) fn set_link(&mut self, link: Link) {
        if link.is_ok() {
            self.remember_good();
        }
        if self.link != link {
            self.link = link;
            self.broadcast_state();
        }
    }

    /// The current endpoint worked: save it as the last good one (once per URL).
    fn remember_good(&mut self) {
        let url = self.config.base_url.clone();
        if self.last_good.as_deref() == Some(url.as_str()) {
            return;
        }
        self.last_good = Some(url.clone());
        let newly = !self.onboarded;
        self.onboarded = true;
        if let Some(paths) = &self.paths
            && let Err(e) = Settings::update(&paths.settings(), |s| s.inference.last_good = Some(url))
        {
            log::warn!("couldn't save the working endpoint: {e}");
        }
        if newly {
            self.broadcast_state();
        }
    }

    /// A reply ended: a success proves the link, a connection error breaks it.
    pub(crate) fn turn_outcome(&mut self, error: Option<&str>) {
        match error {
            None => self.set_link(Link::Ok),
            Some(e) => {
                if let Some(problem) = Problem::classify(e) {
                    self.set_link(Link::Failed { problem: Some(problem), detail: e.to_owned() });
                }
            }
        }
    }

    /// `ClientMsg::TestInference`: one test per client at a time.
    pub(crate) fn test_inference(&mut self, id: ClientId, base_url: String, model: String, api_key: Option<String>) {
        let answer = |problem, detail: String| InferenceTest { base_url: base_url.clone(), model: model.clone(), ok: false, problem, detail, models: vec![] };
        if let Err(e) = check_lengths(&base_url, &model, api_key.as_deref()) {
            self.send_to(id, ServerMsg::InferenceTested(answer(None, e)));
            return;
        }
        if self.testing.contains(&id) {
            self.send_to(id, ServerMsg::InferenceTested(answer(None, "a test is already running".into())));
            return;
        }
        let config = InferenceConfig { api_key: self.key_for(&base_url, api_key), base_url: base_url.clone(), model: model.clone() };
        let client = match Client::new(config) {
            Ok(c) => c,
            Err(e) => {
                let test = answer(Some(Problem::BadUrl), e.to_string());
                self.send_to(id, ServerMsg::InferenceTested(test));
                return;
            }
        };
        if spawn("byob-test", &self.inbox, move || Event::Tested { client: id, test: Box::new(test(&client)) }) {
            self.testing.insert(id);
        }
    }

    /// Look for local servers (`ClientMsg::DetectEndpoints`, and at startup until onboarded).
    pub(crate) fn detect_endpoints(&mut self) {
        if self.detecting {
            return;
        }
        if spawn("byob-detect", &self.inbox, || Event::Detected(detect())) {
            self.detecting = true;
            self.broadcast_state();
        }
    }

    /// `ClientMsg::Retry`: drop a reply that failed before it produced anything, and send
    /// its message again.
    pub(crate) fn retry(&mut self, client: ClientId) {
        let Some(chat) = self.client_chat(client) else { return };
        if self.chat_busy(&chat) {
            self.notice(client, "a reply is still running in this chat".into());
            return;
        }
        let live = match self.live(&chat) {
            Ok(l) => l,
            Err(e) => {
                self.notice(client, e);
                return;
            }
        };
        let n = live.entries.len();
        let pair = n.checked_sub(2).and_then(|i| live.entries.get(i)).zip(live.entries.last());
        let Some((user, _)) = pair.filter(|(u, r)| {
            u.role == EntryRole::User && r.role == EntryRole::Assistant && r.done && r.error.is_some() && r.content.is_empty() && r.steps.is_empty()
        }) else {
            self.notice(client, "only a reply that failed before it started can be retried".into());
            return;
        };
        let (text, attached, from) = (user.content.clone(), user.attachments.clone(), user.id);
        if let Err(e) = self.store.delete_messages_from(&chat, chats::message_id(from)) {
            self.notice(client, format!("couldn't remove the failed reply: {e}"));
            return;
        }
        let entries = match self.live.get_mut(&chat) {
            Some(live) => {
                live.entries.truncate(n.saturating_sub(2));
                live.entries.clone()
            }
            None => Vec::new(),
        };
        self.send_viewers(&chat, ServerMsg::Transcript { entries });
        self.begin_turn(client, text, attached);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_compare_scheme_host_and_port() {
        assert_eq!(origin("HTTP://Example.com:8080/v1/"), "http://example.com:8080");
        assert_eq!(origin("https://api.openai.com/v1"), origin("https://api.openai.com/other"));
        assert_ne!(origin("https://api.openai.com/v1"), origin("https://openrouter.ai/api/v1"));
        assert_ne!(origin("http://127.0.0.1:8080/v1"), origin("http://127.0.0.1:11434/v1"));
    }

    #[test]
    fn verdict_wants_a_listed_model_except_from_llama_server() {
        let base = InferenceTest { base_url: "u".into(), model: "qwen3".into(), ..Default::default() };
        let ollama = Capabilities { server: ServerKind::Ollama, context_length: Some(32_768), ..Default::default() };
        let t = verdict(base.clone(), vec!["llama3".into(), "gemma".into()], &ollama);
        assert!(!t.ok);
        assert_eq!(t.problem, Some(Problem::ModelNotFound));
        assert_eq!(t.models, ["llama3", "gemma"]);
        let t = verdict(base.clone(), vec!["qwen3".into()], &ollama);
        assert!(t.ok);
        assert_eq!(t.detail, "Ollama · 32k ctx · 1 model");
        // llama-server ignores the name; servers that list nothing can't be judged.
        let llama = Capabilities { server: ServerKind::LlamaCpp, vision: Some(true), ..Default::default() };
        assert!(verdict(base.clone(), vec!["model.gguf".into()], &llama).ok);
        assert!(verdict(base.clone(), vec![], &Capabilities::default()).ok);
        let blank = InferenceTest { model: " ".into(), ..base };
        assert_eq!(verdict(blank, vec!["a".into()], &ollama).detail, "choose one of the models it lists");
    }

    #[test]
    fn lengths_are_capped() {
        assert!(check_lengths("http://x/v1", "m", Some("k")).is_ok());
        assert!(check_lengths(&"x".repeat(MAX_URL_CHARS + 1), "m", None).is_err());
        assert!(check_lengths("u", &"m".repeat(MAX_MODEL_CHARS + 1), None).is_err());
        assert!(check_lengths("u", "m", Some(&"k".repeat(MAX_KEY_CHARS + 1))).is_err());
    }

    #[test]
    fn check_results_become_links() {
        assert_eq!(link_of(&Ok(vec![])), Link::Ok);
        let http = |status| Err(LlmError::Http { status, message: "/v1/models not found".into() });
        assert!(matches!(link_of(&http(404)), Link::Failed { problem: Some(Problem::WrongPath), .. }));
        assert!(matches!(link_of(&http(401)), Link::Failed { problem: Some(Problem::Unauthorized), .. }));
        let unreachable =
            Client::new(InferenceConfig { base_url: "http://127.0.0.1:9/v1".into(), ..Default::default() }).unwrap().list_models_within(CHECK_TIMEOUT);
        assert!(matches!(link_of(&unreachable), Link::Failed { problem: Some(Problem::Unreachable), .. }));
    }
}
