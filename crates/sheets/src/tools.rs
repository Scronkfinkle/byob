//! The sheet tools (describe_sheet, read_cells, write_cells, fill, edit_sheet, format_cells,
//! create_sheet): the Python plugin's `backend.py` tool for tool, text for text.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::{Map, Value, json};

use crate::SheetError;
use crate::book::{Book, Kind, num_text};
use crate::py::{json_dumps, pystr, repr, repr_str, strip_lower};
use crate::refs::{MAX_COLS, MAX_ROWS, cell_name, col_name, parse_cell, parse_col, parse_range, parse_row, quote_sheet, range_name, shift_formula};
use crate::store::{Entry, Source, Store, resolve, truthy, xlsx_name};

pub const MAX_WRITE_CELLS: i64 = 100_000;
const READ_CELLS: i64 = 2000;
const READ_ROWS: i64 = 200;
const READ_COLS: i64 = 100;
const SHOW_CELLS: i64 = 400;
const SHOW_ROWS: i64 = 40;
const MAX_ERRORS: usize = 30;
/// Column width units per character (IronCalc's default 90 ≈ 12 characters).
pub const CHAR_WIDTH: f64 = 7.5;
const PROFILE_CELLS: i64 = 300_000;

type R<T> = Result<T, SheetError>;

fn msg<T>(m: impl Into<String>) -> R<T> {
    Err(SheetError::Msg(m.into()))
}

/// A tool call's context: the workspace and chat it runs for.
pub struct Ctx<'a> {
    pub workspace: &'a str,
    pub chat: Option<&'a str>,
}

impl Ctx<'_> {
    fn ws(&self) -> R<&str> {
        if self.workspace.is_empty() {
            return msg("No workspace is open, so there are no spreadsheets to work on.");
        }
        Ok(self.workspace)
    }
}

/// An argument as Python's `str()` would make it, `None` for null.
fn opt_text(v: Option<&Value>) -> Option<String> {
    match v {
        None | Some(Value::Null) => None,
        Some(x) => Some(pystr(x)),
    }
}

/// The first of these keys with a value that isn't null.
fn arg<'a>(args: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|k| args.get(*k).filter(|v| !v.is_null()))
}

// ---- ranges ------------------------------------------------------------------------------

pub fn sheet_index(book: &mut Book, name: Option<&str>, create: bool, added: Option<&mut Vec<String>>) -> R<u32> {
    let names = book.sheet_names();
    let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) else { return Ok(0) };
    if let Some(i) = names.iter().position(|n| n == name) {
        return Ok(u32::try_from(i).unwrap_or(0));
    }
    let low = name.to_lowercase();
    if let Some(i) = names.iter().position(|n| n.to_lowercase() == low) {
        return Ok(u32::try_from(i).unwrap_or(0));
    }
    if create {
        book.add_sheet(name)?;
        if let Some(a) = added {
            a.push(name.to_owned());
        }
        let names = book.sheet_names();
        return Ok(u32::try_from(names.iter().position(|n| n == name).unwrap_or(0)).unwrap_or(0));
    }
    msg(format!("No sheet named {}; the sheets are: {}.", repr_str(name), names.join(", ")))
}

/// (sheet, r1, c1, r2, c2): open sides filled from the used range; `clamp` cuts the range
/// to the used range (for reading).
pub fn resolve_range(
    book: &mut Book,
    text: Option<&str>,
    sheet_name: Option<&str>,
    create: bool,
    added: Option<&mut Vec<String>>,
    clamp: bool,
) -> R<(u32, i64, i64, i64, i64)> {
    let text = text.filter(|t| !t.trim().is_empty());
    let (sname, r1, c1, r2, c2) = match text {
        None => (None, None, None, None, None),
        Some(t) => {
            let r = parse_range(t).map_err(|e| SheetError::Msg(e.0))?;
            (r.sheet, r.r1, r.c1, r.r2, r.c2)
        }
    };
    let s = sheet_index(book, sname.as_deref().or(sheet_name), create, added)?;
    let used = book.used(s);
    let (ur1, ur2, uc1, uc2) = used.unwrap_or((1, 1, 1, 1));
    let has = text.is_some();
    let (r1, r2) = match (r1, r2) {
        (None, _) => (if has { 1 } else { ur1 }, ur2),
        (Some(a), None) => (a, a.max(ur2)),
        (Some(a), Some(b)) => (a, b),
    };
    let (c1, c2) = match (c1, c2) {
        (None, _) => (if has { 1 } else { uc1 }, uc2),
        (Some(a), None) => (a, a.max(uc2)),
        (Some(a), Some(b)) => (a, b),
    };
    let (mut r2, mut c2) = (r2, c2);
    if clamp && used.is_some() {
        r2 = r2.min(ur2);
        c2 = c2.min(uc2);
        if r1 > r2 || c1 > c2 {
            return Ok((s, r1, c1, r1 - 1, c1 - 1));
        }
    }
    Ok((s, r1, c1, r2, c2))
}

fn qualify(book: &mut Book, s: u32, r: &str) -> String {
    let names = book.sheet_names();
    if names.len() > 1 { format!("{}!{r}", quote_sheet(names.get(s as usize).map_or("", String::as_str))) } else { r.to_owned() }
}

// ---- text for the model ------------------------------------------------------------------

pub fn clean(v: &str) -> String {
    v.replace(['\t', '\r', '\n'], " ")
}

/// Lines: a header of column letters, then each non-empty row with its number.
pub fn tsv(book: &Book, s: u32, r1: i64, c1: i64, r2: i64, c2: i64, formulas: bool) -> Vec<String> {
    let mut lines = vec![format!("\t{}", (c1..=c2).map(col_name).collect::<Vec<_>>().join("\t"))];
    for r in r1..=r2 {
        let mut vals = Vec::new();
        for c in c1..=c2 {
            let content = book.content(s, r, c);
            if content.is_empty() {
                vals.push(String::new());
            } else if formulas && content.starts_with('=') {
                vals.push(content);
            } else {
                vals.push(clean(&book.model_text(s, r, c, None)));
            }
        }
        if vals.iter().any(|v| !v.is_empty()) {
            lines.push(format!("{r}\t{}", vals.join("\t")));
        }
    }
    lines
}

/// One error cell: (sheet, row, col, code, formula).
pub type ErrorCell = (u32, i64, i64, String, String);

/// Every error cell, sorted; cached per change.
pub fn errors(book: &mut Book) -> Vec<ErrorCell> {
    if let Some((g, e)) = &book.errors_cache
        && *g == book.version_gen
    {
        return e.clone();
    }
    let mut out: Vec<ErrorCell> = Vec::new();
    for (s, r, c) in book.all_cells() {
        if book.kind(s, r, c) == Kind::E {
            out.push((s, r, c, book.value_str(s, r, c), book.content(s, r, c)));
        }
    }
    out.sort();
    book.errors_cache = Some((book.version_gen, out.clone()));
    out
}

