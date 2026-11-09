//! Headless agent engine.
//!
//! The engine runs on its own thread and owns all state. Clients (the desktop window, each
//! browser tab via `yah-server`) attach with [`EngineHandle::connect`] and speak only
//! [`yah_protocol`] messages, so every client sees the same live session.
//!
//! It also owns the multi-round tool-calling loop (`turn`), the permission gate
//! ([`permissions`]) and pending approvals: a turn that needs approval blocks until any
//! client answers the approval card, or the turn is stopped.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

pub mod dev_tools;
mod limits;
#[cfg(test)]
mod loop_tests;
pub mod permissions;
mod turn;

use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

pub use permissions::{Policy, Rule};
use yah_protocol::{ApprovalAnswer, ClientMsg, Connection, EngineState, Entry, EntryRole, NoticeLevel, PROTOCOL_VERSION, ServerMsg, Step, StepStatus};
use yah_tools::Registry;
use yah_types::InferenceConfig;

/// Everything the engine starts with.
#[derive(Clone)]
pub struct EngineConfig {
    pub inference: InferenceConfig,
    /// Root folder tools work in.
    pub workspace: PathBuf,
    /// Tools offered to the model.
    pub tools: Registry,
    pub policy: Policy,
}

impl EngineConfig {
    /// No tools, default policy, the current directory as workspace.
    pub fn new(inference: InferenceConfig) -> Self {
        let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self { inference, workspace, tools: Registry::default(), policy: Policy::default() }
    }
}

/// Called after the engine queues a message for a client, e.g. to request a UI repaint.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

type ClientId = u64;

enum Input {
    Connect { id: ClientId, tx: Sender<ServerMsg>, wake: Waker },
    Disconnect { id: ClientId },
    Client { id: ClientId, msg: ClientMsg },
    Turn(turn::TurnEvent),
}

/// Cheap, cloneable handle used to attach clients to a running engine.
#[derive(Clone)]
pub struct EngineHandle {
    inbox: Sender<Input>,
    next_id: Arc<std::sync::atomic::AtomicU64>,
}

impl EngineHandle {
    /// Start the engine thread with no tools.
    pub fn spawn(inference: InferenceConfig) -> std::io::Result<Self> {
        Self::spawn_with(EngineConfig::new(inference))
    }

    /// Start the engine thread.
    pub fn spawn_with(config: EngineConfig) -> std::io::Result<Self> {
        let (inbox, rx) = channel();
        let engine = Engine::new(config, inbox.clone());
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
}

/// An approval card waiting for an answer.
struct Pending {
    entry: u64,
    step: Step,
    reply: Sender<turn::Verdict>,
}

struct Engine {
    config: InferenceConfig,
    llm: Result<yah_llm::Client, String>,
    clients: BTreeMap<ClientId, Client>,
    /// Sender into our own inbox, handed to turn threads.
    inbox: Sender<Input>,
    transcript: Vec<Entry>,
    next_entry: u64,
    /// Cancel flag of the running turn, if any.
    running: Option<Arc<AtomicBool>>,
    tools: Registry,
    workspace: Arc<PathBuf>,
    policy: Policy,
    /// Tools the user chose "Always" for, this session.
    always: BTreeSet<String>,
    /// Approval requests by step id.
    pending: BTreeMap<u64, Pending>,
    step_ids: Arc<AtomicU64>,
}

fn run(mut engine: Engine, rx: Receiver<Input>) {
    for input in rx {
        // A bug while handling one message must not take down the engine.
        if catch_unwind(AssertUnwindSafe(|| engine.handle(input))).is_err() {
            log::error!("engine panicked while handling a message; continuing");
        }
    }
}

impl Engine {
    fn new(config: EngineConfig, inbox: Sender<Input>) -> Self {
        let llm = yah_llm::Client::new(config.inference.clone()).map_err(|e| e.to_string());
        Self {
            config: config.inference,
            llm,
            clients: BTreeMap::new(),
            inbox,
            transcript: Vec::new(),
            next_entry: 0,
            running: None,
            tools: config.tools,
            workspace: Arc::new(config.workspace),
            policy: config.policy,
            always: BTreeSet::new(),
            pending: BTreeMap::new(),
            step_ids: Arc::default(),
        }
    }

