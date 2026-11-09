//! The yah UI. Identical on desktop and in the browser: it renders engine state received
//! over a [`Connection`] and sends user intents back. It never touches the engine directly.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

pub mod appview;
pub mod artifact;
pub mod chart;
pub mod export;
pub mod markdown;
pub mod media;
mod settings;
pub mod sidebar;
pub mod steps;
pub mod theme;
pub mod upload;
pub mod ws;

use yah_protocol::{ClientMsg, Connection, EngineState, Entry, EntryRole, NoticeLevel, PROTOCOL_VERSION, ServerMsg, StepStatus};

/// Notices kept on screen.
const MAX_NOTICES: usize = 3;

pub struct YahApp {
    conn: Box<dyn Connection>,
    tokens: theme::Tokens,
    prefs: theme::Prefs,
    /// What `tokens` were last built from and pushed into egui.
    applied: Option<(theme::Prefs, Option<egui::Theme>)>,
    markdown: markdown::Cache,
    sidebar: sidebar::Sidebar,
    state: Option<EngineState>,
    entries: Vec<Entry>,
    notices: Vec<(NoticeLevel, String)>,
    input: String,
    show_settings: bool,
    form_url: String,
    form_model: String,
    form_key: String,
    /// Charts and images shown in the transcript.
    media: media::Cache,
    /// Files attached in the composer.
    uploads: upload::Uploads,
    /// The chat's artifacts: cards, the pane, export.
    artifacts: artifact::Artifacts,
}

impl YahApp {
    pub fn new(mut conn: Box<dyn Connection>, client: &str) -> Self {
        conn.send(ClientMsg::Hello { protocol: PROTOCOL_VERSION, client: client.into() });
        Self {
            conn,
            tokens: theme::Tokens::dark(),
            prefs: theme::Prefs::default(),
            applied: None,
            markdown: markdown::Cache::default(),
            sidebar: sidebar::Sidebar::default(),
            state: None,
            entries: Vec::new(),
            notices: Vec::new(),
            input: String::new(),
            show_settings: false,
            form_url: String::new(),
            form_model: String::new(),
            form_key: String::new(),
            media: media::Cache::default(),
            uploads: upload::Uploads::default(),
            artifacts: artifact::Artifacts::default(),
        }
    }

    /// Plug in how HTML app artifacts are shown (default: an iframe in the browser, a
    /// summary with Open on the desktop).
    pub fn set_app_view(&mut self, view: Box<dyn appview::AppView>) {
        self.artifacts.set_app_view(view);
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
            if self.sidebar.apply(&msg) || self.artifacts.apply(&msg) {
                continue;
            }
            match msg {
                ServerMsg::State(state) => {
                    self.form_url.clone_from(&state.inference_base_url);
                    self.form_model.clone_from(&state.inference_model);
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
            }
        }
    }

    fn entry_mut(&mut self, id: u64) -> Option<&mut Entry> {
        self.entries.iter_mut().rev().find(|e| e.id == id)
    }

    /// Per-device appearance preferences, for the app to persist.
    pub fn prefs(&self) -> theme::Prefs {
        self.prefs
    }

    /// Restore (or change) appearance preferences; applied on the next frame.
    pub fn set_prefs(&mut self, prefs: theme::Prefs) {
        self.prefs = prefs.sanitized();
    }

    /// Rebuild the tokens when the preferences or the OS/browser theme changed.
    fn apply_theme(&mut self, ctx: &egui::Context) {
        let key = (self.prefs, ctx.system_theme());
        if self.applied.is_none() {
            theme::install_fonts(ctx);
        }
        if self.applied != Some(key) {
            self.tokens = theme::Tokens::for_prefs(&self.prefs, key.1);
            self.tokens.apply(ctx, self.prefs.font_scale);
            self.applied = Some(key);
        }
    }

    /// Open or close the settings panel (used by scripted screenshot runs).
    pub fn set_settings_open(&mut self, open: bool) {
        self.show_settings = open;
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

    fn busy(&self) -> bool {
        self.state.as_ref().is_some_and(|s| s.busy)
    }

    /// Draw one frame.
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        self.pump();
        for e in self.uploads.poll(ui.ctx(), self.conn.as_mut()) {
            self.notice(NoticeLevel::Error, e);
        }
        self.apply_theme(ui.ctx());
        let narrow = ui.available_width() < sidebar::NARROW;
        self.artifacts.begin_frame(ui.ctx(), &self.tokens, narrow);
        let list_open = self.sidebar.is_open(narrow);
        self.header(ui, narrow);
        upload::drop_overlay(ui.ctx(), &self.tokens);
        if self.show_settings {
            egui::Panel::top("settings").show(ui, |ui| self.settings(ui));
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
            let sent = egui::Panel::left("sidebar")
                .resizable(true)
                .default_size(240.0)
                .size_range(180.0..=420.0)
                .show(ui, |ui| self.sidebar.show(ui, &t, false))
                .inner;
            self.send_all(sent);
        }
        if self.artifacts.is_open() {
            let width = ui.available_width();
            egui::Panel::right("artifact-pane")
                .resizable(true)
                .default_size((width * 0.48).max(320.0))
                .size_range(300.0..=(width * 0.75).max(320.0))
                .show(ui, |ui| self.artifacts.pane(ui, &t, false));
        }
        egui::Panel::bottom("composer").show(ui, |ui| self.composer(ui));
        egui::CentralPanel::default().show(ui, |ui| self.transcript(ui));
        self.end_frame(ui.ctx());
    }

