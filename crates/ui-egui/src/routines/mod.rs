//! Routines: what byob does by itself, on a schedule or when you press Run now (see
//! `byob_types::routine`). The page lists them (what each is, when it runs, what happened
//! last time, when it's next); a routine's own page edits it and lists its runs, each a chat
//! you can open. Run chats stay out of Recent: they're reached from here.
//!
//! The engine keeps routines and describes their schedules (cap `routines`); this keeps what
//! it last said, the runs asked for, and which reports you've seen on this device (for the
//! sidebar's dot).

pub mod page;
pub mod schedule;

use std::collections::{BTreeMap, BTreeSet};

use byob_protocol::routines::{RoutineView, RunCause, RunRecord, RunStatus, Trigger};
use byob_protocol::{ClientMsg, NoticeLevel, RoutineEventKind, ServerMsg};
use chrono::{Datelike, NaiveDateTime};
use egui::{Color32, Rect, RichText, Sense, Stroke, Ui, pos2, vec2};

use crate::icons;
use crate::theme::Tokens;

/// What the page shows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum View {
    #[default]
    List,
    /// A routine's page, by id.
    Routine(String),
    /// A new routine (the editor, empty or from a starter).
    New,
}

/// What the pages ask of the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Open a run's chat.
    OpenChat(String),
    /// Back to the chat.
    Close,
    /// Settings → System (keep running in the background).
    SystemSettings,
}

/// A run that reported something, waits for you or failed, as the engine said
/// (`ServerMsg::RoutineEvent`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub routine: String,
    pub name: String,
    pub chat: Option<String>,
    pub kind: RoutineEventKind,
    pub title: String,
    pub body: String,
}

/// Routines as the engine last listed them, their runs, and the page's state.
#[derive(Default)]
pub struct Routines {
    pub items: Vec<RoutineView>,
    /// Runs asked for, by routine (newest first).
    runs: BTreeMap<String, Vec<RunRecord>>,
    pub view: View,
    /// The routine page's form ([`page`]).
    pub(crate) editor: Option<page::Editor>,
    /// Delete asked about: (id, name).
    confirm_delete: Option<(String, String)>,
    /// A new routine (or a copy) was saved: switch to it once it's listed. The ids before.
    creating: Option<BTreeSet<String>>,
    /// When each routine's latest notable run was seen on this device (for the dot).
    pub seen: BTreeMap<String, i64>,
    /// Routines first seen in this session: their runs from before count as seen.
    started: i64,
    out: Vec<ClientMsg>,
}

/// A run worth a look: it found something, waits for you, or failed.
fn notable(run: &RunRecord) -> bool {
    matches!(run.status, RunStatus::Reported | RunStatus::Waiting | RunStatus::Failed)
}

/// When a run happened, for "seen" (its end, or its start while it goes).
fn run_time(run: &RunRecord) -> i64 {
    run.finished.unwrap_or(run.started)
}

/// A scheduled run that started well after its slot (byob was closed, or the computer
/// slept) was made up.
pub fn made_up(run: &RunRecord) -> bool {
    run.cause == RunCause::Schedule && run.slot.is_some_and(|s| run.started.saturating_sub(s) > 15 * 60 * 1000)
}

impl Routines {
    /// Take in a routine message; false for others.
    pub fn apply(&mut self, msg: &ServerMsg) -> bool {
        match msg {
            ServerMsg::Routines { items } => {
                self.items.clone_from(items);
                self.listed();
            }
            ServerMsg::RoutineRuns { routine, runs } => {
                self.runs.insert(routine.clone(), runs.clone());
            }
            _ => return false,
        }
        true
    }

    /// After a new list: a routine just made opens; the one in view follows its saved form
    /// (unless you're changing it) and its runs are asked for again when a run changed.
    fn listed(&mut self) {
        if let Some(before) = &self.creating
            && let Some(new) = self.items.iter().find(|v| !before.contains(&v.routine.id))
        {
            let id = new.routine.id.clone();
            self.creating = None;
            self.open(&id);
            return;
        }
        let View::Routine(id) = &self.view else { return };
        let Some(v) = self.items.iter().find(|v| &v.routine.id == id) else {
            // Deleted (here or elsewhere).
            self.view = View::List;
            self.editor = None;
            return;
        };
        let id = id.clone();
        if let Some(e) = &mut self.editor
            && !e.changed()
        {
            *e = page::Editor::of(&v.routine);
        }
        let latest = v.last_run.as_ref().map(|r| (r.chat.clone(), r.status));
        let known = self.runs.get(&id).and_then(|r| r.first()).map(|r| (r.chat.clone(), r.status));
        if latest.is_some() && latest != known {
            self.out.push(ClientMsg::ListRoutineRuns { id });
        }
    }

    /// A routine by id.
    pub fn get(&self, id: &str) -> Option<&RoutineView> {
        self.items.iter().find(|v| v.routine.id == id)
    }

    /// Its runs, as last asked for (newest first).
    pub fn runs(&self, id: &str) -> &[RunRecord] {
        self.runs.get(id).map_or(&[], Vec::as_slice)
    }

    /// The run whose chat is `chat`, if its routine's runs were asked for.
    pub fn run_of_chat(&self, routine: &str, chat: &str) -> Option<&RunRecord> {
        self.runs(routine).iter().find(|r| r.chat.as_deref() == Some(chat))
    }

    /// Show the list.
    pub fn show_list(&mut self) {
        self.view = View::List;
        self.editor = None;
    }

    /// Open routine `id`'s page (and ask for its runs).
    pub fn open(&mut self, id: &str) {
        self.view = View::Routine(id.to_owned());
        self.editor = self.get(id).map(|v| page::Editor::of(&v.routine));
        self.out.push(ClientMsg::ListRoutineRuns { id: id.to_owned() });
        self.mark_seen(id);
    }

