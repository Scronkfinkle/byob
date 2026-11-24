//! Routines: saved instructions the engine carries out by itself (see
//! [`byob_types::routine`]). This module owns `routines.json` and the run logs, the
//! scheduler, starting runs, the unattended approval timeout and run bookkeeping.
//!
//! - **Scheduler.** Checked on the engine's 250 ms tick against [`Engine::now_ms`] (an
//!   injectable clock): when the earliest due time has passed, each enabled routine whose
//!   schedule had a slot since the last one it handled (`RunLog::last_slot`, persisted when a
//!   run starts) is due. However many slots passed (sleep, byob closed), it runs once, for
//!   the latest; `missed: skip` routines only run a slot that's at most [`ON_TIME_MS`] old.
//! - **Runs.** A run is a new chat with `origin` = the routine (in the routine's project, else
//!   a folder of its own), its model profile as the chat's model, the instructions plus earlier
//!   reports as the message, a note that nobody is watching, and the `report` tool. One run
//!   per routine at a time (a slot due while one goes is recorded as skipped); at most
//!   [`MAX_RUNNING`] at once, the others wait in order.
//! - **Unattended approvals.** A run may do what its routine allows without asking;
//!   anything else waits for a person (never refused outright: that's the point of asking).
//!   The routine shows "Waiting for you" and clients get a [`ServerMsg::RoutineEvent`].
//!   Unanswered after [`APPROVAL_TIMEOUT_MS`] the call is denied with [`NOBODY_ANSWERED`].
//! - **Endings.** `finish_turn` hands the run here: its record gets the status (reported,
//!   quiet, failed, stopped), clients get the event, backends `run_finished`, and run chats
//!   beyond the routine's `keep` are deleted (their reports stay in the log).

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use byob_protocol::ServerMsg;
use byob_protocol::routines::{RoutineEventKind, RoutineView};
use byob_store::{Paths, RunLog, RunLogs};
use byob_types::config::{Approvals, Layer};
use byob_types::routine::{self, Missed, Report, Routine, RunCause, RunRecord, RunStatus, Tell, Trigger};
use byob_when::Schedule;
use chrono::{DateTime, TimeZone, Utc};

use crate::permissions::{Policy, Rule};
use crate::{ClientId, Engine};

/// The engine's idea of now. Tests move it to make routines due.
pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// The wall clock.
pub fn system_clock() -> Clock {
    Arc::new(Utc::now)
}

/// Most routine runs going at once; due ones wait in order.
pub const MAX_RUNNING: usize = 2;
/// How long a run waits for a person to answer an approval card before it's denied.
pub const APPROVAL_TIMEOUT_MS: i64 = 12 * 60 * 60 * 1000;
/// A `missed: skip` routine still runs a slot this old (ms): byob was busy, not away.
pub const ON_TIME_MS: i64 = 15 * 60 * 1000;
/// Most slots walked to find the latest one due (a 15-minute schedule over a month).
const MAX_SLOT_STEPS: usize = 4_000;
/// Earlier report titles put in a run's message.
const EARLIER_TITLES: usize = 10;
/// The run's own tool.
pub const REPORT_TOOL: &str = "report";

/// What the model is told when nobody answered its approval card in time.
pub const NOBODY_ANSWERED: &str = "nobody answered within 12 hours (this is an unattended routine run), so it didn't run. Don't try it again in this run: carry on without it, and if the user needs to do it themselves, say so with report.";
/// A run that was going when byob stopped.
const STOPPED_BY_QUIT: &str = "byob stopped before this run finished";

fn ms(t: DateTime<Utc>) -> i64 {
    t.timestamp_millis()
}

fn at_ms(t: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_millis_opt(t).single()
}

/// A routine's schedules, read (one that can't be read says why).
pub(crate) fn schedules(r: &Routine) -> Vec<Result<Schedule, String>> {
    r.schedules().map(|(rule, at, zone, start)| Schedule::parse(rule, at, zone, start)).collect()
}

/// What the scheduler does about a routine now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Due {
    /// Run it for this slot (the latest that's due).
    Run(i64),
    /// Its slots passed while byob wasn't running and it skips missed runs: just note the
    /// latest as handled.
    Pass(i64),
}

/// Whether `r` is due at `now`, given the last slot it handled (`None`: none since it was
/// made). Only enabled routines are ever due.
pub(crate) fn due(r: &Routine, last: Option<i64>, now: i64) -> Option<Due> {
    if !r.enabled {
        return None;
    }
    let base = at_ms(last.unwrap_or(r.created_at))?;
    let now_t = at_ms(now)?;
    let mut latest: Option<DateTime<Utc>> = None;
    for s in schedules(r).into_iter().flatten() {
        let Some(mut slot) = s.next_after(base).filter(|t| *t <= now_t) else { continue };
        for _ in 0..MAX_SLOT_STEPS {
            match s.next_after(slot) {
                Some(next) if next <= now_t => slot = next,
                _ => break,
            }
        }
        latest = latest.max(Some(slot));
    }
    let latest = ms(latest?);
    Some(match r.missed {
        Missed::Skip if now.saturating_sub(latest) > ON_TIME_MS => Due::Pass(latest),
        _ => Due::Run(latest),
    })
}

