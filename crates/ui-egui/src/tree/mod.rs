//! Drawing view trees ([`byob_protocol::tree`]) natively with the theme [`Tokens`].
//!
//! [`show_tree`] draws a tree and returns what the user did as [`Event`]s. The tree is the
//! owner's (a plugin's, the engine's); the client keeps only transient state in a
//! [`TreeState`]: typed text, toggles and choices (by field `name`), the selected table row,
//! a cell being edited, scroll positions (by node `key`, so they survive replies). A field
//! takes the tree's value only when the tree changed it (or a reply forces it with
//! [`TreeState::replied`]), so a reply never clobbers what the user is typing.
//!
//! [`show_tree_with`] also draws image nodes, with the site's [`Images`].
//!
//! [`TreeView`] bundles a tree and its state for render sites that get replies;
//! [`StepTrees`] draws `byob.tree` views on steps (registered in the
//! [`crate::views::ViewRegistry`]).

use std::collections::{HashMap, HashSet};

use byob_protocol::tree::{CellEdit, Event, Fault, GRID_DEFAULT_WIDTH, GridEvent, Kind, Node, Reply, TextStyle, Tone};
use egui::{Color32, FontFamily, RichText, Stroke};
use serde_json::{Map, Value};

use crate::markdown;
use crate::theme::{self, Tokens};

mod canvas;
mod grid;
mod step;
mod table;
#[cfg(test)]
mod tests;

pub use step::{StepEvent, StepTrees};

/// Events kept from one frame (more are dropped: nobody clicks that fast).
const MAX_EVENTS_PER_FRAME: usize = 64;
/// Grids or tables of one name told apart, at most.
const MAX_SAME_NAME: u32 = 64;

/// Client-side state of one drawn tree.
#[derive(Default)]
pub struct TreeState {
    /// Widget ids are made from this (else from the `Ui`'s id).
    id: Option<egui::Id>,
    /// Field name → current value, as sent in events.
    values: Map<String, Value>,
    /// Field name → the value the tree last had: a field takes the tree's value only when it
    /// differs from this.
    seen: HashMap<String, Value>,
    /// Field name → the pass it was last drawn in (fields no longer drawn are dropped).
    live: HashMap<String, u64>,
    pass: u64,
    /// Cells edited here and not yet answered: (table, row, column) → text.
    pending: HashMap<(String, usize, usize), String>,
    editing: Option<Editing>,
    md: markdown::Cache,
    md_live: HashSet<u64>,
    queued: Vec<Queued>,
    /// Grid name → its selection, scroll, editor, pending edits, last window asked for.
    grids: HashMap<String, grid::GridState>,
    /// Height a grid without its own fills.
    fill_height: Option<f32>,
    /// Grids without their formula bar (chat cards).
    compact: bool,
}

/// A table cell being edited.
struct Editing {
    table: String,
    row: usize,
    col: usize,
    text: String,
    focused: bool,
}

struct Queued {
    action: String,
    edit: Option<CellEdit>,
    grid: Option<GridEvent>,
    /// Values for this event only (a canvas click's `_x`/`_y`).
    extra: Map<String, Value>,
}

impl TreeState {
    /// State whose widgets use ids made from `id` (two trees in one `Ui` need different ones).
    pub fn with_id(id: egui::Id) -> Self {
        Self { id: Some(id), ..Self::default() }
    }

    /// The current field values (what the next event will carry).
    pub fn values(&self) -> &Map<String, Value> {
        &self.values
    }

    /// A reply arrived: optimistic cell edits give way to the tree, and `values` (the reply's
    /// own) replace what the user typed.
    pub fn replied(&mut self, values: &Map<String, Value>) {
        self.pending.clear();
        self.grids.values_mut().for_each(grid::GridState::replied);
        for (k, v) in values {
            self.values.insert(k.clone(), v.clone());
        }
    }

    /// The height a grid without its own `height` fills (the artifact pane passes what it
    /// has; a chat card something small); `None`: a default.
    pub fn set_fill_height(&mut self, h: Option<f32>) {
        self.fill_height = h.filter(|h| h.is_finite() && *h > 0.0);
    }

