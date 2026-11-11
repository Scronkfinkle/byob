//! Tolerant decoding of untrusted trees, patches and replies: limits applied, unknown nodes
//! shown as their fallback, everything fixed reported as a [`Fault`].

use std::fmt::Write as _;

use serde_json::{Map, Value};

use std::collections::BTreeMap;

use super::{
    Fault, GridBlock, GridCell, GridRange, Kind, MAX_CELL_CHARS, MAX_DEPTH, MAX_FAULTS, MAX_GRID_BLOCKS, MAX_GRID_CELLS, MAX_GRID_COLS, MAX_GRID_ROWS,
    MAX_KEY_CHARS, MAX_NODES, MAX_OPTIONS, MAX_PATCHES, MAX_SHAPES, MAX_TABLE_COLUMNS, MAX_TABLE_ROWS, MAX_TEXT_CHARS, MAX_VALUES, Node, Patch, Reply, Shape,
    TableColumn, Target, TextStyle, Tone, pointer_segment,
};

/// Decode an untrusted tree. Never fails: what can't be shown is left out (or shown as its
/// `fallback`) and listed in the faults. A root that isn't a node is an empty column.
pub fn decode(v: Value) -> (Node, Vec<Fault>) {
    let mut d = Decoder::default();
    let node = d.node(v, &mut String::new(), 1).unwrap_or_else(|| Node::new(Kind::Column { children: Vec::new() }));
    (node, d.finish())
}

/// [`decode`] from JSON text (text that isn't JSON is an empty column and one fault).
pub fn decode_str(s: &str) -> (Node, Vec<Fault>) {
    match serde_json::from_str::<Value>(s) {
        Ok(v) => decode(v),
        Err(e) => (Node::new(Kind::Column { children: Vec::new() }), vec![Fault::new("", format!("not JSON: {e}"))]),
    }
}

pub(crate) fn reply(v: Value) -> (Reply, Vec<Fault>) {
    let mut d = Decoder::default();
    let Value::Object(mut o) = v else {
        return (Reply::default(), vec![Fault::new("", "a reply must be a JSON object")]);
    };
    let mut path = String::new();
    let view = match o.remove("view") {
        None | Some(Value::Null) => None,
        Some(v) => {
            path.push_str("/view");
            let node = d.node(v, &mut path, 1);
            if node.is_none() {
                d.fault("/view", "the view didn't decode, so the old one stays");
            }
            node
        }
    };
    let mut patch = Vec::new();
    match o.remove("patch") {
        None | Some(Value::Null) => {}
        Some(Value::Array(list)) => {
            if list.len() > MAX_PATCHES {
                d.fault("/patch", format!("at most {MAX_PATCHES} patches; the rest are left out"));
            }
            for (i, p) in list.into_iter().take(MAX_PATCHES).enumerate() {
                path.clear();
                let _ = write!(path, "/patch/{i}");
                patch.extend(d.patch(p, &mut path));
            }
        }
        Some(_) => d.fault("/patch", "patch must be a list of patches"),
    }
    let values = d.values(o.remove("values"), "/values");
    (Reply { view, patch, values }, d.finish())
}

/// Decoding state: the faults so far and the node budget.
#[derive(Default)]
pub(crate) struct Decoder {
    faults: Vec<Fault>,
    /// Faults past [`MAX_FAULTS`].
    dropped: usize,
    /// Nodes decoded (or in the tree being patched); at [`MAX_NODES`] the rest are dropped.
    pub(crate) nodes: usize,
    told_too_many: bool,
}

impl Decoder {
    pub(crate) fn fault(&mut self, path: &str, message: impl Into<String>) {
        if self.faults.len() < MAX_FAULTS {
            self.faults.push(Fault::new(path, message));
        } else {
            self.dropped += 1;
        }
    }

    pub(crate) fn finish(mut self) -> Vec<Fault> {
        if self.dropped > 0 {
            self.faults.push(Fault::new("", format!("… and {} more problems", self.dropped)));
        }
        self.faults
    }

