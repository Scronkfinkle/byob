//! PDF: text per page and pages as PNG, on hayro.
//!
//! Text comes from a [`Device`] that collects every glyph the page draws (invisible text
//! too, which is how scanned PDFs carry their OCR layer) with its position, then groups
//! glyphs into lines by baseline and puts spaces where the gap is wider than a glyph would
//! leave. That is rough on multi-column layouts but needs no layout analysis.

use std::sync::Arc;

use hayro::hayro_interpret::font::GlyphRun;
use hayro::hayro_interpret::hayro_syntax::Pdf;
use hayro::hayro_interpret::hayro_syntax::page::Page;
use hayro::hayro_interpret::kurbo::{Affine, BezPath, Point, Rect};
use hayro::hayro_interpret::util::TransformExt;
use hayro::hayro_interpret::{
    BlendMode, ClipPath, Context, Device, DrawMode, DrawProps, Image, ImageDrawProps, InterpreterCache, InterpreterSettings, SoftMask, interpret_page,
};
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{PixmapSettings, RenderCache, RenderSettings};

use crate::{MAX_FILE_BYTES, guarded};

/// Most characters kept from one page (a page of dense text is ~5k).
pub const MAX_PAGE_CHARS: usize = 40_000;
/// Most glyphs looked at on one page (a hostile page could draw millions).
const MAX_GLYPHS: usize = 200_000;
/// Rendered pages are at most this many pixels on a side before downscaling.
const MAX_RENDER_SIDE: f32 = 4096.0;

/// An opened PDF.
pub struct Document {
    pdf: Pdf,
}

/// What one page holds.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PageText {
    pub text: String,
    /// Pictures drawn on the page (scans, photos, embedded charts).
    pub images: usize,
    /// Vector shapes drawn (lines, bars of a chart, table rules).
    pub shapes: usize,
    /// The text was cut at [`MAX_PAGE_CHARS`].
    pub cut: bool,
}

impl Document {
    /// Open PDF bytes. Encrypted PDFs that need a password are refused.
    pub fn open(bytes: Vec<u8>) -> Result<Self, String> {
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(format!("the PDF is too big (over {} MB)", MAX_FILE_BYTES / (1024 * 1024)));
        }
        guarded("the PDF", || match Pdf::new(Arc::new(bytes)) {
            Ok(pdf) => Ok(Self { pdf }),
            Err(hayro::hayro_interpret::hayro_syntax::LoadPdfError::Decryption(_)) => Err("the PDF is password-protected".into()),
            Err(_) => Err("the PDF is damaged or not really a PDF".into()),
        })
    }

    pub fn page_count(&self) -> usize {
        self.pdf.pages().len()
    }

    /// The text of page `n` (from 1).
    pub fn page_text(&self, n: usize) -> Result<PageText, String> {
        let page = self.page(n)?;
        guarded(&format!("page {n}"), || {
            let cache = InterpreterCache::new();
            let settings = InterpreterSettings::default();
            let (w, h) = page.render_dimensions();
            let bbox = Rect::new(0.0, 0.0, f64::from(w), f64::from(h));
            let mut ctx = Context::new(page.initial_transform(true).to_kurbo(), bbox, &cache, page.xref(), settings);
            let mut dev = TextDevice::default();
            interpret_page(page, &mut ctx, &mut dev);
            Ok(dev.finish())
        })
    }

    /// Page `n` (from 1) as a PNG whose longer side is about `max_side` pixels, on white.
    pub fn render_png(&self, n: usize, max_side: u32) -> Result<Vec<u8>, String> {
        let page = self.page(n)?;
        guarded(&format!("page {n}"), || {
            let (w, h) = page.render_dimensions();
            let long = w.max(h);
            if !(long.is_finite() && long > 0.0) {
                return Err(format!("page {n} has no size"));
            }
            #[allow(clippy::cast_precision_loss)]
            let scale = ((max_side as f32).min(MAX_RENDER_SIDE) / long).clamp(0.01, 10.0);
            let cache = RenderCache::new();
            let pixmap = hayro::render(
                page,
                &cache,
                &InterpreterSettings::default(),
                &RenderSettings::default(),
                &PixmapSettings { x_scale: scale, y_scale: scale, bg_color: WHITE },
            );
            if pixmap.width() == 0 || pixmap.height() == 0 {
                return Err(format!("page {n} rendered empty"));
            }
            pixmap.into_png().map_err(|e| format!("can't encode page {n}: {e}"))
        })
    }

    fn page(&self, n: usize) -> Result<&Page<'_>, String> {
        let total = self.page_count();
        n.checked_sub(1).and_then(|i| self.pdf.pages().get(i)).ok_or_else(|| format!("there is no page {n}: the PDF has {total}"))
    }
}

