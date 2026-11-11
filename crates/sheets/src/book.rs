//! Workbooks: IronCalc's raw `Model` behind the small interface the tools use.
//! Sheets are 0-based indices, rows and columns 1-based (IronCalc's convention).

use std::path::Path;

use ironcalc::base::Model;
use ironcalc::base::cell::CellValue;
use ironcalc::base::expressions::types::Area;
use ironcalc::base::types::{CellType, Style};
use serde_json::Value;

use crate::SheetError;
use crate::text::fmt_g;

pub const MAX_IMPORT_CELLS: usize = 2_000_000;
const LOCALE: &str = "en";
const TZ: &str = "UTC";
const LANG: &str = "en";

/// A cell's type: number, string, bool, error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    N,
    S,
    B,
    E,
}

/// Does an Excel number format show a date or time (`d`, `m`, `y`, `h`, `s` outside quotes,
/// brackets and escapes)?
pub fn is_date_format(fmt: &str) -> bool {
    if fmt.is_empty() || fmt.eq_ignore_ascii_case("general") {
        return false;
    }
    let mut it = fmt.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '"' => {
                // "…" (unclosed: the quote alone)
                let rest: String = it.clone().collect();
                if let Some(end) = rest.find('"') {
                    for _ in 0..=rest[..end].chars().count() {
                        it.next();
                    }
                }
            }
            '[' => {
                let rest: String = it.clone().collect();
                if let Some(end) = rest.find(']') {
                    for _ in 0..=rest[..end].chars().count() {
                        it.next();
                    }
                }
            }
            '\\' => {
                it.next();
            }
            c if matches!(c.to_ascii_lowercase(), 'd' | 'm' | 'y' | 'h' | 's') => return true,
            _ => {}
        }
    }
    false
}

/// A number for the model: 12, 0.125, 1234567.5 (no exponent noise).
pub fn num_text(v: f64) -> String {
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 1e15 {
        // Python's str(int(v)); -0.0 is 0
        return format!("{}", v as i64);
    }
    fmt_g(v, 10)
}

pub fn bool_text(b: bool) -> String {
    if b { "TRUE" } else { "FALSE" }.into()
}

/// `"xlsx"`, `"csv"` or `"tsv"` by the file name.
pub fn file_format(path: &str) -> &'static str {
    let low = path.to_lowercase();
    if low.ends_with(".tsv") || low.ends_with(".tab") {
        "tsv"
    } else if low.ends_with(".csv") || low.ends_with(".txt") {
        "csv"
    } else {
        "xlsx"
    }
}

/// Rows of a .csv/.tsv file (lists of strings), read like Python's `csv` module does.
pub fn read_table(path: &Path, fmt: Option<&str>) -> Result<Vec<Vec<String>>, SheetError> {
    let raw = std::fs::read(path).map_err(|e| SheetError::Io(e.to_string()))?;
    let raw = raw.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&raw);
    let text = String::from_utf8_lossy(raw);
    let fmt = fmt.unwrap_or_else(|| file_format(&path.to_string_lossy()));
    let delim = if fmt == "tsv" { '\t' } else { sniff(&text.chars().take(8192).collect::<String>()).unwrap_or(',') };
    Ok(parse_csv(&text, delim))
}

const DELIMS: [char; 4] = [',', ';', '\t', '|'];
const PREFERRED: [char; 5] = [',', '\t', ';', ' ', ':'];

