//! The chat list: pinned chats first, then recent ones, a "New chat" button, and rename /
//! pin / delete from each row's context menu (right click, or long press on touch screens).
//! On narrow screens (a phone over Tailscale) the list replaces the conversation while open.

use yah_protocol::{ChatCursor, ChatSummary, ClientMsg, ServerMsg};

use crate::theme::Tokens;

/// Below this width the sidebar is a full-screen list you toggle, not a side panel.
pub const NARROW: f32 = 720.0;
/// Chats asked for per "Show more".
const PAGE: u32 = 50;

#[derive(Default)]
pub struct Sidebar {
    pinned: Vec<ChatSummary>,
    recent: Vec<ChatSummary>,
    /// Where the next page starts; `None` when everything is loaded.
    next: Option<ChatCursor>,
    loading: bool,
    /// The chat this client looks at (`None`: a new chat not sent yet).
    current: Option<ChatSummary>,
    /// Chat being renamed, with the edited title.
    renaming: Option<(String, String)>,
    /// Chat waiting for delete confirmation.
    deleting: Option<String>,
    /// Shown? `None` until the first frame decides from the width.
    open: Option<bool>,
    was_narrow: bool,
}

fn sort(pinned: &mut [ChatSummary], recent: &mut [ChatSummary]) {
    pinned.sort_by(|a, b| b.pinned_at.cmp(&a.pinned_at).then_with(|| b.id.cmp(&a.id)));
    recent.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| b.id.cmp(&a.id)));
}

impl Sidebar {
    /// Apply a chat-list message; returns false for messages that aren't about chats.
    pub fn apply(&mut self, msg: &ServerMsg) -> bool {
        match msg {
            ServerMsg::Chats { pinned, recent, after, next } => {
                if after.is_none() {
                    self.pinned.clone_from(pinned);
                    self.recent.clear();
                }
                for c in recent {
                    if !self.recent.iter().any(|r| r.id == c.id) {
                        self.recent.push(c.clone());
                    }
                }
                self.next.clone_from(next);
                self.loading = false;
                sort(&mut self.pinned, &mut self.recent);
            }
            ServerMsg::ChatUpdated(chat) => self.upsert(chat.clone()),
            ServerMsg::ChatDeleted { id } => {
                self.pinned.retain(|c| &c.id != id);
                self.recent.retain(|c| &c.id != id);
            }
            ServerMsg::ChatOpened { chat } => self.current.clone_from(chat),
            _ => return false,
        }
        true
    }

    fn upsert(&mut self, chat: ChatSummary) {
        self.pinned.retain(|c| c.id != chat.id);
        let before = self.recent.len();
        self.recent.retain(|c| c.id != chat.id);
        let was_listed = self.recent.len() != before;
        if let Some(cur) = self.current.as_mut().filter(|c| c.id == chat.id) {
            cur.clone_from(&chat);
        }
        if chat.pinned_at.is_some() {
            self.pinned.push(chat);
        } else {
            // Beyond the loaded pages it will turn up when the user pages there.
            let loaded_up_to = self.recent.last().map(|c| (c.updated_at, c.id.clone()));
            let in_range = self.next.is_none() || was_listed || loaded_up_to.is_none_or(|last| (chat.updated_at, chat.id.clone()) >= last);
            if in_range {
                self.recent.push(chat);
            }
        }
        sort(&mut self.pinned, &mut self.recent);
    }

    pub fn current(&self) -> Option<&ChatSummary> {
        self.current.as_ref()
    }

    /// Is the list open, given the screen width? Changing between wide and narrow resets
    /// it: open beside the chat on wide screens, closed on phones.
    pub fn is_open(&mut self, narrow: bool) -> bool {
        if self.open.is_none() || narrow != self.was_narrow {
            self.open = Some(!narrow);
            self.was_narrow = narrow;
        }
        self.open.unwrap_or(!narrow)
    }

