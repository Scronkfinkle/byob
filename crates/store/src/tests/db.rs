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
fn chats_crud() {
    let s = mem();
    let c = s.create_chat("c1", "Hello", Path::new("/home/me/proj")).unwrap();
    assert_eq!(s.chat("c1").unwrap().unwrap(), c);
    assert!(s.chat("nope").unwrap().is_none());
    assert!(s.create_chat("c1", "dup", Path::new("/")).is_err());

    s.rename_chat("c1", "Renamed").unwrap();
    s.set_chat_workspace("c1", Path::new("/elsewhere")).unwrap();
    let c = s.chat("c1").unwrap().unwrap();
    assert_eq!(c.title, "Renamed");
    assert_eq!(c.workspace, Path::new("/elsewhere"));

    assert!(matches!(s.rename_chat("nope", "x"), Err(StoreError::NotFound(_))));
    assert!(matches!(s.pin_chat("nope", true), Err(StoreError::NotFound(_))));
    assert!(matches!(s.delete_chat("nope"), Err(StoreError::NotFound(_))));
}

#[test]
fn pinning() {
    let s = mem();
    chat(&s, "a");
    chat(&s, "b");
    s.pin_chat("a", true).unwrap();
    let first = s.chat("a").unwrap().unwrap().pinned_at.unwrap();
    s.pin_chat("a", true).unwrap();
    assert_eq!(s.chat("a").unwrap().unwrap().pinned_at, Some(first), "re-pinning keeps the original time");
    assert_eq!(s.pinned_chats().unwrap().iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["a"]);
    s.pin_chat("a", false).unwrap();
    assert!(!s.chat("a").unwrap().unwrap().pinned());
    assert!(s.pinned_chats().unwrap().is_empty());
}

#[test]
fn list_is_recent_first_and_paginates() {
    let s = mem();
    for i in 0..25 {
        chat(&s, &format!("c{i:02}"));
    }
    // Activity moves a chat to the top.
    s.append_message("c03", &NewMessage::user("bump")).unwrap();

    let mut seen = Vec::new();
    let mut after = None;
    let mut pages = 0;
    loop {
        let page = s.list_chats(10, after.as_ref()).unwrap();
        pages += 1;
        assert!(page.chats.len() <= 10);
        seen.extend(page.chats.iter().map(|c| c.id.clone()));
        match page.next {
            Some(n) => after = Some(n),
            None => break,
        }
    }
    assert_eq!(pages, 3);
    assert_eq!(seen.len(), 25);
    assert_eq!(seen[0], "c03");
    let mut dedup = seen.clone();
    dedup.sort();
    dedup.dedup();
    assert_eq!(dedup.len(), 25, "no chat appears twice");
    let all = s.list_chats(100, None).unwrap();
    assert!(all.next.is_none());
    assert!(all.chats.windows(2).all(|w| (w[0].updated_at, &w[0].id) >= (w[1].updated_at, &w[1].id)));
    assert!(s.list_chats(0, None).unwrap().chats.is_empty());
}

#[test]
fn exact_page_size_has_no_next() {
    let s = mem();
    for i in 0..10 {
        chat(&s, &format!("c{i}"));
    }
    let page = s.list_chats(10, None).unwrap();
    assert_eq!(page.chats.len(), 10);
    assert!(page.next.is_none());
}

#[test]
fn streaming_reply_lifecycle() {
    let s = mem();
    chat(&s, "c");
    let u = s.append_message("c", &NewMessage::user("question")).unwrap();
    let a = s.append_message("c", &NewMessage::assistant_streaming(Some("task-1".into()))).unwrap();
    assert!(a > u);
    s.append_message_text(a, "", "thinking ").unwrap();
    s.append_message_text(a, "Hel", "more").unwrap();
    s.append_message_text(a, "lo", "").unwrap();
    s.append_message_text(a, "", "").unwrap();
    let m = s.message(a).unwrap().unwrap();
    assert_eq!((m.content.as_str(), m.reasoning.as_str(), m.done), ("Hello", "thinking more", false));

    let stats = MessageStats { prompt_tokens: 10, completion_tokens: 3, elapsed_ms: 420 };
    s.finish_message(a, None, Some(&stats)).unwrap();
    let m = s.message(a).unwrap().unwrap();
    assert!(m.done);
    assert_eq!(m.stats, Some(stats));
    assert_eq!(m.task_id.as_deref(), Some("task-1"));
    assert_eq!(m.role, Role::Assistant);

    s.update_message(a, "Replaced", "", true).unwrap();
    assert_eq!(s.message(a).unwrap().unwrap().content, "Replaced");

    s.set_message_undone(a, true).unwrap();
    assert!(s.message(a).unwrap().unwrap().undone);

    assert!(matches!(s.append_message_text(MessageId(999), "x", ""), Err(StoreError::NotFound(_))));
    assert!(matches!(s.finish_message(MessageId(999), None, None), Err(StoreError::NotFound(_))));
    assert!(matches!(s.append_message("nope", &NewMessage::user("x")), Err(StoreError::NotFound(_))));
}

