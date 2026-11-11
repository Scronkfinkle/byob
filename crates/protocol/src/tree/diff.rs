//! Engine-side diff: a plugin may always send its whole view; clients get the patches that
//! turn the old tree into the new one (JSON-pointer puts, appends, removes, row writes).

use serde::Serialize;
use serde_json::Value;

use super::{Kind, Node, Patch, Target, pointer_segment};

/// Changed rows of one table written as single puts; more become one `rows` patch.
const ROW_PUTS: usize = 64;

/// Patches that turn `old` into `new` (`apply(old, diff(old, new))` gives `new`); none if
/// they are equal.
pub fn diff(old: &Node, new: &Node) -> Vec<Patch> {
    let mut out = Vec::new();
    node(old, new, &mut String::new(), &mut out);
    out
}

fn put(path: &str, field: &str, v: &impl Serialize, out: &mut Vec<Patch>) {
    let path = if field.is_empty() { path.to_owned() } else { format!("{path}/{field}") };
    out.push(Patch::Put { path, value: serde_json::to_value(v).unwrap_or(Value::Null) });
}

fn node(a: &Node, b: &Node, path: &mut String, out: &mut Vec<Patch>) {
    if a == b {
        return;
    }
    if a.key != b.key || a.type_name() != b.type_name() {
        return put(path, "", b, out);
    }
    match (&a.kind, &b.kind) {
        (Kind::Column { children: x }, Kind::Column { children: y }) | (Kind::Row { children: x }, Kind::Row { children: y }) => children(x, y, path, out),
        (Kind::Card { title: t1, children: x }, Kind::Card { title: t2, children: y }) => {
            if t1 != t2 {
                put(path, "title", t2, out);
            }
            children(x, y, path, out);
        }
        (Kind::Table { name: n1, columns: c1, rows: r1, height: h1, select: s1 }, Kind::Table { name: n2, columns: c2, rows: r2, height: h2, select: s2 }) => {
            if n1 != n2 {
                put(path, "name", n2, out);
            }
            if c1 != c2 {
                put(path, "columns", c2, out);
            }
            if h1 != h2 {
                put(path, "height", h2, out);
            }
            if s1 != s2 {
                put(path, "select", s2, out);
            }
            rows(r1, r2, path, out);
        }
        (Kind::Canvas { width: w1, height: h1, shapes: s1, action: a1 }, Kind::Canvas { width: w2, height: h2, shapes: s2, action: a2 }) => {
            if w1 != w2 {
                put(path, "width", w2, out);
            }
            if h1 != h2 {
                put(path, "height", h2, out);
            }
            if a1 != a2 {
                put(path, "action", a2, out);
            }
            if s1.len() == s2.len() {
                for (i, (x, y)) in s1.iter().zip(s2).enumerate() {
                    if x != y {
                        put(path, &format!("shapes/{i}"), y, out);
                    }
                }
            } else {
                put(path, "shapes", s2, out);
            }
        }
        _ => leaf(a, b, path, out),
    }
}

/// Small nodes: the fields that changed, or the whole node if fields came or went.
fn leaf(a: &Node, b: &Node, path: &str, out: &mut Vec<Patch>) {
    match (serde_json::to_value(a), serde_json::to_value(b)) {
        (Ok(Value::Object(x)), Ok(Value::Object(y))) if x.len() == y.len() && x.keys().all(|k| y.contains_key(k)) => {
            for (k, v) in &y {
                if x.get(k) != Some(v) {
                    put(path, &pointer_segment(k), v, out);
                }
            }
        }
        _ => put(path, "", b, out),
    }
}

fn children(x: &[Node], y: &[Node], path: &mut String, out: &mut Vec<Patch>) {
    let at = path.len();
    if x.len() == y.len() {
        for (i, (a, b)) in x.iter().zip(y).enumerate() {
            path.push_str(&format!("/children/{i}"));
            node(a, b, path, out);
            path.truncate(at);
        }
        return;
    }
    // Added at the end.
    if y.len() > x.len() && y.get(..x.len()) == Some(x) {
        out.push(Patch::Append { at: Target::path(path.as_str()), nodes: y.get(x.len()..).unwrap_or_default().to_vec() });
        return;
    }
    // One removed.
    if x.len() == y.len() + 1 {
        let i = x.iter().zip(y).position(|(a, b)| a != b).unwrap_or(y.len());
        if x.get(i + 1..) == y.get(i..) {
            out.push(Patch::Remove { at: Target::path(format!("{path}/children/{i}")) });
            return;
        }
    }
    put(path, "children", &y, out);
}

fn rows(x: &[Vec<Value>], y: &[Vec<Value>], path: &str, out: &mut Vec<Patch>) {
    let common = x.len().min(y.len());
    let changed: Vec<usize> = (0..common).filter(|&i| x.get(i) != y.get(i)).collect();
    let resized = x.len() != y.len();
    if changed.len() > ROW_PUTS {
        let first = changed.first().copied().unwrap_or(0);
        let rows = y.get(first..).unwrap_or_default().to_vec();
        out.push(Patch::Rows { at: Target::path(path), start: first, rows, len: resized.then_some(y.len()) });
        return;
    }
    for i in changed {
        let (Some(a), Some(b)) = (x.get(i), y.get(i)) else { continue };
        let cells: Vec<usize> = (0..b.len()).filter(|&j| a.get(j) != b.get(j)).collect();
        if a.len() == b.len() && cells.len() * 2 <= b.len() {
            for j in cells {
                put(path, &format!("rows/{i}/{j}"), &b.get(j), out);
            }
        } else {
            put(path, &format!("rows/{i}"), b, out);
        }
    }
    if resized {
        out.push(Patch::Rows { at: Target::path(path), start: common, rows: y.get(common..).unwrap_or_default().to_vec(), len: Some(y.len()) });
    }
}