/// When `r` is next due (it may be now or past, if due and not started yet).
pub(crate) fn next_due(r: &Routine, last: Option<i64>) -> Option<i64> {
    if !r.enabled {
        return None;
    }
    let base = at_ms(last.unwrap_or(r.created_at))?;
    schedules(r).into_iter().flatten().filter_map(|s| s.next_after(base)).min().map(ms)
}

/// The rules for a run of a routine allowed `allowed`: what it allows runs, everything
/// else asks (a run never refuses outright; a person may say yes).
pub fn run_policy(allowed: Approvals) -> Policy {
    let p = Policy::for_approvals(allowed);
    let ask = |r: Rule| if r == Rule::Deny { Rule::Ask } else { r };
    Policy { read: ask(p.read), write: ask(p.write), network: ask(p.network), execute: ask(p.execute), yolo: p.yolo }
}

/// What a routine may do, in words (the creation card's "Can:").
pub fn allowed_words(a: Approvals) -> &'static str {
    match a {
        Approvals::ReadOnly => "read files and search and read the web. Anything else (changing files, running commands, sending things) waits for you.",
        Approvals::Ask => "read files and the web. Changing files, running commands and sending things wait for you.",
        Approvals::AllowEdits => "read and change files, and read the web. Running commands and sending things wait for you.",
        Approvals::AllowAll => "do anything without asking, except what always needs a person (like sending email).",
        Approvals::Yolo => "do anything without asking, even what normally always needs a person.",
    }
}

/// When a routine tells the user, in words.
pub fn tell_words(t: Tell) -> &'static str {
    match t {
        Tell::Found => "only when a run finds something to report",
        Tell::Always => "after every run",
        Tell::Never => "never (reports are kept with the routine)",
    }
}

/// Its schedules in words, one per schedule.
pub(crate) fn described(r: &Routine) -> Vec<String> {
    schedules(r).into_iter().map(|s| s.map_or_else(|e| format!("Unreadable schedule: {e}"), |s| s.describe())).collect()
}

/// A run's chat title: `Loom deals · Mon Nov 24, 8:00 AM`.
fn run_title(r: &Routine, now: DateTime<Utc>) -> String {
    let zone = schedules(r).into_iter().flatten().next().map(|s| s.zone()).unwrap_or_default();
    let local = zone.local(now);
    let name: String = r.name.chars().take(60).collect();
    format!("{name} · {}", local.format("%a %b %-d, %-I:%M %p"))
}

/// The note the model gets before a run's message (not shown in the transcript).
fn run_note(r: &Routine, cause: RunCause, now: DateTime<Utc>) -> String {
    let zone = schedules(r).into_iter().flatten().next().map(|s| s.zone()).unwrap_or_default();
    let when = byob_when::time::show(now, &zone);
    let why = match cause {
        RunCause::Manual => "someone pressed Run now",
        RunCause::Plugin => "a plugin's trigger fired",
        RunCause::Schedule | RunCause::Unknown => "its schedule",
    };
    format!(
        "[This is an unattended run of the routine \"{}\", started {when} ({why}). Nobody is watching this chat: don't ask questions, nobody will answer them. Do what the instructions below say. If you find something worth interrupting the user for, call `report` once with a short title and the details (what, where, links, why it matters). If there's nothing new, don't call `report`: end with one line saying what you checked. Calls this routine isn't allowed to make wait for a person, possibly for hours; avoid them unless they're the point.]",
        r.name
    )
}

/// The run's message: the instructions, then what earlier runs reported.
fn run_message(r: &Routine, log: Option<&RunLog>) -> String {
    let mut text = r.instructions.clone();
    let reports: Vec<(&RunRecord, &Report)> =
        log.map(|l| l.runs.iter().rev().filter_map(|run| run.report.as_ref().map(|rep| (run, rep))).collect()).unwrap_or_default();
    if let Some((_, last)) = reports.first() {
        text.push_str("\n\nEarlier reports of this routine (newest first), so you can tell what's new:");
        for (run, rep) in reports.iter().take(EARLIER_TITLES) {
            let day = at_ms(run.started).map(|t| t.format("%Y-%m-%d").to_string()).unwrap_or_default();
            text.push_str(&format!("\n- {day}: {}", rep.title));
        }
        if !last.body.is_empty() {
            text.push_str(&format!("\n\nThe last one in full:\n{}", last.body));
        }
    }
    text
}

/// A due run waiting for a free slot.
#[derive(Debug, Clone)]
struct Waiting {
    routine: String,
    cause: RunCause,
    slot: Option<i64>,
}

/// A run going now.
#[derive(Debug, Clone)]
pub(crate) struct Active {
    pub(crate) routine: String,
    pub(crate) report: Option<Report>,
    /// Approval cards of the run waiting for a person.
    pub(crate) waiting: usize,
}

/// Routines, their run logs and the runs going now.
pub(crate) struct Routines {
    pub(crate) list: byob_store::Routines,
    pub(crate) logs: RunLogs,
    pub(crate) clock: Clock,
    queue: VecDeque<Waiting>,
    /// Runs going, by chat.
    pub(crate) active: BTreeMap<String, Active>,
    /// The earliest time something is due (ms); `None`: nothing scheduled.
    wake_at: Option<i64>,
}

