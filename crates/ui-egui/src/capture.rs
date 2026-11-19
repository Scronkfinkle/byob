//! Screenshots of the window for the model (the `ui_screenshot` tool; cap
//! [`byob_protocol::look::CAP_CAPTURE`]).
//!
//! The engine sends [`ServerMsg::Capture`](byob_protocol::ServerMsg::Capture); the window
//! asks egui for a screenshot of itself (which paints it even while it's hidden, minimized
//! or covered), scales it down to [`CAPTURE_SIDE`], and answers with a PNG, or a JPEG when
//! the PNG would be too big.

use std::io::Cursor;
use std::sync::{Arc, Mutex, PoisonError};

use byob_protocol::look::CAPTURE_SIDE;
use byob_protocol::media::{self, MAX_IMAGE_BYTES};
use byob_protocol::{ClientMsg, Connection};

/// Screenshots asked for, and those taken and not yet sent.
#[derive(Default)]
pub struct Captures {
    asked: Vec<u64>,
    taken: Arc<Mutex<Vec<(u64, Result<String, String>)>>>,
}

impl Captures {
    /// The engine asked for screenshot `request`.
    pub fn asked(&mut self, request: u64) {
        if self.asked.len() < 8 {
            self.asked.push(request);
        }
    }

    /// Ask egui for the screenshots asked for. The callback runs once the frame is painted.
    pub fn start(&mut self, ctx: &egui::Context) {
        for request in self.asked.drain(..) {
            let (taken, wake) = (Arc::clone(&self.taken), ctx.clone());
            ctx.request_screenshot(move |image| {
                let result = encode(&image);
                taken.lock().unwrap_or_else(PoisonError::into_inner).push((request, result));
                wake.request_repaint();
            });
        }
    }

    /// Send the screenshots taken.
    pub fn send(&mut self, conn: &mut dyn Connection) {
        let taken = std::mem::take(&mut *self.taken.lock().unwrap_or_else(PoisonError::into_inner));
        for (request, result) in taken {
            let (image, error) = match result {
                Ok(i) => (Some(i), None),
                Err(e) => (None, Some(e)),
            };
            conn.send(ClientMsg::Captured { request, image, error });
        }
    }
}

/// The screenshot, scaled to fit [`CAPTURE_SIDE`], as base64 PNG (JPEG if the PNG is too big).
pub fn encode(shot: &egui::ColorImage) -> Result<String, String> {
    let [w, h] = shot.size;
    let (w, h) = (u32::try_from(w).map_err(|e| e.to_string())?, u32::try_from(h).map_err(|e| e.to_string())?);
    if w == 0 || h == 0 {
        return Err("the window has no size".into());
    }
    let rgba = image::RgbaImage::from_raw(w, h, shot.as_raw().to_vec()).ok_or("the screenshot's pixels don't match its size")?;
    let mut rgb = image::DynamicImage::ImageRgba8(rgba).into_rgb8();
    let long = w.max(h);
    if long > CAPTURE_SIDE {
        let scale = f64::from(CAPTURE_SIDE) / f64::from(long);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // within 1..=CAPTURE_SIDE
        let (sw, sh) = (((f64::from(w) * scale).round() as u32).max(1), ((f64::from(h) * scale).round() as u32).max(1));
        rgb = image::imageops::resize(&rgb, sw, sh, image::imageops::FilterType::Triangle);
    }
    let mut png = Vec::new();
    rgb.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png).map_err(|e| e.to_string())?;
    if png.len() <= MAX_IMAGE_BYTES {
        return Ok(media::encode(&png));
    }
    // A busy backdrop makes big PNGs.
    for quality in [85, 60] {
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, quality).encode_image(&rgb).map_err(|e| e.to_string())?;
        if jpeg.len() <= MAX_IMAGE_BYTES {
            return Ok(media::encode(&jpeg));
        }
    }
    Err("the screenshot is too big to send".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoded(b64: &str) -> image::DynamicImage {
        image::load_from_memory(&media::decode(b64).unwrap()).unwrap()
    }

    /// A HiDPI window comes out scaled down to CAPTURE_SIDE, the same shape, as a PNG.
    #[test]
    fn screenshots_are_scaled_to_fit() {
        let shot = egui::ColorImage::filled([3136, 1960], egui::Color32::from_rgb(200, 30, 40));
        let out = encode(&shot).unwrap();
        assert!(media::decode(&out).unwrap().starts_with(b"\x89PNG"));
        let img = decoded(&out);
        assert_eq!((img.width(), img.height()), (CAPTURE_SIDE, 980));
        let small = egui::ColorImage::filled([800, 600], egui::Color32::WHITE);
        let img = decoded(&encode(&small).unwrap());
        assert_eq!((img.width(), img.height()), (800, 600), "small windows stay as they are");
        assert!(encode(&egui::ColorImage::filled([0, 10], egui::Color32::WHITE)).is_err());
    }

    /// Noise doesn't compress: the PNG would pass the limit, so it goes as a JPEG.
    #[test]
    fn a_busy_screenshot_goes_as_jpeg() {
        let mut n: u32 = 1;
        let pixels = (0..CAPTURE_SIDE * 1000)
            .map(|_| {
                n ^= n << 13;
                n ^= n >> 17;
                n ^= n << 5;
                let [r, g, b, _] = n.to_le_bytes();
                egui::Color32::from_rgb(r, g, b)
            })
            .collect();
        let shot = egui::ColorImage { size: [CAPTURE_SIDE as usize, 1000], pixels, source_size: egui::vec2(CAPTURE_SIDE as f32, 1000.0) };
        let bytes = media::decode(&encode(&shot).unwrap()).unwrap();
        assert!(bytes.starts_with(&[0xff, 0xd8, 0xff]) && bytes.len() <= MAX_IMAGE_BYTES);
    }

    /// Asked for, the screenshot goes to eframe as a viewport command, which paints the
    /// window for it even while hidden.
    #[test]
    fn a_capture_asks_egui_for_a_screenshot() {
        let ctx = egui::Context::default();
        let mut c = Captures::default();
        c.asked(5);
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| c.start(ui.ctx()));
        out.textures_delta.clear();
        let commands = &out.viewport_output[&egui::ViewportId::ROOT].commands;
        assert!(commands.iter().any(|c| matches!(c, egui::ViewportCommand::Screenshot(_))), "{commands:?}");
        assert!(c.asked.is_empty());
    }
}
