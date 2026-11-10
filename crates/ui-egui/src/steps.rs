//! An assistant entry's body: reply text interleaved with its tool steps (each step sits
//! where it happened in the text, by [`Step::at`]), approval cards (Allow / Always / Deny)
//! for steps waiting on the user, and the "Undo changes" action. Any client can answer a
//! card. A `delegate` step expands into its subagent's live transcript (text and steps,
//! the same way), with a status line like "explorer · 3 tools · running".

use std::ops::Range;

use byob_protocol::{ApprovalAnswer, ClientMsg, Delegation, DelegationState, DiffLine, Entry, FileChange, LineKind, Preview, Step, StepStatus, ToolAccess};
use egui::{Color32, Pos2, Rect, RichText, Sense, Stroke, Vec2, pos2, vec2};

use crate::markdown;
use crate::theme::{Tokens, bold_family, mix};

/// Apply a `ServerMsg::StepUpdated` to the local transcript.
pub fn apply(entries: &mut [Entry], entry: u64, step: Step) {
    if let Some(e) = entries.iter_mut().rev().find(|e| e.id == entry) {
        e.upsert_step(step);
    }
}

/// Apply a `ServerMsg::EntryUndo`.
pub fn apply_undo(entries: &mut [Entry], id: u64, undoable: bool, undone: bool) {
    if let Some(e) = entries.iter_mut().rev().find(|e| e.id == id) {
        e.undoable = undoable;
        e.undone = undone;
    }
}

/// Some entry has an approval card waiting for the user.
pub fn awaiting_approval(entries: &[Entry]) -> bool {
    entries.iter().any(|e| e.pending_approvals().next().is_some())
}

/// A piece of an entry's body, in display order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment<'a> {
    /// Reply text; `part` numbers the text pieces (stable while the reply streams).
    Text { part: usize, text: &'a str },
    /// A run of consecutive steps: indices into `Entry::steps`.
    Steps(Range<usize>),
}

/// Split `content` at the steps' offsets. Offsets are untrusted: they're clamped to the
/// text, to char boundaries, and to never go backwards. Whitespace-only text is dropped.
pub fn segments<'a>(content: &'a str, steps: &[Step]) -> Vec<Segment<'a>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    let mut part = 0usize;
    let mut text = |out: &mut Vec<Segment<'a>>, piece: &'a str| {
        let piece = piece.trim_start_matches(['\n', '\r']);
        if !piece.trim().is_empty() {
            out.push(Segment::Text { part, text: piece });
            part += 1;
        }
    };
    for (i, step) in steps.iter().enumerate() {
        let mut at = step.at.clamp(pos, content.len());
        while !content.is_char_boundary(at) {
            at -= 1; // terminates: 0 and `pos` are boundaries
        }
        if at > pos {
            text(&mut out, content.get(pos..at).unwrap_or_default());
            pos = at;
        }
        match out.last_mut() {
            Some(Segment::Steps(r)) if r.end == i => r.end = i + 1,
            _ => out.push(Segment::Steps(i..i + 1)),
        }
    }
    text(&mut out, content.get(pos..).unwrap_or_default());
    out
}

/// Draw an assistant entry's text and steps, interleaved, plus the undo action. Returns a
/// message to send if the user answered a card or asked to undo.
/// `cards` draws the artifact cards of a step that made any, right below its row.
pub fn body(ui: &mut egui::Ui, t: &Tokens, entry: &Entry, cache: &mut markdown::Cache, cards: &mut dyn FnMut(&mut egui::Ui, &Step)) -> Option<ClientMsg> {
    let mut msg = transcript(ui, t, Thread { key: entry.id, content: &entry.content, steps: &entry.steps, done: entry.done, agent: None }, cache, cards);
    if entry.done && (entry.undoable || entry.undone) {
        msg = msg.or(undo_row(ui, t, entry));
    }
    msg
}

