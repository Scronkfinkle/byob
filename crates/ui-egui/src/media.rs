//! Charts and images inline in the conversation. Entries and steps carry [`MediaRef`]s; the
//! content is fetched once per path with `GetMedia`, decoded here (PNG/JPEG via `image`,
//! also in the browser) and kept as a texture.

use std::collections::HashMap;
use std::io::Cursor;

use egui::{Sense, Stroke, vec2};
use yah_protocol::media::{self, ChartSpec};
use yah_protocol::{ClientMsg, Entry, MediaContent, MediaKind, MediaRef};

use crate::theme::Tokens;

/// Tallest an inline image is drawn.
const MAX_IMAGE_HEIGHT: f32 = 420.0;
/// Decoder limits for images from the engine.
const MAX_DECODE_SIDE: u32 = 8192;
const MAX_DECODE_ALLOC: u64 = 256 * 1024 * 1024;

enum Item {
    /// Asked the engine; waiting for the answer.
    Requested,
    /// Answer received, not yet turned into a texture.
    Received(MediaContent),
    Chart(Box<ChartSpec>),
    Image(egui::TextureHandle),
    Failed(String),
}

/// Media content by `path` and `size` (a file a later call rewrites shows its new content).
#[derive(Default)]
pub struct Cache {
    items: HashMap<(String, u64), Item>,
    wanted: Vec<String>,
}

impl Cache {
    /// Apply a `ServerMsg::Media`.
    pub fn received(&mut self, path: &str, content: &MediaContent) {
        for ((p, _), item) in &mut self.items {
            if p == path && matches!(item, Item::Requested) {
                *item = Item::Received(content.clone());
            }
        }
    }

    /// Some media is still on its way (for scripted screenshots).
    pub fn loading(&self) -> bool {
        self.items.values().any(|i| matches!(i, Item::Requested | Item::Received(_)))
    }

    /// `GetMedia` requests to send.
    pub fn take_requests(&mut self) -> Vec<ClientMsg> {
        let mut paths = std::mem::take(&mut self.wanted);
        paths.dedup();
        paths.into_iter().map(|path| ClientMsg::GetMedia { path }).collect()
    }

    /// Draw the user's attachments and the media of every step of `entry`.
    pub fn show_entry(&mut self, ui: &mut egui::Ui, t: &Tokens, entry: &Entry) {
        let refs: Vec<&MediaRef> = entry.attachments.iter().chain(entry.steps.iter().flat_map(|s| s.media.iter())).collect();
        if refs.is_empty() {
            return;
        }
        let (files, shown): (Vec<&MediaRef>, Vec<&MediaRef>) = refs.into_iter().partition(|r| r.kind == MediaKind::File);
        if !files.is_empty() {
            ui.horizontal_wrapped(|ui| {
                for r in files {
                    chip(ui, t, r);
                }
            });
        }
        for r in shown {
            self.show(ui, t, r);
        }
    }

    /// Draw one chart or image, requesting it first if needed.
    pub fn show(&mut self, ui: &mut egui::Ui, t: &Tokens, r: &MediaRef) {
        let key = (r.path.clone(), r.size);
        let item = self.items.entry(key.clone()).or_insert_with(|| {
            self.wanted.push(r.path.clone());
            Item::Requested
        });
        if let Item::Received(content) = item {
            *item = convert(ui.ctx(), &r.path, content);
        }
        match item {
            Item::Requested | Item::Received(_) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(egui::RichText::new(format!("Loading {}…", r.name())).small().color(t.text_dim));
                });
            }
            Item::Chart(spec) => crate::chart::show(ui, t, spec, egui::Id::unique(("chart", &key))),
            Item::Image(tex) => image(ui, t, tex, r),
            Item::Failed(e) => {
                ui.label(egui::RichText::new(format!("Couldn't show {}: {e}", r.name())).small().color(t.text_dim));
            }
        }
    }
}

fn convert(ctx: &egui::Context, path: &str, content: &MediaContent) -> Item {
    match content {
        MediaContent::Chart { spec } => Item::Chart(Box::new(spec.clone())),
        MediaContent::Error { message } => Item::Failed(message.clone()),
        MediaContent::Image { data, .. } => match media::decode(data).and_then(|b| decode_image(&b)) {
            Ok(img) => Item::Image(ctx.load_texture(path, img, egui::TextureOptions::LINEAR)),
            Err(e) => Item::Failed(e),
        },
    }
}