    /// A new routine, its form filled from `starter` (empty: from scratch).
    pub fn new_routine(&mut self, starter: Option<&page::Starter>) {
        self.view = View::New;
        self.editor = Some(page::Editor::new(starter));
    }

    /// Ask the engine to save `routine`; a new one (empty id) opens once it's listed.
    pub(crate) fn save(&mut self, routine: byob_protocol::routines::Routine) {
        if routine.id.is_empty() {
            self.creating = Some(self.items.iter().map(|v| v.routine.id.clone()).collect());
        }
        self.out.push(ClientMsg::SaveRoutine { routine });
    }

    pub(crate) fn run_now(&mut self, id: &str) {
        self.out.push(ClientMsg::RunRoutine { id: id.to_owned() });
    }

    pub(crate) fn set_enabled(&mut self, id: &str, enabled: bool) {
        // Shown at once; the engine's list confirms it.
        if let Some(v) = self.items.iter_mut().find(|v| v.routine.id == id) {
            v.routine.enabled = enabled;
        }
        self.out.push(ClientMsg::SetRoutineEnabled { id: id.to_owned(), enabled });
    }

    /// A copy, named "… (copy)" and off, opened once saved.
    pub(crate) fn duplicate(&mut self, id: &str) {
        let Some(v) = self.get(id) else { return };
        let mut r = v.routine.clone();
        r.id = String::new();
        let name: String = r.name.chars().take(byob_types::routine::MAX_NAME_CHARS.saturating_sub(7)).collect();
        r.name = format!("{name} (copy)");
        r.grants.clear();
        r.enabled = false;
        self.save(r);
    }

    /// Ask before deleting.
    pub(crate) fn ask_delete(&mut self, id: &str) {
        if let Some(v) = self.get(id) {
            self.confirm_delete = Some((id.to_owned(), v.routine.name.clone()));
        }
    }

    /// The delete dialog, while asked.
    pub(crate) fn delete_dialog(&mut self, ctx: &egui::Context, t: &Tokens) {
        let Some((id, name)) = self.confirm_delete.clone() else { return };
        let body = format!("{name} stops running. Its past runs stay in your chats.");
        let confirm = crate::menu::Confirm { title: "Delete this routine?", body: &body, action: "Delete", check: None };
        match confirm.show(ctx, t, egui::Id::unique("routine-delete")) {
            crate::menu::Choice::Confirm => {
                self.confirm_delete = None;
                self.out.push(ClientMsg::DeleteRoutine { id: id.clone() });
                if self.view == View::Routine(id) {
                    self.show_list();
                }
            }
            crate::menu::Choice::Cancel => self.confirm_delete = None,
            crate::menu::Choice::Waiting => {}
        }
    }

    /// What to send the engine.
    pub fn take(&mut self) -> Vec<ClientMsg> {
        std::mem::take(&mut self.out)
    }

    // ---- seen ----------------------------------------------------------------------------

    /// Once per frame: routines first seen now count their earlier runs as seen.
    pub fn begin(&mut self, now_ms: i64) {
        if self.started == 0 {
            self.started = now_ms;
        }
    }

    /// Routine `id` has a run worth a look you haven't seen.
    pub fn unseen(&self, id: &str) -> bool {
        let Some(run) = self.get(id).and_then(|v| v.last_run.as_ref()).filter(|r| notable(r)) else { return false };
        match self.seen.get(id) {
            Some(at) => run_time(run) > *at,
            None => self.started > 0 && run_time(run) > self.started,
        }
    }

    /// Any routine has one (the sidebar's dot).
    pub fn any_unseen(&self) -> bool {
        self.items.iter().any(|v| self.unseen(&v.routine.id))
    }

    /// Routine `id`'s latest run has been seen.
    pub fn mark_seen(&mut self, id: &str) {
        if let Some(run) = self.get(id).and_then(|v| v.last_run.as_ref()) {
            let at = run_time(run);
            self.seen.insert(id.to_owned(), at);
        }
    }

    /// Every routine's (the list is in view).
    pub fn mark_all_seen(&mut self) {
        let ids: Vec<String> = self.items.iter().map(|v| v.routine.id.clone()).collect();
        for id in ids {
            self.mark_seen(&id);
        }
        // Routines deleted since don't need remembering.
        let ids: BTreeSet<&String> = self.items.iter().map(|v| &v.routine.id).collect();
        self.seen.retain(|k, _| ids.contains(k));
    }

    /// A routine event, as an in-app notice (where there are no desktop notifications).
    pub fn notice_for(e: &Event) -> (NoticeLevel, String) {
        match e.kind {
            RoutineEventKind::Failed => (NoticeLevel::Info, format!("{}: {}", e.title, short(&e.body, 160))),
            RoutineEventKind::Waiting => (NoticeLevel::Info, e.title.clone()),
            _ => (NoticeLevel::Info, format!("{}: {}", e.name, e.title)),
        }
    }
}

/// `text` on one line, cut to `max` characters with "…".
pub fn short(text: &str, max: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= max {
        return line;
    }
    let mut out: String = line.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

// ---- time in words ---------------------------------------------------------------------

/// This device's offset from UTC at `ms`, in seconds.
fn offset_secs(ms: i64) -> i32 {
    #[cfg(target_arch = "wasm32")]
    {
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)] // minutes, a few hundred
        let minutes = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64)).get_timezone_offset() as i32;
        -minutes * 60
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        use chrono::TimeZone;
        chrono::Local.timestamp_millis_opt(ms).single().map_or(0, |d| d.offset().local_minus_utc())
    }
}

/// `ms` (since the epoch) as this device's local time.
pub fn local(ms: i64) -> NaiveDateTime {
    let shifted = ms.saturating_add(i64::from(offset_secs(ms)) * 1000);
    chrono::DateTime::from_timestamp_millis(shifted).unwrap_or_default().naive_utc()
}

