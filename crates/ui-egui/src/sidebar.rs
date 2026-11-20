//! The chat list, on a floating glass card ([`crate::glass`]). Top to bottom:
//!
//! - The window's controls: the list toggle, then Back and Forward through the chats you
//!   opened (Ctrl/Cmd+[ and ]). They stay at the top left whether the list is open (here)
//!   or closed (in the header), so the toggle never moves.
//! - Navigation rows, each an icon and a word like the rest of the list:
//!   - **New chat** (Ctrl/Cmd+N; Ctrl/Cmd+Shift+O in the browser, where Ctrl+N is the
//!     browser's): a new chat in the workspace in view. A caret on hover opens "New
//!     workspace…" and "New chat in" any workspace.
//!   - **Search** (Ctrl/Cmd+F): the row is the field. It filters the loaded chats by title and
//!     workspace name in place; Enter opens the first match, Esc clears.
//!   - **Artifacts** (with how many the chat has): the chat's artifact pane.
//!   - **Plugins**: Settings › Plugins.
//! - Pinned chats, then **Workspaces** (a "+" there makes one): one collapsible group per
//!   workspace, most recently active first, its chats newest first. A group shows its
//!   [`RECENT`] newest chats and "Show all (k)". Collapsed groups are remembered
//!   ([`SidebarPrefs`]).
//! - **Chats**: chats that belong to no project (each works in a folder of its own that
//!   byob keeps), above the projects; "+" starts one. "Turn into project" in a chat's menu
//!   makes its folder a project, and the chat moves under it.
//! - Settings, pinned at the bottom.
//!
//! Chat rows show how long ago they were active (the time gives way to "⋯" on hover, and to
//! a dot while a reply runs). A workspace row is a folder icon and its name, with a quiet
//! chevron after it; its environment shows as a small badge only when it isn't this
//! machine. Selection is a calm tint of the text colour, never an accent block.
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

use byob_protocol::workspace::EnvKind;
use byob_protocol::{ChatCursor, ChatSummary, ClientMsg, ServerMsg, WorkspaceSummary};
use egui::{Galley, Id, Key, KeyboardShortcut, Modifiers, Rect, Sense, Ui, pos2, vec2};
use serde::{Deserialize, Serialize};

use crate::glass;
use crate::icons;
use crate::menu;
use crate::theme::Tokens;

/// Below this width the sidebar is a full-screen list you toggle, not a side panel.
pub const NARROW: f32 = 720.0;
/// Chats asked for per page.
const PAGE: u32 = 50;
/// Chats a workspace group shows before "Show all".
pub const RECENT: usize = 5;
/// Chats without a project shown before "Show all".
pub const RECENT_CHATS: usize = 10;

/// A list row's height (at least: big text makes it taller). Navigation rows too.
const ROW_H: f32 = 30.0;
/// A section heading's height ("Pinned", "Projects").
const SECTION_H: f32 = 26.0;
/// Space above a section heading.
const SECTION_GAP: f32 = 12.0;
/// One hover action's square.
const ACTION_W: f32 = 22.0;
/// Inside a row: the left padding, the icon and the gap after it. Chat titles start where
/// their workspace's name does.
const PAD: f32 = 8.0;
const ICON: f32 = 16.0;
const ICON_GAP: f32 = 9.0;
const INDENT: f32 = PAD + ICON + ICON_GAP;
/// The disclosure chevron after a workspace's name.
const CHEVRON: f32 = 11.0;
/// Room kept at a chat row's right for its time, its dot or its "⋯" (titles never move).
const TRAIL_W: f32 = 36.0;
/// Space between the list's edges and its rows.
const SIDE_PAD: f32 = 8.0;

/// A title bar the app draws itself (macOS, where the window's content runs under a
/// transparent title bar): the window buttons take `left` points at the top left, in a bar
/// `height` tall. `None` (Windows, Linux, the browser) keeps the system's title bar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Titlebar {
    pub left: f32,
    pub height: f32,
}

impl Titlebar {
    /// macOS: room for the close, minimize and zoom buttons in a standard title bar.
    pub const MACOS: Self = Self { left: 78.0, height: 30.0 };
}

/// The height of the top bar the chat list and the header share, so their controls line up.
pub fn top_bar_height(ui: &Ui, titlebar: Option<Titlebar>) -> f32 {
    let button = crate::icons::button_height(ui);
    titlebar.map_or(button + 16.0, |tb| tb.height.max(button + 4.0))
}

/// Empty space in an app-drawn title bar moves the window; a double click zooms it, as the
/// system's title bar does. Register it before the bar's buttons so they stay on top.
pub fn window_drag(ui: &Ui, rect: Rect, id: Id) {
    let r = ui.interact(rect, id, Sense::click_and_drag());
    if r.drag_started_by(egui::PointerButton::Primary) {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
    if r.double_clicked() {
        let zoomed = ui.input(|i| i.viewport().maximized.unwrap_or(false));
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Maximized(!zoomed));
    }
}

/// New chat, on the desktop.
pub const NEW_CHAT_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::N);
/// New chat anywhere, and the only one in the browser (Ctrl+N opens a browser window and
/// pages can't have it; eframe keeps Ctrl+O combinations for the page).
pub const NEW_CHAT_SHORTCUT_ALT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::O);
/// Focus the chat search.
pub const SEARCH_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::F);
/// Search, the way most apps open it now (Cmd/Ctrl+K); Cmd/Ctrl+F does the same.
pub const PALETTE_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::K);
/// Back and forward through the chats opened (Cmd on macOS, Ctrl elsewhere).
/// Hide or show the sidebar: Cmd/Ctrl+B.
pub const TOGGLE_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::B);
pub const BACK_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::OpenBracket);
pub const FORWARD_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::CloseBracket);
/// Most chats Back remembers.
const HISTORY: usize = 100;

/// The chats opened, in order, and where Back/Forward stand in them.
#[derive(Debug, Default)]
struct History {
    stack: Vec<String>,
    pos: usize,
    /// The chat a Back/Forward asked for: its opening doesn't start a new branch.
    expect: Option<String>,
}

impl History {
    fn visit(&mut self, id: &str) {
        if self.expect.take().as_deref() == Some(id) {
            return;
        }
        if self.stack.get(self.pos).map(String::as_str) == Some(id) {
            return;
        }
        if !self.stack.is_empty() {
            self.stack.truncate(self.pos + 1);
        }
        self.stack.push(id.to_owned());
        if self.stack.len() > HISTORY {
            self.stack.remove(0);
        }
        self.pos = self.stack.len() - 1;
    }

    fn can_back(&self) -> bool {
        self.pos > 0
    }

    fn can_forward(&self) -> bool {
        self.pos + 1 < self.stack.len()
    }

    fn step(&mut self, forward: bool) -> Option<String> {
        let to = if forward { self.can_forward().then(|| self.pos + 1)? } else { self.pos.checked_sub(1)? };
        self.pos = to;
        let id = self.stack.get(to).cloned()?;
        self.expect = Some(id.clone());
        Some(id)
    }

    /// A deleted chat leaves the history.
    fn forget(&mut self, id: &str) {
        let before = self.stack.iter().take(self.pos).filter(|c| *c == id).count();
        self.stack.retain(|c| c != id);
        // Neighbours that became equal collapse into one.
        self.stack.dedup();
        self.pos = self.pos.saturating_sub(before).min(self.stack.len().saturating_sub(1));
    }
}

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
    /// Settings › Plugins.
    Plugins,
    /// Show or hide the chat's artifact pane.
    Artifacts,
    /// Open a pinned artifact: its chat, then the artifact beside it.
    OpenArtifact {
        chat: String,
        id: String,
    },
    /// The models and the connection (the footer's status row).
    Connection,
    /// The Library: what you keep (documents, images, data).
    Library,
    /// Apps: what byob built that runs.
    Apps,
    /// Projects: every project in one place.
    Projects,
    /// Back from a page (Projects, Apps, Library, Plugins) to the chat.
    LeavePage,
    /// Ask byob for help: a new chat ready to ask.
    Help,
    /// Send feedback: the project's issue page in the browser.
    Feedback,
    /// A new chat with this text ready to send (Tab in search).
    Compose(String),
    /// Bring in a file to work on: a new chat, with the file picker open.
    OpenFile,
    /// The Library's list, for search to find every app and document.
    LibraryList,
}

/// The page the app shows in place of the chat, if any: its row is highlighted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum NavPage {
    #[default]
    None,
    Projects,
    Apps,
    Library,
    Plugins,
}

/// An artifact pinned to the top of the list (remembered on this device).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedArtifact {
    pub chat: String,
    pub id: String,
    pub title: String,
}

/// Where a dragged chat can be dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Drop {
    /// Pinned: pin it.
    Pin,
    /// One of the person's sections.
    Section(String),
    /// Archived: archive it.
    Archive,
    /// Chats: back to the plain list (out of its section, unpinned, not archived).
    Chats,
}

/// The connection as the footer shows it: a dot, the model, where it runs.
#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub dot: egui::Color32,
    /// What byob is doing, in a word or two: "Ready", "Working…", "Waiting for you".
    pub state: String,
    pub model: String,
    pub place: String,
}

/// Search, as a palette over the window: what it shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PaletteTab {
    #[default]
    All,
    Chats,
    Projects,
    Artifacts,
}

/// The open search palette.
#[derive(Debug, Clone, Default)]
pub struct Palette {
    pub query: String,
    pub tab: PaletteTab,
    /// The highlighted row (↑ ↓ move it, ↵ opens it).
    pub sel: usize,
    /// The highlight moved by keys this frame: its row scrolls into view.
    pub moved: bool,
}

/// Something the palette can open.
#[derive(Debug, Clone, PartialEq)]
enum Hit {
    Chat(ChatSummary),
    Project(WorkspaceSummary),
    Artifact(PinnedArtifact),
    /// Any app or document byob made (the Library's list).
    Item(byob_protocol::LibraryItem),
    Action(&'static str),
    /// "Show all N": the rest of a section, on its own tab.
    More(PaletteTab, usize),
}

/// The palette's types, in order (← → and Tab go through them).
const PALETTE_TABS: [(PaletteTab, &str); 4] =
    [(PaletteTab::All, "All"), (PaletteTab::Chats, "Chats"), (PaletteTab::Projects, "Projects"), (PaletteTab::Artifacts, "Apps & documents")];

/// What the palette can do besides finding things.
const PALETTE_ACTIONS: [&str; 6] = ["New chat", "New project", "Projects", "Library", "Models and connection", "Settings"];

/// The most a section lists on its own tab.
const PALETTE_MAX: usize = 60;

/// What the list remembers per device: collapsed workspaces, and the app's unsent drafts
/// (see `ByobApp::sidebar_prefs`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SidebarPrefs {
    pub collapsed: BTreeSet<String>,
    /// Unsent composer text by chat (`chat:<id>`) or new chat (`new:<workspace id>`).
    pub drafts: BTreeMap<String, String>,
    /// The person's own sections, in order.
    pub sections: Vec<Section>,
    pub group: GroupBy,
    pub sort: SortBy,
    /// Chats whose replies hide their details (thinking, tool steps, tokens and time).
    pub quiet: BTreeSet<String>,
    /// Artifacts pinned to the top of the list (kept here only by older versions: the list
    /// keeps them now, shared with every window).
    pub pins: Vec<PinnedArtifact>,
    /// When each chat was last seen (for the new-reply dot), on this device.
    pub seen: BTreeMap<String, i64>,
    /// The Space this device is in.
    pub space: Option<String>,
    /// The Projects page: rows or cards, and its order.
    pub projects_view: crate::projects::View,
    pub projects_sort: crate::projects::Sort,
}

/// The person's own sections and the list's view: shared through the engine's layout, so
/// they are the same in every window (see `byob_types::layout::ChatList`).
pub use byob_types::layout::{ChatList, ListGroup as GroupBy, ListSection as Section, ListSort as SortBy};

const GROUPS: [(GroupBy, &str); 3] = [(GroupBy::Project, "By project"), (GroupBy::Date, "By date"), (GroupBy::None, "One list")];
const SORTS: [(SortBy, &str); 2] = [(SortBy::Updated, "Last active"), (SortBy::Name, "Name")];

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
    pub artifacts: Rect,
    /// The list toolbar's view menu and "+" (new chat, project, section).
    pub view: Rect,
    pub plus: Rect,
    /// Where each section's heading was drawn, by section id.
    pub sections: BTreeMap<String, Rect>,
    pub toggle: Rect,
    pub back: Rect,
    pub forward: Rect,
    /// Where each title was drawn: `ws:<id>`, `chat:<id>`.
    pub titles: BTreeMap<String, Rect>,
    /// The first hover action of each row that shows its actions.
    pub actions: BTreeMap<String, Rect>,
    /// Where each folding heading was drawn, by its key (`grp:pinned`, `grp:archived`…).
    pub headings: BTreeMap<String, Rect>,
    /// The footer's status line (`Rect::NOTHING` without one), and its Settings button.
    pub status: Rect,
    pub settings: Rect,
}

