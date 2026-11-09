//! Token estimation and history fitting.
//!
//! The estimator is deliberately cheap and errs high: ~3.5 ASCII characters per token, and
//! one token per non-ASCII character (CJK and emoji are roughly that). For exact counts on
//! llama.cpp use [`crate::Client::count_tokens`].
//!
//! [`fit_history`] adapts hob's `fitHistory` (a 40k-character budget) to token budgets:
//! keep the leading system prompt and the newest turns whole, fold older turns into a short
//! recap, and shrink oversized messages if the newest turn alone does not fit.

use crate::{ChatMessage, ToolSpec};

/// Fixed per-message cost (role markers, separators) in chat templates.
const MESSAGE_OVERHEAD: usize = 4;
/// Context assumed when the server does not report one.
pub const DEFAULT_CONTEXT_TOKENS: usize = 32_768;
/// Messages are never cut below this many characters, so the model still sees what happened.
const SHRINK_FLOOR_CHARS: usize = 600;
/// Older tool results are condensed to this many characters before whole turns are dropped.
const CONDENSED_TOOL_CHARS: usize = 1_500;

const RECAP_HEADER: &str = "[Earlier in this conversation, shortened to save context]";
const RECAP_ACK: &str = "Got it, I have that context.";

/// Estimated tokens in `text`.
pub fn estimate_tokens(text: &str) -> usize {
    let (ascii, other) = text.chars().fold((0usize, 0usize), |(a, o), c| if c.is_ascii() { (a + 1, o) } else { (a, o + 1) });
    // ceil(ascii / 3.5) == ceil(2 * ascii / 7)
    ascii.saturating_mul(2).div_ceil(7).saturating_add(other)
}

/// Estimated tokens for one message, including its tool calls and template overhead.
pub fn estimate_message_tokens(m: &ChatMessage) -> usize {
    let calls: usize = m
        .tool_calls
        .iter()
        .map(|c| estimate_tokens(&c.function.name).saturating_add(estimate_tokens(&c.function.arguments)).saturating_add(MESSAGE_OVERHEAD))
        .fold(0, usize::saturating_add);
    let images = m.images.len().saturating_mul(IMAGE_TOKENS);
    MESSAGE_OVERHEAD.saturating_add(m.content.as_deref().map(estimate_tokens).unwrap_or(0)).saturating_add(calls).saturating_add(images)
}

/// Rough cost of one image part (vision encoders use ~256–1600 tokens per image).
const IMAGE_TOKENS: usize = 1_200;

/// Estimated tokens for a transcript.
pub fn estimate_history_tokens(messages: &[ChatMessage]) -> usize {
    messages.iter().map(estimate_message_tokens).fold(0, usize::saturating_add)
}

/// Estimated tokens the tool definitions add to the prompt.
pub fn estimate_tools_tokens(tools: &[ToolSpec]) -> usize {
    tools
        .iter()
        .map(|t| {
            let spec = serde_json::json!({"name": t.name, "description": t.description, "parameters": t.parameters});
            estimate_tokens(&spec.to_string()).saturating_add(MESSAGE_OVERHEAD)
        })
        .fold(0, usize::saturating_add)
}

/// A sensible reply reservation for a context size: 1/8 of it, between 1k and 16k tokens.
pub fn default_reply_reserve(context_tokens: usize) -> usize {
    (context_tokens / 8).clamp(1_024, 16_384).min(context_tokens / 2)
}

/// Fit `messages` into `budget_tokens - reserve_for_reply` estimated tokens.
///
/// Leading `system` messages are always kept. If the rest does not fit: older tool results
/// are condensed first; then the oldest turns (a turn starts at a `user` message) are folded
/// into a recap user/assistant pair; finally, if the newest turn alone is too big, its largest
/// messages are shortened (head and tail kept). Tool-call/tool-result pairs are never split.
/// Best effort: the result can still exceed the budget if the system prompt alone does.
pub fn fit_history(messages: &[ChatMessage], budget_tokens: usize, reserve_for_reply: usize) -> Vec<ChatMessage> {
    let avail = budget_tokens.saturating_sub(reserve_for_reply);
    let sys_end = messages.iter().position(|m| m.role != "system").unwrap_or(messages.len());
    let (system, rest) = messages.split_at(sys_end);
    let room = avail.saturating_sub(estimate_history_tokens(system));
    if estimate_history_tokens(rest) <= room {
        return messages.to_vec();
    }

    // 1. Condense tool results outside the newest turn.
    let starts = turn_starts(rest);
    let last_start = starts.last().copied().unwrap_or(0);
    let mut rest: Vec<ChatMessage> = rest.to_vec();
    for m in rest.iter_mut().take(last_start).filter(|m| m.role == "tool") {
        shorten_content(m, CONDENSED_TOOL_CHARS);
    }
    let mut out = system.to_vec();
    if estimate_history_tokens(&rest) <= room {
        out.extend(rest);
        return out;
    }

    // 2. Keep whole turns from the end within 80% of the room; the newest always stays.
    let mut keep_from = rest.len();
    for &s in starts.iter().rev() {
        if keep_from < rest.len() && estimate_history_tokens(rest.get(s..).unwrap_or_default()) > room.saturating_mul(4) / 5 {
            break;
        }
        keep_from = s;
    }
    let mut kept = rest.split_off(keep_from);
    if !rest.is_empty() {
        let recap = recap_text(&rest, room / 5);
        out.push(ChatMessage::user(recap));
        out.push(ChatMessage::assistant(RECAP_ACK));
    }

    // 3. Shrink the biggest kept messages until it fits.
    let kept_room = room.saturating_sub(estimate_history_tokens(out.get(system.len()..).unwrap_or_default()));
    shrink_to_fit(&mut kept, kept_room);
    out.extend(kept);
    out
}

