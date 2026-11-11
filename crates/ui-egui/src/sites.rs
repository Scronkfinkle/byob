//! Plugins' live views at the client's render sites (`byob_protocol::sites`): the side
//! pane (beside the chat and the artifact pane), header status items, a section under the
//! plugin in Settings → Plugins, and cards on steps.
//!
//! The engine owns every view's tree. A site that draws a view the first time sends
//! `OpenView`; the engine answers with the tree, then patches whenever the plugin changes
//! it. What the user does goes back as `ViewEvent`. The client keeps only transient state
//! ([`TreeView`]: typed text, scroll, selection), so every client shows the same thing.
//! A view the engine can't have (no backend, the backend failed) shows a small error box; a
//! view whose tree the engine had to fix shows a warning line (the plugin's status has the
//! details, for the model to fix). Without the `view_sites` cap nothing is opened: cards show
//! their stored snapshot and the other sites stay empty.
//!
//! Image nodes are loaded here: workspace paths with `GetMedia`, artifacts with
//! `GetArtifact` (their latest version), decoded once into textures.

use std::collections::{BTreeMap, HashMap};

use byob_protocol::sites::LiveView;
use byob_protocol::tree::{self, Node, Reply};
use byob_protocol::{ClientMsg, Extensions, MediaContent, ServerMsg, Site, View, ViewDecl, ViewRef};
use egui::{RichText, Stroke, vec2};

use crate::theme::Tokens;
use crate::tree::{Images, TreeState, TreeView, show_tree_with};

/// Width of the pane before the user drags it.
const PANE_WIDTH: f32 = 360.0;
/// Widest a header status item gets.
const STATUS_WIDTH: f32 = 220.0;

/// A view a plugin in the chat declares, while the plugin is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declared {
    pub plugin: String,
    pub plugin_name: String,
    pub decl: ViewDecl,
}

impl Declared {
    pub fn view_ref(&self) -> ViewRef {
        ViewRef::new(&self.plugin, &self.decl.id, "")
    }

    fn title(&self) -> &str {
        if self.decl.title.is_empty() { &self.plugin_name } else { &self.decl.title }
    }
}

/// One view as this client has it.
#[derive(Default)]
struct Slot {
    view: Option<TreeView>,
    error: Option<String>,
    faults: Vec<String>,
    /// `OpenView` was sent.
    opened: bool,
}

/// The client's live views and the pane.
#[derive(Default)]
pub struct Sites {
    /// The engine lists `view_sites`.
    enabled: bool,
    declared: Vec<Declared>,
    slots: HashMap<ViewRef, Slot>,
    out: Vec<ClientMsg>,
    /// The pane is open (on a phone: full screen), showing `active`.
    pane_open: bool,
    active: Option<ViewRef>,
    images: SiteImages,
    /// How wide each header status item was last drawn (the header lays out right to left,
    /// so an item gets exactly its width).
    status_w: HashMap<ViewRef, f32>,
}

