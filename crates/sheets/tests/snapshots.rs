//! Whole replies, view trees and messages, character for character, against snapshots in
//! `tests/snapshots/`: every tool, a battery of formulas (lookups, conditional sums, dates,
//! text, dynamic arrays and spills, every error kind), sorts, freezes, formats, structural
//! edits, the grid (paging, tabs, selection, edits, pastes, clears, an older version), CSV
//! and TSV files (delimiter sniffing, column profiles, paging), and wrong input.
//!
//! After an intended change of output, regenerate them and review the diff:
//!
//!     BYOB_BLESS=1 cargo test -p byob-sheets --test snapshots

mod common;

use std::fmt::Write as _;
use std::path::PathBuf;

use common::{Byob, Rng, tsv_row};
use serde_json::{Value, json};

/// (name, formula)
const BATTERY: [(&str, &str); 38] = [
    ("SUM", "=SUM(Data!C2:C11)"),
    ("AVERAGE", "=AVERAGE(Data!C2:C11)"),
    ("IF", "=IF(SUM(Data!C2:C11)>100,\"big\",\"small\")"),
    ("VLOOKUP", "=VLOOKUP(\"Banana\",Data!B2:D11,2,FALSE)"),
    ("XLOOKUP", "=XLOOKUP(\"West\",Data!A2:A11,Data!C2:C11)"),
    ("INDEX/MATCH", "=INDEX(Data!C2:C11,MATCH(\"East\",Data!A2:A11,0))"),
    ("SUMIFS", "=SUMIFS(Data!C2:C11,Data!A2:A11,\"North\",Data!B2:B11,\"Cherry\")"),
    ("COUNTIFS", "=COUNTIFS(Data!A2:A11,\"South\",Data!C2:C11,\">10\")"),
    ("MAXIFS", "=MAXIFS(Data!C2:C11,Data!A2:A11,\"South\")"),
    ("TEXT num", "=TEXT(1234.5,\"#,##0.00\")"),
    ("TEXT date", "=TEXT(DATE(2026,3,1),\"yyyy-mm-dd\")"),
    ("DATE diff", "=DATE(2026,3,1)-DATE(2026,1,5)"),
    ("EOMONTH", "=EOMONTH(DATE(2026,2,10),0)"),
    ("NETWORKDAYS", "=NETWORKDAYS(DATE(2026,1,1),DATE(2026,1,31))"),
    ("SUMPRODUCT", "=SUMPRODUCT(Data!C2:C11,Data!D2:D11)"),
    ("PMT/ROUND", "=ROUND(PMT(0.05/12,360,200000),2)"),
    ("TEXTJOIN", "=TEXTJOIN(\",\",TRUE,\"a\",\"\",\"b\")"),
    ("IFERROR", "=IFERROR(1/0,\"err\")"),
    ("LEN/LEFT", "=LEN(LEFT(\"spreadsheet\",5))"),
    ("CONCAT", "=CONCAT(Data!A2,\"-\",Data!B2)"),
    ("ISERROR", "=ISERROR(VLOOKUP(\"Kiwi\",Data!B2:D11,2,FALSE))"),
    ("INDIRECT", "=INDIRECT(\"Data!C3\")"),
    ("OFFSET", "=SUM(OFFSET(Data!C2,0,0,3,1))"),
    ("array math", "=SUM(Data!C2:C11*Data!D2:D11)"),
    ("LET", "=LET(x,2,x*3)"),
    ("SEQUENCE", "=SUM(SEQUENCE(10))"),
    ("UNIQUE", "=ROWS(UNIQUE(Data!A2:A11))"),
    ("FILTER", "=SUM(FILTER(Data!C2:C11,Data!A2:A11=\"North\"))"),
    ("SORT", "=INDEX(SORT(UNIQUE(Data!A2:A11)),2)"),
    ("div0", "=1/0"),
    ("na", "=VLOOKUP(\"Kiwi\",Data!B2:D11,2,FALSE)"),
    ("ref", "=INDEX(Data!C2:C11,99)"),
    ("name", "=NOPE(1)"),
    ("bool", "=Data!C2>5"),
    ("pct", "=Data!C2/Data!C3"),
    ("big", "=10^20"),
    ("small", "=1/3/10^6"),
    ("spill", "=SORT(UNIQUE(Data!A2:A11))"),
];

