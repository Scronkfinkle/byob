//! Inserted elements: view trees placed by an element (or anywhere), each in its own
//! area, each an element itself (`+id`) that can be moved and restyled like any other.

use std::collections::HashMap;

use byob_protocol::tree::Event;
use egui::{Align2, Context, Pos2, Rect, Vec2};
use serde_json::Value;

use super::doc::{At, Insert, Rule, parse_tree};
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
pub(super) fn expand(v: &mut Value, state: &serde_json::Map<String, Value>) {
    let mut budget = MAX_EXPANDED;
    expand_in(v, state, &mut budget);
}

/// Most JSON values `each` nodes make in one insert (nested ones multiply).
const MAX_EXPANDED: usize = 20_000;

/// Longest a string gets by putting values in it.
const MAX_FILLED: usize = 16 * 1024;

/// `s` with every `pat` replaced by `val`, unless that would make it longer than
/// [`MAX_FILLED`] (then by "…").
fn put(s: &str, pat: &str, val: &str) -> String {
    let n = s.matches(pat).count();
    let grown = s.len().saturating_add(n.saturating_mul(val.len()));
    s.replace(pat, if grown > MAX_FILLED { "…" } else { val })
}

/// How many JSON values `v` holds.
fn values(v: &Value) -> usize {
    match v {
        Value::Array(a) => a.iter().map(values).sum::<usize>() + 1,
        Value::Object(m) => m.values().map(values).sum::<usize>() + 1,
        _ => 1,
    }
}

