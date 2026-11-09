//! "Always allow" decisions from the permission prompt, per chat or global.

use rusqlite::params;

use crate::error::Result;
use crate::model::GrantScope;
use crate::store::{Store, found, now};

fn scope_id(scope: &GrantScope) -> Option<&str> {
    match scope {
        GrantScope::Global => None,
        GrantScope::Chat(id) => Some(id.as_str()),
    }
}

impl Store {
    /// Always allow `tool` in `scope`. Granting twice is a no-op.
    pub fn grant(&self, scope: &GrantScope, tool: &str) -> Result<()> {
        let tx = self.write_tx()?;
        if let GrantScope::Chat(id) = scope {
            let exists: bool = tx.prepare_cached("SELECT EXISTS (SELECT 1 FROM chats WHERE id = ?1)")?.query_row([id], |r| r.get(0))?;
            found(usize::from(exists), || format!("chat {id}"))?;
        }
        tx.prepare_cached("INSERT OR IGNORE INTO grants (chat_id, tool, created_at) VALUES (?1, ?2, ?3)")?.execute(params![scope_id(scope), tool, now()])?;
        tx.commit()?;
        Ok(())
    }

    /// Withdraw a grant. Returns whether there was one.
    pub fn revoke(&self, scope: &GrantScope, tool: &str) -> Result<bool> {
        let n = self.conn.execute("DELETE FROM grants WHERE chat_id IS ?1 AND tool = ?2", params![scope_id(scope), tool])?;
        Ok(n > 0)
    }

    /// Is `tool` always allowed in this chat, either for the chat or globally?
    pub fn is_allowed(&self, chat_id: &str, tool: &str) -> Result<bool> {
        Ok(self
            .conn
            .prepare_cached("SELECT EXISTS (SELECT 1 FROM grants WHERE tool = ?2 AND (chat_id IS NULL OR chat_id = ?1))")?
            .query_row(params![chat_id, tool], |r| r.get(0))?)
    }

    /// Tools granted in exactly this scope (global grants are not included for a chat).
    pub fn grants(&self, scope: &GrantScope) -> Result<Vec<String>> {
        Ok(self
            .conn
            .prepare_cached("SELECT tool FROM grants WHERE chat_id IS ?1 ORDER BY tool")?
            .query_map([scope_id(scope)], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }
}
