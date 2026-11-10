//! The plain-files chat backend: one folder per chat.
//!
//! ```text
//! <root>/chats/<YYYY-MM-DD>-<slug>-<shortid>/
//!     events.jsonl   canonical, append-only (see `event.rs`)
//!     chat.md        generated, read-only view; never read back
//!     agents/<run>/events.jsonl   one subagent run each (see `agents.rs`)
//! <root>/grants.json global always-allow grants
//! <root>/undo.jsonl  undo entries of tasks no chat message names (rare)
//! <root>/.index.json listing cache (derived; safe to delete)
//! ```
//!
//! State is rebuilt by replaying events. The listing comes from an in-memory index of
//! chat summaries, seeded from `.index.json` and checked against each `events.jsonl`'s
//! size + mtime; a chat's full state is replayed lazily the first time it's needed.
//! `list_chats` (first page) rescans the folders, so chats synced in from elsewhere show up.

mod agents;
mod event;
mod render;
mod state;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};

use self::event::{Event, Line, Stamp};
use self::state::{ChatState, Journal, Summary, message_id, split_id};
use crate::backend::{ChatExport, ChatStore};
use crate::compactions::MAX_SUMMARY;
use crate::error::{Result, StoreError};
use crate::files::{load_json, save_json, write_atomic};
use crate::model::{
    AgentRun, Chat, ChatCursor, ChatPage, Compaction, GrantScope, MessageId, MessageStats, NewAgentRun, NewCompaction, NewMessage, NewStep, Step, StepResult,
    StoredMessage, UndoOp, agent_run_id_ok,
};
use crate::settings::StorageKind;
use crate::steps::{MAX_STEP_OUTPUT, bound};
use crate::store::{MAX_PAGE, now};

#[cfg(test)]
pub(crate) use self::render::{folder_name, short_id, slug};

const EVENTS: &str = "events.jsonl";
const CHAT_MD: &str = "chat.md";
const INDEX: &str = ".index.json";
const INDEX_VERSION: u32 = 1;
/// Chats whose full state stays in memory at once; the least recently used is dropped.
const MAX_LOADED: usize = 64;

