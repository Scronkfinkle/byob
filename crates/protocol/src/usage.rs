//! Context usage of a chat and its compactions.
//!
//! When a conversation nears the model's context window, the engine asks the model for a
//! dense summary and continues from that summary plus the newest turns ("compaction"). The
//! transcript keeps every message; clients show a divider where a compaction happened.

use serde::{Deserialize, Serialize};

/// One compaction of the chat in view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Compaction {
    pub id: u64,
    /// The summary stands in for every entry up to and including this one (0: it was made
    /// before the chat's first entry finished). Clients draw the divider after it.
    pub after_entry: u64,
    /// Estimated tokens of the request before and after.
    pub tokens_before: u64,
    pub tokens_after: u64,
    /// The summary the model wrote (markdown).
    pub summary: String,
    /// The user asked for it (`/compact`), rather than the context threshold.
    #[serde(default)]
    pub manual: bool,
    /// What the user asked the summary to keep.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<String>,
    /// Milliseconds since the Unix epoch.
    pub created_at: i64,
}

impl Compaction {
    /// "Context compacted · 182k → 9k tokens".
    pub fn label(&self) -> String {
        format!("Context compacted · {} → {} tokens", short_tokens(self.tokens_before), short_tokens(self.tokens_after))
    }
}

/// Token use of one chat, for the header meter.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatUsage {
    pub chat: String,
    /// Tokens the conversation takes in the context now (the last request's prompt and reply
    /// as the server counted them, or an estimate of the next request).
    pub context_used: u64,
    /// `context_used` is the engine's estimate, not the server's count.
    #[serde(default)]
    pub estimated: bool,
    /// The context window in tokens (a default when the server doesn't say).
    pub context_window: u64,
    /// Requests bigger than this are compacted first (0: never automatically).
    pub compact_at: u64,
    /// Prompt and completion tokens of the most recent model request.
    #[serde(default)]
    pub last_prompt_tokens: u64,
    #[serde(default)]
    pub last_completion_tokens: u64,
    /// Totals over the whole chat, as reported by the server.
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    /// Model requests, tool rounds and compactions included.
    #[serde(default)]
    pub model_calls: u64,
    #[serde(default)]
    pub compactions: u64,
    /// Time spent in replies and compactions.
    #[serde(default)]
    pub elapsed_ms: u64,
    /// A compaction is running in this chat.
    #[serde(default)]
    pub compacting: bool,
    /// Tokens subagents used (not in the chat's context; not in the totals above).
    #[serde(default)]
    pub agent_prompt_tokens: u64,
    #[serde(default)]
    pub agent_completion_tokens: u64,
    /// Model requests subagents made.
    #[serde(default)]
    pub agent_calls: u64,
}

impl ChatUsage {
    /// `context_used` as a fraction of the window, 0..=1.
    pub fn fraction(&self) -> f32 {
        ratio(self.context_used, self.context_window)
    }

    /// `context_used` as a fraction of the compaction threshold (1 = compacts next).
    pub fn pressure(&self) -> f32 {
        if self.compact_at == 0 { self.fraction() } else { ratio(self.context_used, self.compact_at) }
    }

    /// "34k / 262k".
    pub fn label(&self) -> String {
        format!("{}{} / {}", if self.estimated { "~" } else { "" }, short_tokens(self.context_used), short_tokens(self.context_window))
    }

    /// "main 34k · agents 120k" (tokens in and out), or `None` if no subagent ran.
    pub fn split_label(&self) -> Option<String> {
        let agents = self.agent_prompt_tokens.saturating_add(self.agent_completion_tokens);
        if agents == 0 && self.agent_calls == 0 {
            return None;
        }
        let main = self.prompt_tokens.saturating_add(self.completion_tokens);
        Some(format!("main {} · agents {}", short_tokens(main), short_tokens(agents)))
    }
}

#[allow(clippy::cast_precision_loss)] // display only
fn ratio(a: u64, b: u64) -> f32 {
    if b == 0 { 0.0 } else { (a as f64 / b as f64).clamp(0.0, 1.0) as f32 }
}

/// "950", "9.4k", "9k", "34k", "1.2M".
pub fn short_tokens(n: u64) -> String {
    #[allow(clippy::cast_precision_loss)] // display only
    let f = n as f64;
    match n {
        0..1_000 => n.to_string(),
        1_000..10_000 => format!("{:.1}k", f / 1_000.0).replace(".0k", "k"),
        10_000..1_000_000 => format!("{}k", n / 1_000),
        _ => format!("{:.1}M", f / 1_000_000.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_read_naturally() {
        assert_eq!(short_tokens(950), "950");
        assert_eq!(short_tokens(9_400), "9.4k");
        assert_eq!(short_tokens(262_144), "262k");
        assert_eq!(short_tokens(1_200_000), "1.2M");
        let u = ChatUsage { context_used: 34_000, context_window: 262_144, compact_at: 200_000, ..Default::default() };
        assert_eq!(u.label(), "34k / 262k");
        assert!((u.pressure() - 0.17).abs() < 0.001);
        assert_eq!(ChatUsage { estimated: true, ..u.clone() }.label(), "~34k / 262k");
        assert_eq!(ChatUsage::default().fraction(), 0.0);
        assert_eq!(u.split_label(), None);
        let split = ChatUsage { prompt_tokens: 30_000, completion_tokens: 4_000, agent_prompt_tokens: 110_000, agent_completion_tokens: 10_000, ..u.clone() };
        assert_eq!(split.split_label().as_deref(), Some("main 34k · agents 120k"));
        let c = Compaction {
            id: 1,
            after_entry: 4,
            tokens_before: 182_000,
            tokens_after: 9_000,
            summary: String::new(),
            manual: false,
            focus: None,
            created_at: 0,
        };
        assert_eq!(c.label(), "Context compacted · 182k → 9k tokens");
    }
}
