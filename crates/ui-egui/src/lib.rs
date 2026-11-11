//! The byob UI. Identical on desktop and in the browser: it renders engine state received
//! over a [`Connection`] and sends user intents back. It never touches the engine directly.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

pub mod appview;
pub mod artifact;
pub mod chart;
pub mod connect;
mod envpanel;
pub mod export;
pub mod header;
mod highlight;
mod icons;
pub mod markdown;
pub mod media;
pub mod pane;
pub mod plugins;
pub mod queue;
pub mod renderers;
mod settings;
pub mod sidebar;
pub mod sites;
pub mod slash;
pub mod steps;
pub mod theme;
pub mod tree;
pub mod upload;
pub mod usage;
pub mod views;
pub mod web;
pub mod workspace;
pub mod ws;

use byob_protocol::{ClientMsg, Connection, EngineState, Entry, EntryRole, NoticeLevel, ServerMsg, StepStatus, Welcome};

/// Tallest an expanded thinking block gets before it scrolls.
const THINKING_MAX_HEIGHT: f32 = 240.0;

/// "840 chars", "12.4k chars".
fn size_label(chars: usize) -> String {
    if chars < 1000 {
        format!("{chars} chars")
    } else {
        #[allow(clippy::cast_precision_loss)] // display only
        let k = chars as f64 / 1000.0;
        format!("{k:.1}k chars")
    }
}

