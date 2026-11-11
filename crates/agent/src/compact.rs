//! Context compaction: when a chat's next request would pass a threshold of the context
//! window, the model first writes a dense summary of the conversation, and the request
//! continues from the system prompt + that summary + the newest turns verbatim.
//!
//! - Turns check before every model request (so also between tool rounds, when tool output
//!   balloons) and compact automatically; `ClientMsg::Compact` / `/compact [focus]` compact
//!   on demand.
//! - Every compaction is stored with the last message it covers, so after a restart the
//!   engine builds history from the latest compaction plus later messages ([`history`]).
//! - A summary that fails (server error, empty reply) never loses the turn: the request goes
//!   on uncompacted and [`byob_llm::budget::fit_history`] shortens it as before.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use byob_llm::{ChatMessage, StreamEvent, Usage, budget, toolprompt};
use byob_protocol::{Entry, EntryRole};

use crate::adapt::Plan;

/// Opens the user message that carries a summary in the request transcript.
pub const SUMMARY_HEADER: &str = "[Summary of the earlier conversation, written when it was compacted to save context]";
/// The assistant's reply to the summary message (keeps roles alternating).
pub const SUMMARY_ACK: &str = "Understood. I have the context from the summary and will continue from there.";
/// Appended to the user's request when a compaction happens in the middle of a tool loop.
const RESUME_NOTE: &str = "[Note: the context was compacted while you were working on this request. The summary above says what you have done so far; \
continue from there without repeating finished steps.]";

/// At most this many automatic compactions in one turn (a summary that is itself too big
/// must not loop).
pub const MAX_PER_TURN: usize = 3;
/// Longest summary kept, in characters.
const MAX_SUMMARY_CHARS: usize = 40_000;
/// Stop reading the summary after this many bytes of text or of thinking.
const MAX_SUMMARY_BYTES: usize = 64 * 1024;
const MAX_REASONING_BYTES: usize = 128 * 1024;
/// Give up on a summary that takes longer than this.
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(600);
/// Per-message caps when the conversation is written out for the summarizer (characters).
const USER_CHARS: usize = 8_000;
const ASSISTANT_CHARS: usize = 6_000;
const TOOL_CHARS: usize = 2_400;
const ARGS_CHARS: usize = 600;
/// Tokens left for the summarizer's instructions on top of the conversation.
const PROMPT_OVERHEAD: usize = 1_024;

pub(crate) const SYSTEM: &str = "You compress conversations between a user and an AI assistant (byob) that uses tools in a desktop harness. \
You write summaries the assistant will rely on to continue the work without the original messages, so precision matters more than style.";

const INSTRUCTIONS: &str = "Write a dense summary of the conversation above so the assistant can continue it without the original messages. \
Use these markdown sections, skipping empty ones:

## User goals
What the user wants overall and right now, in their words where it matters.
## Decisions and preferences
Choices made, constraints, what the user approved or rejected.
## Facts learned
Concrete findings: numbers, names, errors and their causes, answers given.
## Files and artifacts
Every file path and artifact id touched, and what was done to each.
## Important tool results
Key outputs, quoted briefly when exact values matter.
## Open tasks
What is unfinished or should happen next. If a request was in progress, say exactly where it stands.

Keep paths, ids, numbers, commands and names exactly as written. Don't invent anything. No preamble: start with the first heading. \
Aim for at most about 1200 words.";

/// When to compact. Settings and `--compact-at` fill it in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactOptions {
    /// Compact automatically when a request would pass the threshold.
    pub auto: bool,
    /// Threshold in percent of the context window.
    pub threshold_percent: u8,
    /// Absolute threshold in tokens; overrides the percentage (testing).
    pub at_tokens: Option<usize>,
    /// Most recent turns kept verbatim by an automatic compaction before a request.
    pub keep_turns: usize,
}

impl Default for CompactOptions {
    fn default() -> Self {
        Self { auto: true, threshold_percent: 80, at_tokens: None, keep_turns: 2 }
    }
}

