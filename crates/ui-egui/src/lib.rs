//! The yah UI. Identical on desktop and in the browser: it renders engine state received
//! over a [`Connection`] and sends user intents back. It never touches the engine directly.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

pub mod markdown;
mod settings;
pub mod steps;
pub mod theme;
pub mod ws;

use yah_protocol::{ClientMsg, Connection, EngineState, Entry, EntryRole, NoticeLevel, PROTOCOL_VERSION, ServerMsg};

/// Notices kept on screen.
const MAX_NOTICES: usize = 3;

pub struct YahApp {
    conn: Box<dyn Connection>,
    tokens: theme::Tokens,
    prefs: theme::Prefs,
    /// What `tokens` were last built from and pushed into egui.
    applied: Option<(theme::Prefs, Option<egui::Theme>)>,
    markdown: markdown::Cache,
    state: Option<EngineState>,
    entries: Vec<Entry>,
    notices: Vec<(NoticeLevel, String)>,
    input: String,
    show_settings: bool,
    form_url: String,
    form_model: String,
    form_key: String,
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
            state: None,
            entries: Vec::new(),
            notices: Vec::new(),
            input: String::new(),
            show_settings: false,
            form_url: String::new(),
            form_model: String::new(),
            form_key: String::new(),
        }
    }

    fn pump(&mut self) {
        while let Some(msg) = self.conn.try_recv() {
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
        self.conn.send(ClientMsg::SendMessage { text });
    }

    /// Connected, and nothing will change without the user: no turn running and every
    /// entry finished streaming, or a turn is waiting on an approval card.
    pub fn settled(&self) -> bool {
        self.state.is_some() && (steps::awaiting_approval(&self.entries) || (!self.busy() && self.entries.iter().all(|e| e.done)))
    }

    fn busy(&self) -> bool {
        self.state.as_ref().is_some_and(|s| s.busy)
    }

    /// Draw one frame.
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        self.pump();
        self.apply_theme(ui.ctx());
        self.header(ui);
        if self.show_settings {
            egui::Panel::top("settings").show(ui, |ui| self.settings(ui));
        }
        egui::Panel::bottom("composer").show(ui, |ui| self.composer(ui));
        egui::CentralPanel::default().show(ui, |ui| self.transcript(ui));
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        let t = self.tokens;
        egui::Panel::top("header").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("yah").strong().size(18.0).color(t.accent));
                match &self.state {
                    Some(s) => ui.label(egui::RichText::new(format!("{} · {}", s.inference_model, s.inference_base_url)).color(t.text_dim)),
                    None => ui.label(egui::RichText::new("connecting…").color(t.text_dim)),
                };
                if self.busy() {
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
        ui.horizontal(|ui| {
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
        if send && !self.input.trim().is_empty() {
            let text = std::mem::take(&mut self.input);
            self.conn.send(ClientMsg::SendMessage { text });
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
        let mut answers = Vec::new();
        egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
            ui.add_space(8.0);
            for entry in &self.entries {
                answers.extend(entry_view(ui, &t, entry, markdown));
                ui.add_space(8.0);
            }
        });
        for msg in answers {
            self.conn.send(msg);
        }
    }
}

/// Draws one entry; returns a message to send if the user answered an approval card.
fn entry_view(ui: &mut egui::Ui, t: &theme::Tokens, entry: &Entry, markdown: &mut markdown::Cache) -> Option<ClientMsg> {
    let mut answer = None;
    let (who, fill) = match entry.role {
        EntryRole::User => ("You", t.user_bubble),
        EntryRole::Assistant => ("yah", t.assistant_bubble),
    };
    egui::Frame::new().fill(fill).corner_radius(8.0).inner_margin(10.0).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(who).strong().color(t.accent));
            if !entry.done {
                ui.spinner();
            }
        });
        if !entry.reasoning.is_empty() {
            let title = if entry.done || !entry.content.is_empty() { "Thought" } else { "Thinking…" };
            egui::CollapsingHeader::new(egui::RichText::new(title).color(t.text_dim)).id_salt(("thinking", entry.id)).show(ui, |ui| {
                ui.label(egui::RichText::new(&entry.reasoning).color(t.text_dim).small());
            });
        }
        answer = steps::show(ui, t, entry);
        if !entry.content.is_empty() {
            match entry.role {
                EntryRole::Assistant => {
                    let doc = markdown.doc(entry.id, &entry.content, entry.done);
                    markdown::show(ui, t, doc, egui::Id::unique(("md", entry.id)));
                }
                EntryRole::User => {
                    ui.add(egui::Label::new(egui::RichText::new(&entry.content).color(t.text)).selectable(true));
                }
            }
        }
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
