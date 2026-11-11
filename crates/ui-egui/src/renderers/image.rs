//! PNG and JPEG images: scaled down to fit, in the card (limited height) and the pane.

use std::rc::Rc;

use byob_protocol::artifact::{TYPE_JPEG, TYPE_PNG};

use super::{ArtifactCx, ArtifactRenderer, ArtifactRenderers, Place, RendererCaps};
use crate::theme::Tokens;

/// Tallest an image is drawn in a card.
const CARD_HEIGHT: f32 = 260.0;

pub fn register(r: &mut ArtifactRenderers) {
    let image: Rc<dyn ArtifactRenderer> = Rc::new(Image);
    r.register_rc(TYPE_PNG, Rc::clone(&image));
    r.register_rc(TYPE_JPEG, Rc::clone(&image));
    r.native("image", image);
}

pub struct Image;

fn texture<'a>(ui: &egui::Ui, cx: &'a mut ArtifactCx<'_>) -> super::Fetch<&'a mut egui::TextureHandle> {
    let ctx = ui.ctx().clone();
    let name = format!("artifact-{}-{}", cx.artifact.id, cx.version);
    cx.prepared(move |bytes| crate::media::decode_image(bytes).map(|img| ctx.load_texture(name, img, egui::TextureOptions::LINEAR)))
}

fn draw(ui: &mut egui::Ui, t: &Tokens, tex: &egui::TextureHandle, max_height: f32) {
    let natural = tex.size_vec2();
    if natural.x <= 0.0 || natural.y <= 0.0 {
        return;
    }
    let scale = (ui.available_width() / natural.x).min(max_height / natural.y).min(1.0);
    ui.add(egui::Image::from_texture(egui::load::SizedTexture::new(tex.id(), natural * scale)).corner_radius(t.round(6.0)));
}

impl ArtifactRenderer for Image {
    fn caps(&self) -> RendererCaps {
        RendererCaps { text: false, ..RendererCaps::text("image") }
    }

    fn card(&self, ui: &mut egui::Ui, cx: &mut ArtifactCx<'_>) {
        let (t, name) = (cx.t, cx.artifact.filename.clone());
        let fetch = texture(ui, cx);
        if let Some(tex) = super::ready(ui, t, Place::Card, &name, fetch) {
            draw(ui, t, tex, CARD_HEIGHT);
        }
    }

    fn pane(&self, ui: &mut egui::Ui, cx: &mut ArtifactCx<'_>) {
        let (t, name) = (cx.t, cx.artifact.filename.clone());
        let fetch = texture(ui, cx);
        if let Some(tex) = super::ready(ui, t, Place::Pane, &name, fetch) {
            egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| draw(ui, t, tex, f32::INFINITY));
        }
    }
}
