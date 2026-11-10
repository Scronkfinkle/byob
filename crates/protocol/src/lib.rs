//! The wire protocol between byob clients (desktop window, browser tab) and the engine.
//!
//! Every client, including the in-process desktop window, talks to the engine only through
//! these messages, so the desktop and web UIs can never drift apart. Messages travel as JSON
//! text frames over a WebSocket, or through channels when in-process.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

pub mod artifact;
pub mod connect;
pub mod delegation;
pub mod media;
pub mod usage;

use serde::{Deserialize, Serialize};

pub use artifact::{Artifact, ArtifactKind, ArtifactRef, ArtifactVersion};
pub use connect::{Detected, InferenceTest, Link, PRESETS, Preset, Problem};
pub use delegation::{Delegation, DelegationState, DelegationUpdate};
pub use media::{MediaContent, MediaKind, MediaRef};
pub use usage::{ChatUsage, Compaction};

/// Bumped on any incompatible change to [`ClientMsg`] / [`ServerMsg`].
pub const PROTOCOL_VERSION: u32 = 8;

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
    /// Point the engine at a different inference endpoint (saved, then checked). `api_key`:
    /// `None` keeps the stored key if the server (scheme, host, port) is unchanged and drops
    /// it otherwise; `Some("")` removes it.
    SetInference {
        base_url: String,
        model: String,
        api_key: Option<String>,
    },
    /// Try an endpoint without switching to it; answered with [`ServerMsg::InferenceTested`].
    /// `api_key` as for [`ClientMsg::SetInference`].
    TestInference {
        base_url: String,
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        api_key: Option<String>,
    },
    /// Look again for inference servers on this machine's usual ports
    /// ([`EngineState::detected`]).
    DetectEndpoints,
    /// Run the last message of the chat in view again, after its reply failed before
    /// producing anything (e.g. the server was down). The failed reply is removed.
    Retry,
    /// Send a user message and start a turn. `attachments` are paths the engine returned
    /// in [`ServerMsg::Uploaded`].
    SendMessage {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<String>,
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
    /// One chunk of a file the user attaches. `upload` is chosen by the client (unique per
    /// connection); chunks arrive in order, `offset` is where `data` (base64, at most
    /// [`media::UPLOAD_CHUNK_BYTES`] raw) starts; `size` is the whole file's size. The engine
    /// answers the last chunk (`done`) or the first problem with [`ServerMsg::Uploaded`].
    Upload {
        upload: u64,
        name: String,
        size: u64,
        offset: u64,
        data: String,
        done: bool,
    },
    /// Ask for the content of a [`MediaRef`] in the transcript; answered with [`ServerMsg::Media`].
    GetMedia {
        path: String,
    },
    /// Ask for the bytes of version `version` of artifact `id` in the chat this client looks
    /// at; answered with [`ServerMsg::ArtifactFile`].
    GetArtifact {
        id: String,
        version: u32,
    },
    /// Summarise the conversation in view now, so later requests start from the summary
    /// (also typed as `/compact [focus]`). `focus` says what the summary must keep.
    Compact {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        focus: Option<String>,
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
    /// An upload finished (`attachment`) or failed (`error`). Only sent to the uploader.
    Uploaded { upload: u64, attachment: Option<MediaRef>, error: Option<String> },
    /// Answer to [`ClientMsg::GetMedia`].
    Media { path: String, content: MediaContent },
    /// Every artifact of the chat this client looks at (`chat`, `None` for a new chat).
    /// Sent after [`ServerMsg::Transcript`]; replaces the client's list.
    Artifacts { chat: Option<String>, artifacts: Vec<Artifact> },
    /// An artifact of the chat in view was created or got a new version.
    ArtifactUpdated(Artifact),
    /// Answer to [`ClientMsg::GetArtifact`]: the file, base64 (see [`media::encode`]), or why not.
    ArtifactFile { id: String, version: u32, data: Option<String>, error: Option<String> },
    /// Context usage of the chat in view; sent after the transcript and whenever it changes.
    Usage(ChatUsage),
    /// Every compaction of the chat in view, oldest first (`chat`, `None` for a new chat).
    /// Sent after [`ServerMsg::Transcript`] (before `Artifacts`); replaces the client's list.
    Compactions { chat: Option<String>, items: Vec<Compaction> },
    /// The chat in view was just compacted.
    Compacted(Compaction),
    /// The subagent run of step `step` (in entry `entry`) changed.
    Delegation { entry: u64, step: u64, update: DelegationUpdate },
    /// Answer to [`ClientMsg::TestInference`].
    InferenceTested(InferenceTest),
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
    /// Files the user attached to this message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<MediaRef>,
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

    /// Steps waiting for a user's approval, subagents' steps included.
    pub fn pending_approvals(&self) -> impl Iterator<Item = &Step> {
        self.steps.iter().flat_map(|s| std::iter::once(s).chain(s.delegation.iter().flat_map(|d| d.steps.iter()))).filter(|s| s.status == StepStatus::Waiting)
    }

    /// Apply a [`ServerMsg::Delegation`] to step `step`'s subagent run.
    pub fn apply_delegation(&mut self, step: u64, update: &DelegationUpdate) {
        if let Some(d) = self.steps.iter_mut().find(|s| s.id == step).and_then(|s| s.delegation.as_deref_mut()) {
            d.apply(update);
        }
    }
}

/// How dangerous a tool call is; drives the permission gate. Mirrors `byob_tools::Access`.
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
    /// Charts and images this call made, shown inline.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media: Vec<MediaRef>,
    /// What the call will do, readable, for the approval card and the expanded row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<Preview>,
    /// Artifact versions this call made, shown as cards where it happened.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ArtifactRef>,
    /// The subagent run of a `delegate` step: its live transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<Box<Delegation>>,
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
    /// Model requests the reply took (tool rounds and compactions included).
    #[serde(default)]
    pub model_calls: u64,
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
    /// What the engine knows about the inference endpoint (from its capability probe).
    #[serde(default)]
    pub endpoint_info: Option<EndpointInfo>,
    /// Whether the current endpoint answers.
    #[serde(default)]
    pub link: Link,
    /// An endpoint has worked before (saved in settings.json): returning users skip the
    /// welcome screen.
    #[serde(default)]
    pub onboarded: bool,
    /// Inference servers found on this machine's usual ports.
    #[serde(default)]
    pub detected: Vec<Detected>,
    /// The engine is looking for them.
    #[serde(default)]
    pub detecting: bool,
}

