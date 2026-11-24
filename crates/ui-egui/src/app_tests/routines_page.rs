use byob_protocol::routines::{Report, Routine, RoutineView, RunRecord, RunStatus, Trigger};

use super::*;
use crate::routines::View;

/// Tall enough for a whole routine page.
const H: f32 = 1800.0;

/// One frame `width` × [`H`] with `events`; the text it drew.
fn texts(ctx: &egui::Context, app: &mut ByobApp, width: f32, events: Vec<egui::Event>) -> plugins::tests::Texts {
    let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, H))), events, ..Default::default() };
    let mut out = ctx.run_ui(input, |ui| app.ui(ui));
    out.textures_delta.clear();
    plugins::tests::texts_of(&out)
}

/// Click where `text` (the first drawn that is exactly it, else the first that contains it)
/// was drawn last frame; the text drawn after.
fn click_text(ctx: &egui::Context, app: &mut ByobApp, width: f32, text: &str) -> plugins::tests::Texts {
    let drawn = texts(ctx, app, width, vec![]);
    let at = drawn.iter().find(|(t, _)| t == text).or_else(|| drawn.iter().find(|(t, _)| t.contains(text))).map(|(_, r)| r.center());
    let at = at.unwrap_or_else(|| panic!("no {text:?} in {:?}", drawn.iter().map(|t| &t.0).collect::<Vec<_>>()));
    let button = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
    for events in [vec![egui::Event::PointerMoved(at)], vec![button(true)], vec![button(false)]] {
        texts(ctx, app, width, events);
    }
    texts(ctx, app, width, vec![])
}

fn sched(rule: &str) -> Vec<Trigger> {
    vec![Trigger::Schedule { rule: rule.into(), at: "08:00".into(), zone: Some("UTC".into()), start: Some("2026-11-20".into()) }]
}

fn routine(id: &str, name: &str, enabled: bool) -> Routine {
    Routine { id: id.into(), name: name.into(), instructions: format!("{name}: look around."), enabled, when: sched("FREQ=DAILY"), ..Routine::default() }
}

/// The loom routine that found something at `at`, a quiet one, one waiting for you, one off.
fn four(at: i64) -> Vec<RoutineView> {
    let run = |status, report: Option<&str>| RunRecord {
        chat: Some(format!("run-{status:?}")),
        started: at,
        finished: Some(at + 1000),
        status,
        report: report.map(|r| Report { title: r.into(), body: "At The Woolery until Sunday.".into() }),
        ..RunRecord::default()
    };
    let view = |r: Routine, last: Option<RunRecord>| RoutineView {
        schedule: vec!["Every day at 8:00 AM".into()],
        next_run: r.enabled.then_some(at + 86_400_000),
        runs_per_month: Some(30),
        routine: r,
        last_run: last,
    };
    vec![
        view(routine("loom", "Loom deals", true), Some(run(RunStatus::Reported, Some("Schacht Wolf Pup 8S, 20% off")))),
        view(routine("inbox", "Sort the inbox", true), Some(run(RunStatus::Waiting, None))),
        view(routine("harvest", "Weekly harvest sheet", true), Some(run(RunStatus::Quiet, None))),
        view(routine("tickets", "Concert tickets", false), None),
    ]
}

/// An app whose engine has routines, with `items`.
fn with_routines(items: Vec<RoutineView>) -> (Fake, ByobApp) {
    let fake = Fake::default();
    let app = crowded_with(fake.clone());
    fake.inbox.lock().unwrap().extend([ServerMsg::Welcome(Welcome::current()), ServerMsg::Routines { items }]);
    (fake, app)
}

fn sent(fake: &Fake) -> Vec<ClientMsg> {
    std::mem::take(&mut *fake.sent.lock().unwrap())
}

/// The sidebar has Routines once the engine does; the page lists each routine with what its
/// last run did (found, waiting for you, nothing new), when it's next, Paused when off.
#[test]
fn the_routines_page_lists_what_each_did_last_and_when_its_next() {
    let ctx = egui::Context::default();
    let (_fake, mut app) = crowded_app_pair();
    let before = texts(&ctx, &mut app, 1100.0, vec![]);
    assert!(!plugins::tests::has(&before, "Routines"), "an engine without routines: no row");
    let (_fake, mut app) = with_routines(four(sidebar::now_ms()));
    texts(&ctx, &mut app, 1100.0, vec![]);
    let page = click_text(&ctx, &mut app, 1100.0, "Routines");
    assert_eq!(app.page, Page::Routines);
    for want in [
        "Loom deals",
        "Every day at 8:00 AM",
        "Found:",
        "Schacht Wolf Pup 8S, 20% off",
        "Waiting for you",
        "Nothing new",
        "Paused",
        "New routine",
        "Last run",
        "Next",
    ] {
        assert!(plugins::tests::has(&page, want), "{want}: {:?}", page.iter().map(|t| &t.0).collect::<Vec<_>>());
    }
    assert!(page.iter().any(|t| t.0.starts_with("Tomorrow, ")), "next run in words");
    // A row opens its routine's page.
    let detail = click_text(&ctx, &mut app, 1100.0, "Loom deals");
    assert_eq!(app.routines.view, View::Routine("loom".into()));
    for want in ["Name", "What to do", "When", "It may, on its own", "Tell me", "Runs", "Run now"] {
        assert!(plugins::tests::has(&detail, want), "{want}");
    }
}

