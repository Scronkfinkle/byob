//! The context meter in the header ("34k / 262k" and a bar that warms up as the chat nears
//! its compaction threshold, with a breakdown and "Compact now"), and the "Context
//! compacted" dividers in the transcript (expandable to read the summary).

use std::collections::HashMap;

use egui::{Color32, RichText, Sense, Stroke};
use byob_protocol::usage::short_tokens;
use byob_protocol::{ChatUsage, ClientMsg, Compaction, ServerMsg};

use crate::markdown;
use crate::theme::{Tokens, mix};

/// Bar size in the header (narrow screens show only the bar).
const BAR_WIDTH: f32 = 72.0;
const BAR_WIDTH_NARROW: f32 = 44.0;
const BAR_HEIGHT: f32 = 7.0;

/// Usage and compactions of the chat in view.
#[derive(Default)]
pub struct Usage {
    usage: Option<ChatUsage>,
    compactions: Vec<Compaction>,
    /// Rendered summaries, by compaction id.
    docs: HashMap<u64, markdown::Doc>,
    /// Focus text typed in the meter's popup.
    focus: String,
}

impl Usage {
    /// Take the messages this handles; `ChatOpened` only resets (others handle it too).
    pub fn apply(&mut self, msg: &ServerMsg) -> bool {
        match msg {
            ServerMsg::Usage(u) => self.usage = Some(u.clone()),
            ServerMsg::Compactions { chat, items } => {
                self.compactions.clone_from(items);
                self.docs.clear();
                if chat.is_none() {
                    self.usage = None;
                }
            }
            ServerMsg::Compacted(c) => self.compactions.push(c.clone()),
            ServerMsg::ChatOpened { .. } => {
                self.usage = None;
                self.compactions.clear();
                self.docs.clear();
                return false;
            }
            _ => return false,
        }
        true
    }

    /// Compactions whose divider goes after entry `id` (0: before the first entry).
    pub fn after(&self, id: u64) -> impl Iterator<Item = &Compaction> {
        self.compactions.iter().filter(move |c| c.after_entry == id)
    }

    /// Draw the dividers after entry `id`.
    pub fn dividers(&mut self, ui: &mut egui::Ui, t: &Tokens, id: u64) {
        let ids: Vec<u64> = self.after(id).map(|c| c.id).collect();
        for cid in ids {
            if let Some(c) = self.compactions.iter().find(|c| c.id == cid) {
                let doc = self.docs.entry(cid).or_default();
                divider(ui, t, c, doc);
            }
        }
    }

    /// The header meter. Returns a message if the user asked to compact now.
    pub fn meter(&mut self, ui: &mut egui::Ui, t: &Tokens, narrow: bool) -> Option<ClientMsg> {
        let u = self.usage.as_ref()?;
        let width = if narrow { BAR_WIDTH_NARROW } else { BAR_WIDTH };
        let row = ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            bar(ui, t, u, width);
            if u.compacting {
                ui.label(RichText::new("compacting…").small().color(t.text_dim));
            } else if !narrow {
                ui.label(RichText::new(u.label()).small().color(t.text_dim));
                if let Some(split) = u.split_label() {
                    ui.label(RichText::new(split).small().color(t.text_faint));
                }
            }
        });
        let response = row.response.interact(Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
        let popup_open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&response));
        let response = if popup_open { response } else { response.on_hover_ui(|ui| breakdown(ui, t, u)) };
        let mut out = None;
        let busy = u.compacting;
        egui::Popup::from_toggle_button_response(&response).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
            ui.set_min_width(260.0);
            breakdown(ui, t, u);
            ui.separator();
            ui.add(egui::TextEdit::singleline(&mut self.focus).hint_text("Keep in detail… (optional)").desired_width(f32::INFINITY));
            ui.horizontal(|ui| {
                let button = ui.add_enabled(!busy, egui::Button::new("Compact now"));
                if button.on_hover_text("Summarise the conversation so far; later requests start from the summary").clicked() {
                    let focus = std::mem::take(&mut self.focus);
                    let focus = focus.trim();
                    out = Some(ClientMsg::Compact { focus: (!focus.is_empty()).then(|| focus.to_owned()) });
                    ui.close();
                }
                ui.label(RichText::new("or type /compact").small().color(t.text_faint));
            });
        });
        out
    }
}

