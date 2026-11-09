//! Artifacts in the UI: a card where a step made or changed one (title, kind, version,
//! Show / Open / Export, a versions menu), and the artifact pane beside the conversation
//! (full screen on narrow windows) showing the selected one large — markdown with a
//! Preview/Source switch, charts natively, images, text as source with Copy, and HTML apps
//! through the [`AppView`] seam.
//!
//! The pane has tabs (closing one keeps the artifact) and collapses to a rail of icons on
//! the right (a floating button with a count on phones); Ctrl+Backslash (Cmd on macOS) toggles it. Any
//! artifact can be minimized to a one-line chip, in the chat and the pane; "Collapse all"
//! in the header does that for the whole chat. Minimized apps, background tabs and apps
//! behind the rail are [held](AppView::hold): they keep running without rendering for a
//! while, then pause. What is open and minimized is kept per chat ([`crate::pane`]).
//!
//! The engine sends the chat's artifacts with the transcript and an update whenever a
//! version is added; version bytes are fetched once with `GetArtifact` and kept here (they
//! are also what Export saves).

use std::collections::HashMap;

use egui::{RichText, Stroke, vec2};
use yah_protocol::media::{self, ChartSpec};
use yah_protocol::{Artifact, ArtifactKind, ArtifactRef, ClientMsg, NoticeLevel, ServerMsg};

use crate::appview::{self, AppDoc, AppStatus, AppView};
use crate::export::Exports;
use crate::markdown;
use crate::pane::{Pane, PanePrefs, Shown as PaneShown};
use crate::theme::Tokens;

/// Tallest an image or chart preview is drawn in a card.
const CARD_PREVIEW_HEIGHT: f32 = 260.0;
/// Lines of text shown in a card.
const CARD_LINES: usize = 4;
/// Versions kept in memory before old ones are dropped (they are fetched again if needed).
const MAX_FILES: usize = 64;
/// Width of the collapsed pane's rail.
const RAIL_WIDTH: f32 = 46.0;
/// Size of a rail icon.
const RAIL_ICON: f32 = 32.0;
/// Characters of a title shown on a tab.
const TAB_CHARS: usize = 22;
/// Toggles the pane.
pub const PANE_SHORTCUT: egui::KeyboardShortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::Backslash);

/// Produces the base URL (`http://127.0.0.1:PORT`) the desktop app serves artifacts at,
/// starting a server if needed. The browser client uses its own origin.
pub type LinkBase = Box<dyn Fn() -> Result<String, String>>;

enum Shown {
    Text { text: String, doc: Option<Box<markdown::Doc>> },
    Chart(Box<ChartSpec>),
    Image(egui::TextureHandle),
    Bad(String),
}

enum File {
    Requested,
    Ready { bytes: Vec<u8>, shown: Option<Shown> },
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    Select {
        id: String,
        version: Option<u32>,
    },
    Open {
        id: String,
        version: u32,
    },
    Export {
        id: String,
        version: u32,
    },
    SendError {
        id: String,
        version: u32,
        error: String,
    },
    /// Collapse the pane to the rail (on phones: back to the chat).
    Close,
    Expand,
    Toggle,
    Activate(String),
    CloseTab(String),
    Minimize {
        id: String,
        minimized: bool,
    },
    MinimizeAll(bool),
}

pub struct Artifacts {
    chat: Option<String>,
    list: Vec<Artifact>,
    files: HashMap<(String, u32), File>,
    /// Insertion order of `files`, for dropping the oldest.
    order: Vec<(String, u32)>,
    /// Tabs, minimized artifacts, open/collapsed.
    pane: Pane,
    /// A version picked for a tab (else the latest).
    versions: HashMap<String, u32>,
    /// This frame is narrow (phones).
    narrow: bool,
    /// The HTML app the pane drew this frame (the other tabs are held in the background).
    drawn_app: Option<String>,
    /// Markdown shows its source instead of the preview.
    source: bool,
    /// Open the pane on this artifact at the next frame (if there is room).
    reveal: Option<String>,
    /// Exports waiting for their file.
    pending_export: Vec<(String, u32)>,
    view: Box<dyn AppView>,
    exports: Exports,
    link_base: Option<LinkBase>,
    theme: Vec<(&'static str, String)>,
    outbox: Vec<ClientMsg>,
    notices: Vec<(NoticeLevel, String)>,
    actions: Vec<Action>,
    /// For repaints when an export or open finishes.
    ctx: egui::Context,
}

impl Default for Artifacts {
    fn default() -> Self {
        Self {
            chat: None,
            list: Vec::new(),
            files: HashMap::new(),
            order: Vec::new(),
            pane: Pane::default(),
            versions: HashMap::new(),
            narrow: false,
            drawn_app: None,
            source: false,
            reveal: None,
            pending_export: Vec::new(),
            view: appview::default_view(),
            exports: Exports::default(),
            link_base: None,
            theme: Vec::new(),
            outbox: Vec::new(),
            notices: Vec::new(),
            actions: Vec::new(),
            ctx: egui::Context::default(),
        }
    }
}

impl Artifacts {
    pub fn set_app_view(&mut self, view: Box<dyn AppView>) {
        self.view = view;
    }

    pub fn set_link_base(&mut self, base: LinkBase) {
        self.link_base = Some(base);
    }

    /// What this device remembers about the pane (to store).
    pub fn pane_prefs(&self) -> PanePrefs {
        self.pane.prefs()
    }

    pub fn set_pane_prefs(&mut self, prefs: PanePrefs) {
        self.pane.set_prefs(prefs);
        let list = &self.list;
        self.pane.retain(|id| list.iter().any(|a| a.id == id));
    }

    /// Show or collapse the pane (as the shortcut does).
    pub fn toggle_pane(&mut self) {
        self.actions.push(Action::Toggle);
    }

    /// Minimize (or restore) every artifact in this chat, as "Collapse all" does.
    pub fn minimize_all(&mut self, minimized: bool) {
        self.actions.push(Action::MinimizeAll(minimized));
    }

    /// Open a tab for every artifact in this chat, the first in front (scripted runs).
    pub fn open_all(&mut self) {
        for a in self.list.clone() {
            self.pane.add_tab(&a.id);
        }
        if let Some(first) = self.list.first() {
            self.pane.activate(&first.id);
        }
        self.pane.expand(self.narrow);
    }

