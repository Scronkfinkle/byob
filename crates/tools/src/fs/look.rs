//! `look_at_image`: show the model a workspace image, or a PDF page rendered to a picture.
//!
//! The tool only loads and downscales; the turn decides who sees it (see
//! [`ToolContext::show_image`]): the chat's model if it can see, else the `vision` model
//! role, which answers `question` in words.

use std::io::Read as _;

use byob_protocol::media::{self, MAX_IMAGE_BYTES};
use serde_json::{Value, json};

use super::{arg_str, bytes, opt_u64, req_string, short_name};
use crate::{Access, Tool, ToolContext, ToolOutput, ToolSpec, images, sandbox};

/// Longest side of an image shown to the model (as uploads).
pub const IMAGE_SIDE: u32 = 1024;
/// Longest side of a PDF page: more than a photo, so small print stays readable.
pub const PAGE_SIDE: u32 = 1280;
/// Largest image file read.
const MAX_IMAGE_FILE: u64 = 64 * 1024 * 1024;

pub struct LookAtImage;

impl Tool for LookAtImage {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "look_at_image".into(),
            description: "See an image in the workspace (PNG, JPEG, GIF, WebP) or a page of a PDF: a chart, photo, screenshot or scanned page.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Image or PDF path, relative to the workspace"},
                    "page": {"type": "integer", "description": "For a PDF: the page to look at (default 1)"},
                    "question": {"type": "string", "description": "What you want to know about it (optional)"}
                },
                "required": ["path"]
            }),
        }
    }

    fn title(&self, args: &Value) -> String {
        format!("Looked at {}", target(args))
    }

    fn pending_title(&self, args: &Value) -> String {
        format!("Look at {}", target(args))
    }

    fn preview(&self, _args: &Value, _workspace: &std::path::Path) -> Option<crate::Preview> {
        None
    }

    fn access(&self, _args: &Value) -> Access {
        Access::Read
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        match look(args, ctx) {
            Ok((text, url)) => {
                if ctx.show_image(url) {
                    ToolOutput::ok(text)
                } else {
                    ToolOutput::err("images can't be shown here")
                }
            }
            Err(e) => ToolOutput::err(e),
        }
    }
}

/// "chart.png" or "page 2 of report.pdf", for titles.
fn target(args: &Value) -> String {
    let name = short_name(arg_str(args, "path").unwrap_or("an image"));
    match args.get("page").and_then(|p| p.as_u64().or_else(|| p.as_str().and_then(|s| s.trim().parse().ok()))) {
        Some(p) if name.to_ascii_lowercase().ends_with(".pdf") => format!("page {p} of {name}"),
        _ => name,
    }
}

fn look(args: &Value, ctx: &ToolContext<'_>) -> Result<(String, String), String> {
    let path = req_string(args, "path")?;
    let page = opt_u64(args, "page")?;
    let file = sandbox::resolve_follow(ctx.workspace, &path)?;
    let name = file.show();
    let meta = std::fs::metadata(&file.abs).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!("{name} doesn't exist; use list_files to see what's there"),
        _ => format!("can't open {name}: {e}"),
    })?;
    if !meta.is_file() {
        return Err(format!("{name} is not a file"));
    }
    if meta.len() > MAX_IMAGE_FILE {
        return Err(format!("{name} is too big to look at ({})", bytes(meta.len())));
    }
    let mut data = Vec::new();
    std::fs::File::open(&file.abs).and_then(|f| f.take(MAX_IMAGE_FILE).read_to_end(&mut data)).map_err(|e| format!("can't read {name}: {e}"))?;
    let head = data.get(..data.len().min(1024)).unwrap_or_default();
    if byob_docs::Kind::of(&name, head) == Some(byob_docs::Kind::Pdf) {
        let doc = byob_docs::pdf::Document::open(data).map_err(|e| format!("can't read {name}: {e}"))?;
        let total = doc.page_count();
        let n = usize::try_from(page.unwrap_or(1)).unwrap_or(usize::MAX).max(1);
        if n > total {
            return Err(format!("{name} has {total} page{}; there is no page {n}", if total == 1 { "" } else { "s" }));
        }
        let png = doc.render_png(n, PAGE_SIDE).map_err(|e| format!("can't draw page {n} of {name}: {e}"))?;
        let (mime, small) = images::fit(png, PAGE_SIDE, MAX_IMAGE_BYTES)?;
        return Ok((format!("Page {n} of {total} of {name}, as a picture."), data_url(mime, &small)));
    }
    if images::sniff_any(head).is_none() {
        return Err(format!("{name} isn't an image byob can show (PNG, JPEG, GIF, WebP) or a PDF"));
    }
    let size = images::dimensions(&data);
    let (mime, small) = images::fit_any(data, IMAGE_SIDE, MAX_IMAGE_BYTES).map_err(|e| format!("can't show {name}: {e}"))?;
    let shown = images::dimensions(&small);
    let dims = match (size, shown) {
        (Some((w, h)), Some((sw, sh))) if (w, h) != (sw, sh) => format!(" ({w}×{h} px, shown at {sw}×{sh})"),
        (Some((w, h)), _) => format!(" ({w}×{h} px)"),
        _ => String::new(),
    };
    Ok((format!("Image {name}{dims}."), data_url(mime, &small)))
}

