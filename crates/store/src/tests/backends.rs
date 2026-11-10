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
    compactions_follow_their_messages,
    ids_of_a_deleted_chat_stay_dead,
    agent_runs_record_child_threads,
);

fn agent_runs_record_child_threads(s: &dyn ChatStore) {
    chat(s, "c");
    chat(s, "other");
    s.append_message("c", &NewMessage::user("do two things")).unwrap();
    let reply = s.append_message("c", &NewMessage::assistant_streaming(None)).unwrap();
    let seq = s.add_step(reply, &NewStep { tool: "delegate".into(), title: "Delegate".into(), access: StepAccess::Read, args: json!({}) }).unwrap();
    let new = |id: &str| NewAgentRun { id: id.into(), parent: reply, step_seq: seq, agent: "explorer".into(), task: "look".into(), model: "local".into() };
    s.add_agent_run("c", &new("run-a")).unwrap();
    s.add_agent_run("c", &new("run-b")).unwrap();
    // Wrong chat, duplicate or unusable ids are refused.
    assert!(s.add_agent_run("other", &new("run-c")).is_err());
    assert!(s.add_agent_run("c", &new("run-a")).is_err());
    assert!(matches!(s.add_agent_run("c", &new("../x")), Err(StoreError::BadValue { .. })));
    s.append_agent_text("c", "run-a", "Found ", "thinking").unwrap();
    s.append_agent_text("c", "run-a", "it.", "").unwrap();
    let step = NewStep { tool: "read_file".into(), title: "Read a.txt".into(), access: StepAccess::Read, args: json!({"path": "a.txt"}) };
    assert_eq!(s.add_agent_step("c", "run-a", &step).unwrap(), 1);
    assert_eq!(s.add_agent_step("c", "run-a", &step).unwrap(), 2);
    s.finish_agent_step("c", "run-a", 1, &StepResult { output: "hello".into(), failed: false, changes: vec![] }).unwrap();
    assert!(s.finish_agent_step("c", "run-a", 9, &StepResult::default()).is_err());
    assert!(s.append_agent_text("other", "run-a", "x", "").is_err(), "runs are per chat");
    assert!(s.append_agent_text("c", "nope", "x", "").is_err());
    let stats = MessageStats { prompt_tokens: 100, completion_tokens: 20, elapsed_ms: 900, model_calls: 2 };
    s.finish_agent_run("c", "run-a", None, Some(&stats)).unwrap();
    s.finish_agent_run("c", "run-b", Some("stopped"), None).unwrap();

    let runs = s.agent_runs("c").unwrap();
    assert_eq!(runs.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["run-a", "run-b"]);
    let a = &runs[0];
    assert_eq!((a.parent, a.step_seq, a.agent.as_str(), a.task.as_str(), a.model.as_str()), (reply, seq, "explorer", "look", "local"));
    assert_eq!((a.content.as_str(), a.reasoning.as_str(), a.done, a.stats), ("Found it.", "thinking", true, Some(stats)));
    assert_eq!(a.steps.len(), 2);
    assert_eq!(a.steps[0].result.as_ref().map(|r| r.output.as_str()), Some("hello"));
    assert_eq!((a.steps[1].message_id, a.steps[1].result.is_none()), (reply, true));
    assert_eq!(runs[1].error.as_deref(), Some("stopped"));
    assert!(s.agent_runs("other").unwrap().is_empty());
    assert!(s.agent_runs("missing").unwrap().is_empty());

    // Exported with the chat; dropped with their parent message.
    let exp = s.export_chat("c").unwrap().unwrap();
    assert_eq!(exp.agent_runs.iter().map(|r| r.parent).collect::<Vec<_>>(), [1, 1]);
    s.delete_messages_from("c", reply).unwrap();
    assert!(s.agent_runs("c").unwrap().is_empty());
}

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
        compactions: vec![
            ExportedCompaction { covers: 0, compaction: compaction("nothing yet", 1_700_000_001_500, None, false) },
            ExportedCompaction { covers: 2, compaction: compaction("Q and Done", 1_700_000_004_000, Some("the plan"), true) },
        ],
        agent_runs: vec![ExportedAgentRun {
            parent: 1,
            run: AgentRun {
                id: "imp-run-1".into(),
                chat_id: "imp".into(),
                parent: MessageId(0),
                step_seq: 2,
                agent: "explorer".into(),
                task: "Count lines".into(),
                model: "local".into(),
                content: "It has 3 lines.".into(),
                reasoning: "hmm".into(),
                done: true,
                error: None,
                stats: Some(MessageStats { prompt_tokens: 9, completion_tokens: 3, elapsed_ms: 40, model_calls: 2 }),
                created_at: 1_700_000_002_500,
                steps: vec![step(1, Some(StepResult { output: "3".into(), failed: false, changes: vec![] }))],
            },
        }],
    };
    s.import_chat(&exp).unwrap();
    assert!(s.import_chat(&exp).is_err(), "ids are unique");
    let got = s.export_chat("imp").unwrap().unwrap();
    assert_eq!(super::normalized(got), super::normalized(exp));
}

