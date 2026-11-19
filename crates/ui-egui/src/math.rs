//! LaTeX math in messages: `$…$` inline and `$$…$$` on its own, typeset by RaTeX (KaTeX's
//! layout rules, in Rust) and drawn here with egui: glyphs in the KaTeX fonts (OFL,
//! [`install_fonts`]), rules and boxes as rectangles, and the outlines egui can't fill
//! itself (radicals and big delimiters are concave) triangulated with lyon.
//!
//! It's switched on while a plugin declares the `native:math` renderer (the bundled Math
//! plugin, [`set_enabled`]); off, math shows as its `$…$` source, as before. Formulas come
//! from the model, so they're bounded before parsing ([`MAX_SOURCE`], [`MAX_NESTING`]) and
//! one that doesn't typeset shows as source too.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use egui::epaint::{Mesh, Vertex, WHITE_UV};
use egui::{Color32, FontData, FontDefinitions, FontFamily, FontId, Painter, Pos2, Rect, Vec2, pos2, vec2};
use lyon_tessellation::math::point;
use lyon_tessellation::path::Path;
use lyon_tessellation::{BuffersBuilder, FillOptions, FillTessellator, FillVertex, StrokeOptions, StrokeTessellator, StrokeVertex, VertexBuffers};
use ratex_layout::LayoutOptions;
use ratex_types::color::Color;
use ratex_types::display_item::{DisplayItem, DisplayList};
use ratex_types::math_style::MathStyle;
use ratex_types::path_command::PathCommand;

/// Longest formula typeset (bytes); longer ones show as source.
pub const MAX_SOURCE: usize = 4000;
/// Deepest `{…}` / `\left…\right` nesting typeset (the parser and layout recurse).
pub const MAX_NESTING: usize = 48;
/// Formulas kept typeset.
const CACHE: usize = 512;
/// Math is set a little larger than the text around it, as KaTeX does.
pub const SCALE: f32 = 1.15;

/// The KaTeX faces RaTeX draws with (`Main-Regular` is `KaTeX_Main-Regular.ttf`).
const FACES: &[&str] = &[
    "AMS-Regular",
    "Caligraphic-Bold",
    "Caligraphic-Regular",
    "Fraktur-Bold",
    "Fraktur-Regular",
    "Main-Bold",
    "Main-BoldItalic",
    "Main-Italic",
    "Main-Regular",
    "Math-BoldItalic",
    "Math-Italic",
    "SansSerif-Bold",
    "SansSerif-Italic",
    "SansSerif-Regular",
    "Script-Regular",
    "Size1-Regular",
    "Size2-Regular",
    "Size3-Regular",
    "Size4-Regular",
    "Typewriter-Regular",
];

fn family(face: &str) -> FontFamily {
    FontFamily::Name(format!("katex-{face}").into())
}

/// Add the KaTeX faces to `fonts`, each its own family (static data: nothing is copied).
pub fn install_fonts(fonts: &mut FontDefinitions) {
    for face in FACES {
        let Some(std::borrow::Cow::Borrowed(bytes)) = ratex_katex_fonts::ttf_bytes(&format!("KaTeX_{face}.ttf")) else { continue };
        let name = format!("katex-{face}");
        fonts.font_data.insert(name.clone(), Arc::new(FontData::from_static(bytes)));
        fonts.families.insert(family(face), vec![name]);
    }
}

fn key() -> egui::Id {
    egui::Id::unique("byob-math-enabled")
}

/// Typeset math from now on (a plugin declares `native:math`), or show it as source.
pub fn set_enabled(ctx: &egui::Context, on: bool) {
    ctx.data_mut(|d| d.insert_temp(key(), on));
}

/// Math is typeset.
pub fn enabled(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp::<bool>(key())).unwrap_or(false)
}

/// The color RaTeX gives everything not `\color`ed: drawn in the text color instead.
const INK: Color = Color { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };

/// A typeset formula, in em (of the math font size): drawn from its top left, the baseline
/// `height` below the top.
#[derive(Debug)]
pub struct Formula {
    list: DisplayList,
    /// Filled and stroked outlines, triangulated once, in em from the top left.
    outlines: Vec<(Mesh, Option<Color>)>,
}

impl Formula {
    pub fn width(&self) -> f32 {
        em(self.list.width)
    }
    pub fn height(&self) -> f32 {
        em(self.list.height)
    }
    pub fn depth(&self) -> f32 {
        em(self.list.depth)
    }
}

#[allow(clippy::cast_possible_truncation)] // em-sized layout values
fn em(v: f64) -> f32 {
    if v.is_finite() { v as f32 } else { 0.0 }
}

