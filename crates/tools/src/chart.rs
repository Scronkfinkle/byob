//! `make_chart`: draw a chart in the conversation. Validates a hob-compatible
//! [`ChartSpec`] and saves it as `charts/<title>.chart.json` in the workspace; the agent
//! shows every `.chart.json` a tool creates inline, in every client.
//!
//! Data comes inline (`data`: rows) or from a CSV/JSON file in the workspace (`file`), which
//! the tool can group itself (`agg`: sum/mean/count by `x`, optionally bucketing dates by
//! day/month/year). That makes "chart this CSV" work without any code execution.
//!
//! Access: [`Access::Read`]. Like hob, a chart is treated as harmless: the tool only reads
//! inside the workspace and only ever *creates* a new, uniquely named file under `charts/`
//! (`create_new`, never through links); it can't overwrite, move or delete anything. Asking
//! for approval on every chart would train users to click "Allow" blindly.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{ErrorKind, Read as _, Write as _};
use std::path::{Path, PathBuf};

use byob_types::chart::{self, ChartSpec, ChartType};
use serde_json::{Map, Value, json};

use crate::sandbox;
use crate::{Access, FileChange, Tool, ToolContext, ToolOutput, ToolSpec};

/// Folder (in the workspace) charts are saved in.
pub const CHART_DIR: &str = "charts";
/// Largest data file `make_chart` reads.
pub const MAX_DATA_FILE: u64 = 20 * 1024 * 1024;
/// Rows read from a data file before grouping.
pub const MAX_FILE_ROWS: usize = 200_000;

pub struct MakeChart;

impl Tool for MakeChart {
    fn spec(&self) -> ToolSpec {
        let types: Vec<&str> = ChartType::ALL.iter().map(|t| t.name()).collect();
        ToolSpec {
            name: "make_chart".into(),
            description: format!(
                "Draw a clean, interactive chart in the chat. Types: {}. Give rows of data and which column goes along the bottom (x) and which hold the numbers (series). \
                 For a CSV or JSON file in the workspace, pass `file` instead of `data`: rows with the same x are summed (or use agg: mean/count), dates can be grouped with bucket: month, and split_by makes one series per value of a column. \
                 Prefer this over adding numbers up yourself.",
                types.join(", ")
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "type": {"type": "string", "enum": types},
                    "title": {"type": "string"},
                    "x": {"type": "string", "description": "Column along the bottom (pie: the slice names)"},
                    "series": {"type": "array", "items": {"type": "string"}, "description": "Column(s) with the numbers"},
                    "data": {"type": "array", "items": {"type": "object"}, "description": "Rows, like [{\"month\": \"Jan\", \"sales\": 10}]"},
                    "file": {"type": "string", "description": "Or: a CSV or JSON file in the workspace with the rows"},
                    "agg": {"type": "string", "enum": ["sum", "mean", "count", "none"], "description": "With file: how rows with the same x are combined (default sum when x repeats)"},
                    "bucket": {"type": "string", "enum": ["day", "month", "year"], "description": "With file: group a date column x by day, month or year"},
                    "split_by": {"type": "string", "description": "With file: a column whose values each become a series (e.g. one line per region); give one series column"},
                    "unit": {"type": "string", "description": "Optional, e.g. \"€\" or \"kg\""},
                    "stacked": {"type": "boolean", "description": "Bars or areas on top of each other"}
                },
                "required": ["type", "x", "series"]
            }),
        }
    }

    fn title(&self, args: &Value) -> String {
        let name = args.get("title").and_then(Value::as_str).or_else(|| args.get("type").and_then(Value::as_str)).unwrap_or("chart");
        format!("Made a chart: {}", clip(name, 60))
    }

    fn access(&self, _args: &Value) -> Access {
        Access::Read
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        match make(args, ctx.workspace) {
            Ok(out) => out,
            Err(e) => ToolOutput::err(e),
        }
    }
}

fn clip(s: &str, max: usize) -> String {
    let clean: String = s.chars().filter(|c| !c.is_control()).collect();
    let mut out: String = clean.chars().take(max).collect();
    if clean.chars().count() > max {
        out.push('…');
    }
    out
}

