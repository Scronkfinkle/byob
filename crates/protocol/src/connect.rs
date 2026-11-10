//! Connecting byob to an inference endpoint: the state of the link, why it fails (in words a
//! user can act on), a connection test, local servers found at startup, and presets for
//! common setups.

use serde::{Deserialize, Serialize};

/// Longest base URL a client may send.
pub const MAX_URL_CHARS: usize = 2048;
/// Longest model name a client may send.
pub const MAX_MODEL_CHARS: usize = 256;
/// Longest API key a client may send.
pub const MAX_KEY_CHARS: usize = 4096;

/// What went wrong talking to the inference endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Problem {
    /// The URL isn't an http(s) URL.
    BadUrl,
    /// Nothing answered: connection refused, unknown host, timeout.
    Unreachable,
    /// HTTP 401/403: missing or wrong API key.
    Unauthorized,
    /// HTTP 404 for the API itself: usually a base URL without (or with an extra) `/v1`.
    WrongPath,
    /// The server doesn't have the model that was asked for.
    ModelNotFound,
    /// HTTP 402/429: rate limit or out of credits.
    RateLimited,
    /// HTTP 5xx: the server is there but failing (often still loading the model).
    ServerError,
}

impl Problem {
    /// Recognise a connection problem in an error message from the engine (the text of
    /// `byob_llm::LlmError`, which entries and test results carry). `None` for anything
    /// else (tool loops, cancelled turns, …).
    pub fn classify(error: &str) -> Option<Self> {
        let e = error.trim();
        if e.starts_with("invalid inference URL") || e.starts_with("inference URL is invalid") {
            return Some(Self::BadUrl);
        }
        if e.starts_with("could not reach ") {
            return Some(Self::Unreachable);
        }
        let rest = e.strip_prefix("server returned HTTP ")?;
        let (code, message) = rest.split_once(':').unwrap_or((rest, ""));
        let status: u16 = code.trim().parse().ok()?;
        let m = message.to_ascii_lowercase();
        let about_model = m.contains("model")
            && ["not found", "does not exist", "unknown", "not exist", "no such", "invalid model", "not available", "pull"].iter().any(|w| m.contains(w));
        Some(match status {
            401 | 403 => Self::Unauthorized,
            404 if m.contains("model") => Self::ModelNotFound,
            400 | 422 if about_model => Self::ModelNotFound,
            404 | 405 => Self::WrongPath,
            402 | 429 => Self::RateLimited,
            500..=599 => Self::ServerError,
            _ => return None,
        })
    }

    /// A short headline, e.g. "Can't reach the model server".
    pub fn title(self) -> &'static str {
        match self {
            Self::BadUrl => "That isn't a valid server URL",
            Self::Unreachable => "Can't reach the model server",
            Self::Unauthorized => "The server refused the API key",
            Self::WrongPath => "The server is there, but not at that path",
            Self::ModelNotFound => "The server doesn't have that model",
            Self::RateLimited => "Rate-limited or out of credits",
            Self::ServerError => "The model server reported an error",
        }
    }

    /// What to do about it, in a sentence or two.
    pub fn advice(self) -> &'static str {
        match self {
            Self::BadUrl => "Use the server's OpenAI-compatible base URL, like http://127.0.0.1:8080/v1.",
            Self::Unreachable => "Nothing answered at that address. Start your server (llama-server, Ollama, LM Studio, vLLM…) or check the URL and port.",
            Self::Unauthorized => "Hosted APIs need a key. Enter or replace the API key in the connection settings.",
            Self::WrongPath => "OpenAI-compatible APIs usually end in /v1. Check the base URL.",
            Self::ModelNotFound => "Pick one of the models the server lists, or load the model on the server first.",
            Self::RateLimited => "The provider is limiting requests or your account needs credits. Wait a moment, or check your plan.",
            Self::ServerError => "The server may still be loading the model. Retry in a moment, or check the server's log.",
        }
    }
}

/// The engine's view of its current endpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Link {
    /// Not checked (yet).
    #[default]
    Unknown,
    /// A check is running.
    Checking,
    /// The endpoint answered (a check or a reply worked).
    Ok,
    /// The last check or reply failed to reach it. `detail` is the raw error.
    Failed {
        #[serde(default)]
        problem: Option<Problem>,
        #[serde(default)]
        detail: String,
    },
}

impl Link {
    pub fn is_ok(&self) -> bool {
        *self == Self::Ok
    }

    /// The failure, as an error from a check or a reply.
    pub fn failed(detail: impl Into<String>) -> Self {
        let detail = detail.into();
        Self::Failed { problem: Problem::classify(&detail), detail }
    }
}

/// A local inference server the engine found listening (see [`PRESETS`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Detected {
    /// The [`Preset::id`] whose port answered.
    pub preset: String,
    pub base_url: String,
    /// Model ids it lists.
    #[serde(default)]
    pub models: Vec<String>,
}

/// Answer to `ClientMsg::TestInference` (only to the client that asked).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InferenceTest {
    /// What was tested, so a stale answer can be told apart.
    pub base_url: String,
    pub model: String,
    /// The endpoint answered and has the model.
    pub ok: bool,
    #[serde(default)]
    pub problem: Option<Problem>,
    /// The raw error, or a short summary of what answered ("llama.cpp · 262k ctx").
    #[serde(default)]
    pub detail: String,
    /// Model ids the server lists.
    #[serde(default)]
    pub models: Vec<String>,
}

