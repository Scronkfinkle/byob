//! Charts yah draws itself: `ChartSpec` v1, the same JSON hob saves as `<title>.chart.json`,
//! so chart files move between the two apps.
//!
//! [`validate`] ports hob's `validateChart` (`app/src/shared/chart.ts`): it coerces
//! numbers written as text ("1,234.5", "€12", "40%"), puts timelines in date order, caps
//! rows and series, and fails with one plain sentence the model can act on.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Most rows one chart may hold.
pub const MAX_ROWS: usize = 2000;
/// Most series (number columns) one chart may hold.
pub const MAX_SERIES: usize = 8;
/// Longest title kept, in characters.
pub const MAX_TITLE_CHARS: usize = 120;
/// Longest unit kept, in characters.
pub const MAX_UNIT_CHARS: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChartType {
    Bar,
    Line,
    Area,
    Pie,
    Scatter,
}

impl ChartType {
    pub const ALL: [ChartType; 5] = [ChartType::Bar, ChartType::Line, ChartType::Area, ChartType::Pie, ChartType::Scatter];

    pub fn name(self) -> &'static str {
        match self {
            ChartType::Bar => "bar",
            ChartType::Line => "line",
            ChartType::Area => "area",
            ChartType::Pie => "pie",
            ChartType::Scatter => "scatter",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.name() == s)
    }
}

/// One number column and how the legend names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChartSeries {
    pub key: String,
    pub label: String,
}

/// A validated chart. Build it with [`validate`]; a spec read from disk or the wire should
/// be passed through [`validate`] again (see [`ChartSpec::from_json`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChartSpec {
    /// Always 1.
    pub version: u32,
    #[serde(rename = "type")]
    pub kind: ChartType,
    pub title: String,
    /// The column along the bottom (pie: the slice names).
    pub x: String,
    pub series: Vec<ChartSeries>,
    /// Rows: `x` holds a string or number (scatter: a number or null); each series key a
    /// number or null.
    pub data: Vec<Map<String, Value>>,
    /// Shown with numbers, e.g. "€" or "kg".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// Bars or areas on top of each other.
    #[serde(default, skip_serializing_if = "is_false")]
    pub stacked: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl ChartSpec {
    /// Parse and re-validate an untrusted `.chart.json` (from disk, hob, or a peer).
    pub fn from_json(text: &str) -> Result<Self, String> {
        let v: Value = serde_json::from_str(text).map_err(|e| format!("not a chart file: {e}"))?;
        validate(&v)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// A number cell (`None` for null, missing or non-numbers).
    pub fn number(row: &Map<String, Value>, key: &str) -> Option<f64> {
        row.get(key).and_then(Value::as_f64).filter(|n| n.is_finite())
    }

    /// The `x` cell of a row as text, for labels.
    pub fn label(&self, row: &Map<String, Value>) -> String {
        match row.get(&self.x) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => String::new(),
            Some(v) => js_string(v),
        }
    }
}

