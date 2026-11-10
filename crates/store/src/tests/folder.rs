//! Folder-backend specifics: file layout, crash leftovers, external changes, speed.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::{Value, json};

use super::TempDir;
use crate::folder::{folder_name, short_id, slug};
use crate::*;

fn open(dir: &Path) -> FolderStore {
    FolderStore::open(dir).unwrap()
}

fn events(s: &FolderStore, id: &str) -> PathBuf {
    s.chat_dir(id).unwrap().join("events.jsonl")
}

fn append_raw(path: &Path, bytes: &[u8]) {
    std::fs::OpenOptions::new().append(true).open(path).unwrap().write_all(bytes).unwrap();
}

fn folders(root: &Path, sub: &str) -> Vec<String> {
    let mut v: Vec<String> =
        std::fs::read_dir(root.join(sub)).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    v.sort();
    v
}

#[test]
fn layout_and_self_describing_lines() {
    let dir = TempDir::new();
    let s = open(dir.path());
    s.create_chat("c1", "Plan the trip", Path::new("/w")).unwrap();
    let a = s.append_message("c1", &NewMessage::assistant_streaming(None)).unwrap();
    s.append_message_text(a, "Hi", "").unwrap();
    s.finish_message(a, None, None).unwrap();
    s.grant(&GrantScope::Global, "read_file").unwrap();

    let names = folders(dir.path(), "chats");
    assert_eq!(names.len(), 1);
    assert!(names[0].ends_with(&format!("-plan-the-trip-{}", short_id("c1"))), "{names:?}");
    assert_eq!(names[0].len(), "2026-11-09-plan-the-trip-".len() + 6);
    let chat_dir = dir.path().join("chats").join(&names[0]);
    assert!(chat_dir.join("chat.md").exists());
    let text = std::fs::read_to_string(chat_dir.join("events.jsonl")).unwrap();
    let kinds: Vec<String> = text
        .lines()
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            assert_eq!(v["v"], 1);
            assert!(v["ts"].is_i64());
            v["e"].as_str().unwrap().to_owned()
        })
        .collect();
    assert_eq!(kinds, ["chat_created", "message_added", "message_delta", "message_finished"]);
    assert!(text.ends_with('\n'));
    let grants: Value = serde_json::from_str(&std::fs::read_to_string(dir.path().join("grants.json")).unwrap()).unwrap();
    assert_eq!(grants["global"], json!(["read_file"]));
}

