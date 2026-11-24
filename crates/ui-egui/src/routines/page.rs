//! A routine's page: its fields on the left (name, what to do, when, what it may do on its
//! own, when it tells you, and more), its runs on the right (each opens its chat), one column
//! on a phone. A new routine is the same page with Create routine. Changes wait for Save,
//! as a routine acts later without you: you see the whole of it before it counts.

use byob_protocol::routines::{Missed, Routine, RoutineView, RunRecord, RunStatus, Tell, Trigger};
use byob_types::config::Approvals;
use byob_types::routine::CHANNEL_DESKTOP;
use egui::{Rect, RichText, Sense, Stroke, Ui, pos2, vec2};

use super::schedule::{DAY_NAMES, Freq, HOURS, INITIALS, When, clock, ordinal};
use super::{Action, ListCtx, Routines, View, made_up, short, when_ms};
use crate::icons;
use crate::theme::Tokens;

/// A routine to start from (the empty page's cards).
pub struct Starter {
    pub icon: icons::Paint,
    pub title: &'static str,
    pub sub: &'static str,
    pub name: &'static str,
    pub instructions: &'static str,
    pub when: When,
}

const fn when(freq: Freq, days: [bool; 7], hours: u32, at: (u32, u32)) -> When {
    When { freq, days, hours, month_day: 1, at, zone: None, start: None, custom: None }
}

/// The routines the empty page offers to start from (the brackets are yours to fill in).
pub const STARTERS: [Starter; 3] = [
    Starter {
        icon: icons::globe,
        title: "Deals on something you want",
        sub: "Every morning, look for a better price",
        name: "Deal watch",
        instructions: "Search the web for deals on [what you want]. Compare with what earlier runs found and only report a lower price or something we haven't seen.",
        when: when(Freq::Daily, [true; 7], 2, (8, 0)),
    },
    Starter {
        icon: icons::megaphone,
        title: "News on a topic",
        sub: "Every Monday, the week's most important stories",
        name: "Weekly news",
        instructions: "Find this week's news about [a topic]. Report the three most important stories with a line each and their links. Skip what earlier runs already reported.",
        when: when(Freq::Weekly, [true, false, false, false, false, false, false], 2, (9, 0)),
    },
    Starter {
        icon: icons::eye,
        title: "Watch a page for changes",
        sub: "Every few hours, tell me when it changes",
        name: "Page watch",
        instructions: "Read [the page's address] and report what changed since the last run (new items, prices, dates). If nothing changed, don't report.",
        when: when(Freq::Hourly, [true; 7], 4, (8, 0)),
    },
];

/// What the page needs from the app besides the routines.
#[derive(Default)]
pub struct PageCtx {
    /// Projects a routine can work in: (id, name).
    pub projects: Vec<(String, String)>,
    /// Saved models: (id, name).
    pub models: Vec<(String, String)>,
    /// The default model's name.
    pub default_model: String,
}

/// The routine page's form.
#[derive(Debug, Clone)]
pub struct Editor {
    /// The routine's id; empty for a new one.
    pub id: String,
    /// As saved (a new one: the form it started from).
    saved: Routine,
    pub name: String,
    pub instructions: String,
    pub when: When,
    pub allowed: Approvals,
    pub tell: Tell,
    /// Tell me with a desktop notification.
    pub notify: bool,
    pub project: Option<String>,
    pub model: Option<String>,
    pub missed: Missed,
    /// Why Save didn't go (a field is missing).
    problem: Option<String>,
}

impl Editor {
    /// The form for a saved routine.
    pub fn of(r: &Routine) -> Self {
        Self {
            id: r.id.clone(),
            saved: r.clone(),
            name: r.name.clone(),
            instructions: r.instructions.clone(),
            when: When::of(&r.when),
            allowed: r.allowed,
            tell: r.tell,
            notify: r.channels.iter().any(|c| c == CHANNEL_DESKTOP),
            project: r.project.clone(),
            model: r.model.clone(),
            missed: r.missed,
            problem: None,
        }
    }

