//! The cockpit: byob's interface as a scene rather than a page (experimental).
//!
//! - **Backdrop** ([`wgsl`], [`backdrop`]): a WGSL shader, usually written by the model
//!   through `ui_look`, drawn behind everything; the panels turn to glass over it. It sees
//!   where the panels are, so it can light them, glow around them and cast their shadows.
//! - **Free arrangement** ([`arrange`]): the header, chat list, chat, message box and panes
//!   as modules placed anywhere (by % of the window), each in a bezel (glass, a cockpit
//!   instrument, or bare), dragged into place while arranging, or placed by the model.
//!
//! Safety: a scene is checked with naga before a GPU sees it, and wgpu's errors are logged
//! instead of ending the process. A scene that compiles can still be too slow or hang the
//! GPU, so a new one is on [`Cockpit::trial`] until it has been drawn for a few seconds: one
//! still on trial at the next start is dropped ([`Cockpit::restored`]).

pub mod arrange;
pub mod backdrop;
pub mod compositor;
pub mod free;
pub mod wgsl;

use std::sync::{Arc, Mutex, PoisonError};

use serde::{Deserialize, Serialize};

/// The cockpit's part of the appearance preferences (per device).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Cockpit {
    /// The backdrop scene's WGSL, empty for none.
    pub backdrop: String,
    /// How opaque the panels are over a backdrop ([`Cockpit::GLASS_RANGE`]; 1 opaque).
    pub glass: f32,
    /// How frosted the glass is over the backdrop, 0 (clear) to 1, where the window is
    /// composited ([`compositor`]).
    pub frost: f32,
    /// Space between the panels over a backdrop, in points ([`Cockpit::GAP_RANGE`]): the
    /// scene shows there, around rounded panels.
    pub gap: f32,
    /// The backdrop hasn't been drawn for [`TRIAL_SECONDS`] yet.
    pub trial: bool,
    /// Where the window's parts are: docked as usual, or free.
    pub arrangement: arrange::Arrangement,
}

impl Default for Cockpit {
    fn default() -> Self {
        Self { backdrop: String::new(), glass: 0.82, frost: 0.7, gap: 8.0, trial: false, arrangement: arrange::Arrangement::default() }
    }
}

/// How long a new backdrop is drawn before it's trusted to come back at the next start.
pub const TRIAL_SECONDS: f64 = 3.0;

/// A backdrop byob comes with.
pub struct Scene {
    pub name: &'static str,
    pub source: &'static str,
    /// The glass and gap it looks right with (set with it; a change can say otherwise).
    pub glass: f32,
    pub gap: f32,
}

/// The backdrops byob comes with.
pub const SCENES: &[Scene] = &[
    Scene { name: "cockpit", source: include_str!("scenes/cockpit.wgsl"), glass: 0.6, gap: 14.0 },
    Scene { name: "aurora", source: include_str!("scenes/aurora.wgsl"), glass: 0.72, gap: 10.0 },
];

/// A bundled scene by name.
pub fn scene(name: &str) -> Option<&'static Scene> {
    SCENES.iter().find(|s| s.name.eq_ignore_ascii_case(name.trim()))
}

impl Cockpit {
    pub const GLASS_RANGE: std::ops::RangeInclusive<f32> = 0.2..=1.0;
    pub const GAP_RANGE: std::ops::RangeInclusive<f32> = 0.0..=32.0;

    /// Clamp what may come from storage.
    #[must_use]
    pub fn sanitized(mut self) -> Self {
        if self.backdrop.len() > wgsl::MAX_SOURCE {
            self.backdrop.clear();
        }
        self.arrangement = self.arrangement.sanitized();
        self.frost = if self.frost.is_finite() { self.frost.clamp(0.0, 1.0) } else { Self::default().frost };
        self.gap = if self.gap.is_finite() { self.gap.clamp(*Self::GAP_RANGE.start(), *Self::GAP_RANGE.end()) } else { Self::default().gap };
        self.glass = if self.glass.is_finite() { self.glass.clamp(*Self::GLASS_RANGE.start(), *Self::GLASS_RANGE.end()) } else { Self::default().glass };
        self
    }