    /// Apply a server message; returns whether it was one of ours.
    pub fn apply(&mut self, msg: &ServerMsg) -> bool {
        match msg {
            ServerMsg::Artifacts { chat, artifacts } => {
                if *chat != self.chat {
                    self.chat.clone_from(chat);
                    self.files.clear();
                    self.order.clear();
                    self.versions.clear();
                    self.pane.switch_chat(chat.as_deref());
                    self.pending_export.clear();
                }
                self.list.clone_from(artifacts);
                let list = &self.list;
                self.pane.retain(|id| list.iter().any(|a| a.id == id));
            }
            ServerMsg::ArtifactUpdated(a) => {
                match self.list.iter_mut().find(|x| x.id == a.id) {
                    Some(x) => *x = a.clone(),
                    None => self.list.push(a.clone()),
                }
                self.reveal = Some(a.id.clone());
            }
            ServerMsg::ArtifactFile { id, version, data, error } => {
                let key = (id.clone(), *version);
                if !matches!(self.files.get(&key), Some(File::Requested)) {
                    return true;
                }
                let file = match (data, error) {
                    (Some(data), _) => match media::decode(data) {
                        Ok(bytes) => File::Ready { bytes, shown: None },
                        Err(e) => File::Failed(e),
                    },
                    (None, e) => File::Failed(e.clone().unwrap_or_else(|| "not available".into())),
                };
                self.files.insert(key.clone(), file);
                if let Some(i) = self.pending_export.iter().position(|k| *k == key) {
                    self.pending_export.remove(i);
                    self.export(&key.0, key.1);
                }
            }
            _ => return false,
        }
        true
    }

    /// Something is still being fetched (for scripted screenshots).
    pub fn loading(&self) -> bool {
        self.files.values().any(|f| matches!(f, File::Requested))
    }

    /// The pane is open (full screen on phones).
    pub fn is_open(&self) -> bool {
        self.pane.shown(self.narrow) == PaneShown::Open && self.selected_artifact().is_some()
    }

    /// Messages to send and notices to show, collected while drawing.
    pub fn take_output(&mut self) -> (Vec<ClientMsg>, Vec<(NoticeLevel, String)>) {
        for o in self.exports.poll() {
            match o {
                Ok(text) => self.notices.push((NoticeLevel::Info, text)),
                Err(text) => self.notices.push((NoticeLevel::Error, text)),
            }
        }
        (std::mem::take(&mut self.outbox), std::mem::take(&mut self.notices))
    }

    fn get(&self, id: &str) -> Option<&Artifact> {
        self.list.iter().find(|a| a.id == id)
    }

    fn selected_artifact(&self) -> Option<(Artifact, u32)> {
        self.tab_artifact(self.pane.active()?)
    }

    /// A tab's artifact and the version it shows.
    fn tab_artifact(&self, id: &str) -> Option<(Artifact, u32)> {
        let a = self.get(id)?;
        let n = self.versions.get(id).copied().filter(|n| a.version(*n).is_some()).or_else(|| a.latest().map(|l| l.n))?;
        Some((a.clone(), n))
    }

    /// Running / paused, for HTML apps the view runs.
    fn app_status(&self, a: &Artifact, version: u32) -> AppStatus {
        if a.kind == ArtifactKind::Html { self.view.status(&a.id, version) } else { AppStatus::Unknown }
    }

    /// The file of a version, asking for it the first time.
    fn file(&mut self, id: &str, version: u32) -> &mut File {
        let key = (id.to_owned(), version);
        if !self.files.contains_key(&key) {
            if self.order.len() >= MAX_FILES {
                let old = self.order.remove(0);
                self.files.remove(&old);
            }
            self.order.push(key.clone());
            self.outbox.push(ClientMsg::GetArtifact { id: id.to_owned(), version });
        }
        self.files.entry(key).or_insert(File::Requested)
    }

    /// The version's content, converted for display.
    fn shown(&mut self, ctx: &egui::Context, a: &Artifact, version: u32) -> Result<&mut Shown, Option<String>> {
        match self.file(&a.id, version) {
            File::Requested => Err(None),
            File::Failed(e) => Err(Some(e.clone())),
            File::Ready { bytes, shown } => match shown.get_or_insert_with(|| convert(ctx, a, version, bytes)) {
                Shown::Bad(e) => Err(Some(e.clone())),
                s => Ok(s),
            },
        }
    }

    /// The URL an HTML app version is served at, theme in the hash.
    fn app_url(&self, id: &str, version: u32) -> Result<String, String> {
        let chat = self.chat.as_deref().ok_or("this chat has no artifacts yet")?;
        let base = if cfg!(target_arch = "wasm32") {
            String::new()
        } else {
            match &self.link_base {
                Some(f) => f()?,
                None => return Err("opening artifacts isn't available here".into()),
            }
        };
        Ok(format!("{base}/artifact/{chat}/{id}/{version}{}", appview::theme_hash(&self.theme)))
    }

    fn export(&mut self, id: &str, version: u32) {
        let Some(a) = self.get(id).cloned() else { return };
        match self.files.get(&(id.to_owned(), version)) {
            Some(File::Ready { bytes, .. }) => {
                let name = if version == a.latest().map_or(version, |l| l.n) { a.filename.clone() } else { versioned_name(&a.filename, version) };
                self.exports.save(&self.ctx, &name, a.kind.mime(&a.filename), bytes.clone());
            }
            Some(File::Failed(e)) => self.notices.push((NoticeLevel::Error, format!("Couldn't export {}: {e}", a.filename))),
            Some(File::Requested) | None => {
                let _ = self.file(id, version);
                self.pending_export.push((id.to_owned(), version));
            }
        }
    }

    fn handle(&mut self, ctx: &egui::Context, action: Action) {
        match action {
            Action::Select { id, version } => {
                match version {
                    Some(n) => self.versions.insert(id.clone(), n),
                    None => self.versions.remove(&id),
                };
                self.pane.show(&id, self.narrow);
            }
            Action::Close => self.pane.collapse(self.narrow),
            Action::Expand => self.pane.expand(self.narrow),
            Action::Toggle => {
                let newest = self.list.last().map(|a| a.id.clone());
                self.pane.toggle(self.narrow, newest.as_deref());
            }
            Action::Activate(id) => {
                self.pane.activate(&id);
                self.pane.expand(self.narrow);
            }
            Action::CloseTab(id) => {
                self.pane.close(&id);
                self.versions.remove(&id);
            }
            Action::Minimize { id, minimized } => self.pane.set_minimized(&id, minimized),
            Action::MinimizeAll(minimized) => {
                let ids: Vec<String> = self.list.iter().map(|a| a.id.clone()).collect();
                self.pane.set_all_minimized(ids.iter().map(String::as_str), minimized);
            }
            Action::Export { id, version } => self.export(&id, version),
            Action::Open { id, version } => match self.app_url(&id, version) {
                Ok(url) => self.exports.open(ctx, &url),
                Err(e) => self.notices.push((NoticeLevel::Error, e)),
            },
            Action::SendError { id, version, error } => {
                let title = self.get(&id).map_or(id.clone(), |a| a.title.clone());
                let text = format!(
                    "The artifact \"{title}\" (id {id}, version {version}) shows this error when it runs:\n```\n{error}\n```\nPlease fix it with update_artifact."
                );
                self.outbox.push(ClientMsg::SendMessage { text, attachments: Vec::new() });
            }
        }
    }

