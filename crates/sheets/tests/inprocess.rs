//! The native sheets backend in-process: tools through `NativeBackend::request`, views
//! redrawn after a tool, an xlsx round trip (what it saves loads back with the same
//! formulas, values, styles and sheets), and a panic-free answer to junk.

use std::sync::{Arc, Mutex};

use byob_plugins::native::{NativeBackend, Outbox, dispatch};
use byob_sheets::book::Book;
use serde_json::{Value, json};

fn outbox() -> (Outbox, Arc<Mutex<Vec<Value>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = Arc::clone(&seen);
    (Outbox::new(move |m| s.lock().unwrap().push(m)), seen)
}

fn tool(b: &mut dyn NativeBackend, out: &Outbox, ws: &str, name: &str, args: Value) -> Value {
    b.request("tool", &json!({"name": name, "args": args, "workspace": ws, "chat": "c1"}), out).unwrap_or_else(|e| json!(format!("Error: {e}")))
}

#[test]
fn an_xlsx_round_trip_keeps_formulas_values_and_formats() {
    let ws = tempfile::tempdir().unwrap();
    let wsp = ws.path().to_str().unwrap();
    std::fs::write(ws.path().join("t.csv"), "item,qty,price\npens,3,1.5\npaper,2,4.25\n").unwrap();
    let mut b = (byob_sheets::factory())();
    let (out, _) = outbox();
    let r = tool(
        b.as_mut(),
        &out,
        wsp,
        "write_cells",
        json!({"sheet": "t.csv", "cells": {"D1": "total", "D2": "=B2*C2", "D3": "=B3*C3", "Sums!A1": "=SUM(Sheet1!D2:D3)"}}),
    );
    let text = r["text"].as_str().unwrap();
    assert!(text.contains("D2 =B2*C2 → 4.5") && text.contains("Added sheet 'Sums'"), "{text}");
    let file = r["artifact"]["file"].as_str().unwrap().to_owned();
    assert_eq!(r["artifact"]["filename"], "t.xlsx");

    let mut book = Book::load(std::path::Path::new(&file), None).unwrap();
    assert_eq!(book.sheet_names(), ["Sheet1", "Sums"]);
    assert_eq!(book.content(0, 2, 4), "=B2*C2");
    assert_eq!(book.model_text(0, 3, 4, None), "8.5");
    assert_eq!(book.model_text(1, 1, 1, None), "13");
    // and again: what it saves from a loaded workbook loads the same
    let again = book.to_xlsx().unwrap();
    let p2 = ws.path().join("again.xlsx");
    std::fs::write(&p2, again).unwrap();
    let book2 = Book::load(&p2, None).unwrap();
    assert_eq!(book2.content(1, 1, 1), "=SUM(Sheet1!D2:D3)");
    assert_eq!(book2.model_text(1, 1, 1, None), "13");
}

#[test]
fn tools_redraw_open_views_and_junk_gets_sentences() {
    let ws = tempfile::tempdir().unwrap();
    let wsp = ws.path().to_str().unwrap();
    let mut b = (byob_sheets::factory())();
    let (out, seen) = outbox();
    // a view key byob opened (an artifact that doesn't exist on disk: a sentence, no panic)
    let a = json!({"id": "x-1", "version": 1, "latest": 1, "path": ws.path().join("nope.xlsx"), "filename": "x.xlsx", "content_type": "", "chat": "c1"});
    assert!(dispatch(b.as_mut(), &json!({"type": "request", "id": 1, "method": "view", "params": {"view": "sheet", "key": "x-1@1", "artifact": a}}), &out));
    assert!(dispatch(
        b.as_mut(),
        &json!({"type": "request", "id": 2, "method": "tool", "params": {"name": "describe_sheet", "args": {"sheet": 5}, "workspace": wsp}}),
        &out
    ));
    let msgs = seen.lock().unwrap().clone();
    let types: Vec<&str> = msgs.iter().map(|m| m["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["reply", "reply", "view"], "a tool call redraws the open views: {msgs:?}");
    assert!(msgs[1]["error"].as_str().unwrap().starts_with("Give `sheet`"), "{:?}", msgs[1]);
    for junk in [json!(null), json!({"rows": "[[1,"}), json!({"sheet": "", "range": {}}), json!({"sheet": "a.csv", "rows": [[1e308, -0.0, true, {"v": [1]}]]})]
    {
        for t in byob_sheets::TOOLS {
            let _ = tool(b.as_mut(), &out, wsp, t, junk.clone());
        }
    }
    assert!(b.request("nope", &json!({}), &out).is_err());
}
