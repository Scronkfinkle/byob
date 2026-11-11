//! Applying patches to a typed tree, without a JSON round trip of anything big.

use serde_json::{Map, Value};

use super::decode::{CellBudget, Decoder, cell};
use super::{
    Fault, GridBlock, Kind, MAX_DEPTH, MAX_GRID_BLOCKS, MAX_GRID_CELLS, MAX_NODES, MAX_PATCHES, MAX_SHAPES, MAX_TABLE_COLUMNS, MAX_TABLE_ROWS, Node, Patch,
    Shape, Target, pointer_segments,
};

/// Apply patches in order. One that can't apply (no such key, a field of the wrong type,
/// over a limit) is skipped and reported; the rest still apply.
pub fn apply(root: &mut Node, patches: Vec<Patch>) -> Vec<Fault> {
    let mut cx = Apply { dec: Decoder::default(), count: root.count() };
    for (i, p) in patches.into_iter().take(MAX_PATCHES).enumerate() {
        let path = format!("/patch/{i}");
        if let Err(msg) = cx.one(root, p, &path) {
            cx.dec.fault(&path, msg);
        }
    }
    cx.dec.finish()
}

struct Apply {
    dec: Decoder,
    /// Nodes in the tree.
    count: usize,
}

/// Index path (child positions) to the node `target` names.
fn locate(root: &Node, at: &Target) -> Result<Vec<usize>, String> {
    if let Some(key) = &at.key {
        let mut idx = Vec::new();
        return if find_path(root, key, &mut idx) { Ok(idx) } else { Err(format!("no node has key {key:?}")) };
    }
    let p = at.path.as_deref().unwrap_or_default();
    let segs = pointer_segments(p).ok_or_else(|| format!("{p:?} isn't a JSON pointer (it starts with /)"))?;
    let (idx, rest) = walk(root, &segs);
    if rest.is_empty() { Ok(idx) } else { Err(format!("no node at {p:?}")) }
}

fn find_path(n: &Node, key: &str, idx: &mut Vec<usize>) -> bool {
    if n.key.as_deref() == Some(key) {
        return true;
    }
    for (i, c) in n.children().iter().enumerate() {
        idx.push(i);
        if find_path(c, key, idx) {
            return true;
        }
        idx.pop();
    }
    false
}

/// Follow `children/<i>` pairs from the root as far as they go: the index path and the
/// segments left.
fn walk<'s>(root: &Node, segs: &'s [String]) -> (Vec<usize>, &'s [String]) {
    let mut idx = Vec::new();
    let mut n = root;
    let mut rest = segs;
    while let [first, i, tail @ ..] = rest {
        let Some(c) = (first == "children").then(|| i.parse::<usize>().ok()).flatten().and_then(|i| n.children().get(i).map(|c| (i, c))) else { break };
        idx.push(c.0);
        n = c.1;
        rest = tail;
    }
    (idx, rest)
}

fn at_mut<'a>(mut n: &'a mut Node, idx: &[usize]) -> Option<&'a mut Node> {
    for &i in idx {
        n = n.children_mut()?.get_mut(i)?;
    }
    Some(n)
}

fn at<'a>(mut n: &'a Node, idx: &[usize]) -> Option<&'a Node> {
    for &i in idx {
        n = n.children().get(i)?;
    }
    Some(n)
}

/// Set what `segs` points at in `v`: replace a list item (or append at the end), set an
/// object member (fields left out because they were defaults are absent, so add them).
fn pointer_set(v: &mut Value, segs: &[&str], value: Value) -> bool {
    let Some((last, parents)) = segs.split_last() else { return false };
    let mut cur = v;
    for s in parents {
        let next = match cur {
            Value::Object(m) => m.get_mut(*s),
            Value::Array(a) => s.parse::<usize>().ok().and_then(|i| a.get_mut(i)),
            _ => None,
        };
        let Some(next) = next else { return false };
        cur = next;
    }
    match cur {
        Value::Object(m) => {
            m.insert((*last).to_owned(), value);
            true
        }
        Value::Array(a) => match last.parse::<usize>() {
            Ok(i) if i < a.len() => {
                a[i] = value;
                true
            }
            Ok(i) if i == a.len() => {
                a.push(value);
                true
            }
            _ => false,
        },
        _ => false,
    }
}

/// The big parts of a node, kept out of a field round trip.
enum Heavy {
    None,
    Children(Vec<Node>),
    Rows(Vec<Vec<Value>>),
    Shapes(Vec<Shape>),
    Blocks(Vec<GridBlock>),
}

