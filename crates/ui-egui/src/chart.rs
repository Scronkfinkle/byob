//! Native charts: draws a [`ChartSpec`] (bar, line, area, pie, scatter; stacked; units) with
//! egui's painter, in theme colours, with a hover tooltip and a "Show data" table.
//!
//! Drawn by hand rather than with `egui_plot`: it needs a donut chart, categorical axes and
//! hob's look (rounded bar ends, recessive grid), and must not grab scroll/drag gestures
//! inside the transcript's scroll area.

use egui::{Align2, Color32, CornerRadius, FontId, Mesh, Pos2, Rect, Sense, Shape, Stroke, Vec2, pos2, vec2};
use byob_protocol::media::ChartSpec;
use byob_types::chart::ChartType;

use crate::theme::Tokens;

/// Height of the drawing (axes included).
const PLOT_HEIGHT: f32 = 240.0;
/// Narrowest a chart is drawn.
const MIN_WIDTH: f32 = 220.0;
/// Rows shown in the data table.
const TABLE_ROWS: usize = 100;
/// Line charts get dots on every point up to this many points.
const DOTS_UP_TO: usize = 24;

/// Draw a chart card.
pub fn show(ui: &mut egui::Ui, t: &Tokens, spec: &ChartSpec, id: egui::Id) {
    let palette = t.chart_palette();
    let color = |i: usize| palette.get(i % palette.len()).copied().unwrap_or(t.accent);
    egui::Frame::new().stroke(Stroke::new(1.0, t.border)).corner_radius(t.round(8.0)).inner_margin(t.pad(12.0)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(egui::RichText::new(&spec.title).strong().size(t.text_size(15.0)).color(t.text_strong));
        let items = legend(spec, &color);
        if !items.is_empty() {
            ui.horizontal_wrapped(|ui| {
                for (label, c) in &items {
                    let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
                    ui.painter().circle_filled(r.center(), 4.0, *c);
                    ui.label(egui::RichText::new(label).small().color(t.text_dim));
                    ui.add_space(6.0);
                }
            });
        }
        ui.add_space(4.0);
        let width = ui.available_width().max(MIN_WIDTH);
        let (rect, response) = ui.allocate_exact_size(vec2(width, PLOT_HEIGHT), Sense::hover());
        if ui.is_rect_visible(rect) {
            let hover = response.hover_pos().filter(|p| rect.contains(*p));
            let tip = match spec.kind {
                ChartType::Pie => draw_pie(ui, t, spec, rect, hover, &color),
                ChartType::Scatter => draw_scatter(ui, t, spec, rect, hover, &color),
                _ => draw_categorical(ui, t, spec, rect, hover, &color),
            };
            if let Some(tip) = tip {
                response.on_hover_ui_at_pointer(|ui| tooltip(ui, t, &tip));
            }
        }
        egui::CollapsingHeader::new(egui::RichText::new("Show data").small().color(t.text_dim)).id_salt(id.with("data")).show(ui, |ui| table(ui, t, spec));
    });
}

/// What the legend lists: the series (when there are several), or a pie's slices.
fn legend(spec: &ChartSpec, color: &dyn Fn(usize) -> Color32) -> Vec<(String, Color32)> {
    if spec.kind == ChartType::Pie {
        let key = spec.series.first().map(|s| s.key.as_str()).unwrap_or_default();
        let total: f64 = spec.data.iter().filter_map(|r| ChartSpec::number(r, key)).filter(|v| *v > 0.0).sum();
        return spec
            .data
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let v = ChartSpec::number(r, key).unwrap_or(0.0).max(0.0);
                let label = if total > 0.0 { format!("{} {:.0}%", spec.label(r), v / total * 100.0) } else { spec.label(r) };
                (label, color(i))
            })
            .collect();
    }
    if spec.series.len() > 1 { spec.series.iter().enumerate().map(|(i, s)| (s.label.clone(), color(i))).collect() } else { Vec::new() }
}

/// Hover tooltip content: a heading and (colour, name, value) rows.
struct Tip {
    heading: String,
    rows: Vec<(Color32, String, String)>,
}

