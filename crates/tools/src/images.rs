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
    let img = decode(&bytes, mime)?;
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

/// Decode PNG/JPEG bytes of type `mime`, refusing decompression bombs.
fn decode(bytes: &[u8], mime: &str) -> Result<image::DynamicImage, String> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes));
    reader.set_format(if mime == "image/png" { image::ImageFormat::Png } else { image::ImageFormat::Jpeg });
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DECODE_SIDE);
    limits.max_image_height = Some(MAX_DECODE_SIDE);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    reader.decode().map_err(|e| format!("unreadable image: {e}"))
}

/// A PNG/JPEG `data:` URL re-encoded as JPEG at `quality` (1..=100) if that is smaller;
/// transparency becomes white. Anything else (quality 0, other URLs, undecodable or
/// already smaller images) comes back unchanged.
pub fn compress_data_url(url: &str, quality: u8) -> String {
    let Some(data) = url.strip_prefix("data:image/png;base64,").or_else(|| url.strip_prefix("data:image/jpeg;base64,")) else {
        return url.to_owned();
    };
    if quality == 0 {
        return url.to_owned();
    }
    let jpeg = (|| {
        let bytes = byob_protocol::media::decode(data).ok()?;
        let img = decode(&bytes, sniff(&bytes)?).ok()?;
        let rgb = if img.color().has_alpha() {
            let mut flat = image::RgbImage::from_pixel(img.width(), img.height(), image::Rgb([255, 255, 255]));
            for (x, y, p) in img.to_rgba8().enumerate_pixels() {
                let a = u16::from(p[3]);
                let blend = |c: u8| ((u16::from(c) * a + 255 * (255 - a)) / 255) as u8;
                flat.put_pixel(x, y, image::Rgb([blend(p[0]), blend(p[1]), blend(p[2])]));
            }
            flat
        } else {
            img.to_rgb8()
        };
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality.min(100)).encode_image(&rgb).ok()?;
        Some(out)
    })();
    match jpeg {
        Some(out) if out.len() < bytes_len(data) => format!("data:image/jpeg;base64,{}", byob_protocol::media::encode(&out)),
        _ => url.to_owned(),
    }
}

/// Decoded size of base64 `data`.
fn bytes_len(data: &str) -> usize {
    data.len() / 4 * 3
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data_url(img: &image::DynamicImage, fmt: image::ImageFormat, mime: &str) -> String {
        let mut b = Vec::new();
        img.write_to(&mut Cursor::new(&mut b), fmt).unwrap();
        format!("data:{mime};base64,{}", byob_protocol::media::encode(&b))
    }

    /// A photo-like picture (gradients with a little grain): PNG compresses it poorly.
    fn noisy(alpha: bool) -> image::DynamicImage {
        let img = image::RgbaImage::from_fn(400, 300, |x, y| {
            let grain = ((x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503)) >> 13) % 9;
            let v = |base: u32| u8::try_from((base + grain).min(255)).unwrap_or(255);
            image::Rgba([v(x * 200 / 400), v(y * 200 / 300), v((x + y) * 100 / 700), if alpha { 128 } else { 255 }])
        });
        if alpha { image::DynamicImage::ImageRgba8(img) } else { image::DynamicImage::ImageRgb8(image::DynamicImage::ImageRgba8(img).to_rgb8()) }
    }

    #[test]
    fn a_light_jpeg_pass_shrinks_images_for_the_model() {
        let png = data_url(&noisy(false), image::ImageFormat::Png, "image/png");
        let jpeg = compress_data_url(&png, 85);
        assert!(jpeg.starts_with("data:image/jpeg;base64,"), "{}", &jpeg[..40]);
        assert!(jpeg.len() * 2 < png.len(), "{} vs {}", jpeg.len(), png.len());
        // Transparency is flattened; the result still decodes at the same size.
        let clear = compress_data_url(&data_url(&noisy(true), image::ImageFormat::Png, "image/png"), 85);
        let bytes = byob_protocol::media::decode(clear.split_once(',').unwrap().1).unwrap();
        let img = image::load_from_memory(&bytes).unwrap();
        assert_eq!((img.width(), img.height(), img.color().has_alpha()), (400, 300, false));
        // Off, other URLs and garbage: unchanged.
        assert_eq!(compress_data_url(&png, 0), png);
        assert_eq!(compress_data_url("https://x/y.png", 85), "https://x/y.png");
        assert_eq!(compress_data_url("data:image/png;base64,AAAA", 85), "data:image/png;base64,AAAA");
        // A JPEG already smaller than the re-encode stays as it is.
        let small = compress_data_url(&png, 30);
        assert_eq!(compress_data_url(&small, 95), small);
    }
}
