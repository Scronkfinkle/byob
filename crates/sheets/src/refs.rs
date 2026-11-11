//! A1 references: forgiving ranges, and formulas shifted like Excel's fill and copy. Rows
//! and columns are 1-based.

use serde_json::Value;

use crate::text::{pystr, repr, repr_str};

pub const MAX_ROWS: i64 = 1_048_576;
pub const MAX_COLS: i64 = 16_384;

/// A range or cell reference the model wrote that we can't read (the message says why).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeError(pub String);

type R<T> = Result<T, RangeError>;

fn err<T>(msg: String) -> R<T> {
    Err(RangeError(msg))
}

pub fn col_num(letters: &str) -> i64 {
    let mut n: i64 = 0;
    for ch in letters.chars() {
        let u = ch.to_ascii_uppercase();
        n = n.saturating_mul(26).saturating_add(i64::from(u as u32) - 64);
    }
    n
}

pub fn col_name(mut n: i64) -> String {
    let mut s = Vec::new();
    while n > 0 {
        let r = (n - 1) % 26;
        n = (n - 1) / 26;
        s.push(char::from(b'A' + u8::try_from(r).unwrap_or(0)));
    }
    s.iter().rev().collect()
}

pub fn cell_name(row: i64, col: i64) -> String {
    format!("{}{row}", col_name(col))
}

pub fn range_name(r1: i64, c1: i64, r2: i64, c2: i64) -> String {
    let a = cell_name(r1, c1);
    if (r1, c1) == (r2, c2) { a } else { format!("{a}:{}", cell_name(r2, c2)) }
}

fn is_ident(name: &str) -> bool {
    let mut it = name.chars();
    let Some(first) = it.next() else { return false };
    (first.is_ascii_alphabetic() || first == '_') && it.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

fn looks_like_cell(name: &str) -> bool {
    let letters = name.chars().take_while(char::is_ascii_alphabetic).count();
    let digits = &name[letters..];
    (1..=3).contains(&letters) && !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

pub fn quote_sheet(name: &str) -> String {
    if is_ident(name) && !looks_like_cell(name) {
        return name.to_owned();
    }
    format!("'{}'", name.replace('\'', "''"))
}

/// `'Sheet 2'!A1:B3` → (`Some("Sheet 2")`, `"A1:B3"`); `A1` → (`None`, `"A1"`).
pub fn split_sheet(text: &str) -> R<(Option<String>, String)> {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix('\'') {
        let b = rest.as_bytes();
        let mut i = 0;
        loop {
            let Some(q) = rest[i..].find('\'').map(|q| q + i) else {
                return err(format!("{} has an unclosed quote around the sheet name.", repr_str(text)));
            };
            if b.get(q + 1) == Some(&b'\'') {
                i = q + 2;
                continue;
            }
            let name = rest[..q].replace("''", "'");
            let after = &rest[q + 1..];
            let Some(r) = after.strip_prefix('!') else {
                return err(format!("{}: expected ! after the sheet name, like 'My Sheet'!A1:B4.", repr_str(text)));
            };
            return Ok((Some(name), r.trim().to_owned()));
        }
    }
    if let Some(i) = text.rfind('!') {
        return Ok((Some(text[..i].trim().to_owned()), text[i + 1..].trim().to_owned()));
    }
    Ok((None, text.to_owned()))
}

/// `\$?([A-Za-z]{1,3})\$?(\d+)`, `\$?([A-Za-z]{1,3})`, `\$?(\d+)` (fullmatch): one side.
fn part(p: &str, whole: &str) -> R<(Option<i64>, Option<i64>)> {
    let s = p.strip_prefix('$').unwrap_or(p);
    let letters = s.chars().take_while(char::is_ascii_alphabetic).count();
    let rest = &s[letters..];
    let digits_ok = |d: &str| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit());
    if (1..=3).contains(&letters) {
        let rest2 = rest.strip_prefix('$').unwrap_or(rest);
        if digits_ok(rest2) {
            return Ok((Some(parse_digits(rest2)), Some(col_num(&s[..letters]))));
        }
        if rest.is_empty() {
            return Ok((None, Some(col_num(&s[..letters]))));
        }
    }
    if letters == 0 && digits_ok(s) {
        return Ok((Some(parse_digits(s)), None));
    }
    err(format!("{} isn't a cell or range; write it like A1, B2:D9, B:B, 3:3 or Sheet2!A1:C5.", repr_str(whole)))
}

fn parse_digits(d: &str) -> i64 {
    d.bytes().fold(0i64, |n, b| n.saturating_mul(10).saturating_add(i64::from(b - b'0')))
}

/// A parsed range; open sides are `None` (`B:B` has no rows, `2:2` no columns, `A2:A` no last row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Range {
    pub sheet: Option<String>,
    pub r1: Option<i64>,
    pub c1: Option<i64>,
    pub r2: Option<i64>,
    pub c2: Option<i64>,
}