    /// Decode one node at `depth` (the root is 1); `None`: nothing to show.
    pub(crate) fn node(&mut self, v: Value, path: &mut String, depth: usize) -> Option<Node> {
        let Value::Object(mut o) = v else {
            self.fault(path, "a node must be an object like {\"type\": \"text\", \"text\": \"hi\"}");
            return None;
        };
        if depth > MAX_DEPTH {
            self.fault(path, format!("nested too deep (at most {MAX_DEPTH} levels); left out"));
            return None;
        }
        if self.nodes >= MAX_NODES {
            if !self.told_too_many {
                self.told_too_many = true;
                self.fault(path, format!("too many nodes (at most {MAX_NODES}); the rest are left out"));
            }
            return None;
        }
        self.nodes += 1;
        let key = match o.remove("key") {
            None | Some(Value::Null) => None,
            Some(k) => self.key(k, path),
        };
        let fallback = match o.remove("fallback") {
            Some(Value::String(s)) => Some(s),
            _ => None,
        };
        let ty = match o.get("type") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                self.fault(path, "a node needs a \"type\" (column, row, card, text, button, …)");
                return self.fallback(key, fallback);
            }
        };
        let kind = match ty.as_str() {
            "column" | "row" | "card" => {
                let children = self.children(o.remove("children"), path, depth);
                match ty.as_str() {
                    "column" => Kind::Column { children },
                    "row" => Kind::Row { children },
                    _ => {
                        let title = match o.remove("title") {
                            None | Some(Value::Null) => String::new(),
                            Some(Value::String(s)) => s,
                            Some(other @ (Value::Number(_) | Value::Bool(_))) => other.to_string(),
                            Some(_) => {
                                self.fault(&format!("{path}/title"), "title must be text");
                                String::new()
                            }
                        };
                        Kind::Card { title, children }
                    }
                }
            }
            "chart" => match o.remove("spec") {
                Some(spec) => match byob_types::chart::validate(&spec) {
                    Ok(spec) => Kind::Chart { spec },
                    Err(e) => {
                        self.fault(&format!("{path}/spec"), e);
                        return self.fallback(key, fallback);
                    }
                },
                None => {
                    self.fault(path, "a chart needs a \"spec\" (version, type, title, x, series, data)");
                    return self.fallback(key, fallback);
                }
            },
            "table" => {
                let rows = o.remove("rows");
                o.insert("rows".into(), Value::Array(Vec::new()));
                match self.leaf(&ty, o, path) {
                    Some(Kind::Table { name, mut columns, height, select, .. }) => {
                        if columns.len() > MAX_TABLE_COLUMNS {
                            self.fault(&format!("{path}/columns"), format!("at most {MAX_TABLE_COLUMNS} columns; the rest are left out"));
                            columns.truncate(MAX_TABLE_COLUMNS);
                        }
                        let rows = self.rows(rows, &columns, path);
                        Kind::Table { name, columns, rows, height, select }
                    }
                    _ => return self.fallback(key, fallback),
                }
            }
            "canvas" => {
                let shapes = o.remove("shapes");
                o.insert("shapes".into(), Value::Array(Vec::new()));
                match self.leaf(&ty, o, path) {
                    Some(Kind::Canvas { width, height, action, .. }) => Kind::Canvas { width, height, shapes: self.shapes(shapes, path), action },
                    _ => return self.fallback(key, fallback),
                }
            }
            "grid" => self.grid(o, path),
            "text" | "markdown" | "chip" | "button" | "input" | "toggle" | "select" | "progress" | "separator" | "image" => match self.leaf(&ty, o, path) {
                Some(kind) => kind,
                None => return self.fallback(key, fallback),
            },
            other => {
                let what = if fallback.is_some() { "showing its fallback" } else { "left out (give it a \"fallback\" text for older clients)" };
                self.fault(path, format!("unknown node type {:?}; {what}", cut_str(other, MAX_KEY_CHARS)));
                return self.fallback(key, fallback);
            }
        };
        let mut node = Node { key, kind };
        self.sanitize(&mut node.kind, path);
        Some(node)
    }

    /// A leaf's fields through serde (children, rows and shapes already taken out).
    fn leaf(&mut self, ty: &str, o: Map<String, Value>, path: &str) -> Option<Kind> {
        match serde_json::from_value::<Kind>(Value::Object(o)) {
            Ok(kind) => Some(kind),
            Err(e) => {
                self.fault(path, format!("{ty}: {e}"));
                None
            }
        }
    }

    fn fallback(&mut self, key: Option<String>, fallback: Option<String>) -> Option<Node> {
        let mut text = fallback?;
        cut(&mut text);
        Some(Node { key, kind: Kind::Text { text, style: TextStyle::Body, tone: Tone::Dim } })
    }

    pub(crate) fn key(&mut self, k: Value, path: &str) -> Option<String> {
        let k = match k {
            Value::String(s) => s,
            Value::Number(n) => n.to_string(),
            _ => {
                self.fault(&format!("{path}/key"), "a key must be text");
                return None;
            }
        };
        if k.chars().count() > MAX_KEY_CHARS {
            self.fault(&format!("{path}/key"), format!("a key can be at most {MAX_KEY_CHARS} characters"));
            return None;
        }
        Some(k)
    }

    pub(crate) fn children(&mut self, v: Option<Value>, path: &mut String, depth: usize) -> Vec<Node> {
        let list = match v {
            None | Some(Value::Null) => return Vec::new(),
            Some(Value::Array(list)) => list,
            Some(_) => {
                self.fault(&format!("{path}/children"), "children must be a list of nodes");
                return Vec::new();
            }
        };
        let at = path.len();
        let mut out = Vec::with_capacity(list.len().min(MAX_NODES));
        for (i, c) in list.into_iter().enumerate() {
            let _ = write!(path, "/children/{i}");
            out.extend(self.node(c, path, depth + 1));
            path.truncate(at);
        }
        out
    }

    /// Table rows: lists of cells in column order, or objects by column key.
    pub(crate) fn rows(&mut self, v: Option<Value>, columns: &[TableColumn], path: &str) -> Vec<Vec<Value>> {
        let mut list = match v {
            None | Some(Value::Null) => return Vec::new(),
            Some(Value::Array(list)) => list,
            Some(_) => {
                self.fault(&format!("{path}/rows"), "rows must be a list of rows (each a list of cells)");
                return Vec::new();
            }
        };
        if list.len() > MAX_TABLE_ROWS {
            self.fault(&format!("{path}/rows"), format!("at most {MAX_TABLE_ROWS} rows; the rest are left out"));
            list.truncate(MAX_TABLE_ROWS);
        }
        list.into_iter().enumerate().map(|(i, r)| self.row(r, columns, path, i)).collect()
    }

    pub(crate) fn row(&mut self, r: Value, columns: &[TableColumn], path: &str, i: usize) -> Vec<Value> {
        match r {
            Value::Array(mut cells) => {
                cells.truncate(MAX_TABLE_COLUMNS);
                cells.into_iter().map(cell).collect()
            }
            Value::Object(mut by_key) => columns.iter().map(|c| by_key.remove(&c.key).map(cell).unwrap_or(Value::Null)).collect(),
            _ => {
                self.fault(&format!("{path}/rows/{i}"), "a row must be a list of cells");
                Vec::new()
            }
        }
    }

    pub(crate) fn shapes(&mut self, v: Option<Value>, path: &str) -> Vec<Shape> {
        let mut list = match v {
            None | Some(Value::Null) => return Vec::new(),
            Some(Value::Array(list)) => list,
            Some(_) => {
                self.fault(&format!("{path}/shapes"), "shapes must be a list");
                return Vec::new();
            }
        };
        if list.len() > MAX_SHAPES {
            self.fault(&format!("{path}/shapes"), format!("at most {MAX_SHAPES} shapes; the rest are left out"));
            list.truncate(MAX_SHAPES);
        }
        list.into_iter().enumerate().filter_map(|(i, s)| self.shape(s, &format!("{path}/shapes/{i}"))).collect()
    }

    pub(crate) fn shape(&mut self, s: Value, path: &str) -> Option<Shape> {
        match serde_json::from_value::<Shape>(s) {
            Ok(mut s) => {
                sanitize_shape(&mut s);
                Some(s)
            }
            Err(e) => {
                self.fault(path, format!("shape (rect, circle, line or text): {e}"));
                None
            }
        }
    }

    pub(crate) fn values(&mut self, v: Option<Value>, path: &str) -> Map<String, Value> {
        match v {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(mut m)) => {
                if m.len() > MAX_VALUES {
                    self.fault(path, format!("at most {MAX_VALUES} values"));
                    m = m.into_iter().take(MAX_VALUES).collect();
                }
                m
            }
            Some(_) => {
                self.fault(path, "values must be an object of field name → value");
                Map::new()
            }
        }
    }

    /// A whole number field up to `max` (over it: `max`, with a fault); `None` if absent or
    /// not a number (with a fault).
    fn uint(&mut self, v: Option<Value>, path: &str, max: u32) -> Option<u32> {
        let v = match v {
            None | Some(Value::Null) => return None,
            Some(v) => v,
        };
        let n = match v.as_u64() {
            Some(n) => n,
            None => match v.as_f64() {
                Some(f) if f.is_finite() && f >= 0.0 => f.floor() as u64,
                _ => {
                    self.fault(path, "must be a whole number from 0");
                    return None;
                }
            },
        };
        if n > u64::from(max) {
            self.fault(path, format!("at most {max}"));
            return Some(max);
        }
        u32::try_from(n).ok()
    }

    fn number(&mut self, v: Option<Value>, path: &str, lo: f32, hi: f32) -> Option<f32> {
        match v {
            None | Some(Value::Null) => None,
            Some(v) => match v.as_f64().map(|f| f as f32).filter(|f| f.is_finite()) {
                Some(f) => Some(f.clamp(lo, hi)),
                None => {
                    self.fault(path, "must be a number");
                    None
                }
            },
        }
    }

    fn flag(&mut self, v: Option<Value>, path: &str, default: bool) -> bool {
        match v {
            None | Some(Value::Null) => default,
            Some(Value::Bool(b)) => b,
            Some(_) => {
                self.fault(path, "must be true or false");
                default
            }
        }
    }

    /// A grid, field by field: what's wrong is fixed or left out, the grid always decodes.
    pub(crate) fn grid(&mut self, mut o: Map<String, Value>, path: &str) -> Kind {
        let name = match o.remove("name") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(s)) => s,
            Some(Value::Number(n)) => n.to_string(),
            Some(_) => {
                self.fault(&format!("{path}/name"), "name must be text");
                String::new()
            }
        };
        let given_rows = self.uint(o.remove("rows"), &format!("{path}/rows"), MAX_GRID_ROWS);
        let given_cols = self.uint(o.remove("cols"), &format!("{path}/cols"), MAX_GRID_COLS);
        let blocks = self.blocks(o.remove("blocks"), given_rows.unwrap_or(MAX_GRID_ROWS), given_cols.unwrap_or(MAX_GRID_COLS), path);
        if given_rows.is_none() || given_cols.is_none() {
            self.fault(path, "a grid needs \"rows\" and \"cols\" (its size); taken from its blocks");
        }
        let rows = given_rows.unwrap_or_else(|| blocks.iter().map(|b| b.row.saturating_add(b.height())).max().unwrap_or(0)).clamp(1, MAX_GRID_ROWS);
        let cols = given_cols.unwrap_or_else(|| blocks.iter().map(|b| b.col.saturating_add(b.width())).max().unwrap_or(0)).clamp(1, MAX_GRID_COLS);
        let mut frozen = |d: &mut Self, field: &str, max: u32| {
            let n = d.uint(o.remove(field), &format!("{path}/{field}"), MAX_GRID_ROWS).unwrap_or(0);
            if n >= max && n > 0 {
                d.fault(&format!("{path}/{field}"), "more than the grid has; one less than its size is frozen");
            }
            n.min(max.saturating_sub(1))
        };
        let frozen_rows = frozen(self, "frozen_rows", rows);
        let frozen_cols = frozen(self, "frozen_cols", cols);
        let mut widths = BTreeMap::new();
        match o.remove("widths") {
            None | Some(Value::Null) => {}
            Some(Value::Object(m)) => {
                for (k, v) in m {
                    let at = format!("{path}/widths/{}", pointer_segment(&cut_str(&k, MAX_KEY_CHARS)));
                    let Some(c) = k.trim().parse::<u32>().ok().filter(|c| *c < cols) else {
                        self.fault(&at, format!("a width's key is a column index from 0 to {}", cols - 1));
                        continue;
                    };
                    if let Some(w) = self.number(Some(v), &at, 16.0, 2000.0) {
                        widths.insert(c, w);
                    }
                }
            }
            Some(_) => self.fault(&format!("{path}/widths"), "widths must be an object of column index → points, like {\"0\": 140}"),
        }
        let default_width = self.number(o.remove("default_width"), &format!("{path}/default_width"), 16.0, 2000.0);
        let editable = self.flag(o.remove("editable"), &format!("{path}/editable"), false);
        let headers = self.flag(o.remove("headers"), &format!("{path}/headers"), true);
        let height = self.number(o.remove("height"), &format!("{path}/height"), 60.0, 4000.0);
        let selection = match o.remove("selection") {
            None | Some(Value::Null) => None,
            Some(Value::Object(mut s)) => {
                let sp = format!("{path}/selection");
                let row = self.uint(s.remove("row"), &format!("{sp}/row"), rows - 1);
                let col = self.uint(s.remove("col"), &format!("{sp}/col"), cols - 1);
                match (row, col) {
                    (Some(row), Some(col)) => {
                        let to_row = self.uint(s.remove("to_row"), &format!("{sp}/to_row"), rows - 1).unwrap_or(row);
                        let to_col = self.uint(s.remove("to_col"), &format!("{sp}/to_col"), cols - 1).unwrap_or(col);
                        Some(GridRange { row, col, to_row, to_col })
                    }
                    _ => {
                        self.fault(&sp, "a selection needs a \"row\" and a \"col\"");
                        None
                    }
                }
            }
            Some(_) => {
                self.fault(&format!("{path}/selection"), "selection must be an object like {\"row\": 0, \"col\": 0}");
                None
            }
        };
        Kind::Grid { name, rows, cols, blocks, frozen_rows, frozen_cols, widths, default_width, editable, headers, height, selection }
    }

    /// A grid's blocks, cut to a `rows` × `cols` sheet and the cell limits.
    pub(crate) fn blocks(&mut self, v: Option<Value>, rows: u32, cols: u32, path: &str) -> Vec<GridBlock> {
        let mut list = match v {
            None | Some(Value::Null) => return Vec::new(),
            Some(Value::Array(list)) => list,
            Some(_) => {
                self.fault(&format!("{path}/blocks"), "blocks must be a list of {\"row\", \"col\", \"cells\"}");
                return Vec::new();
            }
        };
        if list.len() > MAX_GRID_BLOCKS {
            self.fault(&format!("{path}/blocks"), format!("at most {MAX_GRID_BLOCKS} blocks; the rest are left out"));
            list.truncate(MAX_GRID_BLOCKS);
        }
        let mut cx = CellBudget::with(MAX_GRID_CELLS);
        let mut out = Vec::with_capacity(list.len());
        for (i, b) in list.into_iter().enumerate() {
            let bp = format!("{path}/blocks/{i}");
            if let Some(b) = self.block(b, rows, cols, &bp, &mut cx) {
                out.push(b);
            }
        }
        cx.report(self, &format!("{path}/blocks"));
        out
    }

    /// One block; `None` if it can't be placed.
    pub(crate) fn block(&mut self, b: Value, rows: u32, cols: u32, bp: &str, cx: &mut CellBudget) -> Option<GridBlock> {
        let Value::Object(mut bo) = b else {
            self.fault(bp, "a block must be an object like {\"row\": 0, \"col\": 0, \"cells\": [[...]]}");
            return None;
        };
        let at = |d: &mut Self, v: Option<Value>, field: &str| -> Option<u32> {
            match v {
                None | Some(Value::Null) => Some(0),
                v => d.uint(v, &format!("{bp}/{field}"), u32::MAX),
            }
        };
        let row = at(self, bo.remove("row"), "row")?;
        let col = at(self, bo.remove("col"), "col")?;
        if row >= rows || col >= cols {
            self.fault(bp, format!("the block starts outside the grid ({rows} rows × {cols} columns); left out"));
            return None;
        }
        let list = match bo.remove("cells") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(l)) => l,
            Some(_) => {
                self.fault(&format!("{bp}/cells"), "cells must be a list of rows (each a list of cells)");
                return None;
            }
        };
        let (max_h, max_w) = (usize::try_from(rows - row).unwrap_or(usize::MAX), usize::try_from(cols - col).unwrap_or(usize::MAX));
        if list.len() > max_h {
            self.fault(&format!("{bp}/cells"), "rows past the grid's last row are left out");
        }
        let mut cells = Vec::with_capacity(list.len().min(max_h));
        let mut wide = false;
        for (r, rv) in list.into_iter().take(max_h).enumerate() {
            if cx.left == 0 {
                cx.over = true;
                break;
            }
            let Value::Array(mut cs) = rv else {
                self.fault(&format!("{bp}/cells/{r}"), "a row of cells must be a list");
                cells.push(Vec::new());
                continue;
            };
            if cs.len() > max_w {
                wide = true;
                cs.truncate(max_w);
            }
            if cs.len() > cx.left {
                cx.over = true;
                cs.truncate(cx.left);
            }
            cx.left -= cs.len();
            let row: Vec<GridCell> = cs
                .into_iter()
                .enumerate()
                .map(|(c, v)| match GridCell::decode(v) {
                    Ok((cell, long)) => {
                        cx.long |= long;
                        cell
                    }
                    Err(msg) => {
                        if cx.bad == 0 {
                            cx.first_bad = Some((format!("{bp}/cells/{r}/{c}"), msg));
                        }
                        cx.bad += 1;
                        GridCell::default()
                    }
                })
                .collect();
            cells.push(row);
        }
        if wide {
            self.fault(&format!("{bp}/cells"), "cells past the grid's last column are left out");
        }
        Some(GridBlock { row, col, cells })
    }

    /// One patch; `None` (with a fault) if it can't be read.
    pub(crate) fn patch(&mut self, v: Value, path: &mut String) -> Option<Patch> {
        const OPS: &str = "put, replace, set, append, remove or rows";
        let Value::Object(o) = v else {
            self.fault(path, format!("a patch must be an object with one operation ({OPS})"));
            return None;
        };
        if o.len() != 1 {
            self.fault(path, format!("a patch has exactly one operation ({OPS})"));
            return None;
        }
        let (op, body) = o.into_iter().next()?;
        let _ = write!(path, "/{}", pointer_segment(&cut_str(&op, MAX_KEY_CHARS)));
        let Value::Object(mut b) = body else {
            self.fault(path, "the operation's value must be an object");
            return None;
        };
        if op == "put" {
            let Some(p) = text_field(self, &mut b, "path", path) else {
                self.fault(path, "put needs a \"path\" (a JSON pointer like /children/0/text)");
                return None;
            };
            let Some(value) = b.remove("value") else {
                self.fault(path, "put needs a \"value\"");
                return None;
            };
            return Some(Patch::Put { path: p, value });
        }
        let at = Target { key: text_field(self, &mut b, "key", path), path: text_field(self, &mut b, "path", path) };
        if at.key.is_none() && at.path.is_none() {
            self.fault(path, "a patch needs the \"key\" of the node it changes (or its \"path\")");
            return None;
        }
        match op.as_str() {
            "replace" => {
                let at_len = path.len();
                path.push_str("/node");
                let node = b.remove("node").and_then(|n| self.node(n, path, 1));
                path.truncate(at_len);
                match node {
                    Some(node) => Some(Patch::Replace { at, node }),
                    None => {
                        self.fault(path, "replace needs a \"node\"");
                        None
                    }
                }
            }
            "set" => match b.remove("fields") {
                Some(Value::Object(fields)) => Some(Patch::Set { at, fields }),
                _ => {
                    self.fault(path, "set needs \"fields\", an object like {\"text\": \"3 left\"}");
                    None
                }
            },
            "append" => {
                let nodes = match b.remove("nodes").or_else(|| b.remove("node").map(|n| Value::Array(vec![n]))) {
                    Some(Value::Array(list)) => {
                        let at_len = path.len();
                        let mut out = Vec::new();
                        for (i, n) in list.into_iter().enumerate() {
                            let _ = write!(path, "/nodes/{i}");
                            out.extend(self.node(n, path, 1));
                            path.truncate(at_len);
                        }
                        out
                    }
                    _ => {
                        self.fault(path, "append needs \"nodes\", a list of nodes");
                        return None;
                    }
                };
                Some(Patch::Append { at, nodes })
            }
            "remove" => Some(Patch::Remove { at }),
            "rows" => {
                let start = match b.remove("start") {
                    None | Some(Value::Null) => 0,
                    Some(v) => match v.as_u64().and_then(|n| usize::try_from(n).ok()) {
                        Some(n) => n,
                        None => {
                            self.fault(&format!("{path}/start"), "start must be a row index");
                            return None;
                        }
                    },
                };
                let len = match b.remove("len") {
                    None | Some(Value::Null) => None,
                    Some(v) => match v.as_u64().and_then(|n| usize::try_from(n).ok()) {
                        Some(n) => Some(n),
                        None => {
                            self.fault(&format!("{path}/len"), "len must be a row count");
                            return None;
                        }
                    },
                };
                let rows = self.rows(b.remove("rows"), &[], path);
                Some(Patch::Rows { at, start, rows, len })
            }
            _ => {
                self.fault(path, format!("unknown patch operation (use {OPS})"));
                None
            }
        }
    }

    /// Cut long strings and lists, make numbers finite.
    fn sanitize(&mut self, k: &mut Kind, path: &str) {
        let mut long = false;
        let mut c = |s: &mut String| long |= cut(s);
        match k {
            Kind::Column { .. } | Kind::Row { .. } | Kind::Separator {} | Kind::Chart { .. } => {}
            // Checked as it was decoded.
            Kind::Grid { name, .. } => c(name),
            Kind::Card { title, .. } => c(title),
            Kind::Text { text, .. } | Kind::Markdown { text } | Kind::Chip { text, .. } => c(text),
            Kind::Button { label, action, .. } => {
                c(label);
                c(action);
            }
            Kind::Input { name, value, placeholder, submit } => {
                c(name);
                c(value);
                c(placeholder);
                submit.iter_mut().for_each(c);
            }
            Kind::Toggle { name, label, action, .. } => {
                c(name);
                c(label);
                action.iter_mut().for_each(c);
            }
            Kind::Select { name, options, value, action } => {
                if options.len() > MAX_OPTIONS {
                    self.fault(&format!("{path}/options"), format!("at most {MAX_OPTIONS} options; the rest are left out"));
                    options.truncate(MAX_OPTIONS);
                }
                c(name);
                c(value);
                options.iter_mut().for_each(&mut c);
                action.iter_mut().for_each(c);
            }
            Kind::Progress { value, label } => {
                *value = if value.is_finite() { value.clamp(0.0, 1.0) } else { 0.0 };
                c(label);
            }
            Kind::Table { name, columns, height, select, .. } => {
                c(name);
                for col in columns.iter_mut() {
                    c(&mut col.key);
                    c(&mut col.label);
                    col.width = col.width.filter(|w| w.is_finite()).map(|w| w.clamp(20.0, 2000.0));
                }
                *height = height.filter(|h| h.is_finite()).map(|h| h.clamp(40.0, 4000.0));
                select.iter_mut().for_each(c);
            }
            Kind::Image { media, artifact, alt, width, height } => {
                media.iter_mut().for_each(&mut c);
                artifact.iter_mut().for_each(&mut c);
                c(alt);
                for side in [width, height] {
                    *side = side.filter(|s| s.is_finite()).map(|s| s.clamp(1.0, 4000.0));
                }
            }
            Kind::Canvas { width, height, action, .. } => {
                for side in [width, height] {
                    *side = if side.is_finite() { side.clamp(1.0, 4000.0) } else { 100.0 };
                }
                action.iter_mut().for_each(c);
            }
        }
        if long {
            self.fault(path, format!("text longer than {MAX_TEXT_CHARS} characters was cut"));
        }
    }
}

