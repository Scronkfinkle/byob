//! The wire protocol between yah clients (desktop window, browser tab) and the engine.
//!
//! Every client, including the in-process desktop window, talks to the engine only through
//! these messages, so the desktop and web UIs can never drift apart. Messages travel as JSON
//! text frames over a WebSocket, or through channels when in-process.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

use serde::{Deserialize, Serialize};

/// Bumped on any incompatible change to [`ClientMsg`] / [`ServerMsg`].
pub const PROTOCOL_VERSION: u32 = 3;

/// Hard cap on a single incoming frame, so a hostile peer can't make us allocate unboundedly.
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Client -> engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// First message on every connection.
    Hello {
        protocol: u32,
        client: String,
    },
    /// Point the engine at a different inference endpoint.
    SetInference {
        base_url: String,
        model: String,
        api_key: Option<String>,
    },
    /// Send a user message and start a turn.
    SendMessage {
        text: String,
    },
    /// Stop the running turn. Pending approvals are denied.
    Cancel,
    /// Answer a pending approval (a [`Step`] with status [`StepStatus::Waiting`]). Any
    /// client may answer; the first answer wins and later ones are ignored.
    Approve {
        step: u64,
        answer: ApprovalAnswer,
    },
    /// Reverse the file changes of assistant entry `entry` (see [`Entry::undoable`]). The
    /// engine refuses, changing nothing, if any of those files changed since.
    Undo {
        entry: u64,
    },
    /// Ask for a page of the chat list (answered with [`ServerMsg::Chats`]). `after` is the
    /// previous page's `next`; `None` asks for the first page (pinned chats included).
    ListChats {
        after: Option<ChatCursor>,
        limit: u32,
    },
    /// Start a new, empty chat in this client. The chat is created when the first message
    /// is sent; until then this client sees an empty transcript.
    NewChat,
    /// Look at another chat. Only this client switches; a turn running elsewhere goes on.
    OpenChat {
        id: String,
    },
    RenameChat {
        id: String,
        title: String,
    },
    /// Delete a chat and its messages. Refused while a reply is running in it.
    DeleteChat {
        id: String,
    },
    PinChat {
        id: String,
        pinned: bool,
    },
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
    /// A tool step of entry `entry` was added or changed (replace by `step.id`).
    StepUpdated { entry: u64, step: Step },
    /// Whether entry `id`'s file changes can be undone, or have been.
    EntryUndo { id: u64, undoable: bool, undone: bool },
    /// A page of the chat list. With `after: None` it replaces the client's list (pinned
    /// chats are in `pinned`); otherwise `recent` continues the list after `after`.
    Chats { pinned: Vec<ChatSummary>, recent: Vec<ChatSummary>, after: Option<ChatCursor>, next: Option<ChatCursor> },
    /// A chat was created or changed (title, pin, activity, busy). Sent to every client.
    ChatUpdated(ChatSummary),
    /// A chat was deleted. Sent to every client.
    ChatDeleted { id: String },
    /// This client now looks at `chat` (`None`: a new chat, not sent yet). The chat's
    /// [`ServerMsg::Transcript`] follows; entry messages only arrive for the chat in view.
    ChatOpened { chat: Option<ChatSummary> },
}

/// A chat as shown in the sidebar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatSummary {
    pub id: String,
    pub title: String,
    /// Milliseconds since the Unix epoch of the last activity; the list is ordered by it.
    pub updated_at: i64,
    /// When it was pinned (ms since the epoch), if it is. Pinned chats list first.
    pub pinned_at: Option<i64>,
    /// A reply is running in this chat.
    #[serde(default)]
    pub busy: bool,
}

impl ChatSummary {
    pub fn cursor(&self) -> ChatCursor {
        ChatCursor { updated_at: self.updated_at, id: self.id.clone() }
    }
}

/// Position in the recent-first chat list, for paging.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatCursor {
    pub updated_at: i64,
    pub id: String,
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
    /// Tool calls made while producing this reply, in order. Part of the transcript so a
    /// client that connects late sees them and can answer pending approvals.
    #[serde(default)]
    pub steps: Vec<Step>,
    /// Every file change of this reply can be reversed with [`ClientMsg::Undo`].
    #[serde(default)]
    pub undoable: bool,
    /// The reply's file changes were undone.
    #[serde(default)]
    pub undone: bool,
}