/// The folder backend. `Send` (and `Sync`, via an internal mutex). One `FolderStore` per
/// root at a time: two byob processes writing the same folder would interleave lines.
pub struct FolderStore {
    root: PathBuf,
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for FolderStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FolderStore").field("root", &self.root).finish_non_exhaustive()
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct GrantsFile {
    #[serde(default)]
    v: u32,
    #[serde(default)]
    global: BTreeSet<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct IndexEntry {
    folder: String,
    stamp: Stamp,
    summary: Summary,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct IndexFile {
    v: u32,
    chats: Vec<IndexEntry>,
}

#[derive(Debug)]
struct Entry {
    folder: String,
    summary: Summary,
    /// Of `events.jsonl` as last read or written by us.
    stamp: Stamp,
    slot: u32,
}

#[derive(Debug)]
struct Cached {
    state: ChatState,
    used: u64,
}

#[derive(Debug)]
struct Inner {
    root: PathBuf,
    chats_dir: PathBuf,
    /// By chat id.
    entries: HashMap<String, Entry>,
    /// Folder name → chat id.
    by_folder: HashMap<String, String>,
    /// Session slot → chat id (message ids embed the slot). Never reused.
    slots: Vec<String>,
    slot_of: HashMap<String, u32>,
    loaded: HashMap<String, Cached>,
    /// Folders that aren't (usable) chats, with the stamp we saw, so they're not re-read.
    ignored: HashMap<String, Stamp>,
    /// Task id → chat whose messages carry it (routes undo entries).
    tasks: HashMap<String, String>,
    global: BTreeSet<String>,
    orphans: Journal,
    tick: u64,
    index_dirty: bool,
    /// Agent run id → (chat, highest step seq), for runs written this session.
    runs: HashMap<String, (String, i64)>,
}

fn not_found(what: impl Into<String>) -> StoreError {
    StoreError::NotFound(what.into())
}

fn msg_not_found(id: MessageId) -> StoreError {
    not_found(format!("message {}", id.0))
}

impl FolderStore {
    /// Open (creating if needed) a chat folder at `root`. Damaged files are skipped with
    /// warnings; only an unusable root (permissions, not a directory) is an error.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_owned();
        let chats_dir = root.join("chats");
        std::fs::create_dir_all(&chats_dir).map_err(|e| StoreError::io(&chats_dir, e))?;
        let grants: GrantsFile = load_json(&root.join("grants.json")).value;
        let mut inner = Inner {
            root: root.clone(),
            chats_dir,
            entries: HashMap::new(),
            by_folder: HashMap::new(),
            slots: Vec::new(),
            slot_of: HashMap::new(),
            loaded: HashMap::new(),
            ignored: HashMap::new(),
            tasks: HashMap::new(),
            global: grants.global,
            orphans: Journal::default(),
            tick: 0,
            index_dirty: false,
            runs: HashMap::new(),
        };
        inner.load_orphans();
        let cache = inner.read_index();
        if inner.refresh(&cache)? {
            inner.save_index();
        }
        Ok(Self { root, inner: Mutex::new(inner) })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Rescan the chat folders for changes made by someone else (sync, another machine).
    /// `list_chats` does this for its first page.
    pub fn refresh(&self) -> Result<()> {
        let mut inner = self.lock();
        if inner.refresh(&HashMap::new())? {
            inner.save_index();
        }
        Ok(())
    }

    /// The folder holding a chat, if it exists.
    pub fn chat_dir(&self, id: &str) -> Option<PathBuf> {
        let inner = self.lock();
        inner.entries.get(id).map(|e| inner.chats_dir.join(&e.folder))
    }
}

impl Drop for FolderStore {
    fn drop(&mut self) {
        let inner = self.inner.get_mut().unwrap_or_else(PoisonError::into_inner);
        if inner.index_dirty {
            inner.save_index();
        }
    }
}

impl Inner {
    fn events_path(&self, folder: &str) -> PathBuf {
        self.chats_dir.join(folder).join(EVENTS)
    }

    fn orphans_path(&self) -> PathBuf {
        self.root.join("undo.jsonl")
    }

    fn load_orphans(&mut self) {
        let path = self.orphans_path();
        if !path.exists() {
            return;
        }
        match event::read(&path) {
            Ok(parsed) => {
                for w in &parsed.warnings {
                    log::warn!("byob-store: {w}");
                }
                for l in &parsed.lines {
                    self.orphans.apply(&l.event);
                }
                self.orphans.note_bad(&parsed.bad);
            }
            Err(e) => log::warn!("byob-store: {e}"),
        }
    }

    fn read_index(&self) -> HashMap<String, IndexEntry> {
        let path = self.root.join(INDEX);
        let Ok(bytes) = std::fs::read(&path) else { return HashMap::new() };
        match serde_json::from_slice::<IndexFile>(&bytes) {
            Ok(f) if f.v == INDEX_VERSION => f.chats.into_iter().map(|e| (e.folder.clone(), e)).collect(),
            Ok(_) => HashMap::new(),
            Err(e) => {
                log::warn!("byob-store: ignoring {}: {e}", path.display());
                HashMap::new()
            }
        }
    }

    fn save_index(&mut self) {
        let mut chats: Vec<IndexEntry> =
            self.entries.values().map(|e| IndexEntry { folder: e.folder.clone(), stamp: e.stamp, summary: e.summary.clone() }).collect();
        chats.sort_by(|a, b| a.folder.cmp(&b.folder));
        let file = IndexFile { v: INDEX_VERSION, chats };
        let path = self.root.join(INDEX);
        match serde_json::to_vec(&file).map_err(StoreError::from).and_then(|b| write_atomic(&path, &b, None)) {
            Ok(()) => self.index_dirty = false,
            Err(e) => log::warn!("byob-store: couldn't save {}: {e}", path.display()),
        }
    }

    /// Bring the index in line with the folders on disk. Folders whose `events.jsonl` size
    /// and mtime match what we know (or `cache`) aren't read. Returns whether anything changed.
    fn refresh(&mut self, cache: &HashMap<String, IndexEntry>) -> Result<bool> {
        let rd = std::fs::read_dir(&self.chats_dir).map_err(|e| StoreError::io(&self.chats_dir, e))?;
        let mut names: Vec<String> = rd
            .flatten()
            .filter_map(|ent| {
                let name = ent.file_name().to_str().map(str::to_owned);
                if name.is_none() {
                    log::warn!("byob-store: skipping non-UTF-8 folder {:?}", ent.file_name());
                }
                name
            })
            .filter(|n| !n.starts_with('.'))
            .collect();
        // Sorted, so which of two folders claiming the same chat wins doesn't depend on
        // directory order.
        names.sort();
        let mut seen = HashSet::new();
        let mut changed = false;
        for name in names {
            let Some(stamp) = Stamp::read(&self.events_path(&name)) else { continue };
            seen.insert(name.clone());
            if self.by_folder.get(&name).and_then(|id| self.entries.get(id)).is_some_and(|e| e.stamp == stamp) {
                continue;
            }
            if self.ignored.get(&name) == Some(&stamp) {
                continue;
            }
            self.ignored.remove(&name);
            changed = true;
            match cache.get(&name).filter(|c| c.stamp == stamp) {
                Some(c) if self.insert_entry(&name, c.summary.clone(), stamp, None) => {}
                Some(_) => {
                    self.ignored.insert(name.clone(), stamp);
                }
                None => self.load_folder(&name, stamp),
            }
        }
        self.ignored.retain(|f, _| seen.contains(f));
        let gone: Vec<String> = self.by_folder.keys().filter(|f| !seen.contains(*f)).cloned().collect();
        for f in gone {
            changed = true;
            self.forget_folder(&f);
        }
        Ok(changed)
    }

    /// Replay a folder's events and index it. `stamp` was taken before reading.
    fn load_folder(&mut self, folder: &str, stamp: Stamp) {
        let path = self.events_path(folder);
        let parsed = match event::read(&path) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("byob-store: {e}");
                return;
            }
        };
        let mut warnings = parsed.warnings.clone();
        let state = ChatState::replay(&parsed, &mut warnings);
        for w in &warnings {
            log::warn!("byob-store: {w}");
        }
        match state {
            None => {
                log::warn!("byob-store: {} has no chat_created event; ignoring the folder", path.display());
                self.ignored.insert(folder.to_owned(), stamp);
            }
            Some(s) if s.deleted => {
                self.forget_folder(folder);
                self.ignored.insert(folder.to_owned(), stamp);
            }
            Some(s) => {
                if !self.insert_entry(folder, s.summary(), stamp, Some(s)) {
                    self.ignored.insert(folder.to_owned(), stamp);
                }
            }
        }
    }

    /// Index a chat found in `folder`. A loaded state is replaced by `state` (or dropped as
    /// stale when there's none). False if another folder already holds this chat.
    fn insert_entry(&mut self, folder: &str, summary: Summary, stamp: Stamp, state: Option<ChatState>) -> bool {
        let id = summary.id.clone();
        if let Some(old) = self.entries.get(&id).map(|e| e.folder.clone())
            && old != folder
        {
            if Stamp::read(&self.events_path(&old)).is_some() {
                log::warn!("byob-store: folders {old} and {folder} both hold chat {id}; using {old}");
                return false;
            }
            self.forget_folder(&old);
        }
        let slot = match self.slot_of.get(&id) {
            Some(s) => *s,
            None => {
                let s = u32::try_from(self.slots.len()).unwrap_or(u32::MAX);
                self.slots.push(id.clone());
                self.slot_of.insert(id.clone(), s);
                s
            }
        };
        for t in &summary.tasks {
            self.tasks.insert(t.clone(), id.clone());
        }
        match state {
            Some(state) => {
                if let Some(c) = self.loaded.get_mut(&id) {
                    c.state = state;
                }
            }
            None => {
                self.loaded.remove(&id);
            }
        }
        self.by_folder.insert(folder.to_owned(), id.clone());
        self.entries.insert(id, Entry { folder: folder.to_owned(), summary, stamp, slot });
        self.index_dirty = true;
        true
    }

    fn forget_folder(&mut self, folder: &str) {
        let Some(id) = self.by_folder.remove(folder) else { return };
        if self.entries.get(&id).is_some_and(|e| e.folder == folder) {
            self.entries.remove(&id);
            self.loaded.remove(&id);
            self.tasks.retain(|_, c| *c != id);
        }
        self.index_dirty = true;
    }

    /// Make sure a chat's full state is in memory. False if there's no such chat.
    fn ensure_loaded(&mut self, id: &str) -> Result<bool> {
        self.tick += 1;
        let tick = self.tick;
        if let Some(c) = self.loaded.get_mut(id) {
            c.used = tick;
            return Ok(true);
        }
        let Some(folder) = self.entries.get(id).map(|e| e.folder.clone()) else { return Ok(false) };
        let path = self.events_path(&folder);
        let stamp = Stamp::read(&path);
        let parsed = match event::read(&path) {
            Ok(p) => p,
            Err(e) => {
                // Folder vanished under us (synced delete): forget it.
                log::warn!("byob-store: {e}");
                self.forget_folder(&folder);
                return Ok(false);
            }
        };
        let mut warnings = parsed.warnings.clone();
        let state = ChatState::replay(&parsed, &mut warnings);
        for w in &warnings {
            log::warn!("byob-store: {w}");
        }
        let Some(state) = state.filter(|s| s.chat.id == id && !s.deleted) else {
            log::warn!("byob-store: {} no longer holds chat {id}", path.display());
            self.forget_folder(&folder);
            return Ok(false);
        };
        if self.loaded.len() >= MAX_LOADED
            && let Some(old) = self.loaded.iter().min_by_key(|(_, c)| c.used).map(|(k, _)| k.clone())
        {
            self.loaded.remove(&old);
        }
        let summary = state.summary();
        self.loaded.insert(id.to_owned(), Cached { state, used: tick });
        if let Some(e) = self.entries.get_mut(id)
            && let Some(stamp) = stamp
            && (e.stamp != stamp || e.summary != summary)
        {
            e.stamp = stamp;
            e.summary = summary;
            self.index_dirty = true;
        }
        Ok(true)
    }

    fn state(&self, id: &str) -> Option<&ChatState> {
        self.loaded.get(id).map(|c| &c.state)
    }

    fn state_or_404(&mut self, id: &str) -> Result<&ChatState> {
        if !self.ensure_loaded(id)? {
            return Err(not_found(format!("chat {id}")));
        }
        self.state(id).ok_or_else(|| not_found(format!("chat {id}")))
    }

    /// The chat and message number a message id points at, if both exist.
    fn resolve(&mut self, id: MessageId) -> Result<Option<(String, u32, u32)>> {
        let Some((slot, m)) = split_id(id) else { return Ok(None) };
        let Some(chat) = self.slots.get(usize::try_from(slot).unwrap_or(usize::MAX)).cloned() else { return Ok(None) };
        if !self.ensure_loaded(&chat)? {
            return Ok(None);
        }
        Ok(self.state(&chat).filter(|s| s.messages.contains_key(&m)).map(|_| (chat, slot, m)))
    }

    fn target(&mut self, id: MessageId) -> Result<(String, u32, u32)> {
        self.resolve(id)?.ok_or_else(|| msg_not_found(id))
    }

    fn write(&mut self, id: &str, events: Vec<Event>, sync: bool, render: bool) -> Result<()> {
        let ts = now();
        self.write_lines(id, events.into_iter().map(|e| Line::new(ts, e)).collect(), sync, render)
    }

    /// Append events to a loaded chat's file, then apply them to its state.
    fn write_lines(&mut self, id: &str, lines: Vec<Line>, sync: bool, render: bool) -> Result<()> {
        let text = lines.iter().map(Line::encode).collect::<Result<String>>()?;
        let Some(folder) = self.entries.get(id).map(|e| e.folder.clone()) else { return Err(not_found(format!("chat {id}"))) };
        let path = self.events_path(&folder);
        let (before, after) = event::append(&path, &text, sync)?;
        let external = self.entries.get(id).is_some_and(|e| e.stamp != before);
        if let Some(e) = self.entries.get_mut(id) {
            e.stamp = after;
        }
        if external {
            // Someone else appended since we last read it: replay everything, ours included.
            log::info!("byob-store: {} changed outside byob; reloading", path.display());
            self.loaded.remove(id);
            self.ensure_loaded(id)?;
        } else if let Some(c) = self.loaded.get_mut(id) {
            for l in &lines {
                if let Err(w) = c.state.apply(l) {
                    log::warn!("byob-store: chat {id}: {w}");
                }
            }
        }
        if let Some(summary) = self.state(id).map(ChatState::summary) {
            for t in &summary.tasks {
                self.tasks.insert(t.clone(), id.to_owned());
            }
            if let Some(e) = self.entries.get_mut(id) {
                e.summary = summary;
            }
        }
        self.index_dirty = true;
        if render {
            self.render(id);
        }
        Ok(())
    }

    /// Regenerate `chat.md`. Failure only costs the view, so it's logged, not returned.
    fn render(&self, id: &str) {
        let (Some(state), Some(e)) = (self.state(id), self.entries.get(id)) else { return };
        let path = self.chats_dir.join(&e.folder).join(CHAT_MD);
        if let Err(err) = write_atomic(&path, render::chat_md(state).as_bytes(), None) {
            log::warn!("byob-store: couldn't write {}: {err}", path.display());
        }
    }

    /// Make a new chat folder named after the chat, adding `-2`, `-3`, … if taken.
    fn new_folder(&self, created_at: i64, title: &str, id: &str) -> Result<String> {
        let base = render::folder_name(created_at, title, id);
        let mut name = base.clone();
        for n in 2..10_000 {
            let dir = self.chats_dir.join(&name);
            match std::fs::create_dir(&dir) {
                Ok(()) => return Ok(name),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => name = format!("{base}-{n}"),
                Err(e) => return Err(StoreError::io(dir, e)),
            }
        }
        Err(StoreError::Exists(format!("folder {base}")))
    }

    /// Create a chat folder holding `lines` (starting with `chat_created`) and index it.
    fn create_from_lines(&mut self, id: &str, title: &str, created_at: i64, lines: &[Line]) -> Result<ChatState> {
        if self.entries.contains_key(id) {
            return Err(StoreError::Exists(format!("chat {id}")));
        }
        let text = lines.iter().map(Line::encode).collect::<Result<String>>()?;
        let folder = self.new_folder(created_at, title, id)?;
        let stamp = event::create(&self.events_path(&folder), &text)?;
        let parsed = event::Parsed { lines: lines.to_vec(), ..Default::default() };
        let mut warnings = Vec::new();
        let state = ChatState::replay(&parsed, &mut warnings).ok_or_else(|| StoreError::BadValue { column: "events", detail: "no chat_created".into() })?;
        for w in &warnings {
            log::warn!("byob-store: {w}");
        }
        if !self.insert_entry(&folder, state.summary(), stamp, None) {
            return Err(StoreError::Exists(format!("chat {id}")));
        }
        self.tick += 1;
        self.loaded.insert(id.to_owned(), Cached { state: state.clone(), used: self.tick });
        self.render(id);
        Ok(state)
    }

    fn chat_dir(&self, id: &str) -> Option<PathBuf> {
        self.entries.get(id).map(|e| self.chats_dir.join(&e.folder))
    }

    /// A run's chat folder, checking the run belongs to `chat` and exists.
    fn run_chat_dir(&mut self, chat: &str, run_id: &str) -> Result<PathBuf> {
        let missing = || not_found(format!("agent run {run_id}"));
        if !agent_run_id_ok(run_id) {
            return Err(missing());
        }
        let dir = self.chat_dir(chat).ok_or_else(missing)?;
        if !self.runs.contains_key(run_id) {
            let rec = agents::replay(&agents::run_file(&dir, run_id), run_id, chat).ok_or_else(missing)?;
            let last = rec.run.steps.iter().map(|s| s.seq).max().unwrap_or(0);
            self.runs.insert(run_id.to_owned(), (chat.to_owned(), last));
        }
        match self.runs.get(run_id) {
            Some((c, _)) if c == chat => Ok(dir),
            _ => Err(missing()),
        }
    }

    fn save_grants(&self) -> Result<()> {
        save_json(&self.root.join("grants.json"), &GrantsFile { v: 1, global: self.global.clone() }, None)
    }

    fn append_orphan(&mut self, event: Event) -> Result<()> {
        let path = self.orphans_path();
        if !path.exists() {
            event::create(&path, "")?;
        }
        event::append(&path, &Line::new(now(), event.clone()).encode()?, false)?;
        self.orphans.apply(&event);
        Ok(())
    }

    /// The chat an undo task belongs to, loaded.
    fn task_chat(&mut self, task: &str) -> Result<Option<String>> {
        let Some(chat) = self.tasks.get(task).cloned() else { return Ok(None) };
        Ok(self.ensure_loaded(&chat)?.then_some(chat))
    }
}

fn damaged(task: &str) -> StoreError {
    StoreError::BadValue { column: "undo_ops.op", detail: format!("task {task}: a journal entry is damaged") }
}

/// Events that recreate an exported chat, timestamps included.
fn import_lines(e: &ChatExport) -> Result<Vec<Line>> {
    let c = &e.chat;
    let mut lines = vec![Line::new(c.created_at, Event::ChatCreated { id: c.id.clone(), title: c.title.clone(), workspace: c.workspace.clone() })];
    for (i, m) in e.messages.iter().enumerate() {
        let n = u32::try_from(i + 1).map_err(|_| StoreError::BadValue { column: "messages", detail: "too many messages".into() })?;
        let ts = m.created_at;
        lines.push(Line::new(
            ts,
            Event::MessageAdded { m: n, role: m.role, content: m.content.clone(), reasoning: m.reasoning.clone(), done: m.done, task: m.task_id.clone() },
        ));
        for s in &m.steps {
            lines.push(Line::new(
                ts,
                Event::StepAdded { m: n, seq: s.seq, tool: s.tool.clone(), title: s.title.clone(), access: s.access, args: s.args.clone() },
            ));
            if let Some(r) = &s.result {
                lines.push(Line::new(
                    ts,
                    Event::StepFinished {
                        m: n,
                        seq: s.seq,
                        output: bound(&r.output, MAX_STEP_OUTPUT),
                        failed: r.failed,
                        changes: r.changes.clone(),
                        card: r.card.clone(),
                    },
                ));
            }
        }
        if m.done && (m.error.is_some() || m.stats.is_some()) {
            lines.push(Line::new(
                ts,
                Event::MessageFinished {
                    m: n,
                    error: m.error.clone(),
                    stats: m.stats,
                    content_len: Some(m.content.len()),
                    reasoning_len: Some(m.reasoning.len()),
                },
            ));
        }
        if m.undone {
            lines.push(Line::new(ts, Event::MessageUndone { m: n, undone: true }));
        }
    }
    let count = e.messages.len();
    for (i, x) in e.compactions.iter().enumerate() {
        let k = &x.compaction;
        let n = u32::try_from(i + 1).map_err(|_| StoreError::BadValue { column: "compactions", detail: "too many compactions".into() })?;
        // Messages were numbered 1.. above, so covering the first `covers` is `through = covers`.
        let through = u32::try_from(x.covers.min(count)).unwrap_or(u32::MAX);
        lines.push(Line::new(
            k.created_at,
            Event::CompactionAdded {
                n,
                through,
                summary: bound(&k.summary, MAX_SUMMARY),
                tokens_before: k.tokens_before,
                tokens_after: k.tokens_after,
                prompt_tokens: k.prompt_tokens,
                completion_tokens: k.completion_tokens,
                focus: k.focus.clone(),
                manual: k.manual,
            },
        ));
    }
    for tool in &e.grants {
        lines.push(Line::new(c.updated_at, Event::Grant { tool: tool.clone(), allowed: true }));
    }
    for (task, ops) in &e.undo {
        for op in ops {
            lines.push(Line::new(c.updated_at, Event::UndoOp { task: task.clone(), op: op.clone() }));
        }
    }
    if c.pinned_at.is_some() {
        lines.push(Line::new(c.updated_at, Event::Pinned { pinned_at: c.pinned_at }));
    }
    lines.push(Line::new(c.updated_at, Event::Touched { updated_at: c.updated_at }));
    Ok(lines)
}

fn sorted_page(mut chats: Vec<Chat>, limit: usize) -> ChatPage {
    chats.sort_by(|a, b| (b.updated_at, &b.id).cmp(&(a.updated_at, &a.id)));
    let next = if chats.len() > limit {
        chats.truncate(limit);
        chats.last().map(Chat::cursor)
    } else {
        None
    };
    ChatPage { chats, next }
}

impl ChatStore for FolderStore {
    fn kind(&self) -> StorageKind {
        StorageKind::Folder
    }

