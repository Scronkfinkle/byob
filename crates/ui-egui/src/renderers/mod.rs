//! How artifacts are drawn: the [`ArtifactRenderers`] registry, content type → renderer.
//!
//! The artifact UI ([`crate::artifact`]) owns the frame around an artifact — its card in the
//! chat (badge, title, Show / Export, versions), the pane (tabs, rail, maximize, versions,
//! Preview/Source, Export, Copy), fetching and caching version bytes, minimizing. What is
//! drawn *inside* — the card preview and the pane view — and any extra buttons come from the
//! artifact's renderer, picked by its content type ([`byob_protocol::Artifact::content_type`]):
//! an exact registration, else one for `type/*`, else the fallback, which shows the text
//! source. byob's own renderers ([`ArtifactRenderers::default`]: text, markdown, chart,
//! image, HTML app) register exactly like anyone else's (`ByobApp::artifact_renderers_mut`).
//!
//! A renderer says what it is with [`RendererCaps`]: its badge label, whether the bytes are
//! text (Copy), whether it has a rendered view besides the source (the pane's Preview/Source
//! switch, the source drawn by the pane itself), and whether it runs on the web surface (the
//! [`AppView`] runtime: Servo helper on the desktop, an iframe in the browser), which makes
//! the pane keep it alive in the background and show running/paused.

use std::any::{Any, TypeId};
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use byob_protocol::{Artifact, ClientMsg};
use egui::RichText;

use crate::appview::{self, AppView};
use crate::theme::Tokens;

pub mod chart;
pub mod html_app;
pub mod image;
pub mod markdown;
pub mod text;

/// Versions kept in memory before old ones are dropped (they are fetched again if needed).
const MAX_FILES: usize = 64;

/// What a renderer is and needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RendererCaps {
    /// The badge on its card and in the pane ("document", "app"), shown upper case.
    pub label: String,
    /// The bytes are text: the pane offers Copy.
    pub text: bool,
    /// It draws a rendered view and the bytes are its source: the pane offers
    /// Preview/Source and draws the source itself.
    pub source_view: bool,
    /// It runs on the web surface ([`ArtifactCx::web`]): kept running for a while when
    /// hidden (minimized, a background tab, the rail), with a running/paused dot.
    pub web_surface: bool,
}

impl RendererCaps {
    /// A text type labelled `label`, with nothing else.
    pub fn text(label: &str) -> Self {
        Self { label: label.to_owned(), text: true, source_view: false, web_surface: false }
    }
}

/// Where a renderer is drawing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// A card in the chat (preview, limited height).
    Card,
    /// The artifact pane (the space left, maybe the whole window).
    Pane,
}

