//! Routines, end to end: the model makes one (the card says it in plain words), the scheduler
//! runs it when the (fake) clock says so, the run reports, the client hears about it; quiet
//! runs, a run stuck on an approval nobody answers, sleep and restarts, a routine that's off,
//! and the cap on runs at once.

use std::sync::atomic::{AtomicU64, Ordering};

use byob_protocol::routines::{Routine, RoutineEventKind, RoutineView, RunRecord, RunStatus, Trigger};
use byob_protocol::{ApprovalAnswer, Preview};
use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};

use super::*;
use crate::routines::NOBODY_ANSWERED;

/// A clock the scenario moves. Each engine start gets its own reading of it; an engine from
/// before a restart (whose thread lingers) sees the epoch, so it never runs anything again.
#[derive(Clone)]
struct Time {
    now: Arc<Mutex<DateTime<Utc>>>,
    starts: Arc<AtomicU64>,
}

fn at(s: &str) -> DateTime<Utc> {
    Utc.from_utc_datetime(&NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap())
}

impl Time {
    fn new(s: &str) -> Self {
        Self { now: Arc::new(Mutex::new(at(s))), starts: Arc::default() }
    }

    fn set(&self, s: &str) {
        *self.now.lock().unwrap() = at(s);
    }

    fn clock(&self) -> crate::Clock {
        let me = self.starts.fetch_add(1, Ordering::SeqCst) + 1;
        let (now, starts) = (Arc::clone(&self.now), Arc::clone(&self.starts));
        Arc::new(move || if starts.load(Ordering::SeqCst) == me { *now.lock().unwrap() } else { DateTime::UNIX_EPOCH })
    }

    fn world(&self) -> World {
        let t = self.clone();
        World::with(move |c| c.clock = t.clock())
    }
}

fn ms(s: &str) -> i64 {
    at(s).timestamp_millis()
}

fn daily(name: &str, at: &str) -> Routine {
    Routine {
        name: name.into(),
        instructions: format!("{name}: look for news."),
        when: vec![Trigger::Schedule { rule: "FREQ=DAILY".into(), at: at.into(), zone: Some("UTC".into()), start: None }],
        ..Routine::default()
    }
}

impl World {
    /// The next routine list byob sends that `f` accepts.
    fn routines_where(&mut self, mut f: impl FnMut(&[RoutineView]) -> bool) -> Vec<RoutineView> {
        self.wait(|m| match m {
            ServerMsg::Routines { items } if f(items) => Some(items.clone()),
            _ => None,
        })
    }

    /// Save a routine as a client would; its saved form.
    fn save(&mut self, r: Routine) -> Routine {
        let name = r.name.clone();
        self.tell(ClientMsg::SaveRoutine { routine: r });
        let items = self.routines_where(|items| items.iter().any(|v| v.routine.name == name));
        items.into_iter().find(|v| v.routine.name == name).unwrap().routine
    }

    /// A routine's runs, newest first.
    fn runs(&mut self, id: &str) -> Vec<RunRecord> {
        self.tell(ClientMsg::ListRoutineRuns { id: id.into() });
        self.wait(|m| match m {
            ServerMsg::RoutineRuns { routine, runs } if routine == id => Some(runs.clone()),
            _ => None,
        })
    }

    /// Wait for a routine event of `kind`: (routine, chat, title, body).
    fn event(&mut self, kind: RoutineEventKind) -> (String, Option<String>, String, String) {
        self.wait(|m| match m {
            ServerMsg::RoutineEvent { routine, chat, kind: k, title, body, .. } if *k == kind => {
                Some((routine.clone(), chat.clone(), title.clone(), body.clone()))
            }
            _ => None,
        })
    }

    /// Wait until routine `id`'s latest run has finished; its runs, newest first.
    fn finished(&mut self, id: &str) -> Vec<RunRecord> {
        self.routines_where(|items| items.iter().any(|v| v.routine.id == id && v.last_run.as_ref().is_some_and(|r| !r.status.active())));
        self.runs(id)
    }

    /// Give the scheduler a few ticks.
    fn settle(&mut self) {
        std::thread::sleep(Duration::from_millis(800));
    }
}

/// The loom story: asked in a chat, confirmed on a card that says it in plain words, run by
/// the scheduler at 8:00 in a chat filed under the routine, and the report reaches the client.
/// The next day's run finds nothing new and stays quiet, knowing what was reported before.
/// Instructions longer than a card line.
const LOOM: &str = "Search the web for deals on a new floor loom (new, not used). Compare with what earlier runs found and only report a lower price or a model we haven't seen before; skip kits without the loom itself, and shops that don't ship here.";

