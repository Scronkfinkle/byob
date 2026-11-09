//! Tool steps inside an assistant entry: compact collapsible rows, and an approval card
//! (Allow / Always / Deny) for steps waiting on the user. Any client can answer a card.

use yah_protocol::{ApprovalAnswer, ClientMsg, Entry, FileChange, Step, StepStatus, ToolAccess};

use crate::theme::Tokens;

/// Apply a `ServerMsg::StepUpdated` to the local transcript.
pub fn apply(entries: &mut [Entry], entry: u64, step: Step) {
    if let Some(e) = entries.iter_mut().rev().find(|e| e.id == entry) {
        e.upsert_step(step);
    }
}

/// Some entry has an approval card waiting for the user.
pub fn awaiting_approval(entries: &[Entry]) -> bool {
    entries.iter().any(|e| e.pending_approvals().next().is_some())
}

/// Draw the steps of `entry`. Returns the user's answer to an approval card, if clicked.
pub fn show(ui: &mut egui::Ui, t: &Tokens, entry: &Entry) -> Option<ClientMsg> {
    let mut answer = None;
    for step in &entry.steps {
        if step.status == StepStatus::Waiting {
            answer = answer.or(approval_card(ui, t, step));
        } else {
            row(ui, t, step);
        }
    }
    answer
}

fn status_mark(status: StepStatus) -> &'static str {
    match status {
        StepStatus::Running | StepStatus::Waiting => "…",
        // Glyphs the default egui fonts cover.
        StepStatus::Done => "•",
        StepStatus::Failed => "×",
        StepStatus::Denied => "–",
    }
}

fn row(ui: &mut egui::Ui, t: &Tokens, step: &Step) {
    let color = match step.status {
        StepStatus::Failed | StepStatus::Denied => t.danger,
        _ => t.text_dim,
    };
    let header = egui::RichText::new(format!("{} {}", status_mark(step.status), step.title)).small().color(color);
    let id = ui.make_persistent_id(("step", step.id));
    egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false)
        .show_header(ui, |ui| {
            ui.label(header);
            if step.status == StepStatus::Running {
                ui.spinner();
            }
            if !step.changes.is_empty() {
                ui.label(egui::RichText::new(format!("{} file(s)", step.changes.len())).small().color(t.accent));
            }
        })
        .body(|ui| {
            ui.label(egui::RichText::new(format!("{} · {} access", step.tool, step.access.label())).small().color(t.text_dim));
            if !step.detail.is_empty() {
                ui.add(egui::Label::new(egui::RichText::new(&step.detail).small().monospace().color(t.text_dim)).selectable(true).wrap());
            }
            for change in &step.changes {
                ui.label(egui::RichText::new(change_text(change)).small().color(t.text));
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

fn access_text(a: ToolAccess) -> &'static str {
    match a {
        ToolAccess::Read => "reads files",
        ToolAccess::Write => "changes files",
        ToolAccess::Network => "uses the network",
        ToolAccess::Execute => "runs code",
    }
}

fn approval_card(ui: &mut egui::Ui, t: &Tokens, step: &Step) -> Option<ClientMsg> {
    let mut answer = None;
    egui::Frame::new().stroke(egui::Stroke::new(1.0, t.accent)).corner_radius(6.0).inner_margin(8.0).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(egui::RichText::new(format!("Allow: {}?", step.title)).strong().color(t.text));
        ui.label(egui::RichText::new(format!("{} {}", step.tool, access_text(step.access))).small().color(t.text_dim));
        if !step.detail.is_empty() {
            ui.add(egui::Label::new(egui::RichText::new(&step.detail).small().monospace().color(t.text_dim)).selectable(true).wrap());
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button(egui::RichText::new("Allow").strong()).clicked() {
                answer = Some(ApprovalAnswer::Allow);
            }
            if ui.button(format!("Always allow {}", step.tool)).on_hover_text("Don't ask again for this tool in this chat").clicked() {
                answer = Some(ApprovalAnswer::Always);
            }
            if ui.button("…in every chat").on_hover_text(format!("Don't ask again for {} in any chat", step.tool)).clicked() {
                answer = Some(ApprovalAnswer::AlwaysEverywhere);
            }
            if ui.button(egui::RichText::new("Deny").color(t.danger)).clicked() {
                answer = Some(ApprovalAnswer::Deny);
            }
        });
    });
    answer.map(|answer| ClientMsg::Approve { step: step.id, answer })
}

#[cfg(test)]
mod tests {
    use super::*;
    use yah_protocol::EntryRole;

    fn entry(steps: Vec<Step>) -> Entry {
        Entry { id: 1, role: EntryRole::Assistant, content: String::new(), reasoning: String::new(), done: false, error: None, stats: None, steps }
    }

    fn step(id: u64, status: StepStatus) -> Step {
        Step { id, tool: "write_file".into(), title: "Write a.txt".into(), access: ToolAccess::Write, status, detail: "{}".into(), changes: vec![], at: 0 }
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
    }

    #[test]
    fn renders_rows_and_cards_without_clicks() {
        let ctx = egui::Context::default();
        let e = entry(vec![step(1, StepStatus::Done), step(2, StepStatus::Failed), step(3, StepStatus::Waiting)]);
        let mut answer = Some(ClientMsg::Cancel);
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            answer = show(ui, &Tokens::dark(), &e);
        });
        out.textures_delta.clear();
        assert_eq!(answer, None);
    }
}
