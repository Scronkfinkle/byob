//! The chat list. Top to bottom:
//!
//! - **New chat** (Ctrl/Cmd+N; Ctrl/Cmd+Shift+O in the browser, where Ctrl+N is the
//!   browser's): a new chat in the workspace in view. Its caret opens "New workspace…" and
//!   "New chat in" any workspace.
//! - **Search** (Ctrl/Cmd+F): filters the loaded chats by title and workspace name in
//!   place; Enter opens the first match, Esc clears.
//! - Pinned chats, then **Workspaces** (a "+" there makes one): one collapsible group per
//!   workspace, most recently active first, its chats newest first. A group shows its
//!   [`RECENT`] newest chats and "Show all (k)". Collapsed groups are remembered
//!   ([`SidebarPrefs`]).
//! - Chats outside any workspace, under "Other chats".
//! - Settings, pinned at the bottom.
//!
//! Row actions appear on hover only, in space every row keeps for them so titles never
//! move: a workspace row shows "+" (new chat there) and "⋯" (its menu); a chat row "⋯". A
//! right click (a long press on touch screens) opens the same menus. On narrow screens (a
//! phone over Tailscale) the actions always show, and the list replaces the conversation
//! while open.
//!
//! The engine pages the chat list most recent first, across workspaces; groups fill as
//! pages arrive, and "Show all" pages on until its group is complete.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use byob_protocol::{ChatCursor, ChatSummary, ClientMsg, ServerMsg, WorkspaceSummary};
use egui::{Galley, Id, Key, KeyboardShortcut, Modifiers, Rect, Sense, Ui, pos2, vec2};
use serde::{Deserialize, Serialize};

use crate::icons;
use crate::theme::Tokens;

/// Below this width the sidebar is a full-screen list you toggle, not a side panel.
pub const NARROW: f32 = 720.0;
/// Chats asked for per page.
const PAGE: u32 = 50;
/// Chats a workspace group shows before "Show all".
pub const RECENT: usize = 5;

/// The top controls' height (New chat, search).
const TOP_H: f32 = 30.0;
/// A list row's height (at least: big text makes it taller).
const ROW_H: f32 = 26.0;
/// A section heading's height ("Pinned", "Workspaces").
const SECTION_H: f32 = 24.0;
/// One hover action's square.
const ACTION_W: f32 = 20.0;
/// Inside a row: the left padding, the disclosure chevron and the gap after it. Chat
/// titles start where their workspace's icon does.
const PAD: f32 = 6.0;
const CHEVRON: f32 = 12.0;
const GAP: f32 = 4.0;
const ICON: f32 = 14.0;
const INDENT: f32 = PAD + CHEVRON + GAP;
/// The New chat button's caret part.
const CARET_W: f32 = 26.0;

/// New chat, on the desktop.
pub const NEW_CHAT_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::N);
/// New chat anywhere, and the only one in the browser (Ctrl+N opens a browser window and
/// pages can't have it; eframe keeps Ctrl+O combinations for the page).
pub const NEW_CHAT_SHORTCUT_ALT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::O);
/// Focus the chat search.
pub const SEARCH_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::F);

/// The New chat shortcut shown on this platform.
pub fn new_chat_shortcut() -> KeyboardShortcut {
    if cfg!(target_arch = "wasm32") { NEW_CHAT_SHORTCUT_ALT } else { NEW_CHAT_SHORTCUT }
}

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

/// What the list remembers per device: collapsed workspaces, and the app's unsent drafts
/// (see `ByobApp::sidebar_prefs`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SidebarPrefs {
    pub collapsed: BTreeSet<String>,
    /// Unsent composer text by chat (`chat:<id>`) or new chat (`new:<workspace id>`).
    pub drafts: BTreeMap<String, String>,
}

impl SidebarPrefs {
    /// Where apps store them (eframe storage / the browser's localStorage).
    pub const STORAGE_KEY: &'static str = "byob.sidebar";
}

/// Where rows were drawn last frame (tests, scripted runs).
#[derive(Debug, Clone)]
pub struct Rects {
    pub new_chat: Rect,
    pub new_chat_caret: Rect,
    pub search: Rect,
    /// Where each title was drawn: `ws:<id>`, `chat:<id>`.
    pub titles: BTreeMap<String, Rect>,
    /// The first hover action of each row that shows its actions.
    pub actions: BTreeMap<String, Rect>,
}

impl Default for Rects {
    fn default() -> Self {
        Self { new_chat: Rect::NOTHING, new_chat_caret: Rect::NOTHING, search: Rect::NOTHING, titles: BTreeMap::new(), actions: BTreeMap::new() }
    }
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
    /// Groups showing all their chats (`""`: Other chats).
    expanded: BTreeSet<String>,
    /// The search box.
    search: String,
    focus_search: bool,
    /// The first chat the search matched last frame (Enter opens it).
    first_match: Option<String>,
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
    /// Scripted runs: draw the current workspace's and chat's rows as if hovered, open
    /// the current workspace's menu, open the New chat menu.
    force_hover: bool,
    open_ws_menu: bool,
    open_new_menu: bool,
    pub rects: Rects,
}

/// One workspace's part of the list.
#[derive(Debug)]
pub struct Group<'a> {
    /// `None`: chats whose workspace isn't known (yet), or that have none.
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

/// The search, normalised: trimmed and lower case (`None`: no search).
fn query(search: &str) -> Option<String> {
    Some(search.trim().to_lowercase()).filter(|q| !q.is_empty())
}

/// Does a chat match the search `q` (lower case): its title, or its workspace's name?
pub fn matches(title: &str, workspace: Option<&str>, q: &str) -> bool {
    title.to_lowercase().contains(q) || workspace.is_some_and(|w| w.to_lowercase().contains(q))
}

/// The chats a group shows: all when `all`, else the [`RECENT`] newest plus `current` if it
/// is further down. Returns them and how many are left out.
fn visible<'a>(chats: &[&'a ChatSummary], all: bool, current: Option<&str>) -> (Vec<&'a ChatSummary>, usize) {
    if all || chats.len() <= RECENT {
        return (chats.to_vec(), 0);
    }
    let mut shown: Vec<&ChatSummary> = chats.iter().take(RECENT).copied().collect();
    if let Some(c) = chats.iter().skip(RECENT).find(|c| Some(c.id.as_str()) == current) {
        shown.push(c);
    }
    let hidden = chats.len() - shown.len();
    (shown, hidden)
}