#[test]
fn the_model_makes_a_routine_the_scheduler_runs_it_and_its_report_arrives() {
    let time = Time::new("2026-11-24 07:00");
    let mut w = time.world();
    w.model.calls("create_routine", json!({"name": "Loom deals", "instructions": LOOM, "schedule": {"rule": "FREQ=DAILY", "at": "08:00", "zone": "UTC"}}));
    w.model.answers(|r| {
        assert!(r.last_result().contains("Created the routine \"Loom deals\""), "{}", r.last_result());
        Answer::text("Done: every day at 8:00.")
    });
    w.tell(ClientMsg::SendMessage { text: "Every morning, search the web for a deal on a new loom.".into(), attachments: vec![] });
    let card = w.wait(|m| match m {
        ServerMsg::StepUpdated { step, .. } if step.tool == "create_routine" && step.status == StepStatus::Waiting => Some(step.clone()),
        _ => None,
    });
    // A must-confirm card, in plain words.
    let ask = card.ask.as_ref().expect("a must-confirm card");
    assert!(ask.always && ask.warning.as_deref().is_some_and(|w| w.contains("runs later by itself")), "{ask:?}");
    assert_eq!(card.title, "Create the routine Loom deals");
    let Some(Preview::Args { fields, .. }) = &card.preview else { panic!("no card fields: {:?}", card.preview) };
    let field = |k: &str| fields.iter().find(|(f, _)| f == k).map(|(_, v)| v.as_str()).unwrap_or_else(|| panic!("no {k} in {fields:?}"));
    // Plain words only, in order; the instructions whole; no raw arguments after them.
    let keys: Vec<&str> = fields.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, ["Routine", "When", "Does", "It can", "It can't", "Tells you", "If byob is closed"], "{fields:?}");
    assert_eq!(field("When"), "Every day at 8:00 AM (UTC)");
    assert_eq!(field("Does"), LOOM);
    assert_eq!(field("It can"), "Search and read the web, and read files");
    assert!(field("It can't").starts_with("Change files"), "{}", field("It can't"));
    assert_eq!(field("Tells you"), "Only when it finds something new: a notification");
    assert_eq!(field("If byob is closed"), "It runs once when byob is back");
    let words = ask.card.as_ref().expect("the tool's own card words");
    assert_eq!((words.access.as_str(), words.allow.as_deref(), words.deny.as_deref()), ("Sets up a routine", Some("Create routine"), Some("Not now")));
    w.tell(ClientMsg::Approve { step: card.id, answer: ApprovalAnswer::Allow });
    let items = w.routines_where(|items| items.len() == 1);
    let r = items[0].routine.clone();
    assert_eq!((r.id.as_str(), r.allowed), ("loom-deals", byob_types::config::Approvals::ReadOnly));
    assert!(matches!(&r.when[0], Trigger::Schedule { start: Some(s), zone: Some(z), .. } if s == "2026-11-24" && z == "UTC"), "{:?}", r.when);
    assert_eq!(items[0].next_run, Some(ms("2026-11-24 08:00")));
    assert_eq!(items[0].schedule, ["Every day at 8:00 AM"]);
    w.wait(|m| matches!(m, ServerMsg::EntryFinished { .. }).then_some(()));
    assert!(w.runs("loom-deals").is_empty(), "nothing ran before 8:00");

    // 8:00: the run, told it's unattended, with the report tool.
    w.model.answers(|r| {
        let text = r.user_texts().join("\n");
        assert!(text.contains("unattended run of the routine \"Loom deals\"") && text.contains("Nobody is watching"), "{text}");
        assert!(text.contains("Search the web for deals on a new floor loom"), "{text}");
        assert!(r.tools().contains(&"report".to_owned()), "{:?}", r.tools());
        Answer::call("report", json!({"title": "Schacht Wolf Pup 8S, 20% off at The Woolery", "body": "$1,040 until Sunday."}))
    });
    w.model.says("Reported the Wolf Pup.");
    time.set("2026-11-24 08:00");
    let (routine, chat, title, body) = w.event(RoutineEventKind::Reported);
    assert_eq!((routine.as_str(), title.as_str(), body.as_str()), ("loom-deals", "Schacht Wolf Pup 8S, 20% off at The Woolery", "$1,040 until Sunday."));
    let chat = chat.expect("the run's chat");
    let runs = w.finished("loom-deals");
    assert_eq!(runs.len(), 1);
    assert_eq!((runs[0].status, runs[0].chat.as_deref(), runs[0].slot), (RunStatus::Reported, Some(chat.as_str()), Some(ms("2026-11-24 08:00"))));
    assert_eq!(runs[0].report.as_ref().unwrap().title, "Schacht Wolf Pup 8S, 20% off at The Woolery");
    // The run's chat is filed under the routine, and the client still looks at its own chat.
    let row = w.store().chat(&chat).unwrap().unwrap();
    assert_eq!(row.origin.as_deref(), Some("loom-deals"));
    assert!(row.title.starts_with("Loom deals · Tue Nov 24"), "{}", row.title);
    assert_ne!(w.chat_id(), chat);
    assert_eq!(w.seen.listed().iter().find(|c| c.id == chat).and_then(|c| c.origin.as_deref()), Some("loom-deals"));

    // The next morning: nothing new. No event; the model knew what was reported before.
    w.model.answers(|r| {
        let text = r.user_texts().join("\n");
        assert!(text.contains("- 2026-11-24: Schacht Wolf Pup 8S") && text.contains("The last one in full:\n$1,040 until Sunday."), "{text}");
        Answer::text("Checked three shops: nothing new.")
    });
    time.set("2026-11-25 08:00");
    let runs = w.routines_where(|items| items[0].last_run.as_ref().is_some_and(|r| r.status == RunStatus::Quiet));
    assert_eq!(runs[0].next_run, Some(ms("2026-11-26 08:00")));
    let runs = w.runs("loom-deals");
    assert_eq!(runs.iter().map(|r| r.status).collect::<Vec<_>>(), [RunStatus::Quiet, RunStatus::Reported]);
    assert_eq!(w.model.unused(), 0);
}