    fn push_entry(&mut self, role: EntryRole, content: String, done: bool) -> u64 {
        let id = self.next_entry;
        self.next_entry = self.next_entry.saturating_add(1);
        let entry = Entry { id, role, content, reasoning: String::new(), done, error: None, stats: None, steps: Vec::new() };
        self.transcript.push(entry.clone());
        self.broadcast(ServerMsg::EntryAdded(entry));
        id
    }

    fn entry_mut(&mut self, id: u64) -> Option<&mut Entry> {
        self.transcript.iter_mut().rev().find(|e| e.id == id)
    }

    fn state(&self) -> EngineState {
        EngineState {
            version: env!("CARGO_PKG_VERSION").into(),
            inference_base_url: self.config.base_url.clone(),
            inference_model: self.config.model.clone(),
            has_api_key: self.config.api_key.is_some(),
            endpoint: self.llm.as_ref().ok().map(|c| c.endpoint().to_string()),
            busy: self.running.is_some(),
        }
    }

    fn handle(&mut self, input: Input) {
        match input {
            Input::Connect { id, tx, wake } => {
                self.clients.insert(id, Client { tx, wake });
            }
            Input::Disconnect { id } => {
                self.clients.remove(&id);
            }
            Input::Client { id, msg } => self.handle_client(id, msg),
            Input::Turn(event) => self.handle_turn(event),
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
                self.send_to(id, ServerMsg::State(self.state()));
                self.send_to(id, ServerMsg::Transcript { entries: self.transcript.clone() });
            }
            ClientMsg::SetInference { base_url, model, api_key } => {
                let config = InferenceConfig { base_url, model, api_key: api_key.or_else(|| self.config.api_key.clone()) };
                match yah_llm::Client::new(config.clone()) {
                    Ok(client) => {
                        self.config = config;
                        self.llm = Ok(client);
                        self.broadcast(ServerMsg::State(self.state()));
                    }
                    Err(e) => self.send_to(id, ServerMsg::Notice { level: NoticeLevel::Error, text: e.to_string() }),
                }
            }
            ClientMsg::SendMessage { text } => self.start_turn(id, text),
            ClientMsg::Cancel => {
                if let Some(flag) = &self.running {
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                self.deny_all_pending("stopped");
            }
            ClientMsg::Approve { step, answer } => self.answer(step, answer),
        }
    }