/// Colour of the bar: accent while there is room, warning near the threshold, danger past it.
fn bar_color(t: &Tokens, pressure: f32) -> Color32 {
    if pressure < 0.6 {
        t.accent
    } else if pressure < 0.9 {
        mix(t.accent, t.warning, (pressure - 0.6) / 0.3)
    } else {
        mix(t.warning, t.danger, ((pressure - 0.9) / 0.1).min(1.0))
    }
}

fn bar(ui: &mut egui::Ui, t: &Tokens, u: &ChatUsage, width: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, BAR_HEIGHT), Sense::hover());
    let p = ui.painter();
    let radius = BAR_HEIGHT / 2.0;
    p.rect_filled(rect, radius, t.field_bg);
    p.rect_stroke(rect, radius, Stroke::new(1.0, t.border), egui::StrokeKind::Inside);
    let fill = u.fraction();
    if fill > 0.0 {
        let mut f = rect;
        f.set_width((rect.width() * fill).max(BAR_HEIGHT));
        p.rect_filled(f, radius, bar_color(t, u.pressure()));
    }
    // A tick where compaction kicks in.
    if u.compact_at > 0 && u.compact_at < u.context_window {
        #[allow(clippy::cast_precision_loss)] // display only
        let x = rect.left() + rect.width() * (u.compact_at as f32 / u.context_window as f32);
        p.vline(x, rect.y_range().expand(2.0), Stroke::new(1.5, t.text_dim));
    }
}

/// The numbers behind the meter.
fn breakdown(ui: &mut egui::Ui, t: &Tokens, u: &ChatUsage) {
    let row = |ui: &mut egui::Ui, k: &str, v: String| {
        ui.label(RichText::new(k).color(t.text_dim));
        ui.label(RichText::new(v).color(t.text));
        ui.end_row();
    };
    ui.label(RichText::new("Context").strong().color(t.text_strong));
    egui::Grid::new("usage-breakdown").num_columns(2).spacing([12.0, 2.0]).show(ui, |ui| {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // display only
        let percent = (u.fraction() * 100.0).round() as u32;
        let used =
            format!("{} of {} ({percent}%){}", short_tokens(u.context_used), short_tokens(u.context_window), if u.estimated { ", estimated" } else { "" });
        row(ui, "In use", used);
        row(ui, "Compacts at", if u.compact_at == 0 { "off".into() } else { short_tokens(u.compact_at) });
        if u.last_prompt_tokens > 0 || u.last_completion_tokens > 0 {
            row(ui, "Last request", format!("{} in · {} out", u.last_prompt_tokens, u.last_completion_tokens));
        }
    });
    ui.add_space(4.0);
    ui.label(RichText::new("This chat").strong().color(t.text_strong));
    egui::Grid::new("usage-totals").num_columns(2).spacing([12.0, 2.0]).show(ui, |ui| {
        row(ui, "Prompt tokens", u.prompt_tokens.to_string());
        row(ui, "Completion tokens", u.completion_tokens.to_string());
        row(ui, "Model calls", u.model_calls.to_string());
        row(ui, "Compactions", u.compactions.to_string());
        if u.agent_calls > 0 {
            row(ui, "Subagents", format!("{} in · {} out · {} calls", u.agent_prompt_tokens, u.agent_completion_tokens, u.agent_calls));
        }
        #[allow(clippy::cast_precision_loss)] // display only
        let secs = u.elapsed_ms as f64 / 1000.0;
        row(ui, "Time", if secs >= 60.0 { format!("{}m {:.0}s", (secs / 60.0).floor(), secs % 60.0) } else { format!("{secs:.1}s") });
    });
}