    /// A new routine's form, from a starter or empty (every day at 8:00 AM).
    pub fn new(starter: Option<&Starter>) -> Self {
        let mut e = Self::of(&Routine::default());
        e.when = When::default();
        if let Some(s) = starter {
            s.name.clone_into(&mut e.name);
            s.instructions.clone_into(&mut e.instructions);
            e.when = s.when.clone();
        }
        e
    }

    pub fn is_new(&self) -> bool {
        self.id.is_empty()
    }

    /// Its triggers: the saved ones while When is as it was (so a rule is kept word for
    /// word), else the controls' schedule first and the other triggers after it.
    fn triggers(&self) -> Result<Vec<Trigger>, String> {
        if !self.is_new() && self.when == When::of(&self.saved.when) {
            return Ok(self.saved.when.clone());
        }
        let mut first = true;
        let others = self.saved.when.iter().filter(|t| !matches!(t, Trigger::Schedule { .. }) || !std::mem::replace(&mut first, false)).cloned();
        Ok(self.when.trigger()?.into_iter().chain(others).collect())
    }

    /// The routine the form says, or what's missing.
    pub fn routine(&self) -> Result<Routine, String> {
        let mut r = self.saved.clone();
        self.name.trim().clone_into(&mut r.name);
        self.instructions.trim().clone_into(&mut r.instructions);
        if r.name.is_empty() {
            return Err("Give it a name.".into());
        }
        if r.instructions.is_empty() {
            return Err("Say what it should do.".into());
        }
        r.when = self.triggers()?;
        r.allowed = self.allowed;
        r.tell = self.tell;
        r.channels.retain(|c| c != CHANNEL_DESKTOP);
        if self.notify {
            r.channels.insert(0, CHANNEL_DESKTOP.to_owned());
        }
        r.project.clone_from(&self.project);
        r.model.clone_from(&self.model);
        r.missed = self.missed;
        Ok(r)
    }

    /// The form differs from what's saved.
    pub fn changed(&self) -> bool {
        self.routine().map_or(true, |r| r != self.saved)
    }
}

/// What a routine may do on its own, in plain words.
fn allowed_label(a: Approvals) -> &'static str {
    match a {
        Approvals::ReadOnly => "Read only: search and read the web, read files",
        Approvals::Ask => "Read the web and files; ask before anything else",
        Approvals::AllowEdits => "Read, and change files in its folder",
        Approvals::AllowAll => "Work freely: anything but what always asks you",
        Approvals::Yolo => "Anything, without ever asking",
    }
}

/// What waits for you at that setting.
fn allowed_note(a: Approvals) -> &'static str {
    match a {
        Approvals::AllowAll => "Sending email and the like still wait for you. If nobody answers within 12 hours, that step is skipped.",
        Approvals::Yolo => "Nothing waits for you, not even sending things. Use this only for routines you trust completely.",
        _ => "Anything else waits for you. If nobody answers within 12 hours, that step is skipped.",
    }
}

/// A small label above a field.
fn label(ui: &mut Ui, t: &Tokens, text: &str) {
    ui.label(RichText::new(text).size(t.text_size(13.0)).color(t.text_dim));
    ui.add_space(4.0);
}

/// A select whose choices are listed by index (they scroll when there are many). Returns
/// the index picked this frame.
fn pick(ui: &mut Ui, t: &Tokens, current: &str, choices: &[String], on: Option<usize>, min_w: f32) -> Option<usize> {
    let b = crate::form::pill(ui, t, min_w, None, current);
    let mut picked = None;
    crate::menu::below(&b, t, b.rect.width().max(200.0)).show(|ui| {
        crate::menu::tighten(ui);
        egui::ScrollArea::vertical().max_height(280.0).show(ui, |ui| {
            for (i, c) in choices.iter().enumerate() {
                if crate::menu::item(ui, t, None, c, (on == Some(i)).then_some("✓")).clicked() {
                    picked = Some(i);
                    ui.close();
                }
            }
        });
    });
    picked
}

