//! Artifacts in the UI: a card where a step made or changed one (title, kind, version,
//! Show / Open / Export, a versions menu), and the artifact pane beside the conversation
//! (full screen on narrow windows) showing the selected one large — markdown with a
//! Preview/Source switch, charts natively, images, text as source with Copy, and HTML apps
//! through the [`AppView`] seam.
//!
//! The engine sends the chat's artifacts with the transcript and an update whenever a
//! version is added; version bytes are fetched once with `GetArtifact` and kept here (they
//! are also what Export saves).

use std::collections::HashMap;

use egui::{RichText, Stroke, vec2};
use yah_protocol::media::{self, ChartSpec};
use yah_protocol::{Artifact, ArtifactKind, ArtifactRef, ClientMsg, NoticeLevel, ServerMsg};

use crate::appview::{self, AppDoc, AppView};
use crate::export::Exports;
use crate::markdown;
use crate::theme::Tokens;

/// Tallest an image or chart preview is drawn in a card.
const CARD_PREVIEW_HEIGHT: f32 = 260.0;
/// Lines of text shown in a card.
const CARD_LINES: usize = 4;
/// Versions kept in memory before old ones are dropped (they are fetched again if needed).
const MAX_FILES: usize = 64;

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
    Select { id: String, version: Option<u32> },
    Open { id: String, version: u32 },
    Export { id: String, version: u32 },
    SendError { id: String, version: u32, error: String },
    Close,
}

pub struct Artifacts {
    chat: Option<String>,
    list: Vec<Artifact>,
    files: HashMap<(String, u32), File>,
    /// Insertion order of `files`, for dropping the oldest.
    order: Vec<(String, u32)>,
    /// Shown in the pane: id, and a version (`None`: the latest).
    selected: Option<(String, Option<u32>)>,
    open: bool,
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
            selected: None,
            open: false,
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

    /// Apply a server message; returns whether it was one of ours.
    pub fn apply(&mut self, msg: &ServerMsg) -> bool {
        match msg {
            ServerMsg::Artifacts { chat, artifacts } => {
                if *chat != self.chat {
                    self.chat.clone_from(chat);
                    self.files.clear();
                    self.order.clear();
                    self.selected = None;
                    self.open = false;
                    self.pending_export.clear();
                }
                self.list.clone_from(artifacts);
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

    pub fn is_open(&self) -> bool {
        self.open && self.selected_artifact().is_some()
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
        let (id, v) = self.selected.as_ref()?;
        let a = self.get(id)?;
        let n = v.filter(|n| a.version(*n).is_some()).or_else(|| a.latest().map(|l| l.n))?;
        Some((a.clone(), n))
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
                self.selected = Some((id, version));
                self.open = true;
            }
            Action::Close => self.open = false,
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
        let theme = appview::theme_vars(t);
        if theme != self.theme {
            self.view.set_theme(&theme);
            self.theme = theme;
        }
        if let Some(id) = self.reveal.take() {
            // On a phone the pane would cover the chat mid-reply; there the card has Show.
            if !narrow || self.open {
                self.selected = Some((id, None));
                self.open = true;
            }
        }
    }

    /// Per frame, after drawing: run what the user asked for, sync the app overlay.
    pub fn end_frame(&mut self, ctx: &egui::Context) {
        for a in std::mem::take(&mut self.actions) {
            self.handle(ctx, a);
        }
        self.view.end_frame(ctx);
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
        let note = a.version(version).map(|v| v.note.clone()).unwrap_or_default();
        let selected = self.open && self.selected.as_ref().is_some_and(|(id, _)| *id == a.id);
        let stroke = if selected { t.accent } else { t.border };
        egui::Frame::new().fill(t.code_bg).stroke(Stroke::new(1.0, stroke)).corner_radius(8.0).inner_margin(vec2(10.0, 8.0)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                kind_badge(ui, t, a.kind);
                ui.label(RichText::new(&a.title).strong().color(t.text_strong));
                ui.label(RichText::new(format!("v{version}")).small().color(t.text_dim));
                if !note.is_empty() {
                    ui.add(egui::Label::new(RichText::new(&note).small().color(t.text_dim)).truncate()).on_hover_text(&note);
                }
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
        });
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
                ui.add(egui::Spinner::new().size(12.0));
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
        let Some((a, version)) = self.selected_artifact() else {
            self.open = false;
            return;
        };
        self.pane_header(ui, t, &a, version, narrow);
        ui.separator();
        let ctx = ui.ctx().clone();
        let id = egui::Id::unique(("artifact-pane", &a.id, version));
        if a.kind == ArtifactKind::Html {
            self.app(ui, t, &a, version);
            return;
        }
        let source = self.source;
        match self.shown(&ctx, &a, version) {
            Err(None) => {
                ui.centered_and_justified(|ui| ui.spinner());
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

    fn pane_header(&mut self, ui: &mut egui::Ui, t: &Tokens, a: &Artifact, version: u32, narrow: bool) {
        ui.add_space(4.0);
        // Title row: back (phones) · kind · title … close.
        ui.horizontal(|ui| {
            if narrow && ui.button("← Chat").clicked() {
                self.actions.push(Action::Close);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !narrow && ui.button("×").on_hover_text("Close").clicked() {
                    self.actions.push(Action::Close);
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    kind_badge(ui, t, a.kind);
                    ui.add(egui::Label::new(RichText::new(&a.title).strong().size(15.0).color(t.text_strong)).truncate()).on_hover_text(&a.filename);
                });
            });
        });
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
            egui::Frame::new().fill(t.code_bg).stroke(Stroke::new(1.0, t.danger)).corner_radius(6.0).inner_margin(8.0).show(ui, |ui| {
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

fn kind_badge(ui: &mut egui::Ui, t: &Tokens, kind: ArtifactKind) {
    egui::Frame::new().fill(crate::theme::mix(t.accent, t.code_bg, 0.8)).corner_radius(4.0).inner_margin(vec2(5.0, 1.0)).show(ui, |ui| {
        ui.label(RichText::new(kind.label().to_uppercase()).small().color(t.accent));
    });
}

fn image(ui: &mut egui::Ui, t: &Tokens, tex: &egui::TextureHandle, max_height: f32) {
    let natural = tex.size_vec2();
    if natural.x <= 0.0 || natural.y <= 0.0 {
        return;
    }
    let scale = (ui.available_width() / natural.x).min(max_height / natural.y).min(1.0);
    ui.add(egui::Image::from_texture(egui::load::SizedTexture::new(tex.id(), natural * scale)).corner_radius(6.0));
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

    #[test]
    fn names_for_older_versions() {
        assert_eq!(versioned_name("loan.html", 2), "loan v2.html");
        assert_eq!(versioned_name("Sales.chart.json", 3), "Sales v3.chart.json");
        assert_eq!(versioned_name("noext", 1), "noext v1");
    }
}