/// Notices kept on screen.
const MAX_NOTICES: usize = 3;

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
    state: Option<EngineState>,
    entries: Vec<Entry>,
    notices: Vec<(NoticeLevel, String)>,
    input: String,
    show_settings: bool,
    /// Scroll the settings to the connection form on the next frame.
    reveal_connection: bool,
    /// The connection form (welcome screen and settings).
    connect: connect::ConnectForm,
    /// Where the header's buttons were last drawn.
    header_rects: header::Rects,
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
}

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
            state: None,
            entries: Vec::new(),
            notices: Vec::new(),
            input: String::new(),
            show_settings: false,
            reveal_connection: false,
            connect: connect::ConnectForm::default(),
            header_rects: header::Rects::default(),
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
    pub fn pane_prefs(&self) -> pane::PanePrefs {
        self.artifacts.pane_prefs()
    }

    /// Where the desktop app serves artifacts for "Open in browser" (may start a server).
    pub fn set_link_base(&mut self, base: artifact::LinkBase) {
        self.artifacts.set_link_base(base);
    }

    fn notice(&mut self, level: NoticeLevel, text: String) {
        self.notices.push((level, text));
        let excess = self.notices.len().saturating_sub(MAX_NOTICES);
        self.notices.drain(..excess);
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
            if matches!(msg, ServerMsg::ChatOpened { .. }) {
                self.workspaces.chat_changed();
                // The queued message being edited stays queued in the chat we leave.
                self.end_edit();
            }
            if self.queue.apply(&msg) {
                self.queue_changed();
                continue;
            }
            // Which plugins draw which artifact types follows the chat in view.
            if let ServerMsg::Extensions(e) = &msg {
                self.artifacts.renderers_mut().apply_plugins(&e.plugins);
            }
            if self.usage.apply(&msg) || self.workspaces.apply(&msg) || self.sidebar.apply(&msg) || self.artifacts.apply(&msg) || self.plugins.apply(&msg) {
                continue;
            }
            match msg {
                ServerMsg::Welcome(w) => {
                    log::info!("engine speaks protocol {}.{} with caps {:?}", w.protocol, w.minor, w.caps);
                    self.engine = Some(w);
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
                    self.notices.push((level, text));
                    let excess = self.notices.len().saturating_sub(MAX_NOTICES);
                    self.notices.drain(..excess);
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
                ServerMsg::InferenceTested(test) => self.connect.tested(test),
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
                | ServerMsg::WorkspaceCreated { .. } => {}
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

    /// Restore (or change) appearance preferences; applied on the next frame.
    pub fn set_prefs(&mut self, prefs: theme::Prefs) {
        let prefs = prefs.sanitized();
        let reload = prefs.imported != self.prefs.imported;
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
        if self.applied != Some(key) {
            self.tokens = theme::Tokens::for_prefs(&self.prefs, key.1, &self.user_themes);
            self.tokens.apply(ctx);
            self.applied = Some(key);
        }
    }

    /// Open or close the settings panel (used by scripted screenshot runs).
    pub fn set_settings_open(&mut self, open: bool) {
        self.show_settings = open;
    }

    /// Open the settings at the connection form.
    pub fn open_connection(&mut self) {
        self.show_settings = true;
        self.reveal_connection = true;
    }

    /// Scripted runs: put an endpoint in the connection form and test it, as if typed and
    /// "Test connection" pressed.
    pub fn test_connection(&mut self, url: &str, model: &str) {
        self.connect.fill(url, model);
        self.connect.start_test();
    }

    /// Scripted runs: open a workspace view: `new-workspace`, `new-workspace-container`,
    /// `workspace-settings` (of the chat in view's workspace), `chat-settings`, `more-menu` (the
    /// header's "⋯" menu), `plugins` (Settings → Plugins). False for an
    /// unknown name, or a workspace view before the workspace list arrived.
    pub fn open_panel(&mut self, name: &str) -> bool {
        use byob_protocol::workspace::EnvKind;
        match name {
            "new-workspace" => self.workspaces.open_new(EnvKind::Auto),
            "new-workspace-container" => {
                self.workspaces.open_new(EnvKind::Container);
                self.conn.send(ClientMsg::CheckContainers);
            }
            "workspace-settings" => match self.sidebar.current_workspace().map(|w| w.id.clone()) {
                Some(id) => self.workspaces.open_settings(&id, false),
                None => return false,
            },
            "more-menu" => self.header_rects.open_menu = true,
            "plugins" => {
                self.plugins.open_on_trouble();
                self.show_settings = true;
            }
            "plugin-pane" => return self.plugins.sites.open_pane(""),
            _ if name.starts_with("plugin-pane:") => return self.plugins.sites.open_pane(name.trim_start_matches("plugin-pane:")),
            _ if name.starts_with("artifact:") => return self.artifacts.show_matching(name.trim_start_matches("artifact:")),
            _ if name.starts_with("plugin:") => {
                self.plugins.open(Some(name.trim_start_matches("plugin:")));
                self.show_settings = true;
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
    pub const PANELS: &'static str = "new-workspace, new-workspace-container, workspace-settings, chat-settings, more-menu, plugins";
    /// Names [`ByobApp::open_panel`] takes once plugins are known: `plugin-pane` (the first
    /// plugin pane), `plugin-pane:<plugin>[/<view>]`, `plugin:<id>` (Settings → Plugins, open
    /// on that plugin).
    pub const PLUGIN_PANELS: &'static str = "plugin-pane, plugin-pane:<plugin>[/<view>], plugin:<id>, artifact:<prefix>";

    /// Scripted runs: open the settings at an appearance section (`knobs`, `advanced`).
    pub fn reveal_settings(&mut self, section: &str) {
        self.show_settings = true;
        self.appearance.reveal = Some(section.to_owned());
    }

    /// Type `text` into the composer without sending it, as a person would (scripted runs:
    /// `/` shows the menu).
    pub fn set_draft(&mut self, text: &str) {
        text.clone_into(&mut self.input);
        self.focus_composer = true;
    }

    /// Send a user message, as if typed into the composer.
    pub fn submit(&mut self, text: String) {
        let attachments = self.uploads.take();
        self.conn.send(ClientMsg::SendMessage { text, attachments });
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
        for e in self.uploads.poll(ui.ctx(), self.conn.as_mut()) {
            self.notice(NoticeLevel::Error, e);
        }
        self.poll_appearance();
        self.apply_theme(ui.ctx());
        let narrow = ui.available_width() < sidebar::NARROW;
        self.artifacts.begin_frame(ui.ctx(), &self.tokens, narrow);
        let list_open = self.sidebar.is_open(narrow);
        self.header_shortcuts(ui.ctx());
        self.header(ui);
        upload::drop_overlay(ui.ctx(), &self.tokens);
        if self.show_settings {
            // Tall on purpose (gallery, knobs, every colour role): scrolls within most of the window.
            let max = (ui.available_height() * 0.72).max(200.0);
            egui::Panel::top("settings").show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("settings-scroll")
                    .max_height(max)
                    .min_scrolled_height(max)
                    .auto_shrink([false, true])
                    .show(ui, |ui| self.settings(ui));
            });
        }
        let t = self.tokens;
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
        if list_open {
            // The id carries the width knob so changing it resizes the panel (egui remembers dragged sizes per id).
            let w = t.knobs.sidebar_width;
            #[allow(clippy::cast_possible_truncation)] // a few hundred points
            let sent = egui::Panel::left(("sidebar", w as i32))
                .resizable(true)
                .default_size(w)
                .size_range(180.0..=420.0)
                .frame(egui::Frame::side_top_panel(ui.style()).fill(t.sidebar_bg))
                .show(ui, |ui| self.sidebar.show(ui, &t, false))
                .inner;
            self.send_all(sent);
        }
        if !narrow {
            // The pane, or its rail when collapsed; then the plugins' pane beside it.
            self.artifacts.side_panel(ui, &t);
            self.plugins.sites.side_panel(ui, &t, self.artifacts.web_mut());
        }
        egui::Panel::bottom("composer").show(ui, |ui| self.composer(ui));
        egui::CentralPanel::default().show(ui, |ui| {
            self.plugins.trust_card(ui, &t, &self.entries);
            self.transcript(ui);
        });
        if narrow {
            self.artifacts.floating_button(ui.ctx(), &t);
        }
        self.end_frame(ui.ctx());
    }

    /// After drawing: artifact actions and overlays, and what they want sent or shown.
    fn end_frame(&mut self, ctx: &egui::Context) {
        for r in self.sidebar.take_requests() {
            match r {
                sidebar::Request::NewWorkspace => self.workspaces.open_new(byob_protocol::workspace::EnvKind::Auto),
                sidebar::Request::WorkspaceSettings { id, remove } => self.workspaces.open_settings(&id, remove),
                sidebar::Request::AppSettings => self.show_settings = true,
            }
        }
        let t = self.tokens;
        let title = self.sidebar.current().map_or("New chat", |c| c.title.as_str()).to_owned();
        let gear = self.header_rects.chat_settings;
        let anchor = egui::pos2(gear.left(), gear.bottom() + 6.0);
        let anchor = if gear.is_positive() { anchor } else { egui::pos2(ctx.content_rect().center().x - 200.0, 48.0) };
        let msgs = self.workspaces.windows(ctx, &t, self.sidebar.workspaces(), &title, anchor, &mut self.plugins);
        self.send_all(msgs);
        self.artifacts.end_frame(ctx);
        let (msgs, notices) = self.artifacts.take_output();
        self.send_all(msgs);
        let msgs = self.connect.take();
        self.send_all(msgs);
        let msgs = self.plugins.take();
        self.send_all(msgs);
        if self.plugins.take_open_request() {
            self.show_settings = true;
        }
        for (level, text) in notices {
            self.notice(level, text);
        }
    }

    fn send_all(&mut self, msgs: Vec<ClientMsg>) {
        for m in msgs {
            self.conn.send(m);
        }
    }

    fn composer(&mut self, ui: &mut egui::Ui) {
        let t = self.tokens;
        ui.add_space(6.0);
        for (level, text) in &self.notices {
            let color = if *level == NoticeLevel::Error { t.danger } else { t.text_dim };
            ui.label(egui::RichText::new(text).color(color));
        }
        let id = egui::Id::unique("composer");
        if std::mem::take(&mut self.focus_composer) {
            ui.memory_mut(|m| m.request_focus(id));
        }
        // Enter sends (queues while a reply runs); Shift+Enter inserts a newline (consume
        // Enter before the editor sees it). Escape stops editing a queued message. While the
        // `/` menu is open it takes ↑/↓/Enter/Tab/Esc instead.
        let focused = ui.memory(|m| m.has_focus(id));
        let commands: Vec<byob_protocol::SlashItem> = slash::matches(self.plugins.slash(), &self.input).into_iter().cloned().collect();
        // egui drops the focus on Esc before we see the key: the menu still takes that Esc.
        let escaped = !focused && self.slash.had_focus && ui.input(|i| i.key_pressed(egui::Key::Escape));
        let menu = self.slash.open(&self.input, commands.len()) && (focused || escaped);
        self.slash.had_focus = focused;
        let mut picked = if menu { self.slash.keys(ui, &self.input, commands.len()) } else { None };
        if escaped && menu {
            ui.memory_mut(|m| m.request_focus(id));
        }
        let enter = focused && !menu && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
        let busy = self.busy();
        let editing = self.queue.editing;
        let mut send = enter;
        let mut cancel_edit = editing.is_some() && focused && !menu && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        let actions = self.queue.show(ui, &t);
        self.queue_actions(actions);
        self.uploads.chips(ui, &t);
        let hint = match (editing.is_some(), busy) {
            (true, _) => "Edit the queued message…  (Enter to save, Esc to cancel)",
            (false, true) => "Queue a follow-up…  (Enter to queue, Shift+Enter for newline)",
            (false, false) => "Message byob…  (Enter to send, Shift+Enter for newline)",
        };
        ui.horizontal(|ui| {
            self.uploads.button(ui);
            let buttons = if editing.is_some() || busy { 132.0 } else { 72.0 };
            let width = (ui.available_width() - buttons).max(100.0);
            let field = ui.add(egui::TextEdit::multiline(&mut self.input).id(id).desired_rows(2).desired_width(width).hint_text(hint));
            // Still open after this frame's typing (and not just picked): draw it.
            if menu && picked.is_none() && slash::query(&self.input).is_some() {
                let shown: Vec<&byob_protocol::SlashItem> = commands.iter().collect();
                picked = self.slash.show(ui, &t, field.rect, &shown);
            }
            if editing.is_some() {
                send |= ui.button("Save").on_hover_text("Save the queued message").clicked();
                cancel_edit |= ui.button("Cancel").clicked();
            } else if busy {
                send |= ui.button("Queue").on_hover_text("Send this when the reply is done").clicked();
                if ui.button("Stop").clicked() {
                    self.conn.send(ClientMsg::Cancel);
                }
            } else {
                send |= ui.button("Send").clicked();
            }
        });
        if let Some(item) = picked.and_then(|k| commands.get(k)) {
            self.slash.reset();
            self.input = slash::insert(item);
            let mut state = egui::TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
            let end = egui::text::CCursor::new(self.input.chars().count());
            state.cursor.set_char_range(Some(egui::text::CCursorRange::one(end)));
            egui::TextEdit::store_state(ui.ctx(), id, state);
            ui.memory_mut(|m| m.request_focus(id));
        }
        if cancel_edit {
            self.end_edit();
        }
        // Wait for attachments to finish uploading before sending.
        let ready = (!self.input.trim().is_empty() || self.uploads.has_ready()) && !self.uploads.busy();
        match self.queue.editing {
            Some(item) if send && !self.uploads.busy() => {
                let msg = ClientMsg::EditQueued { id: item, text: std::mem::take(&mut self.input), attachments: self.uploads.take() };
                self.conn.send(msg);
                self.queue.editing = None;
                let (text, files) = self.queue.stash.take().unwrap_or_default();
                self.input = text;
                self.uploads.restore(files);
                ui.memory_mut(|m| m.request_focus(id));
            }
            None if send && ready => {
                let text = std::mem::take(&mut self.input);
                self.submit(text);
                ui.memory_mut(|m| m.request_focus(id));
            }
            _ => {}
        }
        ui.add_space(6.0);
    }

    fn transcript(&mut self, ui: &mut egui::Ui) {
        let t = self.tokens;
        if self.entries.is_empty() {
            self.empty_chat(ui);
            return;
        }
        if self.markdown.len() > self.entries.len() {
            let entries = &self.entries;
            self.markdown.retain(|id| entries.iter().any(|e| e.id == id));
        }
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
        egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
            readable_column(ui, &t, |ui| {
                ui.add_space(t.pad(8.0));
                usage.dividers(ui, &t, 0);
                for entry in &self.entries {
                    answers.extend(entry_view(ui, &t, entry, markdown, media, artifacts, (plugins, views, &latest), retry_id == Some(entry.id), &mut card));
                    ui.add_space(t.pad(8.0));
                    usage.dividers(ui, &t, entry.id);
                }
            });
        });
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
                // puts the prompt in the composer.
                let starters = self.plugins.starters();
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
                    self.input = p;
                    self.focus_composer = true;
                }
            }
        }
    }
}

