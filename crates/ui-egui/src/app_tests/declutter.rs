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

fn pears(fake: &Fake) {
    let stats = byob_protocol::TurnStats { prompt_tokens: 1234, completion_tokens: 56, elapsed_ms: 2500, model_calls: 1, context_tokens: 0 };
    fake.inbox.lock().unwrap().push(ServerMsg::Transcript {
        entries: vec![
            entry(1, byob_protocol::EntryRole::User, "How many pears?", None),
            entry(2, byob_protocol::EntryRole::Assistant, "Seven pears.", Some(stats)),
        ],
    });
}

/// Settle and draw with the pointer away from everything.
fn away(ctx: &egui::Context, app: &mut ByobApp) -> String {
    let away = egui::pos2(999.0, 1.0);
    for _ in 0..4 {
        drawn(ctx, app, away);
    }
    drawn(ctx, app, away)
}

/// Whether hovering somewhere on the reply (above the composer) draws `want`. The bubble's
/// rect is last frame's, so each spot gets two frames.
fn hovering_the_reply_shows(ctx: &egui::Context, app: &mut ByobApp, want: &str) -> bool {
    let x = app.composer_rects.frame.center().x;
    let bottom = app.composer_rects.frame.top();
    (40..bottom as i32).step_by(8).any(|y| {
        let at = egui::pos2(x, y as f32);
        drawn(ctx, app, at);
        drawn(ctx, app, at).contains(want)
    })
}

/// By default (and with an engine that has no layout) a reply says who wrote it, how long
/// its reasoning was, and its tokens and time under it, and the composer shows its keys.
#[test]
fn by_default_every_detail_is_shown() {
    let fake = Fake::default();
    let mut app = crowded_with(fake.clone());
    pears(&fake);
    let ctx = egui::Context::default();
    let text = away(&ctx, &mut app);
    for want in ["Seven pears.", "You\n", "byob\n", "Thought · ~8 tokens", "1234 in · 56 out tokens · 2.5 s", "Shift+Enter for a new line", "1%"] {
        assert!(text.contains(want), "{want:?} isn't drawn: {text}");
    }
    assert!(!text.contains("tokens/s"), "speed is off by default: {text}");
}

/// The minimal preset: no labels, no reasoning length, no keys in the hint, and a reply's
/// tokens and time only while the pointer is on it.
#[test]
fn the_minimal_layout_keeps_details_out_of_the_way_until_hovered() {
    let fake = Fake::default();
    let mut app = crowded_with(fake.clone());
    pears(&fake);
    fake.inbox.lock().unwrap().push(ServerMsg::Layout(byob_protocol::layout::Layout::preset("minimal").unwrap()));
    let ctx = egui::Context::default();
    let text = away(&ctx, &mut app);
    assert!(text.contains("Seven pears.") && text.contains("How many pears?"), "{text}");
    for noise in ["You\n", "byob\n", "out tokens", "chars", "Shift+Enter"] {
        assert!(!text.contains(noise), "{noise:?} drew: {text}");
    }
    assert!(text.contains("Thought\n") && text.contains("Message byob…\n"), "{text}");
    assert!(hovering_the_reply_shows(&ctx, &mut app, "1234 in · 56 out tokens · 2.5 s"), "hovering the reply shows its stats");
}

/// A reply's details in the layout's order, with speed switched on and tokens hidden.
#[test]
fn a_replys_details_follow_the_layouts_order() {
    use byob_protocol::layout::{Layout, REPLY, Show};
    let fake = Fake::default();
    let mut app = crowded_with(fake.clone());
    pears(&fake);
    let mut l = Layout::default();
    l.set_show("reply.speed", Show::Shown);
    l.set_show("reply.tokens", Show::Hidden);
    l.set_order(REPLY, vec!["reply.speed".into(), "reply.time".into()]);
    fake.inbox.lock().unwrap().push(ServerMsg::Layout(l));
    let ctx = egui::Context::default();
    let text = away(&ctx, &mut app);
    assert!(text.contains("22 tokens/s · 2.5 s"), "{text}");
    assert!(!text.contains("out tokens"), "{text}");
}

/// The model and the context ring sit under the message box (they moved out of the
/// header). They're drawn right to left, so the text has the ring first.
#[test]
fn the_model_and_context_sit_under_the_message_box() {
    let fake = Fake::default();
    let mut app = crowded_with(fake.clone());
    let ctx = egui::Context::default();
    let text = away(&ctx, &mut app);
    let (box_, ring, model) = (text.find("Message byob…"), text.find("1%\n"), text.rfind("yet-another-extremely-long-model-name"));
    assert!(box_.zip(ring).zip(model).is_some_and(|((b, r), m)| b < r && r < m), "{text}");
    assert!(!text.contains("3.4k / 262k"), "the header meter is gone: {text}");
}