/// Whether `src` is small and shallow enough to typeset.
fn bounded(src: &str) -> bool {
    if src.len() > MAX_SOURCE || src.trim().is_empty() {
        return false;
    }
    let (mut depth, mut deepest) = (0usize, 0usize);
    let bytes = src.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes.get(i) {
            Some(b'\\') => {
                let rest = src.get(i..).unwrap_or_default();
                if rest.starts_with("\\left") || rest.starts_with("\\begin") {
                    depth += 1;
                } else if rest.starts_with("\\right") || rest.starts_with("\\end") {
                    depth = depth.saturating_sub(1);
                }
                i += 2;
                continue;
            }
            Some(b'{') => depth += 1,
            Some(b'}') => depth = depth.saturating_sub(1),
            _ => {}
        }
        deepest = deepest.max(depth);
        i += 1;
    }
    deepest <= MAX_NESTING
}

/// Typeset `src` (`display`: on its own, bigger limits and fractions), or `None` when it
/// can't be. Cached.
pub fn typeset(src: &str, display: bool) -> Option<Arc<Formula>> {
    thread_local! {
        static DONE: RefCell<HashMap<(String, bool), Option<Arc<Formula>>>> = RefCell::new(HashMap::new());
    }
    if let Some(hit) = DONE.with(|d| d.borrow().get(&(src.to_owned(), display)).cloned()) {
        return hit;
    }
    let made = bounded(src).then(|| make(src, display)).flatten().map(Arc::new);
    DONE.with(|d| {
        let mut d = d.borrow_mut();
        if d.len() >= CACHE {
            d.clear();
        }
        d.insert((src.to_owned(), display), made.clone());
    });
    made
}

fn make(src: &str, display: bool) -> Option<Formula> {
    let nodes = ratex_parser::parser::parse(src).ok()?;
    let options = LayoutOptions { style: if display { MathStyle::Display } else { MathStyle::Text }, color: INK, ..LayoutOptions::default() };
    let root = ratex_layout::layout(&nodes, &options);
    ratex_layout::validate_layout(&root).ok()?;
    let list = ratex_layout::to_display_list(&root);
    if !(list.width.is_finite() && list.height.is_finite() && list.depth.is_finite()) || list.width > 500.0 || list.height + list.depth > 200.0 {
        return None;
    }
    let outlines = list
        .items
        .iter()
        .filter_map(|item| match item {
            DisplayItem::Path { x, y, commands, fill, color } => outline(*x, *y, commands, *fill).map(|m| (m, (*color != INK).then_some(*color))),
            _ => None,
        })
        .collect();
    Some(Formula { list, outlines })
}

/// An outline's triangles, in em from the formula's top left (white: tinted when drawn).
fn outline(x: f64, y: f64, commands: &[PathCommand], fill: bool) -> Option<Mesh> {
    let p = |px: f64, py: f64| point(em(x + px), em(y + py));
    let mut b = Path::builder();
    let mut open = false;
    for c in commands {
        match *c {
            PathCommand::MoveTo { x, y } => {
                if open {
                    b.end(false);
                }
                b.begin(p(x, y));
                open = true;
            }
            PathCommand::LineTo { x, y } if open => {
                b.line_to(p(x, y));
            }
            PathCommand::QuadTo { x1, y1, x, y } if open => {
                b.quadratic_bezier_to(p(x1, y1), p(x, y));
            }
            PathCommand::CubicTo { x1, y1, x2, y2, x, y } if open => {
                b.cubic_bezier_to(p(x1, y1), p(x2, y2), p(x, y));
            }
            PathCommand::Close if open => {
                b.end(true);
                open = false;
            }
            _ => {}
        }
    }
    if open {
        b.end(false);
    }
    let path = b.build();
    let mut geometry: VertexBuffers<[f32; 2], u32> = VertexBuffers::new();
    // Tolerance in em: a few hundredths of a pixel at chat sizes.
    let ok = if fill {
        FillTessellator::new()
            .tessellate_path(
                &path,
                &FillOptions::non_zero().with_tolerance(0.002),
                &mut BuffersBuilder::new(&mut geometry, |v: FillVertex| v.position().to_array()),
            )
            .is_ok()
    } else {
        StrokeTessellator::new()
            .tessellate_path(
                &path,
                &StrokeOptions::default().with_line_width(0.04).with_tolerance(0.002),
                &mut BuffersBuilder::new(&mut geometry, |v: StrokeVertex| v.position().to_array()),
            )
            .is_ok()
    };
    if !ok || geometry.indices.is_empty() {
        return None;
    }
    let vertices = geometry.vertices.iter().map(|[x, y]| Vertex { pos: pos2(*x, *y), uv: WHITE_UV, color: Color32::WHITE }).collect();
    Some(Mesh { indices: geometry.indices, vertices, ..Mesh::default() })
}