    /// Per frame, before drawing: theme for apps, and a just-updated artifact to reveal.
    pub fn begin_frame(&mut self, ctx: &egui::Context, t: &Tokens, narrow: bool) {
        self.ctx.clone_from(ctx);
        self.narrow = narrow;
        self.drawn_app = None;
        let theme = appview::theme_vars(t);
        if theme != self.theme {
            self.view.set_theme(&theme);
            self.theme = theme;
        }
        if let Some(id) = self.reveal.take() {
            // A tab in front showing the latest; on a phone the pane would cover the chat
            // mid-reply, so it stays closed there (the floating button counts it).
            self.versions.remove(&id);
            self.pane.reveal(&id, narrow);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&PANE_SHORTCUT)) {
            self.handle(ctx, Action::Toggle);
        }
    }

    /// Per frame, after drawing: run what the user asked for, keep background tabs alive,
    /// sync the app overlay.
    pub fn end_frame(&mut self, ctx: &egui::Context) {
        for a in std::mem::take(&mut self.actions) {
            self.handle(ctx, a);
        }
        // Apps in tabs the pane didn't draw (behind another tab, or the rail) keep running
        // for a while without rendering.
        for id in self.pane.tabs().to_vec() {
            if self.drawn_app.as_deref() == Some(id.as_str()) {
                continue;
            }
            if let Some((a, v)) = self.tab_artifact(&id).filter(|(a, _)| a.kind == ArtifactKind::Html) {
                self.view.hold(&a.id, v);
            }
        }
        self.view.end_frame(ctx);
    }

    // ---- header, rail, floating button ---------------------------------------------------

    /// Header buttons (in a right-to-left layout): the pane toggle and "Collapse all".
    pub fn header_buttons(&mut self, ui: &mut egui::Ui, t: &Tokens) {
        if self.list.is_empty() {
            return;
        }
        if !self.narrow {
            let open = self.pane.shown(false) == PaneShown::Open;
            let label = format!("Artifacts {}", self.list.len());
            let shortcut = ui.ctx().format_shortcut(&PANE_SHORTCUT);
            if ui.selectable_label(open, label).on_hover_text(format!("Show or hide the artifact pane ({shortcut})")).clicked() {
                self.actions.push(Action::Toggle);
            }
        }
        let all = self.list.iter().all(|a| self.pane.is_minimized(&a.id));
        // Phones: short labels, the header is crowded there.
        let (label, tip) = match (all, self.narrow) {
            (true, false) => ("Expand all", "Show every artifact in this chat in full"),
            (true, true) => ("Expand", "Show every artifact in this chat in full"),
            (false, false) => ("Collapse all", "Minimize every artifact in this chat to one line"),
            (false, true) => ("Collapse", "Minimize every artifact in this chat to one line"),
        };
        if ui.button(RichText::new(label).color(t.text_dim)).on_hover_text(tip).clicked() {
            self.actions.push(Action::MinimizeAll(!all));
        }
    }

    /// The pane or its rail on the right (wide windows; call before the central panel).
    pub fn side_panel(&mut self, ui: &mut egui::Ui, t: &Tokens) {
        match self.pane.shown(false) {
            PaneShown::Open if self.selected_artifact().is_some() => {
                let width = ui.available_width();
                let max = (width * 0.75).max(320.0);
                let stored = self.pane.width();
                let r = egui::Panel::right("artifact-pane")
                    .resizable(true)
                    .default_size(stored.unwrap_or((width * t.knobs.pane_share).max(320.0)).min(max))
                    .size_range(crate::pane::MIN_WIDTH..=max)
                    .show(ui, |ui| self.pane(ui, t, false));
                // Remember what the user dragged it to (not what a smaller window squeezed it to).
                let w = r.response.rect.width();
                if ui.input(|i| i.pointer.any_down()) && stored.is_none_or(|s| (s - w).abs() > 0.5) {
                    self.pane.set_width(w);
                }
            }
            PaneShown::Rail if self.selected_artifact().is_some() => {
                egui::Panel::right("artifact-rail").resizable(false).exact_size(RAIL_WIDTH).show(ui, |ui| self.rail(ui, t));
            }
            _ => {}
        }
    }

    /// The collapsed pane: an icon per tab with a live/paused dot; click to open it there.
    fn rail(&mut self, ui: &mut egui::Ui, t: &Tokens) {
        ui.add_space(6.0);
        ui.vertical_centered(|ui| {
            let shortcut = ui.ctx().format_shortcut(&PANE_SHORTCUT);
            if ui.add(egui::Button::new(RichText::new("«").size(16.0)).frame(false)).on_hover_text(format!("Show the pane ({shortcut})")).clicked() {
                self.actions.push(Action::Expand);
            }
            ui.add_space(4.0);
            let active = self.pane.active().map(str::to_owned);
            for id in self.pane.tabs().to_vec() {
                let Some((a, v)) = self.tab_artifact(&id) else { continue };
                let status = self.app_status(&a, v);
                let resp = rail_icon(ui, t, &a, Some(&id) == active.as_ref(), status);
                let tip = match status.label() {
                    Some(s) => format!("{} · v{v} · {s}", a.title),
                    None => format!("{} · v{v}", a.title),
                };
                let resp = resp.on_hover_text(tip);
                if resp.clicked() {
                    self.actions.push(Action::Activate(id.clone()));
                }
                if resp.middle_clicked() {
                    self.actions.push(Action::CloseTab(id.clone()));
                }
                resp.context_menu(|ui| {
                    if ui.button("Close tab").clicked() {
                        self.actions.push(Action::CloseTab(id.clone()));
                    }
                });
                ui.add_space(4.0);
            }
        });
    }

