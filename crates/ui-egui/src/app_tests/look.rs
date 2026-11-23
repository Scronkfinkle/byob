use super::*;

fn frames(ctx: &egui::Context, app: &mut ByobApp, n: usize) {
    for _ in 0..n {
        let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 700.0))), ..Default::default() };
        let mut out = ctx.run_ui(input, |ui| app.ui(ui));
        out.textures_delta.clear();
    }
}

fn reports(fake: &Fake) -> Vec<byob_protocol::look::LookReport> {
    fake.sent.lock().unwrap().iter().filter_map(|m| if let ClientMsg::ReportLook(r) = m { Some(r.clone()) } else { None }).collect()
}

/// With an engine that takes looks, the window reports its own once, again when it
/// changes, and answers a change the model asked for with what it applied and what not.
#[test]
fn the_window_reports_its_look_and_answers_changes() {
    let fake = Fake::default();
    let mut app = crowded_with(fake.clone());
    let ctx = egui::Context::default();
    frames(&ctx, &mut app, 3);
    assert!(reports(&fake).is_empty(), "no report to an engine that didn't say it takes them");
    fake.inbox.lock().unwrap().push(ServerMsg::Welcome(Welcome::current()));
    frames(&ctx, &mut app, 3);
    let first = reports(&fake);
    assert_eq!(first.len(), 1, "once, not every frame");
    assert_eq!(first[0].get("corners"), Some(&serde_json::json!(app.prefs().knobs.radius)));
    let changes = serde_json::from_value(serde_json::json!({"corners": 14, "accent": "teal", "zoom": 40})).unwrap();
    fake.inbox.lock().unwrap().push(ServerMsg::SetLook { request: 7, changes });
    frames(&ctx, &mut app, 3);
    let all = reports(&fake);
    assert_eq!(all.len(), 2, "{all:?}");
    let answer = &all[1];
    assert_eq!(answer.answered, Some(7));
    assert_eq!(answer.get("corners"), Some(&serde_json::json!(14.0)));
    assert_eq!(answer.get("accent"), Some(&serde_json::json!("teal")));
    assert!(answer.problems.len() == 1 && answer.problems[0].starts_with("zoom:"), "{:?}", answer.problems);
    assert_eq!(app.prefs().knobs.radius, 14.0, "applied (and kept by the app like any change)");
    // The colors it reports are the ones drawn now: the accent role follows.
    assert_ne!(first[0].get("color.accent"), answer.get("color.accent"));
}

/// Regression: a window out of sight (minimized, or covered by other windows) runs no
/// frames, and look changes waited for one: the model's `ui_look` timed out, retried, and
/// every change applied at once when the window came back. They're applied and answered
/// between frames now (`logic`, which eframe still runs then), each with its own answer.
#[test]
fn a_hidden_window_answers_every_look_change() {
    let fake = Fake::default();
    let mut app = crowded_with(fake.clone());
    let ctx = egui::Context::default();
    fake.inbox.lock().unwrap().push(ServerMsg::Welcome(Welcome::current()));
    frames(&ctx, &mut app, 3);
    let before = reports(&fake).len();
    for (request, corners) in [(8, 3), (9, 11)] {
        let changes = serde_json::from_value(serde_json::json!({"corners": corners})).unwrap();
        fake.inbox.lock().unwrap().push(ServerMsg::SetLook { request, changes });
    }
    // No frame, only logic.
    app.logic(&ctx);
    let answers: Vec<_> = reports(&fake).into_iter().skip(before).collect();
    assert_eq!(answers.iter().map(|r| r.answered).collect::<Vec<_>>(), [Some(8), Some(9)], "{answers:?}");
    assert_eq!(answers[1].get("corners"), Some(&serde_json::json!(11.0)));
    assert_eq!(app.prefs().knobs.radius, 11.0);
    // The next frame doesn't answer again.
    frames(&ctx, &mut app, 2);
    assert_eq!(reports(&fake).len(), before + 2);
}

/// Regression: a change with both look settings and fluid ops was kept as two undo steps
/// (the look, then the fluid), so "undo that" took back only the fluid half; and fluid
/// changes grew the undo history without bound.
#[test]
fn one_undo_takes_back_a_look_and_fluid_change() {
    let fake = Fake::default();
    let mut app = crowded_with(fake.clone());
    let ctx = egui::Context::default();
    fake.inbox.lock().unwrap().push(ServerMsg::Welcome(Welcome::current()));
    frames(&ctx, &mut app, 3);
    let start = app.prefs().clone();
    let change = serde_json::json!({"corners": 3, "fluid": {"set": [{"path": "header", "hidden": true}]}});
    fake.inbox.lock().unwrap().push(ServerMsg::SetLook { request: 1, changes: serde_json::from_value(change).unwrap() });
    frames(&ctx, &mut app, 2);
    assert_eq!(app.prefs().knobs.radius, 3.0);
    assert_ne!(app.prefs().fluid, start.fluid, "the fluid op applied");
    fake.inbox.lock().unwrap().push(ServerMsg::SetLook { request: 2, changes: serde_json::from_value(serde_json::json!({"undo": true})).unwrap() });
    frames(&ctx, &mut app, 2);
    assert_eq!(app.prefs(), &start, "one undo, the whole change");
    for n in 0..(crate::look::HISTORY + 10) {
        let ops = serde_json::json!({"fluid": {"set": [{"path": "header", "dx": n}]}});
        fake.inbox.lock().unwrap().push(ServerMsg::SetLook { request: 10 + n as u64, changes: serde_json::from_value(ops).unwrap() });
    }
    app.logic(&ctx);
    assert!(app.look_history.len() <= crate::look::HISTORY, "{}", app.look_history.len());
}
