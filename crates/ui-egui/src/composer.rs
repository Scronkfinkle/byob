//! The composer: one rounded field holding the attachment chips, the message text (growing
//! with what is typed, then scrolling) and, along its bottom edge, the paperclip on the left
//! and the send control on the right. Send is a round arrow, muted until there is something
//! to send and filled with the accent once there is; while a reply runs the same spot is
//! Stop (with Queue beside it once something is typed), and while a queued message is being
//! edited it is Save (with Cancel beside it). The queue list sits above the field and the `/`
//! menu pops up from it.

use byob_protocol::{ClientMsg, NoticeLevel};
use egui::{Color32, CornerRadius, Margin, Rect, Response, Sense, Stroke, Ui, vec2};

use crate::theme::Tokens;
use crate::{ByobApp, icons, readable_column, slash};

/// Lines of text the field grows to before it scrolls.
const MAX_LINES: f32 = 10.0;

/// Where the composer was last drawn, and how its send control looked (tests).
#[derive(Debug, Clone, Copy)]
pub struct Rects {
    /// The rounded field around it all.
    pub frame: Rect,
    /// The text area.
    pub field: Rect,
    pub attach: Rect,
    /// The rightmost control: Send, Stop or Save.
    pub send: Rect,
    /// The control beside it: Queue or Cancel (`Rect::NOTHING` when there is none).
    pub second: Rect,
    /// The send control's fill: the accent when it is the thing Enter does.
    pub send_fill: Color32,
}

impl Default for Rects {
    fn default() -> Self {
        Self { frame: Rect::NOTHING, field: Rect::NOTHING, attach: Rect::NOTHING, send: Rect::NOTHING, second: Rect::NOTHING, send_fill: Color32::TRANSPARENT }
    }
}

/// The height of the controls along the field's bottom edge (and the send circle's diameter).
pub fn control_size(ui: &Ui) -> f32 {
    (icons::button_height(ui) + 8.0).round()
}

impl ByobApp {
    pub(crate) fn composer(&mut self, ui: &mut Ui) {
        let t = self.tokens;
        ui.add_space(t.pad(6.0));
        readable_column(ui, &t, |ui| self.composer_column(ui, &t));
        ui.add_space(t.pad(8.0));
    }

