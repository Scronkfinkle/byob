//! The undo journal: file-system changes a task made, in order, so Undo can reverse them.

use rusqlite::params;

use crate::error::{Result, StoreError};
use crate::model::UndoOp;
use crate::store::Store;

impl Store {
    /// Append one change to a task's journal. Returns its 1-based `seq`.
    pub fn add_undo_op(&self, task_id: &str, op: &UndoOp) -> Result<i64> {
        let json = serde_json::to_string(op)?;
        let tx = self.write_tx()?;
        let seq: i64 = tx.prepare_cached("SELECT coalesce(max(seq), 0) + 1 FROM undo_ops WHERE task_id = ?1")?.query_row([task_id], |r| r.get(0))?;
        tx.prepare_cached("INSERT INTO undo_ops (task_id, seq, op) VALUES (?1, ?2, ?3)")?.execute(params![task_id, seq, json])?;
        tx.commit()?;
        Ok(seq)
    }

    /// A task's changes, oldest first (undo walks them in reverse). A damaged entry is an
    /// error rather than skipped: undoing half a task silently would be worse.
    pub fn undo_ops(&self, task_id: &str) -> Result<Vec<UndoOp>> {
        let rows = self
            .conn
            .prepare_cached("SELECT seq, op FROM undo_ops WHERE task_id = ?1 ORDER BY seq")?
            .query_map([task_id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(seq, op)| {
                serde_json::from_str(&op).map_err(|e| StoreError::BadValue { column: "undo_ops.op", detail: format!("task {task_id} seq {seq}: {e}") })
            })
            .collect()
    }

    /// Forget a task's journal (after undoing it, or when its backups are pruned).
    pub fn delete_undo_ops(&self, task_id: &str) -> Result<usize> {
        Ok(self.conn.execute("DELETE FROM undo_ops WHERE task_id = ?1", [task_id])?)
    }
}
