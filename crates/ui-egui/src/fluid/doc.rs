//! What the user (or the model, or a plugin) changed about the screen: per-element rules
//! (moved, scaled, restyled, retitled, hidden) addressed by path, and inserted elements.
//! Pure data: kept in the prefs, edited by [`apply`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Most rules and inserts kept.
pub const MAX_RULES: usize = 400;
pub const MAX_INSERTS: usize = 64;
pub const MAX_TEXT: usize = 400;

/// How one element (or every element a glob matches) is changed. Every field is optional:
/// a rule only says what it changes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rule {
    /// Moved by this much, in points (right and down).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dx: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dy: Option<f32>,
    /// Drawn this much bigger (about its center), 0.2..=6.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scale: Option<f32>,
    /// 0 (invisible, still there) ..= 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f32>,
    /// Not drawn and not clickable.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
    /// Its text, instead of what byob writes there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Its text this much bigger (1 = as it is).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<f32>,
    /// Everything it draws in this color (#rrggbb), keeping each part's lightness.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// A background behind it (#rrggbb or #rrggbbaa).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fill: Option<String>,
    /// An outline around it (#rrggbb).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outline: Option<String>,
    /// Corner radius of `fill` and `outline`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub radius: Option<f32>,
    /// Drawn next to another element, wherever that one is: `after:PATH`, `before:`,
    /// `above:`, `below:`, `over:`, or `swap:PATH` (each where the other was laid out).
    /// `dx`/`dy` then nudge it from there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub place: Option<String>,
    /// Shown only while the pointer is near ("hover"), faded the rest of the time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reveal: Option<String>,
    /// Gently moving: "float" (bobs), "bounce", "pulse" (breathes), "shake", "glow".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub motion: Option<String>,
}

impl Rule {
    pub fn is_empty(&self) -> bool {
        *self == Rule::default()
    }

    /// `other`'s fields over these.
    pub fn merge(&mut self, other: &Rule) {
        macro_rules! take {
            ($($f:ident),*) => { $( if other.$f.is_some() { self.$f = other.$f.clone(); } )* };
        }
        take!(dx, dy, scale, opacity, text, size, color, fill, outline, radius, place, reveal, motion);
        self.hidden |= other.hidden;
    }

    pub fn offset(&self) -> egui::Vec2 {
        egui::vec2(self.dx.unwrap_or(0.0), self.dy.unwrap_or(0.0))
    }

    /// Changes where and how big it is drawn (and so where it's clicked).
    pub fn moves(&self) -> bool {
        self.offset() != egui::Vec2::ZERO || self.scale.is_some_and(|s| s != 1.0) || self.motion.is_some() || self.place.is_some()
    }
}

/// Where an inserted element goes, relative to its anchor element.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum At {
    /// Just right of it.
    #[default]
    After,
    /// Just left of it.
    Before,
    Above,
    Below,
    /// On top of it, centered.
    Over,
    /// Anywhere in the window: `x`, `y` in % of the window (its top left corner).
    Free,
}

impl At {
    pub const ALL: [At; 6] = [At::After, At::Before, At::Above, At::Below, At::Over, At::Free];
    pub fn name(self) -> &'static str {
        match self {
            At::After => "after",
            At::Before => "before",
            At::Above => "above",
            At::Below => "below",
            At::Over => "over",
            At::Free => "free",
        }
    }
}

/// A new element: a view tree (byob's plugin view vocabulary) placed relative to an element.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Insert {
    pub id: String,
    /// The element it's placed by (a path; ignored for `free`).
    #[serde(default)]
    pub anchor: String,
    #[serde(default)]
    pub at: At,
    /// For `free`: % of the window.
    #[serde(default)]
    pub x: f32,
    #[serde(default)]
    pub y: f32,
    /// Widest it gets, in points (0: as wide as it needs, up to 360).
    #[serde(default)]
    pub width: f32,
    /// Drawn in a card (glass, rounded) rather than bare.
    #[serde(default = "yes")]
    pub card: bool,
    /// The content: a view tree node (`{"type": "column", "children": [...]}` …).
    #[serde(default)]
    pub tree: Value,
    /// Or a plugin's live view instead: `plugin/view`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub view: String,
    /// Who made it: "you", "model", or a plugin id.
    #[serde(default)]
    pub by: String,
    /// Its own data, kept with it: `{state.key}` in its text, `"each": "key"` repeats a
    /// node per item, and its actions change it (`set:`, `add:`, `drop:`, `flip:`, `inc:`).
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub state: serde_json::Map<String, Value>,
}