/// Python's `csv.Sniffer().sniff(sample, delimiters=",;\t|").delimiter` (its quote heuristic
/// approximated: the delimiter seen most often right before or after a quoted field).
fn sniff(sample: &str) -> Option<char> {
    if sample.contains('"') || sample.contains('\'') {
        let b: Vec<char> = sample.chars().collect();
        let mut counts = [0usize; 4];
        for (i, &c) in b.iter().enumerate() {
            if c != '"' && c != '\'' {
                continue;
            }
            for d in [i.checked_sub(1), i.checked_sub(2).filter(|j| b.get(j + 1) == Some(&' '))].into_iter().flatten() {
                if let Some(k) = b.get(d).and_then(|x| DELIMS.iter().position(|y| y == x)) {
                    counts[k] += 1;
                    break;
                }
            }
            if let Some(k) = b.get(i + 1).and_then(|x| DELIMS.iter().position(|y| y == x)) {
                counts[k] += 1;
            }
        }
        let best = (0..4).max_by_key(|&k| (counts[k], std::cmp::Reverse(k)));
        if let Some(k) = best
            && counts[k] > 0
        {
            return Some(DELIMS[k]);
        }
    }
    guess_delimiter(sample)
}

/// Python's `Sniffer._guess_delimiter`, for the four candidates.
fn guess_delimiter(sample: &str) -> Option<char> {
    let data: Vec<&str> = sample.split('\n').filter(|l| !l.is_empty()).collect();
    if data.is_empty() {
        return None;
    }
    let chunk = data.len().min(10);
    let mut freq: Vec<Vec<(usize, usize)>> = vec![Vec::new(); 4]; // per delim: (count per line, lines), in first-seen order
    let mut delims: Vec<(char, (usize, i64))> = Vec::new();
    let (mut start, mut end, mut iteration) = (0, chunk, 0usize);
    while start < data.len() {
        iteration += 1;
        for line in data.get(start..end.min(data.len())).unwrap_or_default() {
            for (k, d) in DELIMS.iter().enumerate() {
                let n = line.matches(*d).count();
                match freq[k].iter_mut().find(|(f, _)| *f == n) {
                    Some(e) => e.1 += 1,
                    None => freq[k].push((n, 1)),
                }
            }
        }
        let mut modes: Vec<(char, (usize, i64))> = Vec::new();
        for (k, d) in DELIMS.iter().enumerate() {
            let items = &freq[k];
            if items.len() == 1 && items[0].0 == 0 {
                continue;
            }
            // the mode (the first of the most common), minus every other frequency's count
            let mut best = 0;
            for (i, it) in items.iter().enumerate() {
                if it.1 > items[best].1 {
                    best = i;
                }
            }
            let others: usize = items.iter().enumerate().filter(|(i, _)| *i != best).map(|(_, x)| x.1).sum();
            let m = items[best];
            modes.push((*d, (m.0, i64::try_from(m.1).unwrap_or(0) - i64::try_from(others).unwrap_or(0))));
        }
        let total = (chunk * iteration).min(data.len()) as f64;
        let mut consistency = 1.0f64;
        while delims.is_empty() && consistency >= 0.9 {
            for (d, v) in &modes {
                if v.0 > 0 && v.1 > 0 && (v.1 as f64 / total) >= consistency {
                    delims.push((*d, *v));
                }
            }
            consistency -= 0.01;
        }
        if delims.len() == 1 {
            return Some(delims[0].0);
        }
        start = end;
        end += chunk;
    }
    if delims.is_empty() {
        return None;
    }
    for p in PREFERRED {
        if delims.iter().any(|(d, _)| *d == p) {
            return Some(p);
        }
    }
    delims.iter().max_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0))).map(|(d, _)| *d)
}