fn make(args: &Value, workspace: &Path) -> Result<ToolOutput, String> {
    let mut input = args.as_object().cloned().unwrap_or_default();
    let file = ["file", "data_file"].iter().find_map(|k| input.get(*k).and_then(Value::as_str).map(str::to_owned)).filter(|f| !f.trim().is_empty());
    let mut note = String::new();
    let has_data = input.get("data").is_some_and(|d| !d.is_null());
    if let (Some(file), false) = (&file, has_data) {
        let mut rows = rows_from_file(workspace, file)?;
        if let Some(split) = input.get("split_by").and_then(Value::as_str).map(str::to_owned).filter(|s| !s.trim().is_empty()) {
            let (split_rows, keys) = split_by(rows, &split, &input)?;
            rows = split_rows;
            if input.get("agg").and_then(Value::as_str) == Some("count") {
                // each row now holds a 1 under its split value; counting is summing those
                input.insert("agg".into(), json!("sum"));
            }
            input.insert("series".into(), json!(keys));
        }
        let (rows, how) = group(rows, &input)?;
        note = how;
        input.insert("data".into(), Value::Array(rows.into_iter().map(Value::Object).collect()));
        if input.get("agg").and_then(Value::as_str) == Some("count") && series_keys(&input).is_empty() {
            input.insert("series".into(), json!(["count"]));
        }
    }
    let spec = chart::validate(&Value::Object(input))?;
    let saved = save(workspace, &spec)?;
    let rel = saved.strip_prefix(sandbox::root(workspace)?).map(Path::to_path_buf).unwrap_or_else(|_| saved.clone());
    let mut text = format!("Made the chart \"{}\" ({} rows, saved as {}).", spec.title, spec.data.len(), rel.display());
    if !note.is_empty() {
        text.push(' ');
        text.push_str(&note);
    }
    text.push_str(" It shows in the chat; don't describe it again, just give the main takeaway.");
    Ok(ToolOutput { text, failed: false, changes: vec![FileChange::Created(saved)] })
}

/// Rows from a CSV or JSON file inside the workspace.
fn rows_from_file(workspace: &Path, file: &str) -> Result<Vec<Map<String, Value>>, String> {
    let r = sandbox::resolve_follow(workspace, file).map_err(|e| format!("Couldn't use {file}: {e}"))?;
    let name = r.show();
    let meta = std::fs::metadata(&r.abs).map_err(|e| format!("Couldn't read {name}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("Couldn't read {name}: it is not a file."));
    }
    if meta.len() > MAX_DATA_FILE {
        return Err(format!(
            "{name} is too big to chart directly ({} MB; max {} MB). Sum or group it first.",
            meta.len() / 1_048_576,
            MAX_DATA_FILE / 1_048_576
        ));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&r.abs).and_then(|f| f.take(MAX_DATA_FILE).read_to_end(&mut bytes)).map_err(|e| format!("Couldn't read {name}: {e}"))?;
    let text = String::from_utf8_lossy(&bytes);
    let is_json = r.abs.extension().is_some_and(|e| e.eq_ignore_ascii_case("json"));
    let rows: Vec<Map<String, Value>> = if is_json {
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("Couldn't read {name} as JSON ({e}). Save the data as CSV or a JSON list of rows."))?;
        let list = match v {
            Value::Array(a) => a,
            Value::Object(mut o) => match o.remove("data") {
                Some(Value::Array(a)) => a,
                _ => return Err(format!("{name} isn't a list of rows. Save it as CSV or a JSON list like [{{\"month\": \"Jan\", \"sales\": 10}}].")),
            },
            _ => return Err(format!("{name} isn't a list of rows.")),
        };
        list.into_iter().filter_map(|r| if let Value::Object(o) = r { Some(o) } else { None }).collect()
    } else {
        chart::parse_csv(&text)
    };
    if rows.is_empty() {
        return Err(format!("{name} has no rows. It needs a header row and data rows."));
    }
    if rows.len() > MAX_FILE_ROWS {
        return Err(format!("{name} has {} rows; make_chart reads at most {MAX_FILE_ROWS}. Group it first.", rows.len()));
    }
    Ok(rows)
}

fn series_keys(input: &Map<String, Value>) -> Vec<String> {
    match input.get("series") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|s| match s {
                Value::String(k) => Some(k.clone()),
                Value::Object(o) => o.get("key").and_then(Value::as_str).map(str::to_owned),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Agg {
    None,
    Sum,
    Mean,
    Count,
}

#[derive(Clone, Copy, PartialEq)]
enum Bucket {
    Day,
    Month,
    Year,
}

/// Seconds since 1970 → "YYYY-MM-DD".
fn civil(secs: i64) -> (i64, u32, u32) {
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, u32::try_from(m).unwrap_or(1), u32::try_from(d).unwrap_or(1))
}

fn bucket_label(v: &Value, bucket: Bucket) -> Option<String> {
    let (y, m, d) = civil(chart::date_of(v)?);
    Some(match bucket {
        Bucket::Day => format!("{y:04}-{m:02}-{d:02}"),
        Bucket::Month => format!("{y:04}-{m:02}"),
        Bucket::Year => format!("{y:04}"),
    })
}

fn cell_text(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.trim().to_owned(),
        Some(other) => other.to_string(),
    }
}

