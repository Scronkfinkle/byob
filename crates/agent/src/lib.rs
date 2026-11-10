//! Headless agent engine.
//!
//! The engine runs on its own thread and owns all state. Clients (the desktop window, each
//! browser tab via `byob-server`) attach with [`EngineHandle::connect`] and speak only
//! [`byob_protocol`] messages.
//!
//! Chats live in `byob-store`. Each client looks at one chat at a time (desktop and phone may
//! look at different ones); turns run in the engine regardless of who is watching, one per
//! chat, and entry updates only go to the clients looking at that chat. Streamed text is
//! written to the database in batches (every [`FLUSH_EVERY`]), not per token.
//!
//! It also owns the multi-round tool-calling loop (`turn`), the permission gate
//! ([`permissions`]) and pending approvals: a turn that needs approval blocks until a
//! client answers the approval card, or the turn is stopped.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

mod adapt;
#[cfg(test)]
mod adapt_tests;
pub mod attachments;
mod chats;
mod compact;
#[cfg(test)]
mod compact_tests;
mod connect;
#[cfg(test)]
mod connect_tests;
mod delegate;
#[cfg(test)]
mod delegate_tests;
pub mod dev_tools;
#[cfg(test)]
mod engine_tests;
mod limits;
#[cfg(test)]
mod loop_tests;
pub mod permissions;
mod queue;
#[cfg(test)]
mod queue_tests;
mod steps;
mod subagents;
#[cfg(test)]
mod test_store;
mod turn;
#[cfg(test)]
mod workspace_tests;
mod workspaces;

use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

pub use adapt::{AdaptOptions, ToolMode};
pub use byob_plugins::AgentDef;
use byob_protocol::{
    ApprovalAnswer, Artifact, ArtifactRef, ChatCursor, ChatSummary, ClientMsg, Connection, DelegationState, EngineState, Entry, EntryRole, Link, MediaRef,
    NoticeLevel, PROTOCOL_VERSION, ServerMsg, Step, StepStatus,
};
use byob_store::{ChatStore, GrantScope, NewMessage, Paths, Role, Secrets, Settings, StorageKind, Store, StoreError};
use byob_tools::Registry;
pub use byob_tools::artifact;
use byob_types::InferenceConfig;
pub use byob_types::config::Layer;
pub use compact::CompactOptions;
pub use delegate::SubagentOptions;
pub use permissions::{Policy, Rule};

/// How often streamed text is written to the database while a reply runs.
pub const FLUSH_EVERY: Duration = Duration::from_millis(250);
/// Chats in the first page of the list sent to a client when it says hello.
const FIRST_PAGE: usize = 50;

/// Everything the engine starts with.
#[derive(Clone)]
pub struct EngineConfig {
    pub inference: InferenceConfig,
    /// Folder new chats work in. Each chat keeps the folder it was created with.
    pub workspace: PathBuf,
    /// Tools offered to the model.
    pub tools: Registry,
    /// Forces this permission policy on every chat, whatever the approvals settings say
    /// (`--auto-approve`, tests). `None`: each chat's resolved approvals decide.
    pub policy: Option<Policy>,
    /// The global level of the layered settings (settings.json), apart from the endpoint,
    /// which is `inference`. See `byob_types::config`.
    pub defaults: Layer,
    /// API keys of workspaces and chats with their own endpoint (`secrets.json`
    /// `scoped_keys`, by `workspace:<id>` / `chat:<id>`).
    pub scoped_keys: BTreeMap<String, String>,
    /// Where "New workspace" creates fresh folders.
    pub workspaces_root: Option<PathBuf>,
    /// Register `workspace` as a workspace at startup (`--workspace`, or the saved
    /// default), rather than when its first chat is made.
    pub register_workspace: bool,
    /// System prompt for every turn (plugins and skills append to it).
    pub system_prompt: String,
    /// Where the database, settings and secrets live. `None` keeps chats in memory and
    /// saves nothing (tests, throwaway runs) — unless `storage` is a folder with
    /// `chats_folder` set.
    pub paths: Option<Paths>,
    /// Chat storage backend (`settings.json` `storage`, or `--storage`).
    pub storage: StorageKind,
    /// Root of the folder backend (default `<data>/chats-folder`).
    pub chats_folder: Option<PathBuf>,
    /// Name each chat with a short model request after its first reply (otherwise the
    /// first line of the first message stays the title).
    pub auto_title: bool,
    /// Endpoint probing and tool-calling mode.
    pub adapt: AdaptOptions,
    /// When to summarise a conversation that outgrows the context window.
    pub compaction: CompactOptions,
    /// Subagent definitions from the user's config folder and plugins (a chat's workspace
    /// `.byob/agents` and `.claude/agents` are read per turn and come first).
    pub agents: Vec<AgentDef>,
    /// Model roles (`settings.json` `models`): name → endpoint, for agents' `model:`.
    pub models: BTreeMap<String, InferenceConfig>,
    pub subagents: SubagentOptions,
    /// An endpoint has worked before, or the user named one (settings.json
    /// `inference.last_good`, `--inference-url`): no welcome screen.
    pub onboarded: bool,
    /// Until onboarded, look for inference servers on this machine's usual ports at startup.
    /// Off in this crate's unit tests.
    pub detect_local: bool,
    /// Workspace environment settings clients can see and change (`byob-exec`).
    pub envs: Option<Arc<dyn byob_tools::EnvControl>>,
}

impl EngineConfig {
    /// No tools, default policy, the current directory as workspace, chats in memory.
    pub fn new(inference: InferenceConfig) -> Self {
        let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            inference,
            workspace,
            tools: Registry::default(),
            policy: None,
            defaults: Layer::default(),
            scoped_keys: BTreeMap::new(),
            workspaces_root: None,
            register_workspace: false,
            system_prompt: turn::DEFAULT_SYSTEM_PROMPT.to_owned(),
            paths: None,
            storage: StorageKind::default(),
            chats_folder: None,
            auto_title: true,
            adapt: AdaptOptions::default(),
            compaction: CompactOptions::default(),
            agents: Vec::new(),
            models: BTreeMap::new(),
            subagents: SubagentOptions::default(),
            onboarded: false,
            detect_local: !cfg!(test),
            envs: None,
        }
    }
}

/// Called after the engine queues a message for a client, e.g. to request a UI repaint.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

type ClientId = u64;

enum Input {
    Connect {
        id: ClientId,
        tx: Sender<ServerMsg>,
        wake: Waker,
    },
    Disconnect {
        id: ClientId,
    },
    Client {
        id: ClientId,
        msg: ClientMsg,
    },
    Turn(turn::TurnEvent),
    /// An event of the subagent run shown by step `step` (of the event's entry).
    Child {
        step: u64,
        event: turn::TurnEvent,
    },
    /// The model suggested a title for a chat.
    Titled {
        chat: String,
        title: String,
    },
    Adapt(adapt::Event),
    /// The HTTP server asks where a chat's files are (to serve its artifacts).
    ChatWorkspace {
        chat: String,
        reply: Sender<Option<PathBuf>>,
    },
    /// A manual compaction (`ClientMsg::Compact`) finished.
    Compacted(Box<ManualDone>),
    /// A connection check, test or local server search finished.
    Link(connect::Event),
    /// A workspace environment request finished: the answer for `client`.
    Env {
        client: ClientId,
        env: byob_protocol::WorkspaceEnv,
    },
    /// A worker thread's answer for `client` (folder listings, container checks).
    Reply {
        client: ClientId,
        msg: Box<ServerMsg>,
    },
}

/// The outcome of a manual compaction.
struct ManualDone {
    chat: String,
    /// Last entry covered.
    through: u64,
    focus: Option<String>,
    tokens_before: usize,
    elapsed_ms: u64,
    /// The summary and the estimated tokens after.
    result: Result<(compact::Summary, usize), compact::Failure>,
}

/// Cheap, cloneable handle used to attach clients to a running engine.
#[derive(Clone)]
pub struct EngineHandle {
    inbox: Sender<Input>,
    next_id: Arc<std::sync::atomic::AtomicU64>,
}

impl EngineHandle {
    /// Start the engine thread with no tools and chats in memory.
    pub fn spawn(inference: InferenceConfig) -> std::io::Result<Self> {
        Self::spawn_with(EngineConfig::new(inference))
    }

    /// Open the database and start the engine thread. Replies that were still streaming
    /// when byob last stopped are closed with an error first.
    pub fn spawn_with(config: EngineConfig) -> std::io::Result<Self> {
        #[cfg(test)]
        let config = test_store::applied(config);
        let store = open_store(&config).map_err(std::io::Error::other)?;
        let (inbox, rx) = channel();
        let engine = Engine::new(config, store, inbox.clone());
        thread::Builder::new().name("byob-engine".into()).spawn(move || run(engine, rx))?;
        Ok(Self { inbox, next_id: Arc::default() })
    }

    /// The workspace folder of `chat`, or `None` if there is no such chat (or the engine
    /// doesn't answer within a few seconds). Used to serve artifacts over HTTP.
    pub fn chat_workspace(&self, chat: &str) -> Option<PathBuf> {
        let (reply, answer) = channel();
        self.inbox.send(Input::ChatWorkspace { chat: chat.to_owned(), reply }).ok()?;
        answer.recv_timeout(Duration::from_secs(5)).ok().flatten()
    }

    /// Attach a new client. `wake` is called whenever a message is queued for it.
    pub fn connect(&self, wake: Waker) -> LocalConnection {
        let id = self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = channel();
        // If the engine thread is gone, the connection simply never receives anything.
        let _ = self.inbox.send(Input::Connect { id, tx, wake });
        LocalConnection { id, inbox: self.inbox.clone(), rx }
    }
}