pub fn error_lines(book: &mut Book, sheet: Option<u32>) -> Vec<String> {
    let errs: Vec<ErrorCell> = errors(book).into_iter().filter(|e| sheet.is_none_or(|s| e.0 == s)).collect();
    if errs.is_empty() {
        return vec!["No error cells.".into()];
    }
    let mut parts = Vec::new();
    for (s, r, c, code, f) in errs.iter().take(MAX_ERRORS) {
        let at = qualify(book, *s, &cell_name(*r, *c));
        parts.push(if f.starts_with('=') { format!("{at} {code} ({f})") } else { format!("{at} {code}") });
    }
    let more = if errs.len() > MAX_ERRORS { format!(" (and {} more)", errs.len() - MAX_ERRORS) } else { String::new() };
    vec![format!("Errors ({}): {}{more}", errs.len(), parts.join("; "))]
}

/// Is row r1 a header: text over typed data, or distinct labels over other text?
pub fn detect_header(book: &Book, s: u32, r1: i64, r2: i64, c1: i64, c2: i64) -> bool {
    if r2 <= r1 {
        return false;
    }
    let mut texts: Vec<(i64, String)> = Vec::new();
    for c in c1..=c2 {
        let content = book.content(s, r1, c);
        if content.is_empty() {
            continue;
        }
        if content.starts_with('=') || book.kind(s, r1, c) != Kind::S || book.is_date(s, r1, c) {
            return false;
        }
        texts.push((c, strip_lower(&content)));
    }
    if texts.is_empty() {
        return false;
    }
    let distinct: BTreeSet<&String> = texts.iter().map(|(_, t)| t).collect();
    if distinct.len() < texts.len() {
        return false;
    }
    let probe = (r1 + 1)..=r2.min(r1 + 50);
    for (c, _) in &texts {
        for r in probe.clone() {
            let content = book.content(s, r, *c);
            if content.is_empty() {
                continue;
            }
            let k = book.kind(s, r, *c);
            if matches!(k, Kind::N | Kind::B) || content.starts_with('=') {
                return true;
            }
        }
    }
    for (c, h) in &texts {
        for r in probe.clone() {
            if strip_lower(&book.content(s, r, *c)) == *h {
                return false;
            }
        }
    }
    texts.len() >= 2
}

/// A date for sorting the way the Python does (numbers as `f"{v:020.6f}"`).
enum DateV {
    Num(f64),
    Text(String),
}

fn date_key(d: &DateV) -> String {
    match d {
        DateV::Num(v) => format!("{v:020.6}"),
        DateV::Text(t) => t.clone(),
    }
}

fn profile_column(book: &Book, s: u32, c: i64, rows: std::ops::RangeInclusive<i64>) -> String {
    let (mut nums, mut dates, mut texts): (Vec<f64>, Vec<(DateV, String)>, BTreeMap<String, usize>) = (Vec::new(), Vec::new(), BTreeMap::new());
    let (mut bools, mut errs, mut blank) = (0usize, 0usize, 0usize);
    let mut forms: Vec<(i64, String)> = Vec::new();
    for r in rows {
        let content = book.content(s, r, c);
        if content.is_empty() {
            blank += 1;
            continue;
        }
        if content.starts_with('=') {
            forms.push((r, content.clone()));
        }
        match book.kind(s, r, c) {
            Kind::N => {
                if book.is_date(s, r, c) {
                    dates.push((DateV::Num(book.number(s, r, c)), book.text(s, r, c)));
                } else {
                    nums.push(book.number(s, r, c));
                }
            }
            Kind::B => bools += 1,
            Kind::E => errs += 1,
            Kind::S => {
                let t = book.text(s, r, c);
                if book.is_date(s, r, c) {
                    dates.push((DateV::Text(t.clone()), t));
                } else {
                    *texts.entry(t).or_insert(0) += 1;
                }
            }
        }
    }
    let ntext: usize = texts.values().sum();
    let n = nums.len() + dates.len() + ntext + bools + errs;
    if n == 0 {
        return "empty".into();
    }
    let counts = [("number", nums.len()), ("date", dates.len()), ("text", ntext), ("bool", bools), ("error", errs)];
    let mut top = counts[0];
    for x in counts {
        if x.1 > top.1 {
            top = x;
        }
    }
    let mut kind = if top.1 as f64 >= 0.9 * n as f64 {
        top.0.to_owned()
    } else {
        format!("mixed ({})", counts.iter().filter(|(_, v)| *v > 0).map(|(k, v)| format!("{v} {k}")).collect::<Vec<_>>().join(", "))
    };
    if !forms.is_empty() && forms.len() as f64 >= n as f64 / 2.0 {
        kind = format!("formula → {kind}");
    }
    let mut parts = vec![kind.clone(), format!("{n} values")];
    if blank > 0 {
        parts.push(format!("{blank} blank"));
    }
    if !nums.is_empty() && (top.0 == "number" || kind.starts_with("mixed")) {
        let mut lo = nums[0];
        let mut hi = nums[0];
        for &v in &nums {
            if v < lo {
                lo = v;
            }
            if v > hi {
                hi = v;
            }
        }
        let sum = nums.iter().fold(0.0, |a, b| a + b);
        parts.push(format!("min {}, max {}, sum {}", num_text(lo), num_text(hi), num_text(sum)));
    }
    if !dates.is_empty() && top.0 == "date" {
        let (mut lo, mut hi) = (&dates[0], &dates[0]);
        for d in &dates {
            if date_key(&d.0) < date_key(&lo.0) {
                lo = d;
            }
            if date_key(&d.0) > date_key(&hi.0) {
                hi = d;
            }
        }
        parts.push(format!("{} … {}", lo.1, hi.1));
    }
    if !texts.is_empty() {
        let mut distinct: Vec<(&String, &usize)> = texts.iter().collect();
        distinct.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        if distinct.len() <= 10 {
            parts.push(format!("{} distinct: {}", distinct.len(), distinct.iter().map(|(t, k)| format!("{} ({k})", clean(t))).collect::<Vec<_>>().join(", ")));
        } else {
            parts.push(format!(
                "{} distinct, e.g. {}",
                distinct.len(),
                distinct.iter().take(5).map(|(t, _)| clean(t).chars().take(40).collect::<String>()).collect::<Vec<_>>().join(", ")
            ));
        }
    }
    if bools > 0 && top.0 == "bool" {
        parts.push(format!("{bools} TRUE/FALSE"));
    }
    if let Some((r, f)) = forms.first() {
        parts.push(format!("e.g. {}{r} {f}", col_name(c)));
    }
    parts.join(", ")
}

fn describe_one(book: &mut Book, s: u32) -> Vec<String> {
    let name = book.sheet_names().get(s as usize).cloned().unwrap_or_default();
    let Some((r1, r2, c1, c2)) = book.used(s) else { return vec![format!("Sheet \"{name}\": empty.")] };
    let (fr, fc) = book.frozen(s);
    let mut head = format!("Sheet \"{name}\": used range {} ({} rows × {} columns)", range_name(r1, c1, r2, c2), r2 - r1 + 1, c2 - c1 + 1);
    if fr > 0 || fc > 0 {
        head.push_str(&format!(", frozen {fr} rows, {fc} columns"));
    }
    let mut lines = vec![format!("{head}.")];
    let header = detect_header(book, s, r1, r2, c1, c2);
    let d1 = if header { r1 + 1 } else { r1 };
    let mut last = r2;
    if (r2 - d1 + 1) * (c2 - c1 + 1) > PROFILE_CELLS {
        last = d1 + PROFILE_CELLS / (c2 - c1 + 1);
        lines.push(format!("(Profiled rows {d1}–{last} only.)"));
    }
    if header {
        lines.push(format!("Header in row {r1}; data in rows {d1}–{r2}."));
    } else {
        lines.push(format!("No header row detected; data in rows {r1}–{r2}."));
    }
    for c in c1..=c2.min(c1 + 99) {
        let label = if header { format!("{} \"{}\"", col_name(c), clean(&book.content(s, r1, c))) } else { col_name(c) };
        lines.push(format!("  {label}: {}", profile_column(book, s, c, d1..=last)));
    }
    if c2 - c1 >= 100 {
        lines.push(format!("  … {} more columns (read_cells shows them).", c2 - c1 - 99));
    }
    let sample_end = r2.min(d1 + 2);
    let cc2 = c2.min(c1 + 29);
    lines.push("First rows:".into());
    lines.extend(tsv(book, s, r1, c1, sample_end, cc2, false));
    lines.extend(error_lines(book, Some(s)));
    lines
}