    /// Phones, pane closed: a floating button with the number of open tabs.
    pub fn floating_button(&mut self, ctx: &egui::Context, t: &Tokens) {
        if self.pane.shown(true) != PaneShown::Rail {
            return;
        }
        let n = self.pane.tabs().len();
        egui::Area::new(egui::Id::unique("artifact-fab")).anchor(egui::Align2::RIGHT_BOTTOM, vec2(-14.0, -96.0)).order(egui::Order::Foreground).show(
            ctx,
            |ui| {
                let text = RichText::new(format!("Artifacts  {n}")).strong().color(t.on_accent);
                let b = egui::Button::new(text).fill(t.accent).corner_radius(t.round(18.0)).min_size(vec2(44.0, 36.0));
                if ui.add(b).on_hover_text("Show the open artifacts").clicked() {
                    self.actions.push(Action::Expand);
                }
            },
        );
    }

    // ---- card ---------------------------------------------------------------------------

    /// The card for artifact versions a step made.
    pub fn cards(&mut self, ui: &mut egui::Ui, t: &Tokens, refs: &[ArtifactRef]) {
        for r in refs {
            self.card(ui, t, r);
        }
    }

    fn card(&mut self, ui: &mut egui::Ui, t: &Tokens, r: &ArtifactRef) {
        let Some(a) = self.get(&r.id).cloned() else {
            ui.label(RichText::new("This artifact is no longer available.").small().color(t.text_faint));
            return;
        };
        let version = a.version(r.version).map_or(r.version, |v| v.n);
        let selected = self.is_open() && self.pane.active() == Some(a.id.as_str());
        if self.pane.is_minimized(&a.id) {
            self.chip(ui, t, &a, version, selected, true);
            return;
        }
        let note = a.version(version).map(|v| v.note.clone()).unwrap_or_default();
        let stroke = if selected { t.accent } else { t.border };
        egui::Frame::new().fill(t.code_bg).stroke(Stroke::new(1.0, stroke)).corner_radius(t.round(8.0)).inner_margin(vec2(t.pad(10.0), t.pad(8.0))).show(
            ui,
            |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if minimize_button(ui, t, a.kind).clicked() {
                            self.actions.push(Action::Minimize { id: a.id.clone(), minimized: true });
                        }
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            kind_badge(ui, t, a.kind);
                            ui.add(egui::Label::new(RichText::new(&a.title).strong().color(t.text_strong)).truncate());
                            ui.label(RichText::new(format!("v{version}")).small().color(t.text_dim));
                            if !note.is_empty() {
                                ui.add(egui::Label::new(RichText::new(&note).small().color(t.text_dim)).truncate()).on_hover_text(&note);
                            }
                        });
                    });
                });
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Show").on_hover_text("Show it beside the chat").clicked() {
                        self.actions.push(Action::Select { id: a.id.clone(), version: Some(version) });
                    }
                    if a.kind == ArtifactKind::Html && ui.button("Open").on_hover_text("Open the app in a browser tab").clicked() {
                        self.actions.push(Action::Open { id: a.id.clone(), version });
                    }
                    if ui.button("Export").on_hover_text(format!("Save {}", a.filename)).clicked() {
                        self.actions.push(Action::Export { id: a.id.clone(), version });
                    }
                    if a.versions.len() > 1 {
                        let mut pick = None;
                        egui::ComboBox::from_id_salt(("artifact-versions", &a.id, version)).selected_text(format!("{} versions", a.versions.len())).show_ui(
                            ui,
                            |ui| {
                                for v in a.versions.iter().rev() {
                                    if ui.selectable_label(v.n == version, version_label(v)).clicked() {
                                        pick = Some(v.n);
                                    }
                                }
                            },
                        );
                        if let Some(n) = pick {
                            self.actions.push(Action::Select { id: a.id.clone(), version: Some(n) });
                        }
                    }
                });
                self.card_preview(ui, t, &a, version);
            },
        );
    }

    /// A minimized artifact: one line with its kind, title, version and (apps) whether it
    /// still runs, and Restore. Keeps a running app alive in the background.
    fn chip(&mut self, ui: &mut egui::Ui, t: &Tokens, a: &Artifact, version: u32, selected: bool, in_chat: bool) {
        if a.kind == ArtifactKind::Html {
            self.view.hold(&a.id, version);
        }
        let status = self.app_status(a, version);
        let stroke = if selected { t.accent } else { t.border };
        egui::Frame::new().fill(t.code_bg).stroke(Stroke::new(1.0, stroke)).corner_radius(t.round(16.0)).inner_margin(vec2(t.pad(8.0), t.pad(3.0))).show(
            ui,
            |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Restore").on_hover_text("Show it in full again").clicked() {
                            self.actions.push(Action::Minimize { id: a.id.clone(), minimized: false });
                        }
                        if in_chat && ui.small_button("Show").on_hover_text("Show it beside the chat").clicked() {
                            self.actions.push(Action::Select { id: a.id.clone(), version: Some(version) });
                        }
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            kind_badge(ui, t, a.kind);
                            ui.add(egui::Label::new(RichText::new(&a.title).color(t.text_strong)).truncate()).on_hover_text(&a.filename);
                            ui.label(RichText::new(format!("v{version}")).small().color(t.text_dim));
                            if let Some(label) = status.label() {
                                status_dot(ui, t, status);
                                ui.label(RichText::new(label).small().color(t.text_dim));
                            }
                        });
                    });
                });
            },
        );
    }

    fn card_preview(&mut self, ui: &mut egui::Ui, t: &Tokens, a: &Artifact, version: u32) {
        if a.kind == ArtifactKind::Html {
            let size = a.version(version).map_or(0, |v| v.size);
            if self.view.live_cards() {
                let source = match self.file(&a.id, version) {
                    File::Ready { bytes, .. } => Some(String::from_utf8_lossy(bytes).into_owned()),
                    _ => None,
                };
                if let Some(source) = source {
                    ui.add_space(4.0);
                    let doc = AppDoc { id: &a.id, url: None, title: &a.title, version, size, source: Some(&source) };
                    if let Some(resp) = self.view.card(ui, t, &doc) {
                        if resp.open {
                            self.actions.push(Action::Open { id: a.id.clone(), version });
                        }
                        return;
                    }
                }
            }
            ui.label(RichText::new(format!("Interactive app · {}", media::size_label(size))).small().color(t.text_dim));
            return;
        }
        let ctx = ui.ctx().clone();
        let key = egui::Id::unique(("artifact-card", &a.id, version));
        match self.shown(&ctx, a, version) {
            Err(None) => {
                crate::theme::spinner(ui, t, Some(12.0));
            }
            Err(Some(e)) => {
                ui.label(RichText::new(format!("Couldn't show it: {e}")).small().color(t.text_dim));
            }
            Ok(Shown::Bad(_)) => {}
            Ok(Shown::Chart(spec)) => {
                ui.add_space(4.0);
                crate::chart::show(ui, t, spec, key);
            }
            Ok(Shown::Image(tex)) => image(ui, t, tex, CARD_PREVIEW_HEIGHT),
            Ok(Shown::Text { text, .. }) => {
                let markdown = a.kind == ArtifactKind::Markdown;
                for line in text.lines().filter(|l| !l.trim().is_empty()).take(CARD_LINES) {
                    // Markdown reads better without its markers.
                    let line = if markdown { line.trim_start_matches(['#', '>', ' ']) } else { line };
                    let mut rt = RichText::new(line).small().color(t.text_dim);
                    if !markdown {
                        rt = rt.monospace();
                    }
                    ui.add(egui::Label::new(rt).truncate());
                }
            }
        }
    }

    // ---- pane ---------------------------------------------------------------------------

    /// The pane's contents (the caller decides where it goes).
    pub fn pane(&mut self, ui: &mut egui::Ui, t: &Tokens, narrow: bool) {
        self.tab_strip(ui, t, narrow);
        let Some((a, version)) = self.selected_artifact() else {
            return;
        };
        let minimized = self.pane.is_minimized(&a.id);
        self.pane_header(ui, t, &a, version, minimized);
        ui.separator();
        if minimized {
            ui.add_space(6.0);
            self.chip(ui, t, &a, version, false, false);
            ui.add_space(6.0);
            let hint = if a.kind == ArtifactKind::Html {
                "Minimized: the app keeps running in the background for a minute, then pauses (a snapshot is kept; restoring it after that starts it afresh)."
            } else {
                "Minimized. Restore shows it in full here and in the chat."
            };
            ui.label(RichText::new(hint).small().color(t.text_faint));
            return;
        }
        let ctx = ui.ctx().clone();
        let id = egui::Id::unique(("artifact-pane", &a.id, version));
        if a.kind == ArtifactKind::Html {
            self.app(ui, t, &a, version);
            return;
        }
        let source = self.source;
        match self.shown(&ctx, &a, version) {
            Err(None) => {
                ui.centered_and_justified(|ui| crate::theme::spinner(ui, t, None));
            }
            Err(Some(e)) => {
                ui.label(RichText::new(format!("Couldn't show {}: {e}", a.filename)).color(t.text_dim));
            }
            Ok(Shown::Bad(_)) => {}
            Ok(Shown::Chart(spec)) => {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| crate::chart::show(ui, t, spec, id));
            }
            Ok(Shown::Image(tex)) => {
                egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| image(ui, t, tex, f32::INFINITY));
            }
            Ok(Shown::Text { text, doc }) => match doc {
                Some(doc) if !source => {
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| markdown::show(ui, t, doc, id));
                }
                _ => {
                    egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                        let mut view = text.as_str();
                        ui.add(egui::TextEdit::multiline(&mut view).code_editor().desired_width(f32::INFINITY));
                    });
                }
            },
        }
    }

    /// Back (phones) · the tabs … collapse to the rail.
    fn tab_strip(&mut self, ui: &mut egui::Ui, t: &Tokens, narrow: bool) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if narrow && ui.button("← Chat").clicked() {
                self.actions.push(Action::Close);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !narrow {
                    let shortcut = ui.ctx().format_shortcut(&PANE_SHORTCUT);
                    if ui
                        .add(egui::Button::new(RichText::new("»").size(16.0)).frame(false))
                        .on_hover_text(format!("Collapse to the rail ({shortcut})"))
                        .clicked()
                    {
                        self.actions.push(Action::Close);
                    }
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    egui::ScrollArea::horizontal().id_salt("artifact-tabs").auto_shrink([false, true]).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let active = self.pane.active().map(str::to_owned);
                            for id in self.pane.tabs().to_vec() {
                                let Some((a, v)) = self.tab_artifact(&id) else { continue };
                                let status = self.app_status(&a, v);
                                self.tab(ui, t, &a, Some(&id) == active.as_ref(), status);
                            }
                        });
                    });
                });
            });
        });
    }

    fn tab(&mut self, ui: &mut egui::Ui, t: &Tokens, a: &Artifact, active: bool, status: AppStatus) {
        let fill = if active { t.assistant_bubble } else { t.code_bg };
        let stroke = if active { t.accent } else { t.border };
        let title: String =
            if a.title.chars().count() > TAB_CHARS { a.title.chars().take(TAB_CHARS - 1).chain(std::iter::once('…')).collect() } else { a.title.clone() };
        egui::Frame::new().fill(fill).stroke(Stroke::new(1.0, stroke)).corner_radius(t.round(6.0)).inner_margin(vec2(t.pad(6.0), t.pad(2.0))).show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            if status.label().is_some() {
                status_dot(ui, t, status);
            }
            let color = if active { t.text_strong } else { t.text_dim };
            let resp = ui.add(egui::Label::new(RichText::new(title).color(color)).sense(egui::Sense::click())).on_hover_text(&a.title);
            if resp.clicked() {
                self.actions.push(Action::Activate(a.id.clone()));
            }
            let close =
                ui.add(egui::Button::new(RichText::new("×").color(t.text_dim)).frame(false).small()).on_hover_text("Close the tab (the artifact stays)");
            if close.clicked() || resp.middle_clicked() {
                self.actions.push(Action::CloseTab(a.id.clone()));
            }
        });
    }

    fn pane_header(&mut self, ui: &mut egui::Ui, t: &Tokens, a: &Artifact, version: u32, minimized: bool) {
        // Title row: kind · title … minimize.
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !minimized && minimize_button(ui, t, a.kind).clicked() {
                    self.actions.push(Action::Minimize { id: a.id.clone(), minimized: true });
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    kind_badge(ui, t, a.kind);
                    ui.add(egui::Label::new(RichText::new(&a.title).strong().size(t.text_size(15.0)).color(t.text_strong)).truncate())
                        .on_hover_text(&a.filename);
                });
            });
        });
        if minimized {
            return;
        }
        // Tool row: versions, view switch, actions.
        ui.horizontal_wrapped(|ui| {
            let current = a.version(version).map(version_label).unwrap_or_default();
            let mut pick = None;
            egui::ComboBox::from_id_salt(("pane-versions", &a.id)).selected_text(current).truncate().width(200.0).show_ui(ui, |ui| {
                for v in a.versions.iter().rev() {
                    if ui.selectable_label(v.n == version, version_label(v)).clicked() {
                        pick = Some(v.n);
                    }
                }
            });
            if let Some(n) = pick {
                self.actions.push(Action::Select { id: a.id.clone(), version: Some(n) });
            }
            if a.kind == ArtifactKind::Markdown {
                ui.selectable_value(&mut self.source, false, "Preview");
                ui.selectable_value(&mut self.source, true, "Source");
            }
            if a.kind == ArtifactKind::Html && ui.button("Open").on_hover_text("Open in a browser tab").clicked() {
                self.actions.push(Action::Open { id: a.id.clone(), version });
            }
            if ui.button("Export").on_hover_text(format!("Save {}", a.filename)).clicked() {
                self.actions.push(Action::Export { id: a.id.clone(), version });
            }
            if a.kind.is_text()
                && let Some(File::Ready { bytes, .. }) = self.files.get(&(a.id.clone(), version))
                && ui.button("Copy").on_hover_text("Copy the file's text").clicked()
            {
                ui.ctx().copy_text(String::from_utf8_lossy(bytes).into_owned());
            }
        });
    }

    fn app(&mut self, ui: &mut egui::Ui, t: &Tokens, a: &Artifact, version: u32) {
        // The iframe loads the app by URL; the desktop renderer and the placeholder take the source.
        let url = if self.view.wants_url() { self.app_url(&a.id, version).ok() } else { None };
        let source = if self.view.wants_source() {
            match self.file(&a.id, version) {
                File::Ready { bytes, .. } => Some(String::from_utf8_lossy(bytes).into_owned()),
                _ => None,
            }
        } else {
            None
        };
        let errors = self.view.errors();
        if !errors.is_empty() {
            egui::Frame::new().fill(t.code_bg).stroke(Stroke::new(1.0, t.danger)).corner_radius(t.round(6.0)).inner_margin(t.pad(8.0)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new(if errors.len() == 1 { "The app reported an error".to_owned() } else { format!("The app reported {} errors", errors.len()) })
                        .color(t.danger),
                );
                for e in errors.iter().take(3) {
                    ui.add(egui::Label::new(RichText::new(e).small().monospace().color(t.text)).truncate());
                }
                if ui.button("Send error to yah").on_hover_text("Ask yah to fix it").clicked() {
                    self.actions.push(Action::SendError { id: a.id.clone(), version, error: errors.join("\n") });
                }
            });
            ui.add_space(4.0);
        }
        let size = a.version(version).map_or(0, |v| v.size);
        let doc = AppDoc { id: &a.id, url: url.as_deref(), title: &a.title, version, size, source: source.as_deref() };
        self.drawn_app = Some(a.id.clone());
        if self.view.show(ui, t, &doc).open {
            self.actions.push(Action::Open { id: a.id.clone(), version });
        }
    }
}

