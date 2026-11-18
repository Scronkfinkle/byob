//! Charts drawn from a sheet: `chart_sheet` makes a chart artifact from a sheet's columns
//! (the computed values, formulas included) that remembers where it came from
//! ([`byob_types::chart::ChartSource`]), and [`refresh`] draws those charts again from the
//! sheet each time it is saved, by a tool or by the user's edits in the grid, so a chart
//! never goes stale and nobody copies numbers by hand.

use byob_types::chart::{self, ChartSource};
use ironcalc::base::cell::CellValue;
use serde_json::{Map, Value, json};

use crate::SheetError;
use crate::book::Book;
use crate::refs::{col_name, col_num};
use crate::store::{Source, Store, metas, resolve, s};
use crate::tools::{Ctx, sheet_index};

/// The chart artifact type (the bundled charts plugin's).
pub const CHART_TYPE: &str = "application/vnd.byob.chart+json";
/// Most charts kept in step with one sheet per save.
pub const MAX_LINKED: usize = 8;

type R<T> = Result<T, SheetError>;

fn msg<T>(m: impl Into<String>) -> R<T> {
    Err(SheetError::Msg(m.into()))
}

/// The header row: the first non-empty row of the sheet's used area (1 when empty).
fn header_row(book: &Book, sh: u32) -> i64 {
    book.used(sh).map_or(1, |(r1, _, _, _)| r1)
}

/// What a column is called: its header text, else its letter.
fn header(book: &Book, sh: u32, row: i64, col: i64) -> String {
    let t = book.text(sh, row, col).trim().to_owned();
    if t.is_empty() { col_name(col) } else { t }
}

/// The column the model named: a header (any case) or a letter.
fn column(book: &Book, sh: u32, row: i64, name: &str) -> R<i64> {
    let want = name.trim();
    if want.is_empty() {
        return msg("Name the column, by its header or letter.");
    }
    if let Some((_, _, c1, c2)) = book.used(sh) {
        for c in c1..=c2 {
            if book.text(sh, row, c).trim().eq_ignore_ascii_case(want) {
                return Ok(c);
            }
        }
    }
    if (1..=3).contains(&want.len()) && want.bytes().all(|b| b.is_ascii_alphabetic()) {
        return Ok(col_num(&want.to_ascii_uppercase()));
    }
    let headers: Vec<String> = book.used(sh).map(|(_, _, c1, c2)| (c1..=c2).map(|c| header(book, sh, row, c)).collect()).unwrap_or_default();
    msg(format!("There's no column \"{want}\" in row {row}. Columns: {}.", headers.join(", ")))
}

/// A cell as chart data: numbers as numbers, the rest as shown (`null` when empty).
fn cell(book: &Book, sh: u32, r: i64, c: i64, as_text: bool) -> Value {
    match book.value(sh, r, c) {
        CellValue::None => Value::Null,
        CellValue::Number(n) if !as_text => serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number),
        _ => {
            let t = book.text(sh, r, c);
            if t.is_empty() { Value::Null } else { Value::String(t) }
        }
    }
}

/// The chart's rows and series from the sheet: from under the header to the last row with
/// an `x`. Keys are the headers.
fn rows(book: &mut Book, src: &ChartSource, kind: chart::ChartType) -> R<(String, Vec<Value>, Vec<Map<String, Value>>)> {
    let sh = sheet_index(book, (!src.tab.is_empty()).then_some(src.tab.as_str()), false, None)?;
    let hr = i64::from(src.header_row);
    let xc = col_num(&src.x);
    let x = header(book, sh, hr, xc);
    let mut series = Vec::new();
    let mut keys = Vec::new();
    for letter in &src.series {
        let c = col_num(letter);
        let mut key = header(book, sh, hr, c);
        if key == x || keys.iter().any(|(k, _)| *k == key) {
            key = format!("{key} ({letter})");
        }
        series.push(json!({"key": key, "label": key}));
        keys.push((key, c));
    }
    let last = book.used(sh).map_or(hr, |(_, r2, _, _)| r2);
    let mut out = Vec::new();
    for r in hr + 1..=last {
        let xv = cell(book, sh, r, xc, kind != chart::ChartType::Scatter);
        if xv.is_null() {
            break;
        }
        if out.len() >= chart::MAX_ROWS {
            break;
        }
        let mut row = Map::new();
        row.insert(x.clone(), xv);
        for (k, c) in &keys {
            row.insert(k.clone(), cell(book, sh, r, *c, false));
        }
        out.push(row);
    }
    if out.is_empty() {
        return msg(format!("There are no rows under the header (row {hr}) with a value in column {}.", src.x));
    }
    Ok((x, series, out))
}

/// The chart spec for `src`, checked (`base` keeps the chart's type, title, unit, stacking).
fn spec(book: &mut Book, src: &ChartSource, base: &Map<String, Value>) -> R<Value> {
    let kind = base.get("type").and_then(Value::as_str).and_then(chart::ChartType::from_name).unwrap_or(chart::ChartType::Bar);
    let (x, series, data) = rows(book, src, kind)?;
    let mut o = base.clone();
    o.insert("x".into(), json!(x));
    o.insert("series".into(), Value::Array(series));
    o.insert("data".into(), Value::Array(data.into_iter().map(Value::Object).collect()));
    o.insert("source".into(), serde_json::to_value(src).unwrap_or(Value::Null));
    let checked = chart::validate(&Value::Object(o)).map_err(SheetError::Msg)?;
    serde_json::to_value(&checked).map_err(|e| SheetError::Msg(e.to_string()))
}

