//! Fluid: every element on screen has an address and can be moved, scaled, recolored,
//! retitled, faded or hidden, and new elements can be put beside any of them, by the user
//! (edit mode), the model (`ui_fluid`) or a plugin.
//!
//! How, without rewriting byob's interface: an egui [`egui::Plugin`] that
//! - lists every widget each frame from egui's AccessKit tree (role, label, rect) and the
//!   named regions byob marks with [`region`] / [`mark`], and gives each a path
//!   (`composer/button:Send`, `sidebar/row.3/text#1`);
//! - at the end of each pass, rewrites what those elements painted (their shapes, found by
//!   their rect on their layer): translated, scaled, tinted, faded, retitled, removed;
//! - before each pass, maps the pointer back from where a moved element is drawn to where
//!   egui laid it out, so it is clicked, hovered and dragged where it's seen.
//!
//! Like a CSS transform: the element keeps its place in the layout; only where it's
//! drawn (and clicked) moves.

pub mod doc;
pub mod edit;
pub mod inserts;

use std::collections::HashMap;
use std::sync::Arc;

use egui::emath::TSTransform;
use egui::{Color32, Context, Id, LayerId, Pos2, Rect, Shape, Ui, Vec2};

pub use doc::{At, FluidDoc, Insert, Rule};

/// Layers fluid draws its own things on (never rewritten, never listed).
pub fn own_layer(layer: LayerId) -> bool {
    layer.id == overlay_id() || layer.id == inspector_id() || layer.id == lift_id()
}

/// Where moved elements are drawn: over the panels (whatever was drawn after them where
/// they were laid out), under popups.
fn lift_id() -> Id {
    Id::unique("fluid-lift")
}

pub fn overlay_id() -> Id {
    Id::unique("fluid-overlay")
}

pub fn inspector_id() -> Id {
    Id::unique("fluid-inspector")
}

/// One element on screen.
#[derive(Debug, Clone)]
pub struct Element {
    pub path: String,
    /// `region` (a part byob names), or a widget's role: `button`, `text`, `input`, …
    pub role: String,
    pub label: String,
    /// Where byob laid it out.
    pub rect: Rect,
    /// Where it's drawn now.
    pub shown: Rect,
    pub layer: LayerId,
    pub parent: Option<usize>,
    pub depth: usize,
    pub interactive: bool,
    /// A hidden region that isn't drawn at all (the layout closed up): `rect` is where it
    /// was last.
    pub collapsed: bool,
    /// A region whose text size is laid out (a zoom from its top left), not drawn bigger.
    pub zoom: f32,
    /// Moved (by its rule or a parent's): drawn over the panels.
    pub lifted: bool,
    /// Its own rule (globs and exact, merged), if any.
    pub rule: Option<Rule>,
    /// Hidden by its rule or a parent's.
    pub hidden: bool,
    /// Where it's drawn, relative to its layout: its parents' and its own.
    pub transform: TSTransform,
    pub opacity: f32,
}

impl Element {
    pub fn is_region(&self) -> bool {
        self.role == "region"
    }

    /// An inserted element as a whole (`+id`).
    pub fn is_insert(&self) -> bool {
        self.is_region() && self.path.starts_with('+') && !self.path.contains('/')
    }

    /// Not moved by its own rule (only zoomed or styled).
    fn offset_free(&self) -> bool {
        self.rule.as_ref().is_none_or(|r| r.offset() == Vec2::ZERO)
    }
}

/// A widget as AccessKit listed it last frame.
#[derive(Debug, Clone)]
struct Seen {
    id: u64,
    role: String,
    label: String,
}

/// A region byob marked this pass.
#[derive(Debug, Clone)]
struct Region {
    path: String,
    rect: Rect,
    layer: LayerId,
    /// Hidden and not drawn: listed where it was last.
    collapsed: bool,
    /// Its text size, laid out (see [`region`]).
    zoom: f32,
}

/// The plugin's state (one per egui context).
pub struct Fluid {
    /// What the user (and the model) changed.
    pub doc: Arc<FluidDoc>,
    /// What plugins that are on change, under the user's changes.
    pub base: Arc<FluidDoc>,
    /// Bumped on every change made in the window (edit mode), for the app to save.
    pub edited: u64,
    stack: Vec<String>,
    regions: Vec<Region>,
    seen: Vec<Seen>,
    /// Last pass's elements.
    pub elements: Vec<Element>,
    pub editing: bool,
    /// The element being dragged in edit mode: drawn where the pointer puts it, no easing.
    pub dragging: Option<String>,
    /// Eased offset and scale per path, for the fluid part.
    anim: HashMap<String, (Vec2, f32)>,
    /// Where each region was last drawn (for collapsed ones).
    last: HashMap<String, (Rect, LayerId)>,
    /// Eased visibility of elements shown on hover.
    reveal: HashMap<String, f32>,
    /// For input: elements drawn elsewhere than laid out (shown rect, transform, hidden).
    mapped: Vec<(Rect, Rect, TSTransform, bool)>,
    /// The transform a pointer press was mapped with, kept until it's let go.
    capture: Option<TSTransform>,
    /// The last press on a moved element (its transform, where it's laid out): a popup it
    /// opens there is moved along with it.
    opener: Option<(TSTransform, Rect)>,
    /// Popup layers moved that way.
    moved_popups: Vec<LayerId>,
    /// The layers open when that press came (a popup is one that opened after it).
    open_before: Vec<LayerId>,
    start: std::time::Instant,
    /// Plugins' changes left out (edit mode's switch, for a plugin whose changes are in the
    /// way of turning it off); not kept across a restart.
    pub ignore_base: bool,
    /// Listing elements even with nothing changed, until then (the model asked to see them).
    warm_until: Option<web_time::Instant>,
    /// Passes in a row that listed the elements (the first ones after AccessKit is turned
    /// on don't have its widgets yet).
    scans: u32,
    /// AccessKit turned on (by fluid; it's left on).
    accesskit: bool,
}

impl Default for Fluid {
    fn default() -> Self {
        Self {
            doc: Arc::default(),
            base: Arc::default(),
            edited: 0,
            stack: Vec::new(),
            regions: Vec::new(),
            seen: Vec::new(),
            elements: Vec::new(),
            editing: false,
            dragging: None,
            anim: HashMap::new(),
            reveal: HashMap::new(),
            last: HashMap::new(),
            mapped: Vec::new(),
            capture: None,
            opener: None,
            moved_popups: Vec::new(),
            open_before: Vec::new(),
            start: std::time::Instant::now(),
            ignore_base: false,
            warm_until: None,
            scans: 0,
            accesskit: false,
        }
    }
}