fn tooltip(ui: &mut egui::Ui, t: &Tokens, tip: &Tip) {
    if !tip.heading.is_empty() {
        ui.label(egui::RichText::new(&tip.heading).strong().color(t.text_strong));
    }
    egui::Grid::new("chart-tip").num_columns(2).spacing(vec2(12.0, 2.0)).show(ui, |ui| {
        for (c, name, value) in &tip.rows {
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 4.0, *c);
                ui.label(egui::RichText::new(name).color(t.text_dim));
            });
            ui.label(egui::RichText::new(value).strong().color(t.text));
            ui.end_row();
        }
    });
}

fn table(ui: &mut egui::Ui, t: &Tokens, spec: &ChartSpec) {
    egui::ScrollArea::horizontal().id_salt("chart-table").show(ui, |ui| {
        egui::Grid::new(ui.scope_id().with("grid")).striped(true).spacing(vec2(16.0, 2.0)).show(ui, |ui| {
            ui.label(egui::RichText::new(&spec.x).small().strong().color(t.text));
            for s in &spec.series {
                ui.label(egui::RichText::new(&s.label).small().strong().color(t.text));
            }
            ui.end_row();
            for r in spec.data.iter().take(TABLE_ROWS) {
                let x = match spec.kind {
                    ChartType::Scatter => ChartSpec::number(r, &spec.x).map(full).unwrap_or_default(),
                    _ => spec.label(r),
                };
                ui.label(egui::RichText::new(x).small().color(t.text));
                for s in &spec.series {
                    let v = ChartSpec::number(r, &s.key).map(|v| with_unit(&full(v), spec.unit.as_deref())).unwrap_or_else(|| "–".into());
                    ui.label(egui::RichText::new(v).small().color(t.text));
                }
                ui.end_row();
            }
        });
    });
    if spec.data.len() > TABLE_ROWS {
        ui.label(egui::RichText::new(format!("… {} more rows", spec.data.len() - TABLE_ROWS)).small().color(t.text_dim));
    }
}

// ---- numbers ---------------------------------------------------------------------------

/// "1234.5" → "1,234.5": thousands separators, at most two decimals.
pub fn full(v: f64) -> String {
    if !v.is_finite() {
        return String::new();
    }
    let rounded = (v * 100.0).round() / 100.0;
    let neg = rounded < 0.0;
    let s = format!("{:.2}", rounded.abs());
    let (int, frac) = s.split_once('.').unwrap_or((&s, ""));
    let mut grouped = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    let frac = frac.trim_end_matches('0');
    let mut out = if neg { format!("-{grouped}") } else { grouped };
    if !frac.is_empty() {
        out.push('.');
        out.push_str(frac);
    }
    out
}

/// Axis labels: 1200 → "1.2k", 3400000 → "3.4M".
pub fn compact(v: f64) -> String {
    let a = v.abs();
    let (div, suffix) = if a >= 1e12 {
        (1e12, "T")
    } else if a >= 1e9 {
        (1e9, "B")
    } else if a >= 1e6 {
        (1e6, "M")
    } else if a >= 1e3 {
        (1e3, "k")
    } else {
        return full(v);
    };
    let n = (v / div * 10.0).round() / 10.0;
    let s = format!("{n:.1}");
    format!("{}{suffix}", s.trim_end_matches(".0"))
}

/// The unit where people expect it: "€1,234", "40%", "12 kg".
pub fn with_unit(n: &str, unit: Option<&str>) -> String {
    match unit {
        None | Some("") => n.to_owned(),
        Some(u @ ("€" | "$" | "£" | "¥" | "₹")) => match n.strip_prefix('-') {
            Some(rest) => format!("-{u}{rest}"),
            None => format!("{u}{n}"),
        },
        Some("%") => format!("{n}%"),
        Some(u) => format!("{n} {u}"),
    }
}

