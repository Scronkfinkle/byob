//! Recreating a whole chat with its original timestamps (see [`crate::export`]).

use rusqlite::params;

use crate::backend::ChatExport;
use crate::compactions::{MAX_SUMMARY, int};
use crate::error::Result;
use crate::messages::role_str;
use crate::steps::{MAX_STEP_OUTPUT, bound};
use crate::store::{Store, now, path_text};

impl Store {
    /// Insert a chat exactly as exported, in one transaction. Fails (constraint error) if
    /// the id is taken.
    pub fn import_chat(&self, e: &ChatExport) -> Result<()> {
        let tx = self.write_tx()?;
        let c = &e.chat;
        tx.execute(
            "INSERT INTO chats (id, title, workspace, created_at, updated_at, pinned_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![c.id, c.title, path_text(&c.workspace), c.created_at, c.updated_at, c.pinned_at],
        )?;
        let mut ids = Vec::with_capacity(e.messages.len());
        for m in &e.messages {
            let stats = m.stats.as_ref().map(serde_json::to_string).transpose()?;
            tx.prepare_cached(
                "INSERT INTO messages (chat_id, role, content, reasoning, done, error, stats, task_id, undone, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )?
            .execute(params![c.id, role_str(m.role), m.content, m.reasoning, m.done, m.error, stats, m.task_id, m.undone, m.created_at])?;
            let id = tx.last_insert_rowid();
            ids.push(id);
            for s in &m.steps {
                let (output, failed, changes) = match &s.result {
                    Some(r) => (Some(bound(&r.output, MAX_STEP_OUTPUT)), r.failed, serde_json::to_string(&r.changes)?),
                    None => (None, false, "[]".to_owned()),
                };
                tx.prepare_cached(
                    "INSERT INTO steps (message_id, seq, tool, title, access, args, output, failed, changes) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                )?
                .execute(params![id, s.seq, s.tool, s.title, s.access.as_str(), serde_json::to_string(&s.args)?, output, failed, changes])?;
            }
        }
        for tool in &e.grants {
            tx.prepare_cached("INSERT OR IGNORE INTO grants (chat_id, tool, created_at) VALUES (?1, ?2, ?3)")?.execute(params![c.id, tool, now()])?;
        }
        for (task, ops) in &e.undo {
            for op in ops {
                let json = serde_json::to_string(op)?;
                tx.prepare_cached(
                    "INSERT INTO undo_ops (task_id, seq, op) VALUES (?1, (SELECT coalesce(max(seq), 0) + 1 FROM undo_ops WHERE task_id = ?1), ?2)",
                )?
                .execute(params![task, json])?;
            }
        }
        for x in &e.compactions {
            let k = &x.compaction;
            // Covering none of the messages is `0`, as for a live compaction.
            let through = x.covers.checked_sub(1).and_then(|i| ids.get(i)).copied().unwrap_or(0);
            tx.prepare_cached(
                "INSERT INTO compactions (chat_id, through_id, summary, tokens_before, tokens_after, prompt_tokens, completion_tokens, focus, manual, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )?
            .execute(params![
                c.id,
                through,
                bound(&k.summary, MAX_SUMMARY),
                int(k.tokens_before),
                int(k.tokens_after),
                int(k.prompt_tokens),
                int(k.completion_tokens),
                k.focus,
                k.manual,
                k.created_at
            ])?;
        }
        tx.commit()?;
        Ok(())
    }
}
