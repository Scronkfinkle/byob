//! Schema versions, tracked in `PRAGMA user_version`.
//!
//! Each entry in [`MIGRATIONS`] takes the schema from version `i` to `i + 1`. Never edit a
//! released migration; append a new one. Opening a database newer than [`LATEST`] fails
//! with [`StoreError::TooNew`] rather than risk writing rows an old build can't represent.

use rusqlite::{Connection, Transaction, TransactionBehavior};

use crate::error::{Result, StoreError};

/// v1: chats, messages, undo journal.
const V1: &str = "
CREATE TABLE chats (
    id         TEXT PRIMARY KEY,
    title      TEXT NOT NULL,
    workspace  TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX chats_recent ON chats (updated_at DESC, id DESC);
CREATE TABLE messages (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    chat_id    TEXT NOT NULL REFERENCES chats (id) ON DELETE CASCADE,
    role       TEXT NOT NULL,
    content    TEXT NOT NULL DEFAULT '',
    task_id    TEXT,
    created_at INTEGER NOT NULL
);
CREATE INDEX messages_chat ON messages (chat_id, id);
CREATE TABLE undo_ops (
    task_id TEXT NOT NULL,
    seq     INTEGER NOT NULL,
    op      TEXT NOT NULL,
    PRIMARY KEY (task_id, seq)
);
";

/// v2: pinning, streamed reasoning/errors/stats, tool steps, always-allow grants.
const V2: &str = "
ALTER TABLE chats ADD COLUMN pinned_at INTEGER;
ALTER TABLE messages ADD COLUMN reasoning TEXT NOT NULL DEFAULT '';
ALTER TABLE messages ADD COLUMN done INTEGER NOT NULL DEFAULT 1;
ALTER TABLE messages ADD COLUMN error TEXT;
ALTER TABLE messages ADD COLUMN stats TEXT;
ALTER TABLE messages ADD COLUMN undone INTEGER NOT NULL DEFAULT 0;
CREATE INDEX chats_pinned ON chats (pinned_at) WHERE pinned_at IS NOT NULL;
CREATE TABLE steps (
    message_id INTEGER NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    seq        INTEGER NOT NULL,
    tool       TEXT NOT NULL,
    title      TEXT NOT NULL,
    access     TEXT NOT NULL,
    args       TEXT NOT NULL,
    output     TEXT,
    failed     INTEGER NOT NULL DEFAULT 0,
    changes    TEXT NOT NULL DEFAULT '[]',
    PRIMARY KEY (message_id, seq)
);
CREATE TABLE grants (
    chat_id    TEXT REFERENCES chats (id) ON DELETE CASCADE,
    tool       TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX grants_unique ON grants (ifnull(chat_id, ''), tool);
";

/// v3: context compactions (a summary that stands in for the messages up to `through_id`).
const V3: &str = "
CREATE TABLE compactions (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    chat_id           TEXT NOT NULL REFERENCES chats (id) ON DELETE CASCADE,
    through_id        INTEGER NOT NULL,
    summary           TEXT NOT NULL,
    tokens_before     INTEGER NOT NULL DEFAULT 0,
    tokens_after      INTEGER NOT NULL DEFAULT 0,
    prompt_tokens     INTEGER NOT NULL DEFAULT 0,
    completion_tokens INTEGER NOT NULL DEFAULT 0,
    focus             TEXT,
    manual            INTEGER NOT NULL DEFAULT 0,
    created_at        INTEGER NOT NULL
);
CREATE INDEX compactions_chat ON compactions (chat_id, id);
";

pub(crate) const MIGRATIONS: &[&str] = &[V1, V2, V3];

/// Schema version this build reads and writes.
pub const LATEST: i64 = MIGRATIONS.len() as i64;

pub(crate) fn version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("PRAGMA user_version", [], |r| r.get(0))
}

/// Bring the schema up to `target` (≤ [`LATEST`]) in one transaction.
pub(crate) fn migrate_to(conn: &Connection, target: i64) -> Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let found = version(&tx)?;
    if found > LATEST {
        return Err(StoreError::TooNew { found, supported: LATEST });
    }
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        let to = i64::try_from(i).unwrap_or(i64::MAX).saturating_add(1);
        if to <= found || to > target {
            continue;
        }
        log::info!("yah-store: migrating schema v{} -> v{to}", to - 1);
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", to)?;
    }
    tx.commit()?;
    Ok(())
}
