//! Find/replace edits on text, shared by `edit_file` (workspace files) and `update_artifact`.
//!
//! Each `find` is looked up exactly first. When it isn't there, a whitespace-tolerant match
//! is tried: runs of spaces, tabs and line breaks count as one space and the ends of `find`
//! are trimmed, so a `find` copied with different indentation or line endings still lands.
//! The replacement is then re-indented to the file's indentation. Files that use CRLF
//! throughout are edited as LF and written back as CRLF.
//!
//! Errors are written for the model: what to fix, never a partial result.

use std::fmt::Write as _;

/// Find/replace edits in one call.
pub const MAX_EDITS: usize = 50;
/// Lines looked at when suggesting what a missing `find` might have meant.
const MAX_SCAN_LINES: usize = 50_000;
/// Close lines suggested for a missing `find`.
const MAX_CANDIDATES: usize = 3;
/// Line numbers listed for an ambiguous `find`.
const MAX_LISTED: usize = 6;

/// How [`apply`] runs and what its messages say.
#[derive(Debug, Clone, Copy)]
pub struct Opts<'a> {
    /// Replace every occurrence of a `find` instead of requiring exactly one.
    pub replace_all: bool,
    /// The model may set `replace_all` (mentioned in the "occurs N times" error).
    pub offer_replace_all: bool,
    /// Largest result in bytes.
    pub max_bytes: usize,
    /// What is edited, for messages: "notes.md", "the current version".
    pub what: &'a str,
    /// How to see the current text, for messages: "read_file", "read_artifact".
    pub reread: &'a str,
}

/// The result of [`apply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub text: String,
    /// Places replaced, over all edits.
    pub replaced: usize,
    /// Edits (numbered from 1) that only matched ignoring whitespace.
    pub loose: Vec<usize>,
}

/// Apply `(find, replace)` edits in order; each sees the text as the earlier ones left it.
/// Either all apply or none does.
pub fn apply(text: &str, edits: &[(String, String)], opts: &Opts<'_>) -> Result<Applied, String> {
    if edits.is_empty() {
        return Err("edits is empty; give at least one {find, replace}".into());
    }
    if edits.len() > MAX_EDITS {
        return Err(format!("too many edits ({}; at most {MAX_EDITS}); send fewer, or rewrite the whole file", edits.len()));
    }
    let crlf = is_crlf(text);
    let mut out = if crlf { text.replace("\r\n", "\n") } else { text.to_owned() };
    let mut replaced = 0usize;
    let mut loose = Vec::new();
    for (i, (find, replace)) in edits.iter().enumerate() {
        let k = i.saturating_add(1);
        let (find, replace) = if crlf { (find.replace("\r\n", "\n"), replace.replace("\r\n", "\n")) } else { (find.clone(), replace.clone()) };
        if find.is_empty() {
            return Err(format!("edit {k}: `find` is empty; give the text to replace"));
        }
        let (next, n, was_loose) = apply_one(&out, &find, &replace, k, opts)?;
        if next.len() > opts.max_bytes {
            return Err(format!("the edits make {} too big (over {} bytes)", opts.what, opts.max_bytes));
        }
        out = next;
        replaced = replaced.saturating_add(n);
        if was_loose {
            loose.push(k);
        }
    }
    if crlf {
        out = out.replace('\n', "\r\n");
    }
    Ok(Applied { text: out, replaced, loose })
}

/// Every line break is CRLF (and there is at least one).
fn is_crlf(text: &str) -> bool {
    let lf = text.matches('\n').count();
    lf > 0 && text.matches("\r\n").count() == lf
}