impl Insert {
    /// Apply a state action (`set:k=v`, `add:k=v`, `drop:k=i`, `flip:k`, `inc:k=n`);
    /// false if `verb` isn't one.
    pub fn act(&mut self, verb: &str, rest: &str) -> bool {
        let (k, v) = rest.split_once('=').map_or((rest.trim(), ""), |(k, v)| (k.trim(), v.trim()));
        let k = k.strip_prefix("state.").unwrap_or(k).to_owned();
        let parsed = || serde_json::from_str::<Value>(v).unwrap_or_else(|_| Value::String(v.to_owned()));
        match verb {
            "set" => {
                self.state.insert(k, parsed());
            }
            "add" if !v.is_empty() => {
                let list = self.state.entry(k).or_insert_with(|| Value::Array(Vec::new()));
                if !list.is_array() {
                    *list = Value::Array(Vec::new());
                }
                if let Value::Array(a) = list {
                    a.push(parsed());
                    let over = a.len().saturating_sub(500);
                    a.drain(..over);
                }
            }
            "add" => {}
            "drop" => {
                if let (Some(Value::Array(a)), Ok(i)) = (self.state.get_mut(&k), v.parse::<usize>())
                    && i < a.len()
                {
                    a.remove(i);
                }
            }
            "flip" => {
                let now = self.state.get(&k).and_then(Value::as_bool).unwrap_or(false);
                self.state.insert(k, Value::Bool(!now));
            }
            "inc" => {
                let by = v.parse::<f64>().unwrap_or(1.0);
                let now = self.state.get(&k).and_then(Value::as_f64).unwrap_or(0.0);
                self.state.insert(k, serde_json::json!(now + by));
            }
            _ => return false,
        }
        true
    }
}

fn yes() -> bool {
    true
}

/// A saved arrangement of the screen, to switch back to.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Scene {
    pub rules: BTreeMap<String, Rule>,
    pub inserts: Vec<Insert>,
}

/// Everything changed about the screen.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FluidDoc {
    /// Path (or glob: `*` within a part, `**` any parts) → rule.
    pub rules: BTreeMap<String, Rule>,
    pub inserts: Vec<Insert>,
    /// Saved scenes by name (`load` puts one on screen).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub scenes: BTreeMap<String, Scene>,
}

impl FluidDoc {
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty() && self.inserts.is_empty()
    }

    /// Keep what's on screen as scene `name`.
    pub fn save(&mut self, name: &str) {
        self.scenes.insert(name.to_owned(), Scene { rules: self.rules.clone(), inserts: self.inserts.clone() });
    }

    /// Put scene `name` on screen (false: there's none).
    pub fn load(&mut self, name: &str) -> bool {
        let Some(sc) = self.scenes.get(name).cloned() else { return false };
        self.rules = sc.rules;
        self.inserts = sc.inserts;
        true
    }

    /// The rule for `path`: every matching glob's (in key order), then its own on top.
    pub fn rule_for(&self, path: &str) -> Option<Rule> {
        self.rule_for_named(path, None)
    }

    /// [`Self::rule_for`], with globs also matched against `named`: the path with its full
    /// label as the last part (`chat/byob.2/text:17k context · 295 out`), however long.
    pub fn rule_for_named(&self, path: &str, named: Option<&str>) -> Option<Rule> {
        let mut out: Option<Rule> = None;
        for (k, r) in &self.rules {
            if k != path && k.contains('*') && (glob(k, path) || named.is_some_and(|n| glob(k, n))) {
                out.get_or_insert_with(Rule::default).merge(r);
            }
        }
        if let Some(r) = self.rules.get(path) {
            out.get_or_insert_with(Rule::default).merge(r);
        }
        out
    }

    /// The rule kept for exactly `path`, made if missing.
    pub fn rule_mut(&mut self, path: &str) -> &mut Rule {
        self.rules.entry(path.to_owned()).or_default()
    }

    pub fn tidy(&mut self) {
        self.rules.retain(|_, r| !r.is_empty());
        while self.rules.len() > MAX_RULES {
            self.rules.pop_first();
        }
        self.inserts.truncate(MAX_INSERTS);
    }
}