#[test]
fn failed_reply_and_interrupted_recovery() {
    let s = mem();
    chat(&s, "c");
    let a = s.append_message("c", &NewMessage::assistant_streaming(None)).unwrap();
    s.finish_message(a, Some("connection reset"), None).unwrap();
    let b = s.append_message("c", &NewMessage::assistant_streaming(None)).unwrap();
    assert_eq!(s.finish_interrupted("yah stopped").unwrap(), 1);
    let ms = s.messages("c").unwrap();
    assert_eq!(ms[0].error.as_deref(), Some("connection reset"));
    assert_eq!(ms[1].id, b);
    assert!(ms[1].done);
    assert_eq!(ms[1].error.as_deref(), Some("yah stopped"));
    assert_eq!(s.finish_interrupted("again").unwrap(), 0);
}

#[test]
fn delete_messages_from_truncates_chat() {
    let s = mem();
    chat(&s, "c");
    chat(&s, "other");
    let ids: Vec<_> = (0..5).map(|i| s.append_message("c", &NewMessage::user(format!("m{i}"))).unwrap()).collect();
    let keep = s.append_message("other", &NewMessage::user("x")).unwrap();
    assert_eq!(s.delete_messages_from("c", ids[2]).unwrap(), 3);
    assert_eq!(s.messages("c").unwrap().iter().map(|m| m.content.as_str()).collect::<Vec<_>>(), ["m0", "m1"]);
    assert!(s.message(keep).unwrap().is_some());
}