/// About `target` round tick values covering `lo..=hi` (steps of 1, 2 or 5 × 10^k).
pub fn ticks(lo: f64, hi: f64, target: usize) -> Vec<f64> {
    let (lo, hi) = if lo.is_finite() && hi.is_finite() { (lo.min(hi), lo.max(hi)) } else { (0.0, 1.0) };
    let span = if hi - lo > f64::EPSILON { hi - lo } else { lo.abs().max(1.0) };
    let raw = span / target.max(1) as f64;
    let mag = 10f64.powf(raw.log10().floor());
    let step = [1.0, 2.0, 2.5, 5.0, 10.0].iter().map(|m| m * mag).find(|s| *s >= raw).unwrap_or(10.0 * mag);
    if !step.is_finite() || step <= 0.0 {
        return vec![lo, hi];
    }
    let start = (lo / step).floor() * step;
    let mut out = Vec::new();
    let mut v = start;
    while v <= hi + step * 0.999 && out.len() < 50 {
        out.push(if v.abs() < step * 1e-9 { 0.0 } else { v });
        v += step;
    }
    if out.len() < 2 {
        out.push(start + step);
    }
    out
}

// ---- layout ----------------------------------------------------------------------------

/// Maps values to pixels inside the plot rect.
struct Frame {
    plot: Rect,
    lo: f64,
    hi: f64,
}

impl Frame {
    fn y(&self, v: f64) -> f32 {
        let f = if self.hi > self.lo { (v - self.lo) / (self.hi - self.lo) } else { 0.5 };
        #[allow(clippy::cast_possible_truncation)] // pixel coordinates
        let px = self.plot.bottom() - (f as f32) * self.plot.height();
        px
    }
}

fn font(size: f32) -> FontId {
    FontId::proportional(size)
}

/// Draw the y grid and labels; returns the frame for the remaining plot area.
fn y_axis(ui: &egui::Ui, t: &Tokens, spec: &ChartSpec, rect: Rect, lo: f64, hi: f64, x_label_h: f32) -> Frame {
    let ticks = ticks(lo, hi, 5);
    let lo = ticks.first().copied().unwrap_or(lo).min(lo);
    let hi = ticks.last().copied().unwrap_or(hi).max(hi);
    let painter = ui.painter();
    let labels: Vec<String> = ticks.iter().map(|v| with_unit(&compact(*v), spec.unit.as_deref())).collect();
    let label_w = labels.iter().map(|l| painter.layout_no_wrap(l.clone(), font(11.0), t.text_dim).size().x).fold(0.0f32, f32::max);
    let plot = Rect::from_min_max(pos2(rect.left() + label_w + 8.0, rect.top() + 6.0), pos2(rect.right() - 6.0, rect.bottom() - x_label_h));
    let f = Frame { plot, lo, hi };
    for (v, label) in ticks.iter().zip(labels) {
        let y = f.y(*v);
        let stroke = if *v == 0.0 { Stroke::new(1.0, t.border) } else { Stroke::new(1.0, t.separator) };
        painter.line_segment([pos2(plot.left(), y), pos2(plot.right(), y)], stroke);
        painter.text(pos2(plot.left() - 6.0, y), Align2::RIGHT_CENTER, label, font(11.0), t.text_dim);
    }
    f
}

