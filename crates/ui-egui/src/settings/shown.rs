//! Settings → Appearance → What's shown: the layout (`byob_protocol::layout`) as presets and
//! a list of every part, by region, with its mode and (in a region) up and down. The engine
//! keeps the layout; a change goes to it and comes back to every window.

use std::sync::Arc;

use byob_protocol::ClientMsg;
use byob_protocol::layout::{ELEMENTS, Layout, PRESETS, REGIONS, Show, element};
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
        let mut picked = None;
        let mut any = false;
        for (name, what) in PRESETS {
            let Some(p) = Layout::preset(name) else { continue };
            // A preset is about modes; the order is kept.
            let on = p.show == current.show;
            any |= on;
            if ui.selectable_label(on, capitalized(name)).on_hover_text(*what).clicked() && !on {
                picked = Some(p);
            }
        }
        if !any {
            ui.label(RichText::new("Custom").color(t.text_dim));
        }
        if let Some(p) = picked {
            self.change_layout(Layout { show: p.show, order: current.order });
        }
    }

    pub(super) fn layout_list(&mut self, ui: &mut egui::Ui, t: &Tokens) {
        let mut layout = (*self.layout).clone();
        let views = self.plugins.sites.status_ids();
        for r in REGIONS {
            let extra: &[String] = if r.plugin_views { &views } else { &[] };
            let ids = layout.arrange(r.id, extra);
            ui.add_space(t.pad(6.0));
            ui.label(RichText::new(capitalized(r.describe)).small().color(t.text_dim));
            let n = ids.len();
            for (i, id) in ids.iter().enumerate() {
                ui.horizontal(|ui| {
                    if ui.add_enabled(i > 0, egui::Button::new("↑").small()).on_hover_text("Earlier").clicked() {
                        layout.nudge(r.id, extra, id, -1);
                    }
                    if ui.add_enabled(i + 1 < n, egui::Button::new("↓").small()).on_hover_text("Later").clicked() {
                        layout.nudge(r.id, extra, id, 1);
                    }
                    self.layout_row(ui, t, &mut layout, id);
                });
            }
        }
        ui.add_space(t.pad(6.0));
        ui.label(RichText::new("Elsewhere").small().color(t.text_dim));
        for e in ELEMENTS.iter().filter(|e| e.region.is_none()) {
            ui.horizontal(|ui| self.layout_row(ui, t, &mut layout, e.id));
        }
        ui.add_space(t.pad(6.0));
        if !layout.is_default() && ui.button("Reset").on_hover_text("Every part as byob shows it by default").clicked() {
            layout = Layout::default();
        }
        self.change_layout(layout);
    }

    /// One part: its mode menu and its name (its description on hover).
    fn layout_row(&self, ui: &mut egui::Ui, t: &Tokens, layout: &mut Layout, id: &str) {
        let e = element(id);
        let (label, what) = match e {
            Some(e) => (e.label.to_owned(), capitalized(e.describe)),
            None => (self.plugins.sites.status_label(id).unwrap_or_else(|| id.to_owned()), "A plugin's item in the header".to_owned()),
        };
        let hover_ok = e.is_none_or(|e| e.hover);
        let mut mode = layout.show(id);
        egui::ComboBox::from_id_salt(("layout-mode", id)).width(96.0).selected_text(mode_label(mode)).show_ui(ui, |ui| {
            for s in Show::ALL.into_iter().filter(|s| hover_ok || *s != Show::Hover) {
                ui.selectable_value(&mut mode, s, mode_label(s));
            }
        });
        if mode != layout.show(id) {
            layout.set_show(id, mode);
        }
        let color = if mode == Show::Hidden { t.text_faint } else { t.text };
        ui.label(RichText::new(label).color(color)).on_hover_text(format!("{what}\n\n{id}"));
    }
}

fn capitalized(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}