impl CompactOptions {
    /// Requests estimated above this many tokens are compacted first. Never more than the
    /// window minus the reply reserve (where `fit_history` would start cutting).
    pub fn threshold(&self, plan: &Plan) -> usize {
        let ceiling = plan.context.saturating_sub(plan.reserve).max(1);
        let percent = usize::from(self.threshold_percent.clamp(10, 95));
        self.at_tokens.unwrap_or_else(|| plan.context / 100 * percent).clamp(1, ceiling)
    }
}

/// `/compact` or `/compact <focus>`: `Some(focus)` (empty focus → `None`) if `text` is the command.
pub fn command(text: &str) -> Option<Option<String>> {
    let rest = text.trim().strip_prefix("/compact")?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let focus = rest.trim();
    Some((!focus.is_empty()).then(|| focus.to_owned()))
}

/// The user/assistant pair that carries a summary in the request transcript.
pub fn summary_messages(summary: &str) -> [ChatMessage; 2] {
    [ChatMessage::user(format!("{SUMMARY_HEADER}\n\n{summary}")), ChatMessage::assistant(SUMMARY_ACK)]
}

fn is_summary(m: &ChatMessage) -> bool {
    m.role == "user" && m.content.as_deref().is_some_and(|c| c.starts_with(SUMMARY_HEADER))
}

/// Starts a turn: a user message that is neither a tool result nor a summary.
fn starts_turn(m: &ChatMessage) -> bool {
    m.role == "user" && !toolprompt::is_tool_result(m) && !is_summary(m)
}

/// Estimated tokens of `messages` that aren't an earlier summary pair: what a new summary
/// would actually condense.
pub fn fresh_tokens(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .filter(|m| !is_summary(m) && !(m.role == "assistant" && m.content.as_deref() == Some(SUMMARY_ACK)))
        .map(budget::estimate_message_tokens)
        .fold(0, usize::saturating_add)
}

/// The request transcript built from a chat: the system prompt, the latest compaction's
/// summary, the finished entries after it, and the new user message (if any). Reasoning and
/// tool steps are not sent back; failed or empty replies are skipped. Also returns the entry
/// id each message came from (0 for the system prompt, the summary pair and the new message).
pub fn history(entries: &[Entry], latest: Option<(u64, &str)>, new_user: Option<&str>, system_prompt: &str) -> (Vec<ChatMessage>, Vec<u64>) {
    let mut out = vec![ChatMessage::system(system_prompt)];
    let mut ids = vec![0];
    let through = latest.map_or(0, |(t, _)| t);
    if let Some((_, summary)) = latest {
        out.extend(summary_messages(summary));
        ids.extend([0, 0]);
    }
    for e in entries.iter().filter(|e| e.id > through && e.done && e.error.is_none() && (!e.content.is_empty() || !e.attachments.is_empty())) {
        out.push(match e.role {
            EntryRole::User => ChatMessage::user(crate::attachments::with_names(&e.content, &e.attachments)),
            EntryRole::Assistant => ChatMessage::assistant(e.content.clone()),
        });
        ids.push(e.id);
    }
    if let Some(text) = new_user {
        out.push(ChatMessage::user(text));
        ids.push(0);
    }
    (out, ids)
}

/// Index of the first message that isn't a system prompt.
pub fn content_start(messages: &[ChatMessage]) -> usize {
    messages.iter().position(|m| m.role != "system").unwrap_or(messages.len())
}

/// Where the verbatim tail starts when compacting before a request: the newest `keep` turns
/// before `end` (the new user message) that fit in `room` tokens together. `None` if that
/// leaves nothing worth summarising (only an earlier summary, or nothing at all).
pub fn split_point(messages: &[ChatMessage], end: usize, keep: usize, room: usize) -> Option<usize> {
    let start = content_start(messages);
    let end = end.min(messages.len());
    let mut split = end;
    let mut kept = 0usize;
    let mut cost = 0usize;
    for i in (start..end).rev() {
        let Some(m) = messages.get(i) else { break };
        cost = cost.saturating_add(budget::estimate_message_tokens(m));
        if cost > room {
            break;
        }
        if starts_turn(m) {
            if kept == keep {
                break;
            }
            kept += 1;
            split = i;
        }
    }
    let old_summary = messages.get(start).is_some_and(is_summary);
    let first_new = if old_summary { start.saturating_add(2) } else { start };
    (split > first_new).then_some(split)
}

