//! Context compactions: model-written summaries that stand in for a chat's older messages
//! when the conversation outgrows the context window. The transcript keeps every message;
//! the engine builds its next request from the latest compaction plus later messages.

use rusqlite::{Row, params};

use crate::error::Result;
use crate::model::{Compaction, MessageId, NewCompaction};
use crate::store::{Store, found, now};

/// Longest summary stored, in bytes (a summary is meant to be a few thousand tokens).
pub const MAX_SUMMARY: usize = 256 * 1024;

const COLS: &str = "id, chat_id, through_id, summary, tokens_before, tokens_after, prompt_tokens, completion_tokens, focus, manual, created_at";

fn row(r: &Row<'_>) -> rusqlite::Result<Compaction> {
    let n = |i: usize| -> rusqlite::Result<u64> { Ok(u64::try_from(r.get::<_, i64>(i)?).unwrap_or(0)) };
    Ok(Compaction {
        id: r.get(0)?,
        chat_id: r.get(1)?,
        through: MessageId(r.get(2)?),
        summary: r.get(3)?,
        tokens_before: n(4)?,
        tokens_after: n(5)?,
        prompt_tokens: n(6)?,
        completion_tokens: n(7)?,
        focus: r.get(8)?,
        manual: r.get(9)?,
        created_at: r.get(10)?,
    })
}

pub(crate) fn int(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

impl Store {
    /// Record a compaction of `chat_id`. The summary is cut to [`MAX_SUMMARY`] bytes.
    pub fn add_compaction(&self, chat_id: &str, c: &NewCompaction) -> Result<Compaction> {
        let tx = self.write_tx()?;
        let exists: bool = tx.prepare_cached("SELECT EXISTS (SELECT 1 FROM chats WHERE id = ?1)")?.query_row([chat_id], |r| r.get(0))?;
        found(usize::from(exists), || format!("chat {chat_id}"))?;
        let summary = crate::steps::bound(&c.summary, MAX_SUMMARY);
        let created_at = now();
        tx.prepare_cached(
            "INSERT INTO compactions (chat_id, through_id, summary, tokens_before, tokens_after, prompt_tokens, completion_tokens, focus, manual, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )?
        .execute(params![
            chat_id,
            c.through.0,
            summary,
            int(c.tokens_before),
            int(c.tokens_after),
            int(c.prompt_tokens),
            int(c.completion_tokens),
            c.focus,
            c.manual,
            created_at
        ])?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(Compaction {
            id,
            chat_id: chat_id.to_owned(),
            through: c.through,
            summary,
            tokens_before: c.tokens_before,
            tokens_after: c.tokens_after,
            prompt_tokens: c.prompt_tokens,
            completion_tokens: c.completion_tokens,
            focus: c.focus.clone(),
            manual: c.manual,
            created_at,
        })
    }

    /// A chat's compactions, oldest first. The last one is what the next request builds on.
    pub fn compactions(&self, chat_id: &str) -> Result<Vec<Compaction>> {
        Ok(self
            .conn
            .prepare_cached(&format!("SELECT {COLS} FROM compactions WHERE chat_id = ?1 ORDER BY id"))?
            .query_map([chat_id], row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }
}
