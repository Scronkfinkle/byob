use super::*;

/// One frame 1000 × 700 with the pointer at `pointer`; every piece of text it drew.
fn drawn(ctx: &egui::Context, app: &mut ByobApp, pointer: egui::Pos2) -> String {
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
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 700.0));
    let input = egui::RawInput { screen_rect: Some(screen), events: vec![egui::Event::PointerMoved(pointer)], ..Default::default() };
    let mut out = ctx.run_ui(input, |ui| app.ui(ui));
    out.textures_delta.clear();
    let mut text = String::new();
    for s in &out.shapes {
        walk(&s.shape, &mut text);
    }
    text
}

fn entry(id: u64, role: byob_protocol::EntryRole, content: &str, stats: Option<byob_protocol::TurnStats>) -> byob_protocol::Entry {
    byob_protocol::Entry {
        id,
        role,
        content: content.into(),
        reasoning: if stats.is_some() { "Let me think about pears.".into() } else { String::new() },
        done: true,
        error: None,
        stats,
        steps: vec![],
        undoable: false,
        undone: false,
        attachments: vec![],
    }
}

/// A reply shows its text, not who wrote it, how long its reasoning was, or its token
/// counts; those (the counts) appear while the pointer is on it.
#[test]
fn replies_keep_their_details_out_of_the_way_until_hovered() {
    let fake = Fake::default();
    let mut app = crowded_with(fake.clone());
    let stats = byob_protocol::TurnStats { prompt_tokens: 1234, completion_tokens: 56, elapsed_ms: 2500, model_calls: 1 };
    fake.inbox.lock().unwrap().push(ServerMsg::Transcript {
        entries: vec![
            entry(1, byob_protocol::EntryRole::User, "How many pears?", None),
            entry(2, byob_protocol::EntryRole::Assistant, "Seven pears.", Some(stats)),
        ],
    });
    let ctx = egui::Context::default();
    let away = egui::pos2(999.0, 1.0);
    for _ in 0..4 {
        drawn(&ctx, &mut app, away);
    }
    let text = drawn(&ctx, &mut app, away);
    assert!(text.contains("Seven pears.") && text.contains("How many pears?"), "{text}");
    for noise in ["You\n", "byob\n", "out tokens", "chars"] {
        assert!(!text.contains(noise), "{noise:?} drew: {text}");
    }
    assert!(text.contains("Thought\n"), "{text}");
    // On the reply (somewhere above the composer): its time and tokens. The bubble's rect is
    // last frame's, so each spot gets two frames.
    let x = app.composer_rects.frame.center().x;
    let bottom = app.composer_rects.frame.top();
    let shows = (40..bottom as i32).step_by(8).any(|y| {
        let at = egui::pos2(x, y as f32);
        drawn(&ctx, &mut app, at);
        drawn(&ctx, &mut app, at).contains("2.5 s · 1234 in, 56 out tokens")
    });
    assert!(shows, "hovering the reply shows its stats");
}

/// The composer's hint says what the box is for, nothing about keys.
#[test]
fn the_composer_hint_is_plain() {
    let fake = Fake::default();
    let mut app = crowded_with(fake);
    let ctx = egui::Context::default();
    for _ in 0..3 {
        drawn(&ctx, &mut app, egui::pos2(999.0, 1.0));
    }
    let text = drawn(&ctx, &mut app, egui::pos2(999.0, 1.0));
    assert!(text.contains("Message byob…\n"), "{text}");
    assert!(!text.contains("Shift+Enter"), "{text}");
}
