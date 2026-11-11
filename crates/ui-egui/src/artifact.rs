//! Artifacts in the UI: a card where a step made or changed one (title, type, version,
//! Show / Export, a versions menu), and the artifact pane beside the conversation (full
//! screen on narrow windows) showing the selected one large, with Copy for text and a
//! Preview/Source switch for types that render their source.
//!
//! What is drawn inside the card and the pane (and extra buttons such as an app's Open)
//! comes from the artifact's renderer, picked by its content type in the
//! [`ArtifactRenderers`] registry (see [`crate::renderers`]): markdown, charts, images, HTML
//! apps (through the [`AppView`] web surface) and the text-source fallback are registered
//! there like anyone else's.
//!
//! The pane has tabs (closing one keeps the artifact) and collapses to a rail of icons on
//! the right (a floating button with a count on phones); Ctrl+Backslash (Cmd on macOS) toggles it.
//! On wide windows it can be maximized over the chat and the chat list (Ctrl+Shift+F, Esc or
//! Restore to go back; per session, and the running app keeps its state). Any
//! artifact can be minimized to a one-line chip, in the chat and the pane; "Collapse all"
//! in the header does that for the whole chat. Minimized apps, background tabs and apps
//! behind the rail (anything on the web surface) are [held](AppView::hold): they keep running without rendering for a
//! while, then pause. What is open and minimized is kept per chat ([`crate::pane`]).
//!
//! The engine sends the chat's artifacts with the transcript and an update whenever a
//! version is added; version bytes are fetched once with `GetArtifact` and kept here (they
//! are also what Export saves).

use std::collections::HashMap;

use byob_protocol::media;
use byob_protocol::{Artifact, ArtifactRef, ClientMsg, NoticeLevel, ServerMsg};
use egui::{RichText, Stroke, vec2};

use crate::appview::{self, AppStatus, AppView};
use crate::export::Exports;
use crate::pane::{Pane, PanePrefs, Shown as PaneShown};
use crate::renderers::{ArtifactCx, ArtifactRenderer, ArtifactRenderers, File, Files, Links, Place, RendererCaps, Request};
use crate::theme::Tokens;
/// Width of the collapsed pane's rail.
const RAIL_WIDTH: f32 = 46.0;
/// Size of a rail icon.
const RAIL_ICON: f32 = 32.0;
/// Characters of a title shown on a tab.
const TAB_CHARS: usize = 22;
/// Toggles the pane.
pub const PANE_SHORTCUT: egui::KeyboardShortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::Backslash);
/// Maximizes the pane or restores it.
pub const MAXIMIZE_SHORTCUT: egui::KeyboardShortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT), egui::Key::F);

/// Produces the base URL (`http://127.0.0.1:PORT`) the desktop app serves artifacts at,
/// starting a server if needed. The browser client uses its own origin.
pub type LinkBase = Box<dyn Fn() -> Result<String, String>>;

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
    /// Send the model a message (a renderer asked, e.g. an app's "Send error to byob").
    Send(String),
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
    /// Maximize the pane over the chat, or restore it.
    Maximize(bool),
}

pub struct Artifacts {
    chat: Option<String>,
    list: Vec<Artifact>,
    files: Files,
    /// Content type → renderer.
    renderers: ArtifactRenderers,
    /// What renderers asked for this frame.
    requests: Vec<Request>,
    /// Tabs, minimized artifacts, open/collapsed.
    pane: Pane,
    /// A version picked for a tab (else the latest).
    versions: HashMap<String, u32>,
    /// This frame is narrow (phones).
    narrow: bool,
    /// The web-surface artifact the pane drew this frame (the other tabs are held in the
    /// background).
    drawn_app: Option<String>,
    /// Markdown shows its source instead of the preview.
    source: bool,
    /// Open the pane on this artifact at the next frame (if there is room).
    reveal: Option<String>,
    /// Exports waiting for their file.
    pending_export: Vec<(String, u32)>,
    pub(crate) web: crate::web::Web,
    /// Plugins' live views drawing artifacts (`view:` renderers).
    views: crate::renderers::view::ArtifactViews,
    exports: Exports,
    link_base: Option<LinkBase>,
    theme: Vec<(&'static str, String)>,
    outbox: Vec<ClientMsg>,
    notices: Vec<(NoticeLevel, String)>,
    actions: Vec<Action>,
    /// For repaints when an export or open finishes.
    ctx: egui::Context,
    /// A widget (an app, a text field) had keyboard focus at the end of the last frame: Esc
    /// is its to take (egui drops the focus on Esc before we see the key).
    focus_held: bool,
}

impl Default for Artifacts {
    fn default() -> Self {
        Self {
            chat: None,
            list: Vec::new(),
            files: Files::default(),
            renderers: ArtifactRenderers::default(),
            requests: Vec::new(),
            pane: Pane::default(),
            versions: HashMap::new(),
            narrow: false,
            drawn_app: None,
            source: false,
            reveal: None,
            pending_export: Vec::new(),
            web: crate::web::Web::new(appview::default_view()),
            views: Default::default(),
            exports: Exports::default(),
            link_base: None,
            theme: Vec::new(),
            outbox: Vec::new(),
            notices: Vec::new(),
            actions: Vec::new(),
            ctx: egui::Context::default(),
            focus_held: false,
        }
    }
}

impl Artifacts {
    pub fn set_app_view(&mut self, view: Box<dyn AppView>) {
        self.web.view = view;
    }