/// How turns pass tools to the model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolsMode {
    /// The server's structured `tools` / `tool_calls`.
    Native,
    /// Tools described in the system prompt; calls parsed from the reply text.
    Prompt,
    /// Not known yet: native first, switching to prompt mode if the server rejects tools.
    #[default]
    Unknown,
}

/// Capability summary of the inference endpoint, for a small header detail.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EndpointInfo {
    /// The probe is still running.
    #[serde(default)]
    pub probing: bool,
    /// Server software, e.g. "llama.cpp", "vLLM", "Ollama"; empty if unknown.
    #[serde(default)]
    pub server: String,
    /// Context window in tokens, if the server reports it.
    #[serde(default)]
    pub context_tokens: Option<u64>,
    #[serde(default)]
    pub tools: ToolsMode,
    /// Takes image input: `Some(true)` yes, `Some(false)` no, `None` unknown.
    #[serde(default)]
    pub vision: Option<bool>,
}

impl EndpointInfo {
    /// One short line, e.g. "llama.cpp · 262k ctx · native tools · vision".
    pub fn summary(&self) -> String {
        if self.probing {
            return "checking endpoint…".into();
        }
        let mut parts = Vec::new();
        if !self.server.is_empty() {
            parts.push(self.server.clone());
        }
        if let Some(n) = self.context_tokens {
            parts.push(if n >= 1000 { format!("{}k ctx", n / 1000) } else { format!("{n} ctx") });
        }
        parts.push(
            match self.tools {
                ToolsMode::Native => "native tools",
                ToolsMode::Prompt => "prompt tools",
                ToolsMode::Unknown => "tools untested",
            }
            .into(),
        );
        if self.vision == Some(true) {
            parts.push("vision".into());
        }
        parts.join(" · ")
    }
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
        Step {
            id,
            tool: "t".into(),
            title: "T".into(),
            access: ToolAccess::Write,
            status,
            detail: String::new(),
            changes: vec![],
            at: 0,
            preview: None,
            media: vec![],
            artifacts: vec![],
            delegation: None,
        }
    }

    #[test]
    fn delegations_stream_into_their_step_and_round_trip() {
        let mut parent = step(1, StepStatus::Running);
        parent.delegation = Some(Box::new(Delegation { agent: "explorer".into(), task: "count lines".into(), ..Default::default() }));
        let mut e = Entry {
            id: 9,
            role: EntryRole::Assistant,
            content: String::new(),
            reasoning: String::new(),
            done: false,
            error: None,
            stats: None,
            steps: vec![parent],
            undoable: false,
            undone: false,
            attachments: vec![],
        };
        let updates = [
            DelegationUpdate::Status { state: DelegationState::Running, prompt_tokens: 0, completion_tokens: 0, error: None },
            DelegationUpdate::Delta { content: "Reading".into(), reasoning: "hm".into() },
            DelegationUpdate::Step(step(2, StepStatus::Waiting)),
            DelegationUpdate::Delta { content: " a.txt".into(), reasoning: String::new() },
        ];
        for u in &updates {
            let msg = ServerMsg::Delegation { entry: 9, step: 1, update: u.clone() };
            assert_eq!(ServerMsg::from_json(&msg.to_json()).unwrap(), msg);
            e.apply_delegation(1, u);
        }
        // An unknown step is ignored.
        e.apply_delegation(77, &updates[1]);
        let d = e.steps[0].delegation.as_deref().unwrap();
        assert_eq!((d.content.as_str(), d.reasoning.as_str(), d.state), ("Reading a.txt", "hm", DelegationState::Running));
        assert_eq!(d.status_line(), "explorer · 1 tool · waiting for approval");
        // A child's approval card counts as pending in the entry.
        assert_eq!(e.pending_approvals().map(|s| s.id).collect::<Vec<_>>(), vec![2]);
        e.apply_delegation(1, &DelegationUpdate::Step(step(2, StepStatus::Done)));
        e.apply_delegation(1, &DelegationUpdate::Status { state: DelegationState::Done, prompt_tokens: 50, completion_tokens: 5, error: None });
        assert_eq!(e.steps[0].delegation.as_deref().unwrap().status_line(), "explorer · 1 tool · done");
        assert_eq!(e.pending_approvals().count(), 0);
        let msg = ServerMsg::StepUpdated { entry: 9, step: e.steps[0].clone() };
        assert_eq!(ServerMsg::from_json(&msg.to_json()).unwrap(), msg);
        // Steps from an older engine have no delegation.
        let json = r#"{"id":1,"tool":"t","title":"T","access":"read","status":"done","detail":""}"#;
        assert!(serde_json::from_str::<Step>(json).unwrap().delegation.is_none());
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
            attachments: vec![],
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
    fn endpoint_info_is_optional_and_summarised() {
        let old = r#"{"version":"0","inference_base_url":"u","inference_model":"m","has_api_key":false,"endpoint":null,"busy":false}"#;
        assert_eq!(serde_json::from_str::<EngineState>(old).unwrap().endpoint_info, None);
        let info = EndpointInfo { probing: false, server: "llama.cpp".into(), context_tokens: Some(262_144), tools: ToolsMode::Native, vision: Some(true) };
        assert_eq!(info.summary(), "llama.cpp · 262k ctx · native tools · vision");
        assert_eq!(EndpointInfo { probing: true, ..info }.summary(), "checking endpoint…");
        assert_eq!(EndpointInfo::default().summary(), "tools untested");
    }

    #[test]
    fn media_messages_round_trip() {
        let msgs = vec![
            ClientMsg::SendMessage { text: "chart it".into(), attachments: vec!["uploads/sales.csv".into()] },
            ClientMsg::Upload { upload: 1, name: "a.csv".into(), size: 3, offset: 0, data: media::encode(b"a,b"), done: true },
            ClientMsg::GetMedia { path: "charts/x.chart.json".into() },
        ];
        for m in msgs {
            assert_eq!(ClientMsg::from_json(&m.to_json()).unwrap(), m);
        }
        let spec = byob_types::chart::validate(&serde_json::json!({"type": "bar", "x": "m", "series": ["v"], "data": [{"m": "a", "v": 1}]})).unwrap();
        let msgs = vec![
            ServerMsg::Uploaded { upload: 1, attachment: Some(MediaRef::new("uploads/a.csv", 3)), error: None },
            ServerMsg::Media { path: "c.chart.json".into(), content: MediaContent::Chart { spec } },
            ServerMsg::Media { path: "p.png".into(), content: MediaContent::Image { mime: "image/png".into(), data: "AA==".into() } },
            ServerMsg::Media { path: "p.png".into(), content: MediaContent::Error { message: "gone".into() } },
        ];
        for m in msgs {
            assert_eq!(ServerMsg::from_json(&m.to_json()).unwrap(), m);
        }
        let mut s = step(1, StepStatus::Done);
        s.media = vec![MediaRef::new("charts/x.chart.json", 10)];
        let m = ServerMsg::StepUpdated { entry: 1, step: s };
        assert_eq!(ServerMsg::from_json(&m.to_json()).unwrap(), m);
    }

    #[test]
    fn artifact_messages_round_trip() {
        let v = ArtifactVersion { n: 2, path: ".byob/artifacts/calc/v2.html".into(), created_at: 1, note: "blue button".into(), size: 10 };
        let a = Artifact { id: "calc".into(), kind: ArtifactKind::Html, title: "Calc".into(), filename: "calc.html".into(), versions: vec![v] };
        let msgs = vec![
            ServerMsg::Artifacts { chat: Some("c1".into()), artifacts: vec![a.clone()] },
            ServerMsg::Artifacts { chat: None, artifacts: vec![] },
            ServerMsg::ArtifactUpdated(a),
            ServerMsg::ArtifactFile { id: "calc".into(), version: 2, data: Some(media::encode(b"<p>")), error: None },
            ServerMsg::ArtifactFile { id: "calc".into(), version: 9, data: None, error: Some("no such version".into()) },
        ];
        for m in msgs {
            assert_eq!(ServerMsg::from_json(&m.to_json()).unwrap(), m);
        }
        let m = ClientMsg::GetArtifact { id: "calc".into(), version: 2 };
        assert_eq!(m.to_json(), r#"{"type":"get_artifact","id":"calc","version":2}"#);
        assert_eq!(ClientMsg::from_json(&m.to_json()).unwrap(), m);
        let mut s = step(1, StepStatus::Done);
        s.artifacts = vec![ArtifactRef { id: "calc".into(), version: 2 }];
        let m = ServerMsg::StepUpdated { entry: 1, step: s };
        assert_eq!(ServerMsg::from_json(&m.to_json()).unwrap(), m);
        // A step from an older engine has no artifacts.
        let json = r#"{"id":1,"tool":"t","title":"T","access":"read","status":"done","detail":""}"#;
        assert!(serde_json::from_str::<Step>(json).unwrap().artifacts.is_empty());
    }

    #[test]
    fn older_send_message_without_attachments_parses() {
        let m = ClientMsg::from_json(r#"{"type":"send_message","text":"hi"}"#).unwrap();
        assert_eq!(m, ClientMsg::SendMessage { text: "hi".into(), attachments: vec![] });
        assert_eq!(m.to_json(), r#"{"type":"send_message","text":"hi"}"#);
    }

    #[test]
    fn compaction_messages_round_trip() {
        let c = Compaction {
            id: 1,
            after_entry: 7,
            tokens_before: 182_000,
            tokens_after: 9_000,
            summary: "## Goals".into(),
            manual: true,
            focus: Some("CSV".into()),
            created_at: 5,
        };
        let u = ChatUsage { chat: "c1".into(), context_used: 34_000, context_window: 262_144, compact_at: 209_715, model_calls: 3, ..Default::default() };
        for m in [ServerMsg::Usage(u), ServerMsg::Compactions { chat: Some("c1".into()), items: vec![c.clone()] }, ServerMsg::Compacted(c)] {
            assert_eq!(ServerMsg::from_json(&m.to_json()).unwrap(), m);
        }
        assert_eq!(ClientMsg::Compact { focus: None }.to_json(), r#"{"type":"compact"}"#);
        let m = ClientMsg::from_json(r#"{"type":"compact","focus":"keep the CSV"}"#).unwrap();
        assert_eq!(m, ClientMsg::Compact { focus: Some("keep the CSV".into()) });
        // Stats from an older engine have no call count.
        let s: TurnStats = serde_json::from_str(r#"{"prompt_tokens":1,"completion_tokens":2,"elapsed_ms":3}"#).unwrap();
        assert_eq!(s.model_calls, 0);
    }

    #[test]
    fn connection_messages_round_trip() {
        for m in [
            ClientMsg::TestInference { base_url: "http://127.0.0.1:11434/v1".into(), model: "qwen3".into(), api_key: None },
            ClientMsg::TestInference { base_url: "https://api.openai.com/v1".into(), model: "m".into(), api_key: Some("sk".into()) },
            ClientMsg::DetectEndpoints,
            ClientMsg::Retry,
        ] {
            assert_eq!(ClientMsg::from_json(&m.to_json()).unwrap(), m);
        }
        assert_eq!(ClientMsg::Retry.to_json(), r#"{"type":"retry"}"#);
        let t = InferenceTest { base_url: "u".into(), model: "m".into(), ok: false, problem: Some(Problem::Unauthorized), detail: "x".into(), models: vec![] };
        let m = ServerMsg::InferenceTested(t);
        assert_eq!(ServerMsg::from_json(&m.to_json()).unwrap(), m);
        let state = EngineState {
            link: Link::failed("could not reach u: refused"),
            onboarded: true,
            detected: vec![Detected { preset: "ollama".into(), base_url: "http://127.0.0.1:11434/v1".into(), models: vec!["qwen3".into()] }],
            ..Default::default()
        };
        let m = ServerMsg::State(state);
        assert_eq!(ServerMsg::from_json(&m.to_json()).unwrap(), m);
        // State from an older engine: link unknown, not onboarded.
        let old = r#"{"version":"0","inference_base_url":"u","inference_model":"m","has_api_key":false,"endpoint":null,"busy":false}"#;
        let s = serde_json::from_str::<EngineState>(old).unwrap();
        assert_eq!((s.link, s.onboarded, s.detected.len()), (Link::Unknown, false, 0));
    }

    #[test]
    fn state_never_carries_api_key() {
        let json = serde_json::to_string(&EngineState::default()).unwrap();
        assert!(!json.contains("\"api_key\""));
    }
}
