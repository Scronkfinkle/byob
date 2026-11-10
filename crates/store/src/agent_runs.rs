//! Subagent runs: the child threads `delegate` steps start, with their own streamed text
//! and tool steps, linked to the parent message and step (SQLite side).

use rusqlite::{OptionalExtension, params};

use crate::error::{Result, StoreError};
use crate::model::{AgentRun, MessageId, MessageStats, NewAgentRun, NewStep, Step, StepResult, agent_run_id_ok};
use crate::steps::{MAX_STEP_OUTPUT, Raw, bound, raw_row};
use crate::store::{Store, found, now};

/// Longest task text stored (the parent model wrote it).
pub const MAX_AGENT_TASK: usize = 64 * 1024;

pub(crate) fn bad_run_id(id: &str) -> StoreError {
    StoreError::BadValue { column: "agent_runs.id", detail: format!("unusable run id {id:?}") }
}

pub(crate) fn decode_stats(run: &str, s: Option<String>) -> Option<MessageStats> {
    s.and_then(|s| match serde_json::from_str::<MessageStats>(&s) {
        Ok(v) => Some(v),
        Err(e) => {
            log::warn!("yah-store: agent run {run}: ignoring bad stats {s:?}: {e}");
            None
        }
    })
}

impl Store {
    /// Record a subagent run of `chat_id` started by step `run.step_seq` of `run.parent`.
    pub fn add_agent_run(&self, chat_id: &str, run: &NewAgentRun) -> Result<()> {
        if !agent_run_id_ok(&run.id) {
            return Err(bad_run_id(&run.id));
        }
        let tx = self.write_tx()?;
        let parent_chat: Option<String> =
            tx.prepare_cached("SELECT chat_id FROM messages WHERE id = ?1")?.query_row([run.parent.0], |r| r.get(0)).optional()?;
        if parent_chat.as_deref() != Some(chat_id) {
            return Err(StoreError::NotFound(format!("message {} in chat {chat_id}", run.parent.0)));
        }
        tx.prepare_cached("INSERT INTO agent_runs (id, chat_id, parent_id, step_seq, agent, task, model, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)")?
            .execute(params![run.id, chat_id, run.parent.0, run.step_seq, run.agent, bound(&run.task, MAX_AGENT_TASK), run.model, now()])?;
        tx.commit()?;
        Ok(())
    }

    /// Append batched streamed text to a run.
    pub fn append_agent_text(&self, chat_id: &str, run_id: &str, content: &str, reasoning: &str) -> Result<()> {
        if content.is_empty() && reasoning.is_empty() {
            return Ok(());
        }
        let n = self
            .conn
            .prepare_cached("UPDATE agent_runs SET content = content || ?3, reasoning = reasoning || ?4 WHERE id = ?1 AND chat_id = ?2")?
            .execute(params![run_id, chat_id, content, reasoning])?;
        found(n, || format!("agent run {run_id}"))
    }

    /// Record a run's tool call before it runs; returns its 1-based `seq`.
    pub fn add_agent_step(&self, chat_id: &str, run_id: &str, step: &NewStep) -> Result<i64> {
        let tx = self.write_tx()?;
        let exists: bool =
            tx.prepare_cached("SELECT EXISTS (SELECT 1 FROM agent_runs WHERE id = ?1 AND chat_id = ?2)")?.query_row([run_id, chat_id], |r| r.get(0))?;
        found(usize::from(exists), || format!("agent run {run_id}"))?;
        let seq: i64 = tx.prepare_cached("SELECT coalesce(max(seq), 0) + 1 FROM agent_steps WHERE run_id = ?1")?.query_row([run_id], |r| r.get(0))?;
        tx.prepare_cached("INSERT INTO agent_steps (run_id, seq, tool, title, access, args) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?.execute(params![
            run_id,
            seq,
            step.tool,
            step.title,
            step.access.as_str(),
            serde_json::to_string(&step.args)?
        ])?;
        tx.commit()?;
        Ok(seq)
    }

    pub fn finish_agent_step(&self, chat_id: &str, run_id: &str, seq: i64, result: &StepResult) -> Result<()> {
        let n = self
            .conn
            .prepare_cached(
                "UPDATE agent_steps SET output = ?4, failed = ?5, changes = ?6
                 WHERE run_id = ?1 AND seq = ?3 AND EXISTS (SELECT 1 FROM agent_runs WHERE id = ?1 AND chat_id = ?2)",
            )?
            .execute(params![run_id, chat_id, seq, bound(&result.output, MAX_STEP_OUTPUT), result.failed, serde_json::to_string(&result.changes)?])?;
        found(n, || format!("agent step {run_id}/{seq}"))
    }

    /// Mark a run finished, with its error (if it failed) and stats.
    pub fn finish_agent_run(&self, chat_id: &str, run_id: &str, error: Option<&str>, stats: Option<&MessageStats>) -> Result<()> {
        let stats = stats.map(serde_json::to_string).transpose()?;
        let n = self
            .conn
            .prepare_cached("UPDATE agent_runs SET done = 1, error = ?3, stats = ?4 WHERE id = ?1 AND chat_id = ?2")?
            .execute(params![run_id, chat_id, error, stats])?;
        found(n, || format!("agent run {run_id}"))
    }

    /// A chat's subagent runs, oldest first, each with its steps.
    pub fn agent_runs(&self, chat_id: &str) -> Result<Vec<AgentRun>> {
        let mut runs = self
            .conn
            .prepare_cached(
                "SELECT id, chat_id, parent_id, step_seq, agent, task, model, content, reasoning, done, error, stats, created_at
                 FROM agent_runs WHERE chat_id = ?1 ORDER BY created_at, rowid",
            )?
            .query_map([chat_id], |r| {
                let id: String = r.get(0)?;
                let stats: Option<String> = r.get(11)?;
                Ok(AgentRun {
                    stats: decode_stats(&id, stats),
                    id,
                    chat_id: r.get(1)?,
                    parent: MessageId(r.get(2)?),
                    step_seq: r.get(3)?,
                    agent: r.get(4)?,
                    task: r.get(5)?,
                    model: r.get(6)?,
                    content: r.get(7)?,
                    reasoning: r.get(8)?,
                    done: r.get(9)?,
                    error: r.get(10)?,
                    created_at: r.get(12)?,
                    steps: Vec::new(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for run in &mut runs {
            run.steps = self.agent_steps(&run.id)?;
        }
        Ok(runs)
    }

    fn agent_steps(&self, run_id: &str) -> Result<Vec<Step>> {
        self.conn
            .prepare_cached(
                "SELECT r.parent_id, s.seq, s.tool, s.title, s.access, s.args, s.output, s.failed, s.changes
                 FROM agent_steps s JOIN agent_runs r ON r.id = s.run_id WHERE s.run_id = ?1 ORDER BY s.seq",
            )?
            .query_map([run_id], raw_row)?
            .map(|r| r.map_err(StoreError::from).and_then(Raw::decode))
            .collect()
    }
}
