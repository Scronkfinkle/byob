//! The grid view the artifact pane shows (`view:sheet`) and the user's edits in it: the
//! tree, window paging, formula text (`i`), frozen strips, tabs, and draft versions.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{Value, json};

use crate::SheetError;
use crate::book::{Book, Kind, file_format};
use crate::refs::{MAX_COLS, MAX_ROWS, range_name};
use crate::store::{Entry, Store, s, sheet_format, xlsx_name};
use crate::tools::{MAX_WRITE_CELLS, errors};

pub const DEFAULT_WIDTH: f64 = 90.0;
const MAX_BLOCK_CELLS: i64 = 40_000;

/// Per artifact (not per version): the sheet shown, each sheet's window and selection.
#[derive(Default)]
pub struct ViewState {
    sheet: Option<String>,
    win: BTreeMap<String, (i64, i64, i64, i64)>,
    sel: BTreeMap<String, Value>,
}

/// Python's `_int`: `int(v)`, `default` where that fails, clamped to `lo..=hi`.
fn int(v: Option<&Value>, default: i64, lo: i64, hi: i64) -> i64 {
    let n = match v {
        Some(Value::Bool(b)) => i64::from(*b),
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f.trunc() as i64)).unwrap_or(default),
        Some(Value::String(t)) => crate::refs::py_int(t).unwrap_or(default),
        _ => default,
    };
    n.clamp(lo, hi.max(lo))
}

fn hex6(t: &str) -> bool {
    let h = t.strip_prefix('#').unwrap_or(t);
    h.len() == 6 && h.chars().all(|c| c.is_ascii_hexdigit())
}

