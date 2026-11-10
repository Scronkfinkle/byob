//! Connecting to an inference endpoint: the welcome screen shown until an endpoint has
//! worked, the connection form (also at the top of Settings), the header's connection chip,
//! and the card that explains a reply that couldn't reach the server.
//!
//! The engine does the talking ([`ClientMsg::TestInference`], [`ClientMsg::SetInference`]);
//! this only fills the form and shows answers, so it works the same in a browser tab.

use byob_protocol::{ClientMsg, Detected, EngineState, InferenceTest, Link, PRESETS, Preset, Problem};
use egui::text::LayoutJob;
use egui::{Color32, RichText, TextFormat};

use crate::theme::{self, Tokens};

/// Widest the welcome screen gets.
const WELCOME_WIDTH: f32 = 580.0;

/// What an empty chat shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmptyView {
    /// No engine state yet.
    Waiting,
    /// Nothing has worked yet: explain byob and connect something.
    Welcome,
    /// It worked before, but the endpoint doesn't answer now.
    Trouble,
    /// "Ask anything".
    Ready,
}

pub fn empty_view(state: Option<&EngineState>) -> EmptyView {
    match state {
        None => EmptyView::Waiting,
        Some(s) if !s.onboarded && !s.link.is_ok() => EmptyView::Welcome,
        Some(s) if matches!(s.link, Link::Failed { .. }) => EmptyView::Trouble,
        Some(_) => EmptyView::Ready,
    }
}

/// A connection test the form asked for.
#[derive(Debug, Clone, Default, PartialEq)]
enum Test {
    #[default]
    Idle,
    Running {
        url: String,
        model: String,
    },
    Done(InferenceTest),
}

/// What the user asked for from an error card or the trouble screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardAction {
    Retry,
    OpenSettings,
}

/// The connection form's state between frames.
#[derive(Debug, Default)]
pub struct ConnectForm {
    pub url: String,
    pub model: String,
    /// A new API key; never filled from the engine (it doesn't send keys).
    pub key: String,
    /// The engine endpoint the fields were last filled from.
    synced: Option<(String, String)>,
    /// Edited since: a state update doesn't overwrite the user's typing.
    dirty: bool,
    /// The model was filled in for the user (preset, placeholder): a test may replace it
    /// with one the server lists.
    auto_model: bool,
    test: Test,
    /// Models the server listed (last test or detection), for the picker.
    models: Vec<String>,
    /// Messages for the engine, taken by the app each frame.
    out: Vec<ClientMsg>,
}

impl ConnectForm {
    /// Follow the engine's endpoint unless the user is editing.
    pub fn sync(&mut self, s: &EngineState) {
        let now = (s.inference_base_url.clone(), s.inference_model.clone());
        if self.synced.as_ref() == Some(&now) {
            return;
        }
        if !self.dirty || (self.url.trim() == now.0 && self.model.trim() == now.1) {
            self.url.clone_from(&now.0);
            self.model.clone_from(&now.1);
            self.dirty = false;
            // byob's placeholder model name: a server's own list is better.
            self.auto_model = now.1 == "default" || now.1.trim().is_empty();
        }
        self.synced = Some(now);
    }

    /// Messages to send.
    pub fn take(&mut self) -> Vec<ClientMsg> {
        std::mem::take(&mut self.out)
    }

    /// Type an endpoint into the form.
    pub fn fill(&mut self, url: &str, model: &str) {
        url.clone_into(&mut self.url);
        model.clone_into(&mut self.model);
        self.auto_model = false;
        self.edited();
    }

    fn edited(&mut self) {
        self.dirty = true;
        self.test = Test::Idle;
    }

    /// Fill the form from a preset; local servers are tested at once.
    pub fn pick_preset(&mut self, p: &Preset, detected: &[Detected]) {
        self.url = p.base_url.into();
        self.models = detected.iter().find(|d| d.preset == p.id).map(|d| d.models.clone()).unwrap_or_default();
        self.model = self.models.first().cloned().unwrap_or_else(|| p.model.into());
        self.auto_model = true;
        self.edited();
        if p.local || !self.key.trim().is_empty() {
            self.start_test();
        }
    }