    /// Compact grids (chat cards) have no formula bar.
    pub fn set_compact(&mut self, compact: bool) {
        self.compact = compact;
    }

    /// A grid's selection: (row, col, to_row, to_col), the active cell first.
    pub fn grid_selection(&self, name: &str) -> Option<(u32, u32, u32, u32)> {
        let v = self.values.get(name)?;
        let n = |k: &str| v.get(k).and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok());
        Some((n("row")?, n("col")?, n("to_row")?, n("to_col")?))
    }

    fn fire(&mut self, action: &str, edit: Option<CellEdit>, extra: Map<String, Value>) {
        if self.queued.len() < MAX_EVENTS_PER_FRAME {
            self.queued.push(Queued { action: action.to_owned(), edit, grid: None, extra });
        }
    }

    fn fire_grid(&mut self, e: GridEvent) {
        if self.queued.len() < MAX_EVENTS_PER_FRAME {
            self.queued.push(Queued { action: e.op.clone(), edit: None, grid: Some(e), extra: Map::new() });
        }
    }

    fn touch(&mut self, name: &str) {
        match self.live.get_mut(name) {
            Some(p) => *p = self.pass,
            None => {
                self.live.insert(name.to_owned(), self.pass);
            }
        }
    }

    /// The name a grid or table keeps its state under: `name`, or, when a field already took
    /// it this pass (two unnamed tables, a grid named like an input), `name-2`, `name-3`, …
    /// so they don't share (or clobber) each other's value. `explicit`: the tree named it (a
    /// clash is the tree's mistake, logged).
    fn claim<'n>(&self, name: &'n str, explicit: bool) -> std::borrow::Cow<'n, str> {
        let taken = |n: &str| self.live.get(n) == Some(&self.pass);
        if !taken(name) {
            return name.into();
        }
        for k in 2..=MAX_SAME_NAME {
            let other = format!("{name}-{k}");
            if !taken(&other) {
                if explicit {
                    log::warn!("view tree: the name {name:?} is used twice; this one goes by {other:?}");
                }
                return other.into();
            }
        }
        name.into()
    }

    /// Sync a field with the tree's value (taken only if the tree changed it); its current
    /// value.
    fn field(&mut self, name: &str, tree_value: impl FnOnce() -> Value, same: impl Fn(&Value) -> bool) -> &mut Value {
        self.touch(name);
        let changed = !self.seen.get(name).is_some_and(&same);
        if changed || !self.values.contains_key(name) {
            let v = tree_value();
            if changed {
                self.seen.insert(name.to_owned(), v.clone());
            }
            self.values.insert(name.to_owned(), v);
        }
        self.values.entry(name.to_owned()).or_insert(Value::Null)
    }
}

/// A tree and its client state, for a render site that sends events and gets replies.
pub struct TreeView {
    pub tree: Node,
    pub state: TreeState,
}

impl TreeView {
    pub fn new(tree: Node) -> Self {
        Self { tree, state: TreeState::default() }
    }

    /// Apply a reply; the faults are for its sender.
    pub fn reply(&mut self, mut r: Reply) -> Vec<Fault> {
        let values = std::mem::take(&mut r.values);
        let faults = r.apply_to(&mut self.tree);
        self.state.replied(&values);
        faults
    }

    pub fn show(&mut self, ui: &mut egui::Ui, t: &Tokens) -> Vec<Event> {
        show_tree(ui, t, &self.tree, &mut self.state)
    }
}

/// Where a render site gets the pictures of `image` nodes from (workspace files, artifacts).
pub trait Images {
    /// The texture for an image node's `media` path or `artifact` id: `Ok(None)` while it
    /// loads, `Err` if it can't be shown.
    fn image(&mut self, ctx: &egui::Context, media: Option<&str>, artifact: Option<&str>) -> Result<Option<egui::TextureHandle>, String>;
}

/// What drawing a node needs.
struct Cx<'a, 'i> {
    t: &'a Tokens,
    st: &'a mut TreeState,
    /// Ids of keyed nodes are made from this, so they don't move with their position.
    root: egui::Id,
    bold: FontFamily,
    images: Option<&'i mut dyn Images>,
}