/// One glyph as placed on the page (y grows downwards).
#[derive(Debug, Clone)]
struct Placed {
    x: f64,
    /// Baseline.
    y: f64,
    /// Where the glyph ends (estimated when the font doesn't say).
    end: f64,
    /// Font size in page units.
    size: f64,
    text: String,
}

#[derive(Default)]
struct TextDevice {
    glyphs: Vec<Placed>,
    images: usize,
    shapes: usize,
    seen: usize,
}

impl TextDevice {
    fn glyph_run(&mut self, run: &GlyphRun<'_, '_>, props: &DrawProps<'_>) {
        for g in run.glyphs() {
            self.seen = self.seen.saturating_add(1);
            if self.seen > MAX_GLYPHS {
                return;
            }
            let Some(text) = g.as_unicode().map(|u| match u {
                hayro::hayro_interpret::hayro_cmap::BfString::Char(c) => c.to_string(),
                hayro::hayro_interpret::hayro_cmap::BfString::String(s) => s,
            }) else {
                continue;
            };
            // Glyph space is 1000 units per em.
            let t: Affine = props.transform * g.transform();
            let origin = t * Point::ZERO;
            let up = t * Point::new(0.0, 1000.0);
            let size = (up - origin).hypot();
            if !(origin.x.is_finite() && origin.y.is_finite() && size.is_finite()) || size <= 0.0 {
                continue;
            }
            let advance = match &**g {
                hayro::hayro_interpret::font::Glyph::Outline(o) => o.advance_width(),
                hayro::hayro_interpret::font::Glyph::Type3(t3) => t3.advance_width(),
            };
            let end = match advance.filter(|a| a.is_finite() && *a > 0.0) {
                Some(a) => (t * Point::new(f64::from(a), 0.0)).x,
                None => origin.x + size * 0.5,
            };
            self.glyphs.push(Placed { x: origin.x, y: origin.y, end, size, text });
        }
    }

    /// Group glyphs into lines, top to bottom, and lines into text.
    fn finish(mut self) -> PageText {
        // Content order is reading order in most PDFs; sorting by line keeps that inside a line
        // and puts lines in page order. Glyphs within a line are ordered by x.
        let mut lines: Vec<(f64, f64, Vec<Placed>)> = Vec::new(); // (baseline, size, glyphs)
        for g in self.glyphs.drain(..) {
            let tolerance = g.size * 0.4;
            match lines.iter_mut().rev().take(8).find(|(y, _, _)| (y - g.y).abs() <= tolerance) {
                Some((_, size, glyphs)) => {
                    *size = size.max(g.size);
                    glyphs.push(g);
                }
                None => lines.push((g.y, g.size, vec![g])),
            }
        }
        lines.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut text = String::new();
        let mut cut = false;
        let mut prev: Option<(f64, f64)> = None;
        for (y, size, mut glyphs) in lines {
            glyphs.sort_by(|a, b| a.x.total_cmp(&b.x));
            let line = join_line(&glyphs);
            if line.trim().is_empty() {
                continue;
            }
            if let Some((py, psize)) = prev {
                text.push('\n');
                // A gap of more than about a line and a half: a new paragraph.
                if y - py > psize.max(size) * 1.9 {
                    text.push('\n');
                }
            }
            text.push_str(line.trim_end());
            prev = Some((y, size));
            if text.chars().count() > MAX_PAGE_CHARS {
                text = text.chars().take(MAX_PAGE_CHARS).collect();
                cut = true;
                break;
            }
        }
        PageText { text, images: self.images, shapes: self.shapes, cut }
    }
}

