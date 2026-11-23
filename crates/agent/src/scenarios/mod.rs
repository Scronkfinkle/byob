//! Scenario tests: whole stories, end to end.
//!
//! Each scenario runs the real engine with byob's real tools (files, charts, artifacts,
//! `run_command`), the bundled plugins over their native backends (spreadsheets), a real chat
//! store and undo journal, on a temporary data folder and project folder. Only the model is
//! fake: a [`Model`] answers each request with the next answer its scenario queued, which
//! may be worked out from what byob sent (an artifact id in a tool result, say).
//!
//! The bugs these guard lived between components that each passed their own tests, so a
//! scenario reads like what a person did: open a CSV as a spreadsheet, edit it, restart
//! byob, come back.
//!
//! ```text
//! let mut w = World::new();
//! w.file("harvest.csv", "crop,kg\nbeans,4\n");
//! w.model.calls("create_sheet", json!({"title": "Harvest", "from": "harvest.csv"}));
//! w.model.says("Opened it.");
//! let turn = w.send("open harvest.csv as a spreadsheet");
//! turn.step("create_sheet").done();
//! ```
//!
//! [`Model::answers`] takes a closure over the request byob sent, to pick up an id from a
//! tool result or to check what the model was told; a failed check there fails the scenario
//! at the next [`World::send`]. [`World`] also restarts byob on the same data
//! ([`World::restart`]), reconnects a client, undoes a reply, links folders to the project,
//! and keeps what a client would show in [`World::seen`]. Tests here run on SQLite; wrap them
//! in `folder_too!` to run them on the folder backend as well.

mod chats;
mod folders;
mod recovery;
mod sheets;
mod streams;

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use byob_protocol::{Artifact, ChatSummary, ClientMsg, Connection, Entry, NoticeLevel, ServerMsg, Step, StepStatus, WorkspaceSummary};
use byob_store::Paths;
use byob_types::InferenceConfig;
use serde_json::{Value, json};

use crate::extensions::PluginsConfig;
use crate::natives::{Natives, bundled_plugins};
use crate::{CompactOptions, EngineConfig, EngineHandle, LocalConnection, Policy};

/// How long any one wait may take before the scenario fails (nothing here should take long).
const PATIENCE: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------------------------------
// The scripted model
// ---------------------------------------------------------------------------------------

/// One answer of the model: text, tool calls, or both.
#[derive(Debug, Clone, Default)]
pub(crate) struct Answer {
    text: String,
    /// (tool, raw JSON arguments)
    calls: Vec<(String, String)>,
    /// The finish reason to send instead of the usual one (`length`: cut off).
    finish: Option<String>,
    /// End the stream with this error instead of finishing (a server that broke off).
    fail: Option<String>,
}

impl Answer {
    pub(crate) fn text(text: impl Into<String>) -> Self {
        Self { text: text.into(), ..Self::default() }
    }

    pub(crate) fn call(name: &str, args: Value) -> Self {
        Self { calls: vec![(name.to_owned(), args.to_string())], ..Self::default() }
    }

    /// A call with arguments exactly as given, valid JSON or not (models get them wrong).
    pub(crate) fn raw_call(name: &str, args: &str) -> Self {
        Self { calls: vec![(name.to_owned(), args.to_owned())], ..Self::default() }
    }

    /// Cut off at the model's output limit (`finish_reason: length`).
    pub(crate) fn cut(mut self) -> Self {
        self.finish = Some("length".into());
        self
    }

    /// The stream breaks off with an error after what was sent.
    pub(crate) fn failing(mut self, error: &str) -> Self {
        self.fail = Some(error.into());
        self
    }

    /// Text, then tool calls, in one answer.
    pub(crate) fn with_text(mut self, text: &str) -> Self {
        text.clone_into(&mut self.text);
        self
    }

    /// Another tool call in the same answer (the model asking for several at once).
    pub(crate) fn and(mut self, name: &str, args: Value) -> Self {
        self.calls.push((name.to_owned(), args.to_string()));
        self
    }