pub fn describe(book: &mut Book, src: Option<&Source>) -> Vec<String> {
    let names = book.sheet_names();
    let mut lines = Vec::new();
    if let Some(src) = src {
        if !src.note.is_empty() {
            lines.push(src.note.clone());
        }
        lines.push(format!("{}: {} sheet{} ({}).", src.label(), names.len(), if names.len() != 1 { "s" } else { "" }, names.join(", ")));
    }
    for s in 0..names.len() {
        lines.extend(describe_one(book, u32::try_from(s).unwrap_or(0)));
    }
    lines
}

pub fn user_edit_lines(entry: Option<&Entry>) -> Vec<String> {
    let Some(e) = entry.filter(|e| !e.user_edits.is_empty()) else { return Vec::new() };
    let cells = &e.user_edits[e.user_edits.len().saturating_sub(30)..];
    let more = e.user_edits.len() - cells.len();
    let more = if more > 0 { format!(" (and {more} more)") } else { String::new() };
    vec![format!("The user changed {}{more} in the grid since your last change.", cells.join(", "))]
}

// ---- lints -------------------------------------------------------------------------------

/// `IFERROR\s*\(.*,\s*""\s*\)` (case-insensitive, `.` across lines), searched.
fn iferror_blank(f: &str) -> bool {
    let low = f.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = low[from..].find("iferror").map(|i| i + from) {
        from = i + 1;
        let rest = low[i + 7..].trim_start();
        let Some(after) = rest.strip_prefix('(') else { continue };
        for (j, _) in after.match_indices(',') {
            let t = after[j + 1..].trim_start();
            if let Some(t) = t.strip_prefix("\"\"")
                && t.trim_start().starts_with(')')
            {
                return true;
            }
        }
    }
    false
}

fn lint_iferror(book: &mut Book, written: &[(u32, i64, i64)]) -> Vec<String> {
    let bad: Vec<&(u32, i64, i64)> = written.iter().filter(|(s, r, c)| iferror_blank(&book.content(*s, *r, *c))).collect();
    if bad.is_empty() {
        return Vec::new();
    }
    let refs: Vec<String> = bad.iter().take(5).map(|(s, r, c)| qualify(book, *s, &cell_name(*r, *c))).collect();
    let more = if bad.len() > 5 { format!(" and {} more", bad.len() - 5) } else { String::new() };
    vec![format!(
        "Check: {}{more} use IFERROR(…, \"\") which hides errors as blanks; fix what causes the error (e.g. a lookup value that is missing) or show a visible fallback.",
        refs.join(", ")
    )]
}

/// Typed numbers inside a column run that is otherwise formulas.
fn lint_hardcoded(book: &mut Book, written: &[(u32, i64, i64)]) -> Vec<String> {
    let mut by_col: BTreeMap<(u32, i64), (i64, i64)> = BTreeMap::new();
    for &(s, r, c) in written {
        let e = by_col.entry((s, c)).or_insert((r, r));
        *e = (e.0.min(r), e.1.max(r));
    }
    if by_col.is_empty() {
        return Vec::new();
    }
    let mut rows_by: BTreeMap<(u32, i64), BTreeSet<i64>> = BTreeMap::new();
    for (s, r, c) in book.all_cells() {
        if by_col.contains_key(&(s, c)) {
            rows_by.entry((s, c)).or_default().insert(r);
        }
    }
    let mut out = Vec::new();
    for (&(s, c), &(lo, hi)) in &by_col {
        let rows: Vec<i64> = rows_by.get(&(s, c)).map(|x| x.iter().copied().collect()).unwrap_or_default();
        let mut run: Vec<i64> = Vec::new();
        for r in rows.iter().map(|r| Some(*r)).chain([None]) {
            if let Some(r) = r
                && run.last().is_none_or(|l| r == l + 1)
            {
                run.push(r);
                continue;
            }
            if let (Some(&first), Some(&lst)) = (run.first(), run.last())
                && first <= hi
                && lst >= lo
            {
                let forms: Vec<i64> = run.iter().copied().filter(|x| book.content(s, *x, c).starts_with('=')).collect();
                if forms.len() >= 3 {
                    let (f0, fl) = (forms[0], forms[forms.len() - 1]);
                    let typed: Vec<i64> = run
                        .iter()
                        .copied()
                        .filter(|x| f0 < *x && *x < fl && !book.content(s, *x, c).starts_with('=') && book.kind(s, *x, c) == Kind::N)
                        .collect();
                    if !typed.is_empty() && forms.len() >= 2 * typed.len() {
                        let span = qualify(book, s, &format!("{}{f0}:{}{fl}", col_name(c), col_name(c)));
                        let cells: Vec<String> = typed.iter().take(5).map(|x| cell_name(*x, c)).collect();
                        let verb = if typed.len() == 1 { "is a typed number" } else { "are typed numbers" };
                        out.push(format!("Check: {} {verb} in {span} where the rest are formulas.", cells.join(", ")));
                    }
                }
            }
            run = r.into_iter().collect();
        }
        if out.len() >= 5 {
            break;
        }
    }
    out
}

// ---- writing -----------------------------------------------------------------------------

fn snapshot(book: &Book, limit: usize) -> Option<BTreeMap<(u32, i64, i64), String>> {
    let mut snap = BTreeMap::new();
    for (s, r, c) in book.all_cells() {
        if book.content(s, r, c).starts_with('=') {
            snap.insert((s, r, c), book.model_text(s, r, c, None));
            if snap.len() > limit {
                return None;
            }
        }
    }
    Some(snap)
}

/// What a write did, for its report.
#[derive(Default)]
struct Change {
    lines: Vec<String>,
    shown: Vec<(u32, i64, i64, i64, i64)>,
    cells: Vec<(u32, i64, i64)>,
    listed: Option<Vec<(u32, i64, i64)>>,
    added: Vec<String>,
}

