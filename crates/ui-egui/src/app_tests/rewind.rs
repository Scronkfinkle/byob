use super::*;
use byob_protocol::layout::{Layout, Show};

/// One frame 1000 × 700 with `events`; every piece of text it drew.
fn frame(ctx: &egui::Context, app: &mut ByobApp, events: Vec<egui::Event>) -> String {
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
    let input = egui::RawInput { screen_rect: Some(screen), events, ..Default::default() };
    let mut out = ctx.run_ui(input, |ui| app.ui(ui));
    out.textures_delta.clear();
    let mut text = String::new();
    out.shapes.iter().for_each(|s| walk(&s.shape, &mut text));
    text
}

fn chat(fake: &Fake, rewind: bool) {
    let user = byob_protocol::Entry {
        id: 1,
        role: byob_protocol::EntryRole::User,
        content: "How many pears?".into(),
        reasoning: String::new(),
        done: true,
        error: None,
        stats: None,
        steps: vec![],
        undoable: false,
        undone: false,
        attachments: vec![],
    };
    let reply = byob_protocol::Entry { id: 2, role: byob_protocol::EntryRole::Assistant, content: "Seven.".into(), ..user.clone() };
    let mut welcome = byob_protocol::Welcome::current();
    if !rewind {
        welcome.caps.retain(|c| c != byob_protocol::version::CAP_REWIND);
    }
    let mut l = Layout::default();
    l.set_show("message.edit", Show::Shown);
    fake.inbox.lock().unwrap().extend([ServerMsg::Welcome(welcome), ServerMsg::Transcript { entries: vec![user, reply] }, ServerMsg::Layout(l)]);
}

/// Edit on your message opens it in place; Ctrl+Enter goes back to it with the new text.
#[test]
fn editing_a_message_starts_over_from_it() {
    let fake = Fake::default();
    let mut app = crowded_with(fake.clone());
    chat(&fake, true);
    let ctx = egui::Context::default();
    for _ in 0..3 {
        frame(&ctx, &mut app, vec![]);
    }
    let text = frame(&ctx, &mut app, vec![]);
    assert!(text.contains("Edit\n"), "{text}");
    // What the Edit button does.
    app.editing.open = Some((1, "How many pears, really?".into()));
    let text = frame(&ctx, &mut app, vec![]);
    assert!(text.contains("Sending starts over from here") && text.contains("Cancel"), "{text}");
    frame(&ctx, &mut app, vec![]);
    let send = egui::Event::Key { key: egui::Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::COMMAND };
    frame(&ctx, &mut app, vec![send]);
    let sent = fake.sent.lock().unwrap();
    assert!(sent.iter().any(|m| matches!(m, ClientMsg::Rewind { entry: 1, text } if text == "How many pears, really?")), "{sent:?}");
    assert!(app.editing.open.is_none());
}

/// An engine that can't go back: no Edit.
#[test]
fn no_edit_without_an_engine_that_goes_back() {
    let fake = Fake::default();
    let mut app = crowded_with(fake.clone());
    chat(&fake, false);
    let ctx = egui::Context::default();
    for _ in 0..3 {
        frame(&ctx, &mut app, vec![]);
    }
    assert!(!frame(&ctx, &mut app, vec![]).contains("Edit\n"));
}

/// Regression: the Edit row was a right-to-left layout that took all the height left, so
/// your message filled the window. With Edit shown, and while editing, the reply stays near
/// the top.
#[test]
fn edit_keeps_your_message_its_size() {
    let fake = Fake::default();
    let mut app = crowded_with(fake.clone());
    chat(&fake, true);
    let ctx = egui::Context::default();
    let reply_top = |ctx: &egui::Context, app: &mut ByobApp| {
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 700.0));
        let mut out = ctx.run_ui(egui::RawInput { screen_rect: Some(screen), ..Default::default() }, |ui| app.ui(ui));
        out.textures_delta.clear();
        let mut top = None;
        fn find(shape: &egui::Shape, top: &mut Option<f32>) {
            match shape {
                egui::Shape::Text(t) if t.galley.text() == "Seven." => *top = Some(t.pos.y),
                egui::Shape::Vec(v) => v.iter().for_each(|s| find(s, top)),
                _ => {}
            }
        }
        out.shapes.iter().for_each(|s| find(&s.shape, &mut top));
        top
    };
    for _ in 0..3 {
        reply_top(&ctx, &mut app);
    }
    let y = reply_top(&ctx, &mut app).expect("the reply is drawn");
    assert!(y < 300.0, "the reply starts at {y}");
    app.editing.open = Some((1, "How many pears?".into()));
    reply_top(&ctx, &mut app);
    let y = reply_top(&ctx, &mut app).expect("the reply is drawn");
    assert!(y < 400.0, "while editing, the reply starts at {y}");
}

/// Regression: on a touch screen (a phone) there is no hover, so Edit, shown while the
/// pointer is on your message, could never be reached. A touch on the message shows it.
#[test]
fn a_touch_on_your_message_shows_edit() {
    let fake = Fake::default();
    let mut app = crowded_with(fake.clone());
    let user = byob_protocol::Entry {
        id: 1,
        role: byob_protocol::EntryRole::User,
        content: "How many pears?".into(),
        reasoning: String::new(),
        done: true,
        error: None,
        stats: None,
        steps: vec![],
        undoable: false,
        undone: false,
        attachments: vec![],
    };
    let reply = byob_protocol::Entry { id: 2, role: byob_protocol::EntryRole::Assistant, content: "Seven.".into(), ..user.clone() };
    // The default layout: Edit while the pointer is on the message.
    fake.inbox.lock().unwrap().extend([ServerMsg::Welcome(byob_protocol::Welcome::current()), ServerMsg::Transcript { entries: vec![user, reply] }]);
    let ctx = egui::Context::default();
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(390.0, 844.0));
    let mut run = |events: Vec<egui::Event>| {
        let input = egui::RawInput { screen_rect: Some(screen), events, ..Default::default() };
        let mut out = ctx.run_ui(input, |ui| app.ui(ui));
        out.textures_delta.clear();
        plugins::tests::texts_of(&out)
    };
    for _ in 0..3 {
        run(vec![]);
    }
    let texts = run(vec![]);
    assert!(!texts.iter().any(|t| t.0 == "Edit"), "no Edit before: {texts:?}");
    let at = texts.iter().find(|t| t.0 == "How many pears?").map(|t| t.1.center()).unwrap();
    // A tap, as the browser sends it: a touch, and the pointer down, up and gone.
    let touch = |phase| egui::Event::Touch { device_id: egui::TouchDeviceId(1), id: egui::TouchId(0), phase, pos: at, force: None };
    let button = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
    run(vec![touch(egui::TouchPhase::Start), egui::Event::PointerMoved(at), button(true)]);
    run(vec![touch(egui::TouchPhase::End), button(false), egui::Event::PointerGone]);
    run(vec![]);
    let texts = run(vec![]);
    assert!(texts.iter().any(|t| t.0 == "Edit"), "Edit after a touch: {texts:?}");
}