/// Draws one or more content types. Every method but [`ArtifactRenderer::caps`] gets an
/// [`ArtifactCx`] with the artifact, the version shown and what it may use.
pub trait ArtifactRenderer {
    fn caps(&self) -> RendererCaps;
    /// The preview under a card's buttons.
    fn card(&self, ui: &mut egui::Ui, cx: &mut ArtifactCx<'_>);
    /// The artifact in the pane, filling the space left.
    fn pane(&self, ui: &mut egui::Ui, cx: &mut ArtifactCx<'_>);
    /// Extra buttons in the card's row (after Show) and the pane's tool row (before Export).
    fn actions(&self, _ui: &mut egui::Ui, _cx: &mut ArtifactCx<'_>, _place: Place) {}
}

/// Content type → renderer.
pub struct ArtifactRenderers {
    by_type: BTreeMap<String, Rc<dyn ArtifactRenderer>>,
    fallback: Rc<dyn ArtifactRenderer>,
}

impl Default for ArtifactRenderers {
    /// With byob's own renderers.
    fn default() -> Self {
        let mut r = Self::empty();
        markdown::register(&mut r);
        chart::register(&mut r);
        image::register(&mut r);
        html_app::register(&mut r);
        r
    }
}

impl ArtifactRenderers {
    /// Only the fallback: everything shows as text source.
    pub fn empty() -> Self {
        Self { by_type: BTreeMap::new(), fallback: Rc::new(text::Text) }
    }

    /// Draw `content_type` (or every `type/*`) with `renderer`, replacing an earlier one.
    pub fn register(&mut self, content_type: &str, renderer: impl ArtifactRenderer + 'static) {
        self.register_rc(content_type, Rc::new(renderer));
    }

    /// [`ArtifactRenderers::register`] a renderer shared by several types.
    pub fn register_rc(&mut self, content_type: &str, renderer: Rc<dyn ArtifactRenderer>) {
        self.by_type.insert(content_type.to_ascii_lowercase(), renderer);
    }

    /// What draws types nobody registered (by default the text source).
    pub fn set_fallback(&mut self, renderer: impl ArtifactRenderer + 'static) {
        self.fallback = Rc::new(renderer);
    }

    /// An exact registration for `content_type`, else one for its `type/*`, else the fallback.
    pub fn get(&self, content_type: &str) -> Rc<dyn ArtifactRenderer> {
        let exact = self.by_type.get(content_type);
        let wild = || content_type.split_once('/').and_then(|(major, _)| self.by_type.get(&format!("{major}/*")));
        Rc::clone(exact.or_else(wild).unwrap_or(&self.fallback))
    }

    /// The renderer of `a`.
    pub fn of(&self, a: &Artifact) -> Rc<dyn ArtifactRenderer> {
        self.get(a.content_type())
    }

    /// The registered content types (and `type/*` patterns).
    pub fn types(&self) -> Vec<String> {
        self.by_type.keys().cloned().collect()
    }
}

/// A version's bytes as far as they have come.
pub enum Fetch<T> {
    /// Asked for, not here yet.
    Loading,
    /// Not available, or not readable by this renderer.
    Failed(String),
    Ready(T),
}

/// One version's bytes on this client, and what a renderer made of them.
pub(crate) enum File {
    Requested,
    Ready { bytes: Vec<u8>, prepared: Option<(TypeId, Result<Box<dyn Any>, String>)> },
    Failed(String),
}

/// Version bytes fetched with `GetArtifact`, oldest dropped first.
#[derive(Default)]
pub(crate) struct Files {
    pub map: HashMap<(String, u32), File>,
    /// Insertion order of `map`, for dropping the oldest.
    pub order: Vec<(String, u32)>,
}

impl Files {
    /// The file of a version, asking for it (into `outbox`) the first time.
    pub fn fetch(&mut self, outbox: &mut Vec<ClientMsg>, id: &str, version: u32) -> &mut File {
        let key = (id.to_owned(), version);
        if !self.map.contains_key(&key) {
            if self.order.len() >= MAX_FILES {
                let old = self.order.remove(0);
                self.map.remove(&old);
            }
            self.order.push(key.clone());
            outbox.push(ClientMsg::GetArtifact { id: id.to_owned(), version });
        }
        self.map.entry(key).or_insert(File::Requested)
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }
}

/// Something a renderer asks the artifact UI to do (after the frame).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Request {
    /// Open the version in the system browser (served at its URL).
    Open { id: String, version: u32 },
    /// Send the model a message as if the user typed it.
    Send(String),
}

/// Where an app version is served: `[base]/artifact/<chat>/<id>/<version>#theme`.
pub(crate) struct Links<'a> {
    pub chat: Option<&'a str>,
    pub base: Option<&'a crate::artifact::LinkBase>,
    pub theme: &'a [(&'static str, String)],
}

impl Links<'_> {
    pub fn url(&self, id: &str, version: u32) -> Result<String, String> {
        let chat = self.chat.ok_or("this chat has no artifacts yet")?;
        let base = if cfg!(target_arch = "wasm32") {
            String::new()
        } else {
            match self.base {
                Some(f) => f()?,
                None => return Err("opening artifacts isn't available here".into()),
            }
        };
        Ok(format!("{base}/artifact/{chat}/{id}/{version}{}", appview::theme_hash(self.theme)))
    }
}

/// What a renderer draws with.
pub struct ArtifactCx<'a> {
    pub t: &'a Tokens,
    pub artifact: &'a Artifact,
    /// The version shown.
    pub version: u32,
    /// Salt for widget ids (the card and the pane differ).
    pub id: egui::Id,
    pub(crate) files: &'a mut Files,
    pub(crate) outbox: &'a mut Vec<ClientMsg>,
    pub(crate) web: &'a mut dyn AppView,
    pub(crate) requests: &'a mut Vec<Request>,
    pub(crate) links: Links<'a>,
}

impl ArtifactCx<'_> {
    /// The version's size as saved.
    pub fn size(&self) -> u64 {
        self.artifact.version(self.version).map_or(0, |v| v.size)
    }

    /// The version's bytes (fetched the first time).
    pub fn bytes(&mut self) -> Fetch<&[u8]> {
        match self.files.fetch(self.outbox, &self.artifact.id, self.version) {
            File::Requested => Fetch::Loading,
            File::Failed(e) => Fetch::Failed(e.clone()),
            File::Ready { bytes, .. } => Fetch::Ready(bytes),
        }
    }

    /// What `make` makes of the version's bytes, made once and kept with them.
    pub fn prepared<T: Any>(&mut self, make: impl FnOnce(&[u8]) -> Result<T, String>) -> Fetch<&mut T> {
        match self.files.fetch(self.outbox, &self.artifact.id, self.version) {
            File::Requested => Fetch::Loading,
            File::Failed(e) => Fetch::Failed(e.clone()),
            File::Ready { bytes, prepared } => {
                let want = TypeId::of::<T>();
                if prepared.as_ref().is_none_or(|(ty, _)| *ty != want) {
                    *prepared = Some((want, make(bytes).map(|v| Box::new(v) as Box<dyn Any>)));
                }
                match prepared {
                    Some((_, Ok(v))) => (**v).downcast_mut::<T>().map_or_else(|| Fetch::Failed("can't show it".into()), Fetch::Ready),
                    Some((_, Err(e))) => Fetch::Failed(e.clone()),
                    None => Fetch::Loading,
                }
            }
        }
    }

    /// The web surface: where HTML runs (Servo helper, iframe, or a placeholder).
    pub fn web(&mut self) -> &mut dyn AppView {
        &mut *self.web
    }

    /// Where this version is served for a browser (theme in the hash).
    pub fn url(&self) -> Result<String, String> {
        self.links.url(&self.artifact.id, self.version)
    }

    /// Open this version in the system browser.
    pub fn open_in_browser(&mut self) {
        self.requests.push(Request::Open { id: self.artifact.id.clone(), version: self.version });
    }

    /// Send the model `text` as if the user typed it.
    pub fn send_message(&mut self, text: String) {
        self.requests.push(Request::Send(text));
    }
}