/// One edit: the new text, places replaced, and whether it matched loosely.
fn apply_one(text: &str, find: &str, replace: &str, k: usize, opts: &Opts<'_>) -> Result<(String, usize, bool), String> {
    let exact: Vec<usize> = text.match_indices(find).map(|(i, _)| i).collect();
    if exact.len() == 1 || (opts.replace_all && !exact.is_empty()) {
        let n = exact.len();
        return Ok((text.replace(find, replace), n, false));
    }
    if exact.len() > 1 {
        return Err(ambiguous(text, &exact, k, opts, ""));
    }
    let spans = loose_matches(text, find);
    if spans.is_empty() {
        return Err(not_found(text, find, k, opts));
    }
    if spans.len() > 1 && !opts.replace_all {
        let starts: Vec<usize> = spans.iter().map(|s| s.0).collect();
        return Err(ambiguous(text, &starts, k, opts, " (ignoring whitespace)"));
    }
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for &(start, end) in &spans {
        let (start, end, rep) = loose_replacement(text, start, end, find, replace);
        out.push_str(text.get(last..start).unwrap_or_default());
        out.push_str(&rep);
        last = end;
    }
    out.push_str(text.get(last..).unwrap_or_default());
    Ok((out, spans.len(), true))
}

/// `text` with whitespace runs collapsed to one space, and for each byte of it the byte
/// range of `text` it came from.
struct Collapsed {
    s: String,
    from: Vec<(usize, usize)>,
}

fn collapse(text: &str) -> Collapsed {
    let mut s = String::with_capacity(text.len());
    let mut from = Vec::with_capacity(text.len());
    let mut run: Option<(usize, usize)> = None;
    for (i, c) in text.char_indices() {
        let end = i.saturating_add(c.len_utf8());
        if c.is_whitespace() {
            run = Some(run.map_or((i, end), |(a, _)| (a, end)));
            continue;
        }
        if let Some(r) = run.take() {
            s.push(' ');
            from.push(r);
        }
        s.push(c);
        from.extend(std::iter::repeat_n((i, end), c.len_utf8()));
    }
    if let Some(r) = run {
        s.push(' ');
        from.push(r);
    }
    Collapsed { s, from }
}

/// Byte spans of `text` matching `find` with whitespace differences ignored; ends trimmed.
fn loose_matches(text: &str, find: &str) -> Vec<(usize, usize)> {
    let needle = collapse(find.trim()).s;
    if needle.is_empty() {
        return Vec::new();
    }
    let hay = collapse(text);
    let mut spans = Vec::new();
    for (i, m) in hay.s.match_indices(needle.as_str()) {
        let (Some(first), Some(last)) = (hay.from.get(i), hay.from.get(i.saturating_add(m.len()).saturating_sub(1))) else { continue };
        spans.push((first.0, last.1));
    }
    spans
}

fn indent(line: &str) -> &str {
    let n = line.len().saturating_sub(line.trim_start_matches([' ', '\t']).len());
    line.get(..n).unwrap_or_default()
}

/// The span to replace and its replacement for a loose match of `find` at `start..end`:
/// `replace` loses the whitespace `find` had around it, takes the file's indentation, and
/// deleting whole lines takes their line breaks too.
fn loose_replacement(text: &str, start: usize, end: usize, find: &str, replace: &str) -> (usize, usize, String) {
    let lead = find.len().saturating_sub(find.trim_start().len());
    let trail = find.len().saturating_sub(find.trim_end().len());
    let mut rep = replace;
    if let Some(r) = rep.strip_prefix(find.get(..lead).unwrap_or_default()) {
        rep = r;
    }
    if let Some(r) = rep.strip_suffix(find.get(find.len().saturating_sub(trail)..).unwrap_or_default()) {
        rep = r;
    }
    let matched = text.get(start..end).unwrap_or_default();
    let mut out = reindent(find.trim(), matched, rep);
    let (mut start, mut end) = (start, end);
    let line_start = text.get(..start).and_then(|s| s.rfind('\n')).map_or(0, |i| i.saturating_add(1));
    let before = text.get(line_start..start).unwrap_or_default();
    let after = text.get(end..).unwrap_or_default();
    let rest = after.split('\n').next().unwrap_or_default();
    if out.trim().is_empty() && before.trim().is_empty() && rest.trim().is_empty() {
        // the edit deletes whole lines: drop them, not just their text
        out.clear();
        start = line_start;
        end = end.saturating_add(rest.len());
        if text.get(end..).is_some_and(|t| t.starts_with('\n')) {
            end = end.saturating_add(1);
        } else if start > 0 {
            start = start.saturating_sub(1);
        }
    }
    (start, end, out)
}

