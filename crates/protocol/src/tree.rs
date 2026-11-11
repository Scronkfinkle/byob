//! The view tree (v1): declarative UI that plugins and the model send as JSON and every client
//! draws natively with its theme (`docs/views.md` is the authoring guide).
//!
//! A tree is a [`Node`]: containers (`column`, `row`, `card`) holding leaves (`text`,
//! `markdown`, `chip`, `button`, `input`, `toggle`, `select`, `progress`, `table`, `chart`,
//! `separator`, `image`, `canvas`, `grid`). Any node may carry a `key` (patches address it, clients keep
//! its transient state under it). The user's actions come back as an [`Event`] (an action name
//! plus the value of every named field, like an HTML form); a plugin answers with a [`Reply`]:
//! a whole new view, or [`Patch`]es to the current one.
//!
//! Trees are untrusted. [`decode`] never fails: it applies the limits ([`MAX_DEPTH`],
//! [`MAX_NODES`], …), shows a node it doesn't know as its `fallback` text (or leaves it out),
//! and lists everything it had to fix as [`Fault`]s (a JSON pointer and a sentence) that the
//! engine can hand back to whoever wrote the tree. [`apply`] applies patches the same way.
//! [`diff`] turns two trees into patches, so a plugin can always send its whole view and
//! clients still get small updates.
//!
//! On a step, a tree travels as a [`crate::View`] of kind [`KIND`] (`byob.tree`), with a
//! markdown summary as the fallback for clients that can't draw it.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

pub use byob_types::chart::ChartSpec;

mod decode;
mod diff;
mod patch;
#[cfg(test)]
mod tests;

pub use decode::{decode, decode_str};
pub use diff::diff;
pub use patch::apply;

/// The view kind of a tree on a step.
pub const KIND: &str = "byob.tree";
/// Schema version of the tree.
pub const VERSION: u32 = 1;

/// Deepest nesting (the root is depth 1).
pub const MAX_DEPTH: usize = 32;
/// Most nodes in one tree (table rows and canvas shapes aren't nodes).
pub const MAX_NODES: usize = 20_000;
/// Longest string anywhere in a node (characters); longer ones are cut.
pub const MAX_TEXT_CHARS: usize = 64 * 1024;
/// Longest node key or field name.
pub const MAX_KEY_CHARS: usize = 128;
/// Most rows in one table.
pub const MAX_TABLE_ROWS: usize = 100_000;
/// Most columns in one table.
pub const MAX_TABLE_COLUMNS: usize = 64;
/// Most shapes on one canvas.
pub const MAX_SHAPES: usize = 20_000;
/// Most options in one select.
pub const MAX_OPTIONS: usize = 1_000;
/// Most patches in one reply.
pub const MAX_PATCHES: usize = 10_000;
/// Most named values in an event or reply.
pub const MAX_VALUES: usize = 1_000;
/// Most rows in a grid (a spreadsheet's).
pub const MAX_GRID_ROWS: u32 = 1_048_576;
/// Most columns in a grid.
pub const MAX_GRID_COLS: u32 = 16_384;
/// Most blocks of known cells in one grid.
pub const MAX_GRID_BLOCKS: usize = 16;
/// Most cells in all the blocks of one grid.
pub const MAX_GRID_CELLS: usize = 50_000;
/// Longest text of one grid cell (shown value or input; characters).
pub const MAX_CELL_CHARS: usize = 4096;
/// Most faults reported for one tree or reply (the rest are counted in the last one).
pub const MAX_FAULTS: usize = 50;

/// One node of a view tree.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Node {
    /// Any string unique in the tree: patches address the node by it, and clients keep its
    /// state (typed text, scroll, selection) under it across replies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(flatten)]
    pub kind: Kind,
}