/// `chart_sheet`: a chart artifact drawn from a sheet, kept in step with it.
pub fn chart_sheet(store: &mut Store, cx: &Ctx<'_>, args: &Value) -> R<Value> {
    let ws = cx.ws()?;
    let src = resolve(ws, cx.chat, args.get("sheet"), true)?;
    if !src.artifact {
        return msg("chart_sheet charts a spreadsheet artifact: make one from the file with create_sheet (`from`) first, then chart that.");
    }
    let book = store.read_book(&src)?;
    let tab = args.get("tab").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty());
    let sh = sheet_index(book, tab, false, None)?;
    let tab = book.sheet_names().get(sh as usize).cloned().unwrap_or_default();
    let hr = match args.get("header_row").and_then(Value::as_i64) {
        Some(r) if r >= 1 => r,
        _ => header_row(book, sh),
    };
    let x = column(book, sh, hr, args.get("x").and_then(Value::as_str).unwrap_or_default())?;
    let names: Vec<String> = match args.get("series") {
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(str::to_owned).collect(),
        Some(Value::String(t)) => t.split(',').map(str::trim).filter(|t| !t.is_empty()).map(str::to_owned).collect(),
        _ => Vec::new(),
    };
    if names.is_empty() {
        return msg("Give `series`: the column(s) with the numbers, by header or letter.");
    }
    let mut series = Vec::new();
    for n in names.iter().take(chart::MAX_SERIES) {
        series.push(col_name(column(book, sh, hr, n)?));
    }
    let source = ChartSource {
        sheet: src.aid.clone(),
        tab: if book.sheet_names().len() > 1 { tab } else { String::new() },
        header_row: u32::try_from(hr).unwrap_or(1),
        x: col_name(x),
        series,
    };
    let mut base = Map::new();
    base.insert("type".into(), json!(args.get("type").and_then(Value::as_str).unwrap_or("bar")));
    for k in ["title", "unit", "stacked"] {
        if let Some(v) = args.get(k).filter(|v| !v.is_null()) {
            base.insert(k.into(), v.clone());
        }
    }
    let spec = spec(book, &source, &base)?;
    let title = s(&spec, "title").unwrap_or("Chart").to_owned();
    let rows = spec.get("data").and_then(Value::as_array).map_or(0, Vec::len);
    let filename = format!("{}.chart.json", slug(&title));
    let path = store.save_tmp(spec.to_string().as_bytes(), &filename)?;
    let text = format!(
        "Made the chart \"{title}\" from {} ({rows} rows, x = column {}, series = {}). It is linked to the sheet: whenever the sheet changes (your edits or the user's), the chart is drawn again, so don't remake it. It shows in the chat; give the main takeaway, not the numbers.",
        src.label(),
        source.x,
        source.series.join(", ")
    );
    Ok(json!({"text": text, "artifact": {"title": title, "filename": filename, "file": path, "note": format!("From {}", or_title(&src))}}))
}

fn or_title(src: &Source) -> String {
    s(&src.meta, "title").filter(|t| !t.is_empty()).unwrap_or(&src.aid).to_owned()
}

/// A file name from a title.
fn slug(title: &str) -> String {
    let mut out = String::new();
    for ch in title.chars() {
        if ch.is_alphanumeric() {
            out.extend(ch.to_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
        if out.len() >= 60 {
            break;
        }
    }
    let out = out.trim_end_matches('-').to_owned();
    if out.is_empty() { "chart".into() } else { out }
}

/// A chart linked to a sheet, drawn again from it.
pub struct Redrawn {
    /// The chart artifact.
    pub id: String,
    pub title: String,
    pub filename: String,
    /// Its new spec (to save as its next version).
    pub spec: Value,
}

/// The charts of chat `chat` drawn from sheet artifact `sheet`, each drawn again from `book`
/// (just saved) where its rows changed. A chart whose columns no longer work is left as it
/// was.
pub fn refresh(ws: &str, chat: Option<&str>, sheet: &str, book: &mut Book) -> Vec<Redrawn> {
    let mut out = Vec::new();
    for m in metas(ws) {
        if out.len() >= MAX_LINKED {
            break;
        }
        if s(&m, "content_type") != Some(CHART_TYPE) || (chat.is_some() && s(&m, "chat") != chat) {
            continue;
        }
        let Some(path) = crate::store::latest(ws, &m).1 else { continue };
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(old) = serde_json::from_str::<Value>(&text) else { continue };
        let Some(src) = old.get("source").and_then(|v| serde_json::from_value::<ChartSource>(v.clone()).ok()).filter(|src| src.sheet == sheet) else {
            continue;
        };
        let base: Map<String, Value> = ["type", "title", "unit", "stacked"].iter().filter_map(|k| old.get(*k).map(|v| ((*k).to_owned(), v.clone()))).collect();
        let Ok(new) = spec(book, &src, &base) else { continue };
        if new.get("data") == old.get("data") && new.get("series") == old.get("series") {
            continue;
        }
        out.push(Redrawn {
            id: s(&m, "id").unwrap_or_default().to_owned(),
            title: s(&new, "title").unwrap_or("chart").to_owned(),
            filename: s(&m, "filename").unwrap_or("chart.chart.json").to_owned(),
            spec: new,
        });
    }
    out
}

/// Write each redrawn chart's spec to a file for byob to save: (chart, file).
pub fn files(store: &mut Store, redrawn: Vec<Redrawn>) -> Vec<(Redrawn, String)> {
    redrawn.into_iter().filter_map(|r| store.save_tmp(r.spec.to_string().as_bytes(), &r.filename).ok().map(|f| (r, f))).collect()
}