/// Text and steps of a reply, or of a subagent's run.
struct Thread<'a> {
    /// Markdown cache key (an entry id, or [`delegation_key`]).
    key: u64,
    content: &'a str,
    steps: &'a [Step],
    done: bool,
    /// The subagent this is, for its approval cards.
    agent: Option<&'a str>,
}

/// Markdown cache key of a delegation's text: step ids and entry ids come from different
/// counters, so step ids get the top bit.
fn delegation_key(step: u64) -> u64 {
    step | (1 << 63)
}

fn transcript(ui: &mut egui::Ui, t: &Tokens, th: Thread<'_>, cache: &mut markdown::Cache, cards: &mut dyn FnMut(&mut egui::Ui, &Step)) -> Option<ClientMsg> {
    let mut msg = None;
    let segs = segments(th.content, th.steps);
    let last = segs.len().saturating_sub(1);
    for (k, seg) in segs.into_iter().enumerate() {
        match seg {
            Segment::Text { part, text } => {
                let doc = cache.part(th.key, part, text, th.done || k < last);
                markdown::show(ui, t, doc, egui::Id::unique(("md", th.key, part)));
            }
            Segment::Steps(range) => {
                ui.add_space(2.0);
                for step in th.steps.get(range).unwrap_or_default() {
                    if step.status == StepStatus::Waiting {
                        msg = msg.or(approval_card(ui, t, step, th.agent));
                    } else if let Some(d) = step.delegation.as_deref() {
                        msg = msg.or(delegation(ui, t, step, d, cache, cards));
                    } else {
                        row(ui, t, step);
                        if !step.artifacts.is_empty() {
                            cards(ui, step);
                        }
                    }
                }
                ui.add_space(2.0);
            }
        }
    }
    msg
}

/// Tallest a running subagent's transcript gets before it scrolls (several can run at once).
const DELEGATION_MAX_HEIGHT: f32 = 260.0;
/// Lines of a delegation's task shown above its transcript.
const TASK_LINES: usize = 3;

/// A `delegate` step: a frame with the subagent's status line and task, expanding into its
/// transcript. Open while it runs (or waits for approval), collapsed once done.
fn delegation(
    ui: &mut egui::Ui,
    t: &Tokens,
    step: &Step,
    d: &Delegation,
    cache: &mut markdown::Cache,
    cards: &mut dyn FnMut(&mut egui::Ui, &Step),
) -> Option<ClientMsg> {
    let mut msg = None;
    let live = !d.state.finished();
    let waiting = d.steps.iter().any(|s| s.status == StepStatus::Waiting);
    let edge = match d.state {
        DelegationState::Failed => t.danger,
        DelegationState::Stopped => t.text_faint,
        _ if waiting => t.warning,
        _ => mix(t.border, t.accent, if live { 0.6 } else { 0.25 }),
    };
    let frame =
        egui::Frame::new().fill(mix(t.assistant_bubble, t.accent, 0.03)).stroke(Stroke::new(1.0, edge)).corner_radius(t.round(6.0)).inner_margin(t.pad(8.0));
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        let id = ui.make_persistent_id(("delegation", step.id));
        let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, live);
        // A card inside must be seen: open it when the subagent asks.
        if waiting {
            state.set_open(true);
        }
        state
            .show_header(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                match d.state {
                    DelegationState::Queued | DelegationState::Running if !waiting => {
                        crate::theme::spinner(ui, t, Some(13.0));
                    }
                    DelegationState::Done => {
                        icon(ui, t, Icon::Done, 15.0);
                    }
                    DelegationState::Failed => {
                        icon(ui, t, Icon::Failed, 15.0);
                    }
                    DelegationState::Stopped => {
                        icon(ui, t, Icon::Denied, 15.0);
                    }
                    _ => {
                        icon(ui, t, Icon::Waiting, 15.0);
                    }
                }
                ui.label(RichText::new(d.status_line()).color(if waiting { t.warning } else { t.text }));
                let tokens = d.prompt_tokens.saturating_add(d.completion_tokens);
                if tokens > 0 {
                    ui.label(RichText::new(format!("{} tokens", byob_protocol::usage::short_tokens(tokens))).small().color(t.text_faint));
                }
            })
            .body(|ui| {
                // The task can be long: its first lines here, all of it on hover.
                let lines: Vec<&str> = d.task.lines().filter(|l| !l.trim().is_empty()).collect();
                let mut task = lines.iter().take(TASK_LINES).copied().collect::<Vec<_>>().join("\n");
                if lines.len() > TASK_LINES {
                    task.push_str(" …");
                }
                ui.add(egui::Label::new(RichText::new(task).small().italics().color(t.text_dim)).wrap()).on_hover_text(&d.task);
                if let Some(e) = &d.error {
                    ui.label(RichText::new(e).small().color(if d.state == DelegationState::Failed { t.danger } else { t.text_dim }));
                }
                ui.add_space(2.0);
                let th = Thread { key: delegation_key(step.id), content: &d.content, steps: &d.steps, done: !live, agent: Some(&d.agent) };
                if live {
                    egui::ScrollArea::vertical()
                        .id_salt(("delegation-scroll", step.id))
                        .max_height(DELEGATION_MAX_HEIGHT)
                        .auto_shrink([false, true])
                        .stick_to_bottom(true)
                        .show(ui, |ui| msg = transcript(ui, t, th, cache, cards));
                } else {
                    msg = transcript(ui, t, th, cache, cards);
                }
                if d.content.is_empty() && d.steps.is_empty() && live {
                    let text = if d.state == DelegationState::Queued { "waiting for a free slot…" } else { "starting…" };
                    ui.label(RichText::new(text).small().color(t.text_faint));
                }
            });
    });
    msg
}

