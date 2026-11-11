//! HTML apps, on the web surface ([`crate::appview::AppView`]: the Servo helper on the
//! desktop, an iframe in the browser, else a summary with Open). Everything about them on
//! the client is here: the live card preview, the pane view with the errors the app
//! reported and "Send error to byob" (the model gets them to fix), and Open in a browser
//! tab. The engine side (prompt rules, content check, theme and error-reporting script) is
//! `byob_tools::artifact_types::html_app`; the web surface runtime itself stays in the core.

use byob_protocol::artifact::TYPE_HTML;
use egui::{RichText, Stroke};

use super::{ArtifactCx, ArtifactRenderer, ArtifactRenderers, Fetch, Place, RendererCaps};
use crate::appview::AppDoc;

pub fn register(r: &mut ArtifactRenderers) {
    r.register(TYPE_HTML, HtmlApp);
    // `web` in a manifest: the web surface.
    r.native("web", std::rc::Rc::new(HtmlApp));
    r.native("html_app", std::rc::Rc::new(HtmlApp));
}

pub struct HtmlApp;

/// The message "Send error to byob" sends.
pub fn error_report(title: &str, id: &str, version: u32, error: &str) -> String {
    format!("The artifact \"{title}\" (id {id}, version {version}) shows this error when it runs:\n```\n{error}\n```\nPlease fix it with update_artifact.")
}

fn source(cx: &mut ArtifactCx<'_>) -> Option<String> {
    match cx.bytes() {
        Fetch::Ready(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        _ => None,
    }
}

impl ArtifactRenderer for HtmlApp {
    fn caps(&self) -> RendererCaps {
        RendererCaps { web_surface: true, ..RendererCaps::text("app") }
    }

    fn card(&self, ui: &mut egui::Ui, cx: &mut ArtifactCx<'_>) {
        let (t, a, version, size) = (cx.t, cx.artifact, cx.version, cx.size());
        if cx.web().live_cards()
            && let Some(source) = source(cx)
        {
            ui.add_space(4.0);
            let doc = AppDoc { id: &a.id, url: None, title: &a.title, version, size, source: Some(&source) };
            if let Some(resp) = cx.web().card(ui, t, &doc) {
                if resp.open {
                    cx.open_in_browser();
                }
                return;
            }
        }
        ui.label(RichText::new(format!("Interactive app · {}", byob_protocol::media::size_label(size))).small().color(t.text_dim));
    }

    fn pane(&self, ui: &mut egui::Ui, cx: &mut ArtifactCx<'_>) {
        let (t, a, version, size) = (cx.t, cx.artifact, cx.version, cx.size());
        // The iframe loads the app by URL; the desktop renderer and the placeholder take the source.
        let url = if cx.web().wants_url() { cx.url().ok() } else { None };
        let source = if cx.web().wants_source() { source(cx) } else { None };
        let errors = cx.web().errors();
        if !errors.is_empty() {
            egui::Frame::new().fill(t.code_bg).stroke(Stroke::new(1.0, t.danger)).corner_radius(t.round(6.0)).inner_margin(t.pad(8.0)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                let head = if errors.len() == 1 { "The app reported an error".to_owned() } else { format!("The app reported {} errors", errors.len()) };
                ui.label(RichText::new(head).color(t.danger));
                for e in errors.iter().take(3) {
                    ui.add(egui::Label::new(RichText::new(e).small().monospace().color(t.text)).truncate());
                }
                if ui.button("Send error to byob").on_hover_text("Ask byob to fix it").clicked() {
                    cx.send_message(error_report(&a.title, &a.id, version, &errors.join("\n")));
                }
            });
            ui.add_space(4.0);
        }
        let doc = AppDoc { id: &a.id, url: url.as_deref(), title: &a.title, version, size, source: source.as_deref() };
        if cx.web().show(ui, t, &doc).open {
            cx.open_in_browser();
        }
    }

    fn actions(&self, ui: &mut egui::Ui, cx: &mut ArtifactCx<'_>, place: Place) {
        let tip = match place {
            Place::Card => "Open the app in a browser tab",
            Place::Pane => "Open in a browser tab",
        };
        if ui.button("Open").on_hover_text(tip).clicked() {
            cx.open_in_browser();
        }
    }
}
