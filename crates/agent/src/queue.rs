//! Per-chat message queues: follow-ups sent while a reply runs (see [`byob_protocol::queue`]).
//!
//! A queue belongs to the engine, not to a client, so every client of the chat sees and edits
//! the same one and it outlives the window or tab that filled it. It is kept in memory only.
//!
//! - A turn (or manual compaction) that ends normally starts the next queued message, in order;
//!   a turn's own auto-compaction happens inside the next turn, before its first request.
//! - One that is stopped or fails pauses the queue; it waits, unchanged, until a client resumes
//!   it, sends one item now, or retries the failed reply. A message typed into a paused chat
//!   that isn't busy is sent at once and the queue stays paused.
//! - A turn waiting for an approval is still running: the queue just waits.

use std::collections::VecDeque;

use byob_protocol::queue::{MAX_QUEUE_BYTES, MAX_QUEUED, MAX_QUEUED_TEXT_BYTES};
use byob_protocol::{ChatQueue, MediaRef, NoticeLevel, QueuePause, QueuedMessage, ServerMsg};

use crate::{ClientId, Engine, compact};

#[derive(Default)]
pub(crate) struct Queue {
    items: VecDeque<QueuedMessage>,
    paused: Option<QueuePause>,
    /// The turn running now is being stopped on purpose ("Send now"): its end doesn't pause.
    go_on: bool,
}

impl Queue {
    fn bytes(&self) -> usize {
        self.items.iter().map(|i| i.text.len()).sum()
    }

    fn position(&self, id: u64) -> Option<usize> {
        self.items.iter().position(|i| i.id == id)
    }

    /// Room for a message of `len` bytes, `replacing` one already queued.
    fn check_room(&self, len: usize, replacing: Option<usize>) -> Result<(), String> {
        if len > MAX_QUEUED_TEXT_BYTES {
            return Err(format!("that message is too long to queue (the limit is {} KB); send it when the reply is done", MAX_QUEUED_TEXT_BYTES / 1024));
        }
        let old = replacing.and_then(|i| self.items.get(i)).map_or(0, |i| i.text.len());
        if replacing.is_none() && self.items.len() >= MAX_QUEUED {
            return Err(format!("the queue is full ({MAX_QUEUED} messages)"));
        }
        if self.bytes().saturating_sub(old).saturating_add(len) > MAX_QUEUE_BYTES {
            return Err(format!("the queue is full ({} KB of text)", MAX_QUEUE_BYTES / 1024));
        }
        Ok(())
    }
}

impl Engine {
    /// What the viewers of `chat` see of its queue.
    pub(crate) fn queue_view(&self, chat: Option<&str>) -> ChatQueue {
        let q = chat.and_then(|c| self.queues.get(c));
        ChatQueue { chat: chat.map(str::to_owned), items: q.map(|q| q.items.iter().cloned().collect()).unwrap_or_default(), paused: q.and_then(|q| q.paused) }
    }

    /// Drop an empty queue (its pause goes with it) and tell the chat's viewers.
    fn queue_changed(&mut self, chat: &str) {
        if self.queues.get(chat).is_some_and(|q| q.items.is_empty()) {
            self.queues.remove(chat);
        }
        let view = self.queue_view(Some(chat));
        self.send_viewers(chat, ServerMsg::Queue(view));
    }

    /// `SendMessage` while `chat` is busy: append the message to its queue.
    pub(crate) fn enqueue(&mut self, client: ClientId, chat: &str, text: String, attachments: Vec<MediaRef>) {
        let q = self.queues.entry(chat.to_owned()).or_default();
        if let Err(e) = q.check_room(text.len(), None) {
            self.notice(client, e);
            self.queue_changed(chat);
            return;
        }
        self.queue_ids = self.queue_ids.wrapping_add(1);
        q.items.push_back(QueuedMessage { id: self.queue_ids, text, attachments });
        self.queue_changed(chat);
    }

    /// The queue of the chat `client` looks at and the position of item `id` in it, or a
    /// notice that it is gone (most likely just sent).
    fn queued(&mut self, client: ClientId, id: u64) -> Option<(String, usize)> {
        let found = self.client_chat(client).and_then(|c| self.queues.get(&c).and_then(|q| q.position(id)).map(|i| (c, i)));
        if found.is_none() {
            self.send_to(client, ServerMsg::Notice { level: NoticeLevel::Info, text: "That message isn't queued any more.".into() });
        }
        found
    }

    pub(crate) fn unqueue(&mut self, client: ClientId, id: u64) {
        let Some((chat, i)) = self.queued(client, id) else { return };
        if let Some(q) = self.queues.get_mut(&chat) {
            q.items.remove(i);
        }
        self.queue_changed(&chat);
    }