impl Default for Rects {
    fn default() -> Self {
        Self {
            new_chat: Rect::NOTHING,
            new_chat_caret: Rect::NOTHING,
            search: Rect::NOTHING,
            artifacts: Rect::NOTHING,
            view: Rect::NOTHING,
            plus: Rect::NOTHING,
            sections: BTreeMap::new(),
            toggle: Rect::NOTHING,
            back: Rect::NOTHING,
            forward: Rect::NOTHING,
            titles: BTreeMap::new(),
            actions: BTreeMap::new(),
            headings: BTreeMap::new(),
            status: Rect::NOTHING,
            settings: Rect::NOTHING,
        }
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
    /// Chats whose replies hide their details ([`SidebarPrefs::quiet`]).
    pub quiet: BTreeSet<String>,
    /// Groups showing all their chats (`""`: Other chats).
    expanded: BTreeSet<String>,
    /// The search box.
    search: String,
    /// The first chat the search matched last frame (Enter opens it).
    first_match: Option<String>,
    /// Chat being renamed, with the edited title.
    renaming: Option<(String, String)>,
    /// A destructive action waiting for its confirmation dialog.
    confirm: Option<Pending>,
    /// Workspace being renamed, with the edited name.
    renaming_ws: Option<(String, String)>,
    requests: Vec<Request>,
    /// The page shown in place of the chat (its row is highlighted; Back leaves it).
    pub page: NavPage,
    /// Every app and document (the Library's list), for search.
    pub library: Vec<byob_protocol::LibraryItem>,
    /// A press on a chat row: which chat, and where it began.
    press: Option<(String, egui::Pos2)>,
    /// Spaces (shared with every window) and the one this window is in.
    pub spaces: Vec<byob_types::layout::ListSpace>,
    pub space: Option<String>,
    /// The Space settings dialog: which Space, and its name being edited.
    space_edit: Option<(String, String)>,
    /// Something in Plugins needs you (a dot on its row).
    pub plugins_attention: bool,
    /// Shown? `None` until the first frame decides from the width.
    open: Option<bool>,
    was_narrow: bool,
    /// Scripted runs: draw the current workspace's and chat's rows as if hovered, open
    /// the current workspace's menu, open the New chat menu.
    force_hover: bool,
    open_ws_menu: bool,
    open_new_menu: bool,
    /// Scripted runs: open the footer's menu on the next frame.
    open_footer_menu: bool,
    /// Now (ms since the epoch), read once per frame for the rows' "4h".
    now_ms: i64,
    history: History,
    /// The app draws the title bar (macOS): the controls leave room for the window buttons.
    pub titlebar: Option<Titlebar>,
    /// How many artifacts the chat in view has (the app says, every frame).
    artifacts: usize,
    /// The engine starts chats without a project and turns them into projects (cap
    /// `unattached`; the app says, every frame).
    pub unattached: bool,
    sections: Vec<Section>,
    group_by: GroupBy,
    sort_by: SortBy,
    /// Section being renamed, with the edited name.
    renaming_section: Option<(String, String)>,
    /// The sections or the view changed here: the app shares it ([`Self::take_list_change`]).
    list_changed: bool,
    /// The engine archives chats (cap `archive`; the app says, every frame).
    pub archive: bool,
    /// Artifacts pinned to the top of the list.
    pub pins: Vec<PinnedArtifact>,
    /// Projects pinned to the top of the list, by id.
    pub pinned_projects: Vec<String>,
    /// When each chat was last seen open (its `updated_at` then): newer means a reply you
    /// haven't seen. Chats not listed count as seen unless they changed after `started`
    /// (set on the first frame).
    seen: BTreeMap<String, i64>,
    started: i64,
    /// A chat being dragged (id, title), and where it can land: this frame's targets and
    /// last frame's (rows above a target are drawn before it).
    dragging: Option<(String, String)>,
    drops: Vec<(Rect, Drop)>,
    drops_before: Vec<(Rect, Drop)>,
    /// The connection, for the footer (the app says, every frame).
    pub status: Option<Status>,
    /// Search: open over the window while `Some`.
    pub palette: Option<Palette>,
    pub rects: Rects,
}

/// A destructive action the list asks about first ([`menu::Confirm`]).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pending {
    DeleteChat { id: String, title: String },
    RemoveWorkspace { id: String, name: String, chats: u64, clean: bool },
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

/// The search, normalized: trimmed and lower case (`None`: no search).
fn query(search: &str) -> Option<String> {
    Some(search.trim().to_lowercase()).filter(|q| !q.is_empty())
}

/// Does a chat match the search `q` (lower case): its title, or its workspace's name?
pub fn matches(title: &str, workspace: Option<&str>, q: &str) -> bool {
    title.to_lowercase().contains(q) || workspace.is_some_and(|w| w.to_lowercase().contains(q))
}

/// The search as words: trimmed, lower case, one space between words ("" for none).
fn search_words(search: &str) -> String {
    search.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// How well `title` (and `extra`: its project, its folder, its chat) matches `q` (from
/// [`search_words`]): every word must be in one of them, in any order. Lower is better:
/// the title starts with the search, its words start with the search's, the title has
/// them all, some are only in `extra`. `None`: no match. An empty search matches all.
fn rank(title: &str, extra: &[&str], q: &str) -> Option<u8> {
    if q.is_empty() {
        return Some(0);
    }
    let t = title.to_lowercase();
    let extra: Vec<String> = extra.iter().map(|e| e.to_lowercase()).collect();
    let words: Vec<&str> = q.split(' ').collect();
    if !words.iter().all(|w| t.contains(w) || extra.iter().any(|e| e.contains(w))) {
        return None;
    }
    let starts = |w: &str| t.split(|c: char| !c.is_alphanumeric()).any(|part| part.starts_with(w));
    Some(if t.starts_with(q) {
        0
    } else if words.iter().all(|w| starts(w)) {
        1
    } else if words.iter().all(|w| t.contains(w)) {
        2
    } else {
        3
    })
}

/// Where the search's words are in `title`, as byte ranges in order (none overlapping), to
/// show them in bold. None when lower-casing changes the title's length (some scripts), so a
/// range can never cut a character.
fn highlights(title: &str, q: &str) -> Vec<(usize, usize)> {
    let lower = title.to_lowercase();
    if q.is_empty() || lower.len() != title.len() {
        return Vec::new();
    }
    let mut marks: Vec<(usize, usize)> = Vec::new();
    for w in q.split(' ').filter(|w| !w.is_empty()) {
        let mut from = 0;
        while let Some(i) = lower.get(from..).and_then(|rest| rest.find(w)) {
            let s = from + i;
            let e = s + w.len();
            if title.is_char_boundary(s) && title.is_char_boundary(e) {
                marks.push((s, e));
            }
            from = e;
        }
    }
    marks.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (s, e) in marks {
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    merged
}

/// A palette type: its name, and how many it finds while searching.
fn palette_tab(ui: &mut Ui, t: &Tokens, name: &str, count: Option<usize>, on: bool) -> egui::Response {
    let font = egui::TextStyle::Body.resolve(ui.style());
    let small = egui::TextStyle::Small.resolve(ui.style());
    let label = ui.painter().layout_no_wrap(name.to_owned(), font, if on { t.text_strong } else { t.text_dim });
    let num = count.map(|n| ui.painter().layout_no_wrap(n.to_string(), small, t.text_faint));
    let w = label.size().x + 20.0 + num.as_ref().map_or(0.0, |g| g.size().x + 6.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 26.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let state = if on {
            glass::RowState::Selected
        } else if resp.hovered() {
            glass::RowState::Hovered
        } else {
            glass::RowState::Idle
        };
        glass::row(ui.painter(), t, rect, state);
        let y = rect.center().y - label.size().y / 2.0;
        let lw = label.size().x;
        ui.painter().galley(pos2(rect.left() + 10.0, y), label, t.text);
        if let Some(g) = num {
            let gy = rect.center().y - g.size().y / 2.0;
            ui.painter().galley(pos2(rect.left() + 10.0 + lw + 6.0, gy), g, t.text_faint);
        }
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// What the palette says when it lists nothing: why, and what to do instead.
fn palette_empty(ui: &mut Ui, t: &Tokens, p: &Palette) {
    let typed = p.query.trim();
    let (title, sub) = match (p.tab, typed.is_empty()) {
        (PaletteTab::Artifacts, true) => ("No apps or documents yet".to_owned(), "What byob makes for you shows up here"),
        (PaletteTab::Projects, true) => ("No projects yet".to_owned(), "A project is a folder byob works in"),
        (_, true) => ("Nothing here yet".to_owned(), "Your chats show up here"),
        (_, false) => {
            let mut shown: String = typed.chars().take(40).collect();
            if typed.chars().count() > 40 {
                shown.push('…');
            }
            (format!("No matches for \u{201c}{shown}\u{201d}"), "Press Tab to ask byob in a new chat")
        }
    };
    ui.add_space(28.0);
    ui.vertical_centered(|ui| {
        ui.label(egui::RichText::new(title).color(t.text));
        ui.add_space(2.0);
        ui.label(egui::RichText::new(sub).small().color(t.text_faint));
    });
    ui.add_space(20.0);
}

/// The chats a group shows: all when `all`, else the [`RECENT`] newest plus `current` if it
/// is further down. Returns them and how many are left out.
fn visible<'a>(chats: &[&'a ChatSummary], all: bool, current: Option<&str>) -> (Vec<&'a ChatSummary>, usize) {
    visible_n(chats, all, current, RECENT)
}

/// [`visible`] with the `n` newest instead of [`RECENT`].
fn visible_n<'a>(chats: &[&'a ChatSummary], all: bool, current: Option<&str>, n: usize) -> (Vec<&'a ChatSummary>, usize) {
    if all || chats.len() <= n {
        return (chats.to_vec(), 0);
    }
    let mut shown: Vec<&ChatSummary> = chats.iter().take(n).copied().collect();
    if let Some(c) = chats.iter().skip(n).find(|c| Some(c.id.as_str()) == current) {
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
/// case) in the accent color.
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
    (ui.text_style_height(&egui::TextStyle::Body) + 12.0).max(ROW_H).round()
}

/// A row's look from its response: pressed, hovered (or its menu open), or idle.
fn row_state(resp: &egui::Response, over: bool, selected: bool) -> glass::RowState {
    if selected {
        glass::RowState::Selected
    } else if resp.is_pointer_button_down_on() {
        glass::RowState::Pressed
    } else if over {
        glass::RowState::Hovered
    } else {
        glass::RowState::Idle
    }
}

/// How long ago `ms` (since the epoch) was, in a few characters: "now", "5m", "4h", "3d",
/// "2w", "5mo", "1y".
pub fn ago(ms: i64, now_ms: i64) -> String {
    let mins = now_ms.saturating_sub(ms).max(0) / 60_000;
    match mins {
        0 => "now".into(),
        m if m < 60 => format!("{m}m"),
        m if m < 60 * 24 => format!("{}h", m / 60),
        m if m < 60 * 24 * 7 => format!("{}d", m / (60 * 24)),
        m if m < 60 * 24 * 30 => format!("{}w", m / (60 * 24 * 7)),
        m if m < 60 * 24 * 365 => format!("{}mo", m / (60 * 24 * 30)),
        m => format!("{}y", m / (60 * 24 * 365)),
    }
}

/// Now, in milliseconds since the epoch (works in the browser too).
pub(crate) fn now_ms() -> i64 {
    web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// What a navigation row shows at its right.
enum Trail {
    None,
    /// Keys, while hovered (and the layout shows shortcuts).
    Shortcut(String),
}

/// A navigation row: an icon and a word, a light pill on hover, its [`Trail`] at the right.
/// `trail_room` keeps space at the right for an action drawn over it.
#[allow(clippy::too_many_arguments)] // one row
fn nav_row(ui: &mut Ui, t: &Tokens, id: Id, paint: icons::Paint, label: &str, trail: Trail, trail_room: f32, narrow: bool) -> egui::Response {
    nav_row_on(ui, t, id, paint, label, trail, trail_room, narrow, false)
}

/// [`nav_row`], highlighted when `selected` (the page it opens is showing).
#[allow(clippy::too_many_arguments)] // a row's parts
fn nav_row_on(
    ui: &mut Ui,
    t: &Tokens,
    id: Id,
    paint: icons::Paint,
    label: &str,
    trail: Trail,
    trail_room: f32,
    narrow: bool,
    selected: bool,
) -> egui::Response {
    let h = row_height(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::hover());
    let resp = ui.interact(rect, id, Sense::click());
    if ui.is_rect_visible(rect) {
        let over = resp.hovered();
        glass::row(ui.painter(), t, rect, row_state(&resp, over, selected));
        let color = if over || selected { t.text_strong } else { t.text };
        paint(ui.painter(), Rect::from_center_size(pos2(rect.left() + PAD + ICON / 2.0, rect.center().y), vec2(ICON, ICON)), color);
        let galley = ui.painter().layout_no_wrap(label.to_owned(), egui::TextStyle::Body.resolve(ui.style()), color);
        let label_right = rect.left() + INDENT + galley.size().x;
        ui.painter().galley(pos2(rect.left() + INDENT, rect.center().y - galley.size().y / 2.0), galley, color);
        let text = match trail {
            Trail::Shortcut(keys) if !narrow && crate::layout::visible(ui.ctx(), "sidebar.shortcuts", over) => Some(keys),
            _ => None,
        };
        if let Some(keys) = text {
            let keys = ui.painter().layout_no_wrap(keys, egui::TextStyle::Small.resolve(ui.style()), t.text_faint);
            let x = rect.right() - PAD - trail_room - keys.size().x;
            if x > label_right + 10.0 {
                ui.painter().galley(pos2(x, rect.center().y - keys.size().y / 2.0), keys, t.text_faint);
            }
        }
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A hover action: an icon in a square that lights up under the pointer. Drawn only when
/// `shown` (in `idle` until the pointer is on it); always takes its click area so the row's
/// layout never changes.
fn action(ui: &mut Ui, t: &Tokens, rect: Rect, id: Id, paint: icons::Paint, shown: bool, idle: egui::Color32) -> egui::Response {
    let resp = ui.interact(rect, id, Sense::click());
    if shown && ui.is_rect_visible(rect) {
        let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
        if resp.hovered() || open {
            let state = if resp.is_pointer_button_down_on() { glass::RowState::Pressed } else { glass::RowState::Selected };
            glass::row(ui.painter(), t, rect.shrink(1.0), state);
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
                self.history.forget(id);
                self.pins.retain(|p| &p.chat != id);
                let key = format!("chat:{id}");
                for s in &mut self.sections {
                    s.items.retain(|i| *i != key);
                }
            }
            ServerMsg::ChatOpened { chat, workspace } => {
                if let Some(c) = chat {
                    self.history.visit(&c.id);
                }
                self.current.clone_from(chat);
                self.current_ws.clone_from(workspace);
            }
            ServerMsg::Workspaces { items, current, .. } => {
                self.workspaces.clone_from(items);
                self.default_ws.clone_from(current);
                let ids: BTreeSet<&String> = items.iter().map(|w| &w.id).collect();
                self.collapsed.retain(|id| id.starts_with("sec:") || id.starts_with("grp:") || ids.contains(id));
                for s in &mut self.sections {
                    s.items.retain(|i| i.strip_prefix("ws:").is_none_or(|w| ids.contains(&w.to_owned())));
                }
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

    /// Can Back / Forward go anywhere?
    pub fn can_go(&self, forward: bool) -> bool {
        if forward { self.history.can_forward() } else { self.history.can_back() }
    }

    /// Back or Forward: the chat to open, if there is one.
    pub fn go(&mut self, forward: bool) -> Option<ClientMsg> {
        self.history.step(forward).map(|id| ClientMsg::OpenChat { id })
    }

    /// How many artifacts the chat in view has, for the Artifacts row.
    pub fn set_artifacts(&mut self, n: usize) {
        self.artifacts = n;
    }

    /// A reply is running in some chat.
    pub fn any_busy(&self) -> bool {
        self.pinned.iter().chain(self.recent.iter()).any(|c| c.busy)
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
        match self.current_workspace() {
            Some(w) => ClientMsg::NewChat { workspace: Some(w.id.clone()) },
            // Not in a project: a chat of its own, when the engine can.
            None if self.unattached => ClientMsg::NewUnattachedChat,
            None => ClientMsg::NewChat { workspace: None },
        }
    }

    /// Start a new chat (the button, the shortcut): closes the list on phones.
    pub fn new_chat(&mut self, narrow: bool) -> ClientMsg {
        self.after_pick(narrow);
        self.new_chat_msg()
    }

    /// Open search: a palette over the window (the list may stay closed).
    pub fn focus_search(&mut self) {
        self.palette = Some(Palette::default());
        // Search finds every app and document: ask for the current list.
        self.requests.push(Request::LibraryList);
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

    /// Scripted runs: open the footer's menu (Spaces, status, Settings, help).
    pub fn open_footer_menu(&mut self) {
        self.open_footer_menu = true;
    }

    /// Scripted runs: open search with `text` typed.
    pub fn open_palette(&mut self, text: &str) {
        self.focus_search();
        if let Some(p) = &mut self.palette {
            text.clone_into(&mut p.query);
        }
    }

    /// What to remember on this device (drafts are the app's: empty here).
    pub fn prefs(&self) -> SidebarPrefs {
        SidebarPrefs {
            collapsed: self.collapsed.clone(),
            drafts: BTreeMap::new(),
            sections: self.sections.clone(),
            group: self.group_by,
            sort: self.sort_by,
            quiet: self.quiet.clone(),
            pins: self.pins.clone(),
            seen: self.seen.clone(),
            space: self.space.clone(),
            ..SidebarPrefs::default()
        }
    }

    pub fn set_prefs(&mut self, prefs: &SidebarPrefs) {
        self.collapsed.clone_from(&prefs.collapsed);
        self.sections.clone_from(&prefs.sections);
        self.group_by = prefs.group;
        self.sort_by = prefs.sort;
        self.quiet.clone_from(&prefs.quiet);
        self.seen.clone_from(&prefs.seen);
        self.space.clone_from(&prefs.space);
        if self.pins.is_empty() && !prefs.pins.is_empty() {
            // Pins this device kept before the list did: share them.
            self.pins.clone_from(&prefs.pins);
            self.list_changed = true;
        }
    }

    /// The list's arrangement (sections, grouping, order).
    pub fn chat_list(&self) -> ChatList {
        ChatList {
            sections: self.sections.clone(),
            group: self.group_by,
            sort: self.sort_by,
            pinned_projects: self.pinned_projects.clone(),
            pinned_artifacts: self
                .pins
                .iter()
                .map(|p| byob_types::layout::ListPin { chat: p.chat.clone(), id: p.id.clone(), title: p.title.clone() })
                .collect(),
            spaces: self.spaces.clone(),
        }
    }

    /// The arrangement another window (or the engine) has: shown here too.
    pub fn set_chat_list(&mut self, list: &ChatList) {
        self.sections.clone_from(&list.sections);
        (self.group_by, self.sort_by) = (list.group, list.sort);
        self.pinned_projects.clone_from(&list.pinned_projects);
        self.spaces.clone_from(&list.spaces);
        if !list.pinned_artifacts.is_empty() || !self.list_changed {
            self.pins = list.pinned_artifacts.iter().map(|p| PinnedArtifact { chat: p.chat.clone(), id: p.id.clone(), title: p.title.clone() }).collect();
        }
    }

    /// The Space in view: the chosen one, else the first (`None` with fewer than two).
    pub fn current_space(&self) -> Option<&byob_types::layout::ListSpace> {
        if self.spaces.len() < 2 {
            return None;
        }
        self.space.as_deref().and_then(|id| self.spaces.iter().find(|s| s.id == id)).or_else(|| self.spaces.first())
    }

    /// The Space the footer shows: the one in view, else the only one, else the default
    /// ("Personal") that everyone starts in.
    pub fn shown_space(&self) -> byob_types::layout::ListSpace {
        self.current_space().or_else(|| self.spaces.first()).cloned().unwrap_or_else(default_space)
    }

    /// The Space in view for the Projects page: its name and its projects (`None` without
    /// Spaces to choose between).
    pub fn space_scope(&self) -> Option<crate::projects::SpaceScope> {
        let space = self.current_space()?;
        let ids = self.workspaces.iter().filter(|w| self.in_space(&format!("ws:{}", w.id))).map(|w| w.id.clone()).collect();
        Some(crate::projects::SpaceScope { name: space.name.clone(), ids })
    }

    /// Is `key` (`ws:<id>`, `chat:<id>`) in the Space in view? What no Space claims is the
    /// first Space's.
    fn in_space(&self, key: &str) -> bool {
        let Some(cur) = self.current_space() else { return true };
        match self.spaces.iter().find(|s| s.items.iter().any(|i| i == key)) {
            Some(owner) => owner.id == cur.id,
            None => self.spaces.first().is_some_and(|f| f.id == cur.id),
        }
    }

    /// Is chat `c` in the Space in view (by itself, or by its project)?
    fn chat_in_space(&self, c: &ChatSummary) -> bool {
        let own = format!("chat:{}", c.id);
        if self.spaces.iter().any(|s| s.items.contains(&own)) {
            return self.in_space(&own);
        }
        c.workspace.as_ref().map_or_else(|| self.in_space(&own), |w| self.in_space(&format!("ws:{w}")))
    }

    /// Switch to Space number `n` (from 0); false if there's none.
    pub fn switch_space(&mut self, n: usize) -> bool {
        if self.spaces.len() < 2 {
            return false;
        }
        let Some(id) = self.spaces.get(n).map(|s| s.id.clone()) else { return false };
        self.space = Some(id);
        true
    }

    /// A new Space (and, the first time, one for everything so far): switch to it and
    /// open its settings.
    pub fn new_space(&mut self) {
        let colors = ["#3b82f6", "#16a34a", "#d97706", "#9333ea", "#e11d48", "#0891b2"];
        if self.spaces.is_empty() {
            self.spaces.push(default_space());
        }
        let n = self.spaces.len();
        let id = format!("space-{}", now_ms());
        let color = colors[n % colors.len()].to_owned();
        self.spaces.push(byob_types::layout::ListSpace { id: id.clone(), name: "New space".into(), color, items: Vec::new() });
        self.space = Some(id.clone());
        self.space_edit = Some((id, "New space".into()));
        self.list_changed = true;
    }

    /// What's new in this Space stays in it: the chat or project you start here.
    fn claim_for_space(&mut self) {
        let Some(cur) = self.current_space().map(|s| s.id.clone()) else { return };
        let mut keys = Vec::new();
        if let Some(c) = &self.current {
            keys.push(c.workspace.as_ref().map_or_else(|| format!("chat:{}", c.id), |w| format!("ws:{w}")));
        } else if let Some(w) = &self.current_ws {
            keys.push(format!("ws:{w}"));
        }
        for key in keys {
            let owned = self.spaces.iter().any(|s| s.items.contains(&key));
            let first = self.spaces.first().is_some_and(|f| f.id == cur);
            if !owned
                && !first
                && let Some(sp) = self.spaces.iter_mut().find(|s| s.id == cur)
            {
                sp.items.push(key);
                self.list_changed = true;
            }
        }
    }

    /// The Space settings dialog: its name, its colour, and deleting it (what it held goes
    /// back to the first Space).
    pub fn space_dialog(&mut self, ctx: &egui::Context, t: &Tokens) {
        let Some((id, mut name)) = self.space_edit.clone() else { return };
        let Some(color) = self.spaces.iter().find(|s| s.id == id).map(|s| s.color.clone()) else {
            self.space_edit = None;
            return;
        };
        let mut close = false;
        let mut delete = false;
        let mut new_color = None;
        let resp = egui::Modal::new(Id::unique("space-settings")).frame(menu::frame(t).inner_margin(egui::Margin::same(20))).show(ctx, |ui| {
            ui.set_width(360.0);
            ui.label(egui::RichText::new("Space settings").size(t.text_size(17.0)).strong().color(t.text_strong));
            ui.label(egui::RichText::new("A Space shows only its own projects and chats.").color(t.text_dim));
            ui.add_space(14.0);
            ui.label(egui::RichText::new("Name").color(t.text_strong));
            ui.add(egui::TextEdit::singleline(&mut name).desired_width(f32::INFINITY).margin(egui::Margin::symmetric(10, 8)));
            ui.add_space(12.0);
            ui.label(egui::RichText::new("Colour").color(t.text_strong));
            ui.horizontal(|ui| {
                for c in ["#3b82f6", "#16a34a", "#d97706", "#9333ea", "#e11d48", "#0891b2", "#64748b"] {
                    let (r, resp) = ui.allocate_exact_size(vec2(26.0, 26.0), Sense::click());
                    let col = space_color(c, t.accent);
                    ui.painter().rect_filled(r, t.round(7.0), col);
                    if c == color {
                        ui.painter().rect_stroke(r.expand(2.0), t.round(8.0), egui::Stroke::new(2.0, t.text_strong), egui::StrokeKind::Outside);
                    }
                    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        new_color = Some(c.to_owned());
                    }
                }
            });
            ui.add_space(18.0);
            ui.horizontal(|ui| {
                if self.spaces.len() > 1
                    && ui
                        .add(egui::Button::new(egui::RichText::new("Delete space").color(t.danger)).frame(false))
                        .on_hover_text("Its projects and chats go back to the first Space")
                        .clicked()
                {
                    delete = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::new(egui::RichText::new("Done").color(t.field_bg))
                                .fill(t.text_strong)
                                .corner_radius(t.round(14.0))
                                .min_size(vec2(80.0, 30.0)),
                        )
                        .clicked()
                    {
                        close = true;
                    }
                });
            });
        });
        if let Some(sp) = self.spaces.iter_mut().find(|s| s.id == id) {
            if sp.name != name.trim() && !name.trim().is_empty() {
                name.trim().clone_into(&mut sp.name);
                self.list_changed = true;
            }
            if let Some(c) = new_color {
                sp.color = c;
                self.list_changed = true;
            }
        }
        if delete {
            self.spaces.retain(|s| s.id != id);
            if self.spaces.len() < 2 {
                self.spaces.clear();
            }
            self.space = None;
            self.list_changed = true;
            close = true;
        }
        self.space_edit = if close || resp.should_close() { None } else { Some((id, name)) };
    }

    /// Pin or unpin project `id` (shared with every window).
    pub fn toggle_project_pin(&mut self, id: &str) {
        if let Some(i) = self.pinned_projects.iter().position(|p| p == id) {
            self.pinned_projects.remove(i);
        } else {
            self.pinned_projects.push(id.to_owned());
        }
        self.list_changed = true;
    }

    /// A reply came in `chat` that you haven't seen (not the chat in view).
    pub fn is_unread(&self, chat: &ChatSummary) -> bool {
        if self.current.as_ref().is_some_and(|c| c.id == chat.id) || chat.busy || self.started == 0 {
            return false;
        }
        match self.seen.get(&chat.id) {
            Some(&at) => chat.updated_at > at,
            None => chat.updated_at > self.started,
        }
    }

    /// The newest chat of project `id`, if it has one.
    pub fn latest_chat_in(&self, id: &str) -> Option<String> {
        self.recent.iter().filter(|c| c.workspace.as_deref() == Some(id) && c.archived_at.is_none()).max_by_key(|c| c.updated_at).map(|c| c.id.clone())
    }

    /// One of project `id`'s chats has a reply you haven't seen.
    fn project_unread(&self, id: &str) -> bool {
        self.recent.iter().filter(|c| c.workspace.as_deref() == Some(id) && c.archived_at.is_none()).any(|c| self.is_unread(c))
    }

    /// The chat in view has been seen as it is now.
    fn mark_seen(&mut self) {
        if self.started == 0 {
            self.started = self.now_ms;
        }
        let Some(c) = &self.current else { return };
        let at = self.pinned.iter().chain(self.recent.iter()).find(|x| x.id == c.id).map_or(c.updated_at, |x| x.updated_at);
        if self.seen.get(&c.id) != Some(&at) {
            self.seen.insert(c.id.clone(), at);
            if self.seen.len() > 600 {
                // Keep the newest: drop the oldest hundred.
                let mut by_time: Vec<(String, i64)> = self.seen.iter().map(|(k, v)| (k.clone(), *v)).collect();
                by_time.sort_by_key(|(_, v)| *v);
                for (k, _) in by_time.into_iter().take(100) {
                    self.seen.remove(&k);
                }
            }
        }
    }

    /// The arrangement, if it changed here since the last call (for the app to share).
    pub fn take_list_change(&mut self) -> Option<ChatList> {
        std::mem::take(&mut self.list_changed).then(|| self.chat_list())
    }

    /// Mark the arrangement as changed here (e.g. to share sections made before the engine
    /// kept them).
    pub fn share_list(&mut self) {
        self.list_changed = true;
    }

    /// The section `key` (`chat:<id>`, `ws:<id>`) is in, if any.
    fn section_of(&self, key: &str) -> Option<&str> {
        self.sections.iter().find(|s| s.items.iter().any(|i| i == key)).map(|s| s.id.as_str())
    }

    /// Put `key` in section `to` (out of any other), or in none.
    fn move_to_section(&mut self, key: &str, to: Option<&str>) {
        self.list_changed = true;
        for s in &mut self.sections {
            s.items.retain(|i| i != key);
        }
        if let Some(s) = to.and_then(|id| self.sections.iter_mut().find(|s| s.id == id)) {
            s.items.push(key.to_owned());
        }
    }

    /// A new section (holding `key`, if given), its name being edited.
    fn new_section(&mut self, key: Option<&str>) {
        let id = format!("s{}-{}", self.now_ms, self.sections.len());
        self.sections.push(Section { id: id.clone(), name: "New section".into(), items: Vec::new() });
        self.list_changed = true;
        if let Some(k) = key {
            self.move_to_section(k, Some(&id));
        }
        self.renaming_section = Some((id, "New section".into()));
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

    /// Draw the list. Returns messages for the engine. On a wide screen it sits on a glass
    /// card filling `ui`; on a phone it is the whole screen, without the card.
    pub fn show(&mut self, ui: &mut Ui, t: &Tokens, narrow: bool) -> Vec<ClientMsg> {
        self.rects = Rects::default();
        self.now_ms = now_ms();
        self.mark_seen();
        self.claim_for_space();
        // In a Space, the list shows only what's in it: draw from filtered copies.
        let full = (self.current_space().is_some()).then(|| {
            let keep_ws: Vec<WorkspaceSummary> = self.workspaces.iter().filter(|w| self.in_space(&format!("ws:{}", w.id))).cloned().collect();
            let keep_pinned: Vec<ChatSummary> = self.pinned.iter().filter(|c| self.chat_in_space(c)).cloned().collect();
            let keep_recent: Vec<ChatSummary> = self.recent.iter().filter(|c| self.chat_in_space(c)).cloned().collect();
            (
                std::mem::replace(&mut self.workspaces, keep_ws),
                std::mem::replace(&mut self.pinned, keep_pinned),
                std::mem::replace(&mut self.recent, keep_recent),
            )
        });
        let out = self.show_list(ui, t, narrow);
        if let Some((ws, pinned, recent)) = full {
            (self.workspaces, self.pinned, self.recent) = (ws, pinned, recent);
        }
        out
    }

    /// [`Self::show`] for the lists as they are (filtered to the Space in view).
    fn show_list(&mut self, ui: &mut Ui, t: &Tokens, narrow: bool) -> Vec<ClientMsg> {
        let mut out = Vec::new();
        if !narrow {
            let full = ui.max_rect();
            glass::panel(ui.painter(), t, full);
            // The top bar: the window controls (beside the macOS window buttons when the
            // title bar is the app's), and a handle to move the window by.
            let (bar, _) = ui.allocate_exact_size(vec2(full.width(), top_bar_height(ui, self.titlebar)), Sense::hover());
            if self.titlebar.is_some() {
                window_drag(ui, bar, Id::unique("sidebar-drag"));
            }
            self.window_controls(ui, t, bar, &mut out);
        }
        #[allow(clippy::cast_possible_truncation)] // a few points
        let margin = if narrow { egui::Margin::ZERO } else { egui::Margin { left: SIDE_PAD as i8, right: SIDE_PAD as i8, top: 2, bottom: SIDE_PAD as i8 } };
        egui::Frame::NONE.inner_margin(margin).show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            // Settings at the bottom, always visible below the scrolling list.
            egui::Panel::bottom("sidebar-footer")
                .show_separator_line(false)
                .frame(egui::Frame::NONE.inner_margin(egui::Margin { left: 0, right: 0, top: 6, bottom: 0 }))
                .show(ui, |ui| {
                    let r = ui.max_rect();
                    glass::hairline(ui.painter(), t, r.top() - 3.0, r.left() + PAD, r.right() - PAD);
                    // One row, one button: is byob ready, and where it runs. Never the model's
                    // name here (it's in the menu); the menu has everything else.
                    let h = row_height(ui);
                    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::hover());
                    if let Some(st) = self.status.clone() {
                        // Always the Space you're in (the default one until you make others).
                        let shown = self.shown_space();
                        let space = Some((shown.name.clone(), space_color(&shown.color, t.accent)));
                        let r = ui.put(row, |ui: &mut Ui| status_row(ui, t, &st, space.as_ref()));
                        self.rects.status = r.rect;
                        self.rects.settings = r.rect;
                        let r = crate::tip::note(r, t, &st.state, &format!("{} · {}", st.model, st.place));
                        self.footer_menu(&r, t, &st, narrow);
                    } else {
                        let gear = ui.put(row, |ui: &mut Ui| icons::icon_button(ui, t, icons::gear, Some("Settings"), false));
                        self.rects.settings = gear.rect;
                        if gear.clicked() {
                            self.requests.push(Request::AppSettings);
                            self.after_pick(narrow);
                        }
                    }
                });
            ui.spacing_mut().item_spacing.y = 2.0;
            self.new_chat_button(ui, t, narrow, &mut out);
            let keys = ui.ctx().format_shortcut(&PALETTE_SHORTCUT);
            let search = nav_row(ui, t, Id::unique("sidebar-search"), icons::search, "Search", Trail::Shortcut(keys.clone()), 0.0, narrow);
            self.rects.search = search.rect;
            if crate::tip::keys(search, t, &[("Search chats, projects, artifacts", &[keys.as_str()])]).clicked() {
                self.focus_search();
            }
            let page = self.page;
            let projects = nav_row_on(ui, t, Id::unique("sidebar-projects"), icons::folder, "Projects", Trail::None, 0.0, narrow, page == NavPage::Projects);
            if crate::tip::note(projects, t, "Projects", "Every project in one place: open, pin, create").clicked() {
                self.requests.push(Request::Projects);
                self.after_pick(narrow);
            }
            let apps = nav_row_on(ui, t, Id::unique("sidebar-apps"), icons::apps_grid, "Apps", Trail::None, 0.0, narrow, page == NavPage::Apps);
            if crate::tip::note(apps, t, "Apps", "What byob built for you: open and use them, or take them out").clicked() {
                self.requests.push(Request::Apps);
                self.after_pick(narrow);
            }
            let art = nav_row_on(ui, t, Id::unique("sidebar-artifacts"), icons::pages, "Library", Trail::None, 0.0, narrow, page == NavPage::Library);
            self.rects.artifacts = art.rect;
            if crate::tip::note(art, t, "Library", "Documents, spreadsheets, images and charts from every chat").clicked() {
                self.requests.push(Request::Library);
                self.after_pick(narrow);
            }
            let plug = nav_row_on(ui, t, Id::unique("sidebar-plugins"), icons::puzzle_piece, "Plugins", Trail::None, 0.0, narrow, page == NavPage::Plugins);
            if self.plugins_attention {
                // Something needs setting up: a small dot at the row's end.
                ui.painter().circle_filled(pos2(plug.rect.right() - PAD - 4.0, plug.rect.center().y), 3.5, t.warning);
            }
            let plug_tip = if self.plugins_attention { "Something needs setting up" } else { "What byob itself can do: connections and tools" };
            if crate::tip::note(plug, t, "Plugins", plug_tip).clicked() {
                self.requests.push(Request::Plugins);
                self.after_pick(narrow);
            }
            ui.add_space(4.0);
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                match query(&self.search) {
                    Some(q) => self.results(ui, t, &q, narrow, &mut out),
                    None => self.tree(ui, t, narrow, &mut out),
                }
                ui.add_space(8.0);
            });
        });
        self.still_press(ui, &mut out);
        self.drag_end(ui, t, &mut out);
        self.confirm_dialog(ui.ctx(), t, &mut out);
        out
    }

    /// A press on a chat that ends where it began opens it, even when egui took it for a
    /// drag: a press that outlasts its click time (slow frames while a reply streams) comes
    /// back as a drag that never moved, and nothing happened.
    fn still_press(&mut self, ui: &Ui, out: &mut Vec<ClientMsg>) {
        if ui.input(|i| i.pointer.any_down()) {
            return;
        }
        let Some((id, start)) = self.press.take() else { return };
        let still = ui.ctx().pointer_latest_pos().is_some_and(|p| p.distance(start) < 6.0);
        let opened = out.iter().any(|m| matches!(m, ClientMsg::OpenChat { .. }));
        let current = self.current.as_ref().is_some_and(|c| c.id == id);
        if still && !opened && !current {
            self.dragging = None;
            out.push(ClientMsg::OpenChat { id });
        }
    }

    /// What the palette lists for its tab and query, in sections (title, hits). Every word
    /// of the search must match (in any order); the best matches come first. Results are
    /// the Space in view; with a search, what matches elsewhere comes last ("Other Spaces").
    fn palette_hits(&self, p: &Palette) -> Vec<(&'static str, Vec<Hit>)> {
        let q = search_words(&p.query);
        let searching = !q.is_empty();
        let tab = p.tab;
        let all = tab == PaletteTab::All;
        let ws_name = |c: &ChatSummary| c.workspace.as_deref().and_then(|w| self.workspace(w)).map(|w| w.name.clone()).unwrap_or_default();
        // Pinned and recent can list the same chat: once.
        let mut seen = BTreeSet::new();
        let chats: Vec<&ChatSummary> = self.pinned.iter().chain(self.recent.iter()).filter(|c| seen.insert(c.id.as_str())).collect();
        let ranked_chats = |here: bool| -> Vec<Hit> {
            let mut hits: Vec<(u8, &ChatSummary)> = chats
                .iter()
                .filter(|c| self.chat_in_space(c) == here)
                .filter_map(|c| {
                    let project = ws_name(c);
                    rank(&c.title, &[&project], &q).map(|r| (r, *c))
                })
                .collect();
            hits.sort_by_key(|(r, _)| *r);
            hits.into_iter().map(|(_, c)| Hit::Chat(c.clone())).collect()
        };
        let ranked_projects = |here: bool| -> Vec<Hit> {
            let mut hits: Vec<(u8, &WorkspaceSummary)> = self
                .workspaces
                .iter()
                .filter(|w| self.in_space(&format!("ws:{}", w.id)) == here)
                .filter_map(|w| rank(&w.name, &[&w.folder], &q).map(|r| (r, w)))
                .collect();
            hits.sort_by_key(|(r, _)| *r);
            hits.into_iter().map(|(_, w)| Hit::Project(w.clone())).collect()
        };
        // In "All", a section shows its first few and a row for the rest (on its own tab).
        let capped = |mut hits: Vec<Hit>, n: usize, more: PaletteTab| -> Vec<Hit> {
            if all && hits.len() > n {
                let total = hits.len();
                hits.truncate(n);
                hits.push(Hit::More(more, total));
            }
            hits
        };
        let mut out: Vec<(&'static str, Vec<Hit>)> = Vec::new();
        let mut push = |title: &'static str, hits: Vec<Hit>| {
            if !hits.is_empty() {
                out.push((title, hits));
            }
        };
        if matches!(tab, PaletteTab::All | PaletteTab::Chats) {
            if !searching && all {
                let here: Vec<&&ChatSummary> = chats.iter().filter(|c| self.chat_in_space(c)).collect();
                push("Working now", here.iter().filter(|c| c.busy).map(|c| Hit::Chat((**c).clone())).collect());
                push("Recent", here.iter().filter(|c| !c.busy && c.archived_at.is_none()).take(8).map(|c| Hit::Chat((**c).clone())).collect());
            } else {
                let mut hits = ranked_chats(true);
                hits.truncate(PALETTE_MAX);
                push("Chats", capped(hits, 6, PaletteTab::Chats));
            }
        }
        if matches!(tab, PaletteTab::All | PaletteTab::Projects) && (searching || tab == PaletteTab::Projects) {
            push("Projects", capped(ranked_projects(true), 4, PaletteTab::Projects));
        }
        if matches!(tab, PaletteTab::All | PaletteTab::Artifacts) {
            let pinned: Vec<Hit> = self.pins.iter().filter(|a| rank(&a.title, &[], &q).is_some()).map(|a| Hit::Artifact(a.clone())).collect();
            push("Pinned", pinned);
            // Every app and document (not only pinned): with a search, or on its own tab.
            if searching || tab == PaletteTab::Artifacts {
                let pinned: BTreeSet<&str> = self.pins.iter().map(|a| a.id.as_str()).collect();
                let mut hits: Vec<(u8, Hit)> = self
                    .library
                    .iter()
                    .filter(|i| !pinned.contains(i.artifact.id.as_str()))
                    .filter_map(|i| rank(&i.artifact.title, &[&i.chat_title], &q).map(|r| (r, Hit::Item(i.clone()))))
                    .collect();
                hits.sort_by_key(|(r, _)| *r);
                let mut hits: Vec<Hit> = hits.into_iter().map(|(_, h)| h).collect();
                hits.truncate(PALETTE_MAX);
                push("Apps and documents", capped(hits, 6, PaletteTab::Artifacts));
            }
        }
        if all {
            push("Actions", PALETTE_ACTIONS.iter().filter(|a| rank(a, &[], &q).is_some()).map(|a| Hit::Action(a)).collect());
        }
        // Elsewhere: only with a search, and only when there are Spaces to be elsewhere in.
        if searching && self.current_space().is_some() && tab != PaletteTab::Artifacts {
            let mut hits = Vec::new();
            if tab != PaletteTab::Projects {
                hits.extend(ranked_chats(false));
            }
            if tab != PaletteTab::Chats {
                hits.extend(ranked_projects(false));
            }
            hits.truncate(if all { 6 } else { PALETTE_MAX });
            push("Other Spaces", hits);
        }
        out
    }

    /// Open what the palette picked. "Show all" isn't opened: it changes the palette's tab
    /// (the caller does that).
    fn palette_open(&mut self, hit: Hit, out: &mut Vec<ClientMsg>) {
        self.palette = None;
        match hit {
            Hit::Chat(c) => {
                if self.current.as_ref().is_none_or(|cur| cur.id != c.id) {
                    out.push(ClientMsg::OpenChat { id: c.id });
                }
            }
            // A project: its latest chat, or a new one in it.
            Hit::Project(w) => match self.recent.iter().find(|c| c.workspace.as_deref() == Some(w.id.as_str())) {
                Some(c) => out.push(ClientMsg::OpenChat { id: c.id.clone() }),
                None => out.push(ClientMsg::NewChat { workspace: Some(w.id) }),
            },
            Hit::Artifact(a) => self.requests.push(Request::OpenArtifact { chat: a.chat, id: a.id }),
            // From a deleted chat it can't open beside it: the Library has it.
            Hit::Item(i) if i.chat_gone => self.requests.push(Request::Library),
            Hit::Item(i) => self.requests.push(Request::OpenArtifact { chat: i.chat, id: i.artifact.id }),
            Hit::More(..) => {}
            Hit::Action("New chat") => out.push(self.new_chat(false)),
            Hit::Action("New project") => self.requests.push(Request::NewWorkspace),
            Hit::Action("Projects") => self.requests.push(Request::Projects),
            Hit::Action("Library") => self.requests.push(Request::Library),
            Hit::Action("Models and connection") => self.requests.push(Request::Connection),
            Hit::Action(_) => self.requests.push(Request::AppSettings),
        }
    }

    /// Act on a picked row: "Show all" goes to its tab (the palette stays); anything else
    /// opens and the palette closes.
    fn palette_pick(&mut self, p: &mut Palette, hit: Hit, out: &mut Vec<ClientMsg>) -> bool {
        if let Hit::More(tab, _) = hit {
            p.tab = tab;
            p.sel = 0;
            p.moved = true;
            return false;
        }
        self.palette_open(hit, out);
        true
    }

    /// Search over the window: a field, the types, what matches in sections, and the keys
    /// that matter right now. Drawn by the app every frame (the list may be closed).
    pub fn palette_ui(&mut self, ctx: &egui::Context, t: &Tokens) -> Vec<ClientMsg> {
        let mut out = Vec::new();
        let Some(mut p) = self.palette.clone() else { return out };
        // Keys first, so the field doesn't take them. ← → change the type while the field is
        // empty (with text they move the cursor); Tab starts a chat with what's typed, and
        // with nothing typed goes to the next type (Shift+Tab: the one before).
        let empty = p.query.is_empty();
        let typed = !p.query.trim().is_empty();
        // Shift+Tab before Tab: a plain Tab check would take it too (Shift is ignored).
        let (up, down, enter, back_tab, tab_key, esc, left, right) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                i.consume_key(Modifiers::NONE, Key::Enter),
                i.consume_key(Modifiers::SHIFT, Key::Tab),
                i.consume_key(Modifiers::NONE, Key::Tab),
                i.consume_key(Modifiers::NONE, Key::Escape),
                empty && i.consume_key(Modifiers::NONE, Key::ArrowLeft),
                empty && i.consume_key(Modifiers::NONE, Key::ArrowRight),
            )
        });
        if esc {
            self.palette = None;
            return out;
        }
        if tab_key && typed {
            self.requests.push(Request::Compose(p.query.trim().to_owned()));
            self.palette = None;
            return out;
        }
        let back = left || (back_tab && !typed);
        if back || right || (tab_key && !typed) {
            let i = PALETTE_TABS.iter().position(|(k, _)| *k == p.tab).unwrap_or(0);
            let n = PALETTE_TABS.len();
            p.tab = PALETTE_TABS[if back { (i + n - 1) % n } else { (i + 1) % n }].0;
            p.sel = 0;
        }
        let sections = self.palette_hits(&p);
        let flat: Vec<Hit> = sections.iter().flat_map(|(_, h)| h.iter().cloned()).collect();
        if down && !flat.is_empty() {
            p.sel = (p.sel + 1) % flat.len();
            p.moved = true;
        }
        if up && !flat.is_empty() {
            p.sel = (p.sel + flat.len() - 1) % flat.len();
            p.moved = true;
        }
        p.sel = p.sel.min(flat.len().saturating_sub(1));
        if enter && let Some(hit) = flat.get(p.sel).cloned() {
            if self.palette_pick(&mut p, hit, &mut out) {
                return out;
            }
            self.palette = Some(p);
            return out;
        }
        // How many each type finds, on its tab, while searching.
        let counts: Vec<Option<usize>> = PALETTE_TABS
            .iter()
            .map(|(k, _)| {
                (typed && *k != PaletteTab::All).then(|| {
                    let mut q = p.clone();
                    q.tab = *k;
                    self.palette_hits(&q).iter().filter(|(title, _)| *title != "Other Spaces").map(|(_, h)| h.len()).sum()
                })
            })
            .collect();
        let screen = ctx.content_rect();
        let w = (screen.width() - 32.0).clamp(280.0, 620.0);
        let list_h = (screen.height() * 0.5).clamp(160.0, 400.0);
        let words = search_words(&p.query);
        let mut picked = None;
        let modal = egui::Modal::new(Id::unique("search-palette")).frame(menu::frame(t).inner_margin(egui::Margin::same(0))).show(ctx, |ui| {
            ui.set_width(w);
            ui.spacing_mut().item_spacing.y = 0.0;
            // The field: a magnifier, what you type, and clear (or Esc's keycap) at the end.
            egui::Frame::NONE.inner_margin(egui::Margin { left: 16, right: 12, top: 14, bottom: 10 }).show(ui, |ui| {
                ui.horizontal(|ui| {
                    let s = ui.text_style_height(&egui::TextStyle::Heading);
                    let (ir, _) = ui.allocate_exact_size(vec2(s, s), Sense::hover());
                    icons::search(ui.painter(), ir.shrink(1.0), t.text_dim);
                    ui.add_space(4.0);
                    let field_w = ui.available_width() - 40.0;
                    let edit = egui::TextEdit::singleline(&mut p.query)
                        .id(Id::unique("search-palette-field"))
                        .frame(egui::Frame::NONE)
                        .hint_text("Search chats, projects, apps and documents")
                        .font(egui::TextStyle::Heading)
                        .char_limit(200)
                        .desired_width(field_w);
                    let r = ui.add(edit);
                    if !r.has_focus() {
                        r.request_focus();
                    }
                    if r.changed() {
                        p.sel = 0;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if p.query.is_empty() {
                            crate::tip::keycap(ui, t, "Esc");
                        } else if icons::icon_button(ui, t, icons::cross, None, false).on_hover_text("Clear").clicked() {
                            p.query.clear();
                            p.sel = 0;
                        }
                    });
                });
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    for ((k, name), count) in PALETTE_TABS.iter().zip(&counts) {
                        if palette_tab(ui, t, name, *count, p.tab == *k).clicked() {
                            p.tab = *k;
                            p.sel = 0;
                        }
                    }
                });
            });
            let r = ui.max_rect();
            glass::hairline(ui.painter(), t, ui.cursor().top(), r.left(), r.right());
            // The results.
            egui::ScrollArea::vertical().max_height(list_h).min_scrolled_height(list_h).auto_shrink([false, false]).show(ui, |ui| {
                egui::Frame::NONE.inner_margin(egui::Margin::symmetric(8, 6)).show(ui, |ui| {
                    if flat.is_empty() {
                        palette_empty(ui, t, &p);
                    }
                    let mut n = 0;
                    for (title, hits) in &sections {
                        ui.add_space(6.0);
                        let (hr, _) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::hover());
                        let g = ui.painter().layout_no_wrap((*title).to_owned(), egui::TextStyle::Small.resolve(ui.style()), t.text_faint);
                        ui.painter().galley(pos2(hr.left() + 8.0, hr.bottom() - g.size().y - 4.0), g, t.text_faint);
                        for hit in hits {
                            let selected = n == p.sel;
                            let r = self.palette_row(ui, t, hit, selected, &words);
                            if selected && p.moved {
                                r.scroll_to_me(None);
                            }
                            if r.hovered() && ui.input(|i| i.pointer.delta() != egui::Vec2::ZERO) {
                                p.sel = n;
                            }
                            if r.clicked() {
                                picked = Some(hit.clone());
                            }
                            n += 1;
                        }
                    }
                    ui.add_space(6.0);
                });
            });
            p.moved = false;
            let top = ui.cursor().top();
            glass::hairline(ui.painter(), t, top, r.left(), r.right());
            // The keys that do something right now.
            egui::Frame::NONE.inner_margin(egui::Margin { left: 16, right: 16, top: 8, bottom: 10 }).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let hint = |ui: &mut Ui, keys: &[&str], label: &str| {
                        for k in keys {
                            crate::tip::keycap(ui, t, k);
                        }
                        ui.label(egui::RichText::new(label).small().color(t.text_dim));
                        ui.add_space(12.0);
                    };
                    if !flat.is_empty() {
                        hint(ui, &["↑", "↓"], "Move");
                        hint(ui, &[crate::tip::ENTER], "Open");
                    }
                    if typed {
                        hint(ui, &["Tab"], "Ask in a new chat");
                    } else {
                        hint(ui, &["←", "→"], "Change type");
                    }
                });
            });
        });
        if modal.should_close() {
            self.palette = None;
            return out;
        }
        if let Some(hit) = picked
            && self.palette_pick(&mut p, hit, &mut out)
        {
            return out;
        }
        self.palette = Some(p);
        out
    }

    /// One palette row: an icon, the title (what matched in bold), where it is (dim), and on
    /// the right when it was active, or ↵ when highlighted.
    fn palette_row(&self, ui: &mut Ui, t: &Tokens, hit: &Hit, selected: bool, words: &str) -> egui::Response {
        let now = self.now_ms.max(now_ms());
        let (paint, title, note, when): (icons::Paint, String, String, String) = match hit {
            Hit::Chat(c) => {
                let mut note = c.workspace.as_deref().and_then(|w| self.workspace(w)).map(|w| w.name.clone()).unwrap_or_default();
                if c.archived_at.is_some() {
                    note = if note.is_empty() { "Archived".into() } else { format!("{note} · Archived") };
                }
                let paint: icons::Paint = if c.busy { icons::bolt } else { icons::bubble };
                (paint, c.title.clone(), note, if c.busy { "Working".into() } else { ago(c.updated_at, now) })
            }
            Hit::Project(w) => {
                let chats = chats_label(w.chats);
                (icons::folder, w.name.clone(), format!("{chats} · {}", crate::workspace::short_path(&w.folder, 40)), ago(w.updated_at, now))
            }
            Hit::Artifact(a) => {
                let from = self.pinned.iter().chain(self.recent.iter()).find(|c| c.id == a.chat).map(|c| c.title.clone()).unwrap_or_default();
                (icons::pin, a.title.clone(), from, String::new())
            }
            Hit::Item(i) => {
                // What it is, by its icon; where it is: its project, its chat.
                let paint: icons::Paint = match i.artifact.kind {
                    byob_protocol::ArtifactKind::Html => icons::app_window,
                    byob_protocol::ArtifactKind::Image => icons::image,
                    byob_protocol::ArtifactKind::Chart => icons::chart_column,
                    byob_protocol::ArtifactKind::Markdown => icons::file_text,
                    byob_protocol::ArtifactKind::Text => icons::file_text,
                };
                let project = i.workspace.as_deref().and_then(|w| self.workspace(w)).map(|w| w.name.clone());
                let from = if i.chat_gone { "From a deleted chat".to_owned() } else { i.chat_title.clone() };
                let note = project.map_or(from.clone(), |p| format!("{p} · {from}"));
                (paint, i.artifact.title.clone(), note, String::new())
            }
            Hit::More(tab, total) => {
                let what = match tab {
                    PaletteTab::Projects => "projects",
                    PaletteTab::Artifacts => "apps and documents",
                    _ => "chats",
                };
                (icons::forward, format!("Show all {total} {what}"), String::new(), String::new())
            }
            Hit::Action(name) => (
                match *name {
                    "New chat" => icons::compose,
                    "New project" => icons::folder_plus,
                    "Projects" => icons::folder,
                    "Library" => icons::pages,
                    "Models and connection" => icons::plug,
                    _ => icons::gear,
                },
                (*name).to_owned(),
                String::new(),
                String::new(),
            ),
        };
        let more = matches!(hit, Hit::More(..));
        let h = row_height(ui) + 6.0;
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::click());
        if ui.is_rect_visible(rect) {
            if selected {
                glass::row(ui.painter(), t, rect, glass::RowState::Selected);
            } else if resp.hovered() {
                glass::row(ui.painter(), t, rect, glass::RowState::Hovered);
            }
            let p = ui.painter();
            let strong = if selected { t.text_strong } else { t.text };
            let color = if more { t.text_dim } else { strong };
            paint(
                p,
                Rect::from_center_size(pos2(rect.left() + 8.0 + ICON / 2.0, rect.center().y), vec2(ICON, ICON)),
                if selected { t.text_strong } else { t.text_dim },
            );
            // The right end first (↵ or when); the title takes what is left.
            let small = egui::TextStyle::Small.resolve(ui.style());
            let end = if selected { crate::tip::ENTER.to_owned() } else { when };
            let end_w = if end.is_empty() {
                0.0
            } else {
                let g = p.layout_no_wrap(end, small.clone(), t.text_faint);
                let w = g.size().x;
                p.galley(pos2(rect.right() - 10.0 - w, rect.center().y - g.size().y / 2.0), g, t.text_faint);
                w + 16.0
            };
            let x = rect.left() + 8.0 + ICON + 12.0;
            let body = egui::TextStyle::Body.resolve(ui.style());
            let bold = egui::FontId::new(body.size, crate::theme::bold_or_regular(ui.ctx()));
            let mut job = egui::text::LayoutJob::default();
            let marks = if more { Vec::new() } else { highlights(&title, words) };
            let mut at = 0;
            for (s, e) in marks {
                job.append(&title[at..s], 0.0, egui::TextFormat::simple(body.clone(), color));
                job.append(&title[s..e], 0.0, egui::TextFormat::simple(bold.clone(), t.text_strong));
                at = e;
            }
            job.append(&title[at..], 0.0, egui::TextFormat::simple(body.clone(), color));
            if !note.is_empty() {
                job.append(&format!("   {note}"), 0.0, egui::TextFormat::simple(small, t.text_faint));
            }
            job.wrap = egui::text::TextWrapping::truncate_at_width((rect.right() - end_w - 10.0 - x).max(20.0));
            let g = p.layout_job(job);
            p.galley(pos2(x, rect.center().y - g.size().y / 2.0), g, color);
        }
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    }

    /// A heading that folds what is under it: its name, a chevron, and how many it holds
    /// while folded. `key` is remembered (in `collapsed` when it differs from
    /// `open_by_default`). A dragged chat can land on it when `drop` says so. Returns
    /// whether it is open.
    #[allow(clippy::too_many_arguments)] // a heading: where, what, how many, its default, its drop
    fn fold(&mut self, ui: &mut Ui, t: &Tokens, key: &str, name: &str, count: usize, open_by_default: bool, drop: Option<Drop>) -> bool {
        ui.add_space(SECTION_GAP);
        let open = open_by_default != self.collapsed.contains(key);
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), SECTION_H), Sense::click());
        let over = ui.rect_contains_pointer(rect);
        self.rects.headings.insert(key.to_owned(), rect);
        if let Some(d) = drop {
            self.drop_target(ui, t, rect, d);
        }
        if ui.is_rect_visible(rect) {
            if over && self.dragging.is_none() {
                glass::row(ui.painter(), t, rect, row_state(&resp, true, false));
            }
            let color = if over { t.text_dim } else { t.text_faint };
            let galley = ui.painter().layout_no_wrap(name.to_owned(), egui::TextStyle::Small.resolve(ui.style()), color);
            let pos = pos2(rect.left() + PAD, rect.center().y - galley.size().y / 2.0);
            let after = pos.x + galley.size().x + 5.0;
            ui.painter().galley(pos, galley, color);
            let chevron = Rect::from_center_size(pos2(after + CHEVRON / 2.0, rect.center().y), vec2(CHEVRON, CHEVRON));
            icons::chevron(ui.painter(), chevron, open, color);
            if !open && count > 0 && !over {
                let n = ui.painter().layout_no_wrap(count.to_string(), egui::TextStyle::Small.resolve(ui.style()), t.text_faint);
                ui.painter().galley(pos2(rect.right() - PAD - n.size().x, rect.center().y - n.size().y / 2.0), n, t.text_faint);
            }
        }
        let tip = if open { "Click to fold" } else { "Click to unfold" };
        if crate::tip::note(resp.on_hover_cursor(egui::CursorIcon::PointingHand), t, name, tip).clicked() {
            self.toggle_collapsed(key);
        }
        open
    }

    /// Somewhere a dragged chat can land: lit while one hovers it.
    fn drop_target(&mut self, ui: &Ui, t: &Tokens, rect: Rect, drop: Drop) {
        if self.dragging.is_some() && ui.rect_contains_pointer(rect) {
            ui.painter().rect_stroke(rect, t.round(6.0), egui::Stroke::new(1.5, t.accent), egui::StrokeKind::Inside);
            ui.painter().rect_filled(rect, t.round(6.0), t.accent.gamma_multiply(0.12));
        }
        self.drops.push((rect, drop));
    }

    /// While a chat is dragged: its title follows the pointer; let go over a target to act.
    fn drag_end(&mut self, ui: &Ui, t: &Tokens, out: &mut Vec<ClientMsg>) {
        let targets = std::mem::take(&mut self.drops);
        let before = std::mem::replace(&mut self.drops_before, targets);
        let Some((id, title)) = self.dragging.clone() else { return };
        let pointer = ui.ctx().pointer_latest_pos();
        if ui.input(|i| i.pointer.any_down()) {
            if let Some(p) = pointer {
                let layer = egui::LayerId::new(egui::Order::Tooltip, Id::unique("sidebar-drag-ghost"));
                let painter = ui.ctx().layer_painter(layer);
                let g = painter.layout_no_wrap(title, egui::TextStyle::Body.resolve(ui.style()), t.text_strong);
                let r = Rect::from_min_size(p + vec2(12.0, 8.0), g.size() + vec2(16.0, 8.0));
                painter.rect_filled(r, t.round(6.0), menu::frame(t).fill);
                painter.rect_stroke(r, t.round(6.0), egui::Stroke::new(1.0, t.border), egui::StrokeKind::Inside);
                painter.galley(r.min + vec2(8.0, 4.0), g, t.text_strong);
            }
            ui.ctx().request_repaint();
            return;
        }
        // Let go.
        self.dragging = None;
        let Some(p) = pointer else { return };
        let Some(drop) = before.iter().chain(self.drops_before.iter()).find(|(r, _)| r.contains(p)).map(|(_, d)| d.clone()) else { return };
        let chat = self.pinned.iter().chain(self.recent.iter()).find(|c| c.id == id).cloned();
        let key = format!("chat:{id}");
        let archived = chat.as_ref().is_some_and(|c| c.archived_at.is_some());
        let pinned = chat.as_ref().is_some_and(|c| c.pinned_at.is_some());
        match drop {
            Drop::Pin => {
                if archived {
                    out.push(ClientMsg::ArchiveChat { id: id.clone(), archived: false });
                }
                if !pinned {
                    out.push(ClientMsg::PinChat { id, pinned: true });
                }
            }
            Drop::Section(sec) => self.move_to_section(&key, Some(&sec)),
            Drop::Archive => {
                if !archived {
                    out.push(ClientMsg::ArchiveChat { id, archived: true });
                }
            }
            Drop::Chats => {
                self.move_to_section(&key, None);
                if pinned {
                    out.push(ClientMsg::PinChat { id: id.clone(), pinned: false });
                }
                if archived {
                    out.push(ClientMsg::ArchiveChat { id, archived: false });
                }
            }
        }
    }

    /// A pinned artifact: its icon and title; a click opens it in its chat.
    fn artifact_row(&mut self, ui: &mut Ui, t: &Tokens, pin: &PinnedArtifact, narrow: bool) {
        let id = Id::unique(("sidebar-artifact", &pin.id));
        let r = nav_row(ui, t, id, icons::artifacts, &pin.title, Trail::None, 0.0, narrow);
        let chat = self.pinned.iter().chain(self.recent.iter()).find(|c| c.id == pin.chat).map_or_else(String::new, |c| format!("From “{}”", c.title));
        let r = crate::tip::note(r, t, &pin.title, if chat.is_empty() { "Opens it beside its chat" } else { &chat });
        if r.clicked() {
            self.requests.push(Request::OpenArtifact { chat: pin.chat.clone(), id: pin.id.clone() });
            self.after_pick(narrow);
        }
        let mut unpin = false;
        menu::context(&r, t, 160.0).show(|ui| {
            menu::tighten(ui);
            if menu::item(ui, t, Some(icons::pin), "Unpin", Some("P")).clicked() || menu::key(ui, Key::P) {
                unpin = true;
                ui.close();
            }
        });
        if unpin {
            self.unpin_artifact(&pin.id);
        }
    }

    /// Pin an artifact to the top of the list (or keep it there with a new title).
    pub fn pin_artifact(&mut self, chat: &str, id: &str, title: &str) {
        self.list_changed = true;
        match self.pins.iter_mut().find(|p| p.id == id) {
            Some(p) => p.title = title.to_owned(),
            None => self.pins.push(PinnedArtifact { chat: chat.to_owned(), id: id.to_owned(), title: title.to_owned() }),
        }
    }

    pub fn unpin_artifact(&mut self, id: &str) {
        self.list_changed = true;
        self.pins.retain(|p| p.id != id);
    }

    pub fn artifact_pinned(&self, id: &str) -> bool {
        self.pins.iter().any(|p| p.id == id)
    }

    /// The pending destructive action's dialog, if one is waiting.
    fn confirm_dialog(&mut self, ctx: &egui::Context, t: &Tokens, out: &mut Vec<ClientMsg>) {
        let Some(pending) = self.confirm.as_mut() else { return };
        let id = Id::unique("sidebar-confirm");
        let choice = match pending {
            Pending::DeleteChat { title, .. } => menu::Confirm {
                title: "Delete this chat?",
                body: &format!("\u{201c}{title}\u{201d} and its messages are deleted; what was made in it stays in your Library. This can't be undone."),
                action: "Delete",
                check: None,
            }
            .show(ctx, t, id),
            Pending::RemoveWorkspace { name, chats, clean, .. } => menu::Confirm {
                title: "Remove this project?",
                body: &match *chats {
                    0 => format!("\u{201c}{name}\u{201d} is removed from byob. It has no chats; the folder and its files stay."),
                    1 => format!("\u{201c}{name}\u{201d} and its chat are removed from byob. The folder and its files stay."),
                    n => format!("\u{201c}{name}\u{201d} and its {n} chats are removed from byob. The folder and its files stay."),
                },
                action: "Remove",
                check: Some(("Also delete what byob keeps for it elsewhere: installed packages, its container", clean)),
            }
            .show(ctx, t, id),
        };
        match choice {
            menu::Choice::Waiting => {}
            menu::Choice::Cancel => self.confirm = None,
            menu::Choice::Confirm => match self.confirm.take() {
                Some(Pending::DeleteChat { id, .. }) => out.push(ClientMsg::DeleteChat { id }),
                Some(Pending::RemoveWorkspace { id, clean, .. }) => out.push(ClientMsg::RemoveWorkspace { id, clean }),
                None => {}
            },
        }
    }

    /// The list toggle, Back and Forward, in the top bar `bar` (where the header shows them
    /// while the list is closed), after the window buttons when the title bar is ours.
    fn window_controls(&mut self, ui: &mut Ui, t: &Tokens, bar: Rect, out: &mut Vec<ClientMsg>) {
        let size = (bar.height() - 4.0).min(crate::icons::button_height(ui) + 2.0);
        let row = Rect::from_center_size(bar.center(), vec2(bar.width(), size));
        let mut x = bar.left() + self.titlebar.map_or(SIDE_PAD + 2.0, |tb| tb.left);
        let (back_keys, forward_keys) = (ui.ctx().format_shortcut(&BACK_SHORTCUT), ui.ctx().format_shortcut(&FORWARD_SHORTCUT));
        let toggle_keys = ui.ctx().format_shortcut(&TOGGLE_SHORTCUT);
        let on_page = self.page != NavPage::None;
        let back_tip = if on_page { "Back to the chat" } else { "Back to the last chat" };
        let (toggle, back, forward) = {
            let mut b = |paint: icons::Paint, enabled: bool, tip: String, name: &str| {
                let r = window_button(
                    ui,
                    t,
                    Rect::from_min_size(pos2(x, row.top()), vec2(row.height(), row.height())),
                    Id::unique(("sidebar-window", name)),
                    paint,
                    enabled,
                );
                x += row.height() + 2.0;
                if tip.is_empty() { r } else { r.on_hover_text(tip) }
            };
            let toggle = b(icons::sidebar, true, String::new(), "toggle");
            let back = b(icons::back, on_page || self.history.can_back(), String::new(), "back");
            let forward = b(icons::forward, self.history.can_forward(), String::new(), "forward");
            let toggle = crate::tip::keys(toggle, t, &[("Hide sidebar", &[toggle_keys.as_str()])]);
            let back = crate::tip::keys(back, t, &[(back_tip, &[back_keys.as_str()])]);
            let forward = crate::tip::keys(forward, t, &[("Forward", &[forward_keys.as_str()])]);
            (toggle, back, forward)
        };
        (self.rects.toggle, self.rects.back, self.rects.forward) = (toggle.rect, back.rect, forward.rect);
        if toggle.clicked() {
            self.toggle();
        }
        // Back from a page returns to the chat; otherwise through the chats opened.
        if back.clicked() && self.page != NavPage::None {
            self.requests.push(Request::LeavePage);
        } else {
            for (resp, fwd) in [(back, false), (forward, true)] {
                if resp.clicked()
                    && let Some(msg) = self.go(fwd)
                {
                    out.push(msg);
                }
            }
        }
    }

    /// New chat, as a row: a chat in the project in view, else one of its own. Where it
    /// works can still change under its field until its first message.
    fn new_chat_button(&mut self, ui: &mut Ui, t: &Tokens, narrow: bool, out: &mut Vec<ClientMsg>) {
        let keys = ui.ctx().format_shortcut(&new_chat_shortcut());
        // The shortcut ends where the other rows' end (⌘K under it), moving left only while
        // the ▾ shows (on hover, or its menu open).
        let caret_id = Id::unique("sidebar-new-chat-more");
        let caret_open = egui::Popup::is_id_open(ui.ctx(), caret_id.with("popup"));
        let row = Rect::from_min_size(ui.cursor().min, vec2(ui.available_width(), row_height(ui)));
        let shown = narrow || caret_open || ui.rect_contains_pointer(row);
        let room = if shown { ACTION_W + 4.0 } else { 0.0 };
        let main = nav_row(ui, t, Id::unique("sidebar-new-chat"), icons::compose, "New chat", Trail::Shortcut(keys.clone()), room, narrow);
        self.rects.new_chat = main.rect;
        let label = match self.current_workspace() {
            Some(w) => format!("New chat in {}", w.name),
            None => "New chat".to_owned(),
        };
        // ▾ at the row's end (on hover): start with a file to work on.
        let caret_rect = Rect::from_min_size(pos2(main.rect.right() - ACTION_W - 4.0, main.rect.center().y - ACTION_W / 2.0), vec2(ACTION_W, ACTION_W));
        let main_clicked = crate::tip::keys(main, t, &[(label.as_str(), &[keys.as_str()])]).clicked();
        let caret = action(ui, t, caret_rect, caret_id, icons::chevron_down, shown, t.text_dim);
        let caret = crate::tip::note(caret, t, "More ways to start", "Open a file to work on");
        if main_clicked && !caret.hovered() {
            out.push(self.new_chat(narrow));
        }
        menu::below(&caret, t, 230.0).show(|ui| {
            menu::tighten(ui);
            if menu::item(ui, t, Some(icons::compose), "New chat", Some(keys.as_str())).clicked() {
                out.push(self.new_chat(narrow));
                ui.close();
            }
            if menu::item(ui, t, Some(icons::paperclip), "Open a file…", None)
                .on_hover_text("A spreadsheet, a page, a document: work on it in a new chat")
                .clicked()
            {
                self.requests.push(Request::OpenFile);
                self.after_pick(narrow);
                ui.close();
            }
        });
    }

    /// The list's toolbar: how it is arranged (a click opens the view menu), the view menu,
    /// and the one "+" for everything new: a chat, a project, a section.
    /// On a group's heading, while the pointer is on it: ≡ (how the list is shown and sorted,
    /// a new section) and + (what that group makes: a chat, or a project).
    fn head_tools(&mut self, ui: &mut Ui, t: &Tokens, key: &str, chats: bool, narrow: bool, out: &mut Vec<ClientMsg>) {
        let Some(rect) = self.rects.headings.get(key).copied() else { return };
        let plus_rect = Rect::from_min_size(pos2(rect.right() - ACTION_W - 4.0, rect.center().y - ACTION_W / 2.0), vec2(ACTION_W, ACTION_W));
        let view_rect = plus_rect.translate(vec2(-ACTION_W - 2.0, 0.0));
        let view_id = Id::unique(("sidebar-view", key));
        let menu_open = egui::Popup::is_id_open(ui.ctx(), view_id.with("popup"));
        let shown = narrow || menu_open || ui.rect_contains_pointer(rect) || self.force_hover;
        let view = action(ui, t, view_rect, view_id, icons::funnel, shown, t.text_dim);
        let view = crate::tip::note(view, t, "Show and sort", "How the list is grouped and sorted, and a new section");
        let (plus_tip, plus_sub) = if chats { ("New chat", "Not in a project") } else { ("New project", "Open a folder, or start an empty one") };
        let plus = action(ui, t, plus_rect, Id::unique(("sidebar-plus", key)), icons::plus, shown, t.text_dim);
        let plus = crate::tip::note(plus, t, plus_tip, plus_sub);
        self.rects.view = view.rect;
        self.rects.plus = plus.rect;
        self.rects.new_chat_caret = plus.rect;
        if std::mem::take(&mut self.open_new_menu) {
            egui::Popup::open_id(ui.ctx(), egui::Popup::default_response_id(&view));
        }
        if plus.clicked() {
            if chats {
                out.push(if self.unattached { ClientMsg::NewUnattachedChat } else { ClientMsg::NewChat { workspace: None } });
                self.after_pick(narrow);
            } else {
                self.requests.push(Request::NewWorkspace);
                self.after_pick(narrow);
            }
        }
        // Two settings, each one row with its current value (its choices in a submenu), then
        // the one action. Reset shows only once something differs from the defaults.
        let group_name = GROUPS.iter().find(|g| g.0 == self.group_by).map_or("", |g| g.1);
        let sort_name = SORTS.iter().find(|o| o.0 == self.sort_by).map_or("", |o| o.1);
        let width = menu::fit(ui, &[("Show", Some("By project  ›")), ("Sort by", Some("Last active  ›")), ("Reset to defaults", None)]);
        menu::below(&view, t, width).show(|ui| {
            menu::tighten(ui);
            let row = crate::chatbar::value_row(ui, t, "Show", group_name).on_hover_text("Group the list by project, by date, or not at all");
            egui::containers::menu::SubMenu::new().config(crate::chatbar::sub_menu_config(t)).show(ui, &row, |ui| {
                ui.set_width(150.0);
                for (g, label) in GROUPS {
                    if crate::chatbar::choice_row(ui, t, label, self.group_by == g).clicked() {
                        self.group_by = g;
                        self.list_changed = true;
                        ui.close();
                    }
                }
            });
            let row = crate::chatbar::value_row(ui, t, "Sort by", sort_name).on_hover_text("Newest activity first, or A to Z");
            egui::containers::menu::SubMenu::new().config(crate::chatbar::sub_menu_config(t)).show(ui, &row, |ui| {
                ui.set_width(150.0);
                for (o, label) in SORTS {
                    if crate::chatbar::choice_row(ui, t, label, self.sort_by == o).clicked() {
                        self.sort_by = o;
                        self.list_changed = true;
                        ui.close();
                    }
                }
            });
            menu::separator(ui, t);
            if menu::item(ui, t, Some(icons::list_plus), "New section", None).on_hover_text("A group of your own for chats and projects").clicked() {
                self.new_section(None);
                ui.close();
            }
            if self.view_changed() && menu::item(ui, t, None, "Reset to defaults", None).clicked() {
                (self.group_by, self.sort_by) = (GroupBy::default(), SortBy::default());
                self.list_changed = true;
                ui.close();
            }
        });
    }

    /// Is the list shown or sorted other than the defaults?
    fn view_changed(&self) -> bool {
        (self.group_by, self.sort_by) != (GroupBy::default(), SortBy::default())
    }

    /// The list without a search: pinned, the toolbar, the person's sections, then the rest
    /// as the view menu says (by project, by date, or one list).
    fn tree(&mut self, ui: &mut Ui, t: &Tokens, narrow: bool, out: &mut Vec<ClientMsg>) {
        self.first_match = None;
        let dragging = self.dragging.is_some();
        let workspaces_all = self.workspaces.clone();
        let recent_all: Vec<ChatSummary> = self.recent.iter().filter(|c| c.archived_at.is_none()).cloned().collect();
        let pinned_groups: Vec<WorkspaceSummary> = self.pinned_projects.iter().filter_map(|id| workspaces_all.iter().find(|w| &w.id == id)).cloned().collect();
        let pinned_n = self.pinned.len() + self.pins.len() + pinned_groups.len();
        if (pinned_n > 0 || dragging) && self.fold(ui, t, "grp:pinned", "Pinned", pinned_n, true, Some(Drop::Pin)) {
            // Pinned projects first (with their chats), then apps and documents, then chats.
            let all_groups = groups(&workspaces_all, &recent_all);
            for w in &pinned_groups {
                if let Some(g) = all_groups.iter().find(|g| g.id() == Some(w.id.as_str())) {
                    self.group(ui, t, g, narrow, out);
                }
            }
            for pin in self.pins.clone() {
                self.artifact_row(ui, t, &pin, narrow);
            }
            for chat in self.pinned.clone() {
                self.chat_row(ui, t, &chat, PAD, None, narrow, out);
            }
            if pinned_n == 0 {
                hint(ui, t, "Drop here to pin", PAD);
            }
        }
        let workspaces = self.workspaces.clone();
        // Archived chats are kept apart, at the bottom.
        let (archived, recent): (Vec<ChatSummary>, Vec<ChatSummary>) = self.recent.iter().cloned().partition(|c| c.archived_at.is_some());
        let mut all = groups(&workspaces, &recent);
        if self.sort_by == SortBy::Name {
            all.sort_by(|a, b| a.workspace.map(|w| w.name.to_lowercase()).cmp(&b.workspace.map(|w| w.name.to_lowercase())));
            for g in &mut all {
                g.chats.sort_by_key(|c| c.title.to_lowercase());
            }
        }
        for sec in self.sections.clone() {
            self.user_section(ui, t, &sec, &all, narrow, out);
        }
        let placed: BTreeSet<String> = self.sections.iter().flat_map(|s| s.items.iter().cloned()).collect();
        match self.group_by {
            GroupBy::Project => self.by_project(ui, t, &all, &placed, narrow, out),
            GroupBy::Date | GroupBy::None => self.by_time(ui, t, &recent, &placed, narrow, out),
        }
        if self.pinned.is_empty() && self.recent.is_empty() && self.workspaces.is_empty() {
            hint(ui, t, "No chats yet", PAD);
        }
        if self.archive && (!archived.is_empty() || dragging) && self.fold(ui, t, "grp:archived", "Archived", archived.len(), false, Some(Drop::Archive)) {
            for chat in &archived {
                self.chat_row(ui, t, chat, PAD, None, narrow, out);
            }
            if archived.is_empty() {
                hint(ui, t, "Drop here to archive", PAD);
            }
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

    /// Grouped by project: chats of their own (quick questions need no folder), then each
    /// project with its chats. What the person put in a section (`placed`) is shown there.
    fn by_project(&mut self, ui: &mut Ui, t: &Tokens, all: &[Group<'_>], placed: &BTreeSet<String>, narrow: bool, out: &mut Vec<ClientMsg>) {
        // Projects first: few and stable. Chats without a folder below: they grow every day.
        let pinned: BTreeSet<String> = self.pinned_projects.iter().cloned().collect();
        let projects: Vec<&Group<'_>> =
            all.iter().filter(|g| g.workspace.is_some_and(|w| !placed.contains(&format!("ws:{}", w.id)) && !pinned.contains(&w.id))).collect();
        if !projects.is_empty() || self.workspaces.is_empty() {
            let open = self.fold(ui, t, "grp:projects", "Projects", projects.len(), true, None);
            self.head_tools(ui, t, "grp:projects", false, narrow, out);
            if open {
                if self.workspaces.is_empty() {
                    hint(ui, t, "None yet: + makes one from a folder", PAD);
                }
                for g in projects {
                    self.group(ui, t, g, narrow, out);
                }
            }
        }
        let other: Vec<&ChatSummary> = all
            .iter()
            .find(|g| g.workspace.is_none())
            .map(|g| g.chats.iter().copied().filter(|c| !placed.contains(&format!("chat:{}", c.id))).collect())
            .unwrap_or_default();
        let drafting_here = self.unattached && self.current.is_none() && self.current_ws.is_none();
        let dragging = self.dragging.is_some();
        // Always "Chats": for someone without projects, it's simply the list.
        let open = self.fold(ui, t, "grp:chats", "Chats", other.len(), true, Some(Drop::Chats));
        self.head_tools(ui, t, "grp:chats", true, narrow, out);
        if open && (!other.is_empty() || drafting_here || dragging) {
            if drafting_here {
                pending_row(ui, t, PAD);
            }
            let all_shown = self.expanded.contains("");
            let current = self.current.as_ref().map(|c| c.id.clone());
            let (shown, hidden) = visible_n(&other, all_shown, current.as_deref(), RECENT_CHATS);
            for chat in shown {
                self.chat_row(ui, t, chat, PAD, None, narrow, out);
            }
            self.more_row(ui, t, "", hidden, other.len(), all_shown, PAD);
        }
    }

    /// Every chat by time (headed Today, Yesterday, This week, Earlier when grouped by
    /// date), whichever project it is in.
    fn by_time(&mut self, ui: &mut Ui, t: &Tokens, recent: &[ChatSummary], placed: &BTreeSet<String>, narrow: bool, out: &mut Vec<ClientMsg>) {
        let mut chats: Vec<&ChatSummary> = recent.iter().filter(|c| !placed.contains(&format!("chat:{}", c.id))).collect();
        if self.sort_by == SortBy::Name {
            chats.sort_by_key(|c| c.title.to_lowercase());
        }
        let all_open = self.fold(ui, t, "grp:chats", "Chats", chats.len(), true, Some(Drop::Chats));
        self.head_tools(ui, t, "grp:chats", true, narrow, out);
        if !all_open {
            return;
        }
        if self.current.is_none() {
            pending_row(ui, t, PAD);
        }
        let mut heading = None;
        let mut open = true;
        for chat in &chats {
            if self.group_by == GroupBy::Date {
                let h = when_heading(chat.updated_at, self.now_ms);
                if heading != Some(h) {
                    let n = chats.iter().filter(|c| when_heading(c.updated_at, self.now_ms) == h).count();
                    open = self.fold(ui, t, &format!("grp:{h}"), h, n, true, Some(Drop::Chats));
                    heading = Some(h);
                }
            }
            if open {
                self.chat_row(ui, t, chat, PAD, None, narrow, out);
            }
        }
    }

    /// One of the person's sections: its heading (a click folds it; "⋯" renames or deletes
    /// it), then what was moved into it, projects with their chats.
    fn user_section(&mut self, ui: &mut Ui, t: &Tokens, sec: &Section, all: &[Group<'_>], narrow: bool, out: &mut Vec<ClientMsg>) {
        ui.add_space(SECTION_GAP);
        let key = format!("sec:{}", sec.id);
        if let Some((_, name)) = self.renaming_section.as_mut().filter(|(id, _)| *id == sec.id) {
            let edit = ui.add_sized(vec2(ui.available_width(), row_height(ui)), egui::TextEdit::singleline(name).align(egui::Align2::LEFT_CENTER));
            if !edit.has_focus() && !edit.lost_focus() {
                edit.request_focus();
            }
            if edit.lost_focus() {
                let name = name.trim().to_owned();
                if !name.is_empty()
                    && let Some(s) = self.sections.iter_mut().find(|s| s.id == sec.id)
                {
                    s.name = name;
                    self.list_changed = true;
                }
                self.renaming_section = None;
            }
            return;
        }
        let collapsed = self.collapsed.contains(&key);
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), SECTION_H + 2.0), Sense::click());
        self.rects.sections.insert(sec.id.clone(), rect);
        self.drop_target(ui, t, rect, Drop::Section(sec.id.clone()));
        let more_rect = Rect::from_min_size(pos2(rect.right() - ACTION_W - 4.0, rect.center().y - ACTION_W / 2.0), vec2(ACTION_W, ACTION_W));
        let more_id = Id::unique(("sidebar-section-more", &sec.id));
        let menu_open = egui::Popup::is_id_open(ui.ctx(), more_id.with("popup"));
        let over = ui.rect_contains_pointer(rect);
        if ui.is_rect_visible(rect) {
            glass::row(ui.painter(), t, rect, row_state(&resp, over || menu_open, false));
            let color = if over { t.text } else { t.text_dim };
            let galley = ui.painter().layout_no_wrap(sec.name.clone(), egui::TextStyle::Small.resolve(ui.style()), color);
            let pos = pos2(rect.left() + PAD, rect.center().y - galley.size().y / 2.0);
            let after = pos.x + galley.size().x + 5.0;
            ui.painter().galley(pos, galley, color);
            let chevron = Rect::from_center_size(pos2(after + CHEVRON / 2.0, rect.center().y), vec2(CHEVRON, CHEVRON));
            icons::disclosure(ui.painter(), chevron, !collapsed, t.text_faint);
        }
        let more = action(ui, t, more_rect, more_id, icons::more, narrow || over || menu_open, t.text_dim).on_hover_text("Section menu");
        let width = menu::fit(ui, &[("Rename", Some("R")), ("Delete section", None)]);
        menu::below(&more, t, width).show(|ui| {
            menu::tighten(ui);
            if menu::item(ui, t, Some(icons::pencil), "Rename", Some("R")).clicked() || menu::key(ui, Key::R) {
                self.renaming_section = Some((sec.id.clone(), sec.name.clone()));
                ui.close();
            }
            menu::separator(ui, t);
            if menu::danger(ui, t, Some(icons::trash), "Delete section", None)
                .on_hover_text("Only the section: its chats and projects go back to the list")
                .clicked()
            {
                self.sections.retain(|s| s.id != sec.id);
                self.list_changed = true;
                ui.close();
            }
        });
        if resp.clicked() {
            self.toggle_collapsed(&key);
        }
        if collapsed {
            return;
        }
        if sec.items.is_empty() {
            hint(ui, t, "Drag chats here, or use ⋯", PAD);
        }
        for item in &sec.items {
            if let Some(id) = item.strip_prefix("ws:") {
                if let Some(g) = all.iter().find(|g| g.id() == Some(id)) {
                    self.group(ui, t, g, narrow, out);
                }
            } else if let Some(id) = item.strip_prefix("chat:")
                && let Some(chat) = self.recent.iter().find(|c| c.id == id && c.archived_at.is_none()).cloned()
            {
                self.chat_row(ui, t, &chat, PAD, None, narrow, out);
            }
        }
    }

    /// "Move to section" in a chat's or project's menu: each section (✓ where it is), a new
    /// section with it, and out of its section.
    fn section_items(&mut self, ui: &mut Ui, t: &Tokens, key: &str) {
        menu::separator(ui, t);
        menu::heading(ui, t, "Section");
        let current = self.section_of(key).map(str::to_owned);
        for sec in self.sections.clone() {
            let here = current.as_deref() == Some(sec.id.as_str());
            if menu::item(ui, t, Some(icons::lines), &sec.name, here.then_some("✓")).clicked() {
                self.move_to_section(key, if here { None } else { Some(&sec.id) });
                ui.close();
            }
        }
        if menu::item(ui, t, Some(icons::plus), "New section with it", None).clicked() {
            self.new_section(Some(key));
            ui.close();
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
                    let busy = g.chats.iter().any(|c| c.busy);
                    self.workspace_row(ui, t, ws, false, busy, Some(q), narrow, out);
                    INDENT
                }
                None => {
                    section(ui, t, "Chats", None);
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
        let busy = g.chats.iter().any(|c| c.busy);
        self.workspace_row(ui, t, ws, collapsed, busy, None, narrow, out);
        if collapsed {
            return;
        }
        let here = self.current_ws.as_deref() == Some(ws.id.as_str());
        // The new chat about to start here: the draft is the form.
        if self.current.is_none() && here {
            pending_row(ui, t, INDENT);
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
    fn workspace_row(
        &mut self,
        ui: &mut Ui,
        t: &Tokens,
        ws: &WorkspaceSummary,
        collapsed: bool,
        busy: bool,
        q: Option<&str>,
        narrow: bool,
        out: &mut Vec<ClientMsg>,
    ) {
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
        let more_rect = Rect::from_min_size(pos2(rect.right() - ACTION_W - 4.0, rect.center().y - ACTION_W / 2.0), vec2(ACTION_W, ACTION_W));
        let plus_rect = more_rect.translate(vec2(-ACTION_W, 0.0));
        let more_id = id.with("more");
        let menu_open = egui::Popup::is_id_open(ui.ctx(), more_id.with("popup")) || egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
        let over = ui.rect_contains_pointer(rect) || forced;
        let shown = narrow || over || menu_open;
        // Phones show every row's actions: quietly, until touched.
        let idle = if over || menu_open { t.text_dim } else { t.text_faint };
        if ui.is_rect_visible(rect) {
            glass::row(ui.painter(), t, rect, row_state(&resp, over || menu_open, false));
            // A folder: what a workspace is. Where it runs is a badge after the name.
            let icon = Rect::from_center_size(pos2(rect.left() + PAD + ICON / 2.0, rect.center().y), vec2(ICON, ICON));
            let strong = here || over || menu_open;
            icons::folder(ui.painter(), icon, if strong { t.text } else { t.text_dim });
            let x = rect.left() + INDENT;
            let badge = !matches!(ws.env, EnvKind::Auto | EnvKind::Host);
            let room = plus_rect.left() - 4.0 - x - CHEVRON - 6.0 - if badge { ICON } else { 0.0 };
            let font = egui::TextStyle::Body.resolve(ui.style());
            let color = if strong { t.text_strong } else { t.text };
            let galley = title_galley(ui, &ws.name, font, color, room, q, t.accent);
            let pos = pos2(x, rect.center().y - galley.size().y / 2.0);
            let mut after = pos.x + galley.size().x + 5.0;
            self.rects.titles.insert(format!("ws:{}", ws.id), Rect::from_min_size(pos, galley.size()));
            ui.painter().galley(pos, galley, color);
            if badge {
                let b = Rect::from_center_size(pos2(after + 6.0, rect.center().y), vec2(12.0, 12.0));
                icons::env(ui.painter(), b, ws.env, icons::env_color(t, ws.env));
                after = b.right() + 4.0;
            }
            // The chevron says it folds: always when folded, else on hover.
            if (collapsed || over || menu_open) && q.is_none() {
                let chevron = Rect::from_center_size(pos2(after + CHEVRON / 2.0, rect.center().y), vec2(CHEVRON, CHEVRON));
                icons::disclosure(ui.painter(), chevron, !collapsed, t.text_faint);
            }
            // One of its chats is replying: a live dot where the actions go, so work shows
            // even while the project is folded.
            if busy && !(narrow || over || menu_open) {
                let pulse = (ui.input(|i| i.time) * 3.0).sin().mul_add(0.25, 0.75);
                #[allow(clippy::cast_possible_truncation)] // 0.5..=1.0
                ui.painter().circle_filled(more_rect.center(), 3.5, t.accent.gamma_multiply(pulse as f32));
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(50));
            } else if !(narrow || over || menu_open) && self.project_unread(&ws.id) {
                // One of its chats has a reply you haven't seen (shows while folded too).
                ui.painter().circle_filled(more_rect.center(), 3.5, t.accent);
            }
        }
        let plus = action(ui, t, plus_rect, id.with("plus"), icons::plus, shown, idle).on_hover_text(format!("New chat in {}", ws.name));
        let more = action(ui, t, more_rect, more_id, icons::more, shown, idle).on_hover_text("Project menu");
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
        let names: Vec<String> = self.sections.iter().map(|s| s.name.clone()).collect();
        let mut items: Vec<(&str, Option<&str>)> = vec![
            ("New chat here", Some("N")),
            ("Rename", Some("R")),
            ("Copy folder path", Some("C")),
            ("New section with it", None),
            ("Project settings", Some("S")),
            ("Remove project", Some("⌫")),
        ];
        items.extend(names.iter().map(|n| (n.as_str(), Some("✓"))));
        let width = menu::fit(ui, &items);
        menu::below(&more, t, width).show(|ui| self.workspace_menu(ui, t, ws, narrow, out));
        menu::context(&resp, t, width).show(|ui| self.workspace_menu(ui, t, ws, narrow, out));
        let resp =
            resp.on_hover_text_at_pointer(format!("{}\n{} chat{} · {}", ws.folder, ws.chats, if ws.chats == 1 { "" } else { "s" }, icons::env_tip(ws.env)));
        if resp.clicked() && q.is_none() {
            self.toggle_collapsed(&ws.id);
        }
    }

    fn workspace_menu(&mut self, ui: &mut Ui, t: &Tokens, ws: &WorkspaceSummary, narrow: bool, out: &mut Vec<ClientMsg>) {
        menu::tighten(ui);
        if menu::item(ui, t, Some(icons::compose), "New chat here", Some("N")).clicked() || menu::key(ui, Key::N) {
            out.push(ClientMsg::NewChat { workspace: Some(ws.id.clone()) });
            self.after_pick(narrow);
            ui.close();
        }
        let pinned = self.pinned_projects.contains(&ws.id);
        if menu::item(ui, t, Some(icons::pin), if pinned { "Unpin" } else { "Pin" }, Some("P")).clicked() || menu::key(ui, Key::P) {
            self.toggle_project_pin(&ws.id);
            ui.close();
        }
        if menu::item(ui, t, Some(icons::pencil), "Rename", Some("R")).clicked() || menu::key(ui, Key::R) {
            self.renaming_ws = Some((ws.id.clone(), ws.name.clone()));
            ui.close();
        }
        if menu::item(ui, t, Some(icons::copy), "Copy folder path", Some("C")).on_hover_text(&ws.folder).clicked() || menu::key(ui, Key::C) {
            ui.ctx().copy_text(ws.folder.clone());
            ui.close();
        }
        self.section_items(ui, t, &format!("ws:{}", ws.id));
        menu::separator(ui, t);
        if menu::item(ui, t, Some(icons::sliders), "Project settings", Some("S")).clicked() || menu::key(ui, Key::S) {
            self.requests.push(Request::WorkspaceSettings { id: ws.id.clone(), remove: false });
            self.after_pick(narrow);
            ui.close();
        }
        menu::separator(ui, t);
        if menu::danger(ui, t, Some(icons::trash), "Remove project", Some("⌫")).clicked() || menu::key(ui, Key::Backspace) {
            self.confirm = Some(Pending::RemoveWorkspace { id: ws.id.clone(), name: ws.name.clone(), chats: ws.chats, clean: false });
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
        let id = Id::unique(("sidebar-chat", &chat.id));
        // Drag a chat onto Pinned, a section, Chats or Archived (see `Drop`).
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::click_and_drag());
        if resp.drag_started() {
            self.dragging = Some((chat.id.clone(), chat.title.clone()));
        }
        let ghost = self.dragging.as_ref().is_some_and(|(d, _)| *d == chat.id);
        let selected = self.current.as_ref().is_some_and(|c| c.id == chat.id);
        let more_rect = Rect::from_min_size(pos2(rect.right() - ACTION_W - 4.0, rect.center().y - ACTION_W / 2.0), vec2(ACTION_W, ACTION_W));
        let more_id = id.with("more");
        let menu_open = egui::Popup::is_id_open(ui.ctx(), more_id.with("popup")) || egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
        let over = ui.rect_contains_pointer(rect) || (self.force_hover && selected);
        let shown = narrow || over || menu_open;
        // Phones show every row's actions: quietly, until touched.
        let idle = if over || menu_open { t.text_dim } else { t.text_faint };
        let mut truncated = false;
        if ui.is_rect_visible(rect) {
            glass::row(ui.painter(), t, rect, row_state(&resp, over || menu_open, selected));
            let x = rect.left() + indent;
            let color = if selected || over { t.text_strong } else { t.text };
            let trail = rect.right() - TRAIL_W;
            let galley = title_galley(ui, &chat.title, egui::TextStyle::Body.resolve(ui.style()), color, trail - 4.0 - x, q, t.accent);
            truncated = galley.elided;
            let pos = pos2(x, rect.center().y - galley.size().y / 2.0);
            self.rects.titles.insert(format!("chat:{}", chat.id), Rect::from_min_size(pos, galley.size()));
            ui.painter().galley(pos, galley, color);
            // The trailing slot: "⋯" while hovered, a dot while replying, else how long ago.
            if !shown {
                if chat.busy {
                    let pulse = (ui.input(|i| i.time) * 3.0).sin().mul_add(0.25, 0.75);
                    #[allow(clippy::cast_possible_truncation)] // 0.5..=1.0
                    ui.painter().circle_filled(more_rect.center(), 3.5, t.accent.gamma_multiply(pulse as f32));
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(50));
                } else if self.is_unread(chat) {
                    // A reply you haven't seen: the only mark a quiet row gets.
                    ui.painter().circle_filled(more_rect.center(), 3.5, t.accent);
                }
            }
        }
        if ghost {
            // The row being dragged stays in place, faded, until it lands.
            ui.painter().rect_filled(rect, t.round(6.0), t.panel_bg.gamma_multiply(0.6));
        }
        let more = action(ui, t, more_rect, more_id, icons::more, shown, idle).on_hover_text("Chat menu");
        if shown {
            self.rects.actions.insert(format!("chat:{}", chat.id), more_rect);
        }
        let names: Vec<String> = self.sections.iter().map(|s| s.name.clone()).collect();
        let mut items: Vec<(&str, Option<&str>)> = vec![
            ("Unpin", Some("P")),
            ("Rename", Some("R")),
            ("Turn into project", Some("T")),
            ("New section with it", None),
            ("Unarchive", Some("A")),
            ("Delete", Some("D")),
        ];
        items.extend(names.iter().map(|n| (n.as_str(), Some("✓"))));
        let width = menu::fit(ui, &items);
        menu::below(&more, t, width).show(|ui| self.chat_menu(ui, t, chat, out));
        menu::context(&resp, t, width).show(|ui| self.chat_menu(ui, t, chat, out));
        let resp = if truncated { resp.on_hover_text(&chat.title) } else { resp };
        let resp = if chat.busy {
            resp.on_hover_text("Replying…")
        } else if self.is_unread(chat) {
            resp.on_hover_text(format!("New reply · {}", ago(chat.updated_at, self.now_ms)))
        } else if truncated {
            resp
        } else {
            resp.on_hover_text(format!("Last active {}", ago(chat.updated_at, self.now_ms)))
        };
        // A press held through slow frames (a reply streaming) comes back as a drag that
        // never moved: that's a click too.
        // Where a press on this row began (see `still_press`).
        if resp.is_pointer_button_down_on() && self.press.is_none() {
            self.press = ui.ctx().pointer_latest_pos().map(|p| (chat.id.clone(), p));
        }
        if resp.clicked() && !selected {
            out.push(ClientMsg::OpenChat { id: chat.id.clone() });
            self.after_pick(narrow);
        }
    }

    /// The same for "⋯" and a right click.
    fn chat_menu(&mut self, ui: &mut Ui, t: &Tokens, chat: &ChatSummary, out: &mut Vec<ClientMsg>) {
        menu::tighten(ui);
        let pinned = chat.pinned_at.is_some();
        if menu::item(ui, t, Some(icons::pin), if pinned { "Unpin" } else { "Pin" }, Some("P")).clicked() || menu::key(ui, Key::P) {
            out.push(ClientMsg::PinChat { id: chat.id.clone(), pinned: !pinned });
            ui.close();
        }
        if menu::item(ui, t, Some(icons::pencil), "Rename", Some("R")).clicked() || menu::key(ui, Key::R) {
            self.renaming = Some((chat.id.clone(), chat.title.clone()));
            ui.close();
        }
        self.section_items(ui, t, &format!("chat:{}", chat.id));
        // A chat of its own can grow into a project: its folder becomes one, named after it.
        if self.unattached
            && chat.workspace.is_none()
            && (menu::item(ui, t, Some(icons::folder_plus), "Turn into project", Some("T"))
                .on_hover_text("Make this chat's folder a project, named after the chat; the chat moves under it")
                .clicked()
                || menu::key(ui, Key::T))
        {
            out.push(ClientMsg::ChatToProject { chat: chat.id.clone() });
            ui.close();
        }
        if self.archive {
            let archived = chat.archived_at.is_some();
            let (label, tip) = if archived { ("Unarchive", "Back to the list") } else { ("Archive", "Out of the way, kept: find it under Archived") };
            if menu::item(ui, t, Some(icons::archive), label, Some("A")).on_hover_text(tip).clicked() || menu::key(ui, Key::A) {
                out.push(ClientMsg::ArchiveChat { id: chat.id.clone(), archived: !archived });
                ui.close();
            }
        }
        menu::separator(ui, t);
        if chat.workspace.is_none()
            && menu::item(ui, t, Some(icons::folder), "Show files in Finder", None).on_hover_text("Where this chat keeps the files it makes").clicked()
        {
            out.push(ClientMsg::RevealChatFolder { chat: chat.id.clone() });
            ui.close();
        }
        if menu::danger(ui, t, Some(icons::trash), "Delete", Some("D")).clicked() || menu::key(ui, Key::D) {
            self.confirm = Some(Pending::DeleteChat { id: chat.id.clone(), title: chat.title.clone() });
            ui.close();
        }
    }
}

/// One of the window controls (toggle, Back, Forward): an icon in a square, a light pill
/// on hover; faint and inert when `enabled` is false.
impl Sidebar {
    /// The footer's menu, opening upward: the brain (status, models), the app (settings),
    /// help. A broken connection puts "Fix connection" first.
    fn footer_menu(&mut self, row: &egui::Response, t: &Tokens, st: &Status, narrow: bool) {
        let broken = st.dot == t.danger;
        if broken && row.clicked() {
            // Can't reach the model: straight to where it's fixed.
            self.requests.push(Request::Connection);
            self.after_pick(narrow);
            return;
        }
        let keys = crate::header::settings_shortcut(&row.ctx);
        let mut pick = None;
        if std::mem::take(&mut self.open_footer_menu) {
            egui::Popup::open_id(&row.ctx, egui::Popup::default_response_id(row));
        }
        // Most personal to most general: the Spaces you work in, how byob is doing, Settings,
        // then help.
        // Wide enough for the status and its model in full ("Can't reach the model  sonnet").
        let width = menu::fit_ctx(&row.ctx, &[(st.state.as_str(), Some(st.model.as_str())), ("Space settings", None)]);
        egui::Popup::menu(row).align(egui::RectAlign::TOP_START).gap(6.0).frame(menu::frame(t)).width(row.rect.width().max(width).max(240.0)).show(|ui| {
            menu::tighten(ui);
            menu::heading(ui, t, "Spaces");
            let current = self.shown_space().id;
            let listed = if self.spaces.is_empty() { vec![default_space()] } else { self.spaces.clone() };
            for (n, sp) in listed.iter().enumerate() {
                let on = current == sp.id;
                let keys = (n < 9 && listed.len() > 1).then(|| format!("⌃{}", n + 1));
                let trail = if on { Some("✓") } else { keys.as_deref() };
                if space_item(ui, t, &sp.name, space_color(&sp.color, t.accent), trail).clicked() {
                    self.space = Some(sp.id.clone());
                    ui.close();
                }
            }
            if menu::item(ui, t, Some(icons::plus), "New space", None)
                .on_hover_text("Work, Personal, a client: each shows only its own projects and chats")
                .clicked()
            {
                self.new_space();
                ui.close();
            }
            if menu::item(ui, t, Some(icons::sliders), "Space settings", None).on_hover_text("Its name and colour").clicked() {
                // The default Space becomes a real one the first time it's edited.
                if self.spaces.is_empty() {
                    self.spaces.push(default_space());
                    self.list_changed = true;
                }
                let name = self.spaces.iter().find(|s| s.id == current).map(|s| s.name.clone()).unwrap_or_default();
                self.space_edit = Some((current.clone(), name));
                ui.close();
            }
            menu::separator(ui, t);
            // How byob is doing, and the model: one row, to where both are set.
            if status_item(ui, t, st).on_hover_text(format!("{} · Models and connection", st.place)).clicked() {
                pick = Some(Request::Connection);
            }
            if menu::item(ui, t, Some(icons::gear), "Settings", Some(keys.as_str())).clicked() {
                pick = Some(Request::AppSettings);
            }
            menu::separator(ui, t);
            if menu::item(ui, t, Some(icons::help_circle), "Get help", None).on_hover_text("Ask byob how to do something").clicked() {
                pick = Some(Request::Help);
            }
            if menu::item(ui, t, Some(icons::megaphone), "Send feedback", None).on_hover_text("Opens byob's issue page in your browser").clicked() {
                pick = Some(Request::Feedback);
            }
            if pick.is_some() {
                ui.close();
            }
        });
        if let Some(r) = pick {
            self.requests.push(r);
            self.after_pick(narrow);
        }
    }
}

pub fn window_button(ui: &mut Ui, t: &Tokens, rect: Rect, id: Id, paint: icons::Paint, enabled: bool) -> egui::Response {
    let resp = ui.interact(rect, id, if enabled { Sense::click() } else { Sense::hover() });
    if ui.is_rect_visible(rect) {
        if enabled && resp.hovered() {
            let state = if resp.is_pointer_button_down_on() { glass::RowState::Pressed } else { glass::RowState::Hovered };
            glass::row(ui.painter(), t, rect, state);
        }
        let color = match (enabled, resp.hovered()) {
            (false, _) => t.text_faint.gamma_multiply(0.7),
            (true, true) => t.text_strong,
            (true, false) => t.text_dim,
        };
        let s = (rect.height() * 0.62).clamp(12.0, 18.0);
        paint(ui.painter(), Rect::from_center_size(rect.center(), vec2(s, s)), color);
    }
    resp
}

/// The heading a chat goes under when the list is grouped by date.
fn when_heading(updated_at: i64, now_ms: i64) -> &'static str {
    const DAY: i64 = 24 * 60 * 60 * 1000;
    match now_ms.saturating_sub(updated_at) {
        age if age < DAY => "Today",
        age if age < 2 * DAY => "Yesterday",
        age if age < 7 * DAY => "This week",
        _ => "Earlier",
    }
}