impl Routines {
    pub(crate) fn open(paths: Option<&Paths>, clock: Clock) -> Self {
        let list = byob_store::Routines::open(paths).value;
        let ids: Vec<String> = list.items().iter().map(|r| r.id.clone()).collect();
        let mut logs = RunLogs::open(paths, ids.iter().map(String::as_str)).value;
        let now = ms(clock());
        // Runs that were going when byob stopped are over.
        for id in &ids {
            if logs.get(id).is_some_and(|l| l.runs.iter().any(|r| r.status.active())) {
                let fixed = logs.update(id, |l| {
                    for r in l.runs.iter_mut().filter(|r| r.status.active()) {
                        r.status = RunStatus::Stopped;
                        r.finished = Some(now);
                        r.error.get_or_insert_with(|| STOPPED_BY_QUIT.to_owned());
                    }
                });
                if let Err(e) = fixed {
                    log::warn!("routine {id}: couldn't close its interrupted run: {e}");
                }
            }
        }
        let mut r = Self { list, logs, clock, queue: VecDeque::new(), active: BTreeMap::new(), wake_at: None };
        r.refresh_wake();
        r
    }

    fn last_slot(&self, id: &str) -> Option<i64> {
        self.logs.get(id).and_then(|l| l.last_slot)
    }

    fn refresh_wake(&mut self) {
        self.wake_at = self.list.items().iter().filter_map(|r| next_due(r, self.last_slot(&r.id))).min();
    }

    fn running(&self, routine: &str) -> bool {
        self.active.values().any(|a| a.routine == routine)
    }

    fn queued(&self, routine: &str) -> bool {
        self.queue.iter().any(|w| w.routine == routine)
    }

    /// Some routine is on and has a schedule (closing the window should keep byob running).
    pub(crate) fn any_scheduled(&self) -> bool {
        self.list.items().iter().any(|r| r.enabled && r.when.iter().any(|t| matches!(t, Trigger::Schedule { .. })))
    }

    fn view(&self, r: &Routine, now: i64) -> RoutineView {
        let last = self.last_slot(&r.id);
        let scheduled = schedules(r).into_iter().flatten().next().map(|s| s.runs_in_30_days(at_ms(now).unwrap_or_default()));
        let mut last_run = self.logs.get(&r.id).and_then(|l| l.runs.last().cloned());
        if self.queued(&r.id) && !self.running(&r.id) {
            last_run = Some(RunRecord { status: RunStatus::Queued, started: now, ..RunRecord::default() });
        }
        RoutineView { routine: r.clone(), schedule: described(r), next_run: next_due(r, last), runs_per_month: scheduled, last_run }
    }

    pub(crate) fn views(&self) -> Vec<RoutineView> {
        let now = ms((self.clock)());
        let mut items: Vec<RoutineView> = self.list.items().iter().map(|r| self.view(r, now)).collect();
        items.sort_by_key(|a| a.routine.name.to_lowercase());
        items
    }
}

/// What a tool asks of the routines (see `routine_tools`).
pub(crate) enum Op {
    /// A new routine (validated, no id yet); in the asking chat's project if `here`.
    Create {
        routine: Box<Routine>,
        here: bool,
    },
    /// Change fields of a routine (`apply` gets the current one).
    Update {
        id: String,
        apply: Box<dyn FnOnce(Routine) -> Result<Routine, String> + Send>,
    },
    Delete {
        id: String,
    },
    List,
    Run {
        id: String,
    },
}

impl Engine {
    pub(crate) fn now_ms(&self) -> i64 {
        ms((self.routines.clock)())
    }

    pub(crate) fn routines_msg(&self) -> ServerMsg {
        ServerMsg::Routines { items: self.routines.views() }
    }

    fn takes_routines(c: &crate::Client) -> bool {
        c.attached && c.caps.iter().any(|c| c == byob_protocol::routines::CAP_ROUTINES)
    }

    /// Send a message to every client that takes routine messages.
    fn send_routine_clients(&mut self, msg: &ServerMsg) {
        let ids: Vec<ClientId> = self.clients.iter().filter(|(_, c)| Self::takes_routines(c)).map(|(id, _)| *id).collect();
        for id in ids {
            self.send_to(id, msg.clone());
        }
    }

    /// The routines, to client `id` if it takes them (after the Welcome).
    pub(crate) fn send_routines(&mut self, id: ClientId) {
        if self.clients.get(&id).is_some_and(Self::takes_routines) {
            let msg = self.routines_msg();
            self.send_to(id, msg);
        }
    }

    pub(crate) fn broadcast_routines(&mut self) {
        let msg = self.routines_msg();
        self.send_routine_clients(&msg);
    }

    fn routine_event(&mut self, routine: &Routine, chat: Option<&str>, kind: RoutineEventKind, title: String, body: String) {
        let msg = ServerMsg::RoutineEvent { routine: routine.id.clone(), name: routine.name.clone(), chat: chat.map(str::to_owned), kind, title, body };
        self.send_routine_clients(&msg);
    }

    /// Routines changed in a way the state shows (on, off, scheduled or not).
    fn routines_changed(&mut self) {
        self.routines.refresh_wake();
        self.broadcast_routines();
        self.broadcast_state();
    }

