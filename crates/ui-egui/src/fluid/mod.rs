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
    layer.id == overlay_id() || layer.id == inspector_id()
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
}

/// The plugin's state (one per egui context).
pub struct Fluid {
    pub doc: Arc<FluidDoc>,
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
    /// For input: elements drawn elsewhere than laid out (shown rect, transform, hidden).
    mapped: Vec<(Rect, Rect, TSTransform, bool)>,
    /// The transform a pointer press was mapped with, kept until it's let go.
    capture: Option<TSTransform>,
    start: std::time::Instant,
}

impl Default for Fluid {
    fn default() -> Self {
        Self {
            doc: Arc::default(),
            edited: 0,
            stack: Vec::new(),
            regions: Vec::new(),
            seen: Vec::new(),
            elements: Vec::new(),
            editing: false,
            dragging: None,
            anim: HashMap::new(),
            mapped: Vec::new(),
            capture: None,
            start: std::time::Instant::now(),
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

/// A named part of the interface: `add` drawn in a scope whose rect is the element `name`
/// (nested in the regions it's drawn in).
pub fn region<R>(ui: &mut Ui, name: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    let path = with(ui.ctx(), |f| {
        let path = join(f.stack.last().map(String::as_str), name);
        f.stack.push(path.clone());
        path
    });
    let r = ui.scope(add);
    if let Some(path) = path {
        let (rect, layer) = (r.response.rect, ui.layer_id());
        with(ui.ctx(), |f| {
            f.stack.pop();
            f.regions.push(Region { path, rect, layer });
        });
    }
    r.inner
}

/// Name `rect` (drawn already, on `layer`) as the element `name`, nested in the region
/// being drawn.
pub fn mark(ctx: &Context, name: &str, rect: Rect, layer: LayerId) {
    with(ctx, |f| {
        let path = join(f.stack.last().map(String::as_str), name);
        f.regions.push(Region { path, rect, layer });
    });
}

fn join(parent: Option<&str>, name: &str) -> String {
    match parent {
        Some(p) => format!("{p}/{name}"),
        None => name.to_owned(),
    }
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
    fn time(&self) -> f32 {
        self.start.elapsed().as_secs_f32()
    }

    /// Every element this pass: the marked regions and the widgets AccessKit saw last pass
    /// (where they are this pass), each under the innermost region holding it.
    fn scan(&self, ctx: &Context) -> Vec<Element> {
        let mut regions = self.regions.clone();
        // Parents first: by depth, then in the order drawn.
        regions.sort_by_key(|r| r.path.matches('/').count());
        let mut out: Vec<Element> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for r in regions {
            if index.contains_key(&r.path) || own_layer(r.layer) {
                continue;
            }
            let parent = r.path.rsplit_once('/').and_then(|(p, _)| index.get(p).copied());
            let depth = r.path.matches('/').count();
            let label = r.path.rsplit('/').next().unwrap_or_default().to_owned();
            index.insert(r.path.clone(), out.len());
            out.push(element(r.path, "region".into(), label, r.rect, r.layer, parent, depth, false));
        }
        let regions = out.len();
        // Widgets, under the smallest region on their layer that holds them.
        let mut counts: HashMap<(Option<usize>, String), usize> = HashMap::new();
        let mut taken: HashMap<String, usize> = HashMap::new();
        for s in &self.seen {
            let Some(id) = egui_id(s.id) else { continue };
            let Some(resp) = ctx.read_response(id) else { continue };
            if own_layer(resp.layer_id) || !resp.rect.is_positive() {
                continue;
            }
            let rect = resp.rect;
            let parent = (0..regions)
                .filter(|&i| out[i].layer == resp.layer_id && out[i].rect.expand(2.0).contains_rect(rect))
                .min_by(|&a, &b| out[a].rect.area().total_cmp(&out[b].rect.area()));
            let part = match slug(&s.label) {
                Some(l) => format!("{}:{l}", s.role),
                None => {
                    let n = counts.entry((parent, s.role.clone())).or_insert(0);
                    *n += 1;
                    format!("{}#{n}", s.role)
                }
            };
            let base = join(parent.map(|p| out[p].path.as_str()).or(Some("window")), &part);
            let k = taken.entry(base.clone()).or_insert(0);
            *k += 1;
            let path = if *k == 1 { base } else { format!("{base}#{k}") };
            let depth = parent.map_or(1, |p| out[p].depth + 1);
            let interactive = resp.sense.senses_click() || resp.sense.senses_drag() || resp.sense.is_focusable();
            out.push(element(path, s.role.clone(), s.label.clone(), rect, resp.layer_id, parent, depth, interactive));
        }
        out
    }

    /// Rules, easing and motion: each element's transform, opacity and hiding.
    fn resolve(&mut self, ctx: &Context, els: &mut [Element]) -> bool {
        let doc = Arc::clone(&self.doc);
        let dt = ctx.input(|i| i.stable_dt).clamp(0.0, 0.1);
        let k = 1.0 - (-dt * 14.0).exp();
        let t = self.time();
        let mut busy = false;
        for i in 0..els.len() {
            let rule = doc.rule_for(&els[i].path);
            let (parent_t, parent_o, parent_h) = match els[i].parent {
                Some(p) => (els[p].transform, els[p].opacity, els[p].hidden),
                None => (TSTransform::IDENTITY, 1.0, false),
            };
            let (mut off, mut scale) = rule.as_ref().map_or((Vec2::ZERO, 1.0), |r| (r.offset(), r.scale.unwrap_or(1.0)));
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
                self.anim.insert(path, (off, scale));
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
            let c = els[i].rect.center().to_vec2();
            let own = TSTransform::new(c * (1.0 - scale) + off, scale);
            let e = &mut els[i];
            e.transform = parent_t * own;
            e.opacity = parent_o * rule.as_ref().and_then(|r| r.opacity).unwrap_or(1.0);
            e.hidden = parent_h || rule.as_ref().is_some_and(|r| r.hidden);
            e.shown = e.transform.mul_rect(e.rect);
            e.rule = rule;
        }
        busy
    }

    /// Rewrite what the changed elements painted.
    fn repaint(ui: &Ui, els: &[Element], time: f32) {
        let ctx = ui.ctx();
        // The elements that change anything, by layer, innermost first.
        let mut by_layer: HashMap<LayerId, Vec<usize>> = HashMap::new();
        for (i, e) in els.iter().enumerate() {
            let styled = e
                .rule
                .as_ref()
                .is_some_and(|r| r.text.is_some() || r.size.is_some() || r.color.is_some() || r.fill.is_some() || r.outline.is_some() || r.motion.is_some());
            if e.hidden || e.opacity < 1.0 || e.transform != TSTransform::IDENTITY || styled {
                by_layer.entry(e.layer).or_default().push(i);
            }
        }
        for v in by_layer.values_mut() {
            v.sort_by(|&a, &b| els[a].rect.area().total_cmp(&els[b].rect.area()));
        }
        for (layer, list) in by_layer {
            let bounds: Vec<Rect> =
                ctx.graphics(|g| g.get(layer).map(|l| l.all_entries().map(|c| c.shape.visual_bounding_rect()).collect()).unwrap_or_default());
            let mut decorated: Vec<usize> = Vec::new();
            let mut retitled: Vec<usize> = Vec::new();
            for (n, b) in bounds.iter().enumerate() {
                if !b.is_finite() || !b.is_positive() {
                    continue;
                }
                let Some(&i) = list.iter().find(|&&i| els[i].rect.expand(1.5).contains_rect(*b)) else { continue };
                let e = &els[i];
                let first = !decorated.contains(&i);
                if first {
                    decorated.push(i);
                }
                let retitle = e.rule.as_ref().and_then(|r| r.text.clone()).filter(|_| !retitled.contains(&i));
                let size = e.rule.as_ref().and_then(|r| r.size);
                let (text_galley, fill) = {
                    let r = e.rule.clone().unwrap_or_default();
                    let galley = ctx.graphics(|g| {
                        g.get(layer).and_then(|l| l.all_entries().nth(n)).and_then(|c| match &c.shape {
                            Shape::Text(ts) if retitle.is_some() || size.is_some() => Some(ts.galley.clone()),
                            _ => None,
                        })
                    });
                    (galley, first.then_some(r))
                };
                let new_galley = text_galley.map(|g| {
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
                    if let Some(k) = size {
                        for s in &mut job.sections {
                            s.format.font_id.size = (s.format.font_id.size * k).clamp(4.0, 200.0);
                        }
                    }
                    ctx.fonts_mut(|f| f.layout_job(job))
                });
                if new_galley.is_some() && retitle.is_some() {
                    retitled.push(i);
                }
                let tint = e.rule.as_ref().and_then(|r| r.color.as_deref()).and_then(doc::parse_color);
                let (hidden, opacity, transform, rect) = (e.hidden, e.opacity, e.transform, e.rect);
                let glow = e.rule.as_ref().and_then(|r| r.motion.as_deref()) == Some("glow");
                ctx.graphics_mut(|g| {
                    let Some(l) = g.get_mut(layer) else { return };
                    l.mutate_shape(egui::layers::ShapeIdx(n), |cs| {
                        if hidden {
                            cs.shape = Shape::Noop;
                            return;
                        }
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
                            cs.shape.transform(transform);
                            cs.clip_rect = transform.mul_rect(cs.clip_rect);
                        }
                    });
                });
            }
        }
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
fn element(path: String, role: String, label: String, rect: Rect, layer: LayerId, parent: Option<usize>, depth: usize, interactive: bool) -> Element {
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

    fn setup(&mut self, ctx: &Context) {
        ctx.enable_accesskit();
    }

    fn on_begin_pass(&mut self, _ui: &mut Ui) {
        self.stack.clear();
        self.regions.clear();
    }

    fn on_end_pass(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let mut els = self.scan(&ctx);
        let busy = self.resolve(&ctx, &mut els);
        Self::repaint(ui, &els, self.time());
        self.mapped = els
            .iter()
            .filter(|e| e.hidden || e.transform != TSTransform::IDENTITY)
            .filter(|e| e.interactive || e.is_region())
            .map(|e| (e.shown, e.rect, e.transform, e.hidden))
            .collect();
        // Smallest first: the innermost element under the pointer wins.
        self.mapped.sort_by(|a, b| a.0.area().total_cmp(&b.0.area()));
        self.elements = els;
        if busy {
            ctx.request_repaint();
        }
    }

    fn input_hook(&mut self, _ctx: &Context, input: &mut egui::RawInput) {
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
                    }
                    Some(t.inverse() * *pos)
                }
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

    fn output_hook(&mut self, _ctx: &Context, output: &mut egui::FullOutput) {
        let Some(update) = &output.platform_output.accesskit_update else { return };
        self.seen = update
            .nodes
            .iter()
            .filter_map(|(id, node)| {
                let role = role_name(node.role())?;
                let label = node.label().or_else(|| node.value()).unwrap_or_default().to_owned();
                Some(Seen { id: id.0, role: role.to_owned(), label })
            })
            .collect();
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