    /// As saved last time: a backdrop still on trial is dropped (it may be what stopped
    /// byob, or the GPU). The note to show when one was.
    #[must_use]
    pub fn restored(mut self) -> (Self, Option<String>) {
        if self.trial && !self.backdrop.is_empty() {
            self.backdrop.clear();
            self.trial = false;
            return (self, Some("The backdrop was switched off: byob closed before it had run for a few seconds, so it may have stopped the GPU.".into()));
        }
        self.trial = false;
        (self, None)
    }

    /// Use a bundled scene, with its glass and gap.
    pub fn set_scene(&mut self, scene: &Scene) {
        self.set_backdrop(scene.source);
        self.glass = scene.glass;
        self.gap = scene.gap;
    }

    /// Set a new backdrop (`""` for none), on trial.
    pub fn set_backdrop(&mut self, source: &str) {
        if source != self.backdrop {
            self.backdrop = source.to_owned();
            self.trial = !source.is_empty();
        }
    }
}

/// Panels drawn this frame, for the scene (`u.panels`).
fn panels_key() -> egui::Id {
    egui::Id::unique("byob-cockpit-panels")
}

fn gap_key() -> egui::Id {
    egui::Id::unique("byob-cockpit-gap")
}

/// The space around panels this frame (0 without a backdrop).
pub fn gap(ctx: &egui::Context) -> f32 {
    ctx.data(|d| d.get_temp::<f32>(gap_key())).unwrap_or_default()
}

/// A panel's frame: over a backdrop, set off from its neighbours by the gap, with rounded
/// corners, so the scene shows around it.
pub fn framed(ctx: &egui::Context, frame: egui::Frame) -> egui::Frame {
    let g = gap(ctx);
    if g <= 0.0 {
        return frame;
    }
    #[allow(clippy::cast_possible_truncation)] // at most 32 points
    let m = (g / 2.0).round() as i8;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let r = (g * 1.5).round().clamp(4.0, 24.0) as u8;
    frame.outer_margin(egui::Margin::same(m)).corner_radius(r)
}

/// Tell the scene a panel is at `rect` this frame (its whole space; the part drawn, inside
/// the gap, is what the scene sees).
pub fn mark(ctx: &egui::Context, rect: egui::Rect) {
    mark_exact(ctx, rect.shrink(gap(ctx) / 2.0));
}

/// Tell the scene a panel is exactly at `rect` (a free module).
pub fn mark_exact(ctx: &egui::Context, rect: egui::Rect) {
    if rect.is_positive() {
        ctx.data_mut(|d| d.get_temp_mut_or_default::<Vec<egui::Rect>>(panels_key()).push(rect));
    }
}

fn rgba(c: egui::Color32) -> [f32; 4] {
    let [r, g, b, a] = c.to_srgba_unmultiplied();
    [f32::from(r) / 255.0, f32::from(g) / 255.0, f32::from(b) / 255.0, f32::from(a) / 255.0]
}

/// What happened to the backdrop this frame.
#[derive(Debug, Default, PartialEq)]
pub struct Outcome {
    /// It has been drawn long enough to come back at the next start.
    pub trusted: bool,
    /// The GPU refused it (switch it off, say why).
    pub failed: Vec<String>,
}

/// The window's side of the cockpit: whether it can draw scenes, and the current one's state.
#[derive(Default)]
pub struct Stage {
    /// Set once the app gave its wgpu render state.
    errors: Option<backdrop::Errors>,
    inputs: Arc<Mutex<backdrop::Inputs>>,
    /// `u.busy`, eased.
    busy: f32,
    /// The scene drawn, and since when (egui time).
    since: Option<(u64, f64)>,
    /// The last scene checked, and whether it compiled.
    checked: Option<(u64, bool)>,
    /// Scenes the GPU refused (not drawn again this run).
    refused: Vec<u64>,
    /// The source being drawn (shared with the frame's callback).
    source: Option<(u64, Arc<str>)>,
    /// The control under the pointer, and since when (egui time).
    hot: Option<(egui::Id, f64)>,
    /// Last frame was drawn by a compositor.
    composited: bool,
}

impl Stage {
    /// Draw backdrops with eframe's wgpu (desktop and browser).
    pub fn install(&mut self, rs: &egui_wgpu::RenderState) {
        self.errors = Some(backdrop::install(rs));
    }