    // ---- saving --------------------------------------------------------------------------

    /// Check and save a routine from a client or a tool. A new one (no id, or an id that
    /// doesn't exist yet from a client) gets an id, its creation time and first days.
    pub(crate) fn save_routine(&mut self, r: Routine, here: Option<&str>) -> Result<Routine, String> {
        let mut r = r.validated()?;
        let now = self.now_ms();
        let existing = (!r.id.is_empty()).then(|| self.routines.list.get(&r.id).cloned()).flatten();
        if existing.is_none() {
            if self.routines.list.items().len() >= routine::MAX_ROUTINES {
                return Err(format!("byob keeps at most {} routines", routine::MAX_ROUTINES));
            }
            let taken = |id: &str| self.routines.list.get(id).is_some();
            if r.id.is_empty() || taken(&r.id) {
                r.id = routine::new_id(&r.name, taken);
            }
            r.created_at = now;
        } else if let Some(old) = &existing {
            r.created_at = old.created_at;
        }
        // Each schedule: readable, at most every 15 minutes, with a zone and a first day so
        // it doesn't drift (every 2 weeks stays on its weeks).
        let today = at_ms(now).unwrap_or_default();
        for t in &mut r.when {
            if let Trigger::Schedule { rule, at, zone, start } = t {
                let s = Schedule::parse(rule, at, zone.as_deref(), start.as_deref())?;
                if zone.is_none()
                    && let byob_when::time::UserZone::Iana(tz) = s.zone()
                {
                    *zone = Some(tz.name().to_owned());
                }
                if start.is_none() {
                    *start = Some(s.zone().local(today).format("%Y-%m-%d").to_string());
                }
            }
        }
        if let Some(chat) = here
            && r.project.is_none()
            && let Some(folder) = self.chat_folder_of(chat)
            && let Some(w) = self.workspaces.by_folder(&folder)
        {
            r.project = Some(w.id.clone());
        }
        if let Some(p) = &r.project
            && self.workspaces.get(p).is_none()
        {
            return Err(format!("there's no project {p:?}"));
        }
        if let Some(m) = &r.model
            && !self.profiles.contains_key(m)
        {
            return Err(format!("there's no saved model {m:?}"));
        }
        let schedule_changed = existing.as_ref().is_some_and(|old| old.when != r.when || (!old.enabled && r.enabled));
        self.routines.list.put(r.clone()).map_err(|e| format!("couldn't save the routine: {e}"))?;
        if schedule_changed {
            // A changed schedule starts from now: nothing "missed" under the old one.
            self.set_last_slot(&r.id, now);
        }
        self.routines_changed();
        Ok(r)
    }

    fn set_last_slot(&mut self, id: &str, slot: i64) {
        if let Err(e) = self.routines.logs.update(id, |l| l.last_slot = Some(l.last_slot.unwrap_or(slot).max(slot))) {
            log::warn!("routine {id}: couldn't save its last run time: {e}");
        }
    }

    pub(crate) fn delete_routine(&mut self, id: &str) -> Result<String, String> {
        let Some(r) = self.routines.list.get(id).cloned() else { return Err(format!("there's no routine {id:?}")) };
        self.routines.list.remove(id).map_err(|e| format!("couldn't delete the routine: {e}"))?;
        self.routines.logs.remove(id);
        self.routines.queue.retain(|w| w.routine != id);
        self.routines_changed();
        Ok(format!("Deleted the routine {:?} (its run chats stay).", r.name))
    }

    pub(crate) fn set_routine_enabled(&mut self, id: &str, enabled: bool) -> Result<(), String> {
        let Some(mut r) = self.routines.list.get(id).cloned() else { return Err(format!("there's no routine {id:?}")) };
        if r.enabled == enabled {
            return Ok(());
        }
        r.enabled = enabled;
        self.save_routine(r, None).map(|_| ())
    }

    pub(crate) fn send_routine_runs(&mut self, client: ClientId, id: &str) {
        let runs: Vec<RunRecord> = self.routines.logs.get(id).map(|l| l.runs.iter().rev().cloned().collect()).unwrap_or_default();
        self.send_to(client, ServerMsg::RoutineRuns { routine: id.to_owned(), runs });
    }

    /// A client's routine message.
    pub(crate) fn routine_client_msg(&mut self, client: ClientId, msg: byob_protocol::ClientMsg) {
        use byob_protocol::ClientMsg;
        let result = match msg {
            ClientMsg::SaveRoutine { routine } => self.save_routine(routine, None).map(|_| ()),
            ClientMsg::DeleteRoutine { id } => self.delete_routine(&id).map(|_| ()),
            ClientMsg::RunRoutine { id } => self.run_routine_now(&id).map(|_| ()),
            ClientMsg::SetRoutineEnabled { id, enabled } => self.set_routine_enabled(&id, enabled),
            ClientMsg::ListRoutineRuns { id } => {
                self.send_routine_runs(client, &id);
                Ok(())
            }
            _ => Ok(()),
        };
        if let Err(e) = result {
            self.notice(client, e);
        }
    }

    // ---- the scheduler ---------------------------------------------------------------------