/// Category labels under the plot, skipping some when they would collide.
fn x_labels(ui: &egui::Ui, t: &Tokens, labels: &[String], plot: Rect, band: f32) {
    let painter = ui.painter();
    let short: Vec<String> = labels.iter().map(|l| clip(l, 18)).collect();
    let widest = short.iter().map(|l| painter.layout_no_wrap(l.clone(), font(11.0), t.text_dim).size().x).fold(0.0f32, f32::max);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // small positive counts
    let every = (((widest + 10.0) / band.max(1.0)).ceil() as usize).max(1);
    for (i, l) in short.into_iter().enumerate().filter(|(i, _)| i % every == 0) {
        #[allow(clippy::cast_precision_loss)]
        let x = plot.left() + band * (i as f32 + 0.5);
        painter.text(pos2(x, plot.bottom() + 6.0), Align2::CENTER_TOP, l, font(11.0), t.text_dim);
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Value range for categorical charts (stacked: per-row sums of positives and negatives).
fn value_range(spec: &ChartSpec) -> (f64, f64) {
    let (mut lo, mut hi) = (0.0f64, 0.0f64);
    for r in &spec.data {
        let vals = spec.series.iter().filter_map(|s| ChartSpec::number(r, &s.key));
        if spec.stacked && matches!(spec.kind, ChartType::Bar | ChartType::Area) {
            let (pos, neg) = vals.fold((0.0, 0.0), |(p, n), v| if v >= 0.0 { (p + v, n) } else { (p, n + v) });
            hi = hi.max(pos);
            lo = lo.min(neg);
        } else {
            for v in vals {
                hi = hi.max(v);
                lo = lo.min(v);
            }
        }
    }
    if hi <= lo {
        hi = lo + 1.0;
    }
    (lo, hi)
}

// ---- bar / line / area -----------------------------------------------------------------

fn draw_categorical(ui: &egui::Ui, t: &Tokens, spec: &ChartSpec, rect: Rect, hover: Option<Pos2>, color: &dyn Fn(usize) -> Color32) -> Option<Tip> {
    let n = spec.data.len().max(1);
    let (lo, hi) = value_range(spec);
    let f = y_axis(ui, t, spec, rect, lo, hi, 22.0);
    #[allow(clippy::cast_precision_loss)]
    let band = f.plot.width() / n as f32;
    let labels: Vec<String> = spec.data.iter().map(|r| spec.label(r)).collect();
    x_labels(ui, t, &labels, f.plot, band);
    let painter = ui.painter_at(rect);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let hovered = hover.filter(|p| f.plot.x_range().contains(p.x)).map(|p| (((p.x - f.plot.left()) / band).floor().max(0.0) as usize).min(n - 1));
    #[allow(clippy::cast_precision_loss)]
    let cx = |i: usize| f.plot.left() + band * (i as f32 + 0.5);
    if let Some(i) = hovered {
        if spec.kind == ChartType::Bar {
            #[allow(clippy::cast_precision_loss)]
            let band_rect = Rect::from_x_y_ranges((f.plot.left() + band * i as f32)..=(f.plot.left() + band * (i + 1) as f32), f.plot.y_range());
            painter.rect_filled(band_rect, 4.0, t.hover.gamma_multiply(0.6));
        } else {
            painter.line_segment([pos2(cx(i), f.plot.top()), pos2(cx(i), f.plot.bottom())], Stroke::new(1.0, t.border));
        }
    }
    match spec.kind {
        ChartType::Bar => bars(&painter, t, spec, &f, band, color),
        ChartType::Area => areas(&painter, spec, &f, &cx, color),
        _ => {}
    }
    if matches!(spec.kind, ChartType::Line | ChartType::Area) {
        lines(&painter, t, spec, &f, &cx, hovered, color);
    }
    let i = hovered?;
    let row = spec.data.get(i)?;
    let rows = spec
        .series
        .iter()
        .enumerate()
        .map(|(k, s)| {
            (color(k), s.label.clone(), ChartSpec::number(row, &s.key).map(|v| with_unit(&full(v), spec.unit.as_deref())).unwrap_or_else(|| "–".into()))
        })
        .collect();
    Some(Tip { heading: spec.label(row), rows })
}

fn bars(painter: &egui::Painter, t: &Tokens, spec: &ChartSpec, f: &Frame, band: f32, color: &dyn Fn(usize) -> Color32) {
    let s = spec.series.len().max(1);
    let group = band * 0.76;
    let gap = if s > 1 && !spec.stacked { 2.0 } else { 0.0 };
    #[allow(clippy::cast_precision_loss)]
    let width = if spec.stacked { group.min(48.0) } else { ((group - gap * (s - 1) as f32) / s as f32).clamp(1.0, 48.0) };
    #[allow(clippy::cast_precision_loss)]
    let total = if spec.stacked { width } else { width * s as f32 + gap * (s - 1) as f32 };
    let zero = f.y(0.0);
    for (i, r) in spec.data.iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let left = f.plot.left() + band * (i as f32 + 0.5) - total / 2.0;
        let (mut pos, mut neg) = (0.0f64, 0.0f64);
        // index of the last positive/negative segment, which gets the rounded end
        let last_pos = spec.series.iter().rposition(|se| ChartSpec::number(r, &se.key).is_some_and(|v| v > 0.0));
        let last_neg = spec.series.iter().rposition(|se| ChartSpec::number(r, &se.key).is_some_and(|v| v < 0.0));
        for (k, se) in spec.series.iter().enumerate() {
            let Some(v) = ChartSpec::number(r, &se.key) else { continue };
            #[allow(clippy::cast_precision_loss)]
            let x0 = if spec.stacked { left } else { left + (width + gap) * k as f32 };
            let (a, b, end) = if spec.stacked {
                if v >= 0.0 {
                    let a = pos;
                    pos += v;
                    (a, pos, last_pos == Some(k))
                } else {
                    let a = neg;
                    neg += v;
                    (a, neg, last_neg == Some(k))
                }
            } else {
                (0.0, v, true)
            };
            let (y0, y1) = (f.y(a), f.y(b));
            let mut rect = Rect::from_x_y_ranges(x0..=(x0 + width), y0.min(y1)..=y0.max(y1));
            if spec.stacked && !end {
                // 1px surface gap between stacked segments
                rect = rect.shrink2(vec2(0.0, 0.5));
            }
            let r4 = if end { 4u8.min((rect.height() / 2.0).floor().clamp(0.0, 4.0) as u8) } else { 0 };
            let radius = if v >= 0.0 { CornerRadius { nw: r4, ne: r4, sw: 0, se: 0 } } else { CornerRadius { nw: 0, ne: 0, sw: r4, se: r4 } };
            painter.rect_filled(rect, radius, color(k));
        }
    }
    painter.line_segment([pos2(f.plot.left(), zero), pos2(f.plot.right(), zero)], Stroke::new(1.0, t.border));
}

/// Per-series (x, top, base) points; stacked areas sit on the series below.
fn area_points(spec: &ChartSpec, f: &Frame, cx: &dyn Fn(usize) -> f32) -> Vec<Vec<Option<(f32, f32, f32)>>> {
    let mut base = vec![0.0f64; spec.data.len()];
    spec.series
        .iter()
        .map(|s| {
            spec.data
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    let v = ChartSpec::number(r, &s.key);
                    let b = base.get(i).copied().unwrap_or(0.0);
                    if spec.stacked && spec.kind == ChartType::Area {
                        let top = b + v.unwrap_or(0.0);
                        if let Some(slot) = base.get_mut(i) {
                            *slot = top;
                        }
                        Some((cx(i), f.y(top), f.y(b)))
                    } else {
                        v.map(|v| (cx(i), f.y(v), f.y(0.0)))
                    }
                })
                .collect()
        })
        .collect()
}