/// Run `action` on the workbook `sheet` names, recalculate, and save a new version; the tool
/// result (text + the artifact to save).
fn write_op(store: &mut Store, cx: &Ctx<'_>, sheet: Option<&Value>, note: &str, action: impl FnOnce(&mut Book, &mut Change) -> R<()>) -> R<Value> {
    let ws = cx.ws()?;
    let src = resolve(ws, cx.chat, sheet, true)?;
    // The artifact's live entry is taken out while we work on it: put back on success,
    // dropped on failure (half-applied: reloaded from disk next time).
    let (mut entry, mut fresh) = if src.artifact {
        store.artifact_entry(&src.aid, &src.path, src.fmt, src.version)?;
        (store.entries.remove(&src.aid), None)
    } else {
        (None, Some(Book::load(&src.path, Some(src.fmt))?))
    };
    let book: &mut Book = match (entry.as_mut(), fresh.as_mut()) {
        (Some(e), _) => &mut e.book,
        (None, Some(b)) => b,
        (None, None) => return msg(format!("Artifact {} isn't loaded.", src.aid)),
    };
    let before = snapshot(book, 50_000);
    let mut ch = Change::default();
    action(book, &mut ch)?;
    book.evaluate();
    let mut lines = Vec::new();
    if !src.note.is_empty() {
        lines.push(src.note.clone());
    }
    lines.append(&mut ch.lines);
    for name in &ch.added {
        lines.push(format!("Added sheet {}.", repr_str(name)));
    }
    let written: BTreeSet<(u32, i64, i64)> = ch.cells.iter().copied().collect();
    if let Some(listed) = &ch.listed {
        let show = usize::try_from(SHOW_ROWS).unwrap_or(40);
        for &(s, r, c) in listed.iter().take(show) {
            let content = book.content(s, r, c);
            let at = qualify(book, s, &cell_name(r, c));
            if content.starts_with('=') {
                lines.push(format!("{at} {content} → {}", clean(&book.model_text(s, r, c, None))));
            } else {
                lines.push(format!("{at} = {}", if content.is_empty() { "(empty)".into() } else { clean(&book.model_text(s, r, c, None)) }));
            }
        }
        if listed.len() > show {
            lines.push(format!("… {} more cells.", listed.len() - show));
        }
    }
    for &(s, r1, c1, r2, c2) in ch.shown.iter().take(3) {
        let ncols = c2 - c1 + 1;
        let rows = SHOW_ROWS.min(SHOW_CELLS / ncols.max(1)).max(1);
        let end = r2.min(r1 + rows - 1);
        let cend = c2.min(c1 + 39);
        let at = qualify(book, s, &range_name(r1, c1, r2, c2));
        lines.push(format!("{at} now:"));
        lines.extend(tsv(book, s, r1, c1, end, cend, false));
        if end < r2 || cend < c2 {
            lines.push(format!("(Showing {}; read_cells shows the rest.)", range_name(r1, c1, end, cend)));
        }
    }
    if let Some(before) = &before {
        let after = snapshot(book, 50_000).unwrap_or_default();
        let changed: Vec<((u32, i64, i64), String, String)> = after
            .into_iter()
            .filter_map(|(k, v)| {
                let old = before.get(&k)?;
                (!written.contains(&k) && *old != v).then(|| (k, old.clone(), v))
            })
            .collect();
        if !changed.is_empty() {
            let parts: Vec<String> = changed
                .iter()
                .take(10)
                .map(|((s, r, c), old, v)| format!("{} {} (was {})", qualify(book, *s, &cell_name(*r, *c)), clean(v), clean(old)))
                .collect();
            let more = if changed.len() > 10 { format!(" and {} more", changed.len() - 10) } else { String::new() };
            lines.push(format!("Formulas elsewhere that changed: {}{more}.", parts.join("; ")));
        }
    }
    let written: Vec<(u32, i64, i64)> = written.into_iter().collect();
    lines.extend(error_lines(book, None));
    lines.extend(lint_iferror(book, &written));
    lines.extend(lint_hardcoded(book, &written));
    let data = book.to_xlsx()?;
    lines.extend(user_edit_lines(entry.as_ref()));
    let mut art = Map::new();
    art.insert("note".into(), json!(note));
    if let Some(mut e) = entry {
        art.insert("id".into(), json!(src.aid));
        let filename = src.meta.get("filename").filter(|f| truthy(Some(f))).map_or_else(|| src.aid.clone(), pystr);
        let mut save_as = filename.clone();
        if src.fmt != "xlsx" {
            let n = xlsx_name(&filename, &src.aid);
            lines.push(format!("(Saved as {n}: spreadsheets with formulas are stored as .xlsx.)"));
            art.insert("filename".into(), json!(n));
            save_as = n;
        }
        let file = store.save_tmp(&data, &save_as);
        e.wrote(&data, src.version);
        e.user_edits.clear();
        store.entries.insert(src.aid.clone(), e);
        art.insert("file".into(), json!(file?));
    } else {
        let filename = xlsx_name(&src.rel, "sheet");
        art.insert("title".into(), json!(src.title()));
        art.insert("filename".into(), json!(filename));
        art.insert("note".into(), json!(format!("From {}", src.rel)));
        art.insert("file".into(), json!(store.save_tmp(&data, &filename)?));
        lines.push(format!(
            "Saved as a new spreadsheet artifact ({filename}); {} itself is unchanged. Use the artifact id below as `sheet` from now on.",
            src.rel
        ));
    }
    Ok(json!({"text": lines.join("\n"), "artifact": Value::Object(art)}))
}

fn parse_json_arg(v: Option<&Value>, what: &str) -> R<Option<Value>> {
    match v {
        Some(Value::String(t)) => {
            let t = t.trim();
            if t.starts_with('[') || t.starts_with('{') {
                return serde_json::from_str(t).map(Some).map_err(|_| SheetError::Msg(format!("`{what}` isn't valid JSON.")));
            }
            if what == "rows" {
                let rows: Vec<Value> = t
                    .lines()
                    .map(|line| if line.contains('\t') { Value::Array(line.split('\t').map(|x| json!(x)).collect()) } else { json!([line]) })
                    .collect();
                return Ok(Some(Value::Array(rows)));
            }
            Ok(v.cloned())
        }
        Some(Value::Null) | None => Ok(None),
        Some(other) => Ok(Some(other.clone())),
    }
}

/// What a cell gets from a JSON value: `{"formula"|"value"|"v": …}` unwrapped, lists and
/// other objects as JSON text, `"  =…"` as a formula.
fn cell_value(v: &Value) -> Value {
    match v {
        Value::Object(o) => {
            for k in ["formula", "value", "v"] {
                if let Some(x) = o.get(k) {
                    return cell_value(x);
                }
            }
            Value::String(json_dumps(v))
        }
        Value::Array(_) => Value::String(json_dumps(v)),
        Value::String(t) => {
            let l = t.trim_start();
            if l.starts_with('=') { Value::String(l.to_owned()) } else { v.clone() }
        }
        other => other.clone(),
    }
}

fn plural(n: i64) -> &'static str {
    if n != 1 { "s" } else { "" }
}

// ---- the tools ---------------------------------------------------------------------------