/// The chat store `config` asks for, via [`byob_store::open_backend`].
fn open_store(config: &EngineConfig) -> Result<Box<dyn ChatStore>, StoreError> {
    let store: Box<dyn ChatStore> = match (&config.paths, config.storage, &config.chats_folder) {
        (Some(p), storage, folder) => {
            p.ensure()?;
            let settings = Settings { storage, chats_folder: folder.clone(), ..Settings::default() };
            byob_store::open_backend(&settings, p)?
        }
        (None, StorageKind::Folder, Some(folder)) => Box::new(byob_store::FolderStore::open(folder)?),
        (None, _, _) => Box::new(Store::open_in_memory()?),
    };
    log::info!("chats: {:?} store at {}", store.kind(), store.path().display());
    #[cfg(test)]
    test_store::opened(store.kind());
    let cut = store.finish_interrupted(chats::INTERRUPTED)?;
    if cut > 0 {
        log::warn!("{cut} reply(s) were interrupted when byob last stopped");
    }
    Ok(store)
}

/// An attached client. Detaches on drop.
pub struct LocalConnection {
    id: ClientId,
    inbox: Sender<Input>,
    rx: Receiver<ServerMsg>,
}

impl Connection for LocalConnection {
    fn send(&mut self, msg: ClientMsg) {
        let _ = self.inbox.send(Input::Client { id: self.id, msg });
    }

    fn try_recv(&mut self) -> Option<ServerMsg> {
        self.rx.try_recv().ok()
    }
}

impl Drop for LocalConnection {
    fn drop(&mut self) {
        let _ = self.inbox.send(Input::Disconnect { id: self.id });
    }
}

struct Client {
    tx: Sender<ServerMsg>,
    wake: Waker,
    /// The chat this client looks at; `None` is a new chat that has no messages yet.
    chat: Option<String>,
    /// Said `Hello`: gets broadcasts (before that it has no transcript to apply them to).
    attached: bool,
    /// The new chat this client is about to start (`chat: None`): its workspace (else the
    /// default one), and the settings and key given to it before its first message.
    new_in: Option<String>,
    new_layer: Layer,
    new_key: Option<String>,
}

impl Client {
    fn new(tx: Sender<ServerMsg>, wake: Waker) -> Self {
        Self { tx, wake, chat: None, attached: false, new_in: None, new_layer: Layer::default(), new_key: None }
    }
}

/// An approval card waiting for an answer.
struct Pending {
    entry: u64,
    step: Step,
    reply: Sender<turn::Verdict>,
    /// The card is a subagent's: the `delegate` step it belongs to.
    child: Option<u64>,
}

/// A subagent run of a running turn (keyed by its `delegate` step id).
struct ChildRun {
    /// Its id in the store (`None` if it couldn't be recorded).
    run: Option<String>,
    /// Streamed (content, reasoning) not yet written to the store.
    unsaved: (String, String),
    /// Store `seq` of each of its steps, by step id.
    steps: BTreeMap<u64, i64>,
    started: Instant,
    model_calls: u64,
    ended: bool,
}

/// A chat whose transcript is in memory: someone looks at it or a reply runs in it.
struct Live {
    entries: Vec<Entry>,
    workspace: Arc<PathBuf>,
    /// The chat's artifacts (see [`byob_tools::artifact`]).
    artifacts: Vec<Artifact>,
    /// The chat's compactions, oldest first; the next request builds on the last one.
    compactions: Vec<byob_store::Compaction>,
    /// Token use, for the header meter.
    tally: compact::Tally,
}

/// A running turn, keyed by its reply entry.
struct Running {
    chat: String,
    cancel: Arc<AtomicBool>,
    /// Streamed (content, reasoning) not yet written to the store.
    unsaved: (String, String),
    saved_at: Instant,
    /// Store `seq` of each step, by step id.
    steps: BTreeMap<u64, i64>,
    /// The user's text, if this is the chat's first reply and it should be named after it.
    title_from: Option<String>,
    /// Its subagent runs, by `delegate` step id.
    children: BTreeMap<u64, ChildRun>,
    /// The chat's settings when the turn started: who decides on its calls, and which
    /// endpoint it talks to (for the title and the link status).
    policy: Policy,
    llm: byob_llm::Client,
}

struct Engine {
    config: InferenceConfig,
    llm: Result<byob_llm::Client, String>,
    clients: BTreeMap<ClientId, Client>,
    /// Sender into our own inbox, handed to turn threads.
    inbox: Sender<Input>,
    /// Message ids it issues are only valid while it is open (the folder backend renumbers
    /// them per run): never persist one, use positions or the store's own references.
    store: Box<dyn ChatStore>,
    paths: Option<Paths>,
    default_workspace: PathBuf,
    live: BTreeMap<String, Live>,
    turns: BTreeMap<u64, Running>,
    tools: Registry,
    /// A policy that beats the settings (`--auto-approve`, tests).
    forced_policy: Option<Policy>,
    /// Global level of the layered settings, apart from the endpoint (`config`).
    defaults: Layer,
    /// `--tool-mode` (anything but auto) beats the settings.
    forced: byob_types::config::Forced,
    /// Workspaces and each chat's own settings.
    workspaces: byob_store::Workspaces,
    /// Keys of workspaces and chats with their own endpoint, by `workspace:<id>` / `chat:<id>`.
    scoped_keys: BTreeMap<String, String>,
    /// Chat count and latest activity per workspace folder (refreshed when chats are
    /// made or deleted).
    folder_stats: Vec<byob_store::FolderStats>,
    workspaces_root: Option<PathBuf>,
    /// How endpoints other than the global one did, by [`adapt::key`].
    links: BTreeMap<String, Link>,
    system_prompt: String,
    /// Approval requests by step id.
    pending: BTreeMap<u64, Pending>,
    step_ids: Arc<AtomicU64>,
    /// Undo journals of replies that changed files.
    journals: steps::Journals,
    auto_title: bool,
    /// Chats still titled from their first line (the title we gave them): a model title
    /// may replace it, but never one the user chose.
    provisional: BTreeMap<String, String>,
    /// Probed endpoint capabilities (see [`adapt`]).
    endpoints: adapt::Endpoints,
    /// Files being uploaded, and finished uploads that may be attached.
    uploads: attachments::Uploads,
    compaction: CompactOptions,
    /// Manual compactions running, by chat (the flag stops them).
    compacting: BTreeMap<String, Arc<AtomicBool>>,
    /// Configured subagents (user folder, plugins).
    agents: Vec<AgentDef>,
    /// Model roles by name.
    models: BTreeMap<String, InferenceConfig>,
    subagents: SubagentOptions,
    /// Inference slots subagents may use, shared by every chat.
    slots: Arc<delegate::Slots>,
    envs: Option<Arc<dyn byob_tools::EnvControl>>,
    /// Whether the current endpoint answers (see [`connect`]).
    link: Link,
    /// Check endpoints when they are set (off in unit tests with scripted servers).
    checks: bool,
    onboarded: bool,
    /// The base URL saved as working, to save it only once.
    last_good: Option<String>,
    /// Local inference servers found, and whether a search is running.
    detected: Vec<byob_protocol::Detected>,
    detecting: bool,
    /// Clients with a connection test running.
    testing: BTreeSet<ClientId>,
    /// Messages waiting for their chat to be free, by chat (in memory only).
    queues: BTreeMap<String, queue::Queue>,
    /// Last id given to a queued message.
    queue_ids: u64,
}

