//! Artifacts drawn by a plugin's live view (`{"type": …, "renderer": "view:<view id>"}`):
//! the artifact's card and pane show the plugin's view tree for the version shown, keyed
//! `"<artifact id>@<version>"` (`byob_protocol::sites::artifact_key`). The plugin's backend
//! draws it from the version's file and gets what the user does in it (a spreadsheet grid,
//! say), like any render site (`crate::sites`).
//!
//! [`ArtifactViews`] holds those views on this client: one tree per view (the engine's),
//! with a [`TreeState`] per place it is drawn (the card in the chat and the pane differ, so
//! their selections and scroll positions don't fight). A place moving on to a newer version
//! of the same artifact takes its state along (the user's selection survives their own
//! edit becoming a new version) and shows the old tree until the new one comes. Views no
//! place drew for a while are closed.

use std::collections::HashMap;

use byob_protocol::sites::{LiveView, artifact_key};
use byob_protocol::tree::{self, Node, Reply};
use byob_protocol::{ClientMsg, ViewRef};
use egui::RichText;

use super::{ArtifactCx, ArtifactRenderer, Place, RendererCaps};
use crate::theme::Tokens;
use crate::tree::{TreeState, show_tree};

/// Height of a view in a chat card.
pub const CARD_HEIGHT: f32 = 260.0;
/// Seconds a view nobody drew stays open.
const KEEP_SECS: f64 = 10.0;

/// Draws an artifact type with plugin `plugin`'s view `view`.
pub struct ViewRenderer {
    pub plugin: String,
    pub view: String,
    /// The badge ("spreadsheet").
    pub label: String,
}

impl ViewRenderer {
    fn view_ref(&self, cx: &ArtifactCx<'_>) -> ViewRef {
        ViewRef::new(&self.plugin, &self.view, artifact_key(&cx.artifact.id, cx.version))
    }

    fn draw(&self, ui: &mut egui::Ui, cx: &mut ArtifactCx<'_>, place: Place) {
        let r = self.view_ref(cx);
        // The pane's state follows the artifact across versions; each card (one per step,
        // each pinned to its version) has its own.
        let version = if place == Place::Card { cx.version } else { 0 };
        let salt = egui::Id::unique(("artifact-view", place == Place::Card, &cx.artifact.id, version, &self.plugin, &self.view));
        let t = cx.t;
        match place {
            Place::Card => {
                ui.allocate_ui(egui::vec2(ui.available_width(), CARD_HEIGHT), |ui| {
                    ui.set_max_height(CARD_HEIGHT);
                    cx.views.draw(ui, t, &r, salt, Some(CARD_HEIGHT), true);
                });
            }
            Place::Pane => {
                let h = ui.available_height().max(160.0);
                cx.views.draw(ui, t, &r, salt, Some(h), false);
            }
        }
    }
}

impl ArtifactRenderer for ViewRenderer {
    fn caps(&self) -> RendererCaps {
        RendererCaps { label: self.label.clone(), text: false, source_view: false, web_surface: false }
    }

    fn card(&self, ui: &mut egui::Ui, cx: &mut ArtifactCx<'_>) {
        self.draw(ui, cx, Place::Card);
    }

    fn pane(&self, ui: &mut egui::Ui, cx: &mut ArtifactCx<'_>) {
        self.draw(ui, cx, Place::Pane);
    }
}

/// One view as this client has it.
#[derive(Default)]
struct Slot {
    tree: Option<Node>,
    /// Another version's tree, shown until this one's comes.
    stale: Option<Node>,
    error: Option<String>,
    faults: Vec<String>,
    /// Client state per place it is drawn (by salt).
    states: HashMap<egui::Id, TreeState>,
    /// `OpenView` was sent.
    opened: bool,
    /// When a place last drew it (`egui` time).
    drawn: f64,
}

/// The artifact views of the chat in view.
#[derive(Default)]
pub struct ArtifactViews {
    /// The engine lists `view_sites`.
    enabled: bool,
    slots: HashMap<ViewRef, Slot>,
    out: Vec<ClientMsg>,
    now: f64,
}