/// What a node is (`"type"` in JSON).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Kind {
    /// Children stacked top to bottom.
    Column {
        children: Vec<Node>,
    },
    /// Children side by side (wrapping); a row holding cards or columns splits the width
    /// equally between them.
    Row {
        children: Vec<Node>,
    },
    /// A framed box with an optional title.
    Card {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        title: String,
        children: Vec<Node>,
    },
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "is_default")]
        style: TextStyle,
        #[serde(default, skip_serializing_if = "is_default")]
        tone: Tone,
    },
    Markdown {
        text: String,
    },
    /// A small status pill.
    Chip {
        text: String,
        #[serde(default, skip_serializing_if = "is_default")]
        tone: Tone,
    },
    Button {
        label: String,
        /// Sent as the event's action.
        action: String,
        #[serde(default, skip_serializing_if = "is_false")]
        primary: bool,
        #[serde(default, skip_serializing_if = "is_false")]
        danger: bool,
    },
    /// A one-line text field; its text is `values[name]` in every event.
    Input {
        name: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        value: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        placeholder: String,
        /// Action sent when Enter is pressed in it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        submit: Option<String>,
    },
    /// A checkbox; `values[name]` is a bool.
    Toggle {
        name: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        label: String,
        #[serde(default, skip_serializing_if = "is_false")]
        value: bool,
        /// Action sent when it is flipped.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        action: Option<String>,
    },
    /// A drop-down; `values[name]` is the chosen option.
    Select {
        name: String,
        options: Vec<String>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        value: String,
        /// Action sent when the choice changes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        action: Option<String>,
    },
    /// A bar, `value` from 0 to 1.
    Progress {
        value: f32,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        label: String,
    },
    /// Rows of cells, drawn virtualized (only what is on screen), with editable columns.
    Table {
        /// Names the table in edit events and is `values[name]` (the selected row).
        #[serde(default, skip_serializing_if = "String::is_empty")]
        name: String,
        columns: Vec<TableColumn>,
        /// Cells in column order: strings, numbers, booleans or null.
        rows: Vec<Vec<Value>>,
        /// Tallest the table is drawn (points; it scrolls inside).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        height: Option<f32>,
        /// Action sent when a row is clicked (`values[name]` is its index).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        select: Option<String>,
    },
    /// A chart: the same spec as `.chart.json` artifacts.
    Chart {
        spec: ChartSpec,
    },
    Separator {},
    /// An image from the chat's media (a workspace path) or an artifact.
    Image {
        /// A workspace path, as in a media reference.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        media: Option<String>,
        /// An artifact id.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        artifact: Option<String>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        alt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        width: Option<f32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        height: Option<f32>,
    },
    /// A display list: shapes in a `width` × `height` box (scaled down to fit).
    Canvas {
        width: f32,
        height: f32,
        shapes: Vec<Shape>,
        /// Action sent on a click, with the point in `values._x` / `values._y` (canvas units).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        action: Option<String>,
    },
    /// A spreadsheet: a `rows` × `cols` sheet of which the client has the cells in `blocks`
    /// and asks for others (a `window` event) as the user scrolls. Its events carry a
    /// [`GridEvent`]; `values[name]` is the selection (a [`GridRange`]).
    Grid {
        /// Names the grid in events and is `values[name]`.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        name: String,
        rows: u32,
        cols: u32,
        /// Rectangles of known cells; the rest aren't loaded.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        blocks: Vec<GridBlock>,
        /// Data rows (from the top) that stay put while scrolling.
        #[serde(default, skip_serializing_if = "is_zero_u32")]
        frozen_rows: u32,
        /// Data columns (from the left) that stay put while scrolling.
        #[serde(default, skip_serializing_if = "is_zero_u32")]
        frozen_cols: u32,
        /// Column index → width in points.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        widths: BTreeMap<u32, f32>,
        /// Width of the other columns ([`GRID_DEFAULT_WIDTH`] if not given).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_width: Option<f32>,
        /// Cells can be edited, cleared and pasted into.
        #[serde(default, skip_serializing_if = "is_false")]
        editable: bool,
        /// Column letters and row numbers.
        #[serde(default = "yes", skip_serializing_if = "is_true")]
        headers: bool,
        /// Tallest the grid is drawn (points); else the height its site gives.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        height: Option<f32>,
        /// The selection to show (taken when the tree changes it, like a field's value).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selection: Option<GridRange>,
    },
}

/// Width of a grid column without its own.
pub const GRID_DEFAULT_WIDTH: f32 = 90.0;

