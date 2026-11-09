//! Headless agent engine.
//!
//! The engine runs on its own thread and owns all state. Clients (the desktop window, each
//! browser tab via `yah-server`) attach with [`EngineHandle::connect`] and speak only
//! [`yah_protocol`] messages.
//!
//! Chats live in `yah-store`. Each client looks at one chat at a time (desktop and phone may
//! look at different ones); turns run in the engine regardless of who is watching, one per
//! chat, and entry updates only go to the clients looking at that chat. Streamed text is
//! written to the database in batches (every [`FLUSH_EVERY`]), not per token.
//!
//! It also owns the multi-round tool-calling loop (`turn`), the permission gate
//! ([`permissions`]) and pending approvals: a turn that needs approval blocks until a
//! client answers the approval card, or the turn is stopped.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

mod chats;
pub mod dev_tools;
#[cfg(test)]
mod engine_tests;
mod limits;
#[cfg(test)]
mod loop_tests;
pub mod permissions;
mod steps;
mod turn;

use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

pub use permissions::{Policy, Rule};
use yah_protocol::{
    ApprovalAnswer, ChatCursor, ChatSummary, ClientMsg, Connection, EngineState, Entry, EntryRole, NoticeLevel, PROTOCOL_VERSION, ServerMsg, Step, StepStatus,
};
use yah_store::{GrantScope, NewMessage, Paths, Role, Secrets, Settings, Store, StoreError};
use yah_tools::Registry;
use yah_types::InferenceConfig;

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
    pub policy: Policy,
    /// System prompt for every turn (plugins and skills append to it).
    pub system_prompt: String,
    /// Where the database, settings and secrets live. `None` keeps chats in memory and
    /// saves nothing (tests, throwaway runs).
    pub paths: Option<Paths>,
    /// Name each chat with a short model request after its first reply (otherwise the
    /// first line of the first message stays the title).
    pub auto_title: bool,
}