/// A read-only routine's run wants to write a file: it waits for a person (the routine says
/// so, the client hears), and after 12 hours with no answer it's denied and the model is told
/// nobody answered.
#[test]
fn a_run_waiting_for_an_approval_nobody_gives_is_denied_after_12_hours() {
    let time = Time::new("2026-11-24 07:00");
    let mut w = time.world();
    w.save(daily("Notes", "08:00"));
    w.model.calls("write_file", json!({"path": "found.md", "content": "a loom"}));
    w.model.answers(|r| {
        assert!(r.last_result().contains(NOBODY_ANSWERED), "{}", r.last_result());
        Answer::text("Couldn't save it.")
    });
    time.set("2026-11-24 08:00");
    let items = w.routines_where(|items| items[0].last_run.as_ref().is_some_and(|r| r.status == RunStatus::Waiting));
    let (routine, chat, title, body) = w.event(RoutineEventKind::Waiting);
    assert_eq!((routine.as_str(), title.as_str()), ("notes", "Notes is waiting for you"));
    assert!(body.contains("found.md"), "{body}");
    assert_eq!(items[0].last_run.as_ref().unwrap().chat, chat);
    // 11 hours: still waiting.
    time.set("2026-11-24 19:00");
    w.settle();
    assert_eq!(w.runs("notes")[0].status, RunStatus::Waiting);
    time.set("2026-11-24 20:00");
    let runs = w.finished("notes");
    assert_eq!(runs[0].status, RunStatus::Quiet);
    assert!(w.read("found.md").is_none());
    assert_eq!(w.model.unused(), 0, "the model was told and went on");
}

/// Three days asleep: one run (for the latest morning), not three. A restart doesn't run it
/// again; the next morning does.
#[test]
fn missed_mornings_run_once_and_a_restart_doesnt_run_again() {
    let time = Time::new("2026-11-20 07:00");
    let mut w = time.world();
    w.save(daily("Deals", "08:00"));
    w.model.says("Nothing new.");
    time.set("2026-11-23 09:30");
    let runs = w.finished("deals");
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].slot, Some(ms("2026-11-23 08:00")));
    w.settle();
    assert_eq!(w.runs("deals").len(), 1);
    w.restart();
    w.settle();
    assert_eq!(w.runs("deals").len(), 1, "a restart doesn't run it again");
    assert_eq!(w.model.unused(), 0);
    w.model.says("Still nothing.");
    time.set("2026-11-24 08:01");
    let runs = w.finished("deals");
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].slot, Some(ms("2026-11-24 08:00")));
}