pub fn create_sheet(store: &mut Store, cx: &Ctx<'_>, args: &Value) -> R<Value> {
    let src_arg = ["from", "source", "from_file"].iter().find_map(|k| args.get(*k).filter(|v| truthy(Some(v)))).map(pystr);
    let title_in = args.get("title").filter(|v| truthy(Some(v))).map(pystr).unwrap_or_default();
    let mut title = title_in.trim().to_owned();
    if title.is_empty() {
        title = match &src_arg {
            Some(a) => {
                let base = Path::new(a).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
                crate::store::splitext(&base).0.to_owned()
            }
            None => "Spreadsheet".into(),
        };
    }
    let filename = match args.get("filename").filter(|v| truthy(Some(v))) {
        Some(f) => xlsx_name(&pystr(f), "sheet"),
        None => {
            let mut slug = String::new();
            let mut dash = false;
            for ch in title.to_lowercase().chars() {
                if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
                    slug.push(ch);
                    dash = false;
                } else if !dash {
                    slug.push('-');
                    dash = true;
                }
            }
            let slug: String = slug.trim_matches('-').chars().take(60).collect();
            format!("{}.xlsx", if slug.is_empty() { "sheet" } else { &slug })
        }
    };
    let mut note = "Created".to_owned();
    let mut lines = Vec::new();
    let mut b = match &src_arg {
        Some(a) => {
            let ws = cx.ws()?;
            let src = resolve(ws, cx.chat, Some(&Value::String(a.clone())), false)?;
            let b = Book::load(&src.path, Some(src.fmt))?;
            if src.artifact {
                note = format!("From artifact {}", src.aid);
            } else {
                note = format!("From {}", src.rel);
                lines.push(format!("Imported {} (the file itself is unchanged).", src.rel));
            }
            b
        }
        None => Book::new(&title)?,
    };
    lines.push(format!("New spreadsheet \"{title}\" ({filename})."));
    lines.extend(describe(&mut b, None));
    lines.push("Use the artifact id byob reports below as `sheet` in write_cells, fill, read_cells and the other sheet tools.".into());
    let path = store.save_tmp(&b.to_xlsx()?, &filename)?;
    Ok(json!({"text": lines.join("\n"), "artifact": {"title": title, "filename": filename, "file": path, "note": note}}))
}

pub fn describe_sheet(store: &mut Store, cx: &Ctx<'_>, args: &Value) -> R<Value> {
    let src = resolve(cx.ws()?, cx.chat, args.get("sheet"), true)?;
    let book = store.read_book(&src)?;
    let mut lines = describe(book, Some(&src));
    if src.artifact {
        lines.extend(user_edit_lines(store.entries.get(&src.aid)));
    }
    Ok(Value::String(lines.join("\n")))
}

pub fn read_cells(store: &mut Store, cx: &Ctx<'_>, args: &Value) -> R<Value> {
    let src = resolve(cx.ws()?, cx.chat, args.get("sheet"), true)?;
    let what_in = args.get("what").filter(|v| truthy(Some(v))).map(pystr).unwrap_or_default();
    let formulas = what_in.to_lowercase().starts_with("form");
    let what = if formulas { "formulas" } else { "values" };
    let range = opt_text(args.get("range"));
    let range_truthy = truthy(args.get("range"));
    let sheet_name = opt_text(args.get("sheet_name"));
    let book = store.read_book(&src)?;
    let (s, r1, c1, r2, mut c2) = resolve_range(book, range.as_deref(), sheet_name.as_deref(), false, None, true)?;
    let name = book.sheet_names().get(s as usize).cloned().unwrap_or_default();
    let mut lines = Vec::new();
    if !src.note.is_empty() {
        lines.push(src.note.clone());
    }
    if r2 < r1 || c2 < c1 || book.used(s).is_none() {
        lines.push(if range_truthy {
            format!("{}!{} is empty.", quote_sheet(&name), range.as_deref().unwrap_or("A1"))
        } else {
            format!("Sheet \"{name}\" is empty.")
        });
        return Ok(Value::String(lines.join("\n")));
    }
    let mut note = String::new();
    if c2 - c1 + 1 > READ_COLS {
        note = format!(" (columns {}–{} of {}–{})", col_name(c1), col_name(c1 + READ_COLS - 1), col_name(c1), col_name(c2));
        c2 = c1 + READ_COLS - 1;
    }
    let per = READ_ROWS.min(READ_CELLS / (c2 - c1 + 1)).max(1);
    let end = r2.min(r1 + per - 1);
    lines.push(format!("{}!{} {what}, rows {r1}–{end} of {r1}–{r2}{note}:", quote_sheet(&name), range_name(r1, c1, end, c2)));
    lines.extend(tsv(book, s, r1, c1, end, c2, formulas));
    if end < r2 {
        let nxt = range_name(end + 1, c1, r2, c2);
        lines.push(format!("More rows: read_cells(range=\"{}\").", qualify(book, s, &nxt)));
    }
    if src.artifact {
        lines.extend(user_edit_lines(store.entries.get(&src.aid)));
    }
    Ok(Value::String(lines.join("\n")))
}

pub fn write_cells(store: &mut Store, cx: &Ctx<'_>, args: &Value) -> R<Value> {
    let rows_in = match args.get("rows").filter(|v| !v.is_null()) {
        Some(r) => Some(r),
        None => arg(args, &["values"]).or_else(|| args.get("data")),
    };
    let start = match args.get("start").filter(|v| !v.is_null()) {
        Some(s) => pystr(s),
        None => ["range", "cell"].iter().find_map(|k| args.get(*k).filter(|v| truthy(Some(v)))).map_or_else(|| "A1".into(), pystr),
    };
    let mut rows = parse_json_arg(rows_in, "rows")?;
    let cells = parse_json_arg(args.get("cells"), "cells")?;
    let sheet_name = opt_text(args.get("sheet_name"));
    if rows.is_none() && cells.is_none() {
        return msg(
            "Give `rows` (a 2D list written from `start`, e.g. [[\"Item\",\"Cost\"],[\"Rent\",1200]]) or `cells` (e.g. {\"B2\": \"=SUM(B3:B9)\", \"C1\": 5}).",
        );
    }
    if rows.as_ref().is_some_and(|r| !r.is_array()) {
        return msg("`rows` must be a list of rows, each a list of values.");
    }
    if cells.as_ref().is_some_and(|c| !c.is_object()) {
        return msg("`cells` must be an object like {\"B2\": \"=SUM(B3:B9)\", \"C1\": 5}.");
    }
    if let Some(Value::Array(list)) = &rows
        && !list.is_empty()
        && !list.iter().any(Value::is_array)
    {
        rows = Some(Value::Array(vec![Value::Array(list.clone())])); // one row given flat
    }
    let rows: Option<Vec<Value>> = rows.and_then(|r| r.as_array().cloned());
    let (sname, r0, c0) = match &rows {
        Some(_) => parse_cell(start.split(':').next().unwrap_or_default()).map_err(|e| SheetError::Msg(e.0))?,
        None => (None, 1, 1),
    };
    let mut targets = Vec::new();
    if let Some(Value::Object(o)) = &cells {
        for (k, v) in o {
            targets.push((parse_cell(k).map_err(|e| SheetError::Msg(e.0))?, v.clone()));
        }
    }
    let count: usize = rows.iter().flatten().map(|r| r.as_array().map_or(1, Vec::len)).sum::<usize>() + targets.len();
    if count as i64 > MAX_WRITE_CELLS {
        return msg(format!("That's {count} cells; write at most {MAX_WRITE_CELLS} per call, or use fill for formula columns."));
    }
    for (i, row) in rows.iter().flatten().enumerate() {
        let w = row.as_array().map_or(1, Vec::len) as i64;
        if r0 + i as i64 > MAX_ROWS || c0 + w - 1 > MAX_COLS {
            return msg("Those rows run off the end of the sheet.");
        }
    }
    write_op(store, cx, args.get("sheet"), "write_cells", |book, ch| {
        let mut n = 0i64;
        if let Some(rows) = &rows {
            let s = sheet_index(book, sname.as_deref().or(sheet_name.as_deref()), true, Some(&mut ch.added))?;
            let mut width = 0i64;
            for (i, row) in rows.iter().enumerate() {
                let one = [row.clone()];
                let row: &[Value] = row.as_array().map_or(&one[..], Vec::as_slice);
                width = width.max(row.len() as i64);
                for (j, v) in row.iter().enumerate() {
                    if v.is_null() {
                        continue;
                    }
                    let (r, c) = (r0 + i as i64, c0 + j as i64);
                    book.put(s, r, c, &cell_value(v))?;
                    ch.cells.push((s, r, c));
                    n += 1;
                }
            }
            if !rows.is_empty() && width > 0 {
                ch.shown.push((s, r0, c0, r0 + rows.len() as i64 - 1, c0 + width - 1));
            }
        }
        if !targets.is_empty() {
            let mut listed = Vec::new();
            for ((tsn, r, c), v) in &targets {
                let s = sheet_index(book, tsn.as_deref().or(sheet_name.as_deref()), true, Some(&mut ch.added))?;
                book.put(s, *r, *c, &cell_value(v))?;
                ch.cells.push((s, *r, *c));
                listed.push((s, *r, *c));
                n += 1;
            }
            ch.listed = Some(listed);
        }
        ch.lines.push(format!("Wrote {n} cell{}.", plural(n)));
        Ok(())
    })
}