    /// Use a detected server right away.
    pub fn use_detected(&mut self, d: &Detected) {
        self.url.clone_from(&d.base_url);
        self.models.clone_from(&d.models);
        if let Some(m) = d.models.first() {
            self.model.clone_from(m);
        }
        self.edited();
        self.connect();
    }

    pub fn start_test(&mut self) {
        let (url, model) = (self.url.trim().to_owned(), self.model.trim().to_owned());
        let api_key = Some(self.key.trim().to_owned()).filter(|k| !k.is_empty());
        self.out.push(ClientMsg::TestInference { base_url: url.clone(), model: model.clone(), api_key });
        self.test = Test::Running { url, model };
    }

    /// A test answer. Stale ones (the form changed since) are dropped. If the server lacks
    /// a model we filled in for the user, take its first and test again.
    pub fn tested(&mut self, t: InferenceTest) {
        let Test::Running { url, model } = &self.test else { return };
        if *url != t.base_url || *model != t.model {
            return;
        }
        if !t.models.is_empty() {
            self.models.clone_from(&t.models);
        }
        if t.problem == Some(Problem::ModelNotFound)
            && (self.auto_model || self.model.trim().is_empty())
            && let Some(first) = t.models.first().filter(|m| **m != t.model)
        {
            self.model.clone_from(first);
            self.auto_model = false;
            self.start_test();
            return;
        }
        self.test = Test::Done(t);
    }

    /// Switch the engine to the form's endpoint (a typed key goes along; none keeps the
    /// saved one for the same server).
    pub fn connect(&mut self) {
        let api_key = Some(std::mem::take(&mut self.key)).map(|k| k.trim().to_owned()).filter(|k| !k.is_empty());
        self.out.push(ClientMsg::SetInference { base_url: self.url.trim().to_owned(), model: self.model.trim().to_owned(), api_key });
        self.dirty = false;
        self.test = Test::Idle;
    }

    fn remove_key(&mut self) {
        self.out.push(ClientMsg::SetInference { base_url: self.url.trim().to_owned(), model: self.model.trim().to_owned(), api_key: Some(String::new()) });
        self.key.clear();
    }