/// What's left of a grid's cell limit, and what was wrong with its cells (reported once).
#[derive(Default)]
pub(crate) struct CellBudget {
    pub(crate) left: usize,
    over: bool,
    long: bool,
    bad: usize,
    first_bad: Option<(String, &'static str)>,
}

impl CellBudget {
    pub(crate) fn with(left: usize) -> Self {
        Self { left, ..Self::default() }
    }

    pub(crate) fn report(self, d: &mut Decoder, path: &str) {
        if let Some((path, msg)) = self.first_bad {
            let more = if self.bad > 1 { format!(" ({} cells like it are empty)", self.bad) } else { "; it is empty".to_owned() };
            d.fault(&path, format!("{msg}{more}"));
        }
        if self.long {
            d.fault(path, format!("grid cell text longer than {MAX_CELL_CHARS} characters was cut"));
        }
        if self.over {
            d.fault(path, format!("a grid has at most {MAX_GRID_CELLS} cells in its blocks; the rest are left out"));
        }
    }
}

/// A text field of a patch (numbers are taken as text).
fn text_field(d: &mut Decoder, b: &mut Map<String, Value>, field: &str, path: &str) -> Option<String> {
    match b.remove(field) {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(_) => {
            d.fault(&format!("{path}/{field}"), format!("{field} must be text"));
            None
        }
    }
}

/// A table cell: a scalar (lists and objects become their JSON text), long text cut.
pub(crate) fn cell(v: Value) -> Value {
    match v {
        Value::String(mut s) => {
            cut(&mut s);
            Value::String(s)
        }
        v @ (Value::Null | Value::Bool(_) | Value::Number(_)) => v,
        other => {
            let mut s = other.to_string();
            cut(&mut s);
            Value::String(s)
        }
    }
}

fn sanitize_shape(s: &mut Shape) {
    let fin = |v: &mut f32| {
        if !v.is_finite() {
            *v = 0.0;
        }
        *v = v.clamp(-100_000.0, 100_000.0);
    };
    match s {
        Shape::Rect { x, y, w, h, radius, .. } => [x, y, w, h, radius].into_iter().for_each(fin),
        Shape::Circle { x, y, r, .. } => [x, y, r].into_iter().for_each(fin),
        Shape::Line { points, width, .. } => {
            points.truncate(MAX_SHAPES);
            points.iter_mut().flatten().for_each(fin);
            fin(width);
            *width = width.clamp(0.0, 100.0);
        }
        Shape::Text { x, y, text, size, .. } => {
            [x, y].into_iter().for_each(fin);
            cut(text);
            *size = if size.is_finite() { size.clamp(6.0, 96.0) } else { 14.0 };
        }
    }
}

/// Cut `s` to [`MAX_TEXT_CHARS`]; whether it was longer.
fn cut(s: &mut String) -> bool {
    // A byte length under the limit can't be over it in characters.
    if s.len() <= MAX_TEXT_CHARS {
        return false;
    }
    match s.char_indices().nth(MAX_TEXT_CHARS) {
        Some((at, _)) => {
            s.truncate(at);
            true
        }
        None => false,
    }
}

fn cut_str(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}