fn areas(painter: &egui::Painter, spec: &ChartSpec, f: &Frame, cx: &dyn Fn(usize) -> f32, color: &dyn Fn(usize) -> Color32) {
    for (k, pts) in area_points(spec, f, cx).iter().enumerate() {
        let c = color(k);
        let mut mesh = Mesh::default();
        for w in pts.windows(2) {
            let (Some(Some(a)), Some(Some(b))) = (w.first(), w.get(1)) else { continue };
            let i = u32::try_from(mesh.vertices.len()).unwrap_or(0);
            mesh.colored_vertex(pos2(a.0, a.1), c.gamma_multiply(0.35));
            mesh.colored_vertex(pos2(b.0, b.1), c.gamma_multiply(0.35));
            mesh.colored_vertex(pos2(b.0, b.2), c.gamma_multiply(0.04));
            mesh.colored_vertex(pos2(a.0, a.2), c.gamma_multiply(0.04));
            mesh.add_triangle(i, i + 1, i + 2);
            mesh.add_triangle(i, i + 2, i + 3);
        }
        painter.add(Shape::mesh(mesh));
    }
}

fn lines(
    painter: &egui::Painter,
    t: &Tokens,
    spec: &ChartSpec,
    f: &Frame,
    cx: &dyn Fn(usize) -> f32,
    hovered: Option<usize>,
    color: &dyn Fn(usize) -> Color32,
) {
    let dots = spec.data.len() <= DOTS_UP_TO;
    for (k, pts) in area_points(spec, f, cx).iter().enumerate() {
        let c = color(k);
        let mut run: Vec<Pos2> = Vec::new();
        let flush = |run: &mut Vec<Pos2>| {
            if run.len() > 1 {
                painter.add(Shape::line(std::mem::take(run), Stroke::new(2.0, c)));
            }
            run.clear();
        };
        for p in pts {
            match p {
                Some((x, y, _)) => run.push(pos2(*x, *y)),
                None => flush(&mut run),
            }
        }
        flush(&mut run);
        for (i, p) in pts.iter().enumerate() {
            let Some((x, y, _)) = p else { continue };
            if hovered == Some(i) {
                painter.circle(pos2(*x, *y), 4.5, c, Stroke::new(2.0, t.assistant_bubble));
            } else if dots {
                painter.circle_filled(pos2(*x, *y), 3.0, c);
            }
        }
    }
}

