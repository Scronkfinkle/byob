//! Subagent runs in the folder backend: one append-only file per run,
//! `chats/<chat>/agents/<run>/events.jsonl`, in the same line format as a chat's events.
//!
//! ```text
//! {"v":1,"ts":…,"e":"run_started","parent":3,"step_seq":1,"agent":"explorer","task":"…","model":"local"}
//! {"v":1,"ts":…,"e":"run_delta","content":"…"}
//! {"v":1,"ts":…,"e":"run_step_added","seq":1,"tool":"read_file",…}
//! {"v":1,"ts":…,"e":"run_step_finished","seq":1,"output":"…"}
//! {"v":1,"ts":…,"e":"run_finished","stats":{…}}
//! ```
//!
//! `parent` is the parent message's number `m` in the chat (stable across runs, unlike
//! message ids). A run whose parent message was deleted is not listed.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::event;
use crate::error::{Result, StoreError};
use crate::model::{AgentRun, MessageId, MessageStats, Millis, NewStep, Step, StepAccess, StepChange, StepResult};

pub(crate) const AGENTS_DIR: &str = "agents";
const EVENTS: &str = "events.jsonl";

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "e", rename_all = "snake_case")]
pub(crate) enum RunEvent {
    #[serde(rename = "run_started")]
    Started {
        parent: u32,
        step_seq: i64,
        agent: String,
        task: String,
        #[serde(default)]
        model: String,
    },
    #[serde(rename = "run_delta")]
    Delta {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        content: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        reasoning: String,
    },
    #[serde(rename = "run_step_added")]
    StepAdded { seq: i64, tool: String, title: String, access: StepAccess, args: Value },
    #[serde(rename = "run_step_finished")]
    StepFinished {
        seq: i64,
        output: String,
        #[serde(default, skip_serializing_if = "is_false")]
        failed: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        changes: Vec<StepChange>,
    },
    #[serde(rename = "run_finished")]
    Finished {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stats: Option<MessageStats>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct RunLine {
    v: u32,
    ts: Millis,
    #[serde(flatten)]
    event: RunEvent,
}

pub(crate) fn encode(ts: Millis, event: RunEvent) -> Result<String> {
    let mut s = serde_json::to_string(&RunLine { v: event::EVENT_VERSION, ts, event })?;
    s.push('\n');
    Ok(s)
}

/// A run as replayed from its file, by parent message number.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RunRec {
    pub parent: u32,
    pub run: AgentRun,
}

pub(crate) fn run_dir(chat_dir: &Path, run_id: &str) -> PathBuf {
    chat_dir.join(AGENTS_DIR).join(run_id)
}

pub(crate) fn run_file(chat_dir: &Path, run_id: &str) -> PathBuf {
    run_dir(chat_dir, run_id).join(EVENTS)
}

/// Replay one run's file. `None` if it has no `run_started` line (or can't be read).
pub(crate) fn replay(path: &Path, run_id: &str, chat_id: &str) -> Option<RunRec> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            log::warn!("byob-store: {}: {e}", path.display());
            return None;
        }
    };
    let mut rec: Option<RunRec> = None;
    let mut rest = bytes.as_slice();
    let mut n = 0usize;
    while !rest.is_empty() {
        n += 1;
        let Some(i) = rest.iter().position(|&b| b == b'\n') else {
            log::warn!("byob-store: {}: ignoring unfinished last line {n}", path.display());
            break;
        };
        let raw = rest.get(..i).unwrap_or_default();
        rest = rest.get(i.saturating_add(1)..).unwrap_or_default();
        let text = String::from_utf8_lossy(raw);
        if text.trim().is_empty() {
            continue;
        }
        let line = match serde_json::from_str::<RunLine>(text.trim()) {
            Ok(l) => l,
            Err(e) => {
                log::warn!("byob-store: {}: skipping line {n}: {e}", path.display());
                continue;
            }
        };
        match (line.event, rec.as_mut()) {
            (RunEvent::Started { parent, step_seq, agent, task, model }, None) => {
                rec = Some(RunRec {
                    parent,
                    run: AgentRun {
                        id: run_id.to_owned(),
                        chat_id: chat_id.to_owned(),
                        parent: MessageId(0),
                        step_seq,
                        agent,
                        task,
                        model,
                        content: String::new(),
                        reasoning: String::new(),
                        done: false,
                        error: None,
                        stats: None,
                        created_at: line.ts,
                        steps: Vec::new(),
                    },
                });
            }
            (_, None) | (RunEvent::Started { .. }, Some(_)) => log::warn!("byob-store: {}: line {n} is out of place", path.display()),
            (RunEvent::Delta { content, reasoning }, Some(r)) => {
                r.run.content.push_str(&content);
                r.run.reasoning.push_str(&reasoning);
            }
            (RunEvent::StepAdded { seq, tool, title, access, args }, Some(r)) => {
                r.run.steps.retain(|s| s.seq != seq);
                r.run.steps.push(Step { message_id: MessageId(0), seq, tool, title, access, args, result: None });
            }
            (RunEvent::StepFinished { seq, output, failed, changes }, Some(r)) => {
                if let Some(s) = r.run.steps.iter_mut().find(|s| s.seq == seq) {
                    s.result = Some(StepResult { output, failed, changes });
                }
            }
            (RunEvent::Finished { error, stats }, Some(r)) => {
                r.run.done = true;
                r.run.error = error;
                r.run.stats = stats;
            }
        }
    }
    if let Some(r) = rec.as_mut() {
        r.run.steps.sort_by_key(|s| s.seq);
    }
    rec
}