    /// The engine's tick: start what's due, give up on approvals nobody answered.
    pub(crate) fn routines_tick(&mut self) {
        let now = self.now_ms();
        self.expire_waits(now);
        if self.routines.wake_at.is_none_or(|w| w > now) {
            return;
        }
        let items: Vec<Routine> = self.routines.list.items().to_vec();
        for r in items {
            if self.routines.queued(&r.id) {
                continue;
            }
            match due(&r, self.routines.last_slot(&r.id), now) {
                Some(Due::Run(slot)) => self.start_run(&r.id, RunCause::Schedule, Some(slot)),
                Some(Due::Pass(slot)) => {
                    log::info!("routine {}: missed runs skipped", r.id);
                    self.set_last_slot(&r.id, slot);
                }
                None => {}
            }
        }
        self.routines.refresh_wake();
        self.broadcast_routines();
    }

    /// `RunRoutine` / `run_routine`: a run now, whatever its schedule.
    pub(crate) fn run_routine_now(&mut self, id: &str) -> Result<String, String> {
        let Some(r) = self.routines.list.get(id).cloned() else { return Err(format!("there's no routine {id:?}")) };
        if self.routines.running(id) {
            return Err(format!("{:?} is running already", r.name));
        }
        if self.routines.queued(id) {
            return Err(format!("{:?} is waiting to start already", r.name));
        }
        self.start_run(id, RunCause::Manual, None);
        self.broadcast_routines();
        let chat = self.routines.active.iter().find(|(_, a)| a.routine == id).map(|(c, _)| c.clone());
        Ok(match chat {
            Some(c) => format!("Started a run of {:?} (chat {c}).", r.name),
            None if self.routines.queued(id) => format!("{:?} will start when one of the {MAX_RUNNING} routine runs going now ends.", r.name),
            None => self
                .routines
                .logs
                .get(id)
                .and_then(|l| l.runs.last())
                .and_then(|run| run.error.clone())
                .map_or_else(|| format!("{:?} didn't start.", r.name), |e| format!("{:?} couldn't start: {e}", r.name)),
        })
    }

    /// Start a run of `id` now, or queue it while [`MAX_RUNNING`] are going. A scheduled run
    /// due while the routine runs already is recorded as skipped.
    fn start_run(&mut self, id: &str, cause: RunCause, slot: Option<i64>) {
        let now = self.now_ms();
        if self.routines.running(id) {
            if let Some(s) = slot {
                self.set_last_slot(id, s);
                let rec = RunRecord { started: now, finished: Some(now), status: RunStatus::Skipped, cause, slot, ..RunRecord::default() };
                let _ = self.routines.logs.update(id, |l| l.runs.push(rec));
            }
            return;
        }
        if self.routines.active.len() >= MAX_RUNNING {
            if !self.routines.queued(id) {
                self.routines.queue.push_back(Waiting { routine: id.to_owned(), cause, slot });
            }
            return;
        }
        if let Some(s) = slot {
            // Before the run starts: a crash during it must not run this slot again.
            self.set_last_slot(id, s);
        }
        if let Err(e) = self.launch_run(id, cause, slot, now) {
            log::warn!("routine {id}: the run couldn't start: {e}");
            let rec = RunRecord { started: now, finished: Some(now), status: RunStatus::Failed, cause, slot, error: Some(e.clone()), ..RunRecord::default() };
            let _ = self.routines.logs.update(id, |l| l.runs.push(rec));
            if let Some(r) = self.routines.list.get(id).cloned()
                && r.tell != Tell::Never
            {
                self.routine_event(&r, None, RoutineEventKind::Failed, format!("{} couldn't run", r.name), e);
            }
        }
    }

    /// Make the run's chat and start its turn.
    fn launch_run(&mut self, id: &str, cause: RunCause, slot: Option<i64>, now: i64) -> Result<(), String> {
        let r = self.routines.list.get(id).cloned().ok_or("the routine is gone")?;
        let when = at_ms(now).unwrap_or_default();
        let chat = crate::chats::new_chat_id();
        let folder = match r.project.as_deref().and_then(|p| self.workspaces.get(p)) {
            Some(w) => w.folder.clone(),
            None => self.unattached_folder(&chat).map_err(|e| e.to_string())?,
        };
        let row = self.store.create_chat(&chat, &run_title(&r, when), &folder).map_err(|e| format!("couldn't make its chat: {e}"))?;
        if let Err(e) = self.store.set_chat_origin(&chat, Some(&r.id)) {
            log::warn!("routine {id}: couldn't mark its run chat: {e}");
        }
        if let Some(profile) = r.model.clone().filter(|p| self.profiles.contains_key(p))
            && let Err(e) = self.workspaces.set_chat_config(&chat, Layer { profile: Some(profile), ..Layer::default() })
        {
            log::warn!("routine {id}: couldn't give its run its model: {e}");
        }
        self.live.insert(
            chat.clone(),
            crate::Live {
                entries: Vec::new(),
                workspace: Arc::new(row.workspace.clone()),
                artifacts: Vec::new(),
                compactions: Vec::new(),
                tally: crate::compact::Tally::default(),
                app_context: BTreeMap::new(),
            },
        );
        self.routines.active.insert(chat.clone(), Active { routine: id.to_owned(), report: None, waiting: 0 });
        let rec = RunRecord { chat: Some(chat.clone()), started: now, status: RunStatus::Running, cause, slot, ..RunRecord::default() };
        if let Err(e) = self.routines.logs.update(id, |l| l.runs.push(rec)) {
            log::warn!("routine {id}: couldn't record its run: {e}");
        }
        self.chat_changed(&chat);
        self.refresh_folder_stats();
        let run = crate::RunSetup {
            routine: id.to_owned(),
            note: run_note(&r, cause, when),
            policy: run_policy(r.allowed),
            tools: vec![Arc::new(crate::routine_tools::ReportTool { inbox: self.inbox.clone() })],
        };
        let message = run_message(&r, self.routines.logs.get(id));
        if let Err(e) = self.begin_turn_with(&chat, message, Vec::new(), Some(run)) {
            self.routines.active.remove(&chat);
            let _ = self.routines.logs.update(id, |l| {
                if let Some(rec) = l.runs.iter_mut().rev().find(|x| x.chat.as_deref() == Some(chat.as_str())) {
                    rec.status = RunStatus::Failed;
                    rec.finished = Some(now);
                    rec.error = Some(e.clone());
                }
            });
            return Err(e);
        }
        self.emit(&chat, "run_started", serde_json::json!({"routine": id, "cause": cause}));
        Ok(())
    }

