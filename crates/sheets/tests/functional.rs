//! The sheets backend's behaviour through its tools and grid view, with a fake byob around
//! it (`common`): writes and recalculation, fills, errors and lints, structural edits,
//! formats, the grid (frozen strips, paging, tabs, edits saved as drafts, stale keys, older
//! versions read-only), CSV/TSV files and artifacts, and wrong input answered in sentences.

mod common;

use common::{Byob, Rng, XLSX, cell, grid_of, tsv_row, views};
use serde_json::{Value, json};

fn row(text: &str, r: u32) -> Vec<String> {
    tsv_row(text, r).unwrap_or_else(|| panic!("no row {r} in:\n{text}"))
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn create_write_recalculates() {
    let mut b = Byob::new();
    let (aid, text) = b.new_sheet("Budget");
    assert!(text.contains("budget.xlsx") && text.contains("Saved: artifact"), "{text}");
    let out = b.tool(
        "write_cells",
        json!({"sheet": aid, "rows": [["Item", "Qty", "Price", "Total"], ["Pens", 3, 1.5, "=B2*C2"], ["Paper", "2", "4.25", "=B3*C3"], ["Sum", null, null, "=SUM(D2:D3)"]]}),
    );
    assert!(out.contains("Wrote 14 cells"), "{out}");
    assert_eq!(row(&out, 2), strs(&["Pens", "3", "1.5", "4.5"]));
    assert_eq!(row(&out, 4)[3], "13");
    assert!(out.contains("No error cells."), "{out}");
    assert_eq!(b.latest(&aid), 2);
    // the next call sees the saved version
    let read = b.tool("read_cells", json!({"sheet": aid, "range": "D1:D4", "what": "formulas"}));
    assert!(read.contains("=SUM(D2:D3)"), "{read}");
    let cells = b.tool("write_cells", json!({"sheet": aid, "cells": {"B2": 10}}));
    assert!(cells.contains("Formulas elsewhere that changed") && cells.contains("D4 23.5 (was 13)"), "{cells}");
}

#[test]
fn sumifs_summary_by_category() {
    let mut b = Byob::new();
    let (aid, _) = b.new_sheet("Spend");
    let mut rnd = Rng::new(7);
    let cats = ["Food", "Rent", "Fun", "Travel"];
    let mut rows = vec![json!(["Category", "Amount"])];
    let mut want = [0.0f64; 4];
    for _ in 0..120 {
        let i = (rnd.next() % 4) as usize;
        let a = rnd.int(100, 50_000) as f64 / 100.0;
        rows.push(json!([cats[i], a]));
        want[i] += a;
    }
    b.tool("write_cells", json!({"sheet": aid, "rows": rows}));
    let mut summary = vec![json!(["Category", "Total"])];
    for (i, c) in cats.iter().enumerate() {
        summary.push(json!([c, format!("=SUMIFS(Data!B:B,Data!A:A,A{})", i + 2)]));
    }
    b.tool("edit_sheet", json!({"sheet": aid, "op": "rename_sheet", "name": "Sheet1", "new_name": "Data"}));
    let out = b.tool("write_cells", json!({"sheet": aid, "start": "Summary!A1", "rows": summary}));
    assert!(out.contains("Added sheet 'Summary'"), "{out}");
    for (i, w) in want.iter().enumerate() {
        let got: f64 = row(&out, i as u32 + 2)[1].parse().unwrap();
        assert!((got - w).abs() < 1e-6, "{} {got} != {w}", cats[i]);
    }
}

#[test]
fn fill_relative_and_absolute() {
    let mut b = Byob::new();
    let (aid, _) = b.new_sheet("Budget");
    let mut rows = vec![json!(["Month", "Units", "Price", "Revenue", "Share"])];
    rows.extend((1..=12).map(|i| json!([format!("M{i}"), i, 2.5, null, null])));
    rows.push(json!(["Total", "=SUM(B2:B13)", null, "=SUM(D2:D13)", null]));
    b.tool("write_cells", json!({"sheet": aid, "rows": rows}));
    let out = b.tool("fill", json!({"sheet": aid, "range": "D2:D13", "formula": "=B2*C2"}));
    assert!(out.contains("D13 got =B13*C13"), "{out}");
    assert_eq!(row(&out, 13)[0], "30");
    let out = b.tool("fill", json!({"sheet": aid, "range": "E2:E13", "formula": "=D2/$D$14"}));
    assert!(out.contains("E13 got =D13/$D$14"), "{out}");
    let f = b.tool("read_cells", json!({"sheet": aid, "range": "E2:E13", "what": "formulas"}));
    assert_eq!(row(&f, 7), strs(&["=D7/$D$14"]));
    let v = b.tool("read_cells", json!({"sheet": aid, "range": "E2:E13"}));
    let total: f64 = (2..14).map(|r| row(&v, r)[0].parse::<f64>().unwrap()).sum();
    assert!((total - 1.0).abs() < 1e-9, "{total}");
    // filling right shifts columns; D2:D picks the last used row
    let out = b.tool("fill", json!({"sheet": aid, "range": "F2:G", "formula": "=$B2+B$1"}));
    assert!(out.contains("G14 got =$B14+C$1"), "{out}");
}

#[test]
fn errors_and_lints() {
    let mut b = Byob::new();
    let (aid, _) = b.new_sheet("Budget");
    let mut rows = vec![json!(["A", "B", "Ratio"])];
    rows.extend((1..=10).map(|i| json!([i, i % 3, null])));
    b.tool("write_cells", json!({"sheet": aid, "rows": rows}));
    let out = b.tool("fill", json!({"sheet": aid, "range": "C2:C11", "formula": "=A2/B2"}));
    assert!(out.contains("#DIV/0!") && out.contains("C4 #DIV/0! (=A4/B4)"), "{out}");
    let out = b.tool("write_cells", json!({"sheet": aid, "cells": {"D2": "=IFERROR(VLOOKUP(99,A2:B11,2,FALSE),\"\")"}}));
    assert!(out.contains("IFERROR(…, \"\")"), "{out}");
    let out = b.tool("write_cells", json!({"sheet": aid, "cells": {"C7": 42}}));
    assert!(out.contains("C7 is a typed number in C2:C11 where the rest are formulas"), "{out}");
    let d = b.tool("describe_sheet", json!({"sheet": aid}));
    assert!(d.contains("Errors ("), "{d}");
}

#[test]
fn edit_ops() {
    let mut b = Byob::new();
    let (aid, _) = b.new_sheet("Budget");
    b.tool("write_cells", json!({"sheet": aid, "rows": [["Name", "Score", "Double"], ["b", 2, "=B2*2"], ["a", 9, "=B3*2"], ["c", 5, "=B4*2"]]}));
    let out = b.tool("edit_sheet", json!({"sheet": aid, "op": "sort", "range": "A1:C4", "by": "B", "descending": true}));
    assert!(out.contains("row 1 kept as the header"), "{out}");
    assert_eq!(row(&out, 2), strs(&["a", "9", "18"]));
    assert_eq!(row(&out, 4), strs(&["b", "2", "4"]));
    let f = b.tool("read_cells", json!({"sheet": aid, "what": "formulas"}));
    assert_eq!(row(&f, 2)[2], "=B2*2");
    b.tool("edit_sheet", json!({"sheet": aid, "op": "freeze", "rows": 1, "cols": 1}));
    let out = b.tool("edit_sheet", json!({"sheet": aid, "op": "insert_rows", "at": 2, "count": 2}));
    assert!(out.contains("Inserted 2 empty rows at row 2"), "{out}");
    let f = b.tool("read_cells", json!({"sheet": aid, "what": "formulas"}));
    assert_eq!(row(&f, 4)[2], "=B4*2");
    b.tool("edit_sheet", json!({"sheet": aid, "op": "delete_rows", "at": "2", "count": 2}));
    b.tool("edit_sheet", json!({"sheet": aid, "op": "insert_cols", "at": "B"}));
    let out = b.tool("read_cells", json!({"sheet": aid, "what": "formulas"}));
    assert_eq!(row(&out, 2), strs(&["a", "", "9", "=C2*2"]));
    b.tool("edit_sheet", json!({"sheet": aid, "op": "delete_cols", "at": "B"}));
    b.tool("edit_sheet", json!({"sheet": aid, "op": "add_sheet", "name": "Notes"}));
    let d = b.tool("describe_sheet", json!({"sheet": aid}));
    assert!(d.contains("frozen 1 rows, 1 columns") && d.contains("Sheet \"Notes\": empty."), "{d}");
    let out = b.tool("edit_sheet", json!({"sheet": aid, "op": "delete_sheet", "name": "Notes"}));
    assert!(out.contains("sheets are now: Sheet1"), "{out}");
    let bad = b.tool("edit_sheet", json!({"sheet": aid, "op": "delete_sheet", "name": "Sheet1"}));
    assert!(bad.contains("only sheet"), "{bad}");
    let bad = b.tool("edit_sheet", json!({"sheet": aid, "op": "explode"}));
    assert!(bad.starts_with("Error: `op` should be one of"), "{bad}");
}

#[test]
fn bad_input_is_a_sentence() {
    let mut b = Byob::new();
    let (aid, _) = b.new_sheet("Budget");
    let t = b.tool("read_cells", json!({"sheet": aid, "range": "hello world"}));
    assert!(t.contains("isn't a cell or range"), "{t}");
    let t = b.tool("read_cells", json!({"sheet": aid, "sheet_name": "Nope"}));
    assert!(t.contains("No sheet named 'Nope'"), "{t}");
    let t = b.tool("describe_sheet", json!({"sheet": "nothing-here"}));
    assert!(t.contains("No spreadsheet 'nothing-here'"), "{t}");
    let t = b.tool("write_cells", json!({"sheet": aid}));
    assert!(t.contains("Give `rows`"), "{t}");
    let t = b.tool("format_cells", json!({"sheet": aid, "range": "A1", "fill": "blurple"}));
    assert!(t.contains("`fill` should be a colour"), "{t}");
    // stringified JSON rows are accepted
    let out = b.tool("write_cells", json!({"sheet": aid, "rows": "[[\"x\", 1]]"}));
    assert!(out.contains("Wrote 2 cells"), "{out}");
}

#[test]
fn format_then_view() {
    let mut b = Byob::new();
    let (aid, _) = b.new_sheet("Budget");
    b.tool("write_cells", json!({"sheet": aid, "rows": [["Amount", "Share"], [1234.5, 0.25], ["=A2*2", 0.5]]}));
    let out = b.tool("format_cells", json!({"sheet": aid, "range": "A2:A3", "number_format": "#,##0.00", "bold": true, "fill": "#ffe599"}));
    assert!(out.contains("A2 1,234.50"), "{out}");
    b.tool("format_cells", json!({"sheet": aid, "range": "B:B", "number_format": "0%", "width": 20}));
    b.tool("format_cells", json!({"sheet": aid, "range": "1:1", "bold": true}));
    let tree = b.view(&aid);
    let g = grid_of(&tree).unwrap();
    assert_eq!(cell(g, 1, 0), json!({"v": "1,234.50", "t": "n", "i": "1234.5", "b": true, "fill": "#ffe599"}));
    assert_eq!(cell(g, 2, 0)["i"], "=A2*2");
    assert_eq!(cell(g, 2, 0)["v"], "2,469.00");
    assert_eq!(cell(g, 1, 1)["v"], "25%");
    assert_eq!(cell(g, 0, 0), json!({"v": "Amount", "b": true}));
    assert_eq!(g["widths"], json!({"1": 150.0}));
    assert_eq!(g["editable"], true);
    assert!(g["rows"].as_i64().unwrap() >= 100 && g["cols"].as_i64().unwrap() >= 26, "{g}");
}

#[test]
fn view_frozen_strips_and_window() {
    let mut b = Byob::new();
    let (aid, _) = b.new_sheet("Budget");
    let mut rows = vec![Value::Array((0..40).map(|c| json!(format!("H{c}"))).collect())];
    rows.extend((1..300).map(|r| Value::Array((0..40).map(|c| json!(r * 100 + c)).collect())));
    b.tool("write_cells", json!({"sheet": aid, "rows": rows}));
    b.tool("edit_sheet", json!({"sheet": aid, "op": "freeze", "rows": 1, "cols": 2}));
    let tree = b.view(&aid);
    let g = grid_of(&tree).unwrap();
    assert_eq!((g["frozen_rows"].as_i64(), g["frozen_cols"].as_i64()), (Some(1), Some(2)));
    assert_eq!(g["blocks"].as_array().unwrap().len(), 1, "the window starts at the corner");
    assert_eq!(g["rows"], 350);
    let msgs = b.event(&aid, "window", json!({"row": 150, "col": 20, "rows": 50, "cols": 15}));
    let vs = views(&msgs);
    let g = grid_of(&vs.last().unwrap()["view"]).unwrap();
    let blocks = g["blocks"].as_array().unwrap();
    assert_eq!(blocks.len(), 4);
    let main = &blocks[0];
    assert_eq!((main["row"].as_i64(), main["col"].as_i64()), (Some(150), Some(20)));
    assert_eq!((main["cells"].as_array().unwrap().len(), main["cells"][0].as_array().unwrap().len()), (50, 15));
    assert_eq!(cell(g, 150, 20)["v"], (150 * 100 + 20).to_string());
    assert_eq!(cell(g, 0, 25), "H25", "the top strip");
    assert_eq!(cell(g, 160, 1)["v"], (160 * 100 + 1).to_string(), "the left strip");
    assert_eq!(cell(g, 0, 0), "H0", "the corner");
    // the window is kept per artifact: a new version opens at the same place
    b.tool("write_cells", json!({"sheet": aid, "cells": {"A2": 1}}));
    let tree = b.view(&aid);
    assert_eq!(grid_of(&tree).unwrap()["blocks"][0]["row"], 150);
}

#[test]
fn grid_edits_save_drafts_and_are_reported() {
    let mut b = Byob::new();
    let (aid, _) = b.new_sheet("Budget");
    b.tool("write_cells", json!({"sheet": aid, "rows": [["x", "y"], [1, "=A2*10"]]}));
    let msgs = b.event(&aid, "edit", json!({"row": 1, "col": 0, "value": "5"}));
    let arts: Vec<&Value> = msgs.iter().filter(|m| m["type"] == "artifact").collect();
    assert_eq!(arts.len(), 1);
    assert_eq!((&arts[0]["draft"], arts[0]["id"].as_str(), &arts[0]["note"]), (&json!(true), Some(aid.as_str()), &json!("Edited in the grid")));
    let v3 = b.latest(&aid);
    b.event(&aid, "edit", json!({"row": 2, "col": 0, "value": "=B2+1"}));
    assert_eq!(b.latest(&aid), v3, "one draft version");
    let tree = b.view(&aid);
    let g = grid_of(&tree).unwrap();
    assert_eq!(cell(g, 1, 1)["v"], "50");
    assert_eq!(cell(g, 2, 0)["v"], "51");
    b.event(&aid, "paste", json!({"row": 4, "col": 0, "text": "a\tb\n1\t2\n"}));
    b.event(&aid, "clear", json!({"row": 4, "col": 0, "to_row": 4, "to_col": 1}));
    let r = b.tool("read_cells", json!({"sheet": aid}));
    assert!(r.contains("The user changed A2, A3, A5:B6, A5:B5 in the grid"), "{r}");
    assert_eq!(row(&r, 6), strs(&["1", "2"]));
    let out = b.tool("write_cells", json!({"sheet": aid, "cells": {"C1": "z"}}));
    assert!(out.contains("The user changed"), "{out}");
    let out = b.tool("write_cells", json!({"sheet": aid, "cells": {"C2": "z"}}));
    assert!(!out.contains("The user changed"), "{out}");
    // an older version is read-only and shows what it had
    let tree = b.view_at(&aid, Some(2), false);
    let g = grid_of(&tree).unwrap();
    assert_eq!(g["editable"], false);
    assert_eq!(cell(g, 1, 0)["v"], "1");
    let msgs = b.event_at(&aid, "edit", Some(2), None, false, json!({"row": 1, "col": 0, "value": "7"}));
    assert!(!msgs.iter().any(|m| m["type"] == "artifact"), "{msgs:?}");
}

#[test]
fn stale_key_keeps_unsaved_grid_edits() {
    let mut b = Byob::new();
    let aid = b.add_artifact("t.csv", b"a,b\n1,2\n");
    b.view(&aid);
    b.event(&aid, "edit", json!({"row": 1, "col": 0, "value": "10"})); // draft: version 2
    assert_eq!(b.latest(&aid), 2);
    // the client still draws its old key (byob said version 1 was the latest then)
    let tree = b.view_at(&aid, Some(1), true);
    assert_eq!(cell(grid_of(&tree).unwrap(), 1, 0)["v"], "10");
    b.event_at(&aid, "edit", Some(1), None, true, json!({"row": 1, "col": 1, "value": "20"}));
    let r = b.tool("read_cells", json!({"sheet": aid}));
    assert_eq!(row(&r, 2), strs(&["10", "20"]));
    assert_eq!(b.meta(&aid)["filename"], "t.xlsx");
}

#[test]
fn tabs() {
    let mut b = Byob::new();
    let (aid, _) = b.new_sheet("Budget");
    b.tool("write_cells", json!({"sheet": aid, "start": "Two!B2", "rows": [["hi"]]}));
    let tree = b.view(&aid);
    let tabs = tree["children"].as_array().unwrap().iter().find(|n| n["key"] == "tabs").unwrap();
    let actions: Vec<&str> = tabs["children"].as_array().unwrap().iter().map(|b| b["action"].as_str().unwrap()).collect();
    assert_eq!(actions, ["tab:Sheet1", "tab:Two"]);
    let msgs = b.event_at(&aid, "tab", None, Some("tab:Two"), false, json!({}));
    let g = grid_of(&views(&msgs).last().unwrap()["view"]).unwrap().clone();
    assert_eq!(cell(&g, 1, 1), "hi");
}

#[test]
fn csv_workspace_file_becomes_a_new_artifact() {
    let mut b = Byob::new();
    b.file("data/sales.csv", b"region,units,zip\nNorth,10,02134\nSouth,20,01001\n");
    let d = b.tool("describe_sheet", json!({"sheet": "data/sales.csv"}));
    assert!(d.contains("C \"zip\": text"), "leading zeros stay text: {d}");
    let out = b.tool("write_cells", json!({"sheet": "data/sales.csv", "cells": {"D1": "double", "D2": "=B2*2"}}));
    assert!(out.contains("Saved as a new spreadsheet artifact (sales.xlsx); data/sales.csv itself is unchanged"), "{out}");
    let aid = b.last_id.clone();
    assert_eq!(b.meta(&aid)["filename"], "sales.xlsx");
    assert_eq!(b.meta(&aid)["content_type"], XLSX);
    assert!(!std::fs::read_to_string(b.ws.path().join("data/sales.csv")).unwrap().contains("double"));
    // the path now leads to that artifact
    let out = b.tool("write_cells", json!({"sheet": "data/sales.csv", "cells": {"D3": "=B3*2"}}));
    assert!(out.contains(&format!("was saved as artifact {aid} earlier")), "{out}");
    assert_eq!(b.last_id, aid);
    let r = b.tool("read_cells", json!({"sheet": aid, "range": "D1:D3"}));
    assert_eq!([row(&r, 1), row(&r, 2), row(&r, 3)], [strs(&["double"]), strs(&["20"]), strs(&["40"])]);
}

#[test]
fn csv_artifact_becomes_xlsx_on_first_write() {
    let mut b = Byob::new();
    let aid = b.add_artifact("prices.csv", b"item,price\nA,1\nB,2\n");
    let tree = b.view(&aid);
    assert_eq!(cell(grid_of(&tree).unwrap(), 1, 1), json!({"v": "1", "t": "n"}));
    let out = b.tool("write_cells", json!({"sheet": aid, "cells": {"B4": "=SUM(B2:B3)"}}));
    assert!(out.contains("Saved as prices.xlsx") && out.contains("B4 =SUM(B2:B3) → 3"), "{out}");
    assert_eq!(b.last_id, aid);
    assert_eq!(b.meta(&aid)["filename"], "prices.xlsx");
}

#[test]
fn create_from_csv() {
    let mut b = Byob::new();
    b.file("t.tsv", b"a\tb\n1\t2\n");
    let out = b.tool("create_sheet", json!({"title": "T", "from": "t.tsv"}));
    assert!(out.contains("Imported t.tsv") && out.contains("used range A1:B2"), "{out}");
}

#[test]
fn describe_500_row_csv_and_paging() {
    let mut b = Byob::new();
    let mut rnd = Rng::new(3);
    let mut lines = vec!["date,category,amount,paid".to_owned()];
    for i in 0..500 {
        lines.push(format!(
            "2024-{:02}-{:02},{},{},{}",
            1 + i % 12,
            1 + i % 28,
            rnd.pick(&["Food", "Rent", "Fun"]),
            rnd.int(1, 900),
            rnd.pick(&["TRUE", "FALSE"])
        ));
    }
    b.file("big.csv", (lines.join("\n") + "\n").as_bytes());
    let d = b.tool("describe_sheet", json!({"sheet": "big.csv"}));
    for want in [
        "used range A1:D501 (501 rows × 4 columns)",
        "Header in row 1; data in rows 2–501.",
        "A \"date\": date, 500 values, 2024-01-01 … 2024-12-",
        "D \"paid\": bool",
    ] {
        assert!(d.contains(want), "{want} in:\n{d}");
    }
    let b_line = d.lines().find(|l| l.contains("B \"category\"")).unwrap();
    assert!(b_line.contains("B \"category\": text, 500 values, 3 distinct: "), "{b_line}");
    let c_line = d.lines().find(|l| l.contains("C \"amount\"")).unwrap();
    assert!(c_line.contains("C \"amount\": number, 500 values, min ") && c_line.contains(", max ") && c_line.contains(", sum "), "{c_line}");
    let r = b.tool("read_cells", json!({"sheet": "big.csv"}));
    assert!(r.contains("rows 1–200 of 1–501") && r.contains("More rows: read_cells(range=\"A201:D501\")"), "{r}");
    let r = b.tool("read_cells", json!({"sheet": "big.csv", "range": "A201:D501"}));
    assert!(r.contains("More rows: read_cells(range=\"A401:D501\")"), "{r}");
    let wide = b.tool("read_cells", json!({"sheet": "big.csv", "range": "a1:b2"}));
    assert_eq!(wide.lines().nth(1), Some("\tA\tB"));
}