    fn path(&self) -> &Path {
        &self.root
    }

    fn create_chat(&self, id: &str, title: &str, workspace: &Path) -> Result<Chat> {
        let ts = now();
        let line = Line::new(ts, Event::ChatCreated { id: id.to_owned(), title: title.to_owned(), workspace: workspace.to_owned() });
        Ok(self.lock().create_from_lines(id, title, ts, &[line])?.chat)
    }

    fn chat(&self, id: &str) -> Result<Option<Chat>> {
        Ok(self.lock().entries.get(id).map(|e| e.summary.chat()))
    }

    fn list_chats(&self, limit: usize, after: Option<&ChatCursor>) -> Result<ChatPage> {
        let limit = limit.min(MAX_PAGE);
        if limit == 0 {
            return Ok(ChatPage { chats: Vec::new(), next: None });
        }
        let mut inner = self.lock();
        if after.is_none() && inner.refresh(&HashMap::new())? {
            inner.save_index();
        }
        let before = |c: &Summary| after.is_none_or(|a| c.updated_at < a.updated_at || (c.updated_at == a.updated_at && c.id < a.id));
        let mut chats: Vec<Chat> = inner.entries.values().filter(|e| before(&e.summary)).map(|e| e.summary.chat()).collect();
        // Only the first `limit + 1` matter; partial selection keeps big folders cheap.
        if chats.len() > limit + 1 {
            chats.select_nth_unstable_by(limit, |a, b| (b.updated_at, &b.id).cmp(&(a.updated_at, &a.id)));
            chats.truncate(limit + 1);
        }
        Ok(sorted_page(chats, limit))
    }

