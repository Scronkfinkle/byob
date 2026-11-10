//! Plain text, and the fallback for types without a renderer: the source, monospace.

use egui::RichText;

use super::{ArtifactCx, ArtifactRenderer, Place, RendererCaps};

pub struct Text;

fn text_of<'a>(cx: &'a mut ArtifactCx<'_>) -> super::Fetch<&'a mut String> {
    cx.prepared(|bytes| Ok(String::from_utf8_lossy(bytes).into_owned()))
}

impl ArtifactRenderer for Text {
    fn caps(&self) -> RendererCaps {
        RendererCaps::text("file")
    }

    fn card(&self, ui: &mut egui::Ui, cx: &mut ArtifactCx<'_>) {
        let (t, name) = (cx.t, cx.artifact.filename.clone());
        if let Some(text) = super::ready(ui, t, Place::Card, &name, text_of(cx)) {
            super::card_lines(ui, t, text, |l| RichText::new(l).monospace());
        }
    }

    fn pane(&self, ui: &mut egui::Ui, cx: &mut ArtifactCx<'_>) {
        let (t, name) = (cx.t, cx.artifact.filename.clone());
        if let Some(text) = super::ready(ui, t, Place::Pane, &name, text_of(cx)) {
            super::source(ui, text);
        }
    }
}