    /// The OpenAI-style event stream for it, with the arguments split over two chunks as
    /// real servers do.
    fn sse(&self) -> String {
        let frame = |v: Value| format!("data: {v}\n\n");
        let mut out = String::new();
        for word in self.text.split_inclusive(' ') {
            out += &frame(json!({"choices": [{"delta": {"content": word}}]}));
        }
        for (i, (name, args)) in self.calls.iter().enumerate() {
            let mid = args.char_indices().nth(args.chars().count() / 2).map_or(0, |(i, _)| i);
            let (a, b) = args.split_at(mid);
            out += &frame(
                json!({"choices": [{"delta": {"tool_calls": [{"index": i, "id": format!("call{i}"), "type": "function", "function": {"name": name, "arguments": a}}]}}]}),
            );
            out += &frame(json!({"choices": [{"delta": {"tool_calls": [{"index": i, "function": {"arguments": b}}]}}]}));
        }
        if let Some(e) = &self.fail {
            return out + &frame(json!({"error": {"message": e}}));
        }
        let finish = self.finish.as_deref().unwrap_or(if self.calls.is_empty() { "stop" } else { "tool_calls" });
        out += &frame(json!({"choices": [{"delta": {}, "finish_reason": finish}], "usage": {"prompt_tokens": 100, "completion_tokens": 10}}));
        out + "data: [DONE]\n\n"
    }
}

/// What byob sent the model in one request.
#[derive(Debug, Clone)]
pub(crate) struct Request(pub(crate) Value);

impl Request {
    fn messages(&self) -> &[Value] {
        self.0["messages"].as_array().map_or(&[], Vec::as_slice)
    }

    pub(crate) fn system(&self) -> &str {
        self.messages().first().and_then(|m| m["content"].as_str()).unwrap_or_default()
    }

    /// The results of tool calls, oldest first, as the model reads them.
    pub(crate) fn tool_results(&self) -> Vec<String> {
        self.messages().iter().filter(|m| m["role"] == "tool").map(content_text).collect()
    }

    /// The latest tool result (what the model just learned).
    pub(crate) fn last_result(&self) -> String {
        self.tool_results().pop().unwrap_or_default()
    }

    /// The user's messages, oldest first.
    pub(crate) fn user_texts(&self) -> Vec<String> {
        self.messages().iter().filter(|m| m["role"] == "user").map(content_text).collect()
    }

    /// Names of the tools offered.
    pub(crate) fn tools(&self) -> Vec<String> {
        self.0["tools"].as_array().map(|t| t.iter().filter_map(|t| t["function"]["name"].as_str().map(str::to_owned)).collect()).unwrap_or_default()
    }

    /// A summary request of compaction, not a turn of the conversation.
    fn is_summary(&self) -> bool {
        self.system() == crate::compact::SYSTEM
    }
}

/// A message's text, whether a string or a list of parts.
fn content_text(m: &Value) -> String {
    match &m["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// The first artifact id in `text` that starts with `prefix-` (ids are `[a-z0-9-]`).
pub(crate) fn artifact_id(text: &str, prefix: &str) -> Option<String> {
    let start = format!("{prefix}-");
    let at = text.find(&start)?;
    let id: String = text[at..].chars().take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-').collect();
    Some(id.trim_end_matches('-').to_owned())
}

type Script = Box<dyn FnOnce(&Request) -> Answer + Send>;

/// A fake OpenAI-compatible server playing the model from a script: each conversation
/// request takes the next queued answer; compaction's summary requests are answered with a
/// short summary. With nothing queued it answers HTTP 500 ("the script ran out"), so a
/// scenario that asks more than it planned fails saying so instead of hanging.
pub(crate) struct Model {
    url: String,
    script: Arc<Mutex<VecDeque<Script>>>,
    requests: Arc<Mutex<Vec<Request>>>,
    /// Why a scripted answer's check failed, if one did.
    failed: Arc<Mutex<Option<String>>>,
}

/// The summary compaction gets (checked for in the next request).
pub(crate) const SUMMARY: &str = "## User goals\nTrack the harvest. (summary of earlier turns)";

impl Model {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let script: Arc<Mutex<VecDeque<Script>>> = Arc::default();
        let requests: Arc<Mutex<Vec<Request>>> = Arc::default();
        let failed: Arc<Mutex<Option<String>>> = Arc::default();
        let (queue, log, failure) = (Arc::clone(&script), Arc::clone(&requests), Arc::clone(&failed));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let req = Request(crate::loop_tests::read_request(&mut s));
                let answer = if req.is_summary() {
                    Some(Answer::text(SUMMARY))
                } else {
                    let next = queue.lock().unwrap().pop_front();
                    // A check in a scripted answer that fails is the scenario's failure, not
                    // a dead server: kept for `World::send` to report.
                    match next.map(|f| std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&req)))) {
                        Some(Ok(a)) => Some(a),
                        Some(Err(e)) => {
                            let why = e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| (*s).to_owned()));
                            *failure.lock().unwrap() = Some(why.unwrap_or_else(|| "a scripted answer panicked".into()));
                            None
                        }
                        None => None,
                    }
                };
                log.lock().unwrap().push(req);
                match answer {
                    Some(a) => {
                        let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{}", a.sse());
                    }
                    None => {
                        let body = json!({"error": {"message": "the scenario's model script ran out"}}).to_string();
                        let _ =
                            write!(s, "HTTP/1.1 500 X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    }
                }
            }
        });
        Self { url: format!("http://{addr}/v1"), script, requests, failed }
    }

    /// Fail the scenario if a check in a scripted answer failed.
    fn check(&self) {
        if let Some(why) = self.failed.lock().unwrap().take() {
            panic!("the model's check failed: {why}");
        }
    }

    /// Queue an answer worked out from what byob sends at that point.
    pub(crate) fn answers(&self, f: impl FnOnce(&Request) -> Answer + Send + 'static) {
        self.script.lock().unwrap().push_back(Box::new(f));
    }

    /// Queue a plain text answer (ending the turn).
    pub(crate) fn says(&self, text: &str) {
        let a = Answer::text(text);
        self.answers(move |_| a);
    }

    /// Queue an answer that calls one tool.
    pub(crate) fn calls(&self, name: &str, args: Value) {
        let a = Answer::call(name, args);
        self.answers(move |_| a);
    }

    /// Every request so far, oldest first.
    pub(crate) fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    /// The latest request.
    pub(crate) fn last(&self) -> Request {
        self.requests().pop().expect("the model was never asked")
    }

    /// Answers queued but not asked for yet.
    pub(crate) fn unused(&self) -> usize {
        self.script.lock().unwrap().len()
    }
}

