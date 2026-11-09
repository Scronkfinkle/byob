//! One assistant turn: stream a reply from the inference endpoint on a worker thread and
//! report deltas back into the engine inbox. Tool calling will be layered on here.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Instant;

use yah_llm::{ChatMessage, StreamEvent};
use yah_protocol::{Entry, EntryRole, TurnStats};

use crate::Input;

const SYSTEM_PROMPT: &str = "You are yah, a helpful assistant running inside a desktop harness. Be concise and accurate.";

pub enum TurnEvent {
    Delta { id: u64, content: String, reasoning: String },
    Finished { id: u64, error: Option<String>, stats: Option<TurnStats> },
}

/// Build the request transcript from finished entries plus the new user message.
/// Reasoning is not sent back; failed or empty replies are skipped.
pub fn history(transcript: &[Entry], new_user_text: &str) -> Vec<ChatMessage> {
    let mut out = vec![ChatMessage::system(SYSTEM_PROMPT)];
    for e in transcript.iter().filter(|e| e.done && e.error.is_none() && !e.content.is_empty()) {
        out.push(match e.role {
            EntryRole::User => ChatMessage::user(e.content.clone()),
            EntryRole::Assistant => ChatMessage::assistant(e.content.clone()),
        });
    }
    out.push(ChatMessage::user(new_user_text));
    out
}

pub fn spawn(llm: yah_llm::Client, history: Vec<ChatMessage>, id: u64, cancel: Arc<AtomicBool>, inbox: Sender<Input>) -> std::io::Result<()> {
    thread::Builder::new().name("yah-turn".into()).spawn(move || {
        let started = Instant::now();
        let result = catch_unwind(AssertUnwindSafe(|| {
            llm.stream_chat(&history, &[], &cancel, |ev| {
                let (content, reasoning) = match ev {
                    StreamEvent::Content(t) => (t, String::new()),
                    StreamEvent::Reasoning(t) => (String::new(), t),
                };
                let _ = inbox.send(Input::Turn(TurnEvent::Delta { id, content, reasoning }));
            })
        }));
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let (error, stats) = match result {
            Ok(Ok(out)) => {
                let usage = out.usage.unwrap_or_default();
                let stats = TurnStats { prompt_tokens: usage.prompt_tokens, completion_tokens: usage.completion_tokens, elapsed_ms };
                (out.cancelled.then(|| "stopped".to_owned()), Some(stats))
            }
            Ok(Err(e)) => (Some(e.to_string()), None),
            Err(_) => (Some("internal error: the turn crashed".to_owned()), None),
        };
        let _ = inbox.send(Input::Turn(TurnEvent::Finished { id, error, stats }));
    })?;
    Ok(())
}