impl Sites {
    /// The engine handles live views (from its `Welcome`).
    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
    }

    /// Look at a server message others handle too (call it for every message, before
    /// anything else takes it): another chat opened, artifact lists, media and files.
    pub fn peek(&mut self, msg: &ServerMsg) {
        self.images.peek(msg);
        if let ServerMsg::ChatOpened { .. } = msg {
            // The engine forgot what we had open: the new chat's views resolve anew.
            self.slots.clear();
            self.images.clear();
        }
    }

    /// Take in a server message; true if it was only for us (`LiveView`).
    pub fn apply(&mut self, msg: &ServerMsg) -> bool {
        match msg {
            ServerMsg::LiveView(lv) => {
                self.live(lv.clone());
                true
            }
            ServerMsg::Extensions(e) => {
                self.extensions(e);
                false
            }
            _ => false,
        }
    }

    fn extensions(&mut self, e: &Extensions) {
        self.declared = e
            .plugins
            .iter()
            .filter(|p| p.shadowed_by.is_none() && p.enabled && p.trusted && p.backend.as_ref().is_some_and(|b| b.state != "off"))
            .flat_map(|p| p.views.iter().map(|d| Declared { plugin: p.id.clone(), plugin_name: p.name.clone(), decl: d.clone() }))
            .collect();
        let declared = &self.declared;
        let gone: Vec<ViewRef> = self.slots.keys().filter(|r| !declared.iter().any(|d| d.plugin == r.plugin && d.decl.id == r.view)).cloned().collect();
        for r in gone {
            if self.slots.remove(&r).is_some_and(|s| s.opened) {
                self.out.push(ClientMsg::CloseView { view: r });
            }
        }
        if self.active.as_ref().is_none_or(|a| !self.panes().any(|d| d.view_ref() == *a)) {
            let first = self.panes().next().map(Declared::view_ref);
            self.active = first;
        }
        if self.active.is_none() {
            self.pane_open = false;
        }
    }

    fn live(&mut self, lv: LiveView) {
        if lv.reset {
            self.slots.remove(&lv.view);
            return;
        }
        let r = lv.view.clone();
        let slot = self.slots.entry(r.clone()).or_default();
        slot.faults = lv.faults;
        if let Some(e) = lv.error {
            slot.error = Some(e);
            return;
        }
        slot.error = None;
        match (lv.tree, slot.view.as_mut()) {
            (Some(t), Some(v)) => {
                v.tree = t;
                v.state.replied(&lv.values);
            }
            (Some(t), None) => {
                let mut v = TreeView::new(t);
                v.state = TreeState::with_id(egui::Id::unique(("live-view", &r)));
                v.state.replied(&lv.values);
                slot.view = Some(v);
            }
            (None, Some(v)) => {
                let faults = v.reply(Reply { view: None, patch: lv.patch, values: lv.values });
                if !faults.is_empty() {
                    log::warn!("view {}/{}: patches that didn't apply: {}", r.plugin, r.view, tree::Fault::report(&faults));
                }
            }
            (None, None) => {}
        }
    }

    /// Messages for the engine since the last call.
    pub fn take(&mut self) -> Vec<ClientMsg> {
        let mut out = std::mem::take(&mut self.out);
        out.extend(self.images.take());
        out
    }

    /// Something is still on its way (for scripted screenshots).
    pub fn loading(&self) -> bool {
        self.slots.values().any(|s| s.opened && s.view.is_none() && s.error.is_none()) || self.images.loading()
    }

    pub fn declared(&self) -> &[Declared] {
        &self.declared
    }

    fn panes(&self) -> impl Iterator<Item = &Declared> {
        self.declared.iter().filter(|d| d.decl.site == Site::Pane)
    }

    /// Draw view `r`, opening it first. `snapshot`: what to show until the live tree comes
    /// (a card's stored state), and all there is without the cap.
    fn draw(&mut self, ui: &mut egui::Ui, t: &Tokens, r: &ViewRef, snapshot: Option<&Node>) {
        let enabled = self.enabled;
        let slot = self.slots.entry(r.clone()).or_default();
        if enabled && !slot.opened {
            slot.opened = true;
            self.out.push(ClientMsg::OpenView { view: r.clone() });
        }
        if slot.view.is_none()
            && let Some(s) = snapshot
        {
            let mut v = TreeView::new(s.clone());
            v.state = TreeState::with_id(egui::Id::unique(("live-view", r)));
            slot.view = Some(v);
        }
        match (&mut slot.view, &slot.error) {
            (Some(v), _) => {
                let events = show_tree_with(ui, t, &v.tree, &mut v.state, Some(&mut self.images));
                if enabled {
                    for event in events {
                        self.out.push(ClientMsg::ViewEvent { view: r.clone(), event });
                    }
                }
                if let Some(e) = &slot.error {
                    ui.label(RichText::new(format!("⚠ {e}")).small().color(t.text_faint)).on_hover_text("This shows the last state the plugin sent");
                }
            }
            (None, Some(e)) => error_box(ui, t, e),
            (None, None) if enabled => {
                ui.horizontal(|ui| {
                    crate::theme::spinner(ui, t, None);
                    ui.label(RichText::new("Loading…").small().color(t.text_dim));
                });
            }
            (None, None) => {}
        }
        if !slot.faults.is_empty() {
            let n = slot.faults.len();
            let text = if n == 1 { "⚠ 1 problem in this view".to_owned() } else { format!("⚠ {n} problems in this view") };
            ui.label(RichText::new(text).small().color(t.warning))
                .on_hover_text(format!("byob drew what it could; the plugin's status has the details (ask byob to fix it):\n{}", slot.faults.join("\n")));
        }
    }

    // ---- cards -----------------------------------------------------------------------

    /// A step's `byob.tree` view owned by a plugin: the live card (its stored state until
    /// the live one comes). `false` if it isn't one this client can draw.
    pub fn card(&mut self, ui: &mut egui::Ui, t: &Tokens, view: &View) -> bool {
        let Some(owner) = &view.owner else { return false };
        if view.kind != tree::KIND || view.v > tree::VERSION {
            return false;
        }
        let snapshot = tree::decode(view.data.clone()).0;
        ui.scope(|ui| self.draw(ui, t, owner, Some(&snapshot)));
        true
    }

    // ---- header ----------------------------------------------------------------------

    /// The header's status items (right to left, like the rest of the header's actions).
    pub fn status_items(&mut self, ui: &mut egui::Ui, t: &Tokens) {
        let items: Vec<Declared> = self.declared.iter().filter(|d| d.decl.site == Site::Status).cloned().collect();
        for d in items.iter().rev() {
            let r = d.view_ref();
            let w = self.status_w.get(&r).copied().unwrap_or(STATUS_WIDTH);
            let shown = ui.allocate_ui_with_layout(vec2(w, ui.available_height()), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.set_max_width(STATUS_WIDTH);
                self.draw(ui, t, &r, None);
            });
            let used = shown.response.rect.width().clamp(4.0, STATUS_WIDTH);
            if (used - w).abs() > 0.5 {
                self.status_w.insert(r, used);
                ui.ctx().request_repaint();
            }
            shown.response.on_hover_text(format!("{} ({})", d.title(), d.plugin_name));
        }
    }

    /// The header's pane toggles: one per pane view.
    pub fn pane_toggles(&mut self, ui: &mut egui::Ui, t: &Tokens) {
        let panes: Vec<Declared> = self.panes().cloned().collect();
        for d in panes.iter().rev() {
            let r = d.view_ref();
            let on = self.pane_open && self.active.as_ref() == Some(&r);
            let label = RichText::new(d.title()).color(if on { t.text_strong } else { t.text });
            if ui.selectable_label(on, label).on_hover_text(format!("Show or hide {} ({})", d.title(), d.plugin_name)).clicked() {
                self.toggle_pane(Some(r));
            }
        }
    }

    /// Open the pane on `view` (the first pane view if `None`), or close it if that is what
    /// it shows.
    pub fn toggle_pane(&mut self, view: Option<ViewRef>) {
        let view = view.or_else(|| self.panes().next().map(Declared::view_ref));
        if self.pane_open && view == self.active {
            self.pane_open = false;
        } else if view.is_some() {
            self.active = view;
            self.pane_open = true;
        }
    }

    /// Scripted runs: show pane view `view` (`plugin/view`, or any when empty).
    pub fn open_pane(&mut self, view: &str) -> bool {
        let found = self.panes().find(|d| view.is_empty() || format!("{}/{}", d.plugin, d.decl.id) == view || d.plugin == view).map(Declared::view_ref);
        match found {
            Some(r) => {
                self.active = Some(r);
                self.pane_open = true;
                true
            }
            None => false,
        }
    }

    /// The pane is open (full screen on a phone).
    pub fn pane_is_open(&self) -> bool {
        self.pane_open && self.active.is_some()
    }

    // ---- the pane --------------------------------------------------------------------

    /// The pane on the right (wide windows; call before the central panel).
    pub fn side_panel(&mut self, ui: &mut egui::Ui, t: &Tokens) {
        if !self.pane_is_open() {
            return;
        }
        let max = (ui.available_width() * 0.6).max(280.0);
        egui::Panel::right("plugin-pane").resizable(true).default_size(PANE_WIDTH.min(max)).size_range(240.0..=max).show(ui, |ui| self.pane(ui, t, false));
    }

    /// The pane's contents: its views as tabs, the one in front, × to close.
    pub fn pane(&mut self, ui: &mut egui::Ui, t: &Tokens, narrow: bool) {
        let Some(active) = self.active.clone() else { return };
        let panes: Vec<Declared> = self.panes().cloned().collect();
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if narrow && ui.button("← Chat").clicked() {
                self.pane_open = false;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(egui::Button::new(RichText::new("×").size(16.0).color(t.text_dim)).frame(false)).on_hover_text("Close the pane").clicked() {
                    self.pane_open = false;
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    for d in &panes {
                        let r = d.view_ref();
                        let on = r == active;
                        let fill = if on { t.assistant_bubble } else { t.code_bg };
                        let stroke = if on { t.accent } else { t.border };
                        let resp = egui::Frame::new()
                            .fill(fill)
                            .stroke(Stroke::new(1.0, stroke))
                            .corner_radius(t.round(6.0))
                            .inner_margin(vec2(t.pad(6.0), t.pad(2.0)))
                            .show(ui, |ui| {
                                ui.add(
                                    egui::Label::new(RichText::new(d.title()).color(if on { t.text_strong } else { t.text_dim })).sense(egui::Sense::click()),
                                )
                            })
                            .inner
                            .on_hover_text(format!("{} · {}", d.title(), d.plugin_name));
                        if resp.clicked() {
                            self.active = Some(r);
                        }
                    }
                });
            });
        });
        ui.separator();
        let Some(active) = self.active.clone() else { return };
        egui::ScrollArea::vertical().id_salt(("plugin-pane-scroll", &active)).auto_shrink([false, false]).show(ui, |ui| {
            ui.add_space(4.0);
            self.draw(ui, t, &active, None);
        });
    }

    // ---- settings --------------------------------------------------------------------

    /// Plugin `plugin`'s settings sections (in its row of Settings → Plugins).
    pub fn settings_sections(&mut self, ui: &mut egui::Ui, t: &Tokens, plugin: &str) {
        let items: Vec<Declared> = self.declared.iter().filter(|d| d.plugin == plugin && d.decl.site == Site::Settings).cloned().collect();
        for d in items {
            let r = d.view_ref();
            egui::Frame::new().stroke(Stroke::new(1.0, t.border)).corner_radius(t.round(6.0)).inner_margin(t.pad(8.0)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(d.title()).small().strong().color(t.text_dim));
                self.draw(ui, t, &r, None);
            });
        }
    }
}