/// `loan.html`, version 2 → `loan v2.html`.
fn versioned_name(filename: &str, version: u32) -> String {
    let lower = filename.to_ascii_lowercase();
    let split =
        if lower.ends_with(".chart.json") { filename.len().saturating_sub(11) } else { filename.rfind('.').filter(|i| *i > 0).unwrap_or(filename.len()) };
    let (stem, ext) = (filename.get(..split).unwrap_or(filename), filename.get(split..).unwrap_or(""));
    format!("{stem} v{version}{ext}")
}

fn version_label(v: &yah_protocol::ArtifactVersion) -> String {
    let note: String = v.note.chars().take(40).collect();
    if note.is_empty() { format!("v{}", v.n) } else { format!("v{} · {note}", v.n) }
}

/// The "–" that minimizes a card or the pane's artifact.
fn minimize_button(ui: &mut egui::Ui, t: &Tokens, kind: ArtifactKind) -> egui::Response {
    let tip = if kind == ArtifactKind::Html { "Minimize (it keeps running for a minute, then pauses)" } else { "Minimize to one line" };
    ui.add(egui::Button::new(RichText::new("–").strong().color(t.text_dim)).frame(false)).on_hover_text(tip)
}

fn status_color(t: &Tokens, status: AppStatus) -> Option<egui::Color32> {
    match status {
        AppStatus::Unknown => None,
        AppStatus::Running => Some(t.success),
        AppStatus::Paused => Some(t.text_faint),
        AppStatus::Stopped => Some(t.danger),
    }
}