/// Known cells of a grid: row-major `cells` from (`row`, `col`), 0-based.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GridBlock {
    pub row: u32,
    pub col: u32,
    #[serde(default)]
    pub cells: Vec<Vec<GridCell>>,
}

impl GridBlock {
    /// Rows the block covers.
    pub fn height(&self) -> u32 {
        u32::try_from(self.cells.len()).unwrap_or(u32::MAX)
    }

    /// Columns the block covers (its longest row; a short row's missing cells are empty).
    pub fn width(&self) -> u32 {
        u32::try_from(self.cells.iter().map(Vec::len).max().unwrap_or(0)).unwrap_or(u32::MAX)
    }

    /// Whether (`row`, `col`) of the sheet is in the block.
    pub fn covers(&self, row: u32, col: u32) -> bool {
        row >= self.row && col >= self.col && row - self.row < self.height() && col - self.col < self.width()
    }

    /// The cell at (`row`, `col`) of the sheet: `Some(None)` for an empty cell of the block,
    /// `None` outside it.
    pub fn get(&self, row: u32, col: u32) -> Option<Option<&GridCell>> {
        if row < self.row || col < self.col {
            return None;
        }
        let r = self.cells.get(usize::try_from(row - self.row).ok()?)?;
        let c = usize::try_from(col - self.col).ok()?;
        if c >= usize::try_from(self.width()).unwrap_or(0) {
            return None;
        }
        Some(r.get(c))
    }

    /// Cells in the block.
    pub fn cell_count(&self) -> usize {
        self.cells.iter().map(Vec::len).sum()
    }
}

/// A rectangle of a grid: the anchor (`row`, `col`, the active cell) and the other corner
/// (`to_row`, `to_col`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GridRange {
    pub row: u32,
    pub col: u32,
    pub to_row: u32,
    pub to_col: u32,
}

impl GridRange {
    pub fn cell(row: u32, col: u32) -> Self {
        Self { row, col, to_row: row, to_col: col }
    }

    /// Top row, left column, bottom row, right column.
    pub fn bounds(&self) -> (u32, u32, u32, u32) {
        (self.row.min(self.to_row), self.col.min(self.to_col), self.row.max(self.to_row), self.col.max(self.to_col))
    }

    pub fn contains(&self, row: u32, col: u32) -> bool {
        let (t, l, b, r) = self.bounds();
        (t..=b).contains(&row) && (l..=r).contains(&col)
    }
}

/// What a grid cell holds (`"t"` in JSON: `s`, `n`, `b`, `e`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum CellType {
    /// Nothing (null).
    #[default]
    Empty,
    Text,
    Number,
    Bool,
    Error,
}

/// Where a cell's text sits (by default numbers right, booleans centred, the rest left).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CellAlign {
    Left,
    Center,
    Right,
}

/// One grid cell. In JSON: null, a string, a number, a bool, or an object `{"v": shown text,
/// "i": input (a formula), "t": "s"|"n"|"b"|"e", "b": bold, "it": italic, "fill", "color",
/// "align"}`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GridCell {
    /// What the cell shows.
    pub v: String,
    /// What editing starts with and the formula bar shows (`=SUM(B2:B9)`), if not `v`.
    pub i: Option<String>,
    pub t: CellType,
    pub bold: bool,
    pub italic: bool,
    /// Background: a theme role (`accent`, `chart3`, …) or `#rrggbb`.
    pub fill: Option<String>,
    /// Text colour, the same way.
    pub color: Option<String>,
    pub align: Option<CellAlign>,
}

/// Longest colour name of a cell.
const MAX_COLOR_CHARS: usize = 64;

impl GridCell {
    pub fn text(v: impl Into<String>) -> Self {
        Self { v: v.into(), t: CellType::Text, ..Self::default() }
    }

    /// What editing the cell starts with.
    pub fn input(&self) -> &str {
        self.i.as_deref().unwrap_or(&self.v)
    }

    fn plain(&self) -> bool {
        self.i.is_none() && !self.bold && !self.italic && self.fill.is_none() && self.color.is_none() && self.align.is_none()
    }