/// One series per distinct value of column `split` (e.g. one line per region): each row's
/// number (the single series column; or 1 when counting) moves to a column named after its
/// split value. Returns the rows and the new series keys, in first-seen order.
fn split_by(rows: Vec<Map<String, Value>>, split: &str, input: &Map<String, Value>) -> Result<(Vec<Map<String, Value>>, Vec<String>), String> {
    let counting = input.get("agg").and_then(Value::as_str) == Some("count");
    let keys = series_keys(input);
    let value = match (keys.as_slice(), counting) {
        ([v], _) => Some(v.clone()),
        ([], true) => None,
        _ => {
            return Err(
                "Can't draw this chart: with split_by, give exactly one series (the number column); each value of the split column becomes its own series."
                    .into(),
            );
        }
    };
    if !rows.iter().any(|r| r.contains_key(split)) {
        let cols = rows.first().map(|r| r.keys().cloned().collect::<Vec<_>>().join(", ")).unwrap_or_default();
        return Err(format!("Can't draw this chart: the file has no column \"{split}\" to split by. Columns: {cols}."));
    }
    let mut names: Vec<String> = Vec::new();
    let mut out = Vec::with_capacity(rows.len());
    for mut r in rows {
        let mut name = cell_text(r.get(split));
        if name.is_empty() {
            name = "(blank)".into();
        }
        if !names.contains(&name) {
            if names.len() >= chart::MAX_SERIES {
                return Err(format!(
                    "Can't draw this chart: \"{split}\" has more than {} different values; at most {} series fit.",
                    chart::MAX_SERIES,
                    chart::MAX_SERIES
                ));
            }
            names.push(name.clone());
        }
        let cell = match &value {
            Some(v) => r.get(v).cloned().unwrap_or(Value::Null),
            None => json!(1),
        };
        let mut row = Map::new();
        if let Some(x) = input.get("x").and_then(Value::as_str).and_then(|x| r.remove(x).map(|v| (x.to_owned(), v))) {
            row.insert(x.0, x.1);
        }
        row.insert(name, cell);
        out.push(row);
    }
    Ok((out, names))
}