/// Times of day every half hour (and `at`, if it's between them).
fn times(at: (u32, u32)) -> Vec<(u32, u32)> {
    let mut all: Vec<(u32, u32)> = (0..48).map(|i| (i / 2, (i % 2) * 30)).collect();
    if !all.contains(&at) {
        all.push(at);
        all.sort_unstable();
    }
    all
}

impl Routines {
    /// A routine's page (or a new one's).
    pub(super) fn routine_page(&mut self, ui: &mut Ui, t: &Tokens, ctx: &ListCtx, more: &PageCtx) -> Option<Action> {
        let Some(mut ed) = self.editor.take() else {
            self.view = View::List;
            return None;
        };
        let view = (!ed.is_new()).then(|| self.get(&ed.id).cloned()).flatten();
        let mut action = None;
        let page_w = (ui.available_width() - 48.0).clamp(240.0, 1040.0);
        let wide = page_w >= 760.0;
        egui::ScrollArea::vertical().id_salt(("routine-page", ed.id.as_str())).auto_shrink([false, false]).show(ui, |ui| {
            ui.add_space(14.0);
            let extra = ((ui.available_width() - page_w) / 2.0).max(0.0);
            let rect = Rect::from_min_size(ui.cursor().min + vec2(extra, 0.0), vec2(page_w, f32::INFINITY));
            ui.scope_builder(egui::UiBuilder::new().max_rect(rect).layout(egui::Layout::top_down(egui::Align::Min)), |ui| {
                ui.set_max_width(page_w);
                self.page_header(ui, t, &ed, view.as_ref(), wide);
                ui.add_space(16.0);
                if wide {
                    let runs_w = 300.0;
                    let form_w = page_w - runs_w - 48.0;
                    ui.horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = 48.0;
                        ui.allocate_ui_with_layout(vec2(form_w, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                            ui.set_width(form_w);
                            self.form(ui, t, &mut ed, view.as_ref(), more);
                        });
                        ui.allocate_ui_with_layout(vec2(runs_w, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                            ui.set_width(runs_w);
                            action = self.runs_column(ui, t, &ed, view.as_ref(), ctx.now_ms);
                        });
                    });
                } else {
                    self.form(ui, t, &mut ed, view.as_ref(), more);
                    ui.add_space(28.0);
                    action = self.runs_column(ui, t, &ed, view.as_ref(), ctx.now_ms);
                }
            });
            ui.add_space(40.0);
        });
        // Something on the page may have opened another view (the list, a copy).
        if self.editor.is_none() && matches!(self.view, View::Routine(_) | View::New) {
            self.editor = Some(ed);
        }
        action
    }

    /// "Routines › Loom deals", and its switch, Run now and ⋯ (a saved routine).
    fn page_header(&mut self, ui: &mut Ui, t: &Tokens, ed: &Editor, view: Option<&RoutineView>, wide: bool) {
        let title = if ed.is_new() { "New routine".to_owned() } else { view.map_or_else(|| ed.name.clone(), |v| v.routine.name.clone()) };
        let crumbs = |ui: &mut Ui, this: &mut Self| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let back = ui.add(egui::Label::new(RichText::new("Routines").color(t.text_dim)).sense(Sense::click()));
            if back.hovered() {
                ui.painter().hline(back.rect.x_range(), back.rect.bottom(), Stroke::new(1.0, t.text_dim));
            }
            if back.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("All routines").clicked() {
                this.show_list();
            }
            ui.label(RichText::new("›").color(t.text_faint));
            ui.add(egui::Label::new(RichText::new(title.as_str()).color(t.text_strong).size(t.text_size(15.0))).truncate());
        };
        let Some(v) = view else {
            ui.horizontal(|ui| crumbs(ui, self));
            return;
        };
        let id = v.routine.id.clone();
        let tools = |ui: &mut Ui, this: &mut Self| {
            ui.spacing_mut().item_spacing.x = 10.0;
            let more_r = ui.allocate_exact_size(vec2(32.0, 32.0), Sense::hover()).0;
            let more = crate::projects::tool(ui, t, more_r, egui::Id::unique(("routine-page-more", &id)), icons::more, None, "More");
            crate::menu::below(&more, t, 190.0).show(|ui| {
                crate::menu::tighten(ui);
                if crate::menu::item(ui, t, Some(icons::copy), "Duplicate", None).clicked() {
                    this.duplicate(&id);
                    ui.close();
                }
                crate::menu::separator(ui, t);
                if crate::menu::danger(ui, t, Some(icons::trash), "Delete", None).clicked() {
                    this.ask_delete(&id);
                    ui.close();
                }
            });
            let run_r = ui.allocate_exact_size(vec2(104.0, 32.0), Sense::hover()).0;
            if super::run_button(ui, t, run_r, egui::Id::unique(("routine-page-run", &id)), "Run now").clicked() {
                this.run_now(&id);
            }
            let mut on = v.routine.enabled;
            let tip = if on { "On: it runs on its schedule" } else { "Off: it doesn't run on its own (Run now still works)" };
            if crate::form::switch(ui, t, &mut on).on_hover_text(tip).changed() {
                this.set_enabled(&id, on);
            }
        };
        if wide {
            ui.horizontal(|ui| {
                ui.set_min_height(34.0);
                crumbs(ui, self);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| tools(ui, self));
            });
        } else {
            ui.horizontal(|ui| crumbs(ui, self));
            ui.add_space(8.0);
            ui.allocate_ui_with_layout(vec2(ui.available_width(), 34.0), egui::Layout::right_to_left(egui::Align::Center), |ui| tools(ui, self));
        }
    }

    /// The fields, then Save.
    fn form(&mut self, ui: &mut Ui, t: &Tokens, ed: &mut Editor, view: Option<&RoutineView>, more: &PageCtx) {
        let w = ui.available_width();
        label(ui, t, "Name");
        let id = egui::Id::unique(("routine-name", ed.id.as_str()));
        crate::workspace::form_field(ui, t, id, |frame| {
            egui::TextEdit::singleline(&mut ed.name)
                .id(id)
                .hint_text("Loom deals")
                .char_limit(byob_types::routine::MAX_NAME_CHARS)
                .desired_width(w)
                .frame(frame)
        });
        ui.add_space(18.0);
        label(ui, t, "What to do");
        let id = egui::Id::unique(("routine-instructions", ed.id.as_str()));
        crate::workspace::form_field(ui, t, id, |frame| {
            egui::TextEdit::multiline(&mut ed.instructions)
                .id(id)
                .hint_text("Search the web for deals on a new floor loom. Only tell me about a lower price or a model we haven't seen.")
                .desired_rows(3)
                .desired_width(w)
                .frame(frame)
        });
        ui.add_space(18.0);
        when_field(ui, t, ed, view);
        ui.add_space(20.0);
        label(ui, t, "It may, on its own");
        let mut choices = vec![Approvals::ReadOnly, Approvals::AllowEdits, Approvals::AllowAll];
        if !choices.contains(&ed.allowed) {
            choices.push(ed.allowed);
        }
        let names: Vec<String> = choices.iter().map(|a| allowed_label(*a).to_owned()).collect();
        if let Some(i) = pick(ui, t, allowed_label(ed.allowed), &names, choices.iter().position(|a| *a == ed.allowed), 200.0) {
            ed.allowed = choices.get(i).copied().unwrap_or(ed.allowed);
        }
        ui.add_space(4.0);
        ui.add(egui::Label::new(RichText::new(allowed_note(ed.allowed)).small().color(t.text_faint)).wrap());
        ui.add_space(20.0);
        label(ui, t, "Tell me");
        for (tell, text) in [(Tell::Found, "When it finds something"), (Tell::Always, "After every run"), (Tell::Never, "Never; I'll look")] {
            ui.radio_value(&mut ed.tell, tell, RichText::new(text).color(t.text));
        }
        ui.add_space(6.0);
        ui.add_enabled_ui(ed.tell != Tell::Never, |ui| {
            ui.checkbox(&mut ed.notify, RichText::new("With a notification").color(t.text)).on_hover_text("On this computer, and on others where byob is open");
        });
        ui.add_space(20.0);
        more_fields(ui, t, ed, more);
        ui.add_space(22.0);
        self.save_bar(ui, t, ed);
    }

    /// Save and Discard once something changed (Create routine and Cancel for a new one).
    fn save_bar(&mut self, ui: &mut Ui, t: &Tokens, ed: &mut Editor) {
        let new = ed.is_new();
        if !new && !ed.changed() {
            return;
        }
        if let Some(p) = &ed.problem {
            ui.label(RichText::new(p).color(t.danger));
            ui.add_space(6.0);
        }
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let save = crate::workspace::dialog_button(ui, t, if new { "Create routine" } else { "Save" }, true, true, 34.0);
            if save.clicked() {
                match ed.routine() {
                    Ok(r) => {
                        ed.problem = None;
                        self.save(r);
                    }
                    Err(e) => ed.problem = Some(e),
                }
            }
            if crate::workspace::dialog_button(ui, t, if new { "Cancel" } else { "Discard" }, false, true, 34.0).clicked() {
                if new {
                    self.show_list();
                } else {
                    *ed = Editor::of(&ed.saved);
                }
            }
            if !new {
                ui.label(RichText::new("Changes count once saved.").small().color(t.text_faint));
            }
        });
    }

    /// Its runs, newest first: what each found, when; a click opens its chat. Then what it
    /// costs.
    fn runs_column(&mut self, ui: &mut Ui, t: &Tokens, ed: &Editor, view: Option<&RoutineView>, now: i64) -> Option<Action> {
        ui.label(RichText::new("Runs").strong().color(t.text_strong));
        ui.add_space(6.0);
        let w = ui.available_width();
        let (r, _) = ui.allocate_exact_size(vec2(w, 1.0), Sense::hover());
        ui.painter().hline(r.x_range(), r.center().y, Stroke::new(1.0, t.separator));
        let Some(v) = view else {
            ui.add_space(8.0);
            ui.add(egui::Label::new(RichText::new("Each run is a chat you can open and ask about. They show up here.").small().color(t.text_faint)).wrap());
            return None;
        };
        let runs: Vec<RunRecord> = self.runs(&ed.id).to_vec();
        let mut action = None;
        if runs.is_empty() {
            ui.add_space(8.0);
            ui.add(egui::Label::new(RichText::new("No runs yet. Run now shows you what a run does.").small().color(t.text_faint)).wrap());
        }
        for (i, run) in runs.iter().take(30).enumerate() {
            match run_item(ui, t, run, now, (w, i)) {
                Some(RunClick::Open(chat)) => action = Some(Action::OpenChat(chat)),
                Some(RunClick::Again) => self.run_now(&ed.id),
                None => {}
            }
        }
        ui.add_space(12.0);
        let mut cost = Vec::new();
        if let Some(n) = v.runs_per_month.filter(|n| *n > 0) {
            cost.push(format!("About {} a month.", crate::sidebar::plural(usize::try_from(n).unwrap_or(0), "run")));
        }
        if let Some(tokens) = runs.iter().find(|r| r.tokens > 0).map(|r| r.tokens) {
            cost.push(format!("The last run used {} tokens.", byob_protocol::usage::short_tokens(tokens)));
        }
        if !cost.is_empty() {
            ui.add(egui::Label::new(RichText::new(cost.join(" ")).small().color(t.text_faint)).wrap());
        }
        action
    }
}