    // ---- while a run goes ----------------------------------------------------------------

    /// The run in `chat` called `report`. The latest report of a run is its report.
    pub(crate) fn routine_reported(&mut self, chat: &str, report: Report) {
        let Some(a) = self.routines.active.get_mut(chat) else { return };
        a.report = Some(report);
    }

    /// The routine whose run turn `entry` is.
    pub(crate) fn run_of(&self, entry: u64) -> Option<String> {
        self.turns.get(&entry).and_then(|t| t.routine.clone())
    }

    fn set_run_status(&mut self, chat: &str, status: RunStatus) {
        let Some(id) = self.routines.active.get(chat).map(|a| a.routine.clone()) else { return };
        let _ = self.routines.logs.update(&id, |l| {
            if let Some(rec) = l.runs.iter_mut().rev().find(|x| x.chat.as_deref() == Some(chat)) {
                rec.status = status;
            }
        });
        self.broadcast_routines();
    }

    /// An approval card of a run's turn is waiting for a person.
    pub(crate) fn routine_waiting(&mut self, entry: u64, step_title: &str) {
        let Some(chat) = self.turns.get(&entry).filter(|t| t.routine.is_some()).map(|t| t.chat.clone()) else { return };
        let Some(a) = self.routines.active.get_mut(&chat) else { return };
        a.waiting = a.waiting.saturating_add(1);
        let routine = a.routine.clone();
        self.set_run_status(&chat, RunStatus::Waiting);
        if let Some(r) = self.routines.list.get(&routine).cloned() {
            let title = format!("{} is waiting for you", r.name);
            self.routine_event(&r, Some(&chat), RoutineEventKind::Waiting, title, format!("It wants to: {step_title}"));
        }
    }

    /// An approval card of a run's turn was answered (or denied).
    pub(crate) fn routine_answered(&mut self, entry: u64) {
        let Some(chat) = self.turns.get(&entry).filter(|t| t.routine.is_some()).map(|t| t.chat.clone()) else { return };
        let Some(a) = self.routines.active.get_mut(&chat) else { return };
        a.waiting = a.waiting.saturating_sub(1);
        if a.waiting == 0 {
            self.set_run_status(&chat, RunStatus::Running);
        }
    }

    /// "Always for this routine": the tool runs without asking in its later runs too.
    pub(crate) fn grant_routine(&mut self, routine: &str, tool: &str) {
        let Some(mut r) = self.routines.list.get(routine).cloned() else { return };
        if r.grants.iter().any(|g| g == tool) || r.grants.len() >= routine::MAX_GRANTS {
            return;
        }
        r.grants.push(tool.to_owned());
        if let Err(e) = self.routines.list.put(r) {
            log::warn!("routine {routine}: couldn't save the grant for {tool}: {e}");
        }
        self.broadcast_routines();
    }

    /// The routine allows `tool` for good ("Always for this routine").
    pub(crate) fn routine_grants(&self, routine: &str, tool: &str) -> bool {
        self.routines.list.get(routine).is_some_and(|r| r.grants.iter().any(|g| g == tool))
    }

    /// Deny run approvals nobody answered in [`APPROVAL_TIMEOUT_MS`].
    fn expire_waits(&mut self, now: i64) {
        if self.pending.is_empty() {
            return;
        }
        let expired = |p: &crate::Pending, run: Option<&crate::Running>| {
            run.is_some_and(|r| r.routine.is_some()) && now.saturating_sub(p.asked_at) >= APPROVAL_TIMEOUT_MS
        };
        if !self.pending.values().any(|p| expired(p, self.turns.get(&p.entry))) {
            return;
        }
        let entries: Vec<u64> = self.pending.values().filter(|p| expired(p, self.turns.get(&p.entry))).map(|p| p.entry).collect();
        self.deny_pending(expired, NOBODY_ANSWERED);
        for e in entries {
            self.routine_answered(e);
        }
    }

    // ---- endings -------------------------------------------------------------------------