fn error_box(ui: &mut egui::Ui, t: &Tokens, e: &str) {
    egui::Frame::new().stroke(Stroke::new(1.0, t.danger.gamma_multiply(0.6))).corner_radius(t.round(6.0)).inner_margin(t.pad(6.0)).show(ui, |ui| {
        let short: String = e.lines().next().unwrap_or_default().chars().take(160).collect();
        ui.add(egui::Label::new(RichText::new(format!("⚠ {short}")).small().color(t.danger)).wrap()).on_hover_text(e);
    });
}

// ---- images ---------------------------------------------------------------------------

enum Img {
    Requested,
    Got(Result<Vec<u8>, String>),
    Ready(egui::TextureHandle),
    Failed(String),
}

/// Pictures for image nodes, by `m:<path>` or `a:<id>@<version>`.
#[derive(Default)]
struct SiteImages {
    items: HashMap<String, Img>,
    wanted: Vec<ClientMsg>,
    /// Artifact id → latest version, from the chat's artifact list.
    latest: BTreeMap<String, u32>,
}

impl SiteImages {
    fn clear(&mut self) {
        self.items.clear();
        self.latest.clear();
    }

    fn take(&mut self) -> Vec<ClientMsg> {
        std::mem::take(&mut self.wanted)
    }

    fn loading(&self) -> bool {
        self.items.values().any(|i| matches!(i, Img::Requested | Img::Got(_)))
    }