/// What a click on a run asks.
enum RunClick {
    Open(String),
    Again,
}

/// One run: a pip and what happened (a report's title in bold), the report's first lines,
/// when (and a note if it was made up), Run again on a failure.
fn run_item(ui: &mut Ui, t: &Tokens, run: &RunRecord, now: i64, (w, i): (f32, usize)) -> Option<RunClick> {
    let (pip, lead, rest) = super::last_run_words(t, Some(run));
    let (title, strong) = match (&run.report, run.status) {
        (Some(rep), _) => (short(&rep.title, 140), true),
        (None, RunStatus::Failed) => (format!("{lead} {rest}"), false),
        _ => (rest, false),
    };
    let body = run.report.as_ref().map(|r| short(&r.body, 400)).filter(|b| !b.is_empty());
    let mut meta = when_ms(run.started, now);
    if made_up(run) {
        meta.push_str(" · made up after byob was closed or asleep");
    }
    let top = ui.cursor().top();
    let mut again = None;
    ui.add_space(10.0);
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let (dot, _) = ui.allocate_exact_size(vec2(7.0, ui.text_style_height(&egui::TextStyle::Body)), Sense::hover());
        ui.painter().circle_filled(dot.center(), 3.5, pip);
        ui.vertical(|ui| {
            ui.set_width(w - 15.0);
            ui.spacing_mut().item_spacing.y = 2.0;
            let mut text = RichText::new(title).color(if strong { t.text_strong } else { t.text });
            if strong {
                text = text.strong();
            }
            // The title on one line (in full on hover); two lines of the report under it.
            ui.add(egui::Label::new(text).truncate().selectable(false));
            if let Some(b) = body {
                let font = egui::TextStyle::Small.resolve(ui.style());
                let mut job = egui::text::LayoutJob::simple(b, font, t.text_dim, ui.available_width());
                job.wrap.max_rows = 2;
                ui.add(egui::Label::new(job).selectable(false));
            }
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.label(RichText::new(meta).small().color(t.text_faint));
                if run.status == RunStatus::Failed {
                    ui.label(RichText::new("·").small().color(t.text_faint));
                    let r = ui.add(egui::Label::new(RichText::new("Run again").small().color(t.text_dim).underline()).sense(Sense::click()));
                    again = Some(r.rect);
                }
            });
        });
    });
    ui.add_space(10.0);
    let rect = Rect::from_min_max(pos2(ui.max_rect().left(), top), pos2(ui.max_rect().left() + w, ui.cursor().top()));
    ui.painter().hline(rect.x_range(), rect.bottom(), Stroke::new(1.0, t.separator.gamma_multiply(0.7)));
    let resp = ui.interact(rect, egui::Id::unique(("routine-run", i, run.started)), Sense::click());
    let over_again = again.is_some_and(|r| resp.hover_pos().is_some_and(|p| r.expand(2.0).contains(p)));
    if resp.hovered() && (run.chat.is_some() || over_again) {
        ui.painter().rect_filled(rect.shrink2(vec2(-6.0, 1.0)), t.round(6.0), crate::form::row_hover(t));
    }
    if over_again {
        return resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked().then_some(RunClick::Again);
    }
    match &run.chat {
        Some(chat) => {
            resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Open this run's chat").clicked().then(|| RunClick::Open(chat.clone()))
        }
        None => None,
    }
}

