//! A chat's state, rebuilt by replaying its events. Live writes go through the same
//! [`ChatState::apply`], so what's in memory is always what a replay would produce.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::event::{Bad, Event, Line, Parsed};
use crate::model::{Chat, MessageId, MessageStats, Millis, Role, Step, StepAccess, StepResult, StoredMessage, UndoOp};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StepRec {
    pub seq: i64,
    pub tool: String,
    pub title: String,
    pub access: StepAccess,
    pub args: Value,
    pub result: Option<StepResult>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Msg {
    pub role: Role,
    pub content: String,
    pub reasoning: String,
    pub done: bool,
    pub error: Option<String>,
    pub stats: Option<MessageStats>,
    pub task: Option<String>,
    pub undone: bool,
    pub created_at: Millis,
    pub steps: Vec<StepRec>,
}

/// Undo journals by task, plus tasks with a damaged entry.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Journal {
    pub ops: HashMap<String, Vec<UndoOp>>,
    pub damaged: HashSet<String>,
}

impl Journal {
    pub(crate) fn apply(&mut self, event: &Event) {
        match event {
            Event::UndoOp { task, op } => self.ops.entry(task.clone()).or_default().push(op.clone()),
            Event::UndoCleared { task } => {
                self.ops.remove(task);
                self.damaged.remove(task);
            }
            _ => {}
        }
    }

    pub(crate) fn note_bad(&mut self, bad: &[Bad]) {
        for b in bad {
            if let Bad::UndoOp { task } = b {
                self.damaged.insert(task.clone());
            }
        }
    }

    pub(crate) fn len(&self, task: &str) -> usize {
        self.ops.get(task).map_or(0, Vec::len)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ChatState {
    pub chat: Chat,
    pub deleted: bool,
    pub messages: BTreeMap<u32, Msg>,
    /// Highest message number ever used (numbers are never reused).
    pub last_m: u32,
    pub grants: BTreeSet<String>,
    pub journal: Journal,
    /// Every task any message (even a deleted one) carried, for routing undo entries.
    pub tasks: BTreeSet<String>,
}

/// The listing-relevant part of a chat, cached in the folder index.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Summary {
    pub id: String,
    pub title: String,
    pub workspace: PathBuf,
    pub created_at: Millis,
    pub updated_at: Millis,
    #[serde(default)]
    pub pinned_at: Option<Millis>,
    #[serde(default)]
    pub tasks: Vec<String>,
    /// Replies still marked streaming.
    #[serde(default)]
    pub open: u32,
}

impl Summary {
    pub(crate) fn chat(&self) -> Chat {
        Chat {
            id: self.id.clone(),
            title: self.title.clone(),
            workspace: self.workspace.clone(),
            created_at: self.created_at,
            updated_at: self.updated_at,
            pinned_at: self.pinned_at,
        }
    }
}

/// Encode a message number in a chat's session slot as a store-wide id. Ids are only
/// valid for the lifetime of a `FolderStore`; within a chat they increase with `m`.
pub(crate) fn message_id(slot: u32, m: u32) -> MessageId {
    MessageId((i64::from(slot) + 1) << 32 | i64::from(m))
}

/// Inverse of [`message_id`]: `(slot, m)`.
pub(crate) fn split_id(id: MessageId) -> Option<(u32, u32)> {
    let hi = id.0 >> 32;
    let slot = u32::try_from(hi.checked_sub(1)?).ok()?;
    let m = u32::try_from(id.0 & 0xffff_ffff).ok()?;
    (m > 0).then_some((slot, m))
}

impl ChatState {
    pub(crate) fn new(id: &str, title: &str, workspace: &Path, ts: Millis) -> Self {
        Self {
            chat: Chat { id: id.to_owned(), title: title.to_owned(), workspace: workspace.to_owned(), created_at: ts, updated_at: ts, pinned_at: None },
            deleted: false,
            messages: BTreeMap::new(),
            last_m: 0,
            grants: BTreeSet::new(),
            journal: Journal::default(),
            tasks: BTreeSet::new(),
        }
    }

