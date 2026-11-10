//! Readable previews of tool calls for approval cards: content excerpts, compact line
//! diffs, and key arguments. Everything is bounded; inputs are untrusted.

pub use byob_protocol::{DiffLine, LineKind, MAX_PREVIEW_LINE_CHARS, MAX_PREVIEW_LINES, Preview};
use serde_json::Value;

/// Unchanged lines kept around each change in a diff.
const CONTEXT: usize = 2;
/// Most cells of the LCS table; bigger diffs fall back to "all removed, all added".
const MAX_LCS_CELLS: usize = 1_000_000;
/// Most top-level arguments listed for a generic tool.
const MAX_FIELDS: usize = 8;
/// Argument names whose value is code or a command, shown monospaced.
const CODE_KEYS: &[&str] = &["command", "cmd", "code", "script", "source", "sql", "query", "program", "shell", "python"];
/// Most characters of a code block.
const MAX_CODE_CHARS: usize = 4000;

/// One line, control characters removed (tabs become spaces), capped.
pub fn clip_line(s: &str) -> String {
    let mut out = String::new();
    let mut n = 0;
    for c in s.chars() {
        let c = if c == '\t' { ' ' } else { c };
        if c.is_control() {
            continue;
        }
        if n == MAX_PREVIEW_LINE_CHARS {
            out.push('…');
            break;
        }
        out.push(c);
        n += 1;
    }
    out
}

fn line(kind: LineKind, text: &str) -> DiffLine {
    DiffLine { kind, text: clip_line(text) }
}

/// The first lines of `text`; returns them and how many were left out.
pub fn head(text: &str) -> (Vec<DiffLine>, usize) {
    let total = text.lines().count();
    let lines: Vec<DiffLine> = text.lines().take(MAX_PREVIEW_LINES).map(|l| line(LineKind::Same, l)).collect();
    let more = total.saturating_sub(lines.len());
    (lines, more)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Same,
    Del,
    Add,
}

/// Line-level edit script from `a` to `b`: (op, index into a or b).
fn script<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<(Op, usize)> {
    // common prefix and suffix first: most edits are local
    let pre = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let rest_a = a.get(pre..).unwrap_or_default();
    let rest_b = b.get(pre..).unwrap_or_default();
    let suf = rest_a.iter().rev().zip(rest_b.iter().rev()).take_while(|(x, y)| x == y).count();
    let mid_a = rest_a.get(..rest_a.len() - suf).unwrap_or_default();
    let mid_b = rest_b.get(..rest_b.len() - suf).unwrap_or_default();
    let mut out: Vec<(Op, usize)> = (0..pre).map(|i| (Op::Same, i)).collect();
    let (n, m) = (mid_a.len(), mid_b.len());
    if n.saturating_mul(m) <= MAX_LCS_CELLS && n > 0 && m > 0 {
        // lcs[i][j] = LCS length of mid_a[i..], mid_b[j..]
        let w = m + 1;
        let mut lcs = vec![0u32; (n + 1) * w];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                let v = if mid_a.get(i) == mid_b.get(j) {
                    lcs.get((i + 1) * w + j + 1).copied().unwrap_or(0).saturating_add(1)
                } else {
                    let down = lcs.get((i + 1) * w + j).copied().unwrap_or(0);
                    let right = lcs.get(i * w + j + 1).copied().unwrap_or(0);
                    down.max(right)
                };
                if let Some(cell) = lcs.get_mut(i * w + j) {
                    *cell = v;
                }
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n || j < m {
            if i < n && j < m && mid_a.get(i) == mid_b.get(j) {
                out.push((Op::Same, pre + i));
                i += 1;
                j += 1;
            } else if i < n && (j == m || lcs.get((i + 1) * w + j) >= lcs.get(i * w + j + 1)) {
                // removals before additions, like unified diffs
                out.push((Op::Del, pre + i));
                i += 1;
            } else {
                out.push((Op::Add, pre + j));
                j += 1;
            }
        }
    } else {
        out.extend((0..n).map(|i| (Op::Del, pre + i)));
        out.extend((0..m).map(|j| (Op::Add, pre + j)));
    }
    out.extend((0..suf).map(|k| (Op::Same, pre + n + k)));
    out
}

/// A compact line diff from `old` to `new`: changed lines with a little context, runs of
/// unchanged lines collapsed. Returns the lines and how many changed lines were left out.
pub fn diff(old: &str, new: &str) -> (Vec<DiffLine>, usize) {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let ops = script(&a, &b);
    // keep changes and CONTEXT lines around them
    let changed: Vec<bool> = ops.iter().map(|(op, _)| *op != Op::Same).collect();
    let keep = |k: usize| {
        let lo = k.saturating_sub(CONTEXT);
        let hi = (k + CONTEXT + 1).min(changed.len());
        changed.get(lo..hi).is_some_and(|w| w.iter().any(|c| *c))
    };
    let mut out = Vec::new();
    let mut skipped = 0usize;
    let mut more = 0usize;
    for (k, (op, idx)) in ops.iter().enumerate() {
        if !keep(k) {
            skipped += 1;
            continue;
        }
        if out.len() >= MAX_PREVIEW_LINES {
            if *op != Op::Same {
                more += 1;
            }
            continue;
        }
        if skipped > 0 {
            out.push(skipped_line(skipped));
            skipped = 0;
        }
        out.push(match op {
            Op::Same => line(LineKind::Same, a.get(*idx).copied().unwrap_or_default()),
            Op::Del => line(LineKind::Removed, a.get(*idx).copied().unwrap_or_default()),
            Op::Add => line(LineKind::Added, b.get(*idx).copied().unwrap_or_default()),
        });
    }
    if skipped > 0 && out.len() < MAX_PREVIEW_LINES && !out.is_empty() {
        out.push(skipped_line(skipped));
    }
    (out, more)
}