/// Where a view's workbook lives this time.
pub enum Which {
    Live(String),
    Old(std::path::PathBuf, &'static str),
}

/// (which workbook, editable, note) for the artifact byob gave a view.
pub fn view_book(store: &mut Store, a: &Value) -> Result<(Which, bool, String), SheetError> {
    let aid = s(a, "id").unwrap_or_default().to_owned();
    let path = s(a, "path").unwrap_or_default().to_owned();
    let name = s(a, "filename").filter(|f| !f.is_empty()).unwrap_or(&path).to_owned();
    let fmt = sheet_format(s(a, "content_type"), &name).unwrap_or_else(|| file_format(&path));
    let (version, latest) = (a.get("version").filter(|v| !v.is_null()), a.get("latest").filter(|v| !v.is_null()));
    if let (Some(v), Some(l)) = (version, latest)
        && v.as_f64() != l.as_f64()
    {
        store.version_book(Path::new(&path), fmt)?;
        let note = format!("version {}, read-only (the latest is {})", crate::text::pystr(v), crate::text::pystr(l));
        return Ok((Which::Old(path.into(), fmt), false, note));
    }
    store.artifact_entry(&aid, Path::new(&path), fmt, version.and_then(Value::as_i64))?;
    Ok((Which::Live(aid), true, String::new()))
}

fn cell_json(book: &Book, sh: u32, r: i64, c: i64) -> Value {
    let content = book.content(sh, r, c);
    let st = book.style(sh, r, c);
    let font = st.get("font").cloned().unwrap_or(Value::Null);
    let mut extra = serde_json::Map::new();
    if font.get("b").and_then(Value::as_bool).unwrap_or(false) {
        extra.insert("b".into(), json!(true));
    }
    if font.get("i").and_then(Value::as_bool).unwrap_or(false) {
        extra.insert("it".into(), json!(true));
    }
    if let Some(f) = st.get("fill").and_then(|f| f.get("color")).and_then(Value::as_str).filter(|f| hex6(f)) {
        extra.insert("fill".into(), json!(format!("#{}", f.trim_start_matches('#').to_lowercase())));
    }
    if let Some(fc) = font.get("color").and_then(Value::as_str).filter(|f| hex6(f))
        && fc.trim_start_matches('#').to_lowercase() != "000000"
    {
        extra.insert("color".into(), json!(format!("#{}", fc.trim_start_matches('#').to_lowercase())));
    }
    if let Some(al) = st.get("alignment").and_then(|a| a.get("horizontal")).and_then(Value::as_str).filter(|a| ["left", "center", "right"].contains(a)) {
        extra.insert("align".into(), json!(al));
    }
    if content.is_empty() {
        if !extra.is_empty() {
            extra.insert("v".into(), json!(""));
            return Value::Object(extra);
        }
        return Value::Null;
    }
    let k = book.kind(sh, r, c);
    let text = book.text(sh, r, c);
    if k == Kind::S && content == text && extra.is_empty() {
        return Value::String(text);
    }
    let mut d = serde_json::Map::new();
    d.insert("v".into(), json!(text));
    if k != Kind::S {
        d.insert(
            "t".into(),
            json!(match k {
                Kind::N => "n",
                Kind::B => "b",
                Kind::E => "e",
                Kind::S => "s",
            }),
        );
    }
    if content != text {
        d.insert("i".into(), json!(content));
    }
    d.extend(extra);
    Value::Object(d)
}

/// Cells of rows row..row+rows, cols col..col+cols (0-based).
fn block(book: &Book, sh: u32, row: i64, col: i64, rows: i64, cols: i64) -> Value {
    let cells: Vec<Value> = (row..row + rows).map(|r| Value::Array((col..col + cols).map(|c| cell_json(book, sh, r + 1, c + 1)).collect())).collect();
    json!({"row": row, "col": col, "cells": cells})
}

fn round1(w: f64) -> f64 {
    (w * 10.0).round() / 10.0
}

pub fn grid(book: &mut Book, sh: u32, state: &ViewState, editable: bool) -> Value {
    let name = book.sheet_names().get(sh as usize).cloned().unwrap_or_default();
    let used = book.used(sh);
    let (ur2, uc2) = used.map_or((0, 0), |u| (u.1, u.3));
    let total_rows = MAX_ROWS.min((ur2 + 50).max(100));
    let total_cols = MAX_COLS.min((uc2 + 10).max(26));
    let (row, col, rows, cols) = state.win.get(&name).copied().unwrap_or((0, 0, 100, 26));
    let mut rows = rows.clamp(1, 500);
    let cols = cols.clamp(1, 200);
    while rows * cols > MAX_BLOCK_CELLS {
        rows = (rows / 2).max(1);
    }
    let row = row.clamp(0, (total_rows - 1).max(0));
    let col = col.clamp(0, (total_cols - 1).max(0));
    let rows = rows.min(total_rows - row);
    let cols = cols.min(total_cols - col);
    let mut blocks = vec![block(book, sh, row, col, rows, cols)];
    let (fr, fc) = book.frozen(sh);
    let (top, left) = (fr.min(row), fc.min(col));
    if top > 0 {
        blocks.push(block(book, sh, 0, col, top, cols));
    }
    if left > 0 {
        blocks.push(block(book, sh, row, 0, rows, left));
    }
    if top > 0 && left > 0 {
        blocks.push(block(book, sh, 0, 0, top, left));
    }
    let mut widths = serde_json::Map::new();
    for c in (0..left).chain(col..col + cols) {
        let w = book.width(sh, c + 1);
        if (w - DEFAULT_WIDTH).abs() > 0.01 {
            widths.insert(c.to_string(), json!(round1(w)));
        }
    }
    let mut node = json!({
        "type": "grid", "key": "g", "name": "sheet", "rows": total_rows, "cols": total_cols,
        "blocks": blocks, "frozen_rows": fr, "frozen_cols": fc, "widths": widths,
        "default_width": DEFAULT_WIDTH, "editable": editable, "headers": true,
    });
    if let Some(sel) = state.sel.get(&name) {
        node["selection"] = sel.clone();
    }
    node
}

fn current_sheet(book: &mut Book, state: &mut ViewState) -> u32 {
    let names = book.sheet_names();
    if let Some(i) = state.sheet.as_ref().and_then(|n| names.iter().position(|x| x == n)) {
        return u32::try_from(i).unwrap_or(0);
    }
    state.sheet = names.first().cloned();
    0
}

/// The views' state and the artifacts they show.
#[derive(Default)]
pub struct Views {
    states: BTreeMap<String, ViewState>,
}

fn book_of<'a>(store: &'a mut Store, which: &Which) -> Result<&'a mut Book, SheetError> {
    match which {
        Which::Live(aid) => store.entries.get_mut(aid).map(|e| &mut e.book).ok_or_else(|| SheetError::Msg(format!("Artifact {aid} isn't loaded."))),
        Which::Old(p, fmt) => store.version_book(p, fmt),
    }
}