impl Entry {
    /// Apply a [`ServerMsg::EntryDelta`].
    pub fn append(&mut self, content: &str, reasoning: &str) {
        self.content.push_str(content);
        self.reasoning.push_str(reasoning);
    }

    /// Apply a [`ServerMsg::StepUpdated`]: replace the step with the same id, or append it.
    pub fn upsert_step(&mut self, step: Step) {
        match self.steps.iter_mut().find(|s| s.id == step.id) {
            Some(existing) => *existing = step,
            None => self.steps.push(step),
        }
    }

    /// Steps waiting for a user's approval.
    pub fn pending_approvals(&self) -> impl Iterator<Item = &Step> {
        self.steps.iter().filter(|s| s.status == StepStatus::Waiting)
    }
}

/// How dangerous a tool call is; drives the permission gate. Mirrors `yah_tools::Access`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolAccess {
    Read,
    Write,
    Network,
    Execute,
}

impl ToolAccess {
    pub fn label(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Network => "network",
            Self::Execute => "execute",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Running,
    /// An approval request: blocked until a client sends [`ClientMsg::Approve`].
    Waiting,
    Done,
    /// The tool ran (or could not run) and reported an error.
    Failed,
    /// The user (or the policy) said no, or the turn was stopped while waiting.
    Denied,
}

impl StepStatus {
    pub fn finished(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Denied)
    }
}

/// A user's answer to an approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalAnswer {
    Allow,
    /// Allow, and stop asking about this tool in this chat (remembered across restarts).
    Always,
    Deny,
    /// Allow, and stop asking about this tool in every chat.
    AlwaysEverywhere,
}

/// A file-system effect of a tool call. Paths are for display (workspace-relative when possible).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileChange {
    Created { path: String },
    Modified { path: String },
    Deleted { path: String },
    Moved { from: String, to: String },
}

/// One tool call inside an assistant reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    /// Unique within the engine; also the approval id.
    pub id: u64,
    /// Tool name, e.g. `read_file`.
    pub tool: String,
    /// Short human title, e.g. "Read notes.md".
    pub title: String,
    pub access: ToolAccess,
    pub status: StepStatus,
    /// Short detail: the arguments while running or waiting, then a result excerpt or reason.
    pub detail: String,
    #[serde(default)]
    pub changes: Vec<FileChange>,
    /// Byte offset into the entry's `content` when the step started, for interleaving.
    #[serde(default)]
    pub at: usize,
    /// What the call will do, readable, for the approval card and the expanded row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<Preview>,
}

/// Most lines in a [`Preview`]'s line lists.
pub const MAX_PREVIEW_LINES: usize = 40;
/// Most characters in one preview line or value.
pub const MAX_PREVIEW_LINE_CHARS: usize = 300;

/// A readable account of a tool call, built from its (untrusted, bounded) arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Preview {
    /// Create or replace a file. `lines` is the start of the new content, or for a
    /// replacement a compact line diff against the current file.
    Write {
        path: String,
        bytes: u64,
        /// The file exists and will be replaced.
        replaces: bool,
        lines: Vec<DiffLine>,
        /// Lines (or changes) not shown.
        #[serde(default)]
        more: usize,
    },
    /// Move or rename: `(from, to)` pairs.
    Move {
        moves: Vec<(String, String)>,
        #[serde(default)]
        more: usize,
    },
    /// Move something to the trash; `what` describes it, e.g. "file, 2.1 KB".
    Delete { path: String, what: String },
    /// Any other tool: its key arguments. `code` is command/script text, shown monospaced.
    Args {
        fields: Vec<(String, String)>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
        #[serde(default)]
        more: usize,
    },
}