/// A small live (green) / paused (grey) / stopped (red) dot.
fn status_dot(ui: &mut egui::Ui, t: &Tokens, status: AppStatus) {
    let (rect, _) = ui.allocate_exact_size(vec2(8.0, 8.0), egui::Sense::hover());
    if let Some(c) = status_color(t, status) {
        ui.painter().circle_filled(rect.center(), 3.5, c);
    }
}

/// Up to two letters standing for a title ("Loan calculator" → "LC").
fn initials(title: &str) -> String {
    let words: Vec<&str> = title.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let first = |w: &str| w.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default();
    match words.as_slice() {
        [] => "?".into(),
        [one] => one.chars().take(2).collect::<String>().to_uppercase(),
        [a, b, ..] => format!("{}{}", first(a), first(b)),
    }
}

/// One artifact on the rail: its initials on a rounded square, a status dot in the corner.
fn rail_icon(ui: &mut egui::Ui, t: &Tokens, a: &Artifact, active: bool, status: AppStatus) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(RAIL_ICON, RAIL_ICON), egui::Sense::click());
    let p = ui.painter();
    let fill = if active {
        crate::theme::mix(t.accent, t.code_bg, 0.75)
    } else if resp.hovered() {
        t.assistant_bubble
    } else {
        t.code_bg
    };
    p.rect_filled(rect, 8.0, fill);
    p.rect_stroke(rect, 8.0, Stroke::new(1.0, if active { t.accent } else { t.border }), egui::StrokeKind::Inside);
    p.text(rect.center(), egui::Align2::CENTER_CENTER, initials(&a.title), egui::FontId::proportional(12.0), if active { t.text_strong } else { t.text });
    if let Some(c) = status_color(t, status) {
        let at = rect.right_bottom() - vec2(5.0, 5.0);
        p.circle_filled(at, 4.5, t.panel_bg);
        p.circle_filled(at, 3.2, c);
    }
    resp
}

fn kind_badge(ui: &mut egui::Ui, t: &Tokens, kind: ArtifactKind) {
    egui::Frame::new().fill(crate::theme::mix(t.accent, t.code_bg, 0.8)).corner_radius(t.round(4.0)).inner_margin(vec2(t.pad(5.0), t.pad(1.0))).show(
        ui,
        |ui| {
            ui.label(RichText::new(kind.label().to_uppercase()).small().color(t.accent));
        },
    );
}