impl ArtifactViews {
    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
    }

    /// Another chat: forget everything (the engine did too).
    pub fn clear(&mut self) {
        self.slots.clear();
    }

    /// A `LiveView` for one of ours: take it (true), else leave it to the other sites.
    pub fn live(&mut self, lv: &LiveView) -> bool {
        let Some(slot) = self.slots.get_mut(&lv.view) else { return false };
        if lv.reset {
            slot.tree = None;
            slot.error = None;
            slot.opened = false;
            return true;
        }
        slot.faults.clone_from(&lv.faults);
        if let Some(e) = &lv.error {
            slot.error = Some(e.clone());
            return true;
        }
        slot.error = None;
        match (&lv.tree, slot.tree.as_mut()) {
            (Some(t), _) => {
                slot.tree = Some(t.clone());
                slot.stale = None;
            }
            (None, Some(tree)) => {
                let faults = Reply { view: None, patch: lv.patch.clone(), values: Default::default() }.apply_to(tree);
                if !faults.is_empty() {
                    log::warn!("view {}/{}: patches that didn't apply: {}", lv.view.plugin, lv.view.view, tree::Fault::report(&faults));
                }
            }
            (None, None) => {}
        }
        for st in slot.states.values_mut() {
            st.replied(&lv.values);
        }
        true
    }

    /// Messages for the engine since the last call; views nobody drew for a while closed.
    pub fn take(&mut self) -> Vec<ClientMsg> {
        let now = self.now;
        let stale: Vec<ViewRef> = self.slots.iter().filter(|(_, s)| now - s.drawn > KEEP_SECS).map(|(r, _)| r.clone()).collect();
        for r in stale {
            if self.slots.remove(&r).is_some_and(|s| s.opened) && self.enabled {
                self.out.push(ClientMsg::CloseView { view: r });
            }
        }
        std::mem::take(&mut self.out)
    }

    /// A view is still on its way.
    pub fn loading(&self) -> bool {
        self.enabled && self.slots.values().any(|s| s.opened && s.tree.is_none() && s.error.is_none())
    }

    /// Draw view `r` at the place `salt` names: `height` is what a grid without its own
    /// fills, `compact` for chat cards.
    pub fn draw(&mut self, ui: &mut egui::Ui, t: &Tokens, r: &ViewRef, salt: egui::Id, height: Option<f32>, compact: bool) {
        self.now = ui.input(|i| i.time);
        if !self.enabled {
            ui.label(RichText::new("This byob can't show plugin views here.").small().color(t.text_dim));
            return;
        }
        // A place moving on from another version of this artifact takes its state along
        // (and shows that version's tree until this one comes).
        let artifact = r.key.rsplit_once('@').map_or(r.key.as_str(), |(id, _)| id);
        let mut inherited: Option<(TreeState, Option<Node>)> = None;
        if !self.slots.get(r).is_some_and(|s| s.states.contains_key(&salt)) {
            let from = self
                .slots
                .iter()
                .filter(|(o, s)| {
                    *o != r
                        && o.plugin == r.plugin
                        && o.view == r.view
                        && o.key.rsplit_once('@').is_some_and(|(id, _)| id == artifact)
                        && s.states.contains_key(&salt)
                })
                .map(|(o, _)| o.clone())
                .next();
            if let Some(o) = from
                && let Some(s) = self.slots.get_mut(&o)
                && let Some(st) = s.states.remove(&salt)
            {
                inherited = Some((st, s.tree.clone()));
            }
        }
        let slot = self.slots.entry(r.clone()).or_default();
        slot.drawn = self.now;
        if !slot.opened {
            slot.opened = true;
            self.out.push(ClientMsg::OpenView { view: r.clone() });
        }
        let state = match inherited {
            Some((st, tree)) => {
                if slot.tree.is_none() {
                    slot.stale = tree;
                }
                st
            }
            None => TreeState::with_id(salt.with("tree")),
        };
        let st = slot.states.entry(salt).or_insert(state);
        st.set_fill_height(height);
        st.set_compact(compact);
        let shown = slot.tree.as_ref().or(slot.stale.as_ref());
        match (shown, &slot.error) {
            (Some(tree), _) => {
                let events = show_tree(ui, t, tree, st);
                if slot.tree.is_some() {
                    for event in events {
                        self.out.push(ClientMsg::ViewEvent { view: r.clone(), event });
                    }
                }
                if let Some(e) = &slot.error {
                    ui.label(RichText::new(format!("⚠ {e}")).small().color(t.text_faint)).on_hover_text("This shows the last state the plugin sent");
                }
            }
            (None, Some(e)) => {
                let short: String = e.lines().next().unwrap_or_default().chars().take(200).collect();
                ui.add(egui::Label::new(RichText::new(format!("⚠ {short}")).small().color(t.danger)).wrap()).on_hover_text(e);
            }
            (None, None) => {
                ui.horizontal(|ui| {
                    crate::theme::spinner(ui, t, None);
                    ui.label(RichText::new("Loading…").small().color(t.text_dim));
                });
            }
        }
        if !slot.faults.is_empty() {
            let n = slot.faults.len();
            let text = if n == 1 { "⚠ 1 problem in this view".to_owned() } else { format!("⚠ {n} problems in this view") };
            ui.label(RichText::new(text).small().color(t.warning)).on_hover_text(slot.faults.join("\n"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use byob_protocol::tree::{Kind, TextStyle, Tone};

    fn frame(ctx: &egui::Context, t: f64, f: &mut dyn FnMut(&mut egui::Ui)) {
        let input = egui::RawInput { time: Some(t), ..Default::default() };
        let mut out = ctx.run_ui(input, |ui| f(ui));
        out.textures_delta.clear();
    }

    fn text(s: &str) -> Node {
        Node::new(Kind::Text { text: s.into(), style: TextStyle::Body, tone: Tone::Normal })
    }

    #[test]
    fn a_view_opens_takes_its_tree_and_moves_on_to_newer_versions() {
        let ctx = egui::Context::default();
        let t = Tokens::dark();
        let mut v = ArtifactViews::default();
        v.set_enabled(true);
        let v1 = ViewRef::new("sheets", "sheet", artifact_key("budget-1a2b", 1));
        let pane = egui::Id::unique("pane");
        frame(&ctx, 0.0, &mut |ui| v.draw(ui, &t, &v1, pane, Some(400.0), false));
        assert_eq!(v.take(), vec![ClientMsg::OpenView { view: v1.clone() }]);
        assert!(v.loading());
        // Not ours: left to the other sites.
        assert!(!v.live(&LiveView { view: ViewRef::new("todo", "list", ""), reset: true, ..Default::default() }));
        assert!(v.live(&LiveView { view: v1.clone(), tree: Some(text("v1")), ..Default::default() }));
        assert!(!v.loading());
        // Version 2: the pane's state comes along and v1's tree shows until v2's comes.
        let v2 = ViewRef::new("sheets", "sheet", artifact_key("budget-1a2b", 2));
        frame(&ctx, 1.0, &mut |ui| v.draw(ui, &t, &v2, pane, Some(400.0), false));
        assert_eq!(v.take(), vec![ClientMsg::OpenView { view: v2.clone() }]);
        assert!(v.slots[&v2].stale.is_some() && v.slots[&v2].states.contains_key(&pane));
        assert!(!v.slots[&v1].states.contains_key(&pane));
        v.live(&LiveView { view: v2.clone(), tree: Some(text("v2")), ..Default::default() });
        assert!(v.slots[&v2].stale.is_none());
        // v1, drawn nowhere for a while, is closed.
        frame(&ctx, 20.0, &mut |ui| v.draw(ui, &t, &v2, pane, Some(400.0), false));
        assert_eq!(v.take(), vec![ClientMsg::CloseView { view: v1 }]);
    }

    #[test]
    fn plugins_name_their_view_as_the_renderer() {
        let mut r = super::super::ArtifactRenderers::default();
        let mut p = crate::plugins::tests::info("sheets", byob_protocol::PluginScope::Bundled, byob_protocol::PluginOrigin::Bundled);
        p.views = vec![byob_protocol::ViewDecl { id: "sheet".into(), site: byob_protocol::Site::Artifact, title: "Spreadsheet".into(), icon: String::new() }];
        p.renderers = vec![byob_protocol::RendererDecl { content_type: "text/csv".into(), renderer: "view:sheet".into() }];
        r.apply_plugins(&[p]);
        let caps = r.get("text/csv").caps();
        assert_eq!(caps.label, "spreadsheet");
        assert!(!caps.text && !caps.web_surface);
    }
}
