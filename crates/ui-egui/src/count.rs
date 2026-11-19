//! How big a reply's thinking is ("Thought · ~3.1k tokens"): in tokens (the default) or
//! characters, as the user picks (Settings → Appearance → Messages).
//!
//! Tokens come from a tokenizer the app installs ([`install`]; the desktop uses OpenAI's
//! o200k, close to most models' counts) and otherwise from an estimate of 3.5 characters a
//! token, so the browser build carries no vocabulary. Neither is the model's own tokenizer,
//! hence the "~". Counts are cached per entry and grow with the streamed text instead of
//! being redone every frame.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

use serde::{Deserialize, Serialize};

/// The unit thinking is measured in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SizeUnit {
    #[default]
    Tokens,
    Characters,
}

impl SizeUnit {
    pub const ALL: [Self; 2] = [Self::Tokens, Self::Characters];

    pub fn label(self) -> &'static str {
        match self {
            Self::Tokens => "Tokens",
            Self::Characters => "Characters",
        }
    }
}

/// Counts tokens, or `None` while the tokenizer isn't ready yet.
pub type Counter = fn(&str) -> Option<usize>;

static COUNTER: OnceLock<Counter> = OnceLock::new();
static CHARACTERS: AtomicBool = AtomicBool::new(false);

/// Most entries whose counts are kept.
const MAX_CACHED: usize = 4096;

/// Per entry: bytes counted, tokens in them, and whether that came from the tokenizer.
static CACHE: Mutex<Option<HashMap<u64, (usize, usize, bool)>>> = Mutex::new(None);

/// Use `counter` for token counts (once; later calls are ignored).
pub fn install(counter: Counter) {
    let _ = COUNTER.set(counter);
}

/// Measure in `unit` from now on.
pub fn set_unit(unit: SizeUnit) {
    CHARACTERS.store(unit == SizeUnit::Characters, Ordering::Relaxed);
}

/// Estimated tokens in `text`: 3.5 ASCII characters a token, one for any other character
/// (as the engine budgets, `byob_llm::estimate_tokens`).
pub fn estimate(text: &str) -> usize {
    let (ascii, other) = text.chars().fold((0usize, 0usize), |(a, o), c| if c.is_ascii() { (a + 1, o) } else { (a, o + 1) });
    ascii.saturating_mul(2).div_ceil(7).saturating_add(other)
}

/// Tokens in `text` and whether the tokenizer counted them.
fn tokens(text: &str) -> (usize, bool) {
    match COUNTER.get().and_then(|c| c(text)) {
        Some(n) => (n, true),
        None => (estimate(text), false),
    }
}

/// Tokens in entry `id`'s `text`, which only grows while it streams: only the new part is
/// counted (a token split across the seam may count twice, which "~" covers). A count
/// estimated before the tokenizer was ready is redone with it.
pub fn entry_tokens(id: u64, text: &str) -> usize {
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    let cache = cache.get_or_insert_with(HashMap::new);
    let tokenizer = COUNTER.get().is_some();
    let n = match cache.get(&id).copied() {
        Some((len, n, exact)) if len == text.len() && (exact || !tokenizer) => return n,
        Some((len, n, exact)) if len < text.len() && (exact || !tokenizer) && text.is_char_boundary(len) => {
            let (more, e) = tokens(text.get(len..).unwrap_or_default());
            (n.saturating_add(more), exact && e)
        }
        _ => tokens(text),
    };
    if cache.len() >= MAX_CACHED {
        cache.clear();
    }
    cache.insert(id, (text.len(), n.0, n.1));
    n.0
}

/// "~840 tokens", "~12.4k tokens", or "840 chars", "12.4k chars", in the chosen unit.
pub fn thinking_size(id: u64, text: &str) -> String {
    if CHARACTERS.load(Ordering::Relaxed) { short(text.chars().count(), "", "chars") } else { short(entry_tokens(id, text), "~", "tokens") }
}

fn short(n: usize, prefix: &str, unit: &str) -> String {
    if n < 1000 {
        format!("{prefix}{n} {unit}")
    } else {
        #[allow(clippy::cast_precision_loss)] // display only
        let k = n as f64 / 1000.0;
        format!("{prefix}{k:.1}k {unit}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_short() {
        assert_eq!(short(840, "", "chars"), "840 chars");
        assert_eq!(short(12_400, "~", "tokens"), "~12.4k tokens");
        assert_eq!(estimate("abcdefg"), 2);
        assert_eq!(estimate("héllo"), 2 + 1);
    }

    /// Streaming text is counted as it grows, and a changed entry (not a continuation) is
    /// counted again.
    #[test]
    fn counts_grow_with_the_text() {
        let id = 9_000_001;
        let a = "Let me think about this. ".repeat(10);
        let first = entry_tokens(id, &a);
        assert_eq!(first, estimate(&a));
        let more = format!("{a}{}", "And then some more. ".repeat(5));
        assert_eq!(entry_tokens(id, &more), first + estimate(&"And then some more. ".repeat(5)));
        assert_eq!(entry_tokens(id, "short"), estimate("short"), "shorter: counted again");
    }
}
