//! The chat list: pinned chats first, then one collapsible group per workspace (most
//! recently active first) with its chats newest first, a "+" per group for a new chat
//! there, and a menu for the workspace's settings. Rename / pin / delete from each chat
//! row's context menu (right click, or long press on touch screens). On narrow screens (a
//! phone over Tailscale) the list replaces the conversation while open. Settings sits at
//! the bottom of the list, where it is easy to find on every screen size.
//!
//! The engine pages the chat list most recent first, across workspaces; groups fill as
//! pages arrive, and a group that has more chats than are loaded offers "Show more".

use std::collections::BTreeSet;

use byob_protocol::{ChatCursor, ChatSummary, ClientMsg, ServerMsg, WorkspaceSummary};

use crate::theme::Tokens;

/// Below this width the sidebar is a full-screen list you toggle, not a side panel.
pub const NARROW: f32 = 720.0;
/// Chats asked for per "Show more".
const PAGE: u32 = 50;

/// Something the list wants the app to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    NewWorkspace,
    /// The workspace's settings (`remove`: at its Remove section).
    WorkspaceSettings {
        id: String,
        remove: bool,
    },
    /// The app settings (connection, defaults, appearance).
    AppSettings,
}

#[derive(Default)]
pub struct Sidebar {
    pinned: Vec<ChatSummary>,
    recent: Vec<ChatSummary>,
    /// Where the next page starts; `None` when everything is loaded.
    next: Option<ChatCursor>,
    loading: bool,
    /// The chat this client looks at (`None`: a new chat not sent yet).
    current: Option<ChatSummary>,
    /// The workspace of the chat in view, or of the new chat about to start.
    current_ws: Option<String>,
    workspaces: Vec<WorkspaceSummary>,
    /// Where new chats go by default.
    default_ws: Option<String>,
    collapsed: BTreeSet<String>,
    /// Chat being renamed, with the edited title.
    renaming: Option<(String, String)>,
    /// Chat waiting for delete confirmation.
    deleting: Option<String>,
    /// Workspace being renamed, with the edited name.
    renaming_ws: Option<(String, String)>,
    requests: Vec<Request>,
    /// Shown? `None` until the first frame decides from the width.
    open: Option<bool>,
    was_narrow: bool,
}

/// One workspace's part of the list.
#[derive(Debug)]
pub struct Group<'a> {
    /// `None`: chats whose workspace isn't known (yet).
    pub workspace: Option<&'a WorkspaceSummary>,
    pub chats: Vec<&'a ChatSummary>,
    /// Latest activity: its newest chat, or the workspace's own.
    pub latest: i64,
}

impl Group<'_> {
    pub fn id(&self) -> Option<&str> {
        self.workspace.map(|w| w.id.as_str())
    }
}