/// When, as the lists say it: "Today, 8:02 AM", "Tomorrow, 8:00 AM", "Sunday, 6:00 PM",
/// "Nov 18" (further back), "Dec 3, 8:00 AM" (further ahead).
pub fn when_words(at: NaiveDateTime, now: NaiveDateTime) -> String {
    let time = schedule::clock(at.time().format("%H").to_string().parse().unwrap_or(0), at.time().format("%M").to_string().parse().unwrap_or(0));
    let days = (at.date() - now.date()).num_days();
    match days {
        0 => format!("Today, {time}"),
        1 => format!("Tomorrow, {time}"),
        -1 => format!("Yesterday, {time}"),
        -6..=6 => format!("{}, {time}", at.format("%A")),
        _ if days > 0 && at.year() == now.year() => format!("{}, {time}", at.format("%b %-d")),
        _ if at.year() == now.year() => at.format("%b %-d").to_string(),
        _ => at.format("%b %-d, %Y").to_string(),
    }
}

/// [`when_words`] for ms since the epoch, on this device's clock.
pub fn when_ms(at: i64, now: i64) -> String {
    when_words(local(at), local(now))
}

/// The same inside a sentence: "today at 8:00 AM", "on Sunday at 6:00 PM", "on Nov 18".
pub fn when_in_sentence(at: i64, now: i64) -> String {
    let w = when_ms(at, now);
    match w.split_once(", ") {
        Some((day @ ("Today" | "Tomorrow" | "Yesterday"), time)) => format!("{} at {time}", day.to_lowercase()),
        Some((day, time)) if !time.contains(' ') || time.ends_with('M') => format!("on {day} at {time}"),
        _ => format!("on {w}"),
    }
}

// ---- the list ----------------------------------------------------------------------------

/// Narrower than this (a phone), rows drop their columns and say it all under the name.
const COMPACT: f32 = 560.0;
/// Where a row's text starts, after its tile.
const TEXT_X: f32 = 60.0;
/// Room at a row's right end for its switch and ⋯.
const TOOLS_W: f32 = 104.0;

/// What the list needs from the app.
pub struct ListCtx {
    pub now_ms: i64,
    /// Routines are paused (the tray, or the banner here).
    pub paused: bool,
    /// What closing the window does (routines run while byob runs).
    pub keep_running: Option<crate::desktop::KeepRunning>,
    /// The app can open Settings → System (the desktop).
    pub desktop: bool,
}

/// What a row's icon is: a guess from what it does.
fn icon_for(v: &RoutineView) -> icons::Paint {
    if v.routine.when.iter().any(|t| matches!(t, Trigger::Plugin { plugin, .. } if plugin.contains("mail"))) {
        return icons::mail;
    }
    let words = format!("{} {}", v.routine.name, v.routine.instructions).to_lowercase();
    let has = |any: &[&str]| any.iter().any(|w| words.contains(w));
    if has(&["email", "inbox", "mail"]) {
        icons::mail
    } else if has(&["calendar", "meeting", "agenda"]) {
        icons::calendar
    } else if has(&["news", "headline"]) {
        icons::megaphone
    } else if has(&["web", "search", "price", "deal", "site", "page", "http"]) {
        icons::globe
    } else if has(&["sheet", "spreadsheet", "file", "folder", "document"]) {
        icons::file_text
    } else {
        icons::repeat
    }
}

/// A routine's tile: its icon on a tint of a color of its own (by id, so it stays).
fn tile(p: &egui::Painter, t: &Tokens, r: Rect, v: &RoutineView, faded: bool) {
    let hash = v.routine.id.bytes().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b)));
    let color = t.chart[usize::try_from(hash).unwrap_or(0) % t.chart.len()];
    let color = if faded { crate::theme::mix(color, t.text_faint, 0.7) } else { color };
    p.rect_filled(r, t.round(crate::form::CONTROL_R), color.gamma_multiply(if t.dark { 0.2 } else { 0.14 }));
    icon_for(v)(p, r.shrink(r.height() * 0.28), color);
}

/// How it runs, in words: its schedules, its plugin triggers, or by hand.
pub fn when_line(v: &RoutineView) -> String {
    let mut parts = v.schedule.clone();
    for t in &v.routine.when {
        if let Trigger::Plugin { plugin, trigger, .. } = t {
            parts.push(format!("When {} ({plugin})", trigger.replace(['_', '-'], " ")));
        }
    }
    if parts.is_empty() { "When you run it".to_owned() } else { parts.join(" · ") }
}

/// The last run, as the list says it: a pip's color, the lead in bold ("Found:"), the rest.
pub fn last_run_words(t: &Tokens, run: Option<&RunRecord>) -> (Color32, String, String) {
    let Some(run) = run else { return (t.text_faint.gamma_multiply(0.6), String::new(), "Not run yet".to_owned()) };
    match run.status {
        RunStatus::Reported => {
            let title = run.report.as_ref().map(|r| short(&r.title, 120)).unwrap_or_default();
            (t.accent, "Found:".to_owned(), title)
        }
        RunStatus::Quiet => (t.text_faint, String::new(), "Nothing new".to_owned()),
        RunStatus::Waiting => (t.warning, String::new(), "Waiting for you".to_owned()),
        RunStatus::Failed => (t.danger, "Failed:".to_owned(), short(run.error.as_deref().unwrap_or("something went wrong"), 120)),
        RunStatus::Running => (t.accent, String::new(), "Running now…".to_owned()),
        s => (t.text_faint, String::new(), s.label().to_owned()),
    }
}

/// When it's next: its next run, Paused, or how it starts.
fn next_words(v: &RoutineView, ctx: &ListCtx) -> String {
    if !v.routine.enabled || (ctx.paused && !v.schedule.is_empty()) {
        return "Paused".to_owned();
    }
    match v.next_run {
        Some(at) => when_ms(at, ctx.now_ms),
        None if v.routine.when.iter().any(|t| matches!(t, Trigger::Plugin { .. })) => "When it's triggered".to_owned(),
        None => "When you run it".to_owned(),
    }
}

