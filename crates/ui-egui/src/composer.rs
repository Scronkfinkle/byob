//! The composer: one rounded field holding the attachment chips, the message text (growing
//! with what is typed, then scrolling) with the paperclip on its left and the send control on
//! its right, on one line until the text grows (then they keep to its bottom edge). The text is
//! Markdown, styled as it is typed ([`crate::compose_md`]). Send is a round arrow, muted until there is something
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

/// The share of the window height the text may take before it scrolls.
const MAX_SHARE: f32 = 0.38;

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
        // Frames left in which the field scrolls to show the cursor (egui does it after typing,
        // not when a long draft is put in): a few, while the field grows to its height.
        let reveal_id = id.with("reveal-cursor");
        let mut reveal = ui.data(|d| d.get_temp::<u8>(reveal_id)).unwrap_or(0);
        if std::mem::take(&mut self.focus_composer) {
            // The text was just put in (a chat's draft, a starter, a scripted draft): the cursor
            // goes to its end.
            let mut state = egui::TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
            let end = egui::text::CCursor::new(self.input.chars().count());
            state.cursor.set_char_range(Some(egui::text::CCursorRange::one(end)));
            egui::TextEdit::store_state(ui.ctx(), id, state);
            ui.memory_mut(|m| m.request_focus(id));
            reveal = 2;
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
        let enter = focused && !menu && ui.input_mut(consume_plain_enter);
        // Cmd/Ctrl+B and +I wrap the selection in `**` / `*` (or unwrap it).
        let wrap = if focused && !menu {
            ui.input_mut(|i| {
                if i.consume_key(egui::Modifiers::COMMAND, egui::Key::B) {
                    Some("**")
                } else if i.consume_key(egui::Modifiers::COMMAND, egui::Key::I) {
                    Some("*")
                } else {
                    None
                }
            })
        } else {
            None
        };
        if let Some(marker) = wrap {
            let mut state = egui::TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
            let end = egui::text::CCursor::new(self.input.chars().count());
            let [a, b] = state.cursor.char_range().unwrap_or(egui::text::CCursorRange::one(end)).sorted_cursors();
            let (text, sel) = crate::compose_md::toggle(&self.input, a.index.0..b.index.0, marker);
            self.input = text;
            let range = egui::text::CCursorRange::two(egui::text::CCursor::new(sel.start), egui::text::CCursor::new(sel.end));
            state.cursor.set_char_range(Some(range));
            egui::TextEdit::store_state(ui.ctx(), id, state);
        }
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
        let (outer_radius, control_radius) = radii(t, d);
        let frame = egui::Frame::new()
            .fill(t.field_bg)
            .stroke(Stroke::new(FRAME_STROKE, t.field_border))
            .corner_radius(outer_radius)
            .inner_margin(Margin::same(FRAME_MARGIN));
        let mut prepared = frame.begin(ui);
        let mut rects = Rects::default();
        {
            let ui = &mut prepared.content_ui;
            ui.set_min_width(ui.available_width());
            if self.uploads.chips(ui, t) {
                ui.add_space(t.pad(4.0));
            }
            // One row: paperclip, the text, the send control. One line of text is as tall as
            // the controls; longer text grows the row and the controls stay on its bottom edge.
            // egui lays out in one pass, so the row takes the text's height from the last frame.
            let height_id = id.with("row-height");
            let row_height = ui.data(|data| data.get_temp::<f32>(height_id)).unwrap_or(d).max(d);
            let line = ui.text_style_height(&egui::TextStyle::Body);
            #[allow(clippy::cast_possible_truncation)] // a few points
            let pad_y = ((d - line) / 2.0).clamp(0.0, 40.0).floor() as i8;
            let max_height = max_height(line, f32::from(pad_y), ui.ctx().content_rect().height(), d);
            // Placed by hand inside the row: nested egui layouts would put the scroll area
            // below a fixed-height row.
            let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), row_height), Sense::hover());
            let gap = 6.0;
            let attach_rect = Rect::from_min_size(egui::pos2(row.left(), row.bottom() - d), vec2(d, d));
            rects.attach = ui.scope_builder(egui::UiBuilder::new().max_rect(attach_rect), |ui| self.uploads.button(ui, t, d, control_radius)).inner.rect;
            let controls_rect = Rect::from_min_max(egui::pos2(attach_rect.right() + gap, row.bottom() - d), row.max);
            let controls_left = ui
                .scope_builder(egui::UiBuilder::new().max_rect(controls_rect).layout(egui::Layout::right_to_left(egui::Align::Center)), |ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    if editing.is_some() {
                        let save = pill(ui, t, "Save", d, control_radius, true).on_hover_text("Save the queued message (Enter)");
                        send |= save.clicked();
                        rects.send = save.rect;
                        rects.send_fill = t.accent;
                        let cancel = pill(ui, t, "Cancel", d, control_radius, false).on_hover_text("Stop editing (Esc)");
                        cancel_edit |= cancel.clicked();
                        rects.second = cancel.rect;
                    } else if busy {
                        // Stop is the strong one until there's a follow-up to queue.
                        let fill = if ready { t.button_bg } else { t.text_strong };
                        let icon = if ready { t.text_strong } else { t.field_bg };
                        let stop = round_button(ui, t, d, control_radius, fill, icon, icons::stop, true).on_hover_text("Stop the reply");
                        if stop.clicked() {
                            self.conn.send(ClientMsg::Cancel);
                        }
                        rects.send = stop.rect;
                        rects.send_fill = fill;
                        if ready {
                            let queue = pill(ui, t, "Queue", d, control_radius, true).on_hover_text("Send this when the reply is done (Enter)");
                            send |= queue.clicked();
                            rects.second = queue.rect;
                        }
                    } else {
                        let (fill, icon) = if ready { (t.accent, t.on_accent) } else { (t.button_bg, t.text_faint) };
                        let tip = if ready {
                            "Send (Enter; Shift+Enter for a new line)"
                        } else if self.uploads.busy() {
                            "Waiting for the files to upload"
                        } else {
                            "Type a message to send"
                        };
                        let b = round_button(ui, t, d, control_radius, fill, icon, icons::arrow_up, ready).on_hover_text(tip);
                        send |= b.clicked();
                        rects.send = b.rect;
                        rects.send_fill = fill;
                    }
                    ui.min_rect().left()
                })
                .inner;
            // The text takes what's left between the paperclip and the controls.
            let text_rect = Rect::from_min_max(
                egui::pos2(attach_rect.right() + gap, row.top()),
                egui::pos2((controls_left - gap).max(attach_rect.right() + gap + 40.0), row.bottom()),
            );
            ui.scope_builder(egui::UiBuilder::new().max_rect(text_rect).layout(egui::Layout::top_down(egui::Align::Min)), |ui| {
                // What the box is for, with its keys where the layout shows them (they're
                // also in the buttons' tooltips); shorter where the long one would wrap.
                let (long, short, tiny) = match (editing.is_some(), busy) {
                    (true, _) => ("Edit the queued message…  (Enter to save, Esc to cancel)", "Edit the queued message…", "Edit…"),
                    (false, true) => ("Queue a follow-up…  (Enter to queue, Shift+Enter for a new line)", "Queue a follow-up…", "Queue…"),
                    (false, false) => ("Message byob…  (Enter to send, Shift+Enter for a new line)", "Message byob…", "Message…"),
                };
                let keys = crate::layout::visible(ui.ctx(), "composer.keys", false);
                let room = ui.available_width() - 8.0;
                let fits = |hint: &str| ui.painter().layout_no_wrap(hint.to_owned(), egui::TextStyle::Body.resolve(ui.style()), t.text).size().x <= room;
                let hint = [long].into_iter().filter(|_| keys).chain([short]).find(|h| fits(h)).unwrap_or(tiny);
                let input = &mut self.input;
                let tk = *t;
                // Live Markdown: the text stays as typed, styled as it is laid out.
                let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, wrap: f32| {
                    let mut job = (*crate::compose_md::cached(ui, &tk, id, buf.as_str()).job).clone();
                    job.wrap.max_width = wrap;
                    ui.fonts_mut(|f| f.layout_job(job))
                };
                let field = egui::ScrollArea::vertical()
                    .id_salt("composer-scroll")
                    .max_height(max_height)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        let edit = egui::TextEdit::multiline(input)
                            .id(id)
                            .frame(egui::Frame::NONE.inner_margin(Margin::symmetric(4, pad_y)))
                            .desired_rows(1)
                            // While the `/` menu is open Tab picks from it (consumed above), and must not
                            // move the focus away from the text.
                            .lock_focus(menu)
                            // Plain Enter sends (consumed above); Shift+Enter is the newline.
                            .return_key(egui::KeyboardShortcut::new(egui::Modifiers::SHIFT, egui::Key::Enter))
                            .desired_width(f32::INFINITY)
                            .hint_text(hint)
                            .layouter(&mut layouter);
                        // Code blocks get a background behind their whole lines, under the text.
                        let behind = ui.painter().add(egui::Shape::Noop);
                        let out = edit.show(ui);
                        let styled = crate::compose_md::cached(ui, &tk, id, input.as_str());
                        if !styled.code_blocks.is_empty() {
                            let origin = out.galley_pos - vec2(out.galley.rect.left(), 0.0);
                            let r = out.response.response.rect;
                            let total = input.chars().count();
                            let shapes =
                                crate::compose_md::code_backgrounds(&tk, &out.galley, origin, r.left() + 1.0, r.right() - 1.0, &styled.code_blocks, total);
                            ui.painter().set(behind, egui::Shape::Vec(shapes));
                        }
                        if reveal > 0
                            && let Some(c) = out.cursor_range
                        {
                            let origin = out.galley_pos - vec2(out.galley.rect.left(), 0.0);
                            let at = out.galley.pos_from_cursor(c.primary).translate(origin.to_vec2()).expand(2.0);
                            ui.scroll_to_rect_animation(at, None, egui::style::ScrollAnimation::none());
                        }
                        out.response.response
                    })
                    .inner;
                rects.field = field.rect;
                let shown = field.rect.height().min(max_height).max(d);
                let growing = (shown - row_height).abs() > 0.5;
                if reveal > 0 {
                    // Count down once the field has stopped growing.
                    ui.data_mut(|d| d.insert_temp(reveal_id, if growing { reveal } else { reveal - 1 }));
                    ui.ctx().request_repaint();
                }
                if growing {
                    ui.data_mut(|data| data.insert_temp(height_id, shown));
                    ui.ctx().request_repaint();
                }
            });
        }
        let focused = ui.memory(|m| m.has_focus(id));
        if focused {
            prepared.frame.stroke = Stroke::new(1.0, t.accent);
        }
        let outer = prepared.end(ui).rect;
        if focused {
            // A soft ring outside the border.
            // Outside the frame, so its radius grows by the ring's offset (the same rule outwards).
            let radius = CornerRadius::from(outer_radius + 0.5);
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

/// How tall the text may grow before it scrolls: [`MAX_LINES`] lines, but no more than
/// [`MAX_SHARE`] of the window (small windows and phones keep their transcript), and never
/// less than the controls.
pub fn max_height(line: f32, pad_y: f32, window: f32, d: f32) -> f32 {
    let lines = line * MAX_LINES + pad_y * 2.0;
    let share = if window.is_finite() && window > 0.0 { window * MAX_SHARE } else { lines };
    lines.min(share).max(d)
}

/// Consume an Enter pressed with no modifiers. (`consume_key` ignores Shift, so it would
/// take Shift+Enter, the newline, too.)
fn consume_plain_enter(i: &mut egui::InputState) -> bool {
    let before = i.events.len();
    i.events.retain(|e| !matches!(e, egui::Event::Key { key: egui::Key::Enter, pressed: true, modifiers, .. } if modifiers.is_none()));
    i.events.len() != before
}

/// A round button `d` across: `fill` with `paint` in `icon` on it, a shade lighter or darker
/// on hover. Not `enabled`: it only shows its tooltip.
/// The composer frame's inner margin and border, in points: what its controls are inset by.
const FRAME_MARGIN: i8 = 6;
const FRAME_STROKE: f32 = 1.0;

/// The composer frame's corner radius and its controls': at the default corner radius the
/// frame is a pill around round controls; smaller radii square both off together, the
/// controls always `outer − inset` ([`crate::theme::inner_radius`]) so the corners stay
/// concentric.
pub fn radii(t: &Tokens, d: f32) -> (f32, f32) {
    let inset = f32::from(FRAME_MARGIN) + FRAME_STROKE;
    let outer = t.round(d / 2.0 + inset).min(d / 2.0 + inset);
    (outer, crate::theme::inner_radius(outer, inset).min(d / 2.0))
}

#[allow(clippy::too_many_arguments)] // a painter helper: size, shape, colours, icon, state
pub fn round_button(ui: &mut Ui, t: &Tokens, d: f32, radius: f32, fill: Color32, icon: Color32, paint: icons::Paint, enabled: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(d, d), if enabled { Sense::click() } else { Sense::hover() });
    if ui.is_rect_visible(rect) {
        let fill = if enabled && resp.is_pointer_button_down_on() {
            crate::theme::mix(fill, if t.dark { Color32::BLACK } else { Color32::WHITE }, 0.2)
        } else if enabled && resp.hovered() {
            crate::theme::mix(fill, if t.dark { Color32::WHITE } else { Color32::BLACK }, 0.12)
        } else {
            fill
        };
        ui.painter().rect_filled(rect, radius, fill);
        paint(ui.painter(), Rect::from_center_size(rect.center(), vec2(d * 0.55, d * 0.55)), icon);
    }
    resp
}

/// A frameless round icon button (the paperclip): tinted on hover.
pub fn ghost_icon(ui: &mut Ui, t: &Tokens, d: f32, radius: f32, paint: icons::Paint, enabled: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(d, d), if enabled { Sense::click() } else { Sense::hover() });
    if ui.is_rect_visible(rect) {
        let hot = enabled && resp.hovered();
        if enabled && resp.is_pointer_button_down_on() {
            ui.painter().rect_filled(rect, radius, t.pressed);
        } else if hot {
            ui.painter().rect_filled(rect, radius, t.hover);
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
pub fn pill(ui: &mut Ui, t: &Tokens, label: &str, d: f32, radius: f32, primary: bool) -> Response {
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
            ui.painter().rect_filled(rect, radius, fill);
        }
        ui.painter().galley(rect.center() - galley.size() / 2.0, galley, color);
    }
    resp
}