fn data() -> Value {
    json!([
        ["Region", "Fruit", "Units", "Price", "Day"],
        ["North", "Apple", 10, 1.5, "2026-01-05"],
        ["South", "Banana", 20, 0.5, "2026-01-10"],
        ["North", "Banana", 5, 0.5, "2026-02-01"],
        ["East", "Apple", 8, 1.5, "2026-02-15"],
        ["South", "Cherry", 12, 3, "2026-03-01"],
        ["North", "Cherry", 7, 3, "2026-03-12"],
        ["East", "Banana", 15, 0.5, "2026-03-20"],
        ["West", "Apple", 9, 1.5, "2026-04-02"],
        ["West", "Cherry", 4, 3, "2026-04-18"],
        ["South", "Apple", 11, 1.5, "2026-05-01"]
    ])
}

/// A fake byob that writes down every call's result and the messages it caused.
struct Rec {
    b: Byob,
    log: String,
    n: usize,
    /// The last tree written down per view key (a redraw that changes nothing is one line).
    drawn: std::collections::HashMap<String, String>,
}

impl Rec {
    fn new() -> Self {
        Self { b: Byob::new(), log: String::new(), n: 0, drawn: Default::default() }
    }

    /// Temp files and the workspace folder differ between runs.
    fn norm(&self, v: &Value) -> Value {
        let ws = self.b.ws();
        match v {
            Value::String(s) => {
                let s = s.replace(&ws, "<ws>");
                // An absolute temp path, `/tmp/…` or `C:\…\Temp\…`.
                if s.contains("byob-sheets-") && (s.starts_with('/') || s.get(1..3) == Some(":\\")) {
                    let base = s.rsplit(['/', '\\']).next().unwrap_or_default();
                    return json!(format!("<tmp>/{}", base.split_once('-').map_or(base, |(_, b)| b)));
                }
                json!(s)
            }
            Value::Array(a) => Value::Array(a.iter().map(|x| self.norm(x)).collect()),
            Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (k.clone(), self.norm(x))).collect()),
            other => other.clone(),
        }
    }

    fn record<T>(&mut self, label: &str, f: impl FnOnce(&mut Byob) -> T) -> T
    where
        T: Into<Value> + Clone,
    {
        let start = self.b.msgs.len();
        let r = f(&mut self.b);
        self.n += 1;
        let shown = match self.norm(&r.clone().into()) {
            Value::String(s) => s,
            other => other.to_string(),
        };
        let _ = writeln!(self.log, "### {} {label}\n{shown}", self.n);
        let msgs: Vec<Value> = self.b.msgs[start..].iter().map(|m| self.norm(m)).collect();
        for m in msgs {
            let line = m.to_string();
            if m["type"] == "view" {
                let key = m["key"].as_str().unwrap_or_default().to_owned();
                if self.drawn.get(&key) == Some(&line) {
                    let _ = writeln!(self.log, "> view {key}: unchanged");
                    continue;
                }
                self.drawn.insert(key, line.clone());
            }
            let _ = writeln!(self.log, "> {line}");
        }
        r
    }

    fn tool(&mut self, name: &str, args: Value) -> String {
        let label = format!("{name} {args}");
        self.record(&label, |b| b.tool(name, args))
    }

    fn check(&self, name: &str) {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots").join(format!("{name}.txt"));
        if std::env::var_os("BYOB_BLESS").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &self.log).unwrap();
            return;
        }
        let want = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e} (BYOB_BLESS=1 writes it)", path.display()));
        if want != self.log {
            let (w, g): (Vec<&str>, Vec<&str>) = (want.lines().collect(), self.log.lines().collect());
            let i = w.iter().zip(&g).position(|(a, b)| a != b).unwrap_or(w.len().min(g.len()));
            let call = g[..i.min(g.len())].iter().rev().find(|l| l.starts_with("### ")).copied().unwrap_or_default();
            panic!(
                "{} differs from line {} (in {call}):\nwant: {}\n got: {}\n(BYOB_BLESS=1 rewrites it after an intended change)",
                path.display(),
                i + 1,
                w.get(i).unwrap_or(&"<end>"),
                g.get(i).unwrap_or(&"<end>")
            );
        }
    }
}