#[test]
fn truncated_last_line_is_ignored_and_later_appends_stay_readable() {
    let dir = TempDir::new();
    let path;
    {
        let s = open(dir.path());
        s.create_chat("c", "t", Path::new("/w")).unwrap();
        let a = s.append_message("c", &NewMessage::assistant_streaming(None)).unwrap();
        s.append_message_text(a, "kept", "").unwrap();
        path = events(&s, "c");
    }
    // A crash mid-write: half a delta line, no newline.
    append_raw(&path, br#"{"v":1,"ts":1,"e":"message_delta","m":1,"content":"LOST"#);
    let s = open(dir.path());
    let ms = s.messages("c").unwrap();
    assert_eq!(ms[0].content, "kept");
    // The next append starts a fresh line instead of gluing onto the fragment.
    s.append_message_text(ms[0].id, " more", "").unwrap();
    s.finish_message(ms[0].id, None, None).unwrap();
    drop(s);
    let s = open(dir.path());
    let m = &s.messages("c").unwrap()[0];
    assert_eq!((m.content.as_str(), m.done), ("kept more", true));
}

#[test]
fn garbage_lines_are_skipped_but_a_damaged_undo_entry_is_an_error() {
    let dir = TempDir::new();
    let path;
    {
        let s = open(dir.path());
        s.create_chat("c", "t", Path::new("/w")).unwrap();
        s.append_message("c", &NewMessage::assistant_streaming(Some("t1".into()))).unwrap();
        s.add_undo_op("t1", &UndoOp::Created { path: "/a".into() }).unwrap();
        path = events(&s, "c");
    }
    append_raw(&path, b"not json at all\n\n{\"v\":1}\n\xff\xfe binary\n");
    append_raw(&path, br#"{"v":7,"ts":1,"e":"hologram","m":1}"#);
    append_raw(&path, b"\n");
    append_raw(&path, br#"{"v":1,"ts":1,"e":"undo_op","task":"t1","op":{"type":"exploded"}}"#);
    append_raw(&path, b"\n");
    append_raw(&path, br#"{"v":1,"ts":5,"e":"message_delta","m":1,"content":"still here"}"#);
    append_raw(&path, b"\n");
    let s = open(dir.path());
    assert_eq!(s.messages("c").unwrap()[0].content, "still here");
    assert!(matches!(s.undo_ops("t1"), Err(StoreError::BadValue { column: "undo_ops.op", .. })));
    // Clearing the journal clears the damage too.
    s.delete_undo_ops("t1").unwrap();
    assert!(s.undo_ops("t1").unwrap().is_empty());
}

#[test]
fn slugs_and_folder_names() {
    assert_eq!(slug("Hello, World!!"), "hello-world");
    assert_eq!(slug("  --Résumé 2026: draft #3--  "), "r-sum-2026-draft-3");
    assert_eq!(slug("日本語のチャット"), "chat");
    assert_eq!(slug(""), "chat");
    assert_eq!(slug("../../etc/passwd"), "etc-passwd");
    assert_eq!(slug("CON"), "con");
    assert_eq!(slug(&"a".repeat(100)).len(), 40);
    assert!(slug(&"ab ".repeat(30)).len() <= 40);
    assert!(!slug(&"ab ".repeat(30)).ends_with('-'));
    assert_eq!(short_id("c1").len(), 6);
    assert_ne!(short_id("c19a6b2c3f1e0"), short_id("c19a6b2c3f1e1"));
    assert_eq!(folder_name(0, "x", "id"), format!("1970-01-01-x-{}", short_id("id")));

    let dir = TempDir::new();
    let s = open(dir.path());
    s.create_chat("ü/ñ\\😀", "Ünïcødé 📄 / ..", Path::new("/home/zoë")).unwrap();
    let name = s.chat_dir("ü/ñ\\😀").unwrap().file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.is_ascii() && !name.contains('/') && !name.contains('\\'), "{name}");
    assert!(name.contains("-n-c-d-"), "{name}");
    drop(s);
    let s = open(dir.path());
    assert_eq!(s.chat("ü/ñ\\😀").unwrap().unwrap().title, "Ünïcødé 📄 / ..");
}

#[test]
fn same_title_same_day_get_their_own_folders() {
    let dir = TempDir::new();
    let s = open(dir.path());
    for id in ["a", "b", "c"] {
        s.create_chat(id, "New chat", Path::new("/w")).unwrap();
    }
    let names = folders(dir.path(), "chats");
    assert_eq!(names.len(), 3);
    assert!(names.iter().all(|n| n.contains("-new-chat-")));
    // A taken name (another chat's folder, or anything else) gets a numeric suffix.
    let taken = folder_name(crate::store::now(), "New chat", "d");
    std::fs::create_dir(dir.path().join("chats").join(&taken)).unwrap();
    s.create_chat("d", "New chat", Path::new("/w")).unwrap();
    assert_eq!(s.chat_dir("d").unwrap().file_name().unwrap().to_string_lossy(), format!("{taken}-2"));
    drop(s);
    let s = open(dir.path());
    assert_eq!(s.list_chats(10, None).unwrap().chats.len(), 4, "an empty folder is not a chat");
}

#[test]
fn rename_keeps_the_folder_and_updates_chat_md() {
    let dir = TempDir::new();
    let s = open(dir.path());
    s.create_chat("c", "First title", Path::new("/w")).unwrap();
    let before = s.chat_dir("c").unwrap();
    s.rename_chat("c", "Totally different").unwrap();
    assert_eq!(s.chat_dir("c").unwrap(), before);
    let md = std::fs::read_to_string(before.join("chat.md")).unwrap();
    assert!(md.contains("title: \"Totally different\""), "{md}");
    drop(s);
    let s = open(dir.path());
    assert_eq!(s.chat("c").unwrap().unwrap().title, "Totally different");
    assert_eq!(s.chat_dir("c").unwrap(), before);
}

#[test]
fn delete_moves_the_folder_to_trash() {
    let dir = TempDir::new();
    let s = open(dir.path());
    s.create_chat("c", "Doomed", Path::new("/w")).unwrap();
    s.append_message("c", &NewMessage::assistant_streaming(Some("t".into()))).unwrap();
    s.add_undo_op("t", &UndoOp::Created { path: "/x".into() }).unwrap();
    let name = s.chat_dir("c").unwrap().file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(s.delete_chat("c").unwrap(), ["t"]);
    assert!(folders(dir.path(), "chats").is_empty());
    assert_eq!(folders(dir.path(), ".trash"), std::slice::from_ref(&name));
    let trashed = std::fs::read_to_string(dir.path().join(".trash").join(&name).join("events.jsonl")).unwrap();
    assert!(trashed.trim_end().ends_with(r#""e":"deleted"}"#), "{trashed}");
    assert!(s.undo_ops("t").unwrap().is_empty());
    assert!(s.chat("c").unwrap().is_none());

    // A chat with the same folder name deleted later doesn't overwrite the first.
    s.create_chat("c", "Doomed", Path::new("/w")).unwrap();
    s.delete_chat("c").unwrap();
    assert_eq!(folders(dir.path(), ".trash").len(), 2);

    // A synced-in folder whose log says deleted stays hidden.
    std::fs::rename(dir.path().join(".trash").join(&name), dir.path().join("chats").join(&name)).unwrap();
    drop(s);
    assert!(open(dir.path()).list_chats(10, None).unwrap().chats.is_empty());
}

#[test]
fn changes_from_elsewhere_show_up_on_list() {
    let dir = TempDir::new();
    let mine = open(dir.path());
    mine.create_chat("mine", "Mine", Path::new("/w")).unwrap();
    mine.append_message("mine", &NewMessage::user("hello")).unwrap();
    let mid = mine.messages("mine").unwrap()[0].id;

    // "Another machine": a second store on the same (synced) folder.
    {
        let theirs = open(dir.path());
        theirs.create_chat("theirs", "Theirs", Path::new("/w")).unwrap();
        theirs.append_message("mine", &NewMessage::user("from the laptop")).unwrap();
    }
    let ids: Vec<String> = mine.list_chats(10, None).unwrap().chats.into_iter().map(|c| c.id).collect();
    assert_eq!(ids.len(), 2, "{ids:?}");
    assert_eq!(mine.messages("mine").unwrap().iter().map(|m| m.content.as_str()).collect::<Vec<_>>(), ["hello", "from the laptop"]);
    assert_eq!(mine.messages("theirs").unwrap().len(), 0);
    // Our ids stay valid after the reload, and our next write merges with theirs.
    mine.append_message_text(mid, "!", "").unwrap();
    assert_eq!(mine.message(mid).unwrap().unwrap().content, "hello!");

    // Removed elsewhere: gone here at the next listing.
    std::fs::remove_dir_all(mine.chat_dir("theirs").unwrap()).unwrap();
    assert_eq!(mine.list_chats(10, None).unwrap().chats.len(), 1);
    assert!(mine.chat("theirs").unwrap().is_none());

    // A write after an unnoticed external append replays the file instead of losing it.
    let path = events(&mine, "mine");
    append_raw(&path, br#"{"v":1,"ts":2,"e":"renamed","title":"Renamed elsewhere"}"#);
    append_raw(&path, b"\n");
    mine.append_message("mine", &NewMessage::user("third")).unwrap();
    assert_eq!(mine.chat("mine").unwrap().unwrap().title, "Renamed elsewhere");
    assert_eq!(mine.messages("mine").unwrap().len(), 3);
}

#[test]
fn junk_and_duplicate_folders_are_ignored() {
    let dir = TempDir::new();
    let s = open(dir.path());
    s.create_chat("c", "Real", Path::new("/w")).unwrap();
    let real = s.chat_dir("c").unwrap();
    drop(s);
    let chats = dir.path().join("chats");
    // No chat_created event.
    std::fs::create_dir(chats.join("junk")).unwrap();
    std::fs::write(chats.join("junk/events.jsonl"), "{\"v\":1,\"ts\":1,\"e\":\"renamed\",\"title\":\"x\"}\n").unwrap();
    // A stray file, and a sync tool's conflict copy of a whole folder.
    std::fs::write(chats.join("notes.txt"), "hi").unwrap();
    let copy = chats.join("zz-conflict-copy");
    std::fs::create_dir(&copy).unwrap();
    std::fs::copy(real.join("events.jsonl"), copy.join("events.jsonl")).unwrap();
    let s = open(dir.path());
    let page = s.list_chats(10, None).unwrap();
    assert_eq!(page.chats.len(), 1);
    assert_eq!(s.chat_dir("c").unwrap(), real);
}

#[test]
fn index_cache_is_derived() {
    let dir = TempDir::new();
    {
        let s = open(dir.path());
        s.create_chat("c", "Cached", Path::new("/w")).unwrap();
        s.append_message("c", &NewMessage::user("x")).unwrap();
    }
    let index = dir.path().join(".index.json");
    assert!(index.exists());
    let expect = open(dir.path()).list_chats(10, None).unwrap();
    // Garbage or missing cache: rebuilt from the events.
    std::fs::write(&index, "{{{{").unwrap();
    assert_eq!(open(dir.path()).list_chats(10, None).unwrap(), expect);
    std::fs::remove_file(&index).unwrap();
    assert_eq!(open(dir.path()).list_chats(10, None).unwrap(), expect);
}

#[test]
fn chat_md_snapshot() {
    let dir = TempDir::new();
    let s = open(dir.path());
    let step = |seq, tool: &str, title: &str, result| Step {
        message_id: MessageId(0),
        seq,
        tool: tool.into(),
        title: title.into(),
        access: StepAccess::Read,
        args: json!({}),
        result,
    };
    let msg = |role, content: &str, created_at| StoredMessage {
        id: MessageId(0),
        chat_id: "snap".into(),
        role,
        content: content.into(),
        reasoning: String::new(),
        done: true,
        error: None,
        stats: None,
        task_id: None,
        undone: false,
        created_at,
        steps: vec![],
    };
    let mut reply = msg(Role::Assistant, "It's **42**.\n", 1_762_684_260_000);
    reply.reasoning = "Count carefully.".into();
    reply.steps = vec![
        step(1, "read_file", "Read notes.md", Some(StepResult::default())),
        step(2, "run_command", "Run\nmake", Some(StepResult { failed: true, ..Default::default() })),
        step(3, "fetch", "Fetch docs", None),
    ];
    let mut failed = msg(Role::Assistant, "Partial", 1_762_684_320_000);
    failed.error = Some("connection\nreset".into());
    failed.undone = true;
    let exp = ChatExport {
        chat: Chat {
            id: "snap".into(),
            title: "The \"answer\": 42".into(),
            workspace: "/home/me/proj".into(),
            created_at: 1_762_684_200_000,
            updated_at: 1_762_684_320_000,
            pinned_at: Some(1_762_684_300_000),
        },
        messages: vec![msg(Role::User, "What is the answer?", 1_762_684_200_000), reply, failed],
        grants: vec![],
        undo: vec![],
        compactions: vec![],
        agent_runs: vec![],
    };
    s.import_chat(&exp).unwrap();
    let md = std::fs::read_to_string(s.chat_dir("snap").unwrap().join("chat.md")).unwrap();
    let want = r#"---
title: "The \"answer\": 42"
id: "snap"
created: 2025-11-09T10:30:00Z
updated: 2025-11-09T10:32:00Z
workspace: "/home/me/proj"
pinned: true
---

<!-- Generated by yah from events.jsonl; edits here are overwritten. -->

# The "answer": 42

## You · 2025-11-09 10:30 UTC

What is the answer?

## yah · 2025-11-09 10:31 UTC

<details><summary>Thinking</summary>

Count carefully.

</details>

- ✓ `read_file` Read notes.md
- ✗ `run_command` Run make (failed)
- … `fetch` Fetch docs (running or interrupted)

It's **42**.

## yah · 2025-11-09 10:32 UTC · _changes undone_

Partial

> **Error:** connection reset

"#;
    assert_eq!(md, want);
}

/// Write `n` chats straight to disk (as if synced in), each a short conversation.
fn seed(root: &Path, n: usize) {
    let chats = root.join("chats");
    for i in 0..n {
        let id = format!("seed{i:05}");
        let ts = 1_700_000_000_000i64 + i as i64 * 1000;
        let d = chats.join(format!("2023-11-14-chat-{i}-{}", short_id(&id)));
        std::fs::create_dir_all(&d).unwrap();
        let mut text = format!("{{\"v\":1,\"ts\":{ts},\"e\":\"chat_created\",\"id\":\"{id}\",\"title\":\"Chat number {i}\",\"workspace\":\"/w\"}}\n");
        for m in 1..=10 {
            let role = if m % 2 == 1 { "user" } else { "assistant" };
            text.push_str(&format!(
                "{{\"v\":1,\"ts\":{},\"e\":\"message_added\",\"m\":{m},\"role\":\"{role}\",\"content\":\"{}\",\"done\":true}}\n",
                ts + m,
                "lorem ipsum dolor sit amet ".repeat(20)
            ));
        }
        std::fs::write(d.join("events.jsonl"), text).unwrap();
    }
}

#[test]
fn listing_2000_chats_is_fast() {
    let dir = TempDir::new();
    seed(dir.path(), 2000);
    let t = Instant::now();
    let s = open(dir.path());
    let cold = t.elapsed();
    let t = Instant::now();
    let first = s.list_chats(50, None).unwrap();
    let list = t.elapsed();
    let t = Instant::now();
    let mut after = first.next.clone();
    let mut total = first.chats.len();
    while let Some(c) = after {
        let p = s.list_chats(50, Some(&c)).unwrap();
        total += p.chats.len();
        after = p.next;
    }
    let paged = t.elapsed();
    assert_eq!(total, 2000);
    assert_eq!(first.chats[0].id, "seed01999");
    drop(s);
    let t = Instant::now();
    let s = open(dir.path());
    let warm = t.elapsed();
    let t = Instant::now();
    let open_one = s.messages("seed00042").unwrap().len();
    let lazy = t.elapsed();
    assert_eq!(open_one, 10);
    eprintln!(
        "folder store, 2000 chats × 10 messages: cold open {:.1} ms, warm open (index) {:.1} ms, \
         first page incl. rescan {:.2} ms, remaining 39 pages {:.2} ms, open one chat {:.2} ms",
        cold.as_secs_f64() * 1e3,
        warm.as_secs_f64() * 1e3,
        list.as_secs_f64() * 1e3,
        paged.as_secs_f64() * 1e3,
        lazy.as_secs_f64() * 1e3
    );
    // Generous bounds: debug builds on a loaded CI box.
    assert!(cold.as_secs() < 20 && warm.as_secs() < 5 && list.as_secs() < 2, "too slow");
}

#[test]
fn compactions_are_events_and_render_as_a_note() {
    let dir = TempDir::new();
    let s = open(dir.path());
    s.create_chat("c", "Long chat", Path::new("/w")).unwrap();
    s.append_message("c", &NewMessage::user("first question")).unwrap();
    let a = s.append_message("c", &NewMessage::assistant_streaming(None)).unwrap();
    s.append_message_text(a, "first answer", "").unwrap();
    s.finish_message(a, None, None).unwrap();
    s.append_message("c", &NewMessage::user("second question")).unwrap();
    let new =
        NewCompaction { through: a, summary: "We talked about **the first question**.".into(), tokens_before: 9000, tokens_after: 400, ..Default::default() };
    s.add_compaction("c", &new).unwrap();
    // An id from another chat's slot doesn't name a message of this one.
    assert!(matches!(s.add_compaction("c", &NewCompaction { through: MessageId(a.0 + (1 << 32)), ..new.clone() }), Err(StoreError::NotFound(_))));

    let text = std::fs::read_to_string(events(&s, "c")).unwrap();
    let last: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    assert_eq!((last["v"].as_u64(), last["e"].as_str(), last["n"].as_u64(), last["through"].as_u64()), (Some(1), Some("compaction_added"), Some(1), Some(2)));
    assert!(last.get("manual").is_none() && last.get("focus").is_none(), "defaults are left out: {last}");

    let md = std::fs::read_to_string(s.chat_dir("c").unwrap().join("chat.md")).unwrap();
    let note = md.find("— context compacted —").unwrap();
    assert!(md.find("first answer").unwrap() < note && note < md.find("second question").unwrap(), "{md}");
    assert!(md.contains("<details><summary>Summary the model continues from</summary>\n\nWe talked about **the first question**.\n\n</details>"), "{md}");
    assert!(md.contains("(automatic, ~9000 → ~400 tokens,"), "{md}");

    // Retrying from the reply drops the compaction, in the file view too.
    s.delete_messages_from("c", a).unwrap();
    assert!(s.compactions("c").unwrap().is_empty());
    let md = std::fs::read_to_string(s.chat_dir("c").unwrap().join("chat.md")).unwrap();
    assert!(!md.contains("context compacted"), "{md}");
}

#[test]
fn hand_written_and_newer_compaction_lines_degrade() {
    // A well-formed line written by someone else counts; one this build can't parse is
    // skipped. Either way the chat still opens.
    let dir = TempDir::new();
    let s = open(dir.path());
    s.create_chat("c", "x", Path::new("/w")).unwrap();
    s.append_message("c", &NewMessage::user("hi")).unwrap();
    let path = events(&s, "c");
    drop(s);
    append_raw(&path, b"{\"v\":1,\"ts\":1,\"e\":\"compaction_added\",\"n\":1,\"through\":1,\"summary\":\"ok\"}\n");
    append_raw(&path, b"{\"v\":2,\"ts\":2,\"e\":\"compaction_added\",\"n\":\"two\"}\n");
    let s = open(dir.path());
    let cs = s.compactions("c").unwrap();
    assert_eq!(cs.len(), 1);
    assert_eq!(cs[0].through, s.messages("c").unwrap()[0].id);
    assert_eq!(cs[0].summary, "ok");
}