/// A section heading ("Pinned", "Projects"), with a "+" on its right if `plus` (its
/// tooltip). Returns whether the "+" was clicked. Space above it sets the groups apart.
fn section(ui: &mut Ui, t: &Tokens, name: &str, plus: Option<&str>) -> bool {
    section_plus(ui, t, name, plus).is_some_and(|r| r.clicked())
}

/// [`section`], returning its "+" (if it has one).
fn section_plus(ui: &mut Ui, t: &Tokens, name: &str, plus: Option<&str>) -> Option<egui::Response> {
    ui.add_space(SECTION_GAP);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), SECTION_H), Sense::hover());
    let font = egui::TextStyle::Small.resolve(ui.style());
    let galley = ui.painter().layout_no_wrap(name.to_owned(), font, t.text_faint);
    ui.painter().galley(pos2(rect.left() + PAD, rect.center().y - galley.size().y / 2.0), galley, t.text_faint);
    let tip = plus?;
    let r = Rect::from_min_size(pos2(rect.right() - ACTION_W - 4.0, rect.center().y - ACTION_W / 2.0), vec2(ACTION_W, ACTION_W));
    Some(action(ui, t, r, Id::unique(("sidebar-section", name)), icons::plus, true, t.text_faint).on_hover_text(tip))
}