/// Python's `csv.reader(io.StringIO(text), delimiter=d)` with the default (excel) dialect.
pub fn parse_csv(text: &str, delim: char) -> Vec<Vec<String>> {
    #[derive(PartialEq)]
    enum St {
        StartRecord,
        StartField,
        InField,
        InQuoted,
        QuoteInQuoted,
        EatCrnl,
    }
    let mut rows = Vec::new();
    let mut fields: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut st = St::StartRecord;
    let chars = text.chars();
    for c in chars {
        let nl = c == '\n' || c == '\r';
        match st {
            St::StartRecord | St::StartField => {
                if nl {
                    if st == St::StartField {
                        fields.push(std::mem::take(&mut field));
                    }
                    rows.push(std::mem::take(&mut fields));
                    st = if c == '\n' { St::StartRecord } else { St::EatCrnl };
                } else if c == '"' {
                    st = St::InQuoted;
                } else if c == delim {
                    fields.push(std::mem::take(&mut field));
                    st = St::StartField;
                } else {
                    field.push(c);
                    st = St::InField;
                }
            }
            St::InField => {
                if nl {
                    fields.push(std::mem::take(&mut field));
                    rows.push(std::mem::take(&mut fields));
                    st = if c == '\n' { St::StartRecord } else { St::EatCrnl };
                } else if c == delim {
                    fields.push(std::mem::take(&mut field));
                    st = St::StartField;
                } else {
                    field.push(c);
                }
            }
            St::InQuoted => {
                if c == '"' {
                    st = St::QuoteInQuoted;
                } else {
                    field.push(c);
                }
            }
            St::QuoteInQuoted => {
                if c == '"' {
                    field.push('"');
                    st = St::InQuoted;
                } else if c == delim {
                    fields.push(std::mem::take(&mut field));
                    st = St::StartField;
                } else if nl {
                    fields.push(std::mem::take(&mut field));
                    rows.push(std::mem::take(&mut fields));
                    st = if c == '\n' { St::StartRecord } else { St::EatCrnl };
                } else {
                    field.push(c);
                    st = St::InField;
                }
            }
            St::EatCrnl => {
                if c == '\n' {
                    st = St::StartRecord;
                } else if c != '\r' {
                    // a lone \r ended the line; this starts the next record
                    if c == '"' {
                        st = St::InQuoted;
                    } else if c == delim {
                        fields.push(String::new());
                        st = St::StartField;
                    } else {
                        field.push(c);
                        st = St::InField;
                    }
                }
            }
        }
    }
    match st {
        St::StartRecord | St::EatCrnl => {}
        _ => {
            fields.push(field);
            rows.push(fields);
        }
    }
    rows
}

fn leading_zero(v: &str) -> bool {
    v.len() >= 2 && v.starts_with('0') && v.bytes().all(|b| b.is_ascii_digit())
}

fn engine(e: impl std::fmt::Display) -> SheetError {
    SheetError::Engine(e.to_string())
}

/// An IronCalc workbook (nothing recalculates until [`Book::evaluate`]).
pub struct Book {
    pub m: Model<'static>,
    /// Bumped on every change (caches key on it).
    pub version_gen: u64,
    names: Option<Vec<String>>,
    /// Error cells cached for `version_gen`.
    pub errors_cache: Option<(u64, Vec<crate::tools::ErrorCell>)>,
}

impl Book {
    fn wrap(m: Model<'static>) -> Self {
        Self { m, version_gen: 0, names: None, errors_cache: None }
    }

    pub fn new(name: &str) -> Result<Self, SheetError> {
        let mut m = Model::new_empty("Workbook", LOCALE, TZ, LANG).map_err(engine)?;
        m.workbook.name = name.to_owned();
        Ok(Self::wrap(m))
    }