/// `String(v)` as JavaScript would write it, for the values the model sends.
fn js_string(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => match n.as_f64() {
            Some(f) if f.fract() == 0.0 && f.abs() < 1e15 => format!("{f:.0}"),
            _ => n.to_string(),
        },
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().map(|x| if x.is_null() { String::new() } else { js_string(x) }).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

/// A chart cell that should hold a number but doesn't.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotANumber;

/// A cell as a number: `Ok(None)` for empty, `Err` for something that isn't a number.
/// "1,234.5" → 1234.5; currency, percent signs and spaces are dropped.
pub fn to_number(v: Option<&Value>) -> Result<Option<f64>, NotANumber> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.is_empty() => Ok(None),
        Some(Value::Number(n)) => n.as_f64().filter(|f| f.is_finite()).map(Some).ok_or(NotANumber),
        Some(Value::String(s)) => parse_number(s).map(Some).ok_or(NotANumber),
        Some(_) => Err(NotANumber),
    }
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// JavaScript `Number(...)` after hob's clean-up: thousands separators (a comma followed by
/// exactly three digits) and `%€$£¥` and whitespace removed.
fn parse_number(s: &str) -> Option<f64> {
    let chars: Vec<char> = s.trim().chars().collect();
    let mut cleaned = String::with_capacity(chars.len());
    for (i, &c) in chars.iter().enumerate() {
        if c == ',' {
            let three = (1..=3).all(|k| chars.get(i + k).is_some_and(char::is_ascii_digit));
            let boundary = !chars.get(i + 4).is_some_and(|&n| is_word(n));
            if three && boundary {
                continue;
            }
        }
        if matches!(c, '%' | '€' | '$' | '£' | '¥') || c.is_whitespace() {
            continue;
        }
        cleaned.push(c);
    }
    if cleaned.is_empty() {
        // Number("") is 0 in JavaScript.
        return Some(0.0);
    }
    // Rust accepts "inf"/"nan" spellings JavaScript doesn't; neither is finite anyway.
    let lower = cleaned.to_ascii_lowercase();
    if lower.contains("inf") || lower.contains("nan") {
        return None;
    }
    cleaned.parse::<f64>().ok().filter(|f| f.is_finite())
}

const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];

fn month_of(word: &str) -> Option<u32> {
    let w = word.trim_end_matches('.').to_ascii_lowercase();
    if w.len() < 3 {
        return None;
    }
    let pos = MONTHS.iter().position(|m| w.starts_with(m))?;
    // Accept "mar", "march", "sept"; refuse e.g. "marble".
    const FULL: [&str; 12] = ["january", "february", "march", "april", "may", "june", "july", "august", "september", "october", "november", "december"];
    let full = FULL.get(pos)?;
    (full.starts_with(&w) || w == "sept").then(|| u32::try_from(pos).unwrap_or(0) + 1)
}

/// Days since 1970-01-01 (proleptic Gregorian).
fn days_from_civil(y: i64, m: u32, d: u32) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let dim = [31, if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if d > *dim.get(usize::try_from(m - 1).ok()?)? {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

/// Seconds into the day for "HH:MM[:SS]" (after a `T` or a space), if present.
fn time_of(rest: &str) -> Option<i64> {
    let rest = rest.trim().trim_end_matches('Z');
    if rest.is_empty() {
        return Some(0);
    }
    let main = rest.split(['+', 'Z', ' ']).next().unwrap_or_default();
    let mut parts = main.split(':');
    let h: i64 = parts.next()?.trim().parse().ok()?;
    let m: i64 = parts.next().map_or(Some(0), |p| p.trim().parse().ok())?;
    let s: f64 = parts.next().map_or(Some(0.0), |p| p.trim().parse().ok())?;
    if !(0..24).contains(&h) || !(0..60).contains(&m) || !(0.0..61.0).contains(&s) {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)] // checked range above
    Some(h * 3600 + m * 60 + s as i64)
}

/// A sortable time (seconds since 1970) for strings that look like dates — "2026-03-05",
/// "3/5/2026", "Mar 5, 2026", "5 March 2026", "March 2026", "2026-03". Bare names ("Mar",
/// "North") and plain numbers are not dates, as in hob.
pub fn date_of(v: &Value) -> Option<i64> {
    let s = v.as_str()?.trim();
    if !s.chars().any(|c| c.is_ascii_digit()) || !s.chars().any(|c| matches!(c, '-' | '/' | '.') || c.is_whitespace()) {
        return None;
    }
    let (date, time) = match s.find(['T', ' ']) {
        Some(i) if s.as_bytes().first().is_some_and(u8::is_ascii_digit) && s.get(..i).is_some_and(|d| d.contains(['-', '/'])) => {
            (s.get(..i).unwrap_or(s), s.get(i + 1..).unwrap_or(""))
        }
        _ => (s, ""),
    };
    let secs = time_of(time)?;
    let day = numeric_date(date).or_else(|| worded_date(s))?;
    day.checked_mul(86_400)?.checked_add(secs)
}

/// "2026-03-05", "2026/3/5", "2026-03", "3/5/2026" (US month first, like JavaScript).
fn numeric_date(s: &str) -> Option<i64> {
    let sep = if s.contains('-') {
        '-'
    } else if s.contains('/') {
        '/'
    } else {
        return None;
    };
    let parts: Vec<&str> = s.split(sep).collect();
    if parts.iter().any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()) || p.len() > 4) {
        return None;
    }
    let n = |i: usize| parts.get(i).and_then(|p| p.parse::<i64>().ok());
    match parts.as_slice() {
        [y, _, _] if y.len() == 4 => days_from_civil(n(0)?, u32::try_from(n(1)?).ok()?, u32::try_from(n(2)?).ok()?),
        [y, _] if y.len() == 4 => days_from_civil(n(0)?, u32::try_from(n(1)?).ok()?, 1),
        [_, _, y] if sep == '/' && (y.len() == 4 || y.len() == 2) => {
            let year = if y.len() == 2 { 2000 + n(2)? } else { n(2)? };
            days_from_civil(year, u32::try_from(n(0)?).ok()?, u32::try_from(n(1)?).ok()?)
        }
        _ => None,
    }
}