    /// A run's turn ended (`finish_turn`).
    pub(crate) fn routine_turn_finished(&mut self, chat: &str, routine: &str, error: Option<&str>, stopped: bool, tokens: u64) {
        let now = self.now_ms();
        let report = self.routines.active.remove(chat).and_then(|a| a.report);
        let status = if stopped {
            RunStatus::Stopped
        } else if error.is_some() {
            RunStatus::Failed
        } else if report.is_some() {
            RunStatus::Reported
        } else {
            RunStatus::Quiet
        };
        // A routine deleted while its run went keeps no log.
        let exists = self.routines.list.get(routine).is_some();
        let _ = exists.then(|| {
            self.routines.logs.update(routine, |l| {
                if let Some(rec) = l.runs.iter_mut().rev().find(|x| x.chat.as_deref() == Some(chat)) {
                    rec.status = status;
                    rec.finished = Some(now);
                    rec.report.clone_from(&report);
                    rec.error = error.map(str::to_owned);
                    rec.tokens = tokens;
                }
            })
        });
        if let Some(r) = self.routines.list.get(routine).cloned() {
            match (status, &report) {
                (RunStatus::Reported, Some(rep)) if r.tell != Tell::Never => {
                    self.routine_event(&r, Some(chat), RoutineEventKind::Reported, rep.title.clone(), rep.body.clone());
                }
                (RunStatus::Quiet, _) if r.tell == Tell::Always => {
                    self.routine_event(&r, Some(chat), RoutineEventKind::Reported, format!("{}: nothing new", r.name), String::new());
                }
                (RunStatus::Failed, _) if r.tell != Tell::Never => {
                    self.routine_event(&r, Some(chat), RoutineEventKind::Failed, format!("{} failed", r.name), error.unwrap_or_default().to_owned());
                }
                _ => {}
            }
            self.prune_runs(&r);
        }
        let data = serde_json::json!({"routine": routine, "status": status, "report": report});
        self.emit(chat, "run_finished", data);
        // The next waiting run, if any.
        while self.routines.active.len() < MAX_RUNNING {
            let Some(w) = self.routines.queue.pop_front() else { break };
            self.start_run(&w.routine, w.cause, w.slot);
        }
        self.routines.refresh_wake();
        self.broadcast_routines();
    }

    /// Delete run chats beyond the routine's `keep` (oldest first); their records keep the
    /// report, and records with neither chat nor report go.
    fn prune_runs(&mut self, r: &Routine) {
        let keep = usize::try_from(r.keep).unwrap_or(usize::MAX);
        let Some(log) = self.routines.logs.get(&r.id) else { return };
        let with_chats: Vec<String> = log.runs.iter().filter(|x| !x.status.active()).filter_map(|x| x.chat.clone()).collect();
        let excess = with_chats.len().saturating_sub(keep);
        if excess == 0 {
            return;
        }
        let gone: Vec<String> = with_chats.into_iter().take(excess).filter(|c| !self.chat_busy(c)).collect();
        for chat in &gone {
            if let Err(e) = self.remove_chat(chat) {
                log::warn!("routine {}: couldn't delete an old run chat: {e}", r.id);
            }
        }
        let _ = self.routines.logs.update(&r.id, |l| {
            for x in &mut l.runs {
                if x.chat.as_ref().is_some_and(|c| gone.contains(c)) {
                    x.chat = None;
                }
            }
            let mut extra = l.runs.iter().filter(|x| x.chat.is_none()).count().saturating_sub(keep);
            l.runs.retain(|x| {
                let drop = extra > 0 && x.chat.is_none() && x.report.is_none() && !x.status.active();
                if drop {
                    extra -= 1;
                }
                !drop
            });
        });
        self.refresh_folder_stats();
        self.broadcast_workspaces();
    }

    // ---- tools ---------------------------------------------------------------------------

    /// A routine tool's request from the turn in `chat`.
    pub(crate) fn routine_op(&mut self, chat: Option<&str>, op: Op) -> Result<String, String> {
        match op {
            Op::Create { routine, here } => {
                let r = self.save_routine(*routine, chat.filter(|_| here))?;
                let when = described(&r);
                let next = next_due(&r, None)
                    .and_then(at_ms)
                    .map(|t| byob_when::time::show(t, &schedules(&r).into_iter().flatten().next().map(|s| s.zone()).unwrap_or_default()));
                Ok(format!(
                    "Created the routine {:?} (id {}): {}.{} Offer to run it once now (run_routine) so the user sees what a run does.",
                    r.name,
                    r.id,
                    if when.is_empty() { "no schedule, it runs when asked".to_owned() } else { when.join("; ") },
                    next.map(|n| format!(" Its first scheduled run is {n}.")).unwrap_or_default()
                ))
            }
            Op::Update { id, apply } => {
                let Some(current) = self.routines.list.get(&id).cloned() else { return Err(format!("there's no routine {id:?}")) };
                let r = self.save_routine(apply(current)?, None)?;
                Ok(format!("Updated the routine {:?}: {}.", r.name, described(&r).join("; ")))
            }
            Op::Delete { id } => self.delete_routine(&id),
            Op::List => Ok(self.list_text()),
            Op::Run { id } => self.run_routine_now(&id),
        }
    }

