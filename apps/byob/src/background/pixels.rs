//! Drawing on the CPU for the X11 tray fallback ([`super::xembed`]): the icon at the size
//! the tray gives it, and the right-click menu (its layout, what's under the pointer, its
//! pixels and text). Nothing here talks to X.

use ab_glyph_rasterizer::{Point, Rasterizer, point};
use skrifa::instance::{LocationRef, Size};
use skrifa::outline::{DrawSettings, OutlinePen};
use skrifa::{FontRef, MetadataProvider};

pub use super::tray::Look;

pub use super::tray::Rgb;

fn byte(v: u32) -> u8 {
    u8::try_from(v).unwrap_or(u8::MAX)
}

/// Channel `c` at `alpha` (0..=255) over `under`.
fn mix(c: u8, under: u8, alpha: u32) -> u8 {
    byte((u32::from(c) * alpha + u32::from(under) * (255 - alpha.min(255)) + 127) / 255)
}

pub fn mix_rgb(c: Rgb, under: Rgb, alpha: u32) -> Rgb {
    [mix(c[0], under[0], alpha), mix(c[1], under[1], alpha), mix(c[2], under[2], alpha)]
}

/// The source pixels destination pixel `i` of `d` covers, out of `s`.
fn span(i: usize, s: usize, d: usize) -> (usize, usize) {
    let lo = i * s / d.max(1);
    let hi = ((i + 1) * s).div_ceil(d.max(1)).max(lo + 1).min(s);
    (lo, hi)
}

/// `src` (RGBA, straight alpha, `sw`×`sh`) scaled to `d`×`d`, premultiplied: each pixel
/// averages the source pixels under it.
pub fn scale_premul(src: &[u8], sw: usize, sh: usize, d: usize) -> Vec<[u8; 4]> {
    let mut out = Vec::with_capacity(d * d);
    for y in 0..d {
        let (y0, y1) = span(y, sh, d);
        for x in 0..d {
            let (x0, x1) = span(x, sw, d);
            let (mut acc, mut n) = ([0u32; 4], 0u32);
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let i = (sy * sw + sx) * 4;
                    if let Some(&[r, g, b, a]) = src.get(i..i + 4) {
                        let a32 = u32::from(a);
                        for (k, c) in [r, g, b].into_iter().enumerate() {
                            acc[k] += u32::from(c) * a32 / 255;
                        }
                        acc[3] += a32;
                        n += 1;
                    }
                }
            }
            let n = n.max(1);
            out.push([byte(acc[0] / n), byte(acc[1] / n), byte(acc[2] / n), byte(acc[3] / n)]);
        }
    }
    out
}

/// A dot in the icon's top right corner (something is waiting for you).
pub fn add_dot(img: &mut [[u8; 4]], d: usize, color: Rgb) {
    let r = (d as f32 * 0.22).max(2.0);
    let (cx, cy) = (d as f32 - r, r);
    for y in 0..d {
        for x in 0..d {
            // 4×4 samples per pixel for a smooth edge.
            let mut hits = 0u32;
            for sy in 0..4 {
                for sx in 0..4 {
                    let (px, py) = (x as f32 + (sx as f32 + 0.5) / 4.0, y as f32 + (sy as f32 + 0.5) / 4.0);
                    if (px - cx).powi(2) + (py - cy).powi(2) <= r * r {
                        hits += 1;
                    }
                }
            }
            if hits > 0
                && let Some(p) = img.get_mut(y * d + x)
            {
                let cov = hits * 255 / 16;
                for k in 0..3 {
                    p[k] = byte((u32::from(color[k]) * cov + u32::from(p[k]) * (255 - cov)) / 255);
                }
                p[3] = byte(cov + u32::from(p[3]) * (255 - cov) / 255);
            }
        }
    }
}

/// A premultiplied pixel over an opaque one.
pub fn over(p: [u8; 4], under: Rgb) -> Rgb {
    let inv = 255 - u32::from(p[3]);
    [0, 1, 2].map(|k| byte(u32::from(p[k]) + (u32::from(under[k]) * inv + 127) / 255))
}

