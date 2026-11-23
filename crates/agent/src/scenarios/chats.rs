//! Chats over restarts: pinned, archived, brought back, and which one a client opens.

use byob_protocol::{ChatSummary, ClientMsg, ServerMsg};

use super::World;
use crate::test_store::folder_too;

/// Make a chat with one exchange; returns its id.
fn chat(w: &mut World, text: &str) -> String {
    w.new_chat();
    w.model.says("Noted.");
    assert_eq!(w.send(text).error, None);
    w.chat_id()
}

fn summary<'a>(w: &'a World, id: &str) -> &'a ChatSummary {
    w.seen.listed().into_iter().find(|c| c.id == id).unwrap_or_else(|| panic!("{id} isn't listed"))
}

fn pin(w: &mut World, id: &str, pinned: bool) {
    w.tell(ClientMsg::PinChat { id: id.into(), pinned });
    w.wait(|m| matches!(m, ServerMsg::ChatUpdated(s) if s.id == id && s.pinned_at.is_some() == pinned).then_some(()));
}

fn archive(w: &mut World, id: &str, archived: bool) {
    w.tell(ClientMsg::ArchiveChat { id: id.into(), archived });
    w.wait(|m| matches!(m, ServerMsg::ChatUpdated(s) if s.id == id && s.archived_at.is_some() == archived).then_some(()));
}

fn opened(w: &World) -> Option<String> {
    w.seen.chat.as_ref().map(|c| c.id.clone())
}

/// Regression (2a8be5d): archiving the latest chat, restarting and reconnecting opened the
/// archived chat again (archiving doesn't touch its last-active time). A client opens the
/// most recent chat that isn't archived, after restarts and reconnects alike; pinning
/// brings an archived chat back, archiving unpins; with every chat archived a client starts
/// a new one, and its first message makes a new chat rather than reviving one.
#[test]
fn archived_chats_stay_out_of_the_way_across_restarts() {
    let mut w = World::new();
    let garden = chat(&mut w, "Plan the garden");
    let seeds = chat(&mut w, "Order the seeds");
    pin(&mut w, &garden, true);
    archive(&mut w, &seeds, true);

    w.restart();
    assert_eq!(opened(&w), Some(garden.clone()), "the archived latest chat isn't reopened");
    assert!(summary(&w, &garden).pinned_at.is_some(), "the pin survived the restart");
    assert!(summary(&w, &seeds).archived_at.is_some(), "the archive survived the restart");
    w.reconnect();
    assert_eq!(opened(&w), Some(garden.clone()), "nor on reconnecting");

    // Pinning the archived chat brings it back; it's the latest again.
    pin(&mut w, &seeds, true);
    assert!(summary(&w, &seeds).archived_at.is_none());
    w.restart();
    assert_eq!(opened(&w), Some(seeds.clone()));
    let s = summary(&w, &seeds);
    assert!(s.pinned_at.is_some() && s.archived_at.is_none(), "{s:?}");

    // Archiving a pinned chat unpins it.
    archive(&mut w, &garden, true);
    w.restart();
    let g = summary(&w, &garden);
    assert!(g.archived_at.is_some() && g.pinned_at.is_none(), "{g:?}");
    assert_eq!(opened(&w), Some(seeds.clone()));

    // Everything archived: a client starts on a new chat, and talking makes a third one.
    archive(&mut w, &seeds, true);
    w.restart();
    assert_eq!(opened(&w), None, "nothing to reopen: a new chat");
    w.model.says("Fresh start.");
    w.send("Something new");
    let third = w.chat_id();
    assert!(third != garden && third != seeds);
    assert!(summary(&w, &seeds).archived_at.is_some() && summary(&w, &garden).archived_at.is_some(), "neither came back");
    w.restart();
    assert_eq!(opened(&w), Some(third));
    // What's on disk agrees with what was shown.
    let store = w.store();
    assert!(store.chat(&garden).unwrap().is_some_and(|c| c.archived_at.is_some() && c.pinned_at.is_none()));
}

folder_too!(archived_chats_stay_out_of_the_way_across_restarts);
