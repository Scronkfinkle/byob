use std::path::Path;

use rusqlite::Connection;
use serde_json::json;

use super::TempDir;
use crate::schema;
use crate::*;

fn mem() -> Store {
    Store::open_in_memory().unwrap()
}

fn chat(s: &Store, id: &str) -> Chat {
    s.create_chat(id, "New chat", Path::new("/work")).unwrap()
}

#[test]
fn store_is_send() {
    fn send<T: Send>() {}
    send::<Store>();
}

#[test]
fn open_creates_wal_db_at_latest_version() {
    let dir = TempDir::new();
    let path = dir.path().join("nested/yah.db");
    let s = Store::open(&path).unwrap();
    assert!(path.exists());
    assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
    let mode: String = s.conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
    assert_eq!(mode, "wal");
    let fk: bool = s.conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0)).unwrap();
    assert!(fk);
    s.close().unwrap();
    // Reopening an up-to-date database is a no-op.
    let s = Store::open(&path).unwrap();
    assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
}

#[test]
fn corrupt_file_is_a_clear_error() {
    let dir = TempDir::new();
    let path = dir.path().join("yah.db");
    std::fs::write(&path, b"this is definitely not a sqlite database, just some text that is long enough").unwrap();
    let err = Store::open(&path).unwrap_err();
    assert!(matches!(err, StoreError::Corrupt { .. }), "{err:?}");
    assert!(err.is_corrupt());
    assert!(err.to_string().contains("corrupt"), "{err}");
}

#[test]
fn truncated_database_is_an_error_not_a_panic() {
    let dir = TempDir::new();
    let path = dir.path().join("yah.db");
    {
        let s = Store::open(&path).unwrap();
        chat(&s, "a");
        s.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
        s.close().unwrap();
    }
    let bytes = std::fs::read(&path).unwrap();
    // Keep the header but scramble every page after it.
    let mut broken = bytes.clone();
    for b in broken.iter_mut().skip(100) {
        *b = 0xA5;
    }
    std::fs::write(&path, &broken).unwrap();
    // Whatever SQLite makes of this, it must come back as an error (or, at worst, an
    // empty-looking database), never a panic.
    match Store::open(&path) {
        Ok(s) => {
            let _ = s.list_chats(10, None);
        }
        Err(e) => assert!(e.is_corrupt() || matches!(e, StoreError::Sqlite(_)), "{e:?}"),
    }
}

#[test]
fn newer_schema_is_refused_without_touching_it() {
    let dir = TempDir::new();
    let path = dir.path().join("yah.db");
    {
        let c = Connection::open(&path).unwrap();
        c.pragma_update(None, "user_version", 99).unwrap();
    }
    let err = Store::open(&path).unwrap_err();
    assert!(matches!(err, StoreError::TooNew { found: 99, .. }), "{err:?}");
    let c = Connection::open(&path).unwrap();
    assert_eq!(schema::version(&c).unwrap(), 99);
    let mode: String = c.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
    assert_eq!(mode, "delete", "must not switch a too-new db to WAL");
}

#[test]
fn migrates_v1_to_latest_keeping_data() {
    let dir = TempDir::new();
    let path = dir.path().join("yah.db");
    {
        let c = Connection::open(&path).unwrap();
        schema::migrate_to(&c, 1).unwrap();
        assert_eq!(schema::version(&c).unwrap(), 1);
        c.execute_batch(
            "INSERT INTO chats VALUES ('old', 'Old chat', '/w', 1, 2);
             INSERT INTO messages (chat_id, role, content, task_id, created_at) VALUES ('old', 'user', 'hi', NULL, 1);
             INSERT INTO messages (chat_id, role, content, task_id, created_at) VALUES ('old', 'assistant', 'hello', 't1', 2);
             INSERT INTO undo_ops VALUES ('t1', 1, '{\"type\":\"created\",\"path\":\"/w/a.txt\"}');",
        )
        .unwrap();
    }
    let s = Store::open(&path).unwrap();
    assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
    let c = s.chat("old").unwrap().unwrap();
    assert_eq!((c.title.as_str(), c.updated_at, c.pinned_at), ("Old chat", 2, None));
    let msgs = s.messages("old").unwrap();
    assert_eq!(msgs.len(), 2);
    assert!(msgs.iter().all(|m| m.done && m.reasoning.is_empty() && m.error.is_none() && m.stats.is_none() && !m.undone));
    assert_eq!(msgs[1].task_id.as_deref(), Some("t1"));
    assert_eq!(s.undo_ops("t1").unwrap(), vec![UndoOp::Created { path: "/w/a.txt".into() }]);
    // v2 features work on migrated rows.
    s.add_step(msgs[1].id, &NewStep { tool: "read".into(), title: "Read".into(), access: StepAccess::Read, args: json!({}) }).unwrap();
    s.grant(&GrantScope::Chat("old".into()), "read").unwrap();
    s.pin_chat("old", true).unwrap();
}