/// Pixels as X wants them for a 32-bit-per-pixel image: `lsb` is the server's byte order
/// (least significant first: B, G, R, A in memory).
pub fn pack(px: impl Iterator<Item = [u8; 4]>, lsb: bool) -> Vec<u8> {
    px.flat_map(|[r, g, b, a]| if lsb { [b, g, r, a] } else { [a, r, g, b] }).collect()
}

/// The pixels of a 32-bit-per-pixel X image (the reverse of [`pack`]; alpha dropped).
pub fn unpack(data: &[u8], lsb: bool) -> Vec<Rgb> {
    data.as_chunks::<4>().0.iter().map(|&[a, b, c, d]| if lsb { [c, b, a] } else { [b, c, d] }).collect()
}

/// An opaque image to draw on.
pub struct Canvas {
    pub w: usize,
    pub h: usize,
    pub px: Vec<Rgb>,
}

impl Canvas {
    pub fn new(w: usize, h: usize, fill: Rgb) -> Self {
        Self { w, h, px: vec![fill; w * h] }
    }

    pub fn rect(&mut self, x: usize, y: usize, w: usize, h: usize, c: Rgb) {
        for yy in y..(y + h).min(self.h) {
            for xx in x..(x + w).min(self.w) {
                self.px[yy * self.w + xx] = c;
            }
        }
    }

    /// `c` at `alpha` (0.0..=1.0) over the pixel at `x`, `y`.
    fn blend(&mut self, x: usize, y: usize, c: Rgb, alpha: f32) {
        if x < self.w
            && y < self.h
            && let Some(p) = self.px.get_mut(y * self.w + x)
        {
            *p = mix_rgb(c, *p, (alpha.clamp(0.0, 1.0) * 255.0).round() as u32);
        }
    }

    /// A one-pixel frame around the edge.
    pub fn frame(&mut self, c: Rgb) {
        let (w, h) = (self.w, self.h);
        self.rect(0, 0, w, 1, c);
        self.rect(0, h.saturating_sub(1), w, 1, c);
        self.rect(0, 0, 1, h, c);
        self.rect(w.saturating_sub(1), 0, 1, h, c);
    }

    /// Paths filled with `c`, in a box at `x`, `y`.
    fn fill(&mut self, x: f32, y: f32, w: usize, h: usize, c: Rgb, draw: impl FnOnce(&mut Rasterizer)) {
        let mut r = Rasterizer::new(w, h);
        draw(&mut r);
        let (ox, oy) = (x.floor().max(0.0) as usize, y.floor().max(0.0) as usize);
        r.for_each_pixel_2d(|px, py, a| {
            if a > 0.0 {
                self.blend(ox + px as usize, oy + py as usize, c, a);
            }
        });
    }

    /// A check mark in a `size` box at `x`, `y`.
    pub fn check(&mut self, x: f32, y: f32, size: f32, c: Rgb) {
        let n = size.ceil() as usize + 2;
        let t = (size * 0.13).max(1.2);
        // Two strokes, as thin quads: down to the bottom, up to the top right.
        let pts = [(0.12, 0.55), (0.4, 0.82), (0.9, 0.2)].map(|(a, b)| (a * size + 1.0, b * size + 1.0));
        self.fill(x, y, n, n, c, |r| {
            for seg in pts.windows(2) {
                let [(x0, y0), (x1, y1)] = [seg[0], seg[1]];
                let (dx, dy) = (x1 - x0, y1 - y0);
                let len = (dx * dx + dy * dy).sqrt().max(0.001);
                let (nx, ny) = (-dy / len * t / 2.0, dx / len * t / 2.0);
                let q = [point(x0 + nx, y0 + ny), point(x1 + nx, y1 + ny), point(x1 - nx, y1 - ny), point(x0 - nx, y0 - ny)];
                for i in 0..4 {
                    r.draw_line(q[i], q[(i + 1) % 4]);
                }
            }
        });
    }
}

/// Text in one font, laid out on a line (no kerning or shaping: menu labels).
pub struct Font {
    font: FontRef<'static>,
}

