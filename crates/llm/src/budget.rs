//! Token estimation and history fitting.
//!
//! The estimator is deliberately cheap and errs high: ~3.5 ASCII characters per token, and
//! one token per non-ASCII character (CJK and emoji are roughly that). For exact counts on
//! llama.cpp use [`crate::Client::count_tokens`].
//!
//! [`fit_history`] adapts hob's `fitHistory` (a 40k-character budget) to token budgets:
//! keep the leading system prompt and the newest turns whole, fold older turns into a short
//! recap, and shrink oversized messages if the newest turn alone does not fit.

use crate::{ChatMessage, ToolSpec, toolprompt};

/// Fixed per-message cost (role markers, separators) in chat templates.
const MESSAGE_OVERHEAD: usize = 4;
/// Flat estimate for one image input (vision encoders use a few hundred to ~1.5k tokens).
const IMAGE_TOKENS: usize = 1_024;
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
///
/// Each message is estimated once and the steps work on those numbers, so the cost stays
/// linear in the history size even for very large contexts and long transcripts.
pub fn fit_history(messages: &[ChatMessage], budget_tokens: usize, reserve_for_reply: usize) -> Vec<ChatMessage> {
    let avail = budget_tokens.saturating_sub(reserve_for_reply);
    let sys_end = messages.iter().position(|m| m.role != "system").unwrap_or(messages.len());
    let (system, rest) = messages.split_at(sys_end);
    let room = avail.saturating_sub(estimate_history_tokens(system));
    let mut costs: Vec<usize> = rest.iter().map(estimate_message_tokens).collect();
    if sum(&costs) <= room {
        return messages.to_vec();
    }

    // 1. Condense tool results outside the newest turn.
    let starts = turn_starts(rest);
    let last_start = starts.last().copied().unwrap_or(0);
    let mut rest: Vec<ChatMessage> = rest.to_vec();
    for (m, cost) in rest.iter_mut().zip(costs.iter_mut()).take(last_start) {
        if toolprompt::is_tool_result(m) && shorten_content(m, CONDENSED_TOOL_CHARS) {
            *cost = estimate_message_tokens(m);
        }
    }
    let mut out = system.to_vec();
    if sum(&costs) <= room {
        out.extend(rest);
        return out;
    }

    // 2. Keep whole turns from the end within 80% of the room; the newest always stays.
    let mut keep_from = rest.len();
    let mut kept_cost = 0usize;
    for &s in starts.iter().rev() {
        let cost = kept_cost.saturating_add(sum(costs.get(s..keep_from).unwrap_or_default()));
        if keep_from < rest.len() && cost > room.saturating_mul(4) / 5 {
            break;
        }
        keep_from = s;
        kept_cost = cost;
    }
    let mut kept = rest.split_off(keep_from);
    let kept_costs = costs.split_off(keep_from);
    if !rest.is_empty() {
        let recap = recap_text(&rest, room / 5);
        out.push(ChatMessage::user(recap));
        out.push(ChatMessage::assistant(RECAP_ACK));
    }

    // 3. Shrink the biggest kept messages until it fits.
    let kept_room = room.saturating_sub(estimate_history_tokens(out.get(system.len()..).unwrap_or_default()));
    shrink_to_fit(&mut kept, kept_costs, kept_room);
    out.extend(kept);
    out
}

fn sum(costs: &[usize]) -> usize {
    costs.iter().copied().fold(0, usize::saturating_add)
}

/// Indices where a turn starts: 0, then every `user` message that is not a tool result.
fn turn_starts(messages: &[ChatMessage]) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(messages.iter().enumerate().skip(1).filter(|(_, m)| m.role == "user" && !toolprompt::is_tool_result(m)).map(|(i, _)| i));
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
/// `costs` are the messages' current estimates; only changed messages are re-estimated.
fn shrink_to_fit(messages: &mut [ChatMessage], mut costs: Vec<usize>, max_tokens: usize) {
    let mut chars: Vec<usize> = messages.iter().map(|m| m.content.as_ref().map_or(0, |c| c.chars().count())).collect();
    let mut total = sum(&costs);
    for _ in 0..messages.len().saturating_mul(2) {
        if total <= max_tokens {
            return;
        }
        let excess = total - max_tokens;
        let biggest = messages
            .iter()
            .zip(&chars)
            .enumerate()
            .filter(|(_, (_, n))| **n > SHRINK_FLOOR_CHARS)
            .max_by_key(|(_, (m, n))| (toolprompt::is_tool_result(m), **n))
            .map(|(i, _)| i);
        let Some(i) = biggest else { return };
        let (Some(m), Some(cost), Some(n)) = (messages.get_mut(i), costs.get_mut(i), chars.get_mut(i)) else { return };
        // Tokens -> characters at the ASCII rate, plus room for the marker.
        let cut = excess.saturating_mul(7).div_ceil(2).saturating_add(120);
        if !shorten_content(m, n.saturating_sub(cut).max(SHRINK_FLOOR_CHARS)) {
            // Can't get shorter (the marker makes floor-sized cuts a no-op): skip it from now on.
            *n = 0;
            continue;
        }
        let new_cost = estimate_message_tokens(m);
        total = total.saturating_sub(*cost).saturating_add(new_cost);
        *cost = new_cost;
        *n = m.content.as_ref().map_or(0, |c| c.chars().count());
    }
}