/// The glyphs of one line (sorted by x) as text, with spaces at wide gaps.
fn join_line(glyphs: &[Placed]) -> String {
    let mut out = String::new();
    let mut last_end: Option<f64> = None;
    for g in glyphs {
        if let Some(end) = last_end {
            let gap = g.x - end;
            // Overprinted glyphs (fake bold, shadows) repeat at the same place.
            if gap < -g.size * 0.3 && out.ends_with(&g.text) {
                continue;
            }
            if gap > g.size * 0.18 && !out.ends_with(' ') && g.text != " " {
                // Very wide gaps are columns of a table or form.
                out.push_str(if gap > g.size * 2.5 { "   " } else { " " });
            }
        }
        if !(g.text == " " && out.ends_with(' ')) {
            out.extend(g.text.chars().filter(|c| !c.is_control()));
        }
        last_end = Some(g.end.max(g.x));
    }
    out
}

impl<'a> Device<'a> for TextDevice {
    fn draw_path(&mut self, _: &BezPath, _: DrawProps<'a>, _: &DrawMode) {
        self.shapes = self.shapes.saturating_add(1);
    }
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_glyph_run(&mut self, run: &GlyphRun<'_, 'a>, props: DrawProps<'a>, _: &DrawMode) {
        self.glyph_run(run, &props);
    }
    fn draw_image(&mut self, _: Image<'a, '_>, _: ImageDrawProps<'a>) {
        self.images = self.images.saturating_add(1);
    }
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::make_pdf;

    #[test]
    fn text_per_page_in_reading_order() {
        let pdf = make_pdf(
            &[
                &[(72, 720, 20, "Quarterly report"), (72, 690, 11, "Sales grew by 12 percent."), (300, 690, 11, "See page 2.")],
                &[(72, 720, 14, "Chart: revenue by region")],
            ],
            3,
        );
        let doc = Document::open(pdf).unwrap();
        assert_eq!(doc.page_count(), 2);
        let p1 = doc.page_text(1).unwrap();
        assert_eq!(p1.text, "Quarterly report\nSales grew by 12 percent.   See page 2.");
        assert_eq!((p1.images, p1.shapes, p1.cut), (0, 0, false));
        let p2 = doc.page_text(2).unwrap();
        assert_eq!(p2.text, "Chart: revenue by region");
        assert_eq!(p2.shapes, 3);
        assert!(doc.page_text(3).unwrap_err().contains("has 2"));
        assert!(doc.page_text(0).is_err());
    }

    #[test]
    fn renders_a_page_to_png() {
        let doc = Document::open(make_pdf(&[&[(72, 720, 20, "Hello")]], 2)).unwrap();
        let png = doc.render_png(1, 600).unwrap();
        assert!(png.starts_with(b"\x89PNG"));
        // 612x792 points scaled to a 600 px long side.
        let h = u32::from_be_bytes(png[20..24].try_into().unwrap());
        assert!((598..=601).contains(&h), "{h}");
    }

    #[test]
    fn hostile_input_is_an_error_not_a_panic() {
        assert!(Document::open(b"%PDF-1.4\ngarbage".to_vec()).is_err());
        assert!(Document::open(Vec::new()).is_err());
        let mut pdf = make_pdf(&[&[(72, 720, 12, "x")]], 0);
        // Truncate in the middle: the xref is gone, hayro falls back to scanning.
        pdf.truncate(pdf.len() / 2);
        if let Ok(doc) = Document::open(pdf) {
            for n in 1..=doc.page_count() {
                let _ = doc.page_text(n);
                let _ = doc.render_png(n, 100);
            }
        }
    }
}