/// "── Context compacted · 182k → 9k tokens ──", with the summary underneath on click.
fn divider(ui: &mut egui::Ui, t: &Tokens, c: &Compaction, doc: &mut markdown::Doc) {
    let id = egui::Id::unique(("compaction", c.id));
    let mut open = ui.data(|d| d.get_temp::<bool>(id).unwrap_or(false));
    let mut label = c.label();
    if c.manual {
        label.push_str(" · by you");
    }
    let action = if open { "hide summary" } else { "show summary" };
    let font = egui::TextStyle::Small.resolve(ui.style());
    let measure = |s: &str| ui.painter().layout_no_wrap(s.to_owned(), font.clone(), t.text_dim).size().x;
    let gap = ui.spacing().item_spacing.x;
    let text_width = (measure(&label) + measure(action) + gap + 4.0).min(ui.available_width());
    ui.add_space(2.0);
    // A full-width row: rules either side of the centred label and link.
    let (row, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 18.0), Sense::hover());
    let (x0, x1) = (row.center().x - text_width / 2.0, row.center().x + text_width / 2.0);
    let stroke = Stroke::new(1.0, t.separator);
    let p = ui.painter();
    if x0 - 10.0 > row.left() {
        p.hline(row.left()..=x0 - 10.0, row.center().y, stroke);
    }
    if x1 + 10.0 < row.right() {
        p.hline(x1 + 10.0..=row.right(), row.center().y, stroke);
    }
    let text_rect = egui::Rect::from_x_y_ranges(x0..=x1, row.y_range());
    let mut inner = ui.new_child(egui::UiBuilder::new().max_rect(text_rect).layout(egui::Layout::left_to_right(egui::Align::Center)));
    inner.label(RichText::new(&label).small().color(t.text_dim)).on_hover_text(
        c.focus
            .as_deref()
            .map_or_else(|| "Older messages were summarised to free context; the transcript still shows them all.".to_owned(), |f| format!("Focus: {f}")),
    );
    if inner.link(RichText::new(action).small()).clicked() {
        open = !open;
        ui.data_mut(|d| d.insert_temp(id, open));
    }
    if open {
        doc.update(&c.summary, true);
        egui::Frame::new().fill(t.code_bg).stroke(Stroke::new(1.0, t.code_border)).corner_radius(6.0).inner_margin(10.0).show(ui, |ui| {
            ui.set_width(ui.available_width());
            markdown::show(ui, t, doc, id.with("summary"));
        });
    }
    ui.add_space(2.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comp(id: u64, after: u64) -> Compaction {
        Compaction { id, after_entry: after, tokens_before: 10, tokens_after: 1, summary: "s".into(), manual: false, focus: None, created_at: 0 }
    }

    #[test]
    fn follows_the_chat_in_view() {
        let mut u = Usage::default();
        assert!(u.apply(&ServerMsg::Compactions { chat: Some("c".into()), items: vec![comp(1, 4)] }));
        assert!(u.apply(&ServerMsg::Usage(ChatUsage { chat: "c".into(), context_used: 5, context_window: 10, ..Default::default() })));
        assert!(u.apply(&ServerMsg::Compacted(comp(2, 9))));
        assert_eq!(u.after(4).count(), 1);
        assert_eq!(u.after(9).map(|c| c.id).collect::<Vec<_>>(), [2]);
        assert!(u.usage.is_some());
        // Opening another chat resets, but leaves the message to the other handlers.
        assert!(!u.apply(&ServerMsg::ChatOpened { chat: None }));
        assert!(u.usage.is_none() && u.compactions.is_empty());
        assert!(!u.apply(&ServerMsg::Notice { level: byob_protocol::NoticeLevel::Info, text: String::new() }));
    }

    #[test]
    fn bar_warms_up_towards_the_threshold() {
        let t = Tokens::dark();
        assert_eq!(bar_color(&t, 0.1), t.accent);
        assert_eq!(bar_color(&t, 0.9), t.warning);
        assert_eq!(bar_color(&t, 1.0), t.danger);
    }
}