fn compaction(summary: &str, created_at: Millis, focus: Option<&str>, manual: bool) -> Compaction {
    Compaction {
        id: 0,
        chat_id: "imp".into(),
        through: MessageId(0),
        summary: summary.into(),
        tokens_before: 182_000,
        tokens_after: 9_000,
        prompt_tokens: 150_000,
        completion_tokens: 2_000,
        focus: focus.map(str::to_owned),
        manual,
        created_at,
    }
}

fn new_compaction(through: MessageId, summary: &str) -> NewCompaction {
    NewCompaction {
        through,
        summary: summary.into(),
        tokens_before: 182_000,
        tokens_after: 9_000,
        prompt_tokens: 150_000,
        completion_tokens: 2_000,
        focus: Some("the CSV".into()),
        manual: true,
    }
}

/// Same semantics as the SQLite-only test in `db.rs`, on every backend.
fn compactions_follow_their_messages(s: &dyn ChatStore) {
    chat(s, "c");
    chat(s, "other");
    let ids: Vec<_> = (0..4).map(|i| s.append_message("c", &NewMessage::user(format!("m{i}"))).unwrap()).collect();
    assert!(s.compactions("c").unwrap().is_empty());
    assert!(s.compactions("nope").unwrap().is_empty());
    let none = s.add_compaction("c", &NewCompaction { manual: false, focus: None, ..new_compaction(MessageId(0), "before anything") }).unwrap();
    assert_eq!(none.through, MessageId(0));
    let first = s.add_compaction("c", &new_compaction(ids[1], "first")).unwrap();
    let second = s.add_compaction("c", &new_compaction(ids[3], &"x".repeat(MAX_SUMMARY + 10))).unwrap();
    assert!(second.summary.len() < MAX_SUMMARY + 100 && second.summary.contains("not saved"));
    let all = s.compactions("c").unwrap();
    assert_eq!(all, vec![none.clone(), first.clone(), second]);
    assert!(all[0].id < all[1].id && all[1].id < all[2].id, "ids order a chat's compactions");
    assert_eq!((all[1].through, all[1].tokens_before, all[1].focus.as_deref(), all[1].manual), (ids[1], 182_000, Some("the CSV"), true));
    assert_eq!(all[1].chat_id, "c");
    assert!(s.compactions("other").unwrap().is_empty());
    assert!(matches!(s.add_compaction("nope", &new_compaction(ids[0], "x")), Err(StoreError::NotFound(_))));
    // Editing history from m2 on drops the compaction that covered m3, keeps the earlier ones.
    s.delete_messages_from("c", ids[2]).unwrap();
    assert_eq!(s.compactions("c").unwrap(), vec![none, first]);
    // Compacting again after the edit works and sorts last.
    let m = s.append_message("c", &NewMessage::user("again")).unwrap();
    let third = s.add_compaction("c", &new_compaction(m, "third")).unwrap();
    assert_eq!(s.compactions("c").unwrap().last(), Some(&third));
    // Deleting the chat deletes its compactions, also for a new chat with the same id.
    s.delete_chat("c").unwrap();
    assert!(s.compactions("c").unwrap().is_empty());
    chat(s, "c");
    assert!(s.compactions("c").unwrap().is_empty());
}

#[test]
fn compactions_point_at_the_same_message_after_reopen() {
    for f in [sqlite(), folder()] {
        let s = &*f;
        chat(s, "c");
        let ids: Vec<_> = (0..3).map(|i| s.append_message("c", &NewMessage::user(format!("m{i}"))).unwrap()).collect();
        s.add_compaction("c", &new_compaction(ids[1], "covers m0 and m1")).unwrap();
        let kind = s.kind();
        let f = f.reopen();
        let s = &*f;
        let ms = s.messages("c").unwrap();
        let cs = s.compactions("c").unwrap();
        assert_eq!(cs.len(), 1, "{kind:?}");
        // The (new) id of the second message, whatever ids this store issues now.
        assert_eq!(cs[0].through, ms[1].id, "{kind:?}");
        assert_eq!(ms.iter().filter(|m| m.id <= cs[0].through).count(), 2, "{kind:?}");
        // And deletion semantics still hold after the reopen.
        s.delete_messages_from("c", ms[1].id).unwrap();
        assert!(s.compactions("c").unwrap().is_empty(), "{kind:?}");
    }
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
    assert_eq!(s.finish_interrupted("byob stopped").unwrap(), 1);
    let ms = s.messages("c").unwrap();
    assert_eq!(ms[0].error.as_deref(), Some("connection reset"));
    assert_eq!(ms[1].id, b);
    assert!(ms[1].done);
    assert_eq!(ms[1].error.as_deref(), Some("byob stopped"));
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
