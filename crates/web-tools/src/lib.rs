//! byob's web tools: the bundled `web` plugin's backend, in-process. `web_search` returns a
//! short list of results (title, link, snippet) from the first search provider that answers
//! (`search`); `web_fetch` returns a page as readable text, a window at a time (`fetch`).
//! The plugin's manifest (`crates/bundled/plugins/web`) names it with
//! `"backend": {"native": "web"}`; the app registers [`factory`] under [`ID`].
//!
//! Everything a server sends is hostile: requests have deadlines, redirect limits and size
//! caps (`http`), HTML goes through a tokenizer that can't fail or recurse (`html`), and
//! nothing here panics on what it reads. Blocking I/O on the backend's own thread, no async.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

pub mod fetch;
pub mod html;
pub mod http;
pub mod search;

#[cfg(test)]
mod fake;

use std::sync::Arc;

use byob_plugins::native::{NativeBackend, NativeFactory, Outbox};
use serde_json::Value;

use crate::fetch::{DEFAULT_CHARS, FetchArgs, Fetcher, Reader};
use crate::http::Http;
use crate::search::{Provider, SearchConfig};

/// The id manifests name (`"backend": {"native": "web"}`).
pub const ID: &str = "web";
/// The tools it serves.
pub const TOOLS: [&str; 2] = ["web_search", "web_fetch"];

/// The backend: an HTTP client, the settings from `init`, and the fetch cache.
#[derive(Default)]
pub struct Web {
    http: Http,
    search: SearchConfig,
    fetcher: Fetcher,
}

pub fn factory() -> NativeFactory {
    Arc::new(|| Box::new(Web::default()) as Box<dyn NativeBackend>)
}

/// A setting's value from `init` (`settings`, or `secrets` for secret ones), blank as unset.
fn setting(init: &Value, key: &str) -> Option<String> {
    ["settings", "secrets"].iter().find_map(|m| init.get(m)?.get(key)?.as_str().map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned))
}

fn off(v: Option<&str>) -> bool {
    v.is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "false" | "0" | "off" | "no"))
}

impl Web {
    /// Apply the plugin's settings (see the manifest).
    pub fn configure(&mut self, init: &Value, out: Option<&Outbox>) {
        let provider = setting(init, "SEARCH_PROVIDER");
        self.search.provider = provider.as_deref().and_then(Provider::parse);
        if let Some(p) = provider.as_deref().filter(|p| self.search.provider.is_none() && !p.eq_ignore_ascii_case("auto"))
            && let Some(out) = out
        {
            out.fault(&format!("SEARCH_PROVIDER {p:?} isn't one of auto, searxng, brave, tavily, exa, wikipedia; using auto"));
        }
        self.search.searxng_url = setting(init, "SEARXNG_URL");
        self.search.brave_key = setting(init, "BRAVE_API_KEY");
        self.search.tavily_key = setting(init, "TAVILY_API_KEY");
        self.search.exa_key = setting(init, "EXA_API_KEY");
        self.fetcher.reader = (!off(setting(init, "READER_FALLBACK").as_deref())).then(|| Reader { key: setting(init, "JINA_API_KEY"), ..Reader::default() });
    }

    /// A tool call: the text for the model, or the error sentence.
    pub fn tool(&mut self, name: &str, args: &Value) -> Result<Value, String> {
        match name {
            "web_search" => {
                let query = args.get("query").and_then(Value::as_str).map(str::trim).unwrap_or_default();
                if query.is_empty() {
                    return Err("Give a search query (query).".into());
                }
                let query: String = query.chars().take(400).collect();
                let count = uint(args, "count").unwrap_or(5);
                let (provider, hits, problems) = search::search(&self.http, &self.search, &query, count)?;
                Ok(Value::String(search::format(&query, provider, &hits, &problems)))
            }
            "web_fetch" => {
                let url = args.get("url").and_then(Value::as_str).unwrap_or_default().to_owned();
                let start = uint(args, "start").unwrap_or(0);
                let max_chars = uint(args, "max_chars").unwrap_or(DEFAULT_CHARS);
                let links = args.get("links").is_some_and(|v| v == &Value::Bool(true) || v.as_str() == Some("true"));
                self.fetcher.fetch(&self.http, &FetchArgs { url, start, max_chars, links }).map(Value::String)
            }
            other => Err(format!("the web backend has no tool {other:?}")),
        }
    }
}