/// Compacting in the middle of a tool loop: everything is summarised and the turn goes on
/// from the summary and a copy of the user's request (with a note).
pub fn resume_message(messages: &[ChatMessage]) -> Option<ChatMessage> {
    let mut m = messages.iter().rev().find(|m| starts_turn(m))?.clone();
    let text = m.content.take().unwrap_or_default();
    m.content = Some(format!("{text}\n\n{RESUME_NOTE}"));
    Some(m)
}

/// A summary the model wrote.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub text: String,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The user stopped it.
    Stopped,
    Error(String),
}

/// Ask the model for a summary of `messages` (the system prompt is left out). Not streamed
/// to anyone; bounded in size and time. `focus` is what the user wants kept in detail.
pub fn summarize(llm: &byob_llm::Client, messages: &[ChatMessage], focus: Option<&str>, plan: &Plan, cancel: &AtomicBool) -> Result<Summary, Failure> {
    let room = plan.context.saturating_sub(plan.reserve).saturating_sub(PROMPT_OVERHEAD).max(256);
    let request = request(messages, focus, room);
    let stop = AtomicBool::new(false);
    let started = Instant::now();
    let (mut content, mut reasoning) = (0usize, 0usize);
    let mut overthought = false;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let until = byob_llm::StopWhen { cancel, also: Some(&stop), deadline: started.checked_add(SUMMARY_TIMEOUT) };
        llm.stream_chat_until(&request, &[], until, |ev| {
            match ev {
                StreamEvent::Content(t) => content = content.saturating_add(t.len()),
                StreamEvent::Reasoning(t) => reasoning = reasoning.saturating_add(t.len()),
            }
            overthought |= reasoning > MAX_REASONING_BYTES && content == 0;
            if cancel.load(Ordering::Relaxed) || content > MAX_SUMMARY_BYTES || overthought || started.elapsed() > SUMMARY_TIMEOUT {
                stop.store(true, Ordering::Relaxed);
            }
        })
    }));
    if cancel.load(Ordering::Relaxed) {
        return Err(Failure::Stopped);
    }
    let out = match result {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return Err(Failure::Error(e.to_string())),
        Err(_) => return Err(Failure::Error("internal error while summarising".into())),
    };
    if overthought {
        return Err(Failure::Error("the model kept thinking instead of writing the summary".into()));
    }
    let text = clean(&out.content);
    if text.is_empty() {
        return Err(Failure::Error("the model returned an empty summary".into()));
    }
    Ok(Summary { text, usage: out.usage })
}

/// The summarizer's request: instructions, and the conversation written out as text (so it
/// works the same in every tool-calling mode) within `room` tokens.
pub fn request(messages: &[ChatMessage], focus: Option<&str>, room: usize) -> Vec<ChatMessage> {
    let mut ask = format!("<conversation>\n{}\n</conversation>\n\n{INSTRUCTIONS}", render(messages, room));
    if let Some(f) = focus.map(str::trim).filter(|f| !f.is_empty()) {
        ask.push_str(&format!("\n\nThe user asked this summary to focus on: {}. Keep everything relevant to that in full detail.", clip(f, 1_000)));
    }
    vec![ChatMessage::system(SYSTEM), ChatMessage::user(ask)]
}

