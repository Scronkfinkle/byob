//! Drawing [`View`]s: the cards under a step (see `byob_protocol::view`).
//!
//! The [`ViewRegistry`] maps a view kind to a renderer. byob's own cards register at startup
//! ([`ViewRegistry::default`]); anything else (a newer engine's card, a plugin's) shows its
//! markdown fallback. A renderer that can't read its view (data that doesn't fit, a newer
//! schema) returns `false` and the fallback shows instead.

use std::collections::{BTreeMap, BTreeSet};

use byob_protocol::{Entry, PluginCard, Step, View};
use egui::{Stroke, vec2};

use crate::markdown;
use crate::plugins::PluginsUi;
use crate::theme::Tokens;
use crate::tree::{StepEvent, StepTrees};

/// What a renderer draws with besides the view.
pub struct ViewCx<'a> {
    pub t: &'a Tokens,
    /// The step the view is on.
    pub step: u64,
    /// This is the newest view of its kind and [`Renderer::key`] in the chat (e.g. only a
    /// plugin's latest card offers its settings and Keep).
    pub latest: bool,
    /// The client's plugin state (the plugin card reads and changes it).
    pub plugins: &'a mut PluginsUi,
    /// The view's index on the step.
    pub index: usize,
    /// View trees on steps, decoded, with their client state.
    pub trees: &'a mut StepTrees,
}

/// Draws one view kind.
#[derive(Clone, Copy)]
pub struct Renderer {
    /// Draw `view`; `false`: can't (the fallback is drawn instead).
    pub draw: fn(&mut egui::Ui, &mut ViewCx<'_>, &View) -> bool,
    /// What views of this kind are "the same thing" for [`ViewCx::latest`] (e.g. the plugin
    /// id); `None`: every view is its own latest.
    pub key: Option<fn(&View) -> Option<String>>,
}

/// View kind → renderer.
pub struct ViewRegistry {
    renderers: BTreeMap<String, Renderer>,
    /// Parsed fallbacks, by step (one part per view).
    fallbacks: markdown::Cache,
    /// Kinds already logged as unknown.
    logged: BTreeSet<String>,
    trees: StepTrees,
}

impl Default for ViewRegistry {
    /// With byob's own renderers.
    fn default() -> Self {
        let mut r = Self::empty();
        r.register(<PluginCard as byob_protocol::ViewData>::KIND, Renderer { draw: plugin_card, key: Some(plugin_card_key) });
        r.register(byob_protocol::tree::KIND, Renderer { draw: tree_view, key: None });
        r
    }
}

/// Markdown cache keys of fallbacks: entry ids, delegation keys (top bit) and these differ.
fn fallback_key(step: u64) -> u64 {
    (step & !(1 << 63)) | (1 << 62)
}

impl ViewRegistry {
    /// No renderers: every view shows its fallback.
    pub fn empty() -> Self {
        Self { renderers: BTreeMap::new(), fallbacks: markdown::Cache::default(), logged: BTreeSet::new(), trees: StepTrees::default() }
    }

    /// What the user did in view trees on steps since the last call (sent on by the app
    /// once the engine routes them to their owners).
    pub fn take_tree_events(&mut self) -> Vec<StepEvent> {
        self.trees.take_events()
    }

    /// Draw views of `kind` with `renderer` (replacing an earlier one).
    pub fn register(&mut self, kind: &str, renderer: Renderer) {
        self.renderers.insert(kind.to_owned(), renderer);
    }

    pub fn has(&self, kind: &str) -> bool {
        self.renderers.contains_key(kind)
    }

    /// For each (kind, key), the step with the newest such view among `entries`' steps.
    pub fn latest(&self, entries: &[Entry]) -> BTreeMap<(String, String), u64> {
        let mut out = BTreeMap::new();
        for step in entries.iter().flat_map(|e| &e.steps) {
            for v in &step.views {
                if let Some(key) = self.renderers.get(&v.kind).and_then(|r| r.key).and_then(|k| k(v)) {
                    out.insert((v.kind.clone(), key), step.id);
                }
            }
        }
        out
    }

    /// Draw a step's views, in order.
    pub fn show_step(&mut self, ui: &mut egui::Ui, t: &Tokens, step: &Step, latest: &BTreeMap<(String, String), u64>, plugins: &mut PluginsUi) {
        for (i, view) in step.views.iter().enumerate() {
            let renderer = self.renderers.get(&view.kind).copied();
            let is_latest = match renderer.and_then(|r| r.key).and_then(|k| k(view)) {
                Some(key) => latest.get(&(view.kind.clone(), key)) == Some(&step.id),
                None => true,
            };
            let mut cx = ViewCx { t, step: step.id, latest: is_latest, plugins: &mut *plugins, index: i, trees: &mut self.trees };
            let drawn = renderer.is_some_and(|r| (r.draw)(ui, &mut cx, view));
            if !drawn {
                if self.logged.insert(view.kind.clone()) {
                    log::info!("no renderer for view {} v{}; showing its fallback", view.kind, view.v);
                }
                self.fallback(ui, t, step.id, i, view);
            }
        }
    }

    /// A view's markdown fallback, in a card frame.
    fn fallback(&mut self, ui: &mut egui::Ui, t: &Tokens, step: u64, i: usize, view: &View) {
        let text = view.fallback_bounded();
        let text = if text.trim().is_empty() { "*(something this version of byob can't show)*" } else { text };
        let doc = self.fallbacks.part(fallback_key(step), i, text, true);
        let frame =
            egui::Frame::new().fill(t.code_bg).stroke(Stroke::new(1.0, t.border)).corner_radius(t.round(8.0)).inner_margin(vec2(t.pad(10.0), t.pad(8.0)));
        frame.show(ui, |ui| {
            ui.set_width(ui.available_width());
            markdown::show(ui, t, doc, egui::Id::unique(("view-fallback", step, i)));
        });
    }
}

fn plugin_card(ui: &mut egui::Ui, cx: &mut ViewCx<'_>, view: &View) -> bool {
    let Some(card) = view.get::<PluginCard>() else { return false };
    cx.plugins.card(ui, cx.t, cx.step, &card, cx.latest);
    true
}

/// `byob.tree`: any step view can be a view tree.
fn tree_view(ui: &mut egui::Ui, cx: &mut ViewCx<'_>, view: &View) -> bool {
    // A plugin's live card: drawn by the sites, its events go to the plugin.
    if view.owner.is_some() {
        return cx.plugins.sites.card(ui, cx.t, view);
    }
    cx.trees.show(ui, cx.t, cx.step, cx.index, view)
}

fn plugin_card_key(view: &View) -> Option<String> {
    view.get::<PluginCard>().map(|c| c.plugin)
}
