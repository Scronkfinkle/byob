//! The byob UI. Identical on desktop and in the browser: it renders engine state received
//! over a [`Connection`] and sends user intents back. It never touches the engine directly.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

pub mod appview;
pub mod artifact;
pub mod capture;
pub mod chart;
mod chatbar;
pub mod cockpit;
pub mod compose_md;
pub mod composer;
pub mod connect;
pub mod count;
mod envpanel;
pub mod export;
mod fit;
pub mod fluid;
mod fluid_app;
mod form;
pub mod glass;
pub mod header;
mod highlight;
mod icons;
pub mod layout;
mod library;
pub mod look;
pub mod markdown;
pub mod math;
pub mod media;
mod menu;
pub mod models;
mod notices;
pub mod pane;
pub mod plugins;
mod projects;
pub mod queue;
pub mod renderers;
mod settings;
pub mod sidebar;
pub mod sites;
pub mod slash;
pub mod steps;
pub mod theme;
mod tip;
pub mod tree;
pub mod upload;
pub mod usage;
pub mod views;
pub mod web;
pub mod workspace;
pub mod ws;

use byob_protocol::{ClientMsg, Connection, EngineState, Entry, EntryRole, NoticeLevel, ServerMsg, StepStatus, Welcome};

/// Tallest an expanded thinking block gets before it scrolls.
const THINKING_MAX_HEIGHT: f32 = 320.0;

/// What fills the space beside the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Page {
    Chat,
    Projects,
    Apps,
    Library,
    Plugins,
}

/// What "Hide details" hides in a chat's replies.
const DETAILS: [&str; 8] = [
    "message.thinking",
    "message.thought_size",
    "message.steps",
    "message.step_files",
    "message.subagent_details",
    "reply.tokens",
    "reply.time",
    "reply.speed",
];

pub struct ByobApp {
    conn: Box<dyn Connection>,
    tokens: theme::Tokens,
    prefs: theme::Prefs,
    /// Bumped whenever `prefs` or the user's themes change.
    prefs_gen: u64,
    /// What `tokens` were last built from (generation, OS theme) and pushed into egui.
    applied: Option<(u64, Option<egui::Theme>)>,
    /// Fonts installed (UI, code).
    fonts: Option<(theme::UiFont, theme::MonoFont)>,
    /// Theme files: the desktop themes folder and the browser's imported ones.
    user_themes: Vec<theme::ThemeDef>,
    /// Problems found in theme files, shown in the appearance settings.
    theme_warnings: Vec<String>,
    /// Desktop: `<config>/themes`.
    themes_dir: Option<std::path::PathBuf>,
    /// Appearance settings UI state (import/export, open sections).
    appearance: settings::AppearanceUi,
    markdown: markdown::Cache,
    sidebar: sidebar::Sidebar,
    /// The chat list is beside the chat this frame (drawn before the header, full height,
    /// with the window controls in it); else the header has them.
    sidebar_docked: bool,
    /// Tucks the sidebar and pane away when the chat would get too narrow.
    fit: fit::Fit,
    /// The title menu's rename or delete dialog.
    chat_edit: Option<header::ChatEdit>,
    /// A pinned artifact to open once its chat is in view (chat, artifact).
    pending_artifact: Option<(String, String)>,
    /// The Apps and Library pages (one list of every chat's artifacts, shown two ways).
    library: library::Library,
    projects: projects::Projects,
    /// What fills the space beside the list: the chat, or one of the pages.
    pub(crate) page: Page,
    /// The Plugins page's search.
    plugins_query: String,
    /// Text for the composer once a new chat opens ("Make an app that ").
    pending_draft: Option<String>,
    /// Show the pending artifact full page (an app opened from Apps).
    pending_full: bool,
    /// Bumped when the sidebar is dragged shut, so it reopens at its normal width.
    sidebar_gen: u32,
    /// The list's width last frame, and whether you've been dragging its edge: only a drag
    /// you let go of narrow shuts it (not a first frame that comes out narrow).
    sidebar_last_w: f32,
    sidebar_resized: bool,
    /// A layout came from the engine since connecting (see `ServerMsg::Layout`).
    layout_seen: bool,
    state: Option<EngineState>,
    entries: Vec<Entry>,
    notices: notices::Notices,
    input: String,
    show_settings: bool,
    /// The connection form (welcome screen and settings).
    connect: connect::ConnectForm,
    /// Settings → Models (profiles), and the header's model switch.
    models: models::ModelsUi,
    /// Where the header's buttons were last drawn.
    header_rects: header::Rects,
    /// Where the composer's parts were last drawn.
    composer_rects: composer::Rects,
    chat_config: chatbar::ChatConfig,
    /// Listening for speech since this time (the voice UI under the box).
    voice: Option<f64>,
    /// Until when the row says speech to text isn't connected.
    voice_note: Option<f64>,
    /// Charts and images shown in the transcript.
    media: media::Cache,
    /// Files attached in the composer.
    uploads: upload::Uploads,
    /// The chat's artifacts: cards, the pane, export.
    artifacts: artifact::Artifacts,
    /// Context meter and compaction dividers.
    usage: usage::Usage,
    /// Workspaces: the new-workspace form, workspace settings, the chat settings popover,
    /// the global defaults.
    workspaces: workspace::WorkspaceUi,
    /// Follow-ups queued while a reply runs.
    queue: queue::QueueUi,
    /// Plugins: cards, the panel, the first-open card, the header chip.
    plugins: plugins::PluginsUi,
    /// The composer's `/` menu.
    slash: slash::SlashMenu,
    /// Put the keyboard focus in the composer on the next frame (scripted drafts).
    focus_composer: bool,
    /// How step views are drawn, by kind.
    views: views::ViewRegistry,
    /// The engine's version and caps, once it said.
    engine: Option<Welcome>,
    /// Messages skipped so far, logged once each (unknown types, plugins without a handler here).
    skipped: std::collections::BTreeSet<String>,
    /// Unsent composer text of the chats not in view, by [`draft_key`].
    drafts: std::collections::BTreeMap<String, String>,
    /// The [`draft_key`] of what is in view (the composer holds its draft).
    draft_key: String,
    /// The first message of a new chat went out: the chat it makes keeps the composer.
    sent_new: bool,
    /// What the interface shows, as the engine last said ([`layout`]).
    layout: std::sync::Arc<byob_protocol::layout::Layout>,
    /// The look last reported to the engine (preferences generation, dark), and a change it
    /// asked for to answer in the next report ([`look`]).
    look_reported: Option<(u64, bool)>,
    /// Look changes applied and not answered yet: their requests and problems.
    look_answer: Vec<(u64, Vec<String>)>,
    /// Screenshots the engine asked for ([`capture`]).
    captures: capture::Captures,
    /// Looks from before each change the model made, newest last (`ui_look`'s undo).
    look_history: Vec<theme::Prefs>,
    /// Fluid: the edit mode, inserted elements' tree state, and what the plugin's document
    /// was last saved from (its `edited` count).
    fluid_editor: fluid::edit::Editor,
    fluid_trees: std::collections::HashMap<String, tree::TreeState>,
    fluid_saved: u64,
    /// The plugins' fluid changes last made into the base layer.
    fluid_layers: Vec<(String, serde_json::Value)>,
    /// The answer to the model's last fluid request, for the next look report.
    fluid_note: Option<(String, (String, String))>,
    /// Since when that answer waits for the elements to be listed (fluid was idle).
    fluid_wait: Option<web_time::Instant>,
    /// Fluid's environment switches, read once.
    fluid_env: fluid_app::FluidEnv,
    /// One of your messages being changed, to start over from it.
    editing: Editing,
    /// Math in messages is typeset (a plugin names `native:math`, [`math`]).
    math: bool,
    /// The cockpit's backdrop: whether this window can draw one, and the one drawn ([`cockpit`]).
    stage: cockpit::Stage,
    /// Placing the free arrangement's modules by hand.
    arranging: cockpit::free::Arranging,
}

/// Changing one of your messages to start the chat over from it (`ClientMsg::Rewind`).
#[derive(Default)]
struct Editing {
    /// The message being changed, and its text so far.
    open: Option<(u64, String)>,
    /// Going back is possible now: the engine takes it and no reply is running.
    allowed: bool,
}

/// Where a draft is kept: `chat:<id>` for a chat, `new:<workspace>` for the new chat about
/// to start in a workspace (`new:` where it has none).
fn draft_key(chat: Option<&str>, workspace: Option<&str>) -> String {
    match chat {
        Some(id) => format!("chat:{id}"),
        None => format!("new:{}", workspace.unwrap_or_default()),
    }
}

/// Drafts kept across launches, at most.
const MAX_DRAFTS: usize = 100;

impl ByobApp {
    pub fn new(mut conn: Box<dyn Connection>, client: &str) -> Self {
        conn.send(ClientMsg::hello(client));
        Self {
            conn,
            tokens: theme::Tokens::dark(),
            prefs: theme::Prefs::default(),
            prefs_gen: 0,
            applied: None,
            fonts: None,
            user_themes: Vec::new(),
            theme_warnings: Vec::new(),
            themes_dir: None,
            appearance: settings::AppearanceUi::default(),
            markdown: markdown::Cache::default(),
            sidebar: sidebar::Sidebar::default(),
            sidebar_docked: false,
            layout_seen: false,
            state: None,
            entries: Vec::new(),
            notices: notices::Notices::default(),
            input: String::new(),
            show_settings: false,
            connect: connect::ConnectForm::default(),
            models: models::ModelsUi::default(),
            header_rects: header::Rects::default(),
            composer_rects: composer::Rects::default(),
            chat_config: chatbar::ChatConfig::default(),
            fit: fit::Fit::default(),
            chat_edit: None,
            pending_artifact: None,
            library: library::Library::default(),
            projects: projects::Projects::default(),
            page: Page::Chat,
            plugins_query: String::new(),
            pending_draft: None,
            pending_full: false,
            sidebar_gen: 0,
            sidebar_last_w: 0.0,
            sidebar_resized: false,
            voice: None,
            voice_note: None,
            media: media::Cache::default(),
            uploads: upload::Uploads::default(),
            artifacts: artifact::Artifacts::default(),
            usage: usage::Usage::default(),
            workspaces: workspace::WorkspaceUi::default(),
            queue: queue::QueueUi::default(),
            plugins: plugins::PluginsUi::default(),
            slash: slash::SlashMenu::default(),
            focus_composer: false,
            views: views::ViewRegistry::default(),
            engine: None,
            skipped: std::collections::BTreeSet::new(),
            drafts: std::collections::BTreeMap::new(),
            draft_key: String::new(),
            sent_new: false,
            layout: std::sync::Arc::default(),
            look_reported: None,
            look_answer: Vec::new(),
            captures: capture::Captures::default(),
            look_history: Vec::new(),
            fluid_editor: fluid::edit::Editor::default(),
            fluid_trees: std::collections::HashMap::new(),
            fluid_saved: 0,
            fluid_layers: Vec::new(),
            fluid_note: None,
            fluid_wait: None,
            fluid_env: fluid_app::FluidEnv::read(),
            editing: Editing::default(),
            math: false,
            stage: cockpit::Stage::default(),
            arranging: cockpit::free::Arranging::default(),
        }
    }

    /// Register renderers for more view kinds.
    pub fn views_mut(&mut self) -> &mut views::ViewRegistry {
        &mut self.views
    }

    /// Register renderers for more artifact content types (or replace byob's own).
    pub fn artifact_renderers_mut(&mut self) -> &mut renderers::ArtifactRenderers {
        self.artifacts.renderers_mut()
    }

    /// Show artifact `id` beside chat `chat`, opening the chat first if it isn't in view;
    /// `full`: the whole window (an app opened from the Apps page).
    pub(crate) fn open_artifact_in(&mut self, chat: String, id: String, full: bool) {
        self.page = Page::Chat;
        if self.sidebar.current().is_some_and(|c| c.id == chat) {
            if self.artifacts.open(&id) && full && !self.artifacts.is_maximized() {
                self.artifacts.toggle_maximized();
            }
        } else {
            self.conn.send(ClientMsg::OpenChat { id: chat.clone() });
            self.pending_artifact = Some((chat, id));
            self.pending_full = full;
        }
    }

    /// The Apps or Library page (asks the engine for the list each time one opens).
    pub(crate) fn open_page(&mut self, mode: library::Mode) {
        if self.engine_has(byob_protocol::version::CAP_LIBRARY) {
            let msg = self.library.ask(mode);
            self.conn.send(msg);
            self.page = match mode {
                library::Mode::Apps => Page::Apps,
                library::Mode::Library => Page::Library,
            };
            self.close_settings();
        } else if !self.artifacts.is_empty() {
            // An older engine: this chat's artifacts, as before.
            self.artifacts.toggle_pane();
        }
    }

    /// A new chat of its own with its first words written (the person finishes them).
    pub(crate) fn begin_chat_with(&mut self, words: String) {
        let msg = if self.engine_has(byob_protocol::version::CAP_UNATTACHED) { ClientMsg::NewUnattachedChat } else { ClientMsg::NewChat { workspace: None } };
        self.conn.send(msg);
        self.pending_draft = Some(words);
        self.page = Page::Chat;
    }

    /// The Projects page in place of the chat.
    fn projects_page(&mut self, ui: &mut egui::Ui, t: &theme::Tokens) {
        let frame = egui::Frame::central_panel(ui.style()).fill(t.app_bg).inner_margin(0);
        let all = self.sidebar.workspaces().to_vec();
        let pinned = self.sidebar.pinned_projects.clone();
        let root = self.workspaces.root.clone();
        let scope = self.sidebar.space_scope();
        let action =
            egui::CentralPanel::default().frame(frame).show(ui, |ui| self.projects.ui(ui, t, &all, &pinned, &root, scope.as_ref(), sidebar::now_ms())).inner;
        match action {
            Some(projects::Action::Open(id)) => {
                self.page = Page::Chat;
                let latest = self.sidebar.latest_chat_in(&id);
                self.conn.send(match latest {
                    Some(chat) => ClientMsg::OpenChat { id: chat },
                    None => ClientMsg::NewChat { workspace: Some(id) },
                });
            }
            Some(projects::Action::NewChat(id)) => {
                self.page = Page::Chat;
                self.conn.send(ClientMsg::NewChat { workspace: Some(id) });
            }
            Some(projects::Action::TogglePin(id)) => self.sidebar.toggle_project_pin(&id),
            Some(projects::Action::Settings(id)) => self.workspaces.open_settings(&id, false),
            Some(projects::Action::Remove(id)) => self.workspaces.open_settings(&id, true),
            Some(projects::Action::New(start)) => self.workspaces.open_start(start),
            Some(projects::Action::Close) => self.page = Page::Chat,
            None => {}
        }
    }

    /// The Plugins page.
    pub(crate) fn open_plugins_page(&mut self) {
        self.page = Page::Plugins;
        self.close_settings();
    }

    /// The Plugins page in place of the chat.
    fn plugins_page(&mut self, ui: &mut egui::Ui, t: &theme::Tokens) {
        let frame = egui::Frame::central_panel(ui.style()).fill(t.app_bg).inner_margin(0);
        let mut query = std::mem::take(&mut self.plugins_query);
        let action = egui::CentralPanel::default().frame(frame).show(ui, |ui| self.plugins.page(ui, t, &mut query)).inner;
        self.plugins_query = query;
        match action {
            Some(plugins::PageAction::Start(words)) => self.begin_chat_with(words),
            Some(plugins::PageAction::Close) => self.page = Page::Chat,
            None => {}
        }
    }

    /// The Library in place of the chat; what it asked for.
    fn library_page(&mut self, ui: &mut egui::Ui, t: &theme::Tokens) {
        let pinned: std::collections::BTreeSet<String> = self.sidebar.pins.iter().map(|p| p.id.clone()).collect();
        let frame = egui::Frame::central_panel(ui.style()).fill(t.app_bg).inner_margin(0);
        let action = egui::CentralPanel::default().frame(frame).show(ui, |ui| self.library.ui(ui, t, &pinned, sidebar::now_ms())).inner;
        match action {
            Some(library::Action::Open { chat, id }) => {
                let full = self.library.mode == library::Mode::Apps;
                self.open_artifact_in(chat, id, full);
            }
            Some(library::Action::Start(words)) => self.begin_chat_with(words),
            Some(library::Action::TogglePin { chat, id, title }) => {
                if self.sidebar.artifact_pinned(&id) {
                    self.sidebar.unpin_artifact(&id);
                } else {
                    self.sidebar.pin_artifact(&chat, &id, &title);
                }
            }
            Some(library::Action::Close) => self.page = Page::Chat,
            None => {}
        }
    }

    /// The sidebar footer's line: whether the model answers, which one, where it runs.
    fn footer_status(&self) -> Option<sidebar::Status> {
        let state = self.state.as_ref()?;
        let (link, model) = match &state.chat_endpoint {
            Some(e) => (&e.link, &e.model),
            None => (&state.link, &state.inference_model),
        };
        let t = &self.tokens;
        // In plain words: can it answer, and is it doing something now?
        let working = self.busy() || self.sidebar.any_busy();
        let (dot, state) = match link {
            byob_protocol::Link::Failed { .. } => (t.danger, "Can't reach the model"),
            byob_protocol::Link::Checking => (t.warning, "Connecting…"),
            _ if steps::awaiting_approval(&self.entries) => (t.warning, "Waiting for you"),
            _ if working => (t.accent, "Working…"),
            byob_protocol::Link::Ok => (t.success, "Ready"),
            byob_protocol::Link::Unknown => (t.text_faint, "Ready"),
        };
        let state = state.to_owned();
        let profile = self.chat_profile();
        let model = profile.as_ref().map_or_else(|| model.clone(), |p| p.label().to_owned());
        let place = profile.as_ref().map_or("", chatbar::place).to_owned();
        Some(sidebar::Status { dot, state, model, place })
    }

    /// The engine lists `cap` (see `byob_protocol::version`); `false` until it said.
    pub fn engine_has(&self, cap: &str) -> bool {
        self.engine.as_ref().is_some_and(|w| byob_protocol::version::shared(&byob_protocol::version::client_caps(), &w.caps, cap))
    }

    /// Send plugin `plugin` a message through the plugin channel, if the engine takes them.
    pub fn send_plugin(&mut self, plugin: &str, msg: serde_json::Value) -> bool {
        if !self.engine_has(byob_protocol::version::CAP_PLUGIN_CHANNEL) {
            return false;
        }
        self.conn.send(ClientMsg::Plugin { plugin: plugin.to_owned(), msg });
        true
    }

    /// Log a skipped message once per `what`.
    fn skip(&mut self, what: String) {
        if self.skipped.insert(what.clone()) {
            log::info!("skipping {what} (newer engine or plugin?)");
        }
    }