/// One line of text, cut to `width`.
fn line(p: &egui::Painter, text: String, font: egui::FontId, color: Color32, width: f32) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text, font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(width.max(10.0));
    p.layout_job(job)
}

/// The last run's words with a bold lead, cut to `width`.
fn lead_line(p: &egui::Painter, ui: &Ui, t: &Tokens, (lead, rest): (&str, &str), color: Color32, width: f32) -> std::sync::Arc<egui::Galley> {
    let font = egui::FontId::proportional(t.text_size(13.0));
    let mut job = egui::text::LayoutJob::default();
    if !lead.is_empty() {
        let bold = egui::FontId::new(font.size, crate::theme::bold_or_regular(ui.ctx()));
        job.append(lead, 0.0, egui::TextFormat::simple(bold, color));
        job.append(" ", 0.0, egui::TextFormat::simple(font.clone(), color));
    }
    job.append(rest, 0.0, egui::TextFormat::simple(font, color));
    job.wrap = egui::text::TextWrapping::truncate_at_width(width.max(10.0));
    p.layout_job(job)
}

impl Routines {
    /// The Routines page: title, what routines are, New routine; the routines; a note on
    /// running in the background. Or a routine's page.
    pub fn ui(&mut self, ui: &mut Ui, t: &Tokens, ctx: &ListCtx, more: &page::PageCtx) -> Option<Action> {
        let menus_open = egui::Popup::is_any_open(ui.ctx());
        let behind_dialog = !ui.ctx().memory(|m| m.is_above_modal_layer(ui.layer_id()));
        let typing = ui.ctx().memory(|m| m.focused().is_some());
        if !menus_open && !behind_dialog && !typing && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            if self.view == View::List {
                return Some(Action::Close);
            }
            self.show_list();
        }
        let action = match self.view.clone() {
            View::List => {
                self.mark_all_seen();
                self.list_page(ui, t, ctx)
            }
            View::Routine(_) | View::New => self.routine_page(ui, t, ctx, more),
        };
        self.delete_dialog(ui.ctx(), t);
        action
    }

    fn list_page(&mut self, ui: &mut Ui, t: &Tokens, ctx: &ListCtx) -> Option<Action> {
        let mut action = None;
        let page_w = (ui.available_width() - 64.0).clamp(240.0, 880.0);
        egui::ScrollArea::vertical().id_salt("routines-page").auto_shrink([false, false]).show(ui, |ui| {
            ui.add_space(28.0);
            let extra = ((ui.available_width() - page_w) / 2.0).max(0.0);
            let rect = Rect::from_min_size(ui.cursor().min + vec2(extra, 0.0), vec2(page_w, f32::INFINITY));
            ui.scope_builder(egui::UiBuilder::new().max_rect(rect).layout(egui::Layout::top_down(egui::Align::Min)), |ui| {
                ui.set_max_width(page_w);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    ui.label(RichText::new("Routines").size(t.text_size(24.0)).strong().color(t.text_strong));
                    if !self.items.is_empty() {
                        ui.label(RichText::new(self.items.len().to_string()).size(t.text_size(15.0)).color(t.text_faint));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let b = crate::workspace::dialog_button(ui, t, "New routine", true, true, 36.0);
                        if b.on_hover_text("Say what to do and when").clicked() {
                            self.new_routine(None);
                        }
                    });
                });
                ui.add_space(2.0);
                ui.add(
                    egui::Label::new(
                        RichText::new("Things byob does by itself, on a schedule or when you press Run now. It tells you when a run finds something.")
                            .size(t.text_size(13.0))
                            .color(t.text_dim),
                    )
                    .wrap(),
                );
                ui.add_space(16.0);
                if ctx.paused && !self.items.is_empty() {
                    self.paused_banner(ui, t, page_w);
                    ui.add_space(12.0);
                }
                if self.items.is_empty() {
                    self.empty_page(ui, t);
                } else {
                    action = self.list(ui, t, ctx, page_w);
                }
                ui.add_space(14.0);
                if let Some(a) = footer(ui, t, ctx) {
                    action = Some(a);
                }
            });
            ui.add_space(40.0);
        });
        action
    }

    /// "Routines are paused" and Resume.
    fn paused_banner(&mut self, ui: &mut Ui, t: &Tokens, width: f32) {
        egui::Frame::new()
            .fill(crate::theme::mix(t.app_bg, t.warning, 0.12))
            .corner_radius(t.round(crate::form::CONTROL_R))
            .inner_margin(egui::Margin::symmetric(14, 10))
            .show(ui, |ui| {
                ui.set_width(width - 28.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Routines are paused: none runs on its schedule until you resume them.").color(t.text));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if crate::form::button(ui, t, "Resume", true).clicked() {
                            self.out.push(ClientMsg::PauseRoutines { paused: false });
                        }
                    });
                });
            });
    }

    /// No routines: what they are, and a few to start from.
    fn empty_page(&mut self, ui: &mut Ui, t: &Tokens) {
        ui.add_space(28.0);
        ui.vertical_centered(|ui| {
            icons::repeat(ui.painter(), Rect::from_center_size(pos2(ui.max_rect().center().x, ui.cursor().top() + 18.0), vec2(34.0, 34.0)), t.text_faint);
            ui.add_space(44.0);
            ui.label(RichText::new("No routines yet").size(t.text_size(17.0)).color(t.text_strong));
            ui.add_space(4.0);
            ui.add(
                egui::Label::new(
                    RichText::new("A routine is something byob checks for you again and again, like a price, the news or a page, and it tells you when it finds something.")
                        .color(t.text_dim),
                )
                .wrap(),
            );
            ui.add_space(4.0);
            ui.label(RichText::new("Start from one of these, or ask in any chat: \u{201c}Every morning, look for…\u{201d}").color(t.text_faint));
            ui.add_space(18.0);
        });
        let w = ui.available_width().min(520.0);
        let x = ui.max_rect().left() + (ui.available_width() - w) / 2.0;
        for s in page::STARTERS {
            let top = ui.cursor().top();
            let r = Rect::from_min_size(pos2(x, top), vec2(w, 64.0));
            if crate::projects::start_card(ui, t, r, s.icon, s.title, s.sub).clicked() {
                self.new_routine(Some(&s));
            }
            ui.allocate_space(vec2(w, 72.0));
        }
    }

    /// The routines: a header, then a row each.
    fn list(&mut self, ui: &mut Ui, t: &Tokens, ctx: &ListCtx, width: f32) -> Option<Action> {
        let compact = width < COMPACT;
        let cols = Cols::new(width);
        if !compact {
            let (rect, _) = ui.allocate_exact_size(vec2(width, 26.0), Sense::hover());
            let small = egui::TextStyle::Small.resolve(ui.style());
            for (x, text) in [(TEXT_X, "Routine"), (cols.last, "Last run"), (cols.next, "Next")] {
                let g = ui.painter().layout_no_wrap(text.to_owned(), small.clone(), t.text_faint);
                ui.painter().galley(pos2(rect.left() + x, rect.center().y - g.size().y / 2.0), g, t.text_faint);
            }
            ui.painter().hline(rect.x_range(), rect.bottom(), Stroke::new(1.0, t.separator.gamma_multiply(0.7)));
        }
        let mut action = None;
        let items = self.items.clone();
        for (i, v) in items.iter().enumerate() {
            let last = i + 1 == items.len();
            if let Some(a) = self.row(ui, t, v, ctx, &cols, (width, compact, last)) {
                action = Some(a);
            }
        }
        action
    }

    /// One routine: its tile, name and when; its last run; when it's next (Run now on
    /// hover); its switch and ⋯. Off rows fade; one waiting for you stands out.
    fn row(&mut self, ui: &mut Ui, t: &Tokens, v: &RoutineView, ctx: &ListCtx, cols: &Cols, (width, compact, last): (f32, bool, bool)) -> Option<Action> {
        let id = v.routine.id.clone();
        let h = if compact { 84.0 } else { 68.0 };
        let (rect, resp) = ui.allocate_exact_size(vec2(width, h), Sense::click());
        let more_id = egui::Id::unique(("routine-more", &id));
        let menu_open = egui::Popup::is_id_open(ui.ctx(), more_id.with("popup"));
        let over = resp.contains_pointer() || menu_open;
        let faded = !v.routine.enabled;
        let p = ui.painter().clone();
        if over {
            p.rect_filled(rect.shrink2(vec2(0.0, 2.0)), t.round(crate::form::CARD_R - 2.0), crate::form::row_hover(t));
        }
        let fade = |c: Color32| if faded { c.gamma_multiply(0.55) } else { c };
        tile(&p, t, Rect::from_center_size(pos2(rect.left() + 30.0, rect.center().y), vec2(36.0, 36.0)), v, faded);
        let body = egui::TextStyle::Body.resolve(ui.style());
        let small = egui::FontId::proportional(t.text_size(12.5));
        let x = rect.left() + TEXT_X;
        let name_room = if compact { width - TEXT_X - TOOLS_W } else { cols.last - TEXT_X - 16.0 };
        let name = line(&p, v.routine.name.clone(), body.clone(), fade(t.text_strong), name_room);
        let sub = line(&p, when_line(v), small.clone(), fade(t.text_faint), name_room);
        let (pip, lead, rest) = last_run_words(t, v.last_run.as_ref());
        let waiting = v.last_run.as_ref().is_some_and(|r| r.status == RunStatus::Waiting);
        if compact {
            let last_g = lead_line(&p, ui, t, (&lead, &rest), fade(t.text), name_room - 12.0);
            let top = rect.center().y - (name.size().y + sub.size().y + last_g.size().y + 6.0) / 2.0;
            let (nh, sh) = (name.size().y, sub.size().y);
            p.galley(pos2(x, top), name, t.text_strong);
            p.galley(pos2(x, top + nh + 3.0), sub, t.text_faint);
            let ly = top + nh + sh + 6.0;
            p.circle_filled(pos2(x + 3.5, ly + last_g.size().y / 2.0), 3.5, fade(pip));
            p.galley(pos2(x + 12.0, ly), last_g, t.text);
        } else {
            let top = rect.center().y - (name.size().y + 3.0 + sub.size().y) / 2.0;
            let nh = name.size().y;
            p.galley(pos2(x, top), name, t.text_strong);
            p.galley(pos2(x, top + nh + 3.0), sub, t.text_faint);
            self.last_cell(ui, t, v, (rect, cols), (pip, &lead, &rest), (waiting, faded, ctx.now_ms));
            let nx = rect.left() + cols.next;
            let room = width - TOOLS_W - cols.next - 8.0;
            if over && !menu_open {
                let r = Rect::from_min_size(pos2(nx, rect.center().y - 15.0), vec2(96.0, 30.0));
                if run_button(ui, t, r, egui::Id::unique(("routine-run", &id)), "Run now").clicked() {
                    self.run_now(&id);
                }
            } else {
                let g = line(&p, next_words(v, ctx), body, fade(t.text_dim), room);
                p.galley(pos2(nx, rect.center().y - g.size().y / 2.0), g, t.text_dim);
            }
        }
        if !last {
            p.hline(rect.x_range(), rect.bottom(), Stroke::new(1.0, t.separator.gamma_multiply(0.7)));
        }
        // The switch and ⋯ at the right.
        let mut on = v.routine.enabled;
        let sw = Rect::from_min_size(pos2(rect.right() - TOOLS_W + 8.0, rect.center().y - 12.0), vec2(48.0, 24.0));
        // A child that doesn't move the list's cursor (the row has its height already).
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(sw).layout(egui::Layout::left_to_right(egui::Align::Center)));
        let r = crate::form::switch(&mut child, t, &mut on);
        let tip = if v.routine.enabled { "On: it runs on its schedule. Turn off to pause it." } else { "Off: it doesn't run on its own. Run now still works." };
        if r.on_hover_text(tip).changed() {
            self.set_enabled(&id, on);
        }
        let more_r = Rect::from_center_size(pos2(rect.right() - 18.0, rect.center().y), vec2(32.0, 32.0));
        let more = crate::projects::tool(ui, t, more_r, more_id, icons::more, None, "More");
        crate::menu::below(&more, t, 190.0).show(|ui| self.menu_items(ui, t, &id));
        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        crate::menu::context(&resp, t, 190.0).show(|ui| self.menu_items(ui, t, &id));
        if resp.clicked() {
            self.open(&id);
        }
        if self.unseen(&id) {
            p.circle_filled(pos2(rect.left() + 46.0, rect.top() + 18.0), 4.0, t.accent);
        }
        None
    }

    /// The last run column: pip, words, when; waiting for you as a highlighted pill.
    fn last_cell(
        &self,
        ui: &Ui,
        t: &Tokens,
        v: &RoutineView,
        (rect, cols): (Rect, &Cols),
        (pip, lead, rest): (Color32, &str, &str),
        (waiting, faded, now): (bool, bool, i64),
    ) {
        let p = ui.painter();
        let fade = |c: Color32| if faded { c.gamma_multiply(0.55) } else { c };
        let x = rect.left() + cols.last;
        let room = cols.next - cols.last - 20.0;
        let small = egui::FontId::proportional(t.text_size(12.5));
        let when = v.last_run.as_ref().map(|r| when_ms(r.started, now));
        let g = if waiting {
            let ink = crate::theme::mix(t.warning, t.text_strong, if t.dark { 0.1 } else { 0.45 });
            line(p, rest.to_owned(), egui::FontId::proportional(t.text_size(13.0)), ink, room - 12.0)
        } else {
            lead_line(p, ui, t, (lead, rest), fade(t.text), room - 12.0)
        };
        let block = g.size().y + if when.is_some() { 3.0 + small.size + 2.0 } else { 0.0 };
        let top = rect.center().y - block / 2.0;
        let gh = g.size().y;
        if waiting {
            let pill = Rect::from_min_size(pos2(x - 6.0, top - 2.0), g.size() + vec2(12.0, 4.0));
            p.rect_filled(pill, t.round(5.0), t.warning.gamma_multiply(if t.dark { 0.22 } else { 0.18 }));
            p.galley(pos2(x, top), g, t.text);
        } else {
            p.circle_filled(pos2(x + 3.5, top + gh / 2.0), 3.5, fade(pip));
            p.galley(pos2(x + 12.0, top), g, t.text);
        }
        if let Some(when) = when {
            let w = line(p, when, small, fade(t.text_faint), room);
            p.galley(pos2(if waiting { x } else { x + 12.0 }, top + gh + 4.0), w, t.text_faint);
        }
    }

    /// A routine's ⋯ menu (and right-click): Run now, Edit, Duplicate, Delete.
    fn menu_items(&mut self, ui: &mut Ui, t: &Tokens, id: &str) {
        crate::menu::tighten(ui);
        if crate::menu::item(ui, t, Some(icons::play), "Run now", None).clicked() {
            self.run_now(id);
            ui.close();
        }
        if crate::menu::item(ui, t, Some(icons::pencil), "Edit", None).clicked() {
            self.open(id);
            ui.close();
        }
        if crate::menu::item(ui, t, Some(icons::copy), "Duplicate", None).on_hover_text("A copy to change, off until you turn it on").clicked() {
            self.duplicate(id);
            ui.close();
        }
        crate::menu::separator(ui, t);
        if crate::menu::danger(ui, t, Some(icons::trash), "Delete", None).on_hover_text("Asks first; its runs stay in your chats").clicked() {
            self.ask_delete(id);
            ui.close();
        }
    }
}