pub fn parse_range(text: &str) -> R<Range> {
    if text.trim().is_empty() {
        return err("The range is empty; write it like A1:C10.".into());
    }
    let (sheet, r) = split_sheet(text)?;
    let r = r.replace(' ', "").replace("..", ":");
    let parts: Vec<&str> = r.split(':').collect();
    if parts.len() > 2 || parts.iter().any(|p| p.is_empty()) {
        return err(format!("{} isn't a cell or range; write it like A1, B2:D9, B:B, 3:3 or Sheet2!A1:C5.", repr_str(text)));
    }
    let (mut r1, mut c1) = part(parts[0], text)?;
    let (mut r2, mut c2) = part(parts[parts.len() - 1], text)?;
    if parts.len() == 1 && (r1.is_none() || c1.is_none()) {
        (r2, c2) = (r1, c1);
    }
    if let (Some(a), Some(b)) = (r1, r2)
        && b < a
    {
        (r1, r2) = (r2, r1);
    }
    if let (Some(a), Some(b)) = (c1, c2)
        && b < a
    {
        (c1, c2) = (c2, c1);
    }
    for r in [r1, r2].into_iter().flatten() {
        if !(1..=MAX_ROWS).contains(&r) {
            return err(format!("{}: rows go from 1 to {MAX_ROWS}.", repr_str(text)));
        }
    }
    for c in [c1, c2].into_iter().flatten() {
        if !(1..=MAX_COLS).contains(&c) {
            return err(format!("{}: columns go from A to XFD.", repr_str(text)));
        }
    }
    Ok(Range { sheet, r1, c1, r2, c2 })
}

/// A single cell → (sheet, row, col).
pub fn parse_cell(text: &str) -> R<(Option<String>, i64, i64)> {
    let r = parse_range(text)?;
    match (r.r1, r.c1) {
        (Some(row), Some(col)) => Ok((r.sheet, row, col)),
        _ => err(format!("{} isn't a single cell; write it like B2.", repr_str(text))),
    }
}

/// A column given as a letter (`"C"`), a number (`3`) or a cell (`"C1"`) → 3.
pub fn parse_col(value: &Value) -> R<i64> {
    let bad = || err(format!("{} isn't a column; give a letter like C.", repr(value)));
    let n = match value {
        Value::Bool(_) => return bad(),
        Value::Number(x) => x.as_i64().or_else(|| x.as_f64().map(|f| f.trunc() as i64)).unwrap_or(0),
        other => {
            let s = pystr(other).trim().replace('$', "");
            if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) {
                parse_digits(&s)
            } else {
                let letters = s.chars().take_while(char::is_ascii_alphabetic).count();
                let rest = &s[letters..];
                if !(1..=3).contains(&letters) || !rest.chars().all(|c| c.is_ascii_digit()) {
                    return bad();
                }
                col_num(&s[..letters])
            }
        }
    };
    if !(1..=MAX_COLS).contains(&n) {
        return err(format!("{} isn't a column; columns go from A to XFD.", repr(value)));
    }
    Ok(n)
}

pub fn parse_row(value: &Value) -> R<i64> {
    if value.is_boolean() {
        return err(format!("{} isn't a row number.", repr(value)));
    }
    let text = pystr(value);
    let t = text.trim();
    let n = match py_int(t) {
        Some(n) => n,
        None => {
            let s = t.strip_prefix('$').unwrap_or(t);
            let letters = s.chars().take_while(char::is_ascii_alphabetic).count();
            let rest = &s[letters..];
            let rest = rest.strip_prefix('$').unwrap_or(rest);
            if !(1..=3).contains(&letters) || rest.is_empty() || !rest.chars().all(|c| c.is_ascii_digit()) {
                return err(format!("{} isn't a row number; rows are 1, 2, 3, ...", repr(value)));
            }
            parse_digits(rest)
        }
    };
    if !(1..=MAX_ROWS).contains(&n) {
        return err(format!("{}: rows go from 1 to {MAX_ROWS}.", repr(value)));
    }
    Ok(n)
}