    /// Decode an untrusted cell (unknown fields ignored); `Err` if it isn't one. Long text is
    /// cut to [`MAX_CELL_CHARS`] (the bool says so).
    pub fn decode(v: Value) -> Result<(GridCell, bool), &'static str> {
        let mut long = false;
        let mut cut = |s: String| {
            let (s, l) = cut_chars(s, MAX_CELL_CHARS);
            long |= l;
            s
        };
        let cell = match v {
            Value::Null => GridCell::default(),
            Value::String(s) => GridCell { v: cut(s), t: CellType::Text, ..GridCell::default() },
            Value::Number(n) => GridCell { v: n.to_string(), t: CellType::Number, ..GridCell::default() },
            Value::Bool(b) => GridCell { v: bool_text(b).into(), t: CellType::Bool, ..GridCell::default() },
            Value::Object(mut o) => {
                let (v, guessed) = match o.remove("v") {
                    None | Some(Value::Null) => (String::new(), None),
                    Some(Value::String(s)) => (cut(s), Some(CellType::Text)),
                    Some(Value::Number(n)) => (n.to_string(), Some(CellType::Number)),
                    Some(Value::Bool(b)) => (bool_text(b).into(), Some(CellType::Bool)),
                    Some(other) => (cut(other.to_string()), Some(CellType::Text)),
                };
                let t = match o.get("t").and_then(Value::as_str) {
                    Some("n") => CellType::Number,
                    Some("b") => CellType::Bool,
                    Some("e") => CellType::Error,
                    Some("s") => CellType::Text,
                    _ => guessed.unwrap_or(CellType::Empty),
                };
                let text = |o: &mut Map<String, Value>, k: &str| match o.remove(k) {
                    Some(Value::String(s)) => Some(s),
                    Some(Value::Number(n)) => Some(n.to_string()),
                    _ => None,
                };
                let flag = |o: &Map<String, Value>, k: &str| o.get(k).and_then(Value::as_bool).unwrap_or(false);
                let short = |s: Option<String>| s.map(|s| cut_chars(s, MAX_COLOR_CHARS).0);
                let i = text(&mut o, "i").map(&mut cut);
                GridCell {
                    v,
                    i,
                    t,
                    bold: flag(&o, "b"),
                    italic: flag(&o, "it"),
                    fill: short(text(&mut o, "fill")),
                    color: short(text(&mut o, "color")),
                    align: match o.get("align").and_then(Value::as_str) {
                        Some("left") => Some(CellAlign::Left),
                        Some("center" | "centre") => Some(CellAlign::Center),
                        Some("right") => Some(CellAlign::Right),
                        _ => None,
                    },
                }
            }
            Value::Array(_) => return Err("a cell is null, text, a number, a bool or an object like {\"v\": \"shown\"}"),
        };
        Ok((cell, long))
    }
}

fn bool_text(b: bool) -> &'static str {
    if b { "TRUE" } else { "FALSE" }
}

/// `s` cut to `max` characters; whether it was longer.
fn cut_chars(mut s: String, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s, false);
    }
    match s.char_indices().nth(max) {
        Some((at, _)) => {
            s.truncate(at);
            (s, true)
        }
        None => (s, false),
    }
}

impl Serialize for GridCell {
    /// The shortest form that decodes back to the same cell.
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        if self.plain() {
            match self.t {
                CellType::Empty if self.v.is_empty() => return s.serialize_none(),
                CellType::Text => return s.serialize_str(&self.v),
                CellType::Bool if self.v == "TRUE" || self.v == "FALSE" => return s.serialize_bool(self.v == "TRUE"),
                CellType::Number => {
                    if let Ok(n) = self.v.parse::<serde_json::Number>()
                        && n.to_string() == self.v
                    {
                        return n.serialize(s);
                    }
                }
                _ => {}
            }
        }
        let mut m = s.serialize_map(None)?;
        // Without "v" and "t" an object is an empty cell; with "v" and without "t", text.
        let t = match self.t {
            CellType::Empty if self.v.is_empty() => None,
            CellType::Empty => Some(""),
            CellType::Text => None,
            CellType::Number => Some("n"),
            CellType::Bool => Some("b"),
            CellType::Error => Some("e"),
        };
        if self.t != CellType::Empty || !self.v.is_empty() {
            m.serialize_entry("v", &self.v)?;
        }
        if let Some(t) = t {
            m.serialize_entry("t", t)?;
        }
        if let Some(i) = &self.i {
            m.serialize_entry("i", i)?;
        }
        if self.bold {
            m.serialize_entry("b", &true)?;
        }
        if self.italic {
            m.serialize_entry("it", &true)?;
        }
        if let Some(c) = &self.fill {
            m.serialize_entry("fill", c)?;
        }
        if let Some(c) = &self.color {
            m.serialize_entry("color", c)?;
        }
        if let Some(a) = &self.align {
            m.serialize_entry("align", a)?;
        }
        m.end()
    }
}