    /// Plug in how HTML app artifacts are shown (default: an iframe in the browser, a
    /// summary with Open on the desktop).
    pub fn set_app_view(&mut self, view: Box<dyn appview::AppView>) {
        self.artifacts.set_app_view(view);
    }

    /// The artifacts (pane prefs to store and restore, scripted runs).
    pub fn artifacts_mut(&mut self) -> &mut artifact::Artifacts {
        &mut self.artifacts
    }

    /// Per-device artifact pane state (width, collapsed, tabs per chat), for the app to persist.
    /// Per-device chat list state (collapsed workspaces) and unsent drafts, for the app to
    /// persist.
    pub fn sidebar_prefs(&self) -> sidebar::SidebarPrefs {
        let mut p = self.sidebar.prefs();
        p.projects_view = self.projects.view;
        p.projects_sort = self.projects.sort;
        p.projects_reverse = self.projects.reverse;
        let current = (!self.draft_key.is_empty() && !self.input.trim().is_empty()).then(|| (self.draft_key.clone(), self.input.clone()));
        p.drafts = self.drafts.iter().map(|(k, v)| (k.clone(), v.clone())).chain(current).take(MAX_DRAFTS).collect();
        p
    }

    /// Restore them (at startup, before the engine said which chat is in view).
    pub fn set_sidebar_prefs(&mut self, prefs: &sidebar::SidebarPrefs) {
        self.sidebar.set_prefs(prefs);
        self.projects.set_prefs(prefs.projects_view, prefs.projects_sort, prefs.projects_reverse);
        self.drafts = prefs.drafts.iter().filter(|(_, v)| !v.trim().is_empty()).take(MAX_DRAFTS).map(|(k, v)| (k.clone(), v.clone())).collect();
    }

    /// The view changed to `key`: the composer's text is kept as the old view's draft, and
    /// the new view's draft comes back. The first message of a new chat makes the chat, which
    /// keeps the composer as it is.
    fn switch_draft(&mut self, key: String, existing_chat: bool) {
        if key == self.draft_key {
            return;
        }
        let old = std::mem::replace(&mut self.draft_key, key);
        if std::mem::take(&mut self.sent_new) && existing_chat && old.starts_with("new:") {
            self.drafts.remove(&old);
            return;
        }
        let text = std::mem::take(&mut self.input);
        if text.trim().is_empty() {
            self.drafts.remove(&old);
        } else {
            self.drafts.insert(old, text);
        }
        self.input = self.drafts.remove(&self.draft_key).unwrap_or_default();
    }

    pub fn pane_prefs(&self) -> pane::PanePrefs {
        self.artifacts.pane_prefs()
    }

    /// Where the desktop app serves artifacts for "Open in browser" (may start a server).
    /// The app draws the title bar (macOS, with the window's content under a transparent
    /// title bar): the top bar leaves room for the window buttons and moves the window.
    pub fn set_titlebar(&mut self, titlebar: Option<sidebar::Titlebar>) {
        self.sidebar.titlebar = titlebar;
    }

    pub fn set_link_base(&mut self, base: artifact::LinkBase) {
        self.artifacts.set_link_base(base);
    }

    fn notice(&mut self, level: NoticeLevel, text: String) {
        self.notices.push(level, text);
    }

    /// Attach a file to the next message, as if picked in the composer (scripted runs).
    pub fn attach(&mut self, name: &str, bytes: &[u8]) {
        if let Err(e) = self.uploads.add(self.conn.as_mut(), name, bytes) {
            self.notice(NoticeLevel::Error, e);
        }
    }

    /// Attachments are still uploading.
    pub fn uploading(&self) -> bool {
        self.uploads.busy()
    }

    fn pump(&mut self) {
        while let Some(msg) = self.conn.try_recv() {
            self.plugins.sites.peek(&msg);
            if let ServerMsg::ChatOpened { chat, workspace } = &msg {
                self.workspaces.chat_changed();
                // The queued message being edited stays queued in the chat we leave.
                self.end_edit();
                // Each chat keeps its unsent text (and the new chat its own).
                self.switch_draft(draft_key(chat.as_ref().map(|c| c.id.as_str()), workspace.as_deref()), chat.is_some());
                // A new chat: its composer is the form, ready to type in (with the first words
                // when a page started it).
                if chat.is_none() {
                    self.focus_composer = true;
                    if let Some(words) = self.pending_draft.take() {
                        self.input = words;
                    }
                }
            }
            if self.queue.apply(&msg) {
                self.queue_changed();
                continue;
            }
            // Which plugins draw which artifact types follows the chat in view.
            if let ServerMsg::Extensions(e) = &msg {
                self.artifacts.renderers_mut().apply_plugins(&e.plugins);
                // Math in messages follows the plugin that names `native:math`.
                self.math = self.artifacts.renderers_mut().native_in_use(renderers::math::NATIVE);
            }
            if let ServerMsg::Profiles(l) = &msg {
                self.workspaces.set_profiles(l);
            }
            if let ServerMsg::Config(v) = &msg {
                self.chat_config.received(v);
            }
            if self.models.apply(&msg) {
                continue;
            }
            if let ServerMsg::ChatOpened { .. } = &msg {
                // A chat opened (from the list, the palette, a page): it takes the space.
                self.page = Page::Chat;
            }
            if let ServerMsg::Library { items } = &msg {
                self.sidebar.library.clone_from(items);
            }
            if self.library.apply(&msg) || self.plugins.apply_logo(&msg) {
                continue;
            }
            if self.usage.apply(&msg) || self.workspaces.apply(&msg) || self.sidebar.apply(&msg) || self.artifacts.apply(&msg) || self.plugins.apply(&msg) {
                continue;
            }
            match msg {
                ServerMsg::Welcome(w) => {
                    self.chat_config.reset();
                    log::info!("engine speaks protocol {}.{} with caps {:?}", w.protocol, w.minor, w.caps);
                    self.engine = Some(w);
                    self.layout_seen = false;
                    let sites = self.engine_has(byob_protocol::sites::CAP_VIEW_SITES);
                    self.plugins.sites.set_enabled(sites);
                    self.artifacts.set_views_enabled(sites);
                }
                // No client-side plugin handlers yet.
                ServerMsg::Plugin { plugin, .. } => self.skip(format!("messages from plugin {plugin:?}")),
                ServerMsg::Unknown => self.skip("messages of a type this client doesn't know".into()),
                // Handled by the plugins' sites above.
                ServerMsg::LiveView(_) => {}
                // Handled by the web surfaces (in the artifacts) above.
                ServerMsg::WebReply { .. } | ServerMsg::WebPush { .. } => {}
                ServerMsg::State(state) => {
                    self.connect.sync(&state);
                    self.state = Some(state);
                }
                ServerMsg::Notice { level, text } => {
                    self.notice(level, text);
                }
                ServerMsg::Transcript { entries } => self.entries = entries,
                ServerMsg::EntryAdded(entry) => self.entries.push(entry),
                ServerMsg::EntryDelta { id, content, reasoning } => {
                    if let Some(e) = self.entry_mut(id) {
                        e.append(&content, &reasoning);
                    }
                }
                ServerMsg::EntryFinished { id, error, stats } => {
                    if let Some(e) = self.entry_mut(id) {
                        e.done = true;
                        e.error = error;
                        e.stats = stats;
                    }
                }
                ServerMsg::StepUpdated { entry, step } => steps::apply(&mut self.entries, entry, step),
                ServerMsg::Delegation { entry, step, update } => {
                    if let Some(e) = self.entry_mut(entry) {
                        e.apply_delegation(step, &update);
                    }
                }
                ServerMsg::EntryUndo { id, undoable, undone } => steps::apply_undo(&mut self.entries, id, undoable, undone),
                // Handled by the sidebar above.
                ServerMsg::Chats { .. } | ServerMsg::ChatUpdated(_) | ServerMsg::ChatDeleted { .. } | ServerMsg::ChatOpened { .. } => {}
                ServerMsg::Uploaded { upload, attachment, error } => {
                    if let Some(e) = self.uploads.uploaded(upload, attachment, error) {
                        self.notice(NoticeLevel::Error, e);
                    }
                }
                ServerMsg::Media { path, content } => self.media.received(&path, &content),
                // Handled by the artifacts above.
                ServerMsg::Artifacts { .. } | ServerMsg::ArtifactUpdated(_) | ServerMsg::ArtifactFile { .. } => {}
                // Handled by the usage meter above.
                ServerMsg::Usage(_) | ServerMsg::Compactions { .. } | ServerMsg::Compacted(_) => {}
                ServerMsg::InferenceTested(test) => {
                    self.models.tested(&test);
                    self.connect.tested(test);
                }
                // Handled by the models section above.
                ServerMsg::Profiles(_) | ServerMsg::ProfileTested(_) => {}
                ServerMsg::Layout(l) => {
                    let l = l.sanitized();
                    // The chat list's sections and view live in the layout, the same in every
                    // window. Sections made on this device before the engine kept them go up,
                    // once: on the first layout after connecting. Later ones always apply, or
                    // two windows would hand an old arrangement back and forth.
                    let first = !std::mem::replace(&mut self.layout_seen, true);
                    if first && l.chat_list.is_default() && !self.sidebar.chat_list().is_default() {
                        self.sidebar.share_list();
                    } else {
                        self.sidebar.set_chat_list(&l.chat_list);
                    }
                    self.layout = std::sync::Arc::new(l);
                }
                ServerMsg::SetLook { request, mut changes } => {
                    let mut prefs = self.prefs.clone();
                    let fluid_ops = changes.remove("fluid");
                    let mut problems = look::change(&mut prefs, &mut self.look_history, &changes, self.tokens.dark, &self.user_themes, self.stage.available());
                    if let Some(ops) = fluid_ops {
                        let looked = prefs.clone();
                        problems.extend(self.fluid_from_model(&mut prefs, &ops));
                        // One undo takes back the whole change, look and fluid together.
                        if prefs != looked && !changes.contains_key("undo") {
                            look::remember(&mut self.look_history, self.prefs.clone());
                        }
                    }
                    self.set_prefs(prefs);
                    // Answered once the new look is drawn (its colors are what the theme makes).
                    self.look_answer.push((request, problems));
                    self.look_reported = None;
                }
                ServerMsg::Capture { request } => self.captures.asked(request),
                // Handled by the queue above.
                ServerMsg::Queue(_) => {}
                // Handled by the plugins above.
                ServerMsg::Extensions(_) => {}
                // Handled by the workspace views and the sidebar above.
                ServerMsg::WorkspaceEnv(_)
                | ServerMsg::Workspaces { .. }
                | ServerMsg::Config(_)
                | ServerMsg::Folders(_)
                | ServerMsg::ContainersChecked { .. }
                | ServerMsg::WorkspaceCreated { .. }
                | ServerMsg::Library { .. }
                | ServerMsg::LibraryFile { .. }
                | ServerMsg::PluginLogo { .. } => {}
            }
        }
    }

    fn entry_mut(&mut self, id: u64) -> Option<&mut Entry> {
        self.entries.iter_mut().rev().find(|e| e.id == id)
    }

    /// Per-device appearance preferences, for the app to persist.
    pub fn prefs(&self) -> &theme::Prefs {
        &self.prefs
    }

    /// Tell the engine this window's look when it changed (or it asked for a change), once
    /// the tokens are built from it.
    fn report_look(&mut self, ctx: &egui::Context) {
        let now = (self.prefs_gen, self.tokens.dark);
        if self.look_reported == Some(now) || !self.engine_has(byob_protocol::look::CAP_LOOK) {
            return;
        }
        // The model asked to see the elements: once they're listed (fluid may have been idle).
        if !self.fluid_ready(ctx) {
            return;
        }
        self.look_reported = Some(now);
        let mut report = look::report(&self.prefs, &self.tokens, &self.user_themes, self.stage.available());
        if let Some(note) = self.fluid_report(ctx) {
            report.notes.insert("fluid".into(), note);
        }
        // Every change gets its answer (several can arrive between two frames).
        let answers = std::mem::take(&mut self.look_answer);
        if answers.is_empty() {
            self.conn.send(ClientMsg::ReportLook(report));
            return;
        }
        for (request, problems) in answers {
            self.conn.send(ClientMsg::ReportLook(byob_protocol::look::LookReport { answered: Some(request), problems, ..report.clone() }));
        }
    }

    /// The app's logic between frames, and the only thing eframe runs while the window is
    /// hidden (minimized, or covered by other windows): messages from the engine are taken
    /// and look changes answered then too. They used to wait for the next frame, so a
    /// `ui_look` change made while the window was out of sight timed out and applied
    /// when it came back.
    pub fn logic(&mut self, ctx: &egui::Context) {
        self.pump();
        // A screenshot paints the window even while it's hidden.
        self.captures.start(ctx);
        self.captures.send(self.conn.as_mut());
        if !self.look_answer.is_empty() {
            // The colors the change makes, as the next frame will apply them.
            let mut tokens = theme::Tokens::for_prefs(&self.prefs, ctx.system_theme(), &self.user_themes);
            if self.stage.active(&self.prefs.cockpit) {
                cockpit::glaze(&mut tokens, self.prefs.cockpit.glass);
            }
            self.tokens = tokens;
            self.report_look(ctx);
        }
    }

    /// Draw backdrops ([`cockpit`]) with eframe's wgpu render state.
    pub fn enable_backdrops(&mut self, rs: &egui_wgpu::RenderState) {
        self.stage.install(rs);
        self.prefs_gen = self.prefs_gen.wrapping_add(1);
    }

    /// Show the free arrangement's handles (from byob's floating one if there's none yet).
    pub fn arrange_by_hand(&mut self) {
        self.start_arranging();
    }

    /// The preferences saved last time: like [`Self::set_prefs`], except that a backdrop
    /// that hadn't run long enough to be trusted is dropped, with a note.
    pub fn restore_prefs(&mut self, mut prefs: theme::Prefs) {
        let (cockpit, note) = std::mem::take(&mut prefs.cockpit).restored();
        prefs.cockpit = cockpit;
        if let Some(note) = note {
            self.notice(NoticeLevel::Info, note);
        }
        self.set_prefs(prefs);
    }

    /// Restore (or change) appearance preferences; applied on the next frame.
    pub fn set_prefs(&mut self, prefs: theme::Prefs) {
        let prefs = prefs.sanitized();
        let reload = prefs.imported != self.prefs.imported;
        count::set_unit(prefs.thinking_size);
        self.prefs = prefs;
        self.prefs_gen = self.prefs_gen.wrapping_add(1);
        if reload {
            self.reload_themes();
        }
    }

    /// Desktop: where user theme files live (`<config>/themes`); loaded now and on Reload.
    pub fn set_themes_dir(&mut self, dir: std::path::PathBuf) {
        self.themes_dir = Some(dir);
        self.reload_themes();
    }

    /// The user's themes (themes folder or imported), for scripted runs and the CLI.
    pub fn user_themes(&self) -> &[theme::ThemeDef] {
        &self.user_themes
    }

    /// Re-read theme files: the themes folder (desktop) and imported ones (browser).
    fn reload_themes(&mut self) {
        #[cfg_attr(target_arch = "wasm32", allow(unused_mut))]
        let (mut defs, mut warnings) = theme::file::imported(&self.prefs);
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(dir) = &self.themes_dir {
            let (folder, w) = theme::file::load_dir(dir);
            warnings.extend(w);
            for d in folder {
                if defs.iter().any(|o| o.id == d.id) {
                    warnings.push(format!("two themes are called {:?}; showing the first", d.name));
                } else {
                    defs.push(d);
                }
            }
        }
        self.user_themes = defs;
        self.theme_warnings = warnings;
        self.prefs_gen = self.prefs_gen.wrapping_add(1);
    }

    /// Rebuild the tokens when the preferences or the OS/browser theme changed.
    fn apply_theme(&mut self, ctx: &egui::Context) {
        let key = (self.prefs_gen, ctx.system_theme());
        let fonts = (self.prefs.knobs.ui_font, self.prefs.knobs.mono_font);
        if self.fonts != Some(fonts) {
            theme::install_fonts(ctx, fonts.0, fonts.1);
            self.fonts = Some(fonts);
        }
        // The zoom: the stored preference is the one source of truth (byob handles Ctrl +/−/0
        // itself, see `header_shortcuts`, so egui's own keyboard zoom is off).
        if ctx.options(|o| o.zoom_with_keyboard) {
            ctx.options_mut(|o| o.zoom_with_keyboard = false);
        }
        if (ctx.zoom_factor() - self.prefs.zoom).abs() > 1e-3 {
            ctx.set_zoom_factor(self.prefs.zoom);
        }
        if self.applied != Some(key) {
            self.tokens = theme::Tokens::for_prefs(&self.prefs, key.1, &self.user_themes);
            if self.stage.active(&self.prefs.cockpit) {
                cockpit::glaze(&mut self.tokens, self.prefs.cockpit.glass);
            }
            self.tokens.apply(ctx);
            self.applied = Some(key);
        }
    }

    /// Open or close the settings (used by scripted screenshot runs).
    pub fn set_settings_open(&mut self, open: bool) {
        if open {
            self.open_settings();
        } else {
            self.close_settings();
        }
    }

    /// Open the settings at Models (the connection form on engines without saved models).
    pub fn open_connection(&mut self) {
        self.open_settings_at(settings::Tab::Models);
    }

    /// Scripted runs: put an endpoint in the connection form and test it, as if typed and
    /// "Test connection" pressed.
    pub fn test_connection(&mut self, url: &str, model: &str) {
        self.connect.fill(url, model);
        self.connect.start_test();
    }