fn sort(pinned: &mut [ChatSummary], recent: &mut [ChatSummary]) {
    pinned.sort_by(|a, b| b.pinned_at.cmp(&a.pinned_at).then_with(|| b.id.cmp(&a.id)));
    recent.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| b.id.cmp(&a.id)));
}

/// A title laid out on one line, cut with "…" to `width`, with the part matching `q` (lower
/// case) in the accent colour.
fn title_galley(ui: &Ui, text: &str, font: egui::FontId, color: egui::Color32, width: f32, q: Option<&str>, accent: egui::Color32) -> Arc<Galley> {
    use egui::text::{LayoutJob, TextFormat, TextWrapping};
    let fmt = |color| TextFormat { font_id: font.clone(), color, ..Default::default() };
    let mut job = LayoutJob::default();
    // Highlight only where lower-casing keeps byte positions (ASCII-like titles).
    let lower = text.to_lowercase();
    let hit = q.filter(|_| lower.len() == text.len()).and_then(|q| lower.find(q).map(|i| (i, i + q.len())));
    match hit.filter(|(a, b)| text.is_char_boundary(*a) && text.is_char_boundary(*b)) {
        Some((a, b)) => {
            job.append(&text[..a], 0.0, fmt(color));
            job.append(&text[a..b], 0.0, fmt(accent));
            job.append(&text[b..], 0.0, fmt(color));
        }
        None => job.append(text, 0.0, fmt(color)),
    }
    job.wrap = TextWrapping::truncate_at_width(width.max(1.0));
    ui.painter().layout_job(job)
}

/// A row's height for the current text size.
fn row_height(ui: &Ui) -> f32 {
    (ui.text_style_height(&egui::TextStyle::Body) + 9.0).max(ROW_H).round()
}