impl<'de> Deserialize<'de> for GridCell {
    /// Leniently: what isn't a cell is an empty one.
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(GridCell::decode(Value::deserialize(d)?).map(|c| c.0).unwrap_or_default())
    }
}

/// A grid column's name: `A`…`Z`, `AA`…
pub fn column_name(col: u32) -> String {
    let mut n = u64::from(col) + 1;
    let mut out = Vec::new();
    while n > 0 {
        let rem = u8::try_from((n - 1) % 26).unwrap_or(0);
        out.push(char::from(b'A' + rem));
        n = (n - 1) / 26;
    }
    out.iter().rev().collect()
}

/// A cell's name (`B3`) from 0-based indices.
pub fn cell_name(row: u32, col: u32) -> String {
    format!("{}{}", column_name(col), u64::from(row) + 1)
}

/// A colour role, not a colour: clients map it to their theme. Unknown names (a newer tone)
/// are [`Tone::Normal`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    #[default]
    Normal,
    Dim,
    Accent,
    Success,
    Warning,
    Danger,
}

/// Unknown names (a newer style) are [`TextStyle::Body`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TextStyle {
    #[default]
    Body,
    Heading,
    Strong,
    Small,
    Mono,
}

/// Deserialize an enum by name, unknown names (or non-strings) as the default.
macro_rules! lenient_names {
    ($t:ty { $($name:literal => $v:expr),* $(,)? }) => {
        impl<'de> Deserialize<'de> for $t {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                Ok(match Value::deserialize(d)?.as_str() {
                    $(Some($name) => $v,)*
                    _ => <$t>::default(),
                })
            }
        }
    };
}

lenient_names!(Tone { "dim" => Tone::Dim, "accent" => Tone::Accent, "success" => Tone::Success, "warning" => Tone::Warning, "danger" => Tone::Danger });
lenient_names!(TextStyle { "heading" => TextStyle::Heading, "strong" => TextStyle::Strong, "small" => TextStyle::Small, "mono" => TextStyle::Mono });

/// A table column.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TableColumn {
    /// Names the column in edit events.
    pub key: String,
    /// The header (the key if empty).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    /// Points; columns without one share the width left.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<f32>,
    /// Double-clicking a cell edits it and sends an `edit` event.
    #[serde(default, skip_serializing_if = "is_false")]
    pub editable: bool,
}

impl TableColumn {
    pub fn header(&self) -> &str {
        if self.label.is_empty() { &self.key } else { &self.label }
    }
}

/// One canvas shape (`"type"` in JSON). Colours are theme role names (`accent`, `text_dim`,
/// `border`, …), `chart1`…`chart8`, or `#rrggbb`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Shape {
    Rect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fill: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stroke: Option<String>,
        #[serde(default, skip_serializing_if = "is_zero")]
        radius: f32,
    },
    Circle {
        x: f32,
        y: f32,
        r: f32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fill: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stroke: Option<String>,
    },
    Line {
        points: Vec<[f32; 2]>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stroke: Option<String>,
        #[serde(default = "one", skip_serializing_if = "is_one")]
        width: f32,
    },
    Text {
        x: f32,
        y: f32,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<String>,
        #[serde(default = "fourteen", skip_serializing_if = "is_fourteen")]
        size: f32,
    },
}

fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}
fn is_false(b: &bool) -> bool {
    !*b
}
fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}
fn yes() -> bool {
    true
}
fn is_true(b: &bool) -> bool {
    *b
}
fn is_zero(v: &f32) -> bool {
    *v == 0.0
}
fn one() -> f32 {
    1.0
}
fn is_one(v: &f32) -> bool {
    *v == 1.0
}
fn fourteen() -> f32 {
    14.0
}
fn is_fourteen(v: &f32) -> bool {
    *v == 14.0
}