fn expand_in(v: &mut Value, state: &serde_json::Map<String, Value>, budget: &mut usize) {
    // A node shown only while a state value is true (or, with `!key`, false):
    // {"if": "running", "then": node, "else": node}.
    if let Some(cond) = v.get("if").and_then(Value::as_str).map(str::to_owned) {
        let (neg, key) = cond.strip_prefix('!').map_or((false, cond.as_str()), |k| (true, k));
        let key = key.trim().trim_start_matches("state.");
        let truthy = match state.get(key) {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
            Some(Value::String(s)) => !s.is_empty() && s != "false",
            Some(Value::Array(a)) => !a.is_empty(),
            Some(Value::Object(o)) => !o.is_empty(),
        };
        let pick = if truthy != neg { "then" } else { "else" };
        *v = v.get(pick).cloned().unwrap_or_else(|| serde_json::json!({"type": "column", "children": []}));
        expand_in(v, state, budget);
        return;
    }
    match v {
        Value::Object(m) => {
            if let Some(key) = m.get("each").and_then(Value::as_str).map(|k| k.trim_start_matches("state.").to_owned()) {
                let item = m.remove("item").unwrap_or(Value::Null);
                let empty = m.remove("empty");
                m.remove("each");
                let items = state.get(&key).and_then(Value::as_array).cloned().unwrap_or_default();
                // As many as the budget allows (each copy of `item` costs its size).
                let cost = values(&item);
                let fit = (*budget / cost.max(1)).min(items.len());
                *budget -= fit * cost;
                let mut children: Vec<Value> = items
                    .iter()
                    .take(fit)
                    .enumerate()
                    .map(|(i, it)| {
                        let mut node = item.clone();
                        substitute(&mut node, &|s: &str| {
                            let mut s = put(&put(s, "{index}", &i.to_string()), "{item}", &text_of(it));
                            if let Value::Object(fields) = it
                                && s.contains("{item.")
                            {
                                for (k, f) in fields {
                                    s = put(&s, &format!("{{item.{k}}}"), &text_of(f));
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
                expand_in(x, state, budget);
            }
        }
        Value::Array(a) => {
            for x in a {
                expand_in(x, state, budget);
            }
        }
        Value::String(s) if s.contains("{state.") || s.contains("{since:") || s.contains("{until:") => {
            for (k, val) in state {
                let pat = format!("{{state.{k}}}");
                if s.contains(&pat) {
                    *s = put(s, &pat, &text_of(val));
                }
            }
            // Time since / until a moment kept in the state (seconds since 1970), as m:ss.
            let now = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
            for (k, val) in state {
                let Some(at) = val.as_f64() else { continue };
                if s.contains(&format!("{{since:{k}}}")) {
                    *s = s.replace(&format!("{{since:{k}}}"), &clock((now - at).max(0.0).floor()));
                }
                if s.contains(&format!("{{until:{k}}}")) {
                    let left = at - now;
                    *s = s.replace(&format!("{{until:{k}}}"), &if left <= 0.0 { "done".to_owned() } else { clock(left.ceil()) });
                }
            }
        }
        _ => {}
    }
}

/// Seconds as m:ss (h:mm:ss past an hour).
fn clock(secs: f64) -> String {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // a duration on a clock
    let s = secs.round().max(0.0) as u64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

/// Does the tree read the time (so it's drawn again every second)?
fn ticks(v: &Value) -> bool {
    match v {
        Value::String(s) => s.contains("{since:") || s.contains("{until:"),
        Value::Array(a) => a.iter().any(ticks),
        Value::Object(m) => m.values().any(ticks),
        _ => false,
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
            clock
        }
        Value::Array(a) => a.iter_mut().fold(false, |c, x| fill(x, d) | c),
        Value::Object(m) => m.values_mut().fold(false, |c, x| fill(x, d) | c),
        _ => false,
    }
}

/// Where an insert goes, as (point, which corner of it sits there): by `anchor` (`at` it),
/// or for `free`, at `x`/`y` % of the window.
fn place(at: At, anchor: &str, x: f32, y: f32, els: &[Element], screen: Rect) -> Option<(Pos2, Align2)> {
    if at == At::Free {
        let p = screen.min + Vec2::new(screen.width() * x.clamp(0.0, 100.0) / 100.0, screen.height() * y.clamp(0.0, 100.0) / 100.0);
        return Some((p, Align2::LEFT_TOP));
    }
    let anchor = els.iter().find(|e| e.path == anchor).or_else(|| els.iter().find(|e| super::doc::glob(anchor, &e.path)))?;
    let r = anchor.shown;
    let gap = 6.0;
    Some(match at {
        At::After => (r.right_center() + Vec2::new(gap, 0.0), Align2::LEFT_CENTER),
        At::Before => (r.left_center() - Vec2::new(gap, 0.0), Align2::RIGHT_CENTER),
        At::Above => (r.center_top() - Vec2::new(0.0, gap), Align2::CENTER_BOTTOM),
        At::Below => (r.center_bottom() + Vec2::new(0.0, gap), Align2::CENTER_TOP),
        At::Over | At::Free => (r.center(), Align2::CENTER_CENTER),
    })
}

/// Where an insert is drawn: by its anchor, unless its own rule (`+id`) places it by
/// another element (`place`), then nudged by the rule's `dx`/`dy`. An insert is moved for
/// real (its area is there, laid out and clicked there), not drawn elsewhere afterwards.
fn spot(ins: &Insert, rule: Option<&Rule>, els: &[Element], screen: Rect) -> Option<(Pos2, Align2)> {
    let own = format!("+{}", ins.id);
    let by = rule.and_then(|r| r.place.as_deref()).and_then(|p| p.split_once(':')).and_then(|(side, target)| {
        let at = match side.trim() {
            "after" => At::After,
            "before" => At::Before,
            "above" => At::Above,
            "below" => At::Below,
            "over" | "swap" => At::Over,
            _ => return None,
        };
        // Not by something inside itself (it would chase itself).
        let target = target.trim();
        (target != own && !target.starts_with(&format!("{own}/"))).then_some((at, target))
    });
    let (pos, pivot) = match by {
        Some((at, target)) => place(at, target, 0.0, 0.0, els, screen).or_else(|| place(ins.at, &ins.anchor, ins.x, ins.y, els, screen))?,
        None => place(ins.at, &ins.anchor, ins.x, ins.y, els, screen)?,
    };
    Some((pos + rule.map_or(Vec2::ZERO, Rule::offset), pivot))
}

/// Where inserts go: the elements on screen (anchors), the rule for a path (its own `+id`
/// one places it), and the one being dragged in edit mode (it follows the pointer, not eased).
pub struct Moves<'a> {
    pub els: &'a [Element],
    pub rules: &'a dyn Fn(&str) -> Option<Rule>,
    pub dragging: Option<&'a str>,
}

/// Draw every insert; the events their trees sent, by insert id.
/// `views` draws a plugin's live view (`plugin/view`).
pub fn show(
    ctx: &Context,
    t: &Tokens,
    inserts: &[Insert],
    states: &mut HashMap<String, TreeState>,
    data: &Data,
    moves: &Moves<'_>,
    views: &mut dyn FnMut(&mut egui::Ui, &str),
) -> Vec<(String, Event)> {
    let mut out = Vec::new();
    let screen = ctx.content_rect();
    let mut clock = false;
    states.retain(|k, _| inserts.iter().any(|i| &i.id == k));
    for ins in inserts {
        let own = format!("+{}", ins.id);
        let Some((pos, pivot)) = spot(ins, (moves.rules)(&own).as_ref(), moves.els, screen) else { continue };
        // Eased to a new spot (not while it's dragged: then it follows the pointer).
        let pos = if moves.dragging == Some(own.as_str()) {
            pos
        } else {
            let id = egui::Id::unique(("fluid-insert-pos", &ins.id));
            Pos2::new(ctx.animate_value_with_time(id.with(0), pos.x, 0.2), ctx.animate_value_with_time(id.with(1), pos.y, 0.2))
        };
        let mut tree = ins.tree.clone();
        clock |= ticks(&tree);
        expand(&mut tree, &ins.state);
        clock |= fill(&mut tree, data);
        let node = if ins.view.is_empty() { parse_tree(&tree).0 } else { None };
        if node.is_none() && ins.view.is_empty() {
            continue;
        }
        let st = states.entry(ins.id.clone()).or_default();
        let width = if ins.width > 0.0 { ins.width.clamp(40.0, 1200.0) } else { 360.0 };
        // Over the panels, under popups and edit mode's overlay.
        egui::Area::new(egui::Id::unique(("fluid-insert", &ins.id))).order(egui::Order::Middle).pivot(pivot).fixed_pos(pos).constrain_to(screen).show(
            ctx,
            |ui| {
                // A width asked for is the width; otherwise as wide as it needs, up to 360.
                if ins.width > 0.0 {
                    ui.set_width(width);
                } else {
                    ui.set_max_width(width);
                }
                region(ui, &own, |ui| {
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

    #[test]
    fn nested_each_stays_within_a_budget() {
        let items: Vec<Value> = (0..1000).map(Value::from).collect();
        let state = json!({"xs": items}).as_object().cloned().unwrap_or_default();
        let leaf = json!({"type": "text", "text": "{item}"});
        let mut v = json!({"each": "xs", "item": {"each": "xs", "item": {"each": "xs", "item": leaf}}});
        let t = std::time::Instant::now();
        expand(&mut v, &state);
        assert!(values(&v) <= MAX_EXPANDED * 2, "{}", values(&v));
        assert!(t.elapsed() < std::time::Duration::from_secs(2), "{:?}", t.elapsed());
    }

    #[test]
    fn filled_text_stays_short() {
        let state = json!({"big": "x".repeat(10_000)}).as_object().cloned().unwrap_or_default();
        let mut v = json!({"type": "text", "text": "{state.big}".repeat(100)});
        expand(&mut v, &state);
        assert!(v["text"].as_str().is_some_and(|s| s.len() <= MAX_FILLED), "{}", v["text"].as_str().map_or(0, str::len));
        let mut v = json!({"type": "text", "text": "{state.big}!"});
        expand(&mut v, &state);
        assert_eq!(v["text"].as_str().map(str::len), Some(10_001));
    }

    #[test]
    fn if_picks_by_state() {
        let state = json!({"running": true, "laps": []}).as_object().cloned().unwrap_or_default();
        let mut v = json!([{"if": "running", "then": {"type": "text", "text": "on"}, "else": {"type": "text", "text": "off"}}, {"if": "!laps", "then": {"type": "text", "text": "no laps"}}, {"if": "laps", "then": 1}]);
        expand(&mut v, &state);
        assert_eq!(v[0]["text"], "on");
        assert_eq!(v[1]["text"], "no laps");
        assert_eq!(v[2]["type"], "column");
    }

    fn el(path: &str, rect: Rect) -> Element {
        super::super::element(path.into(), "button".into(), String::new(), rect, egui::LayerId::background(), None, 0, true)
    }

    #[test]
    fn an_insert_is_placed_by_its_own_rule() {
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 800.0));
        let els = vec![el("sidebar/button:Chats", Rect::from_min_size(Pos2::new(8.0, 300.0), Vec2::new(224.0, 26.0)))];
        let ins: Insert = serde_json::from_value(json!({"id": "pomo", "at": "free", "x": 50, "y": 50, "tree": {}})).unwrap_or_else(|_| unreachable!());
        // By its anchor (free: % of the window) without a rule.
        assert_eq!(spot(&ins, None, &els, screen).map(|s| s.0), Some(Pos2::new(500.0, 400.0)));
        // A place rule puts the insert itself there (below Chats), and dx/dy nudge it.
        let rule = Rule { place: Some("below:sidebar/button:Chats".into()), dx: Some(4.0), ..Rule::default() };
        assert_eq!(spot(&ins, Some(&rule), &els, screen), Some((Pos2::new(124.0, 332.0), Align2::CENTER_TOP)));
        // Not by something inside itself.
        let rule = Rule { place: Some("after:+pomo/button:Go".into()), ..Rule::default() };
        assert_eq!(spot(&ins, Some(&rule), &els, screen).map(|s| s.0), Some(Pos2::new(500.0, 400.0)));
    }

    #[test]
    fn timers_read_the_clock() {
        let now = chrono::Utc::now().timestamp() as f64;
        let state = json!({"start": now - 75.0, "end": now + 600.0, "old": now - 5.0}).as_object().cloned().unwrap_or_default();
        let mut v = json!(["{since:start}", "{until:end}", "{until:old}"]);
        assert!(ticks(&v));
        expand(&mut v, &state);
        assert_eq!(v, json!(["1:15", "10:00", "done"]));
    }
}