/// One line of a preview's content or diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    pub kind: LineKind,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineKind {
    /// Unchanged (or, for a new file, plain content).
    Same,
    Added,
    Removed,
    /// Unchanged lines left out; `text` says how many.
    Skipped,
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
    /// A turn is running in the chat this client is looking at.
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
    fn approve_wire_format() {
        let m = ClientMsg::Approve { step: 7, answer: ApprovalAnswer::Always };
        assert_eq!(m.to_json(), r#"{"type":"approve","step":7,"answer":"always"}"#);
        assert_eq!(ClientMsg::from_json(&m.to_json()).unwrap(), m);
    }

    fn step(id: u64, status: StepStatus) -> Step {
        Step { id, tool: "t".into(), title: "T".into(), access: ToolAccess::Write, status, detail: String::new(), changes: vec![], at: 0, preview: None }
    }

    #[test]
    fn steps_upsert_and_round_trip() {
        let mut e = Entry {
            id: 1,
            role: EntryRole::Assistant,
            content: String::new(),
            reasoning: String::new(),
            done: false,
            error: None,
            stats: None,
            steps: vec![],
            undoable: false,
            undone: false,
        };
        e.upsert_step(step(3, StepStatus::Running));
        e.upsert_step(step(4, StepStatus::Waiting));
        e.upsert_step(step(3, StepStatus::Done));
        assert_eq!(e.steps.len(), 2);
        assert_eq!(e.steps[0].status, StepStatus::Done);
        assert_eq!(e.pending_approvals().map(|s| s.id).collect::<Vec<_>>(), vec![4]);
        let mut s = step(5, StepStatus::Done);
        s.changes = vec![FileChange::Moved { from: "a".into(), to: "b".into() }];
        let msg = ServerMsg::StepUpdated { entry: 1, step: s };
        assert_eq!(ServerMsg::from_json(&msg.to_json()).unwrap(), msg);
    }

    #[test]
    fn entry_without_steps_still_parses() {
        let json = r#"{"id":1,"role":"user","content":"hi","reasoning":"","done":true,"error":null,"stats":null}"#;
        let e: Entry = serde_json::from_str(json).unwrap();
        assert!(e.steps.is_empty());
    }

    #[test]
    fn undo_and_preview_wire_format() {
        let m = ClientMsg::Undo { entry: 4 };
        assert_eq!(m.to_json(), r#"{"type":"undo","entry":4}"#);
        assert_eq!(ClientMsg::from_json(&m.to_json()).unwrap(), m);
        let mut s = step(1, StepStatus::Waiting);
        s.preview =
            Some(Preview::Write { path: "a.md".into(), bytes: 3, replaces: true, lines: vec![DiffLine { kind: LineKind::Added, text: "hi".into() }], more: 0 });
        let msg = ServerMsg::StepUpdated { entry: 1, step: s };
        assert_eq!(ServerMsg::from_json(&msg.to_json()).unwrap(), msg);
        let msg = ServerMsg::EntryUndo { id: 1, undoable: false, undone: true };
        assert_eq!(ServerMsg::from_json(&msg.to_json()).unwrap(), msg);
    }

    #[test]
    fn chat_messages_round_trip() {
        let summary = ChatSummary { id: "c1".into(), title: "Plans".into(), updated_at: 5, pinned_at: None, busy: true };
        for m in [
            ServerMsg::Chats { pinned: vec![], recent: vec![summary.clone()], after: None, next: Some(summary.cursor()) },
            ServerMsg::ChatUpdated(summary.clone()),
            ServerMsg::ChatOpened { chat: Some(summary) },
            ServerMsg::ChatDeleted { id: "c1".into() },
        ] {
            assert_eq!(ServerMsg::from_json(&m.to_json()).unwrap(), m);
        }
        let m = ClientMsg::ListChats { after: Some(ChatCursor { updated_at: 1, id: "x".into() }), limit: 50 };
        assert_eq!(ClientMsg::from_json(&m.to_json()).unwrap(), m);
        assert_eq!(ClientMsg::NewChat.to_json(), r#"{"type":"new_chat"}"#);
    }

    #[test]
    fn state_never_carries_api_key() {
        let json = serde_json::to_string(&EngineState::default()).unwrap();
        assert!(!json.contains("\"api_key\""));
    }
}