/// Install fluid on `ctx` (once; later calls do nothing).
pub fn install(ctx: &Context) {
    if ctx.plugin_opt::<Fluid>().is_none() {
        ctx.add_plugin(Fluid::default());
    }
}

/// Run `f` on the plugin, if it's installed.
pub fn with<R>(ctx: &Context, f: impl FnOnce(&mut Fluid) -> R) -> Option<R> {
    let handle = ctx.plugin_opt::<Fluid>()?;
    let mut guard = handle.lock();
    Some(f(&mut guard))
}

/// Is the region at `path` hidden, so it isn't drawn at all and the layout closes up
/// around it? (Not while editing: then it's drawn faded, to be found and shown again.)
pub fn collapsed(ctx: &Context, path: &str) -> bool {
    with(ctx, |f| {
        let gone = !f.editing && f.rule_of(path).is_some_and(|r| r.is_hidden());
        if gone {
            // Still listed (where it was last), so it can be found and shown again.
            if let Some(&(rect, layer)) = f.last.get(path) {
                f.regions.push(Region { path: path.to_owned(), rect, layer, collapsed: true, zoom: 1.0 });
            }
        }
        gone
    })
    .unwrap_or(false)
}

/// A named part of the interface: `add` drawn in a scope whose rect is the element `name`
/// (nested in the regions it's drawn in). Not drawn at all while it's hidden.
pub fn region<R: Default>(ui: &mut Ui, name: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    let parent = with(ui.ctx(), |f| f.stack.last().cloned()).flatten();
    if collapsed(ui.ctx(), &join(parent.as_deref(), name)) {
        return R::default();
    }
    let path = with(ui.ctx(), |f| {
        let path = join(f.stack.last().map(String::as_str), name);
        f.stack.push(path.clone());
        path
    });
    // Bigger text in a region is a zoom that reflows: laid out narrower by the factor, drawn
    // scaled up from its top left (so it fills the width it had), and the height it gains
    // taken in the layout so what follows moves down.
    let zoom = path.as_deref().and_then(|p| with(ui.ctx(), |f| f.rule_of(p).and_then(|r| r.size)).flatten()).filter(|k| (*k - 1.0).abs() > 0.001);
    let r = ui.scope(|ui| {
        if let Some(k) = zoom {
            ui.set_max_width((ui.available_width() / k).max(40.0));
        }
        add(ui)
    });
    if let Some(k) = zoom {
        let more = r.response.rect.height() * (k - 1.0);
        if more > 0.0 {
            ui.allocate_space(Vec2::new(0.0, more));
        }
    }
    if let Some(path) = path {
        let (rect, layer) = (r.response.rect, ui.layer_id());
        with(ui.ctx(), |f| {
            f.stack.pop();
            f.last.insert(path.clone(), (rect, layer));
            f.regions.push(Region { path, rect, layer, collapsed: false, zoom: zoom.unwrap_or(1.0) });
        });
    }
    r.inner
}

/// Name `rect` (drawn already, on `layer`) as the element `name`, nested in the region
/// being drawn.
pub fn mark(ctx: &Context, name: &str, rect: Rect, layer: LayerId) {
    with(ctx, |f| {
        let path = join(f.stack.last().map(String::as_str), name);
        f.last.insert(path.clone(), (rect, layer));
        f.regions.push(Region { path, rect, layer, collapsed: false, zoom: 1.0 });
    });
}

fn join(parent: Option<&str>, name: &str) -> String {
    match parent {
        Some(p) => format!("{p}/{name}"),
        None => name.to_owned(),
    }
}

/// Text without icon-font glyphs (private-use characters), trimmed.
pub fn clean(text: &str) -> String {
    text.chars().filter(|c| !('\u{E000}'..='\u{F8FF}').contains(c)).collect::<String>().trim().to_owned()
}

/// A label as a path part: short single-line labels as they are, others not at all.
fn slug(label: &str) -> Option<String> {
    let l = label.trim();
    if l.is_empty() || l.chars().count() > 28 || l.contains('\n') {
        return None;
    }
    Some(l.replace(['/', '#'], " "))
}

fn role_name(role: egui::accesskit::Role) -> Option<&'static str> {
    use egui::accesskit::Role as R;
    Some(match role {
        R::Button | R::DefaultButton => "button",
        R::Label => "text",
        R::Link => "link",
        R::TextInput | R::MultilineTextInput | R::SearchInput | R::EmailInput | R::NumberInput | R::PasswordInput => "input",
        R::CheckBox | R::Switch => "check",
        R::RadioButton => "radio",
        R::Slider | R::SpinButton => "slider",
        R::ComboBox => "select",
        R::Image => "image",
        R::ProgressIndicator => "progress",
        R::Tab => "tab",
        R::MenuItem => "menu-item",
        R::Window | R::Dialog => "window",
        R::ScrollView => return None,
        R::GenericContainer | R::Unknown => return None,
        _ => "widget",
    })
}

/// egui's id for an AccessKit node id (they're the same number).
fn egui_id(n: u64) -> Option<Id> {
    serde_json::from_value(serde_json::Value::from(n)).ok()
}

impl Fluid {
    /// The rule for `path`: the plugins', the user's on top.
    pub fn rule_of(&self, path: &str) -> Option<Rule> {
        if self.doc.is_empty() && self.base().is_empty() {
            return None;
        }
        doc::merged(self.base(), &self.doc, path)
    }

    /// The plugins' changes, unless they're left out.
    pub fn base(&self) -> &FluidDoc {
        static NONE: std::sync::LazyLock<FluidDoc> = std::sync::LazyLock::new(FluidDoc::default);
        if self.ignore_base { &NONE } else { &self.base }
    }

    /// Anything to do this pass: changes to draw, edit mode, or the model about to look.
    fn active(&self) -> bool {
        !self.doc.is_empty() || !self.base().is_empty() || self.editing || self.warm_until.is_some_and(|t| web_time::Instant::now() < t)
    }

    /// Keep listing the elements for a while, though nothing is changed (the model asked
    /// to see them).
    pub fn wake(&mut self) {
        self.warm_until = Some(web_time::Instant::now() + std::time::Duration::from_secs(60));
    }

    /// [`Self::elements`] is this window's, as of a pass or so ago.
    pub fn fresh(&self) -> bool {
        self.scans >= 3
    }