/// Every detail of a busy reply can be hidden: the thinking row, a tool's row and its file
/// count, the undo button, a code block's language and Copy. Failed steps go too (asked for).
#[test]
fn a_busy_replys_details_can_each_be_hidden() {
    use byob_protocol::layout::{Layout, Show};
    use byob_protocol::{FileChange, Step, StepStatus, ToolAccess};
    let step = |id, title: &str, status| Step {
        id,
        tool: "write_file".into(),
        title: title.into(),
        access: ToolAccess::Write,
        status,
        detail: String::new(),
        changes: vec![FileChange::Created { path: "a.txt".into() }],
        at: 0,
        preview: None,
        media: vec![],
        artifacts: vec![],
        delegation: None,
        ask: None,
        views: vec![],
    };
    let busy = || {
        let stats = byob_protocol::TurnStats { prompt_tokens: 10, completion_tokens: 5, elapsed_ms: 1000, model_calls: 1, context_tokens: 0 };
        let mut e = entry(2, byob_protocol::EntryRole::Assistant, "Done.\n\n```rust\nfn main() {}\n```\n", Some(stats));
        e.steps = vec![step(1, "Wrote a.txt", StepStatus::Done), step(2, "Wrote b.txt", StepStatus::Failed)];
        e.undoable = true;
        e
    };
    let run = |layout: Option<Layout>| {
        let fake = Fake::default();
        let mut app = crowded_with(fake.clone());
        fake.inbox.lock().unwrap().push(ServerMsg::Transcript { entries: vec![entry(1, byob_protocol::EntryRole::User, "Go", None), busy()] });
        if let Some(l) = layout {
            fake.inbox.lock().unwrap().push(ServerMsg::Layout(l));
        }
        away(&egui::Context::default(), &mut app)
    };
    let all = ["Thought", "Wrote a.txt", "1 file", "Undo changes", "rust\n", "Copy\n"];
    let text = run(None);
    for want in all {
        assert!(text.contains(want), "{want:?} isn't drawn: {text}");
    }
    let mut l = Layout::default();
    for id in ["message.thinking", "message.steps", "message.step_files", "message.undo", "code.language", "code.copy"] {
        l.set_show(id, Show::Hidden);
    }
    let text = run(Some(l));
    for gone in all {
        assert!(!text.contains(gone), "{gone:?} drew: {text}");
    }
    assert!(!text.contains("Wrote b.txt") && text.contains("fn main"), "failed steps go too, the code stays: {text}");
}

/// A reply's tokens say how full the context was at its end (as the meter counts) and what
/// several requests read together; replies saved before that say what they always said.
#[test]
fn a_replys_tokens_are_its_context_not_every_request_added_up() {
    let s = |calls, prompt, out, context| byob_protocol::TurnStats {
        prompt_tokens: prompt,
        completion_tokens: out,
        elapsed_ms: 1000,
        model_calls: calls,
        context_tokens: context,
    };
    assert_eq!(tokens_detail(&s(11, 134_400, 2_008, 12_400)), "12k context · 2k out · 11 calls read 134k");
    assert_eq!(tokens_detail(&s(1, 9_000, 300, 9_300)), "9.3k context · 300 out");
    assert_eq!(tokens_detail(&s(3, 1234, 56, 0)), "1234 in · 56 out tokens", "saved before the context was recorded");
}

/// The chat's settings show their groups: the basics (model, mode, instructions), where it
/// works, and the advanced ones behind one line until asked for.
#[test]
fn the_chat_settings_show_basics_where_and_advanced_folded() {
    let fake = Fake::default();
    let mut app = crowded_with(fake.clone());
    let ctx = egui::Context::default();
    away(&ctx, &mut app);
    app.workspaces.toggle_chat();
    away(&ctx, &mut app);
    let view = crate::workspace::tests::view(byob_protocol::ConfigScope::Chat, byob_protocol::workspace::Layer::default(), None);
    fake.inbox.lock().unwrap().push(ServerMsg::Config(Box::new(view)));
    let text = away(&ctx, &mut app);
    for want in ["Basics", "Model", "Mode", "How it answers", "Instructions", "Where it works", "Commands run on", "Advanced: tools, memory, helpers, plugins"]
    {
        assert!(text.contains(want), "{want:?} isn't drawn: {text}");
    }
    assert!(!text.contains("Steps per reply"), "advanced starts folded: {text}");
}

/// "Hide details" under the latest reply hides its thinking, tokens and time in this chat
/// (and only this one); "Show details" brings them back.
#[test]
fn hide_details_quiets_this_chats_replies() {
    let fake = Fake::default();
    let mut app = crowded_with(fake.clone());
    pears(&fake);
    let ctx = egui::Context::default();
    let text = away(&ctx, &mut app);
    assert!(text.contains("Hide details") && text.contains("1234 in · 56 out tokens · 2.5 s") && text.contains("Thought"), "{text}");
    let id = app.sidebar.current().map(|c| c.id.clone()).unwrap();
    app.sidebar.quiet.insert(id.clone());
    let text = away(&ctx, &mut app);
    assert!(text.contains("Show details") && text.contains("Seven pears."), "{text}");
    for gone in ["out tokens", "Thought"] {
        assert!(!text.contains(gone), "{gone:?} still drawn: {text}");
    }
    assert!(app.sidebar.prefs().quiet.contains(&id), "remembered on this device");
}