    /// The form: presets, URL, model, key, Test and Connect, and how it went. `salt` keeps
    /// widget ids apart when the form is on screen twice (welcome and Settings).
    pub fn form(&mut self, ui: &mut egui::Ui, t: &Tokens, state: &EngineState, salt: &str) {
        ui.label(RichText::new("Quick setup").small().color(t.text_dim));
        ui.horizontal_wrapped(|ui| {
            let current = Preset::matching(&self.url);
            for p in PRESETS {
                let running = state.detected.iter().any(|d| d.preset == p.id);
                let mut job = LayoutJob::default();
                if running {
                    job.append("● ", 0.0, fmt(ui, t.success));
                }
                job.append(p.name, 0.0, fmt(ui, t.text));
                if p.needs_key {
                    job.append("  key", 0.0, fmt_small(ui, t.text_faint));
                }
                let mut tip = format!("{}\n{}", p.hint, p.base_url);
                if running {
                    tip.push_str("\nRunning on this computer");
                }
                if ui.selectable_label(current == Some(p), job).on_hover_text(tip).clicked() {
                    self.pick_preset(p, &state.detected);
                }
            }
        });
        ui.add_space(6.0);
        let narrow = ui.available_width() < 420.0;
        let w = if narrow { ui.available_width() } else { (ui.available_width() - 100.0).clamp(160.0, 440.0) };
        let row = |ui: &mut egui::Ui, label: &str, add: &mut dyn FnMut(&mut egui::Ui)| {
            if narrow {
                ui.label(RichText::new(label).small().color(t.text_dim));
                add(ui);
            } else {
                ui.horizontal(|ui| {
                    let size = egui::vec2(88.0, ui.spacing().interact_size.y);
                    ui.allocate_ui_with_layout(size, egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.set_min_size(size);
                        ui.label(label);
                    });
                    add(ui);
                });
            }
        };
        row(ui, "Server URL", &mut |ui| {
            let r = ui.add(egui::TextEdit::singleline(&mut self.url).id_salt((salt, "url")).hint_text("http://127.0.0.1:8080/v1").desired_width(w));
            if r.changed() {
                self.edited();
            }
        });
        let models = self.models.clone();
        row(ui, "Model", &mut |ui| {
            ui.horizontal(|ui| {
                let field = if models.is_empty() { w } else { (w - 80.0).max(80.0) };
                let r = ui.add(egui::TextEdit::singleline(&mut self.model).id_salt((salt, "model")).hint_text("as the server names it").desired_width(field));
                if r.changed() {
                    self.auto_model = false;
                    self.edited();
                }
                if !models.is_empty() {
                    let picked = ui
                        .menu_button("Choose…", |ui| {
                            let mut picked = None;
                            egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                                for m in &models {
                                    if ui.selectable_label(*m == self.model, m).clicked() {
                                        picked = Some(m.clone());
                                        ui.close();
                                    }
                                }
                            });
                            picked
                        })
                        .inner
                        .flatten();
                    if let Some(m) = picked {
                        self.model = m;
                        self.auto_model = false;
                        self.edited();
                    }
                }
            });
        });
        let preset = Preset::matching(&self.url);
        let hint = match (state.has_api_key, preset.is_some_and(|p| p.needs_key)) {
            (true, _) => "saved — leave blank to keep (same server only)",
            (false, true) => "required by this provider",
            (false, false) => "optional — hosted APIs need one",
        };
        row(ui, "API key", &mut |ui| {
            ui.horizontal(|ui| {
                let field = if state.has_api_key { (w - 70.0).max(80.0) } else { w };
                let edit = egui::TextEdit::singleline(&mut self.key).id_salt((salt, "key")).password(true).hint_text(hint).desired_width(field);
                if ui.add(edit).changed() {
                    self.test = Test::Idle;
                }
                if state.has_api_key && ui.small_button("Remove").on_hover_text("Forget the saved API key").clicked() {
                    self.remove_key();
                }
            });
        });
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            let testing = matches!(self.test, Test::Running { .. });
            let empty = self.url.trim().is_empty();
            if ui.add_enabled(!testing && !empty, egui::Button::new("Test connection")).on_hover_text("Try it without switching").clicked() {
                self.start_test();
            }
            let connect = egui::Button::new(RichText::new("Connect").color(t.on_accent)).fill(t.accent);
            if ui.add_enabled(!empty, connect).on_hover_text("Use this server for chats (saved)").clicked() {
                self.connect();
            }
        });
        ui.add_space(4.0);
        self.status(ui, t, state);
    }

    /// How the last test went, or else how the engine's endpoint is doing (when the form
    /// shows it).
    fn status(&self, ui: &mut egui::Ui, t: &Tokens, state: &EngineState) {
        match &self.test {
            Test::Running { url, .. } => {
                ui.horizontal(|ui| {
                    theme::spinner(ui, t, Some(12.0));
                    ui.label(RichText::new(format!("Testing {url}…")).color(t.text_dim));
                });
            }
            Test::Done(r) if r.ok => {
                let what = if r.detail.is_empty() { "It answers.".to_owned() } else { format!("It answers: {}.", r.detail) };
                status_line(ui, t.success, &format!("{what} Press Connect to use it."), t);
            }
            Test::Done(r) => problem_lines(ui, t, r.problem, &r.detail),
            Test::Idle => {
                let showing = self.url.trim() == state.inference_base_url && self.model.trim() == state.inference_model;
                if !showing {
                    return;
                }
                match &state.link {
                    Link::Checking => {
                        ui.horizontal(|ui| {
                            theme::spinner(ui, t, Some(12.0));
                            ui.label(RichText::new("Connecting…").color(t.text_dim));
                        });
                    }
                    Link::Ok => {
                        let info = state.endpoint_info.as_ref().map(byob_protocol::EndpointInfo::summary).unwrap_or_default();
                        status_line(ui, t.success, &format!("Connected. {info}"), t);
                    }
                    // Before anything has worked, an empty port is the expected state, not an error.
                    Link::Failed { problem: Some(Problem::Unreachable), .. } if !state.onboarded => {
                        let text = format!("Nothing answers at {} yet. Start a server there, or pick another above.", short_url(&state.inference_base_url));
                        status_line(ui, t.text_faint, &text, t);
                    }
                    Link::Failed { problem, detail } => problem_lines(ui, t, *problem, detail),
                    Link::Unknown => {}
                }
            }
        }
    }

    /// The welcome screen: what byob needs, servers found on this computer, and the form.
    pub fn welcome(&mut self, ui: &mut egui::Ui, t: &Tokens, state: &EngineState) {
        egui::ScrollArea::vertical().id_salt("welcome").auto_shrink([false, false]).show(ui, |ui| {
            let full = ui.available_width();
            let width = full.min(WELCOME_WIDTH);
            let side = ((full - width) / 2.0).max(0.0);
            ui.horizontal_top(|ui| {
                ui.add_space(side);
                ui.vertical(|ui| {
                    ui.set_width(width);
                    ui.add_space((ui.ctx().content_rect().height() * 0.06).clamp(8.0, 56.0));
                    ui.label(RichText::new("Connect a model").size(t.text_size(24.0)).family(theme::bold_or_regular(ui.ctx())).color(t.text_strong));
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(
                            "byob brings no model of its own. Point it at any OpenAI-compatible server: \
                             one running on this computer (llama-server, Ollama, LM Studio, vLLM), or a hosted API with a key.",
                        )
                        .color(t.text),
                    );
                    ui.add_space(14.0);
                    self.found(ui, t, state);
                    ui.add_space(12.0);
                    egui::Frame::new().fill(t.panel_bg).stroke(egui::Stroke::new(1.0, t.border)).corner_radius(t.round(10.0)).inner_margin(t.pad(14.0)).show(
                        ui,
                        |ui| {
                            ui.set_width(ui.available_width());
                            self.form(ui, t, state, "welcome");
                        },
                    );
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new("The byob engine makes these requests, so 127.0.0.1 means the computer byob runs on. API keys stay there.")
                            .small()
                            .color(t.text_faint),
                    );
                    ui.add_space(16.0);
                });
            });
        });
    }

    /// Servers found on this computer, each with Use; or that none were.
    fn found(&mut self, ui: &mut egui::Ui, t: &Tokens, state: &EngineState) {
        if state.detecting {
            ui.horizontal(|ui| {
                theme::spinner(ui, t, Some(12.0));
                ui.label(RichText::new("Looking for model servers on this computer…").color(t.text_dim));
            });
            return;
        }
        if state.detected.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("No model server found on the usual ports of this computer.").color(t.text_dim));
                if ui.small_button("Look again").clicked() {
                    self.out.push(ClientMsg::DetectEndpoints);
                }
            });
            return;
        }
        ui.label(RichText::new("Found on this computer").small().color(t.text_dim));
        for d in &state.detected {
            let name = Preset::by_id(&d.preset).map_or(d.preset.as_str(), |p| p.name);
            let used = !self.dirty && state.inference_base_url == d.base_url && !matches!(state.link, Link::Failed { .. });
            ui.horizontal_wrapped(|ui| {
                dot(ui, t.success);
                ui.label(RichText::new(name).strong().color(t.text_strong));
                let models = match d.models.len() {
                    1 => d.models.first().cloned().unwrap_or_default(),
                    n => format!("{n} models"),
                };
                ui.label(RichText::new(format!("{} · {models}", d.base_url)).color(t.text_dim));
                if !used && ui.add(egui::Button::new(RichText::new("Use").color(t.on_accent)).fill(t.accent)).clicked() {
                    self.use_detected(d);
                }
            });
        }
    }
}