pub fn fill(store: &mut Store, cx: &Ctx<'_>, args: &Value) -> R<Value> {
    let formula = arg(args, &["formula", "value"]);
    if formula.is_none_or(|f| f.as_str().is_some_and(|t| t.trim().is_empty())) {
        return msg(
            "Give `formula`: what the first cell of the range gets, like \"=B2*C2\"; the other cells get it with their references moved, like Excel's fill.",
        );
    }
    if !truthy(args.get("range")) {
        return msg("Give `range`, like \"D2:D13\" (or \"D2:D\" for down to the last used row).");
    }
    let range = opt_text(args.get("range")).unwrap_or_default();
    let formula = cell_value(formula.unwrap_or(&Value::Null));
    parse_range(&range).map_err(|e| SheetError::Msg(e.0))?;
    let sheet_name = opt_text(args.get("sheet_name"));
    let note = format!("fill {range}");
    write_op(store, cx, args.get("sheet"), &note, |book, ch| {
        let (s, r1, c1, r2, c2) = resolve_range(book, Some(&range), sheet_name.as_deref(), true, Some(&mut ch.added), false)?;
        let n = (r2 - r1 + 1) * (c2 - c1 + 1);
        if n > MAX_WRITE_CELLS {
            return msg(format!("{range} has {n} cells; fill at most {MAX_WRITE_CELLS} per call."));
        }
        let shifted = |dr: i64, dc: i64| match &formula {
            Value::String(f) => Value::String(shift_formula(f, dr, dc)),
            other => other.clone(),
        };
        for r in r1..=r2 {
            for c in c1..=c2 {
                book.put(s, r, c, &shifted(r - r1, c - c1))?;
                ch.cells.push((s, r, c));
            }
        }
        let last = pystr(&shifted(r2 - r1, c2 - c1));
        let at = qualify(book, s, &range_name(r1, c1, r2, c2));
        let mut m = format!("Filled {at} ({n} cells) with {}", pystr(&formula));
        if n > 1 {
            m.push_str(&format!("; {} got {last}", cell_name(r2, c2)));
        }
        ch.lines.push(format!("{m}."));
        ch.shown.push((s, r1, c1, r2, c2));
        Ok(())
    })
}

/// A sort key: (0, number), (1, text lowercased), (2, bool), (3, error text).
#[derive(Debug, Clone, PartialEq)]
struct SortKey(u8, f64, String);

impl SortKey {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        self.0.cmp(&o.0).then(self.1.partial_cmp(&o.1).unwrap_or(std::cmp::Ordering::Equal)).then_with(|| self.2.cmp(&o.2))
    }
}

fn sort_key(book: &Book, s: u32, r: i64, c: i64) -> Option<SortKey> {
    if book.content(s, r, c).is_empty() {
        return None;
    }
    use ironcalc::base::cell::CellValue;
    let v = book.value(s, r, c);
    Some(match book.kind(s, r, c) {
        Kind::N => SortKey(0, if let CellValue::Number(n) = v { n } else { 0.0 }, String::new()),
        Kind::S => SortKey(1, 0.0, book.value_str(s, r, c).to_lowercase()),
        Kind::B => SortKey(2, if matches!(v, CellValue::Boolean(true)) || matches!(v, CellValue::Number(n) if n != 0.0) { 1.0 } else { 0.0 }, String::new()),
        Kind::E => SortKey(3, 0.0, book.value_str(s, r, c)),
    })
}

#[allow(clippy::too_many_arguments)]
fn sort(book: &mut Book, ch: &mut Change, s: u32, r1: i64, c1: i64, r2: i64, c2: i64, by: Option<&Value>, descending: bool, header: Option<bool>) -> R<()> {
    let by_c = match by.map(pystr).filter(|b| !b.trim().is_empty()) {
        None => c1,
        Some(b) => match parse_col(by.unwrap_or(&Value::Null)) {
            Ok(c) => c,
            Err(_) => {
                let mut found = None;
                for c in c1..=c2 {
                    if strip_lower(&book.content(s, r1, c)) == strip_lower(&b) {
                        found = Some(c);
                    }
                }
                match found {
                    Some(c) => c,
                    None => return msg(format!("`by` should be a column letter of the range (like C); {} isn't one.", repr(by.unwrap_or(&Value::Null)))),
                }
            }
        },
    };
    if !(c1..=c2).contains(&by_c) {
        return msg(format!("Column {} isn't inside the range {}.", col_name(by_c), range_name(r1, c1, r2, c2)));
    }
    let header = header.unwrap_or_else(|| detect_header(book, s, r1, r2, c1, c2));
    let start = if header { r1 + 1 } else { r1 };
    let mut rows = Vec::new();
    for r in start..=r2 {
        let cells: Vec<(String, Value)> = (c1..=c2).map(|c| (book.content(s, r, c), book.style(s, r, c))).collect();
        rows.push((sort_key(book, s, r, by_c), r, cells));
    }
    let (mut full, empty): (Vec<_>, Vec<_>) = rows.into_iter().partition(|x| x.0.is_some());
    full.sort_by(|a, b| {
        let (x, y) = (a.0.as_ref(), b.0.as_ref());
        let o = match (x, y) {
            (Some(x), Some(y)) => x.cmp(y),
            _ => std::cmp::Ordering::Equal,
        };
        if descending { o.reverse() } else { o }
    });
    let mut moved = 0;
    for (i, (_, old, cells)) in full.into_iter().chain(empty).enumerate() {
        let new = start + i as i64;
        if new != old {
            moved += 1;
        }
        for (j, (content, st)) in cells.into_iter().enumerate() {
            let c = c1 + j as i64;
            let content = if content.starts_with('=') { shift_formula(&content, new - old, 0) } else { content };
            book.put_text(s, new, c, &content)?;
            book.set_style(s, new, c, &st)?;
        }
    }
    let at = qualify(book, s, &range_name(start, c1, r2, c2));
    ch.lines.push(format!(
        "Sorted {at} by column {}, {}{}; {moved} rows moved.",
        col_name(by_c),
        if descending { "descending" } else { "ascending" },
        if header { format!(" (row {r1} kept as the header)") } else { String::new() }
    ));
    ch.shown.push((s, r1, c1, r2, c2));
    Ok(())
}