impl EngineConfig {
    /// No tools, default policy, the current directory as workspace, chats in memory.
    pub fn new(inference: InferenceConfig) -> Self {
        let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            inference,
            workspace,
            tools: Registry::default(),
            policy: Policy::default(),
            system_prompt: turn::DEFAULT_SYSTEM_PROMPT.to_owned(),
            paths: None,
            auto_title: true,
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
    /// The model suggested a title for a chat.
    Titled {
        chat: String,
        title: String,
    },
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
    /// when yah last stopped are closed with an error first.
    pub fn spawn_with(config: EngineConfig) -> std::io::Result<Self> {
        let store = open_store(config.paths.as_ref()).map_err(std::io::Error::other)?;
        let (inbox, rx) = channel();
        let engine = Engine::new(config, store, inbox.clone());
        thread::Builder::new().name("yah-engine".into()).spawn(move || run(engine, rx))?;
        Ok(Self { inbox, next_id: Arc::default() })
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

fn open_store(paths: Option<&Paths>) -> Result<Store, StoreError> {
    let store = match paths {
        Some(p) => {
            p.ensure()?;
            Store::open(p.db())?
        }
        None => Store::open_in_memory()?,
    };
    let cut = store.finish_interrupted(chats::INTERRUPTED)?;
    if cut > 0 {
        log::warn!("{cut} reply(s) were interrupted when yah last stopped");
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
}

/// An approval card waiting for an answer.
struct Pending {
    entry: u64,
    step: Step,
    reply: Sender<turn::Verdict>,
}

/// A chat whose transcript is in memory: someone looks at it or a reply runs in it.
struct Live {
    entries: Vec<Entry>,
    workspace: Arc<PathBuf>,
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
}

struct Engine {
    config: InferenceConfig,
    llm: Result<yah_llm::Client, String>,
    clients: BTreeMap<ClientId, Client>,
    /// Sender into our own inbox, handed to turn threads.
    inbox: Sender<Input>,
    store: Store,
    paths: Option<Paths>,
    default_workspace: PathBuf,
    live: BTreeMap<String, Live>,
    turns: BTreeMap<u64, Running>,
    tools: Registry,
    policy: Policy,
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
    fn new(config: EngineConfig, store: Store, inbox: Sender<Input>) -> Self {
        let llm = yah_llm::Client::new(config.inference.clone()).map_err(|e| e.to_string());
        Self {
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
            policy: config.policy,
            system_prompt: config.system_prompt,
            pending: BTreeMap::new(),
            step_ids: Arc::default(),
            journals: steps::Journals::default(),
            auto_title: config.auto_title,
            provisional: BTreeMap::new(),
        }
    }

    // ---- chats ------------------------------------------------------------------------

    fn chat_busy(&self, chat: &str) -> bool {
        self.turns.values().any(|t| t.chat == chat)
    }

    fn summary(&self, chat: &str) -> Option<ChatSummary> {
        match self.store.chat(chat) {
            Ok(Some(c)) => Some(chats::summary(&c, self.chat_busy(chat))),
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
            let entries = messages.into_iter().filter_map(|m| chats::entry(m, &self.step_ids)).collect();
            self.live.insert(chat.to_owned(), Live { entries, workspace: Arc::new(row.workspace) });
        }
        self.live.get_mut(chat).ok_or_else(|| "that chat no longer exists".to_owned())
    }

    /// Forget transcripts nobody looks at and nothing runs in.
    fn gc(&mut self) {
        let keep: BTreeSet<&String> = self.clients.values().filter_map(|c| c.chat.as_ref()).chain(self.turns.values().map(|t| &t.chat)).collect();
        self.live.retain(|k, _| keep.contains(k));
    }

    /// Point client `id` at `chat` and send it the state, the chat and its transcript.
    fn show_chat(&mut self, id: ClientId, chat: Option<String>) {
        self.show_chat_with(id, chat, true);
    }

    fn show_chat_with(&mut self, id: ClientId, chat: Option<String>, send_state: bool) {
        let entries = match &chat {
            Some(c) => match self.live(c) {
                Ok(live) => live.entries.clone(),
                Err(text) => {
                    self.send_to(id, ServerMsg::Notice { level: NoticeLevel::Error, text });
                    return;
                }
            },
            None => Vec::new(),
        };
        let summary = chat.as_deref().and_then(|c| self.summary(c));
        if let Some(client) = self.clients.get_mut(&id) {
            client.chat.clone_from(&chat);
        }
        if send_state {
            self.send_to(id, ServerMsg::State(self.state(chat.as_deref())));
        }
        self.send_to(id, ServerMsg::ChatOpened { chat: summary });
        self.send_to(id, ServerMsg::Transcript { entries });
        self.gc();
    }

    fn send_chats(&mut self, id: ClientId, after: Option<ChatCursor>, limit: usize) {
        let limit = limit.clamp(1, yah_store::MAX_PAGE);
        let pinned = if after.is_none() { self.store.pinned_chats() } else { Ok(Vec::new()) };
        let page = self.store.list_chats(limit, after.as_ref().map(chats::store_cursor).as_ref());
        match (pinned, page) {
            (Ok(pinned), Ok(page)) => {
                let pinned = pinned.iter().map(|c| chats::summary(c, self.chat_busy(&c.id))).collect();
                let recent = page.chats.iter().filter(|c| !c.pinned()).map(|c| chats::summary(c, self.chat_busy(&c.id))).collect();
                let next = page.next.as_ref().map(chats::protocol_cursor);
                self.send_to(id, ServerMsg::Chats { pinned, recent, after, next });
            }
            (Err(e), _) | (_, Err(e)) => self.notice(id, format!("couldn't list chats: {e}")),
        }
    }

    /// Create a chat titled after its first message.
    fn create_chat(&mut self, first_message: &str) -> Result<String, StoreError> {
        let title = chats::heuristic_title(first_message);
        let mut attempt = 0;
        let chat = loop {
            let id = chats::new_chat_id();
            match self.store.create_chat(&id, &title, &self.default_workspace) {
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
        self.live.insert(chat.id.clone(), Live { entries: Vec::new(), workspace: Arc::new(chat.workspace.clone()) });
        self.broadcast(ServerMsg::ChatUpdated(chats::summary(&chat, false)));
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
        match self.store.delete_chat(chat) {
            Ok(tasks) => {
                if let Some(paths) = &self.paths {
                    for task in tasks.iter().filter(|t| !t.is_empty() && !t.contains(['/', '\\', '.'])) {
                        let _ = std::fs::remove_dir_all(paths.undo_dir().join(task));
                    }
                }
            }
            Err(e) => {
                self.notice(client, format!("couldn't delete the chat: {e}"));
                return;
            }
        }
        self.live.remove(chat);
        self.provisional.remove(chat);
        self.broadcast(ServerMsg::ChatDeleted { id: chat.to_owned() });
        let viewers: Vec<ClientId> = self.clients.iter().filter(|(_, c)| c.chat.as_deref() == Some(chat)).map(|(id, _)| *id).collect();
        for id in viewers {
            self.show_chat(id, None);
        }
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

    fn state(&self, chat: Option<&str>) -> EngineState {
        EngineState {
            version: env!("CARGO_PKG_VERSION").into(),
            inference_base_url: self.config.base_url.clone(),
            inference_model: self.config.model.clone(),
            has_api_key: self.config.api_key.is_some(),
            endpoint: self.llm.as_ref().ok().map(|c| c.endpoint().to_string()),
            busy: chat.is_some_and(|c| self.chat_busy(c)),
        }
    }

    /// Send every client the state as it sees it (busy is per chat).
    fn broadcast_state(&mut self) {
        let ids: Vec<ClientId> = self.clients.keys().copied().collect();
        for id in ids {
            let state = self.state(self.clients.get(&id).and_then(|c| c.chat.as_deref()));
            self.send_to(id, ServerMsg::State(state));
        }
    }

    fn handle(&mut self, input: Input) {
        match input {
            Input::Connect { id, tx, wake } => {
                self.clients.insert(id, Client { tx, wake, chat: None });
            }
            Input::Disconnect { id } => {
                self.clients.remove(&id);
                self.gc();
            }
            Input::Client { id, msg } => self.handle_client(id, msg),
            Input::Turn(event) => self.handle_turn(event),
            Input::Titled { chat, title } => self.titled(&chat, &title),
        }
    }

    fn handle_client(&mut self, id: ClientId, msg: ClientMsg) {
        match msg {
            ClientMsg::Hello { protocol, client } => {
                if protocol != PROTOCOL_VERSION {
                    let text = format!("protocol mismatch: client {protocol}, engine {PROTOCOL_VERSION}; reload the page");
                    self.send_to(id, ServerMsg::Notice { level: NoticeLevel::Error, text });
                    return;
                }
                log::info!("client {id} attached: {client}");
                let mut chat = self.client_chat(id).or_else(|| self.most_recent_chat());
                if let Some(c) = chat.clone()
                    && let Err(e) = self.live(&c)
                {
                    log::error!("opening chat {c}: {e}");
                    chat = None;
                }
                // State first: it is the documented answer to Hello.
                self.send_to(id, ServerMsg::State(self.state(chat.as_deref())));
                self.send_chats(id, None, FIRST_PAGE);
                self.show_chat_with(id, chat, false);
            }
            ClientMsg::SetInference { base_url, model, api_key } => self.set_inference(id, base_url, model, api_key),
            ClientMsg::SendMessage { text } => self.start_turn(id, text),
            ClientMsg::Cancel => {
                if let Some(chat) = self.client_chat(id) {
                    for t in self.turns.values().filter(|t| t.chat == chat) {
                        t.cancel.store(true, Ordering::Relaxed);
                    }
                    self.deny_pending(|_, t| t.is_some_and(|t| t.chat == chat), "stopped");
                }
            }
            ClientMsg::Approve { step, answer } => self.answer(step, answer),
            ClientMsg::Undo { entry } => self.undo(id, entry),
            ClientMsg::ListChats { after, limit } => self.send_chats(id, after, usize::try_from(limit).unwrap_or(FIRST_PAGE)),
            ClientMsg::NewChat => self.show_chat(id, None),
            ClientMsg::OpenChat { id: chat } => self.show_chat(id, Some(chat)),
            ClientMsg::RenameChat { id: chat, title } => self.rename_chat(id, &chat, &title),
            ClientMsg::DeleteChat { id: chat } => self.delete_chat(id, &chat),
            ClientMsg::PinChat { id: chat, pinned } => self.pin_chat(id, &chat, pinned),
        }
    }

    fn set_inference(&mut self, id: ClientId, base_url: String, model: String, api_key: Option<String>) {
        let new_key = api_key.is_some();
        let config = InferenceConfig { base_url, model, api_key: api_key.or_else(|| self.config.api_key.clone()) };
        match yah_llm::Client::new(config.clone()) {
            Ok(client) => {
                if let Err(e) = self.save_inference(&config, new_key) {
                    self.notice(id, format!("inference settings apply now but couldn't be saved: {e}"));
                }
                self.config = config;
                self.llm = Ok(client);
                self.broadcast_state();
            }
            Err(e) => self.notice(id, e.to_string()),
        }
    }

    /// Remember the endpoint in settings.json and a new API key in secrets.json (only there).
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
        let Some(Pending { entry, mut step, reply }) = self.pending.remove(&step_id) else { return };
        let chat = self.turns.get(&entry).map(|t| t.chat.clone());
        let scope = match answer {
            ApprovalAnswer::Always => chat.map(GrantScope::Chat),
            ApprovalAnswer::AlwaysEverywhere => Some(GrantScope::Global),
            ApprovalAnswer::Allow | ApprovalAnswer::Deny => None,
        };
        if let Some(scope) = scope
            && let Err(e) = self.store.grant(&scope, &step.tool)
        {
            log::error!("saving the grant for {}: {e}", step.tool);
        }
        let verdict = match answer {
            ApprovalAnswer::Allow | ApprovalAnswer::Always | ApprovalAnswer::AlwaysEverywhere => turn::Verdict::Allow,
            ApprovalAnswer::Deny => turn::Verdict::Deny("the user said no".into()),
        };
        step.status = if verdict == turn::Verdict::Allow { StepStatus::Running } else { StepStatus::Denied };
        if let turn::Verdict::Deny(reason) = &verdict {
            step.detail.clone_from(reason);
        }
        // The turn thread may be gone (stopped); the step update still settles the card.
        let _ = reply.send(verdict);
        self.update_step(entry, step);
    }

    /// Deny the pending approvals of turns matching `which`.
    fn deny_pending(&mut self, which: impl Fn(&Pending, Option<&Running>) -> bool, reason: &str) {
        let ids: Vec<u64> = self.pending.iter().filter(|(_, p)| which(p, self.turns.get(&p.entry))).map(|(id, _)| *id).collect();
        for id in ids {
            let Some(Pending { entry, mut step, reply }) = self.pending.remove(&id) else { continue };
            let _ = reply.send(turn::Verdict::Deny(reason.to_owned()));
            step.status = StepStatus::Denied;
            reason.clone_into(&mut step.detail);
            self.update_step(entry, step);
        }
    }

    /// A turn asks whether a step may run.
    fn gate(&mut self, entry: u64, mut step: Step, reply: Sender<turn::Verdict>) {
        let Some(run) = self.turns.get(&entry).filter(|t| !t.cancel.load(Ordering::Relaxed)) else {
            let _ = reply.send(turn::Verdict::Deny("stopped".into()));
            return;
        };
        let granted = self.store.is_allowed(&run.chat, &step.tool).unwrap_or_else(|e| {
            log::error!("reading grants: {e}");
            false
        });
        match permissions::decide(&self.policy, granted, step.access) {
            permissions::Gate::Allow => {
                let _ = reply.send(turn::Verdict::Allow);
            }
            permissions::Gate::Deny(reason) => {
                let _ = reply.send(turn::Verdict::Deny(reason));
            }
            permissions::Gate::Ask => {
                step.status = StepStatus::Waiting;
                self.update_step(entry, step.clone());
                self.pending.insert(step.id, Pending { entry, step, reply });
            }
        }
    }

    // ---- turns ------------------------------------------------------------------------

    fn reply_mut(&mut self, entry: u64) -> Option<&mut Entry> {
        let chat = &self.turns.get(&entry)?.chat;
        self.live.get_mut(chat)?.entries.iter_mut().rev().find(|e| e.id == entry)
    }

    fn update_step(&mut self, entry: u64, step: Step) {
        self.persist_step(entry, &step);
        let Some(chat) = self.turns.get(&entry).map(|t| t.chat.clone()) else { return };
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
    fn add_message(&mut self, chat: &str, m: &NewMessage) -> Result<u64, StoreError> {
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
        };
        if let Some(live) = self.live.get_mut(chat) {
            live.entries.push(entry.clone());
        }
        self.send_viewers(chat, ServerMsg::EntryAdded(entry));
        Ok(id)
    }

    fn start_turn(&mut self, client: ClientId, text: String) {
        let text = text.trim().to_owned();
        if text.is_empty() {
            return;
        }
        let llm = match &self.llm {
            Ok(llm) => llm.clone(),
            Err(e) => {
                let text = format!("inference URL is invalid: {e}");
                self.notice(client, text);
                return;
            }
        };
        let chat = match self.client_chat(client) {
            Some(chat) => chat,
            None => match self.create_chat(&text) {
                Ok(chat) => {
                    self.show_chat(client, Some(chat.clone()));
                    chat
                }
                Err(e) => {
                    self.notice(client, format!("couldn't create the chat: {e}"));
                    return;
                }
            },
        };
        if self.chat_busy(&chat) {
            self.notice(client, "a reply is still running in this chat; stop it first".into());
            return;
        }
        let system_prompt = self.system_prompt.clone();
        let (history, workspace, first) = match self.live(&chat) {
            Ok(live) => (turn::history(&live.entries, &text, &system_prompt), Arc::clone(&live.workspace), live.entries.is_empty()),
            Err(e) => {
                self.notice(client, e);
                return;
            }
        };
        let added = self.add_message(&chat, &NewMessage::user(text.clone())).and_then(|_| self.add_message(&chat, &NewMessage::assistant_streaming(None)));
        let reply = match added {
            Ok(reply) => reply,
            Err(e) => {
                self.notice(client, format!("couldn't save the message: {e}"));
                return;
            }
        };
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
            step_ids: Arc::clone(&self.step_ids),
        };
        if let Err(e) = turn::spawn(setup) {
            self.handle_turn(turn::TurnEvent::Finished { id: reply, error: Some(format!("could not start turn: {e}")), stats: None });
        }
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
            turn::TurnEvent::Gate { id, step, reply } => self.gate(id, step, reply),
            turn::TurnEvent::Undo { id, ops, complete } => self.add_undo(id, ops, complete),
            turn::TurnEvent::Finished { id, error, stats } => self.finish_turn(id, error, stats),
        }
    }

    fn finish_turn(&mut self, id: u64, error: Option<String>, stats: Option<yah_protocol::TurnStats>) {
        self.deny_pending(|p, _| p.entry == id, "stopped");
        // Nothing may stay "running" or "waiting" in a finished entry.
        let open: Vec<Step> = self.reply_mut(id).map(|e| e.steps.iter().filter(|s| !s.status.finished()).cloned().collect()).unwrap_or_default();
        for mut step in open {
            step.status = StepStatus::Failed;
            "interrupted".clone_into(&mut step.detail);
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
        self.send_viewers(&run.chat, ServerMsg::EntryFinished { id, error: error.clone(), stats });
        self.chat_changed(&run.chat);
        self.viewer_states(&run.chat);
        if let (Some(user), Some(reply), None, Ok(llm)) = (run.title_from, content, &error, &self.llm) {
            chats::spawn_titler(llm.clone(), run.chat.clone(), user, reply, self.inbox.clone());
        }
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
        self.clients.retain(|_, c| deliver(c, msg.clone()).is_ok());
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

fn deliver(client: &Client, msg: ServerMsg) -> Result<(), ()> {
    client.tx.send(msg).map_err(|_| ())?;
    (client.wake)();
    Ok(())
}
