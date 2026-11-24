//! The routine tools (named by the bundled `routines` plugin): `create_routine`,
//! `update_routine` and `delete_routine` (must confirm: a routine acts later without you, so
//! the card says in plain words when it runs, what it may do and when it tells you),
//! `list_routines` (read) and `run_routine` (write). They ask the engine over its inbox
//! ([`crate::Input::Routine`]). And `report`, the tool only routine runs get.
//!
//! Arguments are model output: every field is checked here or by
//! [`byob_types::routine::Routine::validated`] and the engine's schedule check.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use byob_tools::{Access, Preview, Tool, ToolContext, ToolOutput, ToolSpec};
use byob_types::config::Approvals;
use byob_types::routine::{Missed, Report, Routine, Tell, Trigger};
use byob_when::Schedule;
use serde_json::{Value, json};

use crate::Input;
use crate::routines::{Op, allowed_words, tell_words};

/// How long a tool waits for the engine.
const TIMEOUT: Duration = Duration::from_secs(10);

pub const CREATE: &str = "create_routine";
pub const UPDATE: &str = "update_routine";
pub const DELETE: &str = "delete_routine";
pub const LIST: &str = "list_routines";
pub const RUN: &str = "run_routine";

/// Ask the engine; waits (while not stopped) up to [`TIMEOUT`].
fn request(inbox: &Sender<Input>, chat: Option<&str>, op: Op, cancel: &AtomicBool) -> Result<String, String> {
    let (reply, answer) = channel();
    inbox.send(Input::Routine { chat: chat.map(str::to_owned), op, reply }).map_err(|_| "byob is shutting down".to_owned())?;
    let started = Instant::now();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("stopped".into());
        }
        match answer.recv_timeout(Duration::from_millis(50)) {
            Ok(r) => return r,
            Err(RecvTimeoutError::Timeout) if started.elapsed() < TIMEOUT => {}
            Err(_) => return Err("byob didn't answer in time".into()),
        }
    }
}

fn output(r: Result<String, String>) -> ToolOutput {
    match r {
        Ok(t) => ToolOutput::ok(t),
        Err(e) => ToolOutput::err(e),
    }
}

fn text<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

fn approvals(v: &str) -> Result<Approvals, String> {
    Ok(match v.trim().to_ascii_lowercase().replace([' ', '-'], "_").as_str() {
        "read_only" | "readonly" | "read" => Approvals::ReadOnly,
        "ask" | "ask_before_changes" => Approvals::Ask,
        "allow_edits" | "edit_freely" | "edits" => Approvals::AllowEdits,
        "allow_all" | "work_freely" | "all" => Approvals::AllowAll,
        other => return Err(format!("allowed {other:?} isn't one of read_only, ask, allow_edits, allow_all")),
    })
}

fn tell(v: &str) -> Result<Tell, String> {
    Ok(match v.trim().to_ascii_lowercase().as_str() {
        "found" | "when_found" | "report" => Tell::Found,
        "always" => Tell::Always,
        "never" => Tell::Never,
        other => return Err(format!("tell {other:?} isn't one of found, always, never")),
    })
}

fn missed(v: &str) -> Result<Missed, String> {
    Ok(match v.trim().to_ascii_lowercase().as_str() {
        "once" | "run_once" => Missed::Once,
        "skip" => Missed::Skip,
        other => return Err(format!("missed {other:?} isn't once or skip")),
    })
}

/// A `schedule` argument as a trigger; `null` is none.
fn schedule(v: &Value) -> Result<Option<Trigger>, String> {
    match v {
        Value::Null => Ok(None),
        Value::Object(_) => {
            let s = |k: &str| v.get(k).and_then(Value::as_str).map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
            let rule = s("rule").ok_or("the schedule needs a rule, like FREQ=DAILY")?;
            let at = s("at").unwrap_or_else(|| "00:00".into());
            Ok(Some(Trigger::Schedule { rule, at, zone: s("zone"), start: s("start") }))
        }
        _ => Err("schedule is an object {rule, at, zone?, start?}".into()),
    }
}