/// Draw `tree`; what the user did this frame. Image nodes show as framed boxes with their
/// alt text (see [`show_tree_with`]).
pub fn show_tree(ui: &mut egui::Ui, t: &Tokens, tree: &Node, st: &mut TreeState) -> Vec<Event> {
    show_tree_with(ui, t, tree, st, None)
}

/// [`show_tree`], with `images` drawing image nodes.
pub fn show_tree_with(ui: &mut egui::Ui, t: &Tokens, tree: &Node, st: &mut TreeState, images: Option<&mut dyn Images>) -> Vec<Event> {
    st.pass = st.pass.wrapping_add(1);
    st.md_live.clear();
    let root = st.id.unwrap_or_else(|| ui.scope_id().with("byob-tree"));
    let bold = theme::bold_or_regular(ui.ctx());
    let mut cx = Cx { t, st: &mut *st, root, bold, images };
    ui.vertical(|ui| node(ui, &mut cx, tree, root));
    // Fields no longer in the tree are gone.
    let pass = st.pass;
    st.live.retain(|_, p| *p == pass);
    let live = &st.live;
    st.values.retain(|k, _| live.contains_key(k));
    st.seen.retain(|k, _| live.contains_key(k));
    let md_live = &st.md_live;
    st.md.retain(|id| md_live.contains(&id));
    st.grids.retain(|_, g| g.pass == pass);
    std::mem::take(&mut st.queued)
        .into_iter()
        .map(|q| {
            let mut values = st.values.clone();
            values.extend(q.extra);
            Event { action: q.action, values, edit: q.edit, grid: q.grid }
        })
        .collect()
}

fn node_id(cx: &Cx<'_, '_>, n: &Node, parent: egui::Id, i: usize) -> egui::Id {
    match &n.key {
        Some(k) => cx.root.with(("key", k)),
        None => parent.with(i),
    }
}

pub(crate) fn tone(t: &Tokens, tone: Tone) -> Color32 {
    match tone {
        Tone::Normal => t.text,
        Tone::Dim => t.text_dim,
        Tone::Accent => t.accent,
        Tone::Success => t.success,
        Tone::Warning => t.warning,
        Tone::Danger => t.danger,
    }
}

fn children(ui: &mut egui::Ui, cx: &mut Cx<'_, '_>, list: &[Node], id: egui::Id) {
    for (i, c) in list.iter().enumerate() {
        let cid = node_id(cx, c, id, i);
        node(ui, cx, c, cid);
    }
}

fn is_container(n: &Node) -> bool {
    matches!(n.kind, Kind::Card { .. } | Kind::Column { .. })
}