/// "Mar 5, 2026", "5 March 2026", "March 2026", "Tue, 5 Mar 2026".
fn worded_date(s: &str) -> Option<i64> {
    let words: Vec<&str> = s.split(|c: char| c.is_whitespace() || c == ',' || c == '-' || c == '/').filter(|w| !w.is_empty()).collect();
    let mut month = None;
    let mut year = None;
    let mut day = None;
    for w in &words {
        if let Some(m) = month_of(w) {
            if month.is_some() {
                return None;
            }
            month = Some(m);
        } else if let Ok(n) = w.trim_end_matches(['.', 's', 't', 'n', 'd', 'r', 'h']).parse::<i64>() {
            if w.len() == 4 && year.is_none() {
                year = Some(n);
            } else if day.is_none() && (1..=31).contains(&n) {
                day = Some(n);
            } else {
                return None;
            }
        } else if w.len() >= 3 && ["mon", "tue", "wed", "thu", "fri", "sat", "sun"].iter().any(|d| w.to_ascii_lowercase().starts_with(d)) {
            // weekday names are ignored
        } else {
            return None;
        }
    }
    days_from_civil(year?, month?, u32::try_from(day.unwrap_or(1)).ok()?)
}

fn columns(row: Option<&Map<String, Value>>) -> String {
    row.map(|r| r.keys().cloned().collect::<Vec<_>>().join(", ")).unwrap_or_default()
}

fn fail(why: impl std::fmt::Display) -> String {
    format!("Can't draw this chart: {why}")
}

fn series_of(v: Option<&Value>) -> Vec<ChartSeries> {
    let raw: Vec<Value> = match v {
        Some(Value::Array(a)) => a.clone(),
        Some(Value::String(s)) => vec![Value::String(s.clone())],
        _ => Vec::new(),
    };
    raw.iter()
        .map(|s| match s {
            Value::String(k) => ChartSeries { key: k.clone(), label: k.clone() },
            Value::Object(o) => {
                let key = o.get("key").filter(|k| !k.is_null()).map(js_string).unwrap_or_default();
                let label = o.get("label").filter(|l| !l.is_null()).map(js_string).unwrap_or_else(|| key.clone());
                ChartSeries { key, label }
            }
            _ => ChartSeries { key: String::new(), label: String::new() },
        })
        .collect()
}

