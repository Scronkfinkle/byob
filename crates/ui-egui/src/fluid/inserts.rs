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

/// A value as text in a template.
fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Array(a) => a.len().to_string(),
        Value::Number(n) => n.as_f64().map_or_else(|| n.to_string(), |f| if f.fract() == 0.0 && f.abs() < 1e15 { format!("{f:.0}") } else { format!("{f}") }),
        v => v.to_string(),
    }
}

/// Repeat nodes over the insert's state: a node with `"each": "key"` and `"item": node`
/// gets one child per item of `state.key` (`{item}`, `{index}`, `{item.field}` in it), or
/// its `"empty"` node when there are none. `{state.key}` anywhere reads the state.
fn expand(v: &mut Value, state: &serde_json::Map<String, Value>) {
    match v {
        Value::Object(m) => {
            if let Some(key) = m.get("each").and_then(Value::as_str).map(|k| k.trim_start_matches("state.").to_owned()) {
                let item = m.remove("item").unwrap_or(Value::Null);
                let empty = m.remove("empty");
                m.remove("each");
                let items = state.get(&key).and_then(Value::as_array).cloned().unwrap_or_default();
                let mut children: Vec<Value> = items
                    .iter()
                    .enumerate()
                    .map(|(i, it)| {
                        let mut node = item.clone();
                        substitute(&mut node, &|s: &str| {
                            let mut s = s.replace("{index}", &i.to_string()).replace("{item}", &text_of(it));
                            if let Value::Object(fields) = it {
                                for (k, f) in fields {
                                    s = s.replace(&format!("{{item.{k}}}"), &text_of(f));
                                }
                            }
                            s
                        });
                        node
                    })
                    .collect();
                if children.is_empty()
                    && let Some(e) = empty
                {
                    children.push(e);
                }
                m.insert("children".into(), Value::Array(children));
                if !m.contains_key("type") {
                    m.insert("type".into(), Value::String("column".into()));
                }
            }
            for x in m.values_mut() {
                expand(x, state);
            }
        }
        Value::Array(a) => {
            for x in a {
                expand(x, state);
            }
        }
        Value::String(s) if s.contains("{state.") => {
            for (k, val) in state {
                *s = s.replace(&format!("{{state.{k}}}"), &text_of(val));
            }
        }
        _ => {}
    }
}

fn substitute(v: &mut Value, f: &dyn Fn(&str) -> String) {
    match v {
        Value::String(s) => *s = f(s),
        Value::Array(a) => a.iter_mut().for_each(|x| substitute(x, f)),
        Value::Object(m) => m.values_mut().for_each(|x| substitute(x, f)),
        _ => {}
    }
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
/// `views` draws a plugin's live view (`plugin/view`).
pub fn show(
    ctx: &Context,
    t: &Tokens,
    inserts: &[Insert],
    els: &[Element],
    states: &mut HashMap<String, TreeState>,
    data: &Data,
    views: &mut dyn FnMut(&mut egui::Ui, &str),
) -> Vec<(String, Event)> {
    let mut out = Vec::new();
    let screen = ctx.content_rect();
    let mut clock = false;
    states.retain(|k, _| inserts.iter().any(|i| &i.id == k));
    for ins in inserts {
        let Some((pos, pivot)) = place(ins, els, screen) else { continue };
        let mut tree = ins.tree.clone();
        expand(&mut tree, &ins.state);
        clock |= fill(&mut tree, data);
        let node = if ins.view.is_empty() { parse_tree(&tree).0 } else { None };
        if node.is_none() && ins.view.is_empty() {
            continue;
        }
        let st = states.entry(ins.id.clone()).or_default();
        let width = if ins.width > 0.0 { ins.width.clamp(40.0, 1200.0) } else { 360.0 };
        egui::Area::new(egui::Id::unique(("fluid-insert", &ins.id))).order(egui::Order::Foreground).pivot(pivot).fixed_pos(pos).constrain(true).show(
            ctx,
            |ui| {
                ui.set_max_width(width);
                region(ui, &format!("+{}", ins.id), |ui| {
                    let mut body = |ui: &mut egui::Ui| match &node {
                        Some(node) => show_tree(ui, t, node, st),
                        None => {
                            views(ui, &ins.view);
                            Vec::new()
                        }
                    };
                    let events = if ins.card { crate::menu::frame(t).inner_margin(egui::Margin::same(10)).show(ui, |ui| body(ui)).inner } else { body(ui) };
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn each_repeats_over_state() {
        let state = json!({"items": [{"t": "milk"}, {"t": "eggs"}], "title": "Shop"}).as_object().cloned().unwrap_or_default();
        let mut v = json!({"type": "column", "children": [
            {"type": "text", "text": "{state.title} ({state.items})"},
            {"each": "items", "item": {"type": "button", "label": "{index}: {item.t}", "action": "drop:items={index}"}}
        ]});
        expand(&mut v, &state);
        assert_eq!(v["children"][0]["text"], "Shop (2)");
        assert_eq!(v["children"][1]["children"][1]["label"], "1: eggs");
        assert_eq!(v["children"][1]["children"][1]["action"], "drop:items=1");
        assert_eq!(v["children"][1]["type"], "column");
    }
}
