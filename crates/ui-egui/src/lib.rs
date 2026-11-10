//! The byob UI. Identical on desktop and in the browser: it renders engine state received
//! over a [`Connection`] and sends user intents back. It never touches the engine directly.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

pub mod appview;
pub mod artifact;
pub mod chart;
pub mod connect;
mod envpanel;
pub mod export;
mod highlight;
mod icons;
pub mod markdown;
pub mod media;
pub mod pane;
mod settings;
pub mod sidebar;
pub mod steps;
pub mod theme;
pub mod upload;
pub mod usage;
pub mod workspace;
pub mod ws;

use byob_protocol::{ClientMsg, Connection, EngineState, Entry, EntryRole, NoticeLevel, PROTOCOL_VERSION, ServerMsg, StepStatus};

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
    /// Where the Settings button was last drawn (tests check it stays on screen).
    settings_button: egui::Rect,
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
    /// Where the chat settings button was last drawn (the popover opens below it).
    chat_button: egui::Rect,
}

impl ByobApp {
    pub fn new(mut conn: Box<dyn Connection>, client: &str) -> Self {
        conn.send(ClientMsg::Hello { protocol: PROTOCOL_VERSION, client: client.into() });
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
            settings_button: egui::Rect::NOTHING,
            media: media::Cache::default(),
            uploads: upload::Uploads::default(),
            artifacts: artifact::Artifacts::default(),
            usage: usage::Usage::default(),
            workspaces: workspace::WorkspaceUi::default(),
            chat_button: egui::Rect::NOTHING,
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
            if matches!(msg, ServerMsg::ChatOpened { .. }) {
                self.workspaces.chat_changed();
            }
            if self.usage.apply(&msg) || self.workspaces.apply(&msg) || self.sidebar.apply(&msg) || self.artifacts.apply(&msg) {
                continue;
            }
            match msg {
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
    /// `workspace-settings` (of the chat in view's workspace), `chat-settings`. False for an
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
    pub const PANELS: &'static str = "new-workspace, new-workspace-container, workspace-settings, chat-settings";

    /// Scripted runs: open the settings at an appearance section (`knobs`, `advanced`).
    pub fn reveal_settings(&mut self, section: &str) {
        self.show_settings = true;
        self.appearance.reveal = Some(section.to_owned());
    }

    /// Send a user message, as if typed into the composer.
    pub fn submit(&mut self, text: String) {
        let attachments = self.uploads.take();
        self.conn.send(ClientMsg::SendMessage { text, attachments });
    }

    /// Connected, and nothing will change without the user: no turn running and every
    /// entry finished streaming (and its charts/images loaded), or a turn is waiting on an
    /// approval card.
    pub fn settled(&self) -> bool {
        self.state.is_some()
            && (steps::awaiting_approval(&self.entries)
                || (!self.busy() && self.entries.iter().all(|e| e.done) && !self.uploads.busy() && !self.media.loading() && !self.artifacts.loading()))
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
        self.header(ui, narrow);
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
        if narrow && self.artifacts.is_open() {
            // Phones: the artifact takes the whole screen; "← Chat" goes back.
            egui::CentralPanel::default().show(ui, |ui| self.artifacts.pane(ui, &t, true));
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
            // The pane, or its rail when collapsed.
            self.artifacts.side_panel(ui, &t);
        }
        egui::Panel::bottom("composer").show(ui, |ui| self.composer(ui));
        egui::CentralPanel::default().show(ui, |ui| self.transcript(ui));
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
            }
        }
        let t = self.tokens;
        let title = self.sidebar.current().map_or("New chat", |c| c.title.as_str()).to_owned();
        let anchor = egui::pos2(self.chat_button.left(), self.chat_button.bottom() + 6.0);
        let anchor = if self.chat_button.is_positive() { anchor } else { egui::pos2(ctx.content_rect().center().x - 200.0, 48.0) };
        let msgs = self.workspaces.windows(ctx, &t, self.sidebar.workspaces(), &title, anchor);
        self.send_all(msgs);
        self.artifacts.end_frame(ctx);
        let (msgs, notices) = self.artifacts.take_output();
        self.send_all(msgs);
        let msgs = self.connect.take();
        self.send_all(msgs);
        for (level, text) in notices {
            self.notice(level, text);
        }
    }

    fn send_all(&mut self, msgs: Vec<ClientMsg>) {
        for m in msgs {
            self.conn.send(m);
        }
    }

