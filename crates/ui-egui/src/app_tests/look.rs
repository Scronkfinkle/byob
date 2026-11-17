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
    // The colours it reports are the ones drawn now: the accent role follows.
    assert_ne!(first[0].get("colour.accent"), answer.get("colour.accent"));
}