    fn time(&self) -> f32 {
        self.start.elapsed().as_secs_f32()
    }

    /// Every element this pass, nested by what holds what:
    /// - the regions byob marked (named parts: `sidebar`, `composer`, …);
    /// - the widgets AccessKit saw last pass (buttons, labels, inputs, …), where they are now;
    /// - other clickable things (custom-drawn rows and icons), named by the text in them;
    /// - every run of text painted anywhere that isn't a widget's own label.
    fn scan(&self, ctx: &Context) -> Vec<Element> {
        struct Raw {
            name: Name,
            role: String,
            label: String,
            rect: Rect,
            layer: LayerId,
            interactive: bool,
            collapsed: bool,
            zoom: f32,
        }
        enum Name {
            /// A region's full path.
            Region(String),
            /// A path part, given its parent; numbered when it has no label.
            Part,
        }
        let mut raw: Vec<Raw> = Vec::new();
        for r in &self.regions {
            if !own_layer(r.layer) && !raw.iter().any(|x| matches!(&x.name, Name::Region(p) if *p == r.path)) {
                let label = r.path.rsplit('/').next().unwrap_or_default().to_owned();
                raw.push(Raw {
                    name: Name::Region(r.path.clone()),
                    role: "region".into(),
                    label,
                    rect: r.rect,
                    layer: r.layer,
                    interactive: false,
                    collapsed: r.collapsed,
                    zoom: r.zoom,
                });
            }
        }
        let mut layers: Vec<LayerId> = vec![LayerId::background()];
        let mut widgets: Vec<(Rect, LayerId)> = Vec::new();
        for s in &self.seen {
            let Some(resp) = egui_id(s.id).and_then(|id| ctx.read_response(id)) else { continue };
            if own_layer(resp.layer_id) || !resp.rect.is_positive() {
                continue;
            }
            let interactive = resp.sense.senses_click() || resp.sense.senses_drag() || resp.sense.is_focusable();
            if !layers.contains(&resp.layer_id) {
                layers.push(resp.layer_id);
            }
            widgets.push((resp.rect, resp.layer_id));
            raw.push(Raw {
                name: Name::Part,
                role: s.role.clone(),
                label: s.label.clone(),
                rect: resp.rect,
                layer: resp.layer_id,
                interactive,
                collapsed: false,
                zoom: 1.0,
            });
        }
        for l in ctx.memory(|m| m.layer_ids().collect::<Vec<_>>()) {
            if !layers.contains(&l) && !own_layer(l) {
                layers.push(l);
            }
        }
        for r in &self.regions {
            if !layers.contains(&r.layer) {
                layers.push(r.layer);
            }
        }
        // Painted text, by layer.
        let mut texts: Vec<(String, Rect, LayerId)> = Vec::new();
        for &layer in &layers {
            ctx.graphics(|g| {
                let Some(list) = g.get(layer) else { return };
                for cs in list.all_entries() {
                    collect_text(&cs.shape, layer, &mut texts);
                }
            });
        }
        let same = |a: Rect, b: Rect| a.expand(2.0).contains_rect(b) && b.expand(2.0).contains_rect(a);
        // The inspector's own text, to tell its controls from byob's under it.
        let mut own_texts: Vec<Rect> = Vec::new();
        if self.editing {
            let layer = LayerId::new(egui::Order::Tooltip, inspector_id());
            let mut found = Vec::new();
            ctx.graphics(|g| {
                if let Some(list) = g.get(layer) {
                    for cs in list.all_entries() {
                        collect_text(&cs.shape, layer, &mut found);
                    }
                }
            });
            own_texts = found.into_iter().map(|t| t.1).collect();
        }
        // Clickable things AccessKit didn't name: custom-drawn rows, icons, chips.
        // Fluid's own: the inspector, and the overlay over the whole window while editing.
        let inspector = ctx.memory(|m| m.area_rect(inspector_id()));
        let overlay = ctx.memory(|m| m.area_rect(overlay_id()));
        for rect in ctx.interactive_rects_last_pass() {
            if !rect.is_positive()
                || widgets.iter().any(|(w, _)| same(*w, rect))
                || overlay.is_some_and(|m| same(m, rect))
                // An area as big as a region in it (an inserted element's own): that region.
                || raw.iter().any(|x| x.role == "region" && same(x.rect, rect))
            {
                continue;
            }
            let inside: Vec<&(String, Rect, LayerId)> = texts.iter().filter(|(_, r, _)| rect.expand(1.0).contains_rect(*r)).collect();
            // Where the inspector is, its own controls are left out (egui doesn't say which
            // layer a clickable rect is on): one with the inspector's text in it, or none.
            // byob's own under it (with byob's text) is still listed.
            if inspector.is_some_and(|m| m.expand(1.0).contains_rect(rect))
                && (inside.is_empty() || own_texts.iter().any(|r| rect.expand(1.0).contains_rect(*r)))
            {
                continue;
            }
            let layer = inside.first().map(|t| t.2).or_else(|| {
                raw.iter().filter(|x| x.role == "region" && x.rect.contains_rect(rect)).min_by(|a, b| a.rect.area().total_cmp(&b.rect.area())).map(|x| x.layer)
            });
            let Some(layer) = layer else { continue };
            if own_layer(layer) {
                continue;
            }
            let label = inside
                .iter()
                .map(|t| clean(&t.0))
                .find(|t| !t.is_empty())
                .or_else(|| inside.iter().find_map(|t| crate::icons::glyph_name(&t.0)).map(str::to_owned))
                .unwrap_or_default();
            // A big clickable surface (a panel's background) is an area, not a button named
            // after whatever text happens to be first in it.
            let big = rect.width() > 420.0 || rect.height() > 160.0;
            let (role, label) = if big { ("area", String::new()) } else { ("button", label) };
            raw.push(Raw { name: Name::Part, role: role.into(), label, rect, layer, interactive: true, collapsed: false, zoom: 1.0 });
        }
        // Text that isn't a widget's own label (that one is the widget).
        for (text, rect, layer) in texts {
            let (role, text) = match clean(&text) {
                t if t.is_empty() => ("icon", crate::icons::glyph_name(&text).unwrap_or_default().to_owned()),
                t => ("text", t),
            };
            if role == "icon" {
                if !own_layer(layer) && !raw.iter().any(|x| x.layer == layer && x.role == "button" && same(x.rect, rect)) {
                    raw.push(Raw { name: Name::Part, role: role.into(), label: text, rect, layer, interactive: false, collapsed: false, zoom: 1.0 });
                }
                continue;
            }
            if own_layer(layer) || raw.iter().any(|x| x.layer == layer && x.role == "text" && same(x.rect, rect)) {
                continue;
            }
            if raw.iter().any(|x| x.layer == layer && x.role != "region" && x.rect.expand(1.0).contains_rect(rect) && x.label.trim() == text.trim()) {
                continue;
            }
            raw.push(Raw { name: Name::Part, role: "text".into(), label: text, rect, layer, interactive: false, collapsed: false, zoom: 1.0 });
        }
        // Bigger first, so every element's holder comes before it; regions before the rest.
        raw.sort_by(|a, b| {
            let (ra, rb) = (a.role == "region", b.role == "region");
            match (&a.name, &b.name) {
                (Name::Region(pa), Name::Region(pb)) => pa.matches('/').count().cmp(&pb.matches('/').count()).then(b.rect.area().total_cmp(&a.rect.area())),
                _ => rb.cmp(&ra).then(b.rect.area().total_cmp(&a.rect.area())),
            }
        });
        let mut out: Vec<Element> = Vec::with_capacity(raw.len());
        let mut counts: HashMap<(Option<usize>, String), usize> = HashMap::new();
        let mut taken: HashMap<String, usize> = HashMap::new();
        for x in raw {
            let parent = match &x.name {
                Name::Region(p) => p.rsplit_once('/').and_then(|(pp, _)| out.iter().position(|e| e.path == pp)),
                Name::Part => out
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| {
                        !e.collapsed && e.layer == x.layer && e.rect.expand(1.5).contains_rect(x.rect) && !(e.role == x.role && same(e.rect, x.rect))
                    })
                    // The tightest holder; a region over a widget of the same size.
                    .min_by(|(_, a), (_, b)| a.rect.area().total_cmp(&b.rect.area()).then((a.role != "region").cmp(&(b.role != "region"))))
                    .map(|(i, _)| i),
            };
            let path = match x.name {
                Name::Region(p) => p,
                Name::Part => {
                    let part = match slug(&x.label) {
                        Some(l) => format!("{}:{l}", x.role),
                        None => {
                            let n = counts.entry((parent, x.role.clone())).or_insert(0);
                            *n += 1;
                            format!("{}#{n}", x.role)
                        }
                    };
                    let base = join(Some(parent.map_or("window", |p| out[p].path.as_str())), &part);
                    let k = taken.entry(base.clone()).or_insert(0);
                    *k += 1;
                    if *k == 1 { base } else { format!("{base}#{k}") }
                }
            };
            let depth = parent.map_or(0, |p| out[p].depth + 1);
            let mut e = element(path, x.role, x.label, x.rect, x.layer, parent, depth, x.interactive);
            e.collapsed = x.collapsed;
            e.zoom = x.zoom;
            out.push(e);
        }
        out
    }

    /// Rules, easing and motion: each element's transform, opacity and hiding.
    fn resolve(&mut self, ctx: &Context, els: &mut [Element]) -> bool {
        let doc = Arc::clone(&self.doc);
        let base = if self.ignore_base { Arc::default() } else { Arc::clone(&self.base) };
        // Last frame's elements by path (laid out, drawn), for placing by another element.
        let last: HashMap<String, (Rect, Rect)> = self.elements.iter().map(|e| (e.path.clone(), (e.rect, e.shown))).collect();
        let dt = ctx.input(|i| i.stable_dt).clamp(0.0, 0.1);
        let k = 1.0 - (-dt * 14.0).exp();
        let t = self.time();
        let pointer = ctx.pointer_latest_pos();
        let mut busy = false;
        for i in 0..els.len() {
            // Globs match the full label too (long ones aren't in the path).
            let named = (!els[i].is_region() && !els[i].label.is_empty() && slug(&els[i].label).is_none()).then(|| {
                let parent = els[i].path.rsplit_once('/').map_or("window", |(p, _)| p);
                format!("{parent}/{}:{}", els[i].role, els[i].label.replace(['\n', '/'], " "))
            });
            let rule = match (base.rule_for_named(&els[i].path, named.as_deref()), doc.rule_for_named(&els[i].path, named.as_deref())) {
                (Some(mut b), Some(u)) => {
                    b.merge(&u);
                    Some(b)
                }
                (b, u) => u.or(b),
            };
            let (parent_t, parent_o, parent_h) = match els[i].parent {
                Some(p) => (els[p].transform, els[p].opacity, els[p].hidden),
                None => (TSTransform::IDENTITY, 1.0, false),
            };
            let (mut off, mut scale) = rule.as_ref().map_or((Vec2::ZERO, 1.0), |r| (r.offset(), r.scale.unwrap_or(1.0)));
            // An inserted element is moved and placed where its area is drawn (see
            // inserts::show): really there, not drawn elsewhere than laid out.
            let insert = els[i].is_insert();
            if insert {
                off = Vec2::ZERO;
            }
            // Placed by another element: where that one was drawn last frame.
            if let Some(place) = rule.as_ref().and_then(|r| r.place.as_deref()).filter(|_| !insert)
                && let Some((side, target)) = place.split_once(':')
                && let Some(t) = last.get(target)
            {
                let r = els[i].rect;
                let (size, gap) = (r.size() * scale, 6.0);
                let at = match side {
                    "after" => Pos2::new(t.1.right() + gap, t.1.center().y - size.y / 2.0),
                    "before" => Pos2::new(t.1.left() - gap - size.x, t.1.center().y - size.y / 2.0),
                    "above" => Pos2::new(t.1.center().x - size.x / 2.0, t.1.top() - gap - size.y),
                    "below" => Pos2::new(t.1.center().x - size.x / 2.0, t.1.bottom() + gap),
                    "over" => t.1.center() - size / 2.0,
                    // Where the other one was laid out (so two swapped don't chase each other).
                    _ => t.0.center() - size / 2.0,
                };
                // Kept inside the window (a wide one under a narrow one would stick out).
                let screen = ctx.content_rect();
                let at = Pos2::new(
                    at.x.clamp(screen.left(), (screen.right() - size.x).max(screen.left())),
                    at.y.clamp(screen.top(), (screen.bottom() - size.y).max(screen.top())),
                );
                // The scale is about its center: aim the center, in its parent's terms (the
                // parent may be moved or zoomed itself).
                off += parent_t.inverse() * (at + size / 2.0) - r.center();
            }
            // Ease toward the rule (snap while dragged).
            let path = els[i].path.clone();
            if self.dragging.as_deref() != Some(path.as_str()) {
                let cur = self.anim.get(&path).copied().unwrap_or((Vec2::ZERO, 1.0));
                let next = (cur.0 + (off - cur.0) * k, cur.1 + (scale - cur.1) * k);
                if (next.0 - off).length() > 0.3 || (next.1 - scale).abs() > 0.002 {
                    busy = true;
                    off = next.0;
                    scale = next.1;
                }
            }
            if off != Vec2::ZERO || scale != 1.0 {
                self.anim.insert(path.clone(), (off, scale));
            } else {
                self.anim.remove(&path);
            }
            if let Some(m) = rule.as_ref().and_then(|r| r.motion.as_deref()) {
                busy = true;
                let phase = (els[i].rect.min.x * 0.013 + els[i].rect.min.y * 0.007) % 6.3;
                match m {
                    "float" => off.y += (t * 2.0 + phase).sin() * 3.0,
                    "bounce" => off.y -= (t * 4.0 + phase).sin().abs() * 6.0,
                    "shake" => off.x += (t * 38.0).sin() * 1.6,
                    "pulse" => scale *= 1.0 + 0.045 * (t * 3.0 + phase).sin(),
                    _ => {}
                }
            }
            // Shown on hover: faded in while the pointer is near (always while editing).
            let mut seen = 1.0;
            if rule.as_ref().and_then(|r| r.reveal.as_deref()) == Some("hover") {
                let near = self.editing || pointer.is_some_and(|p| els[i].shown.expand(48.0).contains(p));
                let cur = self.reveal.get(&path).copied().unwrap_or(0.0);
                let target = if near { 1.0 } else { 0.0 };
                let next = cur + (target - cur) * (1.0 - (-dt * 10.0).exp());
                seen = if (next - target).abs() < 0.01 { target } else { next };
                if seen != target {
                    busy = true;
                }
                self.reveal.insert(path.clone(), seen);
            }
            let z = els[i].zoom;
            let zoomed = TSTransform::new(els[i].rect.min.to_vec2() * (1.0 - z), z);
            let c = zoomed.mul_rect(els[i].rect).center().to_vec2();
            let own = TSTransform::new(c * (1.0 - scale) + off, scale) * zoomed;
            let parent_lifted = els[i].parent.is_some_and(|p| els[p].lifted);
            let e = &mut els[i];
            e.lifted = parent_lifted || off.length() > 0.5;
            e.transform = bounded(parent_t * own);
            e.opacity = parent_o * rule.as_ref().and_then(|r| r.opacity).unwrap_or(1.0) * seen;
            e.hidden = parent_h || rule.as_ref().is_some_and(Rule::is_hidden);
            e.shown = e.transform.mul_rect(e.rect);
            e.rule = rule;
        }
        busy
    }

    /// Rewrite what the changed elements painted (moved ones onto a layer of their own).
    fn repaint(ui: &Ui, els: &[Element], time: f32, editing: bool) {
        let ctx = ui.ctx();
        // The elements that change anything, by layer, innermost first.
        let mut by_layer: HashMap<LayerId, Vec<usize>> = HashMap::new();
        for (i, e) in els.iter().enumerate() {
            let styled = e
                .rule
                .as_ref()
                .is_some_and(|r| r.text.is_some() || r.size.is_some() || r.color.is_some() || r.fill.is_some() || r.outline.is_some() || r.motion.is_some());
            if e.collapsed {
                continue;
            }
            if e.hidden || e.opacity < 1.0 || e.transform != TSTransform::IDENTITY || styled {
                by_layer.entry(e.layer).or_default().push(i);
            }
        }
        for v in by_layer.values_mut() {
            v.sort_by(|&a, &b| els[a].rect.area().total_cmp(&els[b].rect.area()));
        }
        let mut lifted: Vec<epaint::ClippedShape> = Vec::new();
        for (layer, list) in by_layer {
            let bounds: Vec<Rect> =
                ctx.graphics(|g| g.get(layer).map(|l| l.all_entries().map(|c| c.shape.visual_bounding_rect()).collect()).unwrap_or_default());
            let mut decorated: Vec<usize> = Vec::new();
            let mut retitled: Vec<usize> = Vec::new();
            for (n, b) in bounds.iter().enumerate() {
                if !b.is_finite() || !b.is_positive() {
                    continue;
                }
                // Its element: the innermost changed one it's inside; else (a card's shadow
                // reaches past it) the innermost changed region it's centered in.
                let Some(&i) = list
                    .iter()
                    .find(|&&i| els[i].rect.expand(1.5).contains_rect(*b))
                    .or_else(|| list.iter().find(|&&i| els[i].is_region() && els[i].rect.contains(b.center()) && els[i].rect.expand(48.0).contains_rect(*b)))
                else {
                    continue;
                };
                let e = &els[i];
                let first = !decorated.contains(&i);
                if first {
                    decorated.push(i);
                }
                let retitle = e.rule.as_ref().and_then(|r| r.text.clone()).filter(|_| !retitled.contains(&i));
                let size = e.rule.as_ref().and_then(|r| r.size).filter(|_| e.zoom == 1.0);
                let (text_galley, fill) = {
                    let r = e.rule.clone().unwrap_or_default();
                    let galley = ctx.graphics(|g| {
                        g.get(layer).and_then(|l| l.all_entries().nth(n)).and_then(|c| match &c.shape {
                            Shape::Text(ts) if retitle.is_some() || size.is_some() || e.transform.scaling != 1.0 => Some(ts.galley.clone()),
                            _ => None,
                        })
                    });
                    (galley, first.then_some(r))
                };
                let new_galley = text_galley.clone().map(|g| {
                    let mut job = (*g.job).clone();
                    if let Some(text) = &retitle {
                        let format = job.sections.first().map(|s| s.format.clone()).unwrap_or_default();
                        job.text.clone_from(text);
                        job.sections = vec![egui::text::LayoutSection {
                            leading_space: 0.0,
                            byte_range: epaint::text::ByteIndex(0)..epaint::text::ByteIndex(text.len()),
                            format,
                        }];
                    }
                    // Drawn bigger: laid out again at the bigger size (crisp), not stretched.
                    let k = size.unwrap_or(1.0) * e.transform.scaling;
                    if k != 1.0 {
                        for s in &mut job.sections {
                            s.format.font_id.size = (s.format.font_id.size * k).clamp(2.0, 400.0);
                        }
                        if job.wrap.max_width.is_finite() {
                            job.wrap.max_width *= e.transform.scaling;
                        }
                    }
                    ctx.fonts_mut(|f| f.layout_job(job))
                });
                if new_galley.is_some() && retitle.is_some() {
                    retitled.push(i);
                }
                // Laid out again bigger, it should be as big as stretched; if it isn't (aligned
                // or justified text lays out otherwise), stretch it after all.
                let mut new_galley = new_galley;
                if retitle.is_none()
                    && let (Some(old), Some(new)) = (&text_galley, &new_galley)
                {
                    let want = old.size() * size.unwrap_or(1.0) * e.transform.scaling;
                    if (new.size().x - want.x).abs() > 4.0 + want.x * 0.06 || (new.size().y - want.y).abs() > 4.0 + want.y * 0.1 {
                        new_galley = if size.is_some() { new_galley } else { None };
                    }
                }
                let crisp = new_galley.is_some() && e.transform.scaling != 1.0;
                let tint = e.rule.as_ref().and_then(|r| r.color.as_deref()).and_then(doc::parse_color);
                let (hidden, opacity, transform, rect, lift) = (e.hidden, e.opacity, e.transform, e.rect, e.lifted);
                // The zoomed part this is in (if it isn't moved itself).
                let zoomed = std::iter::successors(Some(i), |&k| els[k].parent).find(|&k| els[k].zoom != 1.0).map(|k| els[k].rect).filter(|_| e.offset_free());
                let glow = e.rule.as_ref().and_then(|r| r.motion.as_deref()) == Some("glow");
                ctx.graphics_mut(|g| {
                    let Some(l) = g.get_mut(layer) else { return };
                    l.mutate_shape(egui::layers::ShapeIdx(n), |cs| {
                        if hidden && !editing {
                            cs.shape = Shape::Noop;
                            return;
                        }
                        // While editing, hidden things show faintly (to be found and shown again).
                        let opacity = if hidden { opacity * 0.25 } else { opacity };
                        let mut rect = rect;
                        if let (Some(galley), Shape::Text(ts)) = (new_galley.clone(), &mut cs.shape) {
                            ts.galley = galley;
                            rect = rect.union(cs.shape.visual_bounding_rect());
                        }
                        // Text, lines and outlines take the color; filled backgrounds keep theirs.
                        if let Some(c) = tint
                            && !matches!(cs.shape, Shape::Rect(_))
                        {
                            epaint::shape_transform::adjust_colors(&mut cs.shape, move |x| *x = tinted(*x, c));
                        }
                        if opacity < 1.0 {
                            epaint::shape_transform::adjust_colors(&mut cs.shape, move |x| *x = x.gamma_multiply(opacity));
                        }
                        if let Some(r) = &fill {
                            let mut under = Vec::new();
                            let radius = r.radius.unwrap_or(6.0);
                            if let Some(c) = r.fill.as_deref().and_then(doc::parse_color) {
                                under.push(Shape::rect_filled(rect.expand(3.0), radius, c.gamma_multiply(opacity)));
                            }
                            let outline = r.outline.as_deref().and_then(doc::parse_color);
                            if glow {
                                let a = 0.5 + 0.5 * (time * 3.0).sin();
                                let c = outline.or(tint).unwrap_or(Color32::from_rgb(120, 190, 255));
                                for k in 1..=4 {
                                    #[allow(clippy::cast_precision_loss)] // 1..=4
                                    let w = k as f32 * 2.0;
                                    under.push(Shape::rect_stroke(
                                        rect.expand(2.0 + w),
                                        radius + w,
                                        egui::Stroke::new(2.0, c.gamma_multiply(a * 0.35 / k as f32)),
                                        egui::StrokeKind::Outside,
                                    ));
                                }
                            }
                            if !under.is_empty() || outline.is_some() {
                                let shape = std::mem::replace(&mut cs.shape, Shape::Noop);
                                under.push(shape);
                                if let Some(c) = outline {
                                    under.push(Shape::rect_stroke(rect.expand(3.0), radius, egui::Stroke::new(1.5, c), egui::StrokeKind::Outside));
                                }
                                cs.shape = Shape::Vec(under);
                                // The background reaches past the element: don't clip it off.
                                cs.clip_rect = cs.clip_rect.union(rect.expand(8.0));
                            }
                        }
                        if transform != TSTransform::IDENTITY {
                            match (&mut cs.shape, crisp) {
                                // Already laid out at its new size: only moved.
                                (Shape::Text(ts), true) => ts.pos = transform * ts.pos,
                                (shape, _) => shape.transform(transform),
                            }
                            // A zoomed part stays inside its panel (a clip as wide as it is
                            // the panel's); a field's own clip, and a moved part's, go with it.
                            if zoomed.is_none_or(|z| cs.clip_rect.width() < z.width() - 2.0) {
                                cs.clip_rect = transform.mul_rect(cs.clip_rect);
                            }
                        }
                        if lift {
                            lifted.push(cs.clone());
                            cs.shape = Shape::Noop;
                        }
                    });
                });
            }
        }
        if !lifted.is_empty() {
            ctx.graphics_mut(|g| {
                let l = g.entry(LayerId::new(egui::Order::Middle, lift_id()));
                for cs in lifted {
                    l.add(cs.clip_rect, cs.shape);
                }
            });
        }
    }
}