/// A project's chats as a label: "No chats yet", "1 chat", "3 chats".
pub(crate) fn chats_label(n: u64) -> String {
    if n == 0 { "No chats yet".to_owned() } else { plural(usize::try_from(n).unwrap_or(usize::MAX), "chat") }
}

/// "1 artifact", "3 artifacts".
pub(crate) fn plural(n: usize, word: &str) -> String {
    if n == 1 { format!("1 {word}") } else { format!("{n} {word}s") }
}

/// A Space in the footer menu: its colour square (with its letter), its name, and a trail
/// (✓ for the one in view, else its key).
fn space_item(ui: &mut Ui, t: &Tokens, name: &str, color: egui::Color32, trail: Option<&str>) -> egui::Response {
    let letter: String = name.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
    let mark = |p: &egui::Painter, r: Rect, _: egui::Color32| {
        let sq = r.expand(1.0);
        p.rect_filled(sq, t.round(4.0), color);
        p.text(sq.center(), egui::Align2::CENTER_CENTER, &letter, egui::FontId::proportional(sq.height() * 0.68), egui::Color32::WHITE);
    };
    menu::item_marked(ui, t, &mark, name, trail)
}

/// How byob is doing, as a menu row: the dot, the state, and the model (dimmer) at the end.
fn status_item(ui: &mut Ui, t: &Tokens, st: &Status) -> egui::Response {
    let mark = |p: &egui::Painter, r: Rect, _: egui::Color32| {
        p.circle_filled(r.center(), 4.0, st.dot);
    };
    menu::item_marked(ui, t, &mark, &st.state, Some(&st.model))
}