/// Re-indent the lines of `rep` after its first the way the file's lines (`matched`) are
/// indented where `find` had the same indentation: each indentation `find` used maps to the
/// file's; a deeper one keeps its extra part. Only when `find` and `matched` have the same
/// lines (the first line of `find` may start mid-line, so it says nothing).
fn reindent(find: &str, matched: &str, rep: &str) -> String {
    let f: Vec<&str> = find.split('\n').collect();
    let m: Vec<&str> = matched.split('\n').collect();
    if f.len() != m.len() || !rep.contains('\n') {
        return rep.to_owned();
    }
    let mut map: Vec<(&str, &str)> = Vec::new();
    for (a, b) in f.iter().zip(&m).skip(1).filter(|(a, _)| !a.trim().is_empty()) {
        if !map.iter().any(|(k, _)| *k == indent(a)) {
            map.push((indent(a), indent(b)));
        }
    }
    if map.iter().all(|(k, v)| k == v) {
        return rep.to_owned();
    }
    // longest first, so the closest indentation wins
    map.sort_by_key(|(k, _)| std::cmp::Reverse(k.len()));
    let mut out = String::with_capacity(rep.len());
    for (i, line) in rep.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
            let ind = indent(line);
            if !line.trim().is_empty()
                && let Some((k, v)) = map.iter().find(|(k, _)| ind.starts_with(k))
            {
                out.push_str(v);
                out.push_str(line.get(k.len()..).unwrap_or_default());
                continue;
            }
        }
        out.push_str(line);
    }
    out
}

/// 1-based line of byte offset `at`.
fn line_of(text: &str, at: usize) -> usize {
    text.get(..at).map_or(0, |s| s.matches('\n').count()).saturating_add(1)
}

fn ambiguous(text: &str, starts: &[usize], k: usize, opts: &Opts<'_>, how: &str) -> String {
    let mut lines: Vec<String> = starts.iter().take(MAX_LISTED).map(|&s| line_of(text, s).to_string()).collect();
    if starts.len() > MAX_LISTED {
        lines.push("…".into());
    }
    let all = if opts.offer_replace_all { ", or set replace_all to change every one" } else { "" };
    format!(
        "edit {k}: `find` occurs {} times in {}{how} (lines {}). Include more of the surrounding text so it occurs once{all}. No edits were applied.",
        starts.len(),
        opts.what,
        lines.join(", ")
    )
}

fn not_found(text: &str, find: &str, k: usize, opts: &Opts<'_>) -> String {
    let mut msg = format!("edit {k}: `find` is not in {}, not even ignoring whitespace: {:?}.", opts.what, clip(find.trim(), 120));
    let cands = candidates(text, find);
    if !cands.is_empty() {
        msg.push_str(" Closest lines:");
        for (n, line) in cands {
            let _ = write!(msg, "\n{n}: {}", clip(line.trim(), 160));
        }
        msg.push('\n');
    } else {
        msg.push(' ');
    }
    let _ = write!(msg, "Copy `find` from the current text ({} shows it). No edits were applied.", opts.reread);
    msg
}