fn count_arg(v: Option<&Value>) -> R<i64> {
    let n = match v {
        None | Some(Value::Null) => return Ok(1),
        Some(Value::String(t)) if t.is_empty() => return Ok(1),
        Some(Value::Bool(b)) => i64::from(*b),
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)).unwrap_or(0),
        Some(Value::String(t)) => crate::refs::py_int(t).ok_or_else(|| SheetError::Msg(format!("`count` should be a number; got {}.", repr_str(t))))?,
        Some(other) => return msg(format!("`count` should be a number; got {}.", repr(other))),
    };
    if !(1..=100_000).contains(&n) {
        return msg("`count` should be between 1 and 100000.");
    }
    Ok(n)
}

fn flag(v: Option<&Value>) -> bool {
    match v {
        Some(Value::String(t)) => ["true", "yes", "1", "y"].contains(&t.trim().to_lowercase().as_str()),
        other => truthy(other),
    }
}

/// Python's `int(v)` for a JSON value (`None` where it raises).
fn py_int_value(v: &Value) -> Option<i64> {
    match v {
        Value::Bool(b) => Some(i64::from(*b)),
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f.trunc() as i64)),
        Value::String(t) => crate::refs::py_int(t),
        _ => None,
    }
}

const EDIT_OPS: [&str; 9] = ["add_sheet", "rename_sheet", "delete_sheet", "insert_rows", "delete_rows", "insert_cols", "delete_cols", "sort", "freeze"];

pub fn edit_sheet(store: &mut Store, cx: &Ctx<'_>, args: &Value) -> R<Value> {
    let op0 = args.get("op").filter(|v| truthy(Some(v))).map(pystr).unwrap_or_default().trim().to_lowercase().replace(['-', ' '], "_");
    let op = match op0.as_str() {
        "insert_columns" => "insert_cols",
        "delete_columns" => "delete_cols",
        "add" => "add_sheet",
        "rename" => "rename_sheet",
        "delete" => "delete_sheet",
        "freeze_panes" => "freeze",
        o => o,
    }
    .to_owned();
    if !EDIT_OPS.contains(&op.as_str()) {
        return msg(format!("`op` should be one of: {}.", EDIT_OPS.join(", ")));
    }
    let sheet_name = args.get("sheet_name").filter(|v| !v.is_null());
    let name = args.get("name").filter(|v| !v.is_null());
    let new_name = args.get("new_name").filter(|v| !v.is_null());
    let note = format!("edit_sheet {op}");
    write_op(store, cx, args.get("sheet"), &note, |book, ch| {
        let mut target = sheet_name;
        if op == "rename_sheet" || op == "delete_sheet" {
            target = if truthy(name) { name } else { sheet_name };
            if !truthy(target) {
                return msg(format!("{op} needs `name`: the sheet to {}.", op.split('_').next().unwrap_or_default()));
            }
        }
        let t = if op != "add_sheet" { target } else { sheet_name };
        let s = sheet_index(book, t.map(pystr).as_deref(), false, None)?;
        let names = book.sheet_names();
        let here = names.get(s as usize).cloned().unwrap_or_default();
        match op.as_str() {
            "add_sheet" => {
                let nm = [name, new_name].into_iter().flatten().find(|v| truthy(Some(v))).map(pystr).unwrap_or_default().trim().to_owned();
                if nm.is_empty() {
                    return msg("add_sheet needs `name`.");
                }
                if names.iter().any(|x| x.to_lowercase() == nm.to_lowercase()) {
                    return msg(format!("There is already a sheet named {}.", repr_str(&nm)));
                }
                book.add_sheet(&nm)?;
                ch.lines.push(format!("Added sheet {}; the sheets are now: {}.", repr_str(&nm), book.sheet_names().join(", ")));
            }
            "rename_sheet" => {
                let nm = new_name.filter(|v| truthy(Some(v))).map(pystr).unwrap_or_default().trim().to_owned();
                if nm.is_empty() {
                    return msg("rename_sheet needs `new_name`.");
                }
                book.rename_sheet(s, &nm)?;
                ch.lines.push(format!("Renamed sheet {} to {}; formulas that pointed at it were updated.", repr_str(&here), repr_str(&nm)));
            }
            "delete_sheet" => {
                if names.len() == 1 {
                    return msg("That's the only sheet; a workbook needs at least one.");
                }
                book.delete_sheet(s)?;
                ch.lines.push(format!("Deleted sheet {}; the sheets are now: {}.", repr_str(&here), book.sheet_names().join(", ")));
            }
            "insert_rows" | "delete_rows" => {
                let Some(at) = args.get("at").filter(|v| !v.is_null()) else { return msg(format!("{op} needs `at`: a row number.")) };
                let r = parse_row(at).map_err(|e| SheetError::Msg(e.0))?;
                let n = count_arg(args.get("count"))?;
                if op == "insert_rows" {
                    book.insert_rows(s, r, n)?;
                    ch.lines.push(format!(
                        "Inserted {n} empty row{} at row {r} of {}; rows below moved down by {n} and formulas were adjusted.",
                        if n > 1 { "s" } else { "" },
                        repr_str(&here)
                    ));
                } else {
                    book.delete_rows(s, r, n)?;
                    ch.lines.push(format!("Deleted rows {r}–{} of {}; rows below moved up and formulas were adjusted.", r + n - 1, repr_str(&here)));
                }
            }
            "insert_cols" | "delete_cols" => {
                let Some(at) = args.get("at").filter(|v| !v.is_null()) else { return msg(format!("{op} needs `at`: a column letter.")) };
                let c = parse_col(at).map_err(|e| SheetError::Msg(e.0))?;
                let n = count_arg(args.get("count"))?;
                if op == "insert_cols" {
                    book.insert_cols(s, c, n)?;
                    ch.lines.push(format!(
                        "Inserted {n} empty column{} at {} of {}; columns to the right moved and formulas were adjusted.",
                        if n > 1 { "s" } else { "" },
                        col_name(c),
                        repr_str(&here)
                    ));
                } else {
                    book.delete_cols(s, c, n)?;
                    ch.lines.push(format!("Deleted columns {}–{} of {}; formulas were adjusted.", col_name(c), col_name(c + n - 1), repr_str(&here)));
                }
            }
            "sort" => {
                let range = opt_text(args.get("range"));
                let (ss, r1, c1, r2, c2) = resolve_range(book, range.as_deref(), sheet_name.map(pystr).as_deref(), false, None, false)?;
                if book.used(ss).is_none() {
                    return msg("That sheet is empty; there is nothing to sort.");
                }
                if (r2 - r1 + 1) * (c2 - c1 + 1) > MAX_WRITE_CELLS {
                    return msg(format!("That range is too big to sort here (at most {MAX_WRITE_CELLS} cells)."));
                }
                let header = match args.get("header") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(t)) if t.is_empty() => None,
                    h => Some(flag(h)),
                };
                sort(book, ch, ss, r1, c1, r2, c2, args.get("by").filter(|v| !v.is_null()), flag(args.get("descending")), header)?;
            }
            _ => {
                let (fr0, fc0) = book.frozen(s);
                let get = |k: &str, d: i64| match args.get(k) {
                    None | Some(Value::Null) => Some(d),
                    Some(Value::String(t)) if t.is_empty() => Some(d),
                    Some(v) => py_int_value(v),
                };
                let (Some(fr), Some(fc)) = (get("rows", fr0), get("cols", fc0)) else { return msg("freeze takes `rows` (0–1000) and `cols` (0–100).") };
                if !((0..=1000).contains(&fr) && (0..=100).contains(&fc)) {
                    return msg("freeze takes `rows` (0–1000) and `cols` (0–100).");
                }
                book.set_frozen(s, fr, fc)?;
                ch.lines.push(format!("{}: frozen {fr} row{} and {fc} column{}.", repr_str(&here), plural(fr), plural(fc)));
            }
        }
        Ok(())
    })
}