    /// Rebuild from a parsed file. `None` if it has no `chat_created` event. Problems with
    /// individual events are appended to `warnings`.
    pub(crate) fn replay(parsed: &Parsed, warnings: &mut Vec<String>) -> Option<Self> {
        let mut lines = parsed.lines.iter();
        let mut state = loop {
            let l = lines.next()?;
            match &l.event {
                Event::ChatCreated { id, title, workspace } => break Self::new(id, title, workspace, l.ts),
                other => warnings.push(format!("ignoring {} before chat_created", kind(other))),
            }
        };
        for l in lines {
            if let Err(w) = state.apply(l) {
                warnings.push(format!("chat {}: {w}", state.chat.id));
            }
        }
        state.journal.note_bad(&parsed.bad);
        Some(state)
    }

    fn touch(&mut self, ts: Millis) {
        // Same rule as the SQLite store: +1 keeps the order stable within a millisecond.
        self.chat.updated_at = self.chat.updated_at.saturating_add(1).max(ts);
    }

    fn msg(&mut self, m: u32) -> Result<&mut Msg, String> {
        self.messages.get_mut(&m).ok_or_else(|| format!("no message {m}"))
    }

    fn step(&mut self, m: u32, seq: i64) -> Result<&mut StepRec, String> {
        self.msg(m)?.steps.iter_mut().find(|s| s.seq == seq).ok_or_else(|| format!("no step {m}/{seq}"))
    }

    /// Apply one event. An event that refers to something missing (state unchanged) or
    /// doesn't add up (applied anyway) is an `Err` the caller warns about.
    pub(crate) fn apply(&mut self, l: &Line) -> Result<(), String> {
        match &l.event {
            Event::ChatCreated { .. } => return Err("duplicate chat_created".into()),
            Event::Renamed { title } => self.chat.title.clone_from(title),
            Event::WorkspaceSet { workspace } => self.chat.workspace.clone_from(workspace),
            Event::Pinned { pinned_at } => self.chat.pinned_at = *pinned_at,
            Event::Touched { updated_at } => self.chat.updated_at = *updated_at,
            Event::MessageAdded { m, role, content, reasoning, done, task } => {
                let msg = Msg {
                    role: *role,
                    content: content.clone(),
                    reasoning: reasoning.clone(),
                    done: *done,
                    error: None,
                    stats: None,
                    task: task.clone(),
                    undone: false,
                    created_at: l.ts,
                    steps: Vec::new(),
                };
                self.add_message(l.ts, *m, msg);
            }
            Event::MessageDelta { m, content, reasoning } => {
                let msg = self.msg(*m)?;
                msg.content.push_str(content);
                msg.reasoning.push_str(reasoning);
            }
            Event::MessageUpdated { m, content, reasoning, done } => {
                let msg = self.msg(*m)?;
                msg.content.clone_from(content);
                msg.reasoning.clone_from(reasoning);
                msg.done = *done;
            }
            Event::MessageFinished { m, error, stats, content_len, reasoning_len } => {
                self.finish(*m, error, stats, *content_len, *reasoning_len)?;
                self.touch(l.ts);
            }
            Event::MessageInterrupted { m, error } => {
                let msg = self.msg(*m)?;
                msg.done = true;
                msg.error.get_or_insert_with(|| error.clone());
            }
            Event::MessageUndone { m, undone } => self.msg(*m)?.undone = *undone,
            Event::MessagesDeleted { from } => self.messages.retain(|n, _| n < from),
            Event::StepAdded { m, seq, tool, title, access, args } => {
                let rec = StepRec { seq: *seq, tool: tool.clone(), title: title.clone(), access: *access, args: args.clone(), result: None };
                let msg = self.msg(*m)?;
                if msg.steps.iter().any(|s| s.seq == *seq) {
                    return Err(format!("duplicate step {m}/{seq}"));
                }
                msg.steps.push(rec);
            }
            Event::StepFinished { m, seq, output, failed, changes } => {
                self.step(*m, *seq)?.result = Some(StepResult { output: output.clone(), failed: *failed, changes: changes.clone() });
            }
            Event::StepsDeleted { m } => self.msg(*m)?.steps.clear(),
            Event::Grant { tool, allowed: true } => {
                self.grants.insert(tool.clone());
            }
            Event::Grant { tool, allowed: false } => {
                self.grants.remove(tool);
            }
            e @ (Event::UndoOp { .. } | Event::UndoCleared { .. }) => self.journal.apply(e),
            Event::Deleted {} => self.deleted = true,
        }
        Ok(())
    }