fn skipped_line(n: usize) -> DiffLine {
    let text = if n == 1 { "1 unchanged line".to_owned() } else { format!("{n} unchanged lines") };
    DiffLine { kind: LineKind::Skipped, text }
}

/// A short one-line rendering of a JSON value.
fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => clip_line(&s.replace(['\n', '\r'], " ")),
        other => clip_line(&other.to_string()),
    }
}

/// Code-ish text for a monospace block: bounded in characters and lines.
fn code_text(s: &str) -> String {
    let mut out = String::new();
    for (i, l) in s.lines().enumerate() {
        if i == MAX_PREVIEW_LINES || out.len() > MAX_CODE_CHARS {
            out.push_str("\n…");
            break;
        }
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&clip_line(l));
    }
    out
}

/// The default preview for any tool: its key arguments; a command or script argument (or
/// any multi-line string) is shown as code.
pub fn args(args: &Value) -> Preview {
    let Value::Object(map) = args else {
        return Preview::Args { fields: vec![("arguments".into(), value_text(args))], code: None, more: 0 };
    };
    let code_key = CODE_KEYS
        .iter()
        .find_map(|k| map.iter().find(|(name, v)| name.eq_ignore_ascii_case(k) && v.is_string()).map(|(name, _)| name.clone()))
        .or_else(|| map.iter().find(|(_, v)| v.as_str().is_some_and(|s| s.contains('\n'))).map(|(name, _)| name.clone()));
    let code = code_key.as_ref().and_then(|k| map.get(k)).and_then(Value::as_str).map(code_text);
    let mut fields = Vec::new();
    let mut more = 0;
    for (name, v) in map.iter().filter(|(name, _)| Some(*name) != code_key.as_ref()) {
        if v.is_null() {
            continue;
        }
        if fields.len() == MAX_FIELDS {
            more += 1;
            continue;
        }
        fields.push((clip_line(name), value_text(v)));
    }
    Preview::Args { fields, code, more }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn render(lines: &[DiffLine]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                let mark = match l.kind {
                    LineKind::Same => ' ',
                    LineKind::Added => '+',
                    LineKind::Removed => '-',
                    LineKind::Skipped => '~',
                };
                format!("{mark}{}", l.text)
            })
            .collect()
    }

    #[test]
    fn diff_shows_changes_with_context() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\n";
        let new = "a\nb\nc\nd\nE\nf\ng\nh\ni\n";
        let (lines, more) = diff(old, new);
        assert_eq!(more, 0);
        assert_eq!(render(&lines), ["~2 unchanged lines", " c", " d", "-e", "+E", " f", " g", " h", "+i"]);
    }

    #[test]
    fn diff_of_equal_and_empty() {
        assert!(diff("same\n", "same\n").0.is_empty());
        assert_eq!(render(&diff("", "x\ny").0), ["+x", "+y"]);
        assert_eq!(render(&diff("x\ny", "").0), ["-x", "-y"]);
    }

    #[test]
    fn diff_interleaved_lcs() {
        let (lines, _) = diff("1\n2\n3\n4\n", "1\nx\n3\ny\n");
        assert_eq!(render(&lines), [" 1", "-2", "+x", " 3", "-4", "+y"]);
    }

    #[test]
    fn diff_is_bounded() {
        let old: String = (0..5000).map(|i| format!("line {i}\n")).collect();
        let new: String = (0..5000).map(|i| format!("LINE {i}\n")).collect();
        let (lines, more) = diff(&old, &new);
        assert_eq!(lines.len(), MAX_PREVIEW_LINES);
        assert_eq!(more, 10_000 - MAX_PREVIEW_LINES);
        let long = "x".repeat(10_000);
        let (lines, _) = diff("", &long);
        assert_eq!(lines[0].text.chars().count(), MAX_PREVIEW_LINE_CHARS + 1);
    }

    #[test]
    fn head_is_bounded() {
        let text: String = (0..100).map(|i| format!("{i}\n")).collect();
        let (lines, more) = head(&text);
        assert_eq!((lines.len(), more), (MAX_PREVIEW_LINES, 100 - MAX_PREVIEW_LINES));
        assert_eq!(lines[0], DiffLine { kind: LineKind::Same, text: "0".into() });
    }

    #[test]
    fn generic_args() {
        let p = args(&json!({"command": "ls -la\nrm x", "cwd": "/tmp", "n": 3, "skip": null}));
        assert_eq!(p, Preview::Args { fields: vec![("cwd".into(), "/tmp".into()), ("n".into(), "3".into())], code: Some("ls -la\nrm x".into()), more: 0 });
        let Preview::Args { code: None, fields, .. } = args(&json!({"q": "a\u{7}b"})) else { panic!() };
        assert_eq!(fields, [("q".to_owned(), "ab".to_owned())]);
        let many: serde_json::Map<String, Value> = (0..20).map(|i| (format!("k{i:02}"), json!(i))).collect();
        let Preview::Args { fields, more, .. } = args(&Value::Object(many)) else { panic!() };
        assert_eq!((fields.len(), more), (MAX_FIELDS, 12));
        assert!(matches!(args(&json!([1, 2])), Preview::Args { .. }));
    }
}