/// "Waiting for your approval", for headers while a card is open.
pub fn waiting_badge(ui: &mut egui::Ui, t: &Tokens) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        icon(ui, t, Icon::Waiting, 14.0);
        ui.label(RichText::new("Waiting for your approval").color(t.warning));
    });
}

// ---------------------------------------------------------------------------------------
// Icons, drawn with the painter so they don't depend on font coverage.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Icon {
    Done,
    Failed,
    Denied,
    Waiting,
    Access(ToolAccess),
}

fn access_color(t: &Tokens, a: ToolAccess) -> Color32 {
    match a {
        ToolAccess::Read => t.text_dim,
        ToolAccess::Write => t.warning,
        ToolAccess::Network => t.link,
        ToolAccess::Execute => t.danger,
    }
}

fn icon_color(t: &Tokens, i: Icon) -> Color32 {
    match i {
        Icon::Done => t.success,
        Icon::Failed => t.danger,
        Icon::Denied => t.text_dim,
        Icon::Waiting => t.warning,
        Icon::Access(a) => access_color(t, a),
    }
}

fn icon(ui: &mut egui::Ui, t: &Tokens, i: Icon, size: f32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    if ui.is_rect_visible(rect) {
        paint_icon(ui.painter(), rect, i, icon_color(t, i));
    }
    resp
}