    /// An .xlsx, .csv or .tsv file (`fmt`: `xlsx`, `csv` or `tsv`; else from the suffix).
    pub fn load(path: &Path, fmt: Option<&str>) -> Result<Self, SheetError> {
        let fmt = fmt.unwrap_or_else(|| file_format(&path.to_string_lossy()));
        if fmt == "csv" || fmt == "tsv" {
            let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).filter(|s| !s.is_empty()).unwrap_or_else(|| "Workbook".into());
            let mut b = Self::new(&stem)?;
            b.import_rows(0, &read_table(path, Some(fmt))?)?;
            b.evaluate();
            return Ok(b);
        }
        let name = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let m = ironcalc::import::load_from_xlsx(&path.to_string_lossy(), LOCALE, TZ, LANG)
            .map_err(|e| SheetError::Msg(format!("{name} couldn't be opened as an .xlsx workbook ({e}).")))?;
        let mut b = Self::wrap(m);
        b.evaluate();
        Ok(b)
    }

    pub fn import_rows(&mut self, s: u32, rows: &[Vec<String>]) -> Result<(), SheetError> {
        let n: usize = rows.iter().map(Vec::len).sum();
        if n > MAX_IMPORT_CELLS {
            return Err(SheetError::Msg(format!("That table has {n} cells; spreadsheets here hold at most {MAX_IMPORT_CELLS}.")));
        }
        for (i, row) in rows.iter().enumerate() {
            for (j, v) in row.iter().enumerate() {
                if v.is_empty() {
                    continue;
                }
                let v = if leading_zero(v) { format!("'{v}") } else { v.clone() };
                self.m.set_user_input(s, idx(i + 1), idx(j + 1), v).map_err(engine)?;
            }
        }
        self.version_gen += 1;
        Ok(())
    }

    pub fn to_xlsx(&self) -> Result<Vec<u8>, SheetError> {
        let w = ironcalc::export::save_xlsx_to_writer(&self.m, std::io::Cursor::new(Vec::new())).map_err(engine)?;
        Ok(w.into_inner())
    }

    pub fn evaluate(&mut self) {
        self.m.evaluate();
        self.version_gen += 1;
    }

    pub fn touch(&mut self) {
        self.version_gen += 1;
        self.names = None;
    }

    // -- sheets
    pub fn sheet_names(&mut self) -> Vec<String> {
        if self.names.is_none() {
            self.names = Some(self.m.get_worksheets_properties().into_iter().map(|p| p.name).collect());
        }
        self.names.clone().unwrap_or_default()
    }

    /// (min_row, max_row, min_col, max_col), or `None` when the sheet is empty.
    pub fn used(&self, s: u32) -> Option<(i64, i64, i64, i64)> {
        let d = self.m.workbook.worksheet(s).ok()?.dimension();
        let d = (i64::from(d.min_row), i64::from(d.max_row), i64::from(d.min_column), i64::from(d.max_column));
        if d == (1, 1, 1, 1) && self.m.is_empty_cell(s, 1, 1).unwrap_or(true) {
            return None;
        }
        Some(d)
    }

    pub fn frozen(&self, s: u32) -> (i64, i64) {
        (i64::from(self.m.get_frozen_rows_count(s).unwrap_or(0)), i64::from(self.m.get_frozen_columns_count(s).unwrap_or(0)))
    }

    pub fn set_frozen(&mut self, s: u32, rows: i64, cols: i64) -> Result<(), SheetError> {
        self.m.set_frozen_rows(s, idx64(rows)).map_err(engine)?;
        self.m.set_frozen_columns(s, idx64(cols)).map_err(engine)?;
        self.touch();
        Ok(())
    }

    pub fn width(&self, s: u32, c: i64) -> f64 {
        self.m.get_column_width(s, idx64(c)).unwrap_or(crate::view::DEFAULT_WIDTH)
    }

    pub fn set_width(&mut self, s: u32, c: i64, w: f64) -> Result<(), SheetError> {
        self.m.set_column_width(s, idx64(c), w).map_err(engine)?;
        self.touch();
        Ok(())
    }

    /// Every non-empty (sheet, row, col).
    pub fn all_cells(&self) -> Vec<(u32, i64, i64)> {
        self.m.get_all_cells().into_iter().map(|c| (c.index, i64::from(c.row), i64::from(c.column))).collect()
    }

    // -- cells
    /// What was typed: `=B2*C2`, `1200`, `Food` (`""` when empty).
    pub fn content(&self, s: u32, r: i64, c: i64) -> String {
        self.m.get_localized_cell_content(s, idx64(r), idx64(c)).unwrap_or_default()
    }

    pub fn kind(&self, s: u32, r: i64, c: i64) -> Kind {
        match self.m.get_cell_type(s, idx64(r), idx64(c)) {
            Ok(CellType::Number) => Kind::N,
            Ok(CellType::LogicalValue) => Kind::B,
            Ok(CellType::ErrorValue) => Kind::E,
            _ => Kind::S,
        }
    }

    pub fn value(&self, s: u32, r: i64, c: i64) -> CellValue {
        self.m.get_cell_value_by_index(s, idx64(r), idx64(c)).unwrap_or(CellValue::None)
    }

    /// The value as shown: `1,234.50`, `12%`, `2024-01-05`.
    pub fn text(&self, s: u32, r: i64, c: i64) -> String {
        self.m.get_formatted_cell_value(s, idx64(r), idx64(c)).unwrap_or_default()
    }

    /// The cell's style (IronCalc's serde form).
    pub fn style(&self, s: u32, r: i64, c: i64) -> Value {
        self.m.get_style_for_cell(s, idx64(r), idx64(c)).ok().and_then(|st| serde_json::to_value(st).ok()).unwrap_or(Value::Null)
    }

    pub fn set_style(&mut self, s: u32, r: i64, c: i64, st: &Value) -> Result<(), SheetError> {
        let style: Style = serde_json::from_value(st.clone()).map_err(engine)?;
        self.m.set_cell_style(s, idx64(r), idx64(c), &style).map_err(engine)
    }

    pub fn num_fmt(&self, s: u32, r: i64, c: i64) -> String {
        self.m.get_style_for_cell(s, idx64(r), idx64(c)).map(|st| st.num_fmt).unwrap_or_default()
    }

    pub fn is_date(&self, s: u32, r: i64, c: i64) -> bool {
        is_date_format(&self.num_fmt(s, r, c))
    }

    /// A number value (0 for anything else, like the Python's `isinstance` checks).
    pub fn number(&self, s: u32, r: i64, c: i64) -> f64 {
        match self.value(s, r, c) {
            CellValue::Number(n) => n,
            _ => 0.0,
        }
    }

    /// `str(value)` of the Python: numbers as floats (`5.0`), text, `True`/`False`, `None`.
    pub fn value_str(&self, s: u32, r: i64, c: i64) -> String {
        match self.value(s, r, c) {
            CellValue::None => "None".into(),
            CellValue::String(t) => t,
            CellValue::Number(n) => crate::text::float_repr(n),
            CellValue::Boolean(b) => if b { "True" } else { "False" }.into(),
        }
    }

    /// A cell's value for the model: raw numbers (dates as shown), errors as codes.
    pub fn model_text(&self, s: u32, r: i64, c: i64, kind: Option<Kind>) -> String {
        let kind = kind.unwrap_or_else(|| self.kind(s, r, c));
        match (kind, self.value(s, r, c)) {
            (Kind::N, _) if self.is_date(s, r, c) => self.text(s, r, c),
            (Kind::N | Kind::B, CellValue::Number(n)) => num_text(n),
            (Kind::N | Kind::B, CellValue::Boolean(b)) => bool_text(b),
            (_, CellValue::None) => String::new(),
            (_, CellValue::String(t)) => t,
            (_, CellValue::Number(n)) => crate::text::float_repr(n),
            (_, CellValue::Boolean(b)) => if b { "True" } else { "False" }.into(),
        }
    }

    /// Write one value: text goes through `set_user_input` (`=…` formula, `12%`, `1,200`),
    /// numbers and bools as they are, `""` or null clears.
    pub fn put(&mut self, s: u32, r: i64, c: i64, v: &Value) -> Result<(), SheetError> {
        let (row, col) = (idx64(r), idx64(c));
        match v {
            Value::Null => self.clear(s, r, c, r, c),
            Value::String(t) if t.is_empty() => self.clear(s, r, c, r, c),
            Value::Bool(b) => self.m.update_cell_with_bool(s, row, col, *b).map_err(engine),
            Value::Number(n) => self.m.update_cell_with_number(s, row, col, n.as_f64().unwrap_or(0.0)).map_err(engine),
            Value::String(t) => self.m.set_user_input(s, row, col, t.clone()).map_err(engine),
            other => self.m.set_user_input(s, row, col, crate::text::pystr(other)).map_err(engine),
        }
    }

    pub fn put_text(&mut self, s: u32, r: i64, c: i64, v: &str) -> Result<(), SheetError> {
        if v.is_empty() {
            return self.clear(s, r, c, r, c);
        }
        self.m.set_user_input(s, idx64(r), idx64(c), v.to_owned()).map_err(engine)
    }

    pub fn clear(&mut self, s: u32, r1: i64, c1: i64, r2: i64, c2: i64) -> Result<(), SheetError> {
        let (ra, rb) = (r1.min(r2), r1.max(r2));
        let (ca, cb) = (c1.min(c2), c1.max(c2));
        let area = Area { sheet: s, row: idx64(ra), column: idx64(ca), width: idx64(cb - ca + 1), height: idx64(rb - ra + 1) };
        self.m.range_clear_contents(&area).map_err(engine)
    }

    // -- structure
    pub fn add_sheet(&mut self, name: &str) -> Result<(), SheetError> {
        self.m.add_sheet(name).map_err(engine)?;
        self.touch();
        Ok(())
    }

    pub fn rename_sheet(&mut self, s: u32, name: &str) -> Result<(), SheetError> {
        self.m.rename_sheet_by_index(s, name).map_err(engine)?;
        self.touch();
        Ok(())
    }

    pub fn delete_sheet(&mut self, s: u32) -> Result<(), SheetError> {
        self.m.delete_sheet(s).map_err(engine)?;
        self.touch();
        Ok(())
    }

    pub fn insert_rows(&mut self, s: u32, at: i64, n: i64) -> Result<(), SheetError> {
        self.m.insert_rows(s, idx64(at), idx64(n)).map_err(engine)?;
        self.touch();
        Ok(())
    }

    pub fn delete_rows(&mut self, s: u32, at: i64, n: i64) -> Result<(), SheetError> {
        self.m.delete_rows(s, idx64(at), idx64(n)).map_err(engine)?;
        self.touch();
        Ok(())
    }

    pub fn insert_cols(&mut self, s: u32, at: i64, n: i64) -> Result<(), SheetError> {
        self.m.insert_columns(s, idx64(at), idx64(n)).map_err(engine)?;
        self.touch();
        Ok(())
    }

    pub fn delete_cols(&mut self, s: u32, at: i64, n: i64) -> Result<(), SheetError> {
        self.m.delete_columns(s, idx64(at), idx64(n)).map_err(engine)?;
        self.touch();
        Ok(())
    }
}

fn idx(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

pub fn idx64(n: i64) -> i32 {
    i32::try_from(n).unwrap_or(if n < 0 { i32::MIN } else { i32::MAX })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_like_python() {
        assert_eq!(parse_csv("a,b\n1,\"x,\"\"y\"\"\"\n\nz", ','), vec![vec!["a", "b"], vec!["1", "x,\"y\""], vec![], vec!["z"]]);
        assert_eq!(parse_csv("a;b\r\n1;2\r\n", ';'), vec![vec!["a", "b"], vec!["1", "2"]]);
        assert_eq!(sniff("a;b;c\n1;2;3\n4;5;6\n"), Some(';'));
        assert_eq!(sniff("a,b\n1,2\n"), Some(','));
        assert_eq!(sniff("name\nx\n"), None);
        assert!(is_date_format("yyyy-mm-dd") && !is_date_format("#,##0.00") && !is_date_format("\"days\"0") && !is_date_format("[Red]0"));
        assert_eq!(num_text(12.0), "12");
        assert_eq!(num_text(0.125), "0.125");
    }
}