/// "▷ Run now", outlined.
pub(crate) fn run_button(ui: &mut Ui, t: &Tokens, r: Rect, id: egui::Id, label: &str) -> egui::Response {
    let resp = ui.interact(r, id, Sense::click());
    let fill = if resp.hovered() { crate::form::soft_hover(t) } else { crate::form::card_fill(t) };
    ui.painter().rect(r, t.round(crate::form::CONTROL_R), fill, Stroke::new(1.0, t.field_border), egui::StrokeKind::Inside);
    let g = ui.painter().layout_no_wrap(label.to_owned(), egui::TextStyle::Button.resolve(ui.style()), t.text);
    let w = 14.0 + 6.0 + g.size().x;
    let x = r.center().x - w / 2.0;
    icons::play(ui.painter(), Rect::from_center_size(pos2(x + 7.0, r.center().y), vec2(13.0, 13.0)), t.text);
    ui.painter().galley(pos2(x + 20.0, r.center().y - g.size().y / 2.0), g, t.text);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Run it once now, whatever its schedule")
}

/// The list's columns, from a row's left edge.
struct Cols {
    last: f32,
    next: f32,
}

impl Cols {
    fn new(width: f32) -> Self {
        let room = (width - TEXT_X - TOOLS_W).max(60.0);
        Self { last: TEXT_X + room * 0.38, next: TEXT_X + room * 0.76 }
    }
}