/// A non-negative whole number argument (models send `5`, `5.0` or `"5"`).
fn uint(args: &Value, key: &str) -> Option<usize> {
    let v = args.get(key)?;
    let n = v.as_u64().or_else(|| v.as_f64().filter(|f| f.is_finite() && *f >= 0.0).map(|f| f as u64)).or_else(|| v.as_str()?.trim().parse().ok())?;
    usize::try_from(n).ok()
}

impl NativeBackend for Web {
    fn init(&mut self, init: &Value, out: &Outbox) {
        self.configure(init, Some(out));
    }

    fn request(&mut self, method: &str, params: &Value, _out: &Outbox) -> Result<Value, String> {
        match method {
            "tool" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
                self.tool(name, params.get("args").unwrap_or(&Value::Null))
            }
            "hook" => Ok(serde_json::json!({"action": "continue"})),
            other => Err(format!("the web backend doesn't answer {other:?} requests")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn settings_come_from_init_and_secrets() {
        let mut w = Web::default();
        w.configure(
            &json!({"settings": {"SEARCH_PROVIDER": " Brave ", "SEARXNG_URL": "  ", "READER_FALLBACK": "false"}, "secrets": {"BRAVE_API_KEY": "k", "TAVILY_API_KEY": ""}}),
            None,
        );
        assert_eq!(w.search.provider, Some(Provider::Brave));
        assert_eq!((w.search.searxng_url.as_deref(), w.search.brave_key.as_deref(), w.search.tavily_key.as_deref()), (None, Some("k"), None));
        assert!(w.fetcher.reader.is_none());
        w.configure(&json!({"settings": {}}), None);
        assert_eq!(w.search.provider, None);
        assert!(w.fetcher.reader.is_some(), "the reader fallback is on by default");
    }

    #[test]
    fn bad_arguments_are_sentences() {
        let mut w = Web::default();
        assert_eq!(w.tool("web_search", &json!({"query": "  "})).unwrap_err(), "Give a search query (query).");
        assert_eq!(w.tool("web_fetch", &json!({})).unwrap_err(), "Give a URL to read (url).");
        assert!(w.tool("web_fetch", &json!({"url": "ftp://x"})).unwrap_err().contains("not ftp:"));
        assert!(w.tool("nope", &json!({})).is_err());
        assert_eq!(uint(&json!({"n": "7"}), "n"), Some(7));
        assert_eq!(uint(&json!({"n": 3.0}), "n"), Some(3));
        assert_eq!(uint(&json!({"n": -1}), "n"), None);
    }

    #[test]
    fn served_through_the_wire_dispatch() {
        let server = fake::Server::start(|_| fake::Reply::html(&format!("<main><p>{}</p></main>", "hello world ".repeat(40))));
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let s = Arc::clone(&seen);
        let out = Outbox::new(move |m| s.lock().unwrap().push(m));
        let mut w = Web::default();
        byob_plugins::native::dispatch(&mut w, &json!({"type": "init", "settings": {"SEARCH_PROVIDER": "bogus"}, "secrets": {}}), &out);
        byob_plugins::native::dispatch(
            &mut w,
            &json!({"type": "request", "id": 1, "method": "tool", "params": {"name": "web_fetch", "args": {"url": server.url("/p")}}}),
            &out,
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0]["type"], "fault", "an unknown provider is a fault: {:?}", seen[0]);
        assert_eq!(seen[1]["type"], "reply");
        assert!(seen[1]["result"].as_str().unwrap().contains("hello world hello"), "{:?}", seen[1]);
    }

    /// Against the real services (network): `cargo test -p byob-web-tools live -- --ignored
    /// --nocapture`, optionally with `BYOB_WEB_QUERY` and `BYOB_WEB_URL`.
    #[test]
    #[ignore = "uses the network"]
    fn live() {
        let mut w = Web::default();
        w.configure(&json!({"settings": {}}), None);
        let query = std::env::var("BYOB_WEB_QUERY").unwrap_or_else(|_| "llama.cpp latest release".into());
        match w.tool("web_search", &json!({"query": query})) {
            Ok(v) => println!("{}\n", v.as_str().unwrap_or_default()),
            Err(e) => println!("search failed: {e}\n"),
        }
        let url = std::env::var("BYOB_WEB_URL").unwrap_or_else(|_| "https://en.wikipedia.org/wiki/Llama.cpp".into());
        match w.tool("web_fetch", &json!({"url": url, "max_chars": 3000})) {
            Ok(v) => println!("{}", v.as_str().unwrap_or_default()),
            Err(e) => println!("fetch failed: {e}"),
        }
    }
}