    fn pinned_chats(&self) -> Result<Vec<Chat>> {
        let inner = self.lock();
        let mut chats: Vec<Chat> = inner.entries.values().filter(|e| e.summary.pinned_at.is_some()).map(|e| e.summary.chat()).collect();
        chats.sort_by(|a, b| (b.pinned_at, &b.id).cmp(&(a.pinned_at, &a.id)));
        Ok(chats)
    }

    fn rename_chat(&self, id: &str, title: &str) -> Result<()> {
        let mut inner = self.lock();
        inner.state_or_404(id)?;
        inner.write(id, vec![Event::Renamed { title: title.to_owned() }], true, true)
    }

    fn set_chat_workspace(&self, id: &str, workspace: &Path) -> Result<()> {
        let mut inner = self.lock();
        inner.state_or_404(id)?;
        inner.write(id, vec![Event::WorkspaceSet { workspace: workspace.to_owned() }], true, true)
    }

    fn pin_chat(&self, id: &str, pinned: bool) -> Result<()> {
        let mut inner = self.lock();
        let current = inner.state_or_404(id)?.chat.pinned_at;
        let pinned_at = if pinned { current.or_else(|| Some(now())) } else { None };
        if pinned_at == current {
            return Ok(());
        }
        inner.write(id, vec![Event::Pinned { pinned_at }], true, true)
    }

