//! The store contract, run against every backend (SQLite and folder).

use std::path::Path;

use serde_json::json;

use super::{folder, sqlite};
use crate::*;

fn chat(s: &dyn ChatStore, id: &str) -> Chat {
    s.create_chat(id, "New chat", Path::new("/work")).unwrap()
}

macro_rules! for_each_backend {
    ($($name:ident),* $(,)?) => {
        mod on_sqlite {
            $( #[test] fn $name() { super::$name(&*super::sqlite()) } )*
        }
        mod on_folder {
            $( #[test] fn $name() { super::$name(&*super::folder()) } )*
        }
    };
}

for_each_backend!(
    chats_crud,
    pinning,
    list_is_recent_first_and_paginates,
    exact_page_size_has_no_next,
    streaming_reply_lifecycle,
    failed_reply_and_interrupted_recovery,
    delete_messages_from_truncates_chat,
    steps_crud_and_bounds,
    grants,
    undo_journal,
    delete_chat_cascades_and_returns_tasks,
    unicode_round_trips,
    embedded_nul_survives,
    large_streamed_content,
    import_keeps_everything,
    ids_of_a_deleted_chat_stay_dead,
);

fn ids_of_a_deleted_chat_stay_dead(s: &dyn ChatStore) {
    chat(s, "c");
    let old = s.append_message("c", &NewMessage::user("old")).unwrap();
    s.delete_chat("c").unwrap();
    chat(s, "c");
    let new = s.append_message("c", &NewMessage::user("new")).unwrap();
    assert_ne!(old, new);
    assert!(s.message(old).unwrap().is_none());
    assert!(matches!(s.append_message_text(old, "x", ""), Err(StoreError::NotFound(_))));
    assert_eq!(s.message(new).unwrap().unwrap().content, "new");
}

#[test]
fn boxed_store_is_send() {
    fn send<T: Send>(_: &T) {}
    send(&sqlite().store);
    send(&folder().store);
}

#[test]
fn everything_survives_reopen() {
    for f in [sqlite(), folder()] {
        let s = &*f;
        let c = chat(s, "c");
        let u = s.append_message("c", &NewMessage::user("hi")).unwrap();
        let a = s.append_message("c", &NewMessage::assistant_streaming(Some("t1".into()))).unwrap();
        s.append_message_text(a, "Hel", "hmm").unwrap();
        s.append_message_text(a, "lo", "").unwrap();
        s.add_step(a, &NewStep { tool: "write_file".into(), title: "Write a.txt".into(), access: StepAccess::Write, args: json!({"path": "a.txt"}) }).unwrap();
        s.finish_step(a, 1, &StepResult { output: "ok".into(), failed: false, changes: vec![StepChange::Created { path: "/w/a.txt".into() }] }).unwrap();
        s.add_undo_op("t1", &UndoOp::Created { path: "/w/a.txt".into() }).unwrap();
        s.finish_message(a, None, Some(&MessageStats { prompt_tokens: 1, completion_tokens: 2, elapsed_ms: 3, model_calls: 1 })).unwrap();
        s.set_message_undone(u, true).unwrap();
        s.grant(&GrantScope::Chat("c".into()), "write_file").unwrap();
        s.grant(&GrantScope::Global, "read_file").unwrap();
        s.pin_chat("c", true).unwrap();
        s.rename_chat("c", "Renamed").unwrap();
        let before = s.export_chat("c").unwrap().unwrap();
        assert_eq!(before.chat.created_at, c.created_at);
        let f = f.reopen();
        let s = &*f;
        assert_eq!(s.export_chat("c").unwrap().unwrap(), before, "{:?}", s.kind());
        assert_eq!(s.list_chats(10, None).unwrap().chats, std::slice::from_ref(&before.chat));
        assert_eq!(s.pinned_chats().unwrap(), std::slice::from_ref(&before.chat));
        assert!(s.is_allowed("c", "read_file").unwrap());
        // Ids handed out after a reopen still work and keep ordering the chat.
        let ms = s.messages("c").unwrap();
        let z = s.append_message("c", &NewMessage::user("after")).unwrap();
        assert!(z > ms[1].id);
        s.append_message_text(ms[1].id, " again", "").unwrap();
        assert_eq!(s.message(ms[1].id).unwrap().unwrap().content, "Hello again");
    }
}

fn undo_journal(s: &dyn ChatStore) {
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
    assert_eq!(s.delete_undo_ops("t").unwrap(), 4);
    assert!(s.undo_ops("t").unwrap().is_empty());
    assert_eq!(s.add_undo_op("t", &ops[0]).unwrap(), 1, "seq restarts after a delete");

    // A task whose message lives in a chat.
    chat(s, "c");
    s.append_message("c", &NewMessage::assistant_streaming(Some("task-c".into()))).unwrap();
    s.add_undo_op("task-c", &ops[1]).unwrap();
    assert_eq!(s.add_undo_op("task-c", &ops[2]).unwrap(), 2);
    assert_eq!(s.undo_ops("task-c").unwrap(), ops[1..3]);
    assert_eq!(s.delete_undo_ops("task-c").unwrap(), 2);
    assert!(s.undo_ops("task-c").unwrap().is_empty());
}

/// `import_chat` recreates timestamps and flags exactly.
fn import_keeps_everything(s: &dyn ChatStore) {
    let step = |seq, result| Step {
        message_id: MessageId(0),
        seq,
        tool: "run_command".into(),
        title: format!("Run #{seq}"),
        access: StepAccess::Execute,
        args: json!({"cmd": ["ls", "-la"]}),
        result,
    };
    let msg = |content: &str, role, created_at| StoredMessage {
        id: MessageId(0),
        chat_id: "imp".into(),
        role,
        content: content.into(),
        reasoning: String::new(),
        done: true,
        error: None,
        stats: None,
        task_id: None,
        undone: false,
        created_at,
        steps: Vec::new(),
    };
    let mut reply = msg("Done.", Role::Assistant, 1_700_000_002_000);
    reply.reasoning = "think".into();
    reply.task_id = Some("imp-t".into());
    reply.error = Some("partial".into());
    reply.stats = Some(MessageStats { prompt_tokens: 5, completion_tokens: 6, elapsed_ms: 7, model_calls: 1 });
    reply.undone = true;
    reply.steps = vec![step(1, Some(StepResult { output: "a\nb".into(), failed: true, changes: vec![] })), step(2, None)];
    let mut streaming = msg("half", Role::Assistant, 1_700_000_003_000);
    streaming.done = false;
    let exp = ChatExport {
        chat: Chat {
            id: "imp".into(),
            title: "Imported".into(),
            workspace: "/w".into(),
            created_at: 1_700_000_000_000,
            updated_at: 1_700_000_009_000,
            pinned_at: Some(1_700_000_005_000),
        },
        messages: vec![msg("Q?", Role::User, 1_700_000_001_000), reply, streaming],
        grants: vec!["a_tool".into(), "b_tool".into()],
        undo: vec![("imp-t".into(), vec![UndoOp::Created { path: "/w/x".into() }])],
    };
    s.import_chat(&exp).unwrap();
    assert!(s.import_chat(&exp).is_err(), "ids are unique");
    let got = s.export_chat("imp").unwrap().unwrap();
    assert_eq!(super::normalized(got), super::normalized(exp));
}

fn chats_crud(s: &dyn ChatStore) {
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

fn pinning(s: &dyn ChatStore) {
    chat(s, "a");
    chat(s, "b");
    s.pin_chat("a", true).unwrap();
    let first = s.chat("a").unwrap().unwrap().pinned_at.unwrap();
    s.pin_chat("a", true).unwrap();
    assert_eq!(s.chat("a").unwrap().unwrap().pinned_at, Some(first), "re-pinning keeps the original time");
    assert_eq!(s.pinned_chats().unwrap().iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["a"]);
    s.pin_chat("a", false).unwrap();
    assert!(!s.chat("a").unwrap().unwrap().pinned());
    assert!(s.pinned_chats().unwrap().is_empty());
}

fn list_is_recent_first_and_paginates(s: &dyn ChatStore) {
    for i in 0..25 {
        chat(s, &format!("c{i:02}"));
    }
    // Activity moves a chat to the top. (Timestamps are in ms: make sure the bump is
    // strictly later than the last creation, or the id tie-break decides.)
    std::thread::sleep(std::time::Duration::from_millis(3));
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

fn exact_page_size_has_no_next(s: &dyn ChatStore) {
    for i in 0..10 {
        chat(s, &format!("c{i}"));
    }
    let page = s.list_chats(10, None).unwrap();
    assert_eq!(page.chats.len(), 10);
    assert!(page.next.is_none());
}

fn streaming_reply_lifecycle(s: &dyn ChatStore) {
    chat(s, "c");
    let u = s.append_message("c", &NewMessage::user("question")).unwrap();
    let a = s.append_message("c", &NewMessage::assistant_streaming(Some("task-1".into()))).unwrap();
    assert!(a > u);
    s.append_message_text(a, "", "thinking ").unwrap();
    s.append_message_text(a, "Hel", "more").unwrap();
    s.append_message_text(a, "lo", "").unwrap();
    s.append_message_text(a, "", "").unwrap();
    let m = s.message(a).unwrap().unwrap();
    assert_eq!((m.content.as_str(), m.reasoning.as_str(), m.done), ("Hello", "thinking more", false));

    let stats = MessageStats { prompt_tokens: 10, completion_tokens: 3, elapsed_ms: 420, model_calls: 1 };
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

fn failed_reply_and_interrupted_recovery(s: &dyn ChatStore) {
    chat(s, "c");
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

fn delete_messages_from_truncates_chat(s: &dyn ChatStore) {
    chat(s, "c");
    chat(s, "other");
    let ids: Vec<_> = (0..5).map(|i| s.append_message("c", &NewMessage::user(format!("m{i}"))).unwrap()).collect();
    let keep = s.append_message("other", &NewMessage::user("x")).unwrap();
    assert_eq!(s.delete_messages_from("c", ids[2]).unwrap(), 3);
    assert_eq!(s.messages("c").unwrap().iter().map(|m| m.content.as_str()).collect::<Vec<_>>(), ["m0", "m1"]);
    assert!(s.message(keep).unwrap().is_some());
}

fn steps_crud_and_bounds(s: &dyn ChatStore) {
    chat(s, "c");
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

fn grants(s: &dyn ChatStore) {
    chat(s, "a");
    chat(s, "b");
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

fn delete_chat_cascades_and_returns_tasks(s: &dyn ChatStore) {
    chat(s, "c");
    chat(s, "keep");
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

fn unicode_round_trips(s: &dyn ChatStore) {
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

fn embedded_nul_survives(s: &dyn ChatStore) {
    chat(s, "c");
    let id = s.append_message("c", &NewMessage::user("a\0b")).unwrap();
    s.append_message_text(id, "\0c", "").unwrap();
    assert_eq!(s.message(id).unwrap().unwrap().content, "a\0b\0c");
}

fn large_streamed_content(s: &dyn ChatStore) {
    chat(s, "c");
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