/// Does `pattern` match `path`? Parts are split on `/`; `**` matches any number of parts,
/// `*` within a part any characters.
pub fn glob(pattern: &str, path: &str) -> bool {
    let p: Vec<&str> = pattern.split('/').collect();
    let s: Vec<&str> = path.split('/').collect();
    parts(&p, &s)
}

fn parts(p: &[&str], s: &[&str]) -> bool {
    match (p.first(), s.first()) {
        (None, None) => true,
        (Some(&"**"), _) => parts(&p[1..], s) || (!s.is_empty() && parts(p, &s[1..])),
        (Some(a), Some(b)) => part(a, b) && parts(&p[1..], &s[1..]),
        _ => false,
    }
}

fn part(p: &str, s: &str) -> bool {
    let Some((head, rest)) = p.split_once('*') else { return p.eq_ignore_ascii_case(s) };
    let lower = s.to_lowercase();
    let Some(after) = lower.strip_prefix(&head.to_lowercase()) else { return false };
    if rest.is_empty() {
        return true;
    }
    (0..=after.len()).filter(|i| after.is_char_boundary(*i)).any(|i| part(rest, &after[i..]))
}

fn num(v: &Value) -> Option<f32> {
    #[allow(clippy::cast_possible_truncation)] // a point offset or factor
    v.as_f64().map(|f| f as f32).filter(|f| f.is_finite())
}

fn color_ok(s: &str) -> bool {
    parse_color(s).is_some()
}

/// `#rgb`, `#rrggbb` or `#rrggbbaa`, or a few names.
pub fn parse_color(s: &str) -> Option<egui::Color32> {
    let s = s.trim();
    let named = match s.to_ascii_lowercase().as_str() {
        "red" => Some("#ef4444"),
        "orange" => Some("#f97316"),
        "yellow" => Some("#eab308"),
        "green" => Some("#22c55e"),
        "teal" => Some("#14b8a6"),
        "blue" => Some("#3b82f6"),
        "violet" | "purple" => Some("#8b5cf6"),
        "pink" => Some("#ec4899"),
        "white" => Some("#ffffff"),
        "black" => Some("#000000"),
        "gray" | "grey" => Some("#808080"),
        _ => None,
    };
    let hex = named.unwrap_or(s).strip_prefix('#')?;
    let b = |i: usize| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok();
    match hex.len() {
        3 => {
            let d = |i: usize| u8::from_str_radix(hex.get(i..=i)?, 16).ok().map(|v| v * 17);
            Some(egui::Color32::from_rgb(d(0)?, d(1)?, d(2)?))
        }
        6 => Some(egui::Color32::from_rgb(b(0)?, b(2)?, b(4)?)),
        8 => Some(egui::Color32::from_rgba_unmultiplied(b(0)?, b(2)?, b(4)?, b(6)?)),
        _ => None,
    }
}

