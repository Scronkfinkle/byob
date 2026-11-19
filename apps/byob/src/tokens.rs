//! Token counts for the window (a reply's thinking size, `byob_ui_egui::count`): OpenAI's
//! o200k tokenizer, close to most models' own counts. Its vocabulary takes a moment to load,
//! so it loads on a thread at startup and the window estimates until then.

use std::sync::OnceLock;

use tiktoken_rs::CoreBPE;

static BPE: OnceLock<CoreBPE> = OnceLock::new();

/// Start loading the tokenizer and hand the window its counter.
pub fn install() {
    byob_ui_egui::count::install(count);
    let spawned = std::thread::Builder::new().name("tokenizer".into()).spawn(|| match tiktoken_rs::o200k_base() {
        Ok(bpe) => {
            let _ = BPE.set(bpe);
        }
        Err(e) => log::warn!("no tokenizer, token counts are estimates: {e}"),
    });
    if let Err(e) = spawned {
        log::warn!("no tokenizer, token counts are estimates: {e}");
    }
}

fn count(text: &str) -> Option<usize> {
    BPE.get().map(|b| b.encode_ordinary(text).len())
}

#[cfg(test)]
mod tests {
    #[test]
    fn counts_like_openai() {
        let bpe = tiktoken_rs::o200k_base().unwrap();
        assert_eq!(bpe.encode_ordinary("Hello, world!").len(), 4);
    }
}