    fn header(&mut self, ui: &mut egui::Ui, narrow: bool) {
        let t = self.tokens;
        egui::Panel::top("header").show(ui, |ui| {
            ui.horizontal(|ui| {
                // The right-hand buttons first, so a long title or URL can never push
                // Settings off screen; the rest shares what is left.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let settings = ui.toggle_value(&mut self.show_settings, "Settings").on_hover_text("Connection and appearance");
                    self.settings_button = settings.rect;
                    self.artifacts.header_buttons(ui, &t);
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| self.header_left(ui, &t, narrow));
                });
            });
        });
    }

    /// Chats, name, status, chat title and endpoint details, dropping the optional ones
    /// (and shortening the rest) when the window is narrow.
    fn header_left(&mut self, ui: &mut egui::Ui, t: &theme::Tokens, narrow: bool) {
        if ui.button("Chats").on_hover_text("Show or hide the chat list").clicked() {
            self.sidebar.toggle();
        }
        ui.label(egui::RichText::new("byob").strong().size(t.text_size(18.0)).color(t.accent));
        if steps::awaiting_approval(&self.entries) {
            steps::waiting_badge(ui, t);
        } else if self.busy() {
            theme::spinner(ui, t, None);
        }
        let title = self.sidebar.current().map_or("New chat", |c| c.title.as_str()).to_owned();
        // "Workspace / title", the workspace dimmed (dropped on phones).
        let ws = self.sidebar.current_workspace().map(|w| w.name.clone()).filter(|_| !narrow);
        let room = ui.available_width();
        ui.scope(|ui| {
            ui.set_max_width((room * 0.4).max(40.0));
            let mut job = egui::text::LayoutJob::default();
            let font = egui::TextStyle::Body.resolve(ui.style());
            if let Some(ws) = &ws {
                job.append(&format!("{ws} / "), 0.0, egui::TextFormat::simple(font.clone(), t.text_dim));
            }
            job.append(&title, 0.0, egui::TextFormat::simple(font, t.text_strong));
            let tip = ws.as_ref().map_or_else(|| title.clone(), |w| format!("{title}\nin the workspace {w}"));
            ui.add(egui::Label::new(job).truncate()).on_hover_text(tip);
        });
        let gear = icons::settings_button(ui, t).on_hover_text("This chat's settings: model, approvals, where commands run…");
        self.chat_button = gear.rect;
        if gear.clicked() {
            self.workspaces.toggle_chat();
        }
        // The connection: its status dot, model and server; a click opens its settings.
        let room = ui.available_width();
        let clicked = ui
            .scope(|ui| {
                ui.set_max_width((room * 0.6).max(40.0));
                match &self.state {
                    Some(s) => connect::chip(ui, t, s, narrow),
                    None => {
                        ui.label(egui::RichText::new("connecting…").color(t.text_dim));
                        None
                    }
                }
            })
            .inner;
        match clicked {
            Some(connect::ChipClick::Connection) => self.open_connection(),
            Some(connect::ChipClick::ChatSettings) if self.workspaces.chat.is_none() => self.workspaces.toggle_chat(),
            Some(connect::ChipClick::ChatSettings) | None => {}
        }
        let info = self.state.as_ref().and_then(|s| match &s.chat_endpoint {
            Some(e) => e.link.is_ok().then_some(e.info.as_ref()).flatten().map(byob_protocol::EndpointInfo::summary),
            None => s.link.is_ok().then_some(s.endpoint_info.as_ref()).flatten().map(byob_protocol::EndpointInfo::summary),
        });
        if let Some(info) = info.filter(|_| !narrow && ui.available_width() > 360.0) {
            let room = ui.available_width() - 160.0;
            ui.scope(|ui| {
                ui.set_max_width(room);
                ui.add(egui::Label::new(egui::RichText::new(&info).small().color(t.text_dim)).truncate());
            });
        }
        if ui.available_width() > 60.0
            && let Some(msg) = self.usage.meter(ui, t, narrow)
        {
            self.conn.send(msg);
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
        // Enter sends; Shift+Enter inserts a newline (consume Enter before the editor sees it).
        let enter = ui.memory(|m| m.has_focus(id)) && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
        let busy = self.busy();
        let mut send = enter && !busy;
        self.uploads.chips(ui, &t);
        ui.horizontal(|ui| {
            self.uploads.button(ui);
            let width = (ui.available_width() - 72.0).max(100.0);
            ui.add(
                egui::TextEdit::multiline(&mut self.input)
                    .id(id)
                    .desired_rows(2)
                    .desired_width(width)
                    .hint_text("Message byob…  (Enter to send, Shift+Enter for newline)"),
            );
            if busy {
                if ui.button("Stop").clicked() {
                    self.conn.send(ClientMsg::Cancel);
                }
            } else {
                send |= ui.button("Send").clicked();
            }
        });
        // Wait for attachments to finish uploading before sending.
        if send && (!self.input.trim().is_empty() || self.uploads.has_ready()) && !self.uploads.busy() {
            let text = std::mem::take(&mut self.input);
            self.submit(text);
            ui.memory_mut(|m| m.request_focus(id));
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
                    answers.extend(entry_view(ui, &t, entry, markdown, media, artifacts, retry_id == Some(entry.id), &mut card));
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
                ui.centered_and_justified(|ui| {
                    ui.label(egui::RichText::new("Ask anything to get started.").color(t.text_dim));
                });
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
                    EntryRole::Assistant => answer = steps::body(ui, t, entry, markdown, &mut |ui, step| artifacts.cards(ui, t, &step.artifacts)),
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

    #[test]
    fn settings_stays_on_screen_however_crowded_the_header() {
        for width in [320.0, 480.0, 720.0, 1000.0, 1600.0] {
            let fake = Fake::default();
            let title = "A chat with a remarkably long title that would never fit in any header at all, really".to_owned();
            // In a workspace with a long name, with its own endpoint (the chip shows that one).
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
            fake.inbox.lock().unwrap().extend([
                ServerMsg::State(s),
                ServerMsg::Workspaces { items: vec![ws], current: None, root: String::new(), support: Default::default() },
                ServerMsg::ChatOpened {
                    chat: Some(ChatSummary { id: "c".into(), title, updated_at: 1, pinned_at: None, busy: false, workspace: Some("w".into()) }),
                    workspace: Some("w".into()),
                },
            ]);
            let ctx = egui::Context::default();
            let mut app = ByobApp::new(Box::new(fake), "test");
            for _ in 0..3 {
                frame(&ctx, &mut app, width);
            }
            let r = app.settings_button;
            assert!(r.is_positive() && r.min.x >= 0.0 && r.max.x <= width + 0.5, "width {width}: Settings at {r:?}");
            let gear = app.chat_button;
            assert!(gear.is_positive() && gear.max.x <= r.min.x, "width {width}: the chat settings button at {gear:?}");
        }
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
}