/// The conversation as plain text blocks within `room` estimated tokens: long messages keep
/// their head and tail; if that is still too much, the oldest blocks go (an earlier summary
/// at the start is kept).
pub fn render(messages: &[ChatMessage], room: usize) -> String {
    let mut names = std::collections::BTreeMap::new();
    let mut blocks: Vec<String> = Vec::new();
    for m in messages.iter().filter(|m| m.role != "system") {
        let text = m.content.as_deref().unwrap_or_default();
        let images = if m.images.is_empty() { String::new() } else { format!("\n[{} image(s) attached]", m.images.len()) };
        let block = match m.role.as_str() {
            "user" if is_summary(m) => format!("### Summary of what came before\n{}", text.strip_prefix(SUMMARY_HEADER).unwrap_or(text).trim()),
            "user" if toolprompt::is_tool_result(m) => format!("### Tool result\n{}", clip(text, TOOL_CHARS)),
            "user" => format!("### User\n{}{images}", clip(text, USER_CHARS)),
            "assistant" if text == SUMMARY_ACK => continue,
            "assistant" => {
                let mut b = String::from("### Assistant");
                if !text.trim().is_empty() {
                    b.push('\n');
                    b.push_str(&clip(text, ASSISTANT_CHARS));
                }
                for c in &m.tool_calls {
                    names.insert(c.id.clone(), c.function.name.clone());
                    b.push_str(&format!("\n→ called `{}` with {}", c.function.name, clip(&c.function.arguments, ARGS_CHARS)));
                }
                b
            }
            "tool" => {
                let name = m.tool_call_id.as_ref().and_then(|id| names.get(id)).map_or("tool", String::as_str);
                format!("### Result of `{name}`\n{}", clip(text, TOOL_CHARS))
            }
            other => format!("### {other}\n{}", clip(text, TOOL_CHARS)),
        };
        blocks.push(block);
    }
    let costs: Vec<usize> = blocks.iter().map(|b| budget::estimate_tokens(b).saturating_add(2)).collect();
    let total = costs.iter().copied().fold(0, usize::saturating_add);
    if total <= room {
        return blocks.join("\n\n");
    }
    // Keep a leading summary, then as many of the newest blocks as fit.
    let lead = usize::from(blocks.first().is_some_and(|b| b.starts_with("### Summary")));
    let mut used = costs.iter().take(lead).copied().fold(0, usize::saturating_add);
    let mut from = blocks.len();
    for i in (lead..blocks.len()).rev() {
        let c = costs.get(i).copied().unwrap_or(0);
        if used.saturating_add(c) > room && from < blocks.len() {
            break;
        }
        used = used.saturating_add(c);
        from = i;
    }
    let mut out: Vec<String> = blocks.iter().take(lead).cloned().collect();
    if from > lead {
        out.push(format!("[… {} earlier messages left out to fit …]", from - lead));
    }
    out.extend(blocks.into_iter().skip(from));
    let text = out.join("\n\n");
    // A single block can still be too big (a huge summary or message): keep head and tail.
    clip(&text, room.saturating_mul(3))
}

/// At most `max` characters: the head and the tail, with a marker between.
fn clip(text: &str, max: usize) -> String {
    let n = text.chars().count();
    if n <= max {
        return text.to_owned();
    }
    let head = max.saturating_mul(2) / 3;
    let tail = max.saturating_sub(head);
    let mut out: String = text.chars().take(head).collect();
    out.push_str(&format!("\n[… {} characters left out …]\n", n.saturating_sub(head + tail)));
    out.extend(text.chars().skip(n.saturating_sub(tail)));
    out
}

/// Tidy the model's summary: trimmed, a wrapping code fence removed, bounded.
fn clean(raw: &str) -> String {
    let mut t = raw.trim();
    if let Some(inner) = t.strip_prefix("```markdown").or_else(|| t.strip_prefix("```md")).or_else(|| t.strip_prefix("```")) {
        t = inner.strip_suffix("```").unwrap_or(inner).trim();
    }
    if t.chars().count() <= MAX_SUMMARY_CHARS {
        return t.to_owned();
    }
    let mut out: String = t.chars().take(MAX_SUMMARY_CHARS).collect();
    out.push_str("\n…");
    out
}