    /// Scripted runs: open a workspace view: `new-workspace`, `new-workspace-container`,
    /// `workspace-settings` (of the chat in view's workspace), `chat-settings`, `more-menu` (the
    /// header's "⋯" menu), `model-menu` (the header's model switch), `models` (Settings →
    /// Models), `add-models:<url>` (its Add form, listing that server), `edit-model:<id>` (a
    /// profile's edit form there), `plugins` (Settings → Plugins);
    /// the chat list's `new-chat-menu` (New chat's caret menu), `workspace-menu` (the current
    /// workspace's "⋯" menu), `sidebar-hover` (its rows as if hovered), `chat-list` (the list
    /// open, on phones too), `new-chat` (as New chat does), `search:<text>` (typed
    /// into its search), `palette` / `palette:<text>` (the search palette), `footer-menu`,
    /// `projects` / `projects-grid` (the Projects page), `new-project-empty` (New project,
    /// started empty), and `new-workspace-folder:<path>` (the form as if that folder was
    /// dropped). False for an unknown name, or a workspace view before the workspace list arrived.
    pub fn open_panel(&mut self, name: &str) -> bool {
        use byob_protocol::workspace::EnvKind;
        match name {
            "new-workspace" => self.workspaces.open_new(EnvKind::Auto),
            _ if name.starts_with("new-workspace-folder:") => self.workspaces.open_with_folder(name.trim_start_matches("new-workspace-folder:")),
            "new-chat-menu" => self.sidebar.open_new_menu(),
            "workspace-menu" => return self.sidebar.open_workspace_menu(),
            "sidebar-hover" => self.sidebar.show_hover(),
            "chat-list" => self.sidebar.set_open(true),
            "new-chat" => {
                let msg = self.sidebar.new_chat_msg();
                self.conn.send(msg);
            }
            _ if name.starts_with("search:") => self.sidebar.set_search(name.trim_start_matches("search:")),
            _ if name.starts_with("palette:") => self.sidebar.open_palette(name.trim_start_matches("palette:")),
            "palette" => self.sidebar.open_palette(""),
            "footer-menu" => self.sidebar.open_footer_menu(),
            "projects" => self.page = Page::Projects,
            "projects-grid" => {
                self.projects.view = projects::View::Grid;
                self.page = Page::Projects;
            }
            "new-project-empty" => self.workspaces.open_start(projects::Start::Empty),
            "new-workspace-container" => {
                self.workspaces.open_new(EnvKind::Container);
                self.conn.send(ClientMsg::CheckContainers);
            }
            // "workspace-settings" or "workspace-settings:chats" (a part of it).
            _ if name == "workspace-settings" || name.starts_with("workspace-settings:") => {
                match self.sidebar.current_workspace().or_else(|| self.sidebar.workspaces().first()).map(|w| w.id.clone()) {
                    Some(id) => {
                        self.workspaces.open_settings(&id, false);
                        let part = name.trim_start_matches("workspace-settings").trim_start_matches(':');
                        if let Some(w) = &mut self.workspaces.window
                            && let Some(tab) = workspace::ProjectTab::find(part)
                        {
                            w.tab = tab;
                        }
                    }
                    None => return false,
                }
            }
            "more-menu" => self.header_rects.open_menu = true,
            "model-menu" => self.header_rects.open_model_menu = true,
            "models" => self.open_connection(),
            _ if name.starts_with("edit-model:") => {
                self.models.start_edit(name.trim_start_matches("edit-model:"));
                self.open_connection();
            }
            _ if name.starts_with("add-models:") => {
                self.models.start_add(name.trim_start_matches("add-models:"));
                self.open_connection();
            }
            "plugins" => {
                self.plugins.open_on_trouble();
                self.open_settings_at(settings::Tab::Plugins);
            }
            "plugin-pane" => return self.plugins.sites.open_pane(""),
            _ if name.starts_with("plugin-pane:") => return self.plugins.sites.open_pane(name.trim_start_matches("plugin-pane:")),
            _ if name.starts_with("artifact:") => return self.artifacts.show_matching(name.trim_start_matches("artifact:")),
            _ if name.starts_with("plugin:") => {
                self.plugins.open(Some(name.trim_start_matches("plugin:")));
                self.open_settings_at(settings::Tab::Plugins);
            }
            "chat-settings" => {
                if self.workspaces.chat.is_none() {
                    self.workspaces.toggle_chat();
                }
            }
            _ => return false,
        }
        true
    }

    /// Scripted runs: fill the New workspace form (a fresh folder) and press Create, as a
    /// person would.
    pub fn create_workspace(&mut self, name: &str, env: byob_protocol::workspace::EnvKind) {
        self.workspaces.open_new(env);
        if let Some(form) = &mut self.workspaces.form {
            form.name = name.to_owned();
            let msg = form.create_msg();
            self.conn.send(msg);
        }
    }

    /// The chat in view (or the new chat) is in the workspace called `name`.
    pub fn in_workspace(&self, name: &str) -> bool {
        self.sidebar.current_workspace().is_some_and(|w| w.name == name)
    }

    /// The names [`ByobApp::open_panel`] knows.
    pub const PANELS: &'static str = "new-workspace, new-workspace-container, workspace-settings, chat-settings, more-menu, model-menu, models, plugins, new-chat-menu, workspace-menu, sidebar-hover, chat-list, new-chat, palette, footer-menu, projects, projects-grid, new-project-empty";
    /// Names [`ByobApp::open_panel`] takes with an argument: `search:<text>`,
    /// `new-workspace-folder:<path>`.
    pub const ARG_PANELS: &'static str = "search:<text>, palette:<text>, new-workspace-folder:<path>, workspace-settings:<part>";
    /// Names [`ByobApp::open_panel`] takes once plugins are known: `plugin-pane` (the first
    /// plugin pane), `plugin-pane:<plugin>[/<view>]`, `plugin:<id>` (Settings → Plugins, open
    /// on that plugin).
    pub const PLUGIN_PANELS: &'static str = "plugin-pane, plugin-pane:<plugin>[/<view>], plugin:<id>, artifact:<prefix>";

    /// Scripted runs: open the settings on a tab ([`settings::Tab::id`]: `models`, `defaults`,
    /// `plugins`, `appearance`, `text`, `colors`; also `chats`, `knobs`, `advanced`), at one
    /// setting (its registry id, e.g. `accent`, `approvals`: scrolled to and lit), or at a
    /// saved model's form, Advanced expanded (`model:<id>`). False for a name it doesn't know.
    pub fn reveal_settings(&mut self, section: &str) -> bool {
        if let Some(s) = settings::registry::by_id(section.trim()) {
            self.jump_to(s.key);
            return true;
        }
        let Some(tab) = settings::Tab::find(section) else { return false };
        if let Some(id) = section.strip_prefix("model:") {
            self.models.reveal = Some(id.to_owned());
        }
        self.open_settings_at(tab);
        true
    }

    /// Type `text` into the composer without sending it, as a person would (scripted runs:
    /// `/` shows the menu).
    pub fn set_draft(&mut self, text: &str) {
        text.clone_into(&mut self.input);
        self.focus_composer = true;
    }

    /// Send a user message, as if typed into the composer.
    pub fn submit(&mut self, text: String) {
        // The first message makes the new chat: the engine opens it next.
        if self.sidebar.current().is_none() {
            self.sent_new = true;
        }
        let attachments = self.uploads.take();
        self.conn.send(ClientMsg::SendMessage { text, attachments });
    }

    /// A starter was clicked: send its prompt (with any attached files), keeping whatever is
    /// typed. While a file is still uploading it goes into the composer instead.
    fn start_with(&mut self, prompt: String) {
        if self.uploads.busy() {
            self.input = prompt;
            self.focus_composer = true;
        } else {
            self.submit(prompt);
        }
    }

    /// Some turn is stopped on an approval card the user has to answer.
    pub fn awaiting_approval(&self) -> bool {
        steps::awaiting_approval(&self.entries)
    }

    /// Connected, and nothing will change without the user: no turn running, no queued
    /// message about to go, and every entry finished streaming (and its charts/images
    /// loaded), or a turn is waiting on an approval card.
    pub fn settled(&self) -> bool {
        self.state.is_some()
            && (steps::awaiting_approval(&self.entries)
                || (!self.busy()
                    && !self.queue.moving()
                    && self.entries.iter().all(|e| e.done)
                    && !self.uploads.busy()
                    && !self.media.loading()
                    && !self.artifacts.loading()
                    && !self.plugins.sites.loading()))
    }

    /// Messages queued in the chat in view (scripted runs).
    pub fn queued(&self) -> usize {
        self.queue.queue.items.len()
    }

    /// Stop the running reply, as the Stop button does (scripted runs).
    pub fn stop(&mut self) {
        self.conn.send(ClientMsg::Cancel);
    }

    /// Edit the first queued message in the composer, as a click on its row does (scripted runs).
    pub fn edit_first_queued(&mut self) {
        if let Some(id) = self.queue.queue.items.first().map(|i| i.id) {
            self.begin_edit(id);
        }
    }

    /// Edit queued message `id` in the composer: the draft there is set aside until done.
    fn begin_edit(&mut self, id: u64) {
        let Some(item) = self.queue.item(id).cloned() else { return };
        if self.queue.editing.is_none() {
            self.queue.stash = Some((std::mem::take(&mut self.input), self.uploads.take_ready()));
        } else {
            let _ = self.uploads.take_ready();
        }
        self.input = item.text;
        self.uploads.restore(item.attachments);
        self.queue.editing = Some(id);
    }

    /// Stop editing a queued message: the set-aside draft comes back.
    fn end_edit(&mut self) {
        if self.queue.editing.take().is_none() {
            return;
        }
        let _ = self.uploads.take_ready();
        let (text, files) = self.queue.stash.take().unwrap_or_default();
        self.input = text;
        self.uploads.restore(files);
    }

    /// The queue changed: if the message being edited left it (it was sent), what is in the
    /// composer becomes an ordinary draft, with the set-aside one after it.
    fn queue_changed(&mut self) {
        if !self.queue.editing.is_some_and(|id| self.queue.item(id).is_none()) {
            return;
        }
        self.queue.editing = None;
        if let Some((text, files)) = self.queue.stash.take() {
            if !text.trim().is_empty() {
                if !self.input.trim().is_empty() {
                    self.input.push_str("\n\n");
                }
                self.input.push_str(&text);
            }
            self.uploads.restore(files);
        }
        self.notice(NoticeLevel::Info, "That queued message was sent before you finished editing; your edit is a draft now.".into());
    }

    fn queue_actions(&mut self, actions: Vec<queue::Action>) {
        for a in actions {
            match a {
                queue::Action::Edit(id) => self.begin_edit(id),
                queue::Action::Remove(id) => {
                    if self.queue.editing == Some(id) {
                        self.end_edit();
                    }
                    self.conn.send(ClientMsg::Unqueue { id });
                }
                queue::Action::SendNow(id) => self.conn.send(ClientMsg::SendQueuedNow { id }),
                queue::Action::Move(id, to) => self.conn.send(ClientMsg::MoveQueued { id, to }),
                queue::Action::Resume => self.conn.send(ClientMsg::ResumeQueue),
            }
        }
    }

    /// The engine has answered (its state arrived).
    pub fn connected(&self) -> bool {
        self.state.is_some()
    }

    /// A reply or compaction is running in the chat in view.
    pub fn busy(&self) -> bool {
        self.state.as_ref().is_some_and(|s| s.busy)
    }