/// When: how often, the time ("your time"), the days; what it says in words, and when it's
/// next while unchanged.
fn when_field(ui: &mut Ui, t: &Tokens, ed: &mut Editor, view: Option<&RoutineView>) {
    label(ui, t, "When");
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
        let mut freqs: Vec<Freq> = Freq::CHOICES.iter().map(|(f, _)| *f).collect();
        if ed.when.freq == Freq::Custom {
            freqs.insert(0, Freq::Custom);
        }
        let names: Vec<String> = freqs.iter().map(|f| f.label().to_owned()).collect();
        if let Some(i) = pick(ui, t, ed.when.freq.label(), &names, freqs.iter().position(|f| *f == ed.when.freq), 140.0) {
            ed.when.set_freq(freqs.get(i).copied().unwrap_or(Freq::Daily));
        }
        match ed.when.freq {
            Freq::Manual => return,
            Freq::Hourly => {
                ui.label(RichText::new("every").color(t.text_dim));
                let names: Vec<String> = HOURS.iter().map(|h| if *h == 1 { "hour".to_owned() } else { format!("{h} hours") }).collect();
                let current = names.get(HOURS.iter().position(|h| *h == ed.when.hours).unwrap_or(1)).cloned().unwrap_or_default();
                if let Some(i) = pick(ui, t, &current, &names, HOURS.iter().position(|h| *h == ed.when.hours), 90.0) {
                    ed.when.hours = HOURS.get(i).copied().unwrap_or(2);
                }
                ui.label(RichText::new("from").color(t.text_dim));
            }
            Freq::Monthly => {
                ui.label(RichText::new("on the").color(t.text_dim));
                let days: Vec<i32> = (1..=31).chain([-1]).collect();
                let names: Vec<String> = days.iter().map(|d| if *d == -1 { "last day".to_owned() } else { ordinal(*d) }).collect();
                let on = days.iter().position(|d| *d == ed.when.month_day);
                if let Some(i) = pick(ui, t, &names.get(on.unwrap_or(0)).cloned().unwrap_or_default(), &names, on, 80.0) {
                    ed.when.month_day = days.get(i).copied().unwrap_or(1);
                }
                ui.label(RichText::new("at").color(t.text_dim));
            }
            _ => {
                ui.label(RichText::new("at").color(t.text_dim));
            }
        }
        let all = times(ed.when.at);
        let names: Vec<String> = all.iter().map(|(h, m)| clock(*h, *m)).collect();
        if let Some(i) = pick(ui, t, &clock(ed.when.at.0, ed.when.at.1), &names, all.iter().position(|a| *a == ed.when.at), 100.0) {
            ed.when.at = all.get(i).copied().unwrap_or((8, 0));
        }
        let zone = ed.when.zone.clone().unwrap_or_else(|| "the time zone of the computer byob runs on".to_owned());
        ui.label(RichText::new("your time").small().color(t.text_faint)).on_hover_text(zone);
    });
    if ed.when.shows_days() {
        ui.add_space(8.0);
        day_buttons(ui, t, &mut ed.when);
    }
    // In words: the engine's, while it's as saved; else the controls' own.
    let unchanged = !ed.is_new() && ed.when == When::of(&ed.saved.when);
    let words = match view {
        Some(v) if unchanged => super::when_line(v),
        _ => ed.when.words(),
    };
    let next = view.filter(|_| unchanged).and_then(|v| v.next_run.filter(|_| v.routine.enabled));
    let now = crate::sidebar::now_ms();
    let text = match next {
        Some(n) if !words.is_empty() => format!("{words}. Next: {}.", when_ms(n, now)),
        _ => words,
    };
    if !text.is_empty() {
        ui.add_space(6.0);
        ui.add(egui::Label::new(RichText::new(text).small().color(t.text_faint)).wrap());
    }
}