/// Token use of one chat, kept by the engine for the header meter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tally {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub model_calls: u64,
    pub compactions: u64,
    pub elapsed_ms: u64,
    pub last_prompt: u64,
    pub last_completion: u64,
    /// What the context holds now and whether that is an estimate; `None` until a request
    /// ran (the engine estimates the next request then).
    pub context: Option<(u64, bool)>,
    pub compacting: bool,
    /// Subagents' token use and requests (kept apart from the chat's own).
    pub agent_prompt: u64,
    pub agent_completion: u64,
    pub agent_calls: u64,
}

impl Tally {
    /// From what the store remembers: replies' stats and manual compactions (automatic ones
    /// are counted in their reply's stats).
    pub fn from_store(messages: &[byob_store::StoredMessage], compactions: &[byob_store::Compaction]) -> Self {
        let mut t = Self::default();
        for s in messages.iter().filter_map(|m| m.stats.as_ref()) {
            t.prompt_tokens = t.prompt_tokens.saturating_add(s.prompt_tokens);
            t.completion_tokens = t.completion_tokens.saturating_add(s.completion_tokens);
            t.elapsed_ms = t.elapsed_ms.saturating_add(s.elapsed_ms);
            // Rows from before calls were counted made at least one.
            t.model_calls = t.model_calls.saturating_add(s.model_calls.max(1));
        }
        for c in compactions {
            t.compactions = t.compactions.saturating_add(1);
            if c.manual {
                t.prompt_tokens = t.prompt_tokens.saturating_add(c.prompt_tokens);
                t.completion_tokens = t.completion_tokens.saturating_add(c.completion_tokens);
                t.model_calls = t.model_calls.saturating_add(1);
            }
        }
        t
    }

    /// Add what stored subagent runs used.
    pub fn add_runs(&mut self, runs: &[byob_store::AgentRun]) {
        for s in runs.iter().filter_map(|r| r.stats.as_ref()) {
            self.agent_prompt = self.agent_prompt.saturating_add(s.prompt_tokens);
            self.agent_completion = self.agent_completion.saturating_add(s.completion_tokens);
            self.agent_calls = self.agent_calls.saturating_add(s.model_calls);
        }
    }

    /// One subagent model request finished.
    pub fn agent_call(&mut self, prompt: u64, completion: u64) {
        self.agent_prompt = self.agent_prompt.saturating_add(prompt);
        self.agent_completion = self.agent_completion.saturating_add(completion);
        self.agent_calls = self.agent_calls.saturating_add(1);
    }

    /// One model request finished.
    pub fn call(&mut self, prompt: u64, completion: u64) {
        self.prompt_tokens = self.prompt_tokens.saturating_add(prompt);
        self.completion_tokens = self.completion_tokens.saturating_add(completion);
        self.model_calls = self.model_calls.saturating_add(1);
        self.last_prompt = prompt;
        self.last_completion = completion;
    }
}

/// The compaction hook's id (bundled plugin `compaction`, `hooks[].native`).
pub const HOOK: &str = "compaction";
/// Where it runs among `before_request` hooks when no plugin names it.
pub const HOOK_PRIORITY: i64 = 100;

/// Automatic compaction as a `before_request` hook (see the module docs).
pub(crate) struct CompactionHook;

/// What the hook remembers during one turn.
#[derive(Default)]
struct TurnState {
    /// Compactions made, and whether one failed (then this turn stops trying).
    compactions: usize,
    failed: bool,
    /// Estimated request tokens right after this turn's last compaction.
    compacted_to: usize,
}