/// Above a run's chat: "A run of Loom deals, today at 8:00 AM. Ask anything about it here."
/// and Open routine. True when Open routine was clicked.
pub fn run_strip(ui: &mut Ui, t: &Tokens, name: Option<&str>, when: Option<&str>) -> bool {
    let mut open = false;
    let max = t.knobs.max_width;
    let full = ui.available_width();
    let w = if max > 0.0 { full.min(max) } else { full };
    let pad = ((full - w) / 2.0).max(0.0);
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.add_space(pad);
        egui::Frame::new().fill(crate::form::soft(t)).corner_radius(t.round(crate::form::CONTROL_R)).inner_margin(egui::Margin::symmetric(12, 7)).show(
            ui,
            |ui| {
                ui.set_width(w - 24.0);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    let (r, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
                    icons::repeat(ui.painter(), r, t.text_dim);
                    ui.add_space(8.0);
                    ui.label(RichText::new("A run of ").color(t.text_dim));
                    ui.label(RichText::new(name.unwrap_or("a routine")).strong().color(t.text_strong));
                    let when = when.map(|w| format!(", {w}")).unwrap_or_default();
                    ui.label(RichText::new(format!("{when}. Ask anything about it here.")).color(t.text_dim));
                    if name.is_some() {
                        ui.add_space(12.0);
                        let r = ui.add(egui::Label::new(RichText::new("Open routine").color(t.text).underline()).sense(Sense::click()));
                        open = r.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Its settings and its other runs").clicked();
                    }
                });
            },
        );
    });
    ui.add_space(4.0);
    open
}

