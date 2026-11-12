//! Fitting PNG/JPEG images into a size: for clients (artifacts, media) and the model.

use std::io::Cursor;

/// Decoder limits: refuse decompression bombs.
const MAX_DECODE_SIDE: u32 = 16_384;
const MAX_DECODE_ALLOC: u64 = 512 * 1024 * 1024;
/// Longest side of an image shown to the model.
pub const MODEL_IMAGE_SIDE: u32 = 1024;
/// Image files larger than this on disk are not decoded.
pub const MAX_IMAGE_FILE: u64 = 64 * 1024 * 1024;

/// A PNG/JPEG file as a `data:` URL for the model: downscaled to [`MODEL_IMAGE_SIDE`] and
/// [`byob_protocol::media::MAX_IMAGE_BYTES`].
pub fn for_model(path: &std::path::Path) -> Result<String, String> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    std::fs::File::open(path).and_then(|f| f.take(MAX_IMAGE_FILE.saturating_add(1)).read_to_end(&mut bytes)).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_IMAGE_FILE {
        return Err("the image is too big to show".into());
    }
    let (mime, data) = fit(bytes, MODEL_IMAGE_SIDE, byob_protocol::media::MAX_IMAGE_BYTES)?;
    Ok(format!("data:{mime};base64,{}", byob_protocol::media::encode(&data)))
}

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