impl crate::hooks::NativeHook for CompactionHook {
    /// Before a model request: if it would pass the threshold, summarise the conversation
    /// and continue from the summary. Before the first request the newest turns stay
    /// verbatim; later (inside the tool loop) everything is summarised and the request is
    /// repeated after the summary. A failed summary leaves the messages alone (fit_history
    /// still shortens them) and turns compaction off for the rest of the turn.
    fn before_request(&self, ctx: &mut crate::hooks::RequestCtx<'_>) -> crate::hooks::Flow {
        use crate::hooks::Flow;
        use crate::turn::{Compacted, TurnEvent};
        let Some(auto) = ctx.compact else { return Flow::Continue };
        let slot = ctx.state.entry(HOOK.to_owned()).or_insert_with(|| Box::new(TurnState::default()));
        let Some(st) = slot.downcast_mut::<TurnState>() else { return Flow::Continue };
        if st.failed || st.compactions >= MAX_PER_TURN {
            return Flow::Continue;
        }
        let before = ctx.plan.raw_tokens(ctx.messages, ctx.specs);
        // After a compaction in this turn, wait until the request has grown again by a
        // quarter of the threshold (a threshold just above the fixed prompt would churn).
        if before <= auto.at || before < st.compacted_to.saturating_add(auto.at / 4) {
            return Flow::Continue;
        }
        let start = content_start(ctx.messages);
        let (covered, tail, through) = if ctx.round == 0 {
            let end = ctx.messages.len().saturating_sub(1);
            let Some(split) = split_point(ctx.messages, end, auto.keep_turns, auto.at / 4) else { return Flow::Continue };
            let through = ctx.ids.get(..split).and_then(|ids| ids.iter().rev().find(|id| **id > 0)).copied().unwrap_or(0);
            (ctx.messages.get(start..split).unwrap_or_default().to_vec(), ctx.messages.get(split..).unwrap_or_default().to_vec(), through)
        } else {
            let Some(resume) = resume_message(ctx.messages) else { return Flow::Continue };
            (ctx.messages.get(start..).unwrap_or_default().to_vec(), vec![resume], ctx.prior)
        };
        // Not worth a model call if little but an earlier summary would be summarised (the
        // threshold may sit just above the system prompt and tool definitions).
        if fresh_tokens(&covered) < auto.at / 4 {
            return Flow::Continue;
        }
        log::info!("compacting the context of entry {} (~{before} tokens, threshold {})", ctx.entry, auto.at);
        let id = ctx.entry;
        (ctx.emit)(TurnEvent::Compacting { id, on: true });
        let result = summarize(ctx.llm, &covered, None, ctx.plan, ctx.cancel);
        (ctx.emit)(TurnEvent::Compacting { id, on: false });
        let summary = match result {
            Ok(s) => s,
            Err(Failure::Stopped) => return Flow::Stop("stopped".into()),
            Err(Failure::Error(e)) => {
                log::warn!("compaction failed: {e}");
                st.failed = true;
                let text = format!("Couldn't summarise the conversation to free context ({e}); older messages were shortened instead.");
                (ctx.emit)(TurnEvent::Notice { id, text });
                return Flow::Continue;
            }
        };
        *ctx.model_calls = ctx.model_calls.saturating_add(1);
        st.compactions = st.compactions.saturating_add(1);
        let (prompt, completion) = summary.usage.map_or((0, 0), |u| (u.prompt_tokens, u.completion_tokens));
        ctx.usage.prompt_tokens = ctx.usage.prompt_tokens.saturating_add(prompt);
        ctx.usage.completion_tokens = ctx.usage.completion_tokens.saturating_add(completion);
        let mut messages: Vec<ChatMessage> = ctx.messages.get(..start).unwrap_or_default().to_vec();
        messages.extend(summary_messages(&summary.text));
        messages.extend(tail);
        let after = ctx.plan.raw_tokens(&messages, ctx.specs);
        let to_u64 = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
        if after >= before {
            // A summary longer than what it replaces: keep the messages, stop trying.
            st.failed = true;
            (ctx.emit)(TurnEvent::Usage { id, prompt, completion, context: to_u64(before), estimated: true });
            let text = "The conversation summary came out longer than the conversation; older messages were shortened instead.".to_owned();
            (ctx.emit)(TurnEvent::Notice { id, text });
            return Flow::Continue;
        }
        *ctx.messages = messages;
        st.compacted_to = after;
        (ctx.emit)(TurnEvent::Usage { id, prompt, completion, context: to_u64(after), estimated: true });
        let made = Compacted {
            through,
            summary: summary.text,
            tokens_before: to_u64(before),
            tokens_after: to_u64(after),
            prompt_tokens: prompt,
            completion_tokens: completion,
        };
        (ctx.emit)(TurnEvent::Compacted { id, made });
        Flow::Continue
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use byob_llm::ToolCall;

    fn plan(context: usize) -> Plan {
        Plan { key: String::new(), prompt_tools: false, fallback: false, context, reserve: budget::default_reply_reserve(context), vision: None }
    }

    #[test]
    fn threshold_follows_window_and_override() {
        let o = CompactOptions::default();
        assert_eq!(o.threshold(&plan(262_144)), 262_144 / 100 * 80);
        // Never above where fit_history starts cutting.
        assert_eq!(CompactOptions { threshold_percent: 95, ..o }.threshold(&plan(8192)), 8192 - 1024);
        assert_eq!(CompactOptions { at_tokens: Some(3000), ..o }.threshold(&plan(262_144)), 3000);
        assert_eq!(CompactOptions { at_tokens: Some(0), ..o }.threshold(&plan(262_144)), 1);
        assert_eq!(CompactOptions { threshold_percent: 0, ..o }.threshold(&plan(10_000)), 1000);
    }

    #[test]
    fn compact_command_is_recognised() {
        assert_eq!(command("/compact"), Some(None));
        assert_eq!(command("  /compact   keep the CSV analysis details "), Some(Some("keep the CSV analysis details".into())));
        assert_eq!(command("/compactly"), None);
        assert_eq!(command("please /compact"), None);
    }

    fn entry(id: u64, role: EntryRole, text: &str) -> Entry {
        Entry {
            id,
            role,
            content: text.into(),
            reasoning: String::new(),
            done: true,
            error: None,
            stats: None,
            steps: vec![],
            undoable: false,
            undone: false,
            attachments: vec![],
        }
    }

    #[test]
    fn history_starts_after_the_latest_compaction() {
        let entries =
            vec![entry(1, EntryRole::User, "a"), entry(2, EntryRole::Assistant, "b"), entry(3, EntryRole::User, "c"), entry(4, EntryRole::Assistant, "d")];
        let (h, ids) = history(&entries, None, Some("e"), "sys");
        assert_eq!(h.len(), 6);
        assert_eq!(ids, [0, 1, 2, 3, 4, 0]);
        let (h, ids) = history(&entries, Some((2, "they said a, b")), Some("e"), "sys");
        let texts: Vec<_> = h.iter().map(|m| m.content.clone().unwrap()).collect();
        assert!(texts[1].starts_with(SUMMARY_HEADER) && texts[1].ends_with("they said a, b"));
        assert_eq!(&texts[2..], [SUMMARY_ACK, "c", "d", "e"]);
        assert_eq!(ids, [0, 0, 0, 3, 4, 0]);
    }

    #[test]
    fn split_keeps_the_newest_turns_that_fit() {
        let mut h = vec![ChatMessage::system("sys")];
        for i in 0..4 {
            h.push(ChatMessage::user(format!("q{i}")));
            h.push(ChatMessage::assistant(format!("a{i}")));
        }
        h.push(ChatMessage::user("new"));
        let end = h.len() - 1;
        assert_eq!(split_point(&h, end, 2, 10_000), Some(5), "q2 onwards kept");
        assert_eq!(split_point(&h, end, 0, 10_000), Some(end), "everything before the new message summarised");
        // A tiny room keeps no turn.
        assert_eq!(split_point(&h, end, 2, 1), Some(end));
        // Keeping more turns than there are leaves nothing to summarise.
        assert_eq!(split_point(&h, end, 9, 10_000), None);
        // Only an earlier summary before the kept turns: nothing new to summarise.
        let mut s = vec![ChatMessage::system("sys")];
        s.extend(summary_messages("old"));
        s.extend([ChatMessage::user("q"), ChatMessage::assistant("a"), ChatMessage::user("new")]);
        assert_eq!(split_point(&s, 5, 1, 10_000), None);
        assert_eq!(split_point(&s, 5, 0, 10_000), Some(5));
    }

    #[test]
    fn render_writes_tools_and_fits_the_room() {
        let call = ToolCall { id: "c1".into(), name: "read_file".into(), arguments: r#"{"path":"data.csv"}"#.into() };
        let mut h = vec![ChatMessage::system("sys")];
        h.extend(summary_messages("earlier: the user likes CSV"));
        h.push(ChatMessage::user("read data.csv"));
        h.push(ChatMessage::assistant_tool_calls(None, std::slice::from_ref(&call)));
        h.push(ChatMessage::tool_result("c1", format!("HEAD {} TAIL", "x".repeat(20_000))));
        h.push(ChatMessage::assistant("It has 3 rows."));
        let text = render(&h, 100_000);
        assert!(text.starts_with("### Summary of what came before\nearlier: the user likes CSV"), "{text}");
        assert!(!text.contains(SUMMARY_ACK) && !text.contains("sys"));
        assert!(text.contains("→ called `read_file` with {\"path\":\"data.csv\"}"));
        assert!(text.contains("### Result of `read_file`\nHEAD") && text.contains("TAIL") && text.contains("characters left out"));
        // Too little room: the oldest blocks go, the summary and the newest stay.
        let mut long = h.clone();
        for i in 0..50 {
            long.push(ChatMessage::user(format!("question {i} {}", "y".repeat(2000))));
        }
        let text = render(&long, 3_000);
        assert!(text.starts_with("### Summary"));
        assert!(text.contains("earlier messages left out") && text.contains("question 49"));
        assert!(budget::estimate_tokens(&text) <= 3_300, "{}", budget::estimate_tokens(&text));
        // Degenerate rooms don't panic.
        let _ = render(&long, 0);
        let _ = render(&[], 0);
    }

    #[test]
    fn resume_message_copies_the_request() {
        let call = ToolCall { id: "c1".into(), name: "t".into(), arguments: "{}".into() };
        let h = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("old"),
            ChatMessage::assistant("ok"),
            ChatMessage::user("do the thing"),
            toolprompt::fallback_assistant_message("", std::slice::from_ref(&call)),
            toolprompt::fallback_tool_result(&call, "r"),
        ];
        let m = resume_message(&h).unwrap();
        assert!(m.content.unwrap().starts_with("do the thing\n\n[Note: the context was compacted"));
    }

    #[test]
    fn summaries_are_cleaned_and_bounded() {
        assert_eq!(clean("```markdown\n## Goals\nx\n```"), "## Goals\nx");
        assert_eq!(clean("  ## Goals  "), "## Goals");
        assert!(clean(&"z".repeat(MAX_SUMMARY_CHARS + 5)).ends_with('…'));
    }

    #[test]
    fn tally_from_store_counts_manual_compactions_only_once() {
        let msg = |stats: Option<byob_store::MessageStats>| byob_store::StoredMessage {
            id: byob_store::MessageId(1),
            chat_id: "c".into(),
            role: byob_types::Role::Assistant,
            content: String::new(),
            reasoning: String::new(),
            done: true,
            error: None,
            stats,
            task_id: None,
            undone: false,
            created_at: 0,
            steps: vec![],
        };
        let stats = byob_store::MessageStats { prompt_tokens: 100, completion_tokens: 10, elapsed_ms: 1000, model_calls: 3 };
        let old = byob_store::MessageStats { model_calls: 0, ..stats };
        let comp = |manual| byob_store::Compaction {
            id: 1,
            chat_id: "c".into(),
            through: byob_store::MessageId(1),
            summary: String::new(),
            tokens_before: 0,
            tokens_after: 0,
            prompt_tokens: 50,
            completion_tokens: 5,
            focus: None,
            manual,
            created_at: 0,
        };
        let t = Tally::from_store(&[msg(Some(stats)), msg(Some(old)), msg(None)], &[comp(true), comp(false)]);
        assert_eq!((t.prompt_tokens, t.completion_tokens, t.model_calls, t.compactions, t.elapsed_ms), (250, 25, 5, 2, 2000));
    }
}
