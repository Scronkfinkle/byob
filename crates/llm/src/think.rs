//! Splits `<think>…</think>` spans out of streamed content, for servers that inline the
//! model's reasoning instead of sending `reasoning_content`. Tags may arrive split across
//! chunks, so the splitter holds back any suffix that could be the start of a tag.

use crate::StreamEvent;

const OPEN: &str = "<think>";
const CLOSE: &str = "</think>";

#[derive(Debug, Default)]
pub struct ThinkSplitter {
    in_think: bool,
    held: String,
}

impl ThinkSplitter {
    pub fn push(&mut self, chunk: &str) -> Vec<StreamEvent> {
        self.held.push_str(chunk);
        let mut out = Vec::new();
        loop {
            let tag = if self.in_think { CLOSE } else { OPEN };
            if let Some(pos) = self.held.find(tag) {
                let before: String = self.held.drain(..pos).collect();
                self.held.drain(..tag.len());
                self.emit(&mut out, before);
                self.in_think = !self.in_think;
                continue;
            }
            // Keep back the longest suffix that is a prefix of the tag.
            let keep = (1..tag.len())
                .rev()
                .find(|&k| self.held.is_char_boundary(self.held.len().saturating_sub(k)) && self.held.ends_with(tag.get(..k).unwrap_or_default()))
                .unwrap_or(0);
            let split = self.held.len().saturating_sub(keep);
            let ready: String = self.held.drain(..split).collect();
            self.emit(&mut out, ready);
            return out;
        }
    }

    /// Flush whatever is held at end of stream.
    pub fn finish(&mut self) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        let rest = std::mem::take(&mut self.held);
        self.emit(&mut out, rest);
        out
    }

    fn emit(&self, out: &mut Vec<StreamEvent>, text: String) {
        if text.is_empty() {
            return;
        }
        out.push(if self.in_think { StreamEvent::Reasoning(text) } else { StreamEvent::Content(text) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&str]) -> (String, String) {
        let mut s = ThinkSplitter::default();
        let mut evs: Vec<StreamEvent> = chunks.iter().flat_map(|c| s.push(c)).collect();
        evs.extend(s.finish());
        let (mut content, mut reasoning) = (String::new(), String::new());
        for e in evs {
            match e {
                StreamEvent::Content(t) => content.push_str(&t),
                StreamEvent::Reasoning(t) => reasoning.push_str(&t),
            }
        }
        (content, reasoning)
    }

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(run(&["hello ", "world"]), ("hello world".into(), String::new()));
    }

    #[test]
    fn splits_tags_across_chunks() {
        assert_eq!(run(&["<thi", "nk>hmm", "m</th", "ink>answer"]), ("answer".into(), "hmmm".into()));
    }

    #[test]
    fn lone_angle_bracket_is_not_swallowed() {
        assert_eq!(run(&["a < b", " and <t"]), ("a < b and <t".into(), String::new()));
    }

    #[test]
    fn multibyte_text_near_held_suffix() {
        assert_eq!(run(&["héllo <", "é"]), ("héllo <é".into(), String::new()));
    }
}