// ---------------------------------------------------------------------------------------
// The world: data folder, project folder, engine, one client
// ---------------------------------------------------------------------------------------

/// A finished turn: the assistant's reply entry and every message seen on the way.
pub(crate) struct Turn {
    pub(crate) reply: Entry,
    pub(crate) error: Option<String>,
    /// The tool results the model read, in call order (steps show them shortened).
    pub(crate) results: Vec<String>,
}

impl Turn {
    /// The (first) step that ran `tool`.
    pub(crate) fn step(&self, tool: &str) -> &Step {
        self.steps(tool).into_iter().next().unwrap_or_else(|| panic!("no {tool} step in {:?}", self.tools()))
    }

    /// Every step that ran `tool`, in order.
    pub(crate) fn steps(&self, tool: &str) -> Vec<&Step> {
        self.reply.steps.iter().filter(|s| s.tool == tool).collect()
    }

    pub(crate) fn tools(&self) -> Vec<&str> {
        self.reply.steps.iter().map(|s| s.tool.as_str()).collect()
    }

    pub(crate) fn text(&self) -> &str {
        &self.reply.content
    }

    /// The whole result of the (first) `tool` call, as the model read it.
    pub(crate) fn result(&self, tool: &str) -> &str {
        self.results_of(tool).into_iter().next().unwrap_or_else(|| panic!("no {tool} result in {:?}", self.tools()))
    }

    /// The results of every `tool` call, in order.
    pub(crate) fn results_of(&self, tool: &str) -> Vec<&str> {
        assert_eq!(self.results.len(), self.reply.steps.len(), "every step's result went to the model: {:?}", self.tools());
        self.reply.steps.iter().zip(&self.results).filter(|(s, _)| s.tool == tool).map(|(_, r)| r.as_str()).collect()
    }
}

/// Assertions on a step that read like the story.
pub(crate) trait StepCheck {
    /// It ran and succeeded; returns its result text.
    fn done(&self) -> &str;
    /// It ran and failed; returns its result text.
    fn failed(&self) -> &str;
}

impl StepCheck for Step {
    fn done(&self) -> &str {
        assert_eq!(self.status, StepStatus::Done, "{} should have worked: {}", self.tool, self.detail);
        &self.detail
    }

    fn failed(&self) -> &str {
        assert_eq!(self.status, StepStatus::Failed, "{} should have failed: {}", self.tool, self.detail);
        &self.detail
    }
}