    pub fn toggle(&mut self) {
        self.open = Some(!self.open.unwrap_or(true));
    }

    /// Draw the list. Returns messages for the engine.
    pub fn show(&mut self, ui: &mut egui::Ui, t: &Tokens, narrow: bool) -> Vec<ClientMsg> {
        let mut out = Vec::new();
        ui.add_space(6.0);
        let new_chat = egui::Button::new(egui::RichText::new("+  New chat").color(t.text_strong)).min_size(egui::vec2(ui.available_width(), 30.0));
        if ui.add(new_chat).clicked() {
            out.push(ClientMsg::NewChat);
            self.after_pick(narrow);
        }
        ui.add_space(6.0);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            if self.pinned.is_empty() && self.recent.is_empty() {
                ui.label(egui::RichText::new("No chats yet").color(t.text_faint));
            }
            if !self.pinned.is_empty() {
                section(ui, t, "Pinned");
                for chat in self.pinned.clone() {
                    self.row(ui, t, &chat, narrow, &mut out);
                }
                ui.add_space(6.0);
                section(ui, t, "Recent");
            }
            for chat in self.recent.clone() {
                self.row(ui, t, &chat, narrow, &mut out);
            }
            if let Some(after) = self.next.clone() {
                ui.add_space(4.0);
                if self.loading {
                    ui.spinner();
                } else if ui.button("Show more").clicked() {
                    self.loading = true;
                    out.push(ClientMsg::ListChats { after: Some(after), limit: PAGE });
                }
            }
        });
        out
    }

    fn after_pick(&mut self, narrow: bool) {
        if narrow {
            self.open = Some(false);
        }
    }

    fn row(&mut self, ui: &mut egui::Ui, t: &Tokens, chat: &ChatSummary, narrow: bool, out: &mut Vec<ClientMsg>) {
        if let Some((_, title)) = self.renaming.as_mut().filter(|(id, _)| *id == chat.id) {
            let edit = ui.add(egui::TextEdit::singleline(title).desired_width(f32::INFINITY));
            if !edit.has_focus() && !edit.lost_focus() {
                edit.request_focus();
            }
            let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
            if edit.lost_focus() {
                if enter && !title.trim().is_empty() {
                    out.push(ClientMsg::RenameChat { id: chat.id.clone(), title: title.clone() });
                }
                self.renaming = None;
            }
            return;
        }
        if self.deleting.as_deref() == Some(chat.id.as_str()) {
            egui::Frame::new().stroke(egui::Stroke::new(1.0, t.danger)).corner_radius(t.radius).inner_margin(6.0).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(egui::RichText::new(format!("Delete \u{201c}{}\u{201d}?", chat.title)).color(t.text));
                ui.horizontal(|ui| {
                    if ui.button(egui::RichText::new("Delete").color(t.danger)).clicked() {
                        out.push(ClientMsg::DeleteChat { id: chat.id.clone() });
                        self.deleting = None;
                    }
                    if ui.button("Cancel").clicked() {
                        self.deleting = None;
                    }
                });
            });
            return;
        }
        let selected = self.current.as_ref().is_some_and(|c| c.id == chat.id);
        let color = if selected { t.text_strong } else { t.text };
        let mut button =
            egui::Button::selectable(selected, egui::RichText::new(&chat.title).color(color)).truncate().min_size(egui::vec2(ui.available_width(), 28.0));
        if chat.busy {
            button = button.right_text(egui::RichText::new("…").color(t.accent));
        }
        let resp = ui.add(button).on_hover_text(&chat.title);
        if resp.clicked() && !selected {
            out.push(ClientMsg::OpenChat { id: chat.id.clone() });
            self.after_pick(narrow);
        }
        resp.context_menu(|ui| {
            if ui.button("Rename").clicked() {
                self.renaming = Some((chat.id.clone(), chat.title.clone()));
                ui.close();
            }
            let pin = if chat.pinned_at.is_some() { "Unpin" } else { "Pin" };
            if ui.button(pin).clicked() {
                out.push(ClientMsg::PinChat { id: chat.id.clone(), pinned: chat.pinned_at.is_none() });
                ui.close();
            }
            if ui.button(egui::RichText::new("Delete…").color(t.danger)).clicked() {
                self.deleting = Some(chat.id.clone());
                ui.close();
            }
        });
    }
}