    fn composer_column(&mut self, ui: &mut Ui, t: &Tokens) {
        for (level, text) in &self.notices {
            let color = if *level == NoticeLevel::Error { t.danger } else { t.text_dim };
            ui.label(egui::RichText::new(text).color(color));
        }
        let id = egui::Id::unique("composer");
        if std::mem::take(&mut self.focus_composer) {
            ui.memory_mut(|m| m.request_focus(id));
        }
        // Enter sends (queues while a reply runs); Shift+Enter inserts a newline (consume
        // Enter before the editor sees it). Escape stops editing a queued message. While the
        // `/` menu is open it takes ↑/↓/Enter/Tab/Esc instead.
        let focused = ui.memory(|m| m.has_focus(id));
        let commands: Vec<byob_protocol::SlashItem> = slash::matches(self.plugins.slash(), &self.input).into_iter().cloned().collect();
        // egui drops the focus on Esc before we see the key: the menu still takes that Esc.
        let escaped = !focused && self.slash.had_focus && ui.input(|i| i.key_pressed(egui::Key::Escape));
        let menu = self.slash.open(&self.input, commands.len()) && (focused || escaped);
        self.slash.had_focus = focused;
        let mut picked = if menu { self.slash.keys(ui, &self.input, commands.len()) } else { None };
        if escaped && menu {
            ui.memory_mut(|m| m.request_focus(id));
        }
        let enter = focused && !menu && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
        let busy = self.busy();
        let editing = self.queue.editing;
        let mut send = enter;
        let mut cancel_edit = editing.is_some() && focused && !menu && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        let actions = self.queue.show(ui, t);
        self.queue_actions(actions);

        // Something to send (attachments wait until they have finished uploading).
        let has_content = !self.input.trim().is_empty() || self.uploads.has_ready();
        let ready = has_content && !self.uploads.busy();
        let d = control_size(ui);
        let frame = egui::Frame::new()
            .fill(t.field_bg)
            .stroke(Stroke::new(1.0, t.field_border))
            .corner_radius(t.round(14.0).min(d / 2.0 + 6.0))
            .inner_margin(Margin { left: 6, right: 6, top: 8, bottom: 6 });
        let mut prepared = frame.begin(ui);
        let mut rects = Rects::default();
        {
            let ui = &mut prepared.content_ui;
            ui.set_min_width(ui.available_width());
            if self.uploads.chips(ui, t) {
                ui.add_space(t.pad(4.0));
            }
            // Keyboard help only where it fits on one line (phones have no Shift anyway).
            let (long, short) = match (editing.is_some(), busy) {
                (true, _) => ("Edit the queued message…  (Enter to save, Esc to cancel)", "Edit the queued message…"),
                (false, true) => ("Queue a follow-up…  (Enter to queue, Shift+Enter for newline)", "Queue a follow-up…"),
                (false, false) => ("Message byob…  (Enter to send, Shift+Enter for newline)", "Message byob…"),
            };
            let text_left = 4.0;
            let fits = ui.painter().layout_no_wrap(long.to_owned(), egui::TextStyle::Body.resolve(ui.style()), t.text).size().x
                <= ui.available_width() - 2.0 * text_left;
            let hint = if fits { long } else { short };
            let max_height = ui.text_style_height(&egui::TextStyle::Body) * MAX_LINES;
            let input = &mut self.input;
            let field = egui::ScrollArea::vertical()
                .id_salt("composer-scroll")
                .max_height(max_height)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    let edit = egui::TextEdit::multiline(input)
                        .id(id)
                        .frame(egui::Frame::NONE.inner_margin(Margin::symmetric(4, 0)))
                        .desired_rows(1)
                        .desired_width(f32::INFINITY)
                        .hint_text(hint);
                    ui.add(edit)
                })
                .inner;
            rects.field = field.rect;
            ui.add_space(t.pad(6.0));
            // The bottom edge: paperclip, room to click into the text, the send control.
            ui.allocate_ui_with_layout(vec2(ui.available_width(), d), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                rects.attach = self.uploads.button(ui, t, d).rect;
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if editing.is_some() {
                        let save = pill(ui, t, "Save", d, true).on_hover_text("Save the queued message (Enter)");
                        send |= save.clicked();
                        rects.send = save.rect;
                        rects.send_fill = t.accent;
                        let cancel = pill(ui, t, "Cancel", d, false).on_hover_text("Stop editing (Esc)");
                        cancel_edit |= cancel.clicked();
                        rects.second = cancel.rect;
                    } else if busy {
                        // Stop is the strong one until there's a follow-up to queue.
                        let fill = if ready { t.button_bg } else { t.text_strong };
                        let icon = if ready { t.text_strong } else { t.field_bg };
                        let stop = round_button(ui, t, d, fill, icon, icons::stop, true).on_hover_text("Stop the reply");
                        if stop.clicked() {
                            self.conn.send(ClientMsg::Cancel);
                        }
                        rects.send = stop.rect;
                        rects.send_fill = fill;
                        if ready {
                            let queue = pill(ui, t, "Queue", d, true).on_hover_text("Send this when the reply is done (Enter)");
                            send |= queue.clicked();
                            rects.second = queue.rect;
                        }
                    } else {
                        let (fill, icon) = if ready { (t.accent, t.on_accent) } else { (t.button_bg, t.text_faint) };
                        let tip = if ready {
                            "Send (Enter)"
                        } else if self.uploads.busy() {
                            "Waiting for the files to upload"
                        } else {
                            "Type a message to send"
                        };
                        let b = round_button(ui, t, d, fill, icon, icons::arrow_up, ready).on_hover_text(tip);
                        send |= b.clicked();
                        rects.send = b.rect;
                        rects.send_fill = fill;
                    }
                    // The gap between the controls also puts the cursor in the text.
                    let gap = ui.available_rect_before_wrap();
                    if gap.width() > 0.0 && ui.allocate_rect(gap, Sense::click()).on_hover_cursor(egui::CursorIcon::Text).clicked() {
                        ui.memory_mut(|m| m.request_focus(id));
                    }
                });
            });
        }
        let focused = ui.memory(|m| m.has_focus(id));
        if focused {
            prepared.frame.stroke = Stroke::new(1.0, t.accent);
        }
        let outer = prepared.end(ui).rect;
        if focused {
            // A soft ring outside the border.
            let radius = CornerRadius::from(t.round(14.0).min(d / 2.0 + 6.0) + 1.0);
            ui.painter().rect_stroke(outer.expand(0.5), radius, Stroke::new(3.0, t.accent.gamma_multiply(0.22)), egui::StrokeKind::Outside);
        }
        rects.frame = outer;
        self.composer_rects = rects;

        // Still open after this frame's typing (and not just picked): draw it over the field.
        if menu && picked.is_none() && slash::query(&self.input).is_some() {
            let shown: Vec<&byob_protocol::SlashItem> = commands.iter().collect();
            picked = self.slash.show(ui, t, outer, &shown);
        }
        if let Some(item) = picked.and_then(|k| commands.get(k)) {
            self.slash.reset();
            self.input = slash::insert(item);
            let mut state = egui::TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
            let end = egui::text::CCursor::new(self.input.chars().count());
            state.cursor.set_char_range(Some(egui::text::CCursorRange::one(end)));
            egui::TextEdit::store_state(ui.ctx(), id, state);
            ui.memory_mut(|m| m.request_focus(id));
        }
        if cancel_edit {
            self.end_edit();
        }
        match self.queue.editing {
            Some(item) if send && !self.uploads.busy() => {
                let msg = ClientMsg::EditQueued { id: item, text: std::mem::take(&mut self.input), attachments: self.uploads.take() };
                self.conn.send(msg);
                self.queue.editing = None;
                let (text, files) = self.queue.stash.take().unwrap_or_default();
                self.input = text;
                self.uploads.restore(files);
                ui.memory_mut(|m| m.request_focus(id));
            }
            None if send && ready => {
                let text = std::mem::take(&mut self.input);
                self.submit(text);
                ui.memory_mut(|m| m.request_focus(id));
            }
            _ => {}
        }
    }
}