/// The ready value of `fetch`, or its loading/failed state drawn as fits `place`.
pub fn ready<T>(ui: &mut egui::Ui, t: &Tokens, place: Place, filename: &str, fetch: Fetch<T>) -> Option<T> {
    match (fetch, place) {
        (Fetch::Ready(v), _) => return Some(v),
        (Fetch::Loading, Place::Card) => {
            crate::theme::spinner(ui, t, Some(12.0));
        }
        (Fetch::Loading, Place::Pane) => {
            ui.centered_and_justified(|ui| crate::theme::spinner(ui, t, None));
        }
        (Fetch::Failed(e), Place::Card) => {
            ui.label(RichText::new(format!("Couldn't show it: {e}")).small().color(t.text_dim));
        }
        (Fetch::Failed(e), Place::Pane) => {
            ui.label(RichText::new(format!("Couldn't show {filename}: {e}")).color(t.text_dim));
        }
    }
    None
}

/// The bytes as text, read-only, filling the space left (the source view).
pub fn source(ui: &mut egui::Ui, text: &str) {
    egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
        let mut view = text;
        ui.add(egui::TextEdit::multiline(&mut view).code_editor().desired_width(f32::INFINITY));
    });
}

/// The first non-empty lines of `text` for a card, cut to one line each.
pub fn card_lines(ui: &mut egui::Ui, t: &Tokens, text: &str, line: impl Fn(&str) -> RichText) {
    const CARD_LINES: usize = 4;
    for l in text.lines().filter(|l| !l.trim().is_empty()).take(CARD_LINES) {
        ui.add(egui::Label::new(line(l).small().color(t.text_dim)).truncate());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use byob_protocol::ArtifactKind;
    use byob_protocol::artifact::{TYPE_CHART, TYPE_HTML, TYPE_JPEG, TYPE_MARKDOWN, TYPE_PNG, TYPE_TEXT};

    fn label(r: &ArtifactRenderers, ct: &str) -> String {
        r.get(ct).caps().label
    }

    #[test]
    fn types_resolve_exact_then_wildcard_then_fallback() {
        let mut r = ArtifactRenderers::default();
        assert_eq!(label(&r, TYPE_MARKDOWN), "document");
        assert_eq!(label(&r, TYPE_HTML), "app");
        assert_eq!(label(&r, TYPE_CHART), "chart");
        assert_eq!((label(&r, TYPE_PNG), label(&r, TYPE_JPEG)), ("image".into(), "image".into()));
        assert_eq!(label(&r, TYPE_TEXT), "file");
        assert_eq!(label(&r, "application/vnd.acme.sheet+json"), "file", "unknown types show their source");
        assert!(r.get(TYPE_HTML).caps().web_surface && r.get(TYPE_MARKDOWN).caps().source_view && !r.get(TYPE_PNG).caps().text);
        // A wildcard catches the rest of its major type; an exact one still wins.
        struct Pictures;
        impl ArtifactRenderer for Pictures {
            fn caps(&self) -> RendererCaps {
                RendererCaps { label: "picture".into(), text: false, source_view: false, web_surface: false }
            }
            fn card(&self, _: &mut egui::Ui, _: &mut ArtifactCx<'_>) {}
            fn pane(&self, _: &mut egui::Ui, _: &mut ArtifactCx<'_>) {}
        }
        r.register("image/*", Pictures);
        assert_eq!(label(&r, "image/webp"), "picture");
        assert_eq!(label(&r, TYPE_PNG), "image");
        // An artifact from an older engine (no content type) goes by its kind.
        let a = Artifact {
            id: "a".into(),
            kind: ArtifactKind::Chart,
            content_type: String::new(),
            title: "A".into(),
            filename: "a.chart.json".into(),
            versions: vec![],
        };
        assert_eq!(r.of(&a).caps().label, "chart");
        assert_eq!(ArtifactRenderers::empty().get(TYPE_MARKDOWN).caps().label, "file");
    }
}