/// A routine that's off never runs, however long; turned on, it starts from then (nothing
/// "missed" while it was off). Run now works either way.
#[test]
fn a_routine_thats_off_never_runs() {
    let time = Time::new("2026-11-20 07:00");
    let mut w = time.world();
    let r = w.save(Routine { enabled: false, ..daily("Quiet one", "08:00") });
    let state = w.wait(|m| match m {
        ServerMsg::State(s) => Some(s.routines_active),
        _ => None,
    });
    assert!(!state, "no routine is on");
    time.set("2026-11-25 09:00");
    w.settle();
    assert!(w.runs(&r.id).is_empty());
    w.tell(ClientMsg::SetRoutineEnabled { id: r.id.clone(), enabled: true });
    w.wait(|m| match m {
        ServerMsg::State(s) => s.routines_active.then_some(()),
        _ => None,
    });
    w.settle();
    assert!(w.runs(&r.id).is_empty(), "turning it on doesn't make up the days it was off");
    w.model.says("Ran by hand.");
    w.tell(ClientMsg::RunRoutine { id: r.id.clone() });
    let runs = w.finished(&r.id);
    assert_eq!((runs.len(), runs[0].cause), (1, byob_types::routine::RunCause::Manual));
}

/// Three routines due at once: two run, the third waits its turn, then all three are done.
#[test]
fn at_most_two_routine_runs_go_at_once() {
    let time = Time::new("2026-11-20 07:00");
    let mut w = time.world();
    for name in ["One", "Two", "Three"] {
        w.save(daily(name, "08:00"));
    }
    for _ in 0..3 {
        w.model.answers(|_| Answer::text("still looking around ".repeat(8)).slowly(Duration::from_millis(40)));
    }
    time.set("2026-11-20 08:00");
    let mut most = 0;
    let mut queued = false;
    w.routines_where(|items| {
        let running = items.iter().filter(|v| v.last_run.as_ref().is_some_and(|r| matches!(r.status, RunStatus::Running | RunStatus::Waiting))).count();
        most = most.max(running);
        queued |= items.iter().any(|v| v.last_run.as_ref().is_some_and(|r| r.status == RunStatus::Queued));
        items.iter().all(|v| v.last_run.as_ref().is_some_and(|r| r.status == RunStatus::Quiet))
    });
    assert_eq!(most, 2);
    assert!(queued, "the third waited");
    for id in ["one", "two", "three"] {
        assert_eq!(w.runs(id).len(), 1, "{id}");
    }
}

/// The model lists routines and runs one by hand; a routine that keeps one run deletes the
/// older run's chat when the next ends, and keeps its report.
#[test]
fn run_by_the_model_and_old_run_chats_go_but_their_reports_stay() {
    let time = Time::new("2026-11-20 07:00");
    let mut w = time.world();
    let r = w.save(Routine { keep: 1, when: vec![], ..daily("Looms", "08:00") });
    w.model.calls("list_routines", json!({}));
    w.model.answers(|r| {
        assert!(r.last_result().contains("- looms (Looms): when asked, on"), "{}", r.last_result());
        Answer::call("run_routine", json!({"id": "looms"}))
    });
    // The run starts while the chat goes on: its requests and the chat's come in either order.
    for _ in 0..3 {
        w.model.answers(|r| {
            if !r.user_texts().join("\n").contains("unattended run") {
                assert!(r.last_result().starts_with("Started a run of \"Looms\""), "{}", r.last_result());
                Answer::text("Started it.")
            } else if r.tool_results().is_empty() {
                Answer::call("report", json!({"title": "A loom", "body": "Cheap."}))
            } else {
                Answer::text("Reported.")
            }
        });
    }
    let turn = w.send("run my loom routine");
    turn.step("run_routine").done();
    w.event(RoutineEventKind::Reported);
    let first = w.finished(&r.id);
    let old_chat = first[0].chat.clone().unwrap();
    w.model.says("Nothing new.");
    w.tell(ClientMsg::RunRoutine { id: r.id.clone() });
    w.wait(|m| matches!(m, ServerMsg::ChatDeleted { id } if *id == old_chat).then_some(()));
    let runs = w.finished(&r.id);
    assert_eq!(runs.len(), 2);
    assert_eq!((runs[0].status, runs[0].chat.is_some()), (RunStatus::Quiet, true));
    assert_eq!((runs[1].chat.as_deref(), runs[1].report.as_ref().map(|r| r.title.as_str())), (None, Some("A loom")));
    assert!(w.store().chat(&old_chat).unwrap().is_none());
}