/// A reply that failed before it produced anything (see [`ClientMsg::Retry`]).
fn retryable(e: &Entry) -> bool {
    e.role == EntryRole::Assistant && e.done && e.error.is_some() && e.content.is_empty() && e.steps.is_empty()
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
) -> Option<ClientMsg> {
    let mut answer = None;
    let user = entry.role == EntryRole::User;
    let (who, fill, style) = if user { ("You", t.user_bubble, t.knobs.user_bubble) } else { ("byob", t.assistant_bubble, t.knobs.assistant_bubble) };
    let (fill, stroke, margin) = match style {
        theme::BubbleStyle::Filled => (fill, egui::Stroke::NONE, t.pad(10.0)),
        theme::BubbleStyle::Outlined => {
            (egui::Color32::TRANSPARENT, egui::Stroke::new(1.0, if user { theme::mix(t.border, t.accent, 0.6) } else { t.border }), t.pad(10.0))
        }
        theme::BubbleStyle::Minimal => (egui::Color32::TRANSPARENT, egui::Stroke::NONE, t.pad(4.0)),
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
    ui.with_layout(layout, |ui| {
        egui::Frame::new().fill(fill).stroke(stroke).corner_radius(t.round(8.0)).inner_margin(margin).show(ui, |ui| {
            ui.set_width(width - 2.0 * margin);
            ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(who).strong().color(t.accent));
                    if entry.pending_approvals().next().is_some() {
                        steps::waiting_badge(ui, t);
                    } else if !entry.done {
                        theme::spinner(ui, t, None);
                    }
                });
                if !entry.reasoning.is_empty() {
                    let thinking = !entry.done && entry.content.is_empty() && entry.steps.iter().all(|s| s.status == StepStatus::Running);
                    let title = format!("{} · {}", if thinking { "Thinking…" } else { "Thought" }, size_label(entry.reasoning.chars().count()));
                    egui::CollapsingHeader::new(egui::RichText::new(title).color(t.text_dim)).id_salt(("thinking", entry.id)).show(ui, |ui| {
                        // Reasoning can run to many thousands of words: keep it in its own scroll box,
                        // following the newest text while the model is still thinking.
                        egui::ScrollArea::vertical()
                            .id_salt(("thinking-scroll", entry.id))
                            .max_height(THINKING_MAX_HEIGHT)
                            .auto_shrink([false, true])
                            .stick_to_bottom(thinking)
                            .show(ui, |ui| {
                                ui.add(egui::Label::new(egui::RichText::new(&entry.reasoning).color(t.text_dim).small()).selectable(true).wrap());
                            });
                    });
                }
                match entry.role {
                    // reply text with its tool steps where they happened, approval cards, undo
                    EntryRole::Assistant => {
                        let newest = artifact::newest_cards(&entry.steps);
                        answer = steps::body(ui, t, entry, markdown, &mut |ui, step| {
                            artifacts.step_cards(ui, t, step.id, &step.artifacts, &newest);
                            views.show_step_with(ui, t, step, latest, plugins, artifacts.web_mut());
                        });
                    }
                    EntryRole::User if !entry.content.is_empty() => {
                        ui.add(egui::Label::new(egui::RichText::new(&entry.content).color(t.text)).selectable(true));
                    }
                    EntryRole::User => {}
                }
                media.show_entry(ui, t, entry);
                if let Some(err) = &entry.error
                    && let Some(action) = connect::error_card(ui, t, err, retry)
                {
                    *card = Some(action);
                }
                if let Some(s) = entry.stats.filter(|_| entry.role == EntryRole::Assistant) {
                    #[allow(clippy::cast_precision_loss)] // display only
                    let secs = s.elapsed_ms as f64 / 1000.0;
                    ui.label(
                        egui::RichText::new(format!("{} in · {} out tokens · {secs:.1}s", s.prompt_tokens, s.completion_tokens)).small().color(t.text_dim),
                    );
                }
            });
        });
    });
    answer
}