impl Node {
    pub fn new(kind: Kind) -> Self {
        Self { key: None, kind }
    }

    pub fn keyed(key: impl Into<String>, kind: Kind) -> Self {
        Self { key: Some(key.into()), kind }
    }

    /// The `"type"` name.
    pub fn type_name(&self) -> &'static str {
        self.kind.type_name()
    }

    pub fn children(&self) -> &[Node] {
        match &self.kind {
            Kind::Column { children } | Kind::Row { children } | Kind::Card { children, .. } => children,
            _ => &[],
        }
    }

    pub fn children_mut(&mut self) -> Option<&mut Vec<Node>> {
        match &mut self.kind {
            Kind::Column { children } | Kind::Row { children } | Kind::Card { children, .. } => Some(children),
            _ => None,
        }
    }

    /// Nodes in this subtree, itself included.
    pub fn count(&self) -> usize {
        1 + self.children().iter().map(Node::count).sum::<usize>()
    }

    /// Levels in this subtree (a leaf is 1).
    pub fn depth(&self) -> usize {
        1 + self.children().iter().map(Node::depth).max().unwrap_or(0)
    }

    /// The node with `key` in this subtree.
    pub fn find(&self, key: &str) -> Option<&Node> {
        if self.key.as_deref() == Some(key) {
            return Some(self);
        }
        self.children().iter().find_map(|c| c.find(key))
    }

    /// Markdown for a client that can't draw trees: the texts, titles and tables, without
    /// the controls.
    pub fn fallback_markdown(&self) -> String {
        let mut out = String::new();
        fallback(self, &mut out);
        let out = out.trim().to_owned();
        match out.char_indices().nth(crate::view::MAX_VIEW_FALLBACK_CHARS - 1) {
            Some((at, _)) => format!("{}…", out.get(..at).unwrap_or_default()),
            None => out,
        }
    }

    /// This tree as a step view.
    pub fn to_view(&self) -> crate::View {
        crate::View::of(self)
    }
}

impl Kind {
    pub fn type_name(&self) -> &'static str {
        match self {
            Kind::Column { .. } => "column",
            Kind::Row { .. } => "row",
            Kind::Card { .. } => "card",
            Kind::Text { .. } => "text",
            Kind::Markdown { .. } => "markdown",
            Kind::Chip { .. } => "chip",
            Kind::Button { .. } => "button",
            Kind::Input { .. } => "input",
            Kind::Toggle { .. } => "toggle",
            Kind::Select { .. } => "select",
            Kind::Progress { .. } => "progress",
            Kind::Table { .. } => "table",
            Kind::Chart { .. } => "chart",
            Kind::Separator {} => "separator",
            Kind::Image { .. } => "image",
            Kind::Canvas { .. } => "canvas",
            Kind::Grid { .. } => "grid",
        }
    }
}

/// Rows of a table shown in a fallback.
const FALLBACK_ROWS: usize = 20;
/// Columns of a grid shown in a fallback.
const FALLBACK_COLS: u32 = 12;