/// Glyph outlines into the rasterizer, offset by the pen position (y down).
struct Pen<'r> {
    r: &'r mut Rasterizer,
    at: (f32, f32),
    start: Point,
    last: Point,
}

impl Pen<'_> {
    fn p(&self, x: f32, y: f32) -> Point {
        point(self.at.0 + x, self.at.1 - y)
    }
}

impl OutlinePen for Pen<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        self.start = self.p(x, y);
        self.last = self.start;
    }
    fn line_to(&mut self, x: f32, y: f32) {
        let p = self.p(x, y);
        self.r.draw_line(self.last, p);
        self.last = p;
    }
    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        let (c, p) = (self.p(cx0, cy0), self.p(x, y));
        self.r.draw_quad(self.last, c, p);
        self.last = p;
    }
    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        let (c0, c1, p) = (self.p(cx0, cy0), self.p(cx1, cy1), self.p(x, y));
        self.r.draw_cubic(self.last, c0, c1, p);
        self.last = p;
    }
    fn close(&mut self) {
        if self.last != self.start {
            self.r.draw_line(self.last, self.start);
        }
        self.last = self.start;
    }
}

impl Font {
    pub fn new(bytes: &'static [u8]) -> Option<Self> {
        FontRef::new(bytes).ok().map(|font| Self { font })
    }

    /// Distance from the top of a line to its baseline, and the line's height, at `px`.
    pub fn line(&self, px: f32) -> (f32, f32) {
        let m = self.font.metrics(Size::new(px), LocationRef::default());
        (m.ascent, m.ascent - m.descent)
    }

    pub fn width(&self, text: &str, px: f32) -> f32 {
        let (map, metrics) = (self.font.charmap(), self.font.glyph_metrics(Size::new(px), LocationRef::default()));
        text.chars().filter_map(|c| map.map(c)).filter_map(|g| metrics.advance_width(g)).sum()
    }

    /// `text` with its baseline at `y`, starting at `x`.
    pub fn draw(&self, canvas: &mut Canvas, text: &str, x: f32, y: f32, px: f32, c: Rgb) {
        let (ascent, height) = self.line(px);
        let top = y - ascent;
        let (w, h) = (self.width(text, px).ceil() as usize + 2, height.ceil() as usize + 2);
        let (map, metrics, outlines) = (self.font.charmap(), self.font.glyph_metrics(Size::new(px), LocationRef::default()), self.font.outline_glyphs());
        // Within the box: the pen starts one pixel in, the baseline at the ascent.
        let fx = x - x.floor();
        canvas.fill(x, top, w, h, c, |r| {
            let mut pen_x = 1.0 + fx;
            for g in text.chars().filter_map(|ch| map.map(ch)) {
                if let Some(glyph) = outlines.get(g) {
                    let mut pen = Pen { r: &mut *r, at: (pen_x, ascent + (top - top.floor())), start: point(0.0, 0.0), last: point(0.0, 0.0) };
                    let _ = glyph.draw(DrawSettings::unhinted(Size::new(px), LocationRef::default()), &mut pen);
                }
                pen_x += metrics.advance_width(g).unwrap_or(0.0);
            }
        });
    }
}

/// A line of the menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// Something to click; `checked`: it's an on/off item.
    Item {
        label: String,
        checked: Option<bool>,
    },
    /// A label over the items that follow.
    Heading(String),
    Separator,
}

/// The menu laid out at a scale (1.0: 96 dpi).
#[derive(Debug, Clone, PartialEq)]
pub struct MenuLayout {
    pub w: usize,
    pub h: usize,
    scale: f32,
    /// Each row, its top and height.
    rows: Vec<(Row, usize, usize)>,
}

const TEXT_PX: f32 = 13.5;

