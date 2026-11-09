//! Row types. These are the store's own serde types; the engine maps protocol/tool types to
//! and from them so the on-disk format doesn't change when the wire format does.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
pub use yah_types::Role;

/// Milliseconds since the Unix epoch (UTC).
pub type Millis = i64;

/// Row id of a message. Monotonic within a database, so it also orders a chat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MessageId(pub i64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chat {
    pub id: String,
    pub title: String,
    /// Folder the chat works in.
    pub workspace: PathBuf,
    pub created_at: Millis,
    /// Bumped by new messages (and finished replies); the list is ordered by it.
    pub updated_at: Millis,
    /// When it was pinned, if it is.
    pub pinned_at: Option<Millis>,
}

impl Chat {
    pub fn pinned(&self) -> bool {
        self.pinned_at.is_some()
    }

    /// Cursor that continues a listing after this chat.
    pub fn cursor(&self) -> ChatCursor {
        ChatCursor { updated_at: self.updated_at, id: self.id.clone() }
    }
}

/// Keyset position in the recent-first chat list (stable while chats are added).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatCursor {
    pub updated_at: Millis,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatPage {
    pub chats: Vec<Chat>,
    /// Pass back to get the next page; `None` when this was the last one.
    pub next: Option<ChatCursor>,
}

/// Token and timing figures for a finished reply. Unknown fields in stored JSON are ignored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MessageStats {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub elapsed_ms: u64,
}

/// What to insert. Assistant replies usually start empty with `done: false` and are then
/// filled by [`crate::Store::append_message_text`] / [`crate::Store::finish_message`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMessage {
    pub role: Role,
    pub content: String,
    pub reasoning: String,
    pub done: bool,
    /// Groups the undo journal entries this message's tool calls recorded.
    pub task_id: Option<String>,
}

impl NewMessage {
    /// A complete user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: Role::User, content: content.into(), reasoning: String::new(), done: true, task_id: None }
    }

    /// An empty assistant reply about to stream.
    pub fn assistant_streaming(task_id: Option<String>) -> Self {
        Self { role: Role::Assistant, content: String::new(), reasoning: String::new(), done: false, task_id }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoredMessage {
    pub id: MessageId,
    pub chat_id: String,
    pub role: Role,
    pub content: String,
    pub reasoning: String,
    /// False while streaming — or if yah stopped mid-reply (the engine may mark those failed).
    pub done: bool,
    pub error: Option<String>,
    pub stats: Option<MessageStats>,
    pub task_id: Option<String>,
    /// The user undid this message's file changes.
    pub undone: bool,
    pub created_at: Millis,
    /// Tool calls made while producing this message, by `seq`.
    pub steps: Vec<Step>,
}

/// What a tool call may do; mirrors the permission gate's levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepAccess {
    Read,
    Write,
    Network,
    Execute,
}

impl StepAccess {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Network => "network",
            Self::Execute => "execute",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "read" => Self::Read,
            "write" => Self::Write,
            "network" => Self::Network,
            "execute" => Self::Execute,
            _ => return None,
        })
    }
}

/// A file-system effect of a tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StepChange {
    Created { path: PathBuf },
    Modified { path: PathBuf },
    Deleted { path: PathBuf },
    Moved { from: PathBuf, to: PathBuf },
}

/// The start of a tool call, recorded before it runs.
#[derive(Debug, Clone, PartialEq)]
pub struct NewStep {
    pub tool: String,
    pub title: String,
    pub access: StepAccess,
    pub args: Value,
}

/// How a tool call ended.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StepResult {
    /// Truncated to [`crate::MAX_STEP_OUTPUT`] bytes when stored.
    pub output: String,
    pub failed: bool,
    pub changes: Vec<StepChange>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub message_id: MessageId,
    /// 1-based position within the message.
    pub seq: i64,
    pub tool: String,
    pub title: String,
    pub access: StepAccess,
    pub args: Value,
    /// `None` while the call is still running (or yah stopped during it).
    pub result: Option<StepResult>,
}

/// Who an always-allow grant applies to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum GrantScope {
    Global,
    Chat(String),
}

/// One reversible file-system change, as recorded by the undo journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UndoOp {
    Created {
        path: PathBuf,
    },
    /// `backup` holds the previous content.
    Replaced {
        path: PathBuf,
        backup: PathBuf,
    },
    Moved {
        from: PathBuf,
        to: PathBuf,
    },
    /// `backup` is `None` when the file was too big to copy first.
    Deleted {
        path: PathBuf,
        backup: Option<PathBuf>,
    },
}