fn take_heavy(k: &mut Kind) -> Heavy {
    match k {
        Kind::Column { children } | Kind::Row { children } | Kind::Card { children, .. } => Heavy::Children(std::mem::take(children)),
        Kind::Table { rows, .. } => Heavy::Rows(std::mem::take(rows)),
        Kind::Canvas { shapes, .. } => Heavy::Shapes(std::mem::take(shapes)),
        Kind::Grid { blocks, .. } => Heavy::Blocks(std::mem::take(blocks)),
        _ => Heavy::None,
    }
}

fn put_heavy(k: &mut Kind, h: Heavy) {
    match (k, h) {
        (Kind::Column { children } | Kind::Row { children } | Kind::Card { children, .. }, Heavy::Children(c)) => *children = c,
        (Kind::Table { rows, .. }, Heavy::Rows(r)) => *rows = r,
        (Kind::Canvas { shapes, .. }, Heavy::Shapes(s)) => *shapes = s,
        (Kind::Grid { blocks, .. }, Heavy::Blocks(b)) => *blocks = b,
        _ => {}
    }
}

impl Apply {
    fn one(&mut self, root: &mut Node, p: Patch, path: &str) -> Result<(), String> {
        match p {
            Patch::Put { path: ptr, value } => self.put(root, &ptr, value, path),
            Patch::Replace { at, node } => {
                let idx = locate(root, &at)?;
                self.replace(root, &idx, node)
            }
            Patch::Set { at, fields } => {
                let idx = locate(root, &at)?;
                let depth = idx.len() + 1;
                let n = at_mut(root, &idx).ok_or("no such node")?;
                self.set_fields_in(n, fields, path, depth)
            }
            Patch::Append { at, nodes } => {
                let idx = locate(root, &at)?;
                self.append(root, &idx, nodes)
            }
            Patch::Remove { at } => {
                let idx = locate(root, &at)?;
                let Some((last, parent)) = idx.split_last() else { return Err("can't remove the root (send a new view instead)".into()) };
                let children = at_mut(root, parent).and_then(Node::children_mut).ok_or("no such node")?;
                if *last >= children.len() {
                    return Err("no such node".into());
                }
                let gone = children.remove(*last);
                self.count = self.count.saturating_sub(gone.count());
                Ok(())
            }
            Patch::Rows { at, start, rows: new, len } => {
                let idx = locate(root, &at)?;
                let n = at_mut(root, &idx).ok_or("no such node")?;
                let Kind::Table { rows, .. } = &mut n.kind else { return Err(format!("rows needs a table, not a {}", n.type_name())) };
                let end = start.checked_add(new.len()).filter(|e| *e <= MAX_TABLE_ROWS).ok_or(format!("a table has at most {MAX_TABLE_ROWS} rows"))?;
                if len.is_some_and(|l| l > MAX_TABLE_ROWS) {
                    return Err(format!("a table has at most {MAX_TABLE_ROWS} rows"));
                }
                if rows.len() < end {
                    rows.resize(end, Vec::new());
                }
                for (slot, r) in rows.iter_mut().skip(start).zip(new) {
                    *slot = r;
                }
                if let Some(len) = len {
                    rows.resize(len, Vec::new());
                }
                Ok(())
            }
        }
    }

    /// Check a new subtree fits at depth `depth` with `freed` nodes going away.
    fn fits(&self, depth: usize, new: usize, sub_depth: usize, freed: usize) -> Result<(), String> {
        if depth - 1 + sub_depth > MAX_DEPTH {
            return Err(format!("the tree would nest deeper than {MAX_DEPTH} levels"));
        }
        if self.count.saturating_sub(freed).saturating_add(new) > MAX_NODES {
            return Err(format!("the tree would have more than {MAX_NODES} nodes"));
        }
        Ok(())
    }

    fn replace(&mut self, root: &mut Node, idx: &[usize], node: Node) -> Result<(), String> {
        let old = at(root, idx).ok_or("no such node")?.count();
        let new = node.count();
        self.fits(idx.len() + 1, new, node.depth(), old)?;
        let slot = at_mut(root, idx).ok_or("no such node")?;
        *slot = node;
        self.count = self.count.saturating_sub(old) + new;
        Ok(())
    }