#[test]
fn steps_crud_and_bounds() {
    let s = mem();
    chat(&s, "c");
    let a = s.append_message("c", &NewMessage::assistant_streaming(None)).unwrap();
    let new = |tool: &str| NewStep { tool: tool.into(), title: format!("Run {tool}"), access: StepAccess::Write, args: json!({"path": "ä.txt", "n": 1}) };
    assert_eq!(s.add_step(a, &new("write")).unwrap(), 1);
    assert_eq!(s.add_step(a, &new("move")).unwrap(), 2);

    let steps = s.steps(a).unwrap();
    assert_eq!(steps.len(), 2);
    assert!(steps.iter().all(|st| st.result.is_none()));
    assert_eq!(steps[0].args, json!({"path": "ä.txt", "n": 1}));

    let big = "x".repeat(MAX_STEP_OUTPUT * 2);
    let changes = vec![StepChange::Created { path: "/w/a".into() }, StepChange::Moved { from: "/w/b".into(), to: "/w/c".into() }];
    s.finish_step(a, 1, &StepResult { output: big, failed: false, changes: changes.clone() }).unwrap();
    s.finish_step(a, 2, &StepResult { output: "no such file".into(), failed: true, changes: vec![] }).unwrap();
    assert!(matches!(s.finish_step(a, 3, &StepResult::default()), Err(StoreError::NotFound(_))));

    let steps = s.steps(a).unwrap();
    let r1 = steps[0].result.as_ref().unwrap();
    assert!(r1.output.len() < MAX_STEP_OUTPUT + 100);
    assert!(r1.output.contains("more bytes not saved"));
    assert_eq!(r1.changes, changes);
    assert!(steps[1].result.as_ref().unwrap().failed);

    // list_messages carries the steps.
    let ms = s.messages("c").unwrap();
    assert_eq!(ms[0].steps, steps);

    assert!(matches!(s.add_step(MessageId(999), &new("x")), Err(StoreError::NotFound(_))));
    assert_eq!(s.delete_steps(a).unwrap(), 2);
    assert!(s.steps(a).unwrap().is_empty());
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
fn grants() {
    let s = mem();
    chat(&s, "a");
    chat(&s, "b");
    let a = GrantScope::Chat("a".into());
    s.grant(&a, "write_file").unwrap();
    s.grant(&a, "write_file").unwrap();
    s.grant(&GrantScope::Global, "read_file").unwrap();
    s.grant(&GrantScope::Global, "read_file").unwrap();
    assert_eq!(s.grants(&a).unwrap(), ["write_file"]);
    assert_eq!(s.grants(&GrantScope::Global).unwrap(), ["read_file"]);
    assert!(s.is_allowed("a", "write_file").unwrap());
    assert!(!s.is_allowed("b", "write_file").unwrap());
    assert!(s.is_allowed("b", "read_file").unwrap());
    assert!(!s.is_allowed("b", "run").unwrap());

    assert!(s.revoke(&a, "write_file").unwrap());
    assert!(!s.revoke(&a, "write_file").unwrap());
    assert!(!s.is_allowed("a", "write_file").unwrap());
    assert!(matches!(s.grant(&GrantScope::Chat("nope".into()), "x"), Err(StoreError::NotFound(_))));

    s.grant(&GrantScope::Chat("b".into()), "run").unwrap();
    s.delete_chat("b").unwrap();
    assert!(s.grants(&GrantScope::Chat("b".into())).unwrap().is_empty());
    assert_eq!(s.grants(&GrantScope::Global).unwrap(), ["read_file"], "global grants survive chat deletion");
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
fn delete_chat_cascades_and_returns_tasks() {
    let s = mem();
    chat(&s, "c");
    chat(&s, "keep");
    let a = s.append_message("c", &NewMessage::assistant_streaming(Some("t1".into()))).unwrap();
    s.append_message("c", &NewMessage::assistant_streaming(Some("t2".into()))).unwrap();
    s.append_message("c", &NewMessage::user("no task")).unwrap();
    let k = s.append_message("keep", &NewMessage::assistant_streaming(Some("t3".into()))).unwrap();
    s.add_step(a, &NewStep { tool: "t".into(), title: "t".into(), access: StepAccess::Read, args: json!({}) }).unwrap();
    s.add_step(k, &NewStep { tool: "t".into(), title: "t".into(), access: StepAccess::Read, args: json!({}) }).unwrap();
    for t in ["t1", "t2", "t3"] {
        s.add_undo_op(t, &UndoOp::Created { path: "/x".into() }).unwrap();
    }

    assert_eq!(s.delete_chat("c").unwrap(), ["t1", "t2"]);
    assert!(s.chat("c").unwrap().is_none());
    assert!(s.messages("c").unwrap().is_empty());
    assert!(s.steps(a).unwrap().is_empty());
    assert!(s.undo_ops("t1").unwrap().is_empty());
    assert_eq!(s.undo_ops("t3").unwrap().len(), 1);
    assert_eq!(s.steps(k).unwrap().len(), 1);
}

#[test]
fn unicode_round_trips() {
    let s = mem();
    let title = "Résumé 📄 — 日本語 עברית \u{200d}👩‍💻";
    let ws = Path::new("/home/zoë/プロジェクト");
    s.create_chat("ü-ñ-😀", title, ws).unwrap();
    let id = s.append_message("ü-ñ-😀", &NewMessage::user("Ünïcödé ∑ 𝔘 \u{FEFF} tab\tnewline\n")).unwrap();
    s.append_message_text(id, "🎉", "💭").unwrap();
    let c = s.chat("ü-ñ-😀").unwrap().unwrap();
    assert_eq!(c.title, title);
    assert_eq!(c.workspace, ws);
    let m = s.message(id).unwrap().unwrap();
    assert_eq!(m.content, "Ünïcödé ∑ 𝔘 \u{FEFF} tab\tnewline\n🎉");
    assert_eq!(m.reasoning, "💭");
}

#[test]
fn embedded_nul_survives() {
    let s = mem();
    chat(&s, "c");
    let id = s.append_message("c", &NewMessage::user("a\0b")).unwrap();
    s.append_message_text(id, "\0c", "").unwrap();
    assert_eq!(s.message(id).unwrap().unwrap().content, "a\0b\0c");
}

#[test]
fn large_streamed_content() {
    let dir = TempDir::new();
    let s = Store::open(dir.path().join("yah.db")).unwrap();
    chat(&s, "c");
    let id = s.append_message("c", &NewMessage::assistant_streaming(None)).unwrap();
    let chunk = "lorem ipsum ünïcode 🦀 ".repeat(1000); // ~25 KB per flush
    for _ in 0..200 {
        s.append_message_text(id, &chunk, "").unwrap();
    }
    s.finish_message(id, None, None).unwrap();
    let m = s.message(id).unwrap().unwrap();
    assert_eq!(m.content.len(), chunk.len() * 200);
    assert!(m.content.starts_with("lorem") && m.content.ends_with("🦀 "));
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