    fn peek(&mut self, msg: &ServerMsg) {
        match msg {
            ServerMsg::Artifacts { artifacts, .. } => {
                self.latest = artifacts.iter().filter_map(|a| Some((a.id.clone(), a.latest()?.n))).collect();
            }
            ServerMsg::ArtifactUpdated(a) => {
                if let Some(v) = a.latest() {
                    self.latest.insert(a.id.clone(), v.n);
                }
            }
            ServerMsg::Media { path, content } => {
                if let Some(item @ Img::Requested) = self.items.get_mut(&format!("m:{path}")) {
                    *item = Img::Got(match content {
                        MediaContent::Image { data, .. } => byob_protocol::media::decode(data),
                        MediaContent::Chart { .. } => Err("that's a chart, not an image".into()),
                        MediaContent::Error { message } => Err(message.clone()),
                    });
                }
            }
            ServerMsg::ArtifactFile { id, version, data, error } => {
                if let Some(item @ Img::Requested) = self.items.get_mut(&format!("a:{id}@{version}")) {
                    *item = Img::Got(match (data, error) {
                        (Some(d), _) => byob_protocol::media::decode(d),
                        (None, e) => Err(e.clone().unwrap_or_else(|| "not available".into())),
                    });
                }
            }
            _ => {}
        }
    }
}

impl Images for SiteImages {
    fn image(&mut self, ctx: &egui::Context, media: Option<&str>, artifact: Option<&str>) -> Result<Option<egui::TextureHandle>, String> {
        let (key, request) = match (media, artifact) {
            (Some(path), _) => (format!("m:{path}"), ClientMsg::GetMedia { path: path.to_owned() }),
            (None, Some(id)) => {
                let Some(v) = self.latest.get(id).copied() else { return Err(format!("no artifact {id:?} in this chat")) };
                (format!("a:{id}@{v}"), ClientMsg::GetArtifact { id: id.to_owned(), version: v })
            }
            (None, None) => return Err(String::new()),
        };
        let item = self.items.entry(key.clone()).or_insert_with(|| {
            self.wanted.push(request);
            Img::Requested
        });
        if let Img::Got(got) = item {
            *item = match got.as_ref().map_err(Clone::clone).and_then(|b| crate::media::decode_image(b)) {
                Ok(img) => Img::Ready(ctx.load_texture(&key, img, egui::TextureOptions::LINEAR)),
                Err(e) => Img::Failed(e),
            };
        }
        match item {
            Img::Requested | Img::Got(_) => Ok(None),
            Img::Ready(tex) => Ok(Some(tex.clone())),
            Img::Failed(e) => Err(e.clone()),
        }
    }
}

#[cfg(test)]
mod tests;
