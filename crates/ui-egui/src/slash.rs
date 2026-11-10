//! The composer's `/` menu: while the message is `/` and a name being typed, the skills and
//! commands the chat can use (from `Extensions::slash`), each with its scope dimmed. ↑/↓
//! pick, Enter or Tab inserts `/name `, Esc closes it; a click inserts too. The engine
//! expands the command when the message is sent.

use byob_protocol::{SlashItem, SlashKind};
use egui::{Key, Modifiers, RichText};

use crate::theme::Tokens;

/// Most entries the menu shows.
pub const MAX_SHOWN: usize = 8;

/// The menu's state between frames.
#[derive(Debug, Default)]
pub struct SlashMenu {
    selected: usize,
    /// The composer text when Esc closed the menu: it stays closed until the text changes.
    dismissed: Option<String>,
    /// The text the selection belongs to (typing starts again at the top).
    seen: String,
    /// The composer had the keyboard focus in the last frame.
    pub had_focus: bool,
}

/// The name being typed: the text after a leading `/` while it has no whitespace.
pub fn query(input: &str) -> Option<&str> {
    let rest = input.strip_prefix('/')?;
    (!rest.contains(char::is_whitespace)).then_some(rest)
}

/// Entries matching what is typed: names starting with it first, then names containing it.
pub fn matches<'a>(items: &'a [SlashItem], input: &str) -> Vec<&'a SlashItem> {
    let Some(q) = query(input) else { return Vec::new() };
    let q = q.to_lowercase();
    let mut out: Vec<&SlashItem> = items.iter().filter(|i| i.name.starts_with(&q)).collect();
    out.extend(items.iter().filter(|i| !i.name.starts_with(&q) && i.name.contains(&q)));
    out.truncate(MAX_SHOWN);
    out
}

/// What the composer text becomes when `item` is picked.
pub fn insert(item: &SlashItem) -> String {
    format!("/{} ", item.name)
}

impl SlashMenu {
    /// The menu shows for `input` (it has `n` matches and Esc didn't close it).
    pub fn open(&mut self, input: &str, n: usize) -> bool {
        if self.dismissed.as_deref().is_some_and(|d| d != input) {
            self.dismissed = None;
        }
        if n == 0 || self.seen != input {
            self.selected = 0;
            input.clone_into(&mut self.seen);
        }
        n > 0 && self.dismissed.is_none()
    }

    /// Something was picked: the next menu starts at the top.
    pub fn reset(&mut self) {
        self.selected = 0;
        self.seen.clear();
    }

    /// The menu's keys, taken before the text field sees them: ↑/↓ move, Enter/Tab pick,
    /// Esc closes. Returns the picked index.
    pub fn keys(&mut self, ui: &egui::Ui, input: &str, n: usize) -> Option<usize> {
        self.selected = self.selected.min(n.saturating_sub(1));
        let (up, down, pick, close) = ui.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                i.consume_key(Modifiers::NONE, Key::Enter) | i.consume_key(Modifiers::NONE, Key::Tab),
                i.consume_key(Modifiers::NONE, Key::Escape),
            )
        });
        if up {
            self.selected = if self.selected == 0 { n.saturating_sub(1) } else { self.selected - 1 };
        }
        if down {
            self.selected = if self.selected + 1 >= n { 0 } else { self.selected + 1 };
        }
        if close {
            self.dismissed = Some(input.to_owned());
            return None;
        }
        pick.then_some(self.selected)
    }

    /// Draw the menu above `field` (the composer's text field); returns a clicked index.
    pub fn show(&mut self, ui: &egui::Ui, t: &Tokens, field: egui::Rect, items: &[&SlashItem]) -> Option<usize> {
        let mut picked = None;
        let width = field.width().clamp(220.0, 520.0);
        egui::Area::new(egui::Id::unique("slash-menu"))
            .order(egui::Order::Foreground)
            .fixed_pos(field.left_top() - egui::vec2(0.0, 4.0))
            .pivot(egui::Align2::LEFT_BOTTOM)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(width);
                    for (k, item) in items.iter().enumerate() {
                        let mut job = egui::text::LayoutJob::default();
                        let body = egui::TextStyle::Body.resolve(ui.style());
                        let small = egui::TextStyle::Small.resolve(ui.style());
                        let fmt = |font: &egui::FontId, color| egui::TextFormat::simple(font.clone(), color);
                        job.append(&format!("/{}", item.name), 0.0, fmt(&body, t.text_strong));
                        if !item.hint.is_empty() {
                            job.append(&item.hint, 6.0, fmt(&small, t.text_faint));
                        }
                        if !item.description.is_empty() {
                            job.append(&item.description, 10.0, fmt(&small, t.text_dim));
                        }
                        job.append(&format!("· {}", origin(item)), 8.0, fmt(&small, t.text_faint));
                        job.wrap.max_rows = 1;
                        job.wrap.max_width = width - 12.0;
                        job.wrap.break_anywhere = true;
                        // A full-width row: the selection spans the menu.
                        let galley = ui.painter().layout_job(job);
                        let (rect, row) = ui.allocate_exact_size(egui::vec2(width, galley.size().y + 6.0), egui::Sense::click());
                        if k == self.selected {
                            ui.painter().rect_filled(rect, t.round(4.0), t.selection);
                        }
                        ui.painter().galley(egui::pos2(rect.left() + 6.0, rect.center().y - galley.size().y / 2.0), galley, t.text);
                        if row.hovered() {
                            self.selected = k;
                        }
                        if row.clicked() {
                            picked = Some(k);
                        }
                    }
                    ui.label(RichText::new("↑↓ to choose · Enter to insert · Esc to close").small().color(t.text_faint));
                });
            });
        picked
    }
}

/// "this workspace · skill", "built in".
fn origin(item: &SlashItem) -> String {
    let kind = match item.kind {
        SlashKind::Builtin => return "built in".into(),
        SlashKind::Command => "command",
        SlashKind::Skill => "skill",
        SlashKind::Other => "other",
    };
    match item.scope {
        Some(scope) => format!("{kind}, {}", scope.label()),
        None => format!("{kind}, built in"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, kind: SlashKind) -> SlashItem {
        SlashItem { name: name.into(), kind, description: String::new(), hint: String::new(), scope: None, plugin: None }
    }

    #[test]
    fn only_a_name_being_typed_opens_the_menu() {
        let items = [item("compact", SlashKind::Builtin), item("review", SlashKind::Command), item("pdf-review", SlashKind::Skill)];
        assert_eq!(query("/rev"), Some("rev"));
        assert_eq!(query("/review file"), None);
        assert_eq!(query("hello /x"), None);
        let names = |input: &str| matches(&items, input).iter().map(|i| i.name.clone()).collect::<Vec<_>>();
        assert_eq!(names("/"), ["compact", "review", "pdf-review"]);
        assert_eq!(names("/rev"), ["review", "pdf-review"], "prefix matches first");
        assert_eq!(names("/Rev"), ["review", "pdf-review"]);
        assert!(names("/review now").is_empty());
        assert!(names("/zzz").is_empty());
        assert_eq!(insert(&items[1]), "/review ");
        let mut m = SlashMenu::default();
        assert!(m.open("/rev", 2));
        m.dismissed = Some("/rev".into());
        assert!(!m.open("/rev", 2), "Esc keeps it closed");
        assert!(m.open("/re", 2), "until the text changes");
    }
}
