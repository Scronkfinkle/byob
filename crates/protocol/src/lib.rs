//! The wire protocol between byob clients (desktop window, browser tab) and the engine.
//!
//! Every client, including the in-process desktop window, talks to the engine only through
//! these messages, so the desktop and web UIs can never drift apart. Messages travel as JSON
//! text frames over a WebSocket, or through channels when in-process.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

pub mod artifact;
pub mod connect;
pub mod delegation;
pub mod env;
pub mod media;
pub mod plugins;
pub mod queue;
pub mod sites;
pub mod tree;
pub mod usage;
pub mod version;
pub mod view;
pub mod web;
pub mod workspace;

use serde::{Deserialize, Serialize};

pub use artifact::{Artifact, ArtifactKind, ArtifactRef, ArtifactVersion};
pub use connect::{Detected, InferenceTest, Link, PRESETS, Preset, Problem};
pub use delegation::{Delegation, DelegationState, DelegationUpdate};
pub use env::{ContainerEnv, EnvAction, EnvSupport, WorkspaceEnv};
pub use media::{MediaContent, MediaKind, MediaRef};
pub use plugins::{
    BackendInfo, Extensions, PluginAction, PluginCard, PluginInfo, PluginLoadError, PluginOrigin, PluginScope, PluginSetting, PluginTest, RendererDecl,
    ServerInfo, ServerState, SlashItem, SlashKind, StarterDecl,
};
pub use queue::{ChatQueue, QueuePause, QueuedMessage};
pub use sites::{LiveView, Site, ViewDecl, ViewRef};
pub use usage::{ChatUsage, Compaction};
pub use version::{PROTOCOL_MAJOR, PROTOCOL_MINOR, Welcome};
pub use view::{View, ViewData};
pub use workspace::{ChatEndpoint, ConfigScope, ConfigView, FolderList, NewFolder, WorkspaceSummary};

