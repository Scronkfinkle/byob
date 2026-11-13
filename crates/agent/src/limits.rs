//! Caps that keep a turn bounded no matter what the model or a tool does.

/// Most tool calls per turn by default (the chat's settings may change it, or lift it).
/// Further calls are refused and tools are switched off.
#[cfg(test)]
pub const MAX_CALLS: usize = byob_types::config::DEFAULT_TOOL_CALLS as usize;
/// Model requests a turn may make beyond its tool-call limit: a hard stop even if a
/// server ignores "no tools".
pub const EXTRA_ROUNDS: usize = 5;
/// Most bytes of one tool result the model sees.
pub const MAX_OUTPUT: usize = 16 * 1024;
/// Most bytes of tool results per turn at the default call limit (hob used 60k
/// characters); it grows with a higher limit (see [`output_budget`]).
pub const TURN_OUTPUT_BUDGET: usize = 60_000;

/// A turn's tool-call limit from the setting (0: none).
pub fn call_limit(setting: u32) -> usize {
    if setting == 0 { usize::MAX } else { usize::try_from(setting).unwrap_or(usize::MAX) }
}

/// The tool-output budget of a turn allowed `max_calls` calls: [`TURN_OUTPUT_BUDGET`] for
/// up to the default limit, in proportion above it; none without a limit (compaction and
/// the request budget keep the context in check).
pub fn output_budget(max_calls: usize) -> usize {
    let default = byob_types::config::DEFAULT_TOOL_CALLS as usize;
    if max_calls == usize::MAX {
        return usize::MAX;
    }
    TURN_OUTPUT_BUDGET.saturating_mul(max_calls.max(default)) / default
}
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