/// The transcript in a centred column no wider than the user's readable width.
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
mod thinking_tests {
    use super::size_label;

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(size_label(840), "840 chars");
        assert_eq!(size_label(12_400), "12.4k chars");
    }
}

#[cfg(test)]
mod app_tests {
    use std::sync::{Arc, Mutex};

    use byob_protocol::{ChatSummary, EngineState, Link};

    use super::*;

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
        let fake = Fake::default();
        let title = "A chat with a remarkably long title that would never fit in any header at all, really".to_owned();
        let ws = byob_protocol::WorkspaceSummary {
            id: "w".into(),
            name: "A workspace whose name is also much longer than anyone would reasonably type".into(),
            folder: "/w".into(),
            chats: 1,
            updated_at: 1,
            env: byob_protocol::workspace::EnvKind::Container,
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
            versions: vec![byob_protocol::artifact::ArtifactVersion { n: 1, path: "a/v1.md".into(), created_at: 1, note: String::new(), size: 1 }],
        };
        fake.inbox.lock().unwrap().extend([
            ServerMsg::State(s),
            ServerMsg::Workspaces { items: vec![ws], current: None, root: String::new(), support: Default::default() },
            ServerMsg::ChatOpened {
                chat: Some(ChatSummary { id: "c".into(), title, updated_at: 1, pinned_at: None, busy: false, workspace: Some("w".into()) }),
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
            plugins::tests::ext(vec![troubled_plugin()]),
        ]);
        ByobApp::new(Box::new(fake), "test")
    }