// ---- scatter ---------------------------------------------------------------------------

fn draw_scatter(ui: &egui::Ui, t: &Tokens, spec: &ChartSpec, rect: Rect, hover: Option<Pos2>, color: &dyn Fn(usize) -> Color32) -> Option<Tip> {
    let xs: Vec<f64> = spec.data.iter().filter_map(|r| ChartSpec::number(r, &spec.x)).collect();
    let ys: Vec<f64> = spec.data.iter().flat_map(|r| spec.series.iter().filter_map(|s| ChartSpec::number(r, &s.key))).collect();
    let range = |v: &[f64]| {
        let lo = v.iter().copied().fold(f64::INFINITY, f64::min);
        let hi = v.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        if lo.is_finite() && hi.is_finite() { (lo, if hi > lo { hi } else { lo + 1.0 }) } else { (0.0, 1.0) }
    };
    let (ylo, yhi) = range(&ys);
    let f = y_axis(ui, t, spec, rect, ylo.min(0.0), yhi, 22.0);
    let xt = ticks(range(&xs).0, range(&xs).1, 6);
    let (xlo, xhi) = (xt.first().copied().unwrap_or(0.0), xt.last().copied().unwrap_or(1.0));
    let xspan = if xhi > xlo { xhi - xlo } else { 1.0 };
    #[allow(clippy::cast_possible_truncation)]
    let px = |v: f64| f.plot.left() + (((v - xlo) / xspan) as f32) * f.plot.width();
    let painter = ui.painter_at(rect);
    for v in &xt {
        painter.text(pos2(px(*v), f.plot.bottom() + 6.0), Align2::CENTER_TOP, compact(*v), font(11.0), t.text_dim);
    }
    let mut best: Option<(f32, usize, usize)> = None;
    for (k, s) in spec.series.iter().enumerate() {
        for (i, r) in spec.data.iter().enumerate() {
            let (Some(x), Some(y)) = (ChartSpec::number(r, &spec.x), ChartSpec::number(r, &s.key)) else { continue };
            let p = pos2(px(x), f.y(y));
            painter.circle(p, 4.0, color(k).gamma_multiply(0.85), Stroke::new(1.0, t.assistant_bubble));
            if let Some(h) = hover {
                let d = h.distance(p);
                if d < 12.0 && best.is_none_or(|(bd, _, _)| d < bd) {
                    best = Some((d, k, i));
                }
            }
        }
    }
    let (_, k, i) = best?;
    let (s, r) = (spec.series.get(k)?, spec.data.get(i)?);
    let p = pos2(px(ChartSpec::number(r, &spec.x)?), f.y(ChartSpec::number(r, &s.key)?));
    painter.circle(p, 6.0, color(k), Stroke::new(2.0, t.assistant_bubble));
    let rows = vec![
        (t.text_faint, spec.x.clone(), full(ChartSpec::number(r, &spec.x)?)),
        (color(k), s.label.clone(), with_unit(&full(ChartSpec::number(r, &s.key)?), spec.unit.as_deref())),
    ];
    Some(Tip { heading: String::new(), rows })
}