/// [`crowded_app`] and its connection.
fn crowded_app_pair() -> (Fake, ByobApp) {
    let fake = Fake::default();
    let app = crowded_with(fake.clone());
    (fake, app)
}

/// A new routine's When controls make the schedule it's saved with: a day taken off every
/// day makes chosen days; "Every few hours" with "6 hours" an hourly rule.
#[test]
fn the_editor_builds_the_schedule_from_its_controls() {
    let ctx = egui::Context::default();
    let (fake, mut app) = with_routines(vec![]);
    texts(&ctx, &mut app, 1100.0, vec![]);
    assert!(app.open_panel("new-routine"));
    if let Some(e) = &mut app.routines.editor {
        e.name = "Loom deals".into();
        e.instructions = "Look for a loom.".into();
    }
    texts(&ctx, &mut app, 1100.0, vec![]);
    // Wednesday off: Mondays, Tuesdays, Thursdays…
    click_text(&ctx, &mut app, 1100.0, "W");
    sent(&fake);
    click_text(&ctx, &mut app, 1100.0, "Create routine");
    let saved = crate::routines::page::sent_saves(&sent(&fake));
    let Some(Trigger::Schedule { rule, at, zone, .. }) = saved.first().and_then(|r| r.when.first()) else { panic!("{saved:?}") };
    assert_eq!((rule.as_str(), at.as_str(), zone.as_deref()), ("FREQ=WEEKLY;BYDAY=MO,TU,TH,FR,SA,SU", "08:00", None), "the engine's own zone");
    // Every 6 hours, through the selects.
    click_text(&ctx, &mut app, 1100.0, "On chosen days");
    click_text(&ctx, &mut app, 1100.0, "Every few hours");
    click_text(&ctx, &mut app, 1100.0, "2 hours");
    click_text(&ctx, &mut app, 1100.0, "6 hours");
    click_text(&ctx, &mut app, 1100.0, "Create routine");
    let saved = crate::routines::page::sent_saves(&sent(&fake));
    assert!(matches!(saved.first().and_then(|r| r.when.first()), Some(Trigger::Schedule { rule, .. }) if rule == "FREQ=HOURLY;INTERVAL=6"), "{saved:?}");
    let r = saved.first().unwrap();
    assert_eq!(
        (r.id.as_str(), r.allowed, r.channels.as_slice()),
        ("", byob_types::config::Approvals::ReadOnly, &["desktop".to_owned()][..]),
        "new, read only, a notification"
    );
    // Once the engine lists it, its page opens.
    let mut items = vec![RoutineView { routine: Routine { id: "loom-deals".into(), ..r.clone() }, ..RoutineView::default() }];
    fake.inbox.lock().unwrap().push(ServerMsg::Routines { items: std::mem::take(&mut items) });
    texts(&ctx, &mut app, 1100.0, vec![]);
    assert_eq!(app.routines.view, View::Routine("loom-deals".into()));
}

/// Changing a saved routine shows Save and Discard; Discard puts the form back.
#[test]
fn a_changed_routine_saves_or_discards() {
    let ctx = egui::Context::default();
    let (fake, mut app) = with_routines(four(sidebar::now_ms()));
    texts(&ctx, &mut app, 1100.0, vec![]);
    assert!(app.open_panel("routine:loom"));
    let page = texts(&ctx, &mut app, 1100.0, vec![]);
    assert!(!plugins::tests::has(&page, "Discard"), "nothing changed yet");
    if let Some(e) = &mut app.routines.editor {
        e.name = "Loom bargains".into();
    }
    let page = texts(&ctx, &mut app, 1100.0, vec![]);
    assert!(plugins::tests::has(&page, "Discard"));
    click_text(&ctx, &mut app, 1100.0, "Discard");
    assert_eq!(app.routines.editor.as_ref().map(|e| e.name.as_str()), Some("Loom deals"));
    if let Some(e) = &mut app.routines.editor {
        e.name = "Loom bargains".into();
    }
    sent(&fake);
    click_text(&ctx, &mut app, 1100.0, "Save");
    let saved = crate::routines::page::sent_saves(&sent(&fake));
    assert_eq!(saved.first().map(|r| (r.id.as_str(), r.name.as_str())), Some(("loom", "Loom bargains")));
    assert_eq!(saved.first().map(|r| r.when.clone()), Some(sched("FREQ=DAILY")), "an untouched schedule is kept word for word");
}