    fn troubled_plugin() -> byob_protocol::PluginInfo {
        let mut p = plugins::tests::info("weather", byob_protocol::PluginScope::Workspace, byob_protocol::PluginOrigin::Agent);
        p.problems = vec!["tools/x.py is missing".into()];
        p
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
            for (name, rect) in [("chat list", r.chats), ("chat settings", r.chat_settings), ("Settings", r.settings), ("more", r.more)] {
                assert!(rect.is_positive() && rect.min.x >= 0.0 && rect.max.x <= width + 0.5, "width {width}: {name} at {rect:?}");
            }
            assert!(r.chats.max.x <= r.chat_settings.min.x, "width {width}: {r:?}");
            assert!(r.chat_settings.max.x <= r.settings.min.x, "width {width}: {r:?}");
            assert!(r.settings.max.x <= r.more.min.x, "width {width}: {r:?}");
            // The plugins chip, between the chat's settings and the app's.
            assert!(r.plugins.is_positive() && r.chat_settings.max.x <= r.plugins.min.x && r.plugins.max.x <= r.settings.min.x, "width {width}: {r:?}");
            // "Settings" is spelled out wherever it is not a phone.
            let labelled = r.settings.width() > 40.0;
            assert_eq!(labelled, header::Layout::for_width(width).settings_label, "width {width}: {r:?}");
        }
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
        let Some(ClientMsg::CreateWorkspace { name, folder, config }) = last else { panic!("{last:?}") };
        assert_eq!((name.as_str(), folder, config.env), ("Fresh", byob_protocol::NewFolder::Create, Some(byob_protocol::workspace::EnvKind::Container)));
    }

    fn press_enter(ctx: &egui::Context, app: &mut ByobApp) {
        press(ctx, app, egui::Key::Enter);
    }

    /// Focus the composer and press `key` in it.
    fn press(ctx: &egui::Context, app: &mut ByobApp, key: egui::Key) {
        ctx.memory_mut(|m| m.request_focus(egui::Id::unique("composer")));
        frame(ctx, app, 1000.0);
        let key = egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE };
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
        assert_eq!(app.plugins.summary(), Some((1, true)));
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
        assert!(app.show_settings && !app.reveal_connection);
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