fn fmt(ui: &egui::Ui, color: Color32) -> TextFormat {
    TextFormat { font_id: egui::TextStyle::Button.resolve(ui.style()), color, ..Default::default() }
}

fn fmt_small(ui: &egui::Ui, color: Color32) -> TextFormat {
    TextFormat { font_id: egui::TextStyle::Small.resolve(ui.style()), color, ..Default::default() }
}

/// A small filled circle in the text line.
fn dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}

fn status_line(ui: &mut egui::Ui, color: Color32, text: &str, t: &Tokens) {
    ui.horizontal_wrapped(|ui| {
        dot(ui, color);
        ui.label(RichText::new(text).color(t.text));
    });
}

/// A failure in words: the headline, what to do, and the raw error, small.
fn problem_lines(ui: &mut egui::Ui, t: &Tokens, problem: Option<Problem>, detail: &str) {
    match problem {
        Some(p) => {
            ui.horizontal_wrapped(|ui| {
                dot(ui, t.danger);
                ui.label(RichText::new(p.title()).strong().color(t.danger));
            });
            ui.label(RichText::new(p.advice()).color(t.text));
        }
        None => status_line(ui, t.danger, "That didn't work.", t),
    }
    if !detail.is_empty() {
        ui.add(egui::Label::new(RichText::new(detail).small().color(t.text_faint)).selectable(true).wrap());
    }
}

