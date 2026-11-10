//! Shared serde types used across every byob layer.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

pub mod chart;

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

/// Where and how to reach the model. Replaces hob's local llama.cpp setup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InferenceConfig {
    /// Base URL of an OpenAI-compatible API, e.g. `http://127.0.0.1:8080/v1`.
    pub base_url: String,
    /// Model name to request; many single-model servers ignore it.
    pub model: String,
    /// Optional bearer token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

impl Default for InferenceConfig {
    fn default() -> Self {
        Self { base_url: "http://127.0.0.1:8080/v1".into(), model: "default".into(), api_key: None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&Role::Assistant).unwrap(), "\"assistant\"");
    }
}
