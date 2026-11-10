//! Markdown documents: rendered in the pane (the pane's Source switch shows the text), the
//! first lines without their markers in a card.

use byob_protocol::artifact::TYPE_MARKDOWN;
use egui::RichText;

use super::{ArtifactCx, ArtifactRenderer, ArtifactRenderers, Place, RendererCaps};
use crate::markdown;

pub fn register(r: &mut ArtifactRenderers) {
    r.register(TYPE_MARKDOWN, Markdown);
}

pub struct Markdown;

struct Prepared {
    text: String,
    doc: markdown::Doc,
}

fn prepared<'a>(cx: &'a mut ArtifactCx<'_>) -> super::Fetch<&'a mut Prepared> {
    cx.prepared(|bytes| {
        let text = String::from_utf8_lossy(bytes).into_owned();
        let mut doc = markdown::Doc::default();
        doc.update(&text, true);
        Ok(Prepared { text, doc })
    })
}

impl ArtifactRenderer for Markdown {
    fn caps(&self) -> RendererCaps {
        RendererCaps { source_view: true, ..RendererCaps::text("document") }
    }

    fn card(&self, ui: &mut egui::Ui, cx: &mut ArtifactCx<'_>) {
        let (t, name) = (cx.t, cx.artifact.filename.clone());
        if let Some(p) = super::ready(ui, t, Place::Card, &name, prepared(cx)) {
            // Reads better without its markers.
            super::card_lines(ui, t, &p.text, |l| RichText::new(l.trim_start_matches(['#', '>', ' '])));
        }
    }

    fn pane(&self, ui: &mut egui::Ui, cx: &mut ArtifactCx<'_>) {
        let (t, name, id) = (cx.t, cx.artifact.filename.clone(), cx.id);
        if let Some(p) = super::ready(ui, t, Place::Pane, &name, prepared(cx)) {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| markdown::show(ui, t, &p.doc, id));
        }
    }
}