/// Apply the routine fields in `args` to `r` (create: a blank routine; update: the current one).
pub(crate) fn apply(mut r: Routine, args: &Value) -> Result<Routine, String> {
    if !args.is_object() {
        return Err("the arguments are an object".into());
    }
    if let Some(n) = args.get("name").and_then(Value::as_str) {
        n.clone_into(&mut r.name);
    }
    if let Some(i) = args.get("instructions").and_then(Value::as_str) {
        i.clone_into(&mut r.instructions);
    }
    if let Some(v) = args.get("schedule") {
        r.when.retain(|t| !matches!(t, Trigger::Schedule { .. }));
        r.when.extend(schedule(v)?);
    }
    if let Some(v) = text(args, "allowed") {
        r.allowed = approvals(v)?;
    }
    if let Some(v) = text(args, "tell") {
        r.tell = tell(v)?;
    }
    if let Some(v) = text(args, "missed") {
        r.missed = missed(v)?;
    }
    if let Some(v) = args.get("enabled").and_then(Value::as_bool) {
        r.enabled = v;
    }
    if let Some(v) = args.get("keep").and_then(Value::as_u64) {
        r.keep = u32::try_from(v).unwrap_or(u32::MAX);
    }
    if let Some(v) = args.get("model") {
        r.model = v.as_str().map(str::to_owned).filter(|s| !s.trim().is_empty());
    }
    Ok(r)
}

/// The routine's card, in plain words (`fields` of an args preview).
fn card(r: &Routine) -> Vec<(String, String)> {
    let when: Vec<String> = r
        .schedules()
        .map(|(rule, at, zone, start)| match Schedule::parse(rule, at, zone, start) {
            Ok(s) => match zone {
                Some(z) => format!("{} ({z})", s.describe()),
                None => s.describe(),
            },
            Err(e) => format!("can't be read: {e}"),
        })
        .collect();
    let mut fields = vec![
        ("Routine".to_owned(), r.name.clone()),
        ("When".to_owned(), if when.is_empty() { "Only when you run it".to_owned() } else { when.join("; ") }),
        ("Does".to_owned(), byob_tools::preview::clip_line(&r.instructions)),
        ("Can".to_owned(), allowed_words(r.allowed).to_owned()),
        ("Tells you".to_owned(), tell_words(r.tell).to_owned()),
    ];
    if r.missed == Missed::Skip {
        fields.push(("Missed runs".to_owned(), "skipped".to_owned()));
    }
    if !r.enabled {
        fields.push(("State".to_owned(), "off".to_owned()));
    }
    fields
}

/// The whole instructions, when the card's line can't hold them.
fn long_instructions(r: &Routine) -> Option<String> {
    (byob_tools::preview::clip_line(&r.instructions) != r.instructions).then(|| r.instructions.clone())
}

const WARNING: &str = "A routine runs later by itself, without asking you first. Check when it runs and what it may do.";

fn routine_params(create: bool) -> Value {
    let mut props = json!({
        "name": {"type": "string", "description": "A short name, like \"Loom deals\"."},
        "instructions": {"type": "string", "description": "What each run should do, written as a request to you (you'll read it when the run starts, with what earlier runs reported). Be specific: what to look for, what counts as new or worth reporting."},
        "schedule": {
            "type": ["object", "null"],
            "description": "When it runs on its own. Omit (or null) for a routine that only runs when asked. Turn the user's words into a time: \"every morning\" is 08:00, \"in the evening\" 18:00; the card shows it in words so a wrong guess is easy to spot.",
            "properties": {
                "rule": {"type": "string", "description": "An RRULE: FREQ=DAILY; FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR (weekdays); FREQ=WEEKLY;BYDAY=SA; FREQ=MONTHLY;BYMONTHDAY=1; FREQ=MONTHLY;BYDAY=-1FR (last Friday); FREQ=HOURLY;INTERVAL=2 (every 2 hours); FREQ=MINUTELY;INTERVAL=30. At most every 15 minutes."},
                "at": {"type": "string", "description": "24-hour HH:MM it runs at (for hourly rules: the first run's time of day)."},
                "zone": {"type": "string", "description": "IANA time zone; omit for the user's own."},
                "start": {"type": "string", "description": "First day, YYYY-MM-DD; omit for today."}
            },
            "required": ["rule", "at"]
        },
        "allowed": {"type": "string", "enum": ["read_only", "ask", "allow_edits", "allow_all"], "description": "What its runs may do without asking. Default read_only: reading files and searching and reading the web. Anything else waits for the user, so only widen it when the user asked for that."},
        "tell": {"type": "string", "enum": ["found", "always", "never"], "description": "When the user hears about a run: found (default: only runs that report something), always, never."},
        "missed": {"type": "string", "enum": ["once", "skip"], "description": "Runs missed while byob was off or asleep: once (default: made up once) or skip."},
        "keep": {"type": "integer", "description": "How many runs to keep (default 30)."},
        "model": {"type": "string", "description": "A saved model's id for its runs; omit for the default model."},
    });
    if create {
        props["in_this_project"] =
            json!({"type": "boolean", "description": "Its runs work in this chat's project (its files, its settings). Default: a folder of their own."});
        json!({"type": "object", "properties": props, "required": ["name", "instructions"]})
    } else {
        props["id"] = json!({"type": "string", "description": "The routine's id (list_routines shows them)."});
        props["enabled"] = json!({"type": "boolean", "description": "Turn its schedule on or off."});
        json!({"type": "object", "properties": props, "required": ["id"]})
    }
}