    fn add_message(&mut self, ts: Millis, m: u32, msg: Msg) {
        if let Some(t) = &msg.task {
            self.tasks.insert(t.clone());
        }
        self.last_m = self.last_m.max(m);
        self.messages.insert(m, msg);
        self.touch(ts);
    }

    fn finish(
        &mut self,
        m: u32,
        error: &Option<String>,
        stats: &Option<MessageStats>,
        content_len: Option<usize>,
        reasoning_len: Option<usize>,
    ) -> Result<(), String> {
        let msg = self.msg(m)?;
        msg.done = true;
        msg.error.clone_from(error);
        msg.stats = *stats;
        let short = |want: Option<usize>, got: usize| want.is_some_and(|w| w != got);
        if short(content_len, msg.content.len()) || short(reasoning_len, msg.reasoning.len()) {
            return Err(format!(
                "message {m}: text length {}/{} differs from the recorded {content_len:?}/{reasoning_len:?} (lost stream deltas?)",
                msg.content.len(),
                msg.reasoning.len()
            ));
        }
        Ok(())
    }

    pub(crate) fn summary(&self) -> Summary {
        let c = &self.chat;
        Summary {
            id: c.id.clone(),
            title: c.title.clone(),
            workspace: c.workspace.clone(),
            created_at: c.created_at,
            updated_at: c.updated_at,
            pinned_at: c.pinned_at,
            tasks: self.tasks.iter().cloned().collect(),
            open: u32::try_from(self.messages.values().filter(|m| !m.done).count()).unwrap_or(u32::MAX),
        }
    }

    pub(crate) fn steps(&self, slot: u32, m: u32) -> Vec<Step> {
        let Some(msg) = self.messages.get(&m) else { return Vec::new() };
        let mut steps: Vec<Step> = msg
            .steps
            .iter()
            .map(|s| Step {
                message_id: message_id(slot, m),
                seq: s.seq,
                tool: s.tool.clone(),
                title: s.title.clone(),
                access: s.access,
                args: s.args.clone(),
                result: s.result.clone(),
            })
            .collect();
        steps.sort_by_key(|s| s.seq);
        steps
    }

    pub(crate) fn stored(&self, slot: u32, m: u32) -> Option<StoredMessage> {
        let msg = self.messages.get(&m)?;
        Some(StoredMessage {
            id: message_id(slot, m),
            chat_id: self.chat.id.clone(),
            role: msg.role,
            content: msg.content.clone(),
            reasoning: msg.reasoning.clone(),
            done: msg.done,
            error: msg.error.clone(),
            stats: msg.stats,
            task_id: msg.task.clone(),
            undone: msg.undone,
            created_at: msg.created_at,
            steps: self.steps(slot, m),
        })
    }

    /// Task ids of the current messages, sorted and distinct.
    pub(crate) fn message_tasks(&self) -> Vec<String> {
        let set: BTreeSet<&String> = self.messages.values().filter_map(|m| m.task.as_ref()).collect();
        set.into_iter().cloned().collect()
    }
}

/// Event name, for messages.
pub(crate) fn kind(e: &Event) -> String {
    serde_json::to_value(e).ok().and_then(|v| v.get("e").and_then(Value::as_str).map(str::to_owned)).unwrap_or_default()
}
