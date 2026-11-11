//! A benchmark of the sheets backend hosted the way byob hosts it
//! (`byob_plugins::backend::Backend`, in-process): writes, recalculation, the grid view.
//! Ignored by default; run with
//!
//!     cargo test -p byob-sheets --release --test bench -- --ignored --nocapture

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use byob_plugins::backend::{Backend, Out, Sink, Start};
use serde_json::{Value, json};

const T: Duration = Duration::from_secs(120);

/// A fake byob around one backend: an artifact store in a temp workspace (like byob's).
struct Host {
    b: Backend,
    views: Receiver<Value>,
    ws: tempfile::TempDir,
    ids: u32,
}

fn sink() -> (Sink, Receiver<Value>) {
    let (tx, rx) = channel();
    let tx = Mutex::new(tx);
    (
        Arc::new(move |o| {
            if let Out::Msg(m) = o
                && m["type"] == "view"
            {
                let _ = tx.lock().unwrap().send(m);
            }
        }),
        rx,
    )
}

fn init() -> Value {
    json!({"type": "init", "v": 1, "plugin": "sheets", "name": "Spreadsheets", "version": "1.0.0", "byob": "bench", "caps": [],
           "events": [], "settings": {}, "dir": "", "scope": "global", "views": [{"id": "sheet", "site": "artifact", "title": "Spreadsheet"}]})
}

impl Host {
    fn native() -> Self {
        let make = byob_sheets::factory();
        let (s, views) = sink();
        let b = Backend::new("sheets", init(), &[], Box::new(move || Ok(Start::Native(make()))), s);
        Self { b, views, ws: tempfile::tempdir().unwrap(), ids: 0 }
    }

    fn request(&self, method: &str, params: Value) -> Value {
        self.b.request(method, params, T, &|| false).unwrap_or_else(|e| panic!("{method}: {e}"))
    }

    /// A tool call; a returned artifact is stored like byob does. Returns (text, artifact id).
    fn tool(&mut self, name: &str, args: Value) -> (String, Option<String>) {
        let ws = self.ws.path().display().to_string();
        let r = self.request("tool", json!({"name": name, "args": args, "workspace": ws, "chat": "c1"}));
        let text = r.get("text").and_then(Value::as_str).or_else(|| r.as_str()).unwrap_or_default().to_owned();
        let id = r.get("artifact").map(|a| self.store(a));
        (text, id)
    }

