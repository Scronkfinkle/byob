//! Spreadsheets: a CSV opened as a sheet, edited by id and by its file path, charted,
//! named from another chat, and still there after a restart.

use serde_json::json;

use super::{Answer, StepCheck, World, artifact_id};

const HARVEST: &str = "crop,kg,hours\nbeans,40,8\ntomatoes,90,12\nsquash,55,5\n";

/// Regression (a17e2e1): a request opened harvest.csv as a sheet, edited it by the id byob
/// gave, then by the file's path. byob joins the notes of one request's saves, so the
/// sheet's note no longer read "From harvest.csv" alone, and the path imported the CSV
/// again as a new artifact without the edits. The sheets' own tests passed; only the real
/// engine joins notes. Then a second chat names the first chat's sheet: it is told it's
/// another chat's, and naming the file gets it its own copy, the first one untouched.
#[test]
fn a_csv_sheet_is_one_artifact_whether_named_by_id_or_path() {
    let mut w = World::new();
    w.file("harvest.csv", HARVEST);

    // One request: open it, fill a column by id, then label and format it by the path.
    w.model.answers(|sent| {
        assert!(sent.tools().iter().any(|t| t == "create_sheet"), "the sheet tools are offered: {:?}", sent.tools());
        Answer::call("create_sheet", json!({"title": "Harvest", "from": "harvest.csv"}))
    });
    w.model.answers(|sent| {
        let id = artifact_id(&sent.last_result(), "harvest").expect("create_sheet names the new artifact");
        Answer::call("fill", json!({"sheet": id, "range": "D2:D4", "formula": "=B2/C2"}))
    });
    w.model.calls("write_cells", json!({"sheet": "harvest.csv", "cells": {"D1": "kg_per_hour"}}));
    w.model.calls("format_cells", json!({"sheet": "harvest.csv", "range": "D2:D4", "number_format": "0.0"}));
    w.model.says("Added kg_per_hour.");
    let first = w.send("Open harvest.csv as a spreadsheet and add a kg per hour column");
    assert_eq!(first.error, None);
    first.step("create_sheet").done();
    let sheet = artifact_id(first.result("create_sheet"), "harvest").unwrap();
    for tool in ["fill", "write_cells", "format_cells"] {
        first.step(tool).done();
        let said = first.result(tool);
        assert!(said.contains(&format!("Saved: artifact {sheet}, version 1")), "{tool} saved another artifact: {said}");
    }
    assert!(first.result("write_cells").contains(&format!("harvest.csv is open in this chat as artifact {sheet}")));
    let listed: Vec<_> = w.seen.artifacts.iter().map(|a| (a.id.as_str(), a.versions.len())).collect();
    assert_eq!(listed, [(sheet.as_str(), 1)], "one sheet, one version for the request");
    assert_eq!(w.read("harvest.csv").as_deref(), Some(HARVEST), "the file itself is left alone");

    // The next request names the path again: still that sheet, with every edit in it.
    w.model.calls("read_cells", json!({"sheet": "harvest.csv"}));
    w.model.calls("write_cells", json!({"sheet": "harvest.csv", "cells": {"A6": "total", "B6": "=SUM(B2:B4)"}}));
    w.model.says("Totaled.");
    let second = w.send("What's in it now? Add a total.");
    second.step("read_cells").done();
    let read = second.result("read_cells");
    assert!(read.contains("kg_per_hour") && read.contains("7.5"), "the earlier edits are in it: {read}");
    second.step("write_cells").done();
    assert!(second.result("write_cells").contains(&format!("Saved: artifact {sheet}, version 2")), "{}", second.result("write_cells"));
    assert_eq!(w.seen.artifacts.len(), 1, "{:?}", w.seen.artifacts.iter().map(|a| &a.id).collect::<Vec<_>>());

    // A second chat names the first chat's sheet: that's another chat's, and the file is
    // suggested; naming the file makes this chat its own copy.
    let first_chat = w.chat_id();
    w.new_chat();
    let theirs = sheet.clone();
    w.model.calls("read_cells", json!({"sheet": theirs}));
    w.model.answers(|sent| {
        assert!(sent.last_result().contains("harvest.csv"), "the refusal suggests the file: {}", sent.last_result());
        Answer::call("write_cells", json!({"sheet": "harvest.csv", "cells": {"E1": "notes"}}))
    });
    w.model.says("Made a copy here.");
    let other = w.send(&format!("Add a notes column to {sheet}"));
    other.step("read_cells").failed();
    // A failed call keeps its manifest title (it said "Run read_cells").
    assert_eq!(other.step("read_cells").title, "Read cells");
    let refused = other.result("read_cells");
    assert!(refused.contains("another chat"), "{refused}");
    other.step("write_cells").done();
    let copy = other.result("write_cells");
    assert!(copy.contains("imported as a new spreadsheet artifact"), "{copy}");
    let mine = artifact_id(copy, "harvest").unwrap();
    assert_ne!(mine, sheet);
    assert_eq!(w.seen.artifacts.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), [mine.as_str()]);

    // The first chat's sheet is as it was, and still only its own.
    w.open_chat(&first_chat);
    let listed: Vec<_> = w.seen.artifacts.iter().map(|a| (a.id.as_str(), a.versions.len())).collect();
    assert_eq!(listed, [(sheet.as_str(), 2)]);
    assert_eq!(w.model.unused(), 0);
}