fn fallback(n: &Node, out: &mut String) {
    let cell = |v: &Value| cell_text(v).replace('|', "\\|").replace('\n', " ");
    match &n.kind {
        Kind::Column { children } | Kind::Row { children } => children.iter().for_each(|c| fallback(c, out)),
        Kind::Card { title, children } => {
            if !title.is_empty() {
                out.push_str(&format!("\n**{title}**\n\n"));
            }
            children.iter().for_each(|c| fallback(c, out));
        }
        Kind::Text { text, style: TextStyle::Heading, .. } => out.push_str(&format!("\n**{text}**\n\n")),
        Kind::Text { text, .. } | Kind::Markdown { text } => out.push_str(&format!("{text}\n\n")),
        Kind::Chip { text, .. } => out.push_str(&format!("`{text}` ")),
        Kind::Progress { value, label } => out.push_str(&format!("{label} ({:.0}%)\n\n", value.clamp(0.0, 1.0) * 100.0)),
        Kind::Chart { spec } => out.push_str(&format!("*Chart: {}*\n\n", spec.title)),
        Kind::Image { alt, .. } if !alt.is_empty() => out.push_str(&format!("*Image: {alt}*\n\n")),
        Kind::Table { columns, rows, .. } if !columns.is_empty() => {
            out.push_str(&format!("\n| {} |\n", columns.iter().map(|c| c.header().replace('|', "\\|")).collect::<Vec<_>>().join(" | ")));
            out.push_str(&format!("|{}\n", " --- |".repeat(columns.len())));
            for row in rows.iter().take(FALLBACK_ROWS) {
                let cells: Vec<String> = (0..columns.len()).map(|i| row.get(i).map(cell).unwrap_or_default()).collect();
                out.push_str(&format!("| {} |\n", cells.join(" | ")));
            }
            if rows.len() > FALLBACK_ROWS {
                out.push_str(&format!("\n*… {} more rows*\n", rows.len() - FALLBACK_ROWS));
            }
            out.push('\n');
        }
        Kind::Grid { blocks, rows, cols, .. } => {
            let size = format!("*Grid: {rows} × {cols}*\n");
            let Some(b) = blocks.first().filter(|b| b.height() > 0 && b.width() > 0) else {
                out.push_str(&size);
                out.push('\n');
                return;
            };
            let esc = |s: &str| s.replace('|', "\\|").replace('\n', " ");
            let ncols = b.width().min(FALLBACK_COLS);
            let letters: Vec<String> = (0..ncols).map(|c| column_name(b.col.saturating_add(c))).collect();
            out.push_str(&format!("\n|   | {} |\n", letters.join(" | ")));
            out.push_str(&format!("|{}\n", " --- |".repeat(letters.len() + 1)));
            for (r, row) in (0u64..).zip(b.cells.iter().take(FALLBACK_ROWS)) {
                let cells: Vec<String> = (0..letters.len()).map(|c| row.get(c).map(|c| esc(&c.v)).unwrap_or_default()).collect();
                out.push_str(&format!("| {} | {} |\n", u64::from(b.row) + r + 1, cells.join(" | ")));
            }
            out.push('\n');
            out.push_str(&size);
            out.push('\n');
        }
        _ => {}
    }
}

/// A table cell as text: strings as they are, null empty, anything else as JSON.
pub fn cell_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

impl<'de> Deserialize<'de> for Node {
    /// Decodes like [`decode`], dropping the faults.
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(decode(Value::deserialize(d)?).0)
    }
}

impl crate::ViewData for Node {
    const KIND: &'static str = KIND;
    const V: u32 = VERSION;
    fn fallback(&self) -> String {
        self.fallback_markdown()
    }
}

/// Something wrong with a tree, a patch or a reply, for whoever sent it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fault {
    /// JSON pointer into what was sent (`/children/2/action`; `/patch/0/...` in a reply).
    pub path: String,
    pub message: String,
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = if self.path.is_empty() { "/" } else { &self.path };
        write!(f, "{path}: {}", self.message)
    }
}

impl Fault {
    pub fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self { path: path.into(), message: message.into() }
    }

    /// Faults as lines for the model ("/children/1: unknown node type \"slider\"").
    pub fn report(faults: &[Fault]) -> String {
        faults.iter().map(Fault::to_string).collect::<Vec<_>>().join("\n")
    }
}

/// What the user did, for the tree's owner.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// The button's `action`, an input's `submit`, `"edit"` for a cell edit, …
    pub action: String,
    /// The current value of every named field (input text, toggle bool, select option,
    /// table selection), plus `_x`/`_y` for a canvas click.
    #[serde(default)]
    pub values: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit: Option<CellEdit>,
    /// What happened in a grid (the action is its op).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grid: Option<GridEvent>,
}