/// Lines of `text` most like the first non-blank line of `find`, best first.
fn candidates<'t>(text: &'t str, find: &str) -> Vec<(usize, &'t str)> {
    let Some(target) = find.lines().map(str::trim).find(|l| !l.is_empty()) else { return Vec::new() };
    let target = bigrams(target);
    if target.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(u32, usize, &str)> = text
        .lines()
        .take(MAX_SCAN_LINES)
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .filter_map(|(i, l)| {
            let s = dice(&target, &bigrams(l.trim()));
            (s >= 400).then_some((s, i.saturating_add(1), l))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().take(MAX_CANDIDATES).map(|(_, n, l)| (n, l)).collect()
}

/// Lower-cased character pairs of `s` (its first 300 characters), sorted.
fn bigrams(s: &str) -> Vec<(char, char)> {
    let cs: Vec<char> = s.chars().take(300).flat_map(char::to_lowercase).filter(|c| !c.is_whitespace()).collect();
    let mut out: Vec<(char, char)> = cs.windows(2).filter_map(|w| Some((*w.first()?, *w.get(1)?))).collect();
    out.sort_unstable();
    out
}

/// Dice similarity of two sorted bigram lists, in thousandths.
fn dice(a: &[(char, char)], b: &[(char, char)]) -> u32 {
    let total = a.len().saturating_add(b.len());
    if total == 0 {
        return 0;
    }
    let (mut i, mut j, mut common) = (0, 0, 0usize);
    while let (Some(x), Some(y)) = (a.get(i), b.get(j)) {
        match x.cmp(y) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                common += 1;
                i += 1;
                j += 1;
            }
        }
    }
    u32::try_from(common.saturating_mul(2000) / total).unwrap_or(0)
}