    pub fn set_link_base(&mut self, base: LinkBase) {
        self.link_base = Some(base);
    }

    /// Register renderers for more content types (or replace byob's own).
    pub fn renderers_mut(&mut self) -> &mut ArtifactRenderers {
        &mut self.renderers
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

    /// Maximize the pane or restore it, as the shortcut does.
    pub fn toggle_maximized(&mut self) {
        self.actions.push(Action::Maximize(!self.pane.maximized(self.narrow)));
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

    /// Bring the tab after the one in front to the front (scripted runs).
    pub fn next_tab(&mut self) {
        let tabs = self.pane.tabs();
        let next = tabs.iter().position(|t| Some(t.as_str()) == self.pane.active()).map_or(0, |i| i.saturating_add(1)) % tabs.len().max(1);
        if let Some(id) = tabs.get(next).cloned() {
            self.actions.push(Action::Activate(id));
        }
    }

    /// Apply a server message; returns whether it was one of ours.
    pub fn apply(&mut self, msg: &ServerMsg) -> bool {
        if self.web.apply(msg) {
            return true;
        }
        match msg {
            ServerMsg::Artifacts { chat, artifacts } => {
                if *chat != self.chat {
                    self.chat.clone_from(chat);
                    self.files.clear();
                    self.views.clear();
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
                    Some(x) => {
                        // A version saved again (a draft the user keeps editing): fetch it anew.
                        for v in &a.versions {
                            if x.version(v.n).is_some_and(|old| old.size != v.size || old.created_at != v.created_at) {
                                let key = (a.id.clone(), v.n);
                                self.files.map.remove(&key);
                                self.files.order.retain(|k| *k != key);
                            }
                        }
                        *x = a.clone();
                    }
                    None => self.list.push(a.clone()),
                }
                self.reveal = Some(a.id.clone());
            }
            ServerMsg::LiveView(lv) => return self.views.live(lv),
            ServerMsg::ArtifactFile { id, version, data, error } => {
                let key = (id.clone(), *version);
                if !matches!(self.files.map.get(&key), Some(File::Requested)) {
                    return true;
                }
                let file = match (data, error) {
                    (Some(data), _) => match media::decode(data) {
                        Ok(bytes) => File::Ready { bytes, prepared: None },
                        Err(e) => File::Failed(e),
                    },
                    (None, e) => File::Failed(e.clone().unwrap_or_else(|| "not available".into())),
                };
                self.files.map.insert(key.clone(), file);
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
        self.files.map.values().any(|f| matches!(f, File::Requested)) || self.views.loading()
    }

    /// Scripted runs: show the newest artifact whose id, title or file name starts with
    /// `prefix` (case-insensitive) in the pane.
    pub fn show_matching(&mut self, prefix: &str) -> bool {
        let p = prefix.to_lowercase();
        let found = self.list.iter().rev().find(|a| [&a.id, &a.title, &a.filename].iter().any(|s| s.to_lowercase().starts_with(&p))).map(|a| a.id.clone());
        match found {
            Some(id) => {
                self.actions.push(Action::Select { id, version: None });
                true
            }
            None => false,
        }
    }

    /// The engine handles live views (artifacts drawn by plugins' views need it).
    pub fn set_views_enabled(&mut self, on: bool) {
        self.views.set_enabled(on);
    }

    /// The chat in view has no artifacts.
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// The pane is open (full screen on phones).
    pub fn is_open(&self) -> bool {
        self.pane.shown(self.narrow) == PaneShown::Open && self.selected_artifact().is_some()
    }

    /// The pane is maximized: draw only it (with [`Artifacts::pane`]) below the header.
    pub fn is_maximized(&self) -> bool {
        self.pane.maximized(self.narrow) && self.selected_artifact().is_some()
    }

    /// Messages to send and notices to show, collected while drawing.
    pub fn take_output(&mut self) -> (Vec<ClientMsg>, Vec<(NoticeLevel, String)>) {
        for o in self.exports.poll() {
            match o {
                Ok(text) => self.notices.push((NoticeLevel::Info, text)),
                Err(text) => self.notices.push((NoticeLevel::Error, text)),
            }
        }
        let mut out = std::mem::take(&mut self.outbox);
        out.extend(self.views.take());
        out.extend(self.web.take());
        (out, std::mem::take(&mut self.notices))
    }

    /// Web surfaces (MCP Apps, plugin web views) and their bridge.
    pub fn web_mut(&mut self) -> &mut crate::web::Web {
        &mut self.web
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

    /// What draws `a`, and what it is.
    fn renderer(&self, a: &Artifact) -> (std::rc::Rc<dyn ArtifactRenderer>, RendererCaps) {
        let r = self.renderers.of(a);
        let caps = r.caps();
        (r, caps)
    }

    /// Running / paused, for artifacts on the web surface.
    fn app_status(&self, a: &Artifact, version: u32) -> AppStatus {
        if self.renderer(a).1.web_surface { self.web.view.status(&a.id, version) } else { AppStatus::Unknown }
    }

    /// The file of a version, asking for it the first time.
    fn file(&mut self, id: &str, version: u32) -> &mut File {
        self.files.fetch(&mut self.outbox, id, version)
    }

    fn links(&self) -> Links<'_> {
        Links { chat: self.chat.as_deref(), base: self.link_base.as_ref(), theme: &self.theme }
    }

    /// The URL a version is served at, theme in the hash.
    fn app_url(&self, id: &str, version: u32) -> Result<String, String> {
        self.links().url(id, version)
    }

    /// Run `f` with what a renderer draws `a` (version `version`) with.
    fn with_cx<R>(&mut self, t: &Tokens, a: &Artifact, version: u32, id: egui::Id, f: impl FnOnce(&mut ArtifactCx<'_>) -> R) -> R {
        let mut cx = ArtifactCx {
            t,
            artifact: a,
            version,
            id,
            files: &mut self.files,
            outbox: &mut self.outbox,
            web: &mut self.web,
            views: &mut self.views,
            requests: &mut self.requests,
            links: Links { chat: self.chat.as_deref(), base: self.link_base.as_ref(), theme: &self.theme },
        };
        f(&mut cx)
    }

    fn export(&mut self, id: &str, version: u32) {
        let Some(a) = self.get(id).cloned() else { return };
        match self.files.map.get(&(id.to_owned(), version)) {
            Some(File::Ready { bytes, .. }) => {
                let name = if version == a.latest().map_or(version, |l| l.n) { a.filename.clone() } else { versioned_name(&a.filename, version) };
                let mime = byob_protocol::artifact::mime(a.content_type());
                self.exports.save(&self.ctx, &name, &mime, bytes.clone());
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
            Action::Maximize(maximized) => self.pane.set_maximized(maximized),
            Action::MinimizeAll(minimized) => {
                let ids: Vec<String> = self.list.iter().map(|a| a.id.clone()).collect();
                self.pane.set_all_minimized(ids.iter().map(String::as_str), minimized);
            }
            Action::Export { id, version } => self.export(&id, version),
            Action::Open { id, version } => match self.app_url(&id, version) {
                Ok(url) => self.exports.open(ctx, &url),
                Err(e) => self.notices.push((NoticeLevel::Error, e)),
            },
            Action::Send(text) => self.outbox.push(ClientMsg::SendMessage { text, attachments: Vec::new() }),
        }
    }

    /// Per frame, before drawing: theme for apps, and a just-updated artifact to reveal.
    pub fn begin_frame(&mut self, ctx: &egui::Context, t: &Tokens, narrow: bool) {
        self.ctx.clone_from(ctx);
        self.narrow = narrow;
        self.drawn_app = None;
        let theme = appview::theme_vars(t);
        if theme != self.theme {
            self.web.set_theme(&theme);
            self.theme = theme;
        }
        if let Some(id) = self.reveal.take() {
            // A tab in front showing the latest; on a phone the pane would cover the chat
            // mid-reply, so it stays closed there (the floating button counts it).
            self.versions.remove(&id);
            self.pane.reveal(&id, narrow);
        }
        if !narrow && ctx.input_mut(|i| i.consume_shortcut(&MAXIMIZE_SHORTCUT)) {
            let maximized = self.pane.maximized(narrow);
            self.handle(ctx, Action::Maximize(!maximized));
        }
        if ctx.input_mut(|i| i.consume_shortcut(&PANE_SHORTCUT)) {
            self.handle(ctx, Action::Toggle);
        }
        // Esc restores, unless something else wants it: an app or text field that had focus
        // (Esc leaves it first), or an open menu.
        if self.pane.maximized(narrow)
            && !self.focus_held
            && !egui::Popup::is_any_open(ctx)
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            self.handle(ctx, Action::Maximize(false));
        }
    }

    /// Per frame, after drawing: run what the user asked for, keep background tabs alive,
    /// sync the app overlay.
    pub fn end_frame(&mut self, ctx: &egui::Context) {
        for r in std::mem::take(&mut self.requests) {
            self.actions.push(match r {
                Request::Open { id, version } => Action::Open { id, version },
                Request::Send(text) => Action::Send(text),
            });
        }
        for a in std::mem::take(&mut self.actions) {
            self.handle(ctx, a);
        }
        // Web-surface artifacts in tabs the pane didn't draw (behind another tab, or the
        // rail) keep running for a while without rendering.
        for id in self.pane.tabs().to_vec() {
            if self.drawn_app.as_deref() == Some(id.as_str()) {
                continue;
            }
            if let Some((a, v)) = self.tab_artifact(&id).filter(|(a, _)| self.renderer(a).1.web_surface) {
                self.web.view.hold(&a.id, v);
            }
        }
        self.web.view.end_frame(ctx);
        self.web.pump(ctx.input(|i| i.time));
        for url in self.web.take_links() {
            self.exports.open(ctx, &url);
        }
        self.focus_held = ctx.memory(|m| m.focused().is_some());
    }

    // ---- header, rail, floating button ---------------------------------------------------

    /// The header's "Artifacts N" toggle (wide windows; phones have the floating button).
    pub fn header_toggle(&mut self, ui: &mut egui::Ui) {
        if self.list.is_empty() || self.narrow {
            return;
        }
        let open = self.pane.shown(false) == PaneShown::Open;
        let label = format!("Artifacts {}", self.list.len());
        let shortcut = ui.ctx().format_shortcut(&PANE_SHORTCUT);
        if ui.selectable_label(open, label).on_hover_text(format!("Show or hide the artifact pane ({shortcut})")).clicked() {
            self.actions.push(Action::Toggle);
        }
    }

    /// The header's overflow menu entries: show the pane (phones), collapse or expand every
    /// artifact card. Nothing when the chat has no artifacts.
    pub fn menu_items(&mut self, ui: &mut egui::Ui) {
        if self.list.is_empty() {
            return;
        }
        if self.narrow {
            let label = format!("Show artifacts ({})", self.list.len());
            if ui.button(label).clicked() {
                self.actions.push(Action::Expand);
                ui.close();
            }
        }
        let all = self.list.iter().all(|a| self.pane.is_minimized(&a.id));
        let (label, tip) = if all {
            ("Expand all artifacts", "Show every artifact in this chat in full")
        } else {
            ("Collapse all artifacts", "Minimize every artifact in this chat to one line")
        };
        if ui.button(label).on_hover_text(tip).clicked() {
            self.actions.push(Action::MinimizeAll(!all));
            ui.close();
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

    /// The cards of a step in a reply, minus artifacts a later step of the same reply changed
    /// again (`newest` from [`newest_cards`]): one card per artifact per reply, not one per write.
    pub fn step_cards(&mut self, ui: &mut egui::Ui, t: &Tokens, step: u64, refs: &[ArtifactRef], newest: &HashMap<String, u64>) {
        for r in refs.iter().filter(|r| keeps_card(newest, step, &r.id)) {
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
        // While it's shown beside the chat, every version's card collapses to a chip: one
        // place to look at it (and a live app runs in the pane, not mirrored in here).
        if selected || self.pane.is_minimized(&a.id) {
            self.chip(ui, t, &a, version, selected, true);
            return;
        }
        let note = a.version(version).map(|v| v.note.clone()).unwrap_or_default();
        let (renderer, caps) = self.renderer(&a);
        let stroke = if selected { t.accent } else { t.border };
        egui::Frame::new().fill(t.code_bg).stroke(Stroke::new(1.0, stroke)).corner_radius(t.round(8.0)).inner_margin(vec2(t.pad(10.0), t.pad(8.0))).show(
            ui,
            |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if minimize_button(ui, t, &caps).clicked() {
                            self.actions.push(Action::Minimize { id: a.id.clone(), minimized: true });
                        }
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            kind_badge(ui, t, &caps.label);
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
                    let card_id = egui::Id::unique(("artifact-card", &a.id, version));
                    self.with_cx(t, &a, version, card_id, |cx| renderer.actions(ui, cx, Place::Card));
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
                let id = egui::Id::unique(("artifact-card", &a.id, version));
                self.with_cx(t, &a, version, id, |cx| renderer.card(ui, cx));
            },
        );
    }

    /// A minimized artifact: one line with its kind, title, version and (apps) whether it
    /// still runs, and Restore. Keeps a running app alive in the background.
    fn chip(&mut self, ui: &mut egui::Ui, t: &Tokens, a: &Artifact, version: u32, selected: bool, in_chat: bool) {
        let caps = self.renderer(a).1;
        if caps.web_surface {
            self.web.view.hold(&a.id, version);
        }
        let status = self.app_status(a, version);
        let stroke = if selected { t.accent } else { t.border };
        egui::Frame::new().fill(t.code_bg).stroke(Stroke::new(1.0, stroke)).corner_radius(t.round(16.0)).inner_margin(vec2(t.pad(8.0), t.pad(3.0))).show(
            ui,
            |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if in_chat && selected {
                            ui.label(RichText::new("Shown beside the chat").small().color(t.text_dim));
                        } else if ui.small_button("Restore").on_hover_text("Show it in full again").clicked() {
                            self.actions.push(Action::Minimize { id: a.id.clone(), minimized: false });
                        }
                        if in_chat && !selected && ui.small_button("Show").on_hover_text("Show it beside the chat").clicked() {
                            self.actions.push(Action::Select { id: a.id.clone(), version: Some(version) });
                        }
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            kind_badge(ui, t, &caps.label);
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
            let hint = if self.renderer(&a).1.web_surface {
                "Minimized: the app keeps running in the background for a minute, then pauses (a snapshot is kept; restoring it after that starts it afresh)."
            } else {
                "Minimized. Restore shows it in full here and in the chat."
            };
            ui.label(RichText::new(hint).small().color(t.text_faint));
            return;
        }
        let id = egui::Id::unique(("artifact-pane", &a.id, version));
        let (renderer, caps) = self.renderer(&a);
        if caps.source_view && self.source {
            let text = match self.file(&a.id, version) {
                File::Ready { bytes, .. } => Some(String::from_utf8_lossy(bytes).into_owned()),
                _ => None,
            };
            match text {
                Some(text) => crate::renderers::source(ui, &text),
                None => self.with_cx(t, &a, version, id, |cx| crate::renderers::text::Text.pane(ui, cx)),
            }
            return;
        }
        if caps.web_surface {
            self.drawn_app = Some(a.id.clone());
        }
        self.with_cx(t, &a, version, id, |cx| renderer.pane(ui, cx));
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
                    let maximized = self.pane.maximized(false);
                    let max_keys = ui.ctx().format_shortcut(&MAXIMIZE_SHORTCUT);
                    if maximized {
                        // Obvious way back: a labelled button where the collapse arrow was.
                        if ui.button(RichText::new("Restore").color(t.text_strong)).on_hover_text(format!("Back to the chat (Esc or {max_keys})")).clicked() {
                            self.actions.push(Action::Maximize(false));
                        }
                    } else {
                        let shortcut = ui.ctx().format_shortcut(&PANE_SHORTCUT);
                        if ui
                            .add(egui::Button::new(RichText::new("»").size(16.0)).frame(false))
                            .on_hover_text(format!("Collapse to the rail ({shortcut})"))
                            .clicked()
                        {
                            self.actions.push(Action::Close);
                        }
                        if maximize_button(ui, t).on_hover_text(format!("Maximize: use the whole window ({max_keys})")).clicked() {
                            self.actions.push(Action::Maximize(true));
                        }
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
        let (renderer, caps) = self.renderer(a);
        // Title row: type · title … minimize.
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !minimized && minimize_button(ui, t, &caps).clicked() {
                    self.actions.push(Action::Minimize { id: a.id.clone(), minimized: true });
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    kind_badge(ui, t, &caps.label);
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
            if caps.source_view {
                ui.selectable_value(&mut self.source, false, "Preview");
                ui.selectable_value(&mut self.source, true, "Source");
            }
            let id = egui::Id::unique(("artifact-pane", &a.id, version));
            self.with_cx(t, a, version, id, |cx| renderer.actions(ui, cx, Place::Pane));
            if ui.button("Export").on_hover_text(format!("Save {}", a.filename)).clicked() {
                self.actions.push(Action::Export { id: a.id.clone(), version });
            }
            if caps.text
                && let Some(File::Ready { bytes, .. }) = self.files.map.get(&(a.id.clone(), version))
                && ui.button("Copy").on_hover_text("Copy the file's text").clicked()
            {
                ui.ctx().copy_text(String::from_utf8_lossy(bytes).into_owned());
            }
        });
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

fn version_label(v: &byob_protocol::ArtifactVersion) -> String {
    let note: String = v.note.chars().take(40).collect();
    if note.is_empty() { format!("v{}", v.n) } else { format!("v{} · {note}", v.n) }
}

/// The "–" that minimizes a card or the pane's artifact.
/// A frameless button with a window-maximize glyph (a square with a thick top edge).
fn maximize_button(ui: &mut egui::Ui, t: &Tokens) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(22.0, 22.0), egui::Sense::click());
    let hovered = resp.hovered();
    let p = ui.painter();
    if hovered {
        p.rect_filled(rect, t.round(4.0), ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let color = if hovered { t.text_strong } else { t.text_dim };
    let square = egui::Rect::from_center_size(rect.center(), vec2(11.0, 10.0));
    p.rect_stroke(square, 1.0, Stroke::new(1.3, color), egui::StrokeKind::Inside);
    p.rect_filled(egui::Rect::from_min_size(square.min, vec2(square.width(), 2.5)), 1.0, color);
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, "Maximize"));
    resp
}

fn minimize_button(ui: &mut egui::Ui, t: &Tokens, caps: &RendererCaps) -> egui::Response {
    let tip = if caps.web_surface { "Minimize (it keeps running for a minute, then pauses)" } else { "Minimize to one line" };
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

fn kind_badge(ui: &mut egui::Ui, t: &Tokens, label: &str) {
    egui::Frame::new().fill(crate::theme::mix(t.accent, t.code_bg, 0.8)).corner_radius(t.round(4.0)).inner_margin(vec2(t.pad(5.0), t.pad(1.0))).show(
        ui,
        |ui| {
            ui.label(RichText::new(label.to_uppercase()).small().color(t.accent));
        },
    );
}

/// For each artifact a reply's steps changed, the last step that did: only that step shows its
/// card (its version picker lists the others), so a model that writes a sheet thirty times in
/// one reply leaves one card, not thirty.
pub fn newest_cards(steps: &[byob_protocol::Step]) -> HashMap<String, u64> {
    let mut newest = HashMap::new();
    for step in steps {
        for r in &step.artifacts {
            newest.insert(r.id.clone(), step.id);
        }
    }
    newest
}

/// Whether `step` shows its card for artifact `id`: unless a later step of the reply changed it
/// again. Steps outside `newest` (a subagent's) keep theirs.
fn keeps_card(newest: &HashMap<String, u64>, step: u64, id: &str) -> bool {
    newest.get(id).is_none_or(|&s| s == step)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appview::AppDoc;
    use byob_protocol::{ArtifactKind, ArtifactVersion};

    fn wrote(id: u64, refs: &[(&str, u32)]) -> byob_protocol::Step {
        byob_protocol::Step {
            id,
            tool: "write_cells".into(),
            title: "Write cells".into(),
            access: byob_protocol::ToolAccess::Write,
            status: byob_protocol::StepStatus::Done,
            detail: "{}".into(),
            changes: vec![],
            at: 0,
            preview: None,
            media: vec![],
            artifacts: refs.iter().map(|(a, v)| ArtifactRef { id: (*a).into(), version: *v }).collect(),
            delegation: None,
            ask: None,
            views: vec![],
        }
    }

    #[test]
    fn a_reply_shows_one_card_per_artifact_on_its_last_change() {
        // budget written three times (with a chart made in between), then notes once
        let steps =
            [wrote(1, &[("budget", 1)]), wrote(2, &[("budget", 2), ("chart", 1)]), wrote(3, &[]), wrote(4, &[("budget", 3)]), wrote(5, &[("notes", 1)])];
        let newest = newest_cards(&steps);
        let shown: Vec<(u64, &str)> =
            steps.iter().flat_map(|s| s.artifacts.iter().filter(|r| keeps_card(&newest, s.id, &r.id)).map(move |r| (s.id, r.id.as_str()))).collect();
        assert_eq!(shown, [(2, "chart"), (4, "budget"), (5, "notes")]);
        // a subagent's step isn't one of the reply's: its cards stay unless the reply changed it later
        assert!(keeps_card(&newest, 99, "other") && !keeps_card(&newest, 99, "budget"));
    }

    fn artifact(id: &str, kind: ArtifactKind, filename: &str, versions: &[u32]) -> Artifact {
        let versions = versions
            .iter()
            .map(|n| ArtifactVersion { n: *n, path: format!(".byob/artifacts/{id}/v{n}"), created_at: 0, note: format!("note {n}"), size: 3 })
            .collect();
        Artifact { id: id.into(), kind, content_type: kind.content_type(filename).into(), title: id.to_uppercase(), filename: filename.into(), versions }
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
        assert!(!s.is_open() && s.files.map.is_empty());
    }

    #[test]
    fn unasked_or_failed_files_are_handled() {
        let mut s = Artifacts::default();
        s.apply(&ServerMsg::Artifacts { chat: Some("c".into()), artifacts: vec![artifact("p", ArtifactKind::Image, "p.png", &[1])] });
        // A file nobody asked for is ignored.
        s.apply(&ServerMsg::ArtifactFile { id: "p".into(), version: 1, data: Some(media::encode(b"x")), error: None });
        assert!(s.files.map.is_empty());
        let _ = s.file("p", 1);
        s.apply(&ServerMsg::ArtifactFile { id: "p".into(), version: 1, data: Some("!!".into()), error: None });
        assert!(matches!(s.files.map.get(&("p".into(), 1)), Some(File::Failed(_))));
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
        // What a renderer asks for is done after the frame.
        s.requests.push(Request::Send(crate::renderers::html_app::error_report("APP", "app", 2, "x is not defined")));
        s.end_frame(&ctx);
        let (out, _) = s.take_output();
        let [ClientMsg::SendMessage { text, .. }] = &out[..] else { panic!("{out:?}") };
        assert!(text.contains("\"APP\" (id app, version 2)") && text.contains("x is not defined") && text.contains("update_artifact"), "{text}");
        // Without a server to link to, Open says so instead of failing silently.
        if cfg!(not(target_arch = "wasm32")) {
            s.handle(&ctx, Action::Open { id: "app".into(), version: 2 });
            assert!(s.take_output().1.iter().any(|(l, _)| *l == NoticeLevel::Error));
            s.set_link_base(Box::new(|| Ok("http://127.0.0.1:9".into())));
            s.begin_frame(&ctx, &Tokens::dark(), false);
            assert!(s.app_url("app", 2).unwrap().starts_with("http://127.0.0.1:9/artifact/c/app/2#--byob-bg="));
        }
    }

    /// Records what the artifact UI asks of the app view.
    #[derive(Default)]
    struct Recorder {
        log: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
        /// The width the pane gave the app, each time.
        widths: std::rc::Rc<std::cell::RefCell<Vec<f32>>>,
    }

    impl AppView for Recorder {
        fn show(&mut self, ui: &mut egui::Ui, _t: &Tokens, doc: &AppDoc<'_>) -> appview::AppViewResponse {
            self.log.borrow_mut().push(format!("show {}", doc.id));
            self.widths.borrow_mut().push(ui.available_width());
            appview::AppViewResponse::default()
        }
        fn card(&mut self, _ui: &mut egui::Ui, _t: &Tokens, doc: &AppDoc<'_>) -> Option<appview::AppViewResponse> {
            self.log.borrow_mut().push(format!("card {}/{}", doc.id, doc.version));
            Some(appview::AppViewResponse::default())
        }
        fn live_cards(&self) -> bool {
            true
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

    /// Regression: a card mirrored the pane's (narrower) app frame scaled up to the card,
    /// so the app looked zoomed in. Shown beside the chat, every version's card is a chip.
    #[test]
    fn showing_an_artifact_beside_the_chat_collapses_its_cards() {
        let mut s = Artifacts::default();
        let rec = Recorder::default();
        let log = std::rc::Rc::clone(&rec.log);
        s.set_app_view(Box::new(rec));
        s.apply(&ServerMsg::Artifacts { chat: Some("c".into()), artifacts: vec![artifact("app", ArtifactKind::Html, "app.html", &[1, 2])] });
        let ctx = egui::Context::default();
        let t = Tokens::dark();
        let refs = [ArtifactRef { id: "app".into(), version: 1 }, ArtifactRef { id: "app".into(), version: 2 }];
        let run = |s: &mut Artifacts| {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                s.begin_frame(ui.ctx(), &t, false);
                s.side_panel(ui, &t);
                egui::CentralPanel::default().show(ui, |ui| s.cards(ui, &t, &refs));
            });
            out.textures_delta.clear();
            s.end_frame(&ctx);
        };
        run(&mut s);
        for v in [1, 2] {
            s.apply(&ServerMsg::ArtifactFile { id: "app".into(), version: v, data: Some(media::encode(b"<p>hi</p>")), error: None });
        }
        drain(&log);
        run(&mut s);
        assert_eq!(drain(&log), ["card app/1", "card app/2"]);
        // Shown beside the chat: drawn there only; both versions' cards are chips.
        s.handle(&ctx, Action::Select { id: "app".into(), version: Some(2) });
        run(&mut s);
        let l = drain(&log);
        assert!(l.contains(&"show app".to_owned()) && !l.iter().any(|e| e.starts_with("card")), "{l:?}");
        // Closed again: the cards come back.
        s.handle(&ctx, Action::CloseTab("app".into()));
        run(&mut s);
        assert_eq!(drain(&log), ["card app/1", "card app/2"]);
    }

    /// Maximize: the pane takes the window (drawn the way `ByobApp::ui` does), the same app
    /// keeps running at the new size; Esc restores unless a widget had focus (Esc leaves that
    /// first); collapsing restores too; phones ignore it.
    #[test]
    fn maximize_toggles_with_the_shortcut_and_esc_restores() {
        let mut s = Artifacts::default();
        let rec = Recorder::default();
        let (log, widths) = (std::rc::Rc::clone(&rec.log), std::rc::Rc::clone(&rec.widths));
        s.set_app_view(Box::new(rec));
        s.apply(&ServerMsg::Artifacts { chat: Some("c".into()), artifacts: vec![artifact("app", ArtifactKind::Html, "app.html", &[1])] });
        let ctx = egui::Context::default();
        let t = Tokens::dark();
        let key = |key, modifiers| egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers };
        let maximize = key(egui::Key::F, egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT));
        let esc = key(egui::Key::Escape, egui::Modifiers::NONE);
        let field = egui::Id::unique("field");
        // `focus`: a text field (standing in for a focused app) takes keyboard focus.
        let run = |s: &mut Artifacts, events: Vec<egui::Event>, width: f32, focus: bool| {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(width, 700.0))), events, ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| {
                let narrow = width < 600.0;
                s.begin_frame(ui.ctx(), &t, narrow);
                let field = |ui: &mut egui::Ui| {
                    let r = ui.add(egui::TextEdit::singleline(&mut String::new()).id(field));
                    if focus {
                        r.request_focus();
                    }
                };
                if s.is_maximized() {
                    egui::CentralPanel::default().show(ui, |ui| {
                        field(ui);
                        s.pane(ui, &t, false);
                    });
                } else if !narrow {
                    s.side_panel(ui, &t);
                    egui::CentralPanel::default().show(ui, field);
                }
            });
            out.textures_delta.clear();
            s.end_frame(&ctx);
        };
        s.handle(&ctx, Action::Select { id: "app".into(), version: None });
        run(&mut s, vec![], 1200.0, false);
        let beside = widths.borrow_mut().pop().unwrap();
        assert!(!s.is_maximized() && beside < 900.0, "{beside}");
        drain(&log);

        run(&mut s, vec![maximize.clone()], 1200.0, false);
        assert!(s.is_maximized());
        run(&mut s, vec![], 1200.0, false);
        let full = widths.borrow_mut().pop().unwrap();
        assert!(full > 1150.0, "the whole window: {full}");
        assert_eq!(drain(&log), ["show app"], "the same app, never held or restarted");
        assert!(s.pane_prefs().chats.iter().all(|c| c.tabs == ["app"]) && !s.pane_prefs().collapsed);

        // Esc with nothing focused restores; the shortcut maximizes again.
        run(&mut s, vec![esc.clone()], 1200.0, false);
        assert!(!s.is_maximized());
        run(&mut s, vec![maximize.clone()], 1200.0, false);
        assert!(s.is_maximized());
        // A widget with focus (an app, a text field) takes the first Esc.
        run(&mut s, vec![], 1200.0, true);
        run(&mut s, vec![], 1200.0, false);
        assert!(ctx.memory(|m| m.has_focus(field)));
        run(&mut s, vec![esc.clone()], 1200.0, false);
        assert!(s.is_maximized(), "the first Esc leaves the focused widget");
        run(&mut s, vec![esc.clone()], 1200.0, false);
        assert!(!s.is_maximized(), "the next one restores");
        assert!(s.is_open(), "restored beside the chat, not collapsed");

        // Collapsing (Ctrl+\\) ends it; the pane shortcut still works while maximized.
        run(&mut s, vec![maximize.clone()], 1200.0, false);
        run(&mut s, vec![key(egui::Key::Backslash, egui::Modifiers::COMMAND)], 1200.0, false);
        assert!(!s.is_maximized() && !s.is_open());
        run(&mut s, vec![key(egui::Key::Backslash, egui::Modifiers::COMMAND)], 1200.0, false);
        assert!(s.is_open() && !s.is_maximized(), "reopened beside the chat");

        // Phones: the open pane is already full screen; the shortcut does nothing.
        run(&mut s, vec![maximize], 400.0, false);
        assert!(!s.is_maximized());
    }

    #[test]
    fn names_for_older_versions() {
        assert_eq!(versioned_name("loan.html", 2), "loan v2.html");
        assert_eq!(versioned_name("Sales.chart.json", 3), "Sales v3.chart.json");
        assert_eq!(versioned_name("noext", 1), "noext v1");
    }

    /// A renderer registered from outside draws its type in the card and the pane, adds
    /// its buttons, gets the bytes, and with `web_surface` is kept alive like an HTML app;
    /// a type nobody registered still shows (as its source).
    #[test]
    fn a_registered_renderer_is_used() {
        use crate::renderers::{ArtifactCx, ArtifactRenderer, Fetch, Place, RendererCaps};
        type Log = std::rc::Rc<std::cell::RefCell<Vec<String>>>;
        struct Sheet(Log);
        impl ArtifactRenderer for Sheet {
            fn caps(&self) -> RendererCaps {
                RendererCaps { label: "sheet".into(), text: true, source_view: true, web_surface: true }
            }
            fn card(&self, _ui: &mut egui::Ui, cx: &mut ArtifactCx<'_>) {
                let rows = match cx.prepared(|b| Ok(String::from_utf8_lossy(b).lines().count())) {
                    Fetch::Ready(n) => *n,
                    _ => 0,
                };
                self.0.borrow_mut().push(format!("card {} {rows} rows", cx.artifact.id));
            }
            fn pane(&self, _ui: &mut egui::Ui, cx: &mut ArtifactCx<'_>) {
                self.0.borrow_mut().push(format!("pane {}", cx.artifact.id));
            }
            fn actions(&self, _ui: &mut egui::Ui, cx: &mut ArtifactCx<'_>, place: Place) {
                self.0.borrow_mut().push(format!("actions {place:?}"));
                if place == Place::Pane {
                    cx.send_message("sum column B".into());
                }
            }
        }
        let log = Log::default();
        let mut s = Artifacts::default();
        let rec = Recorder::default();
        let held = std::rc::Rc::clone(&rec.log);
        s.set_app_view(Box::new(rec));
        s.renderers_mut().register("application/vnd.acme.sheet+json", Sheet(std::rc::Rc::clone(&log)));
        let mut sheet = artifact("budget", ArtifactKind::Text, "budget.sheet", &[1]);
        sheet.content_type = "application/vnd.acme.sheet+json".into();
        let mut odd = artifact("odd", ArtifactKind::Text, "odd.thing", &[1]);
        odd.content_type = "application/x-nobody-knows".into();
        s.apply(&ServerMsg::Artifacts { chat: Some("c".into()), artifacts: vec![sheet, odd] });
        let ctx = egui::Context::default();
        let t = Tokens::dark();
        let refs = [ArtifactRef { id: "budget".into(), version: 1 }, ArtifactRef { id: "odd".into(), version: 1 }];
        let run = |s: &mut Artifacts| {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                s.begin_frame(ui.ctx(), &t, false);
                s.side_panel(ui, &t);
                egui::CentralPanel::default().show(ui, |ui| s.cards(ui, &t, &refs));
            });
            out.textures_delta.clear();
            s.end_frame(&ctx);
        };
        run(&mut s);
        for id in ["budget", "odd"] {
            s.apply(&ServerMsg::ArtifactFile { id: id.into(), version: 1, data: Some(media::encode(b"a,1\nb,2\n")), error: None });
        }
        run(&mut s);
        assert!(log.borrow().contains(&"card budget 2 rows".to_owned()) && log.borrow().contains(&"actions Card".to_owned()), "{:?}", log.borrow());
        // In the pane: its view, its buttons, its requests; shown in another tab, it is held.
        s.handle(&ctx, Action::Select { id: "budget".into(), version: None });
        run(&mut s);
        assert!(log.borrow().contains(&"pane budget".to_owned()) && log.borrow().contains(&"actions Pane".to_owned()));
        let (out, _) = s.take_output();
        assert!(out.iter().any(|m| matches!(m, ClientMsg::SendMessage { text, .. } if text == "sum column B")), "{out:?}");
        assert_eq!(s.app_status(s.get("budget").unwrap(), 1), AppStatus::Running);
        s.handle(&ctx, Action::Select { id: "odd".into(), version: None });
        drain(&held);
        run(&mut s);
        assert!(drain(&held).contains(&"hold budget/1".to_owned()));
        // The unknown type got the text fallback (nothing recorded for it, no crash).
        assert!(!log.borrow().iter().any(|l| l.contains("odd")));
        assert_eq!(s.app_status(s.get("odd").unwrap(), 1), AppStatus::Unknown);
    }
}
