//! Settings → Appearance → What's shown: the layout (`byob_protocol::layout`) as presets and
//! a list of every part, by region, with its mode and (in a region) up and down. The engine
//! keeps the layout; a change goes to it and comes back to every window.

use std::sync::Arc;

use byob_protocol::ClientMsg;
use byob_protocol::layout::{ELEMENTS, GROUPS, Layout, PRESETS, REGIONS, Show, element, group};
use egui::RichText;

use crate::ByobApp;
use crate::theme::Tokens;

fn mode_label(s: Show) -> &'static str {
    match s {
        Show::Shown => "Shown",
        Show::Hover => "On hover",
        Show::Hidden => "Hidden",
    }
}

impl ByobApp {
    /// Use `layout` now and have the engine keep it (and tell the other windows).
    fn change_layout(&mut self, layout: Layout) {
        let layout = layout.sanitized();
        if layout != *self.layout {
            self.layout = Arc::new(layout.clone());
            self.conn.send(ClientMsg::SetLayout { layout });
        }
    }

    pub(super) fn layout_presets(&mut self, ui: &mut egui::Ui, t: &Tokens) {
        let current = (*self.layout).clone();
        // A preset is about modes; the order is kept. None matching: "Custom".
        let presets: Vec<(&'static str, Layout)> = PRESETS.iter().filter_map(|(name, _)| Layout::preset(name).map(|p| (*name, p))).collect();
        let mut now: Option<&'static str> = presets.iter().find(|(_, p)| p.show == current.show).map(|(n, _)| *n);
        let was = now;
        let labels: Vec<String> = presets.iter().map(|(n, _)| capitalized(n)).collect();
        let choices: Vec<(Option<&'static str>, &str)> = presets.iter().zip(&labels).map(|((n, _), l)| (Some(*n), l.as_str())).collect();
        if was.is_none() {
            ui.label(RichText::new("Custom").color(t.text_faint));
            ui.add_space(t.pad(6.0));
        }
        crate::form::select(ui, t, &mut now, &choices, None);
        if now != was
            && let Some(p) = presets.iter().find(|(n, _)| Some(*n) == now).map(|(_, p)| p.clone())
        {
            self.change_layout(Layout { show: p.show, order: current.order, chat_list: current.chat_list });
        }
    }

    /// Every part, one card per area (the header, messages, code blocks, …) with its name
    /// above it, then a card to put them all back.
    pub(super) fn layout_list(&mut self, ui: &mut egui::Ui, t: &Tokens) {
        let mut layout = (*self.layout).clone();
        let views = self.plugins.sites.status_ids();
        for r in REGIONS {
            let extra: &[String] = if r.plugin_views { &views } else { &[] };
            let ids = layout.arrange(r.id, extra);
            if ids.is_empty() {
                continue;
            }
            group_heading(ui, t, &capitalized(r.describe));
            crate::form::card(t).show(ui, |ui| {
                let width = ui.available_width();
                let n = ids.len();
                for (i, id) in ids.iter().enumerate() {
                    let order = Some((i > 0, i + 1 < n));
                    match self.layout_row(ui, t, &mut layout, id, width, order, i == 0) {
                        Some(-1) => layout.nudge(r.id, extra, id, -1),
                        Some(1) => layout.nudge(r.id, extra, id, 1),
                        _ => {}
                    }
                }
            });
        }
        for (_, name) in GROUPS {
            let members: Vec<_> = ELEMENTS.iter().filter(|e| e.region.is_none() && group(e.id) == *name).collect();
            if members.is_empty() {
                continue;
            }
            group_heading(ui, t, name);
            crate::form::card(t).show(ui, |ui| {
                let width = ui.available_width();
                for (i, e) in members.iter().enumerate() {
                    self.layout_row(ui, t, &mut layout, e.id, width, None, i == 0);
                }
            });
        }
        ui.add_space(t.pad(16.0));
        crate::form::card(t).show(ui, |ui| {
            let id = egui::Id::unique("reset-shown");
            let style = crate::form::RowStyle { first: true, ..crate::form::RowStyle::default() };
            crate::form::setting_row(ui, t, id, "Reset what's shown", "Every part back to how byob shows it.", style, |ui| {
                if crate::form::button(ui, t, "Reset", !layout.is_default()).clicked() {
                    layout = Layout::default();
                }
            });
        });
        self.change_layout(layout);
    }

    /// One part as a row: its name (dim when hidden; what it is on hover) on the left; on the
    /// right how it's shown, and (in an ordered region) earlier / later. Returns a move.
    #[allow(clippy::too_many_arguments)] // one row: the layout, the part, its width, its moves, whether it's first
    fn layout_row(&self, ui: &mut egui::Ui, t: &Tokens, layout: &mut Layout, id: &str, width: f32, order: Option<(bool, bool)>, first: bool) -> Option<i8> {
        let e = element(id);
        let (label, what) = match e {
            Some(e) => (e.label.to_owned(), capitalized(e.describe)),
            None => (self.plugins.sites.status_label(id).unwrap_or_else(|| id.to_owned()), "A plugin's item in the header".to_owned()),
        };
        let hover_ok = e.is_none_or(|e| e.hover);
        let mut mode = layout.show(id);
        let mut moved = None;
        let h = 44.0;
        let (rect, _) = ui.allocate_exact_size(egui::vec2(width, h), egui::Sense::hover());
        if !first {
            ui.painter().hline(rect.x_range(), rect.top(), egui::Stroke::new(1.0, t.separator.gamma_multiply(0.7)));
        }
        let mut child =
            ui.new_child(egui::UiBuilder::new().id_salt(("layout-row", id)).max_rect(rect).layout(egui::Layout::right_to_left(egui::Align::Center)));
        child.spacing_mut().item_spacing.x = 2.0;
        // The select at the right edge (so every row's lines up with the other settings),
        // then earlier / later to its left, only while the pointer is on the row.
        let choices: Vec<(Show, &str)> = Show::ALL.into_iter().filter(|s| hover_ok || *s != Show::Hover).map(|s| (s, mode_label(s))).collect();
        crate::form::select(&mut child, t, &mut mode, &choices, None);
        if let Some((up, down)) = order
            && ui.rect_contains_pointer(rect)
        {
            child.add_space(8.0);
            for (dir, can, paint, tip) in
                [(1_i8, down, crate::icons::chevron_down as crate::icons::Paint, "Later"), (-1, up, crate::icons::chevron_up, "Earlier")]
            {
                if can {
                    if crate::icons::icon_button(&mut child, t, paint, None, false).on_hover_text(tip).clicked() {
                        moved = Some(dir);
                    }
                } else {
                    child.add_space(crate::icons::button_height(&child));
                }
            }
        }
        if mode != layout.show(id) {
            layout.set_show(id, mode);
        }
        let color = if mode == Show::Hidden { t.text_faint } else { t.text };
        let g = ui.painter().layout_no_wrap(label, egui::TextStyle::Body.resolve(ui.style()), color);
        let name = egui::Rect::from_min_size(egui::pos2(rect.left(), rect.center().y - g.size().y / 2.0), g.size());
        ui.painter().galley(name.min, g, color);
        ui.interact(name, egui::Id::unique(("layout-name", id)), egui::Sense::hover()).on_hover_text(format!("{what}\n\n{id}"));
        moved
    }
}

/// A small heading over a group of parts.
/// An area's name over its card: small and quiet, under the section's title.
fn group_heading(ui: &mut egui::Ui, t: &Tokens, text: &str) {
    ui.add_space(t.pad(14.0));
    ui.label(RichText::new(text).size(t.text_size(12.5)).color(t.text_dim));
    ui.add_space(t.pad(6.0));
}

fn capitalized(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}