    /// Draw one frame.
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        self.pump();
        self.captures.start(ui.ctx());
        self.captures.send(self.conn.as_mut());
        self.take_dropped_folders(ui.ctx());
        for e in self.uploads.poll(ui.ctx(), self.conn.as_mut()) {
            self.notice(NoticeLevel::Error, e);
        }
        self.poll_appearance();
        self.apply_theme(ui.ctx());
        self.fluid_begin(ui.ctx());
        layout::store(ui.ctx(), &self.layout);
        self.stage.begin(ui, &self.prefs.cockpit);
        math::set_enabled(ui.ctx(), self.math);
        self.report_look(ui.ctx());
        let narrow = ui.available_width() < sidebar::NARROW;
        self.artifacts.network_switch = self.engine_has(byob_protocol::version::CAP_ARTIFACT_NETWORK);
        self.artifacts.begin_frame(ui.ctx(), &self.tokens, narrow);
        let list_open = self.sidebar.is_open(narrow);
        // The chat comes first: as the window narrows, the sidebar and then the pane make way.
        let list_open = if narrow || self.show_settings {
            self.fit.reset();
            list_open
        } else {
            let mut open = list_open;
            for d in self.fit.step(ui.available_width(), list_open, self.artifacts.is_open()) {
                match d {
                    fit::Do::CloseSidebar => {
                        self.sidebar.set_open(false);
                        open = false;
                    }
                    fit::Do::OpenSidebar => {
                        self.sidebar.set_open(true);
                        open = true;
                    }
                    fit::Do::CollapsePane | fit::Do::OpenPane => self.artifacts.toggle_pane(),
                }
            }
            open
        };
        self.header_shortcuts(ui.ctx());
        let t = self.tokens;
        self.sidebar.set_artifacts(self.artifacts.len());
        self.sidebar.unattached = self.engine_has(byob_protocol::version::CAP_UNATTACHED);
        self.sidebar.archive = self.engine_has(byob_protocol::version::CAP_ARCHIVE);
        self.sidebar.status = self.footer_status();
        self.sidebar.page = match self.page {
            Page::Chat => sidebar::NavPage::None,
            Page::Projects => sidebar::NavPage::Projects,
            Page::Apps => sidebar::NavPage::Apps,
            Page::Library => sidebar::NavPage::Library,
            Page::Plugins => sidebar::NavPage::Plugins,
        };
        self.sidebar.plugins_attention = self.plugins.needs_attention();
        self.artifacts.pinned = self.sidebar.pins.iter().map(|p| p.id.clone()).collect();
        // A pinned artifact asked for from another chat: open it once its chat has loaded.
        // (The chat's artifact list comes last when a chat opens: wait for that one.)
        if let Some((chat, id)) = self.pending_artifact.clone()
            && self.artifacts.chat() == Some(chat.as_str())
        {
            if self.artifacts.open(&id) && std::mem::take(&mut self.pending_full) && !self.artifacts.is_maximized() {
                self.artifacts.toggle_maximized();
            }
            self.pending_artifact = None;
        }
        if self.free_now(narrow) {
            // A free arrangement: the header is one of its modules.
            self.free_frame(ui);
            self.end_frame(ui.ctx());
            return;
        }
        // Beside the chat, the list runs the full height and carries the window controls;
        // the header spans only the chat. Settings and maximized views have no list.
        self.sidebar_docked = list_open && !narrow && !self.show_settings && !self.artifacts.is_maximized() && self.artifacts.web_mut().maximized().is_none();
        if self.sidebar_docked && !fluid::collapsed(ui.ctx(), "sidebar") {
            self.chat_list_panel(ui, &t);
        }
        if !fluid::collapsed(ui.ctx(), "header") {
            self.header(ui);
        }
        upload::drop_overlay(ui.ctx(), &self.tokens);
        if self.show_settings {
            self.settings_keys(ui.ctx(), narrow);
        }
        if self.show_settings {
            // A destination of its own: everything below the header, the settings' navigation
            // where the chat list was. The chat, its list, composer and panes aren't drawn
            // (nor changed), so Back finds them as they were.
            self.settings_screen(ui, narrow);
            self.end_frame(ui.ctx());
            return;
        }
        if narrow && list_open {
            // Phones: the chat list takes the whole screen until a chat is picked.
            let sent = egui::CentralPanel::default().show(ui, |ui| self.sidebar.show(ui, &t, true)).inner;
            self.send_all(sent);
            self.end_frame(ui.ctx());
            return;
        }
        if narrow && self.plugins.sites.pane_is_open() {
            // Phones: a plugin's pane takes the whole screen; "← Chat" goes back.
            egui::CentralPanel::default().show(ui, |ui| self.plugins.sites.pane(ui, &t, true, self.artifacts.web_mut()));
            self.end_frame(ui.ctx());
            return;
        }
        if narrow && self.artifacts.is_open() {
            // Phones: the artifact takes the whole screen; "← Chat" goes back.
            egui::CentralPanel::default().show(ui, |ui| self.artifacts.pane(ui, &t, true));
            self.end_frame(ui.ctx());
            return;
        }
        if self.artifacts.web_mut().maximized().is_some() {
            // An app (MCP App, a plugin's web card) maximized over the window.
            egui::CentralPanel::default().show(ui, |ui| self.artifacts.web_mut().show_maximized(ui, &t));
            self.end_frame(ui.ctx());
            return;
        }
        if self.artifacts.is_maximized() {
            // Maximized: the artifact takes the whole window below the header (same pane, so
            // a running app just gets resized).
            egui::CentralPanel::default().show(ui, |ui| self.artifacts.pane(ui, &t, false));
            self.end_frame(ui.ctx());
            return;
        }
        match self.page {
            Page::Apps | Page::Library => {
                self.library_page(ui, &t);
                let msgs = self.library.take();
                self.send_all(msgs);
                self.end_frame(ui.ctx());
                return;
            }
            Page::Projects => {
                self.projects_page(ui, &t);
                self.end_frame(ui.ctx());
                return;
            }
            Page::Plugins => {
                self.plugins_page(ui, &t);
                self.end_frame(ui.ctx());
                return;
            }
            Page::Chat => {}
        }
        if !narrow {
            // The pane, or its rail when collapsed; then the plugins' pane beside it.
            let before = ui.available_width();
            let open = self.artifacts.is_open();
            self.artifacts.side_panel(ui, &t);
            if open {
                self.fit.pane_w = (before - ui.available_width()).max(crate::pane::MIN_WIDTH);
            }
            self.plugins.sites.side_panel(ui, &t, self.artifacts.web_mut());
        }
        // The chat and its composer sit on the window background (app_bg); the header, chat
        // list and panes on the panel color.
        let composer_frame = cockpit::framed(ui.ctx(), egui::Frame::side_top_panel(ui.style()).fill(t.app_bg));
        let chat_frame = cockpit::framed(ui.ctx(), egui::Frame::central_panel(ui.style()).fill(t.app_bg));
        if !fluid::collapsed(ui.ctx(), "composer") {
            let composer = egui::Panel::bottom("composer").show_separator_line(false).frame(composer_frame).show(ui, |ui| self.composer(ui));
            cockpit::mark(ui.ctx(), composer.response.rect);
        }
        let chat = egui::CentralPanel::default().frame(chat_frame).show(ui, |ui| {
            self.plugins.trust_card(ui, &t, &self.entries);
            self.transcript(ui);
        });
        cockpit::mark(ui.ctx(), chat.response.rect);
        if narrow {
            self.artifacts.floating_button(ui.ctx(), &t, chat.response.rect);
        }
        self.end_frame(ui.ctx());
    }

    /// The chat list at the left (desktop).
    fn chat_list_panel(&mut self, ui: &mut egui::Ui, t: &theme::Tokens) {
        // The id carries the width knob so changing it resizes the panel (egui remembers dragged sizes per id).
        let w = t.knobs.sidebar_width;
        #[allow(clippy::cast_possible_truncation)] // a few hundred points
        // Dragged narrower than this and let go, it closes (and reopens at its normal width).
        const SHUT: f32 = 170.0;
        let r = egui::Panel::left(("sidebar", w as i32, self.sidebar_gen))
            .resizable(true)
            .default_size(w)
            .size_range(130.0..=420.0)
            // Edge to edge; the list draws its glass and the hairline where it meets the chat.
            .show_separator_line(false)
            .frame(cockpit::framed(ui.ctx(), egui::Frame::NONE.fill(glass::fill(t))))
            .show(ui, |ui| self.sidebar.show(ui, t, false));
        cockpit::mark(ui.ctx(), r.response.rect);
        let width = r.response.rect.width();
        let dragging = ui.input(|i| i.pointer.any_down());
        if dragging && self.sidebar_last_w > 0.0 && (width - self.sidebar_last_w).abs() > 0.5 {
            self.sidebar_resized = true;
        }
        // Let go: a drag that ended narrow shuts it (the flag is spent either way). Narrow
        // without a drag (it came up while the window was still opening small): back to
        // its normal width once there's room.
        let resized = !dragging && std::mem::take(&mut self.sidebar_resized);
        if resized && width < SHUT {
            self.sidebar.set_open(false);
            self.sidebar_gen = self.sidebar_gen.wrapping_add(1);
        } else if !dragging && !resized && width < SHUT && ui.ctx().content_rect().width() >= w + fit::CHAT_MIN {
            self.sidebar_gen = self.sidebar_gen.wrapping_add(1);
        }
        if width >= SHUT {
            self.fit.sidebar_w = width;
        }
        self.sidebar_last_w = width;
        self.send_all(r.inner);
    }

    /// A folder dropped on the window (desktop) opens the New workspace form on it; files
    /// are left for the composer to attach. Browsers don't say where a dropped folder is.
    fn take_dropped_folders(&mut self, ctx: &egui::Context) {
        if cfg!(target_arch = "wasm32") {
            return;
        }
        let folders: Vec<egui::DroppedFileHandle> = ctx.input_mut(|i| {
            let (folders, files) = std::mem::take(&mut i.raw.dropped_files).into_iter().partition(|f| f.path().is_dir());
            i.raw.dropped_files = files;
            folders
        });
        // Folders dropped on the window: a new project from them, or more folders for the one
        // being made (what was typed stays).
        match &mut self.workspaces.form {
            Some(form) => folders.iter().for_each(|f| form.add_folder(&f.path().display().to_string())),
            None => {
                if let Some((first, rest)) = folders.split_first() {
                    self.workspaces.open_with_folder(&first.path().display().to_string());
                    if let Some(form) = &mut self.workspaces.form {
                        rest.iter().for_each(|f| form.add_folder(&f.path().display().to_string()));
                    }
                }
            }
        }
    }

    /// After drawing: artifact actions and overlays, and what they want sent or shown.
    fn end_frame(&mut self, ctx: &egui::Context) {
        self.fluid_end(ctx);
        // The chat list's arrangement changed here: shared through the layout.
        if let Some(list) = self.sidebar.take_list_change()
            && self.engine_has(byob_protocol::layout::CAP_LAYOUT)
        {
            let mut layout = (*self.layout).clone();
            layout.chat_list = list;
            self.layout = std::sync::Arc::new(layout.clone());
            self.conn.send(ClientMsg::SetLayout { layout });
        }
        self.end_backdrop(ctx);
        for r in self.sidebar.take_requests() {
            match r {
                sidebar::Request::NewWorkspace => self.workspaces.open_new(byob_protocol::workspace::EnvKind::Auto),
                sidebar::Request::WorkspaceSettings { id, remove } => self.workspaces.open_settings(&id, remove),
                sidebar::Request::AppSettings => self.open_settings(),
                sidebar::Request::Plugins => self.open_plugins_page(),
                sidebar::Request::Artifacts => {
                    if !self.artifacts.is_empty() {
                        self.artifacts.toggle_pane();
                    }
                }
                sidebar::Request::OpenArtifact { chat, id } => self.open_artifact_in(chat, id, false),
                sidebar::Request::Library => self.open_page(library::Mode::Library),
                sidebar::Request::Apps => self.open_page(library::Mode::Apps),
                sidebar::Request::Connection => self.open_connection(),
                sidebar::Request::Projects => {
                    self.page = Page::Projects;
                    self.close_settings();
                }
                sidebar::Request::LeavePage => self.page = Page::Chat,
                sidebar::Request::Help => self.begin_chat_with("How do I ".to_owned()),
                sidebar::Request::Feedback => ctx.open_url(egui::OpenUrl::new_tab("https://github.com/Scronkfinkle/byob/issues")),
                sidebar::Request::Compose(words) => self.begin_chat_with(words),
                sidebar::Request::OpenFile => {
                    // A new chat with the file picker open: what's picked is attached to it.
                    self.begin_chat_with(String::new());
                    self.uploads.pick(ctx);
                }
                sidebar::Request::LibraryList => {
                    if self.engine_has(byob_protocol::version::CAP_LIBRARY) {
                        self.conn.send(ClientMsg::ListLibrary);
                    }
                }
            }
        }
        let t = self.tokens;
        let title = self.sidebar.current().map_or("New chat", |c| c.title.as_str()).to_owned();
        self.chat_edit_dialog(ctx);
        self.sidebar.space_dialog(ctx, &t);
        // "Keep in Library" on a changed file (in a reply's steps).
        if let Some(path) = ctx.data_mut(|d| d.remove_temp::<String>(steps::keep_live_id()))
            && self.engine_has(byob_protocol::version::CAP_LIBRARY)
        {
            self.conn.send(ClientMsg::AddLiveFile { path });
        }
        let msgs = self.sidebar.palette_ui(ctx, &t);
        self.send_all(msgs);
        // The sheet hangs from the settings button, right edges lined up.
        let gear = self.header_rects.chat_settings;
        let anchor = egui::pos2(gear.right(), gear.bottom() + 6.0);
        let anchor = if gear.is_positive() { anchor } else { egui::pos2(ctx.content_rect().center().x - 200.0, 48.0) };
        let msgs = self.workspaces.windows(ctx, &t, self.sidebar.workspaces(), &title, anchor, &mut self.plugins);
        self.send_all(msgs);
        self.artifacts.end_frame(ctx);
        // The pane's pin: on the sidebar, or off it again.
        if let Some((id, title)) = self.artifacts.take_pin_toggle() {
            if self.sidebar.artifact_pinned(&id) {
                self.sidebar.unpin_artifact(&id);
            } else if let Some(chat) = self.sidebar.current().map(|c| c.id.clone()) {
                self.sidebar.pin_artifact(&chat, &id, &title);
            }
        }
        let (msgs, notices) = self.artifacts.take_output();
        self.send_all(msgs);
        let msgs = self.connect.take();
        self.send_all(msgs);
        let msgs = self.models.take();
        self.send_all(msgs);
        let msgs = self.plugins.take();
        self.send_all(msgs);
        if self.plugins.take_open_request() {
            self.open_settings_at(settings::Tab::Plugins);
        }
        for (level, text) in notices {
            self.notice(level, text);
        }
    }

    /// The backdrop's inputs for this frame; trust it after its trial, or switch it off when
    /// the GPU refused it.
    fn end_backdrop(&mut self, ctx: &egui::Context) {
        let busy = self.busy();
        let out = self.stage.end(ctx, &self.prefs.cockpit, &self.tokens, busy);
        if out.trusted {
            self.prefs.cockpit.trial = false;
        }
        if let Some(first) = out.failed.first() {
            self.notice(NoticeLevel::Error, format!("The backdrop was switched off: the GPU refused it ({first})."));
            self.prefs.cockpit.set_backdrop("");
            self.prefs_gen = self.prefs_gen.wrapping_add(1);
        }
    }

    fn send_all(&mut self, msgs: Vec<ClientMsg>) {
        for m in msgs {
            self.conn.send(m);
        }
    }

    fn transcript(&mut self, ui: &mut egui::Ui) {
        crate::fluid::region(ui, "chat", |ui| self.transcript_inner(ui))
    }

    fn transcript_inner(&mut self, ui: &mut egui::Ui) {
        let t = self.tokens;
        if self.entries.is_empty() {
            self.empty_chat(ui);
            return;
        }
        // Going back needs an engine that takes it and no reply running.
        let can_rewind = self.engine_has(byob_protocol::version::CAP_REWIND) && !self.state.as_ref().is_some_and(|s| s.busy);
        self.editing.allowed = can_rewind;
        if self.markdown.len() > self.entries.len() {
            let entries = &self.entries;
            self.markdown.retain(|id| entries.iter().any(|e| e.id == markdown::entry_of(id)));
        }
        let fab = self.artifacts.floating_shown();
        let markdown = &mut self.markdown;
        let media = &mut self.media;
        let artifacts = &mut self.artifacts;
        let usage = &mut self.usage;
        let plugins = &mut self.plugins;
        // E.g. each plugin's latest card is the one that offers its settings and Keep.
        let latest = self.views.latest(&self.entries);
        let views = &mut self.views;
        let mut answers = Vec::new();
        let mut card = None;
        // Only the last reply, failed before it produced anything, can be run again.
        let busy = self.state.as_ref().is_some_and(|s| s.busy);
        let retry_id = self.entries.last().filter(|e| retryable(e) && !busy).map(|e| e.id);
        let editing = &mut self.editing;
        // "Hide details" on this chat: its replies without thinking, tool steps, tokens and
        // time (questions and approvals still show); the layout is put back after.
        let chat_id = self.sidebar.current().map(|c| c.id.clone());
        let quiet = chat_id.as_ref().is_some_and(|id| self.sidebar.quiet.contains(id));
        if quiet {
            let mut l = (*self.layout).clone();
            for id in DETAILS {
                l.set_show(id, byob_protocol::layout::Show::Hidden);
            }
            layout::store(ui.ctx(), &std::sync::Arc::new(l));
        }
        let has_reply = self.entries.iter().any(|e| e.role == byob_protocol::EntryRole::Assistant);
        let mut flip = false;
        egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
            readable_column(ui, &t, |ui| {
                ui.add_space(t.pad(8.0));
                usage.dividers(ui, &t, 0);
                // Each message is an element of its own: `you.3` (the third of yours), `byob.2`.
                let (mut yours, mut replies) = (0, 0);
                for entry in &self.entries {
                    let name = if entry.role == byob_protocol::EntryRole::User {
                        yours += 1;
                        format!("you.{yours}")
                    } else {
                        replies += 1;
                        format!("byob.{replies}")
                    };
                    answers.extend(fluid::region(ui, &name, |ui| {
                        entry_view(ui, &t, entry, markdown, media, artifacts, (plugins, views, &latest), retry_id == Some(entry.id), &mut card, editing)
                    }));
                    ui.add_space(t.pad(8.0));
                    usage.dividers(ui, &t, entry.id);
                }
                // Under the latest reply: show or hide the replies' details for this chat.
                if has_reply && chat_id.is_some() {
                    // One row high: a right-to-left layout would otherwise take all the height left.
                    let row = egui::vec2(ui.available_width(), ui.text_style_height(&egui::TextStyle::Small) + 4.0);
                    ui.allocate_ui_with_layout(row, egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let label = if quiet { "Show details" } else { "Hide details" };
                        let r = ui.add(egui::Label::new(egui::RichText::new(label).small().color(t.text_faint)).sense(egui::Sense::click()));
                        let r = r.on_hover_cursor(egui::CursorIcon::PointingHand);
                        let r = tip::note(r, &t, label, "Thinking, tool steps, tokens and time, for this chat");
                        if r.clicked() {
                            flip = true;
                        }
                    });
                    ui.add_space(t.pad(8.0));
                }
                if fab {
                    // Phones: the end of the chat scrolls clear of the floating button.
                    ui.add_space(artifact::FLOATING_CLEARANCE);
                }
            });
        });
        if quiet {
            layout::store(ui.ctx(), &self.layout);
        }
        if flip
            && let Some(id) = chat_id
            && !self.sidebar.quiet.remove(&id)
        {
            self.sidebar.quiet.insert(id);
        }
        answers.extend(self.media.take_requests());
        match card {
            Some(connect::CardAction::Retry) => answers.push(ClientMsg::Retry),
            Some(connect::CardAction::OpenSettings) => self.open_connection(),
            None => {}
        }
        for msg in answers {
            self.conn.send(msg);
        }
    }

    /// No messages yet: the welcome screen until something has worked, what's wrong if the
    /// endpoint stopped answering, else an invitation.
    fn empty_chat(&mut self, ui: &mut egui::Ui) {
        let t = self.tokens;
        let view = connect::empty_view(self.state.as_ref());
        let Some(state) = self.state.as_ref() else {
            ui.centered_and_justified(|ui| theme::spinner(ui, &t, None));
            return;
        };
        match view {
            connect::EmptyView::Welcome => self.connect.welcome(ui, &t, state),
            connect::EmptyView::Trouble => match connect::trouble(ui, &t, state) {
                // Check again: set the same endpoint, which checks it.
                Some(connect::CardAction::Retry) => {
                    let msg = ClientMsg::SetInference { base_url: state.inference_base_url.clone(), model: state.inference_model.clone(), api_key: None };
                    self.conn.send(msg);
                }
                Some(connect::CardAction::OpenSettings) => self.open_connection(),
                None => {}
            },
            connect::EmptyView::Ready | connect::EmptyView::Waiting => {
                // Switched-on plugins' starters (`starters` in their manifests): a click
                // sends the prompt.
                let starters = if crate::layout::visible(ui.ctx(), "chat.starters", false) { self.plugins.starters() } else { Vec::new() };
                if starters.is_empty() {
                    ui.centered_and_justified(|ui| {
                        ui.label(egui::RichText::new("Ask anything to get started.").color(t.text_dim));
                    });
                    return;
                }
                let mut picked = None;
                ui.vertical_centered(|ui| {
                    ui.add_space((ui.available_height() * 0.35).max(t.pad(12.0)));
                    ui.label(egui::RichText::new("Ask anything to get started, or try:").color(t.text_dim));
                    ui.add_space(t.pad(6.0));
                    for s in &starters {
                        let button = egui::Button::new(egui::RichText::new(&s.title).color(t.text)).corner_radius(t.round(8.0));
                        if ui.add(button).on_hover_text(&s.prompt).clicked() {
                            picked = Some(s.prompt.clone());
                        }
                        ui.add_space(t.pad(4.0));
                    }
                });
                if let Some(p) = picked {
                    self.start_with(p);
                }
            }
        }
    }
}

/// A reply that failed before it produced anything (see [`ClientMsg::Retry`]).
fn retryable(e: &Entry) -> bool {
    e.role == EntryRole::Assistant && e.done && e.error.is_some() && e.content.is_empty() && e.steps.is_empty()
}

const EDIT_TIP: &str = "Change this message and start over from it";

/// The Edit button on one of your messages.
fn edit_button(ui: &mut egui::Ui) -> bool {
    ui.add(egui::Button::new(egui::RichText::new("Edit").small())).on_hover_text(EDIT_TIP).clicked()
}

/// One of your messages, being changed: its text, what sending does, Cancel and Send
/// (Ctrl+Enter sends, Esc cancels). Sending goes back to it (`ClientMsg::Rewind`).
fn edit_box(ui: &mut egui::Ui, t: &theme::Tokens, entry: &Entry, editing: &mut Editing) -> Option<ClientMsg> {
    let allowed = editing.allowed;
    let (id, text) = editing.open.as_mut()?;
    let field_id = egui::Id::unique(("edit-message", *id));
    let first = !ui.data(|d| d.get_temp::<bool>(field_id).unwrap_or(false));
    // Ctrl+Enter and Esc are ours, taken before the field would turn them into a new line
    // or a lost focus.
    let focused = ui.memory(|m| m.has_focus(field_id));
    let keys = if focused {
        ui.input_mut(|i| (i.consume_key(egui::Modifiers::COMMAND, egui::Key::Enter), i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)))
    } else {
        (false, false)
    };
    let resp = ui.add(egui::TextEdit::multiline(text).id(field_id).desired_width(f32::INFINITY).desired_rows(2));
    if first {
        resp.request_focus();
        ui.data_mut(|d| d.insert_temp(field_id, true));
    }
    let attached = match entry.attachments.len() {
        0 => String::new(),
        1 => " It keeps its attachment.".to_owned(),
        n => format!(" It keeps its {n} attachments."),
    };
    let note = format!("Sending starts over from here: the replies after this message are removed (files they changed stay as they are).{attached}");
    ui.add(egui::Label::new(egui::RichText::new(note).small().color(t.text_dim)).wrap());
    let ready = allowed && !text.trim().is_empty();
    let (mut send, mut cancel) = (false, false);
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let tip = if allowed { "Send it (Ctrl+Enter)" } else { "Wait for the reply to finish, or stop it" };
            send = ui
                .add_enabled(ready, egui::Button::new(egui::RichText::new("Send").color(t.on_accent)).fill(t.accent))
                .on_hover_text(tip)
                .on_disabled_hover_text(tip)
                .clicked();
            cancel = ui.button("Cancel").on_hover_text("Keep the chat as it is (Esc)").clicked();
        });
    });
    send |= keys.0 && ready;
    cancel |= keys.1;
    let id = *id;
    if send {
        let msg = ClientMsg::Rewind { entry: id, text: text.trim_end().to_owned() };
        editing.open = None;
        ui.data_mut(|d| d.remove::<bool>(field_id));
        return Some(msg);
    }
    if cancel {
        editing.open = None;
        ui.data_mut(|d| d.remove::<bool>(field_id));
    }
    None
}