// ---- pie -------------------------------------------------------------------------------

fn draw_pie(ui: &egui::Ui, t: &Tokens, spec: &ChartSpec, rect: Rect, hover: Option<Pos2>, color: &dyn Fn(usize) -> Color32) -> Option<Tip> {
    let key = spec.series.first().map(|s| s.key.clone()).unwrap_or_default();
    let values: Vec<f64> = spec.data.iter().map(|r| ChartSpec::number(r, &key).unwrap_or(0.0).max(0.0)).collect();
    let total: f64 = values.iter().sum();
    let painter = ui.painter_at(rect);
    let center = rect.center();
    let outer = (rect.width().min(rect.height()) / 2.0 - 8.0).max(10.0);
    let inner = outer * 0.62;
    if total <= 0.0 {
        painter.text(center, Align2::CENTER_CENTER, "nothing to show", font(12.0), t.text_dim);
        return None;
    }
    let tau = std::f32::consts::TAU;
    // angle from 12 o'clock, clockwise
    let hovered = hover.and_then(|p| {
        let d = p - center;
        let r = d.length();
        if r < inner || r > outer + 6.0 {
            return None;
        }
        let a = d.x.atan2(-d.y).rem_euclid(tau);
        let mut acc = 0.0f32;
        values.iter().position(|v| {
            #[allow(clippy::cast_possible_truncation)]
            let sweep = (v / total) as f32 * tau;
            acc += sweep;
            a < acc
        })
    });
    let gap = if values.iter().filter(|v| **v > 0.0).count() > 1 { 0.012 } else { 0.0 };
    let mut start = 0.0f32;
    for (i, v) in values.iter().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        let sweep = (v / total) as f32 * tau;
        if sweep > gap * 2.0 {
            let grow = if hovered == Some(i) { 5.0 } else { 0.0 };
            ring(&painter, center, inner, outer + grow, start + gap, start + sweep - gap, color(i));
        }
        start += sweep;
    }
    let total_label = with_unit(&compact(total), spec.unit.as_deref());
    painter.text(center - vec2(0.0, 8.0), Align2::CENTER_CENTER, total_label, font(18.0), t.text_strong);
    painter.text(center + vec2(0.0, 12.0), Align2::CENTER_CENTER, "total", font(11.0), t.text_dim);
    let i = hovered?;
    let r = spec.data.get(i)?;
    let v = values.get(i).copied()?;
    let label = spec.series.first().map(|s| s.label.clone()).unwrap_or_default();
    let value = format!("{} ({:.0}%)", with_unit(&full(v), spec.unit.as_deref()), v / total * 100.0);
    Some(Tip { heading: spec.label(r), rows: vec![(color(i), label, value)] })
}

