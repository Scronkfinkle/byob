//! The wire protocol between yah clients (desktop window, browser tab) and the engine.
//!
//! Every client, including the in-process desktop window, talks to the engine only through
//! these messages, so the desktop and web UIs can never drift apart. Messages travel as JSON
//! text frames over a WebSocket, or through channels when in-process.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

use serde::{Deserialize, Serialize};

/// Bumped on any incompatible change to [`ClientMsg`] / [`ServerMsg`].
pub const PROTOCOL_VERSION: u32 = 1;

/// Hard cap on a single incoming frame, so a hostile peer can't make us allocate unboundedly.
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Client -> engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// First message on every connection.
    Hello { protocol: u32, client: String },
    /// Point the engine at a different inference endpoint.
    SetInference { base_url: String, model: String, api_key: Option<String> },
    /// Send a user message and start a turn.
    SendMessage { text: String },
    /// Stop the running turn.
    Cancel,
}

/// Engine -> client(s).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// Full engine state; sent on connect and whenever it changes.
    State(EngineState),
    /// A human-readable message for the user (errors, warnings).
    Notice { level: NoticeLevel, text: String },
    /// The whole conversation; sent on connect.
    Transcript { entries: Vec<Entry> },
    /// A new entry was appended.
    EntryAdded(Entry),
    /// Streamed text appended to an entry.
    EntryDelta { id: u64, content: String, reasoning: String },
    /// An entry finished streaming.
    EntryFinished { id: u64, error: Option<String>, stats: Option<TurnStats> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryRole {
    User,
    Assistant,
}

/// One message in the conversation as shown to users.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: u64,
    pub role: EntryRole,
    pub content: String,
    pub reasoning: String,
    /// False while the reply is still streaming.
    pub done: bool,
    pub error: Option<String>,
    pub stats: Option<TurnStats>,
}

impl Entry {
    /// Apply a [`ServerMsg::EntryDelta`].
    pub fn append(&mut self, content: &str, reasoning: &str) {
        self.content.push_str(content);
        self.reasoning.push_str(reasoning);
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct TurnStats {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    Info,
    Error,
}

/// What clients may know about the engine. Secrets (API keys) never leave the engine.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EngineState {
    pub version: String,
    pub inference_base_url: String,
    pub inference_model: String,
    pub has_api_key: bool,
    /// Resolved `chat/completions` URL, if the base URL is valid.
    pub endpoint: Option<String>,
    /// A turn is running.
    pub busy: bool,
}

impl ClientMsg {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Parse an untrusted frame.
    pub fn from_json(text: &str) -> Result<Self, String> {
        if text.len() > MAX_FRAME_BYTES {
            return Err(format!("frame too large ({} bytes)", text.len()));
        }
        serde_json::from_str(text).map_err(|e| e.to_string())
    }
}

impl ServerMsg {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Parse an untrusted frame.
    pub fn from_json(text: &str) -> Result<Self, String> {
        if text.len() > MAX_FRAME_BYTES {
            return Err(format!("frame too large ({} bytes)", text.len()));
        }
        serde_json::from_str(text).map_err(|e| e.to_string())
    }
}

/// A client's link to an engine, in-process or over a WebSocket. Never blocks.
pub trait Connection {
    fn send(&mut self, msg: ClientMsg);
    /// Next pending message, if any. Called every UI frame.
    fn try_recv(&mut self) -> Option<ServerMsg>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_msg_round_trips() {
        let m = ClientMsg::Hello { protocol: PROTOCOL_VERSION, client: "test".into() };
        assert_eq!(ClientMsg::from_json(&m.to_json()).unwrap(), m);
    }

    #[test]
    fn wire_format_is_tagged() {
        let json = ServerMsg::Notice { level: NoticeLevel::Info, text: "hi".into() }.to_json();
        assert_eq!(json, r#"{"type":"notice","level":"info","text":"hi"}"#);
    }

    #[test]
    fn rejects_garbage_and_oversized_frames() {
        assert!(ClientMsg::from_json("{not json").is_err());
        assert!(ClientMsg::from_json(&" ".repeat(MAX_FRAME_BYTES + 1)).is_err());
    }

    #[test]
    fn state_never_carries_api_key() {
        let json = serde_json::to_string(&EngineState::default()).unwrap();
        assert!(!json.contains("\"api_key\""));
    }
}