    fn delete_chat(&self, id: &str) -> Result<Vec<String>> {
        let mut inner = self.lock();
        let tasks = inner.state_or_404(id)?.message_tasks();
        inner.write(id, vec![Event::Deleted {}], true, false)?;
        let Some(folder) = inner.entries.get(id).map(|e| e.folder.clone()) else { return Ok(tasks) };
        // Deleting is permanent: no trash. The `deleted` event above keeps it hidden if the
        // folder can't be removed (or a sync brings it back).
        let from = inner.chats_dir.join(&folder);
        if let Err(e) = std::fs::remove_dir_all(&from) {
            log::warn!("byob-store: couldn't delete {}: {e}", from.display());
        }
        inner.forget_folder(&folder);
        // Retire the slot: ids of the deleted chat's messages must not resolve again, even if
        // a chat with the same id is created later.
        if let Some(slot) = inner.slot_of.remove(id)
            && let Some(s) = inner.slots.get_mut(usize::try_from(slot).unwrap_or(usize::MAX))
        {
            s.clear();
        }
        Ok(tasks)
    }

    fn append_message(&self, chat_id: &str, m: &NewMessage) -> Result<MessageId> {
        let mut inner = self.lock();
        let last = inner.state_or_404(chat_id)?.last_m;
        let n = last.checked_add(1).ok_or_else(|| StoreError::BadValue { column: "messages", detail: "message number overflow".into() })?;
        let event =
            Event::MessageAdded { m: n, role: m.role, content: m.content.clone(), reasoning: m.reasoning.clone(), done: m.done, task: m.task_id.clone() };
        inner.write(chat_id, vec![event], m.done, m.done)?;
        let slot = inner.entries.get(chat_id).map(|e| e.slot).unwrap_or_default();
        Ok(message_id(slot, n))
    }