fn paint_icon(p: &egui::Painter, rect: Rect, i: Icon, color: Color32) {
    let r = rect.shrink(rect.width() * 0.12);
    let c = r.center();
    let s = r.width();
    let stroke = Stroke::new((s * 0.13).max(1.3), color);
    let at = |x: f32, y: f32| -> Pos2 { pos2(r.left() + x * s, r.top() + y * s) };
    match i {
        Icon::Done => {
            p.circle_filled(c, s * 0.5, mix(color, Color32::TRANSPARENT, 0.82));
            p.line(vec![at(0.24, 0.52), at(0.43, 0.70), at(0.77, 0.32)], stroke);
        }
        Icon::Failed => {
            p.circle_filled(c, s * 0.5, mix(color, Color32::TRANSPARENT, 0.82));
            p.line_segment([at(0.3, 0.3), at(0.7, 0.7)], stroke);
            p.line_segment([at(0.7, 0.3), at(0.3, 0.7)], stroke);
        }
        Icon::Denied => {
            p.circle_stroke(c, s * 0.42, stroke);
            p.line_segment([at(0.2, 0.8), at(0.8, 0.2)], stroke);
        }
        Icon::Waiting => {
            p.circle_stroke(c, s * 0.44, stroke);
            p.line_segment([c, at(0.5, 0.22)], stroke);
            p.line_segment([c, at(0.7, 0.6)], stroke);
        }
        Icon::Access(ToolAccess::Read) => {
            // a page with lines
            p.rect_stroke(Rect::from_min_max(at(0.2, 0.08), at(0.8, 0.92)), 1.5, stroke, egui::StrokeKind::Inside);
            p.line_segment([at(0.34, 0.38), at(0.66, 0.38)], stroke);
            p.line_segment([at(0.34, 0.6), at(0.66, 0.6)], stroke);
        }
        Icon::Access(ToolAccess::Write) => {
            // a pencil
            p.line_segment([at(0.22, 0.78), at(0.78, 0.22)], Stroke::new(stroke.width * 2.0, color));
            p.line(vec![at(0.12, 0.88), at(0.18, 0.66), at(0.34, 0.82), at(0.12, 0.88)], stroke);
        }
        Icon::Access(ToolAccess::Network) => {
            // a globe
            p.circle_stroke(c, s * 0.44, stroke);
            p.line_segment([at(0.06, 0.5), at(0.94, 0.5)], stroke);
            let ellipse: Vec<Pos2> = (0..=16)
                .map(|k| {
                    #[allow(clippy::cast_precision_loss)] // small integers
                    let a = k as f32 / 16.0 * std::f32::consts::TAU;
                    c + vec2(a.cos() * s * 0.18, a.sin() * s * 0.44)
                })
                .collect();
            p.line(ellipse, stroke);
        }
        Icon::Access(ToolAccess::Execute) => {
            // a terminal prompt: >_
            p.line(vec![at(0.12, 0.25), at(0.42, 0.5), at(0.12, 0.75)], stroke);
            p.line_segment([at(0.5, 0.8), at(0.9, 0.8)], stroke);
        }
    }
}

// ---------------------------------------------------------------------------------------
// Rows

fn status_icon(status: StepStatus) -> Option<Icon> {
    match status {
        StepStatus::Running => None,
        StepStatus::Waiting => Some(Icon::Waiting),
        StepStatus::Done => Some(Icon::Done),
        StepStatus::Failed => Some(Icon::Failed),
        StepStatus::Denied => Some(Icon::Denied),
    }
}

fn row(ui: &mut egui::Ui, t: &Tokens, step: &Step) {
    let color = match step.status {
        StepStatus::Failed => t.danger,
        StepStatus::Denied => t.text_dim,
        _ => t.text,
    };
    let id = ui.make_persistent_id(("step", step.id));
    egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false)
        .show_header(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            match status_icon(step.status) {
                Some(i) => icon(ui, t, i, 15.0),
                None => crate::theme::spinner(ui, t, Some(13.0)),
            };
            let mut title = RichText::new(&step.title).color(color);
            if step.status == StepStatus::Denied {
                title = title.strikethrough();
            }
            // One line: a long command must not widen the transcript.
            ui.add(egui::Label::new(title).truncate()).on_hover_text(&step.title);
            let suffix = match step.status {
                StepStatus::Denied => Some("not run"),
                StepStatus::Failed => Some("failed"),
                _ => None,
            };
            if let Some(s) = suffix {
                ui.label(RichText::new(s).small().color(t.text_faint));
            }
            if !step.changes.is_empty() {
                let n = step.changes.len();
                let text = if n == 1 { "1 file".to_owned() } else { format!("{n} files") };
                ui.label(RichText::new(text).small().color(t.accent));
            }
        })
        .body(|ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                icon(ui, t, Icon::Access(step.access), 12.0);
                ui.label(RichText::new(format!("{} · {}", step.tool, access_text(step.access))).small().color(t.text_dim));
            });
            if let Some(p) = &step.preview {
                preview(ui, t, p);
            }
            if !step.detail.is_empty() && step.status.finished() {
                ui.add(egui::Label::new(RichText::new(&step.detail).small().monospace().color(t.text_dim)).selectable(true).wrap());
            }
            for change in &step.changes {
                ui.label(RichText::new(change_text(change)).small().color(t.text));
            }
        });
}