/// Something the user did in a grid. `op` (also the event's action) is `edit` (`value` typed
/// into `row`, `col`), `select` (the selection changed: `to_row`, `to_col` is the other
/// corner), `window` (the client wants the cells of `rows` × `cols` from `row`, `col`),
/// `paste` (`text`, tab-separated, at `row`, `col`) or `clear` (the cells to `to_row`,
/// `to_col`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridEvent {
    /// The grid's `name`.
    pub name: String,
    pub op: String,
    pub row: u32,
    pub col: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_row: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_col: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cols: Option<u32>,
    /// What was typed (always text; `=…` is a formula, the owner works it out).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Pasted text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// A table cell the user edited.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellEdit {
    pub table: String,
    pub row: usize,
    /// The column's key.
    pub column: String,
    /// What was typed (always text: convert it yourself).
    pub value: String,
}

/// A plugin's answer to an event: a whole new `view`, or patches to the current one; both
/// may be empty. `values` overrides what the user typed (e.g. `{"new": ""}` clears an input
/// after Add): a reply never changes a field's text otherwise, unless its `value` changed.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Reply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<Node>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub patch: Vec<Patch>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub values: Map<String, Value>,
}

impl Reply {
    /// Decode an untrusted reply line; faults say what was dropped. Only text that isn't a
    /// JSON object fails.
    pub fn parse(line: &str) -> Result<(Reply, Vec<Fault>), Fault> {
        let v: Value = serde_json::from_str(line).map_err(|e| Fault::new("", format!("not JSON: {e}")))?;
        if !v.is_object() {
            return Err(Fault::new("", "a reply must be a JSON object like {\"view\": {...}} or {\"patch\": [...]}"));
        }
        Ok(decode::reply(v))
    }

    /// Apply to `tree`: the new view (if any), then the patches.
    pub fn apply_to(self, tree: &mut Node) -> Vec<Fault> {
        if let Some(view) = self.view {
            *tree = view;
        }
        apply(tree, self.patch)
    }
}

impl<'de> Deserialize<'de> for Reply {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(decode::reply(Value::deserialize(d)?).0)
    }
}

/// Which node a patch is for: by `key`, or by `path`, a JSON pointer through `children`
/// (`""` is the root, `/children/0/children/2`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl Target {
    pub fn key(k: impl Into<String>) -> Self {
        Self { key: Some(k.into()), path: None }
    }
    pub fn path(p: impl Into<String>) -> Self {
        Self { key: None, path: Some(p.into()) }
    }
}

/// One change to a tree. In JSON each is an object with one field naming the operation:
/// `{"set": {"key": "count", "fields": {"text": "3 left"}}}`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Patch {
    /// Replace whatever is at a JSON pointer into the tree as JSON: a node
    /// (`/children/1`), one field (`/children/1/text`), a table row or cell
    /// (`/children/0/rows/7/2`). This is what [`diff`] makes.
    Put { path: String, value: Value },
    /// Replace the node.
    Replace {
        #[serde(flatten)]
        at: Target,
        node: Node,
    },
    /// Change some fields of the node (`{"text": "3 left", "tone": "success"}`).
    Set {
        #[serde(flatten)]
        at: Target,
        fields: Map<String, Value>,
    },
    /// Add children to the end of a container.
    Append {
        #[serde(flatten)]
        at: Target,
        nodes: Vec<Node>,
    },
    /// Remove the node.
    Remove {
        #[serde(flatten)]
        at: Target,
    },
    /// Write table rows `start..start + rows.len()` (growing the table as needed), then cut
    /// or grow it to `len` rows if given.
    Rows {
        #[serde(flatten)]
        at: Target,
        start: usize,
        #[serde(default)]
        rows: Vec<Vec<Value>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        len: Option<usize>,
    },
}

impl<'de> Deserialize<'de> for Patch {
    /// Decodes like a reply's patch list; a patch that doesn't decode is an error here.
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut dec = decode::Decoder::default();
        let patch = dec.patch(Value::deserialize(d)?, &mut String::new());
        patch.ok_or_else(|| serde::de::Error::custom(Fault::report(&dec.finish())))
    }
}

/// JSON pointer escaping of one segment.
pub fn pointer_segment(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}

/// The segments of a JSON pointer (`""` → none); `None` if it isn't one.
pub fn pointer_segments(p: &str) -> Option<Vec<String>> {
    if p.is_empty() {
        return Some(Vec::new());
    }
    let rest = p.strip_prefix('/')?;
    Some(rest.split('/').map(|s| s.replace("~1", "/").replace("~0", "~")).collect())
}