    /// After drawing: artifact actions and overlays, and what they want sent or shown.
    fn end_frame(&mut self, ctx: &egui::Context) {
        self.artifacts.end_frame(ctx);
        let (msgs, notices) = self.artifacts.take_output();
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
                if ui.button("Chats").on_hover_text("Show or hide the chat list").clicked() {
                    self.sidebar.toggle();
                }
                ui.label(egui::RichText::new("yah").strong().size(18.0).color(t.accent));
                let title = self.sidebar.current().map_or("New chat", |c| c.title.as_str());
                let max = if narrow { 18 } else { 48 };
                let short: String =
                    if title.chars().count() > max { title.chars().take(max - 1).chain(std::iter::once('…')).collect() } else { title.to_owned() };
                ui.label(egui::RichText::new(short).color(t.text_strong)).on_hover_text(title);
                match &self.state {
                    // The endpoint is detail a phone screen has no room for.
                    Some(_) if narrow => ui.label(""),
                    Some(s) => ui.label(egui::RichText::new(format!("{} · {}", s.inference_model, s.inference_base_url)).color(t.text_dim)),
                    None => ui.label(egui::RichText::new("connecting…").color(t.text_dim)),
                };
                if let Some(info) = self.state.as_ref().and_then(|s| s.endpoint_info.as_ref()) {
                    ui.label(egui::RichText::new(info.summary()).small().color(t.text_dim));
                }
                if steps::awaiting_approval(&self.entries) {
                    steps::waiting_badge(ui, &t);
                } else if self.busy() {
                    ui.spinner();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.toggle_value(&mut self.show_settings, "Settings");
                });
            });
        });
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
                    .hint_text("Message yah…  (Enter to send, Shift+Enter for newline)"),
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
            ui.centered_and_justified(|ui| {
                ui.label(egui::RichText::new("Ask anything to get started.").color(t.text_dim));
            });
            return;
        }
        if self.markdown.len() > self.entries.len() {
            let entries = &self.entries;
            self.markdown.retain(|id| entries.iter().any(|e| e.id == id));
        }
        let markdown = &mut self.markdown;
        let media = &mut self.media;
        let artifacts = &mut self.artifacts;
        let mut answers = Vec::new();
        egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
            ui.add_space(8.0);
            for entry in &self.entries {
                answers.extend(entry_view(ui, &t, entry, markdown, media, artifacts));
                ui.add_space(8.0);
            }
        });
        answers.extend(self.media.take_requests());
        for msg in answers {
            self.conn.send(msg);
        }
    }
}

/// Draws one entry; returns a message to send if the user answered an approval card.
fn entry_view(
    ui: &mut egui::Ui,
    t: &theme::Tokens,
    entry: &Entry,
    markdown: &mut markdown::Cache,
    media: &mut media::Cache,
    artifacts: &mut artifact::Artifacts,
) -> Option<ClientMsg> {
    let mut answer = None;
    let (who, fill) = match entry.role {
        EntryRole::User => ("You", t.user_bubble),
        EntryRole::Assistant => ("yah", t.assistant_bubble),
    };
    egui::Frame::new().fill(fill).corner_radius(8.0).inner_margin(10.0).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(who).strong().color(t.accent));
            if entry.pending_approvals().next().is_some() {
                steps::waiting_badge(ui, t);
            } else if !entry.done {
                ui.spinner();
            }
        });
        if !entry.reasoning.is_empty() {
            let thinking = !entry.done && entry.content.is_empty() && entry.steps.iter().all(|s| s.status == StepStatus::Running);
            let title = if thinking { "Thinking…" } else { "Thought" };
            egui::CollapsingHeader::new(egui::RichText::new(title).color(t.text_dim)).id_salt(("thinking", entry.id)).show(ui, |ui| {
                ui.label(egui::RichText::new(&entry.reasoning).color(t.text_dim).small());
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
        if let Some(err) = &entry.error {
            ui.label(egui::RichText::new(err).color(t.danger));
        }
        if let Some(s) = entry.stats.filter(|_| entry.role == EntryRole::Assistant) {
            #[allow(clippy::cast_precision_loss)] // display only
            let secs = s.elapsed_ms as f64 / 1000.0;
            ui.label(egui::RichText::new(format!("{} in · {} out tokens · {secs:.1}s", s.prompt_tokens, s.completion_tokens)).small().color(t.text_dim));
        }
    });
    answer
}