/// A hover action: an icon in a square that lights up under the pointer. Drawn only when
/// `shown` (in `idle` until the pointer is on it); always takes its click area so the row's
/// layout never changes.
fn action(ui: &mut Ui, t: &Tokens, rect: Rect, id: Id, paint: icons::Paint, shown: bool, idle: egui::Color32) -> egui::Response {
    let resp = ui.interact(rect, id, Sense::click());
    if shown && ui.is_rect_visible(rect) {
        let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
        if resp.hovered() || open {
            ui.painter().rect_filled(rect.shrink(1.0), t.round(4.0), if resp.is_pointer_button_down_on() { t.pressed } else { t.pressed.gamma_multiply(0.8) });
        }
        let color = if resp.hovered() || open { t.text_strong } else { idle };
        paint(ui.painter(), Rect::from_center_size(rect.center(), vec2(ICON, ICON)), color);
    }
    resp
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
                self.expanded.retain(|id| id.is_empty() || ids.contains(id));
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

    /// New chat: in the workspace in view (of the chat open, or of the new chat about to
    /// start), else where the engine puts new chats.
    pub fn new_chat_msg(&self) -> ClientMsg {
        ClientMsg::NewChat { workspace: self.current_workspace().map(|w| w.id.clone()) }
    }

    /// Start a new chat (the button, the shortcut): closes the list on phones.
    pub fn new_chat(&mut self, narrow: bool) -> ClientMsg {
        self.after_pick(narrow);
        self.new_chat_msg()
    }

    /// Put the keyboard in the search box (opening the list if it is closed).
    pub fn focus_search(&mut self) {
        self.open = Some(true);
        self.focus_search = true;
    }

    /// Scripted runs: type into the search box.
    pub fn set_search(&mut self, text: &str) {
        text.clone_into(&mut self.search);
    }

    /// Scripted runs: show the hover state of the current workspace and chat rows.
    pub fn show_hover(&mut self) {
        self.force_hover = true;
    }

    /// Scripted runs: open the current workspace's "⋯" menu (false if there is none).
    pub fn open_workspace_menu(&mut self) -> bool {
        self.force_hover = true;
        self.open_ws_menu = self.current_workspace().is_some();
        self.open_ws_menu
    }

    /// Scripted runs: open the New chat button's menu.
    pub fn open_new_menu(&mut self) {
        self.open_new_menu = true;
    }

    /// What to remember on this device (drafts are the app's: empty here).
    pub fn prefs(&self) -> SidebarPrefs {
        SidebarPrefs { collapsed: self.collapsed.clone(), drafts: BTreeMap::new() }
    }

    pub fn set_prefs(&mut self, prefs: &SidebarPrefs) {
        self.collapsed.clone_from(&prefs.collapsed);
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

    /// Open or close the list (scripted runs).
    pub fn set_open(&mut self, open: bool) {
        self.open = Some(open);
    }

    pub fn toggle(&mut self) {
        self.open = Some(!self.open.unwrap_or(true));
    }

    fn toggle_collapsed(&mut self, id: &str) {
        if !self.collapsed.remove(id) {
            self.collapsed.insert(id.to_owned());
        }
    }

    /// Draw the list. Returns messages for the engine.
    pub fn show(&mut self, ui: &mut Ui, t: &Tokens, narrow: bool) -> Vec<ClientMsg> {
        let mut out = Vec::new();
        self.rects = Rects::default();
        // Settings at the bottom, always visible below the scrolling list.
        egui::Panel::bottom("sidebar-footer").frame(egui::Frame::NONE.inner_margin(egui::Margin::symmetric(0, 6))).show(ui, |ui| {
            let button = icons::icon_button(ui, t, icons::gear, Some("Settings"), false).on_hover_text(crate::header::settings_tip(ui.ctx()));
            if button.clicked() {
                self.requests.push(Request::AppSettings);
                self.after_pick(narrow);
            }
        });
        ui.add_space(6.0);
        self.new_chat_button(ui, t, narrow, &mut out);
        ui.add_space(6.0);
        self.search_box(ui, t, narrow, &mut out);
        ui.add_space(6.0);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            match query(&self.search) {
                Some(q) => self.results(ui, t, &q, narrow, &mut out),
                None => self.tree(ui, t, narrow, &mut out),
            }
        });
        out
    }

    /// One wide button, its caret part a menu: New workspace…, New chat in ….
    fn new_chat_button(&mut self, ui: &mut Ui, t: &Tokens, narrow: bool, out: &mut Vec<ClientMsg>) {
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), TOP_H), Sense::hover());
        let main_rect = Rect::from_min_max(rect.min, pos2(rect.right() - CARET_W, rect.bottom()));
        let caret_rect = Rect::from_min_max(pos2(main_rect.right(), rect.top()), rect.max);
        let main = ui.interact(main_rect, Id::unique("sidebar-new-chat"), Sense::click());
        let caret = ui.interact(caret_rect, Id::unique("sidebar-new-chat-caret"), Sense::click());
        self.rects.new_chat = main_rect;
        self.rects.new_chat_caret = caret_rect;
        let menu_open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&caret));
        if ui.is_rect_visible(rect) {
            let p = ui.painter();
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // a few points
            let r = t.round(6.0).round().clamp(0.0, 255.0) as u8;
            p.rect_filled(rect, r, t.button_bg);
            let part = |resp: &egui::Response, on: bool| {
                if resp.is_pointer_button_down_on() {
                    Some(t.pressed)
                } else if resp.hovered() || on {
                    Some(t.hover)
                } else {
                    None
                }
            };
            if let Some(fill) = part(&main, false) {
                p.rect_filled(main_rect, egui::CornerRadius { nw: r, sw: r, ne: 0, se: 0 }, fill);
            }
            if let Some(fill) = part(&caret, menu_open) {
                p.rect_filled(caret_rect, egui::CornerRadius { nw: 0, sw: 0, ne: r, se: r }, fill);
            }
            p.rect_stroke(rect, r, egui::Stroke::new(1.0, t.border), egui::StrokeKind::Inside);
            p.line_segment([pos2(caret_rect.left(), rect.top() + 7.0), pos2(caret_rect.left(), rect.bottom() - 7.0)], egui::Stroke::new(1.0, t.border));
            let icon = Rect::from_center_size(pos2(main_rect.left() + 8.0 + ICON / 2.0, rect.center().y), vec2(ICON, ICON));
            icons::plus(p, icon, t.text_strong);
            let font = egui::TextStyle::Button.resolve(ui.style());
            let label = p.layout_no_wrap("New chat".into(), egui::FontId::new(font.size, crate::theme::bold_or_regular(ui.ctx())), t.text_strong);
            let label_pos = pos2(icon.right() + 8.0, rect.center().y - label.size().y / 2.0);
            let label_right = label_pos.x + label.size().x;
            p.galley(label_pos, label, t.text_strong);
            let keys = p.layout_no_wrap(ui.ctx().format_shortcut(&new_chat_shortcut()), egui::TextStyle::Small.resolve(ui.style()), t.text_faint);
            // The keys as the layout says (always, or only while the pointer is on it).
            if !narrow && crate::layout::visible(ui.ctx(), "sidebar.shortcuts", main.hovered()) && label_right + 12.0 + keys.size().x < main_rect.right() - 8.0
            {
                p.galley(pos2(main_rect.right() - 8.0 - keys.size().x, rect.center().y - keys.size().y / 2.0), keys, t.text_faint);
            }
            icons::chevron_down(
                p,
                Rect::from_center_size(caret_rect.center(), vec2(ICON, ICON)),
                if caret.hovered() || menu_open { t.text_strong } else { t.text_dim },
            );
        }
        let tip = match self.current_workspace() {
            Some(w) => format!("New chat in {} ({})", w.name, ui.ctx().format_shortcut(&new_chat_shortcut())),
            None => format!("New chat ({})", ui.ctx().format_shortcut(&new_chat_shortcut())),
        };
        if main.on_hover_text(tip).clicked() {
            out.push(self.new_chat(narrow));
        }
        if std::mem::take(&mut self.open_new_menu) {
            egui::Popup::open_id(ui.ctx(), egui::Popup::default_response_id(&caret));
        }
        let caret = caret.on_hover_text("New workspace, or a new chat in another workspace");
        egui::Popup::menu(&caret).align(egui::RectAlign::BOTTOM_END).show(|ui| {
            ui.set_min_width(220.0);
            if ui.button("New workspace…").clicked() {
                self.requests.push(Request::NewWorkspace);
                self.after_pick(narrow);
                ui.close();
            }
            if !self.workspaces.is_empty() {
                ui.separator();
                ui.label(egui::RichText::new("New chat in").small().color(t.text_faint));
                let workspaces = self.workspaces.clone();
                for w in workspaces.iter().take(12) {
                    let here = self.current_ws.as_deref() == Some(w.id.as_str());
                    let label = if here { format!("{}  (this one)", w.name) } else { w.name.clone() };
                    if ui.add(egui::Button::new(egui::RichText::new(label).color(t.text)).truncate()).clicked() {
                        out.push(ClientMsg::NewChat { workspace: Some(w.id.clone()) });
                        self.after_pick(narrow);
                        ui.close();
                    }
                }
            }
        });
    }

    /// Filters the list as you type; Enter opens the first match, Esc clears.
    fn search_box(&mut self, ui: &mut Ui, t: &Tokens, narrow: bool, out: &mut Vec<ClientMsg>) {
        let id = Id::unique("sidebar-search");
        if std::mem::take(&mut self.focus_search) {
            ui.memory_mut(|m| m.request_focus(id));
        }
        let w = ui.available_width();
        let edit = egui::TextEdit::singleline(&mut self.search)
            .id(id)
            .hint_text(egui::RichText::new("Search chats").color(t.text_faint))
            .margin(egui::Margin { left: 28, right: 26, top: 0, bottom: 0 })
            .align(egui::Align2::LEFT_CENTER)
            .min_size(vec2(w, TOP_H))
            .desired_width(w);
        let resp = ui.add(edit);
        self.rects.search = resp.rect;
        let (esc, enter) = ui.input(|i| (i.key_pressed(Key::Escape), i.key_pressed(Key::Enter)));
        if resp.lost_focus() && esc {
            self.search.clear();
        }
        if resp.lost_focus()
            && enter
            && let Some(chat) = self.first_match.clone()
        {
            self.search.clear();
            if self.current.as_ref().is_none_or(|c| c.id != chat) {
                out.push(ClientMsg::OpenChat { id: chat });
            }
            self.after_pick(narrow);
        }
        let p = ui.painter();
        let c = resp.rect.left_center() + vec2(8.0 + ICON / 2.0, 0.0);
        icons::search(p, Rect::from_center_size(c, vec2(ICON, ICON)), if resp.has_focus() { t.text } else { t.text_faint });
        if self.search.is_empty() {
            if !resp.has_focus() && !narrow && crate::layout::visible(ui.ctx(), "sidebar.shortcuts", resp.hovered()) {
                let keys = p.layout_no_wrap(ui.ctx().format_shortcut(&SEARCH_SHORTCUT), egui::TextStyle::Small.resolve(ui.style()), t.text_faint);
                p.galley(pos2(resp.rect.right() - 8.0 - keys.size().x, resp.rect.center().y - keys.size().y / 2.0), keys, t.text_faint);
            }
        } else {
            let clear = Rect::from_center_size(resp.rect.right_center() - vec2(4.0 + ACTION_W / 2.0, 0.0), vec2(ACTION_W - 4.0, ACTION_W - 4.0));
            let x = action(ui, t, clear, id.with("clear"), icons::cross, true, t.text_dim).on_hover_text("Clear the search (Esc)");
            if x.clicked() {
                self.search.clear();
            }
        }
    }

    /// The list without a search: pinned, workspaces, other chats.
    fn tree(&mut self, ui: &mut Ui, t: &Tokens, narrow: bool, out: &mut Vec<ClientMsg>) {
        self.first_match = None;
        if !self.pinned.is_empty() {
            section(ui, t, "Pinned", None);
            for chat in self.pinned.clone() {
                self.chat_row(ui, t, &chat, PAD, None, narrow, out);
            }
            ui.add_space(8.0);
        }
        if section(ui, t, "Workspaces", Some("New workspace: a folder byob works in, with its own settings")) {
            self.requests.push(Request::NewWorkspace);
            self.after_pick(narrow);
        }
        let workspaces = self.workspaces.clone();
        let recent = self.recent.clone();
        let all = groups(&workspaces, &recent);
        if workspaces.is_empty() {
            hint(ui, t, "None yet: + makes one from a folder", PAD);
        }
        for g in all.iter().filter(|g| g.workspace.is_some()) {
            self.group(ui, t, g, narrow, out);
        }
        if let Some(other) = all.iter().find(|g| g.workspace.is_none()) {
            ui.add_space(6.0);
            section(ui, t, "Other chats", None);
            let all_shown = self.expanded.contains("");
            let current = self.current.as_ref().map(|c| c.id.clone());
            let (shown, hidden) = visible(&other.chats, all_shown, current.as_deref());
            for chat in shown {
                self.chat_row(ui, t, chat, PAD, None, narrow, out);
            }
            self.more_row(ui, t, "", hidden, other.chats.len(), all_shown, PAD);
        }
        if self.pinned.is_empty() && self.recent.is_empty() && self.workspaces.is_empty() {
            hint(ui, t, "No chats yet", PAD);
        }
        if let Some(after) = self.next.clone() {
            ui.add_space(6.0);
            if self.loading {
                crate::theme::spinner(ui, t, None);
            } else if link_row(ui, t, "Load older chats", PAD).clicked() {
                self.loading = true;
                out.push(ClientMsg::ListChats { after: Some(after), limit: PAGE });
            }
        }
    }

    /// The list filtered by `q` (lower case): every match, groups open.
    fn results(&mut self, ui: &mut Ui, t: &Tokens, q: &str, narrow: bool, out: &mut Vec<ClientMsg>) {
        let mut first = None;
        let pinned: Vec<ChatSummary> = self
            .pinned
            .iter()
            .filter(|c| matches(&c.title, c.workspace.as_deref().and_then(|w| self.workspace(w)).map(|w| w.name.as_str()), q))
            .cloned()
            .collect();
        if !pinned.is_empty() {
            section(ui, t, "Pinned", None);
            for chat in &pinned {
                first.get_or_insert_with(|| chat.id.clone());
                self.chat_row(ui, t, chat, PAD, Some(q), narrow, out);
            }
            ui.add_space(8.0);
        }
        let workspaces = self.workspaces.clone();
        let recent = self.recent.clone();
        let mut any = !pinned.is_empty();
        for g in groups(&workspaces, &recent) {
            let name = g.workspace.map(|w| w.name.as_str());
            let chats: Vec<&ChatSummary> = g.chats.iter().copied().filter(|c| matches(&c.title, name, q)).collect();
            let named = name.is_some_and(|n| n.to_lowercase().contains(q));
            if chats.is_empty() && !named {
                continue;
            }
            any = true;
            let indent = match g.workspace {
                Some(ws) => {
                    self.workspace_row(ui, t, ws, false, Some(q), narrow, out);
                    INDENT
                }
                None => {
                    ui.add_space(6.0);
                    section(ui, t, "Other chats", None);
                    PAD
                }
            };
            for chat in chats {
                first.get_or_insert_with(|| chat.id.clone());
                self.chat_row(ui, t, chat, indent, Some(q), narrow, out);
            }
            ui.add_space(4.0);
        }
        if !any {
            hint(ui, t, &format!("No chats match \u{201c}{}\u{201d}", self.search.trim()), PAD);
        }
        if let Some(after) = self.next.clone() {
            ui.add_space(6.0);
            if self.loading {
                crate::theme::spinner(ui, t, None);
            } else if link_row(ui, t, "Search older chats too", PAD).clicked() {
                self.loading = true;
                out.push(ClientMsg::ListChats { after: Some(after), limit: PAGE });
            }
        }
        self.first_match = first;
    }

    fn group(&mut self, ui: &mut Ui, t: &Tokens, g: &Group<'_>, narrow: bool, out: &mut Vec<ClientMsg>) {
        let Some(ws) = g.workspace else { return };
        let collapsed = self.collapsed.contains(&ws.id);
        self.workspace_row(ui, t, ws, collapsed, None, narrow, out);
        if collapsed {
            return;
        }
        let here = self.current_ws.as_deref() == Some(ws.id.as_str());
        // The new chat about to start here: the draft is the form.
        if self.current.is_none() && here {
            pending_row(ui, t);
        }
        let all = self.expanded.contains(&ws.id);
        let current = self.current.as_ref().map(|c| c.id.clone());
        let (shown, hidden) = visible(&g.chats, all, current.as_deref());
        for chat in shown {
            self.chat_row(ui, t, chat, INDENT, None, narrow, out);
        }
        let total = usize::try_from(ws.chats).unwrap_or(usize::MAX).max(g.chats.len());
        if total == 0 && !(self.current.is_none() && here) {
            hint(ui, t, "No chats yet", INDENT);
        }
        // Showing all: page on until this group is complete.
        if all
            && total > g.chats.len()
            && let Some(after) = self.next.clone()
        {
            if !self.loading {
                self.loading = true;
                out.push(ClientMsg::ListChats { after: Some(after), limit: PAGE });
            }
            ui.horizontal(|ui| {
                ui.add_space(INDENT);
                crate::theme::spinner(ui, t, None);
            });
        }
        let hidden = hidden.max(total.saturating_sub(g.chats.len().max(RECENT)));
        self.more_row(ui, t, &ws.id, hidden, total, all, INDENT);
        ui.add_space(4.0);
    }

    /// "Show all (k)" under a group with hidden chats, "Show fewer" once open.
    #[allow(clippy::too_many_arguments)] // one row
    fn more_row(&mut self, ui: &mut Ui, t: &Tokens, key: &str, hidden: usize, total: usize, all: bool, indent: f32) {
        if all && total > RECENT {
            if link_row(ui, t, "Show fewer", indent).clicked() {
                self.expanded.remove(key);
            }
        } else if !all && hidden > 0 && link_row(ui, t, &format!("Show all ({total})"), indent).clicked() {
            self.expanded.insert(key.to_owned());
        }
    }

    /// A workspace's row: chevron, environment icon, name; "+" and "⋯" on hover. A click
    /// folds it (except while searching: `q`).
    #[allow(clippy::too_many_arguments)] // one row of the list
    fn workspace_row(&mut self, ui: &mut Ui, t: &Tokens, ws: &WorkspaceSummary, collapsed: bool, q: Option<&str>, narrow: bool, out: &mut Vec<ClientMsg>) {
        let h = row_height(ui);
        if let Some((_, name)) = self.renaming_ws.as_mut().filter(|(id, _)| *id == ws.id) {
            let edit = ui.add_sized(vec2(ui.available_width(), h), egui::TextEdit::singleline(name).align(egui::Align2::LEFT_CENTER));
            if !edit.has_focus() && !edit.lost_focus() {
                edit.request_focus();
            }
            let enter = ui.input(|i| i.key_pressed(Key::Enter));
            if edit.lost_focus() {
                if enter && !name.trim().is_empty() {
                    out.push(ClientMsg::RenameWorkspace { id: ws.id.clone(), name: name.trim().to_owned() });
                }
                self.renaming_ws = None;
            }
            return;
        }
        let id = Id::unique(("sidebar-ws", &ws.id));
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::click());
        let here = self.current_ws.as_deref() == Some(ws.id.as_str());
        let forced = self.force_hover && here;
        let more_rect = Rect::from_min_size(pos2(rect.right() - ACTION_W - 2.0, rect.center().y - ACTION_W / 2.0), vec2(ACTION_W, ACTION_W));
        let plus_rect = more_rect.translate(vec2(-ACTION_W, 0.0));
        let more_id = id.with("more");
        let menu_open = egui::Popup::is_id_open(ui.ctx(), more_id.with("popup")) || egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
        let over = ui.rect_contains_pointer(rect) || forced;
        let shown = narrow || over || menu_open;
        // Phones show every row's actions: quietly, until touched.
        let idle = if over || menu_open { t.text_dim } else { t.text_faint };
        if ui.is_rect_visible(rect) {
            if over || menu_open {
                ui.painter().rect_filled(rect, t.round(5.0), t.hover);
            }
            let chevron = Rect::from_center_size(pos2(rect.left() + PAD + CHEVRON / 2.0, rect.center().y), vec2(CHEVRON, CHEVRON));
            icons::disclosure(ui.painter(), chevron, !collapsed, t.text_faint);
            let icon = Rect::from_center_size(pos2(rect.left() + INDENT + ICON / 2.0, rect.center().y), vec2(ICON, ICON));
            icons::env(ui.painter(), icon, ws.env, icons::env_color(t, ws.env));
            let x = icon.right() + 6.0;
            let font = egui::FontId::new(egui::TextStyle::Body.resolve(ui.style()).size, crate::theme::bold_or_regular(ui.ctx()));
            let color = if here { t.text_strong } else { t.text_dim };
            let galley = title_galley(ui, &ws.name, font, color, plus_rect.left() - 2.0 - x, q, t.accent);
            let pos = pos2(x, rect.center().y - galley.size().y / 2.0);
            self.rects.titles.insert(format!("ws:{}", ws.id), Rect::from_min_size(pos, galley.size()));
            ui.painter().galley(pos, galley, color);
        }
        let plus = action(ui, t, plus_rect, id.with("plus"), icons::plus, shown, idle).on_hover_text(format!("New chat in {}", ws.name));
        let more = action(ui, t, more_rect, more_id, icons::more, shown, idle).on_hover_text("Workspace menu");
        if shown {
            self.rects.actions.insert(format!("ws:{}", ws.id), plus_rect);
        }
        if plus.clicked() {
            out.push(ClientMsg::NewChat { workspace: Some(ws.id.clone()) });
            self.after_pick(narrow);
        }
        if here && std::mem::take(&mut self.open_ws_menu) {
            egui::Popup::open_id(ui.ctx(), egui::Popup::default_response_id(&more));
        }
        egui::Popup::menu(&more).align(egui::RectAlign::BOTTOM_END).show(|ui| self.workspace_menu(ui, t, ws, narrow, out));
        egui::Popup::context_menu(&resp).show(|ui| self.workspace_menu(ui, t, ws, narrow, out));
        let resp = resp.on_hover_text_at_pointer(format!("{}\n{} chat{}", ws.folder, ws.chats, if ws.chats == 1 { "" } else { "s" }));
        if resp.clicked() && q.is_none() {
            self.toggle_collapsed(&ws.id);
        }
    }

    fn workspace_menu(&mut self, ui: &mut Ui, t: &Tokens, ws: &WorkspaceSummary, narrow: bool, out: &mut Vec<ClientMsg>) {
        ui.set_min_width(200.0);
        if ui.button("New chat here").clicked() {
            out.push(ClientMsg::NewChat { workspace: Some(ws.id.clone()) });
            self.after_pick(narrow);
            ui.close();
        }
        if ui.button("Workspace settings…").clicked() {
            self.requests.push(Request::WorkspaceSettings { id: ws.id.clone(), remove: false });
            self.after_pick(narrow);
            ui.close();
        }
        if ui.button("Rename").clicked() {
            self.renaming_ws = Some((ws.id.clone(), ws.name.clone()));
            ui.close();
        }
        if ui.button("Copy folder path").on_hover_text(&ws.folder).clicked() {
            ui.ctx().copy_text(ws.folder.clone());
            ui.close();
        }
        ui.separator();
        if ui.button(egui::RichText::new("Remove…").color(t.danger)).clicked() {
            self.requests.push(Request::WorkspaceSettings { id: ws.id.clone(), remove: true });
            self.after_pick(narrow);
            ui.close();
        }
    }

    fn after_pick(&mut self, narrow: bool) {
        if narrow {
            self.open = Some(false);
        }
    }

    /// A chat's row: its title (cut to fit, the full title in a tooltip), a dot while a
    /// reply runs, "⋯" on hover.
    #[allow(clippy::too_many_arguments)] // one row of the list
    fn chat_row(&mut self, ui: &mut Ui, t: &Tokens, chat: &ChatSummary, indent: f32, q: Option<&str>, narrow: bool, out: &mut Vec<ClientMsg>) {
        let h = row_height(ui);
        if let Some((_, title)) = self.renaming.as_mut().filter(|(id, _)| *id == chat.id) {
            let edit = ui.add_sized(vec2(ui.available_width(), h), egui::TextEdit::singleline(title).align(egui::Align2::LEFT_CENTER));
            if !edit.has_focus() && !edit.lost_focus() {
                edit.request_focus();
            }
            let enter = ui.input(|i| i.key_pressed(Key::Enter));
            if edit.lost_focus() {
                if enter && !title.trim().is_empty() {
                    out.push(ClientMsg::RenameChat { id: chat.id.clone(), title: title.trim().to_owned() });
                }
                self.renaming = None;
            }
            return;
        }
        if self.deleting.as_deref() == Some(chat.id.as_str()) {
            egui::Frame::new().stroke(egui::Stroke::new(1.0, t.danger)).corner_radius(t.round(6.0)).inner_margin(6.0).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.add(egui::Label::new(egui::RichText::new(format!("Delete \u{201c}{}\u{201d}?", chat.title)).color(t.text)).wrap());
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
        let id = Id::unique(("sidebar-chat", &chat.id));
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::click());
        let selected = self.current.as_ref().is_some_and(|c| c.id == chat.id);
        let more_rect = Rect::from_min_size(pos2(rect.right() - ACTION_W - 2.0, rect.center().y - ACTION_W / 2.0), vec2(ACTION_W, ACTION_W));
        let more_id = id.with("more");
        let menu_open = egui::Popup::is_id_open(ui.ctx(), more_id.with("popup")) || egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
        let over = ui.rect_contains_pointer(rect) || (self.force_hover && selected);
        let shown = narrow || over || menu_open;
        // Phones show every row's actions: quietly, until touched.
        let idle = if over || menu_open { t.text_dim } else { t.text_faint };
        let mut truncated = false;
        if ui.is_rect_visible(rect) {
            let fill = if selected {
                Some(t.selection)
            } else if resp.is_pointer_button_down_on() {
                Some(t.pressed)
            } else if over || menu_open {
                Some(t.hover)
            } else {
                None
            };
            if let Some(fill) = fill {
                ui.painter().rect_filled(rect, t.round(5.0), fill);
            }
            let x = rect.left() + indent;
            let color = if selected { t.text_strong } else { t.text };
            let galley = title_galley(ui, &chat.title, egui::TextStyle::Body.resolve(ui.style()), color, more_rect.left() - 2.0 - x, q, t.accent);
            truncated = galley.elided;
            let pos = pos2(x, rect.center().y - galley.size().y / 2.0);
            self.rects.titles.insert(format!("chat:{}", chat.id), Rect::from_min_size(pos, galley.size()));
            ui.painter().galley(pos, galley, color);
            if chat.busy && !shown {
                ui.painter().circle_filled(more_rect.center(), 3.0, t.accent);
            }
        }
        let more = action(ui, t, more_rect, more_id, icons::more, shown, idle).on_hover_text("Chat menu");
        if shown {
            self.rects.actions.insert(format!("chat:{}", chat.id), more_rect);
        }
        egui::Popup::menu(&more).align(egui::RectAlign::BOTTOM_END).show(|ui| self.chat_menu(ui, t, chat, out));
        egui::Popup::context_menu(&resp).show(|ui| self.chat_menu(ui, t, chat, out));
        let resp = if truncated { resp.on_hover_text(&chat.title) } else { resp };
        let resp = if chat.busy { resp.on_hover_text("Replying…") } else { resp };
        if resp.clicked() && !selected {
            out.push(ClientMsg::OpenChat { id: chat.id.clone() });
            self.after_pick(narrow);
        }
    }

    /// The same for "⋯" and a right click.
    fn chat_menu(&mut self, ui: &mut Ui, t: &Tokens, chat: &ChatSummary, out: &mut Vec<ClientMsg>) {
        ui.set_min_width(170.0);
        if ui.button("Rename").clicked() {
            self.renaming = Some((chat.id.clone(), chat.title.clone()));
            ui.close();
        }
        let pin = if chat.pinned_at.is_some() { "Unpin" } else { "Pin to the top" };
        if ui.button(pin).clicked() {
            out.push(ClientMsg::PinChat { id: chat.id.clone(), pinned: chat.pinned_at.is_none() });
            ui.close();
        }
        ui.separator();
        if ui.button(egui::RichText::new("Delete…").color(t.danger)).clicked() {
            self.deleting = Some(chat.id.clone());
            ui.close();
        }
    }
}