impl Views {
    /// Draw the view for artifact `a` (`None`: byob hasn't said which).
    pub fn draw(&mut self, store: &mut Store, a: Option<&Value>) -> Value {
        let Some(a) = a.filter(|a| a.is_object()) else {
            return json!({"type": "text", "text": "This spreadsheet isn't available.", "tone": "dim"});
        };
        let (which, editable, note) = match view_book(store, a) {
            Ok(x) => x,
            Err(e) => return json!({"type": "column", "children": [{"type": "text", "text": e.text(), "tone": "warning"}]}),
        };
        let book = match book_of(store, &which) {
            Ok(b) => b,
            Err(e) => return json!({"type": "column", "children": [{"type": "text", "text": e.text(), "tone": "warning"}]}),
        };
        let state = self.states.entry(s(a, "id").unwrap_or_default().to_owned()).or_default();
        let sh = current_sheet(book, state);
        let names = book.sheet_names();
        let mut children = Vec::new();
        if names.len() > 1 {
            let tabs: Vec<Value> = names
                .iter()
                .enumerate()
                .map(|(i, n)| json!({"type": "button", "label": n, "action": format!("tab:{n}"), "primary": i == sh as usize}))
                .collect();
            children.push(json!({"type": "row", "key": "tabs", "children": tabs}));
        }
        let mut status = vec![names.get(sh as usize).cloned().unwrap_or_default()];
        match book.used(sh) {
            Some(u) => status.push(format!("{} rows × {} columns", u.1, u.3)),
            None => status.push("empty".into()),
        }
        let ne = errors(book).iter().filter(|e| e.0 == sh).count();
        if ne > 0 {
            status.push(format!("{ne} error{}", if ne != 1 { "s" } else { "" }));
        }
        if !note.is_empty() {
            status.push(note);
        }
        children.push(json!({"type": "text", "key": "status", "style": "small", "tone": "dim", "text": status.join(" · ")}));
        children.push(grid(book, sh, state, editable));
        json!({"type": "column", "children": children})
    }