/// A ring segment from angle `a0` to `a1` (radians from 12 o'clock, clockwise).
fn ring(painter: &egui::Painter, c: Pos2, r0: f32, r1: f32, a0: f32, a1: f32, color: Color32) {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let steps = (((a1 - a0) / std::f32::consts::TAU * 128.0).ceil() as usize).max(2);
    let at = |a: f32, r: f32| c + Vec2::new(a.sin(), -a.cos()) * r;
    let mut mesh = Mesh::default();
    for s in 0..=steps {
        #[allow(clippy::cast_precision_loss)]
        let a = a0 + (a1 - a0) * s as f32 / steps as f32;
        mesh.colored_vertex(at(a, r0), color);
        mesh.colored_vertex(at(a, r1), color);
        if s > 0 {
            let i = u32::try_from(s * 2).unwrap_or(0);
            mesh.add_triangle(i - 2, i - 1, i);
            mesh.add_triangle(i - 1, i, i + 1);
        }
    }
    painter.add(Shape::mesh(mesh));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn number_formats() {
        assert_eq!(full(1234.5), "1,234.5");
        assert_eq!(full(-1234567.891), "-1,234,567.89");
        assert_eq!(full(0.0), "0");
        assert_eq!(full(100.0), "100");
        assert_eq!(compact(1200.0), "1.2k");
        assert_eq!(compact(3_400_000.0), "3.4M");
        assert_eq!(compact(-2000.0), "-2k");
        assert_eq!(compact(12.5), "12.5");
        assert_eq!(with_unit("12", Some("€")), "€12");
        assert_eq!(with_unit("-12", Some("$")), "-$12");
        assert_eq!(with_unit("40", Some("%")), "40%");
        assert_eq!(with_unit("3", Some("kg")), "3 kg");
        assert_eq!(with_unit("3", None), "3");
    }

    #[test]
    fn nice_ticks() {
        assert_eq!(ticks(0.0, 100.0, 5), vec![0.0, 20.0, 40.0, 60.0, 80.0, 100.0]);
        assert_eq!(ticks(0.0, 1050.0, 5), vec![0.0, 250.0, 500.0, 750.0, 1000.0, 1250.0]);
        assert!(ticks(-3.0, 7.0, 5).contains(&0.0));
        for (lo, hi) in [(5.0, 5.0), (f64::NAN, 1.0), (0.0, 1e-300), (-1e300, 1e300)] {
            let t = ticks(lo, hi, 5);
            assert!(t.len() >= 2 && t.len() <= 50, "{lo} {hi} {t:?}");
        }
    }

    fn spec(kind: &str, extra: serde_json::Value) -> ChartSpec {
        let mut v = json!({"type": kind, "title": "Sales", "x": "m", "series": ["a", "b"], "data": [
            {"m": "Jan", "a": 10, "b": -3}, {"m": "Feb", "a": null, "b": 5}, {"m": "Mar", "a": 30, "b": 8}
        ]});
        if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
            o.extend(e.clone());
        }
        byob_types::chart::validate(&v).unwrap()
    }

    /// Every chart type draws (with and without hover) without panicking, at odd sizes too.
    #[test]
    fn every_kind_draws() {
        let specs = vec![
            spec("bar", json!({})),
            spec("bar", json!({"stacked": true, "unit": "€"})),
            spec("line", json!({})),
            spec("area", json!({"stacked": true})),
            spec("area", json!({})),
            spec("scatter", json!({"x": "a", "series": ["b"]})),
            spec("pie", json!({"series": ["a"]})),
            spec("pie", json!({"series": ["b"], "data": [{"m": "x", "b": 0}]})),
        ];
        for t in [Tokens::dark(), Tokens::light()] {
            for s in &specs {
                for (w, pointer) in [(800.0, Some(pos2(300.0, 150.0))), (30.0, None), (500.0, Some(pos2(250.0, 200.0)))] {
                    let ctx = egui::Context::default();
                    let mut input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(w, 600.0))), ..Default::default() };
                    if let Some(p) = pointer {
                        input.events.push(egui::Event::PointerMoved(p));
                    }
                    for _ in 0..2 {
                        let mut out = ctx.run_ui(input.clone(), |ui| show(ui, &t, s, egui::Id::unique("c")));
                        out.textures_delta.clear();
                    }
                }
            }
        }
    }

    #[test]
    fn pie_legend_has_percentages() {
        let s = spec("pie", json!({"series": ["a"]}));
        let palette = Tokens::dark().chart_palette();
        let items = legend(&s, &|i| palette[i % 8]);
        assert_eq!(items.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>(), vec!["Jan 25%", "Feb 0%", "Mar 75%"]);
        assert!(legend(&spec("line", json!({"series": ["a"]})), &|_| Color32::RED).is_empty());
        assert_eq!(legend(&spec("line", json!({})), &|_| Color32::RED).len(), 2);
    }
}