    fn append_message_text(&self, id: MessageId, content: &str, reasoning: &str) -> Result<()> {
        if content.is_empty() && reasoning.is_empty() {
            return Ok(());
        }
        let mut inner = self.lock();
        let (chat, _, m) = inner.target(id)?;
        inner.write(&chat, vec![Event::MessageDelta { m, content: content.to_owned(), reasoning: reasoning.to_owned() }], false, false)
    }

    fn update_message(&self, id: MessageId, content: &str, reasoning: &str, done: bool) -> Result<()> {
        let mut inner = self.lock();
        let (chat, _, m) = inner.target(id)?;
        inner.write(&chat, vec![Event::MessageUpdated { m, content: content.to_owned(), reasoning: reasoning.to_owned(), done }], done, done)
    }

    fn finish_message(&self, id: MessageId, error: Option<&str>, stats: Option<&MessageStats>) -> Result<()> {
        let mut inner = self.lock();
        let (chat, _, m) = inner.target(id)?;
        let (content_len, reasoning_len) =
            inner.state(&chat).and_then(|s| s.messages.get(&m)).map(|msg| (msg.content.len(), msg.reasoning.len())).unwrap_or_default();
        let event = Event::MessageFinished {
            m,
            error: error.map(str::to_owned),
            stats: stats.copied(),
            content_len: Some(content_len),
            reasoning_len: Some(reasoning_len),
        };
        inner.write(&chat, vec![event], true, true)
    }

    fn set_message_undone(&self, id: MessageId, undone: bool) -> Result<()> {
        let mut inner = self.lock();
        let (chat, _, m) = inner.target(id)?;
        inner.write(&chat, vec![Event::MessageUndone { m, undone }], true, true)
    }

    fn finish_interrupted(&self, error: &str) -> Result<usize> {
        let mut inner = self.lock();
        let open: Vec<String> = inner.entries.values().filter(|e| e.summary.open > 0).map(|e| e.summary.id.clone()).collect();
        let mut total = 0;
        for chat in open {
            if !inner.ensure_loaded(&chat)? {
                continue;
            }
            let ms: Vec<u32> = inner.state(&chat).map(|s| s.messages.iter().filter(|(_, m)| !m.done).map(|(n, _)| *n).collect()).unwrap_or_default();
            if ms.is_empty() {
                continue;
            }
            total += ms.len();
            let events = ms.into_iter().map(|m| Event::MessageInterrupted { m, error: error.to_owned() }).collect();
            inner.write(&chat, events, true, true)?;
        }
        Ok(total)
    }

    fn message(&self, id: MessageId) -> Result<Option<StoredMessage>> {
        let mut inner = self.lock();
        let Some((chat, slot, m)) = inner.resolve(id)? else { return Ok(None) };
        Ok(inner.state(&chat).and_then(|s| s.stored(slot, m)))
    }

    fn messages(&self, chat_id: &str) -> Result<Vec<StoredMessage>> {
        let mut inner = self.lock();
        if !inner.ensure_loaded(chat_id)? {
            return Ok(Vec::new());
        }
        let slot = inner.entries.get(chat_id).map(|e| e.slot).unwrap_or_default();
        Ok(inner.state(chat_id).map(|s| s.messages.keys().filter_map(|m| s.stored(slot, *m)).collect()).unwrap_or_default())
    }

    fn delete_messages_from(&self, chat_id: &str, from: MessageId) -> Result<usize> {
        let mut inner = self.lock();
        if !inner.ensure_loaded(chat_id)? {
            return Ok(0);
        }
        let slot = inner.entries.get(chat_id).map(|e| e.slot).unwrap_or_default();
        let doomed: Vec<u32> = inner.state(chat_id).map(|s| s.messages.keys().copied().filter(|m| message_id(slot, *m) >= from).collect()).unwrap_or_default();
        let Some(first) = doomed.first().copied() else { return Ok(0) };
        inner.write(chat_id, vec![Event::MessagesDeleted { from: first }], true, true)?;
        Ok(doomed.len())
    }

    fn add_step(&self, message_id: MessageId, step: &NewStep) -> Result<i64> {
        let mut inner = self.lock();
        let (chat, _, m) = inner.target(message_id)?;
        let seq = inner.state(&chat).and_then(|s| s.messages.get(&m)).and_then(|msg| msg.steps.iter().map(|s| s.seq).max()).unwrap_or(0).saturating_add(1);
        let event = Event::StepAdded { m, seq, tool: step.tool.clone(), title: step.title.clone(), access: step.access, args: step.args.clone() };
        inner.write(&chat, vec![event], false, false)?;
        Ok(seq)
    }

    fn finish_step(&self, message_id: MessageId, seq: i64, result: &StepResult) -> Result<()> {
        let mut inner = self.lock();
        let missing = || not_found(format!("step {}/{seq}", message_id.0));
        let (chat, _, m) = inner.resolve(message_id)?.ok_or_else(missing)?;
        let exists = inner.state(&chat).and_then(|s| s.messages.get(&m)).is_some_and(|msg| msg.steps.iter().any(|s| s.seq == seq));
        if !exists {
            return Err(missing());
        }
        let event = Event::StepFinished {
            m,
            seq,
            output: bound(&result.output, MAX_STEP_OUTPUT),
            failed: result.failed,
            changes: result.changes.clone(),
            card: result.card.clone(),
        };
        inner.write(&chat, vec![event], false, false)
    }