    /// Whether this window is composited (frosted glass works).
    pub fn composited(&self) -> bool {
        self.composited
    }

    /// Whether this window can draw backdrops at all.
    pub fn available(&self) -> bool {
        self.errors.is_some()
    }

    /// Whether `c`'s backdrop is drawn (installed, compiles, not refused).
    pub fn active(&mut self, c: &Cockpit) -> bool {
        if !self.available() || c.backdrop.is_empty() {
            return false;
        }
        let id = backdrop::scene_id(&c.backdrop);
        if self.refused.contains(&id) {
            return false;
        }
        match self.checked {
            Some((checked, ok)) if checked == id => ok,
            _ => {
                let ok = wgsl::check(&c.backdrop).is_ok();
                self.checked = Some((id, ok));
                ok
            }
        }
    }

    /// At the frame's start, before anything else is drawn: the backdrop under it all.
    pub fn begin(&mut self, ui: &egui::Ui, c: &Cockpit) {
        ui.ctx().data_mut(|d| d.remove::<Vec<egui::Rect>>(panels_key()));
        let compositing = compositor::compositing(ui.ctx());
        self.composited = compositing;
        if !self.active(c) {
            ui.ctx().data_mut(|d| d.remove::<f32>(gap_key()));
            compositor::leave_scene(ui.ctx(), None);
            self.since = None;
            return;
        }
        ui.ctx().data_mut(|d| d.insert_temp(gap_key(), c.gap));
        let id = backdrop::scene_id(&c.backdrop);
        if self.source.as_ref().is_none_or(|(s, _)| *s != id) {
            self.source = Some((id, Arc::from(c.backdrop.as_str())));
        }
        let Some((_, source)) = &self.source else {
            return;
        };
        let now = ui.input(|i| i.time);
        if self.since.is_none_or(|(s, _)| s != id) {
            self.since = Some((id, now));
        }
        if compositing {
            // The compositor draws it (and frosts the glass over it).
            let frame = compositor::SceneFrame { id, source: Arc::clone(source), inputs: Arc::clone(&self.inputs), frost: c.frost };
            compositor::leave_scene(ui.ctx(), Some(frame));
            return;
        }
        let rect = ui.ctx().content_rect();
        let paint = |pass| backdrop::Paint { pass, id, source: Arc::clone(source), inputs: Arc::clone(&self.inputs) };
        ui.ctx().layer_painter(egui::LayerId::background()).add(egui_wgpu::Callback::new_paint_callback(rect, paint(backdrop::Pass::Scene)));
        if wgsl::has_overlay(source) {
            // Over everything, menus and tooltips included.
            let top = egui::LayerId::new(egui::Order::Debug, egui::Id::unique("byob-backdrop-overlay"));
            ui.ctx().layer_painter(top).add(egui_wgpu::Callback::new_paint_callback(rect, paint(backdrop::Pass::Overlay)));
        }
    }

    /// The control under the pointer: the smallest hovered thing that can be clicked or
    /// dragged, its rect, whether it's pressed, and for how long it has been hovered.
    fn hot(&mut self, ctx: &egui::Context, now: f64) -> ([f32; 4], [f32; 4]) {
        let hovered: Vec<egui::Id> = ctx.interaction_snapshot(|s| s.hovered.iter().copied().collect());
        let best = hovered
            .into_iter()
            .filter_map(|id| ctx.read_response(id).map(|r| (id, r)))
            .filter(|(_, r)| (r.sense.senses_click() || r.sense.senses_drag()) && r.rect.is_positive() && r.rect.area() < 400.0 * 120.0)
            .min_by(|(_, a), (_, b)| a.rect.area().total_cmp(&b.rect.area()));
        let Some((id, r)) = best else {
            self.hot = None;
            return ([0.0; 4], [0.0; 4]);
        };
        let since = match self.hot {
            Some((h, s)) if h == id => s,
            _ => now,
        };
        self.hot = Some((id, since));
        let pressed = ctx.input(|i| i.pointer.primary_down());
        #[allow(clippy::cast_possible_truncation)] // seconds hovered, for animation
        let held = (now - since) as f32;
        ([r.rect.min.x, r.rect.min.y, r.rect.max.x, r.rect.max.y], [if pressed { 2.0 } else { 1.0 }, held, 0.0, 0.0])
    }