    fn list_text(&self) -> String {
        let views = self.routines.views();
        if views.is_empty() {
            return "There are no routines yet.".into();
        }
        let mut out = String::from("Routines (names and instructions are the user's own words):\n");
        for v in views {
            let r = &v.routine;
            let when = if v.schedule.is_empty() { "when asked".to_owned() } else { v.schedule.join("; ") };
            out.push_str(&format!("- {} ({}): {when}, {}", r.id, r.name, if r.enabled { "on" } else { "off" }));
            if let Some(n) = v.next_run.and_then(at_ms) {
                out.push_str(&format!("; next run {}", byob_when::time::show(n, &byob_when::time::UserZone::default())));
            }
            if let Some(last) = &v.last_run {
                out.push_str(&format!("; last run: {}", last.status.label()));
                if let Some(rep) = &last.report {
                    out.push_str(&format!(" ({:?})", rep.title));
                }
            }
            out.push_str(&format!("\n  Does: {}\n  Can: {}\n", r.instructions.chars().take(300).collect::<String>(), allowed_words(r.allowed)));
        }
        out
    }

    /// The folder of `chat` (for "in this project").
    fn chat_folder_of(&self, chat: &str) -> Option<std::path::PathBuf> {
        if let Some(l) = self.live.get(chat) {
            return Some(l.workspace.as_ref().clone());
        }
        self.store.chat(chat).ok().flatten().map(|c| c.workspace)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn daily(at: &str) -> Routine {
        Routine {
            id: "r".into(),
            name: "Loom deals".into(),
            instructions: "Find loom deals.".into(),
            when: vec![Trigger::Schedule { rule: "FREQ=DAILY".into(), at: at.into(), zone: Some("UTC".into()), start: Some("2026-11-01".into()) }],
            created_at: utc("2026-11-20 12:00"),
            ..Routine::default()
        }
    }

    fn utc(s: &str) -> i64 {
        let t = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap();
        Utc.from_utc_datetime(&t).timestamp_millis()
    }

    #[test]
    fn due_once_for_the_latest_slot_however_many_were_missed() {
        let r = daily("08:00");
        assert_eq!(due(&r, None, utc("2026-11-21 07:59")), None);
        assert_eq!(due(&r, None, utc("2026-11-21 08:00")), Some(Due::Run(utc("2026-11-21 08:00"))));
        // three days asleep: one run, for the latest slot
        assert_eq!(due(&r, Some(utc("2026-11-21 08:00")), utc("2026-11-24 09:30")), Some(Due::Run(utc("2026-11-24 08:00"))));
        // handled: not again
        assert_eq!(due(&r, Some(utc("2026-11-24 08:00")), utc("2026-11-24 09:30")), None);
        assert_eq!(next_due(&r, Some(utc("2026-11-24 08:00"))), Some(utc("2026-11-25 08:00")));
        let skip = Routine { missed: Missed::Skip, ..daily("08:00") };
        assert_eq!(due(&skip, Some(utc("2026-11-21 08:00")), utc("2026-11-24 09:30")), Some(Due::Pass(utc("2026-11-24 08:00"))));
        assert_eq!(
            due(&skip, Some(utc("2026-11-21 08:00")), utc("2026-11-24 08:10")),
            Some(Due::Run(utc("2026-11-24 08:00"))),
            "a few minutes late is on time"
        );
        let off = Routine { enabled: false, ..daily("08:00") };
        assert_eq!(due(&off, None, utc("2026-11-24 09:30")), None);
        assert_eq!(next_due(&off, None), None);
        let manual = Routine { when: vec![], ..daily("08:00") };
        assert_eq!(due(&manual, None, utc("2026-11-24 09:30")), None);
    }

    #[test]
    fn runs_ask_instead_of_refusing() {
        let p = run_policy(Approvals::ReadOnly);
        assert_eq!((p.read, p.write, p.network, p.execute), (Rule::Allow, Rule::Ask, Rule::Ask, Rule::Ask));
        assert_eq!(run_policy(Approvals::AllowAll), Policy::ALLOW_ALL);
        assert!(run_policy(Approvals::Yolo).yolo);
    }

    #[test]
    fn messages_carry_earlier_reports() {
        let r = daily("08:00");
        let mut log = RunLog::default();
        assert_eq!(run_message(&r, Some(&log)), "Find loom deals.");
        for (i, t) in ["Old find", "New find"].iter().enumerate() {
            log.runs.push(RunRecord {
                started: utc("2026-11-20 08:00") + i64::try_from(i).unwrap() * 86_400_000,
                report: Some(Report::clean(t, &format!("{t} body"))),
                ..RunRecord::default()
            });
        }
        log.runs.push(RunRecord { status: RunStatus::Quiet, ..RunRecord::default() });
        let m = run_message(&r, Some(&log));
        assert!(m.contains("- 2026-11-21: New find\n- 2026-11-20: Old find"), "{m}");
        assert!(m.ends_with("The last one in full:\nNew find body"), "{m}");
        assert!(run_note(&r, RunCause::Schedule, Utc::now()).contains("Nobody is watching"));
        assert!(run_title(&r, at_ms(utc("2026-11-24 08:00")).unwrap()).starts_with("Loom deals · Tue Nov 24, 8:00 AM"));
    }
}