impl MenuLayout {
    pub fn new(rows: Vec<Row>, font: &Font, scale: f32) -> Self {
        let s = scale.clamp(0.5, 4.0);
        let px = |v: f32| (v * s).round() as usize;
        let (pad, item, heading, sep) = (px(4.0), px(26.0), px(22.0), px(9.0));
        let text_w = rows
            .iter()
            .map(|r| match r {
                Row::Item { label, .. } | Row::Heading(label) => font.width(label, TEXT_PX * s),
                Row::Separator => 0.0,
            })
            .fold(0.0, f32::max);
        let w = (text_w.ceil() as usize + px(28.0) + px(20.0)).max(px(170.0));
        let mut y = pad;
        let rows = rows
            .into_iter()
            .map(|r| {
                let h = match r {
                    Row::Item { .. } => item,
                    Row::Heading(_) => heading,
                    Row::Separator => sep,
                };
                let at = y;
                y += h;
                (r, at, h)
            })
            .collect();
        Self { w, h: y + pad, scale: s, rows }
    }

    /// The clickable row at `x`, `y` (in the menu), by index.
    pub fn hit(&self, x: i32, y: i32) -> Option<usize> {
        let (x, y) = (usize::try_from(x).ok()?, usize::try_from(y).ok()?);
        if x >= self.w {
            return None;
        }
        self.rows.iter().position(|(r, top, h)| matches!(r, Row::Item { .. }) && (*top..top + h).contains(&y))
    }

