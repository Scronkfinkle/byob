//! The settings panel: inference endpoint (engine-wide) and appearance (this device only).

use yah_protocol::ClientMsg;

use crate::YahApp;
use crate::theme::{Accent, Prefs, ThemeKind};

impl YahApp {
    pub(crate) fn settings(&mut self, ui: &mut egui::Ui) {
        let t = self.tokens;
        ui.add_space(4.0);
        ui.label(egui::RichText::new("Inference").family(crate::theme::bold_or_regular(ui.ctx())).color(t.text_strong));
        self.inference_settings(ui);
        ui.add_space(6.0);
        ui.label(egui::RichText::new("Appearance").family(crate::theme::bold_or_regular(ui.ctx())).color(t.text_strong))
            .on_hover_text("Saved on this device only");
        let mut prefs = self.prefs;
        appearance(ui, &mut prefs, &t);
        if prefs != self.prefs {
            self.set_prefs(prefs);
        }
        ui.add_space(6.0);
    }

    fn inference_settings(&mut self, ui: &mut egui::Ui) {
        let t = self.tokens;
        let has_key = self.state.as_ref().is_some_and(|s| s.has_api_key);
        egui::Grid::new("inference").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label("Inference URL");
            ui.add(egui::TextEdit::singleline(&mut self.form_url).desired_width(360.0));
            ui.end_row();
            ui.label("Model");
            ui.add(egui::TextEdit::singleline(&mut self.form_model).desired_width(360.0));
            ui.end_row();
            ui.label("API key");
            let hint = if has_key { "(set — leave blank to keep)" } else { "(optional)" };
            ui.add(egui::TextEdit::singleline(&mut self.form_key).password(true).hint_text(hint).desired_width(360.0));
            ui.end_row();
        });
        ui.horizontal(|ui| {
            if ui.button("Apply").clicked() {
                let api_key = Some(std::mem::take(&mut self.form_key)).filter(|k| !k.is_empty());
                self.conn.send(ClientMsg::SetInference { base_url: self.form_url.clone(), model: self.form_model.clone(), api_key });
            }
            let endpoint = self.state.as_ref().and_then(|s| s.endpoint.clone()).unwrap_or_else(|| "(invalid URL)".into());
            ui.label(egui::RichText::new(endpoint).color(t.text_dim));
        });
    }
}

fn appearance(ui: &mut egui::Ui, prefs: &mut Prefs, t: &crate::theme::Tokens) {
    egui::Grid::new("appearance").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
        ui.label("Theme");
        ui.horizontal(|ui| {
            for kind in ThemeKind::ALL {
                ui.selectable_value(&mut prefs.theme, kind, kind.label());
            }
        });
        ui.end_row();

        ui.label("Accent");
        ui.horizontal(|ui| {
            for accent in Accent::ALL {
                if swatch(ui, accent.color(t.dark), prefs.accent == accent, t).on_hover_text(accent.label()).clicked() {
                    prefs.accent = accent;
                }
            }
        });
        ui.end_row();

        ui.label("Text size");
        ui.horizontal(|ui| {
            for (scale, name) in Prefs::FONT_SCALES {
                let selected = (prefs.font_scale - scale).abs() < 0.01;
                if ui.selectable_label(selected, name).clicked() {
                    prefs.font_scale = scale;
                }
            }
        });
        ui.end_row();

        ui.label("Contrast");
        ui.checkbox(&mut prefs.high_contrast, "High contrast");
        ui.end_row();
    });
}

/// A round colour button, ringed when selected.
fn swatch(ui: &mut egui::Ui, color: egui::Color32, selected: bool, t: &crate::theme::Tokens) -> egui::Response {
    let size = ui.spacing().interact_size.y;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
    let center = rect.center();
    let r = size * 0.5;
    if selected {
        ui.painter().circle_stroke(center, r - 1.0, egui::Stroke::new(2.0, t.text_strong));
    } else if resp.hovered() {
        ui.painter().circle_stroke(center, r - 1.0, egui::Stroke::new(1.0, t.border));
    }
    ui.painter().circle_filled(center, r - 4.0, color);
    resp
}