/// A section heading ("Pinned", "Workspaces"), with a "+" on its right if `plus` (its
/// tooltip). Returns whether the "+" was clicked.
fn section(ui: &mut Ui, t: &Tokens, name: &str, plus: Option<&str>) -> bool {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), SECTION_H), Sense::hover());
    let font = egui::TextStyle::Small.resolve(ui.style());
    let galley = ui.painter().layout_no_wrap(name.to_owned(), font, t.text_faint);
    ui.painter().galley(pos2(rect.left() + PAD, rect.center().y - galley.size().y / 2.0), galley, t.text_faint);
    let Some(tip) = plus else { return false };
    let r = Rect::from_min_size(pos2(rect.right() - ACTION_W - 2.0, rect.center().y - ACTION_W / 2.0), vec2(ACTION_W, ACTION_W));
    action(ui, t, r, Id::unique(("sidebar-section", name)), icons::plus, true, t.text_dim).on_hover_text(tip).clicked()
}

/// A faint line of text in the list.
fn hint(ui: &mut Ui, t: &Tokens, text: &str, indent: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), row_height(ui) - 4.0), Sense::hover());
    let galley = ui.painter().layout_no_wrap(text.to_owned(), egui::TextStyle::Small.resolve(ui.style()), t.text_faint);
    ui.painter().galley(pos2(rect.left() + indent, rect.center().y - galley.size().y / 2.0), galley, t.text_faint);
}

