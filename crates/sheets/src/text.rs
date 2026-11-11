//! How replies spell values: Python's conventions (`repr`, `str`, `json.dumps`, `%.10g`), the
//! format the tools' replies have had since the backend was a Python plugin (and that models
//! read well: `'Sheet1'`, `True`, `5.0`), kept so they stay stable.

use serde_json::Value;

/// Python's `repr()` of a string: single quotes unless it has some and no double ones.
pub fn repr_str(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Python's `repr()` of a float.
pub fn float_repr(v: f64) -> String {
    if v.is_nan() {
        return "nan".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    let e = format!("{v:e}"); // shortest round-trip digits: "1.5e-5"
    let (mant, exp) = e.split_once('e').unwrap_or((&e, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    if !(-4..16).contains(&exp) {
        let sign = if exp < 0 { '-' } else { '+' };
        return format!("{mant}e{sign}{:02}", exp.unsigned_abs());
    }
    let s = format!("{v}");
    if s.contains('.') { s } else { format!("{s}.0") }
}

fn num_repr(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    float_repr(n.as_f64().unwrap_or(0.0))
}

/// Python's `repr()` of a JSON value as Python sees it (dicts, lists, str, int, float, bool, None).
pub fn repr(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True" } else { "False" }.into(),
        Value::Number(n) => num_repr(n),
        Value::String(s) => repr_str(s),
        Value::Array(a) => format!("[{}]", a.iter().map(repr).collect::<Vec<_>>().join(", ")),
        Value::Object(o) => format!("{{{}}}", o.iter().map(|(k, v)| format!("{}: {}", repr_str(k), repr(v))).collect::<Vec<_>>().join(", ")),
    }
}

/// Python's `str()`.
pub fn pystr(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => repr(other),
    }
}

/// Python's `json.dumps()` with its defaults (`", "`, `": "`, ASCII only).
pub fn json_dumps(v: &Value) -> String {
    fn string(s: &str, out: &mut String) {
        out.push('"');
        for ch in s.chars() {
            match ch {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                '\u{8}' => out.push_str("\\b"),
                '\u{c}' => out.push_str("\\f"),
                c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                    let mut buf = [0u16; 2];
                    for u in c.encode_utf16(&mut buf) {
                        out.push_str(&format!("\\u{u:04x}"));
                    }
                }
                c => out.push(c),
            }
        }
        out.push('"');
    }
    fn go(v: &Value, out: &mut String) {
        match v {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Number(n) => out.push_str(&num_repr(n)),
            Value::String(s) => string(s, out),
            Value::Array(a) => {
                out.push('[');
                for (i, x) in a.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    go(x, out);
                }
                out.push(']');
            }
            Value::Object(o) => {
                out.push('{');
                for (i, (k, x)) in o.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    string(k, out);
                    out.push_str(": ");
                    go(x, out);
                }
                out.push('}');
            }
        }
    }
    let mut out = String::new();
    go(v, &mut out);
    out
}

/// Python's `f"{v:.{p}g}"`.
pub fn fmt_g(v: f64, p: usize) -> String {
    if !v.is_finite() {
        return float_repr(v);
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0".into() } else { "0".into() };
    }
    let p = p.max(1);
    let e = format!("{:.*e}", p - 1, v);
    let (mant, exp) = e.split_once('e').unwrap_or((&e, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let strip = |s: &str| -> String { if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_owned() } else { s.to_owned() } };
    if exp < -4 || exp >= i32::try_from(p).unwrap_or(i32::MAX) {
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{}e{sign}{:02}", strip(mant), exp.unsigned_abs())
    } else {
        let decimals = usize::try_from(i32::try_from(p).unwrap_or(0) - 1 - exp).unwrap_or(0);
        strip(&format!("{v:.decimals$}"))
    }
}

/// Python's `str.strip().lower()` (close enough: Unicode whitespace, Unicode lowercase).
pub fn strip_lower(s: &str) -> String {
    s.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn like_python() {
        assert_eq!(repr_str("a'b"), "\"a'b\"");
        assert_eq!(repr_str("a\"b'"), "'a\"b\\''");
        assert_eq!(repr(&json!(["x", 1, 2.5, true, null])), "['x', 1, 2.5, True, None]");
        assert_eq!(float_repr(5.0), "5.0");
        assert_eq!(float_repr(1e16), "1e+16");
        assert_eq!(float_repr(1.5e-5), "1.5e-05");
        assert_eq!(float_repr(0.0001), "0.0001");
        assert_eq!(float_repr(1e15), "1000000000000000.0");
        assert_eq!(json_dumps(&json!({"a": [1, 2.0, "é"]})), "{\"a\": [1, 2.0, \"\\u00e9\"]}");
        assert_eq!(fmt_g(0.125, 10), "0.125");
        assert_eq!(fmt_g(1234567.5, 10), "1234567.5");
        assert_eq!(fmt_g(1.0 / 3.0, 10), "0.3333333333");
        assert_eq!(fmt_g(1e20, 10), "1e+20");
        assert_eq!(fmt_g(1.5e-7, 10), "1.5e-07");
        assert_eq!(fmt_g(12345678901.5, 10), "1.23456789e+10");
    }
}