/// The footer's status: a dot (green: ready; blue: working; amber: waiting or checking;
/// red: can't reach the model), what byob is doing, and the model, dimmer.
/// The footer row: with Spaces, the Space's colour square and name, the status dot at the
/// end; without, the dot, the state and where it runs.
fn status_row(ui: &mut Ui, t: &Tokens, st: &Status, space: Option<&(String, egui::Color32)>) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(ui.available_size(), Sense::click());
    if let Some((name, color)) = space {
        if ui.is_rect_visible(rect) {
            if resp.hovered() {
                glass::row(ui.painter(), t, rect, row_state(&resp, true, false));
            }
            let p = ui.painter();
            let sq = Rect::from_center_size(pos2(rect.left() + PAD + ICON / 2.0, rect.center().y), vec2(ICON + 2.0, ICON + 2.0));
            p.rect_filled(sq, t.round(4.0), *color);
            let letter: String = name.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
            p.text(sq.center(), egui::Align2::CENTER_CENTER, letter, egui::FontId::proportional(ICON * 0.75), egui::Color32::WHITE);
            let mut job = egui::text::LayoutJob::simple_singleline(
                name.clone(),
                egui::TextStyle::Body.resolve(ui.style()),
                if resp.hovered() { t.text_strong } else { t.text },
            );
            job.wrap = egui::text::TextWrapping::truncate_at_width((rect.width() - INDENT - 24.0).max(20.0));
            let g = p.layout_job(job);
            p.galley(pos2(rect.left() + INDENT, rect.center().y - g.size().y / 2.0), g, t.text);
            p.circle_filled(pos2(rect.right() - PAD - 4.0, rect.center().y), 4.0, st.dot);
        }
        return resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    }
    if ui.is_rect_visible(rect) {
        if resp.hovered() {
            glass::row(ui.painter(), t, rect, row_state(&resp, true, false));
        }
        let p = ui.painter();
        let c = pos2(rect.left() + PAD + ICON / 2.0, rect.center().y);
        p.circle_filled(c, 4.0, st.dot);
        let mut job = egui::text::LayoutJob::default();
        let body = egui::TextStyle::Body.resolve(ui.style());
        let small = egui::TextStyle::Small.resolve(ui.style());
        job.append(&st.state, 0.0, egui::TextFormat::simple(body, if resp.hovered() { t.text_strong } else { t.text }));
        // Where it runs, not the model's name (that's in the menu).
        if !st.place.is_empty() {
            job.append(&format!("   {}", st.place), 0.0, egui::TextFormat::simple(small, t.text_faint));
        }
        job.wrap = egui::text::TextWrapping::truncate_at_width((rect.width() - INDENT - 4.0).max(20.0));
        let g = p.layout_job(job);
        p.galley(pos2(rect.left() + INDENT, rect.center().y - g.size().y / 2.0), g, t.text);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// The Space everyone starts in, before making others.
fn default_space() -> byob_types::layout::ListSpace {
    byob_types::layout::ListSpace { id: "space-1".into(), name: "Personal".into(), color: "#3b82f6".into(), items: Vec::new() }
}

/// A Space's colour (`#rrggbb`), or `fallback`.
pub fn space_color(hex: &str, fallback: egui::Color32) -> egui::Color32 {
    let h = hex.strip_prefix('#').unwrap_or(hex);
    let byte = |i: usize| h.get(i..i + 2).and_then(|b| u8::from_str_radix(b, 16).ok());
    match (byte(0), byte(2), byte(4)) {
        (Some(r), Some(g), Some(b)) if h.len() == 6 => egui::Color32::from_rgb(r, g, b),
        _ => fallback,
    }
}

/// A faint line of text in the list.
fn hint(ui: &mut Ui, t: &Tokens, text: &str, indent: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), row_height(ui) - 6.0), Sense::hover());
    let galley = ui.painter().layout_no_wrap(text.to_owned(), egui::TextStyle::Small.resolve(ui.style()), t.text_faint);
    ui.painter().galley(pos2(rect.left() + indent, rect.center().y - galley.size().y / 2.0), galley, t.text_faint);
}