fn hex_color(t: &str) -> Option<String> {
    let h = t.strip_prefix('#').unwrap_or(t);
    (h.len() == 6 && h.chars().all(|c| c.is_ascii_hexdigit())).then(|| format!("#{}", h.to_uppercase()))
}

fn align_of(t: &str) -> Option<&'static str> {
    match t.to_lowercase().as_str() {
        "left" => Some("left"),
        "center" | "centre" => Some("center"),
        "right" => Some("right"),
        _ => None,
    }
}

pub fn format_cells(store: &mut Store, cx: &Ctx<'_>, args: &Value) -> R<Value> {
    let number_format: Option<String> = match args.get("number_format").filter(|v| !v.is_null()) {
        Some(v) => Some(pystr(v)),
        None => ["format", "num_fmt"]
            .iter()
            .find_map(|k| args.get(*k).filter(|v| truthy(Some(v))))
            .or_else(|| args.get("num_fmt").filter(|v| !v.is_null()))
            .map(pystr),
    };
    if !truthy(args.get("range")) {
        return msg("Give `range`, like \"B2:B13\" or \"1:1\".");
    }
    let range = opt_text(args.get("range")).unwrap_or_default();
    parse_range(&range).map_err(|e| SheetError::Msg(e.0))?;
    let fill = args.get("fill").filter(|v| !v.is_null() && v.as_str() != Some(""));
    let color = match fill {
        None => None,
        Some(f) => {
            let t = pystr(f);
            if ["none", "no", "clear", "false"].contains(&t.to_lowercase().as_str()) {
                Some(String::new())
            } else {
                match hex_color(t.trim()) {
                    Some(c) => Some(c),
                    None => return msg(format!("`fill` should be a colour like \"#ffe599\"; got {}.", repr(f))),
                }
            }
        }
    };
    let align_v = args.get("align").filter(|v| !v.is_null() && v.as_str() != Some(""));
    if let Some(a) = align_v
        && align_of(&pystr(a)).is_none()
    {
        return msg("`align` should be left, center or right.");
    }
    let align = if truthy(args.get("align")) { align_v.and_then(|a| align_of(&pystr(a))) } else { None };
    let width = match args.get("width").filter(|v| !v.is_null() && v.as_str() != Some("")) {
        None => None,
        Some(w) => {
            let f = match w {
                Value::Number(n) => n.as_f64(),
                Value::Bool(b) => Some(f64::from(u8::from(*b))),
                Value::String(t) => t.trim().parse::<f64>().ok(),
                _ => None,
            };
            let Some(f) = f else { return msg("`width` should be a number of characters, like 14.") };
            if !(1.0..=255.0).contains(&f) {
                return msg("`width` should be between 1 and 255 characters.");
            }
            Some(f)
        }
    };
    let bold = args.get("bold").filter(|v| !v.is_null());
    if number_format.is_none() && bold.is_none() && color.is_none() && width.is_none() && align.is_none() {
        return msg("Give at least one of number_format, bold, fill, width or align.");
    }
    let sheet_name = opt_text(args.get("sheet_name"));
    write_op(store, cx, args.get("sheet"), "format_cells", |book, ch| {
        let (s, r1, c1, r2, c2) = resolve_range(book, Some(&range), sheet_name.as_deref(), true, Some(&mut ch.added), false)?;
        let n = (r2 - r1 + 1) * (c2 - c1 + 1);
        if n > MAX_WRITE_CELLS {
            return msg(format!("{range} has {n} cells; format at most {MAX_WRITE_CELLS} per call."));
        }
        let mut did = Vec::new();
        if number_format.is_some() || bold.is_some() || color.is_some() || align.is_some() {
            for r in r1..=r2 {
                for c in c1..=c2 {
                    let mut st = book.style(s, r, c);
                    let Some(o) = st.as_object_mut() else { continue };
                    if let Some(f) = &number_format {
                        o.insert("num_fmt".into(), json!(if f.is_empty() { "general" } else { f }));
                    }
                    if bold.is_some() {
                        let font = o.entry("font").or_insert_with(|| json!({}));
                        if let Some(font) = font.as_object_mut() {
                            font.insert("b".into(), json!(flag(bold)));
                        }
                    }
                    if let Some(col) = &color {
                        o.insert("fill".into(), if col.is_empty() { json!({}) } else { json!({"color": col}) });
                    }
                    if let Some(a) = align {
                        let mut al = o.get("alignment").filter(|v| truthy(Some(v))).cloned().unwrap_or_else(|| json!({}));
                        if let Some(m) = al.as_object_mut() {
                            m.insert("horizontal".into(), json!(a));
                        }
                        o.insert("alignment".into(), al);
                    }
                    book.set_style(s, r, c, &st)?;
                }
            }
            if let Some(f) = &number_format {
                did.push(format!("number format {f}"));
            }
            if bold.is_some() {
                did.push(if flag(bold) { "bold" } else { "not bold" }.to_owned());
            }
            if let Some(col) = &color {
                did.push(if col.is_empty() { "no fill".into() } else { format!("fill {col}") });
            }
            if let Some(a) = align {
                did.push(format!("aligned {a}"));
            }
        }
        if let Some(w) = width {
            for c in c1..=c2 {
                book.set_width(s, c, w * CHAR_WIDTH)?;
            }
            did.push(format!("columns {}–{} {} characters wide", col_name(c1), col_name(c2), num_text(w)));
        }
        book.touch();
        let at = qualify(book, s, &range_name(r1, c1, r2, c2));
        ch.lines.push(format!("Formatted {at}: {}.", did.join(", ")));
        if number_format.is_some() {
            let mut shown = Vec::new();
            for r in r1..=r2.min(r1 + 4) {
                for c in c1..=c2.min(c1 + 3) {
                    if !book.content(s, r, c).is_empty() {
                        shown.push(format!("{} {}", cell_name(r, c), clean(&book.text(s, r, c))));
                    }
                }
            }
            if !shown.is_empty() {
                ch.lines.push(format!("Shown as: {}.", shown.iter().take(8).cloned().collect::<Vec<_>>().join("; ")));
            }
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iferror_lint_pattern() {
        assert!(iferror_blank("=IFERROR(VLOOKUP(99,A2:B11,2,FALSE),\"\")"));
        assert!(iferror_blank("=iferror ( A1/B1 , \"\" )"));
        assert!(!iferror_blank("=IFERROR(A1/B1,0)"));
        assert!(!iferror_blank("=SUM(A1)"));
    }
}