/// Group `recent` chats by workspace; groups ordered by latest activity, chats keep their
/// (newest first) order. Workspaces without loaded chats still get a group.
pub fn groups<'a>(workspaces: &'a [WorkspaceSummary], recent: &'a [ChatSummary]) -> Vec<Group<'a>> {
    let mut out: Vec<Group<'a>> = workspaces.iter().map(|w| Group { workspace: Some(w), chats: Vec::new(), latest: w.updated_at }).collect();
    let mut other = Group { workspace: None, chats: Vec::new(), latest: i64::MIN };
    for c in recent {
        let slot = c.workspace.as_deref().and_then(|id| out.iter_mut().find(|g| g.id() == Some(id)));
        let g = slot.unwrap_or(&mut other);
        g.latest = g.latest.max(c.updated_at);
        g.chats.push(c);
    }
    out.sort_by(|a, b| b.latest.cmp(&a.latest).then_with(|| a.workspace.map(|w| &w.name).cmp(&b.workspace.map(|w| &w.name))));
    if !other.chats.is_empty() {
        out.push(other);
    }
    out
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
            ServerMsg::ChatOpened { chat, workspace } => {
                self.current.clone_from(chat);
                self.current_ws.clone_from(workspace);
            }
            ServerMsg::Workspaces { items, current, .. } => {
                self.workspaces.clone_from(items);
                self.default_ws.clone_from(current);
                let ids: BTreeSet<&String> = items.iter().map(|w| &w.id).collect();
                self.collapsed.retain(|id| ids.contains(id));
            }
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

    /// The workspace of the chat in view (or of the new chat), if known.
    pub fn current_workspace(&self) -> Option<&WorkspaceSummary> {
        let id = self.current_ws.as_deref()?;
        self.workspaces.iter().find(|w| w.id == id)
    }

    pub fn workspace(&self, id: &str) -> Option<&WorkspaceSummary> {
        self.workspaces.iter().find(|w| w.id == id)
    }

    pub fn workspaces(&self) -> &[WorkspaceSummary] {
        &self.workspaces
    }

    /// What the list asked the app to open since the last call.
    pub fn take_requests(&mut self) -> Vec<Request> {
        std::mem::take(&mut self.requests)
    }

    /// Tests: as if the list asked for this.
    #[cfg(test)]
    pub(crate) fn request(&mut self, r: Request) {
        self.requests.push(r);
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
        // Settings at the bottom, always visible below the scrolling list.
        egui::Panel::bottom("sidebar-footer").frame(egui::Frame::NONE.inner_margin(egui::Margin::symmetric(0, 6))).show(ui, |ui| {
            let button = crate::icons::icon_button(ui, t, crate::icons::gear, Some("Settings"), false).on_hover_text(crate::header::settings_tip(ui.ctx()));
            if button.clicked() {
                self.requests.push(Request::AppSettings);
                self.after_pick(narrow);
            }
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let ws = ui.button(egui::RichText::new("+ Workspace").color(t.text)).on_hover_text("New workspace: a folder with its own settings and environment");
            if ws.clicked() {
                self.requests.push(Request::NewWorkspace);
                self.after_pick(narrow);
            }
            let new_chat = egui::Button::new(egui::RichText::new("+  New chat").color(t.text_strong)).min_size(egui::vec2(ui.available_width(), 28.0));
            let tip = match self.current_workspace() {
                Some(w) => format!("New chat in {}", w.name),
                None => "New chat".into(),
            };
            if ui.add(new_chat).on_hover_text(tip).clicked() {
                out.push(ClientMsg::NewChat { workspace: None });
                self.after_pick(narrow);
            }
        });
        ui.add_space(6.0);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            if self.pinned.is_empty() && self.recent.is_empty() && self.workspaces.is_empty() {
                ui.label(egui::RichText::new("No chats yet").color(t.text_faint));
            }
            if !self.pinned.is_empty() {
                section(ui, t, "Pinned");
                for chat in self.pinned.clone() {
                    self.row(ui, t, &chat, narrow, &mut out);
                }
                ui.add_space(8.0);
            }
            let workspaces = self.workspaces.clone();
            let recent = self.recent.clone();
            for g in groups(&workspaces, &recent) {
                self.group(ui, t, &g, narrow, &mut out);
            }
            if let Some(after) = self.next.clone() {
                ui.add_space(4.0);
                if self.loading {
                    crate::theme::spinner(ui, t, None);
                } else if ui.button("Show more").clicked() {
                    self.loading = true;
                    out.push(ClientMsg::ListChats { after: Some(after), limit: PAGE });
                }
            }
        });
        out
    }

    fn group(&mut self, ui: &mut egui::Ui, t: &Tokens, g: &Group<'_>, narrow: bool, out: &mut Vec<ClientMsg>) {
        let key = g.id().unwrap_or("").to_owned();
        let collapsed = self.collapsed.contains(&key);
        let current = g.id().is_some() && g.id() == self.current_ws.as_deref();
        if let Some(ws) = g.workspace {
            self.group_header(ui, t, ws, collapsed, current, narrow, out);
        } else {
            section(ui, t, "Other chats");
        }
        if collapsed {
            ui.add_space(2.0);
            return;
        }
        ui.indent(("ws", key.as_str()), |ui| {
            // The new chat about to start here.
            if self.current.is_none() && current {
                let pending = egui::Button::selectable(true, egui::RichText::new("New chat").italics().color(t.text_strong))
                    .min_size(egui::vec2(ui.available_width(), 26.0));
                ui.add(pending).on_hover_text("Starts when you send the first message");
            }
            for chat in &g.chats {
                self.row(ui, t, chat, narrow, out);
            }
            let total = g.workspace.map_or(0, |w| w.chats);
            let shown = u64::try_from(g.chats.len()).unwrap_or(u64::MAX);
            if g.chats.is_empty() && !(self.current.is_none() && current) && total == 0 {
                ui.label(egui::RichText::new("No chats yet").small().color(t.text_faint));
            } else if total > shown
                && let Some(after) = self.next.clone()
                && !self.loading
            {
                let more = format!("{} more…", total - shown);
                if ui.add(egui::Button::new(egui::RichText::new(more).small().color(t.text_dim)).frame(false)).clicked() {
                    self.loading = true;
                    out.push(ClientMsg::ListChats { after: Some(after), limit: PAGE });
                }
            }
        });
        ui.add_space(6.0);
    }

    #[allow(clippy::too_many_arguments)] // one row of the list
    fn group_header(&mut self, ui: &mut egui::Ui, t: &Tokens, ws: &WorkspaceSummary, collapsed: bool, current: bool, narrow: bool, out: &mut Vec<ClientMsg>) {
        if let Some((_, name)) = self.renaming_ws.as_mut().filter(|(id, _)| *id == ws.id) {
            let edit = ui.add(egui::TextEdit::singleline(name).desired_width(f32::INFINITY));
            if !edit.has_focus() && !edit.lost_focus() {
                edit.request_focus();
            }
            let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
            if edit.lost_focus() {
                if enter && !name.trim().is_empty() {
                    out.push(ClientMsg::RenameWorkspace { id: ws.id.clone(), name: name.clone() });
                }
                self.renaming_ws = None;
            }
            return;
        }
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                ui.menu_button(egui::RichText::new("…").color(t.text_dim), |ui| self.workspace_menu(ui, t, ws, narrow, out))
                    .response
                    .on_hover_text("Workspace menu: settings, rename, remove");
                let plus = ui.add(egui::Button::new(egui::RichText::new("+").color(t.text_dim)).frame(false)).on_hover_text(format!("New chat in {}", ws.name));
                if plus.clicked() {
                    out.push(ClientMsg::NewChat { workspace: Some(ws.id.clone()) });
                    self.after_pick(narrow);
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    let (arrow, arrow_resp) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::click());
                    crate::icons::disclosure(ui.painter(), arrow, !collapsed, t.text_dim);
                    crate::icons::env_icon(ui, t, ws.env, t.text_size(14.0));
                    ui.add_space(2.0);
                    let color = if current { t.text_strong } else { t.text };
                    let mut text = egui::text::LayoutJob::default();
                    text.append(
                        &ws.name,
                        0.0,
                        egui::TextFormat {
                            font_id: egui::FontId::new(t.text_size(13.0), crate::theme::bold_or_regular(ui.ctx())),
                            color,
                            ..Default::default()
                        },
                    );
                    let header =
                        ui.add(egui::Label::new(text).truncate().sense(egui::Sense::click())).on_hover_text(format!("{}\n{} chat(s)", ws.folder, ws.chats));
                    if header.clicked() || arrow_resp.clicked() {
                        if collapsed {
                            self.collapsed.remove(&ws.id);
                        } else {
                            self.collapsed.insert(ws.id.clone());
                        }
                    }
                    header.context_menu(|ui| self.workspace_menu(ui, t, ws, narrow, out));
                });
            });
        });
    }

    fn workspace_menu(&mut self, ui: &mut egui::Ui, t: &Tokens, ws: &WorkspaceSummary, narrow: bool, out: &mut Vec<ClientMsg>) {
        if ui.button("New chat here").clicked() {
            out.push(ClientMsg::NewChat { workspace: Some(ws.id.clone()) });
            self.after_pick(narrow);
            ui.close();
        }
        if ui.button("Workspace settings…").clicked() {
            self.requests.push(Request::WorkspaceSettings { id: ws.id.clone(), remove: false });
            ui.close();
        }
        if ui.button("Rename").clicked() {
            self.renaming_ws = Some((ws.id.clone(), ws.name.clone()));
            ui.close();
        }
        if ui.button(egui::RichText::new("Remove…").color(t.danger)).clicked() {
            self.requests.push(Request::WorkspaceSettings { id: ws.id.clone(), remove: true });
            ui.close();
        }
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
            egui::Button::selectable(selected, egui::RichText::new(&chat.title).color(color)).truncate().min_size(egui::vec2(ui.available_width(), 26.0));
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
    use byob_protocol::workspace::EnvKind;

    fn chat(id: &str, updated_at: i64, pinned_at: Option<i64>) -> ChatSummary {
        ChatSummary { id: id.into(), title: id.to_uppercase(), updated_at, pinned_at, busy: false, workspace: None }
    }

    fn in_ws(id: &str, updated_at: i64, ws: &str) -> ChatSummary {
        ChatSummary { workspace: Some(ws.into()), ..chat(id, updated_at, None) }
    }

    fn ws(id: &str, updated_at: i64, chats: u64) -> WorkspaceSummary {
        WorkspaceSummary { id: id.into(), name: id.to_uppercase(), folder: format!("/{id}"), chats, updated_at, env: EnvKind::Auto }
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
        s.apply(&ServerMsg::ChatOpened { chat: Some(chat("a", 30, None)), workspace: None });
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
    fn chats_group_by_workspace_most_recent_group_first() {
        let workspaces = vec![ws("w1", 5, 2), ws("w2", 1, 1), ws("empty", 50, 0)];
        let recent = vec![in_ws("c", 40, "w2"), in_ws("a", 30, "w1"), chat("loose", 20, None), in_ws("b", 10, "w1"), in_ws("gone", 5, "w-removed")];
        let g = groups(&workspaces, &recent);
        let shape: Vec<(Option<&str>, Vec<&str>)> = g.iter().map(|g| (g.id(), g.chats.iter().map(|c| c.id.as_str()).collect())).collect();
        assert_eq!(
            shape,
            vec![(Some("empty"), vec![]), (Some("w2"), vec!["c"]), (Some("w1"), vec!["a", "b"]), (None, vec!["loose", "gone"])],
            "a new empty workspace sorts by when it was made; unknown workspaces fall into Other"
        );
        // Activity reorders groups.
        let recent = vec![in_ws("a", 60, "w1"), in_ws("c", 40, "w2")];
        assert_eq!(groups(&workspaces, &recent).first().and_then(Group::id), Some("w1"));
        assert!(groups(&[], &[]).is_empty());
    }

    #[test]
    fn workspaces_and_the_current_one_are_tracked() {
        let mut s = Sidebar::default();
        let items = vec![ws("w1", 5, 2), ws("w2", 1, 1)];
        s.apply(&ServerMsg::Workspaces { items, current: Some("w1".into()), root: String::new(), support: Default::default() });
        s.collapsed.insert("w2".into());
        s.apply(&ServerMsg::ChatOpened { chat: None, workspace: Some("w2".into()) });
        assert_eq!(s.current_workspace().map(|w| w.name.as_str()), Some("W2"));
        // A removed workspace is forgotten, collapsed state included.
        s.apply(&ServerMsg::Workspaces { items: vec![ws("w1", 5, 2)], current: None, root: String::new(), support: Default::default() });
        assert!(s.current_workspace().is_none() && s.collapsed.is_empty());
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
        s.apply(&ServerMsg::Workspaces { items: vec![ws("w1", 5, 3)], current: Some("w1".into()), root: String::new(), support: Default::default() });
        s.apply(&ServerMsg::Chats {
            pinned: vec![chat("p", 1, Some(5))],
            recent: vec![in_ws("a", 10, "w1"), chat("x", 3, None)],
            after: None,
            next: Some(chat("x", 3, None).cursor()),
        });
        s.apply(&ServerMsg::ChatOpened { chat: None, workspace: Some("w1".into()) });
        s.renaming = Some(("a".into(), "A".into()));
        let mut sent = vec![ClientMsg::Cancel];
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| sent = s.show(ui, &Tokens::dark(), false));
        out.textures_delta.clear();
        assert!(sent.is_empty());
        assert!(s.take_requests().is_empty());
    }
}
