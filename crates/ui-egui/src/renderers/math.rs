//! LaTeX formulas (`text/x-tex`): each paragraph of the file is one display formula, drawn
//! by [`crate::math`]; one that doesn't typeset shows as its source. Plugins name it
//! `native:math`, which also switches on math in messages (see `ArtifactRenderers::native_in_use`).

use egui::RichText;

use super::{ArtifactCx, ArtifactRenderer, ArtifactRenderers, Place, RendererCaps};

pub const TYPE_TEX: &str = "text/x-tex";
/// The id plugin manifests name it by.
pub const NATIVE: &str = "math";

pub fn register(r: &mut ArtifactRenderers) {
    r.register(TYPE_TEX, Math);
    r.native(NATIVE, std::rc::Rc::new(Math));
}

pub struct Math;

fn text_of<'a>(cx: &'a mut ArtifactCx<'_>) -> super::Fetch<&'a mut String> {
    cx.prepared(|bytes| Ok(String::from_utf8_lossy(bytes).into_owned()))
}

/// The file's formulas: paragraphs, with `$$` fences taken off.
fn formulas(text: &str) -> Vec<&str> {
    text.split("\n\n").map(|p| p.trim().trim_start_matches("$$").trim_end_matches("$$").trim()).filter(|p| !p.is_empty()).collect()
}

fn show(ui: &mut egui::Ui, cx: &ArtifactCx<'_>, text: &str, most: usize) {
    let t = cx.t;
    let size = ui.text_style_height(&egui::TextStyle::Body) * crate::math::SCALE;
    for (i, src) in formulas(text).into_iter().take(most).enumerate() {
        match crate::math::typeset(src, true) {
            Some(f) => crate::math::block(ui, &f, size, t.text, egui::Id::unique(("tex-formula", &cx.artifact.id, i))),
            None => {
                ui.label(RichText::new(src).monospace().color(t.text_dim));
            }
        }
    }
}

impl ArtifactRenderer for Math {
    fn caps(&self) -> RendererCaps {
        RendererCaps::text("formula")
    }

    fn card(&self, ui: &mut egui::Ui, cx: &mut ArtifactCx<'_>) {
        let (t, name) = (cx.t, cx.artifact.filename.clone());
        if let Some(text) = super::ready(ui, t, Place::Card, &name, text_of(cx)).map(|s| s.clone()) {
            show(ui, cx, &text, 2);
        }
    }

    fn pane(&self, ui: &mut egui::Ui, cx: &mut ArtifactCx<'_>) {
        let (t, name) = (cx.t, cx.artifact.filename.clone());
        if let Some(text) = super::ready(ui, t, Place::Pane, &name, text_of(cx)).map(|s| s.clone()) {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| show(ui, cx, &text, usize::MAX));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_its_paragraphs_without_fences() {
        assert_eq!(formulas("$$a^2$$\n\n\n b + c \n\n$$\\frac12$$\n"), ["a^2", "b + c", "\\frac12"]);
    }
}