/// What a client keeps, as messages arrive.
#[derive(Default)]
pub(crate) struct Seen {
    /// The chat in view (`None`: a new chat, nothing sent yet).
    pub(crate) chat: Option<ChatSummary>,
    pub(crate) entries: Vec<Entry>,
    /// The chat in view's artifacts, as last listed and updated.
    pub(crate) artifacts: Vec<Artifact>,
    /// The chat list: pinned first, then recent; kept current by ChatUpdated.
    pub(crate) pinned: Vec<ChatSummary>,
    pub(crate) recent: Vec<ChatSummary>,
    pub(crate) workspaces: Vec<WorkspaceSummary>,
    pub(crate) notices: Vec<(NoticeLevel, String)>,
    /// The chat in view's compactions (summaries standing in for earlier entries).
    pub(crate) compactions: Vec<byob_protocol::Compaction>,
}

impl Seen {
    fn apply(&mut self, msg: &ServerMsg) {
        match msg {
            ServerMsg::ChatOpened { chat, .. } => {
                self.chat = chat.clone();
                self.entries.clear();
                self.artifacts.clear();
                self.compactions.clear();
            }
            ServerMsg::Transcript { entries } => self.entries = entries.clone(),
            ServerMsg::EntryAdded(e) => self.entries.push(e.clone()),
            ServerMsg::EntryDelta { id, content, reasoning } => {
                if let Some(e) = self.entries.iter_mut().find(|e| e.id == *id) {
                    e.append(content, reasoning);
                }
            }
            ServerMsg::StepUpdated { entry, step } => {
                if let Some(e) = self.entries.iter_mut().find(|e| e.id == *entry) {
                    e.upsert_step(step.clone());
                }
            }
            ServerMsg::EntryFinished { id, error, stats } => {
                if let Some(e) = self.entries.iter_mut().find(|e| e.id == *id) {
                    e.done = true;
                    e.error.clone_from(error);
                    e.stats.clone_from(stats);
                }
            }
            ServerMsg::EntryUndo { id, undoable, undone } => {
                if let Some(e) = self.entries.iter_mut().find(|e| e.id == *id) {
                    (e.undoable, e.undone) = (*undoable, *undone);
                }
            }
            ServerMsg::Artifacts { artifacts, .. } => self.artifacts = artifacts.clone(),
            ServerMsg::ArtifactUpdated(a) => match self.artifacts.iter_mut().find(|x| x.id == a.id) {
                Some(x) => *x = a.clone(),
                None => self.artifacts.push(a.clone()),
            },
            ServerMsg::Chats { pinned, recent, after: None, .. } => (self.pinned, self.recent) = (pinned.clone(), recent.clone()),
            ServerMsg::ChatUpdated(s) => {
                if self.chat.as_ref().is_some_and(|c| c.id == s.id) {
                    self.chat = Some(s.clone());
                }
                self.pinned.retain(|c| c.id != s.id);
                self.recent.retain(|c| c.id != s.id);
                if s.pinned_at.is_some() {
                    self.pinned.insert(0, s.clone());
                } else {
                    self.recent.push(s.clone());
                    self.recent.sort_by(|a, b| (b.updated_at, &b.id).cmp(&(a.updated_at, &a.id)));
                }
            }
            ServerMsg::Workspaces { items, .. } => self.workspaces = items.clone(),
            ServerMsg::Compactions { items, .. } => self.compactions = items.clone(),
            ServerMsg::Compacted(c) => self.compactions.push(c.clone()),
            ServerMsg::Notice { level, text } => self.notices.push((*level, text.clone())),
            _ => {}
        }
    }

    /// Every chat listed (pinned, then recent).
    pub(crate) fn listed(&self) -> Vec<&ChatSummary> {
        self.pinned.iter().chain(&self.recent).collect()
    }
}

/// byob running on a temporary data folder with one project, and a client attached to it.
pub(crate) struct World {
    pub(crate) model: Model,
    /// byob's data folder (database, settings, plugins); kept across [`World::restart`].
    pub(crate) home: tempfile::TempDir,
    _project_dir: tempfile::TempDir,
    /// The project's folder (canonical): new chats work here.
    pub(crate) project: PathBuf,
    /// Applied to the engine's config at every start.
    tune: Box<dyn Fn(&mut EngineConfig)>,
    engine: EngineHandle,
    ui: LocalConnection,
    pub(crate) seen: Seen,
}

impl World {
    pub(crate) fn new() -> Self {
        Self::with(|_| {})
    }