    fn append(&mut self, root: &mut Node, idx: &[usize], nodes: Vec<Node>) -> Result<(), String> {
        let new: usize = nodes.iter().map(Node::count).sum();
        let deepest = nodes.iter().map(Node::depth).max().unwrap_or(0);
        self.fits(idx.len() + 2, new, deepest, 0)?;
        let n = at_mut(root, idx).ok_or("no such node")?;
        let ty = n.type_name();
        let children = n.children_mut().ok_or(format!("append needs a column, row or card, not a {ty}"))?;
        children.extend(nodes);
        self.count += new;
        Ok(())
    }

    /// Decode a node for a place at `depth`, within what's left of the node budget.
    fn decode_node(&mut self, v: Value, path: &str, depth: usize, freed: usize) -> Result<Node, String> {
        self.dec.nodes = self.count.saturating_sub(freed);
        let node = self.dec.node(v, &mut path.to_owned(), depth).ok_or("the value isn't a node that can be shown")?;
        Ok(node)
    }

    fn put(&mut self, root: &mut Node, ptr: &str, value: Value, path: &str) -> Result<(), String> {
        let segs = pointer_segments(ptr).ok_or_else(|| format!("{ptr:?} isn't a JSON pointer (it starts with /)"))?;
        let (idx, rest) = walk(root, &segs);
        let depth = idx.len() + 1;
        let vpath = format!("{path}/value");
        let rest: Vec<&str> = rest.iter().map(String::as_str).collect();
        match rest.as_slice() {
            [] => {
                let old = at(root, &idx).ok_or("no such node")?.count();
                let node = self.decode_node(value, &vpath, depth, old)?;
                self.replace(root, &idx, node)
            }
            ["children", "-"] => {
                let node = self.decode_node(value, &vpath, depth + 1, 0)?;
                self.append(root, &idx, vec![node])
            }
            ["children", i] if i.parse::<usize>().ok() == at(root, &idx).map(|n| n.children().len()) => {
                let node = self.decode_node(value, &vpath, depth + 1, 0)?;
                self.append(root, &idx, vec![node])
            }
            [field] => {
                let n = at_mut(root, &idx).ok_or("no such node")?;
                let mut fields = Map::new();
                fields.insert((*field).to_owned(), value);
                self.set_fields_in(n, fields, path, depth)
            }
            ["rows", i, tail @ ..] => {
                let n = at_mut(root, &idx).ok_or("no such node")?;
                let Kind::Table { rows, columns, .. } = &mut n.kind else { return Err(format!("no rows in a {}", n.type_name())) };
                let i: usize = i.parse().map_err(|_| format!("{i:?} isn't a row index"))?;
                match tail {
                    [] if i < rows.len() => rows[i] = self.dec.row(value, columns, &vpath, 0),
                    [] if i == rows.len() && i < MAX_TABLE_ROWS => rows.push(self.dec.row(value, columns, &vpath, 0)),
                    [j] => {
                        let row = rows.get_mut(i).ok_or(format!("no row {i}"))?;
                        let j: usize = j.parse().ok().filter(|j| *j < MAX_TABLE_COLUMNS).ok_or(format!("{j:?} isn't a column index"))?;
                        if row.len() <= j {
                            row.resize(j + 1, Value::Null);
                        }
                        row[j] = cell(value);
                    }
                    _ => return Err(format!("no row {i}")),
                }
                Ok(())
            }
            ["shapes", i] => {
                let n = at_mut(root, &idx).ok_or("no such node")?;
                let Kind::Canvas { shapes, .. } = &mut n.kind else { return Err(format!("no shapes in a {}", n.type_name())) };
                let i: usize = i.parse().map_err(|_| format!("{i:?} isn't a shape index"))?;
                let shape = self.dec.shape(value, &vpath).ok_or("the value isn't a shape")?;
                let len = shapes.len();
                match shapes.get_mut(i) {
                    Some(slot) => *slot = shape,
                    None if i == len && i < MAX_SHAPES => shapes.push(shape),
                    None => return Err(format!("no shape {i}")),
                }
                Ok(())
            }
            ["blocks", i] => {
                let n = at_mut(root, &idx).ok_or("no such node")?;
                let Kind::Grid { blocks, rows, cols, .. } = &mut n.kind else { return Err(format!("no blocks in a {}", n.type_name())) };
                let i: usize = i.parse().map_err(|_| format!("{i:?} isn't a block index"))?;
                if i > blocks.len() || i >= MAX_GRID_BLOCKS {
                    return Err(format!("no block {i} (a grid has at most {MAX_GRID_BLOCKS})"));
                }
                let others: usize = blocks.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, b)| b.cell_count()).sum();
                let mut budget = CellBudget::with(MAX_GRID_CELLS.saturating_sub(others));
                let block = self.dec.block(value, *rows, *cols, &vpath, &mut budget);
                budget.report(&mut self.dec, &vpath);
                let block = block.ok_or("the value isn't a block that fits the grid")?;
                match blocks.get_mut(i) {
                    Some(slot) => *slot = block,
                    None => blocks.push(block),
                }
                Ok(())
            }
            // Deeper into a small field (a column, an option, the chart spec): round trip the
            // node without its big parts.
            [field, ..] => {
                if matches!(*field, "children" | "rows" | "shapes" | "blocks") {
                    return Err(format!("no node at {ptr:?}"));
                }
                let n = at_mut(root, &idx).ok_or("no such node")?;
                let heavy = take_heavy(&mut n.kind);
                let result = (|| {
                    let mut v = serde_json::to_value(&*n).map_err(|e| e.to_string())?;
                    if !pointer_set(&mut v, &rest, value) {
                        return Err(format!("nothing at {ptr:?}"));
                    }
                    let Value::Object(mut fields) = v else { return Err("not a node".to_owned()) };
                    // Taken out above: empty here, and not to be set.
                    for big in ["children", "rows", "shapes", "blocks"] {
                        fields.remove(big);
                    }
                    Ok(fields)
                })();
                put_heavy(&mut n.kind, heavy);
                self.set_fields_in(n, result?, path, depth)
            }
        }
    }

    /// Change fields of `n`: children, rows and shapes directly, the small fields by a
    /// round trip of the node without its big parts.
    fn set_fields_in(&mut self, n: &mut Node, mut fields: Map<String, Value>, path: &str, depth: usize) -> Result<(), String> {
        let fpath = format!("{path}/fields");
        match fields.remove("type") {
            Some(Value::String(t)) if t == n.type_name() => {}
            None => {}
            Some(_) => return Err("set can't change a node's type; use replace".into()),
        }
        if let Some(k) = fields.remove("key") {
            n.key = if k.is_null() { None } else { self.dec.key(k, &fpath) };
        }
        if let Some(v) = fields.remove("children") {
            let ty = n.type_name();
            let old: usize = n.children().iter().map(Node::count).sum();
            self.dec.nodes = self.count.saturating_sub(old);
            let mut p = fpath.clone();
            let children = self.dec.children(Some(v), &mut p, depth);
            let slot = n.children_mut().ok_or(format!("a {ty} has no children"))?;
            self.count = self.count.saturating_sub(old) + children.iter().map(Node::count).sum::<usize>();
            *slot = children;
        }
        // A grid's `rows` is its size, a small field.
        if !matches!(n.kind, Kind::Grid { .. })
            && let Some(v) = fields.remove("rows")
        {
            let ty = n.type_name();
            let Kind::Table { rows, columns, .. } = &mut n.kind else { return Err(format!("a {ty} has no rows")) };
            *rows = self.dec.rows(Some(v), columns, &fpath);
        }
        if let Some(v) = fields.remove("shapes") {
            let ty = n.type_name();
            let Kind::Canvas { shapes, .. } = &mut n.kind else { return Err(format!("a {ty} has no shapes")) };
            *shapes = self.dec.shapes(Some(v), &fpath);
        }
        if fields.is_empty() {
            return Ok(());
        }
        let ty = n.type_name();
        let heavy = take_heavy(&mut n.kind);
        // New blocks are decoded with the rest (against the grid's new size).
        let new_blocks = matches!(heavy, Heavy::Blocks(_)) && fields.contains_key("blocks");
        let mut obj = match serde_json::to_value(&*n) {
            Ok(Value::Object(o)) => o,
            _ => {
                put_heavy(&mut n.kind, heavy);
                return Err("internal: node didn't encode".into());
            }
        };
        obj.extend(fields);
        let mut d = Decoder::default();
        let new = d.node(Value::Object(obj), &mut fpath.clone(), depth);
        let faults = d.finish();
        match new {
            Some(mut new) if new.type_name() == ty => {
                // Applied, maybe with something cut: say so.
                for f in faults {
                    self.dec.fault(&f.path, f.message);
                }
                if !new_blocks {
                    put_heavy(&mut new.kind, heavy);
                }
                *n = new;
                Ok(())
            }
            _ => {
                put_heavy(&mut n.kind, heavy);
                let why = faults.first().map(|f| format!(" ({})", f.message)).unwrap_or_default();
                Err(format!("the fields didn't fit a {ty}{why}"))
            }
        }
    }
}
