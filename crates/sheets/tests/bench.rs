//! The comparison bench of docs/sheets-comparison.md: the Python `sheets` backend (a
//! process) and the native one (in-process), hosted the way byob hosts them
//! (`byob_plugins::backend::Backend`), doing the same work. Ignored by default; run with
//!
//!     BYOB_SHEETS_PYTHON=/path/to/venv/bin/python \
//!       cargo test -p byob-sheets --release --test bench -- --ignored --nocapture
//!
//! (`BYOB_SHEETS_COLD=1` also times building a fresh venv with pip, which needs the network
//! or a warm pip cache.)

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use byob_plugins::backend::{Backend, Launch, Out, Sink, Start};
use serde_json::{Value, json};

const T: Duration = Duration::from_secs(120);

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

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
    fn python(python: &str) -> Self {
        let plugin = root().join("crates/bundled/plugins/sheets");
        let helper = root().join("crates/plugins/src");
        let python = python.to_owned();
        let prepare = Box::new(move || {
            let mut env = std::collections::BTreeMap::new();
            env.insert("PYTHONPATH".to_owned(), helper.display().to_string());
            env.insert("PYTHONUNBUFFERED".to_owned(), "1".to_owned());
            env.insert("PYTHONDONTWRITEBYTECODE".to_owned(), "1".to_owned());
            Ok(Start::from(Launch { argv: vec![python.clone().into(), "-B".into(), "backend.py".into()], cwd: plugin.clone(), env, remove_env: vec![] }))
        });
        let (s, views) = sink();
        Self { b: Backend::new("sheets", init(), &[], prepare, s), views, ws: tempfile::tempdir().unwrap(), ids: 0 }
    }

    fn native() -> Self {
        let make = byob_sheets::factory();
        let (s, views) = sink();
        let b = Backend::new("sheets-native", init(), &[], Box::new(move || Ok(Start::Native(make()))), s);
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

    fn rss_kb(&self) -> Option<u64> {
        self.b.status().pid.and_then(rss_of)
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
    rss_kb: u64,
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

/// Full recalculation of the 100k-formula workbook: (Python ms, native ms), median of 5.
fn recalc(python: &str, xlsx: &Path) -> (f64, f64) {
    let script = format!(
        "import sys, time; sys.path.insert(0, {plugin:?}); import sheets_book as bk\nb = bk.Book.load({xlsx:?})\nts = []\nfor _ in range(5):\n    t = time.perf_counter(); b.evaluate(); ts.append(time.perf_counter() - t)\nts.sort(); print(ts[2] * 1000)\n",
        plugin = root().join("crates/bundled/plugins/sheets").display().to_string(),
        xlsx = xlsx.display().to_string()
    );
    let out = std::process::Command::new(python).args(["-B", "-c", &script]).output().unwrap();
    let py: f64 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(f64::NAN);
    let mut b = byob_sheets::book::Book::load(xlsx, None).unwrap();
    let mut ts = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        b.evaluate();
        ts.push(ms(t.elapsed()));
    }
    (py, median(ts))
}

fn cold(python_base: &str) -> Option<(f64, f64)> {
    std::env::var_os("BYOB_SHEETS_COLD")?;
    let dir = tempfile::tempdir().unwrap();
    let venv = dir.path().join("venv");
    let t = Instant::now();
    let ok = std::process::Command::new(python_base).args(["-m", "venv"]).arg(&venv).status().unwrap().success()
        && std::process::Command::new(venv.join("bin/pip")).args(["install", "-q", "ironcalc==0.9.1"]).status().unwrap().success();
    let build = ms(t.elapsed());
    assert!(ok, "the venv didn't build");
    let t = Instant::now();
    std::process::Command::new(venv.join("bin/python")).args(["-c", "import ironcalc"]).status().unwrap();
    Some((build, ms(t.elapsed())))
}

#[test]
#[ignore = "a benchmark: needs BYOB_SHEETS_PYTHON (a Python with ironcalc); see the module docs"]
fn compare_python_and_native_backends() {
    let python = std::env::var("BYOB_SHEETS_PYTHON").expect("BYOB_SHEETS_PYTHON: a Python with ironcalc==0.9.1");
    // native first, so this process's RSS growth is the native backend's alone
    let before = self_rss();
    let mut n = Host::native();
    let rn = workload(&mut n);
    let native_rss = self_rss().saturating_sub(before);
    let mut p = Host::python(&python);
    let mut rp = workload(&mut p);
    rp.rss_kb = p.rss_kb().unwrap_or(0);
    // the native one as a process too (same code, its own address space)
    let bin = env!("CARGO_BIN_EXE_byob-sheets-backend").to_owned();
    let prepare = Box::new(move || Ok(Start::from(Launch { argv: vec![bin.clone().into()], cwd: std::env::temp_dir(), ..Default::default() })));
    let (s, views) = sink();
    let mut np = Host { b: Backend::new("sheets-native", init(), &[], prepare, s), views, ws: tempfile::tempdir().unwrap(), ids: 0 };
    let rnp = workload(&mut np);
    let np_rss = np.rss_kb().unwrap_or(0);
    let (py_recalc, rs_recalc) = recalc(&python, &rp.xlsx);
    println!("\n| measure | Python backend | native (in-process) | native as a process |");
    println!("|---|---:|---:|---:|");
    println!("| backend start → first reply | {:.1} ms | {:.2} ms | {:.1} ms |", rp.start_ms, rn.start_ms, rnp.start_ms);
    println!("| write_cells 1,000 rows × 5 (one call) | {:.1} ms | {:.1} ms | {:.1} ms |", rp.write_1k_rows_ms, rn.write_1k_rows_ms, rnp.write_1k_rows_ms);
    println!(
        "| write_cells 1 cell on the 1k-row sheet (median of 15) | {:.1} ms | {:.1} ms | {:.1} ms |",
        rp.write_cell_1k_ms, rn.write_cell_1k_ms, rnp.write_cell_1k_ms
    );
    println!("| write_cells 100k formulas (4 calls of 25k) | {:.0} ms | {:.0} ms | {:.0} ms |", rp.write_100k_ms, rn.write_100k_ms, rnp.write_100k_ms);
    println!("| full recalc of 100k formulas (median of 5) | {py_recalc:.1} ms | {rs_recalc:.1} ms | — |");
    println!("| open the grid view (first draw) | {:.1} ms | {:.1} ms | {:.1} ms |", rp.open_view_ms, rn.open_view_ms, rnp.open_view_ms);
    println!("| page the grid (window event → redraw, median of 20) | {:.2} ms | {:.2} ms | {:.2} ms |", rp.window_ms, rn.window_ms, rnp.window_ms);
    println!(
        "| steady RSS after the workload | {} MB (process) | +{} MB (byob's process) | {} MB (process) |",
        rp.rss_kb / 1024,
        native_rss / 1024,
        np_rss / 1024
    );
    if let Some((build, import)) = cold("python3") {
        println!("| cold first use: python -m venv + pip install ironcalc==0.9.1 | {build:.0} ms (+ import {import:.0} ms) | 0 | 0 |");
    }
}
