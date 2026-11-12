//! Fitting PNG/JPEG images into a size: for clients (artifacts, media) and the model.

use std::io::Cursor;

/// Decoder limits: refuse decompression bombs.
const MAX_DECODE_SIDE: u32 = 16_384;
const MAX_DECODE_ALLOC: u64 = 512 * 1024 * 1024;

/// The MIME type of PNG or JPEG bytes.
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else {
        None
    }
}

/// The MIME type of an image byob can show the model: PNG, JPEG, GIF or WebP.
pub fn sniff_any(bytes: &[u8]) -> Option<&'static str> {
    if let Some(m) = sniff(bytes) {
        return Some(m);
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP".as_slice()) {
        Some("image/webp")
    } else {
        None
    }
}

/// [`fit`] for any image [`sniff_any`] knows: GIF (first frame) and WebP become PNG.
pub fn fit_any(bytes: Vec<u8>, max_side: u32, max_bytes: usize) -> Result<(&'static str, Vec<u8>), String> {
    let format = match sniff_any(&bytes) {
        Some("image/gif") => image::ImageFormat::Gif,
        Some("image/webp") => image::ImageFormat::WebP,
        Some(_) => return fit(bytes, max_side, max_bytes),
        None => return Err("not a PNG, JPEG, GIF or WebP image".into()),
    };
    let mut reader = image::ImageReader::new(Cursor::new(&bytes));
    reader.set_format(format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DECODE_SIDE);
    limits.max_image_height = Some(MAX_DECODE_SIDE);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    let img = reader.decode().map_err(|e| format!("unreadable image: {e}"))?;
    let mut png = Vec::new();
    img.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png).map_err(|e| format!("can't re-encode image: {e}"))?;
    fit(png, max_side, max_bytes)
}

/// Width and height of a PNG/JPEG/GIF/WebP, from its header.
pub fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let format = match sniff_any(bytes)? {
        "image/png" => image::ImageFormat::Png,
        "image/jpeg" => image::ImageFormat::Jpeg,
        "image/gif" => image::ImageFormat::Gif,
        _ => image::ImageFormat::WebP,
    };
    let mut reader = image::ImageReader::new(Cursor::new(bytes));
    reader.set_format(format);
    reader.into_dimensions().ok()
}

/// A PNG/JPEG no larger than `max_side` px and `max_bytes`: passed through when it already
/// fits, else decoded (with limits), downscaled and re-encoded.
pub fn fit(bytes: Vec<u8>, max_side: u32, max_bytes: usize) -> Result<(&'static str, Vec<u8>), String> {
    let mime = sniff(&bytes).ok_or("not a PNG or JPEG image")?;
    let mut reader = image::ImageReader::new(Cursor::new(&bytes));
    reader.set_format(if mime == "image/png" { image::ImageFormat::Png } else { image::ImageFormat::Jpeg });
    let (w, h) = reader.into_dimensions().map_err(|e| format!("unreadable image: {e}"))?;
    if bytes.len() <= max_bytes && w <= max_side && h <= max_side {
        return Ok((mime, bytes));
    }
    let mut reader = image::ImageReader::new(Cursor::new(&bytes));
    reader.set_format(if mime == "image/png" { image::ImageFormat::Png } else { image::ImageFormat::Jpeg });
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DECODE_SIDE);
    limits.max_image_height = Some(MAX_DECODE_SIDE);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    let img = reader.decode().map_err(|e| format!("unreadable image: {e}"))?;
    let mut side = max_side;
    loop {
        let small = if img.width() > side || img.height() > side { img.thumbnail(side, side) } else { img.clone() };
        let mut out = Vec::new();
        let (fmt, mime) = if small.color().has_alpha() { (image::ImageFormat::Png, "image/png") } else { (image::ImageFormat::Jpeg, "image/jpeg") };
        let small = if fmt == image::ImageFormat::Jpeg { image::DynamicImage::ImageRgb8(small.to_rgb8()) } else { small };
        small.write_to(&mut Cursor::new(&mut out), fmt).map_err(|e| format!("can't re-encode image: {e}"))?;
        if out.len() <= max_bytes || side <= 256 {
            return Ok((mime, out));
        }
        side = side.saturating_mul(3) / 4;
    }
}
