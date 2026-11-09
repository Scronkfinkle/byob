//! Messages, including cheap incremental updates while a reply streams.

use std::collections::HashMap;

use rusqlite::{OptionalExtension, Row, params};

use crate::error::{Result, StoreError};
use crate::model::{MessageId, MessageStats, NewMessage, Role, StoredMessage};
use crate::store::{Store, found, now};

const MSG_COLS: &str = "id, chat_id, role, content, reasoning, done, error, stats, task_id, undone, created_at";

pub(crate) fn role_str(r: Role) -> &'static str {
    match r {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn parse_role(s: &str) -> Result<Role> {
    Ok(match s {
        "system" => Role::System,
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        other => return Err(StoreError::BadValue { column: "messages.role", detail: format!("unknown role {other:?}") }),
    })
}

/// Columns as stored; decoded outside the rusqlite row callback so errors stay typed.
struct Raw {
    id: i64,
    chat_id: String,
    role: String,
    content: String,
    reasoning: String,
    done: bool,
    error: Option<String>,
    stats: Option<String>,
    task_id: Option<String>,
    undone: bool,
    created_at: i64,
}

fn raw_row(r: &Row<'_>) -> rusqlite::Result<Raw> {
    Ok(Raw {
        id: r.get(0)?,
        chat_id: r.get(1)?,
        role: r.get(2)?,
        content: r.get(3)?,
        reasoning: r.get(4)?,
        done: r.get(5)?,
        error: r.get(6)?,
        stats: r.get(7)?,
        task_id: r.get(8)?,
        undone: r.get(9)?,
        created_at: r.get(10)?,
    })
}

impl Raw {
    fn decode(self) -> Result<StoredMessage> {
        // Stats are cosmetic: a damaged value is dropped, not fatal for the whole chat.
        let stats = self.stats.and_then(|s| match serde_json::from_str::<MessageStats>(&s) {
            Ok(v) => Some(v),
            Err(e) => {
                log::warn!("yah-store: message {}: ignoring bad stats {s:?}: {e}", self.id);
                None
            }
        });
        Ok(StoredMessage {
            id: MessageId(self.id),
            chat_id: self.chat_id,
            role: parse_role(&self.role)?,
            content: self.content,
            reasoning: self.reasoning,
            done: self.done,
            error: self.error,
            stats,
            task_id: self.task_id,
            undone: self.undone,
            created_at: self.created_at,
            steps: Vec::new(),
        })
    }
}

impl Store {
    /// Add a message to the end of a chat and mark the chat active.
    pub fn append_message(&self, chat_id: &str, m: &NewMessage) -> Result<MessageId> {
        let tx = self.write_tx()?;
        found(Store::touch_chat(&tx, chat_id)?, || format!("chat {chat_id}"))?;
        tx.prepare_cached("INSERT INTO messages (chat_id, role, content, reasoning, done, task_id, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)")?
            .execute(params![chat_id, role_str(m.role), m.content, m.reasoning, m.done, m.task_id, now()])?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(MessageId(id))
    }

    /// Append streamed text to a message. Meant to be called with batched deltas (every
    /// ~250 ms), not per token: each call is one small UPDATE + WAL commit.
    pub fn append_message_text(&self, id: MessageId, content: &str, reasoning: &str) -> Result<()> {
        if content.is_empty() && reasoning.is_empty() {
            return Ok(());
        }
        let n = self
            .conn
            .prepare_cached("UPDATE messages SET content = content || ?2, reasoning = reasoning || ?3 WHERE id = ?1")?
            .execute(params![id.0, content, reasoning])?;
        found(n, || format!("message {}", id.0))
    }

    /// Replace a message's text and done flag (e.g. snapshotting a streamed reply, or
    /// editing a user message).
    pub fn update_message(&self, id: MessageId, content: &str, reasoning: &str, done: bool) -> Result<()> {
        let n = self
            .conn
            .prepare_cached("UPDATE messages SET content = ?2, reasoning = ?3, done = ?4 WHERE id = ?1")?
            .execute(params![id.0, content, reasoning, done])?;
        found(n, || format!("message {}", id.0))
    }

    /// Mark a reply finished, with its error (if it failed) and stats, and its chat active.
    pub fn finish_message(&self, id: MessageId, error: Option<&str>, stats: Option<&MessageStats>) -> Result<()> {
        let stats = stats.map(serde_json::to_string).transpose()?;
        let tx = self.write_tx()?;
        let chat_id: Option<String> = tx
            .prepare_cached("UPDATE messages SET done = 1, error = ?2, stats = ?3 WHERE id = ?1 RETURNING chat_id")?
            .query_row(params![id.0, error, stats], |r| r.get(0))
            .optional()?;
        let chat_id = chat_id.ok_or_else(|| StoreError::NotFound(format!("message {}", id.0)))?;
        Store::touch_chat(&tx, &chat_id)?;
        tx.commit()?;
        Ok(())
    }

    pub fn set_message_undone(&self, id: MessageId, undone: bool) -> Result<()> {
        let n = self.conn.execute("UPDATE messages SET undone = ?2 WHERE id = ?1", params![id.0, undone])?;
        found(n, || format!("message {}", id.0))
    }

    /// After a crash or kill: close every reply still marked streaming, with `error`.
    /// Returns how many there were. Call once at startup.
    pub fn finish_interrupted(&self, error: &str) -> Result<usize> {
        Ok(self.conn.execute("UPDATE messages SET done = 1, error = coalesce(error, ?1) WHERE done = 0", [error])?)
    }

    /// One message with its steps.
    pub fn message(&self, id: MessageId) -> Result<Option<StoredMessage>> {
        let raw = self.conn.prepare_cached(&format!("SELECT {MSG_COLS} FROM messages WHERE id = ?1"))?.query_row([id.0], raw_row).optional()?;
        let Some(raw) = raw else { return Ok(None) };
        let mut m = raw.decode()?;
        m.steps = self.steps(id)?;
        Ok(Some(m))
    }

    /// A chat's messages in order, each with its steps.
    pub fn messages(&self, chat_id: &str) -> Result<Vec<StoredMessage>> {
        let raws = self
            .conn
            .prepare_cached(&format!("SELECT {MSG_COLS} FROM messages WHERE chat_id = ?1 ORDER BY id"))?
            .query_map([chat_id], raw_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut out = raws.into_iter().map(Raw::decode).collect::<Result<Vec<_>>>()?;
        let mut steps = HashMap::new();
        for s in self.chat_steps(chat_id)? {
            steps.entry(s.message_id).or_insert_with(Vec::new).push(s);
        }
        for m in &mut out {
            if let Some(s) = steps.remove(&m.id) {
                m.steps = s;
            }
        }
        Ok(out)
    }

    /// Delete `from` and every later message of its chat (editing a sent message, or
    /// retrying a reply). Returns how many were removed. Undo journals are kept.
    pub fn delete_messages_from(&self, chat_id: &str, from: MessageId) -> Result<usize> {
        Ok(self.conn.execute("DELETE FROM messages WHERE chat_id = ?1 AND id >= ?2", params![chat_id, from.0])?)
    }
}