/// PNG/JPEG bytes → pixels, refusing images too large to hold.
pub fn decode_image(bytes: &[u8]) -> Result<egui::ColorImage, String> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format().map_err(|e| format!("unreadable image: {e}"))?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DECODE_SIDE);
    limits.max_image_height = Some(MAX_DECODE_SIDE);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    let img = reader.decode().map_err(|e| format!("unreadable image: {e}"))?.to_rgba8();
    let size = [usize::try_from(img.width()).unwrap_or(0), usize::try_from(img.height()).unwrap_or(0)];
    Ok(egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw()))
}

fn image(ui: &mut egui::Ui, t: &Tokens, tex: &egui::TextureHandle, r: &MediaRef) {
    let natural = tex.size_vec2();
    if natural.x <= 0.0 || natural.y <= 0.0 {
        return;
    }
    let scale = (ui.available_width() / natural.x).min(MAX_IMAGE_HEIGHT / natural.y).min(1.0);
    let size = natural * scale;
    egui::Frame::new().stroke(Stroke::new(1.0, t.border)).corner_radius(8.0).inner_margin(4.0).show(ui, |ui| {
        ui.add(egui::Image::from_texture(egui::load::SizedTexture::new(tex.id(), size)).corner_radius(6.0)).on_hover_text(&r.path);
        ui.label(egui::RichText::new(r.name()).small().color(t.text_dim));
    });
}

/// A small file chip: name and size.
pub fn chip(ui: &mut egui::Ui, t: &Tokens, r: &MediaRef) {
    egui::Frame::new().fill(t.button_bg).stroke(Stroke::new(1.0, t.border)).corner_radius(6.0).inner_margin(vec2(8.0, 4.0)).show(ui, |ui| {
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(10.0, 12.0), Sense::hover());
            ui.painter().rect_stroke(rect, 2.0, Stroke::new(1.0, t.text_dim), egui::StrokeKind::Inside);
            ui.label(egui::RichText::new(r.name()).small().color(t.text));
            if r.size > 0 {
                ui.label(egui::RichText::new(media::size_label(r.size)).small().color(t.text_dim));
            }
        })
        .response
        .on_hover_text(&r.path);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use yah_protocol::EntryRole;

    fn png() -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(6, 4, image::Rgba([10, 20, 30, 255]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(img).write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
        out
    }

    #[test]
    fn decodes_png_and_refuses_garbage() {
        let img = decode_image(&png()).unwrap();
        assert_eq!(img.size, [6, 4]);
        assert!(decode_image(b"not an image").is_err());
        assert!(decode_image(b"\x89PNG\r\n\x1a\n").is_err());
    }

    #[test]
    fn requests_once_then_draws_what_arrives() {
        let spec = yah_types::chart::validate(&serde_json::json!({"type": "bar", "x": "m", "series": ["v"], "data": [{"m": "a", "v": 1}]})).unwrap();
        let entry = Entry {
            id: 1,
            role: EntryRole::User,
            content: String::new(),
            reasoning: String::new(),
            done: true,
            error: None,
            stats: None,
            steps: vec![],
            attachments: vec![MediaRef::new("c.chart.json", 1), MediaRef::new("p.png", 2), MediaRef::new("bad.png", 3), MediaRef::new("data.csv", 4)],
        };
        let ctx = egui::Context::default();
        let mut cache = Cache::default();
        let frame = |cache: &mut Cache| {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| cache.show_entry(ui, &Tokens::dark(), &entry));
            out.textures_delta.clear();
        };
        frame(&mut cache);
        frame(&mut cache);
        let reqs = cache.take_requests();
        assert_eq!(reqs.len(), 3, "{reqs:?}");
        assert!(cache.take_requests().is_empty());
        assert!(cache.loading());
        cache.received("c.chart.json", &MediaContent::Chart { spec });
        cache.received("p.png", &MediaContent::Image { mime: "image/png".into(), data: media::encode(&png()) });
        cache.received("bad.png", &MediaContent::Image { mime: "image/png".into(), data: "!!".into() });
        frame(&mut cache);
        assert!(!cache.loading());
        assert!(matches!(cache.items.get(&("bad.png".to_owned(), 3)), Some(Item::Failed(_))));
        assert!(matches!(cache.items.get(&("p.png".to_owned(), 2)), Some(Item::Image(_))));
    }
}