/// The header's connection status: a dot (green: answers, amber: checking, red: failing)
/// and the model and server of the chat in view (its own or its workspace's endpoint, if it
/// has one), as much of it as `detail` asks for; the rest, and what the server can do, is in
/// its tooltip. Clicking it opens the connection settings, or the chat's settings when the
/// endpoint isn't the global one ([`ChipClick`]).
pub fn chip(ui: &mut egui::Ui, t: &Tokens, state: &EngineState, detail: ChipDetail) -> Option<ChipClick> {
    let own = state.chat_endpoint.as_ref();
    let (base_url, model, link, info) = match own {
        Some(e) => (&e.base_url, &e.model, &e.link, e.info.as_ref()),
        None => (&state.inference_base_url, &state.inference_model, &state.link, state.endpoint_info.as_ref()),
    };
    let color = match link {
        Link::Ok => t.success,
        Link::Checking => t.warning,
        Link::Failed { .. } => t.danger,
        Link::Unknown => t.text_faint,
    };
    let server = short_url(base_url);
    let text = match detail {
        ChipDetail::Dot => String::new(),
        ChipDetail::Model => model.clone(),
        ChipDetail::Full => format!("{model} · {server}"),
    };
    let mut job = LayoutJob::default();
    job.append(if text.is_empty() { "●" } else { "● " }, 0.0, fmt(ui, color));
    job.append(&text, 0.0, fmt(ui, t.text_dim));
    job.wrap = egui::text::TextWrapping::truncate_at_width(ui.available_width().max(40.0));
    let mut tip = format!("{base_url}\n{model}");
    if let Some(e) = own {
        tip.push_str(&format!("\nChosen in the {} settings", e.from.label()));
    }
    match link {
        Link::Ok => {
            if let Some(info) = info {
                tip.push_str(&format!("\n{}", info.summary()));
            }
        }
        Link::Checking => tip.push_str("\nChecking the connection…"),
        Link::Failed { problem, detail } => {
            tip.push_str(&format!("\n{}", problem.map_or("Not reachable", Problem::title)));
            if !detail.is_empty() {
                tip.push_str(&format!("\n{detail}"));
            }
        }
        Link::Unknown => {}
    }
    tip.push_str(if own.is_some() { "\n\nClick to change this chat's model" } else { "\n\nClick to change the connection" });
    let clicked = ui.add(egui::Button::new(job).frame(false)).on_hover_text(tip).clicked();
    clicked.then_some(if own.is_some() { ChipClick::ChatSettings } else { ChipClick::Connection })
}

/// How much the status chip spells out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipDetail {
    /// Only the status dot (phones).
    Dot,
    /// The dot and the model.
    Model,
    /// The dot, the model and the server.
    Full,
}

/// What a click on the status chip opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipClick {
    Connection,
    ChatSettings,
}

/// `http://127.0.0.1:8080/v1` → `127.0.0.1:8080`.
pub fn short_url(url: &str) -> String {
    let u = url.trim();
    let u = u.split_once("://").map_or(u, |(_, rest)| rest);
    let u = u.trim_end_matches('/');
    u.strip_suffix("/v1").unwrap_or(u).to_owned()
}