fn node(ui: &mut egui::Ui, cx: &mut Cx<'_, '_>, n: &Node, id: egui::Id) {
    let t = cx.t;
    match &n.kind {
        Kind::Column { children: list } => {
            ui.vertical(|ui| children(ui, cx, list, id));
        }
        // Cards or columns side by side share the width equally (egui has no flex layout).
        Kind::Row { children: list } if list.iter().any(is_container) => {
            ui.columns(list.len().max(1), |cols| {
                for (i, (ui, c)) in cols.iter_mut().zip(list).enumerate() {
                    let cid = node_id(cx, c, id, i);
                    node(ui, cx, c, cid);
                }
            });
        }
        Kind::Row { children: list } => {
            ui.horizontal_wrapped(|ui| children(ui, cx, list, id));
        }
        Kind::Card { title, children: list } => {
            egui::Frame::new().fill(t.panel_bg).stroke(Stroke::new(1.0, t.border)).corner_radius(t.round(8.0)).inner_margin(t.pad(12.0)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                if !title.is_empty() {
                    ui.label(RichText::new(title).family(cx.bold.clone()).size(t.text_size(15.0)).color(t.text_strong));
                }
                children(ui, cx, list, id);
            });
        }
        Kind::Text { text, style, tone: tn } => {
            let normal = *tn == Tone::Normal;
            let r = RichText::new(text).color(tone(t, *tn));
            let r = match style {
                TextStyle::Body => r,
                TextStyle::Heading => {
                    let r = r.family(cx.bold.clone()).size(t.text_size(18.0));
                    if normal { r.color(t.text_strong) } else { r }
                }
                TextStyle::Strong => {
                    let r = r.family(cx.bold.clone());
                    if normal { r.color(t.text_strong) } else { r }
                }
                TextStyle::Small => r.small(),
                TextStyle::Mono => r.monospace(),
            };
            ui.add(egui::Label::new(r).wrap());
        }
        Kind::Markdown { text } => {
            cx.st.md_live.insert(id.value());
            let doc = cx.st.md.doc(id.value(), text, true);
            ui.vertical(|ui| markdown::show(ui, t, doc, id));
        }
        Kind::Chip { text, tone: tn } => chip(ui, t, text, tone(t, *tn)),
        Kind::Button { label, action, primary, danger } => {
            let (fill, fg) = match (*primary, *danger) {
                (true, true) => (Some(t.danger), t.on_accent),
                (true, false) => (Some(t.accent), t.on_accent),
                (false, true) => (None, t.danger),
                (false, false) => (None, t.text),
            };
            let mut b = egui::Button::new(RichText::new(label).color(fg));
            if let Some(fill) = fill {
                b = b.fill(fill);
            }
            if ui.add(b).clicked() {
                cx.st.fire(action, None, Map::new());
            }
        }
        Kind::Input { name, value, placeholder, submit } => {
            let width = ui.available_width().clamp(80.0, 260.0);
            let v = cx.st.field(name, || Value::String(value.clone()), |s| s.as_str() == Some(value));
            if !v.is_string() {
                *v = Value::String(String::new());
            }
            let Value::String(s) = v else { return };
            let r = ui.add(egui::TextEdit::singleline(s).id(id).hint_text(placeholder.as_str()).desired_width(width));
            if let Some(a) = submit
                && r.lost_focus()
                && ui.input(|i| i.key_pressed(egui::Key::Enter))
            {
                cx.st.fire(a, None, Map::new());
            }
        }
        Kind::Toggle { name, label, value, action } => {
            let v = cx.st.field(name, || Value::Bool(*value), |b| b.as_bool() == Some(*value));
            let mut b = v.as_bool().unwrap_or(false);
            if ui.checkbox(&mut b, RichText::new(label).color(t.text)).changed() {
                *v = Value::Bool(b);
                if let Some(a) = action {
                    cx.st.fire(a, None, Map::new());
                }
            }
        }
        Kind::Select { name, options, value, action } => {
            let v = cx.st.field(name, || Value::String(value.clone()), |s| s.as_str() == Some(value));
            let before = v.as_str().unwrap_or_default().to_owned();
            let mut cur = before.clone();
            egui::ComboBox::from_id_salt(id).selected_text(RichText::new(&cur).color(t.text)).show_ui(ui, |ui| {
                for o in options {
                    ui.selectable_value(&mut cur, o.clone(), o.as_str());
                }
            });
            if cur != before {
                *v = Value::String(cur);
                if let Some(a) = action {
                    cx.st.fire(a, None, Map::new());
                }
            }
        }
        Kind::Progress { value, label } => {
            // The label under the bar, not on it: text on a part-filled bar has no colour that
            // reads on both parts in every theme.
            ui.vertical(|ui| {
                let bar = egui::ProgressBar::new(value.clamp(0.0, 1.0)).fill(t.accent).desired_width(ui.available_width().min(360.0)).desired_height(8.0);
                ui.add(bar);
                if !label.is_empty() {
                    ui.label(RichText::new(label).small().color(t.text_dim));
                }
            });
        }
        Kind::Table { name, columns, rows, height, select } => {
            let name = cx.st.claim(if name.is_empty() { n.key.as_deref().unwrap_or("table") } else { name }, !name.is_empty());
            table::show(ui, cx, table::Table { name: &name, columns, rows, height: *height, select: select.as_deref() }, id);
        }
        Kind::Chart { spec } => crate::chart::show(ui, t, spec, id),
        Kind::Separator {} => {
            ui.separator();
        }
        Kind::Image { media, artifact, alt, width, height } => {
            let got = match cx.images.as_deref_mut() {
                Some(images) if media.is_some() || artifact.is_some() => images.image(ui.ctx(), media.as_deref(), artifact.as_deref()),
                _ => Err(String::new()),
            };
            match got {
                Ok(Some(tex)) => image(ui, t, &tex, alt, *width, *height),
                Ok(None) => image_placeholder(ui, t, "Loading…", *width, *height),
                Err(e) if e.is_empty() => image_placeholder(ui, t, alt, *width, *height),
                Err(e) => image_placeholder(ui, t, &format!("{}: {e}", if alt.is_empty() { "image" } else { alt }), *width, *height),
            }
        }
        Kind::Canvas { width, height, shapes, action } => canvas::show(ui, cx, *width, *height, shapes, action.as_deref()),
        Kind::Grid { name, rows, cols, blocks, frozen_rows, frozen_cols, widths, default_width, editable, headers, height, selection } => {
            let name = cx.st.claim(if name.is_empty() { n.key.as_deref().unwrap_or("grid") } else { name }, !name.is_empty());
            let g = grid::Grid {
                name: &name,
                rows: (*rows).max(1),
                cols: (*cols).max(1),
                blocks: grid::Blocks::new(blocks),
                frozen_rows: (*frozen_rows).min(rows.saturating_sub(1)),
                frozen_cols: (*frozen_cols).min(cols.saturating_sub(1)),
                widths,
                default_width: default_width.filter(|w| w.is_finite()).unwrap_or(GRID_DEFAULT_WIDTH).clamp(16.0, 2000.0),
                editable: *editable,
                headers: *headers,
                height: height.filter(|h| h.is_finite()).map(|h| h.clamp(60.0, 4000.0)),
                selection: *selection,
            };
            grid::show(ui, cx, g, id);
        }
    }
}