    /// A world whose engine config gets `tune` on top of the defaults (at every start).
    pub(crate) fn with(tune: impl Fn(&mut EngineConfig) + 'static) -> Self {
        let model = Model::start();
        let home = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        let project = project_dir.path().canonicalize().unwrap();
        let tune: Box<dyn Fn(&mut EngineConfig)> = Box::new(tune);
        let engine = start(&model.url, home.path(), &project, &tune);
        let ui = engine.connect(Arc::new(|| {}));
        let mut w = Self { model, home, _project_dir: project_dir, project, tune, engine, ui, seen: Seen::default() };
        w.hello();
        w
    }

    // ---- the project folder ----

    /// Write a file in the project.
    pub(crate) fn file(&self, rel: &str, content: &str) {
        let path = self.project.join(rel);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    /// A file of the project, if it's there.
    pub(crate) fn read(&self, rel: &str) -> Option<String> {
        std::fs::read_to_string(self.project.join(rel)).ok()
    }

    /// The chat in view's id.
    pub(crate) fn chat_id(&self) -> String {
        self.seen.chat.as_ref().map(|c| c.id.clone()).expect("no chat in view yet")
    }

    /// This project's workspace as byob lists it.
    pub(crate) fn workspace(&self) -> &WorkspaceSummary {
        let folder = self.project.display().to_string();
        self.seen.workspaces.iter().find(|w| w.folder == folder).unwrap_or_else(|| panic!("no workspace for {folder} in {:?}", self.seen.workspaces))
    }

    /// An artifact's latest version, as bytes, from the project.
    pub(crate) fn artifact_bytes(&self, id: &str) -> Vec<u8> {
        let meta = byob_tools::artifact::load(&self.project, id).unwrap_or_else(|e| panic!("no artifact {id}: {e}"));
        let latest = meta.artifact.latest().unwrap().clone();
        std::fs::read(self.project.join(&latest.path)).unwrap()
    }

    // ---- talking to byob ----

    /// The next message for the client (failing after [`PATIENCE`]).
    fn next(&mut self) -> ServerMsg {
        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Some(m) = self.ui.try_recv() {
                self.seen.apply(&m);
                return m;
            }
            assert!(Instant::now() < deadline, "timed out waiting for the engine");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Wait for a message `f` picks out (everything on the way is applied to [`World::seen`]).
    pub(crate) fn wait<T>(&mut self, mut f: impl FnMut(&ServerMsg) -> Option<T>) -> T {
        loop {
            let m = self.next();
            if let Some(t) = f(&m) {
                return t;
            }
        }
    }

    pub(crate) fn tell(&mut self, msg: ClientMsg) {
        self.ui.send(msg);
    }

    /// Say hello and take in the answer (up to the chat's artifact list, which comes last).
    fn hello(&mut self) {
        self.seen = Seen::default();
        self.ui.send(ClientMsg::hello("scenario"));
        self.wait(|m| matches!(m, ServerMsg::Artifacts { .. }).then_some(()));
    }

    /// Send a message and wait for the reply to finish.
    pub(crate) fn send(&mut self, text: &str) -> Turn {
        self.ui.send(ClientMsg::SendMessage { text: text.into(), attachments: vec![] });
        // A new chat is made with its first message (announced before the reply starts).
        let mut made = None;
        let mut reply = None;
        let fresh = self.seen.chat.is_none();
        let known: Vec<String> = self.seen.listed().iter().map(|c| c.id.clone()).collect();
        let (id, error) = self.wait(|m| match m {
            ServerMsg::ChatUpdated(s) if fresh && made.is_none() && !known.contains(&s.id) => {
                made = Some(s.clone());
                None
            }
            ServerMsg::EntryAdded(e) if e.role == byob_protocol::EntryRole::Assistant => {
                reply = Some(e.id);
                None
            }
            ServerMsg::EntryFinished { id, error, .. } if Some(*id) == reply => Some((*id, error.clone())),
            _ => None,
        });
        self.model.check();
        if made.is_some() {
            self.seen.chat = made;
        }
        let reply = self.seen.entries.iter().find(|e| e.id == id).cloned().unwrap();
        // The turn's last request carries every result of it (earlier turns' aren't sent).
        let results = if reply.steps.is_empty() { Vec::new() } else { self.model.last().tool_results() };
        Turn { reply, error, results }
    }

    /// Start a new, empty chat in this project.
    pub(crate) fn new_chat(&mut self) {
        self.ui.send(ClientMsg::NewChat { workspace: None });
        self.wait(|m| matches!(m, ServerMsg::Artifacts { .. }).then_some(()));
    }

    pub(crate) fn open_chat(&mut self, id: &str) {
        self.ui.send(ClientMsg::OpenChat { id: id.into() });
        self.wait(|m| matches!(m, ServerMsg::Artifacts { .. }).then_some(()));
    }

    /// Undo reply `entry`'s changes: `Ok` once done (its "Undid …" notice, which comes
    /// last), or the reason byob refused.
    pub(crate) fn undo(&mut self, entry: u64) -> Result<(), String> {
        self.ui.send(ClientMsg::Undo { entry });
        self.wait(|m| match m {
            ServerMsg::Notice { level: NoticeLevel::Info, text } if text.starts_with("Undid ") => Some(Ok(())),
            ServerMsg::Notice { level: NoticeLevel::Error, text } => Some(Err(text.clone())),
            _ => None,
        })?;
        let e = self.seen.entries.iter().find(|e| e.id == entry);
        assert!(e.is_some_and(|e| e.undone && !e.undoable), "the reply shows as undone");
        Ok(())
    }

    /// Replace the other folders this project works in.
    pub(crate) fn link_folders(&mut self, folders: &[&Path]) {
        let id = self.workspace().id.clone();
        let want: Vec<String> = folders.iter().map(|f| f.display().to_string()).collect();
        self.ui.send(ClientMsg::SetWorkspaceFolders { id: id.clone(), folders: want.clone() });
        self.wait(|m| match m {
            ServerMsg::Workspaces { items, .. } => items.iter().any(|w| w.id == id && w.folders == want).then_some(()),
            ServerMsg::Notice { level, text } if *level != NoticeLevel::Info => panic!("linking refused: {text}"),
            _ => None,
        });
    }

    /// The client goes away and comes back (a window reopened, a tab reloaded).
    pub(crate) fn reconnect(&mut self) {
        self.ui = self.engine.connect(Arc::new(|| {}));
        self.hello();
    }

    /// Quit byob and start it again on the same data folder and project, then reconnect.
    pub(crate) fn restart(&mut self) {
        // The engine's thread outlives its handle (it holds its own inbox); dropping the
        // client and handle leaves it idle, with nothing more to do.
        self.engine = start(&self.model.url, self.home.path(), &self.project, &self.tune);
        self.reconnect();
    }

    /// The chat store, opened beside the engine (to look at what was saved).
    pub(crate) fn store(&self) -> Box<dyn byob_store::ChatStore> {
        crate::test_store::open(&Paths::at(self.home.path()))
    }
}

/// The engine as the desktop app makes it, apart from the model and the code sandbox: the
/// built-in tools, `run_command` (unsandboxed: the sandbox helper is the app binary), the
/// bundled plugins with native backends, every tool call allowed.
fn start(url: &str, home: &Path, project: &Path, tune: &dyn Fn(&mut EngineConfig)) -> EngineHandle {
    let mut config = EngineConfig::new(InferenceConfig { base_url: url.to_owned(), ..Default::default() });
    config.workspace = project.to_path_buf();
    config.register_workspace = true;
    config.paths = Some(Paths::at(home));
    config.auto_title = false;
    config.policy = Some(Policy::ALLOW_ALL);
    config.compaction = CompactOptions::default();
    config.tools = byob_tools::builtin();
    let exec = byob_exec::Exec { mode: byob_exec::Mode::Unsandboxed, data_dir: None, envs: Arc::new(byob_exec::env::Envs::none()) };
    byob_exec::register(&mut config.tools, exec);
    let natives =
        Natives::builtin().prompt("exec", |_| "Running code: run_command runs in the project.".to_owned()).backend(byob_sheets::ID, byob_sheets::factory());
    let mut bundled = bundled_plugins();
    // Mail and the calendar need accounts; web would reach the network.
    bundled.retain(|p| !matches!(p.id.as_str(), "mail" | "calendar" | "web"));
    config.plugins = Some(PluginsConfig {
        global_roots: vec![home.join("plugins")],
        scratch_root: Some(home.join("chats")),
        python_root: Some(home.join("python")),
        python: byob_plugins::default_python(),
        bundled,
        bundled_cache: Some(home.join("bundled-plugins")),
        natives,
        ..Default::default()
    });
    tune(&mut config);
    EngineHandle::spawn_with(config).unwrap()
}