#[test]
fn damaged_json_columns_degrade_gracefully() {
    let s = mem();
    chat(&s, "c");
    let a = s.append_message("c", &NewMessage::assistant_streaming(None)).unwrap();
    s.add_step(a, &NewStep { tool: "t".into(), title: "t".into(), access: StepAccess::Read, args: json!(null) }).unwrap();
    s.finish_step(a, 1, &StepResult::default()).unwrap();
    s.conn.execute("UPDATE messages SET stats = '{not json' WHERE id = ?1", [a.0]).unwrap();
    s.conn.execute("UPDATE steps SET changes = 'garbage', args = 'raw text' WHERE message_id = ?1", [a.0]).unwrap();
    let m = s.message(a).unwrap().unwrap();
    assert_eq!(m.stats, None);
    assert_eq!(m.steps[0].args, json!("raw text"));
    assert!(m.steps[0].result.as_ref().unwrap().changes.is_empty());

    // Unknown enum text is an error, not a guess.
    s.conn.execute("UPDATE steps SET access = 'teleport' WHERE message_id = ?1", [a.0]).unwrap();
    assert!(matches!(s.steps(a), Err(StoreError::BadValue { column: "steps.access", .. })));
    s.conn.execute("UPDATE messages SET role = 'wizard' WHERE id = ?1", [a.0]).unwrap();
    assert!(matches!(s.messages("c"), Err(StoreError::BadValue { column: "messages.role", .. })));
}

#[test]
fn undo_journal() {
    let s = mem();
    let ops = vec![
        UndoOp::Created { path: "/w/new.txt".into() },
        UndoOp::Replaced { path: "/w/a.txt".into(), backup: "/undo/t/1-a.txt".into() },
        UndoOp::Moved { from: "/w/b".into(), to: "/w/c".into() },
        UndoOp::Deleted { path: "/w/huge.bin".into(), backup: None },
    ];
    for (i, op) in ops.iter().enumerate() {
        assert_eq!(s.add_undo_op("t", op).unwrap(), i as i64 + 1);
    }
    s.add_undo_op("other", &ops[0]).unwrap();
    assert_eq!(s.undo_ops("t").unwrap(), ops);
    assert!(s.undo_ops("missing").unwrap().is_empty());

    s.conn.execute("UPDATE undo_ops SET op = '{\"type\":\"exploded\"}' WHERE task_id = 'other'", []).unwrap();
    assert!(matches!(s.undo_ops("other"), Err(StoreError::BadValue { column: "undo_ops.op", .. })));

    assert_eq!(s.delete_undo_ops("t").unwrap(), 4);
    assert!(s.undo_ops("t").unwrap().is_empty());
}

#[test]
fn two_connections_see_each_other_and_both_write() {
    let dir = TempDir::new();
    let path = dir.path().join("yah.db");
    let a = Store::open(&path).unwrap();
    let b = Store::open(&path).unwrap();
    chat(&a, "from-a");
    assert!(b.chat("from-a").unwrap().is_some(), "b sees a's committed write");

    chat(&a, "x");
    chat(&a, "y");
    let threads: Vec<_> = [(a, "x"), (b, "y")]
        .into_iter()
        .map(|(s, c)| {
            std::thread::spawn(move || {
                for i in 0..100 {
                    let id = s.append_message(c, &NewMessage::assistant_streaming(Some(format!("{c}-{i}")))).unwrap();
                    s.append_message_text(id, "tok", "").unwrap();
                    s.add_step(id, &NewStep { tool: "t".into(), title: "t".into(), access: StepAccess::Read, args: json!({}) }).unwrap();
                    s.add_undo_op(&format!("{c}-{i}"), &UndoOp::Created { path: "/p".into() }).unwrap();
                    s.finish_message(id, None, None).unwrap();
                }
                s
            })
        })
        .collect();
    let stores: Vec<Store> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    for s in &stores {
        assert_eq!(s.messages("x").unwrap().len(), 100);
        assert_eq!(s.messages("y").unwrap().len(), 100);
    }
}