/// One rule's fields from JSON (`null` clears a field). Problems, by field.
pub fn set_fields(rule: &mut Rule, fields: &serde_json::Map<String, Value>, problems: &mut Vec<String>, path: &str) {
    for (k, v) in fields {
        let clear = v.is_null();
        let bad = |what: &str| format!("{path}: {k} should be {what}, not {v}");
        match k.as_str() {
            "path" => {}
            "dx" | "x" | "right" => {
                rule.dx = if clear {
                    None
                } else {
                    num(v).map(|f| f.clamp(-4000.0, 4000.0)).or_else(|| {
                        problems.push(bad("points"));
                        rule.dx
                    })
                }
            }
            "dy" | "y" | "down" => {
                rule.dy = if clear {
                    None
                } else {
                    num(v).map(|f| f.clamp(-4000.0, 4000.0)).or_else(|| {
                        problems.push(bad("points"));
                        rule.dy
                    })
                }
            }
            "left" => rule.dx = num(v).map(|f| -f),
            "up" => rule.dy = num(v).map(|f| -f),
            "scale" => {
                rule.scale = if clear {
                    None
                } else {
                    num(v).map(|f| f.clamp(0.2, 6.0)).or_else(|| {
                        problems.push(bad("a factor"));
                        rule.scale
                    })
                }
            }
            "opacity" => {
                rule.opacity = if clear {
                    None
                } else {
                    num(v).map(|f| f.clamp(0.0, 1.0)).or_else(|| {
                        problems.push(bad("0 to 1"));
                        rule.opacity
                    })
                }
            }
            "size" | "text_size" => {
                rule.size = if clear {
                    None
                } else {
                    num(v).map(|f| f.clamp(0.3, 6.0)).or_else(|| {
                        problems.push(bad("a factor"));
                        rule.size
                    })
                }
            }
            "radius" => rule.radius = if clear { None } else { num(v).map(|f| f.clamp(0.0, 200.0)) },
            "hidden" | "hide" => rule.hidden = v.as_bool().unwrap_or(false),
            "show" | "visible" => rule.hidden = !v.as_bool().unwrap_or(true),
            "text" | "label" => {
                rule.text = if clear {
                    None
                } else {
                    match v.as_str() {
                        Some(s) => Some(s.chars().take(MAX_TEXT).collect()),
                        None => {
                            problems.push(bad("a string"));
                            rule.text.clone()
                        }
                    }
                }
            }
            "color" | "fill" | "outline" | "background" => {
                let slot = match k.as_str() {
                    "color" => &mut rule.color,
                    "outline" => &mut rule.outline,
                    _ => &mut rule.fill,
                };
                if clear || v.as_str() == Some("") {
                    *slot = None;
                } else if let Some(s) = v.as_str().filter(|s| color_ok(s)) {
                    *slot = Some(s.to_owned());
                } else {
                    problems.push(bad("#rrggbb"));
                }
            }
            "place" => {
                rule.place = match v.as_str().map(str::trim) {
                    None | Some("" | "none") => None,
                    Some(p) => match p.split_once(':') {
                        Some((side, target)) if ["after", "before", "above", "below", "over", "swap"].contains(&side) && !target.trim().is_empty() => {
                            Some(format!("{side}:{}", target.trim()))
                        }
                        _ => {
                            problems.push(bad("after:PATH, before:PATH, above:PATH, below:PATH, over:PATH or swap:PATH"));
                            rule.place.clone()
                        }
                    },
                }
            }
            "reveal" => {
                rule.reveal = match v.as_str().map(str::to_ascii_lowercase) {
                    Some(m) if m == "hover" => Some(m),
                    Some(m) if m.is_empty() || m == "none" || m == "always" => None,
                    None => None,
                    Some(_) => {
                        problems.push(bad("hover or none"));
                        rule.reveal.clone()
                    }
                }
            }
            "motion" | "animate" | "animation" => {
                rule.motion = match v.as_str().map(str::to_ascii_lowercase) {
                    None => None,
                    Some(m) if m.is_empty() || m == "none" || m == "off" => None,
                    Some(m) if ["float", "pulse", "shake", "glow", "bounce"].contains(&m.as_str()) => Some(m),
                    Some(_) => {
                        problems.push(bad("float, bounce, pulse, shake, glow or none"));
                        rule.motion.clone()
                    }
                }
            }
            other => problems
                .push(format!("{path}: no field {other:?} (dx, dy, scale, opacity, hidden, text, size, color, fill, outline, radius, place, reveal, motion)")),
        }
    }
}

/// What a batch of changes did.
#[derive(Debug, Default)]
pub struct Applied {
    pub problems: Vec<String>,
    pub done: Vec<String>,
    /// Asked to list the elements: under this path (empty: all), matching this text.
    pub inspect: Option<(String, String)>,
}