/// Check a chart the model (or a file) asked for; the error says plainly what to fix.
pub fn validate(input: &Value) -> Result<ChartSpec, String> {
    let empty = Map::new();
    let o = input.as_object().unwrap_or(&empty);
    let type_name = o.get("type").filter(|t| !t.is_null()).map(js_string).unwrap_or_default();
    let kind = ChartType::from_name(&type_name).ok_or_else(|| fail(format!("type must be one of {}.", ChartType::ALL.map(ChartType::name).join(", "))))?;
    let data: Vec<&Map<String, Value>> = match o.get("data") {
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_object).collect(),
        _ => Vec::new(),
    };
    if data.is_empty() {
        return Err(fail(r#"there is no data. Give data as a list of rows, like [{"month": "Jan", "sales": 10}]."#));
    }
    if data.len() > MAX_ROWS {
        return Err(fail(format!("{} rows is too many; at most {MAX_ROWS}. Group or sum the data first.", data.len())));
    }
    let first = data.first().copied();
    let x = o.get("x").filter(|x| !x.is_null()).map(js_string).unwrap_or_default();
    if x.is_empty() || !data.iter().any(|r| r.contains_key(&x)) {
        return Err(fail(format!("the rows have no column \"{x}\" for x. Columns: {}.", columns(first))));
    }
    let series = series_of(o.get("series"));
    if series.is_empty() {
        return Err(fail("give at least one series: the column(s) with the numbers."));
    }
    if series.len() > MAX_SERIES {
        return Err(fail(format!("at most {MAX_SERIES} series.")));
    }
    if kind == ChartType::Pie && series.len() != 1 {
        return Err(fail("a pie chart shows one series."));
    }
    for s in &series {
        if !data.iter().any(|r| r.contains_key(&s.key)) {
            return Err(fail(format!("the rows have no column \"{}\". Columns: {}.", s.key, columns(first))));
        }
    }
    let mut out: Vec<Map<String, Value>> = Vec::with_capacity(data.len());
    for r in &data {
        let mut row = Map::new();
        let xv = r.get(&x);
        if kind == ChartType::Scatter {
            let n = to_number(xv)
                .map_err(|NotANumber| fail(format!("a scatter chart needs numbers in \"{x}\"; found \"{}\".", xv.map(js_string).unwrap_or_default())))?;
            row.insert(x.clone(), num(n));
        } else {
            let v = match xv {
                None | Some(Value::Null) => Value::String(String::new()),
                Some(n @ Value::Number(_)) => n.clone(),
                Some(other) => Value::String(js_string(other)),
            };
            row.insert(x.clone(), v);
        }
        for s in &series {
            let cell = r.get(&s.key);
            let n = to_number(cell).map_err(|NotANumber| {
                fail(format!("\"{}\" must hold numbers; found \"{}\".", s.key, cell.map(js_string).unwrap_or_else(|| "undefined".into())))
            })?;
            row.insert(s.key.clone(), num(n));
        }
        out.push(row);
    }
    // A timeline in the wrong order draws a zig-zag; put dates in order (pie slices aren't one).
    if kind != ChartType::Pie && kind != ChartType::Scatter {
        let times: Option<Vec<i64>> = out.iter().map(|r| r.get(&x).and_then(date_of)).collect();
        if let Some(times) = times {
            let mut order: Vec<(i64, Map<String, Value>)> = times.into_iter().zip(out).collect();
            order.sort_by_key(|(t, _)| *t);
            out = order.into_iter().map(|(_, r)| r).collect();
        }
    }
    let title = o.get("title").filter(|t| !t.is_null()).map(js_string).unwrap_or_default();
    let mut title: String = title.trim().chars().take(MAX_TITLE_CHARS).collect();
    if title.is_empty() {
        title = series.iter().map(|s| s.label.as_str()).collect::<Vec<_>>().join(" and ");
    }
    let unit = o.get("unit").and_then(Value::as_str).map(str::trim).filter(|u| !u.is_empty()).map(|u| u.chars().take(MAX_UNIT_CHARS).collect());
    let stacked = o.get("stacked") == Some(&Value::Bool(true));
    Ok(ChartSpec { version: 1, kind, title, x, series, data: out, unit, stacked })
}

fn num(n: Option<f64>) -> Value {
    n.and_then(serde_json::Number::from_f64).map_or(Value::Null, Value::Number)
}

/// Plain CSV as hob reads it: a header row, commas, `"quoted, values"` with `""` escapes.
/// Blank lines are skipped; missing cells are empty.
pub fn parse_csv(text: &str) -> Vec<Map<String, Value>> {
    let rows = csv_rows(text);
    let Some((head, body)) = rows.split_first() else { return Vec::new() };
    body.iter().map(|r| head.iter().enumerate().map(|(i, h)| (h.trim().to_owned(), Value::String(r.get(i).cloned().unwrap_or_default()))).collect()).collect()
}

/// CSV records (hob's tokenizer), header included.
pub fn csv_rows(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut cell = String::new();
    let mut quoted = false;
    let mut it = text.chars().peekable();
    while let Some(c) = it.next() {
        if quoted {
            if c == '"' && it.peek() == Some(&'"') {
                cell.push('"');
                it.next();
            } else if c == '"' {
                quoted = false;
            } else {
                cell.push(c);
            }
        } else if c == '"' {
            quoted = true;
        } else if c == ',' {
            row.push(std::mem::take(&mut cell));
        } else if c == '\n' || c == '\r' {
            if c == '\r' && it.peek() == Some(&'\n') {
                it.next();
            }
            row.push(std::mem::take(&mut cell));
            if row.iter().any(|v| !v.is_empty()) {
                rows.push(std::mem::take(&mut row));
            } else {
                row.clear();
            }
        } else {
            cell.push(c);
        }
    }
    row.push(cell);
    if row.iter().any(|v| !v.is_empty()) {
        rows.push(row);
    }
    // A UTF-8 BOM would otherwise end up in the first column name.
    if let Some(first) = rows.first_mut().and_then(|r| r.first_mut())
        && let Some(stripped) = first.strip_prefix('\u{feff}')
    {
        *first = stripped.to_owned();
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rows() -> Value {
        json!([{ "month": "Jan", "sales": 10, "costs": "4" }, { "month": "Feb", "sales": 30, "costs": 8 }])
    }

    fn err(v: Value) -> String {
        validate(&v).unwrap_err()
    }

    #[test]
    fn a_good_chart_coerces_numbers_and_takes_plain_keys() {
        let spec =
            validate(&json!({"type": "bar", "title": "Sales", "x": "month", "series": ["sales", {"key": "costs", "label": "Costs"}], "data": rows()})).unwrap();
        assert_eq!(spec.kind, ChartType::Bar);
        assert_eq!(spec.title, "Sales");
        assert_eq!(spec.series, vec![ChartSeries { key: "sales".into(), label: "sales".into() }, ChartSeries { key: "costs".into(), label: "Costs".into() }]);
        assert_eq!(
            serde_json::to_value(&spec.data).unwrap(),
            json!([{ "month": "Jan", "sales": 10.0, "costs": 4.0 }, { "month": "Feb", "sales": 30.0, "costs": 8.0 }])
        );
        // Same JSON as hob: version 1, "type", no unit/stacked when unset.
        let v: Value = serde_json::from_str(&spec.to_json()).unwrap();
        assert_eq!(v["version"], 1);
        assert_eq!(v["type"], "bar");
        assert!(v.get("unit").is_none() && v.get("stacked").is_none());
        assert_eq!(ChartSpec::from_json(&spec.to_json()).unwrap(), spec);
    }

    #[test]
    fn says_plainly_what_is_wrong() {
        assert!(err(json!({"type": "radar", "x": "month", "series": ["sales"], "data": rows()})).contains("type must be one of bar, line, area, pie, scatter"));
        assert!(err(json!({"type": "bar", "x": "month", "series": ["sales"], "data": []})).contains("no data"));
        assert!(err(json!({"type": "bar", "x": "week", "series": ["sales"], "data": rows()})).contains("\"week\""));
        assert!(err(json!({"type": "bar", "x": "month", "series": ["profit"], "data": rows()})).contains("\"profit\""));
        assert!(err(json!({"type": "bar", "x": "month", "series": ["month"], "data": rows()})).contains("numbers"));
        assert!(err(json!({"type": "pie", "x": "month", "series": ["sales", "costs"], "data": rows()})).contains("one series"));
        let nine: Vec<String> = (0..9).map(|i| format!("s{i}")).collect();
        assert!(err(json!({"type": "bar", "x": "month", "series": nine, "data": rows()})).contains("at most 8"));
        let many: Vec<Value> = (0..2001).map(|i| json!({"i": i, "v": i})).collect();
        assert!(err(json!({"type": "bar", "x": "i", "series": ["v"], "data": many})).contains("2000"));
        assert!(err(json!({"type": "bar", "x": "month", "series": [], "data": rows()})).contains("at least one series"));
        assert!(err(json!({"type": "scatter", "x": "month", "series": ["sales"], "data": rows()})).contains("scatter chart needs numbers in \"month\""));
        assert!(err(json!("garbage")).starts_with("Can't draw this chart"));
        assert!(err(json!({"type": "bar", "x": "week", "series": ["sales"], "data": rows()})).contains("Columns: costs, month, sales"));
    }

    #[test]
    fn number_coercion_matches_hob() {
        let n = |s: &str| to_number(Some(&json!(s)));
        assert_eq!(n("1,234.5"), Ok(Some(1234.5)));
        assert_eq!(n("€1,234"), Ok(Some(1234.0)));
        assert_eq!(n(" 40 % "), Ok(Some(40.0)));
        assert_eq!(n("$ 1 000"), Ok(Some(1000.0)));
        assert_eq!(n("1,23"), Err(NotANumber));
        assert_eq!(n("1,2345"), Err(NotANumber));
        assert_eq!(n("-3.5e2"), Ok(Some(-350.0)));
        assert_eq!(n(""), Ok(None));
        assert_eq!(n("€"), Ok(Some(0.0)));
        assert_eq!(n("abc"), Err(NotANumber));
        assert_eq!(n("Infinity"), Err(NotANumber));
        assert_eq!(n("NaN"), Err(NotANumber));
        assert_eq!(to_number(None), Ok(None));
        assert_eq!(to_number(Some(&Value::Null)), Ok(None));
        assert_eq!(to_number(Some(&json!(true))), Err(NotANumber));
        assert_eq!(to_number(Some(&json!([1]))), Err(NotANumber));
        // nulls are kept as gaps
        let spec = validate(&json!({"type": "line", "x": "m", "series": ["v"], "data": [{"m": "a", "v": null}, {"m": "b", "v": ""}, {"m": "c"}]})).unwrap();
        assert!(spec.data.iter().all(|r| r["v"].is_null()));
    }

    #[test]
    fn dates_along_the_bottom_are_sorted() {
        let sorted = |data: Value, kind: &str| -> Vec<Value> {
            validate(&json!({"type": kind, "x": "d", "series": ["v"], "data": data})).unwrap().data.into_iter().map(|r| r["d"].clone()).collect()
        };
        assert_eq!(
            sorted(json!([{"d": "5/18/2026", "v": 1}, {"d": "5/6/2026", "v": 2}, {"d": "4/29/2026", "v": 3}]), "line"),
            vec![json!("4/29/2026"), json!("5/6/2026"), json!("5/18/2026")]
        );
        assert_eq!(sorted(json!([{"d": "2026-03-02", "v": 1}, {"d": "2026-02-27", "v": 2}]), "bar"), vec![json!("2026-02-27"), json!("2026-03-02")]);
        assert_eq!(sorted(json!([{"d": "Mar", "v": 1}, {"d": "Jan", "v": 2}]), "line"), vec![json!("Mar"), json!("Jan")]);
        assert_eq!(sorted(json!([{"d": "North", "v": 1}, {"d": "2026-01-01", "v": 2}]), "line"), vec![json!("North"), json!("2026-01-01")]);
        assert_eq!(sorted(json!([{"d": "2026-03-02", "v": 1}, {"d": "2026-02-27", "v": 2}]), "pie"), vec![json!("2026-03-02"), json!("2026-02-27")]);
        assert_eq!(
            sorted(json!([{"d": "Mar 2026", "v": 1}, {"d": "Jan 5, 2026", "v": 2}, {"d": "2025-12", "v": 3}]), "bar"),
            vec![json!("2025-12"), json!("Jan 5, 2026"), json!("Mar 2026")]
        );
    }

    #[test]
    fn date_parsing() {
        let d = |s: &str| date_of(&json!(s));
        assert_eq!(d("1970-01-02"), Some(86_400));
        assert_eq!(d("1970-01-01T01:00:00Z"), Some(3600));
        assert_eq!(d("1970-01-01 00:01"), Some(60));
        assert_eq!(d("2026-02-30"), None);
        assert_eq!(d("5 March 2026"), d("2026-03-05"));
        assert_eq!(d("Thu, 5 Mar 2026"), d("2026-03-05"));
        assert_eq!(d("3/5/26"), d("2026-03-05"));
        assert_eq!(d("Mar"), None);
        assert_eq!(d("2026"), None);
        assert_eq!(d("North 5"), None);
        assert_eq!(d("Marble 5 2026"), None);
        assert_eq!(date_of(&json!(5)), None);
    }

    #[test]
    fn title_unit_and_stacked() {
        let spec =
            validate(&json!({"type": "area", "x": "month", "series": ["sales", "costs"], "data": rows(), "unit": "  € ", "stacked": true, "title": "  "}))
                .unwrap();
        assert_eq!(spec.title, "sales and costs");
        assert_eq!(spec.unit.as_deref(), Some("€"));
        assert!(spec.stacked);
        let long = validate(
            &json!({"type": "bar", "x": "month", "series": "sales", "data": rows(), "title": "t".repeat(500), "unit": "u".repeat(50), "stacked": "yes"}),
        )
        .unwrap();
        assert_eq!(long.title.chars().count(), MAX_TITLE_CHARS);
        assert_eq!(long.unit.map(|u| u.chars().count()), Some(MAX_UNIT_CHARS));
        assert!(!long.stacked);
    }

    #[test]
    fn x_values_become_strings_or_stay_numbers() {
        let spec =
            validate(&json!({"type": "bar", "x": "k", "series": ["v"], "data": [{"k": 2024, "v": 1}, {"k": true, "v": 2}, {"k": null, "v": 3}, {"v": 4}]}))
                .unwrap();
        let xs: Vec<Value> = spec.data.iter().map(|r| r["k"].clone()).collect();
        assert_eq!(xs, vec![json!(2024), json!("true"), json!(""), json!("")]);
        assert_eq!(spec.label(&spec.data[0]), "2024");
    }

    #[test]
    fn reads_simple_csv() {
        let rows = parse_csv("month,sales\nJan,10\n\"Feb, late\",30\n");
        assert_eq!(serde_json::to_value(rows).unwrap(), json!([{"month": "Jan", "sales": "10"}, {"month": "Feb, late", "sales": "30"}]));
        let rows = parse_csv("\u{feff}a,b\r\n\r\n1,\"x \"\"y\"\"\"\r\n2\r\n");
        assert_eq!(serde_json::to_value(rows).unwrap(), json!([{"a": "1", "b": "x \"y\""}, {"a": "2", "b": ""}]));
        assert!(parse_csv("").is_empty());
        assert!(parse_csv("only,header").is_empty());
    }

    #[test]
    fn hostile_input_never_panics() {
        for v in [json!(null), json!([]), json!({"type": "bar", "data": "x"}), json!({"type": "bar", "x": {}, "series": [1, null, {}], "data": [1, "a", {}]})] {
            assert!(validate(&v).is_err());
        }
    }
}