    pub fn paint(&self, font: &Font, look: &Look, hover: Option<usize>) -> Canvas {
        let s = self.scale;
        let mut c = Canvas::new(self.w, self.h, look.bg);
        let inset = (4.0 * s).round() as usize;
        let text_px = TEXT_PX * s;
        let (ascent, line_h) = font.line(text_px);
        let dim = mix_rgb(look.fg, look.bg, 150);
        for (i, (row, top, h)) in self.rows.iter().enumerate() {
            let baseline = |px_h: f32| *top as f32 + (*h as f32 - px_h) / 2.0 + ascent;
            match row {
                Row::Item { label, checked } => {
                    let fg = if hover == Some(i) {
                        c.rect(inset, *top, self.w.saturating_sub(2 * inset), *h, look.hover_bg);
                        look.hover_fg
                    } else {
                        look.fg
                    };
                    if *checked == Some(true) {
                        let size = 12.0 * s;
                        c.check(10.0 * s, *top as f32 + (*h as f32 - size) / 2.0 - 1.0, size, fg);
                    }
                    font.draw(&mut c, label, 28.0 * s, baseline(line_h).round(), text_px, fg);
                }
                Row::Heading(label) => font.draw(&mut c, label, 28.0 * s, baseline(line_h).round(), text_px * 0.92, dim),
                Row::Separator => {
                    let line = mix_rgb(look.fg, look.bg, 40);
                    c.rect(inset * 2, top + h / 2, self.w.saturating_sub(4 * inset), s.round().max(1.0) as usize, line);
                }
            }
        }
        c.frame(look.border);
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn font() -> Font {
        Font::new(byob_ui_egui::theme::INTER).unwrap()
    }

    #[test]
    fn the_icon_scales_down_by_averaging() {
        // 2×2: opaque red, transparent, opaque blue, transparent → one pixel, half covered.
        let src = [255, 0, 0, 255, 0, 0, 0, 0, 0, 0, 255, 255, 0, 0, 0, 0];
        assert_eq!(scale_premul(&src, 2, 2, 1), vec![[63, 0, 63, 127]]);
        // Up: each source pixel repeated.
        assert_eq!(scale_premul(&[10, 20, 30, 255], 1, 1, 2), vec![[10, 20, 30, 255]; 4]);
        // The real icon at a tray's size.
        let (w, h, px) = super::super::tray::icon_rgba().unwrap();
        let small = scale_premul(&px, w as usize, h as usize, 16);
        assert_eq!(small.len(), 256);
        assert!(small.iter().any(|p| p[3] == 255) && small.iter().any(|p| p[3] == 0), "solid middle, clear corners");
    }

    #[test]
    fn premultiplied_pixels_go_over_the_tray() {
        assert_eq!(over([0, 0, 0, 0], [9, 8, 7]), [9, 8, 7]);
        assert_eq!(over([200, 100, 0, 255], [9, 8, 7]), [200, 100, 0]);
        assert_eq!(over([100, 0, 0, 128], [0, 0, 200]), [100, 0, 100]);
        assert_eq!(pack([[1, 2, 3, 4]].into_iter(), true), [3, 2, 1, 4]);
        assert_eq!(pack([[1, 2, 3, 4]].into_iter(), false), [4, 1, 2, 3]);
        assert_eq!(unpack(&[3, 2, 1, 0], true), [[1, 2, 3]]);
        assert_eq!(unpack(&[0, 1, 2, 3], false), [[1, 2, 3]]);
    }

    #[test]
    fn the_dot_goes_top_right() {
        let mut img = vec![[0, 0, 0, 0]; 16 * 16];
        add_dot(&mut img, 16, [255, 0, 0]);
        assert_eq!(img[3 * 16 + 12], [255, 0, 0, 255], "top right");
        assert_eq!(img[15], [0, 0, 0, 0], "a round dot: the corner itself is clear");
        assert_eq!(img[16 * 15], [0, 0, 0, 0], "bottom left untouched");
    }

    fn rows() -> Vec<Row> {
        vec![
            Row::Item { label: "Open byob".into(), checked: None },
            Row::Heading("Run now".into()),
            Row::Item { label: "Loom deals".into(), checked: None },
            Row::Item { label: "Pause routines".into(), checked: Some(true) },
            Row::Separator,
            Row::Item { label: "Quit byob".into(), checked: None },
        ]
    }

    #[test]
    fn the_menu_knows_whats_under_the_pointer() {
        let m = MenuLayout::new(rows(), &font(), 1.0);
        assert_eq!((m.w, m.h), (170, 4 + 26 + 22 + 26 + 26 + 9 + 26 + 4));
        assert_eq!(m.hit(20, 10), Some(0));
        assert_eq!(m.hit(20, 40), None, "a heading");
        assert_eq!(m.hit(20, 60), Some(2));
        assert_eq!(m.hit(20, 108), None, "the separator");
        assert_eq!(m.hit(20, 120), Some(5));
        assert_eq!(m.hit(20, 140), None, "the bottom padding");
        assert_eq!(m.hit(20, 2), None, "the padding");
        assert_eq!(m.hit(-1, 10), None);
        assert_eq!(m.hit(170, 10), None);
        // Twice the size on a 192 dpi screen; long labels widen it.
        let big = MenuLayout::new(rows(), &font(), 2.0);
        assert_eq!((big.w, big.h), (340, 2 * m.h));
        let long = MenuLayout::new(vec![Row::Item { label: "A routine with a long name for the menu".into(), checked: None }], &font(), 1.0);
        assert!(long.w > 250, "{}", long.w);
    }

    #[test]
    fn the_menu_draws_its_text_and_the_hovered_item() {
        let look = Look::default();
        let m = MenuLayout::new(rows(), &font(), 1.0);
        let c = m.paint(&font(), &look, Some(0));
        assert_eq!(c.px.len(), m.w * m.h);
        assert_eq!(c.px[0], look.border);
        let row = |y: usize| &c.px[y * m.w..(y + 1) * m.w];
        assert!(row(6).contains(&look.hover_bg), "the hovered item");
        assert!(row(17).iter().any(|p| *p != look.hover_bg && *p != look.bg && *p != look.border), "its text");
        assert!(!row(60).contains(&look.hover_bg), "the others aren't");
        assert!(row(60).iter().any(|p| *p != look.bg && *p != look.border), "Loom deals is written");
        // Pause routines (on) has its check mark left of the text.
        let check = (80..104).any(|y| row(y)[10..22].iter().any(|p| *p != look.bg));
        assert!(check, "a check mark");
        let unchecked = MenuLayout::new(vec![Row::Item { label: "Pause routines".into(), checked: Some(false) }], &font(), 1.0).paint(&font(), &look, None);
        assert!((4..30).all(|y| unchecked.px[y * unchecked.w + 10..y * unchecked.w + 22].iter().all(|p| *p == look.bg)), "no check mark when off");
    }
}