/// The empty chat when a once-working endpoint doesn't answer.
pub fn trouble(ui: &mut egui::Ui, t: &Tokens, state: &EngineState) -> Option<CardAction> {
    let Link::Failed { problem, detail } = &state.link else { return None };
    let mut action = None;
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() * 0.25).max(12.0));
        ui.set_max_width(WELCOME_WIDTH.min(ui.available_width()));
        ui.label(RichText::new(problem.map_or("The model server isn't answering", Problem::title)).size(t.text_size(18.0)).color(t.text_strong));
        ui.add_space(4.0);
        ui.label(RichText::new(short_url(&state.inference_base_url)).color(t.text_dim));
        if let Some(p) = problem {
            ui.add_space(4.0);
            ui.label(RichText::new(p.advice()).color(t.text));
        }
        ui.add_space(4.0);
        ui.add(egui::Label::new(RichText::new(detail).small().color(t.text_faint)).wrap());
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            let w = ui.available_width();
            ui.add_space(((w - 260.0) / 2.0).max(0.0));
            if ui.button("Check again").clicked() {
                action = Some(CardAction::Retry);
            }
            if ui.add(egui::Button::new(RichText::new("Connection settings").color(t.on_accent)).fill(t.accent)).clicked() {
                action = Some(CardAction::OpenSettings);
            }
        });
    });
    action
}

