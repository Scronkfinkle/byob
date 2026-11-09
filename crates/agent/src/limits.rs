//! Caps that keep a turn bounded no matter what the model or a tool does.

/// Most tool calls per turn (as hob). Further calls are refused and tools are switched off.
pub const MAX_CALLS: usize = 30;
/// Most model requests per turn: a hard stop even if a server ignores "no tools".
pub const MAX_ROUNDS: usize = MAX_CALLS + 5;
/// Most bytes of one tool result the model sees.
pub const MAX_OUTPUT: usize = 16 * 1024;
/// Most bytes of tool results per turn (hob used 60k characters).
pub const TURN_OUTPUT_BUDGET: usize = 60_000;
/// After this many exact repeats of an earlier call, tools are switched off for the turn.
pub const MAX_REPEATS: usize = 3;
/// Longest step detail shown to clients.
pub const DETAIL_CHARS: usize = 240;
/// How much of an earlier result is quoted back when the model repeats a call.
pub const REPEAT_QUOTE: usize = 2000;

/// The longest prefix of `s` that is at most `max` bytes and ends on a char boundary.
pub fn prefix(s: &str, max: usize) -> &str {
    let mut i = max.min(s.len());
    while !s.is_char_boundary(i) {
        i = i.saturating_sub(1);
    }
    s.get(..i).unwrap_or_default()
}

/// Cap `text` at `max` bytes, saying what was cut so the model knows.
pub fn cap(text: String, max: usize, why: &str) -> String {
    if text.len() <= max {
        return text;
    }
    let total = text.len();
    let mut out = prefix(&text, max).to_owned();
    out.push_str(&format!("\n[… cut: showing {} of {total} bytes; {why}]", out.len()));
    out
}

/// One line of at most `max` chars, for step details.
pub fn one_line(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut out: String = flat.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_respects_char_boundaries() {
        assert_eq!(prefix("héllo", 2), "h");
        assert_eq!(prefix("héllo", 3), "hé");
        assert_eq!(prefix("abc", 10), "abc");
        assert_eq!(prefix("", 0), "");
    }

    #[test]
    fn cap_marks_the_cut() {
        assert_eq!(cap("short".into(), 10, "x"), "short");
        let capped = cap("é".repeat(100), 51, "too long");
        assert!(capped.starts_with(&"é".repeat(25)));
        assert!(capped.contains("showing 50 of 200 bytes; too long"));
    }

    #[test]
    fn one_line_flattens_and_truncates() {
        assert_eq!(one_line("a\n  b\tc", 10), "a b c");
        assert_eq!(one_line("abcdef", 4), "abc…");
    }
}