/// A chart drawn from a CSV belongs to its chat after byob restarts: the chat opens again
/// with its chart listed, and the chart's file is still there.
#[test]
fn a_chart_from_a_csv_is_still_the_chats_after_a_restart() {
    let mut w = World::new();
    w.file("harvest.csv", HARVEST);
    w.model.calls("make_chart", json!({"type": "bar", "title": "Harvest by crop", "file": "harvest.csv", "x": "crop", "series": ["kg"], "unit": "kg"}));
    w.model.says("Tomatoes lead.");
    let t = w.send("Chart the harvest by crop");
    assert!(t.step("make_chart").done().contains("Harvest by crop"));
    let chat = w.chat_id();
    let charts: Vec<_> = w.seen.artifacts.iter().map(|a| a.id.clone()).collect();
    assert_eq!(charts.len(), 1, "the chart is an artifact of the chat: {charts:?}");

    w.restart();
    assert_eq!(w.seen.chat.as_ref().map(|c| c.id.as_str()), Some(chat.as_str()), "the chat opens again");
    assert_eq!(w.seen.artifacts.iter().map(|a| a.id.clone()).collect::<Vec<_>>(), charts);
    let chart = String::from_utf8(w.artifact_bytes(&charts[0])).unwrap();
    assert!(chart.contains("tomatoes") && chart.contains("90"), "{chart}");
    // The model is told about it in the next request (tool steps aren't sent back; the
    // chat's artifact list is).
    let charts_id = charts[0].clone();
    w.model.answers(move |sent| {
        let system = sent.system();
        assert!(system.contains("Artifacts in this chat") && system.contains(&format!("{}: \"Harvest by crop\" (chart", charts_id)), "{system}");
        Answer::text("It's the bar chart above.")
    });
    assert_eq!(w.send("Which chart was that?").text(), "It's the bar chart above.");
}

/// Undo, newest first. Undoing the request that edited the sheet (and wrote a file) takes
/// the sheet back to its first version; the next edit by path builds on that one. Undoing
/// the requests that made it takes the sheet away. (Undo journals are kept in memory, so
/// this is within one run.)
///
/// Regressions: undo put the files back, but
/// - the chat kept listing the artifacts as they were, to its clients and in the model's
///   prompt ("harvest-…: v2"), so the model was told of versions, and a whole sheet, that
///   no longer existed;
/// - the sheets backend kept the undone workbook in memory, so the next edit read and saved
///   the undone cells again.
#[test]
fn undone_sheet_requests_leave_the_chat_listing_what_is_left() {
    let mut w = World::new();
    w.file("harvest.csv", HARVEST);
    w.model.calls("create_sheet", json!({"title": "Harvest", "from": "harvest.csv"}));
    w.model.calls("write_cells", json!({"sheet": "harvest.csv", "cells": {"D1": "kg_per_hour"}}));
    w.model.says("Opened.");
    let opened = w.send("Open harvest.csv as a spreadsheet");
    w.model.calls("write_cells", json!({"sheet": "harvest.csv", "cells": {"E1": "notes"}}));
    w.model.calls("write_file", json!({"path": "notes.md", "content": "beans did well\n"}));
    w.model.says("Noted.");
    let noted = w.send("Add a notes column and write my notes down");
    let sheet = w.seen.artifacts[0].id.clone();
    let versions = |w: &World| w.seen.artifacts.iter().map(|a| (a.id.clone(), a.versions.len())).collect::<Vec<_>>();
    assert_eq!(versions(&w), [(sheet.clone(), 2)]);
    assert!(opened.reply.undoable && noted.reply.undoable);

    // Undo the second request: the file goes, the sheet is back at version 1.
    w.undo(noted.reply.id).expect("the later request undoes");
    assert!(w.read("notes.md").is_none());
    assert_eq!(versions(&w), [(sheet.clone(), 1)], "the client is told");
    let v1 = format!("- {sheet}: \"Harvest\" (");
    w.model.answers(move |sent| {
        let listed = sent.system().lines().find(|l| l.starts_with(&v1)).unwrap_or_default().to_owned();
        assert!(listed.contains(", v1,"), "the model is told version 1: {listed:?}");
        Answer::call("read_cells", json!({"sheet": "harvest.csv"}))
    });
    w.model.calls("write_cells", json!({"sheet": "harvest.csv", "cells": {"E1": "remarks"}}));
    w.model.says("Relabeled.");
    let again = w.send("Call the new column remarks");
    let read = again.result("read_cells");
    assert!(read.contains("kg_per_hour") && !read.contains("notes"), "the undone edit is gone, the first one kept: {read}");
    // (Version numbers aren't reused: the undone version 2 stays unused.)
    assert!(again.result("write_cells").contains(&format!("Saved: artifact {sheet}, version 3")), "{}", again.result("write_cells"));

    // Undo that and the first request: no sheet left, and nobody is told of one.
    w.undo(again.reply.id).expect("undo the relabel");
    w.undo(opened.reply.id).expect("then the first request");
    assert_eq!(w.read("harvest.csv").as_deref(), Some(HARVEST));
    assert!(w.seen.artifacts.is_empty(), "{:?}", versions(&w));
    let gone = sheet.clone();
    w.model.answers(move |sent| {
        assert!(!sent.system().contains(&gone), "the model isn't told of an undone sheet");
        Answer::text("Nothing is open.")
    });
    w.send("What's open?");
    w.reconnect();
    assert!(w.seen.artifacts.is_empty(), "{:?}", versions(&w));
    assert_eq!(w.model.unused(), 0);
}