/// M T W T F S S: the days it runs, filled; a click adds or takes one away.
fn day_buttons(ui: &mut Ui, t: &Tokens, when: &mut When) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        for i in 0..7 {
            let (r, resp) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::click());
            let on = when.days[i];
            let (fill, ink, edge) = if on { (t.text_strong, t.app_bg, t.text_strong) } else { (egui::Color32::TRANSPARENT, t.text_dim, t.field_border) };
            let fill = if resp.hovered() && !on { crate::form::soft(t) } else { fill };
            ui.painter().circle(r.center(), 14.0, fill, Stroke::new(1.0, edge));
            let font = egui::FontId::proportional(t.text_size(12.5));
            ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, INITIALS[i], font, ink);
            let name = DAY_NAMES[i];
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(name).clicked() {
                let mut days = when.days;
                days[i] = !on;
                when.set_days(days);
            }
        }
    });
}

/// More: where it works, its model, and what happens to runs missed while byob was closed.
fn more_fields(ui: &mut Ui, t: &Tokens, ed: &mut Editor, more: &PageCtx) {
    label(ui, t, "More");
    let w = ui.available_width();
    let label_w = if w < 420.0 { 0.0 } else { 150.0 };
    let row = |ui: &mut Ui, name: &str, add: &mut dyn FnMut(&mut Ui)| {
        if label_w == 0.0 {
            ui.label(RichText::new(name).small().color(t.text_dim));
            add(ui);
            ui.add_space(8.0);
        } else {
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(label_w, 32.0), Sense::hover());
                let g = ui.painter().layout_no_wrap(name.to_owned(), egui::TextStyle::Body.resolve(ui.style()), t.text_dim);
                ui.painter().galley(pos2(r.left(), r.center().y - g.size().y / 2.0), g, t.text_dim);
                add(ui);
            });
            ui.add_space(6.0);
        }
    };
    // Works in: its own folder, or a project.
    let mut places: Vec<(Option<String>, String)> = vec![(None, "Its own folder".to_owned())];
    places.extend(more.projects.iter().map(|(id, name)| (Some(id.clone()), name.clone())));
    if let Some(p) = &ed.project
        && !places.iter().any(|(id, _)| id.as_ref() == Some(p))
    {
        places.push((Some(p.clone()), "A project that's gone".to_owned()));
    }
    row(ui, "Works in", &mut |ui| {
        let on = places.iter().position(|(id, _)| *id == ed.project);
        let names: Vec<String> = places.iter().map(|(_, n)| n.clone()).collect();
        let current = on.and_then(|i| names.get(i)).cloned().unwrap_or_default();
        if let Some(i) = pick(ui, t, &current, &names, on, 180.0) {
            ed.project = places.get(i).and_then(|(id, _)| id.clone());
        }
    });
    let default = if more.default_model.is_empty() { "Default model".to_owned() } else { format!("Default ({})", more.default_model) };
    let mut models: Vec<(Option<String>, String)> = vec![(None, default)];
    models.extend(more.models.iter().map(|(id, name)| (Some(id.clone()), name.clone())));
    if let Some(m) = &ed.model
        && !models.iter().any(|(id, _)| id.as_ref() == Some(m))
    {
        models.push((Some(m.clone()), m.clone()));
    }
    row(ui, "Model", &mut |ui| {
        let on = models.iter().position(|(id, _)| *id == ed.model);
        let names: Vec<String> = models.iter().map(|(_, n)| n.clone()).collect();
        let current = on.and_then(|i| names.get(i)).cloned().unwrap_or_default();
        if let Some(i) = pick(ui, t, &current, &names, on, 180.0) {
            ed.model = models.get(i).and_then(|(id, _)| id.clone());
        }
    });
    row(ui, "If byob was closed", &mut |ui| {
        let choices = [(Missed::Once, "Run once when it's back"), (Missed::Skip, "Skip that run")];
        let names: Vec<String> = choices.iter().map(|(_, n)| (*n).to_owned()).collect();
        let on = choices.iter().position(|(m, _)| *m == ed.missed);
        let current = on.and_then(|i| names.get(i)).cloned().unwrap_or_default();
        if let Some(i) = pick(ui, t, &current, &names, on, 180.0).and_then(|i| choices.get(i)) {
            ed.missed = i.0;
        }
    });
}

/// `ClientMsg` for tests.
#[cfg(test)]
pub(crate) fn sent_saves(msgs: &[byob_protocol::ClientMsg]) -> Vec<Routine> {
    msgs.iter()
        .filter_map(|m| match m {
            byob_protocol::ClientMsg::SaveRoutine { routine } => Some(routine.clone()),
            _ => None,
        })
        .collect()
}
