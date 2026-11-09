//! `export` between backends: SQLite → folder → SQLite must lose nothing.

use std::path::Path;

use serde_json::json;

use super::{Fixture, folder, normalized, sqlite};
use crate::*;

/// A realistic session: a streamed reply with reasoning, tool steps (one failed, one left
/// running), undo entries, a failed reply, an undone one, grants, a pinned chat and an
/// untouched second chat.
fn populate(s: &dyn ChatStore) {
    s.create_chat("c-main", "Fix the build — ünïcode ✓", Path::new("/home/me/proj")).unwrap();
    s.create_chat("c-other", "Second", Path::new("/tmp")).unwrap();
    s.append_message("c-other", &NewMessage::user("lonely")).unwrap();

    s.append_message("c-main", &NewMessage::user("Why does `cargo build` fail?\n\nSee log.")).unwrap();
    let a = s.append_message("c-main", &NewMessage::assistant_streaming(Some("task-1".into()))).unwrap();
    for (c, r) in [("", "Let me look"), ("", " at the log."), ("Checking", ""), (" now…", " Found it.")] {
        s.append_message_text(a, c, r).unwrap();
    }
    let read = s
        .add_step(a, &NewStep { tool: "read_file".into(), title: "Read Cargo.toml".into(), access: StepAccess::Read, args: json!({"path": "Cargo.toml"}) })
        .unwrap();
    s.finish_step(a, read, &StepResult { output: "[package]\nname = \"x\"".into(), failed: false, changes: vec![] }).unwrap();
    let write = s
        .add_step(
            a,
            &NewStep { tool: "write_file".into(), title: "Write src/lib.rs".into(), access: StepAccess::Write, args: json!({"path": "src/lib.rs", "n": 2}) },
        )
        .unwrap();
    s.add_undo_op("task-1", &UndoOp::Replaced { path: "/home/me/proj/src/lib.rs".into(), backup: "/undo/task-1/1-lib.rs".into() }).unwrap();
    s.add_undo_op("task-1", &UndoOp::Created { path: "/home/me/proj/new.rs".into() }).unwrap();
    s.finish_step(
        a,
        write,
        &StepResult {
            output: "wrote".into(),
            failed: false,
            changes: vec![StepChange::Modified { path: "/home/me/proj/src/lib.rs".into() }, StepChange::Created { path: "/home/me/proj/new.rs".into() }],
        },
    )
    .unwrap();
    let run =
        s.add_step(a, &NewStep { tool: "run_command".into(), title: "Run cargo test".into(), access: StepAccess::Execute, args: json!("not json") }).unwrap();
    s.finish_step(a, run, &StepResult { output: "error[E0425]".into(), failed: true, changes: vec![] }).unwrap();
    s.add_step(a, &NewStep { tool: "fetch".into(), title: "Fetch docs".into(), access: StepAccess::Network, args: json!({}) }).unwrap();
    s.append_message_text(a, "\n\nFixed: a missing `use`.", "").unwrap();
    s.finish_message(a, None, Some(&MessageStats { prompt_tokens: 1200, completion_tokens: 87, elapsed_ms: 4321, model_calls: 3 })).unwrap();

    // An automatic compaction mid-chat (through the first reply) with model_calls in the
    // reply's stats, and a manual one with a focus.
    s.add_compaction(
        "c-main",
        &NewCompaction {
            through: a,
            summary: "User asked why the build fails; fixed a missing `use`.".into(),
            tokens_before: 5000,
            tokens_after: 300,
            ..Default::default()
        },
    )
    .unwrap();
    s.append_message("c-main", &NewMessage::user("Undo that please")).unwrap();
    let b = s.append_message("c-main", &NewMessage::assistant_streaming(Some("task-2".into()))).unwrap();
    s.append_message_text(b, "Sorry, the connec", "").unwrap();
    s.finish_message(b, Some("connection reset"), None).unwrap();
    s.add_undo_op("task-2", &UndoOp::Deleted { path: "/home/me/proj/huge.bin".into(), backup: None }).unwrap();
    s.add_undo_op("task-2", &UndoOp::Moved { from: "/a".into(), to: "/b".into() }).unwrap();
    s.set_message_undone(a, true).unwrap();
    s.add_compaction(
        "c-main",
        &NewCompaction {
            through: b,
            summary: "Build fixed, then undone.".into(),
            tokens_before: 6000,
            tokens_after: 200,
            prompt_tokens: 6000,
            completion_tokens: 40,
            focus: Some("the undo".into()),
            manual: true,
        },
    )
    .unwrap();

    s.grant(&GrantScope::Chat("c-main".into()), "write_file").unwrap();
    s.grant(&GrantScope::Chat("c-main".into()), "run_command").unwrap();
    s.grant(&GrantScope::Global, "read_file").unwrap();
    s.pin_chat("c-main", true).unwrap();
}

fn everything(s: &dyn ChatStore) -> (Vec<ChatExport>, Vec<String>) {
    let mut chats: Vec<ChatExport> = s.list_chats(MAX_PAGE, None).unwrap().chats.iter().map(|c| normalized(s.export_chat(&c.id).unwrap().unwrap())).collect();
    chats.sort_by(|a, b| a.chat.id.cmp(&b.chat.id));
    (chats, s.grants(&GrantScope::Global).unwrap())
}

#[test]
fn sqlite_to_folder_and_back_is_lossless() {
    let src = sqlite();
    populate(&*src);
    let original = everything(&*src);
    assert_eq!(original.0.len(), 2);
    assert_eq!(original.0[0].undo.len(), 2, "both tasks' journals are exported");
    let covers: Vec<usize> = original.0[0].compactions.iter().map(|c| c.covers).collect();
    assert_eq!(covers, [2, 4], "compactions are exported by position");
    assert_eq!(original.0[0].messages[1].stats.map(|s| s.model_calls), Some(3));

    let mut dst = folder();
    let report = export(&*src, &mut *dst.store).unwrap();
    assert_eq!((report.chats, report.messages, report.global_grants, report.compactions), (2, 5, 1, 2));
    assert!(report.skipped.is_empty());
    assert_eq!(everything(&*dst), original, "sqlite → folder");

    // From disk, not just from memory.
    let dst = dst.reopen();
    assert_eq!(everything(&*dst), original, "folder after reopen");
    // Listing order matches too.
    let ids = |s: &dyn ChatStore| s.list_chats(10, None).unwrap().chats.into_iter().map(|c| c.id).collect::<Vec<_>>();
    assert_eq!(ids(&*dst), ids(&*src));

    let mut back = sqlite();
    export(&*dst, &mut *back.store).unwrap();
    assert_eq!(everything(&*back), original, "folder → sqlite");
    assert!(back.is_allowed("c-main", "run_command").unwrap());

    // Running it again changes nothing.
    let again = export(&*dst, &mut *back.store).unwrap();
    assert_eq!(again.chats, 0);
    assert_eq!(again.skipped.len(), 2);
    assert_eq!(everything(&*back), original);
}

#[test]
fn folder_to_sqlite_keeps_a_live_folder_chat() {
    // Written through the folder backend directly (deltas, finish), then exported.
    let src: Fixture = folder();
    populate(&*src);
    let original = everything(&*src);
    let mut dst = sqlite();
    export(&*src, &mut *dst.store).unwrap();
    assert_eq!(everything(&*dst), original);
}