    fn steps(&self, message_id: MessageId) -> Result<Vec<Step>> {
        let mut inner = self.lock();
        let Some((chat, slot, m)) = inner.resolve(message_id)? else { return Ok(Vec::new()) };
        Ok(inner.state(&chat).map(|s| s.steps(slot, m)).unwrap_or_default())
    }

    fn delete_steps(&self, message_id: MessageId) -> Result<usize> {
        let mut inner = self.lock();
        let Some((chat, _, m)) = inner.resolve(message_id)? else { return Ok(0) };
        let n = inner.state(&chat).and_then(|s| s.messages.get(&m)).map_or(0, |msg| msg.steps.len());
        if n > 0 {
            inner.write(&chat, vec![Event::StepsDeleted { m }], false, false)?;
        }
        Ok(n)
    }

    fn add_compaction(&self, chat_id: &str, c: &NewCompaction) -> Result<Compaction> {
        let mut inner = self.lock();
        let (n, last_m) = {
            let s = inner.state_or_404(chat_id)?;
            (s.last_compaction.saturating_add(1), s.last_m)
        };
        let slot = inner.entries.get(chat_id).map(|e| e.slot).unwrap_or_default();
        // Stored by message number, which (unlike the session's message ids) survives a restart.
        let through = if c.through.0 == 0 {
            0
        } else {
            match split_id(c.through) {
                Some((s, m)) if s == slot && m <= last_m => m,
                _ => return Err(not_found(format!("message {} in chat {chat_id}", c.through.0))),
            }
        };
        let event = Event::CompactionAdded {
            n,
            through,
            summary: bound(&c.summary, MAX_SUMMARY),
            tokens_before: c.tokens_before,
            tokens_after: c.tokens_after,
            prompt_tokens: c.prompt_tokens,
            completion_tokens: c.completion_tokens,
            focus: c.focus.clone(),
            manual: c.manual,
        };
        inner.write(chat_id, vec![event], true, true)?;
        let state = inner.state(chat_id).ok_or_else(|| not_found(format!("chat {chat_id}")))?;
        let rec = state.compactions.iter().find(|r| r.n == n).ok_or_else(|| not_found(format!("compaction {n} of chat {chat_id}")))?;
        Ok(state.compaction(slot, rec))
    }

    fn compactions(&self, chat_id: &str) -> Result<Vec<Compaction>> {
        let mut inner = self.lock();
        if !inner.ensure_loaded(chat_id)? {
            return Ok(Vec::new());
        }
        let slot = inner.entries.get(chat_id).map(|e| e.slot).unwrap_or_default();
        Ok(inner.state(chat_id).map(|s| s.compactions.iter().map(|c| s.compaction(slot, c)).collect()).unwrap_or_default())
    }

    fn add_agent_run(&self, chat_id: &str, run: &NewAgentRun) -> Result<()> {
        if !agent_run_id_ok(&run.id) {
            return Err(crate::agent_runs::bad_run_id(&run.id));
        }
        let mut inner = self.lock();
        let (chat, _, m) = inner.resolve(run.parent)?.ok_or_else(|| msg_not_found(run.parent))?;
        if chat != chat_id {
            return Err(not_found(format!("message {} in chat {chat_id}", run.parent.0)));
        }
        let dir = inner.chat_dir(chat_id).ok_or_else(|| not_found(format!("chat {chat_id}")))?;
        let event = agents::RunEvent::Started {
            parent: m,
            step_seq: run.step_seq,
            agent: run.agent.clone(),
            task: bound(&run.task, crate::agent_runs::MAX_AGENT_TASK),
            model: run.model.clone(),
        };
        agents::create(&dir, &run.id, &agents::encode(now(), event)?)?;
        inner.runs.insert(run.id.clone(), (chat_id.to_owned(), 0));
        Ok(())
    }

    fn append_agent_text(&self, chat_id: &str, run_id: &str, content: &str, reasoning: &str) -> Result<()> {
        if content.is_empty() && reasoning.is_empty() {
            return Ok(());
        }
        let mut inner = self.lock();
        let dir = inner.run_chat_dir(chat_id, run_id)?;
        let event = agents::RunEvent::Delta { content: content.to_owned(), reasoning: reasoning.to_owned() };
        agents::append(&dir, run_id, &agents::encode(now(), event)?, false)
    }

    fn add_agent_step(&self, chat_id: &str, run_id: &str, step: &NewStep) -> Result<i64> {
        let mut inner = self.lock();
        let dir = inner.run_chat_dir(chat_id, run_id)?;
        let seq = inner.runs.get(run_id).map_or(0, |(_, s)| *s).saturating_add(1);
        agents::append(&dir, run_id, &agents::encode(now(), agents::step_added(seq, step))?, false)?;
        if let Some(r) = inner.runs.get_mut(run_id) {
            r.1 = seq;
        }
        Ok(seq)
    }

    fn finish_agent_step(&self, chat_id: &str, run_id: &str, seq: i64, result: &StepResult) -> Result<()> {
        let mut inner = self.lock();
        let dir = inner.run_chat_dir(chat_id, run_id)?;
        if seq < 1 || inner.runs.get(run_id).is_none_or(|(_, last)| seq > *last) {
            return Err(not_found(format!("agent step {run_id}/{seq}")));
        }
        let event =
            agents::RunEvent::StepFinished { seq, output: bound(&result.output, MAX_STEP_OUTPUT), failed: result.failed, changes: result.changes.clone() };
        agents::append(&dir, run_id, &agents::encode(now(), event)?, false)
    }

    fn finish_agent_run(&self, chat_id: &str, run_id: &str, error: Option<&str>, stats: Option<&MessageStats>) -> Result<()> {
        let mut inner = self.lock();
        let dir = inner.run_chat_dir(chat_id, run_id)?;
        let event = agents::RunEvent::Finished { error: error.map(str::to_owned), stats: stats.copied() };
        agents::append(&dir, run_id, &agents::encode(now(), event)?, true)
    }