fn change_text(c: &FileChange) -> String {
    match c {
        FileChange::Created { path } => format!("+ created {path}"),
        FileChange::Modified { path } => format!("~ changed {path}"),
        FileChange::Deleted { path } => format!("- deleted {path}"),
        FileChange::Moved { from, to } => format!("→ moved {from} to {to}"),
    }
}

/// What an access level means, in words.
pub fn access_text(a: ToolAccess) -> &'static str {
    match a {
        ToolAccess::Read => "reads files",
        ToolAccess::Write => "changes files",
        ToolAccess::Network => "uses the network",
        ToolAccess::Execute => "runs code",
    }
}

fn undo_row(ui: &mut egui::Ui, t: &Tokens, entry: &Entry) -> Option<ClientMsg> {
    let mut msg = None;
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        if entry.undone {
            ui.label(RichText::new("File changes undone").small().color(t.text_dim));
        } else if ui
            .add(egui::Button::new(RichText::new("Undo changes").small()))
            .on_hover_text("Put the files this reply changed back the way they were")
            .clicked()
        {
            msg = Some(ClientMsg::Undo { entry: entry.id });
        }
    });
    msg
}

// ---------------------------------------------------------------------------------------
// Approval card

/// The question on a card: the pending title, as a question.
pub fn card_question(step: &Step) -> String {
    let title = step.title.trim_end_matches(['.', '?', ' ']);
    format!("{title}?")
}

fn approval_card(ui: &mut egui::Ui, t: &Tokens, step: &Step, agent: Option<&str>) -> Option<ClientMsg> {
    let mut answer = None;
    let fill = mix(t.assistant_bubble, t.accent, 0.05);
    egui::Frame::new().fill(fill).stroke(Stroke::new(1.0, mix(t.border, t.accent, 0.6))).corner_radius(t.radius).inner_margin(10.0).show(ui, |ui| {
        ui.set_width(ui.available_width());
        if let Some(agent) = agent {
            ui.label(RichText::new(format!("Subagent {agent} asks")).small().color(t.text_dim));
        }
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            icon(ui, t, Icon::Waiting, 16.0);
            ui.label(RichText::new(card_question(step)).family(bold_family()).size(t.text_size(15.0)).color(t.text_strong));
        });
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let color = access_color(t, step.access);
            egui::Frame::new().fill(mix(t.assistant_bubble, color, 0.14)).corner_radius(t.round(8.0)).inner_margin(egui::Margin::symmetric(6, 1)).show(
                ui,
                |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    icon(ui, t, Icon::Access(step.access), 12.0);
                    ui.label(RichText::new(access_text(step.access)).small().color(color));
                },
            );
            ui.label(RichText::new(&step.tool).small().monospace().color(t.text_faint));
        });
        ui.add_space(4.0);
        if let Some(ask) = &step.ask {
            if let Some(w) = &ask.warning {
                egui::Frame::new()
                    .fill(mix(t.assistant_bubble, t.danger, 0.12))
                    .stroke(Stroke::new(1.0, t.danger))
                    .corner_radius(t.round(4.0))
                    .inner_margin(t.pad(6.0))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(RichText::new(format!("⚠ {w}")).color(t.danger));
                    });
                ui.add_space(4.0);
            }
            if !ask.text.is_empty() {
                ui.add(egui::Label::new(RichText::new(&ask.text).color(t.text)).wrap());
            }
        }
        match &step.preview {
            Some(p) => preview(ui, t, p),
            None if step.ask.is_some() => {}
            None if !step.detail.is_empty() => {
                code_frame(ui, t, |ui| {
                    ui.add(egui::Label::new(RichText::new(&step.detail).small().monospace().color(t.code_text)).selectable(true).wrap());
                });
            }
            None => {}
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let allow = egui::Button::new(RichText::new("Allow").color(t.on_accent).family(bold_family())).fill(t.accent);
            if ui.add(allow).clicked() {
                answer = Some(ApprovalAnswer::Allow);
            }
            // Questions are remembered by the tool; must-confirm calls are never granted.
            if step.ask.is_none() {
                if ui.button(format!("Always allow {}", step.tool)).on_hover_text("Don't ask again for this tool in this chat").clicked() {
                    answer = Some(ApprovalAnswer::Always);
                }
                if ui.button("…in every chat").on_hover_text(format!("Don't ask again for {} in any chat", step.tool)).clicked() {
                    answer = Some(ApprovalAnswer::AlwaysEverywhere);
                }
            }
            if ui.button(RichText::new("Deny").color(t.danger)).clicked() {
                answer = Some(ApprovalAnswer::Deny);
            }
        });
    });
    answer.map(|answer| ClientMsg::Approve { step: step.id, answer })
}