/// Group file rows by `x` (and date bucket) with sum/mean/count. Returns the rows and a
/// sentence for the model about what was done.
fn group(rows: Vec<Map<String, Value>>, input: &Map<String, Value>) -> Result<(Vec<Map<String, Value>>, String), String> {
    let x = input.get("x").and_then(Value::as_str).unwrap_or_default().to_owned();
    let keys = series_keys(input);
    let bucket = match input.get("bucket").and_then(Value::as_str) {
        None | Some("" | "none") => None,
        Some("day") => Some(Bucket::Day),
        Some("month") => Some(Bucket::Month),
        Some("year") => Some(Bucket::Year),
        Some(other) => return Err(format!("Can't draw this chart: bucket must be day, month or year, not \"{other}\".")),
    };
    let mut agg = match input.get("agg").and_then(Value::as_str) {
        None | Some("") => None,
        Some("none") => Some(Agg::None),
        Some("sum") => Some(Agg::Sum),
        Some("mean" | "avg" | "average") => Some(Agg::Mean),
        Some("count") => Some(Agg::Count),
        Some(other) => return Err(format!("Can't draw this chart: agg must be sum, mean, count or none, not \"{other}\".")),
    };
    if x.is_empty() || !rows.iter().any(|r| r.contains_key(&x)) {
        let cols = rows.first().map(|r| r.keys().cloned().collect::<Vec<_>>().join(", ")).unwrap_or_default();
        return Err(format!("Can't draw this chart: the file has no column \"{x}\" for x. Columns: {cols}."));
    }
    let label = |r: &Map<String, Value>| -> Result<String, String> {
        let v = r.get(&x).cloned().unwrap_or(Value::Null);
        match bucket {
            None => Ok(cell_text(Some(&v))),
            Some(b) => bucket_label(&v, b)
                .ok_or_else(|| format!("Can't draw this chart: \"{x}\" must hold dates to group by {}; found \"{}\".", bucket_name(b), cell_text(Some(&v)))),
        }
    };
    if agg.is_none() {
        let mut seen = std::collections::BTreeSet::new();
        let mut repeats = bucket.is_some();
        for r in &rows {
            if !seen.insert(label(r)?) {
                repeats = true;
                break;
            }
        }
        agg = Some(if repeats { Agg::Sum } else { Agg::None });
    }
    let agg = agg.unwrap_or(Agg::None);
    if agg == Agg::None {
        return Ok((rows, String::new()));
    }
    let count_only = agg == Agg::Count && keys.is_empty();
    // first-seen order of groups; per group, per series: (sum, n)
    let mut order: Vec<String> = Vec::new();
    let mut acc: BTreeMap<String, Vec<(f64, u64)>> = BTreeMap::new();
    let width = if count_only { 1 } else { keys.len() };
    for r in &rows {
        let g = label(r)?;
        let slot = acc.entry(g.clone()).or_insert_with(|| {
            order.push(g.clone());
            vec![(0.0, 0); width]
        });
        if count_only {
            if let Some(s) = slot.first_mut() {
                s.1 = s.1.saturating_add(1);
            }
            continue;
        }
        for (i, key) in keys.iter().enumerate() {
            let cell = r.get(key);
            let n = chart::to_number(cell)
                .map_err(|chart::NotANumber| format!("Can't draw this chart: \"{key}\" must hold numbers; found \"{}\".", cell_text(cell)))?;
            if let (Some(n), Some(s)) = (n, slot.get_mut(i)) {
                s.0 += n;
                s.1 = s.1.saturating_add(1);
            }
        }
    }
    let out: Vec<Map<String, Value>> = order
        .iter()
        .filter_map(|g| {
            let slots = acc.get(g)?;
            let mut row = Map::new();
            row.insert(x.clone(), Value::String(g.clone()));
            if count_only {
                row.insert("count".into(), json!(slots.first().map_or(0, |s| s.1)));
            }
            for (key, (sum, n)) in keys.iter().zip(slots).filter(|_| !count_only) {
                #[allow(clippy::cast_precision_loss)] // counts are far below 2^52
                let v = match agg {
                    Agg::Count => json!(n),
                    Agg::Mean if *n > 0 => serde_json::Number::from_f64(sum / *n as f64).map_or(Value::Null, Value::Number),
                    Agg::Mean => Value::Null,
                    _ => serde_json::Number::from_f64(*sum).map_or(Value::Null, Value::Number),
                };
                row.insert(key.clone(), v);
            }
            Some(row)
        })
        .collect();
    let verb = match agg {
        Agg::Sum => "Summed",
        Agg::Mean => "Averaged",
        _ => "Counted",
    };
    let by = bucket.map(|b| format!(" per {}", bucket_name(b))).unwrap_or_default();
    let what = if count_only { "rows".to_owned() } else { keys.join(", ") };
    Ok((out.clone(), format!("{verb} {what} by {x}{by}: {} file rows became {} points.", rows.len(), out.len())))
}

fn bucket_name(b: Bucket) -> &'static str {
    match b {
        Bucket::Day => "day",
        Bucket::Month => "month",
        Bucket::Year => "year",
    }
}

/// "Sales / month?" → "Sales month": a safe file name on every OS.
pub fn file_title(title: &str) -> String {
    let replaced: String =
        title.chars().map(|c| if matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control() { ' ' } else { c }).collect();
    let joined = replaced.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed: String = joined.trim_start_matches(['.', ' ']).chars().take(80).collect();
    let trimmed = trimmed.trim().to_owned();
    if trimmed.is_empty() { "Chart".into() } else { trimmed }
}