/// Apply `ops` (the model's or a plugin's), JSON:
/// `{"set": [{"path": …, "dx": 8, …}], "clear": [paths], "insert": [Insert], "remove": [ids],
///   "reset": true, "inspect": {"under": …, "find": …}}`.
pub fn apply(doc: &mut FluidDoc, ops: &Value, by: &str) -> Applied {
    let mut a = Applied::default();
    let Some(ops) = ops.as_object() else {
        a.problems.push("fluid: an object of set / clear / insert / remove / reset / inspect".into());
        return a;
    };
    if ops.get("reset").and_then(Value::as_bool) == Some(true) {
        *doc = FluidDoc { scenes: std::mem::take(&mut doc.scenes), ..FluidDoc::default() };
        a.done.push("reset: every element back as byob draws it".into());
    }
    let list = |k: &str| -> Vec<Value> {
        match ops.get(k) {
            Some(Value::Array(v)) => v.clone(),
            Some(Value::Null) | None => Vec::new(),
            Some(v) => vec![v.clone()],
        }
    };
    if let Some(name) = ops.get("load").and_then(Value::as_str) {
        if doc.load(name) {
            a.done.push(format!("loaded scene {name:?}"));
        } else {
            let names: Vec<&str> = doc.scenes.keys().map(String::as_str).collect();
            a.problems.push(format!("load: no scene {name:?} (saved: {})", if names.is_empty() { "none".into() } else { names.join(", ") }));
        }
    }
    for p in list("clear") {
        match p.as_str() {
            Some(path) => {
                let n = doc.rules.len();
                if path.contains('*') && !doc.rules.contains_key(path) {
                    doc.rules.retain(|k, _| !glob(path, k));
                } else {
                    doc.rules.remove(path);
                }
                a.done.push(format!("cleared {path} ({} rule(s))", n - doc.rules.len()));
            }
            None => a.problems.push(format!("clear: paths, not {p}")),
        }
    }
    for s in list("set") {
        let Some(m) = s.as_object() else {
            a.problems.push(format!("set: objects with a path, not {s}"));
            continue;
        };
        let Some(path) = m.get("path").and_then(Value::as_str).map(str::trim).filter(|p| !p.is_empty()) else {
            a.problems.push(format!("set: each needs a \"path\" (from inspect): {s}"));
            continue;
        };
        let mut rule = doc.rules.get(path).cloned().unwrap_or_default();
        set_fields(&mut rule, m, &mut a.problems, path);
        // A swap goes both ways.
        if let Some(other) = rule.place.as_deref().and_then(|p| p.strip_prefix("swap:")).map(str::to_owned)
            && m.contains_key("place")
        {
            doc.rule_mut(&other).place = Some(format!("swap:{path}"));
        }
        a.done.push(format!("{path}: {}", serde_json::to_string(&rule).unwrap_or_default()));
        doc.rules.insert(path.to_owned(), rule);
    }
    for id in list("remove") {
        let id = id.as_str().unwrap_or_default().to_owned();
        let n = doc.inserts.len();
        doc.inserts.retain(|i| i.id != id && !(id.contains('*') && part(&id, &i.id)));
        if doc.inserts.len() == n {
            a.problems.push(format!("remove: no inserted element {id:?}"));
        } else {
            a.done.push(format!("removed {id}"));
        }
    }
    for i in list("insert") {
        let mut v = i.clone();
        if let Some(m) = v.as_object_mut() {
            m.entry("by").or_insert_with(|| Value::String(by.to_owned()));
        }
        match serde_json::from_value::<Insert>(v) {
            Ok(mut ins) => {
                if ins.id.trim().is_empty() {
                    ins.id = format!("new{}", doc.inserts.len() + 1);
                }
                // `"{now}"` / `"{now+1500}"` in its starting state: this moment (+ seconds).
                let now = chrono::Utc::now().timestamp();
                for v in ins.state.values_mut() {
                    if let Some(t) = v.as_str().map(str::trim).and_then(|t| t.strip_prefix("{now")).and_then(|t| t.strip_suffix('}')) {
                        let add = t.trim().trim_start_matches('+').trim().parse::<i64>().unwrap_or(0);
                        *v = Value::from(now + add);
                    }
                }
                if ins.at != At::Free && ins.anchor.trim().is_empty() {
                    a.problems.push(format!("insert {}: an anchor (an element's path), or \"at\": \"free\"", ins.id));
                    continue;
                }
                if ins.tree.is_null() && ins.view.is_empty() {
                    a.problems.push(format!("insert {}: a tree (what to draw) or a view (plugin/view)", ins.id));
                    continue;
                }
                let faults = if ins.view.is_empty() { parse_tree(&ins.tree).1 } else { Vec::new() };
                if !faults.is_empty() {
                    let f: Vec<String> = faults.iter().take(3).map(ToString::to_string).collect();
                    a.problems.push(format!("insert {}: its tree has problems (drawn anyway where it can): {}", ins.id, f.join("; ")));
                }
                a.done.push(format!(
                    "inserted +{} {} {}",
                    ins.id,
                    ins.at.name(),
                    if ins.at == At::Free { format!("{}%,{}%", ins.x, ins.y) } else { ins.anchor.clone() }
                ));
                doc.inserts.retain(|o| o.id != ins.id);
                doc.inserts.push(ins);
            }
            Err(e) => a.problems.push(format!("insert: {e} in {i}")),
        }
    }
    // Saved after the changes above, so "change this and save it as …" works in one go.
    if let Some(name) = ops.get("save").and_then(Value::as_str).map(str::trim).filter(|n| !n.is_empty()) {
        doc.save(name);
        a.done.push(format!("saved this as scene {name:?}"));
    }
    if let Some(name) = ops.get("forget").and_then(Value::as_str) {
        if doc.scenes.remove(name).is_some() {
            a.done.push(format!("forgot scene {name:?}"));
        } else {
            a.problems.push(format!("forget: no scene {name:?}"));
        }
    }
    if let Some(q) = ops.get("inspect") {
        let s = |k: &str| q.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
        a.inspect = Some((s("under"), s("find")));
    }
    doc.tidy();
    a
}