/// `t` within what can be drawn: nested scales multiply (a glob's on every level), and an
/// element blown up thousands of times (or shrunk to nothing) would stall the painter.
fn bounded(mut t: TSTransform) -> TSTransform {
    t.scaling = if t.scaling.is_finite() { t.scaling.clamp(0.05, 20.0) } else { 1.0 };
    let fit = |v: f32| if v.is_finite() { v.clamp(-20_000.0, 20_000.0) } else { 0.0 };
    t.translation = Vec2::new(fit(t.translation.x), fit(t.translation.y));
    t
}

/// Every run of text in `shape` (with where it is).
fn collect_text(shape: &Shape, layer: LayerId, out: &mut Vec<(String, Rect, LayerId)>) {
    match shape {
        Shape::Text(t) if !t.galley.job.text.trim().is_empty() => {
            out.push((t.galley.job.text.clone(), t.visual_bounding_rect(), layer));
        }
        Shape::Callback(_) | Shape::Noop => {}
        Shape::Vec(v) => {
            for s in v {
                collect_text(s, layer, out);
            }
        }
        _ => {}
    }
}

/// `x` in `to`'s hue, keeping its lightness and alpha.
fn tinted(x: Color32, to: Color32) -> Color32 {
    let [r, g, b, a] = x.to_srgba_unmultiplied();
    let l = (f32::from(r) * 0.3 + f32::from(g) * 0.59 + f32::from(b) * 0.11) / 255.0;
    // Light parts (text on dark) take the color as is; dark ones a darker shade of it.
    let k = (0.35 + l * 0.9).min(1.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // 0..=255
    let c = |v: u8| (f32::from(v) * k).round().clamp(0.0, 255.0) as u8;
    let [tr, tg, tb, _] = to.to_srgba_unmultiplied();
    Color32::from_rgba_unmultiplied(c(tr), c(tg), c(tb), a)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn element(
    path: String,
    role: String,
    label: String,
    rect: Rect,
    layer: LayerId,
    parent: Option<usize>,
    depth: usize,
    interactive: bool,
) -> Element {
    Element {
        path,
        role,
        label,
        rect,
        shown: rect,
        layer,
        parent,
        depth,
        interactive,
        collapsed: false,
        zoom: 1.0,
        lifted: false,
        rule: None,
        hidden: false,
        transform: TSTransform::IDENTITY,
        opacity: 1.0,
    }
}

impl egui::Plugin for Fluid {
    fn debug_name(&self) -> &'static str {
        "fluid"
    }

    fn on_begin_pass(&mut self, _ui: &mut Ui) {
        self.stack.clear();
        self.regions.clear();
    }

    fn on_end_pass(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        // Where regions were drawn: only the ones still there.
        let here: std::collections::HashSet<&str> = self.regions.iter().map(|r| r.path.as_str()).collect();
        self.last.retain(|k, _| here.contains(k.as_str()));
        if !self.active() {
            // Nothing changed and nobody looking: nothing to list, rewrite or map.
            self.scans = 0;
            self.warm_until = None;
            self.elements.clear();
            self.mapped.clear();
            self.anim.clear();
            self.reveal.clear();
            self.restore_popups(&ctx);
            return;
        }
        // Widgets are listed from AccessKit's tree: on from the first time it's needed.
        if !self.accesskit {
            self.accesskit = true;
            ctx.enable_accesskit();
            ctx.request_repaint();
        }
        self.scans = self.scans.saturating_add(1);
        if !self.fresh() {
            ctx.request_repaint();
        }
        let t0 = web_time::Instant::now();
        let mut els = self.scan(&ctx);
        let t1 = web_time::Instant::now();
        let busy = self.resolve(&ctx, &mut els);
        Self::repaint(ui, &els, self.time(), self.editing);
        let t2 = web_time::Instant::now();
        if t2 - t0 > std::time::Duration::from_millis(4) {
            log::debug!("fluid: {} elements, scan {:?}, rules and paint {:?}", els.len(), t1 - t0, t2 - t1);
        }
        self.mapped = els
            .iter()
            .filter(|e| e.hidden || e.transform != TSTransform::IDENTITY)
            .filter(|e| e.interactive || e.is_region())
            .map(|e| (e.shown, e.rect, e.transform, e.hidden))
            .collect();
        // Smallest first: the innermost element under the pointer wins.
        self.mapped.sort_by(|a, b| a.0.area().total_cmp(&b.0.area()));
        // A popup the moved element opened (near where it's laid out) goes where it's drawn.
        if let Some((t, at)) = self.opener {
            let shown = t.mul_rect(at);
            let screen = ctx.content_rect();
            for l in ctx.memory(|m| m.layer_ids().collect::<Vec<_>>()) {
                if l.order != egui::Order::Foreground || own_layer(l) || l.id.value() == overlay_id().value() {
                    continue;
                }
                let near = ctx.memory(|m| m.area_rect(l.id)).is_some_and(|r| r.intersects(at.expand(400.0)) && ctx.memory(|m| m.areas().is_visible(&l)));
                if near && !self.moved_popups.contains(&l) && !self.open_before.contains(&l) {
                    let Some(r) = ctx.memory(|m| m.area_rect(l.id)) else { continue };
                    // As it sat by the element where it's laid out, by where it's drawn; on
                    // the other side if that's off screen (a menu that opened upward from the
                    // bottom, moved to the top), and within the window.
                    let mut d = shown.center() - at.center();
                    let moved = r.translate(d);
                    if moved.top() < screen.top() || moved.bottom() > screen.bottom() {
                        d.y = if r.center().y < at.center().y { shown.bottom() + 4.0 - r.top() } else { shown.top() - 4.0 - r.bottom() };
                    }
                    let moved = r.translate(d);
                    d.x += (screen.right() - moved.right()).min(0.0) + (screen.left() - moved.left()).max(0.0);
                    ctx.set_transform_layer(l, TSTransform::from_translation(d));
                    self.moved_popups.push(l);
                }
            }
        }
        self.restore_popups(&ctx);
        self.elements = els;
        if busy {
            ctx.request_repaint();
        }
    }

    fn input_hook(&mut self, ctx: &Context, input: &mut egui::RawInput) {
        self.input(ctx, input);
    }

    fn output_hook(&mut self, _ctx: &Context, output: &mut egui::FullOutput) {
        let Some(update) = &output.platform_output.accesskit_update else { return };
        if !self.active() {
            self.seen.clear();
            return;
        }
        self.seen = update
            .nodes
            .iter()
            .filter_map(|(id, node)| {
                let role = role_name(node.role())?;
                let label = clean(node.label().or_else(|| node.value()).unwrap_or_default());
                Some(Seen { id: id.0, role: role.to_owned(), label })
            })
            .collect();
    }
}

impl Fluid {
    /// Popups a moved element opened go back to where egui puts them once they're closed.
    fn restore_popups(&mut self, ctx: &Context) {
        self.moved_popups.retain(|l| {
            let open = ctx.memory(|m| m.areas().is_visible(l));
            if !open {
                ctx.set_transform_layer(*l, TSTransform::IDENTITY);
            }
            open
        });
    }

    /// The pointer mapped from where moved elements are drawn to where they're laid out.
    fn input(&mut self, ctx: &Context, input: &mut egui::RawInput) {
        if self.editing || self.mapped.is_empty() {
            self.capture = None;
            return;
        }
        let far = Pos2::new(-10_000.0, -10_000.0);
        for ev in &mut input.events {
            let (pos, press) = match ev {
                egui::Event::PointerMoved(p) => (p, None),
                egui::Event::PointerButton { pos, pressed, .. } => (pos, Some(*pressed)),
                _ => continue,
            };
            let mapped = if let Some(t) = self.capture {
                Some(t.inverse() * *pos)
            } else if let Some((_, _, t, hidden)) = self.mapped.iter().find(|m| m.0.contains(*pos)) {
                if *hidden {
                    Some(far)
                } else {
                    if press == Some(true) {
                        self.capture = Some(*t);
                        let at = t.inverse() * *pos;
                        self.opener = self.mapped.iter().find(|m| m.1.contains(at)).map(|m| (*t, m.1));
                        self.open_before = ctx.memory(|m| m.layer_ids().filter(|l| m.areas().is_visible(l)).collect());
                    }
                    Some(t.inverse() * *pos)
                }
            } else if press == Some(true)
                && self.opener.is_some()
                && !self.moved_popups.iter().any(|l| ctx.memory(|m| m.area_rect(l.id)).is_some_and(|r| r.contains(*pos)))
            {
                // A press elsewhere (not in the popup): later popups aren't its.
                self.opener = None;
                None
            } else if self.mapped.iter().any(|m| m.1.contains(*pos) && !m.3) {
                // Where something was laid out but isn't drawn any more: nothing there.
                Some(far)
            } else {
                None
            };
            if press == Some(false) {
                self.capture = None;
            }
            if let Some(p) = mapped {
                *pos = p;
            }
        }
    }
}

/// The elements as lines for the model: path, role, label, where (under `under`, matching
/// `find`), at most `max` of them.
pub fn inventory(els: &[Element], under: &str, find: &str, max: usize) -> String {
    let under = under.trim().trim_end_matches('/');
    let find = find.trim().to_lowercase();
    let mut out = String::new();
    let mut n = 0;
    let mut more = 0;
    for e in els {
        if !under.is_empty() && !(e.path == under || e.path.starts_with(&format!("{under}/")) || doc::glob(under, &e.path)) {
            continue;
        }
        if !find.is_empty() && !e.path.to_lowercase().contains(&find) && !e.label.to_lowercase().contains(&find) {
            continue;
        }
        if n >= max {
            more += 1;
            continue;
        }
        n += 1;
        let label: String = e.label.chars().take(60).collect::<String>().replace('\n', " ");
        let label = if label.is_empty() || e.is_region() { String::new() } else { format!(" {label:?}") };
        let changed = match &e.rule {
            Some(r) => format!("  changed: {}", serde_json::to_string(r).unwrap_or_default()),
            None => String::new(),
        };
        let r = e.shown;
        out.push_str(&format!(
            "{}{} [{}{}] at {:.0},{:.0} {:.0}×{:.0}{changed}\n",
            "  ".repeat(e.depth.min(8)),
            e.path,
            e.role,
            label,
            r.min.x,
            r.min.y,
            r.width(),
            r.height()
        ));
    }
    if more > 0 {
        out.push_str(&format!("… and {more} more (narrow it with under / find)\n"));
    }
    if n == 0 {
        out.push_str("(no elements match)\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(ctx: &Context) {
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))), ..Default::default() };
        let mut out = ctx.run_ui(input, |ui| {
            region(ui, "composer", |ui| {
                let _ = ui.button("Send");
                ui.label("hello there");
            });
        });
        out.textures_delta.clear();
    }

    fn listed(ctx: &Context) -> usize {
        with(ctx, |f| f.elements.len()).unwrap_or(0)
    }

    #[test]
    fn idle_fluid_lists_nothing_and_wakes_for_a_rule() {
        let ctx = Context::default();
        install(&ctx);
        for _ in 0..4 {
            frame(&ctx);
        }
        assert_eq!(listed(&ctx), 0, "nothing changed: nothing listed");
        assert!(with(&ctx, |f| !f.accesskit).unwrap_or(false), "AccessKit isn't turned on for nothing");
        let mut doc = FluidDoc::default();
        doc::apply(&mut doc, &serde_json::json!({"set": [{"path": "composer", "dx": 10}]}), "you");
        with(&ctx, |f| f.doc = Arc::new(doc));
        for _ in 0..4 {
            frame(&ctx);
        }
        assert!(with(&ctx, |f| f.fresh()).unwrap_or(false));
        assert!(with(&ctx, |f| f.elements.iter().any(|e| e.path == "composer" && e.transform.translation.x > 0.0)).unwrap_or(false));
        // Reset: idle again, nothing mapped for input.
        with(&ctx, |f| f.doc = Arc::default());
        frame(&ctx);
        assert_eq!(listed(&ctx), 0);
        assert!(with(&ctx, |f| f.mapped.is_empty()).unwrap_or(false));
    }

    /// A custom-drawn clickable thing (no AccessKit name) with `text` painted in it.
    fn chip(ui: &mut Ui, text: &str) {
        let r = ui.allocate_response(Vec2::new(90.0, 24.0), egui::Sense::click());
        ui.painter().text(r.rect.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(13.0), Color32::WHITE);
    }

    #[test]
    fn edit_mode_lists_what_is_under_the_inspector_but_not_the_inspector() {
        let ctx = Context::default();
        install(&ctx);
        with(&ctx, |f| f.editing = true);
        for _ in 0..4 {
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))), ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| {
                region(ui, "composer", |ui| {
                    ui.add_space(300.0);
                    ui.horizontal(|ui| {
                        ui.add_space(620.0);
                        chip(ui, "model");
                    });
                });
                // The inspector, over the right of the window, with a control of its own.
                egui::Area::new(inspector_id()).order(egui::Order::Tooltip).fixed_pos(Pos2::new(560.0, 0.0)).show(ui.ctx(), |ui| {
                    ui.set_min_size(Vec2::new(240.0, 600.0));
                    ui.add_space(100.0);
                    chip(ui, "Reset all");
                });
            });
            out.textures_delta.clear();
        }
        let paths: Vec<String> = with(&ctx, |f| f.elements.iter().map(|e| e.path.clone()).collect()).unwrap_or_default();
        assert!(paths.iter().any(|p| p == "composer/button:model"), "{paths:?}");
        assert!(!paths.iter().any(|p| p.contains("Reset all")), "{paths:?}");
    }

    #[test]
    fn nested_scales_stay_drawable() {
        let t = bounded(TSTransform::new(Vec2::new(f32::INFINITY, 1e9), 6f32.powi(40)));
        assert!(t.scaling <= 20.0 && t.translation.x == 0.0 && t.translation.y <= 20_000.0);
    }
}