/// Every run under a chat folder (unsorted). A missing `agents/` folder is empty.
pub(crate) fn list(chat_dir: &Path, chat_id: &str) -> Vec<RunRec> {
    let Ok(rd) = std::fs::read_dir(chat_dir.join(AGENTS_DIR)) else { return Vec::new() };
    rd.flatten()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_owned();
            crate::model::agent_run_id_ok(&name).then_some(name)
        })
        .filter_map(|id| replay(&run_file(chat_dir, &id), &id, chat_id))
        .collect()
}

/// Create a run's file holding `text` (its `run_started` line first).
pub(crate) fn create(chat_dir: &Path, run_id: &str, text: &str) -> Result<()> {
    let dir = run_dir(chat_dir, run_id);
    std::fs::create_dir_all(&dir).map_err(|e| StoreError::io(&dir, e))?;
    match event::create(&dir.join(EVENTS), text) {
        Ok(_) => Ok(()),
        Err(StoreError::Io { source, .. }) if source.kind() == std::io::ErrorKind::AlreadyExists => Err(StoreError::Exists(format!("agent run {run_id}"))),
        Err(e) => Err(e),
    }
}

pub(crate) fn append(chat_dir: &Path, run_id: &str, text: &str, sync: bool) -> Result<()> {
    let path = run_file(chat_dir, run_id);
    if !path.is_file() {
        return Err(StoreError::NotFound(format!("agent run {run_id}")));
    }
    event::append(&path, text, sync).map(|_| ())
}

/// The lines that recreate an exported run under parent message `m`.
pub(crate) fn import_text(run: &AgentRun, m: u32) -> Result<String> {
    let ts = run.created_at;
    let mut out =
        encode(ts, RunEvent::Started { parent: m, step_seq: run.step_seq, agent: run.agent.clone(), task: run.task.clone(), model: run.model.clone() })?;
    if !run.content.is_empty() || !run.reasoning.is_empty() {
        out.push_str(&encode(ts, RunEvent::Delta { content: run.content.clone(), reasoning: run.reasoning.clone() })?);
    }
    for s in &run.steps {
        out.push_str(&encode(ts, RunEvent::StepAdded { seq: s.seq, tool: s.tool.clone(), title: s.title.clone(), access: s.access, args: s.args.clone() })?);
        if let Some(r) = &s.result {
            out.push_str(&encode(ts, RunEvent::StepFinished { seq: s.seq, output: r.output.clone(), failed: r.failed, changes: r.changes.clone() })?);
        }
    }
    if run.done {
        out.push_str(&encode(ts, RunEvent::Finished { error: run.error.clone(), stats: run.stats })?);
    }
    Ok(out)
}

/// A step to record, as an event.
pub(crate) fn step_added(seq: i64, step: &NewStep) -> RunEvent {
    RunEvent::StepAdded { seq, tool: step.tool.clone(), title: step.title.clone(), access: step.access, args: step.args.clone() }
}