/// `create_routine`.
pub struct CreateRoutine {
    pub(crate) inbox: Sender<Input>,
}

impl Tool for CreateRoutine {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: CREATE.into(),
            description: "Save a routine: instructions byob carries out by itself on a schedule (\"every morning, look for…\"), each run a new chat. A run that finds something calls report and the user gets a notification; one that finds nothing stays quiet. The user confirms it on a card first.".into(),
            parameters: routine_params(true),
        }
    }

    fn title(&self, args: &Value) -> String {
        format!("Created the routine {}", text(args, "name").unwrap_or("(unnamed)"))
    }

    fn pending_title(&self, args: &Value) -> String {
        format!("Create the routine {}", text(args, "name").unwrap_or("(unnamed)"))
    }

    fn preview(&self, args: &Value, _workspace: &Path) -> Option<Preview> {
        let r = apply(Routine::default(), args).ok()?;
        Some(Preview::Args { code: long_instructions(&r), fields: card(&r), more: 0 })
    }

    fn access(&self, _args: &Value) -> Access {
        Access::Write
    }

    fn always_ask(&self, _args: &Value) -> Option<String> {
        Some(WARNING.into())
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        let r = match apply(Routine::default(), args) {
            Ok(r) => r,
            Err(e) => return ToolOutput::err(e),
        };
        let here = args.get("in_this_project").and_then(Value::as_bool).unwrap_or(false);
        output(request(&self.inbox, ctx.chat(), Op::Create { routine: Box::new(r), here }, ctx.cancel_flag()))
    }
}

fn id_arg(args: &Value) -> Result<String, String> {
    let id = text(args, "id").ok_or("give the routine's id (list_routines shows them)")?;
    if byob_types::routine::id_ok(id) { Ok(id.to_owned()) } else { Err(format!("{id:?} isn't a routine id")) }
}

/// `update_routine`.
pub struct UpdateRoutine {
    pub(crate) inbox: Sender<Input>,
}

impl Tool for UpdateRoutine {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: UPDATE.into(),
            description: "Change a routine: only the fields you give change (schedule: null removes its schedule). The user confirms it on a card.".into(),
            parameters: routine_params(false),
        }
    }

    fn title(&self, args: &Value) -> String {
        format!("Updated the routine {}", text(args, "id").unwrap_or("?"))
    }

    fn pending_title(&self, args: &Value) -> String {
        format!("Change the routine {}", text(args, "id").unwrap_or("?"))
    }

    fn preview(&self, args: &Value, _workspace: &Path) -> Option<Preview> {
        // Only what changes: the card can't see the rest without the engine.
        let base = Routine { name: text(args, "id").unwrap_or("?").to_owned(), when: vec![Trigger::Unknown], ..Routine::default() };
        let after = apply(base.clone(), args).ok()?;
        let fields: Vec<(String, String)> = card(&after)
            .into_iter()
            .filter(|(k, _)| match k.as_str() {
                "Routine" => true,
                "When" => args.get("schedule").is_some(),
                "Does" => args.get("instructions").is_some(),
                "Can" => args.get("allowed").is_some(),
                "Tells you" => args.get("tell").is_some(),
                _ => true,
            })
            .collect();
        let mut fields = fields;
        if let Some(n) = text(args, "name") {
            fields.push(("New name".into(), n.to_owned()));
        }
        if let Some(on) = args.get("enabled").and_then(Value::as_bool) {
            fields.push(("Schedule".into(), if on { "on" } else { "off" }.into()));
        }
        Some(Preview::Args { fields, code: None, more: 0 })
    }

    fn access(&self, _args: &Value) -> Access {
        Access::Write
    }

    fn always_ask(&self, _args: &Value) -> Option<String> {
        Some(WARNING.into())
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        let id = match id_arg(args) {
            Ok(id) => id,
            Err(e) => return ToolOutput::err(e),
        };
        let args = args.clone();
        let op = Op::Update { id, apply: Box::new(move |r| apply(r, &args)) };
        output(request(&self.inbox, ctx.chat(), op, ctx.cancel_flag()))
    }
}

