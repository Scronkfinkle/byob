use super::*;

/// One frame 1000 × 700; every piece of text it drew.
fn drawn(ctx: &egui::Context, app: &mut ByobApp) -> String {
    fn walk(shape: &egui::Shape, out: &mut String) {
        match shape {
            egui::Shape::Text(t) => {
                out.push_str(t.galley.text());
                out.push('\n');
            }
            egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, out)),
            _ => {}
        }
    }
    let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 700.0))), ..Default::default() };
    let mut out = ctx.run_ui(input, |ui| app.ui(ui));
    out.textures_delta.clear();
    let mut text = String::new();
    for s in &out.shapes {
        walk(&s.shape, &mut text);
    }
    text
}

fn frames(ctx: &egui::Context, app: &mut ByobApp, n: usize) -> String {
    (0..n).map(|_| drawn(ctx, app)).last().unwrap_or_default()
}

/// A plugin whose manifest inserts a counter.
fn counter_plugin() -> ServerMsg {
    let mut p = plugins::tests::info("p", byob_protocol::PluginScope::Global, byob_protocol::PluginOrigin::User);
    p.fluid = Some(serde_json::json!({"insert": [{"id": "count", "at": "free", "x": 40, "y": 40,
        "tree": {"type": "text", "text": "Counted {state.n}"}, "state": {"n": 1}}]}));
    plugins::tests::ext(vec![p])
}

/// Regressions: a plugin's insert ignored its own state actions (`inc:`, `set:`, …: they
/// were looked for among the user's inserts only), and edit mode's "Plugins' changes"
/// switch left its rules out but still drew its inserts.
#[test]
fn a_plugins_insert_acts_on_its_state_and_goes_with_its_rules() {
    let fake = Fake::default();
    let mut app = crowded_with(fake.clone());
    fake.inbox.lock().unwrap().push(counter_plugin());
    let ctx = egui::Context::default();
    assert!(frames(&ctx, &mut app, 4).contains("Counted 1"));
    let inc = byob_protocol::tree::Event { action: "inc:n=1".into(), ..Default::default() };
    app.fluid_action(&ctx, "p.count", &inc);
    let text = frames(&ctx, &mut app, 3);
    assert!(text.contains("Counted 2"), "{text}");
    assert!(!text.matches("Counted").nth(1).is_some(), "drawn once");
    assert_eq!(app.prefs().fluid.inserts.len(), 1, "its state is kept with the user's changes");
    crate::fluid::with(&ctx, |f| f.ignore_base = true);
    assert!(!frames(&ctx, &mut app, 3).contains("Counted"), "left out with the plugin's other changes");
}

/// Regression: on a small window edit mode's inspector ran past the bottom edge (its
/// fields, Ask byob and Find out of reach: it can't be moved up past the top either).
#[test]
fn edit_modes_inspector_fits_a_small_window() {
    let mut app = crowded_app();
    let ctx = egui::Context::default();
    let size = egui::vec2(560.0, 420.0);
    let run = |app: &mut ByobApp| {
        let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)), ..Default::default() };
        let mut out = ctx.run_ui(input, |ui| app.ui(ui));
        out.textures_delta.clear();
    };
    run(&mut app);
    crate::fluid::with(&ctx, |f| f.editing = true);
    app.fluid_editor.selected = Some("composer".into());
    for _ in 0..4 {
        run(&mut app);
    }
    let inspector = ctx.memory(|m| m.area_rect(crate::fluid::inspector_id())).unwrap();
    assert!(inspector.bottom() <= size.y + 0.5 && inspector.top() >= 0.0, "{inspector:?}");
}