fn code_frame(ui: &mut egui::Ui, t: &Tokens, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new().fill(t.code_bg).stroke(Stroke::new(1.0, t.code_border)).corner_radius(t.round(4.0)).inner_margin(t.pad(6.0)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 0.0;
        add(ui);
    });
}

/// Human-readable byte count.
fn size_text(n: u64) -> String {
    #[allow(clippy::cast_precision_loss)] // display only
    let f = n as f64;
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", f / 1024.0)
    } else {
        format!("{:.1} MB", f / (1024.0 * 1024.0))
    }
}

/// One-line summary under the question, e.g. "new file · 47 B".
pub fn preview_summary(p: &Preview) -> String {
    match p {
        Preview::Write { path, bytes, replaces: true, .. } => format!("{path} · replaces the current file · {}", size_text(*bytes)),
        Preview::Write { path, bytes, replaces: false, .. } => format!("{path} · new file · {}", size_text(*bytes)),
        Preview::Move { moves, more } => {
            let n = moves.len().saturating_add(*more);
            if n == 1 { "1 item".into() } else { format!("{n} items") }
        }
        Preview::Delete { what, .. } => format!("{what} · goes to the trash, so it can be restored"),
        Preview::Args { .. } => String::new(),
    }
}

fn more_line(ui: &mut egui::Ui, t: &Tokens, more: usize, what: &str) {
    if more > 0 {
        ui.label(RichText::new(format!("… {more} more {what}")).small().color(t.text_faint));
    }
}

fn diff_line(ui: &mut egui::Ui, t: &Tokens, l: &DiffLine) {
    let (mark, color, bg) = match l.kind {
        LineKind::Same => ("  ", t.code_text, None),
        LineKind::Added => ("+ ", t.success, Some(mix(t.code_bg, t.success, 0.12))),
        LineKind::Removed => ("- ", t.danger, Some(mix(t.code_bg, t.danger, 0.12))),
        LineKind::Skipped => {
            ui.label(RichText::new(format!("… {}", l.text)).small().italics().color(t.text_faint));
            return;
        }
    };
    let text = RichText::new(format!("{mark}{}", l.text)).monospace().small().color(color);
    let text = match bg {
        Some(bg) => text.background_color(bg),
        None => text,
    };
    ui.add(egui::Label::new(text).wrap());
}