/// Python's `int(text)` for text (sign, digits, underscores between digits; no decimals).
pub fn py_int(t: &str) -> Option<i64> {
    let t = t.trim();
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    if digits.is_empty() || digits.starts_with('_') || digits.ends_with('_') || digits.contains("__") {
        return None;
    }
    let clean: String = digits.chars().filter(|c| *c != '_').collect();
    if !clean.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let n = parse_digits(&clean);
    Some(if neg { -n } else { n })
}

// ---- shifting formulas -------------------------------------------------------------------

fn word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// A `"…"` or `'…'` literal at `i` (`""`/`''` inside): its end, as Python's regex finds it.
fn quoted(b: &[u8], i: usize, q: u8) -> Option<usize> {
    let mut j = i + 1;
    let mut last_pair = None;
    loop {
        match b.get(j) {
            None => return last_pair.map(|p: usize| p + 1),
            Some(&c) if c == q => {
                if b.get(j + 1) == Some(&q) {
                    last_pair = Some(j);
                    j += 2;
                } else {
                    return Some(j + 1);
                }
            }
            Some(_) => j += 1,
        }
    }
}

enum Tok {
    Cell { cd: bool, c: (usize, usize), rd: bool, r: (usize, usize) },
    Cols { ad: bool, a: (usize, usize), bd: bool, b: (usize, usize) },
    Rows { pd: bool, p: (usize, usize), qd: bool, q: (usize, usize) },
}

fn letters_at(b: &[u8], i: usize) -> usize {
    b[i..].iter().take_while(|c| c.is_ascii_alphabetic()).count()
}

fn digits_at(b: &[u8], i: usize) -> usize {
    b[i..].iter().take_while(|c| c.is_ascii_digit()).count()
}

/// The reference token at `i` (end, token), if any (the three reference alternatives, in
/// order).
fn reference(b: &[u8], i: usize) -> Option<(usize, Tok)> {
    let prev = if i == 0 { None } else { b.get(i - 1).copied() };
    let lb = |extra: &[u8]| prev.is_none_or(|p| !(word(p) || p == b'.' || p == b'$' || extra.contains(&p)));
    let la = |at: usize, extra: &[u8]| b.get(at).is_none_or(|&c| !(word(c) || c == b'(' || c == b'!' || extra.contains(&c)));
    if lb(&[]) {
        // a cell: $?letters{1,3}$?digits+
        let cd = b.get(i) == Some(&b'$');
        let s = i + usize::from(cd);
        let n = letters_at(b, s);
        if (1..=3).contains(&n) {
            let mut j = s + n;
            let rd = b.get(j) == Some(&b'$');
            j += usize::from(rd);
            let d = digits_at(b, j);
            if d > 0 && la(j + d, &[]) {
                return Some((j + d, Tok::Cell { cd, c: (s, s + n), rd, r: (j, j + d) }));
            }
            // a column range: $?letters:$?letters
            if b.get(s + n) == Some(&b':') {
                let mut k = s + n + 1;
                let bd = b.get(k) == Some(&b'$');
                k += usize::from(bd);
                let m = letters_at(b, k);
                if (1..=3).contains(&m) && la(k + m, &[]) {
                    return Some((k + m, Tok::Cols { ad: cd, a: (s, s + n), bd, b: (k, k + m) }));
                }
            }
        }
    }
    if lb(b":") {
        let pd = b.get(i) == Some(&b'$');
        let s = i + usize::from(pd);
        let d = digits_at(b, s);
        if d > 0 && b.get(s + d) == Some(&b':') {
            let mut k = s + d + 1;
            let qd = b.get(k) == Some(&b'$');
            k += usize::from(qd);
            let e = digits_at(b, k);
            if e > 0 && la(k + e, b".:") {
                return Some((k + e, Tok::Rows { pd, p: (s, s + d), qd, q: (k, k + e) }));
            }
        }
    }
    None
}

