//! Message queueing: follow-ups typed while a reply runs.
//!
//! While a chat is busy, [`crate::ClientMsg::SendMessage`] appends to that chat's queue in the
//! engine instead of being refused. When the running turn ends normally the engine starts the
//! first queued message as the next turn, and so on. A turn that is stopped or fails pauses the
//! queue ([`ChatQueue::paused`]); it stays, untouched, until a client resumes it
//! ([`crate::ClientMsg::ResumeQueue`]), sends one item now ([`crate::ClientMsg::SendQueuedNow`])
//! or retries the failed reply. A turn waiting for an approval simply keeps the queue waiting.
//!
//! The queue lives in the engine (so every client of the chat sees the same one, and it
//! survives a closed window or tab) but only in memory: it is gone after an engine restart.
//! Queued messages are not part of the transcript until they are sent.

use serde::{Deserialize, Serialize};

use crate::{ClientMsg, MediaRef};

/// Most messages queued in one chat.
pub const MAX_QUEUED: usize = 20;
/// Most bytes of text in one queued message.
pub const MAX_QUEUED_TEXT_BYTES: usize = 256 * 1024;
/// Most bytes of text in one chat's whole queue.
pub const MAX_QUEUE_BYTES: usize = 1024 * 1024;

/// A message waiting to be sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedMessage {
    /// Unique within the engine run.
    pub id: u64,
    /// The whole text (clients show a preview and edit the whole).
    pub text: String,
    /// Files it carries, uploaded before it was queued.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<MediaRef>,
}

impl QueuedMessage {
    /// The text on one line (whitespace runs, newlines included, become one space),
    /// shortened to `max` characters, for a compact row.
    pub fn preview(&self, max: usize) -> String {
        let mut out = String::new();
        for (i, word) in self.text.split_whitespace().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            out.push_str(word);
            if out.chars().count() > max {
                break;
            }
        }
        if out.chars().count() > max {
            out = out.chars().take(max).collect();
            out.push('…');
        }
        out
    }
}

/// Why a queue stopped moving on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueuePause {
    /// The user stopped the reply.
    Stopped,
    /// The reply failed (or the next one couldn't start).
    Failed,
}

/// The queue of the chat a client looks at (`chat: None` for a new chat: always empty).
/// Sent after [`crate::ServerMsg::Transcript`] and whenever it changes; replaces the
/// client's copy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatQueue {
    pub chat: Option<String>,
    /// In the order they will be sent.
    pub items: Vec<QueuedMessage>,
    /// The queue waits for the user instead of sending the next message when the chat is free.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused: Option<QueuePause>,
}

/// Size checks of the queue messages (see [`ClientMsg::check_sizes`]).
pub(crate) fn check_sizes(msg: &ClientMsg) -> Result<(), String> {
    match msg {
        ClientMsg::EditQueued { text, attachments, .. } => {
            if text.len() > MAX_QUEUED_TEXT_BYTES {
                return Err(format!("a queued message can be at most {} KB", MAX_QUEUED_TEXT_BYTES / 1024));
            }
            if attachments.len() > crate::media::MAX_ATTACHMENTS {
                return Err(format!("at most {} files per message", crate::media::MAX_ATTACHMENTS));
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ServerMsg;

    #[test]
    fn queue_messages_round_trip() {
        let q = ChatQueue {
            chat: Some("c1".into()),
            items: vec![QueuedMessage { id: 3, text: "and then?".into(), attachments: vec![MediaRef::new("uploads/a.csv", 3)] }],
            paused: Some(QueuePause::Stopped),
        };
        let m = ServerMsg::Queue(q);
        assert_eq!(ServerMsg::from_json(&m.to_json()).unwrap(), m);
        for m in [
            ClientMsg::Unqueue { id: 3 },
            ClientMsg::EditQueued { id: 3, text: "x".into(), attachments: vec!["uploads/a.csv".into()] },
            ClientMsg::MoveQueued { id: 3, to: 0 },
            ClientMsg::SendQueuedNow { id: 3 },
            ClientMsg::ResumeQueue,
        ] {
            assert_eq!(ClientMsg::from_json(&m.to_json()).unwrap(), m);
        }
        assert_eq!(ClientMsg::ResumeQueue.to_json(), r#"{"type":"resume_queue"}"#);
        let empty = ServerMsg::Queue(ChatQueue::default()).to_json();
        assert_eq!(empty, r#"{"type":"queue","chat":null,"items":[]}"#);
    }

    #[test]
    fn edits_are_size_checked_and_previews_are_short() {
        let big = ClientMsg::EditQueued { id: 1, text: "x".repeat(MAX_QUEUED_TEXT_BYTES + 1), attachments: vec![] };
        assert!(big.check_sizes().is_err());
        let many = ClientMsg::EditQueued { id: 1, text: "x".into(), attachments: vec!["a".into(); crate::media::MAX_ATTACHMENTS + 1] };
        assert!(many.check_sizes().is_err());
        let q = |t: &str| QueuedMessage { id: 1, text: t.into(), attachments: vec![] };
        assert_eq!(q("\n  first line  \nsecond").preview(40), "first line second");
        assert_eq!(q("abcdef").preview(3), "abc…");
        assert_eq!(q("abc").preview(3), "abc");
    }
}