/// Run chats stay out of Recent and search; viewing one says whose run it is, and Open
/// routine goes to its page.
#[test]
fn run_chats_are_filed_under_their_routine() {
    let ctx = egui::Context::default();
    let (fake, mut app) = with_routines(four(sidebar::now_ms()));
    let chat = |id: &str, title: &str, origin: Option<&str>| ChatSummary {
        id: id.into(),
        title: title.into(),
        updated_at: 5,
        pinned_at: None,
        busy: false,
        workspace: None,
        archived_at: None,
        origin: origin.map(str::to_owned),
    };
    let run = chat("run-Reported", "Loom deals · Tue Nov 24, 8:00 AM", Some("loom"));
    fake.inbox.lock().unwrap().extend([ServerMsg::Chats {
        pinned: vec![],
        recent: vec![chat("mine", "Find me a loom", None), run.clone()],
        after: None,
        next: None,
    }]);
    let list = texts(&ctx, &mut app, 1100.0, vec![]);
    assert!(plugins::tests::has(&list, "Find me a loom"));
    assert!(!plugins::tests::has(&list, "Loom deals · Tue"), "not in Recent");
    // A run that comes in later stays out too.
    fake.inbox.lock().unwrap().push(ServerMsg::ChatUpdated(chat("run-2", "Loom deals · Wed Nov 25, 8:00 AM", Some("loom"))));
    let list = texts(&ctx, &mut app, 1100.0, vec![]);
    assert!(!plugins::tests::has(&list, "Loom deals · Wed"));
    app.open_panel("palette:loom deals");
    let found = texts(&ctx, &mut app, 1100.0, vec![]);
    assert!(!found.iter().any(|t| t.0.starts_with("Loom deals · ")), "nor in search");
    app.sidebar.palette = None;
    // Opened (from its routine, or a notification): the strip over it.
    fake.inbox.lock().unwrap().extend([
        ServerMsg::ChatOpened { chat: Some(run), workspace: None },
        ServerMsg::RoutineRuns { routine: "loom".into(), runs: four(sidebar::now_ms())[0].last_run.clone().into_iter().collect() },
    ]);
    let strip = texts(&ctx, &mut app, 1100.0, vec![]);
    assert!(plugins::tests::has(&strip, "A run of "), "{strip:?}");
    assert!(plugins::tests::has(&strip, "Ask anything about it here."));
    assert!(strip.iter().any(|t| t.0.starts_with(", today at ")), "when it ran: {strip:?}");
    // The reply that reported shows the report as a card.
    fake.inbox.lock().unwrap().push(ServerMsg::Transcript { entries: vec![reply(vec![done_step("report", "Reported.")])] });
    let shown = texts(&ctx, &mut app, 1100.0, vec![]);
    assert!(plugins::tests::has(&shown, "At The Woolery until Sunday."), "the report's body: {shown:?}");
    assert!(plugins::tests::has(&shown, "Ask about this run…"), "the composer: {shown:?}");
    click_text(&ctx, &mut app, 1100.0, "Open routine");
    assert_eq!((app.page, app.routines.view.clone()), (Page::Routines, View::Routine("loom".into())));
}

/// A run that found something after you last looked puts a dot on Routines until you look.
#[test]
fn the_sidebar_dot_shows_a_report_you_havent_seen() {
    let ctx = egui::Context::default();
    let (fake, mut app) = with_routines(four(1));
    texts(&ctx, &mut app, 1100.0, vec![]);
    assert!(!app.sidebar.routines_dot, "reports from before this device knew the routine");
    fake.inbox.lock().unwrap().push(ServerMsg::Routines { items: four(sidebar::now_ms() + 60_000) });
    texts(&ctx, &mut app, 1100.0, vec![]);
    assert!(app.sidebar.routines_dot);
    click_text(&ctx, &mut app, 1100.0, "Routines");
    texts(&ctx, &mut app, 1100.0, vec![]);
    assert!(!app.sidebar.routines_dot, "seen on the page");
    // Remembered on the device.
    let saved: sidebar::SidebarPrefs = serde_json::from_str(&serde_json::to_string(&app.sidebar_prefs()).unwrap()).unwrap();
    assert!(saved.routines_seen.contains_key("loom"));
}