/// `delete_routine`.
pub struct DeleteRoutine {
    pub(crate) inbox: Sender<Input>,
}

impl Tool for DeleteRoutine {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: DELETE.into(),
            description: "Delete a routine (its run chats stay). The user confirms it.".into(),
            parameters: json!({"type": "object", "properties": {"id": {"type": "string", "description": "The routine's id."}}, "required": ["id"]}),
        }
    }

    fn title(&self, args: &Value) -> String {
        format!("Deleted the routine {}", text(args, "id").unwrap_or("?"))
    }

    fn pending_title(&self, args: &Value) -> String {
        format!("Delete the routine {}", text(args, "id").unwrap_or("?"))
    }

    fn access(&self, _args: &Value) -> Access {
        Access::Write
    }

    fn always_ask(&self, _args: &Value) -> Option<String> {
        Some("It stops running for good.".into())
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        match id_arg(args) {
            Ok(id) => output(request(&self.inbox, ctx.chat(), Op::Delete { id }, ctx.cancel_flag())),
            Err(e) => ToolOutput::err(e),
        }
    }
}

/// `list_routines`.
pub struct ListRoutines {
    pub(crate) inbox: Sender<Input>,
}

impl Tool for ListRoutines {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: LIST.into(),
            description: "List the routines: id, name, schedule in words, on or off, next run, the last run and what it reported.".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    fn title(&self, _args: &Value) -> String {
        "Listed the routines".into()
    }

    fn pending_title(&self, _args: &Value) -> String {
        "List the routines".into()
    }

    fn access(&self, _args: &Value) -> Access {
        Access::Read
    }

    fn repeatable(&self, _args: &Value) -> bool {
        true
    }

    fn run(&self, _args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        output(request(&self.inbox, ctx.chat(), Op::List, ctx.cancel_flag()))
    }
}

/// `run_routine`.
pub struct RunRoutine {
    pub(crate) inbox: Sender<Input>,
}

impl Tool for RunRoutine {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: RUN.into(),
            description: "Run a routine now, in a new chat of its own (it doesn't change its schedule). Returns at once; the run reports by itself.".into(),
            parameters: json!({"type": "object", "properties": {"id": {"type": "string", "description": "The routine's id."}}, "required": ["id"]}),
        }
    }

    fn title(&self, args: &Value) -> String {
        format!("Started the routine {}", text(args, "id").unwrap_or("?"))
    }

    fn pending_title(&self, args: &Value) -> String {
        format!("Run the routine {}", text(args, "id").unwrap_or("?"))
    }

    fn access(&self, _args: &Value) -> Access {
        Access::Write
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        match id_arg(args) {
            Ok(id) => output(request(&self.inbox, ctx.chat(), Op::Run { id }, ctx.cancel_flag())),
            Err(e) => ToolOutput::err(e),
        }
    }
}

/// `report`: only in routine runs.
pub struct ReportTool {
    pub(crate) inbox: Sender<Input>,
}

impl Tool for ReportTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: crate::routines::REPORT_TOOL.into(),
            description: "Tell the user what this routine run found: they get a notification with the title, and the body is kept with the run. Only for something worth interrupting them for (new, relevant); with nothing new, don't call it. Call it once, at the end; a later call replaces the earlier one.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string", "description": "One line, the news itself: \"Schacht Wolf Pup 8S, 20% off at The Woolery ($1,040, until Sunday)\"."},
                    "body": {"type": "string", "description": "The details: what, where, prices, links, why it matters, what you compared it with."}
                },
                "required": ["title"]
            }),
        }
    }

    fn title(&self, args: &Value) -> String {
        format!("Reported: {}", crate::limits::prefix(text(args, "title").unwrap_or_default(), 120))
    }

    fn pending_title(&self, _args: &Value) -> String {
        "Report".into()
    }

    /// Its own output: a read-only routine reports too.
    fn access(&self, _args: &Value) -> Access {
        Access::Read
    }

    fn repeatable(&self, _args: &Value) -> bool {
        true
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        let Some(title) = text(args, "title") else { return ToolOutput::err("a report needs a title") };
        let Some(chat) = ctx.chat() else { return ToolOutput::err("report only works in a routine's run") };
        let report = Report::clean(title, text(args, "body").unwrap_or_default());
        match self.inbox.send(Input::Reported { chat: chat.to_owned(), report }) {
            Ok(()) => ToolOutput::ok("Reported: the user gets a notification with the title. End the run now with a one-line summary."),
            Err(_) => ToolOutput::err("byob is shutting down"),
        }
    }
}