/// A small, quiet clickable line ("Show all (12)", "Load older chats").
fn link_row(ui: &mut Ui, t: &Tokens, text: &str, indent: f32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_height(ui) - 6.0), Sense::click());
    if resp.hovered() {
        glass::row(ui.painter(), t, rect, glass::RowState::Hovered);
    }
    let color = if resp.hovered() { t.text } else { t.text_dim };
    let galley = ui.painter().layout_no_wrap(text.to_owned(), egui::TextStyle::Small.resolve(ui.style()), color);
    ui.painter().galley(pos2(rect.left() + indent, rect.center().y - galley.size().y / 2.0), galley, color);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// The new chat about to start in this workspace (it is created by its first message):
/// selected, as the chat in view.
fn pending_row(ui: &mut Ui, t: &Tokens, indent: f32) {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_height(ui)), Sense::hover());
    glass::row(ui.painter(), t, rect, glass::RowState::Selected);
    let galley = ui.painter().layout_no_wrap("New chat".into(), egui::TextStyle::Body.resolve(ui.style()), t.text_strong);
    ui.painter().galley(pos2(rect.left() + indent, rect.center().y - galley.size().y / 2.0), galley, t.text_strong);
    let when = ui.painter().layout_no_wrap("draft".into(), egui::TextStyle::Small.resolve(ui.style()), t.text_faint);
    ui.painter().galley(pos2(rect.right() - PAD - when.size().x, rect.center().y - when.size().y / 2.0), when, t.text_faint);
    resp.on_hover_text("Starts when you send the first message");
}

