//! Undoing a reply, and the chat after it.

use super::*;
use crate::test_store::folder_too;

/// Regression: after the user undid a reply's file changes, the model was still sent that
/// reply as it was ("I wrote notes.md"), so it went on as if the file were there. The reply
/// it reads says its changes were undone (after a restart too).
#[test]
fn the_model_knows_a_reply_was_undone() {
    let mut w = World::new();
    w.model.calls("write_file", json!({"path": "notes.md", "content": "buy milk"}));
    w.model.says("I wrote notes.md.");
    let turn = w.send("write my notes");
    turn.step("write_file").done();
    w.undo(turn.reply.id).unwrap();
    assert_eq!(w.read("notes.md"), None);
    w.restart();
    w.model.answers(|r| {
        let replies: Vec<String> = r.messages().iter().filter(|m| m["role"] == "assistant").map(content_text).collect();
        assert_eq!(replies.len(), 1);
        assert!(replies[0].starts_with("I wrote notes.md.") && replies[0].contains("undid"), "{replies:?}");
        Answer::text("It's gone, since you undid it.")
    });
    assert_eq!(w.send("is notes.md there?").error, None);
}

folder_too!(the_model_knows_a_reply_was_undone);