/// Hard cap on a single incoming frame, so a hostile peer can't make us allocate unboundedly.
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Client -> engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// First message on every connection: the client's [`PROTOCOL_MAJOR`] (`protocol`),
    /// minor and caps (see [`version`]). Answered with [`ServerMsg::Welcome`], or an error
    /// notice if the major differs.
    Hello {
        protocol: u32,
        #[serde(default)]
        minor: u32,
        client: String,
        #[serde(default)]
        caps: Vec<String>,
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
    /// Send a user message and start a turn; while a reply runs in the chat, queue it
    /// instead (see [`queue`]). `attachments` are paths the engine returned in
    /// [`ServerMsg::Uploaded`].
    SendMessage {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<String>,
    },
    /// Stop the running turn. Pending approvals are denied; the chat's queue pauses.
    Cancel,
    /// Take queued message `id` out of the queue of the chat in view.
    Unqueue {
        id: u64,
    },
    /// Replace the text and attachments of queued message `id` (in place). Empty text and no
    /// attachments removes it.
    EditQueued {
        id: u64,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<String>,
    },
    /// Move queued message `id` to position `to` (0 = next; past the end = last).
    MoveQueued {
        id: u64,
        to: u32,
    },
    /// Run queued message `id` next, now: stop the running turn (if any) and resume the queue.
    SendQueuedNow {
        id: u64,
    },
    /// Let a paused queue go on (starts the next message if the chat is free).
    ResumeQueue,
    /// Answer a pending approval (a [`Step`] with status [`StepStatus::Waiting`]). Any
    /// client may answer; the first answer wins and later ones are ignored.
    Approve {
        step: u64,
        answer: ApprovalAnswer,
    },
    /// Answer a question card (a waiting [`Step`] whose [`Ask::choice`] is set): one of its
    /// options or, when it takes one, the user's own text (≤ [`MAX_ANSWER_CHARS`]). Skipping
    /// it is [`ClientMsg::Approve`] with [`ApprovalAnswer::Deny`]. First answer wins (13.8).
    AnswerQuestion {
        step: u64,
        text: String,
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
    /// Start a new, empty chat in this client, in workspace `workspace` (default: the
    /// workspace of the chat in view, else the default one). The chat is created when the
    /// first message is sent; until then this client sees an empty transcript.
    NewChat {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
    },
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
    /// Ask for the environment state of workspace `workspace` (default: the chat in view's),
    /// after applying `action` if given; answered with [`ServerMsg::WorkspaceEnv`].
    WorkspaceEnv {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        action: Option<EnvAction>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
    },
    /// Make a workspace: a name, a folder on the engine's machine, its own settings
    /// (validated for the workspace level). Answered with [`ServerMsg::WorkspaceCreated`];
    /// on success this client starts a new chat in it.
    CreateWorkspace {
        name: String,
        folder: NewFolder,
        #[serde(default)]
        config: workspace::Layer,
    },
    RenameWorkspace {
        id: String,
        name: String,
    },
    /// Remove a workspace from byob with its chats; the folder and its files stay. `clean`
    /// also deletes what byob keeps for it elsewhere (installed packages, its container).
    RemoveWorkspace {
        id: String,
        #[serde(default)]
        clean: bool,
    },
    /// Ask for one level's settings; answered with [`ServerMsg::Config`].
    GetConfig {
        scope: ConfigScope,
    },
    /// Replace one level's own settings (`None` fields inherit). `api_key` as for
    /// [`ClientMsg::SetInference`], for a level with its own URL. Answered with
    /// [`ServerMsg::Config`].
    SetConfig {
        scope: ConfigScope,
        layer: workspace::Layer,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        api_key: Option<String>,
    },
    /// List the subfolders of `path` on the engine's machine (default: the home folder);
    /// answered with [`ServerMsg::Folders`].
    ListFolders {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    /// Is Docker/Podman reachable? Answered with [`ServerMsg::ContainersChecked`].
    CheckContainers,
    /// Ask for the plugins of the chat in view; answered with [`ServerMsg::Extensions`].
    GetExtensions,
    /// Turn plugin `plugin` on (`Some(true)`) or off (`Some(false)`) at one level of the
    /// layered settings, or forget that level's switch (`None`: inherit). Applies from the
    /// next turn. Answered with [`ServerMsg::Extensions`].
    SetPluginEnabled {
        scope: ConfigScope,
        plugin: String,
        enabled: Option<bool>,
    },
    /// Set (or clear, `None`) a setting the plugin declares. Secret settings go to
    /// secrets.json, the others to settings.json; values are never sent back, only whether
    /// each is set ([`PluginSetting::set`]).
    SetPluginSetting {
        plugin: String,
        key: String,
        value: Option<String>,
    },
    /// Keep plugin `plugin` of the chat in view (its scratch copy, or whichever copy the chat
    /// uses) in the chat's workspace or everywhere, as `new_id` if given. The user asked, so
    /// there is no approval card.
    KeepPlugin {
        plugin: String,
        scope: PluginScope,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        new_id: Option<String>,
    },
    /// Delete the copy of plugin `plugin` in `scope` (the chat in view's scratch folder, its
    /// workspace's `.byob/plugins`, or the global plugins folder).
    RemovePlugin {
        plugin: String,
        scope: PluginScope,
    },
    /// Read the plugin folders of the chat in view again now.
    ReloadPlugins,
    /// Let the plugins that came with workspace `workspace`'s folder run their tools and
    /// servers (`trust: true`), or keep them to skills and prompt lines without asking again
    /// (`false`). `plugins` empty: all of them.
    TrustPlugins {
        workspace: String,
        #[serde(default)]
        plugins: Vec<String>,
        trust: bool,
    },
    /// A message for plugin `plugin`'s handler in the engine (the plugin channel; only when
    /// the engine lists [`version::CAP_PLUGIN_CHANNEL`]). Its meaning is the plugin's.
    Plugin {
        plugin: String,
        msg: serde_json::Value,
    },
    /// This client shows live view `view` at its site (13.3+, only when the engine lists
    /// [`sites::CAP_VIEW_SITES`]): answered with a [`ServerMsg::LiveView`] holding its tree,
    /// then its changes until [`ClientMsg::CloseView`] or another chat is opened.
    OpenView {
        view: ViewRef,
    },
    /// This client no longer shows `view`.
    CloseView {
        view: ViewRef,
    },
    /// The user did something in live view `view` (a click, Enter in a field, a cell edit):
    /// for the plugin's backend, which answers with a new view.
    ViewEvent {
        view: ViewRef,
        event: tree::Event,
    },
    /// A call from a web surface for the engine (13.5+, only when the engine lists
    /// [`web::CAP_WEB_BRIDGE`]): JSON-RPC `method` + `params` from the page whose calls
    /// `owner` answers. Answered with one [`ServerMsg::WebReply`] of the same `call` number.
    /// `approved`: the user allowed it after a reply asked ([`web::WebError::approval`]).
    WebCall {
        call: u64,
        owner: web::WebOwner,
        method: String,
        #[serde(default)]
        params: serde_json::Value,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        approved: bool,
    },
    /// A message from a newer client this engine doesn't know (answered with a notice).
    #[serde(other)]
    Unknown,
}

/// Engine -> client(s).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// The answer to [`ClientMsg::Hello`], first: the engine's version and caps.
    Welcome(Welcome),
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
    /// This client now looks at `chat` (`None`: a new chat, not sent yet), which is in
    /// workspace `workspace`. The chat's [`ServerMsg::Transcript`] follows; entry messages
    /// only arrive for the chat in view.
    ChatOpened {
        chat: Option<ChatSummary>,
        #[serde(default)]
        workspace: Option<String>,
    },
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
    /// Answer to [`ClientMsg::WorkspaceEnv`]: the environment of a workspace.
    WorkspaceEnv(WorkspaceEnv),
    /// Every workspace, most recently active first; sent on connect and whenever one is
    /// added, renamed, removed or changes its chat count. `current` is where new chats go
    /// by default; `root` is where fresh workspace folders are created.
    Workspaces { items: Vec<WorkspaceSummary>, current: Option<String>, root: String, support: EnvSupport },
    /// Answer to [`ClientMsg::GetConfig`] / [`ClientMsg::SetConfig`].
    Config(Box<ConfigView>),
    /// Answer to [`ClientMsg::ListFolders`].
    Folders(FolderList),
    /// Answer to [`ClientMsg::CheckContainers`].
    ContainersChecked { ok: bool, message: String },
    /// Answer to [`ClientMsg::CreateWorkspace`].
    WorkspaceCreated { id: Option<String>, error: Option<String> },
    /// The queue of the chat in view; sent after [`ServerMsg::Transcript`] and whenever it
    /// changes.
    Queue(ChatQueue),
    /// The plugins of the chat in view; sent after [`ServerMsg::ChatOpened`], on
    /// [`ClientMsg::GetExtensions`], and whenever they change.
    Extensions(Box<Extensions>),
    /// A message from plugin `plugin` in the engine (the plugin channel; only to clients
    /// that list [`version::CAP_PLUGIN_CHANNEL`]).
    Plugin { plugin: String, msg: serde_json::Value },
    /// A live view's tree, a change to it, or why it can't be shown (13.3+; only to clients
    /// that listed [`sites::CAP_VIEW_SITES`] and opened it, see [`sites`]).
    LiveView(LiveView),
    /// The answer to [`ClientMsg::WebCall`] `call` (only to the client that called): a
    /// JSON-RPC `result` or an `error`.
    WebReply {
        call: u64,
        /// `Some(Value::Null)` for a JSON-RPC `null` result, which is sent as `null`.
        #[serde(default, deserialize_with = "present_even_if_null", skip_serializing_if = "Option::is_none")]
        result: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<web::WebError>,
    },
    /// A JSON-RPC message (a notification) for the web surfaces `owner` answers, from the
    /// owner (a plugin's backend; only to clients that listed [`web::CAP_WEB_BRIDGE`]).
    WebPush { owner: web::WebOwner, msg: serde_json::Value },
    /// A message from a newer engine this client doesn't know: skip it.
    #[serde(other)]
    Unknown,
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
    /// The workspace it belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
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
    /// An answer from a newer client this engine doesn't know: treated as Deny.
    #[serde(other)]
    Unknown,
}

/// A file-system effect of a tool call. Paths are for display (workspace-relative when possible).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileChange {
    Created {
        path: String,
    },
    Modified {
        path: String,
    },
    Deleted {
        path: String,
    },
    Moved {
        from: String,
        to: String,
    },
    /// A change a newer engine reports that this client doesn't know (not shown).
    #[serde(other)]
    Other,
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
    /// Set when the approval card isn't an ordinary tool call: a question a tool asks while
    /// it runs, or a call a person must confirm every time. Such cards offer only Allow
    /// and Deny.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask: Option<Box<Ask>>,
    /// Cards shown under the step ([`View`]s, e.g. a `byob.plugin_card` when it made,
    /// changed, kept or removed a plugin). Set on the finished step, and stored with it (a
    /// transcript loaded from storage has them too).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub views: Vec<View>,
}