    fn agent_runs(&self, chat_id: &str) -> Result<Vec<AgentRun>> {
        let mut inner = self.lock();
        if !inner.ensure_loaded(chat_id)? {
            return Ok(Vec::new());
        }
        let (Some(dir), Some(slot)) = (inner.chat_dir(chat_id), inner.entries.get(chat_id).map(|e| e.slot)) else { return Ok(Vec::new()) };
        let Some(state) = inner.state(chat_id) else { return Ok(Vec::new()) };
        let mut runs: Vec<AgentRun> = agents::list(&dir, chat_id)
            .into_iter()
            .filter(|r| state.messages.contains_key(&r.parent))
            .map(|mut r| {
                let parent = message_id(slot, r.parent);
                r.run.parent = parent;
                for s in &mut r.run.steps {
                    s.message_id = parent;
                }
                r.run
            })
            .collect();
        runs.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        Ok(runs)
    }

    fn grant(&self, scope: &GrantScope, tool: &str) -> Result<()> {
        let mut inner = self.lock();
        match scope {
            GrantScope::Global => {
                if inner.global.insert(tool.to_owned()) {
                    inner.save_grants()?;
                }
                Ok(())
            }
            GrantScope::Chat(id) => {
                if inner.state_or_404(id)?.grants.contains(tool) {
                    return Ok(());
                }
                inner.write(id, vec![Event::Grant { tool: tool.to_owned(), allowed: true }], true, false)
            }
        }
    }

    fn revoke(&self, scope: &GrantScope, tool: &str) -> Result<bool> {
        let mut inner = self.lock();
        match scope {
            GrantScope::Global => {
                let had = inner.global.remove(tool);
                if had {
                    inner.save_grants()?;
                }
                Ok(had)
            }
            GrantScope::Chat(id) => {
                if !inner.ensure_loaded(id)? || !inner.state(id).is_some_and(|s| s.grants.contains(tool)) {
                    return Ok(false);
                }
                inner.write(id, vec![Event::Grant { tool: tool.to_owned(), allowed: false }], true, false)?;
                Ok(true)
            }
        }
    }

    fn is_allowed(&self, chat_id: &str, tool: &str) -> Result<bool> {
        let mut inner = self.lock();
        if inner.global.contains(tool) {
            return Ok(true);
        }
        Ok(inner.ensure_loaded(chat_id)? && inner.state(chat_id).is_some_and(|s| s.grants.contains(tool)))
    }

    fn grants(&self, scope: &GrantScope) -> Result<Vec<String>> {
        let mut inner = self.lock();
        match scope {
            GrantScope::Global => Ok(inner.global.iter().cloned().collect()),
            GrantScope::Chat(id) => {
                if !inner.ensure_loaded(id)? {
                    return Ok(Vec::new());
                }
                Ok(inner.state(id).map(|s| s.grants.iter().cloned().collect()).unwrap_or_default())
            }
        }
    }

    fn add_undo_op(&self, task_id: &str, op: &UndoOp) -> Result<i64> {
        let mut inner = self.lock();
        let event = Event::UndoOp { task: task_id.to_owned(), op: op.clone() };
        let chat = inner.task_chat(task_id)?;
        match &chat {
            Some(chat) => inner.write(chat, vec![event], false, false)?,
            None => inner.append_orphan(event)?,
        }
        let in_chat = chat.as_deref().and_then(|c| inner.state(c)).map_or(0, |s| s.journal.len(task_id));
        Ok(i64::try_from(inner.orphans.len(task_id) + in_chat).unwrap_or(i64::MAX))
    }

    fn undo_ops(&self, task_id: &str) -> Result<Vec<UndoOp>> {
        let mut inner = self.lock();
        if inner.orphans.damaged.contains(task_id) {
            return Err(damaged(task_id));
        }
        let mut ops = inner.orphans.ops.get(task_id).cloned().unwrap_or_default();
        if let Some(chat) = inner.task_chat(task_id)?
            && let Some(s) = inner.state(&chat)
        {
            if s.journal.damaged.contains(task_id) {
                return Err(damaged(task_id));
            }
            ops.extend(s.journal.ops.get(task_id).into_iter().flatten().cloned());
        }
        Ok(ops)
    }

    fn delete_undo_ops(&self, task_id: &str) -> Result<usize> {
        let mut inner = self.lock();
        let clear = || Event::UndoCleared { task: task_id.to_owned() };
        let mut n = inner.orphans.len(task_id);
        if n > 0 || inner.orphans.damaged.contains(task_id) {
            inner.append_orphan(clear())?;
        }
        if let Some(chat) = inner.task_chat(task_id)? {
            let (k, bad) = inner.state(&chat).map_or((0, false), |s| (s.journal.len(task_id), s.journal.damaged.contains(task_id)));
            if k > 0 || bad {
                inner.write(&chat, vec![clear()], false, false)?;
            }
            n += k;
        }
        Ok(n)
    }

    fn import_chat(&self, e: &ChatExport) -> Result<()> {
        let lines = import_lines(e)?;
        let mut inner = self.lock();
        inner.create_from_lines(&e.chat.id, &e.chat.title, e.chat.created_at, &lines)?;
        let Some(dir) = inner.chat_dir(&e.chat.id) else { return Ok(()) };
        for x in &e.agent_runs {
            if !agent_run_id_ok(&x.run.id) {
                return Err(crate::agent_runs::bad_run_id(&x.run.id));
            }
            // Messages were numbered 1.. by `import_lines`.
            let Some(m) = x.parent.checked_add(1).and_then(|m| u32::try_from(m).ok()).filter(|_| x.parent < e.messages.len()) else { continue };
            agents::create(&dir, &x.run.id, &agents::import_text(&x.run, m)?)?;
        }
        Ok(())
    }
}