fn image(ui: &mut egui::Ui, t: &Tokens, tex: &egui::TextureHandle, max_height: f32) {
    let natural = tex.size_vec2();
    if natural.x <= 0.0 || natural.y <= 0.0 {
        return;
    }
    let scale = (ui.available_width() / natural.x).min(max_height / natural.y).min(1.0);
    ui.add(egui::Image::from_texture(egui::load::SizedTexture::new(tex.id(), natural * scale)).corner_radius(t.round(6.0)));
    let _ = t;
}

fn convert(ctx: &egui::Context, a: &Artifact, version: u32, bytes: &[u8]) -> Shown {
    match a.kind {
        ArtifactKind::Image => match crate::media::decode_image(bytes) {
            Ok(img) => Shown::Image(ctx.load_texture(format!("artifact-{}-{version}", a.id), img, egui::TextureOptions::LINEAR)),
            Err(e) => Shown::Bad(e),
        },
        ArtifactKind::Chart => match ChartSpec::from_json(&String::from_utf8_lossy(bytes)) {
            Ok(spec) => Shown::Chart(Box::new(spec)),
            Err(e) => Shown::Bad(e),
        },
        ArtifactKind::Markdown => {
            let text = String::from_utf8_lossy(bytes).into_owned();
            let mut doc = markdown::Doc::default();
            doc.update(&text, true);
            Shown::Text { text, doc: Some(Box::new(doc)) }
        }
        ArtifactKind::Html | ArtifactKind::Text => Shown::Text { text: String::from_utf8_lossy(bytes).into_owned(), doc: None },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yah_protocol::ArtifactVersion;

    fn artifact(id: &str, kind: ArtifactKind, filename: &str, versions: &[u32]) -> Artifact {
        let versions = versions
            .iter()
            .map(|n| ArtifactVersion { n: *n, path: format!(".yah/artifacts/{id}/v{n}"), created_at: 0, note: format!("note {n}"), size: 3 })
            .collect();
        Artifact { id: id.into(), kind, title: id.to_uppercase(), filename: filename.into(), versions }
    }

    fn frame(ctx: &egui::Context, f: impl FnMut(&mut egui::Ui)) {
        let mut out = ctx.run_ui(egui::RawInput::default(), f);
        out.textures_delta.clear();
    }

    #[test]
    fn list_updates_fetch_once_and_reveal() {
        let mut s = Artifacts::default();
        let md = artifact("notes", ArtifactKind::Markdown, "notes.md", &[1]);
        assert!(s.apply(&ServerMsg::Artifacts { chat: Some("c1".into()), artifacts: vec![md.clone()] }));
        assert!(!s.apply(&ServerMsg::Notice { level: NoticeLevel::Info, text: String::new() }));
        let ctx = egui::Context::default();
        let t = Tokens::dark();
        for _ in 0..2 {
            frame(&ctx, |ui| s.cards(ui, &t, &[ArtifactRef { id: "notes".into(), version: 1 }]));
        }
        let (out, _) = s.take_output();
        assert_eq!(out, vec![ClientMsg::GetArtifact { id: "notes".into(), version: 1 }]);
        assert!(s.loading());
        s.apply(&ServerMsg::ArtifactFile { id: "notes".into(), version: 1, data: Some(media::encode(b"# Hi\n\ntext")), error: None });
        assert!(!s.loading());
        frame(&ctx, |ui| s.cards(ui, &t, &[ArtifactRef { id: "notes".into(), version: 1 }]));
        // An update reveals the artifact in the pane (wide windows only).
        let v2 = artifact("notes", ArtifactKind::Markdown, "notes.md", &[1, 2]);
        s.apply(&ServerMsg::ArtifactUpdated(v2));
        s.begin_frame(&ctx, &t, true);
        assert!(!s.is_open(), "not on a phone");
        s.apply(&ServerMsg::ArtifactUpdated(artifact("notes", ArtifactKind::Markdown, "notes.md", &[1, 2])));
        s.begin_frame(&ctx, &t, false);
        assert!(s.is_open());
        assert_eq!(s.selected_artifact().map(|(a, v)| (a.id, v)), Some(("notes".into(), 2)));
        frame(&ctx, |ui| s.pane(ui, &t, false));
        assert_eq!(s.take_output().0, vec![ClientMsg::GetArtifact { id: "notes".into(), version: 2 }]);
        // Another chat starts over.
        s.apply(&ServerMsg::Artifacts { chat: Some("c2".into()), artifacts: vec![] });
        assert!(!s.is_open() && s.files.is_empty());
    }

    #[test]
    fn unasked_or_failed_files_are_handled() {
        let mut s = Artifacts::default();
        s.apply(&ServerMsg::Artifacts { chat: Some("c".into()), artifacts: vec![artifact("p", ArtifactKind::Image, "p.png", &[1])] });
        // A file nobody asked for is ignored.
        s.apply(&ServerMsg::ArtifactFile { id: "p".into(), version: 1, data: Some(media::encode(b"x")), error: None });
        assert!(s.files.is_empty());
        let _ = s.file("p", 1);
        s.apply(&ServerMsg::ArtifactFile { id: "p".into(), version: 1, data: Some("!!".into()), error: None });
        assert!(matches!(s.files.get(&("p".into(), 1)), Some(File::Failed(_))));
        let ctx = egui::Context::default();
        let t = Tokens::light();
        s.handle(&ctx, Action::Select { id: "p".into(), version: None });
        frame(&ctx, |ui| s.pane(ui, &t, true));
        // Export of a failed file is reported, not attempted.
        s.handle(&ctx, Action::Export { id: "p".into(), version: 1 });
        let (_, notices) = s.take_output();
        assert!(notices.iter().any(|(l, n)| *l == NoticeLevel::Error && n.contains("p.png")), "{notices:?}");
    }

    #[test]
    fn export_waits_for_the_file_and_errors_become_messages() {
        let mut s = Artifacts::default();
        s.apply(&ServerMsg::Artifacts { chat: Some("c".into()), artifacts: vec![artifact("app", ArtifactKind::Html, "app.html", &[1, 2])] });
        let ctx = egui::Context::default();
        s.handle(&ctx, Action::Export { id: "app".into(), version: 1 });
        assert_eq!(s.pending_export, vec![("app".to_owned(), 1)]);
        assert_eq!(s.take_output().0, vec![ClientMsg::GetArtifact { id: "app".into(), version: 1 }]);
        s.handle(&ctx, Action::SendError { id: "app".into(), version: 2, error: "x is not defined".into() });
        let (out, _) = s.take_output();
        let [ClientMsg::SendMessage { text, .. }] = &out[..] else { panic!("{out:?}") };
        assert!(text.contains("\"APP\" (id app, version 2)") && text.contains("x is not defined") && text.contains("update_artifact"), "{text}");
        // Without a server to link to, Open says so instead of failing silently.
        if cfg!(not(target_arch = "wasm32")) {
            s.handle(&ctx, Action::Open { id: "app".into(), version: 2 });
            assert!(s.take_output().1.iter().any(|(l, _)| *l == NoticeLevel::Error));
            s.set_link_base(Box::new(|| Ok("http://127.0.0.1:9".into())));
            s.begin_frame(&ctx, &Tokens::dark(), false);
            assert!(s.app_url("app", 2).unwrap().starts_with("http://127.0.0.1:9/artifact/c/app/2#--yah-bg="));
        }
    }

    /// Records what the artifact UI asks of the app view.
    #[derive(Default)]
    struct Recorder {
        log: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
    }

    impl AppView for Recorder {
        fn show(&mut self, _ui: &mut egui::Ui, _t: &Tokens, doc: &AppDoc<'_>) -> appview::AppViewResponse {
            self.log.borrow_mut().push(format!("show {}", doc.id));
            appview::AppViewResponse::default()
        }
        fn runs_inline(&self) -> bool {
            true
        }
        fn hold(&mut self, id: &str, version: u32) {
            self.log.borrow_mut().push(format!("hold {id}/{version}"));
        }
        fn status(&self, _id: &str, _version: u32) -> AppStatus {
            AppStatus::Running
        }
    }

    fn drain(log: &std::rc::Rc<std::cell::RefCell<Vec<String>>>) -> Vec<String> {
        let mut v = std::mem::take(&mut *log.borrow_mut());
        v.sort();
        v.dedup();
        v
    }

    #[test]
    fn tabs_rail_minimize_and_background_apps() {
        let mut s = Artifacts::default();
        let rec = Recorder::default();
        let log = std::rc::Rc::clone(&rec.log);
        s.set_app_view(Box::new(rec));
        let list = vec![
            artifact("calc", ArtifactKind::Html, "calc.html", &[1]),
            artifact("clock", ArtifactKind::Html, "clock.html", &[1, 2]),
            artifact("notes", ArtifactKind::Markdown, "notes.md", &[1]),
        ];
        s.apply(&ServerMsg::Artifacts { chat: Some("c".into()), artifacts: list });
        let ctx = egui::Context::default();
        let t = Tokens::dark();
        let refs = [ArtifactRef { id: "calc".into(), version: 1 }, ArtifactRef { id: "clock".into(), version: 2 }];
        let run = |s: &mut Artifacts, input: egui::RawInput| {
            let mut out = ctx.run_ui(input, |ui| {
                s.begin_frame(ui.ctx(), &t, false);
                s.side_panel(ui, &t);
                egui::CentralPanel::default().show(ui, |ui| s.cards(ui, &t, &refs));
            });
            out.textures_delta.clear();
            s.end_frame(&ctx);
        };
        // Two apps opened: tabs, the one in front drawn, the other held in the background.
        s.handle(&ctx, Action::Select { id: "calc".into(), version: None });
        s.handle(&ctx, Action::Select { id: "clock".into(), version: Some(2) });
        for _ in 0..2 {
            run(&mut s, egui::RawInput::default());
        }
        assert_eq!(s.pane.tabs(), ["calc", "clock"]);
        assert!(s.is_open());
        let l = drain(&log);
        assert!(l.contains(&"show clock".to_owned()) && l.contains(&"hold calc/1".to_owned()), "{l:?}");
        // Minimizing a card: a chip that keeps the app alive in the background.
        s.handle(&ctx, Action::Minimize { id: "clock".into(), minimized: true });
        run(&mut s, egui::RawInput::default());
        let l = drain(&log);
        assert!(!l.iter().any(|e| e.starts_with("show")), "a minimized artifact isn't drawn: {l:?}");
        assert!(l.contains(&"hold clock/2".to_owned()), "{l:?}");
        // Collapse to the rail with the shortcut: every app tab held.
        s.handle(&ctx, Action::Minimize { id: "clock".into(), minimized: false });
        let key = egui::Event::Key { key: egui::Key::Backslash, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::COMMAND };
        run(&mut s, egui::RawInput { events: vec![key], ..Default::default() });
        assert!(!s.is_open() && s.pane.collapsed());
        run(&mut s, egui::RawInput::default());
        assert_eq!(drain(&log), ["hold calc/1", "hold clock/2"]);
        // Clicking a rail icon (as its action) opens the pane on it.
        s.handle(&ctx, Action::Activate("calc".into()));
        assert!(s.is_open() && s.pane.active() == Some("calc"));
        // Closing a tab keeps the artifact.
        s.handle(&ctx, Action::CloseTab("calc".into()));
        assert_eq!((s.pane.tabs(), s.pane.active()), (&["clock".to_owned()][..], Some("clock")));
        assert!(s.get("calc").is_some());
        // Collapse all: every artifact of the chat minimized, kept in the pane prefs.
        s.handle(&ctx, Action::MinimizeAll(true));
        let prefs = s.pane_prefs();
        assert_eq!(prefs.chats[0].minimized, ["calc", "clock", "notes"]);
        run(&mut s, egui::RawInput::default());
        // Restored on a fresh start, for this chat.
        let mut again = Artifacts::default();
        again.set_pane_prefs(prefs);
        again.apply(&ServerMsg::Artifacts { chat: Some("c".into()), artifacts: s.list.clone() });
        assert!(again.pane.is_minimized("notes") && again.pane.tabs() == ["clock"]);
        // Toggle with nothing open opens the newest artifact.
        let mut fresh = Artifacts::default();
        fresh.apply(&ServerMsg::Artifacts { chat: Some("d".into()), artifacts: s.list.clone() });
        fresh.handle(&ctx, Action::Toggle);
        assert_eq!(fresh.pane.active(), Some("notes"));
    }

    #[test]
    fn names_for_older_versions() {
        assert_eq!(versioned_name("loan.html", 2), "loan v2.html");
        assert_eq!(versioned_name("Sales.chart.json", 3), "Sales v3.chart.json");
        assert_eq!(versioned_name("noext", 1), "noext v1");
    }
}