/// Draws one entry; returns a message to send if the user answered an approval card. A
/// connection error is a card (with Retry if `retry`) whose button lands in `card`.
#[allow(clippy::too_many_arguments)] // the transcript's per-entry caches
fn entry_view(
    ui: &mut egui::Ui,
    t: &theme::Tokens,
    entry: &Entry,
    markdown: &mut markdown::Cache,
    media: &mut media::Cache,
    artifacts: &mut artifact::Artifacts,
    (plugins, views, latest): (&mut plugins::PluginsUi, &mut views::ViewRegistry, &std::collections::BTreeMap<(String, String), u64>),
    retry: bool,
    card: &mut Option<connect::CardAction>,
    editing: &mut Editing,
) -> Option<ClientMsg> {
    let mut answer = None;
    let user = entry.role == EntryRole::User;
    let (fill, style) = if user { (t.user_bubble, t.knobs.user_bubble) } else { (t.assistant_bubble, t.knobs.assistant_bubble) };
    let (fill, stroke, margin) = match style {
        theme::BubbleStyle::Filled => (fill, egui::Stroke::NONE, t.pad(t.knobs.bubble_padding)),
        theme::BubbleStyle::Outlined => (
            egui::Color32::TRANSPARENT,
            egui::Stroke::new(1.0, if user { theme::mix(t.border, t.accent, 0.6) } else { t.border }),
            t.pad(t.knobs.bubble_padding),
        ),
        theme::BubbleStyle::Minimal => (egui::Color32::TRANSPARENT, egui::Stroke::NONE, t.pad(t.knobs.bubble_padding * 0.4)),
    };
    // Chat alignment: your messages on the right, as wide as they need up to 80 %.
    let full = ui.available_width();
    let width = if user && t.knobs.align == theme::Align::Chat {
        let natural = ui.painter().layout_no_wrap(entry.content.clone(), egui::TextStyle::Body.resolve(ui.style()), t.text).size().x;
        (natural + 2.0 * margin + 4.0).clamp(140.0_f32.min(full), (full * 0.8).max(140.0_f32.min(full)))
    } else {
        full
    };
    let layout = if width < full { egui::Layout::right_to_left(egui::Align::Min) } else { egui::Layout::top_down(egui::Align::Min) };
    // The bubble's rect from the last frame: the reply's details show while the pointer is on it.
    let rect_id = ui.make_persistent_id(("bubble-rect", entry.id));
    let last = ui.data(|d| d.get_temp::<egui::Rect>(rect_id));
    // A touch screen has no hover: a touch on a message shows what the pointer on it would
    // (Edit on yours, a reply's details) until another message is touched.
    let touched_id = egui::Id::unique("byob-touched-message");
    let (touch, pressed_at) = ui.input(|i| (i.has_touch_screen(), i.pointer.any_pressed().then(|| i.pointer.interact_pos()).flatten()));
    if touch && pressed_at.is_some_and(|p| last.is_some_and(|r| r.contains(p))) {
        ui.data_mut(|d| d.insert_temp(touched_id, entry.id));
    }
    let touched = touch && ui.data(|d| d.get_temp::<u64>(touched_id)) == Some(entry.id);
    let hovered = touched || last.is_some_and(|r| ui.rect_contains_pointer(r));
    let shape = crate::layout::get(ui.ctx());
    let (details, on_hover) = entry.stats.filter(|_| entry.role == EntryRole::Assistant).map(|s| reply_details(&shape, &s)).unwrap_or_default();
    ui.with_layout(layout, |ui| {
        let shown = egui::Frame::new().fill(fill).stroke(stroke).corner_radius(t.round(8.0)).inner_margin(margin).show(ui, |ui| {
            ui.set_width(width - 2.0 * margin);
            ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                let waiting = entry.pending_approvals().next().is_some();
                if shape.visible("message.role", hovered) {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(if user { "You" } else { "byob" }).strong().color(t.accent));
                        if waiting {
                            steps::waiting_badge(ui, t);
                        } else if !entry.done {
                            theme::spinner(ui, t, None);
                        }
                    });
                } else if waiting {
                    // Without the label, only what's going on.
                    ui.horizontal(|ui| steps::waiting_badge(ui, t));
                } else if !entry.done && entry.content.is_empty() && entry.steps.is_empty() && entry.reasoning.is_empty() {
                    theme::spinner(ui, t, None);
                }
                let thinking = !entry.done && entry.content.is_empty() && entry.steps.iter().all(|s| s.status == StepStatus::Running);
                if !entry.reasoning.is_empty() && (thinking || shape.visible("message.thinking", hovered)) {
                    let size = count::thinking_size(entry.id, &entry.reasoning);
                    let title = match (thinking, shape.visible("message.thought_size", hovered)) {
                        (true, true) => format!("Thinking… · {size}"),
                        (true, false) => "Thinking…".to_owned(),
                        (false, true) => format!("Thought · {size}"),
                        (false, false) => "Thought".to_owned(),
                    };
                    egui::CollapsingHeader::new(egui::RichText::new(title).color(t.text_dim))
                        .id_salt(("thinking", entry.id))
                        .show(ui, |ui| {
                            // Reasoning can run to many thousands of words: keep it in its own scroll box,
                            // following the newest text while the model is still thinking. It's
                            // Markdown like the reply (paragraphs, lists, math), set off by a bar.
                            let shown = egui::Frame::new().inner_margin(egui::Margin { left: 12, right: 0, top: 2, bottom: 2 }).show(ui, |ui| {
                                egui::ScrollArea::vertical()
                                    .id_salt(("thinking-scroll", entry.id))
                                    .max_height(THINKING_MAX_HEIGHT)
                                    .auto_shrink([false, true])
                                    .stick_to_bottom(thinking)
                                    .show(ui, |ui| {
                                        let doc = markdown.part(markdown::thinking_key(entry.id), 0, &entry.reasoning, !thinking);
                                        markdown::show_in(ui, t, doc, egui::Id::unique(("thinking-md", entry.id)), t.text_dim);
                                    });
                            });
                            let r = shown.response.rect;
                            ui.painter().rect_filled(egui::Rect::from_min_max(r.min, egui::pos2(r.min.x + 3.0, r.max.y)), 1.5, t.quote_bar);
                        })
                        .header_response
                        .on_hover_text(format!("{size} of reasoning"));
                }
                match entry.role {
                    // reply text with its tool steps where they happened, approval cards, undo
                    EntryRole::Assistant => {
                        let newest = artifact::newest_cards(&entry.steps);
                        answer = steps::body(ui, t, entry, hovered, markdown, &mut |ui, step| {
                            artifacts.step_cards(ui, t, step.id, &step.artifacts, &newest);
                            views.show_step_with(ui, t, step, latest, plugins, artifacts.web_mut());
                        });
                    }
                    EntryRole::User if editing.open.as_ref().is_some_and(|(id, _)| *id == entry.id) => {
                        answer = edit_box(ui, t, entry, editing);
                    }
                    EntryRole::User => {
                        if !entry.content.is_empty() {
                            let mut text = egui::RichText::new(&entry.content).color(t.text);
                            if let Some(h) = theme::line_height(t, egui::TextStyle::Body.resolve(ui.style()).size) {
                                text = text.line_height(Some(h));
                            }
                            ui.add(egui::Label::new(text).selectable(true));
                        }
                        if editing.allowed && shape.show("message.edit") == byob_protocol::layout::Show::Shown {
                            // One row (a bare right-to-left layout would take the rest of the height).
                            ui.horizontal(|ui| {
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if edit_button(ui) {
                                        editing.open = Some((entry.id, entry.content.clone()));
                                    }
                                });
                            });
                        }
                    }
                }
                media.show_entry(ui, t, entry);
                if let Some(err) = &entry.error
                    && let Some(action) = connect::error_card(ui, t, err, retry)
                {
                    *card = Some(action);
                }
                if !details.is_empty() {
                    ui.label(egui::RichText::new(details.join(" · ")).small().color(t.text_dim));
                }
            });
        });
        let bubble = shown.response.rect;
        ui.data_mut(|d| d.insert_temp(rect_id, bubble));
        // Details on hover: over the reply's bottom-right corner while the pointer is on it
        // (a row that appeared and disappeared would move everything under it).
        if hovered && !on_hover.is_empty() {
            let text = on_hover.join(" · ");
            let p = ui.painter();
            let galley = p.layout_no_wrap(text, egui::TextStyle::Small.resolve(ui.style()), t.text_dim);
            let pad = egui::vec2(6.0, 2.0);
            let inset = 4.0;
            let tag = egui::Rect::from_min_size(bubble.right_bottom() - galley.size() - 2.0 * pad - egui::vec2(inset, inset), galley.size() + 2.0 * pad);
            let solid = if fill == egui::Color32::TRANSPARENT { t.app_bg } else { fill };
            p.rect_filled(tag, theme::inner_radius(t.round(8.0), inset), solid);
            p.galley(tag.min + pad, galley, t.text_dim);
        }
        // Edit on hover: over your message's bottom-right corner while the pointer is on it.
        let editing_this = editing.open.as_ref().is_some_and(|(id, _)| *id == entry.id);
        if user && editing.allowed && !editing_this && hovered && shape.show("message.edit") == byob_protocol::layout::Show::Hover {
            let size = egui::vec2(44.0, 20.0);
            let at = egui::Rect::from_min_size(bubble.right_bottom() - size - egui::vec2(4.0, 4.0), size);
            if ui.put(at, egui::Button::new(egui::RichText::new("Edit").small())).on_hover_text(EDIT_TIP).clicked() {
                editing.open = Some((entry.id, entry.content.clone()));
            }
        }
    });
    answer
}

/// A finished reply's details in the layout's order: those always shown, and those shown
/// while the pointer is on the reply.
fn reply_details(layout: &byob_protocol::layout::Layout, s: &byob_protocol::TurnStats) -> (Vec<String>, Vec<String>) {
    use byob_protocol::layout::{REPLY, Show};
    #[allow(clippy::cast_precision_loss)] // display only
    let secs = s.elapsed_ms as f64 / 1000.0;
    let (mut shown, mut hover) = (Vec::new(), Vec::new());
    for id in layout.arrange(REPLY, &[]) {
        let text = match id.as_str() {
            "reply.tokens" => tokens_detail(s),
            "reply.time" => format!("{secs:.1} s"),
            #[allow(clippy::cast_precision_loss)] // display only
            "reply.speed" if secs > 0.05 && s.completion_tokens > 0 => format!("{:.0} tokens/s", s.completion_tokens as f64 / secs),
            _ => continue,
        };
        match layout.show(&id) {
            Show::Shown => shown.push(text),
            Show::Hover => hover.push(text),
            Show::Hidden => {}
        }
    }
    (shown, hover)
}

/// A reply's tokens: how full the context was at its end (as the header's meter counts) and
/// what the model wrote; with several requests, what they read together (each tool round
/// reads the whole conversation again, so that adds up fast). Replies saved before the
/// context was recorded say what they always said.
fn tokens_detail(s: &byob_protocol::TurnStats) -> String {
    use byob_protocol::usage::short_tokens;
    if s.context_tokens == 0 {
        return format!("{} in · {} out tokens", s.prompt_tokens, s.completion_tokens);
    }
    let mut out = format!("{} context · {} out", short_tokens(s.context_tokens), short_tokens(s.completion_tokens));
    if s.model_calls > 1 {
        out.push_str(&format!(" · {} calls read {}", s.model_calls, short_tokens(s.prompt_tokens)));
    }
    out
}

/// The transcript in a centered column no wider than the user's readable width.
fn readable_column(ui: &mut egui::Ui, t: &theme::Tokens, add: impl FnOnce(&mut egui::Ui)) {
    let full = ui.available_width();
    let max = t.knobs.max_width;
    if max <= 0.0 || full <= max {
        add(ui);
        return;
    }
    let pad = ((full - max) / 2.0).floor();
    ui.horizontal_top(|ui| {
        ui.add_space(pad - ui.spacing().item_spacing.x);
        ui.vertical(|ui| {
            ui.set_width(max);
            add(ui);
        });
    });
}

#[cfg(test)]
mod app_tests {
    use std::sync::{Arc, Mutex};

    use byob_protocol::{ChatSummary, EngineState, Link};

    use super::*;

    /// The app settings page: tabs, deep links, and the area it covers.
    mod settings_page;

    /// Markdown in the composer: heights and the bold/italic shortcuts.
    mod composer_md;

    /// The layout: what replies, the composer and the header show, and in what order.
    mod declutter;

    /// The window reports its look and applies changes the model asks for.
    mod look;
    mod rewind;

    /// Fluid in the window: inserts (the plugins' too) and their actions.
    mod inserts;

    /// The Projects page and the project dialogs over it.
    mod projects_page;

    /// The window across arrangements, themes and sizes, checked for layout invariants.
    mod matrix;

    /// A connection that delivers queued messages and records what the app sends.
    #[derive(Clone, Default)]
    struct Fake {
        inbox: Arc<Mutex<Vec<ServerMsg>>>,
        sent: Arc<Mutex<Vec<ClientMsg>>>,
    }

    impl Connection for Fake {
        fn send(&mut self, msg: ClientMsg) {
            self.sent.lock().unwrap().push(msg);
        }
        fn try_recv(&mut self) -> Option<ServerMsg> {
            let mut q = self.inbox.lock().unwrap();
            (!q.is_empty()).then(|| q.remove(0))
        }
    }