/// Cut `m.content` to about `max_chars`, keeping the head and the tail. True if it changed.
fn shorten_content(m: &mut ChatMessage, max_chars: usize) -> bool {
    let Some(text) = m.content.as_ref() else { return false };
    let len = text.chars().count();
    // Cutting less than the marker adds would make the message longer.
    if len <= max_chars.saturating_add(MARKER_CHARS) {
        return false;
    }
    let head = max_chars.saturating_mul(2) / 3;
    let tail = max_chars.saturating_sub(head);
    let omitted = len.saturating_sub(head + tail);
    let mut out: String = text.chars().take(head).collect();
    out.push_str(&format!("\n… [{omitted} characters omitted to fit the context] …\n"));
    out.extend(text.chars().skip(len.saturating_sub(tail)));
    m.content = Some(out);
    true
}

/// Rough length of the "characters omitted" marker.
const MARKER_CHARS: usize = 64;

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
    fn huge_histories_fit_in_linear_time() {
        // 4000 turns, ~2.4 MB of text, against a 262k context: must not go quadratic.
        let mut h = vec![ChatMessage::system("sys")];
        for i in 0..4_000 {
            h.push(ChatMessage::user(format!("q{i} {}", big(300))));
            h.push(ChatMessage::assistant(format!("a{i} {}", big(300))));
        }
        let started = std::time::Instant::now();
        let out = fit_history(&h, 262_144, default_reply_reserve(262_144));
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "{:?}", started.elapsed());
        assert!(estimate_history_tokens(&out) <= 262_144 - 16_384);
        assert_eq!(out.last(), h.last());
        // And a single newest turn made of hundreds of big tool results.
        let call = ToolCall { id: "c".into(), name: "read_file".into(), arguments: "{}".into() };
        for rounds in [60, 1_000] {
            let mut h = vec![ChatMessage::system("sys"), ChatMessage::user("go")];
            for _ in 0..rounds {
                h.push(ChatMessage::assistant_tool_calls(None, std::slice::from_ref(&call)));
                h.push(ChatMessage::tool_result("c", big(16_000)));
            }
            let started = std::time::Instant::now();
            let out = fit_history(&h, 32_768, 4_096);
            assert!(started.elapsed() < std::time::Duration::from_secs(2), "{rounds}: {:?}", started.elapsed());
            assert_eq!(out.len(), h.len());
            // 1000 results can't fit even at the floor size; 60 can.
            if rounds == 60 {
                assert!(estimate_history_tokens(&out) <= 32_768 - 4_096, "{}", estimate_history_tokens(&out));
            }
        }
    }

    #[test]
    fn fallback_tool_responses_do_not_start_turns() {
        let call = ToolCall { id: "c1".into(), name: "read_file".into(), arguments: "{}".into() };
        let h = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("old question"),
            ChatMessage::assistant(big(4_000)),
            ChatMessage::user("read it"),
            toolprompt::fallback_assistant_message("", std::slice::from_ref(&call)),
            toolprompt::fallback_tool_result(&call, &big(20_000)),
        ];
        let out = fit_history(&h, 3_000, 500);
        // The old turn is folded into a recap; the call and its result stay together.
        assert!(out[1].content.as_deref().unwrap().starts_with(RECAP_HEADER));
        let n = out.len();
        assert_eq!(out[n - 3].content.as_deref(), Some("read it"));
        assert!(toolprompt::is_tool_result(&out[n - 1]));
        assert!(out[n - 1].content.as_deref().unwrap().contains("omitted"));
    }

    #[test]
    fn images_cost_tokens() {
        let m = ChatMessage::user_with_images("look", vec!["data:image/png;base64,AAAA".into()]);
        assert!(estimate_message_tokens(&m) >= IMAGE_TOKENS);
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