/// Move the relative references of `formula` by (`drow`, `dcol`), like copying the cell
/// that far in Excel; `$` parts stay. A reference pushed off the sheet becomes `#REF!`.
pub fn shift_formula(formula: &str, drow: i64, dcol: i64) -> String {
    if !formula.starts_with('=') || (drow == 0 && dcol == 0) {
        return formula.to_owned();
    }
    let b = formula.as_bytes();
    let text = |(x, y): (usize, usize)| &formula[x..y];
    let col = |dollar: bool, letters: &str| -> Option<String> {
        if dollar {
            return Some(format!("${letters}"));
        }
        let n = col_num(letters) + dcol;
        (1..=MAX_COLS).contains(&n).then(|| col_name(n))
    };
    let row = |dollar: bool, digits: &str| -> Option<String> {
        if dollar {
            return Some(format!("${digits}"));
        }
        let n = parse_digits(digits) + drow;
        (1..=MAX_ROWS).contains(&n).then(|| n.to_string())
    };
    let mut out = String::with_capacity(formula.len() + 8);
    let mut i = 0;
    let mut copied = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'"' || c == b'\'' {
            if let Some(end) = quoted(b, i, c) {
                i = end;
                continue;
            }
        } else if let Some((end, tok)) = reference(b, i) {
            let new = match tok {
                Tok::Cell { cd, c, rd, r } => {
                    if col_num(text(c)) > MAX_COLS {
                        None
                    } else {
                        Some(col(cd, text(c)).zip(row(rd, text(r))).map_or_else(|| "#REF!".to_owned(), |(a, b)| a + &b))
                    }
                }
                Tok::Cols { ad, a, bd, b } => Some(col(ad, text(a)).zip(col(bd, text(b))).map_or_else(|| "#REF!".to_owned(), |(x, y)| format!("{x}:{y}"))),
                Tok::Rows { pd, p, qd, q } => Some(row(pd, text(p)).zip(row(qd, text(q))).map_or_else(|| "#REF!".to_owned(), |(x, y)| format!("{x}:{y}"))),
            };
            if let Some(new) = new {
                out.push_str(&formula[copied..i]);
                out.push_str(&new);
                copied = end;
            }
            i = end;
            continue;
        }
        // advance one character
        i += formula[i..].chars().next().map_or(1, char::len_utf8);
    }
    out.push_str(&formula[copied..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("B:B").unwrap(), Range { sheet: None, r1: None, c1: Some(2), r2: None, c2: Some(2) });
        assert_eq!(parse_range("'My ''x'''!a3:B1").unwrap(), Range { sheet: Some("My 'x'".into()), r1: Some(1), c1: Some(1), r2: Some(3), c2: Some(2) });
        assert_eq!(parse_range("A2:A").unwrap(), Range { sheet: None, r1: Some(2), c1: Some(1), r2: None, c2: Some(1) });
        assert_eq!(parse_range("3").unwrap(), Range { sheet: None, r1: Some(3), c1: None, r2: Some(3), c2: None });
        assert!(parse_range("hello world").unwrap_err().0.contains("isn't a cell or range"));
        assert_eq!(parse_col(&json!("c1")).unwrap(), 3);
        assert_eq!(parse_row(&json!("B7")).unwrap(), 7);
        assert_eq!(parse_row(&json!(2)).unwrap(), 2);
        assert_eq!(quote_sheet("Sheet 2"), "'Sheet 2'");
        assert_eq!(quote_sheet("AB12"), "'AB12'");
        assert_eq!(quote_sheet("Data"), "Data");
    }

    #[test]
    fn shifts_like_excel() {
        assert_eq!(shift_formula("=B2*C2", 11, 0), "=B13*C13");
        assert_eq!(shift_formula("=D2/$D$14", 11, 0), "=D13/$D$14");
        assert_eq!(shift_formula("=$B2+B$1", 12, 1), "=$B14+C$1");
        assert_eq!(shift_formula("=SUM(A1:A3)+LOG10(A1)", 1, 0), "=SUM(A2:A4)+LOG10(A2)");
        assert_eq!(shift_formula("=\"A1\"&'Sheet 1'!A1", 1, 0), "=\"A1\"&'Sheet 1'!A2");
        assert_eq!(shift_formula("=SUM(B:C)+SUM(2:3)", 1, 1), "=SUM(C:D)+SUM(3:4)");
        assert_eq!(shift_formula("=A1", -1, 0), "=#REF!");
        assert_eq!(shift_formula("=ZZZ1+A1", 1, 0), "=ZZZ1+A2");
        assert_eq!(shift_formula("=Sheet2!A1", 1, 0), "=Sheet2!A2");
    }
}