/// A reply's error: a connection problem gets a card in plain words with Retry (if
/// `can_retry`) and Connection settings; anything else stays a line of red text.
pub fn error_card(ui: &mut egui::Ui, t: &Tokens, error: &str, can_retry: bool) -> Option<CardAction> {
    let Some(problem) = Problem::classify(error) else {
        ui.label(RichText::new(error).color(t.danger));
        return None;
    };
    let mut action = None;
    egui::Frame::new()
        .fill(theme::mix(t.assistant_bubble, t.danger, 0.08))
        .stroke(egui::Stroke::new(1.0, theme::mix(t.border, t.danger, 0.5)))
        .corner_radius(t.round(6.0))
        .inner_margin(t.pad(10.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            problem_lines(ui, t, Some(problem), error);
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                if can_retry && ui.add(egui::Button::new(RichText::new("Retry").color(t.on_accent)).fill(t.accent)).clicked() {
                    action = Some(CardAction::Retry);
                }
                if ui.button("Connection settings").clicked() {
                    action = Some(CardAction::OpenSettings);
                }
            });
        });
    action
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(url: &str, model: &str) -> EngineState {
        EngineState { inference_base_url: url.into(), inference_model: model.into(), ..Default::default() }
    }

    fn test_reply(url: &str, model: &str, problem: Option<Problem>, models: &[&str]) -> InferenceTest {
        InferenceTest {
            base_url: url.into(),
            model: model.into(),
            ok: problem.is_none(),
            problem,
            detail: String::new(),
            models: models.iter().map(|m| (*m).to_owned()).collect(),
        }
    }

    #[test]
    fn empty_chat_shows_welcome_until_something_worked() {
        assert_eq!(empty_view(None), EmptyView::Waiting);
        let mut s = state("u", "m");
        assert_eq!(empty_view(Some(&s)), EmptyView::Welcome);
        s.link = Link::failed("could not reach u: refused");
        assert_eq!(empty_view(Some(&s)), EmptyView::Welcome);
        s.onboarded = true;
        assert_eq!(empty_view(Some(&s)), EmptyView::Trouble);
        s.link = Link::Checking;
        assert_eq!(empty_view(Some(&s)), EmptyView::Ready);
        // Working right now counts even before the engine has saved it.
        s.onboarded = false;
        s.link = Link::Ok;
        assert_eq!(empty_view(Some(&s)), EmptyView::Ready);
    }

    #[test]
    fn form_follows_the_engine_unless_edited() {
        let mut f = ConnectForm::default();
        f.sync(&state("http://a/v1", "default"));
        assert_eq!((f.url.as_str(), f.model.as_str(), f.auto_model), ("http://a/v1", "default", true));
        f.url = "http://typing".into();
        f.edited();
        f.sync(&state("http://b/v1", "m"));
        assert_eq!(f.url, "http://typing");
        // Applying the form makes the engine agree; later engine changes show again.
        f.connect();
        assert!(matches!(f.take().as_slice(), [ClientMsg::SetInference { base_url, api_key: None, .. }] if base_url == "http://typing"));
        f.sync(&state("http://c/v1", "m"));
        assert_eq!(f.url, "http://c/v1");
    }

    #[test]
    fn presets_fill_and_test_local_servers() {
        let mut f = ConnectForm::default();
        let detected = [Detected { preset: "ollama".into(), base_url: "http://127.0.0.1:11434/v1".into(), models: vec!["qwen3".into()] }];
        let ollama = Preset::by_id("ollama").unwrap();
        f.pick_preset(ollama, &detected);
        assert_eq!((f.url.as_str(), f.model.as_str()), ("http://127.0.0.1:11434/v1", "qwen3"));
        assert!(matches!(f.take().as_slice(), [ClientMsg::TestInference { model, .. }] if model == "qwen3"));
        // A hosted preset waits for a key before testing.
        f.pick_preset(Preset::by_id("openai").unwrap(), &[]);
        assert!(f.take().is_empty() && f.test == Test::Idle);
        f.key = "sk-1".into();
        f.pick_preset(Preset::by_id("openrouter").unwrap(), &[]);
        assert!(matches!(f.take().as_slice(), [ClientMsg::TestInference { api_key: Some(k), .. }] if k == "sk-1"));
    }

    #[test]
    fn a_missing_placeholder_model_is_swapped_for_a_listed_one() {
        let mut f = ConnectForm::default();
        f.pick_preset(Preset::by_id("ollama").unwrap(), &[]);
        let sent = f.take();
        let [ClientMsg::TestInference { base_url, model, .. }] = sent.as_slice() else { panic!("{sent:?}") };
        assert_eq!(model, "");
        // A stale answer (another URL) is ignored.
        f.tested(test_reply("http://elsewhere/v1", "", None, &[]));
        assert!(matches!(f.test, Test::Running { .. }));
        f.tested(test_reply(base_url, "", Some(Problem::ModelNotFound), &["llama3", "gemma"]));
        assert_eq!(f.model, "llama3");
        assert!(matches!(f.take().as_slice(), [ClientMsg::TestInference { model, .. }] if model == "llama3"));
        f.tested(test_reply(base_url, "llama3", None, &["llama3", "gemma"]));
        assert!(matches!(&f.test, Test::Done(r) if r.ok));
        assert_eq!(f.models, ["llama3", "gemma"]);
        // A model the user typed is not replaced: they see the problem and the list.
        f.model = "mine".into();
        f.auto_model = false;
        f.start_test();
        f.take();
        f.tested(test_reply(base_url, "mine", Some(Problem::ModelNotFound), &["llama3"]));
        assert_eq!(f.model, "mine");
        assert!(matches!(&f.test, Test::Done(r) if r.problem == Some(Problem::ModelNotFound)));
    }

    #[test]
    fn using_a_detected_server_connects_with_its_first_model() {
        let mut f = ConnectForm::default();
        f.use_detected(&Detected { preset: "lm-studio".into(), base_url: "http://127.0.0.1:1234/v1".into(), models: vec!["qwen".into(), "x".into()] });
        let sent = f.take();
        assert!(
            matches!(sent.as_slice(), [ClientMsg::SetInference { base_url, model, api_key: None }] if base_url == "http://127.0.0.1:1234/v1" && model == "qwen")
        );
    }

    #[test]
    fn urls_shorten_for_the_header() {
        assert_eq!(short_url("http://127.0.0.1:8080/v1"), "127.0.0.1:8080");
        assert_eq!(short_url("https://openrouter.ai/api/v1/"), "openrouter.ai/api");
        assert_eq!(short_url("weird"), "weird");
    }

    #[test]
    fn everything_renders_without_clicks() {
        let ctx = egui::Context::default();
        let mut f = ConnectForm::default();
        let mut s = state("http://127.0.0.1:8080/v1", "default");
        s.detected = vec![Detected { preset: "ollama".into(), base_url: "http://127.0.0.1:11434/v1".into(), models: vec!["a".into()] }];
        s.link = Link::failed("server returned HTTP 401: bad key");
        s.has_api_key = true;
        f.models = vec!["a".into()];
        for width in [1000.0, 340.0] {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 800.0))), ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| {
                let t = Tokens::dark();
                f.welcome(ui, &t, &s);
                for detail in [ChipDetail::Dot, ChipDetail::Model, ChipDetail::Full] {
                    assert!(chip(ui, &t, &s, detail).is_none());
                }
                assert_eq!(trouble(ui, &t, &s), None);
                assert_eq!(error_card(ui, &t, "could not reach x: refused", true), None);
                assert_eq!(error_card(ui, &t, "tool loop", false), None);
            });
            out.textures_delta.clear();
        }
        assert!(f.take().is_empty());
    }
}