    /// A view event (`ev`) for artifact `a`: tabs, window, selection, edits (drafts).
    pub fn event(&mut self, store: &mut Store, ev: &Value, a: &Value, out: &byob_plugins::native::Outbox) -> Result<(), SheetError> {
        let state = self.states.entry(s(a, "id").unwrap_or_default().to_owned()).or_default();
        let action = s(ev, "action").unwrap_or_default().to_owned();
        if let Some(tab) = action.strip_prefix("tab:") {
            state.sheet = Some(tab.to_owned());
            return Ok(());
        }
        let g = ev.get("grid").filter(|g| g.is_object()).cloned().unwrap_or_else(|| json!({}));
        let op = s(&g, "op").filter(|o| !o.is_empty()).map_or(action.clone(), str::to_owned);
        let (which, editable, _) = match view_book(store, a) {
            Ok(x) => x,
            Err(e) => {
                out.notice(&e.text(), "error");
                return Ok(());
            }
        };
        let book = book_of(store, &which)?;
        let sh = current_sheet(book, state);
        let name = book.sheet_names().get(sh as usize).cloned().unwrap_or_default();
        let row = int(g.get("row"), 0, 0, MAX_ROWS - 1);
        let col = int(g.get("col"), 0, 0, MAX_COLS - 1);
        match op.as_str() {
            "window" => {
                state.win.insert(name, (row, col, int(g.get("rows"), 100, 1, 500), int(g.get("cols"), 26, 1, 200)));
                return Ok(());
            }
            "select" => {
                let sel =
                    json!({"row": row, "col": col, "to_row": int(g.get("to_row"), row, 0, MAX_ROWS - 1), "to_col": int(g.get("to_col"), col, 0, MAX_COLS - 1)});
                state.sel.insert(name, sel);
                return Ok(());
            }
            "edit" | "paste" | "clear" => {}
            _ => return Ok(()),
        }
        let Which::Live(aid) = &which else {
            out.notice("This version of the spreadsheet is read-only; open the latest one to edit it.", "info");
            return Ok(());
        };
        if !editable {
            out.notice("This version of the spreadsheet is read-only; open the latest one to edit it.", "info");
            return Ok(());
        }
        let label = |book: &mut Book, r1: i64, c1: i64, r2: i64, c2: i64| {
            let names = book.sheet_names();
            let rf = range_name(r1, c1, r2, c2);
            if names.len() > 1 { format!("{}!{rf}", crate::refs::quote_sheet(&names[sh as usize])) } else { rf }
        };
        let mut labels = Vec::new();
        if op == "edit" {
            let v = g.get("value").filter(|v| !v.is_null()).map(crate::text::pystr).unwrap_or_default();
            book.put_text(sh, row + 1, col + 1, &v)?;
            labels.push(label(book, row + 1, col + 1, row + 1, col + 1));
        } else if op == "paste" {
            let text = g.get("text").filter(|v| crate::store::truthy(Some(v))).map(crate::text::pystr).unwrap_or_default();
            let text = text.replace("\r\n", "\n").replace('\r', "\n");
            let mut lines: Vec<&str> = text.split('\n').collect();
            if lines.last() == Some(&"") {
                lines.pop();
            }
            let grid_rows: Vec<Vec<&str>> = lines.iter().map(|l| l.split('\t').collect()).collect();
            let n: usize = grid_rows.iter().map(Vec::len).sum();
            if n as i64 > MAX_WRITE_CELLS {
                out.notice(&format!("That paste has {n} cells; at most {MAX_WRITE_CELLS} fit in one go."), "error");
                return Ok(());
            }
            let mut width = 0;
            for (i, cells) in grid_rows.iter().enumerate() {
                width = width.max(cells.len());
                for (j, v) in cells.iter().enumerate() {
                    let (r, c) = (row + i as i64 + 1, col + j as i64 + 1);
                    if r <= MAX_ROWS && c <= MAX_COLS {
                        book.put_text(sh, r, c, v)?;
                    }
                }
            }
            if !grid_rows.is_empty() && width > 0 {
                labels.push(label(book, row + 1, col + 1, row + grid_rows.len() as i64, col + width as i64));
            }
        } else {
            let tr = int(g.get("to_row"), row, row, MAX_ROWS - 1);
            let tc = int(g.get("to_col"), col, col, MAX_COLS - 1);
            book.clear(sh, row + 1, col + 1, tr + 1, tc + 1)?;
            labels.push(label(book, row + 1, col + 1, tr + 1, tc + 1));
        }
        book.evaluate();
        let data = book.to_xlsx()?;
        let aid = aid.clone();
        let filename = s(a, "filename").unwrap_or_default().to_owned();
        let fmt = sheet_format(s(a, "content_type"), if filename.is_empty() { s(a, "path").unwrap_or_default() } else { &filename });
        let new_name = (fmt != Some("xlsx")).then(|| xlsx_name(&filename, s(a, "id").filter(|i| !i.is_empty()).unwrap_or("sheet")));
        let path = store.save_tmp(&data, new_name.as_deref().unwrap_or(if filename.is_empty() { "sheet.xlsx" } else { &filename }))?;
        if let Some(e) = store.entries.get_mut(&aid) {
            for lb in labels {
                e.user_edited(lb);
            }
            e.wrote(&data, a.get("version").and_then(Value::as_i64));
        }
        let chat = s(a, "chat").filter(|c| !c.is_empty()).or_else(|| s(ev, "chat"));
        let mut m = json!({"type": "artifact", "chat": chat, "file": path, "note": "Edited in the grid", "draft": true, "id": s(a, "id")});
        if let Some(n) = new_name {
            m["filename"] = json!(n);
        }
        out.send(m);
        Ok(())
    }
}

#[allow(dead_code)]
fn entry_of<'a>(store: &'a Store, aid: &str) -> Option<&'a Entry> {
    store.entries.get(aid)
}