/// Under "Ask before changes", looking at a spreadsheet and working on the sheet artifact
/// run without asking, like reading a file and `update_artifact` do. The sheet tools didn't
/// declare their access, so each counted as running code: describing a CSV showed an
/// approval card labeled "runs code" (here the turn would wait on it and time out).
#[test]
fn looking_at_and_editing_a_sheet_does_not_ask_before_changes() {
    let mut w = World::with(|c| c.policy = Some(crate::Policy::for_approvals(byob_types::config::Approvals::Ask)));
    w.file("harvest.csv", HARVEST);
    w.model.calls("describe_sheet", json!({"sheet": "harvest.csv"}));
    w.model.calls("read_cells", json!({"sheet": "harvest.csv", "range": "A1:C2"}));
    w.model.calls("write_cells", json!({"sheet": "harvest.csv", "cells": {"D1": "notes"}}));
    w.model.calls("chart_sheet", json!({"sheet": "harvest.csv", "type": "bar", "x": "A", "series": ["B"]}));
    w.model.says("Done.");
    let t = w.send("Look at harvest.csv, add a notes column and chart it");
    assert_eq!(t.error, None);
    for tool in ["describe_sheet", "read_cells", "write_cells", "chart_sheet"] {
        t.step(tool).done();
        assert_eq!(t.step(tool).access, byob_protocol::ToolAccess::Read, "{tool}");
    }
    assert_eq!(w.read("harvest.csv").as_deref(), Some(HARVEST), "the file itself is left alone");
}

/// A CSV kept in the Library as a live file, then written to by the model by its id.
/// Regression: write_cells saved an .xlsx version over the live file (renaming it
/// budget.xlsx); opening the Library snapshot the unchanged CSV as the next version, under
/// the new name, so the artifact's latest version was a CSV called v3.xlsx and every sheet
/// tool failed on it ("couldn't be opened as an .xlsx workbook"). The person's file was
/// never changed either. Now the live file's id means the file: a write imports a copy,
/// and update_artifact says to edit the file.
#[test]
fn a_live_csv_written_to_by_id_stays_the_persons_file() {
    use byob_protocol::{ClientMsg, ServerMsg};
    let mut w = World::new();
    w.file("harvest.csv", HARVEST);
    w.model.says("Hi.");
    w.send("hello");
    w.tell(ClientMsg::AddLiveFile { path: "harvest.csv".into() });
    w.wait(|m| matches!(m, ServerMsg::Notice { .. }).then_some(()));
    let live = w.seen.artifacts[0].id.clone();
    let (a, b) = (live.clone(), live.clone());
    w.model.answers(move |_| Answer::call("write_cells", json!({"sheet": a, "cells": {"D1": "notes"}})));
    w.model.answers(move |_| Answer::call("update_artifact", json!({"id": b, "content": "crop\nbeans\n"})));
    w.model.says("Done.");
    let t = w.send("Add a notes column to it");
    let wrote = t.step("write_cells").done();
    let copy = artifact_id(t.result("write_cells"), "harvest").filter(|id| *id != live).unwrap_or_else(|| panic!("a copy: {wrote}"));
    assert!(t.step("update_artifact").failed().contains("harvest.csv"), "{}", t.result("update_artifact"));

    // Opening the Library snapshots the file only if it changed: it didn't.
    w.tell(ClientMsg::ListLibrary);
    w.wait(|m| matches!(m, ServerMsg::Library { .. }).then_some(()));
    let meta = byob_tools::artifact::load(&w.project, &live).unwrap();
    assert_eq!((meta.artifact.versions.len(), meta.artifact.filename.as_str()), (1, "harvest.csv"));
    assert_eq!(w.read("harvest.csv").as_deref(), Some(HARVEST));
    w.model.answers(move |_| Answer::call("describe_sheet", json!({"sheet": copy})));
    w.model.says("It has a notes column.");
    let t = w.send("What's in it?");
    assert!(t.step("describe_sheet").done().contains("notes") || t.result("describe_sheet").contains("notes"), "{}", t.result("describe_sheet"));
}
