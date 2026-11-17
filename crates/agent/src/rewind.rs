//! Going back in a chat (`ClientMsg::Rewind`): one of the user's messages and everything
//! after it are removed, and new text (or the same, to try again) is sent in its place.
//! Files the removed replies changed stay as they are.

use byob_protocol::{EntryRole, NoticeLevel, ServerMsg};

use crate::{ClientId, Engine, chats};

impl Engine {
    pub(crate) fn rewind(&mut self, client: ClientId, entry: u64, text: &str) {
        let Some(chat) = self.client_chat(client) else { return };
        let fail = |e: &mut Self, text: String| e.send_to(client, ServerMsg::Notice { level: NoticeLevel::Error, text });
        if self.chat_busy(&chat) {
            fail(self, "a reply is still running in this chat; stop it first".into());
            return;
        }
        let attached = match self.live(&chat) {
            Ok(live) => match live.entries.iter().find(|e| e.id == entry && e.role == EntryRole::User) {
                Some(e) => e.attachments.clone(),
                None => {
                    fail(self, "that message isn't in this chat any more".into());
                    return;
                }
            },
            Err(e) => {
                fail(self, e);
                return;
            }
        };
        let text = text.trim_end().to_owned();
        if text.trim().is_empty() && attached.is_empty() {
            fail(self, "there's nothing to send".into());
            return;
        }
        if let Err(e) = self.store.delete_messages_from(&chat, chats::message_id(entry)) {
            fail(self, format!("couldn't go back: {e}"));
            return;
        }
        // Read the chat again: the store dropped the compactions that covered what went, and
        // the token tally follows.
        self.live.remove(&chat);
        let (entries, compactions) = match self.live(&chat) {
            Ok(l) => (l.entries.clone(), l.compactions.iter().map(chats::compaction).collect()),
            Err(e) => {
                fail(self, e);
                return;
            }
        };
        self.send_viewers(&chat, ServerMsg::Transcript { entries });
        self.send_viewers(&chat, ServerMsg::Compactions { chat: Some(chat.clone()), items: compactions });
        if let Some(usage) = self.usage(&chat) {
            self.send_viewers(&chat, ServerMsg::Usage(usage));
        }
        self.unpause_queue(&chat);
        self.begin_turn(client, text, attached);
    }
}