fn data_url(mime: &str, bytes: &[u8]) -> String {
    format!("data:{mime};base64,{}", media::encode(bytes))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// Run the tool; also the images it showed.
    fn run(tool: &dyn Tool, ws: &std::path::Path, args: Value) -> (ToolOutput, Vec<String>) {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let images = std::sync::Mutex::new(Vec::new());
        let out = tool.run(&args, &ToolContext::new(ws, &cancel).with_images(&images));
        (out, images.into_inner().unwrap())
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(w, h, image::Rgb([200, 30, 30]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(img).write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
        out
    }

    #[test]
    fn shows_an_image_downscaled() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("big.png"), png(2048, 1024)).unwrap();
        fs::write(d.path().join("small.png"), png(40, 20)).unwrap();
        let (out, images) = run(&LookAtImage, d.path(), json!({"path": "big.png", "question": "what colour?"}));
        assert!(!out.failed, "{}", out.text);
        assert_eq!(out.text, "Image big.png (2048×1024 px, shown at 1024×512).");
        assert_eq!(images.len(), 1);
        assert!(images[0].starts_with("data:image/"));
        let (out, _) = run(&LookAtImage, d.path(), json!({"path": "small.png"}));
        assert_eq!(out.text, "Image small.png (40×20 px).");
        // Nobody to show it to.
        assert!(crate::fs::testutil::run(&LookAtImage, d.path(), json!({"path": "small.png"})).failed);
    }

    #[test]
    fn webp_and_gif_become_png() {
        let d = tempfile::tempdir().unwrap();
        let img = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(10, 10, image::Rgba([1, 2, 3, 255])));
        let mut gif = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut gif), image::ImageFormat::Gif).unwrap();
        fs::write(d.path().join("a.gif"), gif).unwrap();
        let (out, images) = run(&LookAtImage, d.path(), json!({"path": "a.gif"}));
        assert!(!out.failed, "{}", out.text);
        assert!(images[0].starts_with("data:image/png;base64,"));
    }

    #[test]
    fn renders_a_pdf_page() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("r.pdf"), byob_docs::fixtures::sample_pdf()).unwrap();
        let (out, images) = run(&LookAtImage, d.path(), json!({"path": "r.pdf", "page": "2"}));
        assert!(!out.failed, "{}", out.text);
        assert_eq!(out.text, "Page 2 of 2 of r.pdf, as a picture.");
        assert!(images[0].starts_with("data:image/"));
        let (out, _) = run(&LookAtImage, d.path(), json!({"path": "r.pdf", "page": 3}));
        assert!(out.failed && out.text.contains("has 2 pages"), "{}", out.text);
        assert_eq!(LookAtImage.title(&json!({"path": "docs/r.pdf", "page": 2})), "Looked at page 2 of r.pdf");
        assert_eq!(LookAtImage.pending_title(&json!({"path": "x/chart.png"})), "Look at chart.png");
    }

    #[test]
    fn refusals() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("notes.txt"), "hello").unwrap();
        fs::write(d.path().join("broken.png"), b"\x89PNG\r\n\x1a\nnope").unwrap();
        for args in [json!({}), json!({"path": "notes.txt"}), json!({"path": "../x.png"}), json!({"path": "nope.png"}), json!({"path": "broken.png"})] {
            let (out, images) = run(&LookAtImage, d.path(), args.clone());
            assert!(out.failed && images.is_empty(), "{args}: {}", out.text);
        }
    }
}