impl Step {
    /// The step's first view of type `T` this build can read.
    pub fn view<T: ViewData>(&self) -> Option<T> {
        self.views.iter().find_map(View::get)
    }
}

/// The special part of an approval card (see [`Step::ask`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ask {
    /// What saying yes means.
    #[serde(default)]
    pub text: String,
    /// Shown prominently, e.g. that it installs system-wide.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    /// Only a person can allow it: no policy, grant or `--auto-approve` does.
    #[serde(default)]
    pub always: bool,
    /// A question for the user (the `ask_user` tool, 13.8): the card offers these choices
    /// instead of Allow, and is answered with [`ClientMsg::AnswerQuestion`]. A client that
    /// doesn't know it shows Allow/Deny (Allow then means "no answer").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choice: Option<Choice>,
}

/// Most options on a question card.
pub const MAX_CHOICE_OPTIONS: usize = 8;
/// Longest option or answer text (characters).
pub const MAX_ANSWER_CHARS: usize = 2000;

/// The choices of a question card (see [`Ask::choice`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    /// Buttons, in order (≤ [`MAX_CHOICE_OPTIONS`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    /// The user may type their own answer.
    #[serde(default)]
    pub free_text: bool,
    /// What the user answered, once they did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
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
    /// A preview a newer engine sends that this client doesn't know (not shown).
    #[serde(other)]
    Other,
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
    Error,
    /// Also any level a newer engine sends that this client doesn't know.
    #[serde(other)]
    Info,
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
    /// The chat in view uses another endpoint than the one above (its workspace's or its
    /// own); `None`: it uses the global one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_endpoint: Option<ChatEndpoint>,
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
    /// This build's [`ClientMsg::Hello`].
    pub fn hello(client: &str) -> Self {
        Self::Hello { protocol: PROTOCOL_MAJOR, minor: PROTOCOL_MINOR, client: client.to_owned(), caps: version::client_caps() }
    }

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

    /// The frame for a remote client: the JSON, or, if that is over [`MAX_FRAME_BYTES`] (the
    /// client would refuse it and wait forever), a small message in its place that the
    /// client acts on: a view change becomes `reset` (the client opens the view again and
    /// gets the whole tree), a view's tree an `error` at its site, a web reply an error
    /// answer to its call, anything else a notice.
    pub fn to_frame(&self) -> String {
        let json = self.to_json();
        if json.len() <= MAX_FRAME_BYTES {
            return json;
        }
        let why = format!("it is too large to send over this connection ({} KB, at most {} KB)", json.len() / 1024, MAX_FRAME_BYTES / 1024);
        let instead = match self {
            Self::LiveView(lv) if lv.tree.is_none() => Self::LiveView(LiveView { view: lv.view.clone(), reset: true, ..Default::default() }),
            Self::LiveView(lv) => {
                Self::LiveView(LiveView { view: lv.view.clone(), error: Some(format!("This view can't be shown here: {why}.")), ..Default::default() })
            }
            Self::WebReply { call, .. } => {
                Self::WebReply { call: *call, result: None, error: Some(web::WebError::new(-32000, format!("byob couldn't send the answer: {why}"))) }
            }
            _ => Self::Notice { level: NoticeLevel::Error, text: format!("byob couldn't send an update: {why}.") },
        };
        instead.to_json()
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

/// An optional field where a present `null` is `Some(Value::Null)`, not `None` (serde's
/// default for `Option`); pair with `#[serde(default)]` so a missing one is `None`.
fn present_even_if_null<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<serde_json::Value>, D::Error> {
    serde_json::Value::deserialize(d).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: a `null` result decoded as `None`, so remote clients answered the page
    /// with `{}` where the desktop answered `null`.
    #[test]
    fn web_reply_keeps_a_null_result() {
        let null = ServerMsg::WebReply { call: 3, result: Some(serde_json::Value::Null), error: None };
        assert_eq!(null.to_json(), r#"{"type":"web_reply","call":3,"result":null}"#);
        assert_eq!(ServerMsg::from_json(&null.to_json()).unwrap(), null);
        let none = ServerMsg::WebReply { call: 3, result: None, error: None };
        assert_eq!(none.to_json(), r#"{"type":"web_reply","call":3}"#);
        assert_eq!(ServerMsg::from_json(&none.to_json()).unwrap(), none);
        let value = ServerMsg::WebReply { call: 3, result: Some(serde_json::json!({"a": [1]})), error: None };
        assert_eq!(ServerMsg::from_json(&value.to_json()).unwrap(), value);
    }

    #[test]
    fn client_msg_round_trips() {
        let m = ClientMsg::hello("test");
        assert_eq!(ClientMsg::from_json(&m.to_json()).unwrap(), m);
    }

    #[test]
    fn wire_format_is_tagged() {
        let json = ServerMsg::Notice { level: NoticeLevel::Info, text: "hi".into() }.to_json();
        assert_eq!(json, r#"{"type":"notice","level":"info","text":"hi"}"#);
    }

    #[test]
    fn a_message_too_large_for_a_frame_is_sent_as_something_the_client_acts_on() {
        let big = "x".repeat(MAX_FRAME_BYTES);
        let r = ViewRef::new("p", "v", "k");
        let small = ServerMsg::Notice { level: NoticeLevel::Info, text: "hi".into() };
        assert_eq!(small.to_frame(), small.to_json());
        let frame = |m: ServerMsg| {
            let f = m.to_frame();
            assert!(f.len() < 1024, "{} bytes", f.len());
            ServerMsg::from_json(&f).unwrap()
        };
        let mut values = serde_json::Map::new();
        values.insert("f".into(), serde_json::json!(big));
        let change = LiveView { view: r.clone(), values, ..Default::default() };
        assert_eq!(frame(ServerMsg::LiveView(change)), ServerMsg::LiveView(LiveView { view: r.clone(), reset: true, ..Default::default() }));
        // Within the tree caps (64 K characters a text), over a frame all the same.
        let texts: Vec<_> = (0..100).map(|_| serde_json::json!({"type": "text", "text": "y".repeat(60_000)})).collect();
        let (tree, faults) = tree::decode(serde_json::json!({"type": "column", "children": texts}));
        assert!(faults.is_empty(), "{faults:?}");
        match frame(ServerMsg::LiveView(LiveView { view: r.clone(), tree: Some(tree), ..Default::default() })) {
            ServerMsg::LiveView(lv) => assert!(lv.view == r && lv.tree.is_none() && lv.error.is_some_and(|e| e.contains("too large"))),
            other => panic!("{other:?}"),
        }
        match frame(ServerMsg::WebReply { call: 9, result: Some(serde_json::json!(big)), error: None }) {
            ServerMsg::WebReply { call: 9, result: None, error: Some(e) } => assert!(e.message.contains("too large")),
            other => panic!("{other:?}"),
        }
        let plugin = ServerMsg::Plugin { plugin: "p".into(), msg: serde_json::json!(big) };
        assert!(matches!(frame(plugin), ServerMsg::Notice { level: NoticeLevel::Error, .. }));
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
            ask: None,
            views: vec![],
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
            DelegationUpdate::Step(Box::new(step(2, StepStatus::Waiting))),
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
        e.apply_delegation(1, &DelegationUpdate::Step(Box::new(step(2, StepStatus::Done))));
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
        let summary = ChatSummary { id: "c1".into(), title: "Plans".into(), updated_at: 5, pinned_at: None, busy: true, workspace: Some("w1".into()) };
        for m in [
            ServerMsg::Chats { pinned: vec![], recent: vec![summary.clone()], after: None, next: Some(summary.cursor()) },
            ServerMsg::ChatUpdated(summary.clone()),
            ServerMsg::ChatOpened { chat: Some(summary), workspace: None },
            ServerMsg::ChatDeleted { id: "c1".into() },
        ] {
            assert_eq!(ServerMsg::from_json(&m.to_json()).unwrap(), m);
        }
        let m = ClientMsg::ListChats { after: Some(ChatCursor { updated_at: 1, id: "x".into() }), limit: 50 };
        assert_eq!(ClientMsg::from_json(&m.to_json()).unwrap(), m);
        assert_eq!(ClientMsg::NewChat { workspace: None }.to_json(), r#"{"type":"new_chat"}"#);
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
        let a = Artifact {
            id: "calc".into(),
            kind: ArtifactKind::Html,
            content_type: artifact::TYPE_HTML.into(),
            title: "Calc".into(),
            filename: "calc.html".into(),
            versions: vec![v],
        };
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
    fn hello_negotiates_and_old_hellos_still_parse() {
        let m = ClientMsg::hello("desktop");
        let ClientMsg::Hello { protocol, minor, caps, .. } = &m else { panic!() };
        assert_eq!((*protocol, *minor), (PROTOCOL_MAJOR, PROTOCOL_MINOR));
        assert!(caps.iter().any(|c| c == version::CAP_PLUGIN_CHANNEL));
        assert!(m.check_sizes().is_ok());
        // A protocol-12 client says only its version: it parses, so the engine can tell it why not.
        let old = ClientMsg::from_json(r#"{"type":"hello","protocol":12,"client":"old tab"}"#).unwrap();
        assert_eq!(old, ClientMsg::Hello { protocol: 12, minor: 0, client: "old tab".into(), caps: vec![] });
        let bad = ClientMsg::Hello { protocol: PROTOCOL_MAJOR, minor: 0, client: "x".into(), caps: vec!["NOT A CAP".into()] };
        assert!(bad.check_sizes().is_err());
        let w = ServerMsg::Welcome(Welcome::current());
        assert_eq!(ServerMsg::from_json(&w.to_json()).unwrap(), w);
        assert!(w.to_json().starts_with(r#"{"type":"welcome","protocol":13"#), "{}", w.to_json());
    }

    #[test]
    fn plugin_channel_round_trips_and_is_capped() {
        let c = ClientMsg::Plugin { plugin: "sheets".into(), msg: serde_json::json!({"op": "get", "cell": "A1"}) };
        assert_eq!(c.to_json(), r#"{"type":"plugin","plugin":"sheets","msg":{"cell":"A1","op":"get"}}"#);
        assert_eq!(ClientMsg::from_json(&c.to_json()).unwrap(), c);
        assert!(c.check_sizes().is_ok());
        let s = ServerMsg::Plugin { plugin: "sheets".into(), msg: serde_json::json!([1, 2]) };
        assert_eq!(ServerMsg::from_json(&s.to_json()).unwrap(), s);
        for bad in [
            ClientMsg::Plugin { plugin: "Not/an id".into(), msg: serde_json::Value::Null },
            ClientMsg::Plugin { plugin: "sheets".into(), msg: serde_json::Value::String("x".repeat(version::MAX_PLUGIN_MSG_BYTES)) },
        ] {
            assert!(bad.check_sizes().is_err());
        }
    }

    #[test]
    fn question_cards_round_trip_and_older_asks_still_parse() {
        let c = ClientMsg::AnswerQuestion { step: 4, text: "Metric".into() };
        assert_eq!(c.to_json(), r#"{"type":"answer_question","step":4,"text":"Metric"}"#);
        assert_eq!(ClientMsg::from_json(&c.to_json()).unwrap(), c);
        assert!(c.check_sizes().is_ok());
        assert!(ClientMsg::AnswerQuestion { step: 4, text: "x".repeat(MAX_ANSWER_CHARS + 1) }.check_sizes().is_err());
        let ask = Ask {
            text: "Which units?".into(),
            warning: None,
            always: true,
            choice: Some(Choice { options: vec!["Metric".into(), "Imperial".into()], free_text: true, answer: None }),
        };
        let json = serde_json::to_string(&ask).unwrap();
        assert_eq!(serde_json::from_str::<Ask>(&json).unwrap(), ask);
        // An ask from before 13.8 has no choice.
        let old: Ask = serde_json::from_str(r#"{"text":"t","always":true}"#).unwrap();
        assert!(old.choice.is_none());
        assert!(!serde_json::to_string(&old).unwrap().contains("choice"));
    }

    /// A newer peer's additions decode into fallbacks instead of failing the whole frame.
    #[test]
    fn unknown_messages_and_enum_values_decode_to_fallbacks() {
        assert_eq!(ServerMsg::from_json(r#"{"type":"hologram","x":1,"y":{"z":[]}}"#).unwrap(), ServerMsg::Unknown);
        assert_eq!(ClientMsg::from_json(r#"{"type":"teleport","to":"mars"}"#).unwrap(), ClientMsg::Unknown);
        // Malformed known messages are still errors.
        assert!(ServerMsg::from_json(r#"{"type":"notice","level":"info"}"#).is_err());
        let n = ServerMsg::from_json(r#"{"type":"notice","level":"debug","text":"t"}"#).unwrap();
        assert_eq!(n, ServerMsg::Notice { level: NoticeLevel::Info, text: "t".into() });
        let a = ClientMsg::from_json(r#"{"type":"approve","step":1,"answer":"maybe_later"}"#).unwrap();
        assert_eq!(a, ClientMsg::Approve { step: 1, answer: ApprovalAnswer::Unknown });
        // A step with a preview, a change, media, an artifact and a view this build doesn't know.
        let json = r#"{"id":1,"tool":"t","title":"T","access":"read","status":"done","detail":"",
            "changes":[{"kind":"chmod","path":"a","mode":"755"}],
            "preview":{"kind":"hexdump","bytes":"00"},
            "media":[{"kind":"video","path":"a.mp4"}],
            "views":[{"kind":"sheets.grid","v":4,"data":{"rows":[]},"fallback":"a *grid*"}]}"#;
        let s: Step = serde_json::from_str(json).unwrap();
        assert_eq!((s.changes.clone(), s.preview.clone()), (vec![FileChange::Other], Some(Preview::Other)));
        assert_eq!(s.media[0].kind, MediaKind::File);
        assert_eq!((s.views[0].kind.as_str(), s.views[0].fallback.as_str()), ("sheets.grid", "a *grid*"));
        assert!(s.view::<PluginCard>().is_none());
        let art: Artifact = serde_json::from_str(r#"{"id":"a","kind":"spreadsheet","title":"A","filename":"a.xlsx","versions":[]}"#).unwrap();
        assert_eq!(art.kind, ArtifactKind::Text);
        assert_eq!(serde_json::from_str::<SlashKind>(r#""macro""#).unwrap(), SlashKind::Other);
        assert_eq!(serde_json::from_str::<PluginAction>(r#""forked""#).unwrap(), PluginAction::Other);
        let env = ClientMsg::from_json(r#"{"type":"workspace_env","action":{"kind":"rebuild","fast":true}}"#).unwrap();
        assert_eq!(env, ClientMsg::WorkspaceEnv { action: Some(EnvAction::Unsupported), workspace: None });
        let d = ServerMsg::from_json(r#"{"type":"delegation","entry":1,"step":2,"update":{"kind":"progress","pct":5}}"#).unwrap();
        assert_eq!(d, ServerMsg::Delegation { entry: 1, step: 2, update: DelegationUpdate::Other });
    }

    #[test]
    fn plugin_cards_travel_as_views() {
        let card = PluginCard {
            plugin: "scratch".into(),
            scope: PluginScope::Chat,
            action: PluginAction::Made,
            tools: vec!["shout".into()],
            added: vec!["shout".into()],
            removed: vec![],
            test: Some(plugins::PluginTest { ok: true, output: "HI".into() }),
            problems: vec![],
            missing_settings: vec![],
        };
        let mut s = step(1, StepStatus::Done);
        s.views.push(View::of(&card));
        let json = ServerMsg::StepUpdated { entry: 1, step: s }.to_json();
        assert!(json.contains(r#""views":[{"kind":"byob.plugin_card","v":1,"data":{"action":"made""#), "{json}");
        let ServerMsg::StepUpdated { step: back, .. } = ServerMsg::from_json(&json).unwrap() else { panic!() };
        assert_eq!(back.view::<PluginCard>(), Some(card));
        assert!(back.views[0].fallback.contains("**Plugin scratch** (this chat) · tool made") && back.views[0].fallback.contains("shout"));
        // No views: nothing on the wire.
        assert!(!ServerMsg::StepUpdated { entry: 1, step: step(2, StepStatus::Done) }.to_json().contains("views"));
    }

    #[test]
    fn state_never_carries_api_key() {
        let json = serde_json::to_string(&EngineState::default()).unwrap();
        assert!(!json.contains("\"api_key\""));
    }
}
