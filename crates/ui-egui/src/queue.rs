//! Queued follow-ups above the composer (see `byob_protocol::queue`): compact rows in send
//! order, each with Send now, move up and remove; clicking one edits it in the composer. The
//! queue itself lives in the engine; this only shows it and asks for changes.

use byob_protocol::{ChatQueue, MediaRef, QueuePause, QueuedMessage, ServerMsg};
use egui::{Align, Layout, RichText, Sense, Stroke, vec2};

use crate::theme::{self, Tokens};

/// Tallest the list gets before it scrolls.
const MAX_HEIGHT: f32 = 168.0;
/// Room kept on the right of a row for its buttons.
const BUTTONS_WIDTH: f32 = 124.0;

/// Something the user did to the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Edit(u64),
    Remove(u64),
    SendNow(u64),
    /// Move it to this position.
    Move(u64, u32),
    Resume,
}

#[derive(Default)]
pub struct QueueUi {
    pub queue: ChatQueue,
    /// The queued message being edited in the composer.
    pub editing: Option<u64>,
    /// The draft set aside while editing: text and attachments.
    pub stash: Option<(String, Vec<MediaRef>)>,
}

impl QueueUi {
    /// Take a `ServerMsg::Queue`. Returns true if it was one.
    pub fn apply(&mut self, msg: &ServerMsg) -> bool {
        let ServerMsg::Queue(q) = msg else { return false };
        self.queue = q.clone();
        true
    }

    pub fn item(&self, id: u64) -> Option<&QueuedMessage> {
        self.queue.items.iter().find(|i| i.id == id)
    }

    /// Messages waiting that will go on their own (not paused).
    pub fn moving(&self) -> bool {
        !self.queue.items.is_empty() && self.queue.paused.is_none()
    }

    /// The rows; returns what the user did.
    pub fn show(&self, ui: &mut egui::Ui, t: &Tokens) -> Vec<Action> {
        let mut out = Vec::new();
        let items = &self.queue.items;
        if items.is_empty() {
            return out;
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Queued · {}", items.len())).small().color(t.text_dim));
            if let Some(why) = self.queue.paused {
                let text = match why {
                    QueuePause::Stopped => "paused: the reply was stopped",
                    QueuePause::Failed => "paused: the reply failed",
                };
                ui.label(RichText::new(text).small().color(t.warning));
                if ui.small_button("Resume").on_hover_text("Send the queued messages, in order").clicked() {
                    out.push(Action::Resume);
                }
            }
        });
        egui::ScrollArea::vertical().id_salt("queue-scroll").max_height(MAX_HEIGHT).auto_shrink([false, true]).show(ui, |ui| {
            for (i, item) in items.iter().enumerate() {
                self.row(ui, t, i, item, &mut out);
            }
        });
        ui.add_space(t.pad(2.0));
        out
    }

    fn row(&self, ui: &mut egui::Ui, t: &Tokens, i: usize, item: &QueuedMessage, out: &mut Vec<Action>) {
        let editing = self.editing == Some(item.id);
        let stroke = if editing { Stroke::new(1.0, t.accent) } else { Stroke::new(1.0, t.border) };
        let frame = egui::Frame::new()
            .fill(theme::mix(t.panel_bg, t.user_bubble, 0.35))
            .stroke(stroke)
            .corner_radius(t.round(6.0))
            .inner_margin(vec2(t.pad(8.0), t.pad(3.0)))
            .outer_margin(egui::Margin { bottom: 3, ..Default::default() });
        frame.show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{}", i + 1)).small().color(t.text_faint));
                let mut text = if editing { "editing in the composer…".to_owned() } else { item.preview(240) };
                for a in &item.attachments {
                    text.push_str(" · ");
                    text.push_str(a.name());
                }
                let width = (ui.available_width() - BUTTONS_WIDTH).max(40.0);
                let color = if editing { t.accent } else { t.text };
                let label = ui
                    .allocate_ui_with_layout(vec2(width, ui.spacing().interact_size.y), Layout::left_to_right(Align::Center), |ui| {
                        ui.add(egui::Label::new(RichText::new(text).color(color)).truncate().sense(Sense::click()))
                    })
                    .inner;
                // One edit at a time, so unsaved changes are never dropped.
                if self.editing.is_none() && label.on_hover_text("Edit").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    out.push(Action::Edit(item.id));
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let x = egui::Button::new(RichText::new("×").color(t.text_dim)).frame(false);
                    if ui.add(x).on_hover_text("Remove from the queue").clicked() {
                        out.push(Action::Remove(item.id));
                    }
                    // Invisible on the first row, so the buttons line up.
                    let up = egui::Button::new(RichText::new("↑").color(t.text_dim)).frame(false);
                    if ui.add_visible(i > 0, up).on_hover_text("Send earlier").clicked() {
                        out.push(Action::Move(item.id, u32::try_from(i.saturating_sub(1)).unwrap_or(0)));
                    }
                    if !editing && ui.small_button("Send now").on_hover_text("Stop the reply and send this next").clicked() {
                        out.push(Action::SendNow(item.id));
                    }
                });
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue(n: u64, paused: Option<QueuePause>) -> ChatQueue {
        let items =
            (1..=n).map(|id| QueuedMessage { id, text: format!("follow-up {id}\nmore"), attachments: vec![MediaRef::new("uploads/a.csv", 3)] }).collect();
        ChatQueue { chat: Some("c".into()), items, paused }
    }

    #[test]
    fn rows_render_in_both_themes_and_narrow() {
        let mut q = QueueUi::default();
        assert!(!q.apply(&ServerMsg::Notice { level: byob_protocol::NoticeLevel::Info, text: String::new() }));
        assert!(q.apply(&ServerMsg::Queue(queue(3, Some(QueuePause::Stopped)))));
        assert!(!q.moving());
        q.editing = Some(2);
        for t in [Tokens::dark(), Tokens::light()] {
            for width in [300.0, 1200.0] {
                let ctx = egui::Context::default();
                let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(width, 600.0))), ..Default::default() };
                let mut out = ctx.run_ui(input, |ui| {
                    assert!(q.show(ui, &t).is_empty());
                });
                out.textures_delta.clear();
            }
        }
        q.apply(&ServerMsg::Queue(queue(1, None)));
        assert!(q.moving() && q.item(1).is_some() && q.item(2).is_none());
    }
}