/// The routines as replies need them (for "Created …  Run it now" under a `create_routine`
/// call): id, name, next run. Kept in the context each frame.
#[derive(Clone, Default)]
pub struct Known(pub std::sync::Arc<Vec<(String, String, Option<i64>)>>);

fn known_id() -> egui::Id {
    egui::Id::unique("routines-known")
}

/// What a click under a reply asks: run routine `id` now, or open its page.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Ask {
    /// Nothing (what an empty slot reads as).
    #[default]
    None,
    Run(String),
    Open(String),
}

fn ask_id() -> egui::Id {
    egui::Id::unique("routines-ask")
}

impl Routines {
    /// Put this frame's routines where replies find them.
    pub fn share(&self, ctx: &egui::Context) {
        let list = self.items.iter().map(|v| (v.routine.id.clone(), v.routine.name.clone(), v.next_run.filter(|_| v.routine.enabled))).collect();
        ctx.data_mut(|d| d.insert_temp(known_id(), Known(std::sync::Arc::new(list))));
    }
}

/// A click under a reply this frame, if any.
pub fn take_ask(ctx: &egui::Context) -> Option<Ask> {
    ctx.data_mut(|d| d.remove_temp::<Ask>(ask_id())).filter(|a| *a != Ask::None)
}

/// The routine a finished `create_routine` call made (its result says "(id …)").
pub fn created_by(step: &byob_protocol::Step) -> Option<&str> {
    if step.tool != "create_routine" || step.status != byob_protocol::StepStatus::Done {
        return None;
    }
    let rest = step.detail.split_once("(id ")?.1;
    let id = rest.split(')').next()?.trim();
    byob_types::routine::id_ok(id).then_some(id)
}

/// Under a reply that made routines: "✓ Created Loom deals. Next run tomorrow at 8:00 AM."
/// and Run it now, so the first run needn't wait for the schedule.
pub fn created_lines(ui: &mut Ui, t: &Tokens, entry: &byob_protocol::Entry) {
    let made: Vec<&str> = entry.steps.iter().filter_map(created_by).collect();
    if made.is_empty() {
        return;
    }
    let known = ui.ctx().data(|d| d.get_temp::<Known>(known_id())).unwrap_or_default();
    let now = crate::sidebar::now_ms();
    for id in made {
        let Some((_, name, next)) = known.0.iter().find(|(k, _, _)| k == id).cloned() else { continue };
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            let (r, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
            icons::check(ui.painter(), r, t.success);
            ui.label(RichText::new(" Created ").color(t.text_dim));
            let link = ui.add(egui::Label::new(RichText::new(&name).color(t.text).underline()).sense(Sense::click()));
            if link.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Open the routine").clicked() {
                ui.ctx().data_mut(|d| d.insert_temp(ask_id(), Ask::Open(id.to_owned())));
            }
            let after = next.map(|n| format!(". Next run {}.", when_in_sentence(n, now))).unwrap_or_else(|| ".".to_owned());
            ui.label(RichText::new(after).color(t.text_dim));
            ui.add_space(12.0);
            let (r, _) = ui.allocate_exact_size(vec2(112.0, 28.0), Sense::hover());
            if run_button(ui, t, r, egui::Id::unique(("created-run", entry.id, id)), "Run it now").clicked() {
                ui.ctx().data_mut(|d| d.insert_temp(ask_id(), Ask::Run(id.to_owned())));
            }
        });
    }
}

fn report_id() -> egui::Id {
    egui::Id::unique("routines-run-report")
}

/// The report of the run chat in view (none elsewhere), for [`report_card`]; set each frame.
pub fn share_report(ctx: &egui::Context, report: Option<byob_protocol::routines::Report>) {
    ctx.data_mut(|d| match report {
        Some(r) => {
            d.insert_temp(report_id(), r);
        }
        None => d.remove::<byob_protocol::routines::Report>(report_id()),
    });
}

/// In a run's chat, under the reply that reported: the report as a card (what the
/// notification said), title and all.
pub fn report_card(ui: &mut Ui, t: &Tokens, entry: &byob_protocol::Entry) {
    let reported = entry.steps.iter().any(|s| s.tool == "report" && s.status == byob_protocol::StepStatus::Done);
    if !reported {
        return;
    }
    let Some(report) = ui.ctx().data(|d| d.get_temp::<byob_protocol::routines::Report>(report_id())) else { return };
    ui.add_space(6.0);
    let fill = crate::theme::mix(t.assistant_bubble, t.success, if t.dark { 0.1 } else { 0.07 });
    let edge = crate::theme::mix(t.border, t.success, 0.35);
    egui::Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0, edge))
        .corner_radius(t.round(crate::form::CARD_R - 2.0))
        .inner_margin(egui::Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let (r, _) = ui.allocate_exact_size(vec2(15.0, 15.0), Sense::hover());
                icons::star(ui.painter(), r, t.accent);
                ui.add(egui::Label::new(RichText::new(&report.title).strong().color(t.text_strong)).wrap());
            });
            if !report.body.is_empty() {
                ui.add_space(4.0);
                ui.add(egui::Label::new(RichText::new(&report.body).color(t.text)).wrap().selectable(true));
            }
        });
}