/// Write the spec under `charts/` with a name nothing else uses.
fn save(workspace: &Path, spec: &ChartSpec) -> Result<PathBuf, String> {
    let dir = sandbox::resolve(workspace, CHART_DIR)?;
    match std::fs::symlink_metadata(&dir.abs) {
        Ok(m) if m.file_type().is_symlink() => return Err(format!("{CHART_DIR} is a link; can't save charts there")),
        Ok(m) if !m.is_dir() => return Err(format!("{CHART_DIR} exists but is not a folder; can't save charts there")),
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => std::fs::create_dir_all(&dir.abs).map_err(|e| format!("can't create {CHART_DIR}: {e}"))?,
        Err(e) => return Err(format!("can't access {CHART_DIR}: {e}")),
    }
    let base = file_title(&spec.title);
    let json = spec.to_json();
    for n in 1..1000u32 {
        let name = if n == 1 { format!("{base}.chart.json") } else { format!("{base} ({n}).chart.json") };
        let path = dir.abs.join(&name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                f.write_all(json.as_bytes()).map_err(|e| format!("can't write {name}: {e}"))?;
                return Ok(path);
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("can't write {name}: {e}")),
        }
    }
    Err("too many charts with this title; pick another title".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::testutil::run;
    use std::fs;

    fn rows() -> Value {
        json!([{ "month": "Jan", "sales": 10, "costs": "4" }, { "month": "Feb", "sales": 30, "costs": 8 }])
    }

    #[test]
    fn saves_a_chart_with_a_unique_name() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().canonicalize().unwrap();
        let args = json!({"type": "line", "title": "Sales by month", "x": "month", "series": ["sales"], "data": rows()});
        let out = run(&MakeChart, d.path(), args.clone());
        assert!(!out.failed, "{}", out.text);
        let p = root.join("charts/Sales by month.chart.json");
        assert_eq!(out.changes, vec![FileChange::Created(p.clone())]);
        assert!(out.text.contains("charts/Sales by month.chart.json"));
        let spec = ChartSpec::from_json(&fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(spec.kind, ChartType::Line);
        let again = run(&MakeChart, d.path(), args);
        assert_eq!(again.changes, vec![FileChange::Created(root.join("charts/Sales by month (2).chart.json"))]);
        assert_eq!(fs::read_to_string(&p).unwrap(), spec.to_json(), "the first chart is never overwritten");
    }

    #[test]
    fn bad_charts_fail_with_the_reason_and_write_nothing() {
        let d = tempfile::tempdir().unwrap();
        let out = run(&MakeChart, d.path(), json!({"type": "bar", "title": "x", "x": "nope", "series": ["sales"], "data": rows()}));
        assert!(out.failed && out.text.contains("\"nope\""));
        assert!(!d.path().join("charts").exists() || fs::read_dir(d.path().join("charts")).unwrap().count() == 0);
    }

    #[test]
    fn titles_become_safe_file_names() {
        assert_eq!(file_title("Sales / month?"), "Sales month");
        assert_eq!(file_title("../../etc/passwd"), "etc passwd");
        assert_eq!(file_title("..."), "Chart");
        assert_eq!(file_title("a\nb"), "a b");
        assert_eq!(file_title(&"x".repeat(200)).len(), 80);
        let d = tempfile::tempdir().unwrap();
        let out = run(&MakeChart, d.path(), json!({"type": "bar", "title": "../../escape", "x": "month", "series": ["sales"], "data": rows()}));
        assert!(!out.failed, "{}", out.text);
        assert!(d.path().join("charts/escape.chart.json").exists());
    }

    #[test]
    fn reads_rows_from_a_csv_in_the_workspace() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("totals.csv"), "region,total\nNorth,10\nSouth,30\n").unwrap();
        let out = run(&MakeChart, d.path(), json!({"type": "pie", "title": "By region", "x": "region", "series": ["total"], "file": "totals.csv"}));
        assert!(!out.failed, "{}", out.text);
        let FileChange::Created(p) = &out.changes[0] else { panic!() };
        let spec = ChartSpec::from_json(&fs::read_to_string(p).unwrap()).unwrap();
        assert_eq!(serde_json::to_value(spec.data).unwrap(), json!([{"region": "North", "total": 10.0}, {"region": "South", "total": 30.0}]));
    }

    #[test]
    fn groups_a_csv_by_month() {
        let d = tempfile::tempdir().unwrap();
        let csv = "date,region,sales,units\n2026-02-03,North,\"1,000\",2\n2026-01-15,South,250.5,1\n2026-01-20,North,€100,3\n2026-02-28,South,,4\n";
        fs::write(d.path().join("sales.csv"), csv).unwrap();
        let chart = |extra: Value| {
            let mut args = json!({"type": "bar", "title": "t", "x": "date", "series": ["sales"], "data_file": "sales.csv", "bucket": "month"});
            args.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            let out = run(&MakeChart, d.path(), args);
            assert!(!out.failed, "{}", out.text);
            let FileChange::Created(p) = &out.changes[0] else { panic!() };
            (serde_json::to_value(ChartSpec::from_json(&fs::read_to_string(p).unwrap()).unwrap().data).unwrap(), out.text)
        };
        let (data, text) = chart(json!({}));
        assert_eq!(data, json!([{"date": "2026-01", "sales": 350.5}, {"date": "2026-02", "sales": 1000.0}]));
        assert!(text.contains("Summed sales by date per month: 4 file rows became 2 points"), "{text}");
        let (data, _) = chart(json!({"agg": "mean"}));
        assert_eq!(data, json!([{"date": "2026-01", "sales": 175.25}, {"date": "2026-02", "sales": 1000.0}]));
        let (data, _) = chart(json!({"agg": "count", "series": []}));
        assert_eq!(data, json!([{"date": "2026-01", "count": 2.0}, {"date": "2026-02", "count": 2.0}]));
        let (data, _) = chart(json!({"x": "region", "bucket": null, "series": ["sales", "units"]}));
        assert_eq!(data, json!([{"region": "North", "sales": 1100.0, "units": 5.0}, {"region": "South", "sales": 250.5, "units": 5.0}]));
        let (data, _) = chart(json!({"bucket": "year"}));
        assert_eq!(data, json!([{"date": "2026", "sales": 1350.5}]));
        let (data, text) = chart(json!({"split_by": "region"}));
        assert_eq!(data, json!([{"date": "2026-01", "North": 100.0, "South": 250.5}, {"date": "2026-02", "North": 1000.0, "South": 0.0}]));
        assert!(text.contains("Summed North, South by date per month"), "{text}");
        let (data, _) = chart(json!({"split_by": "region", "agg": "count", "series": []}));
        assert_eq!(data, json!([{"date": "2026-01", "North": 1.0, "South": 1.0}, {"date": "2026-02", "North": 1.0, "South": 1.0}]));
    }

    #[test]
    fn file_errors_are_readable() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("bad.csv"), "a,b\nx,notanumber\nx,2\n").unwrap();
        fs::write(d.path().join("rows.json"), r#"{"data": [{"a": "x", "b": 1}]}"#).unwrap();
        fs::write(d.path().join("empty.csv"), "a,b\n").unwrap();
        let try_ = |args: Value| run(&MakeChart, d.path(), args);
        let base = |file: &str| json!({"type": "bar", "x": "a", "series": ["b"], "file": file});
        assert!(try_(base("../outside.csv")).text.contains("outside the workspace"));
        assert!(try_(base("missing.csv")).failed);
        assert!(try_(base("bad.csv")).text.contains("\"b\" must hold numbers; found \"notanumber\""));
        assert!(try_(base("empty.csv")).text.contains("no rows"));
        assert!(!try_(base("rows.json")).failed);
        let mut b = base("bad.csv");
        b["bucket"] = json!("month");
        assert!(try_(b).text.contains("must hold dates"));
        let mut b = base("bad.csv");
        b["agg"] = json!("median");
        assert!(try_(b).text.contains("agg must be"));
        let mut b = base("bad.csv");
        b["x"] = json!("zzz");
        assert!(try_(b).text.contains("Columns: a, b"));
        let mut b = base("bad.csv");
        b["split_by"] = json!("nope");
        assert!(try_(b).text.contains("no column \"nope\" to split by"));
        let mut b = base("bad.csv");
        b["split_by"] = json!("a");
        b["series"] = json!(["b", "a"]);
        assert!(try_(b).text.contains("exactly one series"));
        let many: String = std::iter::once("k,v\n".to_owned()).chain((0..9).map(|i| format!("g{i},1\n"))).collect();
        fs::write(d.path().join("many.csv"), many).unwrap();
        let out = try_(json!({"type": "bar", "x": "v", "series": ["v"], "split_by": "k", "file": "many.csv"}));
        assert!(out.text.contains("more than 8 different values"), "{}", out.text);
    }

    #[cfg(unix)]
    #[test]
    fn never_saves_through_a_linked_charts_folder() {
        let outside = tempfile::tempdir().unwrap();
        let d = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), d.path().join("charts")).unwrap();
        let out = run(&MakeChart, d.path(), json!({"type": "bar", "x": "month", "series": ["sales"], "data": rows()}));
        assert!(out.failed);
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn spec_and_title() {
        let spec = MakeChart.spec();
        assert_eq!(spec.name, "make_chart");
        assert_eq!(MakeChart.title(&json!({"title": "Sales"})), "Made a chart: Sales");
        assert_eq!(MakeChart.access(&json!({})), Access::Read);
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(chart::date_of(&json!("2024-02-29")).unwrap()), (2024, 2, 29));
        assert_eq!(civil(-86_400), (1969, 12, 31));
    }
}