/// A small pill.
pub(crate) fn chip(ui: &mut egui::Ui, t: &Tokens, text: &str, c: Color32) {
    egui::Frame::new()
        .fill(c.gamma_multiply(0.15))
        .stroke(Stroke::new(1.0, c.gamma_multiply(0.5)))
        .corner_radius(t.round(10.0))
        .inner_margin(egui::Margin::symmetric(8, 2))
        .show(ui, |ui| ui.label(RichText::new(text).small().color(c)));
}

/// An image, scaled to fit the width (and `width` × `height` if given), its alt text on hover.
fn image(ui: &mut egui::Ui, t: &Tokens, tex: &egui::TextureHandle, alt: &str, width: Option<f32>, height: Option<f32>) {
    let natural = tex.size_vec2();
    if natural.x <= 0.0 || natural.y <= 0.0 {
        return;
    }
    let max_w = width.unwrap_or(natural.x).min(ui.available_width()).max(8.0);
    let max_h = height.unwrap_or(natural.y.min(480.0)).max(8.0);
    let scale = (max_w / natural.x).min(max_h / natural.y);
    let r = ui.add(egui::Image::from_texture(egui::load::SizedTexture::new(tex.id(), natural * scale)).corner_radius(t.round(6.0)));
    if !alt.is_empty() {
        r.on_hover_text(alt);
    }
}

/// An image without a picture (none to load, still loading, failed): a framed box with the
/// alt text or what went wrong.
fn image_placeholder(ui: &mut egui::Ui, t: &Tokens, alt: &str, width: Option<f32>, height: Option<f32>) {
    let w = width.unwrap_or(200.0).min(ui.available_width()).max(24.0);
    let h = height.unwrap_or(120.0).max(24.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::hover());
    let p = ui.painter_at(rect);
    p.rect(rect, t.round(6.0), t.code_bg, Stroke::new(1.0, t.border), egui::StrokeKind::Inside);
    let label = if alt.is_empty() { "image" } else { alt };
    p.text(rect.center(), egui::Align2::CENTER_CENTER, label, egui::TextStyle::Small.resolve(ui.style()), t.text_dim);
}