#[test]
fn tools_formulas_and_views() {
    let mut r = Rec::new();
    r.tool("create_sheet", json!({"title": "Fruit sales"}));
    let aid = r.b.last_id.clone();
    r.tool("write_cells", json!({"sheet": aid, "rows": data()}));
    r.tool("edit_sheet", json!({"sheet": aid, "op": "rename_sheet", "name": "Sheet1", "new_name": "Data"}));
    r.tool("format_cells", json!({"sheet": aid, "range": "E2:E11", "number_format": "yyyy-mm-dd"}));
    let mut rows = vec![json!(["Name", "Formula", "Value"])];
    rows.extend(BATTERY.iter().map(|(n, f)| json!([n, format!("'{f}"), f])));
    r.tool("write_cells", json!({"sheet": aid, "start": "Calc!A1", "rows": rows}));
    let values = r.tool("read_cells", json!({"sheet": aid, "sheet_name": "Calc", "range": "A1:C60"}));
    r.tool("read_cells", json!({"sheet": aid, "sheet_name": "Calc", "what": "formulas"}));
    r.tool("describe_sheet", json!({"sheet": aid}));
    r.tool("fill", json!({"sheet": aid, "range": "F2:F11", "formula": "=C2*D2", "sheet_name": "Data"}));
    r.tool("fill", json!({"sheet": aid, "range": "G2:G", "formula": "=F2/SUM($F$2:$F$11)", "sheet_name": "Data"}));
    r.tool("write_cells", json!({"sheet": aid, "cells": {"Data!C3": 25, "Data!H2": "=IFERROR(1/0,\"\")"}}));
    r.tool("fill", json!({"sheet": aid, "range": "I2:I11", "formula": "=C2+1", "sheet_name": "Data"}));
    r.tool("write_cells", json!({"sheet": aid, "cells": {"Data!I6": 3}}));
    r.tool("format_cells", json!({"sheet": aid, "range": "Data!F:G", "number_format": "0.0%", "bold": true, "fill": "#ffe599", "align": "right", "width": 14}));
    r.tool("format_cells", json!({"sheet": aid, "range": "Data!1:1", "bold": true, "fill": "none"}));
    r.tool("edit_sheet", json!({"sheet": aid, "op": "sort", "range": "Data!A1:I11", "by": "Units", "descending": true}));
    r.tool("edit_sheet", json!({"sheet": aid, "op": "freeze", "rows": 1, "cols": 1, "sheet_name": "Data"}));
    r.tool("edit_sheet", json!({"sheet": aid, "op": "insert_rows", "at": 3, "count": 2, "sheet_name": "Data"}));
    r.tool("edit_sheet", json!({"sheet": aid, "op": "delete_cols", "at": "H", "sheet_name": "Data"}));
    r.tool("edit_sheet", json!({"sheet": aid, "op": "add_sheet", "name": "Notes"}));
    r.tool("describe_sheet", json!({"sheet": aid}));
    // views: open, page, select, tab, edit, paste, clear; an older version read-only
    r.record("view", |b| b.view(&aid));
    r.record("window", |b| b.event(&aid, "window", json!({"row": 0, "col": 0, "rows": 60, "cols": 12})));
    r.record("tab", |b| b.event_at(&aid, "tab", None, Some("tab:Data"), false, json!({})));
    r.record("select", |b| b.event(&aid, "select", json!({"row": 2, "col": 1, "to_row": 4, "to_col": 3})));
    r.record("edit", |b| b.event(&aid, "edit", json!({"row": 1, "col": 2, "value": "=1+1"})));
    r.record("paste", |b| b.event(&aid, "paste", json!({"row": 20, "col": 0, "text": "a\tb\n1\t=A22*2\n"})));
    r.record("clear", |b| b.event(&aid, "clear", json!({"row": 20, "col": 0, "to_row": 20, "to_col": 1})));
    r.record("view v2", |b| b.view_at(&aid, Some(2), false));
    r.tool("read_cells", json!({"sheet": aid, "sheet_name": "Data"}));
    // mistakes are sentences
    for args in [
        json!({"sheet": aid, "range": "hello world"}),
        json!({"sheet": aid, "sheet_name": "Nope"}),
        json!({"sheet": "nothing-here"}),
        json!({"sheet": aid, "range": "A0"}),
        json!({"sheet": aid, "range": "'x!A1"}),
    ] {
        r.tool("read_cells", args);
    }
    r.tool("write_cells", json!({"sheet": aid}));
    r.tool("write_cells", json!({"sheet": aid, "rows": "[[\"x\", 1]]"}));
    r.tool("write_cells", json!({"sheet": aid, "rows": "a\tb\nc\td"}));
    r.tool("format_cells", json!({"sheet": aid, "range": "A1", "fill": "blurple"}));
    r.tool("format_cells", json!({"sheet": aid, "range": "A1", "width": 999}));
    r.tool("edit_sheet", json!({"sheet": aid, "op": "explode"}));
    r.tool("edit_sheet", json!({"sheet": aid, "op": "delete_rows", "at": "x"}));
    r.tool("edit_sheet", json!({"sheet": aid, "op": "sort", "by": "Q", "range": "Data!A1:C4"}));
    r.tool("fill", json!({"sheet": aid, "range": "Z1:Z2"}));
    r.check("tools_formulas_and_views");

    // the battery's values, spelled out (Calc rows 2..; column C)
    let got: Vec<String> = (0..BATTERY.len()).map(|i| tsv_row(&values, i as u32 + 2).map(|r| r[2].clone()).unwrap_or_default()).collect();
    let want = BATTERY_VALUES;
    for ((name, _), (g, w)) in BATTERY.iter().zip(got.iter().zip(want)) {
        assert_eq!(g, w, "{name}");
    }
}