/// A view tree from untrusted JSON: what could be read, and what was dropped.
pub fn parse_tree(v: &Value) -> (Option<byob_protocol::tree::Node>, Vec<byob_protocol::tree::Fault>) {
    let line = serde_json::json!({ "view": v }).to_string();
    match byob_protocol::tree::Reply::parse(&line) {
        Ok((reply, faults)) => (reply.view, faults),
        Err(f) => (None, vec![f]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn globs_match_parts() {
        assert!(glob("composer/button:send", "composer/button:Send"));
        assert!(glob("sidebar/*/title", "sidebar/row.3/title"));
        assert!(glob("**/button:*", "header/actions/button:More"));
        assert!(glob("chat/msg.*", "chat/msg.12"));
        assert!(!glob("chat/msg.*", "chat/msg.12/text#1"));
        assert!(glob("chat/**", "chat/msg.12/text#1"));
        assert!(!glob("header/*", "composer/x"));
    }

    #[test]
    fn rules_layer_globs_under_exact() {
        let mut d = FluidDoc::default();
        apply(&mut d, &json!({"set": [{"path": "**/button:*", "color": "red"}, {"path": "composer/button:Send", "dx": 8}]}), "you");
        let r = d.rule_for("composer/button:Send").unwrap_or_default();
        assert_eq!(r.dx, Some(8.0));
        assert_eq!(r.color.as_deref(), Some("red"));
        assert!(d.rule_for("composer/text#1").is_none());
        apply(&mut d, &json!({"set": [{"path": "chat/byob.*/text:*context*", "hidden": true}]}), "you");
        assert!(d.rule_for_named("chat/byob.2/text#4", Some("chat/byob.2/text:17k context · 295 out")).is_some_and(|r| r.hidden));
        assert!(d.rule_for("chat/byob.2/text#4").is_none());
    }

    #[test]
    fn ops_report_problems_and_keep_going() {
        let mut d = FluidDoc::default();
        let a = apply(
            &mut d,
            &json!({"set": [{"path": "x", "dx": "lots", "dy": 3}, {"dx": 1}], "insert": [{"id": "clock", "at": "free", "x": 80, "y": 5, "tree": {"type": "text", "text": "{time}"}}], "remove": ["nope"]}),
            "model",
        );
        assert_eq!(d.rules.get("x").and_then(|r| r.dy), Some(3.0));
        assert_eq!(d.inserts.len(), 1);
        assert_eq!(d.inserts[0].by, "model");
        assert_eq!(a.problems.len(), 3, "{:?}", a.problems);
    }

    #[test]
    fn state_actions() {
        let mut i: Insert = serde_json::from_value(json!({"id": "todo", "at": "free", "tree": {}})).unwrap_or_else(|_| unreachable!());
        assert!(i.act("add", "items=buy milk"));
        assert!(i.act("add", "items=call mom"));
        assert!(i.act("drop", "items=0"));
        assert!(i.act("inc", "n"));
        assert!(i.act("flip", "state.dark"));
        assert_eq!(i.state.get("items"), Some(&json!(["call mom"])));
        assert_eq!(i.state.get("n"), Some(&json!(1.0)));
        assert_eq!(i.state.get("dark"), Some(&json!(true)));
        assert!(!i.act("explode", "x"));
    }

    #[test]
    fn colors_parse() {
        assert_eq!(parse_color("#fff"), Some(egui::Color32::WHITE));
        assert_eq!(parse_color("red"), parse_color("#ef4444"));
        assert!(parse_color("#12").is_none());
    }
}