    /// Replace a queued message in place; attachments it already had stay valid, new ones
    /// must be uploads. Empty means remove.
    pub(crate) fn edit_queued(&mut self, client: ClientId, id: u64, text: String, paths: &[String]) {
        let Some((chat, i)) = self.queued(client, id) else { return };
        let text = text.trim().to_owned();
        let had: Vec<MediaRef> = self.queues.get(&chat).and_then(|q| q.items.get(i)).map(|m| m.attachments.clone()).unwrap_or_default();
        let fresh: Vec<String> = paths.iter().filter(|p| !had.iter().any(|h| &h.path == *p)).cloned().collect();
        let attachments = match self.uploads.claim(&fresh) {
            Ok(new) => {
                let mut kept: Vec<MediaRef> = had.into_iter().filter(|h| paths.contains(&h.path)).collect();
                kept.extend(new);
                kept
            }
            Err(e) => return self.notice(client, e),
        };
        if attachments.len() > byob_protocol::media::MAX_ATTACHMENTS {
            return self.notice(client, format!("at most {} files per message", byob_protocol::media::MAX_ATTACHMENTS));
        }
        let Some(q) = self.queues.get_mut(&chat) else { return };
        if text.is_empty() && attachments.is_empty() {
            q.items.remove(i);
        } else if let Err(e) = q.check_room(text.len(), Some(i)) {
            return self.notice(client, e);
        } else if let Some(item) = q.items.get_mut(i) {
            item.text = text;
            item.attachments = attachments;
        }
        self.queue_changed(&chat);
    }

    pub(crate) fn move_queued(&mut self, client: ClientId, id: u64, to: u32) {
        let Some((chat, i)) = self.queued(client, id) else { return };
        if let Some(q) = self.queues.get_mut(&chat)
            && let Some(item) = q.items.remove(i)
        {
            let to = usize::try_from(to).unwrap_or(usize::MAX).min(q.items.len());
            q.items.insert(to, item);
        }
        self.queue_changed(&chat);
    }

    /// Run item `id` next, now: it moves to the front, the queue resumes, and a running turn
    /// (or compaction) is stopped; its end then starts this message.
    pub(crate) fn send_queued_now(&mut self, client: ClientId, id: u64) {
        let Some((chat, i)) = self.queued(client, id) else { return };
        let busy = self.chat_busy(&chat);
        if let Some(q) = self.queues.get_mut(&chat) {
            if let Some(item) = q.items.remove(i) {
                q.items.push_front(item);
            }
            q.paused = None;
            q.go_on = busy;
        }
        self.queue_changed(&chat);
        if busy {
            self.stop_chat(&chat);
        } else {
            self.run_queue(&chat);
        }
    }

    pub(crate) fn resume_queue(&mut self, client: ClientId) {
        let Some(chat) = self.client_chat(client) else { return };
        self.unpause_queue(&chat);
        self.run_queue(&chat);
    }

    /// Clear a pause (resume, retry), without starting anything.
    pub(crate) fn unpause_queue(&mut self, chat: &str) {
        if let Some(q) = self.queues.get_mut(chat).filter(|q| q.paused.is_some()) {
            q.paused = None;
            self.queue_changed(chat);
        }
    }

    /// A turn or compaction of `chat` ended; `pause` says it was stopped or failed. Pause the
    /// queue then (unless "Send now" stopped it), else start the next message.
    pub(crate) fn queue_turn_ended(&mut self, chat: &str, pause: Option<QueuePause>) {
        let Some(q) = self.queues.get_mut(chat) else { return };
        let go_on = std::mem::take(&mut q.go_on);
        if let Some(why) = pause.filter(|_| !go_on && !q.items.is_empty()) {
            q.paused = Some(why);
            self.queue_changed(chat);
            return;
        }
        self.run_queue(chat);
    }

    /// Start queued messages while the chat is free and the queue isn't paused (a `/compact`
    /// with nothing to do leaves it free, so the next one follows at once).
    pub(crate) fn run_queue(&mut self, chat: &str) {
        loop {
            if self.chat_busy(chat) {
                return;
            }
            let Some(q) = self.queues.get_mut(chat).filter(|q| q.paused.is_none()) else { return };
            let Some(item) = q.items.pop_front() else {
                self.queue_changed(chat);
                return;
            };
            // Out of the queue before it shows up in the transcript.
            self.queue_changed(chat);
            let started = match compact::command(&item.text) {
                Some(focus) => match self.compact_chat_in(chat, focus) {
                    Err((NoticeLevel::Info, text)) => {
                        self.send_viewers(chat, ServerMsg::Notice { level: NoticeLevel::Info, text });
                        Ok(())
                    }
                    other => other.map_err(|(_, text)| text),
                },
                None => self.begin_turn_in(chat, item.text.clone(), item.attachments.clone()),
            };
            if let Err(text) = started {
                let q = self.queues.entry(chat.to_owned()).or_default();
                q.items.push_front(item);
                q.paused = Some(QueuePause::Failed);
                self.send_viewers(chat, ServerMsg::Notice { level: NoticeLevel::Error, text: format!("couldn't send the queued message: {text}") });
                self.queue_changed(chat);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: u64, text: &str) -> QueuedMessage {
        QueuedMessage { id, text: text.into(), attachments: vec![] }
    }

    #[test]
    fn room_is_capped_by_count_and_bytes() {
        let mut q = Queue::default();
        assert!(q.check_room(MAX_QUEUED_TEXT_BYTES + 1, None).unwrap_err().contains("too long"));
        for i in 0..MAX_QUEUED {
            q.check_room(1, None).unwrap();
            q.items.push_back(item(u64::try_from(i).unwrap(), "x"));
        }
        assert!(q.check_room(1, None).unwrap_err().contains("20 messages"));
        // Replacing one doesn't need a free slot.
        q.check_room(10, Some(0)).unwrap();
        let mut q = Queue::default();
        for i in 0..4 {
            q.items.push_back(item(i, &"y".repeat(MAX_QUEUED_TEXT_BYTES)));
        }
        assert!(q.check_room(1, None).unwrap_err().contains("KB of text"));
        q.check_room(MAX_QUEUED_TEXT_BYTES, Some(2)).unwrap();
    }
}