    /// At the frame's end: the scene's inputs (the panels are placed now), the next redraw
    /// when it moves, and whether it is trusted or refused.
    pub fn end(&mut self, ctx: &egui::Context, c: &Cockpit, t: &crate::theme::Tokens, busy: bool) -> Outcome {
        let mut out = Outcome::default();
        let Some((id, since)) = self.since else {
            return out;
        };
        let failed = self.errors.as_ref().map(backdrop::Errors::take).unwrap_or_default();
        if !failed.is_empty() {
            self.refused.push(id);
            self.since = None;
            out.failed = failed;
            return out;
        }
        let (now, dt, focused, pointer) = ctx.input(|i| (i.time, i.stable_dt.min(0.1), i.focused, i.pointer.latest_pos()));
        let target = if busy { 1.0 } else { 0.0 };
        self.busy += (target - self.busy) * (dt * 3.0).min(1.0);
        let rect = ctx.content_rect();
        let panels = ctx.data(|d| d.get_temp::<Vec<egui::Rect>>(panels_key())).unwrap_or_default();
        #[allow(clippy::cast_possible_truncation)] // seconds since start, for animation
        let time = if t.knobs.reduce_motion { 0.0 } else { (now - since) as f32 };
        let (hot, hot_state) = self.hot(ctx, now);
        *self.inputs.lock().unwrap_or_else(PoisonError::into_inner) = backdrop::Inputs {
            size: [rect.width(), rect.height()],
            mouse: pointer.map_or([-1.0, -1.0], |p| [p.x, p.y]),
            time,
            busy: self.busy,
            scale: ctx.pixels_per_point(),
            accent: rgba(t.accent),
            background: rgba(t.app_bg.to_opaque()),
            text: rgba(t.text),
            hot,
            hot_state,
            panels: panels.iter().map(|r| [r.min.x, r.min.y, r.max.x, r.max.y]).collect(),
        };
        let moving = (wgsl::animated(&c.backdrop) || (hot_state[0] > 0.0 && c.backdrop.contains("hot_state.y"))) && !t.knobs.reduce_motion;
        if focused && (moving || (self.busy - target).abs() > 0.01) {
            ctx.request_repaint_after(std::time::Duration::from_millis(33));
        }
        if c.trial && now - since >= TRIAL_SECONDS {
            out.trusted = true;
        } else if c.trial {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
        out
    }
}

/// Panels as glass over a backdrop: the surfaces behind text let `glass` of themselves
/// through. Windows and menus stay opaque (see `Tokens::visuals`).
pub fn glaze(t: &mut crate::theme::Tokens, glass: f32) {
    let g = glass.clamp(0.0, 1.0);
    t.app_bg = t.app_bg.gamma_multiply(g);
    t.panel_bg = t.panel_bg.gamma_multiply(g);
    t.sidebar_bg = t.sidebar_bg.gamma_multiply(g);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_backdrop_still_on_trial_is_dropped_at_the_next_start() {
        let mut c = Cockpit::default();
        c.set_scene(scene("cockpit").unwrap());
        assert!(c.trial);
        let (back, note) = c.clone().restored();
        assert!(back.backdrop.is_empty() && note.is_some());
        c.trial = false;
        let (back, note) = c.clone().restored();
        assert_eq!((back, note), (c, None));
    }

    #[test]
    fn stored_values_are_clamped() {
        let c = Cockpit { backdrop: "x".repeat(wgsl::MAX_SOURCE + 1), glass: f32::NAN, gap: 99.0, trial: true, ..Default::default() }.sanitized();
        assert!(c.backdrop.is_empty());
        assert_eq!((c.glass, c.gap), (Cockpit::default().glass, 32.0));
        assert_eq!(Cockpit { glass: 0.0, ..Default::default() }.sanitized().glass, 0.2);
    }

    /// Without a GPU (tests, another renderer) nothing is drawn and nothing is active.
    #[test]
    fn without_wgpu_there_is_no_backdrop() {
        let mut stage = Stage::default();
        let c = Cockpit { backdrop: scene("aurora").unwrap().source.into(), ..Default::default() };
        assert!(!stage.available() && !stage.active(&c));
    }
}