fn preview(ui: &mut egui::Ui, t: &Tokens, p: &Preview) {
    let summary = preview_summary(p);
    if !summary.is_empty() {
        ui.label(RichText::new(summary).small().color(t.text_dim));
    }
    match p {
        Preview::Write { lines, more, replaces, .. } => {
            if lines.is_empty() {
                let text = if *replaces { "same content as now" } else { "empty file" };
                ui.label(RichText::new(text).small().color(t.text_faint));
                return;
            }
            code_frame(ui, t, |ui| {
                for l in lines {
                    diff_line(ui, t, l);
                }
                more_line(ui, t, *more, if *replaces { "changed lines" } else { "lines" });
            });
        }
        Preview::Move { moves, more } => code_frame(ui, t, |ui| {
            for (from, to) in moves {
                ui.add(egui::Label::new(RichText::new(format!("{from}  →  {to}")).monospace().small().color(t.code_text)).wrap());
            }
            more_line(ui, t, *more, "moves");
        }),
        Preview::Delete { path, .. } => code_frame(ui, t, |ui| {
            ui.add(egui::Label::new(RichText::new(path).monospace().small().color(t.code_text)).wrap());
        }),
        Preview::Args { fields, code, more } => {
            if let Some(code) = code {
                code_frame(ui, t, |ui| {
                    ui.add(egui::Label::new(RichText::new(code).monospace().small().color(t.code_text)).selectable(true).wrap());
                });
            }
            if !fields.is_empty() {
                ui.add_space(2.0);
                egui::Grid::new(ui.next_auto_id()).num_columns(2).spacing([10.0, 2.0]).show(ui, |ui| {
                    for (k, v) in fields {
                        ui.label(RichText::new(k).small().color(t.text_dim));
                        ui.add(egui::Label::new(RichText::new(v).small().color(t.text)).selectable(true).wrap());
                        ui.end_row();
                    }
                });
            }
            more_line(ui, t, *more, "arguments");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use byob_protocol::EntryRole;

    fn entry(steps: Vec<Step>) -> Entry {
        Entry {
            id: 1,
            role: EntryRole::Assistant,
            content: String::new(),
            reasoning: String::new(),
            done: false,
            error: None,
            stats: None,
            steps,
            undoable: false,
            undone: false,
            attachments: vec![],
        }
    }

    fn step(id: u64, status: StepStatus) -> Step {
        Step {
            id,
            tool: "write_file".into(),
            title: "Write a.txt".into(),
            access: ToolAccess::Write,
            status,
            detail: "{}".into(),
            changes: vec![],
            at: 0,
            preview: None,
            media: vec![],
            artifacts: vec![],
            delegation: None,
            ask: None,
            plugin: None,
        }
    }

    fn at(id: u64, at: usize) -> Step {
        Step { at, ..step(id, StepStatus::Done) }
    }

    #[test]
    fn apply_and_awaiting() {
        let mut entries = vec![entry(vec![])];
        apply(&mut entries, 1, step(5, StepStatus::Waiting));
        assert!(awaiting_approval(&entries));
        apply(&mut entries, 1, step(5, StepStatus::Done));
        assert!(!awaiting_approval(&entries));
        apply(&mut entries, 99, step(6, StepStatus::Waiting));
        assert_eq!(entries[0].steps.len(), 1);
        apply_undo(&mut entries, 1, true, false);
        assert!(entries[0].undoable);
        // A subagent's card counts too.
        let mut parent = step(7, StepStatus::Running);
        parent.delegation = Some(Box::new(Delegation { agent: "explorer".into(), steps: vec![step(8, StepStatus::Waiting)], ..Default::default() }));
        apply(&mut entries, 1, parent);
        assert!(awaiting_approval(&entries));
        assert_ne!(delegation_key(1), 1);
    }

    #[test]
    fn segments_interleave_text_and_steps() {
        let text = "Let me look.\n\nFound it. Writing now.\n\nDone!";
        let steps = [at(1, 12), at(2, 36), at(3, 36)];
        let segs = segments(text, &steps);
        assert_eq!(
            segs,
            [
                Segment::Text { part: 0, text: "Let me look." },
                Segment::Steps(0..1),
                Segment::Text { part: 1, text: "Found it. Writing now." },
                Segment::Steps(1..3),
                Segment::Text { part: 2, text: "Done!" },
            ]
        );
    }

    #[test]
    fn segments_steps_first_and_no_text() {
        assert_eq!(segments("", &[at(1, 0), at(2, 0)]), [Segment::Steps(0..2)]);
        assert_eq!(segments("Hi", &[]), [Segment::Text { part: 0, text: "Hi" }]);
        assert_eq!(segments("\n\n", &[at(1, 0)]), [Segment::Steps(0..1)]);
        assert!(segments("", &[]).is_empty());
    }

    #[test]
    fn segments_survive_hostile_offsets() {
        // past the end, backwards, and inside a multi-byte char
        let text = "héllo wörld";
        let segs = segments(text, &[at(1, 2), at(2, 1), at(3, 999)]);
        assert_eq!(segs, [Segment::Text { part: 0, text: "h" }, Segment::Steps(0..2), Segment::Text { part: 1, text: "éllo wörld" }, Segment::Steps(2..3),]);
        let all: usize = segs.iter().map(|s| if let Segment::Text { text, .. } = s { text.len() } else { 0 }).sum();
        assert_eq!(all, text.len());
    }

    #[test]
    fn streaming_parts_are_prefix_stable() {
        // while streaming, a text part only ever grows, so the markdown cache stays valid
        let steps = [at(1, 5)];
        let mut prev: Vec<String> = Vec::new();
        let full = "Hello\n\nworld, again";
        for end in 0..=full.len() {
            let Some(content) = full.get(..end) else { continue };
            let texts: Vec<String> =
                segments(content, &steps).iter().filter_map(|s| if let Segment::Text { text, .. } = s { Some((*text).to_owned()) } else { None }).collect();
            for (a, b) in prev.iter().zip(&texts) {
                assert!(b.starts_with(a.as_str()), "{a:?} -> {b:?}");
            }
            prev = texts;
        }
    }

    #[test]
    fn questions_and_summaries() {
        assert_eq!(card_question(&step(1, StepStatus::Waiting)), "Write a.txt?");
        let p = Preview::Write { path: "a.md".into(), bytes: 2048, replaces: false, lines: vec![], more: 0 };
        assert_eq!(preview_summary(&p), "a.md · new file · 2.0 KB");
        let p = Preview::Delete { path: "old.csv".into(), what: "file, 3 B".into() };
        assert!(preview_summary(&p).contains("trash"));
    }

    #[test]
    fn renders_everything_without_clicks() {
        let ctx = egui::Context::default();
        let mut waiting = step(3, StepStatus::Waiting);
        waiting.preview = Some(Preview::Write {
            path: "a.txt".into(),
            bytes: 3,
            replaces: true,
            lines: vec![
                DiffLine { kind: LineKind::Skipped, text: "3 unchanged lines".into() },
                DiffLine { kind: LineKind::Removed, text: "old".into() },
                DiffLine { kind: LineKind::Added, text: "new".into() },
            ],
            more: 2,
        });
        let mut args = step(4, StepStatus::Waiting);
        args.access = ToolAccess::Execute;
        args.preview = Some(Preview::Args { fields: vec![("cwd".into(), "/tmp".into())], code: Some("ls -la".into()), more: 1 });
        let mut moving = step(5, StepStatus::Running);
        moving.preview = Some(Preview::Move { moves: vec![("a".into(), "b".into())], more: 0 });
        let mut e = entry(vec![step(1, StepStatus::Done), step(2, StepStatus::Failed), waiting, args, moving, step(6, StepStatus::Denied)]);
        e.content = "Some **text** first".into();
        for (i, s) in e.steps.iter_mut().enumerate() {
            s.at = i * 3;
        }
        let mut cache = markdown::Cache::default();
        for tokens in [Tokens::dark(), Tokens::light()] {
            let mut answer = Some(ClientMsg::Cancel);
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                answer = body(ui, &tokens, &e, &mut cache, &mut |_, _| {});
                waiting_badge(ui, &tokens);
            });
            out.textures_delta.clear();
            assert_eq!(answer, None);
        }
        e.done = true;
        e.undoable = true;
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            assert_eq!(body(ui, &Tokens::dark(), &e, &mut cache, &mut |_, _| {}), None);
        });
        out.textures_delta.clear();
    }
}