/// A round button `d` across: `fill` with `paint` in `icon` on it, a shade lighter or darker
/// on hover. Not `enabled`: it only shows its tooltip.
pub fn round_button(ui: &mut Ui, t: &Tokens, d: f32, fill: Color32, icon: Color32, paint: icons::Paint, enabled: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(d, d), if enabled { Sense::click() } else { Sense::hover() });
    if ui.is_rect_visible(rect) {
        let fill = if enabled && resp.is_pointer_button_down_on() {
            crate::theme::mix(fill, if t.dark { Color32::BLACK } else { Color32::WHITE }, 0.2)
        } else if enabled && resp.hovered() {
            crate::theme::mix(fill, if t.dark { Color32::WHITE } else { Color32::BLACK }, 0.12)
        } else {
            fill
        };
        ui.painter().circle_filled(rect.center(), d / 2.0, fill);
        paint(ui.painter(), Rect::from_center_size(rect.center(), vec2(d * 0.55, d * 0.55)), icon);
    }
    resp
}

/// A frameless round icon button (the paperclip): tinted on hover.
pub fn ghost_icon(ui: &mut Ui, t: &Tokens, d: f32, paint: icons::Paint, enabled: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(d, d), if enabled { Sense::click() } else { Sense::hover() });
    if ui.is_rect_visible(rect) {
        let hot = enabled && resp.hovered();
        if enabled && resp.is_pointer_button_down_on() {
            ui.painter().circle_filled(rect.center(), d / 2.0, t.pressed);
        } else if hot {
            ui.painter().circle_filled(rect.center(), d / 2.0, t.hover);
        }
        let color = if !enabled {
            t.text_faint
        } else if hot {
            t.text_strong
        } else {
            t.text_dim
        };
        paint(ui.painter(), Rect::from_center_size(rect.center(), vec2(d * 0.62, d * 0.62)), color);
    }
    resp
}

/// A text button `d` high with round ends: the accent when `primary`, else frameless.
pub fn pill(ui: &mut Ui, t: &Tokens, label: &str, d: f32, primary: bool) -> Response {
    let color = if primary { t.on_accent } else { t.text };
    let galley = ui.painter().layout_no_wrap(label.to_owned(), egui::TextStyle::Button.resolve(ui.style()), color);
    let (rect, resp) = ui.allocate_exact_size(vec2(galley.size().x + d * 0.9, d), Sense::click());
    if ui.is_rect_visible(rect) {
        let down = resp.is_pointer_button_down_on();
        let fill = match (primary, down, resp.hovered()) {
            (true, true, _) => Some(crate::theme::mix(t.accent, if t.dark { Color32::BLACK } else { Color32::WHITE }, 0.2)),
            (true, false, true) => Some(t.accent_hover),
            (true, false, false) => Some(t.accent),
            (false, true, _) => Some(t.pressed),
            (false, false, true) => Some(t.hover),
            (false, false, false) => None,
        };
        if let Some(fill) = fill {
            ui.painter().rect_filled(rect, d / 2.0, fill);
        }
        ui.painter().galley(rect.center() - galley.size() / 2.0, galley, color);
    }
    resp
}