    fn store(&mut self, a: &Value) -> String {
        let id = match a["id"].as_str() {
            Some(id) => id.to_owned(),
            None => {
                self.ids += 1;
                format!("{}-{:04x}", Path::new(a["filename"].as_str().unwrap()).file_stem().unwrap().to_string_lossy(), self.ids)
            }
        };
        let dir = self.ws.path().join(".byob/artifacts").join(&id);
        std::fs::create_dir_all(&dir).unwrap();
        let mut meta: Value = std::fs::read(dir.join("artifact.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_else(|| json!({"chat": "c1", "id": id, "title": a["title"], "filename": a["filename"], "content_type": "", "versions": []}));
        if let Some(f) = a["filename"].as_str() {
            meta["filename"] = json!(f);
        }
        meta["content_type"] = json!("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet");
        let n = meta["versions"].as_array().unwrap().len() + 1;
        let rel = format!(".byob/artifacts/{id}/v{n}.xlsx");
        std::fs::copy(a["file"].as_str().unwrap(), self.ws.path().join(&rel)).unwrap();
        meta["versions"].as_array_mut().unwrap().push(json!({"n": n, "path": rel, "note": a["note"]}));
        std::fs::write(dir.join("artifact.json"), meta.to_string()).unwrap();
        id
    }

    fn artifact(&self, id: &str) -> Value {
        let meta: Value = serde_json::from_slice(&std::fs::read(self.ws.path().join(".byob/artifacts").join(id).join("artifact.json")).unwrap()).unwrap();
        let v = meta["versions"].as_array().unwrap().last().unwrap().clone();
        json!({"id": id, "version": v["n"], "latest": v["n"], "path": self.ws.path().join(v["path"].as_str().unwrap()), "filename": meta["filename"],
               "content_type": meta["content_type"], "title": meta["title"], "workspace": self.ws.path(), "chat": "c1"})
    }
}

fn rss_of(pid: u32) -> Option<u64> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    s.lines().find(|l| l.starts_with("VmRSS:"))?.split_whitespace().nth(1)?.parse().ok()
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

#[derive(Default, Debug)]
struct Row {
    start_ms: f64,
    write_1k_rows_ms: f64,
    write_cell_1k_ms: f64,
    write_100k_ms: f64,
    open_view_ms: f64,
    window_ms: f64,
    xlsx: PathBuf,
}

const CATS: [&str; 3] = ["Food", "Rent", "Fun"];

fn rows_1k() -> Value {
    let mut rows = vec![json!(["Date", "Category", "Qty", "Price", "Note"])];
    for r in 0..1000 {
        rows.push(json!([format!("2024-{:02}-{:02}", 1 + r % 12, 1 + r % 28), CATS[r % 3], r % 17 + 1, (r % 50) as f64 + 0.25, format!("row {r}")]));
    }
    Value::Array(rows)
}

/// Rows `from..=to` of a 1000 × 100 sheet of formulas (each cell = the one to its left + 1).
fn rows_100k(from: usize, to: usize) -> Value {
    let col = |n: usize| byob_sheets::refs::col_name(n as i64);
    Value::Array(
        (from..=to).map(|r| Value::Array(std::iter::once(json!(r)).chain((2..=100).map(|c| json!(format!("={}{r}+1", col(c - 1))))).collect())).collect(),
    )
}

/// The same work on one backend.
fn workload(h: &mut Host) -> Row {
    let mut row = Row::default();
    let t = Instant::now();
    h.request("hook", json!({"event": "noop"}));
    row.start_ms = ms(t.elapsed());

    let (_, id) = h.tool("create_sheet", json!({"title": "Spend"}));
    let id = id.unwrap();
    let t = Instant::now();
    let (text, _) = h.tool("write_cells", json!({"sheet": id, "rows": rows_1k()}));
    row.write_1k_rows_ms = ms(t.elapsed());
    assert!(text.contains("Wrote 5005 cells"), "{text}");
    let mut times = Vec::new();
    for i in 0..15 {
        let t = Instant::now();
        let (text, _) = h.tool("write_cells", json!({"sheet": id, "cells": {format!("F{}", i + 2): format!("=C{}*D{}", i + 2, i + 2)}}));
        times.push(ms(t.elapsed()));
        assert!(text.contains("Wrote 1 cell."), "{text}");
    }
    row.write_cell_1k_ms = median(times);

    // the grid: open the newest version, then page through it
    let a = h.artifact(&id);
    let key = format!("{id}@{}", a["version"]);
    let t = Instant::now();
    let tree = h.request("view", json!({"site": "artifact", "view": "sheet", "key": key, "client": 1, "chat": "c1", "artifact": a}));
    row.open_view_ms = ms(t.elapsed());
    assert!(tree.to_string().contains("\"grid\""));
    while h.views.try_recv().is_ok() {}
    let mut times = Vec::new();
    for i in 0..20 {
        let t = Instant::now();
        h.b.event(json!({"type": "event", "event": "view", "view": "sheet", "site": "artifact", "key": key, "client": 1, "chat": "c1",
                         "action": "window", "values": {}, "artifact": a, "grid": {"name": "sheet", "op": "window", "row": 40 * i, "col": 0, "rows": 60, "cols": 12}}));
        loop {
            let m = h.views.recv_timeout(T).expect("a redraw");
            if m["key"] == key.as_str() {
                break;
            }
        }
        times.push(ms(t.elapsed()));
    }
    row.window_ms = median(times);

    // 100k formulas in four calls (a request is at most 1 MiB): the report, error scan,
    // lints and xlsx save of a growing sheet included
    let (_, big) = h.tool("create_sheet", json!({"title": "Big"}));
    let big = big.unwrap();
    let t = Instant::now();
    for k in 0..4 {
        let from = 250 * k + 1;
        let (text, _) = h.tool("write_cells", json!({"sheet": big, "start": format!("A{from}"), "rows": rows_100k(from, from + 249)}));
        assert!(text.contains("Wrote 25000 cells"), "{}", &text[..200.min(text.len())]);
    }
    row.write_100k_ms = ms(t.elapsed());
    row.xlsx = PathBuf::from(h.artifact(&big)["path"].as_str().unwrap());
    row
}

fn self_rss() -> u64 {
    rss_of(std::process::id()).unwrap_or(0)
}

/// Full recalculation of the 100k-formula workbook (ms, median of 5).
fn recalc(xlsx: &Path) -> f64 {
    let mut b = byob_sheets::book::Book::load(xlsx, None).unwrap();
    let mut ts = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        b.evaluate();
        ts.push(ms(t.elapsed()));
    }
    median(ts)
}

#[test]
#[ignore = "a benchmark; see the module docs"]
fn bench_the_sheets_backend() {
    let before = self_rss();
    let mut h = Host::native();
    let r = workload(&mut h);
    let rss = self_rss().saturating_sub(before);
    let recalc = recalc(&r.xlsx);
    println!("\n| measure | sheets (in-process) |");
    println!("|---|---:|");
    println!("| backend start → first reply | {:.2} ms |", r.start_ms);
    println!("| write_cells 1,000 rows × 5 (one call) | {:.1} ms |", r.write_1k_rows_ms);
    println!("| write_cells 1 cell on the 1k-row sheet (median of 15) | {:.1} ms |", r.write_cell_1k_ms);
    println!("| write_cells 100k formulas (4 calls of 25k) | {:.0} ms |", r.write_100k_ms);
    println!("| full recalc of 100k formulas (median of 5) | {recalc:.1} ms |");
    println!("| open the grid view (first draw) | {:.1} ms |", r.open_view_ms);
    println!("| page the grid (window event → redraw, median of 20) | {:.2} ms |", r.window_ms);
    println!("| RSS growth after the workload | +{} MB |", rss / 1024);
}