/// A common setup to fill the connection form from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    pub id: &'static str,
    pub name: &'static str,
    pub base_url: &'static str,
    /// A model name to start with when the server lists none.
    pub model: &'static str,
    /// Runs on your machine (so the engine looks for it at startup).
    pub local: bool,
    /// Needs an API key.
    pub needs_key: bool,
    pub hint: &'static str,
}

/// Common OpenAI-compatible servers, local ones first.
pub const PRESETS: &[Preset] = &[
    Preset {
        id: "llama-server",
        name: "llama-server",
        base_url: "http://127.0.0.1:8080/v1",
        model: "default",
        local: true,
        needs_key: false,
        hint: "llama.cpp's server: llama-server -m model.gguf --jinja",
    },
    Preset {
        id: "ollama",
        name: "Ollama",
        base_url: "http://127.0.0.1:11434/v1",
        model: "",
        local: true,
        needs_key: false,
        hint: "ollama serve, then ollama pull a model",
    },
    Preset {
        id: "lm-studio",
        name: "LM Studio",
        base_url: "http://127.0.0.1:1234/v1",
        model: "",
        local: true,
        needs_key: false,
        hint: "Start the server in LM Studio's Developer tab",
    },
    Preset {
        id: "vllm",
        name: "vLLM",
        base_url: "http://127.0.0.1:8000/v1",
        model: "",
        local: true,
        needs_key: false,
        hint: "vllm serve MODEL --enable-auto-tool-choice",
    },
    Preset {
        id: "openai",
        name: "OpenAI",
        base_url: "https://api.openai.com/v1",
        model: "gpt-4.1-mini",
        local: false,
        needs_key: true,
        hint: "Hosted: needs an API key",
    },
    Preset {
        id: "openrouter",
        name: "OpenRouter",
        base_url: "https://openrouter.ai/api/v1",
        model: "",
        local: false,
        needs_key: true,
        hint: "Hosted, many models: needs an API key",
    },
];

impl Preset {
    pub fn by_id(id: &str) -> Option<&'static Preset> {
        PRESETS.iter().find(|p| p.id == id)
    }

    /// The preset whose URL `base_url` is (ignoring a trailing slash and `localhost` vs `127.0.0.1`).
    pub fn matching(base_url: &str) -> Option<&'static Preset> {
        let norm = |u: &str| u.trim().trim_end_matches('/').replace("://localhost:", "://127.0.0.1:").to_ascii_lowercase();
        let u = norm(base_url);
        PRESETS.iter().find(|p| norm(p.base_url) == u)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_llm_errors() {
        let c = Problem::classify;
        assert_eq!(c("could not reach http://127.0.0.1:9/v1/chat/completions: io: Connection refused"), Some(Problem::Unreachable));
        assert_eq!(c("invalid inference URL: relative URL without a base"), Some(Problem::BadUrl));
        assert_eq!(c("inference URL is invalid: x"), Some(Problem::BadUrl));
        assert_eq!(c("server returned HTTP 401: Incorrect API key provided"), Some(Problem::Unauthorized));
        assert_eq!(c("server returned HTTP 403: forbidden"), Some(Problem::Unauthorized));
        assert_eq!(c("server returned HTTP 404: model \"qwen3\" not found, try pulling it first"), Some(Problem::ModelNotFound));
        assert_eq!(c("server returned HTTP 400: The model `gpt-9` does not exist"), Some(Problem::ModelNotFound));
        assert_eq!(c("server returned HTTP 404: File Not Found"), Some(Problem::WrongPath));
        assert_eq!(c("server returned HTTP 429: slow down"), Some(Problem::RateLimited));
        assert_eq!(c("server returned HTTP 503: Loading model"), Some(Problem::ServerError));
        // Tool and context errors are not connection problems.
        assert_eq!(c("server returned HTTP 400: context length exceeded"), None);
        assert_eq!(c("stopped"), None);
        assert_eq!(c("server returned HTTP abc"), None);
        assert_eq!(c(""), None);
    }

    #[test]
    fn every_problem_has_words() {
        for p in [
            Problem::BadUrl,
            Problem::Unreachable,
            Problem::Unauthorized,
            Problem::WrongPath,
            Problem::ModelNotFound,
            Problem::RateLimited,
            Problem::ServerError,
        ] {
            assert!(!p.title().is_empty() && p.advice().ends_with('.'));
        }
    }

    #[test]
    fn link_round_trips_and_failed_classifies() {
        let l = Link::failed("could not reach http://x/v1/models: timeout");
        assert_eq!(l, Link::Failed { problem: Some(Problem::Unreachable), detail: "could not reach http://x/v1/models: timeout".into() });
        for l in [Link::Unknown, Link::Checking, Link::Ok, l] {
            assert_eq!(serde_json::from_str::<Link>(&serde_json::to_string(&l).unwrap()).unwrap(), l);
        }
        assert_eq!(serde_json::to_string(&Link::Ok).unwrap(), r#"{"state":"ok"}"#);
    }

    #[test]
    fn presets_match_urls() {
        assert_eq!(Preset::matching("http://localhost:11434/v1/").map(|p| p.id), Some("ollama"));
        assert_eq!(Preset::matching("http://127.0.0.1:8080/v1").map(|p| p.id), Some("llama-server"));
        assert_eq!(Preset::matching("http://silicon:9931/v1"), None);
        assert!(PRESETS.iter().all(|p| p.base_url.ends_with("/v1") && (p.local != p.needs_key)));
        assert_eq!(Preset::by_id("openai").map(|p| p.needs_key), Some(true));
    }
}