/// A small, quiet clickable line ("Show all (12)", "Load older chats").
fn link_row(ui: &mut Ui, t: &Tokens, text: &str, indent: f32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_height(ui) - 4.0), Sense::click());
    let color = if resp.hovered() { t.text } else { t.text_dim };
    let galley = ui.painter().layout_no_wrap(text.to_owned(), egui::TextStyle::Small.resolve(ui.style()), color);
    ui.painter().galley(pos2(rect.left() + indent, rect.center().y - galley.size().y / 2.0), galley, color);
    resp
}

/// The new chat about to start in this workspace (it is created by its first message).
fn pending_row(ui: &mut Ui, t: &Tokens) {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_height(ui)), Sense::hover());
    ui.painter().rect_filled(rect, t.round(5.0), t.selection);
    let font = egui::TextStyle::Body.resolve(ui.style());
    let mut job = egui::text::LayoutJob::default();
    job.append("New chat", 0.0, egui::TextFormat { font_id: font, color: t.text_strong, italics: true, ..Default::default() });
    let galley = ui.painter().layout_job(job);
    ui.painter().galley(pos2(rect.left() + INDENT, rect.center().y - galley.size().y / 2.0), galley, t.text_strong);
    resp.on_hover_text("Starts when you send the first message");
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

    fn workspaces(s: &mut Sidebar, items: Vec<WorkspaceSummary>, current: Option<&str>) {
        s.apply(&ServerMsg::Workspaces { items, current: current.map(Into::into), root: String::new(), support: Default::default() });
    }

    /// Draw the list once, `width` wide (as the side panel; a phone's full screen list when
    /// wider than that), with these events.
    fn run(ctx: &egui::Context, s: &mut Sidebar, width: f32, events: Vec<egui::Event>) -> Vec<ClientMsg> {
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(width, 700.0))), events, ..Default::default() };
        let mut sent = Vec::new();
        let mut out = ctx.run_ui(input, |ui| sent = s.show(ui, &Tokens::dark(), width > 300.0));
        out.textures_delta.clear();
        sent
    }

    /// Draw the list once in a 280-point window with the pointer at `pointer`.
    fn draw(ctx: &egui::Context, s: &mut Sidebar, pointer: Option<egui::Pos2>) -> Vec<ClientMsg> {
        run(ctx, s, 280.0, pointer.map(|p| vec![egui::Event::PointerMoved(p)]).unwrap_or_default())
    }

    /// A click at `at`; what it sent.
    fn click(ctx: &egui::Context, s: &mut Sidebar, at: egui::Pos2) -> Vec<ClientMsg> {
        let press = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        let mut sent = draw(ctx, s, Some(at));
        sent.extend(run(ctx, s, 280.0, vec![press(true)]));
        sent.extend(run(ctx, s, 280.0, vec![press(false)]));
        sent
    }

    /// Two workspaces with chats, chat "a" in w1 in view, and an unfiled chat.
    fn listed() -> Sidebar {
        let mut s = Sidebar::default();
        workspaces(&mut s, vec![ws("w1", 5, 2), ws("w2", 1, 1)], Some("w1"));
        s.apply(&ServerMsg::Chats {
            pinned: vec![],
            recent: vec![
                ChatSummary { title: "Quarterly budget".into(), ..in_ws("a", 30, "w1") },
                ChatSummary { title: "Trip to Lisbon".into(), ..in_ws("b", 20, "w2") },
                ChatSummary { title: "Budget for the trip".into(), ..in_ws("c", 10, "w1") },
                ChatSummary { title: "Loose ends".into(), ..chat("d", 5, None) },
            ],
            after: None,
            next: None,
        });
        s.apply(&ServerMsg::ChatOpened { chat: Some(in_ws("a", 30, "w1")), workspace: Some("w1".into()) });
        s
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
        workspaces(&mut s, vec![ws("w1", 5, 2), ws("w2", 1, 1)], Some("w1"));
        s.collapsed.insert("w2".into());
        s.apply(&ServerMsg::ChatOpened { chat: None, workspace: Some("w2".into()) });
        assert_eq!(s.current_workspace().map(|w| w.name.as_str()), Some("W2"));
        // A removed workspace is forgotten, collapsed state included.
        workspaces(&mut s, vec![ws("w1", 5, 2)], None);
        assert!(s.current_workspace().is_none() && s.collapsed.is_empty());
    }

    #[test]
    fn narrow_screens_start_closed_and_close_after_picking() {
        let mut s = Sidebar::default();
        assert!(s.is_open(false));
        assert!(!s.is_open(true));
        s.toggle();
        assert!(s.is_open(true));
        assert_eq!(s.new_chat(true), ClientMsg::NewChat { workspace: None });
        assert!(!s.is_open(true), "a new chat closes the list on phones");
        s.focus_search();
        assert!(s.is_open(true), "the search shortcut opens it");
    }

    #[test]
    fn new_chat_goes_to_the_workspace_in_view() {
        let ctx = egui::Context::default();
        let mut s = listed();
        assert_eq!(s.new_chat_msg(), ClientMsg::NewChat { workspace: Some("w1".into()) });
        // Looking at a chat in w2: there.
        s.apply(&ServerMsg::ChatOpened { chat: Some(in_ws("b", 20, "w2")), workspace: Some("w2".into()) });
        assert_eq!(s.new_chat_msg(), ClientMsg::NewChat { workspace: Some("w2".into()) });
        // The button sends the same.
        draw(&ctx, &mut s, None);
        let at = s.rects.new_chat.center();
        assert_eq!(click(&ctx, &mut s, at), [ClientMsg::NewChat { workspace: Some("w2".into()) }]);
        // A workspace row's "+": that workspace.
        let row = s.rects.titles["ws:w1"];
        draw(&ctx, &mut s, Some(row.center()));
        let plus = s.rects.actions["ws:w1"].center();
        assert_eq!(click(&ctx, &mut s, plus), [ClientMsg::NewChat { workspace: Some("w1".into()) }]);
        // A chat outside any workspace: where the engine puts new chats.
        s.apply(&ServerMsg::ChatOpened { chat: Some(chat("d", 5, None)), workspace: None });
        assert_eq!(s.new_chat_msg(), ClientMsg::NewChat { workspace: None });
    }

    #[test]
    fn search_filters_by_title_and_workspace_name() {
        assert!(matches("Quarterly Budget", None, "budget"));
        assert!(matches("Anything", Some("Data analysis"), "data"));
        assert!(!matches("Trip", Some("Travel"), "budget"));
        let ctx = egui::Context::default();
        let mut s = listed();
        draw(&ctx, &mut s, None);
        assert_eq!(s.rects.titles.len(), 6, "two workspaces, four chats: {:?}", s.rects.titles.keys());
        s.set_search("  BUDGET ");
        draw(&ctx, &mut s, None);
        let shown: Vec<&str> = s.rects.titles.keys().map(String::as_str).collect();
        assert_eq!(shown, ["chat:a", "chat:c", "ws:w1"], "only matches, under their workspace");
        assert_eq!(s.first_match.as_deref(), Some("a"));
        // A workspace's name matches all its chats.
        s.set_search("w2");
        draw(&ctx, &mut s, None);
        assert_eq!(s.rects.titles.keys().map(String::as_str).collect::<Vec<_>>(), ["chat:b", "ws:w2"]);
        // Collapsed groups open while searching.
        s.collapsed.insert("w2".into());
        draw(&ctx, &mut s, None);
        assert!(s.rects.titles.contains_key("chat:b"));
        // No match: nothing listed.
        s.set_search("zzz");
        draw(&ctx, &mut s, None);
        assert!(s.rects.titles.is_empty() && s.first_match.is_none());
    }

    #[test]
    fn esc_clears_the_search_and_enter_opens_the_first_match() {
        let ctx = egui::Context::default();
        let mut s = listed();
        let key = |key| egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE };
        s.focus_search();
        run(&ctx, &mut s, 280.0, vec![]);
        run(&ctx, &mut s, 280.0, vec![egui::Event::Text("lisbon".into())]);
        assert_eq!(s.search, "lisbon");
        run(&ctx, &mut s, 280.0, vec![]);
        assert_eq!(run(&ctx, &mut s, 280.0, vec![key(Key::Enter)]), [ClientMsg::OpenChat { id: "b".into() }]);
        assert!(s.search.is_empty(), "opening a match clears the search");
        s.focus_search();
        run(&ctx, &mut s, 280.0, vec![]);
        run(&ctx, &mut s, 280.0, vec![egui::Event::Text("trip".into())]);
        assert_eq!(s.search, "trip");
        run(&ctx, &mut s, 280.0, vec![key(Key::Escape)]);
        assert!(s.search.is_empty());
    }

    #[test]
    fn collapsed_groups_hide_their_chats_and_are_remembered() {
        let ctx = egui::Context::default();
        let mut s = listed();
        draw(&ctx, &mut s, None);
        assert!(s.rects.titles.contains_key("chat:a"));
        // A click on the workspace's row folds it.
        let at = s.rects.titles["ws:w1"].center();
        click(&ctx, &mut s, at);
        draw(&ctx, &mut s, None);
        assert!(s.collapsed.contains("w1") && !s.rects.titles.contains_key("chat:a") && s.rects.titles.contains_key("chat:b"));
        // Remembered: stored and restored (as at the next launch).
        let json = serde_json::to_string(&s.prefs()).unwrap();
        let mut again = listed();
        again.set_prefs(&serde_json::from_str(&json).unwrap());
        draw(&ctx, &mut again, None);
        assert!(!again.rects.titles.contains_key("chat:a"));
        assert_eq!(serde_json::from_str::<SidebarPrefs>("{}").unwrap(), SidebarPrefs::default());
        // Unfolds again.
        click(&ctx, &mut s, at);
        assert!(!s.collapsed.contains("w1"));
    }

    #[test]
    fn hover_actions_appear_without_moving_titles() {
        let ctx = egui::Context::default();
        let mut s = listed();
        draw(&ctx, &mut s, None);
        draw(&ctx, &mut s, None);
        let before = s.rects.titles.clone();
        assert!(s.rects.actions.is_empty(), "no actions without the pointer: {:?}", s.rects.actions);
        for key in ["ws:w1", "chat:a", "chat:d"] {
            let over = before[key].center();
            draw(&ctx, &mut s, Some(over));
            draw(&ctx, &mut s, Some(over));
            assert_eq!(s.rects.actions.keys().collect::<Vec<_>>(), [key], "only the hovered row");
            assert_eq!(s.rects.titles, before, "{key}: titles moved when its actions showed");
            assert!(s.rects.actions[key].left() >= before[key].right(), "{key}: the actions cover its title");
        }
        // Phones (no hover): always shown.
        run(&ctx, &mut s, 380.0, vec![]);
        assert!(s.rects.actions.contains_key("ws:w2") && s.rects.actions.contains_key("chat:d"));
    }

    #[test]
    fn long_groups_show_the_newest_and_show_all_pages_on() {
        let many: Vec<ChatSummary> = (0..8).map(|i| in_ws(&format!("c{i}"), 100 - i, "w1")).collect();
        let refs: Vec<&ChatSummary> = many.iter().collect();
        let (shown, hidden) = visible(&refs, false, None);
        assert_eq!((shown.len(), hidden), (RECENT, 3));
        // The chat in view stays visible even when it is older.
        let (shown, hidden) = visible(&refs, false, Some("c7"));
        assert_eq!((shown.last().map(|c| c.id.as_str()), hidden), (Some("c7"), 2));
        assert_eq!(visible(&refs, true, None).0.len(), 8);
        // "Show all" asks for the rest of the group's chats, one page at a time.
        let ctx = egui::Context::default();
        let mut s = Sidebar::default();
        workspaces(&mut s, vec![ws("w1", 5, 60)], Some("w1"));
        s.apply(&ServerMsg::Chats { pinned: vec![], recent: many.clone(), after: None, next: many.last().map(ChatSummary::cursor) });
        draw(&ctx, &mut s, None);
        assert!(!s.rects.titles.contains_key("chat:c6"));
        s.expanded.insert("w1".into());
        let sent = draw(&ctx, &mut s, None);
        assert!(s.rects.titles.contains_key("chat:c6"));
        assert!(matches!(sent.as_slice(), [ClientMsg::ListChats { after: Some(_), limit: PAGE }]), "{sent:?}");
        assert!(draw(&ctx, &mut s, None).is_empty(), "one page at a time");
    }

    #[test]
    fn renders_without_clicks() {
        let ctx = egui::Context::default();
        let mut s = Sidebar::default();
        workspaces(&mut s, vec![ws("w1", 5, 3)], Some("w1"));
        s.apply(&ServerMsg::Chats {
            pinned: vec![chat("p", 1, Some(5))],
            recent: vec![in_ws("a", 10, "w1"), chat("x", 3, None)],
            after: None,
            next: Some(chat("x", 3, None).cursor()),
        });
        s.apply(&ServerMsg::ChatOpened { chat: None, workspace: Some("w1".into()) });
        s.renaming = Some(("a".into(), "A".into()));
        s.open_new_menu();
        assert!(s.open_workspace_menu());
        assert!(draw(&ctx, &mut s, None).is_empty());
        assert!(draw(&ctx, &mut s, None).is_empty());
        assert!(s.take_requests().is_empty());
    }
}
