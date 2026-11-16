use super::*;

/// One frame `size`, with `events`.
fn run(ctx: &egui::Context, app: &mut ByobApp, size: egui::Vec2, events: Vec<egui::Event>) {
    let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)), events, ..Default::default() };
    let mut out = ctx.run_ui(input, |ui| app.ui(ui));
    out.textures_delta.clear();
}

fn settle(ctx: &egui::Context, app: &mut ByobApp, size: egui::Vec2) -> composer::Rects {
    for _ in 0..4 {
        run(ctx, app, size, vec![]);
    }
    app.composer_rects
}

#[test]
fn markdown_on_one_line_keeps_the_field_one_line_tall() {
    let ctx = egui::Context::default();
    let mut app = crowded_app();
    let size = egui::vec2(1000.0, 700.0);
    settle(&ctx, &mut app, size);
    app.input = String::new();
    let empty = settle(&ctx, &mut app, size);
    for input in ["**bold**, *italic*, ~~gone~~ and `code`", "- [ ] a task with a [link](https://example.com)", "> quoted"] {
        app.input = input.into();
        let r = settle(&ctx, &mut app, size);
        assert!((r.frame.height() - empty.frame.height()).abs() < 1.5, "{input:?}: {r:?} vs {empty:?}");
    }
}

#[test]
fn the_field_takes_at_most_a_share_of_a_short_window() {
    let ctx = egui::Context::default();
    let mut app = crowded_app();
    settle(&ctx, &mut app, egui::vec2(1000.0, 1000.0));
    app.input = "line\n".repeat(60);
    let tall = settle(&ctx, &mut app, egui::vec2(1000.0, 1000.0));
    let short = settle(&ctx, &mut app, egui::vec2(1000.0, 320.0));
    // (The field rect is the whole text, scrolled; the frame is what shows: the text plus margins.)
    assert!(short.frame.height() <= 320.0 * 0.38 + 16.0, "{short:?}");
    assert!(tall.frame.height() > short.frame.height() + 40.0, "ten lines where there is room: {tall:?} {short:?}");
    // And it holds still from frame to frame.
    let again = settle(&ctx, &mut app, egui::vec2(1000.0, 320.0));
    assert!((again.frame.height() - short.frame.height()).abs() < 0.5);
}

#[test]
fn ctrl_b_and_ctrl_i_wrap_the_selection_while_the_composer_is_focused() {
    let ctx = egui::Context::default();
    let mut app = crowded_app();
    let size = egui::vec2(1000.0, 700.0);
    let id = egui::Id::unique("composer");
    settle(&ctx, &mut app, size);
    app.input = "say hello now".into();
    ctx.memory_mut(|m| m.request_focus(id));
    settle(&ctx, &mut app, size);
    let select = |ctx: &egui::Context, a: usize, b: usize| {
        let mut state = egui::TextEdit::load_state(ctx, id).unwrap_or_default();
        state.cursor.set_char_range(Some(egui::text::CCursorRange::two(egui::text::CCursor::new(a), egui::text::CCursor::new(b))));
        egui::TextEdit::store_state(ctx, id, state);
    };
    let key = |key| egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::COMMAND };
    select(&ctx, 4, 9);
    run(&ctx, &mut app, size, vec![key(egui::Key::B)]);
    assert_eq!(app.input, "say **hello** now");
    // The same words stay selected, so the next press undoes it, and Ctrl+I italicises.
    run(&ctx, &mut app, size, vec![key(egui::Key::B)]);
    assert_eq!(app.input, "say hello now");
    run(&ctx, &mut app, size, vec![key(egui::Key::I)]);
    assert_eq!(app.input, "say *hello* now");
    let range = egui::TextEdit::load_state(&ctx, id).and_then(|s| s.cursor.char_range()).map(|r| r.sorted_cursors().map(|c| c.index.0));
    assert_eq!(range, Some([5, 10]));
    // Not focused: the keys are left alone.
    ctx.memory_mut(|m| m.surrender_focus(id));
    run(&ctx, &mut app, size, vec![key(egui::Key::B)]);
    assert_eq!(app.input, "say *hello* now");
}

#[test]
fn a_long_draft_shows_its_end_where_the_cursor_is() {
    let ctx = egui::Context::default();
    let mut app = crowded_app();
    let size = egui::vec2(1000.0, 700.0);
    settle(&ctx, &mut app, size);
    // The cursor has been at the start of the empty, focused field.
    ctx.memory_mut(|m| m.request_focus(egui::Id::unique("composer")));
    settle(&ctx, &mut app, size);
    let draft = "line\n".repeat(40) + "the end";
    app.set_draft(&draft);
    let r = settle(&ctx, &mut app, size);
    let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)), ..Default::default() };
    let mut out = ctx.run_ui(input, |ui| app.ui(ui));
    out.textures_delta.clear();
    let text = out.shapes.iter().find_map(|c| match &c.shape {
        egui::Shape::Text(t) if t.galley.text() == draft => Some(t.visual_bounding_rect()),
        _ => None,
    });
    let Some(text) = text else { panic!("the draft wasn't drawn") };
    // Scrolled to the bottom: the last line sits at the bottom of the frame, the first far above it.
    assert!(text.bottom() <= r.frame.bottom() && text.bottom() > r.frame.bottom() - 30.0, "{text:?} {r:?}");
    assert!(text.top() < r.frame.top() - 200.0, "{text:?} {r:?}");
}
