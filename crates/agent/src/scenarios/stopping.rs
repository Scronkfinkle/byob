//! Stopping a reply at each stage: while the model streams, while a tool runs, and going on
//! with the chat afterwards.

use super::*;

impl World {
    /// Send a message and wait until `ready` picks a message out (the reply is running).
    fn start(&mut self, text: &str, ready: impl FnMut(&ServerMsg) -> Option<()>) {
        self.tell(ClientMsg::SendMessage { text: text.into(), attachments: vec![] });
        self.wait(ready);
    }

    /// Stop the running reply and wait for it to end; its error.
    fn stop(&mut self) -> Option<String> {
        self.tell(ClientMsg::Cancel);
        self.wait(|m| match m {
            ServerMsg::EntryFinished { error, .. } => Some(error.clone()),
            _ => None,
        })
    }
}

/// Stop while a command runs: the command ends, the reply ends at once as stopped, the step
/// doesn't stay running, and the chat goes on.
#[test]
fn stopping_a_running_command_ends_the_reply_and_the_chat_goes_on() {
    let mut w = World::new();
    w.model.calls("run_command", json!({"command": "sleep 30"}));
    w.start("wait a while", |m| {
        matches!(m, ServerMsg::StepUpdated { step, .. } if step.tool == "run_command" && step.status == StepStatus::Running).then_some(())
    });
    let asked = Instant::now();
    assert_eq!(w.stop().as_deref(), Some("stopped"));
    assert!(asked.elapsed() < Duration::from_secs(5), "stopping took {:?}", asked.elapsed());
    let reply = w.seen.entries.last().unwrap().clone();
    assert!(reply.steps.iter().all(|s| s.status.finished()), "{:?}", reply.steps);
    w.model.answers(|r| {
        assert_eq!(r.user_texts(), ["wait a while", "never mind"]);
        Answer::text("OK.")
    });
    let turn = w.send("never mind");
    assert_eq!(turn.error, None);
    assert_eq!(turn.text(), "OK.");
}

/// Stop while the model is still writing: it stops at once, what it wrote stays on screen
/// and saved, and the next message gets an answer.
#[test]
fn stopping_mid_reply_keeps_what_was_written() {
    let mut w = World::new();
    w.model.answers(|_| Answer::text("word ".repeat(200)).slowly(Duration::from_millis(20)));
    w.start("talk", |m| matches!(m, ServerMsg::EntryDelta { .. }).then_some(()));
    let asked = Instant::now();
    assert_eq!(w.stop().as_deref(), Some("stopped"));
    assert!(asked.elapsed() < Duration::from_secs(2), "stopping took {:?}", asked.elapsed());
    let reply = w.seen.entries.last().unwrap().clone();
    assert!(reply.content.starts_with("word") && reply.content.len() < 1000, "{}", reply.content);
    w.restart();
    let saved = w.seen.entries.iter().find(|e| e.id == reply.id).unwrap();
    assert_eq!(saved.content, reply.content);
    assert_eq!(saved.error.as_deref(), Some("stopped"));
    w.model.says("Sure.");
    assert_eq!(w.send("shorter please").text(), "Sure.");
}
