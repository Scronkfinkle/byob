//! Tool calls made while producing a message.

use rusqlite::{Row, params};
use serde_json::Value;

use crate::error::{Result, StoreError};
use crate::model::{MessageId, NewStep, Step, StepAccess, StepChange, StepResult};
use crate::store::{Store, found};

/// Tool output longer than this (bytes) is truncated when stored. The model saw the
/// (separately bounded) full text already; the transcript only needs enough to show.
pub const MAX_STEP_OUTPUT: usize = 64 * 1024;

const STEP_COLS: &str = "s.message_id, s.seq, s.tool, s.title, s.access, s.args, s.output, s.failed, s.changes";

/// Cut `s` to at most `max` bytes on a char boundary, noting how much was dropped.
pub(crate) fn bound(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let cut = s.get(..end).unwrap_or_default();
    format!("{cut}\n… [{} more bytes not saved]", s.len() - end)
}

struct Raw {
    message_id: i64,
    seq: i64,
    tool: String,
    title: String,
    access: String,
    args: String,
    output: Option<String>,
    failed: bool,
    changes: String,
}

fn raw_row(r: &Row<'_>) -> rusqlite::Result<Raw> {
    Ok(Raw {
        message_id: r.get(0)?,
        seq: r.get(1)?,
        tool: r.get(2)?,
        title: r.get(3)?,
        access: r.get(4)?,
        args: r.get(5)?,
        output: r.get(6)?,
        failed: r.get(7)?,
        changes: r.get(8)?,
    })
}

impl Raw {
    fn decode(self) -> Result<Step> {
        let access = StepAccess::parse(&self.access)
            .ok_or_else(|| StoreError::BadValue { column: "steps.access", detail: format!("unknown access {:?}", self.access) })?;
        // Args came from the model and may not have been JSON at all; keep the raw text.
        let args = serde_json::from_str(&self.args).unwrap_or(Value::String(self.args));
        let result = self.output.map(|output| {
            let changes = serde_json::from_str::<Vec<StepChange>>(&self.changes).unwrap_or_else(|e| {
                log::warn!("byob-store: step {}/{}: ignoring bad changes: {e}", self.message_id, self.seq);
                Vec::new()
            });
            StepResult { output, failed: self.failed, changes }
        });
        Ok(Step { message_id: MessageId(self.message_id), seq: self.seq, tool: self.tool, title: self.title, access, args, result })
    }
}

impl Store {
    /// Record a tool call before it runs. Returns its 1-based `seq` within the message.
    pub fn add_step(&self, message_id: MessageId, step: &NewStep) -> Result<i64> {
        let tx = self.write_tx()?;
        let exists: bool = tx.prepare_cached("SELECT EXISTS (SELECT 1 FROM messages WHERE id = ?1)")?.query_row([message_id.0], |r| r.get(0))?;
        found(usize::from(exists), || format!("message {}", message_id.0))?;
        let seq: i64 = tx.prepare_cached("SELECT coalesce(max(seq), 0) + 1 FROM steps WHERE message_id = ?1")?.query_row([message_id.0], |r| r.get(0))?;
        tx.prepare_cached("INSERT INTO steps (message_id, seq, tool, title, access, args) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?.execute(params![
            message_id.0,
            seq,
            step.tool,
            step.title,
            step.access.as_str(),
            serde_json::to_string(&step.args)?
        ])?;
        tx.commit()?;
        Ok(seq)
    }

    /// Record how a tool call ended. Output is bounded to [`MAX_STEP_OUTPUT`].
    pub fn finish_step(&self, message_id: MessageId, seq: i64, result: &StepResult) -> Result<()> {
        let n = self.conn.prepare_cached("UPDATE steps SET output = ?3, failed = ?4, changes = ?5 WHERE message_id = ?1 AND seq = ?2")?.execute(params![
            message_id.0,
            seq,
            bound(&result.output, MAX_STEP_OUTPUT),
            result.failed,
            serde_json::to_string(&result.changes)?
        ])?;
        found(n, || format!("step {}/{seq}", message_id.0))
    }

    /// A message's tool calls, by `seq`.
    pub fn steps(&self, message_id: MessageId) -> Result<Vec<Step>> {
        self.conn
            .prepare_cached(&format!("SELECT {STEP_COLS} FROM steps s WHERE s.message_id = ?1 ORDER BY s.seq"))?
            .query_map([message_id.0], raw_row)?
            .map(|r| r.map_err(StoreError::from).and_then(Raw::decode))
            .collect()
    }

    pub(crate) fn chat_steps(&self, chat_id: &str) -> Result<Vec<Step>> {
        self.conn
            .prepare_cached(&format!(
                "SELECT {STEP_COLS} FROM steps s JOIN messages m ON m.id = s.message_id WHERE m.chat_id = ?1 ORDER BY s.message_id, s.seq"
            ))?
            .query_map([chat_id], raw_row)?
            .map(|r| r.map_err(StoreError::from).and_then(Raw::decode))
            .collect()
    }

    /// Remove a message's steps; returns how many.
    pub fn delete_steps(&self, message_id: MessageId) -> Result<usize> {
        Ok(self.conn.execute("DELETE FROM steps WHERE message_id = ?1", [message_id.0])?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bound_respects_char_boundaries() {
        let s = "ééééé"; // 2 bytes each
        let b = bound(s, 5);
        assert!(b.starts_with("éé\n"), "{b}");
        assert!(b.contains("6 more bytes"), "{b}");
        assert_eq!(bound("short", 10), "short");
    }
}