fn section(ui: &mut egui::Ui, t: &Tokens, name: &str) {
    ui.label(egui::RichText::new(name).small().color(t.text_dim));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(id: &str, updated_at: i64, pinned_at: Option<i64>) -> ChatSummary {
        ChatSummary { id: id.into(), title: id.to_uppercase(), updated_at, pinned_at, busy: false }
    }

    fn ids(list: &[ChatSummary]) -> Vec<&str> {
        list.iter().map(|c| c.id.as_str()).collect()
    }

    #[test]
    fn list_pages_updates_and_deletes() {
        let mut s = Sidebar::default();
        s.apply(&ServerMsg::Chats {
            pinned: vec![chat("p", 1, Some(5))],
            recent: vec![chat("b", 20, None), chat("a", 10, None)],
            after: None,
            next: Some(chat("a", 10, None).cursor()),
        });
        s.apply(&ServerMsg::Chats { pinned: vec![], recent: vec![chat("old", 5, None)], after: Some(chat("a", 10, None).cursor()), next: None });
        assert_eq!(ids(&s.recent), ["b", "a", "old"]);
        // New activity moves a chat to the top; pinning moves it to the pinned list.
        s.apply(&ServerMsg::ChatUpdated(chat("a", 30, None)));
        assert_eq!(ids(&s.recent), ["a", "b", "old"]);
        s.apply(&ServerMsg::ChatUpdated(chat("b", 20, Some(9))));
        assert_eq!((ids(&s.pinned), ids(&s.recent)), (vec!["b", "p"], vec!["a", "old"]));
        s.apply(&ServerMsg::ChatDeleted { id: "old".into() });
        assert_eq!(ids(&s.recent), ["a"]);
        s.apply(&ServerMsg::ChatOpened { chat: Some(chat("a", 30, None)) });
        s.apply(&ServerMsg::ChatUpdated(ChatSummary { title: "Renamed".into(), ..chat("a", 31, None) }));
        assert_eq!(s.current().map(|c| c.title.as_str()), Some("Renamed"));
        assert!(!s.apply(&ServerMsg::EntryDelta { id: 1, content: String::new(), reasoning: String::new() }));
    }

    #[test]
    fn unloaded_old_chats_wait_for_paging() {
        let mut s = Sidebar::default();
        s.apply(&ServerMsg::Chats { pinned: vec![], recent: vec![chat("b", 20, None)], after: None, next: Some(chat("b", 20, None).cursor()) });
        s.apply(&ServerMsg::ChatUpdated(chat("ancient", 1, None)));
        assert_eq!(ids(&s.recent), ["b"]);
    }

    #[test]
    fn narrow_screens_start_closed_and_close_after_picking() {
        let mut s = Sidebar::default();
        assert!(s.is_open(false));
        assert!(!s.is_open(true));
        s.toggle();
        assert!(s.is_open(true));
        s.after_pick(true);
        assert!(!s.is_open(true));
    }

    #[test]
    fn renders_without_clicks() {
        let ctx = egui::Context::default();
        let mut s = Sidebar::default();
        s.apply(&ServerMsg::Chats {
            pinned: vec![chat("p", 1, Some(5))],
            recent: vec![chat("a", 10, None)],
            after: None,
            next: Some(chat("a", 10, None).cursor()),
        });
        s.renaming = Some(("a".into(), "A".into()));
        let mut sent = vec![ClientMsg::Cancel];
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| sent = s.show(ui, &Tokens::dark(), false));
        out.textures_delta.clear();
        assert!(sent.is_empty());
    }
}