/// On a phone the page is one column: every routine still has its switch and ⋯ in view,
/// and a routine's runs come under its form.
#[test]
fn a_phone_gets_one_column() {
    let ctx = egui::Context::default();
    let (_fake, mut app) = with_routines(four(sidebar::now_ms()));
    texts(&ctx, &mut app, 390.0, vec![]);
    app.open_panel("routines");
    let page = texts(&ctx, &mut app, 390.0, vec![]);
    for (t, r) in &page {
        assert!(r.right() <= 390.5, "{t:?} runs off the screen at {r:?}");
    }
    assert!(plugins::tests::has(&page, "Waiting for you"));
    app.open_panel("routine:loom");
    let page = texts(&ctx, &mut app, 390.0, vec![]);
    let y = |s: &str| page.iter().find(|t| t.0 == s).map(|t| t.1.top()).unwrap_or_else(|| panic!("{s}: {page:?}"));
    assert!(y("Runs") > y("Tell me"), "runs under the form");
}

/// A reply that made a routine offers to run it now.
#[test]
fn a_reply_that_made_a_routine_offers_run_it_now() {
    let ctx = egui::Context::default();
    let (fake, mut app) = with_routines(four(sidebar::now_ms()));
    let step = done_step("create_routine", "Created the routine \"Loom deals\" (id loom): Every day at 8:00 AM. Offer to run it once now");
    let entry = reply(vec![step]);
    fake.inbox.lock().unwrap().push(ServerMsg::Transcript { entries: vec![entry] });
    let shown = texts(&ctx, &mut app, 1100.0, vec![]);
    assert!(plugins::tests::has(&shown, "Created"), "{shown:?}");
    sent(&fake);
    click_text(&ctx, &mut app, 1100.0, "Run it now");
    assert!(sent(&fake).contains(&ClientMsg::RunRoutine { id: "loom".into() }));
}

/// A finished call of `tool` whose result was `detail`.
fn done_step(tool: &str, detail: &str) -> byob_protocol::Step {
    byob_protocol::Step {
        id: 7,
        tool: tool.into(),
        title: format!("{tool} done"),
        access: byob_protocol::ToolAccess::Write,
        status: StepStatus::Done,
        detail: detail.into(),
        changes: vec![],
        at: 0,
        preview: None,
        media: vec![],
        artifacts: vec![],
        delegation: None,
        ask: None,
        views: vec![],
    }
}

/// A finished reply with `steps`.
fn reply(steps: Vec<byob_protocol::Step>) -> Entry {
    Entry {
        id: 3,
        role: byob_protocol::EntryRole::Assistant,
        content: "Done.".into(),
        reasoning: String::new(),
        done: true,
        error: None,
        stats: None,
        steps,
        undoable: false,
        undone: false,
        attachments: vec![],
    }
}

/// A routine's approval card in its own words: what it does instead of the access chip and
/// the tool's name, its fields only (the instructions whole), and its own button labels.
#[test]
fn the_routine_card_reads_plainly() {
    let ctx = egui::Context::default();
    let (fake, mut app) = with_routines(vec![]);
    let does = "Search the web for deals on a new floor loom. Compare with what earlier runs found and only report a lower price or a model we haven't seen before; skip kits without the loom.";
    let mut step = done_step("create_routine", "");
    step.status = StepStatus::Waiting;
    step.title = "Create the routine Loom deals".into();
    step.preview = Some(byob_protocol::Preview::Args {
        fields: vec![("Routine".into(), "Loom deals".into()), ("When".into(), "Every day at 8:00 AM".into()), ("Does".into(), does.into())],
        code: None,
        more: 0,
    });
    step.ask = Some(Box::new(byob_protocol::Ask {
        warning: Some("A routine runs later by itself.".into()),
        always: true,
        card: Some(byob_protocol::CardWords { access: "Sets up a routine".into(), allow: Some("Create routine".into()), deny: Some("Not now".into()) }),
        ..Default::default()
    }));
    let mut entry = reply(vec![step]);
    entry.done = false;
    fake.inbox.lock().unwrap().push(ServerMsg::Transcript { entries: vec![entry] });
    let shown = texts(&ctx, &mut app, 1100.0, vec![]);
    let all: Vec<&str> = shown.iter().map(|t| t.0.as_str()).collect();
    for want in ["Sets up a routine", "Create routine", "Not now", "Every day at 8:00 AM", "A routine runs later by itself."] {
        assert!(plugins::tests::has(&shown, want), "{want}: {all:?}");
    }
    assert!(all.iter().any(|t| t.replace('\n', " ").contains("skip kits without the loom.")), "the instructions whole: {all:?}");
    for not in ["changes files", "create_routine", "Allow", "Deny"] {
        assert!(!all.contains(&not), "{not}: {all:?}");
    }
    sent(&fake);
    click_text(&ctx, &mut app, 1100.0, "Create routine");
    assert!(sent(&fake).iter().any(|m| matches!(m, ClientMsg::Approve { step: 7, answer: byob_protocol::ApprovalAnswer::Allow })));
}