    /// A client answered an approval card. The first answer wins; late or unknown ids are ignored.
    fn answer(&mut self, step_id: u64, answer: ApprovalAnswer) {
        let Some(Pending { entry, mut step, reply }) = self.pending.remove(&step_id) else { return };
        let verdict = match answer {
            ApprovalAnswer::Allow => turn::Verdict::Allow,
            ApprovalAnswer::Always => {
                self.always.insert(step.tool.clone());
                turn::Verdict::Allow
            }
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

    fn deny_all_pending(&mut self, reason: &str) {
        for (_, Pending { entry, mut step, reply }) in std::mem::take(&mut self.pending) {
            let _ = reply.send(turn::Verdict::Deny(reason.to_owned()));
            step.status = StepStatus::Denied;
            reason.clone_into(&mut step.detail);
            self.update_step(entry, step);
        }
    }

    /// A turn asks whether a step may run.
    fn gate(&mut self, entry: u64, mut step: Step, reply: Sender<turn::Verdict>) {
        let cancelled = self.running.as_ref().is_none_or(|f| f.load(std::sync::atomic::Ordering::Relaxed));
        if cancelled {
            let _ = reply.send(turn::Verdict::Deny("stopped".into()));
            return;
        }
        match permissions::decide(&self.policy, &self.always, &step.tool, step.access) {
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

    fn update_step(&mut self, entry: u64, step: Step) {
        if let Some(e) = self.entry_mut(entry) {
            e.upsert_step(step.clone());
        }
        self.broadcast(ServerMsg::StepUpdated { entry, step });
    }

    fn start_turn(&mut self, client: ClientId, text: String) {
        let text = text.trim().to_owned();
        if text.is_empty() {
            return;
        }
        if self.running.is_some() {
            self.send_to(client, ServerMsg::Notice { level: NoticeLevel::Error, text: "a reply is still running; stop it first".into() });
            return;
        }
        let llm = match &self.llm {
            Ok(llm) => llm.clone(),
            Err(e) => {
                let text = format!("inference URL is invalid: {e}");
                self.send_to(client, ServerMsg::Notice { level: NoticeLevel::Error, text });
                return;
            }
        };
        let history = turn::history(&self.transcript, &text);
        self.push_entry(EntryRole::User, text, true);
        let reply = self.push_entry(EntryRole::Assistant, String::new(), false);
        let cancel = Arc::new(AtomicBool::new(false));
        self.running = Some(Arc::clone(&cancel));
        self.broadcast(ServerMsg::State(self.state()));
        let setup = turn::Setup {
            llm,
            history,
            entry: reply,
            cancel,
            inbox: self.inbox.clone(),
            tools: self.tools.clone(),
            workspace: Arc::clone(&self.workspace),
            step_ids: Arc::clone(&self.step_ids),
        };
        if let Err(e) = turn::spawn(setup) {
            self.handle_turn(turn::TurnEvent::Finished { id: reply, error: Some(format!("could not start turn: {e}")), stats: None });
        }
    }

    fn handle_turn(&mut self, event: turn::TurnEvent) {
        match event {
            turn::TurnEvent::Delta { id, content, reasoning } => {
                if let Some(entry) = self.entry_mut(id) {
                    entry.append(&content, &reasoning);
                }
                self.broadcast(ServerMsg::EntryDelta { id, content, reasoning });
            }
            turn::TurnEvent::Step { id, step } => {
                // A pending card keeps its Waiting status until answered.
                if !self.pending.contains_key(&step.id) {
                    self.update_step(id, step);
                }
            }
            turn::TurnEvent::Gate { id, step, reply } => self.gate(id, step, reply),
            turn::TurnEvent::Finished { id, error, stats } => {
                self.deny_all_pending("stopped");
                // Nothing may stay "running" or "waiting" in a finished entry.
                let open: Vec<Step> = self.entry_mut(id).map(|e| e.steps.iter().filter(|s| !s.status.finished()).cloned().collect()).unwrap_or_default();
                for mut step in open {
                    step.status = StepStatus::Failed;
                    "interrupted".clone_into(&mut step.detail);
                    self.update_step(id, step);
                }
                if let Some(entry) = self.entry_mut(id) {
                    entry.done = true;
                    entry.error.clone_from(&error);
                    entry.stats = stats;
                }
                self.running = None;
                self.broadcast(ServerMsg::EntryFinished { id, error, stats });
                self.broadcast(ServerMsg::State(self.state()));
            }
        }
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
}

fn deliver(client: &Client, msg: ServerMsg) -> Result<(), ()> {
    client.tx.send(msg).map_err(|_| ())?;
    (client.wake)();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn recv(conn: &mut LocalConnection) -> ServerMsg {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(m) = conn.try_recv() {
                return m;
            }
            assert!(Instant::now() < deadline, "timed out waiting for engine");
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Say hello; return the State reply and swallow the Transcript that follows.
    fn hello(conn: &mut LocalConnection) -> ServerMsg {
        conn.send(ClientMsg::Hello { protocol: PROTOCOL_VERSION, client: "test".into() });
        let state = recv(conn);
        if matches!(state, ServerMsg::State(_)) {
            assert!(matches!(recv(conn), ServerMsg::Transcript { .. }));
        }
        state
    }

    /// Minimal OpenAI-style server that streams `reply` to every request.
    fn fake_llm(reply: &'static str) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut s = stream.unwrap();
                let mut buf = [0u8; 65536];
                let _ = s.read(&mut buf);
                let chunks: String =
                    reply.split_inclusive(' ').map(|w| format!("data: {}\n\n", serde_json::json!({"choices":[{"delta":{"content": w}}]}))).collect();
                let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{chunks}data: [DONE]\n\n");
            }
        });
        format!("http://{addr}/v1")
    }

    #[test]
    fn turn_streams_to_all_clients_and_records_transcript() {
        let engine = EngineHandle::spawn(InferenceConfig { base_url: fake_llm("hello there friend"), ..Default::default() }).unwrap();
        let mut desktop = engine.connect(Arc::new(|| {}));
        let mut phone = engine.connect(Arc::new(|| {}));
        hello(&mut desktop);
        hello(&mut phone);
        desktop.send(ClientMsg::SendMessage { text: "hi".into() });

        let mut reply = String::new();
        let mut finished = false;
        while !finished {
            match recv(&mut phone) {
                ServerMsg::EntryDelta { content, .. } => reply.push_str(&content),
                ServerMsg::EntryFinished { error, .. } => {
                    assert_eq!(error, None);
                    finished = true;
                }
                _ => {}
            }
        }
        assert_eq!(reply, "hello there friend");

        // A client that connects later gets the whole conversation.
        let mut late = engine.connect(Arc::new(|| {}));
        late.send(ClientMsg::Hello { protocol: PROTOCOL_VERSION, client: "late".into() });
        let ServerMsg::State(state) = recv(&mut late) else { panic!("expected state") };
        assert!(!state.busy);
        let ServerMsg::Transcript { entries } = recv(&mut late) else { panic!("expected transcript") };
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].content, "hello there friend");
        assert!(entries[1].done);
    }

    #[test]
    fn unreachable_endpoint_finishes_with_error() {
        let engine = EngineHandle::spawn(InferenceConfig { base_url: "http://127.0.0.1:9/v1".into(), ..Default::default() }).unwrap();
        let mut c = engine.connect(Arc::new(|| {}));
        hello(&mut c);
        c.send(ClientMsg::SendMessage { text: "hi".into() });
        loop {
            if let ServerMsg::EntryFinished { error, .. } = recv(&mut c) {
                assert!(error.is_some());
                break;
            }
        }
    }

    #[test]
    fn hello_returns_state_without_secrets() {
        let engine = EngineHandle::spawn(InferenceConfig { api_key: Some("sk-secret".into()), ..Default::default() }).unwrap();
        let mut c = engine.connect(Arc::new(|| {}));
        let ServerMsg::State(state) = hello(&mut c) else { panic!("expected state") };
        assert!(state.has_api_key);
        assert!(!serde_json::to_string(&state).unwrap().contains("sk-secret"));
    }

    #[test]
    fn inference_change_reaches_every_client() {
        let engine = EngineHandle::spawn(InferenceConfig::default()).unwrap();
        let mut desktop = engine.connect(Arc::new(|| {}));
        let mut browser = engine.connect(Arc::new(|| {}));
        hello(&mut desktop);
        hello(&mut browser);
        desktop.send(ClientMsg::SetInference { base_url: "http://gpu-box:8000/v1".into(), model: "qwen".into(), api_key: None });
        let ServerMsg::State(state) = recv(&mut browser) else { panic!("expected state") };
        assert_eq!(state.endpoint.as_deref(), Some("http://gpu-box:8000/v1/chat/completions"));
    }

    #[test]
    fn bad_url_only_errors_the_sender() {
        let engine = EngineHandle::spawn(InferenceConfig::default()).unwrap();
        let mut a = engine.connect(Arc::new(|| {}));
        hello(&mut a);
        a.send(ClientMsg::SetInference { base_url: "not a url".into(), model: "m".into(), api_key: None });
        assert!(matches!(recv(&mut a), ServerMsg::Notice { level: NoticeLevel::Error, .. }));
    }

    #[test]
    fn protocol_mismatch_is_reported() {
        let engine = EngineHandle::spawn(InferenceConfig::default()).unwrap();
        let mut c = engine.connect(Arc::new(|| {}));
        c.send(ClientMsg::Hello { protocol: PROTOCOL_VERSION + 1, client: "old tab".into() });
        assert!(matches!(recv(&mut c), ServerMsg::Notice { level: NoticeLevel::Error, .. }));
    }
}