/// What each [`BATTERY`] formula evaluates to (as `read_cells` shows it). IronCalc 0.9.1's
/// own quirks included: a date difference keeps the date format (`2/23/1900` for 55 days),
/// and `TEXTJOIN(…, TRUE, …)` keeps an empty string literal (`a,,b`).
const BATTERY_VALUES: [&str; 38] = [
    "101",
    "10.1",
    "big",
    "20",
    "9",
    "8",
    "7",
    "3",
    "20",
    "1,234.50",
    "2026-03-01",
    "2/23/1900",
    "46081",
    "22",
    "146",
    "-1073.64",
    "a,,b",
    "err",
    "5",
    "North-Apple",
    "TRUE",
    "20",
    "35",
    "146",
    "6",
    "55",
    "4",
    "22",
    "North",
    "#DIV/0!",
    "#N/A",
    "#REF!",
    "#NAME?",
    "TRUE",
    "0.5",
    "1e+20",
    "3.333333333e-07",
    "East",
];

#[test]
fn csv_files_and_profiles() {
    let mut r = Rec::new();
    let mut rnd = Rng::new(5);
    let mut lines = vec!["date,category,amount,paid,zip,note".to_owned()];
    for i in 0..400 {
        lines.push(format!(
            "2024-{:02}-{:02},{},{}.{:02},{},0{},\"x, {i}\"",
            1 + i % 12,
            1 + i % 28,
            rnd.pick(&["Food", "Rent", "Fun", "Travel"]),
            rnd.int(1, 900),
            rnd.int(0, 99),
            rnd.pick(&["TRUE", "FALSE"]),
            rnd.int(1000, 9999)
        ));
    }
    r.b.file("data/big.csv", (lines.join("\n") + "\n").as_bytes());
    r.b.file("semi.csv", b"a;b;c\n1;2;3\n4;5;6\n");
    r.b.file("t.tsv", b"k\tv\nx\t1\ny\t\n");
    for name in ["data/big.csv", "semi.csv", "t.tsv"] {
        r.tool("describe_sheet", json!({"sheet": name}));
        r.tool("read_cells", json!({"sheet": name}));
    }
    r.tool("read_cells", json!({"sheet": "data/big.csv", "range": "A201:F"}));
    r.tool("write_cells", json!({"sheet": "data/big.csv", "cells": {"G1": "double", "G2": "=C2*2"}}));
    let aid = r.b.last_id.clone();
    r.tool("fill", json!({"sheet": "data/big.csv", "range": "G2:G", "formula": "=C2*2"}));
    r.tool("edit_sheet", json!({"sheet": aid, "op": "sort", "by": "C"}));
    r.tool("describe_sheet", json!({"sheet": aid}));
    r.tool("create_sheet", json!({"title": "T", "from": "t.tsv"}));
    r.record("view", |b| {
        let id = b.last_id.clone();
        b.view(&id)
    });
    r.check("csv_files_and_profiles");
}
