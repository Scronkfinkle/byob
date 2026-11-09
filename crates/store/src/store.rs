//! The SQLite handle and chat-level operations.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params};

use crate::error::{Result, StoreError};
use crate::model::{Chat, ChatCursor, ChatPage, Millis};
use crate::schema;

/// How long a write waits for another connection's write lock before failing with `Busy`.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Largest page [`Store::list_chats`] returns.
pub const MAX_PAGE: usize = 500;

/// yah's database: chats, messages, tool steps, always-allow grants and the undo journal.
///
/// # Threads
///
/// `Store` is `Send` but not `Sync`: it wraps one `rusqlite::Connection`. The engine opens it
/// on (or moves it to) its state thread and calls it from there only; turn threads report
/// progress to the engine, which batches writes (e.g. streamed text every ~250 ms).
/// Other processes or a second `Store` on the same file are safe — SQLite's WAL mode lets
/// readers proceed during a write, and writers wait up to [`BUSY_TIMEOUT`] for each other.
pub struct Store {
    pub(crate) conn: Connection,
    path: PathBuf,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").field("path", &self.path).finish_non_exhaustive()
    }
}

pub(crate) fn now() -> Millis {
    chrono::Utc::now().timestamp_millis()
}

/// Paths are stored as text; non-UTF-8 bytes become U+FFFD.
pub(crate) fn path_text(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

impl Store {
    /// Open (creating if needed) the database at `path` and migrate it to the latest schema.
    ///
    /// A file that isn't a SQLite database yields [`StoreError::Corrupt`]; one written by a
    /// newer yah yields [`StoreError::TooNew`]. Neither modifies the file.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_owned();
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| StoreError::io(dir, e))?;
        }
        let conn = Connection::open(&path).map_err(|e| StoreError::classify(e, &path))?;
        Self::init(conn, path, true)
    }

    /// A private in-memory database (tests, `--ephemeral` runs).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::init(conn, PathBuf::from(":memory:"), false)
    }

    fn init(conn: Connection, path: PathBuf, wal: bool) -> Result<Self> {
        let classify = |e| StoreError::classify(e, &path);
        conn.busy_timeout(BUSY_TIMEOUT).map_err(classify)?;
        // The first statement that reads the header: "file is not a database" surfaces here.
        let version = schema::version(&conn).map_err(classify)?;
        if version > schema::LATEST {
            return Err(StoreError::TooNew { found: version, supported: schema::LATEST });
        }
        conn.pragma_update(None, "foreign_keys", true).map_err(classify)?;
        if wal {
            let mode: String = conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0)).map_err(classify)?;
            if !mode.eq_ignore_ascii_case("wal") {
                log::warn!("yah-store: {} uses journal mode {mode}, not WAL", path.display());
            }
            // Safe with WAL (a crash can lose the last commits, never corrupt) and much cheaper.
            conn.pragma_update(None, "synchronous", "NORMAL").map_err(classify)?;
        }
        schema::migrate_to(&conn, schema::LATEST).map_err(|e| match e {
            StoreError::Sqlite(e) => classify(e),
            e => e,
        })?;
        Ok(Self { conn, path })
    }

    /// Flush and close. Dropping a `Store` also closes it, ignoring errors.
    pub fn close(self) -> Result<()> {
        self.conn.close().map_err(|(_, e)| StoreError::Sqlite(e))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Schema version of the open database (always [`schema::LATEST`] after `open`).
    pub fn schema_version(&self) -> Result<i64> {
        Ok(schema::version(&self.conn)?)
    }

    /// A write transaction that takes the lock up front, so it waits (busy timeout) instead
    /// of failing when another connection is writing.
    pub(crate) fn write_tx(&self) -> Result<Transaction<'_>> {
        Ok(Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?)
    }

    // ---- chats ----------------------------------------------------------------------

    pub fn create_chat(&self, id: &str, title: &str, workspace: &Path) -> Result<Chat> {
        let now = now();
        self.conn.execute(
            "INSERT INTO chats (id, title, workspace, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4)",
            params![id, title, path_text(workspace), now],
        )?;
        Ok(Chat { id: id.to_owned(), title: title.to_owned(), workspace: workspace.to_owned(), created_at: now, updated_at: now, pinned_at: None })
    }

    pub fn chat(&self, id: &str) -> Result<Option<Chat>> {
        Ok(self.conn.prepare_cached(&format!("SELECT {CHAT_COLS} FROM chats WHERE id = ?1"))?.query_row([id], chat_row).optional()?)
    }

    /// Chats, most recently active first, `limit` (≤ [`MAX_PAGE`]) at a time. Pass the
    /// previous page's `next` as `after` to continue. Pinned chats are included in their
    /// place; use [`Store::pinned_chats`] to show them separately.
    pub fn list_chats(&self, limit: usize, after: Option<&ChatCursor>) -> Result<ChatPage> {
        let limit = limit.min(MAX_PAGE);
        if limit == 0 {
            return Ok(ChatPage { chats: Vec::new(), next: None });
        }
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {CHAT_COLS} FROM chats
             WHERE ?1 IS NULL OR updated_at < ?1 OR (updated_at = ?1 AND id < ?2)
             ORDER BY updated_at DESC, id DESC LIMIT ?3"
        ))?;
        let fetch = i64::try_from(limit).unwrap_or(i64::MAX).saturating_add(1);
        let mut chats =
            stmt.query_map(params![after.map(|c| c.updated_at), after.map(|c| c.id.as_str()), fetch], chat_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        let next = if chats.len() > limit {
            chats.truncate(limit);
            chats.last().map(Chat::cursor)
        } else {
            None
        };
        Ok(ChatPage { chats, next })
    }

    /// Pinned chats, most recently pinned first.
    pub fn pinned_chats(&self) -> Result<Vec<Chat>> {
        let mut stmt = self.conn.prepare_cached(&format!("SELECT {CHAT_COLS} FROM chats WHERE pinned_at IS NOT NULL ORDER BY pinned_at DESC, id DESC"))?;
        Ok(stmt.query_map([], chat_row)?.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn rename_chat(&self, id: &str, title: &str) -> Result<()> {
        let n = self.conn.execute("UPDATE chats SET title = ?2 WHERE id = ?1", params![id, title])?;
        found(n, || format!("chat {id}"))
    }

    pub fn set_chat_workspace(&self, id: &str, workspace: &Path) -> Result<()> {
        let n = self.conn.execute("UPDATE chats SET workspace = ?2 WHERE id = ?1", params![id, path_text(workspace)])?;
        found(n, || format!("chat {id}"))
    }

    pub fn pin_chat(&self, id: &str, pinned: bool) -> Result<()> {
        let n = self
            .conn
            .execute("UPDATE chats SET pinned_at = CASE WHEN ?2 THEN coalesce(pinned_at, ?3) ELSE NULL END WHERE id = ?1", params![id, pinned, now()])?;
        found(n, || format!("chat {id}"))
    }

    /// Delete a chat with its messages, steps, grants and undo journal entries. Returns the
    /// task ids whose journals were dropped, so the caller can remove their backup folders.
    pub fn delete_chat(&self, id: &str) -> Result<Vec<String>> {
        let tx = self.write_tx()?;
        let tasks = {
            let mut stmt = tx.prepare_cached("SELECT DISTINCT task_id FROM messages WHERE chat_id = ?1 AND task_id IS NOT NULL ORDER BY task_id")?;
            stmt.query_map([id], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for t in &tasks {
            tx.execute("DELETE FROM undo_ops WHERE task_id = ?1", [t])?;
        }
        let n = tx.execute("DELETE FROM chats WHERE id = ?1", [id])?;
        found(n, || format!("chat {id}"))?;
        tx.commit()?;
        Ok(tasks)
    }

    /// Mark a chat as just active (moves it to the top of the list).
    pub(crate) fn touch_chat(conn: &Connection, id: &str) -> Result<usize> {
        // +1 keeps the order stable when two writes land in the same millisecond.
        Ok(conn.prepare_cached("UPDATE chats SET updated_at = max(updated_at + 1, ?2) WHERE id = ?1")?.execute(params![id, now()])?)
    }
}

const CHAT_COLS: &str = "id, title, workspace, created_at, updated_at, pinned_at";

fn chat_row(r: &Row<'_>) -> rusqlite::Result<Chat> {
    Ok(Chat {
        id: r.get(0)?,
        title: r.get(1)?,
        workspace: PathBuf::from(r.get::<_, String>(2)?),
        created_at: r.get(3)?,
        updated_at: r.get(4)?,
        pinned_at: r.get(5)?,
    })
}

/// `NotFound` when an UPDATE/DELETE touched no rows.
pub(crate) fn found(changed: usize, what: impl FnOnce() -> String) -> Result<()> {
    if changed == 0 { Err(StoreError::NotFound(what())) } else { Ok(()) }
}