fn run(mut engine: Engine, rx: Receiver<Input>) {
    loop {
        let input = match rx.recv_timeout(FLUSH_EVERY) {
            Ok(input) => Some(input),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        // A bug while handling one message must not take down the engine.
        let handled = catch_unwind(AssertUnwindSafe(|| {
            if let Some(input) = input {
                engine.handle(input);
            }
            engine.flush_due();
        }));
        if handled.is_err() {
            log::error!("engine panicked while handling a message; continuing");
        }
    }
}

impl Engine {
    fn new(config: EngineConfig, store: Box<dyn ChatStore>, inbox: Sender<Input>) -> Self {
        let llm = byob_llm::Client::new(config.inference.clone()).map_err(|e| e.to_string());
        let mut endpoints = adapt::Endpoints::new(config.adapt);
        if let Ok(client) = &llm {
            endpoints.probe(client, &inbox);
        }
        for (name, role) in &config.models {
            match byob_llm::Client::new(role.clone()) {
                Ok(client) => endpoints.probe(&client, &inbox),
                Err(e) => log::warn!("model role {name}: {e}"),
            }
        }
        let slots = Arc::new(delegate::Slots::new(config.subagents.parallel(None)));
        let detect = config.detect_local && !config.onboarded;
        let workspaces = byob_store::Workspaces::open(config.paths.as_ref()).value;
        let forced = byob_types::config::Forced { approvals: None, tool_mode: Some(config.adapt.tool_mode).filter(|m| *m != ToolMode::Auto) };
        let mut engine = Self {
            forced_policy: config.policy,
            defaults: config.defaults,
            forced,
            workspaces,
            scoped_keys: config.scoped_keys,
            folder_stats: Vec::new(),
            workspaces_root: config.workspaces_root,
            links: BTreeMap::new(),
            link: Link::Unknown,
            checks: config.adapt.probe,
            onboarded: config.onboarded,
            last_good: None,
            detected: Vec::new(),
            detecting: false,
            testing: BTreeSet::new(),
            queues: BTreeMap::new(),
            queue_ids: 0,
            endpoints,
            config: config.inference,
            llm,
            clients: BTreeMap::new(),
            inbox,
            store,
            paths: config.paths,
            default_workspace: config.workspace,
            live: BTreeMap::new(),
            turns: BTreeMap::new(),
            tools: config.tools,
            system_prompt: config.system_prompt,
            pending: BTreeMap::new(),
            step_ids: Arc::default(),
            journals: steps::Journals::default(),
            uploads: attachments::Uploads::default(),
            auto_title: config.auto_title,
            provisional: BTreeMap::new(),
            compaction: config.compaction,
            compacting: BTreeMap::new(),
            agents: config.agents,
            models: config.models,
            subagents: config.subagents,
            slots,
            envs: config.envs,
        };
        engine.check_link();
        if detect {
            engine.detect_endpoints();
        }
        if config.register_workspace {
            let dir = engine.default_workspace.clone();
            if let Err(e) = engine.workspaces.ensure(&dir) {
                log::warn!("registering the workspace {}: {e}", dir.display());
            }
        }
        engine.refresh_folder_stats();
        engine
    }

    // ---- chats ------------------------------------------------------------------------

    fn chat_busy(&self, chat: &str) -> bool {
        self.turns.values().any(|t| t.chat == chat) || self.compacting.contains_key(chat)
    }

    fn summary(&self, chat: &str) -> Option<ChatSummary> {
        match self.store.chat(chat) {
            Ok(Some(c)) => Some(self.chat_summary(&c)),
            Ok(None) => None,
            Err(e) => {
                log::error!("reading chat {chat}: {e}");
                None
            }
        }
    }

    fn client_chat(&self, id: ClientId) -> Option<String> {
        self.clients.get(&id).and_then(|c| c.chat.clone())
    }

    fn most_recent_chat(&self) -> Option<String> {
        match self.store.list_chats(1, None) {
            Ok(page) => page.chats.into_iter().next().map(|c| c.id),
            Err(e) => {
                log::error!("listing chats: {e}");
                None
            }
        }
    }

    /// The chat's transcript, loading it from the store if needed.
    fn live(&mut self, chat: &str) -> Result<&mut Live, String> {
        if !self.live.contains_key(chat) {
            let row = self.store.chat(chat).map_err(|e| e.to_string())?.ok_or_else(|| "that chat no longer exists".to_owned())?;
            let messages = self.store.messages(chat).map_err(|e| format!("couldn't load the chat: {e}"))?;
            // A damaged compaction list only costs context: the full history is used then.
            let compactions = self.store.compactions(chat).unwrap_or_else(|e| {
                log::error!("reading the compactions of {chat}: {e}");
                Vec::new()
            });
            let runs = self.store.agent_runs(chat).unwrap_or_else(|e| {
                log::error!("reading the subagent runs of {chat}: {e}");
                Vec::new()
            });
            let mut tally = compact::Tally::from_store(&messages, &compactions);
            tally.add_runs(&runs);
            let links = chats::run_links(&messages, &runs);
            let mut entries: Vec<Entry> = messages.into_iter().filter_map(|m| chats::entry(m, &self.step_ids)).collect();
            byob_store::migrate_workspace(&row.workspace);
            chats::attach_runs(&mut entries, links, runs, &self.step_ids);
            let artifacts = artifact::list(&row.workspace, chat);
            chats::restore_step_files(&mut entries, &artifacts);
            self.live.insert(chat.to_owned(), Live { entries, workspace: Arc::new(row.workspace), artifacts, compactions, tally });
        }
        self.live.get_mut(chat).ok_or_else(|| "that chat no longer exists".to_owned())
    }

    /// Forget transcripts nobody looks at and nothing runs in.
    fn gc(&mut self) {
        let keep: BTreeSet<&String> =
            self.clients.values().filter_map(|c| c.chat.as_ref()).chain(self.turns.values().map(|t| &t.chat)).chain(self.compacting.keys()).collect();
        self.live.retain(|k, _| keep.contains(k));
    }

    /// Point client `id` at `chat` and send it the state, the chat and its transcript.
    fn show_chat(&mut self, id: ClientId, chat: Option<String>) {
        self.show_chat_with(id, chat, true);
    }

    fn show_chat_with(&mut self, id: ClientId, chat: Option<String>, send_state: bool) {
        let (entries, artifacts, compactions) = match &chat {
            Some(c) => match self.live(c) {
                Ok(live) => (live.entries.clone(), live.artifacts.clone(), live.compactions.iter().map(chats::compaction).collect()),
                Err(text) => {
                    self.send_to(id, ServerMsg::Notice { level: NoticeLevel::Error, text });
                    return;
                }
            },
            None => (Vec::new(), Vec::new(), Vec::new()),
        };
        let summary = chat.as_deref().and_then(|c| self.summary(c));
        if let Some(client) = self.clients.get_mut(&id) {
            client.chat.clone_from(&chat);
        }
        let workspace = match &summary {
            Some(s) => s.workspace.clone(),
            None => self.new_chat_workspace(id).map(|w| w.id),
        };
        if send_state {
            self.send_to(id, ServerMsg::State(self.client_state(id)));
        }
        self.send_to(id, ServerMsg::ChatOpened { chat: summary, workspace });
        self.send_to(id, ServerMsg::Transcript { entries });
        self.send_to(id, ServerMsg::Compactions { chat: chat.clone(), items: compactions });
        if let Some(usage) = chat.as_deref().and_then(|c| self.usage(c)) {
            self.send_to(id, ServerMsg::Usage(usage));
        }
        self.send_to(id, ServerMsg::Queue(self.queue_view(chat.as_deref())));
        // Artifacts come last: clients take it as the end of opening a chat.
        self.send_to(id, ServerMsg::Artifacts { chat, artifacts });
        self.gc();
    }

    fn send_chats(&mut self, id: ClientId, after: Option<ChatCursor>, limit: usize) {
        let limit = limit.clamp(1, byob_store::MAX_PAGE);
        let pinned = if after.is_none() { self.store.pinned_chats() } else { Ok(Vec::new()) };
        let page = self.store.list_chats(limit, after.as_ref().map(chats::store_cursor).as_ref());
        match (pinned, page) {
            (Ok(pinned), Ok(page)) => {
                let pinned = pinned.iter().map(|c| self.chat_summary(c)).collect();
                let recent = page.chats.iter().filter(|c| !c.pinned()).map(|c| self.chat_summary(c)).collect();
                let next = page.next.as_ref().map(chats::protocol_cursor);
                self.send_to(id, ServerMsg::Chats { pinned, recent, after, next });
            }
            (Err(e), _) | (_, Err(e)) => self.notice(id, format!("couldn't list chats: {e}")),
        }
    }

    /// Create a chat titled after its first message, in the workspace `client` chose for
    /// it, with the settings it gave it beforehand.
    fn create_chat(&mut self, client: ClientId, first_message: &str) -> Result<String, StoreError> {
        let title = chats::heuristic_title(first_message);
        let folder = self.new_chat_workspace(client).map_or_else(|| self.default_workspace.clone(), |w| w.folder);
        let mut attempt = 0;
        let chat = loop {
            let id = chats::new_chat_id();
            match self.store.create_chat(&id, &title, &folder) {
                Ok(chat) => break chat,
                // Id collision (clock went backwards): try another.
                Err(e) if attempt < 3 => {
                    log::warn!("creating chat {id}: {e}; retrying");
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        };
        self.provisional.insert(chat.id.clone(), title);
        let live = Live {
            entries: Vec::new(),
            workspace: Arc::new(chat.workspace.clone()),
            artifacts: Vec::new(),
            compactions: Vec::new(),
            tally: compact::Tally::default(),
        };
        self.live.insert(chat.id.clone(), live);
        self.adopt_new_chat_settings(client, &chat.id);
        let new_workspace = self.ensure_workspace(&chat.workspace);
        self.broadcast(ServerMsg::ChatUpdated(self.chat_summary(&chat)));
        self.refresh_folder_stats();
        if new_workspace {
            log::info!("{} is a workspace now", chat.workspace.display());
        }
        self.broadcast_workspaces();
        Ok(chat.id)
    }

    /// Tell every client about a chat's new title, pin, activity or busy state.
    fn chat_changed(&mut self, chat: &str) {
        if let Some(s) = self.summary(chat) {
            self.broadcast(ServerMsg::ChatUpdated(s));
        }
    }

    fn rename_chat(&mut self, client: ClientId, chat: &str, title: &str) {
        let title = chats::clean_title(title, chats::MAX_TITLE_CHARS);
        if title.is_empty() {
            self.notice(client, "a chat title can't be empty".into());
            return;
        }
        match self.store.rename_chat(chat, &title) {
            Ok(()) => {
                self.provisional.remove(chat);
                self.chat_changed(chat);
            }
            Err(e) => self.notice(client, format!("couldn't rename the chat: {e}")),
        }
    }

    fn delete_chat(&mut self, client: ClientId, chat: &str) {
        if self.chat_busy(chat) {
            self.notice(client, "a reply is running in that chat; stop it first".into());
            return;
        }
        if let Err(e) = self.remove_chat(chat) {
            self.notice(client, e);
            return;
        }
        self.refresh_folder_stats();
        self.broadcast_workspaces();
    }

    /// Delete a chat with what byob keeps for it (undo backups, its settings and key) and
    /// tell every client. The caller refreshes the workspace list.
    fn remove_chat(&mut self, chat: &str) -> Result<(), String> {
        match self.store.delete_chat(chat) {
            Ok(tasks) => {
                if let Some(paths) = &self.paths {
                    for task in tasks.iter().filter(|t| !t.is_empty() && !t.contains(['/', '\\', '.'])) {
                        let _ = std::fs::remove_dir_all(paths.undo_dir().join(task));
                    }
                }
            }
            Err(e) => return Err(format!("couldn't delete the chat: {e}")),
        }
        self.live.remove(chat);
        self.queues.remove(chat);
        self.provisional.remove(chat);
        if let Err(e) = self.workspaces.forget_chat(chat) {
            log::warn!("forgetting the settings of chat {chat}: {e}");
        }
        self.set_scoped_key(&format!("chat:{chat}"), None);
        self.broadcast(ServerMsg::ChatDeleted { id: chat.to_owned() });
        let viewers: Vec<ClientId> = self.clients.iter().filter(|(_, c)| c.chat.as_deref() == Some(chat)).map(|(id, _)| *id).collect();
        for id in viewers {
            self.show_chat(id, None);
        }
        Ok(())
    }

    fn pin_chat(&mut self, client: ClientId, chat: &str, pinned: bool) {
        match self.store.pin_chat(chat, pinned) {
            Ok(()) => self.chat_changed(chat),
            Err(e) => self.notice(client, format!("couldn't pin the chat: {e}")),
        }
    }

    fn titled(&mut self, chat: &str, title: &str) {
        // Only replace the first-line title we chose; the user may have renamed it since.
        let current = self.store.chat(chat).ok().flatten().map(|c| c.title);
        if current.is_none() || current != self.provisional.get(chat).cloned() {
            return;
        }
        self.provisional.remove(chat);
        match self.store.rename_chat(chat, title) {
            Ok(()) => self.chat_changed(chat),
            Err(e) => log::warn!("naming chat {chat}: {e}"),
        }
    }

    // ---- clients ----------------------------------------------------------------------

    /// The state as the viewers of `chat` see it.
    fn state(&self, chat: Option<&str>) -> EngineState {
        let effective = chat.map(|c| self.chat_settings(Some(c), None));
        self.state_with(chat, effective)
    }

    /// The state as client `id` sees it (its chat, or the new chat it is about to start).
    fn client_state(&self, id: ClientId) -> EngineState {
        let chat = self.client_chat(id);
        let effective = self.chat_settings(chat.as_deref(), Some(id));
        self.state_with(chat.as_deref(), Some(effective))
    }

    fn state_with(&self, chat: Option<&str>, effective: Option<workspaces::ChatSettings>) -> EngineState {
        let tool_mode = effective.as_ref().map_or(ToolMode::Auto, |e| e.effective.tool_mode.value);
        let chat_endpoint = effective.filter(|e| e.inference != self.config).map(|e| {
            let key = adapt::key(&e.inference);
            byob_protocol::ChatEndpoint {
                base_url: e.inference.base_url.clone(),
                model: e.inference.model.clone(),
                from: e.effective.base_url.from.max(e.effective.model.from),
                has_api_key: e.inference.api_key.is_some(),
                link: if key == adapt::key(&self.config) { self.link.clone() } else { self.links.get(&key).cloned().unwrap_or_default() },
                info: Some(self.endpoints.info_with(&e.inference, e.effective.tool_mode.value)),
            }
        });
        EngineState {
            version: env!("CARGO_PKG_VERSION").into(),
            inference_base_url: self.config.base_url.clone(),
            inference_model: self.config.model.clone(),
            has_api_key: self.config.api_key.is_some(),
            endpoint: self.llm.as_ref().ok().map(|c| c.endpoint().to_string()),
            busy: chat.is_some_and(|c| self.chat_busy(c)),
            endpoint_info: Some(self.endpoints.info_with(&self.config, tool_mode)),
            link: self.link.clone(),
            onboarded: self.onboarded,
            detected: self.detected.clone(),
            detecting: self.detecting,
            chat_endpoint,
        }
    }

    /// Send every client the state as it sees it (busy is per chat).
    fn broadcast_state(&mut self) {
        let ids: Vec<ClientId> = self.clients.iter().filter(|(_, c)| c.attached).map(|(id, _)| *id).collect();
        for id in ids {
            let state = self.client_state(id);
            self.send_to(id, ServerMsg::State(state));
        }
    }

    fn handle(&mut self, input: Input) {
        match input {
            Input::Connect { id, tx, wake } => {
                self.clients.insert(id, Client::new(tx, wake));
            }
            Input::Disconnect { id } => {
                self.clients.remove(&id);
                self.gc();
                self.uploads.forget(id);
                self.testing.remove(&id);
            }
            Input::Client { id, msg } => self.handle_client(id, msg),
            Input::Turn(event) => self.handle_turn(event),
            Input::Child { step, event } => self.handle_child(step, event),
            Input::Titled { chat, title } => self.titled(&chat, &title),
            Input::Adapt(event) => {
                self.endpoints.handle(event);
                self.slots.set_capacity(self.subagents.parallel(self.endpoints.slots(&self.config)));
                self.broadcast_state();
                // The window (and so the threshold) may have changed.
                let viewed: BTreeSet<String> = self.clients.values().filter_map(|c| c.chat.clone()).collect();
                for chat in viewed {
                    self.send_usage(&chat);
                }
            }
            Input::ChatWorkspace { chat, reply } => {
                let _ = reply.send(self.chat_workspace(&chat));
            }
            Input::Compacted(done) => self.manual_compacted(*done),
            Input::Link(event) => self.handle_connect(event),
            Input::Env { client, env } => self.send_to(client, ServerMsg::WorkspaceEnv(env)),
            Input::Reply { client, msg } => self.send_to(client, *msg),
        }
    }

    fn handle_client(&mut self, id: ClientId, msg: ClientMsg) {
        if let Err(e) = msg.check_sizes() {
            self.notice(id, e);
            return;
        }
        match msg {
            ClientMsg::Hello { protocol, client } => {
                if protocol != PROTOCOL_VERSION {
                    let text = format!("protocol mismatch: client {protocol}, engine {PROTOCOL_VERSION}; reload the page");
                    self.send_to(id, ServerMsg::Notice { level: NoticeLevel::Error, text });
                    return;
                }
                log::info!("client {id} attached: {client}");
                if let Some(c) = self.clients.get_mut(&id) {
                    c.attached = true;
                }
                let mut chat = self.client_chat(id).or_else(|| self.most_recent_chat());
                if let Some(c) = chat.clone()
                    && let Err(e) = self.live(&c)
                {
                    log::error!("opening chat {c}: {e}");
                    chat = None;
                }
                if let Some(c) = self.clients.get_mut(&id) {
                    c.chat.clone_from(&chat);
                }
                // State first: it is the documented answer to Hello.
                self.send_to(id, ServerMsg::State(self.client_state(id)));
                self.send_to(id, self.workspaces_msg());
                self.send_chats(id, None, FIRST_PAGE);
                self.show_chat_with(id, chat, false);
            }
            ClientMsg::SetInference { base_url, model, api_key } => self.set_inference(id, base_url, model, api_key),
            ClientMsg::TestInference { base_url, model, api_key } => self.test_inference(id, base_url, model, api_key),
            ClientMsg::DetectEndpoints => self.detect_endpoints(),
            ClientMsg::Retry => self.retry(id),
            ClientMsg::SendMessage { text, attachments } => self.start_turn(id, text, &attachments),
            ClientMsg::Unqueue { id: item } => self.unqueue(id, item),
            ClientMsg::EditQueued { id: item, text, attachments } => self.edit_queued(id, item, text, &attachments),
            ClientMsg::MoveQueued { id: item, to } => self.move_queued(id, item, to),
            ClientMsg::SendQueuedNow { id: item } => self.send_queued_now(id, item),
            ClientMsg::ResumeQueue => self.resume_queue(id),
            ClientMsg::Cancel => {
                if let Some(chat) = self.client_chat(id) {
                    self.stop_chat(&chat);
                }
            }
            ClientMsg::Approve { step, answer } => self.answer(step, answer),
            ClientMsg::Undo { entry } => self.undo(id, entry),
            ClientMsg::ListChats { after, limit } => self.send_chats(id, after, usize::try_from(limit).unwrap_or(FIRST_PAGE)),
            ClientMsg::NewChat { workspace } => self.new_chat(id, workspace),
            ClientMsg::OpenChat { id: chat } => self.show_chat(id, Some(chat)),
            ClientMsg::RenameChat { id: chat, title } => self.rename_chat(id, &chat, &title),
            ClientMsg::DeleteChat { id: chat } => self.delete_chat(id, &chat),
            ClientMsg::PinChat { id: chat, pinned } => self.pin_chat(id, &chat, pinned),
            ClientMsg::Upload { upload, name, size, offset, data, done } => {
                let workspace = self.client_workspace(id);
                let chunk = attachments::Chunk { upload, name: &name, size, offset, data: &data, done };
                if let Some(reply) = self.uploads.chunk(&workspace, id, &chunk) {
                    self.send_to(id, reply);
                }
            }
            ClientMsg::GetMedia { path } => self.serve_media(id, path),
            ClientMsg::GetArtifact { id: artifact, version } => self.serve_artifact(id, artifact, version),
            ClientMsg::Compact { focus } => self.compact_chat(id, focus),
            ClientMsg::WorkspaceEnv { action, workspace } => self.workspace_env(id, action, workspace),
            ClientMsg::CreateWorkspace { name, folder, config } => self.create_workspace(id, &name, folder, config),
            ClientMsg::RenameWorkspace { id: ws, name } => self.rename_workspace(id, &ws, &name),
            ClientMsg::RemoveWorkspace { id: ws, clean } => self.remove_workspace(id, &ws, clean),
            ClientMsg::GetConfig { scope } => self.send_config(id, scope),
            ClientMsg::SetConfig { scope, layer, api_key } => self.set_config(id, scope, layer, api_key),
            ClientMsg::ListFolders { path } => self.list_folders(id, path),
            ClientMsg::CheckContainers => self.check_containers(id),
        }
    }

    /// Stop what runs in `chat` (its turn, or a manual compaction); pending approvals are denied.
    pub(crate) fn stop_chat(&mut self, chat: &str) {
        for t in self.turns.values().filter(|t| t.chat == chat) {
            t.cancel.store(true, Ordering::Relaxed);
        }
        if let Some(flag) = self.compacting.get(chat) {
            flag.store(true, Ordering::Relaxed);
        }
        self.deny_pending(|_, t| t.is_some_and(|t| t.chat == chat), "stopped");
    }

    /// `ClientMsg::WorkspaceEnv`: apply `action` to workspace `workspace` (default: the one
    /// of the chat `client` looks at) and send back its environment, from a worker thread
    /// (stopping a container takes seconds).
    fn workspace_env(&mut self, client: ClientId, action: Option<byob_protocol::EnvAction>, workspace: Option<String>) {
        let (ws, plan) = match workspace {
            Some(id) => match self.workspaces.get(&id) {
                Some(w) => (Arc::new(w.folder.clone()), self.workspace_plan(&id)),
                None => {
                    self.notice(client, "that workspace no longer exists".into());
                    return;
                }
            },
            None => {
                let ws = self.client_workspace(client);
                let plan = self.workspaces.by_folder(&ws).map(|w| w.id.clone()).map(|id| self.workspace_plan(&id)).unwrap_or_default();
                (ws, plan)
            }
        };
        let Some(envs) = self.envs.clone() else {
            let env = byob_protocol::WorkspaceEnv {
                workspace: ws.display().to_string(),
                message: Some("Workspace environments aren't available in this build.".into()),
                failed: true,
                ..Default::default()
            };
            self.send_to(client, ServerMsg::WorkspaceEnv(env));
            return;
        };
        let inbox = self.inbox.clone();
        let spawned = thread::Builder::new().name("byob-env".into()).spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                let done = action.map(|a| envs.apply(&ws, &a));
                let mut env = envs.describe(&ws, &plan);
                match done {
                    Some(Ok(text)) => env.message = Some(text).filter(|t| !t.is_empty()),
                    Some(Err(e)) => {
                        env.message = Some(e);
                        env.failed = true;
                    }
                    None => {}
                }
                env
            }));
            let env =
                result.unwrap_or_else(|_| byob_protocol::WorkspaceEnv { message: Some("that failed unexpectedly".into()), failed: true, ..Default::default() });
            let _ = inbox.send(Input::Env { client, env });
        });
        if let Err(e) = spawned {
            log::error!("workspace environment thread: {e}");
        }
    }

    /// The workspace folder of the chat `client` looks at (for a new chat, the one it will
    /// be created in).
    fn client_workspace(&mut self, client: ClientId) -> Arc<PathBuf> {
        let chat = self.client_chat(client);
        if let Some(ws) = chat.and_then(|c| self.live(&c).ok().map(|l| Arc::clone(&l.workspace))) {
            return ws;
        }
        Arc::new(self.new_chat_workspace(client).map_or_else(|| self.default_workspace.clone(), |w| w.folder))
    }

    /// Load a chart or image for one client off the engine thread (decoding can take a while).
    fn serve_media(&mut self, id: ClientId, path: String) {
        let referenced = self.client_chat(id).and_then(|c| self.live.get(&c)).is_some_and(|l| attachments::referenced(&l.entries, &path));
        if !referenced {
            let content = byob_protocol::MediaContent::Error { message: "not part of this conversation".into() };
            self.send_to(id, ServerMsg::Media { path, content });
            return;
        }
        let workspace = self.client_workspace(id);
        let Some(client) = self.clients.get(&id) else { return };
        let (tx, wake) = (client.tx.clone(), Arc::clone(&client.wake));
        let spawned = thread::Builder::new().name("byob-media".into()).spawn(move || {
            let content = catch_unwind(AssertUnwindSafe(|| attachments::load(&workspace, &path)))
                .unwrap_or_else(|_| byob_protocol::MediaContent::Error { message: "internal error while loading".into() });
            if tx.send(ServerMsg::Media { path, content }).is_ok() {
                wake();
            }
        });
        if let Err(e) = spawned {
            log::error!("could not start a media thread: {e}");
        }
    }

    /// Send one artifact version's bytes to a client, read off the engine thread. Only
    /// artifacts of the chat the client looks at.
    fn serve_artifact(&mut self, id: ClientId, artifact: String, version: u32) {
        let chat = self.client_chat(id);
        let known = chat.as_ref().and_then(|c| self.live.get(c)).is_some_and(|l| l.artifacts.iter().any(|a| a.id == artifact));
        let Some(chat) = chat.filter(|_| known) else {
            self.send_to(id, ServerMsg::ArtifactFile { id: artifact, version, data: None, error: Some("not part of this conversation".into()) });
            return;
        };
        let workspace = self.client_workspace(id);
        let Some(client) = self.clients.get(&id) else { return };
        let (tx, wake) = (client.tx.clone(), Arc::clone(&client.wake));
        let spawned = thread::Builder::new().name("byob-artifact".into()).spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| attachments::artifact_for_client(&workspace, &chat, &artifact, version)))
                .unwrap_or_else(|_| Err("internal error while loading".into()));
            let (data, error) = match result {
                Ok(bytes) => (Some(byob_protocol::media::encode(&bytes)), None),
                Err(e) => (None, Some(e)),
            };
            if tx.send(ServerMsg::ArtifactFile { id: artifact, version, data, error }).is_ok() {
                wake();
            }
        });
        if let Err(e) = spawned {
            log::error!("could not start an artifact thread: {e}");
        }
    }

    /// A step made artifact versions: re-read those artifacts and tell the chat's viewers.
    fn artifacts_changed(&mut self, chat: &str, refs: &[ArtifactRef]) {
        let Some(live) = self.live.get_mut(chat) else { return };
        let mut changed = Vec::new();
        for r in refs {
            match artifact::load_in_chat(&live.workspace, chat, &r.id) {
                Ok(meta) => {
                    match live.artifacts.iter_mut().find(|a| a.id == meta.artifact.id) {
                        Some(a) => *a = meta.artifact.clone(),
                        None => live.artifacts.push(meta.artifact.clone()),
                    }
                    changed.push(meta.artifact);
                }
                Err(e) => log::warn!("artifact {}: {e}", r.id),
            }
        }
        for a in changed {
            self.send_viewers(chat, ServerMsg::ArtifactUpdated(a));
        }
    }

    /// The workspace of `chat`, for serving its artifacts over HTTP.
    fn chat_workspace(&mut self, chat: &str) -> Option<PathBuf> {
        if let Some(live) = self.live.get(chat) {
            return Some(live.workspace.as_ref().clone());
        }
        self.store.chat(chat).ok().flatten().map(|c| c.workspace)
    }

    fn set_inference(&mut self, id: ClientId, base_url: String, model: String, api_key: Option<String>) {
        if let Err(e) = connect::check_lengths(&base_url, &model, api_key.as_deref()) {
            self.notice(id, e);
            return;
        }
        let (base_url, model) = (base_url.trim().to_owned(), model.trim().to_owned());
        let config = InferenceConfig { api_key: self.key_for(&base_url, api_key), base_url, model };
        let new_key = config.api_key != self.config.api_key;
        match byob_llm::Client::new(config.clone()) {
            Ok(client) => {
                if let Err(e) = self.save_inference(&config, new_key) {
                    self.notice(id, format!("inference settings apply now but couldn't be saved: {e}"));
                }
                self.config = config;
                self.endpoints.probe(&client, &self.inbox);
                self.llm = Ok(client);
                self.link = Link::Unknown;
                self.check_link();
                self.broadcast_state();
            }
            Err(e) => self.notice(id, e.to_string()),
        }
    }

    /// Remember the endpoint in settings.json and a changed API key in secrets.json (only there).
    fn save_inference(&self, config: &InferenceConfig, new_key: bool) -> Result<(), StoreError> {
        let Some(paths) = &self.paths else { return Ok(()) };
        Settings::update(&paths.settings(), |s| {
            s.inference.base_url = Some(config.base_url.clone());
            s.inference.model = Some(config.model.clone());
        })?;
        if new_key {
            let mut secrets = Secrets::load(&paths.secrets()).value;
            secrets.api_key.clone_from(&config.api_key);
            secrets.save(&paths.secrets())?;
        }
        Ok(())
    }

    // ---- approvals --------------------------------------------------------------------

    /// A client answered an approval card. The first answer wins; late or unknown ids are ignored.
    fn answer(&mut self, step_id: u64, answer: ApprovalAnswer) {
        let Some(Pending { entry, mut step, reply, child }) = self.pending.remove(&step_id) else { return };
        let chat = self.turns.get(&entry).map(|t| t.chat.clone());
        let scope = match answer {
            // Questions and must-confirm calls are never granted for good.
            _ if step.ask.is_some() => None,
            ApprovalAnswer::Always => chat.map(GrantScope::Chat),
            ApprovalAnswer::AlwaysEverywhere => Some(GrantScope::Global),
            ApprovalAnswer::Allow | ApprovalAnswer::Deny => None,
        };
        if let Some(scope) = scope
            && let Err(e) = self.store.grant(&scope, &step.tool)
        {
            log::error!("saving the grant for {}: {e}", step.tool);
        }
        let verdict = turn::Verdict::Answered(answer != ApprovalAnswer::Deny);
        let denied = verdict.denied();
        step.status = if denied.is_none() { StepStatus::Running } else { StepStatus::Denied };
        if let Some(reason) = denied {
            step.detail = reason;
        }
        // The turn thread may be gone (stopped); the step update still settles the card.
        let _ = reply.send(verdict);
        self.settle_step(entry, child, step);
    }

    /// Update a step of entry `entry`, or of its subagent run `child`.
    fn settle_step(&mut self, entry: u64, child: Option<u64>, step: Step) {
        match child {
            Some(parent) => self.update_child_step(entry, parent, step),
            None => self.update_step(entry, step),
        }
    }

    /// Deny the pending approvals of turns matching `which`.
    fn deny_pending(&mut self, which: impl Fn(&Pending, Option<&Running>) -> bool, reason: &str) {
        let ids: Vec<u64> = self.pending.iter().filter(|(_, p)| which(p, self.turns.get(&p.entry))).map(|(id, _)| *id).collect();
        for id in ids {
            let Some(Pending { entry, mut step, reply, child }) = self.pending.remove(&id) else { continue };
            let _ = reply.send(turn::Verdict::Deny(reason.to_owned()));
            step.status = StepStatus::Denied;
            reason.clone_into(&mut step.detail);
            self.settle_step(entry, child, step);
        }
    }

    /// A turn (or one of its subagents, `child`) asks whether a step may run.
    fn gate(&mut self, entry: u64, mut step: Step, reply: Sender<turn::Verdict>, child: Option<u64>) {
        let Some(run) = self.turns.get(&entry).filter(|t| !t.cancel.load(Ordering::Relaxed)) else {
            let _ = reply.send(turn::Verdict::Deny("stopped".into()));
            return;
        };
        let granted = self.store.is_allowed(&run.chat, &step.tool).unwrap_or_else(|e| {
            log::error!("reading grants: {e}");
            false
        });
        let always = step.ask.as_ref().map(|a| a.always);
        match permissions::decide_ask(&run.policy, granted, step.access, always) {
            permissions::Gate::Allow => {
                let _ = reply.send(turn::Verdict::Allow);
            }
            permissions::Gate::Deny(reason) => {
                let _ = reply.send(turn::Verdict::Deny(reason));
            }
            permissions::Gate::Ask => {
                step.status = StepStatus::Waiting;
                self.settle_step(entry, child, step.clone());
                self.pending.insert(step.id, Pending { entry, step, reply, child });
            }
        }
    }

    // ---- turns ------------------------------------------------------------------------

    fn reply_mut(&mut self, entry: u64) -> Option<&mut Entry> {
        let chat = &self.turns.get(&entry)?.chat;
        self.live.get_mut(chat)?.entries.iter_mut().rev().find(|e| e.id == entry)
    }

    fn update_step(&mut self, entry: u64, mut step: Step) {
        if step.delegation.is_some() {
            // The turn only reports a delegation's state; its transcript lives here.
            let existing = self.reply_mut(entry).and_then(|e| e.steps.iter().find(|s| s.id == step.id)).and_then(|s| s.delegation.clone());
            if let (Some(old), Some(new)) = (existing, step.delegation.as_deref_mut()) {
                new.content = old.content;
                new.reasoning = old.reasoning;
                new.steps = old.steps;
                new.prompt_tokens = new.prompt_tokens.max(old.prompt_tokens);
                new.completion_tokens = new.completion_tokens.max(old.completion_tokens);
            }
        }
        self.persist_step(entry, &step);
        if step.delegation.is_some() {
            self.track_delegation(entry, &step);
        }
        let Some(chat) = self.turns.get(&entry).map(|t| t.chat.clone()) else { return };
        if step.status.finished() && !step.artifacts.is_empty() {
            self.artifacts_changed(&chat, &step.artifacts);
        }
        if let Some(e) = self.reply_mut(entry) {
            e.upsert_step(step.clone());
        }
        self.send_viewers(&chat, ServerMsg::StepUpdated { entry, step });
    }

    fn persist_step(&mut self, entry: u64, step: &Step) {
        let Some(run) = self.turns.get_mut(&entry) else { return };
        let message = chats::message_id(entry);
        let seq = match run.steps.get(&step.id) {
            Some(seq) => *seq,
            None => match self.store.add_step(message, &chats::new_step(step)) {
                Ok(seq) => {
                    run.steps.insert(step.id, seq);
                    seq
                }
                Err(e) => {
                    log::error!("saving step {}: {e}", step.tool);
                    return;
                }
            },
        };
        if step.status.finished()
            && let Err(e) = self.store.finish_step(message, seq, &chats::step_result(step))
        {
            log::error!("saving step result {}: {e}", step.tool);
        }
    }

    /// Append a message to a chat (store, transcript, viewers). Returns its entry id.
    fn add_message(&mut self, chat: &str, m: &NewMessage, attachments: Vec<MediaRef>) -> Result<u64, StoreError> {
        let id = chats::entry_id(self.store.append_message(chat, m)?);
        let role = if m.role == Role::User { EntryRole::User } else { EntryRole::Assistant };
        let entry = Entry {
            id,
            role,
            content: m.content.clone(),
            reasoning: m.reasoning.clone(),
            done: m.done,
            error: None,
            stats: None,
            steps: Vec::new(),
            undoable: false,
            undone: false,
            attachments,
        };
        if let Some(live) = self.live.get_mut(chat) {
            live.entries.push(entry.clone());
        }
        self.send_viewers(chat, ServerMsg::EntryAdded(entry));
        Ok(id)
    }

    /// `ClientMsg::SendMessage`: start a turn, or queue the message while one runs.
    fn start_turn(&mut self, client: ClientId, text: String, attached: &[String]) {
        let text = text.trim().to_owned();
        let busy = self.client_chat(client).filter(|c| self.chat_busy(c));
        if busy.is_none()
            && let Some(focus) = compact::command(&text)
        {
            self.compact_chat(client, focus);
            return;
        }
        let attached = match self.uploads.claim(attached) {
            Ok(a) => a,
            Err(text) => {
                self.send_to(client, ServerMsg::Notice { level: NoticeLevel::Error, text });
                return;
            }
        };
        if text.is_empty() && attached.is_empty() {
            return;
        }
        match busy {
            Some(chat) => self.enqueue(client, &chat, text, attached),
            None => self.begin_turn(client, text, attached),
        }
    }

    /// Add the user's message (and a reply entry) to the client's chat, creating the chat if
    /// it is new, and start the turn.
    fn begin_turn(&mut self, client: ClientId, text: String, attached: Vec<MediaRef>) {
        let chat = match self.client_chat(client) {
            Some(chat) => chat,
            None => {
                // Check the endpoint the new chat would use before creating it.
                let settings = self.chat_settings(None, Some(client));
                if let Err(e) = self.llm_for(&settings.inference) {
                    self.notice(client, format!("inference URL is invalid: {e}"));
                    return;
                }
                match self.create_chat(client, &text) {
                    Ok(chat) => {
                        self.show_chat(client, Some(chat.clone()));
                        chat
                    }
                    Err(e) => {
                        self.notice(client, format!("couldn't create the chat: {e}"));
                        return;
                    }
                }
            }
        };
        if self.chat_busy(&chat) {
            self.notice(client, "a reply is still running in this chat; stop it first".into());
            return;
        }
        if let Err(e) = self.begin_turn_in(&chat, text, attached) {
            self.notice(client, e);
        }
    }

    /// Add the user's message (and a reply entry) to `chat`, which isn't busy, and start the
    /// turn. Nothing is added if it can't start.
    pub(crate) fn begin_turn_in(&mut self, chat: &str, text: String, attached: Vec<MediaRef>) -> Result<(), String> {
        let chat = chat.to_owned();
        // The chat's settings: global, then its workspace's, then its own.
        let settings = self.chat_settings(Some(&chat), None);
        let llm = self.llm_for(&settings.inference).map_err(|e| format!("inference URL is invalid: {e}"))?;
        let eff = &settings.effective;
        let plan = self.endpoints.plan_with(&settings.inference, eff.tool_mode.value);
        let workspace_dir = self.live(&chat).map(|l| Arc::clone(&l.workspace)).ok();
        let delegation = workspace_dir.as_deref().filter(|_| eff.subagents.value).and_then(|ws| self.delegation_env(&settings.inference, &llm, &plan, ws));
        let mut base_prompt = self.prompt_with_instructions(eff);
        if let Some(env) = &delegation {
            base_prompt = format!("{base_prompt}\n\n{}", delegate::prompt_section(&env.agents));
        }
        let compaction = self.compact_options(eff);
        let policy = self.policy_for(eff);
        let (history, ids, prior, workspace, first) = {
            let live = self.live(&chat)?;
            let system_prompt = system_prompt(&base_prompt, live);
            let (history, ids) = compact::history(&live.entries, latest(live), Some(&text), &system_prompt);
            let prior = live.entries.last().map_or(0, |e| e.id);
            (history, ids, prior, Arc::clone(&live.workspace), live.entries.is_empty())
        };
        let added = self
            .add_message(&chat, &NewMessage::user(text.clone()), attached.clone())
            .and_then(|_| self.add_message(&chat, &NewMessage::assistant_streaming(None), Vec::new()));
        let reply = added.map_err(|e| format!("couldn't save the message: {e}"))?;
        let mut history = history;
        if let Some(last) = history.last_mut().filter(|_| !attached.is_empty()) {
            *last = attachments::user_message(&workspace, &text, &attached);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let title_from = (first && self.auto_title && self.provisional.contains_key(&chat)).then(|| text.clone());
        self.turns.insert(
            reply,
            Running {
                chat: chat.clone(),
                cancel: Arc::clone(&cancel),
                unsaved: Default::default(),
                saved_at: Instant::now(),
                steps: BTreeMap::new(),
                title_from,
                children: BTreeMap::new(),
                policy,
                llm: llm.clone(),
            },
        );
        self.chat_changed(&chat);
        self.viewer_states(&chat);
        let setup = turn::Setup {
            llm,
            history,
            entry: reply,
            cancel,
            inbox: self.inbox.clone(),
            tools: self.tools.clone(),
            workspace,
            chat: chat.clone(),
            step_ids: Arc::clone(&self.step_ids),
            compact: compaction.auto.then(|| turn::AutoCompact { at: compaction.threshold(&plan), keep_turns: compaction.keep_turns }),
            plan,
            ids,
            prior,
            child: None,
            parent_cancel: None,
            max_calls: limits::MAX_CALLS,
            delegate: delegation.map(Arc::new),
            env: Arc::new(settings.env.clone()),
        };
        if let Err(e) = turn::spawn(setup) {
            self.handle_turn(turn::TurnEvent::Finished { id: reply, error: Some(format!("could not start turn: {e}")), stats: None });
        }
        Ok(())
    }

    fn handle_turn(&mut self, event: turn::TurnEvent) {
        match event {
            turn::TurnEvent::Delta { id, content, reasoning } => {
                let Some(run) = self.turns.get_mut(&id) else { return };
                run.unsaved.0.push_str(&content);
                run.unsaved.1.push_str(&reasoning);
                let chat = run.chat.clone();
                if let Some(entry) = self.reply_mut(id) {
                    entry.append(&content, &reasoning);
                }
                self.send_viewers(&chat, ServerMsg::EntryDelta { id, content, reasoning });
            }
            turn::TurnEvent::Step { id, step } => {
                // A pending card keeps its Waiting status until answered.
                if !self.pending.contains_key(&step.id) {
                    self.update_step(id, step);
                }
            }
            turn::TurnEvent::Gate { id, step, reply } => self.gate(id, step, reply, None),
            turn::TurnEvent::Undo { id, ops, complete } => self.add_undo(id, ops, complete),
            turn::TurnEvent::Finished { id, error, stats } => self.finish_turn(id, error, stats),
            turn::TurnEvent::Usage { id, prompt, completion, context, estimated } => {
                let Some(chat) = self.turns.get(&id).map(|t| t.chat.clone()) else { return };
                if let Some(live) = self.live.get_mut(&chat) {
                    live.tally.call(prompt, completion);
                    live.tally.context = Some((context, estimated));
                }
                self.send_usage(&chat);
            }
            turn::TurnEvent::Compacting { id, on } => {
                let Some(chat) = self.turns.get(&id).map(|t| t.chat.clone()) else { return };
                if let Some(live) = self.live.get_mut(&chat) {
                    live.tally.compacting = on;
                }
                self.send_usage(&chat);
            }
            turn::TurnEvent::Compacted { id, made } => {
                let Some(chat) = self.turns.get(&id).map(|t| t.chat.clone()) else { return };
                let new = byob_store::NewCompaction {
                    through: chats::message_id(made.through),
                    summary: made.summary,
                    tokens_before: made.tokens_before,
                    tokens_after: made.tokens_after,
                    prompt_tokens: made.prompt_tokens,
                    completion_tokens: made.completion_tokens,
                    focus: None,
                    manual: false,
                };
                self.record_compaction(&chat, &new);
            }
            turn::TurnEvent::Notice { id, text } => {
                let Some(chat) = self.turns.get(&id).map(|t| t.chat.clone()) else { return };
                self.send_viewers(&chat, ServerMsg::Notice { level: NoticeLevel::Info, text });
            }
        }
    }

    // ---- compaction and usage -----------------------------------------------------------

    /// Store a compaction, make it the base of later requests, tell the chat's viewers.
    fn record_compaction(&mut self, chat: &str, new: &byob_store::NewCompaction) {
        let stored = match self.store.add_compaction(chat, new) {
            Ok(c) => c,
            Err(e) => {
                // Not fatal: this turn still goes on from the summary; later ones use the full history.
                log::error!("saving a compaction of {chat}: {e}");
                return;
            }
        };
        let msg = ServerMsg::Compacted(chats::compaction(&stored));
        if let Some(live) = self.live.get_mut(chat) {
            live.tally.compactions = live.tally.compactions.saturating_add(1);
            live.tally.context = Some((new.tokens_after, true));
            live.compactions.push(stored);
        }
        self.send_viewers(chat, msg);
        self.send_usage(chat);
    }

    /// `ClientMsg::Compact` / `/compact [focus]`: summarise the chat in view now, on a
    /// background thread. The chat counts as busy until it is done.
    fn compact_chat(&mut self, client: ClientId, focus: Option<String>) {
        let Some(chat) = self.client_chat(client) else {
            self.send_to(client, ServerMsg::Notice { level: NoticeLevel::Info, text: "Nothing to compact yet.".into() });
            return;
        };
        if let Err((level, text)) = self.compact_chat_in(&chat, focus) {
            self.send_to(client, ServerMsg::Notice { level, text });
        }
    }

    /// Start summarising `chat` (see [`Self::compact_chat`]), or say why not.
    pub(crate) fn compact_chat_in(&mut self, chat: &str, focus: Option<String>) -> Result<(), (NoticeLevel, String)> {
        let chat = chat.to_owned();
        let error = |text: String| Err((NoticeLevel::Error, text));
        if self.chat_busy(&chat) {
            return error("a reply is still running in this chat; stop it first".into());
        }
        let settings = self.chat_settings(Some(&chat), None);
        let llm = match self.llm_for(&settings.inference) {
            Ok(llm) => llm,
            Err(e) => return error(format!("inference URL is invalid: {e}")),
        };
        let plan = self.endpoints.plan_with(&settings.inference, settings.effective.tool_mode.value);
        let specs = turn::specs(&self.tools);
        let base_prompt = self.prompt_with_instructions(&settings.effective);
        let (messages, through) = match self.live(&chat) {
            Ok(live) => {
                let (messages, _) = compact::history(&live.entries, latest(live), None, &system_prompt(&base_prompt, live));
                (messages, live.entries.last().map_or(0, |e| e.id))
            }
            Err(e) => return error(e),
        };
        let start = compact::content_start(&messages);
        let fresh = messages.iter().skip(start).filter(|m| m.content.as_deref() != Some(compact::SUMMARY_ACK)).count();
        let has_summary = messages.get(start).is_some_and(|m| m.content.as_deref().is_some_and(|c| c.starts_with(compact::SUMMARY_HEADER)));
        if fresh <= usize::from(has_summary) {
            return Err((NoticeLevel::Info, "Nothing new to compact.".into()));
        }
        let tokens_before = plan.raw_tokens(&messages, &specs);
        let cancel = Arc::new(AtomicBool::new(false));
        let (flag, inbox) = (Arc::clone(&cancel), self.inbox.clone());
        let (c, f) = (chat.clone(), focus.clone());
        let spawned = thread::Builder::new().name("byob-compact".into()).spawn(move || {
            let started = Instant::now();
            let covered = messages.get(start..).unwrap_or_default();
            let result = catch_unwind(AssertUnwindSafe(|| compact::summarize(&llm, covered, f.as_deref(), &plan, &flag)))
                .unwrap_or_else(|_| Err(compact::Failure::Error("internal error while summarising".into())))
                .map(|summary| {
                    let mut after: Vec<byob_llm::ChatMessage> = messages.get(..start).unwrap_or_default().to_vec();
                    after.extend(compact::summary_messages(&summary.text));
                    let tokens = plan.raw_tokens(&after, &specs);
                    (summary, tokens)
                });
            let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            let _ = inbox.send(Input::Compacted(Box::new(ManualDone { chat: c, through, focus: f, tokens_before, elapsed_ms, result })));
        });
        if let Err(e) = spawned {
            return error(format!("could not start the compaction: {e}"));
        }
        self.compacting.insert(chat.clone(), cancel);
        if let Some(live) = self.live.get_mut(&chat) {
            live.tally.compacting = true;
        }
        self.chat_changed(&chat);
        self.viewer_states(&chat);
        self.send_usage(&chat);
        Ok(())
    }

    fn manual_compacted(&mut self, done: ManualDone) {
        let ManualDone { chat, through, focus, tokens_before, elapsed_ms, result } = done;
        let mut pause = None;
        self.compacting.remove(&chat);
        if let Some(live) = self.live.get_mut(&chat) {
            live.tally.compacting = false;
            live.tally.elapsed_ms = live.tally.elapsed_ms.saturating_add(elapsed_ms);
        }
        match result {
            Ok((summary, tokens_after)) => {
                let (prompt, completion) = summary.usage.map_or((0, 0), |u| (u.prompt_tokens, u.completion_tokens));
                if let Some(live) = self.live.get_mut(&chat) {
                    live.tally.call(prompt, completion);
                }
                if tokens_after >= tokens_before {
                    let text = "The summary came out longer than the conversation it would replace, so nothing changed.".to_owned();
                    self.send_viewers(&chat, ServerMsg::Notice { level: NoticeLevel::Info, text });
                    self.chat_changed(&chat);
                    self.viewer_states(&chat);
                    self.send_usage(&chat);
                    self.queue_turn_ended(&chat, None);
                    self.gc();
                    return;
                }
                let new = byob_store::NewCompaction {
                    through: chats::message_id(through),
                    summary: summary.text,
                    tokens_before: u64::try_from(tokens_before).unwrap_or(u64::MAX),
                    tokens_after: u64::try_from(tokens_after).unwrap_or(u64::MAX),
                    prompt_tokens: prompt,
                    completion_tokens: completion,
                    focus,
                    manual: true,
                };
                self.record_compaction(&chat, &new);
            }
            Err(failure) => {
                let (level, text, why) = match failure {
                    compact::Failure::Stopped => (NoticeLevel::Info, "Compaction stopped.".to_owned(), byob_protocol::QueuePause::Stopped),
                    compact::Failure::Error(e) => (NoticeLevel::Error, format!("couldn't compact the conversation: {e}"), byob_protocol::QueuePause::Failed),
                };
                self.send_viewers(&chat, ServerMsg::Notice { level, text });
                pause = Some(why);
            }
        }
        self.chat_changed(&chat);
        self.viewer_states(&chat);
        self.send_usage(&chat);
        self.queue_turn_ended(&chat, pause);
        self.gc();
    }

    /// The header meter of `chat`, if it is loaded.
    fn usage(&self, chat: &str) -> Option<byob_protocol::ChatUsage> {
        let live = self.live.get(chat)?;
        let settings = self.chat_settings(Some(chat), None);
        let plan = self.endpoints.plan_with(&settings.inference, settings.effective.tool_mode.value);
        let compaction = self.compact_options(&settings.effective);
        let (context_used, estimated) = live.tally.context.unwrap_or_else(|| {
            let base = self.prompt_with_instructions(&settings.effective);
            let (messages, _) = compact::history(&live.entries, latest(live), None, &system_prompt(&base, live));
            (u64::try_from(plan.raw_tokens(&messages, &turn::specs(&self.tools))).unwrap_or(u64::MAX), true)
        });
        let t = &live.tally;
        let n = |v: usize| u64::try_from(v).unwrap_or(u64::MAX);
        Some(byob_protocol::ChatUsage {
            chat: chat.to_owned(),
            context_used,
            estimated,
            context_window: n(plan.context),
            compact_at: if compaction.auto { n(compaction.threshold(&plan)) } else { 0 },
            last_prompt_tokens: t.last_prompt,
            last_completion_tokens: t.last_completion,
            prompt_tokens: t.prompt_tokens,
            completion_tokens: t.completion_tokens,
            model_calls: t.model_calls,
            compactions: t.compactions,
            elapsed_ms: t.elapsed_ms,
            compacting: t.compacting,
            agent_prompt_tokens: t.agent_prompt,
            agent_completion_tokens: t.agent_completion,
            agent_calls: t.agent_calls,
        })
    }

    fn send_usage(&mut self, chat: &str) {
        if let Some(u) = self.usage(chat) {
            self.send_viewers(chat, ServerMsg::Usage(u));
        }
    }

    fn finish_turn(&mut self, id: u64, error: Option<String>, stats: Option<byob_protocol::TurnStats>) {
        self.deny_pending(|p, _| p.entry == id, "stopped");
        // Nothing may stay "running" or "waiting" in a finished entry, subagents' steps included.
        let steps: Vec<Step> = self.reply_mut(id).map(|e| e.steps.clone()).unwrap_or_default();
        for parent in &steps {
            for mut step in parent.delegation.iter().flat_map(|d| d.steps.iter()).filter(|s| !s.status.finished()).cloned() {
                step.status = StepStatus::Failed;
                "interrupted".clone_into(&mut step.detail);
                self.update_child_step(id, parent.id, step);
            }
        }
        for mut step in steps.into_iter().filter(|s| !s.status.finished() || s.delegation.as_ref().is_some_and(|d| !d.state.finished())) {
            if !step.status.finished() {
                step.status = StepStatus::Failed;
                "interrupted".clone_into(&mut step.detail);
            }
            if let Some(d) = step.delegation.as_deref_mut().filter(|d| !d.state.finished()) {
                d.state = DelegationState::Stopped;
                d.error.get_or_insert_with(|| "interrupted".to_owned());
            }
            self.update_step(id, step);
        }
        self.flush(id);
        if let Err(e) = self.store.finish_message(chats::message_id(id), error.as_deref(), stats.as_ref().map(chats::to_stats).as_ref()) {
            log::error!("saving the finished reply: {e}");
        }
        let content = self.reply_mut(id).map(|entry| {
            entry.done = true;
            entry.error.clone_from(&error);
            entry.stats = stats;
            entry.content.clone()
        });
        let Some(run) = self.turns.remove(&id) else { return };
        if let Some(live) = self.live.get_mut(&run.chat) {
            live.tally.compacting = false;
            live.tally.elapsed_ms = live.tally.elapsed_ms.saturating_add(stats.map_or(0, |s| s.elapsed_ms));
        }
        self.send_viewers(&run.chat, ServerMsg::EntryFinished { id, error: error.clone(), stats });
        self.chat_changed(&run.chat);
        self.viewer_states(&run.chat);
        self.send_usage(&run.chat);
        self.turn_outcome(run.llm.config(), error.as_deref());
        // A changed link shows in the header of everyone looking at the chat.
        self.viewer_states(&run.chat);
        if let (Some(user), Some(reply), None) = (run.title_from, content, &error) {
            chats::spawn_titler(run.llm.clone(), run.chat.clone(), user, reply, self.inbox.clone());
        }
        // On to the next queued message, unless the turn was stopped or failed.
        let pause = if run.cancel.load(Ordering::Relaxed) {
            Some(byob_protocol::QueuePause::Stopped)
        } else {
            error.as_ref().map(|_| byob_protocol::QueuePause::Failed)
        };
        self.queue_turn_ended(&run.chat, pause);
        self.gc();
    }

    /// Write batched stream text of turns that haven't saved for [`FLUSH_EVERY`].
    fn flush_due(&mut self) {
        let due: Vec<u64> = self.turns.iter().filter(|(_, t)| t.saved_at.elapsed() >= FLUSH_EVERY).map(|(id, _)| *id).collect();
        for id in due {
            self.flush(id);
        }
    }

    fn flush(&mut self, entry: u64) {
        let Some(run) = self.turns.get_mut(&entry) else { return };
        run.saved_at = Instant::now();
        let chat = run.chat.clone();
        for child in run.children.values_mut() {
            let (content, reasoning) = std::mem::take(&mut child.unsaved);
            if let Some(id) = child.run.as_deref().filter(|_| !content.is_empty() || !reasoning.is_empty())
                && let Err(e) = self.store.append_agent_text(&chat, id, &content, &reasoning)
            {
                log::error!("saving a subagent's streamed text: {e}");
            }
        }
        if run.unsaved.0.is_empty() && run.unsaved.1.is_empty() {
            return;
        }
        let (content, reasoning) = std::mem::take(&mut run.unsaved);
        if let Err(e) = self.store.append_message_text(chats::message_id(entry), &content, &reasoning) {
            log::error!("saving streamed text: {e}");
        }
    }

    // ---- delivery ---------------------------------------------------------------------

    fn notice(&mut self, id: ClientId, text: String) {
        self.send_to(id, ServerMsg::Notice { level: NoticeLevel::Error, text });
    }

    fn send_to(&mut self, id: ClientId, msg: ServerMsg) {
        let gone = self.clients.get(&id).is_some_and(|c| deliver(c, msg).is_err());
        if gone {
            self.clients.remove(&id);
        }
    }

    fn broadcast(&mut self, msg: ServerMsg) {
        self.clients.retain(|_, c| !c.attached || deliver(c, msg.clone()).is_ok());
    }

    /// Send to the clients looking at `chat`.
    fn send_viewers(&mut self, chat: &str, msg: ServerMsg) {
        self.clients.retain(|_, c| c.chat.as_deref() != Some(chat) || deliver(c, msg.clone()).is_ok());
    }

    fn viewer_states(&mut self, chat: &str) {
        let state = self.state(Some(chat));
        self.send_viewers(chat, ServerMsg::State(state));
    }
}

/// The system prompt of a chat's turns: later turns must know its artifacts by id.
fn system_prompt(base: &str, live: &Live) -> String {
    let listed = artifact::prompt_list(&live.artifacts);
    if listed.is_empty() { base.to_owned() } else { format!("{base}\n\n{listed}") }
}

/// The compaction the next request builds on: (last entry covered, summary).
fn latest(live: &Live) -> Option<(u64, &str)> {
    live.compactions.last().map(|c| (chats::entry_id(c.through), c.summary.as_str()))
}

fn deliver(client: &Client, msg: ServerMsg) -> Result<(), ()> {
    client.tx.send(msg).map_err(|_| ())?;
    (client.wake)();
    Ok(())
}

/// Run `work` on its own named thread, off the engine thread; a panic there is logged.
pub(crate) fn spawn_worker(name: &str, work: impl FnOnce() + Send + 'static) {
    let label = name.to_owned();
    let spawned = thread::Builder::new().name(name.into()).spawn(move || {
        if catch_unwind(AssertUnwindSafe(work)).is_err() {
            log::error!("{label} crashed");
        }
    });
    if let Err(e) = spawned {
        log::error!("could not start {name}: {e}");
    }
}