    fn frame(ctx: &egui::Context, app: &mut ByobApp, width: f32) {
        let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 700.0))), ..Default::default() };
        let mut out = ctx.run_ui(input, |ui| app.ui(ui));
        out.textures_delta.clear();
    }

    fn state(link: Link, onboarded: bool) -> EngineState {
        EngineState {
            inference_base_url: "https://a-very-long-hostname.inference.example.com/some/deep/path/v1".into(),
            inference_model: "an-extremely-long-model-name-that-goes-on-and-on-q4_k_m".into(),
            link,
            onboarded,
            ..Default::default()
        }
    }

    /// A chat as crowded as the header gets: long workspace name and title, its own endpoint
    /// with long names, a context meter, an artifact.
    fn crowded_app() -> ByobApp {
        crowded_with(Fake::default())
    }

    /// [`crowded_app`] on `fake`.
    fn crowded_with(fake: Fake) -> ByobApp {
        let title = "A chat with a remarkably long title that would never fit in any header at all, really".to_owned();
        let ws = byob_protocol::WorkspaceSummary {
            id: "w".into(),
            name: "A project whose name is also much longer than anyone would reasonably type".into(),
            folder: "/w".into(),
            chats: 1,
            updated_at: 1,
            env: byob_protocol::workspace::EnvKind::Container,
            ..Default::default()
        };
        let mut s = state(Link::Ok, true);
        s.chat_endpoint = Some(byob_protocol::ChatEndpoint {
            base_url: "https://another-very-long-hostname.example.com/v1".into(),
            model: "yet-another-extremely-long-model-name".into(),
            from: byob_protocol::workspace::Level::Workspace,
            ..Default::default()
        });
        let artifact = byob_protocol::artifact::Artifact {
            id: "a".into(),
            kind: byob_protocol::artifact::ArtifactKind::Markdown,
            content_type: byob_protocol::artifact::TYPE_MARKDOWN.into(),
            title: "Notes".into(),
            filename: "notes.md".into(),
            versions: vec![byob_protocol::artifact::ArtifactVersion {
                n: 1,
                path: "a/v1.md".into(),
                created_at: 1,
                note: String::new(),
                size: 1,
                turn: String::new(),
            }],
            network: false,
        };
        fake.inbox.lock().unwrap().extend([
            ServerMsg::State(s),
            ServerMsg::Workspaces { items: vec![ws], current: None, root: String::new(), support: Default::default() },
            ServerMsg::ChatOpened {
                chat: Some(ChatSummary { id: "c".into(), title, updated_at: 1, pinned_at: None, busy: false, workspace: Some("w".into()), archived_at: None }),
                workspace: Some("w".into()),
            },
            ServerMsg::Usage(byob_protocol::usage::ChatUsage {
                chat: "c".into(),
                context_used: 3400,
                context_window: 262_144,
                compact_at: 200_000,
                ..Default::default()
            }),
            ServerMsg::Artifacts { chat: Some("c".into()), artifacts: vec![artifact] },
            plugins::tests::ext(vec![troubled_plugin(), pane_plugin()]),
        ]);
        ByobApp::new(Box::new(fake), "test")
    }

    fn troubled_plugin() -> byob_protocol::PluginInfo {
        let mut p = plugins::tests::info("weather", byob_protocol::PluginScope::Workspace, byob_protocol::PluginOrigin::Agent);
        p.problems = vec!["tools/x.py is missing".into()];
        p
    }

    /// A plugin with a pane, as the bundled memory plugin has: its toggle is in the header.
    fn pane_plugin() -> byob_protocol::PluginInfo {
        let mut p = plugins::tests::info("memory", byob_protocol::PluginScope::Global, byob_protocol::PluginOrigin::User);
        p.views = vec![byob_protocol::ViewDecl {
            id: "notes".into(),
            site: byob_protocol::Site::Pane,
            title: "Memory".into(),
            icon: String::new(),
            html: String::new(),
        }];
        p.backend = Some(byob_protocol::BackendInfo { state: "running".into(), ..Default::default() });
        p
    }

    /// On a phone the "⋯" menu has the plugins' panes (no room in the header) and no
    /// "Arrange panels…" (free arrangements need a wider window: it did nothing there).
    #[test]
    fn a_phones_more_menu_has_the_panes_and_no_arranging() {
        for (width, phone) in [(390.0, true), (1000.0, false)] {
            let ctx = egui::Context::default();
            let mut app = crowded_app();
            frame(&ctx, &mut app, width);
            app.header_rects.open_menu = true;
            let mut texts = Vec::new();
            for _ in 0..3 {
                let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 700.0))), ..Default::default() };
                let mut out = ctx.run_ui(input, |ui| app.ui(ui));
                out.textures_delta.clear();
                texts = plugins::tests::texts_of(&out);
            }
            let more = app.header_rects.more;
            let below = |want: &str| texts.iter().any(|t| t.0 == want && t.1.top() > more.bottom());
            assert!(below("App settings"), "{width}: the menu is open: {texts:?}");
            assert_eq!(below("Memory"), phone, "{width}: Memory in the menu: {texts:?}");
            assert_eq!(below("Arrange panels…"), !phone, "{width}: Arrange panels: {texts:?}");
        }
    }

    #[test]
    fn header_buttons_stay_on_screen_and_apart_at_every_width() {
        let ctx = egui::Context::default();
        let mut app = crowded_app();
        for width in (320..=1600).step_by(10) {
            #[allow(clippy::cast_precision_loss)] // small
            let width = width as f32;
            for _ in 0..3 {
                frame(&ctx, &mut app, width);
            }
            let r = app.header_rects;
            // The chat list toggle is in the header while the list is closed (phones), else at
            // the top of the list itself.
            let toggle = if r.chats.is_positive() { r.chats } else { app.sidebar.rects.toggle };
            for (name, rect) in [("chat list", toggle), ("chat settings", r.chat_settings), ("more", r.more)] {
                assert!(rect.is_positive() && rect.min.x >= 0.0 && rect.max.x <= width + 0.5, "width {width}: {name} at {rect:?}");
            }
            assert!(toggle.max.x <= r.plugins.min.x, "width {width}: {r:?}");
            // At the right: the plugins chip, the chat's settings, then the "⋯" menu.
            assert!(r.plugins.is_positive() && r.plugins.max.x <= r.chat_settings.min.x && r.chat_settings.max.x <= r.more.min.x, "width {width}: {r:?}");
        }
    }

    /// This crate runs in the browser, where std's clock panics ("time not implemented on
    /// this platform"): outside tests, the time comes from `web_time`.
    #[test]
    fn the_browser_build_never_asks_std_for_the_time() {
        fn walk(dir: &std::path::Path, out: &mut Vec<String>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") && !p.ends_with("tests.rs") && !p.to_string_lossy().contains("_tests") {
                    let text = std::fs::read_to_string(&p).unwrap();
                    // Up to the file's tests (they run natively).
                    let code = text.split("#[cfg(test)]").next().unwrap_or_default();
                    for (n, line) in code.lines().enumerate() {
                        if ["std::time::Instant", "std::time::SystemTime", "use std::time::{"].iter().any(|bad| line.contains(bad)) {
                            out.push(format!("{}:{}: {}", p.display(), n + 1, line.trim()));
                        }
                    }
                }
            }
        }
        let mut found = Vec::new();
        walk(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut found);
        assert!(found.is_empty(), "use web_time's Instant / SystemTime:\n{}", found.join("\n"));
    }

    /// A free arrangement (floating) draws the list toggle, Back and Forward once: at the top
    /// of the chat list while it is shown, in the header once it is hidden.
    #[test]
    fn free_arrangements_show_the_window_controls_once() {
        let ctx = egui::Context::default();
        let mut app = crowded_app();
        app.prefs.cockpit.arrangement = cockpit::arrange::Arrangement::preset("floating").unwrap();
        for _ in 0..3 {
            frame(&ctx, &mut app, 1100.0);
        }
        assert!(app.sidebar.is_open(false), "the list is shown");
        assert!(app.sidebar.rects.toggle.is_positive(), "the list carries the controls");
        assert!(!app.header_rects.chats.is_positive(), "and the header doesn't repeat them: {:?}", app.header_rects.chats);
        app.sidebar.toggle();
        for _ in 0..3 {
            frame(&ctx, &mut app, 1100.0);
        }
        assert!(app.header_rects.chats.is_positive(), "the list hidden, the header has them");
    }

    /// The composer in a state: a reply running or not, a queued message being edited or
    /// not, and what is typed; drawn until it settles in a window `width` wide.
    fn composer_in(ctx: &egui::Context, app: &mut ByobApp, width: f32, busy: bool, editing: bool, input: &str) -> composer::Rects {
        if let Some(s) = app.state.as_mut() {
            s.busy = busy;
        }
        app.queue.editing = editing.then_some(7);
        app.input = input.into();
        for _ in 0..3 {
            frame(ctx, app, width);
        }
        app.composer_rects
    }

    #[test]
    fn the_composer_is_one_row_that_grows_with_the_text() {
        let ctx = egui::Context::default();
        let mut app = crowded_app();
        let five = "one\ntwo\nthree\nfour\nfive";
        for width in [400.0, 1000.0] {
            let mut empty_heights = Vec::new();
            for (busy, editing, input) in
                [(false, false, ""), (false, false, five), (true, false, ""), (true, false, "more"), (false, true, ""), (false, true, five)]
            {
                let r = composer_in(&ctx, &mut app, width, busy, editing, input);
                let at = format!("width {width}, busy {busy}, editing {editing}, {:?}: {r:?}", input.lines().count());
                assert!(r.frame.is_positive() && r.field.is_positive() && r.attach.is_positive() && r.send.is_positive(), "{at}");
                assert!(r.frame.max.x <= width + 0.5, "{at}");
                for c in [r.field, r.attach, r.send] {
                    assert!(r.frame.contains_rect(c), "{at}");
                }
                // One row: paperclip at the left, the text, send at the right, all on one
                // bottom edge (one line of text is as tall as the controls).
                // The mic, when there is one, is rightmost and send sits just left of it.
                let last = if r.mic.is_positive() { r.mic } else { r.send };
                assert!(r.attach.left() - r.frame.left() < 12.0 && r.frame.right() - last.right() < 12.0, "{at}");
                assert!(r.send == last || r.send.right() <= last.left() + 0.5, "{at}");
                assert!((r.attach.center().y - r.send.center().y).abs() < 0.5 && r.frame.bottom() - r.send.bottom() < 12.0, "{at}");
                assert!((r.field.bottom() - r.send.bottom()).abs() < 1.5, "{at}");
                let right_of_text = if r.second.is_positive() { r.second.left() } else { r.send.left() };
                assert!(r.attach.right() <= r.field.left() && r.field.right() <= right_of_text, "{at}");
                if r.second.is_positive() {
                    assert!(r.frame.contains_rect(r.second) && r.second.right() <= r.send.left(), "{at}");
                    assert!((r.second.center().y - r.send.center().y).abs() < 0.5, "{at}");
                }
                if input.is_empty() {
                    assert!((r.field.height() - r.send.height()).abs() < 1.5, "one line, as tall as the controls: {at}");
                    empty_heights.push(r.frame.height());
                } else if input == five {
                    assert!(r.field.height() > 4.0 * 14.0, "the field grows with the text: {at}");
                }
            }
            // No jumps: idle, busy and editing are all the same height when empty.
            assert!(empty_heights.windows(2).all(|w| (w[0] - w[1]).abs() < 0.5), "width {width}: {empty_heights:?}");
            // A long message grows the field only so far, then scrolls.
            let long = "line\n".repeat(80);
            let r = composer_in(&ctx, &mut app, width, false, false, &long);
            assert!(r.frame.height() < 320.0, "width {width}: {r:?}");
        }
    }

    #[test]
    fn send_turns_the_accent_color_once_something_is_typed() {
        let ctx = egui::Context::default();
        let mut app = crowded_app();
        for (input, lit) in [("", false), ("   \n", false), ("hello", true), ("", false)] {
            let r = composer_in(&ctx, &mut app, 1000.0, false, false, input);
            assert_eq!(r.send_fill == app.tokens.text_strong, lit, "{input:?}: {r:?}");
            assert_eq!(r.second, egui::Rect::NOTHING);
        }
    }

    /// In a light theme too, send is a quiet strong "↵" once something is typed (no blue
    /// circle); empty, only the quiet mic.
    #[test]
    fn send_is_quiet_in_a_light_theme() {
        let ctx = egui::Context::default();
        let mut app = crowded_app();
        app.set_prefs(theme::Prefs { theme: theme::ThemeKind::Light, ..theme::Prefs::default() });
        frame(&ctx, &mut app, 1000.0);
        assert!(!app.tokens.dark, "the light theme is on");
        let r = composer_in(&ctx, &mut app, 1000.0, false, false, "hello");
        assert_eq!(r.send_fill, app.tokens.text_strong);
        assert!((r.send.width() - r.send.height()).abs() < 0.5, "round: {:?}", r.send);
        let r = composer_in(&ctx, &mut app, 1000.0, false, false, "");
        assert_eq!(r.send_fill, app.tokens.button_bg);
    }

    #[test]
    fn a_running_reply_shows_stop_and_queue_once_something_is_typed() {
        let ctx = egui::Context::default();
        let fake = Fake::default();
        let mut app = crowded_with(fake.clone());
        let t = app.tokens;
        let idle = composer_in(&ctx, &mut app, 1000.0, false, false, "");
        let r = composer_in(&ctx, &mut app, 1000.0, true, false, "");
        assert_eq!(r.send, idle.send, "Stop takes Send's place");
        assert_eq!(r.send_fill, t.text_strong, "an empty field: Stop is the strong one");
        assert_eq!(r.second, egui::Rect::NOTHING, "nothing typed, nothing to queue");
        let r = composer_in(&ctx, &mut app, 1000.0, true, false, "and then?");
        assert_eq!(r.send, idle.send);
        assert!(r.second.is_positive() && r.second.right() <= r.send.left(), "Queue beside Stop: {r:?}");
        assert_ne!(r.send_fill, t.text_strong, "Queue is the strong one now");
        // Stop cancels the reply.
        fake.sent.lock().unwrap().clear();
        click(&ctx, &mut app, 1000.0, r.send.center());
        let sent = fake.sent.lock().unwrap().clone();
        assert!(sent.contains(&ClientMsg::Cancel), "{sent:?}");
    }

    #[test]
    fn editing_a_queued_message_shows_save_and_cancel_in_the_field() {
        let ctx = egui::Context::default();
        let mut app = crowded_app();
        let idle = composer_in(&ctx, &mut app, 1000.0, false, false, "");
        let r = composer_in(&ctx, &mut app, 1000.0, true, true, "edited");
        assert_eq!(r.send.right(), idle.send.right(), "Save on the right edge: {r:?}");
        assert_eq!(r.send_fill, app.tokens.accent);
        assert!(r.second.is_positive() && r.second.right() <= r.send.left(), "Cancel beside Save: {r:?}");
        click(&ctx, &mut app, 1000.0, r.second.center());
        assert_eq!(app.queue.editing, None, "Cancel stops editing");
    }

    #[test]
    fn ctrl_comma_opens_and_closes_the_settings() {
        let ctx = egui::Context::default();
        let mut app = crowded_app();
        frame(&ctx, &mut app, 1000.0);
        assert!(!app.show_settings);
        let press = |ctx: &egui::Context, app: &mut ByobApp| {
            let key = egui::Event::Key { key: egui::Key::Comma, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::COMMAND };
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 700.0))),
                events: vec![key],
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| app.ui(ui));
            out.textures_delta.clear();
        };
        press(&ctx, &mut app);
        assert!(app.show_settings);
        frame(&ctx, &mut app, 1000.0);
        assert!(app.show_settings, "a frame without the key leaves it open");
        press(&ctx, &mut app);
        assert!(!app.show_settings);
    }

    #[test]
    fn the_settings_navigation_stays_put_when_a_page_scrolls() {
        let ctx = egui::Context::default();
        let mut app = crowded_app();
        app.open_settings_at(settings::Tab::Text);
        frame(&ctx, &mut app, 1000.0);
        frame(&ctx, &mut app, 1000.0);
        let before = (app.header_rects.settings_back, app.header_rects.settings_search);
        assert!(before.0.is_positive() && before.1.is_positive(), "{before:?}");
        let body = app.header_rects.settings_body;
        // Wheel well down inside the page.
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 700.0));
        let over = egui::pos2(body.center().x, 400.0);
        for _ in 0..10 {
            let input = egui::RawInput {
                screen_rect: Some(screen),
                events: vec![
                    egui::Event::PointerMoved(over),
                    egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Point,
                        delta: egui::vec2(0.0, -400.0),
                        modifiers: egui::Modifiers::NONE,
                        phase: egui::TouchPhase::Move,
                        source: egui::MouseWheelSource::Wheel,
                    },
                ],
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| app.ui(ui));
            out.textures_delta.clear();
        }
        frame(&ctx, &mut app, 1000.0);
        assert!(app.header_rects.settings_body.top() < body.top() - 100.0, "the page scrolled: {body:?} → {:?}", app.header_rects.settings_body);
        assert_eq!((app.header_rects.settings_back, app.header_rects.settings_search), before, "Back and search moved when the page scrolled");
    }

    /// A click at `pos` (press and release over two frames) in a window `width` wide.
    fn click(ctx: &egui::Context, app: &mut ByobApp, width: f32, pos: egui::Pos2) {
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 700.0));
        let button = |pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        for events in [vec![egui::Event::PointerMoved(pos)], vec![button(true)], vec![button(false)]] {
            let mut out = ctx.run_ui(egui::RawInput { screen_rect: Some(screen), events, ..Default::default() }, |ui| app.ui(ui));
            out.textures_delta.clear();
        }
        frame(ctx, app, width);
    }

    /// The mode chip under the box sets the chat's approvals: its menu's "Edit freely" sends
    /// the chat's own settings with that mode.
    #[test]
    fn the_mode_chip_sets_the_chats_approvals() {
        use byob_protocol::workspace::{Approvals, Layer};
        let fake = Fake::default();
        let mut app = crowded_with(fake.clone());
        let view = crate::workspace::tests::view(byob_protocol::ConfigScope::Chat, Layer::default(), None);
        let ctx = egui::Context::default();
        // The bar asks for the chat's settings; the engine answers.
        frame(&ctx, &mut app, 1000.0);
        assert!(fake.sent.lock().unwrap().contains(&ClientMsg::GetConfig { scope: byob_protocol::ConfigScope::Chat }));
        fake.inbox.lock().unwrap().push(ServerMsg::Config(Box::new(view)));
        for _ in 0..3 {
            frame(&ctx, &mut app, 1000.0);
        }
        let chip = app.composer_rects.mode;
        assert!(chip.is_positive(), "the mode chip is drawn");
        click(&ctx, &mut app, 1000.0, chip.center());
        // While its menu is open, 3 picks the third mode, "Edit freely".
        let three = egui::Event::Key { key: egui::Key::Num3, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE };
        let mut out = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 700.0))),
                events: vec![three],
                ..Default::default()
            },
            |ui| app.ui(ui),
        );
        out.textures_delta.clear();
        let sent = fake.sent.lock().unwrap();
        let set = sent.iter().find_map(|m| match m {
            ClientMsg::SetConfig { scope: byob_protocol::ConfigScope::Chat, layer, .. } => Some(layer.approvals),
            _ => None,
        });
        assert_eq!(set, Some(Some(Approvals::AllowEdits)), "{sent:?}");
    }

    /// The mic starts listening, a second click ends it, and the row then says speech to text
    /// isn't connected (nothing is put in the message).
    #[test]
    fn the_mic_listens_and_ends_without_inventing_text() {
        let fake = Fake::default();
        let mut app = crowded_with(fake);
        let ctx = egui::Context::default();
        frame(&ctx, &mut app, 1000.0);
        app.toggle_voice(&ctx);
        assert!(app.voice.is_some() && app.voice_note.is_none());
        frame(&ctx, &mut app, 1000.0);
        app.toggle_voice(&ctx);
        assert!(app.voice.is_none() && app.voice_note.is_some());
        assert!(app.input.is_empty());
    }

    /// The sidebar's Library row asks the engine for every chat's artifacts and shows the
    /// page in place of the chat; picking one opens it in its chat, and opening any chat
    /// leaves the page.
    #[test]
    fn the_library_opens_from_the_sidebar_and_leaves_for_a_chat() {
        let fake = Fake::default();
        fake.inbox.lock().unwrap().push(ServerMsg::Welcome(byob_protocol::version::Welcome::current()));
        let mut app = crowded_with(fake.clone());
        let ctx = egui::Context::default();
        frame(&ctx, &mut app, 1000.0);
        app.sidebar.request(sidebar::Request::Library);
        frame(&ctx, &mut app, 1000.0);
        assert_eq!(app.page, Page::Library);
        assert!(fake.sent.lock().unwrap().contains(&ClientMsg::ListLibrary));
        // Picking an artifact of another chat opens that chat and waits for it.
        app.open_artifact_in("other".into(), "calc".into(), false);
        assert_eq!(app.page, Page::Chat);
        assert!(fake.sent.lock().unwrap().contains(&ClientMsg::OpenChat { id: "other".into() }));
        assert_eq!(app.pending_artifact, Some(("other".into(), "calc".into())));
        // The chat opens first; the artifact waits for that chat's list, then shows.
        let other = ChatSummary { id: "other".into(), title: "Other".into(), updated_at: 2, pinned_at: None, busy: false, workspace: None, archived_at: None };
        fake.inbox.lock().unwrap().push(ServerMsg::ChatOpened { chat: Some(other), workspace: None });
        frame(&ctx, &mut app, 1000.0);
        assert!(app.pending_artifact.is_some(), "its list hasn't come yet");
        let calc = byob_protocol::artifact::Artifact {
            id: "calc".into(),
            kind: byob_protocol::artifact::ArtifactKind::Markdown,
            content_type: byob_protocol::artifact::TYPE_MARKDOWN.into(),
            title: "Calc".into(),
            filename: "calc.md".into(),
            versions: vec![byob_protocol::artifact::ArtifactVersion {
                n: 1,
                path: "a/v1.md".into(),
                created_at: 1,
                note: String::new(),
                size: 1,
                turn: String::new(),
            }],
            network: false,
        };
        fake.inbox.lock().unwrap().push(ServerMsg::Artifacts { chat: Some("other".into()), artifacts: vec![calc] });
        frame(&ctx, &mut app, 1000.0);
        frame(&ctx, &mut app, 1000.0);
        assert!(app.pending_artifact.is_none(), "taken once its list came");
        assert!(app.artifacts.is_open(), "shown beside its chat");
        // Any chat opening leaves the page.
        app.open_page(library::Mode::Apps);
        assert_eq!(app.page, Page::Apps);
        fake.inbox.lock().unwrap().push(ServerMsg::ChatOpened { chat: None, workspace: None });
        frame(&ctx, &mut app, 1000.0);
        assert_eq!(app.page, Page::Chat);
        // Plugins is a page too.
        app.sidebar.request(sidebar::Request::Plugins);
        frame(&ctx, &mut app, 1000.0);
        assert_eq!(app.page, Page::Plugins);
        assert!(!app.show_settings);
        // The page uses the whole width beside the list (no room kept for a pane).
        fake.inbox.lock().unwrap().push(crate::plugins::tests::ext(vec![]));
        frame(&ctx, &mut app, 1400.0);
        let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 700.0))), ..Default::default() };
        let out = ctx.run_ui(input, |ui| app.ui(ui));
        let texts = crate::plugins::tests::texts_of(&out);
        // The column is centered in what the list leaves, however wide the window.
        let add = texts.iter().find(|t| t.0 == "Add").map(|t| t.1);
        let title = texts.iter().filter(|t| t.0 == "Plugins").map(|t| t.1).max_by(|a, b| a.height().total_cmp(&b.height()));
        let (Some(add), Some(title)) = (add, title) else { panic!("no page header: {texts:?}") };
        let left = title.left() - 250.0;
        let right = 1400.0 - add.right();
        assert!((left - right).abs() < 60.0, "off center: {left} on the left, {right} on the right");
    }

    #[test]
    fn ctrl_plus_minus_and_zero_zoom_the_whole_interface_and_are_remembered() {
        let ctx = egui::Context::default();
        let mut app = crowded_app();
        let key = |key| egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::COMMAND };
        let press = |ctx: &egui::Context, app: &mut ByobApp, k| {
            let mut out = ctx.run_ui(egui::RawInput { events: vec![key(k)], ..Default::default() }, |ui| app.ui(ui));
            out.textures_delta.clear();
            frame(ctx, app, 1000.0);
            frame(ctx, app, 1000.0);
        };
        frame(&ctx, &mut app, 1000.0);
        press(&ctx, &mut app, egui::Key::Equals);
        press(&ctx, &mut app, egui::Key::Plus);
        assert!((app.prefs().zoom - 1.2).abs() < 1e-4, "{}", app.prefs().zoom);
        assert!((ctx.zoom_factor() - 1.2).abs() < 1e-4, "egui zoom {}", ctx.zoom_factor());
        press(&ctx, &mut app, egui::Key::Minus);
        assert!((app.prefs().zoom - 1.1).abs() < 1e-4);
        press(&ctx, &mut app, egui::Key::Num0);
        assert_eq!(app.prefs().zoom, 1.0);
        assert!((ctx.zoom_factor() - 1.0).abs() < 1e-4);
        // A stored zoom is applied when the preferences are restored (the next launch).
        app.set_prefs(theme::Prefs { zoom: 1.5, ..theme::Prefs::default() });
        frame(&ctx, &mut app, 1000.0);
        frame(&ctx, &mut app, 1000.0);
        assert!((ctx.zoom_factor() - 1.5).abs() < 1e-4);
    }

    #[test]
    fn the_top_left_goes_back_to_the_chat_from_the_settings() {
        let ctx = egui::Context::default();
        let mut app = crowded_app();
        // Over the settings the header says where you are; the chat's title isn't there to
        // open the chat's settings on top (a tester expected "home" at the top left).
        app.show_settings = true;
        frame(&ctx, &mut app, 1600.0);
        frame(&ctx, &mut app, 1600.0);
        assert_eq!((app.header_rects.chats, app.header_rects.chat_settings), (egui::Rect::NOTHING, egui::Rect::NOTHING));
        let at = app.header_rects.title.center();
        click(&ctx, &mut app, 1600.0, at);
        assert!(app.show_settings && app.workspaces.chat.is_none());
        app.close_settings();
        frame(&ctx, &mut app, 1600.0);
        // Without the settings, the button at the top right opens the chat's settings (the
        // title opens the chat's menu).
        let at = app.header_rects.chat_settings.center();
        click(&ctx, &mut app, 1600.0, at);
        assert!(app.workspaces.chat.is_some());
        // The back chevron closes both.
        app.show_settings = true;
        frame(&ctx, &mut app, 1600.0);
        assert!(app.header_rects.home.is_positive());
        let at = app.header_rects.home.center();
        click(&ctx, &mut app, 1600.0, at);
        assert!(!app.show_settings && app.workspaces.chat.is_none());
        // So does "Back to chat", at the top of the settings' navigation.
        app.show_settings = true;
        frame(&ctx, &mut app, 1600.0);
        frame(&ctx, &mut app, 1600.0);
        let back = app.header_rects.settings_back;
        let page = app.header_rects.settings_page;
        assert!(back.is_positive() && back.left() < 24.0 && back.right() <= page.left(), "{back:?} left of {page:?}");
        click(&ctx, &mut app, 1600.0, back.center());
        assert!(!app.show_settings);
    }

    /// The chat's settings hang from their button at the top right, right edges lined up,
    /// and open with the advanced ones folded away; Esc closes them.
    #[test]
    fn the_chat_settings_hang_under_their_button_with_advanced_folded() {
        let ctx = egui::Context::default();
        let mut app = crowded_app();
        frame(&ctx, &mut app, 1000.0);
        let gear = app.header_rects.chat_settings;
        assert!(gear.is_positive() && gear.right() > 900.0, "at the top right: {gear:?}");
        click(&ctx, &mut app, 1000.0, gear.center());
        assert!(app.workspaces.chat.is_some());
        frame(&ctx, &mut app, 1000.0);
        let sheet = ctx.memory(|m| m.area_rect(app.workspaces.chat_window_id())).unwrap();
        assert!((sheet.right() - gear.right()).abs() < 2.0 && sheet.top() >= gear.bottom(), "{sheet:?} under {gear:?}");
        assert!(!ctx.data(|d| d.get_temp::<bool>(egui::Id::unique("chat-settings-advanced"))).unwrap_or(false), "advanced starts folded");
        let esc = egui::Event::Key { key: egui::Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE };
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 700.0));
        let mut out = ctx.run_ui(egui::RawInput { screen_rect: Some(screen), events: vec![esc], ..Default::default() }, |ui| app.ui(ui));
        out.textures_delta.clear();
        assert!(app.workspaces.chat.is_none(), "Esc closes it");
    }

    #[test]
    fn settings_at_the_bottom_of_the_chat_list_opens_the_app_settings() {
        let ctx = egui::Context::default();
        let mut app = crowded_app();
        frame(&ctx, &mut app, 1000.0);
        app.sidebar.request(sidebar::Request::AppSettings);
        frame(&ctx, &mut app, 1000.0);
        assert!(app.show_settings);
    }

    #[test]
    fn workspace_views_open_and_send_what_the_engine_needs() {
        let fake = Fake::default();
        let ws = byob_protocol::WorkspaceSummary {
            id: "w".into(),
            name: "Data".into(),
            folder: "/data".into(),
            chats: 0,
            updated_at: 1,
            env: byob_protocol::workspace::EnvKind::Auto,
            ..Default::default()
        };
        fake.inbox.lock().unwrap().extend([
            ServerMsg::State(state(Link::Ok, true)),
            ServerMsg::Workspaces { items: vec![ws], current: Some("w".into()), root: "/home/me/byob-workspaces".into(), support: Default::default() },
            ServerMsg::ChatOpened { chat: None, workspace: Some("w".into()) },
        ]);
        let ctx = egui::Context::default();
        let mut app = ByobApp::new(Box::new(fake.clone()), "test");
        frame(&ctx, &mut app, 1000.0);
        assert!(app.in_workspace("Data"));
        for panel in ByobApp::PANELS.split(", ") {
            assert!(app.open_panel(panel), "{panel}");
        }
        assert!(!app.open_panel("nope"));
        // The project's environment is asked for when its part is shown.
        assert!(app.open_panel("workspace-settings:environment"));
        for _ in 0..2 {
            frame(&ctx, &mut app, 1000.0);
        }
        let sent = fake.sent.lock().unwrap().clone();
        assert!(sent.contains(&ClientMsg::GetConfig { scope: byob_protocol::ConfigScope::Chat }));
        assert!(sent.contains(&ClientMsg::GetConfig { scope: byob_protocol::ConfigScope::Workspace { id: "w".into() } }));
        assert!(sent.contains(&ClientMsg::WorkspaceEnv { action: None, workspace: Some("w".into()) }));
        assert!(sent.contains(&ClientMsg::CheckContainers));
        app.create_workspace("Fresh", byob_protocol::workspace::EnvKind::Container);
        let last = fake.sent.lock().unwrap().last().cloned();
        let Some(ClientMsg::CreateWorkspace { name, folder, config, .. }) = last else { panic!("{last:?}") };
        assert_eq!((name.as_str(), folder, config.env), ("Fresh", byob_protocol::NewFolder::Create, Some(byob_protocol::workspace::EnvKind::Container)));
    }

    fn press_enter(ctx: &egui::Context, app: &mut ByobApp) {
        press(ctx, app, egui::Key::Enter);
    }

    #[test]
    fn shift_enter_is_a_newline_and_enter_sends() {
        let fake = Fake::default();
        fake.inbox.lock().unwrap().push(ServerMsg::State(state(Link::Ok, true)));
        let ctx = egui::Context::default();
        let mut app = ByobApp::new(Box::new(fake.clone()), "test");
        frame(&ctx, &mut app, 1000.0);
        let sent = |fake: &Fake| fake.sent.lock().unwrap().iter().filter(|m| matches!(m, ClientMsg::SendMessage { .. })).count();
        app.input = "first line".into();
        // Put the cursor at the end, as typing would.
        let id = egui::Id::unique("composer");
        let mut state = egui::TextEdit::load_state(&ctx, id).unwrap_or_default();
        state.cursor.set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(10))));
        egui::TextEdit::store_state(&ctx, id, state);
        press_with(&ctx, &mut app, egui::Key::Enter, egui::Modifiers::SHIFT);
        assert_eq!((app.input.as_str(), sent(&fake)), ("first line\n", 0), "Shift+Enter adds a line and doesn't send");
        press_enter(&ctx, &mut app);
        assert_eq!(sent(&fake), 1, "Enter sends");
        assert!(app.input.is_empty());
    }

    /// Focus the composer and press `key` in it.
    fn press(ctx: &egui::Context, app: &mut ByobApp, key: egui::Key) {
        press_with(ctx, app, key, egui::Modifiers::NONE);
    }

    /// Focus the composer and press `key` with `modifiers` in it.
    fn press_with(ctx: &egui::Context, app: &mut ByobApp, key: egui::Key, modifiers: egui::Modifiers) {
        ctx.memory_mut(|m| m.request_focus(egui::Id::unique("composer")));
        frame(ctx, app, 1000.0);
        let key = egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers };
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 700.0))),
            events: vec![key],
            ..Default::default()
        };
        let mut out = ctx.run_ui(input, |ui| app.ui(ui));
        out.textures_delta.clear();
    }

    fn queued(texts: &[(u64, &str)], paused: Option<byob_protocol::QueuePause>) -> ServerMsg {
        let items = texts.iter().map(|(id, t)| byob_protocol::QueuedMessage { id: *id, text: (*t).into(), attachments: vec![] }).collect();
        ServerMsg::Queue(byob_protocol::ChatQueue { chat: Some("c".into()), items, paused })
    }

    fn chat_in(id: &str, ws: &str) -> ChatSummary {
        ChatSummary { id: id.into(), title: id.to_uppercase(), updated_at: 1, pinned_at: None, busy: false, workspace: Some(ws.into()), archived_at: None }
    }

    /// Workspaces w1 and w2, chat "a" (in w1) in view.
    fn two_workspaces() -> (Fake, ByobApp) {
        let fake = Fake::default();
        let ws = |id: &str| byob_protocol::WorkspaceSummary {
            id: id.into(),
            name: id.to_uppercase(),
            folder: format!("/{id}"),
            chats: 1,
            updated_at: 1,
            env: byob_protocol::workspace::EnvKind::Auto,
            ..Default::default()
        };
        fake.inbox.lock().unwrap().extend([
            ServerMsg::State(state(Link::Ok, true)),
            ServerMsg::Workspaces { items: vec![ws("w1"), ws("w2")], current: Some("w1".into()), root: String::new(), support: Default::default() },
            ServerMsg::ChatOpened { chat: Some(chat_in("a", "w1")), workspace: Some("w1".into()) },
        ]);
        let app = ByobApp::new(Box::new(fake.clone()), "test");
        (fake, app)
    }

    fn shortcut(ctx: &egui::Context, app: &mut ByobApp, key: egui::Key, modifiers: egui::Modifiers) {
        let event = egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers };
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 700.0))),
            events: vec![event],
            ..Default::default()
        };
        let mut out = ctx.run_ui(input, |ui| app.ui(ui));
        out.textures_delta.clear();
    }

    #[test]
    fn ctrl_n_starts_a_chat_in_the_workspace_in_view_and_ctrl_f_searches() {
        let ctx = egui::Context::default();
        let (fake, mut app) = two_workspaces();
        frame(&ctx, &mut app, 1000.0);
        fake.inbox.lock().unwrap().push(ServerMsg::ChatOpened { chat: Some(chat_in("b", "w2")), workspace: Some("w2".into()) });
        frame(&ctx, &mut app, 1000.0);
        app.show_settings = true;
        shortcut(&ctx, &mut app, egui::Key::N, egui::Modifiers::COMMAND);
        assert_eq!(fake.sent.lock().unwrap().last(), Some(&ClientMsg::NewChat { workspace: Some("w2".into()) }));
        assert!(!app.show_settings, "the new chat shows");
        // The browser's binding works everywhere.
        shortcut(&ctx, &mut app, egui::Key::O, egui::Modifiers::COMMAND | egui::Modifiers::SHIFT);
        let sent = fake.sent.lock().unwrap().clone();
        assert_eq!(sent.iter().filter(|m| matches!(m, ClientMsg::NewChat { .. })).count(), 2);
        // The engine answers with the new chat: the composer gets the keyboard.
        fake.inbox.lock().unwrap().push(ServerMsg::ChatOpened { chat: None, workspace: Some("w2".into()) });
        frame(&ctx, &mut app, 1000.0);
        frame(&ctx, &mut app, 1000.0);
        assert!(ctx.memory(|m| m.has_focus(egui::Id::unique("composer"))));
        // Ctrl+F (or Ctrl+K): search, as a palette with the keyboard in its field.
        shortcut(&ctx, &mut app, egui::Key::F, egui::Modifiers::COMMAND);
        frame(&ctx, &mut app, 1000.0);
        frame(&ctx, &mut app, 1000.0);
        assert!(app.sidebar.palette.is_some());
        assert!(ctx.memory(|m| m.has_focus(egui::Id::unique("search-palette-field"))));
        app.sidebar.palette = None;
        shortcut(&ctx, &mut app, egui::Key::K, egui::Modifiers::COMMAND);
        assert!(app.sidebar.palette.is_some());
    }

    #[test]
    fn drafts_stay_with_their_chat() {
        let ctx = egui::Context::default();
        let (fake, mut app) = two_workspaces();
        frame(&ctx, &mut app, 1000.0);
        let open = |app: &mut ByobApp, chat: Option<ChatSummary>, ws: &str| {
            fake.inbox.lock().unwrap().push(ServerMsg::ChatOpened { chat, workspace: Some(ws.into()) });
            frame(&ctx, app, 1000.0);
        };
        app.input = "half a thought for a".into();
        open(&mut app, Some(chat_in("b", "w2")), "w2");
        assert_eq!(app.input, "", "b has no draft");
        app.input = "for b".into();
        open(&mut app, None, "w1");
        assert_eq!(app.input, "", "nor the new chat");
        app.input = "a new idea".into();
        open(&mut app, Some(chat_in("a", "w1")), "w1");
        assert_eq!(app.input, "half a thought for a");
        open(&mut app, Some(chat_in("b", "w2")), "w2");
        assert_eq!(app.input, "for b");
        open(&mut app, None, "w1");
        assert_eq!(app.input, "a new idea", "the new chat's draft comes back too");
        // Sending it makes the chat: the composer carries on there, nothing stashed.
        let text = std::mem::take(&mut app.input);
        app.submit(text);
        app.input = "typed while it starts".into();
        open(&mut app, Some(chat_in("new", "w1")), "w1");
        assert_eq!(app.input, "typed while it starts");
        // Stored with the device's prefs, and back after a restart.
        let prefs = app.sidebar_prefs();
        assert_eq!(prefs.drafts.get("chat:a").map(String::as_str), Some("half a thought for a"));
        assert_eq!(prefs.drafts.get("chat:new").map(String::as_str), Some("typed while it starts"));
        let (_, mut again) = two_workspaces();
        again.set_sidebar_prefs(&serde_json::from_str(&serde_json::to_string(&prefs).unwrap()).unwrap());
        frame(&ctx, &mut again, 1000.0);
        assert_eq!(again.input, "half a thought for a");
    }

    /// New project is centred over the window; the Projects page can start it either way;
    /// a stray click outside closes it only while nothing is chosen.
    #[test]
    fn new_project_is_centred_and_starts_either_way() {
        let ctx = egui::Context::default();
        let (_, mut app) = two_workspaces();
        frame(&ctx, &mut app, 1000.0);
        app.sidebar.request(sidebar::Request::NewWorkspace);
        let mut texts = Vec::new();
        for _ in 0..3 {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 700.0))), ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| app.ui(ui));
            out.textures_delta.clear();
            texts = crate::plugins::tests::texts_of(&out);
        }
        let title = texts.iter().find(|t| t.0 == "Create project" && t.1.top() > 60.0).map(|t| t.1).unwrap_or_else(|| panic!("{texts:?}"));
        let name = texts.iter().find(|t| t.0 == "Project name").map(|t| t.1).unwrap_or_else(|| panic!("{texts:?}"));
        let cancel = texts.iter().find(|t| t.0 == "Cancel").map(|t| t.1).unwrap();
        // The dialog's middle (its first heading's left edge to its buttons' right) is the
        // window's middle.
        let middle = (name.left() + cancel.right() + 100.0) / 2.0;
        assert!((middle - 500.0).abs() < 60.0, "centred across: {middle}");
        let down = (title.top() + cancel.bottom()) / 2.0;
        assert!((down - 350.0).abs() < 60.0, "centred down, not pinned to the top: {title:?} … {cancel:?}");
        // A click on the dimmed window outside it closes it while nothing is chosen.
        let outside = egui::pos2(30.0, 650.0);
        for pressed in [true, false] {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 700.0))),
                events: vec![
                    egui::Event::PointerMoved(outside),
                    egui::Event::PointerButton { pos: outside, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE },
                ],
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| app.ui(ui));
            out.textures_delta.clear();
        }
        assert!(app.workspaces.form.is_none(), "nothing typed: a click outside closes it");
        // Typed something: a stray click keeps it.
        app.workspaces.open_start(projects::Start::Empty);
        assert_eq!(app.workspaces.form.as_ref().map(workspace::NewWorkspaceForm::source), Some(workspace::Source::Empty));
        if let Some(f) = &mut app.workspaces.form {
            f.goal = "Plan the trip".into();
        }
        assert!(!app.workspaces.form.as_ref().unwrap().untouched());
    }

    /// A very long projects folder can't push New project wider than the window (it did:
    /// the new folder's path was one line that never wrapped).
    #[test]
    fn a_long_projects_folder_keeps_new_project_inside_the_window() {
        let ctx = egui::Context::default();
        let (_, mut app) = two_workspaces();
        frame(&ctx, &mut app, 1000.0);
        app.workspaces.root = format!("/private/tmp/{}/byob-home/projects", "very-long-folder-name-".repeat(8));
        app.workspaces.open_start(projects::Start::Empty);
        let mut texts = Vec::new();
        for _ in 0..3 {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 700.0))), ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| app.ui(ui));
            out.textures_delta.clear();
            texts = crate::plugins::tests::texts_of(&out);
        }
        let dialog: Vec<_> = texts.iter().filter(|t| t.0 == "Add folder…" || t.0 == "Folders" || t.0 == "Cancel" || t.0 == "Create project").collect();
        assert!(dialog.len() >= 3, "{texts:?}");
        for t in dialog {
            assert!(t.1.left() >= 0.0 && t.1.right() <= 1000.0, "{:?} is off the window at {:?}", t.0, t.1);
        }
    }

    /// The Projects page's view and order are remembered on the device.
    #[test]
    fn the_projects_view_is_remembered() {
        let (_, mut app) = two_workspaces();
        app.projects.view = projects::View::Grid;
        app.projects.sort = projects::Sort::Name;
        let prefs = app.sidebar_prefs();
        let (_, mut again) = two_workspaces();
        again.set_sidebar_prefs(&serde_json::from_str(&serde_json::to_string(&prefs).unwrap()).unwrap());
        assert_eq!((again.projects.view, again.projects.sort), (projects::View::Grid, projects::Sort::Name));
        // Prefs saved before the page had a view still load.
        let old: sidebar::SidebarPrefs = serde_json::from_str("{\"space\":null}").unwrap();
        assert_eq!(old.projects_view, projects::View::List);
    }

    #[test]
    fn a_new_workspace_opens_from_the_list_and_a_dropped_folder_fills_the_form() {
        let ctx = egui::Context::default();
        let (_, mut app) = two_workspaces();
        frame(&ctx, &mut app, 1000.0);
        app.sidebar.request(sidebar::Request::NewWorkspace);
        frame(&ctx, &mut app, 1000.0);
        let form = app.workspaces.form.as_ref().unwrap();
        assert_eq!((form.source(), form.env), (workspace::Source::Empty, byob_protocol::workspace::EnvKind::Auto));
        // A folder dropped on the window.
        let dir = std::env::temp_dir().join("byob-drop-test-Alpha");
        std::fs::create_dir_all(&dir).unwrap();
        #[derive(Debug)]
        struct Dropped(std::path::PathBuf);
        impl egui::DroppedFile for Dropped {
            fn path(&self) -> &std::path::Path {
                &self.0
            }
            fn bytes(&self) -> Result<Vec<u8>, String> {
                Err("a folder".into())
            }
        }
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 700.0))),
            dropped_files: vec![std::sync::Arc::new(Dropped(dir.clone()))],
            ..Default::default()
        };
        let mut out = ctx.run_ui(input, |ui| app.ui(ui));
        out.textures_delta.clear();
        let form = app.workspaces.form.as_ref().unwrap();
        assert_eq!(
            (form.source(), form.name.as_str(), form.folders.clone()),
            (workspace::Source::Folder, "byob-drop-test-Alpha", vec![dir.display().to_string()])
        );
        assert!(app.notices.is_empty(), "not attached as a file: {:?}", app.notices);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn a_starter_sends_its_prompt_and_keeps_the_draft() {
        let fake = Fake::default();
        let mut app = ByobApp::new(Box::new(fake.clone()), "test");
        app.input = "my draft".into();
        app.start_with("Make a bar chart".into());
        assert_eq!(fake.sent.lock().unwrap().last(), Some(&ClientMsg::SendMessage { text: "Make a bar chart".into(), attachments: vec![] }));
        assert_eq!(app.input, "my draft");
    }

    #[test]
    fn enter_while_busy_queues_and_queued_rows_edit_remove_and_send_now() {
        let fake = Fake::default();
        let busy = EngineState { busy: true, ..state(Link::Ok, true) };
        fake.inbox.lock().unwrap().extend([ServerMsg::State(busy), queued(&[(1, "first follow-up"), (2, "second\nwith more")], None)]);
        let ctx = egui::Context::default();
        let mut app = ByobApp::new(Box::new(fake.clone()), "test");
        frame(&ctx, &mut app, 1000.0);
        assert_eq!(app.queued(), 2);
        assert!(!app.settled(), "messages are about to go");
        // Enter while a reply runs sends the message; the engine queues it.
        app.input = "third".into();
        press_enter(&ctx, &mut app);
        assert!(app.input.is_empty());
        assert_eq!(fake.sent.lock().unwrap().last(), Some(&ClientMsg::SendMessage { text: "third".into(), attachments: vec![] }));
        // Clicking a row edits it in the composer; the draft is set aside meanwhile.
        app.input = "my draft".into();
        app.queue_actions(vec![queue::Action::Edit(2)]);
        assert_eq!((app.input.as_str(), app.queue.editing), ("second\nwith more", Some(2)));
        frame(&ctx, &mut app, 1000.0);
        app.input = "second, edited".into();
        press_enter(&ctx, &mut app);
        assert_eq!(fake.sent.lock().unwrap().last(), Some(&ClientMsg::EditQueued { id: 2, text: "second, edited".into(), attachments: vec![] }));
        assert_eq!((app.input.as_str(), app.queue.editing), ("my draft", None));
        app.queue_actions(vec![queue::Action::Remove(1), queue::Action::SendNow(2), queue::Action::Move(2, 0), queue::Action::Resume]);
        let sent = fake.sent.lock().unwrap().clone();
        assert_eq!(
            sent[sent.len() - 4..],
            [ClientMsg::Unqueue { id: 1 }, ClientMsg::SendQueuedNow { id: 2 }, ClientMsg::MoveQueued { id: 2, to: 0 }, ClientMsg::ResumeQueue]
        );
        // A message sent while being edited leaves the edit as a draft, the set-aside one after it.
        app.input = String::new();
        app.queue_actions(vec![queue::Action::Edit(1)]);
        app.input.push_str(" (changed)");
        fake.inbox.lock().unwrap().push(queued(&[(2, "second, edited")], Some(byob_protocol::QueuePause::Stopped)));
        frame(&ctx, &mut app, 1000.0);
        assert_eq!((app.input.as_str(), app.queue.editing), ("first follow-up (changed)", None));
        assert!(app.notices.iter().any(|(_, t)| t.contains("draft now")));
        // Paused, nothing moves on its own: idle counts as settled.
        fake.inbox.lock().unwrap().push(ServerMsg::State(state(Link::Ok, true)));
        frame(&ctx, &mut app, 1000.0);
        assert!(app.settled());
        // Narrow and light: the rows still draw.
        app.set_prefs(theme::Prefs { theme: theme::ThemeKind::Light, ..Default::default() });
        frame(&ctx, &mut app, 320.0);
    }

    #[test]
    fn the_slash_menu_inserts_with_the_keyboard_and_enter_still_sends() {
        use byob_protocol::{PluginScope, SlashItem, SlashKind};
        let fake = Fake::default();
        let item =
            |name: &str, kind, scope| SlashItem { name: name.into(), kind, description: format!("{name} things"), hint: String::new(), scope, plugin: None };
        let mut ext = plugins::tests::ext(vec![]);
        if let ServerMsg::Extensions(e) = &mut ext {
            e.slash = vec![
                item("compact", SlashKind::Builtin, None),
                item("review", SlashKind::Command, Some(PluginScope::Workspace)),
                item("trip-plan", SlashKind::Skill, Some(PluginScope::Global)),
            ];
        }
        fake.inbox.lock().unwrap().extend([ServerMsg::State(state(Link::Ok, true)), ext]);
        let ctx = egui::Context::default();
        let mut app = ByobApp::new(Box::new(fake.clone()), "test");
        frame(&ctx, &mut app, 1000.0);
        let sent = |fake: &Fake| fake.sent.lock().unwrap().iter().filter(|m| matches!(m, ClientMsg::SendMessage { .. })).count();
        // Enter picks the only match instead of sending.
        app.input = "/re".into();
        press_enter(&ctx, &mut app);
        assert_eq!((app.input.as_str(), sent(&fake)), ("/review ", 0));
        // With arguments the menu is gone: Enter sends, and the engine expands it.
        app.input.push_str("src/a.rs");
        press_enter(&ctx, &mut app);
        assert_eq!(fake.sent.lock().unwrap().last(), Some(&ClientMsg::SendMessage { text: "/review src/a.rs".into(), attachments: vec![] }));
        assert!(app.input.is_empty());
        // ↓↓ then Tab: the third entry; ↑ from the top wraps to the last.
        app.input = "/".into();
        press(&ctx, &mut app, egui::Key::ArrowDown);
        press(&ctx, &mut app, egui::Key::ArrowDown);
        press(&ctx, &mut app, egui::Key::Tab);
        assert_eq!(app.input, "/trip-plan ");
        app.input = "/".into();
        press(&ctx, &mut app, egui::Key::ArrowUp);
        press_enter(&ctx, &mut app);
        assert_eq!((app.input.as_str(), sent(&fake)), ("/trip-plan ", 1));
        // Esc closes it; Enter then sends what is typed.
        app.input = "/co".into();
        press(&ctx, &mut app, egui::Key::Escape);
        press_enter(&ctx, &mut app);
        assert_eq!(fake.sent.lock().unwrap().last(), Some(&ClientMsg::SendMessage { text: "/co".into(), attachments: vec![] }));
        // Nothing matches: an ordinary message.
        app.input = "/zzz".into();
        press_enter(&ctx, &mut app);
        assert_eq!(fake.sent.lock().unwrap().last(), Some(&ClientMsg::SendMessage { text: "/zzz".into(), attachments: vec![] }));
    }

    #[test]
    fn the_plugins_panel_opens_from_the_header_and_scripts() {
        let ctx = egui::Context::default();
        let mut app = crowded_app();
        frame(&ctx, &mut app, 1000.0);
        assert!(!app.show_settings);
        // The troubled one and the one with a pane, both on.
        assert_eq!(app.plugins.summary(), Some((2, true)));
        assert!(app.open_panel("plugins"));
        frame(&ctx, &mut app, 1000.0);
        assert!(app.show_settings);
    }

    #[test]
    fn first_run_shows_the_welcome_and_a_failed_reply_offers_settings() {
        let fake = Fake::default();
        fake.inbox.lock().unwrap().push(ServerMsg::State(state(Link::failed("could not reach x: refused"), false)));
        let ctx = egui::Context::default();
        let mut app = ByobApp::new(Box::new(fake.clone()), "test");
        frame(&ctx, &mut app, 1000.0);
        assert_eq!(connect::empty_view(app.state.as_ref()), connect::EmptyView::Welcome);
        // A scripted test from the form goes to the engine.
        app.test_connection("http://127.0.0.1:11434/v1", "qwen3");
        frame(&ctx, &mut app, 1000.0);
        assert!(fake.sent.lock().unwrap().iter().any(|m| matches!(m, ClientMsg::TestInference { model, .. } if model == "qwen3")));
        // A reply that couldn't reach the server: a card, retryable while it is the last one.
        let failed = Entry {
            id: 2,
            role: EntryRole::Assistant,
            content: String::new(),
            reasoning: String::new(),
            done: true,
            error: Some("could not reach http://x/v1/chat/completions: refused".into()),
            stats: None,
            steps: vec![],
            undoable: false,
            undone: false,
            attachments: vec![],
        };
        assert!(retryable(&failed));
        assert!(!retryable(&Entry { content: "partial".into(), ..failed.clone() }));
        fake.inbox.lock().unwrap().push(ServerMsg::Transcript { entries: vec![failed] });
        frame(&ctx, &mut app, 1000.0);
        app.open_connection();
        frame(&ctx, &mut app, 1000.0);
        assert!(app.show_settings && app.appearance.tab == settings::Tab::Models);
    }

    /// Plugins' header items open their views once the engine says it has live views, and
    /// again in another chat (the engine forgets a client's views when it switches chats).
    #[test]
    fn plugin_status_items_open_their_views_and_reopen_in_another_chat() {
        use byob_protocol::{BackendInfo, Extensions, PluginOrigin, PluginScope, Site, ViewDecl, ViewRef};
        let fake = Fake::default();
        let mut app = ByobApp::new(Box::new(fake.clone()), "test");
        let mut p = plugins::tests::info("todo", PluginScope::Global, PluginOrigin::User);
        p.views = vec![ViewDecl { id: "count".into(), site: Site::Status, title: "Count".into(), icon: String::new(), html: String::new() }];
        p.backend = Some(BackendInfo { state: "running".into(), ..Default::default() });
        fake.inbox.lock().unwrap().extend([
            ServerMsg::Welcome(Welcome::current()),
            ServerMsg::State(state(Link::Ok, true)),
            ServerMsg::Extensions(Box::new(Extensions { plugins: vec![p], ..Default::default() })),
        ]);
        let ctx = egui::Context::default();
        let opens = |fake: &Fake| {
            fake.sent.lock().unwrap().iter().filter(|m| matches!(m, ClientMsg::OpenView { view } if *view == ViewRef::new("todo", "count", ""))).count()
        };
        frame(&ctx, &mut app, 1200.0);
        frame(&ctx, &mut app, 1200.0);
        assert_eq!(opens(&fake), 1);
        assert!(!app.settled(), "waiting for the view");
        fake.inbox.lock().unwrap().push(ServerMsg::ChatOpened { chat: None, workspace: None });
        frame(&ctx, &mut app, 1200.0);
        frame(&ctx, &mut app, 1200.0);
        assert_eq!(opens(&fake), 2);
    }

    /// A plugin's header item can be a checkbox (any view node, not only a chip): it is drawn
    /// in the header and flipping it sends the plugin its event with the new value.
    #[test]
    fn a_header_item_can_be_a_checkbox() {
        use byob_protocol::{BackendInfo, Extensions, LiveView, PluginOrigin, PluginScope, Site, ViewDecl, ViewRef};
        let fake = Fake::default();
        let mut app = ByobApp::new(Box::new(fake.clone()), "test");
        let mut p = plugins::tests::info("focus", PluginScope::Global, PluginOrigin::User);
        p.views = vec![ViewDecl { id: "mode".into(), site: Site::Status, title: "Focus".into(), icon: String::new(), html: String::new() }];
        p.backend = Some(BackendInfo { state: "running".into(), ..Default::default() });
        fake.inbox.lock().unwrap().extend([
            ServerMsg::Welcome(Welcome::current()),
            ServerMsg::State(state(Link::Ok, true)),
            ServerMsg::Extensions(Box::new(Extensions { plugins: vec![p], ..Default::default() })),
        ]);
        let ctx = egui::Context::default();
        frame(&ctx, &mut app, 1200.0);
        let view = ViewRef::new("focus", "mode", "");
        let tree =
            serde_json::from_value(serde_json::json!({"type": "toggle", "name": "on", "label": "Focus mode", "value": false, "action": "flip"})).unwrap();
        fake.inbox.lock().unwrap().push(ServerMsg::LiveView(LiveView { view: view.clone(), tree: Some(tree), ..Default::default() }));
        // Draw it and find the checkbox's label in the header.
        let shapes = |ctx: &egui::Context, app: &mut ByobApp, events: Vec<egui::Event>| {
            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200.0, 700.0));
            let mut out = ctx.run_ui(egui::RawInput { screen_rect: Some(screen), events, ..Default::default() }, |ui| app.ui(ui));
            out.textures_delta.clear();
            let mut found = Vec::new();
            fn walk(shape: &egui::Shape, out: &mut Vec<(String, egui::Pos2)>) {
                match shape {
                    egui::Shape::Text(t) => out.push((t.galley.text().to_owned(), t.pos + t.galley.rect.center().to_vec2())),
                    egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, out)),
                    _ => {}
                }
            }
            out.shapes.iter().for_each(|c| walk(&c.shape, &mut found));
            found
        };
        shapes(&ctx, &mut app, vec![]);
        let text = shapes(&ctx, &mut app, vec![]);
        let at = text.iter().find(|(t, _)| t == "Focus mode").map(|(_, p)| *p).unwrap_or_else(|| panic!("not in the header: {text:?}"));
        assert!(at.y < 40.0, "in the header: {at:?}");
        shapes(&ctx, &mut app, vec![egui::Event::PointerMoved(at)]);
        let press = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
        shapes(&ctx, &mut app, vec![press(true)]);
        shapes(&ctx, &mut app, vec![press(false)]);
        let sent = fake.sent.lock().unwrap();
        let ev = sent.iter().find_map(|m| match m {
            ClientMsg::ViewEvent { view: v, event } if *v == view => Some(event.clone()),
            _ => None,
        });
        let ev = ev.unwrap_or_else(|| panic!("no event: {sent:?}"));
        assert_eq!((ev.action.as_str(), ev.values.get("on")), ("flip", Some(&serde_json::json!(true))));
    }

    /// YOLO on: a badge in the header that opens the Approvals setting; off: none.
    #[test]
    fn yolo_shows_a_badge_that_opens_approvals() {
        let fake = Fake::default();
        let mut app = ByobApp::new(Box::new(fake.clone()), "test");
        let text = |ctx: &egui::Context, app: &mut ByobApp, events: Vec<egui::Event>| {
            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200.0, 700.0));
            let mut out = ctx.run_ui(egui::RawInput { screen_rect: Some(screen), events, ..Default::default() }, |ui| app.ui(ui));
            out.textures_delta.clear();
            let mut found = Vec::new();
            fn walk(shape: &egui::Shape, out: &mut Vec<(String, egui::Pos2)>) {
                match shape {
                    egui::Shape::Text(t) => out.push((t.galley.text().to_owned(), t.pos + t.galley.rect.center().to_vec2())),
                    egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, out)),
                    _ => {}
                }
            }
            out.shapes.iter().for_each(|c| walk(&c.shape, &mut found));
            found
        };
        fake.inbox.lock().unwrap().extend([ServerMsg::Welcome(Welcome::current()), ServerMsg::State(state(Link::Ok, true))]);
        let ctx = egui::Context::default();
        text(&ctx, &mut app, vec![]);
        assert!(!text(&ctx, &mut app, vec![]).iter().any(|(t, _)| t == "YOLO"));
        fake.inbox.lock().unwrap().push(ServerMsg::State(EngineState { yolo: true, ..state(Link::Ok, true) }));
        text(&ctx, &mut app, vec![]);
        let found = text(&ctx, &mut app, vec![]);
        let at = found.iter().find(|(t, _)| t == "YOLO").map(|(_, p)| *p).expect("the badge");
        assert!(at.y < 40.0, "in the header");
        text(&ctx, &mut app, vec![egui::Event::PointerMoved(at)]);
        let press = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
        text(&ctx, &mut app, vec![press(true)]);
        text(&ctx, &mut app, vec![press(false)]);
        assert!(app.show_settings, "the badge opens the settings at Approvals");
    }

    /// A newer engine's messages are skipped, not fatal; plugin messages go out only when the
    /// engine takes them.
    #[test]
    fn unknown_messages_are_skipped_and_the_plugin_channel_waits_for_the_engines_cap() {
        let fake = Fake::default();
        let mut app = ByobApp::new(Box::new(fake.clone()), "test");
        assert!(matches!(&fake.sent.lock().unwrap()[0], ClientMsg::Hello { protocol: byob_protocol::PROTOCOL_MAJOR, caps, .. } if !caps.is_empty()));
        assert!(!app.send_plugin("sheets", serde_json::json!(1)), "the engine hasn't said it takes them");
        let unknown = ServerMsg::from_json(r#"{"type":"hologram","x":1}"#).unwrap();
        fake.inbox.lock().unwrap().extend([
            ServerMsg::Welcome(Welcome { protocol: byob_protocol::PROTOCOL_MAJOR, minor: 9, caps: vec!["plugin_channel".into(), "teleport".into()] }),
            unknown.clone(),
            unknown,
            ServerMsg::Plugin { plugin: "sheets".into(), msg: serde_json::json!({"cell": "A1"}) },
            ServerMsg::State(state(Link::Ok, true)),
        ]);
        let ctx = egui::Context::default();
        frame(&ctx, &mut app, 1000.0);
        assert!(app.state.is_some(), "messages after the unknown ones still arrive");
        assert_eq!(app.skipped.len(), 2, "{:?}", app.skipped);
        assert!(app.engine_has("plugin_channel") && !app.engine_has("teleport"));
        assert!(app.send_plugin("sheets", serde_json::json!({"get": "A1"})));
        assert_eq!(fake.sent.lock().unwrap().last(), Some(&ClientMsg::Plugin { plugin: "sheets".into(), msg: serde_json::json!({"get": "A1"}) }));
    }
}