/// "Always" on a run's card is for the routine: its later runs (new chats) make that call
/// without asking. A routine deleted while its run goes leaves no run log behind.
#[test]
fn always_on_a_runs_card_lets_the_routines_later_runs_go_on() {
    let time = Time::new("2026-11-24 07:00");
    let mut w = time.world();
    let r = w.save(Routine { when: vec![], ..daily("Notes", "08:00") });
    w.model.calls("write_file", json!({"path": "found.md", "content": "a loom"}));
    w.model.says("Saved.");
    w.tell(ClientMsg::RunRoutine { id: r.id.clone() });
    let (_, chat, _, _) = w.event(RoutineEventKind::Waiting);
    let chat = chat.unwrap();
    // The person opens the run (the notification would) and answers its card.
    w.open_chat(&chat);
    let card =
        w.seen.entries.iter().flat_map(|e| &e.steps).find(|s| s.tool == "write_file" && s.status == StepStatus::Waiting).map(|s| s.id).expect("the card");
    w.tell(ClientMsg::Approve { step: card, answer: ApprovalAnswer::Always });
    let runs = w.finished(&r.id);
    assert_eq!(runs[0].status, RunStatus::Quiet);
    let folder = |w: &World, chat: &str| w.store().chat(chat).unwrap().unwrap().workspace;
    assert!(folder(&w, &chat).join("found.md").exists());
    assert!(w.read("found.md").is_none(), "the run works in a folder of its own, not the project");
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(w.home.path().join("routines.json")).unwrap()).unwrap();
    assert_eq!(saved["routines"][0]["grants"], json!(["write_file"]));
    // The next run writes without a card.
    w.model.calls("write_file", json!({"path": "again.md", "content": "another loom"}));
    w.model.says("Saved again.");
    w.tell(ClientMsg::RunRoutine { id: r.id.clone() });
    let runs = w.finished(&r.id);
    assert_eq!((runs.len(), runs[0].status), (2, RunStatus::Quiet));
    let second = runs[0].chat.clone().unwrap();
    assert!(folder(&w, &second).join("again.md").exists());
    // Deleted mid-run: no log comes back.
    w.model.answers(|_| Answer::text("slow ".repeat(20)).slowly(Duration::from_millis(30)));
    w.tell(ClientMsg::RunRoutine { id: r.id.clone() });
    w.routines_where(|items| items.iter().any(|v| v.last_run.as_ref().is_some_and(|x| x.status == RunStatus::Running)));
    w.tell(ClientMsg::DeleteRoutine { id: r.id.clone() });
    w.routines_where(<[RoutineView]>::is_empty);
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(w.model.unused(), 0);
    assert!(!w.home.path().join("routine-runs").join(format!("{}.json", r.id)).exists());
}

/// Pause routines (the tray): nothing runs on schedule while paused, even across a restart;
/// resumed, the slots that passed aren't made up, and the next one runs. Run now still works.
#[test]
fn paused_routines_skip_their_slots_and_resume_without_making_them_up() {
    let time = Time::new("2026-11-20 07:00");
    let mut w = time.world();
    let r = w.save(daily("Deals", "08:00"));
    w.tell(ClientMsg::PauseRoutines { paused: true });
    w.wait(|m| match m {
        ServerMsg::State(s) => (s.routines_paused == Some(true)).then_some(()),
        _ => None,
    });
    time.set("2026-11-20 09:00");
    w.settle();
    assert!(w.runs(&r.id).is_empty(), "paused: the 8:00 slot passes");
    let settings = Paths::at(w.home.path()).settings();
    assert!(byob_store::Settings::load(&settings).value.routines_paused, "saved in settings.json");
    w.restart();
    time.set("2026-11-21 09:00");
    w.settle();
    assert!(w.runs(&r.id).is_empty());
    w.model.says("Ran by hand.");
    w.tell(ClientMsg::RunRoutine { id: r.id.clone() });
    assert_eq!(w.finished(&r.id).len(), 1, "Run now works while paused");
    w.tell(ClientMsg::PauseRoutines { paused: false });
    w.wait(|m| match m {
        ServerMsg::State(s) => (s.routines_paused == Some(false)).then_some(()),
        _ => None,
    });
    w.settle();
    assert_eq!(w.runs(&r.id).len(), 1, "resuming doesn't make up the paused mornings");
    w.model.says("Nothing new.");
    time.set("2026-11-22 08:01");
    let runs = w.finished(&r.id);
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].slot, Some(ms("2026-11-22 08:00")));
}