fn clip(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().nth(max).is_some() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPTS: Opts<'static> = Opts { replace_all: false, offer_replace_all: true, max_bytes: 1 << 20, what: "a.txt", reread: "read_file" };

    fn ed(text: &str, edits: &[(&str, &str)]) -> Result<Applied, String> {
        let e: Vec<(String, String)> = edits.iter().map(|(a, b)| ((*a).into(), (*b).into())).collect();
        apply(text, &e, &OPTS)
    }

    fn ok(text: &str, edits: &[(&str, &str)]) -> String {
        ed(text, edits).unwrap().text
    }

    #[test]
    fn exact_once_in_order() {
        let t = "a b a c";
        assert_eq!(ok(t, &[("b", "B"), ("c", "C")]), "a B a C");
        assert_eq!(ok(t, &[("b", "q"), ("q a", "r")]), "a r c");
        assert!(ed(t, &[]).is_err());
        assert!(ed(t, &[("", "x")]).is_err());
        let many = vec![("b", "B"); MAX_EDITS + 1];
        assert!(ed(t, &many).unwrap_err().contains("too many"));
        let big = "x".repeat(2 << 20);
        assert!(ed("ab", &[("a", &big)]).unwrap_err().contains("too big"));
    }

    #[test]
    fn ambiguous_says_how_many_and_where() {
        let e = ed("x\nfoo\ny\nfoo\nfoo\n", &[("foo", "bar")]).unwrap_err();
        assert!(e.contains("occurs 3 times") && e.contains("lines 2, 4, 5") && e.contains("replace_all"), "{e}");
        let no_all = Opts { offer_replace_all: false, ..OPTS };
        assert!(!apply("a a", &[("a".into(), "b".into())], &no_all).unwrap_err().contains("replace_all"));
        let all = Opts { replace_all: true, ..OPTS };
        let r = apply("foo foo\nfoo", &[("foo".into(), "bar".into())], &all).unwrap();
        assert_eq!((r.text.as_str(), r.replaced), ("bar bar\nbar", 3));
    }

    #[test]
    fn not_found_suggests_close_lines() {
        let t = "fn main() {\n    let total = compute(1, 2);\n    println!(\"{total}\");\n}\n";
        let e = ed(t, &[("let totl = compute(1,2);", "x")]).unwrap_err();
        assert!(e.contains("not in a.txt") && e.contains("2: let total = compute(1, 2);") && e.contains("read_file"), "{e}");
        assert!(!e.contains("1: fn main"), "{e}");
        let e = ed(t, &[("zzzzqqq", "x")]).unwrap_err();
        assert!(!e.contains("Closest"), "{e}");
    }

    #[test]
    fn whitespace_tolerant_fallback() {
        // different indentation and trailing spaces
        let t = "def f():\n        x = 1   \n        return x\n";
        let r = ed(t, &[("x = 1\n    return x", "x = 2\n    return x + 1")]).unwrap();
        assert_eq!(r.text, "def f():\n        x = 2\n        return x + 1\n");
        assert_eq!(r.loose, vec![1]);
        // inside a line, collapsed spaces
        assert_eq!(ok("call(a,   b)", &[("call(a, b)", "call(b, a)")]), "call(b, a)");
        // but a space never matches no space
        assert!(ed("call(a,b)", &[("call(a, b)", "x")]).is_err());
        // the model's find/replace with surrounding newlines
        assert_eq!(ok("a\n  b\nc\n", &[("\nb\n", "\nB\n")]), "a\n  B\nc\n");
        // ambiguous when loose
        let e = ed("  x  y\n x y\n", &[("x\ty", "z")]).unwrap_err();
        assert!(e.contains("2 times") && e.contains("ignoring whitespace"), "{e}");
        // whitespace-only find
        assert!(ed("a b", &[("   ", "x")]).is_err());
    }

    #[test]
    fn reindents_replacement_to_the_file() {
        // model dropped one level of indentation everywhere
        let t = "class A:\n    def f(self):\n        pass\n";
        let r = ok(t, &[("def f(self):\n    pass", "def f(self):\n    if True:\n        pass")]);
        assert_eq!(r, "class A:\n    def f(self):\n        if True:\n            pass\n");
        // tabs in the file, spaces from the model
        let t = "fn a() {\n\tif x {\n\t\ty();\n\t}\n}";
        let r = ok(t, &[("if x {\n    y();\n}", "if x {\n    y();\n    z();\n}")]);
        assert_eq!(r, "fn a() {\n\tif x {\n\t\ty();\n\t\tz();\n\t}\n}");
    }

    #[test]
    fn loose_delete_removes_whole_lines() {
        let t = "a\n    gone();\nb\n";
        // exact matches are taken literally
        assert_eq!(ok(t, &[("gone();\n", "")]), "a\n    b\n");
        assert_eq!(ok(t, &[("gone();  \n", "")]), "a\nb\n");
        assert_eq!(ok("a\n  last", &[("last ", "")]), "a");
        // part of a line: only that part
        assert_eq!(ok("keep  gone", &[("keep gone", "keep")]), "keep");
    }

    #[test]
    fn crlf_files_stay_crlf() {
        let t = "one\r\ntwo\r\nthree\r\n";
        let r = ed(t, &[("two\nthree", "2\n3\n3.5")]).unwrap();
        assert_eq!(r.text, "one\r\n2\r\n3\r\n3.5\r\n");
        assert!(r.loose.is_empty(), "matched exactly once line endings are normalised");
        // the model sending CRLF itself works too
        assert_eq!(ok(t, &[("one\r\ntwo", "1\r\n2")]), "1\r\n2\r\nthree\r\n");
        // mixed endings: left alone, the loose match still finds it
        assert_eq!(ok("a\r\nb\nc", &[("a\nb", "x")]), "x\nc");
    }

    #[test]
    fn unicode() {
        let t = "naïve café\n\u{a0}\u{a0}über straße — ok\n日本語 テキスト\n";
        assert_eq!(ok(t, &[("café", "cafe")]), "naïve cafe\n\u{a0}\u{a0}über straße — ok\n日本語 テキスト\n");
        // NBSP is whitespace for the loose match
        assert_eq!(ok(t, &[("über   straße", "uber strasse")]), "naïve café\n\u{a0}\u{a0}uber strasse — ok\n日本語 テキスト\n");
        assert_eq!(ok(t, &[("日本語\tテキスト", "にほんご")]), "naïve café\n\u{a0}\u{a0}über straße — ok\nにほんご\n");
        let e = ed(t, &[("ﾃｷｽﾄ 日本", "x")]).unwrap_err();
        assert!(e.contains("not in"), "{e}");
    }
}