/// Indices where a turn starts: 0, then every `user` message.
fn turn_starts(messages: &[ChatMessage]) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(messages.iter().enumerate().skip(1).filter(|(_, m)| m.role == "user").map(|(i, _)| i));
    starts
}

/// One line, whitespace collapsed, at most `n` characters.
fn one_line(s: &str, n: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= n {
        return flat;
    }
    let mut cut: String = flat.chars().take(n.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// A recap of dropped messages within `max_tokens`, keeping the newest lines.
fn recap_text(older: &[ChatMessage], max_tokens: usize) -> String {
    let mut lines = Vec::new();
    for m in older {
        let text = m.content.as_deref().unwrap_or_default();
        match m.role.as_str() {
            "user" if !text.trim().is_empty() => lines.push(format!("- The user asked: {}", one_line(text, 160))),
            "assistant" => {
                if !text.trim().is_empty() {
                    lines.push(format!("- You answered: {}", one_line(text, 120)));
                }
                if !m.tool_calls.is_empty() {
                    let names: Vec<&str> = m.tool_calls.iter().map(|c| c.function.name.as_str()).collect();
                    lines.push(format!("- You used tools: {}", one_line(&names.join(", "), 120)));
                }
            }
            "system" if !text.trim().is_empty() => lines.push(format!("- Note: {}", one_line(text, 120))),
            _ => {}
        }
    }
    let mut used = estimate_tokens(RECAP_HEADER).saturating_add(16);
    let mut keep = 0;
    for line in lines.iter().rev() {
        let t = estimate_tokens(line).saturating_add(1);
        if used.saturating_add(t) > max_tokens {
            break;
        }
        used = used.saturating_add(t);
        keep += 1;
    }
    let skipped = lines.len().saturating_sub(keep);
    let mut out = String::from(RECAP_HEADER);
    if skipped > 0 {
        out.push_str(&format!("\n- ({skipped} earlier items omitted)"));
    }
    for line in lines.iter().skip(skipped) {
        out.push('\n');
        out.push_str(line);
    }
    out
}

/// Shorten the largest messages (tool results first) until `messages` fits `max_tokens`.
fn shrink_to_fit(messages: &mut [ChatMessage], max_tokens: usize) {
    for _ in 0..messages.len().saturating_mul(2) {
        let total = estimate_history_tokens(messages);
        if total <= max_tokens {
            return;
        }
        let excess = total - max_tokens;
        let biggest = messages
            .iter_mut()
            .filter(|m| m.content.as_ref().is_some_and(|c| c.chars().count() > SHRINK_FLOOR_CHARS))
            .max_by_key(|m| (m.role == "tool", m.content.as_ref().map(|c| c.len()).unwrap_or(0)));
        let Some(m) = biggest else { return };
        let len = m.content.as_ref().map(|c| c.chars().count()).unwrap_or(0);
        // Tokens -> characters at the ASCII rate, plus room for the marker.
        let cut = excess.saturating_mul(7).div_ceil(2).saturating_add(120);
        shorten_content(m, len.saturating_sub(cut).max(SHRINK_FLOOR_CHARS));
    }
}

/// Cut `m.content` to about `max_chars`, keeping the head and the tail.
fn shorten_content(m: &mut ChatMessage, max_chars: usize) {
    let Some(text) = m.content.as_ref() else { return };
    let len = text.chars().count();
    if len <= max_chars {
        return;
    }
    let head = max_chars.saturating_mul(2) / 3;
    let tail = max_chars.saturating_sub(head);
    let omitted = len.saturating_sub(head + tail);
    let mut out: String = text.chars().take(head).collect();
    out.push_str(&format!("\n… [{omitted} characters omitted to fit the context] …\n"));
    out.extend(text.chars().skip(len.saturating_sub(tail)));
    m.content = Some(out);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolCall;

    fn big(n: usize) -> String {
        "word ".repeat(n / 5)
    }

    #[test]
    fn estimates_are_cheap_and_conservative() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcdefg"), 2);
        assert_eq!(estimate_tokens("日本語"), 3);
        assert!(estimate_message_tokens(&ChatMessage::user("hi")) > estimate_tokens("hi"));
        let call = ToolCall { id: "1".into(), name: "read_file".into(), arguments: "{\"path\":\"a\"}".into() };
        assert!(estimate_message_tokens(&ChatMessage::assistant_tool_calls(None, &[call])) > MESSAGE_OVERHEAD * 2);
        assert_eq!(default_reply_reserve(262_144), 16_384);
        assert_eq!(default_reply_reserve(4_096), 1_024);
        assert_eq!(default_reply_reserve(1_000), 500);
    }

    #[test]
    fn fitting_history_is_untouched() {
        let h = vec![ChatMessage::system("sys"), ChatMessage::user("a"), ChatMessage::assistant("b")];
        assert_eq!(fit_history(&h, 1000, 100), h);
    }

    #[test]
    fn drops_oldest_turns_into_recap_and_keeps_system_and_latest() {
        let mut h = vec![ChatMessage::system("You are yah.")];
        for i in 0..20 {
            h.push(ChatMessage::user(format!("question {i} {}", big(700))));
            h.push(ChatMessage::assistant(format!("answer {i} {}", big(700))));
        }
        let out = fit_history(&h, 3_000, 500);
        assert!(estimate_history_tokens(&out) <= 2_500, "{}", estimate_history_tokens(&out));
        assert_eq!(out[0], h[0]);
        assert!(out[1].content.as_deref().unwrap().starts_with(RECAP_HEADER));
        assert!(out[1].content.as_deref().unwrap().contains("question 0") || out[1].content.as_deref().unwrap().contains("omitted"));
        assert_eq!(out[2].content.as_deref(), Some(RECAP_ACK));
        assert_eq!(out.last(), h.last());
        assert_eq!(out[3].role, "user");
    }

    #[test]
    fn condenses_old_tool_results_before_dropping_turns() {
        let call = ToolCall { id: "c1".into(), name: "read_file".into(), arguments: "{}".into() };
        let h = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("read it"),
            ChatMessage::assistant_tool_calls(None, std::slice::from_ref(&call)),
            ChatMessage::tool_result("c1", big(20_000)),
            ChatMessage::assistant("it says words"),
            ChatMessage::user("thanks, and now?"),
        ];
        let out = fit_history(&h, 2_500, 500);
        assert_eq!(out.len(), h.len(), "no turn dropped");
        assert!(out[3].content.as_deref().unwrap().contains("omitted"));
        assert!(estimate_history_tokens(&out) <= 2_000);
    }

    #[test]
    fn oversized_latest_turn_is_shrunk_not_dropped() {
        let call = ToolCall { id: "c1".into(), name: "read_file".into(), arguments: "{}".into() };
        let h = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("read the big file"),
            ChatMessage::assistant_tool_calls(None, std::slice::from_ref(&call)),
            ChatMessage::tool_result("c1", format!("HEAD {} TAIL", big(100_000))),
        ];
        let out = fit_history(&h, 4_000, 1_000);
        assert_eq!(out.len(), 4);
        assert_eq!(out[2].tool_calls.len(), 1, "call/result pair intact");
        let r = out[3].content.as_deref().unwrap();
        assert!(r.starts_with("HEAD") && r.ends_with("TAIL"));
        assert!(estimate_history_tokens(&out) <= 3_000, "{}", estimate_history_tokens(&out));
    }

    #[test]
    fn degenerate_budgets_do_not_panic() {
        let h = vec![ChatMessage::system(big(1000)), ChatMessage::user(big(1000)), ChatMessage::assistant(big(1000))];
        for (b, r) in [(0, 0), (0, 100), (10, 0), (usize::MAX, 0), (usize::MAX, usize::MAX)] {
            let out = fit_history(&h, b, r);
            assert_eq!(out.first(), h.first());
        }
        assert!(fit_history(&[], 0, 0).is_empty());
        let only_assistant = vec![ChatMessage::assistant(big(10_000))];
        assert_eq!(fit_history(&only_assistant, 100, 0).len(), 1);
    }
}