/// The note under the list: byob keeps running (or doesn't) when its window closes.
fn footer(ui: &mut Ui, t: &Tokens, ctx: &ListCtx) -> Option<Action> {
    use crate::desktop::KeepRunning;
    let text = match ctx.keep_running {
        _ if !ctx.desktop => "Routines run on the computer byob runs on, while byob is running there. This page doesn't need to stay open.",
        Some(KeepRunning::Never) => "Closing byob's window quits it, so routines only run while it's open.",
        Some(KeepRunning::Always) => "byob keeps running in the tray when you close its window, so routines can run.",
        _ => "While a routine is on, byob keeps running in the tray when you close its window, so it can run.",
    };
    let mut action = None;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.label(RichText::new(text).small().color(t.text_faint));
        if ctx.desktop {
            let r = ui.add(egui::Label::new(RichText::new("Change this").small().color(t.text_dim).underline()).sense(Sense::click()));
            if r.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Settings → System").clicked() {
                action = Some(Action::SystemSettings);
            }
        }
    });
    action
}

#[cfg(test)]
mod tests {
    use super::*;
    use byob_protocol::routines::{Report, Routine};

    fn at(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()
    }

    #[test]
    fn times_in_words() {
        let now = at("2026-11-24 10:00");
        assert_eq!(when_words(at("2026-11-24 08:02"), now), "Today, 8:02 AM");
        assert_eq!(when_words(at("2026-11-25 08:00"), now), "Tomorrow, 8:00 AM");
        assert_eq!(when_words(at("2026-11-23 20:00"), now), "Yesterday, 8:00 PM");
        assert_eq!(when_words(at("2026-11-29 18:00"), now), "Sunday, 6:00 PM");
        assert_eq!(when_words(at("2026-11-18 08:00"), now), "Wednesday, 8:00 AM");
        assert_eq!(when_words(at("2026-11-16 08:00"), now), "Nov 16");
        assert_eq!(when_words(at("2026-12-10 08:00"), now), "Dec 10, 8:00 AM");
        assert_eq!(when_words(at("2025-11-18 08:00"), now), "Nov 18, 2025");
    }

    fn view(id: &str, run: Option<RunRecord>) -> RoutineView {
        RoutineView {
            routine: Routine { id: id.into(), name: id.to_uppercase(), instructions: "x".into(), ..Routine::default() },
            last_run: run,
            ..RoutineView::default()
        }
    }

    fn reported(at: i64) -> RunRecord {
        RunRecord {
            chat: Some(format!("c{at}")),
            started: at,
            finished: Some(at + 5),
            status: RunStatus::Reported,
            report: Some(Report { title: "Found it".into(), body: String::new() }),
            ..RunRecord::default()
        }
    }

    /// A report that came after you last looked marks the routine (and the sidebar) until
    /// you look; runs from before this device first saw the routine don't.
    #[test]
    fn reports_you_havent_seen_are_marked_until_you_look() {
        let mut r = Routines::default();
        r.begin(1_000);
        r.apply(&ServerMsg::Routines { items: vec![view("old", Some(reported(10))), view("quiet", None)] });
        assert!(!r.any_unseen(), "from before");
        r.apply(&ServerMsg::Routines { items: vec![view("old", Some(reported(2_000))), view("quiet", None)] });
        assert!(r.unseen("old") && r.any_unseen());
        r.open("old");
        assert!(!r.any_unseen(), "seen on its page");
        r.apply(&ServerMsg::Routines { items: vec![view("old", Some(reported(3_000)))] });
        assert!(r.any_unseen());
        r.mark_all_seen();
        assert!(!r.any_unseen(), "seen on the list");
        let quiet = RunRecord { status: RunStatus::Quiet, started: 4_000, finished: Some(4_001), ..RunRecord::default() };
        r.apply(&ServerMsg::Routines { items: vec![view("old", Some(quiet))] });
        assert!(!r.any_unseen(), "nothing new isn't news");
    }

    /// A new routine (or a copy) opens once the engine lists it; one deleted elsewhere
    /// while its page is open goes back to the list.
    #[test]
    fn a_saved_routine_opens_and_a_deleted_one_closes() {
        let mut r = Routines::default();
        r.apply(&ServerMsg::Routines { items: vec![view("a", None)] });
        r.new_routine(None);
        r.save(Routine { name: "B".into(), instructions: "y".into(), ..Routine::default() });
        assert!(matches!(r.take().as_slice(), [ClientMsg::SaveRoutine { .. }]));
        r.apply(&ServerMsg::Routines { items: vec![view("a", None), view("b", None)] });
        assert_eq!(r.view, View::Routine("b".into()));
        assert_eq!(r.take(), vec![ClientMsg::ListRoutineRuns { id: "b".into() }]);
        r.apply(&ServerMsg::Routines { items: vec![view("a", None)] });
        assert_eq!(r.view, View::List);
    }

    #[test]
    fn made_up_runs_are_those_well_after_their_slot() {
        let on_time = RunRecord { cause: RunCause::Schedule, slot: Some(0), started: 60_000, ..RunRecord::default() };
        let late = RunRecord { started: 3 * 60 * 60 * 1000, ..on_time.clone() };
        assert!(!made_up(&on_time) && made_up(&late));
        assert!(!made_up(&RunRecord { cause: RunCause::Manual, ..late }));
    }
}