#[cfg(test)]
mod tests {
    use super::*;
    use byob_protocol::workspace::EnvKind;

    fn chat(id: &str, updated_at: i64, pinned_at: Option<i64>) -> ChatSummary {
        ChatSummary { id: id.into(), title: id.to_uppercase(), updated_at, pinned_at, busy: false, workspace: None, archived_at: None }
    }

    fn in_ws(id: &str, updated_at: i64, ws: &str) -> ChatSummary {
        ChatSummary { workspace: Some(ws.into()), ..chat(id, updated_at, None) }
    }

    fn ws(id: &str, updated_at: i64, chats: u64) -> WorkspaceSummary {
        WorkspaceSummary { id: id.into(), name: id.to_uppercase(), folder: format!("/{id}"), chats, updated_at, env: EnvKind::Auto, ..Default::default() }
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
    fn outside_a_project_new_chats_are_chats_of_their_own() {
        let ctx = egui::Context::default();
        let mut s = listed();
        s.unattached = true;
        // In a project: there.
        assert_eq!(s.new_chat_msg(), ClientMsg::NewChat { workspace: Some("w1".into()) });
        // Looking at a chat without one: another chat without one.
        s.apply(&ServerMsg::ChatOpened { chat: Some(chat("d", 5, None)), workspace: None });
        assert_eq!(s.new_chat_msg(), ClientMsg::NewUnattachedChat);
        // An older engine can't: where it puts new chats.
        s.unattached = false;
        assert_eq!(s.new_chat_msg(), ClientMsg::NewChat { workspace: None });
        // They list under "Chats", below the projects (few and stable), and its "+" starts one.
        s.unattached = true;
        draw(&ctx, &mut s, None);
        let chats = s.rects.titles["chat:d"];
        let project = s.rects.titles["ws:w1"];
        assert!(chats.top() > project.top(), "Projects above Chats: {chats:?} {project:?}");
        // Each heading's + makes what it holds: Chats a chat without a project, Projects a project.
        let plus_of = |s: &Sidebar, key: &str| {
            let r = s.rects.headings[key];
            pos2(r.right() - 4.0 - ACTION_W / 2.0, r.center().y)
        };
        let at = plus_of(&s, "grp:chats");
        assert!(at.y < chats.top());
        assert_eq!(click(&ctx, &mut s, at), [ClientMsg::NewUnattachedChat]);
        let at = plus_of(&s, "grp:projects");
        click(&ctx, &mut s, at);
        assert!(s.take_requests().contains(&Request::NewWorkspace));
        // No list-wide toolbar row: "By project" isn't a heading any more.
        assert!(!s.rects.headings.contains_key("By project"));
    }

    #[test]
    fn sections_hold_what_is_moved_into_them_and_are_remembered() {
        let ctx = egui::Context::default();
        let mut s = listed();
        s.new_section(Some("chat:b"));
        s.renaming_section = None;
        let id = s.sections[0].id.clone();
        // A change here is shared once (the app sends it to the engine's layout)...
        assert_eq!(s.take_list_change().map(|l| l.sections.len()), Some(1));
        assert_eq!(s.take_list_change(), None);
        // ...and another window's arrangement shows here.
        let mut other = listed();
        other.set_chat_list(&s.chat_list());
        assert_eq!(other.section_of("chat:b"), Some(id.as_str()));
        assert_eq!(other.take_list_change(), None, "applying isn't a change to share back");
        s.move_to_section("ws:w1", Some(&id));
        draw(&ctx, &mut s, None);
        let heading = s.rects.sections[&id];
        // Inside the section, once: chat b and project w1 with its chats.
        for key in ["chat:b", "ws:w1", "chat:a"] {
            assert!(s.rects.titles[key].top() > heading.bottom(), "{key} is under the section");
        }
        assert_eq!(s.rects.titles.len(), 6, "nothing listed twice: {:?}", s.rects.titles.keys());
        // Moving it elsewhere takes it out of the first section.
        s.move_to_section("chat:b", None);
        assert_eq!(s.section_of("chat:b"), None);
        // Remembered across launches, with the view.
        s.group_by = GroupBy::Date;
        let json = serde_json::to_string(&s.prefs()).unwrap();
        let mut again = listed();
        again.set_prefs(&serde_json::from_str(&json).unwrap());
        assert_eq!((again.sections.len(), again.group_by), (1, GroupBy::Date));
        assert_eq!(again.section_of("ws:w1"), Some(id.as_str()));
        // A deleted chat leaves its section; deleting the section keeps what was in it.
        s.move_to_section("chat:c", Some(&id));
        s.apply(&ServerMsg::ChatDeleted { id: "c".into() });
        assert!(!s.sections[0].items.contains(&"chat:c".to_owned()));
        s.sections.clear();
        s.group_by = GroupBy::Project;
        draw(&ctx, &mut s, None);
        assert!(s.rects.titles.contains_key("ws:w1") && s.rects.titles.contains_key("chat:b"));
    }

    #[test]
    fn by_date_and_flat_views_list_every_chat_once() {
        let ctx = egui::Context::default();
        for group in [GroupBy::Date, GroupBy::None] {
            let mut s = listed();
            s.group_by = group;
            draw(&ctx, &mut s, None);
            let chats: Vec<&String> = s.rects.titles.keys().filter(|k| k.starts_with("chat:")).collect();
            assert_eq!(chats.len(), 4, "{group:?}: {chats:?}");
            assert!(!s.rects.titles.keys().any(|k| k.starts_with("ws:")), "{group:?}: no project rows");
        }
        assert_eq!(when_heading(1_000, 1_000 + 60_000), "Today");
        assert_eq!(when_heading(0, 30 * 60 * 60 * 1000), "Yesterday");
        assert_eq!(when_heading(0, 3 * 24 * 60 * 60 * 1000), "This week");
        assert_eq!(when_heading(0, 30 * 24 * 60 * 60 * 1000), "Earlier");
    }

    #[test]
    fn back_and_forward_walk_the_chats_opened() {
        let mut s = Sidebar::default();
        let open = |s: &mut Sidebar, id: &str| s.apply(&ServerMsg::ChatOpened { chat: Some(chat(id, 1, None)), workspace: None });
        assert!(!s.can_go(false) && s.go(false).is_none(), "nothing to go back to");
        for id in ["a", "b", "c"] {
            open(&mut s, id);
        }
        assert_eq!(s.go(false), Some(ClientMsg::OpenChat { id: "b".into() }));
        open(&mut s, "b"); // the engine opens it: no new branch
        assert!(s.can_go(true));
        assert_eq!(s.go(false), Some(ClientMsg::OpenChat { id: "a".into() }));
        open(&mut s, "a");
        assert_eq!(s.go(true), Some(ClientMsg::OpenChat { id: "b".into() }));
        open(&mut s, "b");
        // Opening another chat from the middle drops what was ahead.
        open(&mut s, "d");
        assert!(!s.can_go(true));
        assert_eq!(s.go(false), Some(ClientMsg::OpenChat { id: "b".into() }));
        open(&mut s, "b");
        // A deleted chat leaves the history.
        s.apply(&ServerMsg::ChatDeleted { id: "a".into() });
        assert!(!s.can_go(false), "a was the only chat behind b");
        assert_eq!(s.go(true), Some(ClientMsg::OpenChat { id: "d".into() }));
    }

    #[test]
    fn times_read_in_a_few_characters() {
        let now = 10_000_000_000;
        let ago_mins = |m: i64| ago(now - m * 60_000, now);
        assert_eq!(ago_mins(0), "now");
        assert_eq!(ago_mins(5), "5m");
        assert_eq!(ago_mins(4 * 60), "4h");
        assert_eq!(ago_mins(3 * 60 * 24), "3d");
        assert_eq!(ago_mins(15 * 60 * 24), "2w");
        assert_eq!(ago_mins(100 * 60 * 24), "3mo");
        assert_eq!(ago_mins(800 * 60 * 24), "2y");
        // A clock behind the engine's: "now", never negative.
        assert_eq!(ago(now + 90_000, now), "now");
    }

    #[test]
    fn plugins_row_asks_for_the_plugins_page() {
        let ctx = egui::Context::default();
        let mut s = listed();
        draw(&ctx, &mut s, None);
        // The navigation row after Artifacts.
        let at = s.rects.artifacts.center() + vec2(0.0, s.rects.artifacts.height() + 2.0);
        click(&ctx, &mut s, at);
        assert_eq!(s.take_requests(), [Request::Plugins]);
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
            "a new empty project sorts by when it was made; unknown projects fall into Other"
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
        assert!(!s.is_open(true) && s.palette.is_some(), "search opens over the chat, the list stays closed");
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
        assert_eq!(s.rects.titles.len(), 6, "two projects, four chats: {:?}", s.rects.titles.keys());
        s.set_search("  BUDGET ");
        draw(&ctx, &mut s, None);
        let shown: Vec<&str> = s.rects.titles.keys().map(String::as_str).collect();
        assert_eq!(shown, ["chat:a", "chat:c", "ws:w1"], "only matches, under their project");
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
    fn the_search_palette_finds_moves_opens_and_closes() {
        let ctx = egui::Context::default();
        let mut s = listed();
        let key = |key| egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE };
        let pal = |ctx: &egui::Context, s: &mut Sidebar, events: Vec<egui::Event>| -> Vec<ClientMsg> {
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(900.0, 700.0))), events, ..Default::default() };
            let mut sent = Vec::new();
            let mut out = ctx.run_ui(input, |ui| sent = s.palette_ui(ui.ctx(), &Tokens::dark()));
            out.textures_delta.clear();
            sent
        };
        // Empty: recent chats first, then the actions.
        s.focus_search();
        pal(&ctx, &mut s, vec![]);
        let p = s.palette.clone().unwrap();
        let sections: Vec<&str> = s.palette_hits(&p).iter().map(|(t, _)| *t).collect();
        assert_eq!(sections, ["Recent", "Actions"]);
        // Typed: matches; Enter opens the first.
        pal(&ctx, &mut s, vec![egui::Event::Text("lisbon".into())]);
        assert_eq!(s.palette.as_ref().unwrap().query, "lisbon");
        assert_eq!(pal(&ctx, &mut s, vec![key(Key::Enter)]), [ClientMsg::OpenChat { id: "b".into() }]);
        assert!(s.palette.is_none(), "opening closes it");
        // ↓ moves; Tab goes to the next tab; Esc closes.
        s.focus_search();
        pal(&ctx, &mut s, vec![]);
        pal(&ctx, &mut s, vec![egui::Event::Text("budget".into())]);
        // The title that starts with the search comes first; ↓ then reaches the other.
        pal(&ctx, &mut s, vec![key(Key::ArrowDown)]);
        pal(&ctx, &mut s, vec![key(Key::ArrowUp)]);
        assert_eq!(pal(&ctx, &mut s, vec![key(Key::Enter)]), [ClientMsg::OpenChat { id: "c".into() }], "the best match");
        // Empty field: Tab and → go to the next type, ← back; with text, Tab asks in a new chat.
        s.focus_search();
        pal(&ctx, &mut s, vec![key(Key::Tab)]);
        assert_eq!(s.palette.as_ref().unwrap().tab, PaletteTab::Chats);
        pal(&ctx, &mut s, vec![key(Key::ArrowRight)]);
        assert_eq!(s.palette.as_ref().unwrap().tab, PaletteTab::Projects);
        pal(&ctx, &mut s, vec![key(Key::ArrowLeft)]);
        assert_eq!(s.palette.as_ref().unwrap().tab, PaletteTab::Chats);
        pal(&ctx, &mut s, vec![key(Key::Escape)]);
        assert!(s.palette.is_none());
        s.focus_search();
        pal(&ctx, &mut s, vec![]);
        pal(&ctx, &mut s, vec![egui::Event::Text("plan a trip".into())]);
        s.take_requests();
        pal(&ctx, &mut s, vec![key(Key::Tab)]);
        assert!(s.palette.is_none());
        assert_eq!(s.take_requests(), [Request::Compose("plan a trip".into())]);
        // Projects open their latest chat; actions do what they say.
        s.focus_search();
        s.palette.as_mut().unwrap().tab = PaletteTab::Projects;
        s.palette.as_mut().unwrap().query = "w2".into();
        let hits = s.palette_hits(s.palette.as_ref().unwrap());
        assert!(matches!(hits[0].1[0], Hit::Project(_)));
        let mut out = Vec::new();
        s.palette_open(hits[0].1[0].clone(), &mut out);
        assert_eq!(out, [ClientMsg::OpenChat { id: "b".into() }]);
        s.palette_open(Hit::Action("Settings"), &mut out);
        assert!(s.take_requests().contains(&Request::AppSettings));
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

    /// Pinned, Chats and Projects fold like a project does, and stay folded at the next launch.
    #[test]
    fn the_lists_groups_fold_and_are_remembered() {
        let ctx = egui::Context::default();
        let mut s = listed();
        s.unattached = true;
        s.apply(&ServerMsg::ChatUpdated(ChatSummary { pinned_at: Some(40), ..chat("p", 40, None) }));
        draw(&ctx, &mut s, None);
        assert!(s.rects.titles.contains_key("chat:p") && s.rects.titles.contains_key("chat:d") && s.rects.titles.contains_key("ws:w2"));
        for key in ["grp:pinned", "grp:chats", "grp:projects"] {
            let at = s.rects.headings[key].center();
            click(&ctx, &mut s, at);
            draw(&ctx, &mut s, None);
        }
        assert!(!s.rects.titles.contains_key("chat:p") && !s.rects.titles.contains_key("chat:d") && !s.rects.titles.contains_key("ws:w2"));
        let json = serde_json::to_string(&s.prefs()).unwrap();
        let mut again = listed();
        again.unattached = true;
        again.set_prefs(&serde_json::from_str(&json).unwrap());
        // A project list update keeps them (it prunes folds of projects that went away).
        workspaces(&mut again, vec![ws("w1", 5, 2), ws("w2", 1, 1)], Some("w1"));
        draw(&ctx, &mut again, None);
        assert!(!again.rects.titles.contains_key("chat:d") && !again.rects.titles.contains_key("ws:w2"));
    }

    /// Archived chats leave the list for a folded Archived group at the bottom; the menu
    /// archives and brings back.
    #[test]
    fn archived_chats_sit_apart_folded() {
        let ctx = egui::Context::default();
        let mut s = listed();
        s.unattached = true;
        s.archive = true;
        s.apply(&ServerMsg::ChatUpdated(ChatSummary { archived_at: Some(50), ..chat("d", 5, None) }));
        draw(&ctx, &mut s, None);
        assert!(!s.rects.titles.contains_key("chat:d"), "out of Chats, and Archived starts folded");
        let at = s.rects.headings["grp:archived"].center();
        click(&ctx, &mut s, at);
        draw(&ctx, &mut s, None);
        assert!(s.rects.titles.contains_key("chat:d"));
        // Without the engine's cap there's no Archived group.
        s.archive = false;
        draw(&ctx, &mut s, None);
        assert!(!s.rects.headings.contains_key("grp:archived"));
    }

    /// A chat dragged onto Pinned is pinned; onto Archived, archived; onto a section, moved.
    #[test]
    fn dragging_a_chat_pins_archives_or_files_it() {
        let ctx = egui::Context::default();
        let drag = |s: &mut Sidebar, chat: &str, key: &str| -> Vec<ClientMsg> {
            draw(&ctx, s, None);
            let from = s.rects.titles[&format!("chat:{chat}")].center();
            let press = |pos, pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
            let mut sent = run(&ctx, s, 280.0, vec![egui::Event::PointerMoved(from), press(from, true)]);
            sent.extend(run(&ctx, s, 280.0, vec![egui::Event::PointerMoved(from + vec2(0.0, 12.0))]));
            sent.extend(run(&ctx, s, 280.0, vec![egui::Event::PointerMoved(from + vec2(0.0, 14.0))]));
            let to = if let Some(sec) = key.strip_prefix("sec:") { s.rects.sections[sec].center() } else { s.rects.headings[key].center() };
            sent.extend(run(&ctx, s, 280.0, vec![egui::Event::PointerMoved(to)]));
            sent.extend(run(&ctx, s, 280.0, vec![press(to, false)]));
            sent.extend(draw(&ctx, s, None));
            sent
        };
        let mut s = listed();
        s.unattached = true;
        s.archive = true;
        let sent = drag(&mut s, "d", "grp:pinned");
        assert!(sent.contains(&ClientMsg::PinChat { id: "d".into(), pinned: true }), "{sent:?}");
        let sent = drag(&mut s, "d", "grp:archived");
        assert!(sent.contains(&ClientMsg::ArchiveChat { id: "d".into(), archived: true }), "{sent:?}");
        s.new_section(None);
        let sec = s.sections[0].id.clone();
        s.renaming_section = None;
        drag(&mut s, "b", &format!("sec:{sec}"));
        assert_eq!(s.sections[0].items, ["chat:b"]);
    }

    /// A pinned artifact shows at the top, opens on a click, goes with its chat, and is
    /// remembered.
    #[test]
    fn pinned_artifacts_open_and_follow_their_chat() {
        let ctx = egui::Context::default();
        let mut s = listed();
        s.pin_artifact("b", "calc", "Tip calculator");
        s.pin_artifact("b", "calc", "Tip calculator v2");
        assert_eq!(s.pins.len(), 1, "pinning again renames, doesn't add");
        draw(&ctx, &mut s, None);
        let json = serde_json::to_string(&s.prefs()).unwrap();
        assert!(json.contains("Tip calculator v2"));
        let pinned = s.rects.headings["grp:pinned"];
        // The first row under Pinned is the artifact.
        click(&ctx, &mut s, pos2(pinned.center().x, pinned.bottom() + 16.0));
        assert!(s.take_requests().contains(&Request::OpenArtifact { chat: "b".into(), id: "calc".into() }));
        s.apply(&ServerMsg::ChatDeleted { id: "b".into() });
        assert!(s.pins.is_empty());
    }

    /// The footer is one row (state and where it runs, never the model's name) that opens
    /// a menu; when the model can't be reached, a click goes straight to the models.
    #[test]
    fn the_footer_is_one_row_that_opens_a_menu_or_the_fix() {
        let ctx = egui::Context::default();
        let mut s = listed();
        s.status = Some(Status { dot: egui::Color32::GREEN, state: "Ready".into(), model: "sonnet".into(), place: "Your plan".into() });
        draw(&ctx, &mut s, None);
        draw(&ctx, &mut s, None);
        let at = s.rects.status;
        assert!(at.is_positive() && at.bottom() > 600.0, "at the bottom: {at:?}");
        click(&ctx, &mut s, at.center());
        assert!(s.take_requests().is_empty(), "a click opens the menu");
        s.status = Some(Status { dot: Tokens::dark().danger, state: "Can't reach the model".into(), model: "sonnet".into(), place: String::new() });
        draw(&ctx, &mut s, None);
        let at = s.rects.status.center();
        click(&ctx, &mut s, at);
        assert_eq!(s.take_requests(), [Request::Connection]);
        s.status = None;
        draw(&ctx, &mut s, None);
        assert_eq!(s.rects.status, Rect::NOTHING);
    }

    /// A chat with a reply you haven't seen gets a dot (it and its project); opening it
    /// clears it. Chats untouched since the app started aren't new.
    #[test]
    fn replies_you_havent_seen_are_marked_until_opened() {
        let ctx = egui::Context::default();
        let mut s = listed();
        draw(&ctx, &mut s, None);
        let started = s.started;
        assert!(started > 0);
        // "b" isn't the chat in view ("a" is).
        let i = s.recent.iter().position(|c| c.id == "b").unwrap();
        let mut c = s.recent[i].clone();
        assert!(!s.is_unread(&c), "old chats aren't new");
        c.updated_at = started + 10;
        s.recent[i] = c.clone();
        assert!(s.is_unread(&c));
        if let Some(w) = &c.workspace {
            assert!(s.project_unread(w));
        }
        s.current = Some(c.clone());
        draw(&ctx, &mut s, None);
        s.current = None;
        assert!(!s.is_unread(&c), "seen once opened");
    }

    /// Pinned projects sit in Pinned (not under Projects), and the pin is shared.
    #[test]
    fn projects_can_be_pinned_and_the_pin_is_shared() {
        let ctx = egui::Context::default();
        let mut s = listed();
        let id = s.workspaces[0].id.clone();
        s.toggle_project_pin(&id);
        assert_eq!(s.take_list_change().map(|l| l.pinned_projects), Some(vec![id.clone()]));
        draw(&ctx, &mut s, None);
        assert!(s.rects.headings.contains_key("grp:pinned"));
        let mut other = Sidebar::default();
        other.set_chat_list(&s.chat_list());
        assert_eq!(other.pinned_projects, std::slice::from_ref(&id));
        s.toggle_project_pin(&id);
        assert!(s.pinned_projects.is_empty());
    }

    /// Spaces: hidden with one; with two, each shows only its own; what's new is claimed
    /// by the Space it's made in; Ctrl+number switches.
    #[test]
    fn spaces_show_only_their_own_and_keep_what_is_made_in_them() {
        let mut s = listed();
        assert!(s.current_space().is_none());
        let id = s.workspaces[0].id.clone();
        let key = format!("ws:{id}");
        assert!(s.in_space(&key));
        s.new_space();
        assert_eq!(s.spaces.len(), 2, "the first one holds everything so far");
        assert_eq!(s.current_space().map(|sp| sp.name.as_str()), Some("New space"));
        assert!(!s.in_space(&key), "the new Space starts empty");
        assert!(s.switch_space(0));
        assert!(s.in_space(&key));
        // Made while in the second Space: it stays there.
        s.switch_space(1);
        s.current_ws = Some(id.clone());
        s.claim_for_space();
        assert!(s.in_space(&key));
        s.switch_space(0);
        assert!(!s.in_space(&key));
        assert!(s.take_list_change().is_some_and(|l| l.spaces.len() == 2), "Spaces are shared");
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

    /// While the chat in view is replying, a click on another chat still opens it.
    #[test]
    fn another_chat_opens_while_one_is_replying() {
        let ctx = egui::Context::default();
        let mut s = listed();
        if let Some(c) = s.recent.iter_mut().find(|c| c.id == "a") {
            c.busy = true;
        }
        draw(&ctx, &mut s, None);
        draw(&ctx, &mut s, None);
        let at = s.rects.titles["chat:d"].center();
        let out = click(&ctx, &mut s, at);
        assert!(out.contains(&ClientMsg::OpenChat { id: "d".into() }), "{out:?}");
    }

    /// A press held past egui's click time (slow frames while a reply streams) and released
    /// where it began still opens the chat (it used to come back as a drag and do nothing).
    #[test]
    fn a_long_still_press_on_a_chat_opens_it() {
        let ctx = egui::Context::default();
        let mut s = listed();
        let screen = Rect::from_min_size(pos2(0.0, 0.0), vec2(280.0, 700.0));
        let frame = |s: &mut Sidebar, time: f64, events: Vec<egui::Event>| {
            let input = egui::RawInput { screen_rect: Some(screen), time: Some(time), events, ..Default::default() };
            let mut sent = Vec::new();
            let mut out = ctx.run_ui(input, |ui| sent = s.show(ui, &Tokens::dark(), false));
            out.textures_delta.clear();
            sent
        };
        frame(&mut s, 0.5, vec![]);
        frame(&mut s, 0.6, vec![]);
        let at = s.rects.titles["chat:d"].center();
        let press = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        let mut sent = Vec::new();
        sent.extend(frame(&mut s, 1.0, vec![egui::Event::PointerMoved(at)]));
        sent.extend(frame(&mut s, 1.1, vec![press(true)]));
        sent.extend(frame(&mut s, 2.5, vec![]));
        sent.extend(frame(&mut s, 2.6, vec![press(false)]));
        assert!(sent.contains(&ClientMsg::OpenChat { id: "d".into() }), "{sent:?}");
    }

    /// Before any Space is made, the footer shows the default one ("Personal") and its menu
    /// lists it; editing it makes it a real Space.
    #[test]
    fn the_footer_shows_the_default_space() {
        let ctx = egui::Context::default();
        let mut s = listed();
        s.status = Some(Status { dot: egui::Color32::GREEN, state: "Ready".into(), model: "m".into(), place: "Your plan".into() });
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(280.0, 700.0))), ..Default::default() };
        let mut out = ctx.run_ui(input, |ui| {
            s.show(ui, &Tokens::dark(), false);
        });
        out.textures_delta.clear();
        let texts = crate::plugins::tests::texts_of(&out);
        assert!(crate::plugins::tests::has(&texts, "Personal"), "{texts:?}");
        assert!(s.spaces.is_empty(), "showing it doesn't make it");
        assert_eq!(s.shown_space().name, "Personal");
    }

    /// The list's options menu is two settings showing their current value (choices in a
    /// submenu), then New section; Reset appears only once the view differs from the defaults.
    #[test]
    fn the_list_menu_shows_each_setting_with_its_value() {
        let ctx = egui::Context::default();
        let mut s = listed();
        s.force_hover = true;
        let frame = |s: &mut Sidebar, events: Vec<egui::Event>| {
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(280.0, 700.0))), events, ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| {
                s.show(ui, &Tokens::dark(), false);
            });
            out.textures_delta.clear();
            crate::plugins::tests::texts_of(&out)
        };
        let press = |at, pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        let click = |s: &mut Sidebar, at: egui::Pos2| {
            frame(s, vec![egui::Event::PointerMoved(at)]);
            frame(s, vec![press(at, true)]);
            frame(s, vec![press(at, false)]);
            frame(s, vec![]);
            frame(s, vec![])
        };
        frame(&mut s, vec![]);
        let view = s.rects.view.center();
        let texts = click(&mut s, view);
        let has = |texts: &crate::plugins::tests::Texts, w: &str| crate::plugins::tests::has(texts, w);
        for w in ["Show", "By project", "Sort by", "Last active", "New section"] {
            assert!(has(&texts, w), "{w} missing: {texts:?}");
        }
        assert!(!has(&texts, "By date"), "choices wait in the submenu");
        assert!(!has(&texts, "Reset to defaults"), "nothing to reset yet");
        // Hovering Show opens its choices; picking one changes the list.
        let show = texts.iter().find(|t| t.0 == "Show").map(|t| t.1).unwrap();
        frame(&mut s, vec![egui::Event::PointerMoved(show.center())]);
        let texts = frame(&mut s, vec![egui::Event::PointerMoved(show.center())]);
        let texts = if has(&texts, "By date") { texts } else { frame(&mut s, vec![]) };
        let by_date = texts.iter().find(|t| t.0 == "By date").map(|t| t.1).unwrap_or_else(|| panic!("{texts:?}"));
        click(&mut s, by_date.center());
        assert_eq!(s.group_by, GroupBy::Date);
        // Now there's something to reset.
        let view = s.rects.view.center();
        let texts = click(&mut s, view);
        assert!(has(&texts, "Reset to defaults"), "{texts:?}");
    }

    /// Search's edge cases: what counts as a search, how words match and rank, duplicates,
    /// Spaces, keys at the ends of the list, nothing found, and long or odd text.
    #[test]
    fn the_search_palette_handles_its_edge_cases() {
        let ctx = egui::Context::default();
        let mut s = listed();
        let key = |key, modifiers| egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers };
        let pal = |ctx: &egui::Context, s: &mut Sidebar, events: Vec<egui::Event>| -> Vec<ClientMsg> {
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(900.0, 700.0))), events, ..Default::default() };
            let mut sent = Vec::new();
            let mut out = ctx.run_ui(input, |ui| sent = s.palette_ui(ui.ctx(), &Tokens::dark()));
            out.textures_delta.clear();
            sent
        };
        let hits = |s: &Sidebar, q: &str, tab: PaletteTab| -> Vec<(&'static str, Vec<String>)> {
            let p = Palette { query: q.into(), tab, ..Palette::default() };
            s.palette_hits(&p)
                .into_iter()
                .map(|(title, hs)| {
                    let names = hs
                        .iter()
                        .map(|h| match h {
                            Hit::Chat(c) => c.id.clone(),
                            Hit::Project(w) => w.id.clone(),
                            Hit::More(_, n) => format!("+{n}"),
                            Hit::Action(a) => (*a).to_owned(),
                            Hit::Artifact(a) => a.id.clone(),
                            Hit::Item(i) => i.artifact.id.clone(),
                        })
                        .collect();
                    (title, names)
                })
                .collect()
        };
        let chats = |s: &Sidebar, q: &str| hits(s, q, PaletteTab::Chats).into_iter().find(|(t, _)| *t == "Chats").map(|(_, h)| h).unwrap_or_default();
        // Spaces only is no search: the same as an empty field.
        assert_eq!(hits(&s, "   ", PaletteTab::All), hits(&s, "", PaletteTab::All));
        // Case, extra spaces and word order don't matter; every word must match.
        assert_eq!(chats(&s, "  LISBON "), ["b"]);
        assert_eq!(chats(&s, "trip   budget"), ["c"], "both words, in any order");
        assert_eq!(chats(&s, "budget lisbon"), Vec::<String>::new(), "no chat has both");
        // A project's name finds its chats, after the chats whose title matches.
        s.apply(&ServerMsg::Chats {
            pinned: vec![ChatSummary { title: "W1 notes".into(), ..chat("e", 1, Some(1)) }],
            recent: vec![
                ChatSummary { title: "Quarterly budget".into(), ..in_ws("a", 30, "w1") },
                ChatSummary { title: "Trip to Lisbon".into(), ..in_ws("b", 20, "w2") },
                ChatSummary { title: "Budget for the trip".into(), ..in_ws("c", 10, "w1") },
                ChatSummary { title: "W1 notes".into(), ..chat("e", 1, Some(1)) },
            ],
            after: None,
            next: None,
        });
        assert_eq!(chats(&s, "w1"), ["e", "a", "c"], "its title first, then the project's chats; once, though pinned and recent");
        // Projects match by folder too.
        assert_eq!(hits(&s, "/w2", PaletteTab::Projects), [("Projects", vec!["w2".to_owned()])]);
        // Odd text: symbols, a long paste, other scripts.
        assert!(chats(&s, "(((").is_empty());
        assert!(chats(&s, &"x".repeat(5000)).is_empty());
        assert!(chats(&s, "ß日本").is_empty());
        assert_eq!(highlights("Trip to Lisbon", "lisbon trip"), [(0, 4), (8, 14)]);
        assert_eq!(highlights("aaa", "aa a"), [(0, 3)], "overlaps merge");
        assert!(highlights("İstanbul", "stan").is_empty(), "lower case changes its length: no bold, no cut character");
        // In "All", a long section shows its first rows and "Show all".
        let many: Vec<ChatSummary> = (0..20).map(|i| ChatSummary { title: format!("Plan {i}"), ..chat(&format!("p{i}"), 100 - i, None) }).collect();
        s.apply(&ServerMsg::Chats { pinned: vec![], recent: many, after: None, next: None });
        let all = hits(&s, "plan", PaletteTab::All);
        assert_eq!(all[0].1.len(), 7, "six and a Show all row");
        assert_eq!(all[0].1[6], "+20");
        assert_eq!(chats(&s, "plan").len(), 20, "all of them on the Chats tab");
        // Show all goes to its tab and the palette stays open.
        s.palette = Some(Palette { query: "plan".into(), ..Palette::default() });
        for _ in 0..6 {
            pal(&ctx, &mut s, vec![key(Key::ArrowDown, Modifiers::NONE)]);
        }
        assert!(pal(&ctx, &mut s, vec![key(Key::Enter, Modifiers::NONE)]).is_empty());
        assert_eq!(s.palette.as_ref().map(|p| p.tab), Some(PaletteTab::Chats));
        // ↑ from the first row goes to the last; ↓ from the last back to the first.
        let p = s.palette.as_mut().unwrap();
        p.sel = 0;
        pal(&ctx, &mut s, vec![key(Key::ArrowUp, Modifiers::NONE)]);
        let last = s.palette.as_ref().unwrap().sel;
        assert_eq!(last, 19);
        pal(&ctx, &mut s, vec![key(Key::ArrowDown, Modifiers::NONE)]);
        assert_eq!(s.palette.as_ref().unwrap().sel, 0);
        // Fewer results than the highlight: it stays on a row that exists.
        s.palette.as_mut().unwrap().sel = 19;
        s.palette.as_mut().unwrap().query = "plan 3".into();
        pal(&ctx, &mut s, vec![]);
        assert_eq!(s.palette.as_ref().unwrap().sel, 1, "Plan 3 and Plan 13: the last of the two");
        // Nothing found: Enter does nothing and the palette stays; Tab asks in a new chat.
        s.palette.as_mut().unwrap().query = "zzz".into();
        assert!(pal(&ctx, &mut s, vec![key(Key::Enter, Modifiers::NONE)]).is_empty());
        assert!(s.palette.is_some());
        // Shift+Tab with nothing typed goes back a type (from All, round to the last).
        s.palette = Some(Palette::default());
        pal(&ctx, &mut s, vec![key(Key::Tab, Modifiers::SHIFT)]);
        assert_eq!(s.palette.as_ref().unwrap().tab, PaletteTab::Artifacts);
        // Esc closes from anywhere.
        pal(&ctx, &mut s, vec![key(Key::Escape, Modifiers::NONE)]);
        assert!(s.palette.is_none());
    }

    /// With Spaces, search lists the Space in view; what matches elsewhere comes last.
    #[test]
    fn search_lists_this_space_first_and_others_last() {
        let mut s = listed();
        s.spaces = vec![
            byob_types::layout::ListSpace { id: "s1".into(), name: "Work".into(), color: "#3b82f6".into(), items: vec!["ws:w1".into()] },
            byob_types::layout::ListSpace { id: "s2".into(), name: "Home".into(), color: "#22c55e".into(), items: vec!["ws:w2".into()] },
        ];
        s.space = Some("s1".into());
        let p = Palette { query: "trip".into(), ..Palette::default() };
        let titles: Vec<(&str, Vec<String>)> = s
            .palette_hits(&p)
            .into_iter()
            .map(|(t, h)| (t, h.iter().filter_map(|h| if let Hit::Chat(c) = h { Some(c.id.clone()) } else { None }).collect()))
            .collect();
        assert_eq!(titles.first(), Some(&("Chats", vec!["c".to_owned()])));
        assert_eq!(titles.last(), Some(&("Other Spaces", vec!["b".to_owned()])));
        // Without a search, only this Space.
        let empty = s.palette_hits(&Palette::default());
        assert!(empty.iter().all(|(t, _)| *t != "Other Spaces"));
        assert!(!empty.iter().flat_map(|(_, h)| h).any(|h| matches!(h, Hit::Chat(c) if c.id == "b")));
    }

    /// The footer menu, top to bottom: Spaces (heading, each Space, New space, its
    /// settings), one status row that opens Models and connection, Settings, then help.
    #[test]
    fn the_footer_menu_groups_spaces_status_settings_and_help() {
        let ctx = egui::Context::default();
        let mut s = listed();
        s.status = Some(Status { dot: egui::Color32::GREEN, state: "Ready".into(), model: "sonnet".into(), place: "Your plan".into() });
        let frame = |s: &mut Sidebar, events: Vec<egui::Event>| {
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(280.0, 700.0))), events, ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| {
                s.show(ui, &Tokens::dark(), false);
            });
            out.textures_delta.clear();
            crate::plugins::tests::texts_of(&out)
        };
        let press = |at, pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        frame(&mut s, vec![]);
        frame(&mut s, vec![]);
        let at = s.rects.status.center();
        frame(&mut s, vec![egui::Event::PointerMoved(at)]);
        frame(&mut s, vec![press(at, true)]);
        frame(&mut s, vec![press(at, false)]);
        frame(&mut s, vec![]);
        let texts = frame(&mut s, vec![]);
        let y = |w: &str| texts.iter().find(|t| t.0 == w).map(|t| t.1.center().y).unwrap_or_else(|| panic!("{w} missing: {texts:?}"));
        let order = ["Spaces", "New space", "Space settings", "Ready", "Settings", "Get help", "Send feedback"];
        for pair in order.windows(2) {
            assert!(y(pair[0]) < y(pair[1]), "{} above {}", pair[0], pair[1]);
        }
        // One status row (the model at its end), not a line and an item.
        assert!(crate::plugins::tests::has(&texts, "sonnet"));
        assert!(!crate::plugins::tests::has(&texts, "Models and connection"));
        // It opens Models and connection.
        let ready = texts.iter().find(|t| t.0 == "Ready").map(|t| t.1.center()).unwrap();
        frame(&mut s, vec![egui::Event::PointerMoved(ready)]);
        frame(&mut s, vec![press(ready, true)]);
        frame(&mut s, vec![press(ready, false)]);
        assert_eq!(s.take_requests(), [Request::Connection]);
    }
}
