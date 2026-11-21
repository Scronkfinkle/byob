//! Inserted elements: view trees placed by an element (or anywhere), each in its own
//! area, each an element itself (`+id`) that can be moved and restyled like any other.

use std::collections::HashMap;

use byob_protocol::tree::Event;
use egui::{Align2, Context, Pos2, Rect, Vec2};
use serde_json::Value;

use super::doc::{At, Insert, parse_tree};
use super::{Element, region};
use crate::theme::Tokens;
use crate::tree::{TreeState, show_tree};

/// What templates in an insert's strings read: `{time}`, `{date}`, `{chat}`, `{model}`, …
#[derive(Debug, Clone, Default)]
pub struct Data {
    pub chat: String,
    pub model: String,
    pub project: String,
    pub messages: usize,
    pub busy: bool,
    pub context: String,
}

/// `{name}`s in every string of `v` filled from `d`; whether the clock is read (so it is
/// drawn again each second).
fn fill(v: &mut Value, d: &Data) -> bool {
    match v {
        Value::String(s) if s.contains('{') => {
            let now = chrono::Local::now();
            let mut clock = false;
            let mut r = |k: &str, with: &dyn Fn() -> String| {
                if s.contains(k) {
                    *s = s.replace(k, &with());
                    true
                } else {
                    false
                }
            };
            clock |= r("{time}", &|| now.format("%H:%M:%S").to_string());
            clock |= r("{clock}", &|| now.format("%H:%M").to_string());
            r("{date}", &|| now.format("%A %-d %B").to_string());
            r("{chat}", &|| d.chat.clone());
            r("{model}", &|| d.model.clone());
            r("{project}", &|| d.project.clone());
            r("{messages}", &|| d.messages.to_string());
            r("{status}", &|| if d.busy { "working…".into() } else { "idle".into() });
            r("{context}", &|| d.context.clone());
            clock
        }
        Value::Array(a) => a.iter_mut().fold(false, |c, x| fill(x, d) | c),
        Value::Object(m) => m.values_mut().fold(false, |c, x| fill(x, d) | c),
        _ => false,
    }
}

/// Where `ins` goes, as (point, which corner of it sits there), given the elements.
fn place(ins: &Insert, els: &[Element], screen: Rect) -> Option<(Pos2, Align2)> {
    if ins.at == At::Free {
        let p = screen.min + Vec2::new(screen.width() * ins.x.clamp(0.0, 100.0) / 100.0, screen.height() * ins.y.clamp(0.0, 100.0) / 100.0);
        return Some((p, Align2::LEFT_TOP));
    }
    let anchor = els.iter().find(|e| e.path == ins.anchor).or_else(|| els.iter().find(|e| super::doc::glob(&ins.anchor, &e.path)))?;
    let r = anchor.shown;
    let gap = 6.0;
    Some(match ins.at {
        At::After => (r.right_center() + Vec2::new(gap, 0.0), Align2::LEFT_CENTER),
        At::Before => (r.left_center() - Vec2::new(gap, 0.0), Align2::RIGHT_CENTER),
        At::Above => (r.center_top() - Vec2::new(0.0, gap), Align2::CENTER_BOTTOM),
        At::Below => (r.center_bottom() + Vec2::new(0.0, gap), Align2::CENTER_TOP),
        At::Over | At::Free => (r.center(), Align2::CENTER_CENTER),
    })
}

/// Draw every insert; the events their trees sent, by insert id.
pub fn show(ctx: &Context, t: &Tokens, inserts: &[Insert], els: &[Element], states: &mut HashMap<String, TreeState>, data: &Data) -> Vec<(String, Event)> {
    let mut out = Vec::new();
    let screen = ctx.content_rect();
    let mut clock = false;
    states.retain(|k, _| inserts.iter().any(|i| &i.id == k));
    for ins in inserts {
        let Some((pos, pivot)) = place(ins, els, screen) else { continue };
        let mut tree = ins.tree.clone();
        clock |= fill(&mut tree, data);
        let Some(node) = parse_tree(&tree).0 else { continue };
        let st = states.entry(ins.id.clone()).or_default();
        let width = if ins.width > 0.0 { ins.width.clamp(40.0, 1200.0) } else { 360.0 };
        egui::Area::new(egui::Id::unique(("fluid-insert", &ins.id))).order(egui::Order::Foreground).pivot(pivot).fixed_pos(pos).constrain(true).show(
            ctx,
            |ui| {
                ui.set_max_width(width);
                region(ui, &format!("+{}", ins.id), |ui| {
                    let events = if ins.card {
                        crate::menu::frame(t).inner_margin(egui::Margin::same(10)).show(ui, |ui| show_tree(ui, t, &node, st)).inner
                    } else {
                        show_tree(ui, t, &node, st)
                    };
                    out.extend(events.into_iter().map(|e| (ins.id.clone(), e)));
                });
            },
        );
    }
    if clock {
        ctx.request_repaint_after(std::time::Duration::from_millis(500));
    }
    out
}