fn ink_color(c: &Color, ink: Color32) -> Color32 {
    if *c == INK {
        return ink;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // clamped to 0..=255
    let ch = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    Color32::from_rgba_unmultiplied(ch(c.r), ch(c.g), ch(c.b), ch(c.a))
}

/// Draw `f` with its baseline's left end at `at`, `size` points to the em, in `ink` (what
/// isn't `\color`ed).
pub fn paint(painter: &Painter, f: &Formula, at: Pos2, size: f32, ink: Color32) {
    let top_left = at - vec2(0.0, f.height() * size);
    let to = |x: f64, y: f64| top_left + vec2(em(x), em(y)) * size;
    for item in &f.list.items {
        match item {
            DisplayItem::GlyphPath { x, y, scale, font, char_code, color } => {
                let Some(ch) = char::from_u32(*char_code) else { continue };
                let glyph_size = (size * em(*scale)).clamp(1.0, 2000.0);
                let galley = painter.layout_no_wrap(ch.to_string(), FontId::new(glyph_size, family(font)), ink_color(color, ink));
                // Put the glyph's baseline at (x, y).
                let Some((row, g)) = galley.rows.first().and_then(|r| r.glyphs.first().map(|g| (r, g))) else { continue };
                let offset = Vec2::new(g.pos.x, row.pos.y + g.pos.y);
                painter.galley(to(*x, *y) - offset, galley.clone(), ink_color(color, ink));
            }
            DisplayItem::Line { x, y, width, thickness, color, .. } => {
                let t = (em(*thickness) * size).max(1.0 / painter.pixels_per_point());
                let start = to(*x, *y);
                painter.rect_filled(Rect::from_min_size(start - vec2(0.0, t / 2.0), vec2(em(*width) * size, t)), 0.0, ink_color(color, ink));
            }
            DisplayItem::Rect { x, y, width, height, color } => {
                painter.rect_filled(Rect::from_min_size(to(*x, *y), vec2(em(*width), em(*height)) * size), 0.0, ink_color(color, ink));
            }
            DisplayItem::Path { .. } => {}
        }
    }
    for (mesh, color) in &f.outlines {
        let tint = color.as_ref().map_or(ink, |c| ink_color(c, ink));
        let mut m = mesh.clone();
        for v in &mut m.vertices {
            v.pos = top_left + v.pos.to_vec2() * size;
            v.color = tint;
        }
        painter.add(egui::Shape::mesh(m));
    }
}

/// Where a formula drawn at `at` (baseline left) reaches, for layout and hit tests.
pub fn rect(f: &Formula, at: Pos2, size: f32) -> Rect {
    Rect::from_min_max(at - vec2(0.0, f.height() * size), at + vec2(f.width() * size, f.depth() * size))
}

/// A display formula on its own line: centered, scrolling sideways when wider than the column.
pub fn block(ui: &mut egui::Ui, f: &Formula, size: f32, ink: Color32, id: egui::Id) {
    let w = f.width() * size;
    let h = (f.height() + f.depth()) * size;
    let pad = size * 0.3;
    let draw = |ui: &mut egui::Ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(w.max(ui.available_width()), h + 2.0 * pad), egui::Sense::hover());
        let left = rect.center().x - w / 2.0;
        paint(ui.painter(), f, pos2(left.max(rect.left()), rect.top() + pad + f.height() * size), size, ink);
    };
    if w > ui.available_width() {
        egui::ScrollArea::horizontal().id_salt(id).auto_shrink([false, true]).show(ui, |ui| draw(ui));
    } else {
        draw(ui);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_formulas_typeset() {
        for src in [
            "x^2",
            "\\frac{a}{b}",
            "\\sqrt{x^2+1}",
            "\\sum_{i=1}^{n} i = \\frac{n(n+1)}{2}",
            "\\begin{pmatrix} 1 & 0 \\\\ 0 & 1 \\end{pmatrix}",
            "\\left( \\int_0^\\infty e^{-x}\\,dx \\right)",
            "\\mathbb{R}",
        ] {
            let f = typeset(src, true).unwrap_or_else(|| panic!("{src}"));
            assert!(f.width() > 0.0 && f.height() > 0.0, "{src}");
        }
        // Braces over and under are outlines: filled, so triangulated.
        assert!(!typeset("\\overbrace{a+b+c}", true).unwrap().outlines.is_empty());
    }

    /// Model output: too long, too deep, or not LaTeX shows as source (and nothing recurses
    /// itself to death).
    #[test]
    fn hostile_formulas_are_refused() {
        assert!(typeset(&"x+".repeat(MAX_SOURCE), false).is_none());
        assert!(typeset(&format!("{}x{}", "{".repeat(10_000), "}".repeat(10_000)), false).is_none());
        assert!(typeset(&format!("{}x{}", "\\left(".repeat(200), "\\right)".repeat(200)), false).is_none());
        assert!(typeset("\\frac{", false).is_none());
        assert!(typeset("   ", false).is_none());
        assert!(bounded(&format!("{}x{}", "{".repeat(MAX_NESTING), "}".repeat(MAX_NESTING))));
    }
}
