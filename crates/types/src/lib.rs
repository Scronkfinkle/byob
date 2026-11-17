//! Shared serde types used across every byob layer.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

pub mod chart;
pub mod config;
pub mod layout;
pub mod profile;

use serde::{Deserialize, Serialize};

/// Who authored a chat message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A single chat message as sent to / received from the inference endpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

/// Which wire protocol an endpoint speaks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApiKind {
    /// OpenAI chat completions (`POST {base}/chat/completions`): llama-server, vLLM,
    /// Ollama, LM Studio, OpenAI, OpenRouter, ...
    #[default]
    #[serde(rename = "openai")]
    OpenAi,
    /// Anthropic's Messages API (`POST {base}/messages`), for Claude.
    Anthropic,
}

/// Where and how to reach the model. Replaces hob's local llama.cpp setup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InferenceConfig {
    /// Base URL of the API, e.g. `http://127.0.0.1:8080/v1`.
    pub base_url: String,
    /// Model name to request; many single-model servers ignore it.
    pub model: String,
    /// Optional API key (a bearer token, or Anthropic's `x-api-key`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// The API the endpoint speaks; `None` decides from the URL ([`InferenceConfig::api_kind`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<ApiKind>,
    /// Request options of the profile this endpoint came from (temperature, `max_tokens`,
    /// effort, extra body fields): byob-llm adds them to every chat request made with it
    /// (see `byob_llm::params`). Never to the probe's own requests.
    #[serde(default, skip_serializing_if = "profile::Params::is_empty")]
    pub params: profile::Params,
    /// Context window to budget against, beating what the probe found (a profile's
    /// [`profile::Profile::context`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<u32>,
}

impl Default for InferenceConfig {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:8080/v1".into(),
            model: "default".into(),
            api_key: None,
            api: None,
            params: profile::Params::default(),
            context: None,
        }
    }
}

impl InferenceConfig {
    /// The API to speak: the configured one, else Anthropic for `api.anthropic.com`, else OpenAI.
    pub fn api_kind(&self) -> ApiKind {
        self.api.unwrap_or_else(|| if url_host(&self.base_url).eq_ignore_ascii_case("api.anthropic.com") { ApiKind::Anthropic } else { ApiKind::OpenAi })
    }

    /// The same endpoint (URL, model, key, API), whatever the request options.
    pub fn same_endpoint(&self, other: &Self) -> bool {
        self.base_url == other.base_url && self.model == other.model && self.api_key == other.api_key && self.api == other.api
    }
}

/// The host part of `url` (no scheme, user info, port or path); empty if there is none.
fn url_host(url: &str) -> &str {
    let rest = url.trim().split_once("://").map_or("", |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    host.split(':').next().unwrap_or_default().trim_end_matches('.')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&Role::Assistant).unwrap(), "\"assistant\"");
    }

    #[test]
    fn api_kind_is_set_or_follows_the_host() {
        let at = |url: &str| InferenceConfig { base_url: url.into(), ..Default::default() }.api_kind();
        assert_eq!(at("https://api.anthropic.com/v1"), ApiKind::Anthropic);
        assert_eq!(at("https://API.Anthropic.com:443/v1/"), ApiKind::Anthropic);
        assert_eq!(at("http://127.0.0.1:8080/v1"), ApiKind::OpenAi);
        assert_eq!(at("https://api.anthropic.com.evil.example/v1"), ApiKind::OpenAi);
        assert_eq!(at("https://proxy.example/?u=api.anthropic.com"), ApiKind::OpenAi);
        assert_eq!(at("not a url"), ApiKind::OpenAi);
        let set = InferenceConfig { base_url: "http://proxy:8080/v1".into(), api: Some(ApiKind::Anthropic), ..Default::default() };
        assert_eq!(set.api_kind(), ApiKind::Anthropic);
        let forced = InferenceConfig { base_url: "https://api.anthropic.com/v1".into(), api: Some(ApiKind::OpenAi), ..Default::default() };
        assert_eq!(forced.api_kind(), ApiKind::OpenAi);
        // Existing configs (no `api`) read and write as before.
        let old: InferenceConfig = serde_json::from_str(r#"{"base_url":"http://h/v1","model":"m"}"#).unwrap();
        assert_eq!(old.api, None);
        assert_eq!(serde_json::to_string(&old).unwrap(), r#"{"base_url":"http://h/v1","model":"m"}"#);
        assert_eq!(serde_json::to_string(&ApiKind::OpenAi).unwrap(), r#""openai""#);
        assert_eq!(serde_json::from_str::<ApiKind>(r#""anthropic""#).unwrap(), ApiKind::Anthropic);
    }
}
