//! `web_search`: providers, tried in order until one answers with results.
//!
//! - **SearXNG** (`SEARXNG_URL`): the user's own metasearch instance (JSON format on).
//! - **Brave Search API** (`BRAVE_API_KEY`), **Tavily** (`TAVILY_API_KEY`): keyed APIs.
//! - **Exa** through its hosted MCP endpoint: works without a key (rate-limited; with
//!   `EXA_API_KEY`, the user's own quota). The default when nothing is configured.
//! - **Wikipedia** (the MediaWiki search API): always there, the last resort.
//!
//! No search engine's HTML is scraped: that breaks their terms and breaks often.

use std::time::Duration;

use serde_json::{Value, json};
use url::Url;

use crate::html::{collapse, strip_tags, truncate};
use crate::http::{BYOB_UA, Http, Limits};

/// One result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
    /// When it was published, if the provider says.
    pub date: Option<String>,
}

/// The providers by name, as `SEARCH_PROVIDER` takes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Searxng,
    Brave,
    Tavily,
    Exa,
    Wikipedia,
}

impl Provider {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "searxng" | "searx" => Self::Searxng,
            "brave" => Self::Brave,
            "tavily" => Self::Tavily,
            "exa" => Self::Exa,
            "wikipedia" | "wiki" => Self::Wikipedia,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Searxng => "SearXNG",
            Self::Brave => "Brave Search",
            Self::Tavily => "Tavily",
            Self::Exa => "Exa",
            Self::Wikipedia => "Wikipedia",
        }
    }
}

/// Where each provider is (real ones by default; tests point them at a fake server).
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub exa_mcp: String,
    pub brave: String,
    pub tavily: String,
    pub wikipedia: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            exa_mcp: "https://mcp.exa.ai/mcp".into(),
            brave: "https://api.search.brave.com/res/v1/web/search".into(),
            tavily: "https://api.tavily.com/search".into(),
            wikipedia: "https://en.wikipedia.org/w/api.php".into(),
        }
    }
}

/// The plugin's search settings.
#[derive(Debug, Clone, Default)]
pub struct SearchConfig {
    /// `SEARCH_PROVIDER`: `None` is `auto`.
    pub provider: Option<Provider>,
    pub searxng_url: Option<String>,
    pub brave_key: Option<String>,
    pub tavily_key: Option<String>,
    pub exa_key: Option<String>,
    pub endpoints: Endpoints,
}

impl SearchConfig {
    /// Providers to try, in order: the chosen one first, then those configured, then Exa
    /// and Wikipedia (which need nothing).
    pub fn order(&self) -> Vec<Provider> {
        let mut out = Vec::new();
        let mut add = |p: Provider| {
            if !out.contains(&p) {
                out.push(p);
            }
        };
        if let Some(p) = self.provider {
            add(p);
        }
        if self.searxng_url.is_some() {
            add(Provider::Searxng);
        }
        if self.brave_key.is_some() {
            add(Provider::Brave);
        }
        if self.tavily_key.is_some() {
            add(Provider::Tavily);
        }
        add(Provider::Exa);
        add(Provider::Wikipedia);
        out
    }
}

const LIMITS: Limits = Limits { timeout: Duration::from_secs(15), max_bytes: 2 * 1024 * 1024 };
const SNIPPET_CHARS: usize = 300;
const TITLE_CHARS: usize = 150;
pub const MAX_COUNT: usize = 10;

/// Search with the first provider that answers; `(provider, hits, problems on the way)`.
pub fn search(http: &Http, cfg: &SearchConfig, query: &str, count: usize) -> Result<(Provider, Vec<Hit>, Vec<String>), String> {
    let count = count.clamp(1, MAX_COUNT);
    let mut problems = Vec::new();
    for p in cfg.order() {
        match run(http, cfg, p, query, count) {
            Ok(hits) if !hits.is_empty() => return Ok((p, dedup(hits, count), problems)),
            Ok(_) => problems.push(format!("{}: no results", p.label())),
            Err(e) => problems.push(format!("{}: {e}", p.label())),
        }
    }
    Err(format!("No search results. {}", problems.join("; ")))
}

fn dedup(hits: Vec<Hit>, count: usize) -> Vec<Hit> {
    let mut out: Vec<Hit> = Vec::new();
    for h in hits {
        if h.url.is_empty() || out.iter().any(|o| o.url == h.url) {
            continue;
        }
        out.push(Hit { title: truncate(&h.title, TITLE_CHARS), snippet: truncate(&h.snippet, SNIPPET_CHARS), ..h });
        if out.len() >= count {
            break;
        }
    }
    out
}

fn run(http: &Http, cfg: &SearchConfig, p: Provider, query: &str, count: usize) -> Result<Vec<Hit>, String> {
    match p {
        Provider::Searxng => {
            let base = cfg.searxng_url.as_deref().ok_or("set its URL (SEARXNG_URL) in the plugin's settings")?;
            searxng(http, base, query, count)
        }
        Provider::Brave => brave(http, &cfg.endpoints.brave, cfg.brave_key.as_deref().ok_or("set BRAVE_API_KEY in the plugin's settings")?, query, count),
        Provider::Tavily => tavily(http, &cfg.endpoints.tavily, cfg.tavily_key.as_deref().ok_or("set TAVILY_API_KEY in the plugin's settings")?, query, count),
        Provider::Exa => exa(http, &cfg.endpoints.exa_mcp, cfg.exa_key.as_deref(), query, count),
        Provider::Wikipedia => wikipedia(http, &cfg.endpoints.wikipedia, query, count),
    }
}

fn with_params(base: &str, params: &[(&str, &str)]) -> Result<String, String> {
    let u = Url::parse_with_params(base.trim(), params).map_err(|e| format!("bad URL {base:?}: {e}"))?;
    if !matches!(u.scheme(), "http" | "https") {
        return Err(format!("{base:?} isn't an http(s) URL"));
    }
    Ok(u.to_string())
}

fn json_body(r: &crate::http::Response) -> Result<Value, String> {
    if !r.ok() {
        let text = collapse(&r.text());
        return Err(format!("HTTP {}{}", r.status, if text.is_empty() { String::new() } else { format!(": {}", truncate(&text, 200)) }));
    }
    serde_json::from_slice(&r.body).map_err(|_| "the answer wasn't JSON".to_owned())
}

fn s(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or_default().trim().to_owned()
}

fn opt(v: &Value, key: &str) -> Option<String> {
    Some(s(v, key)).filter(|d| !d.is_empty())
}

fn searxng(http: &Http, base: &str, query: &str, count: usize) -> Result<Vec<Hit>, String> {
    let base = base.trim().trim_end_matches('/');
    let base = base.strip_suffix("/search").unwrap_or(base);
    let url = with_params(&format!("{base}/search"), &[("q", query), ("format", "json")])?;
    let r = http.get(&url, &[("Accept", "application/json")], LIMITS)?;
    if r.status == 403 {
        return Err("the instance refused format=json (enable it under search.formats in its settings.yml)".into());
    }
    let v = json_body(&r)?;
    Ok(list(&v, "results")
        .iter()
        .take(count * 2)
        .map(|h| Hit { title: s(h, "title"), url: s(h, "url"), snippet: collapse(&s(h, "content")), date: opt(h, "publishedDate").map(|d| day(&d)) })
        .collect())
}

fn brave(http: &Http, endpoint: &str, key: &str, query: &str, count: usize) -> Result<Vec<Hit>, String> {
    let url = with_params(endpoint, &[("q", query), ("count", &count.to_string())])?;
    let v = json_body(&http.get(&url, &[("Accept", "application/json"), ("X-Subscription-Token", key)], LIMITS)?)?;
    let web = v.get("web").unwrap_or(&Value::Null);
    Ok(list(web, "results")
        .iter()
        .map(|h| Hit {
            title: strip_tags(&s(h, "title")),
            url: s(h, "url"),
            snippet: strip_tags(&s(h, "description")),
            date: opt(h, "page_age").map(|d| day(&d)),
        })
        .collect())
}

fn tavily(http: &Http, endpoint: &str, key: &str, query: &str, count: usize) -> Result<Vec<Hit>, String> {
    let body = json!({"query": query, "max_results": count, "search_depth": "basic"});
    let auth = format!("Bearer {key}");
    let v = json_body(&http.post_json(endpoint, &[("Authorization", &auth)], &body, LIMITS)?)?;
    Ok(list(&v, "results")
        .iter()
        .map(|h| Hit { title: s(h, "title"), url: s(h, "url"), snippet: collapse(&s(h, "content")), date: opt(h, "published_date").map(|d| day(&d)) })
        .collect())
}

fn wikipedia(http: &Http, endpoint: &str, query: &str, count: usize) -> Result<Vec<Hit>, String> {
    let url = with_params(endpoint, &[("action", "query"), ("list", "search"), ("format", "json"), ("srlimit", &count.to_string()), ("srsearch", query)])?;
    let v = json_body(&http.get(&url, &[("User-Agent", BYOB_UA), ("Accept", "application/json")], LIMITS)?)?;
    let site = Url::parse(endpoint).ok().map(|u| u.origin().ascii_serialization()).unwrap_or_default();
    Ok(list(v.get("query").unwrap_or(&Value::Null), "search")
        .iter()
        .map(|h| {
            let title = s(h, "title");
            let page = title.replace(' ', "_");
            let page: String = url::form_urlencoded::byte_serialize(page.as_bytes()).collect();
            Hit { url: format!("{site}/wiki/{page}"), snippet: strip_tags(&s(h, "snippet")), title, date: None }
        })
        .collect())
}

fn list<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key).and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default()
}

/// `2026-11-11T02:52:55Z` → `2026-11-11`.
fn day(d: &str) -> String {
    let d = d.trim();
    match d.get(..10) {
        Some(head) if head.as_bytes().get(4) == Some(&b'-') && head.as_bytes().get(7) == Some(&b'-') => head.to_owned(),
        _ => truncate(d, 40),
    }
}

/// Exa's hosted MCP server (`web_search_exa`): a stateless JSON-RPC call over HTTP.
fn exa(http: &Http, endpoint: &str, key: Option<&str>, query: &str, count: usize) -> Result<Vec<Hit>, String> {
    let url = match key {
        Some(k) => with_params(endpoint, &[("exaApiKey", k)])?,
        None => endpoint.to_owned(),
    };
    let call = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "web_search_exa", "arguments": {"query": query, "numResults": count, "objective": query}}
    });
    let r = http.post_json(&url, &[("Accept", "application/json, text/event-stream")], &call, LIMITS)?;
    if !r.ok() {
        return Err(format!("HTTP {}{}", r.status, if r.status == 429 { " (rate limited; an EXA_API_KEY raises the limit)" } else { "" }));
    }
    let msg = rpc_message(&r.text()).ok_or("the answer wasn't a JSON-RPC message")?;
    if let Some(e) = msg.get("error") {
        return Err(truncate(&collapse(&e.get("message").and_then(Value::as_str).map_or_else(|| e.to_string(), str::to_owned)), 200));
    }
    let result = msg.get("result").unwrap_or(&Value::Null);
    let text: Vec<&str> = list(result, "content").iter().filter_map(|c| c.get("text").and_then(Value::as_str)).collect();
    if result.get("isError") == Some(&Value::Bool(true)) {
        return Err(truncate(&collapse(&text.join(" ")), 200));
    }
    Ok(parse_exa(&text.join("\n")))
}

/// The JSON-RPC answer in a body that is plain JSON or server-sent events.
fn rpc_message(body: &str) -> Option<Value> {
    if let Ok(v) = serde_json::from_str::<Value>(body.trim()) {
        return Some(v);
    }
    body.lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
        .find(|v| v.get("result").is_some() || v.get("error").is_some())
}

/// Exa's text: blocks of `Title: …`, `URL: …`, `Published: …`, `Author: …`, `Highlights:`
/// (or `Text:`) and the page's lines.
pub fn parse_exa(text: &str) -> Vec<Hit> {
    let mut out: Vec<Hit> = Vec::new();
    let mut body: Vec<&str> = Vec::new();
    let mut in_body = false;
    let flush = |out: &mut Vec<Hit>, body: &mut Vec<&str>| {
        if let Some(h) = out.last_mut()
            && h.snippet.is_empty()
        {
            let lines: Vec<&str> =
                body.iter().map(|l| l.trim().trim_start_matches('#').trim()).filter(|l| !l.is_empty() && *l != "..." && *l != h.title).collect();
            h.snippet = truncate(&collapse(&lines.join(" ")), SNIPPET_CHARS);
        }
        body.clear();
    };
    for line in text.lines() {
        if let Some(t) = line.strip_prefix("Title: ") {
            flush(&mut out, &mut body);
            in_body = false;
            out.push(Hit { title: t.trim().to_owned(), url: String::new(), snippet: String::new(), date: None });
        } else if let (Some(u), Some(h)) = (line.strip_prefix("URL: "), out.last_mut())
            && !in_body
        {
            h.url = u.trim().to_owned();
        } else if let (Some(d), Some(h)) = (line.strip_prefix("Published: "), out.last_mut())
            && !in_body
        {
            let d = d.trim();
            if !d.is_empty() && d != "N/A" {
                h.date = Some(day(d));
            }
        } else if !in_body && (line.starts_with("Author: ") || line.starts_with("Image: ") || line.starts_with("Favicon: ") || line.starts_with("ID: ")) {
        } else if !in_body && (line.trim() == "Highlights:" || line.trim() == "Text:" || line.trim() == "Summary:") {
            in_body = true;
        } else if line.trim() == "---" {
            flush(&mut out, &mut body);
            in_body = false;
        } else if !out.is_empty() && body.len() < 200 {
            in_body = true;
            body.push(line);
        }
    }
    flush(&mut out, &mut body);
    out.retain(|h| h.url.starts_with("http://") || h.url.starts_with("https://"));
    out
}

/// Hits as the model reads them: numbered, one line each for title, link and snippet.
pub fn format(query: &str, provider: Provider, hits: &[Hit], problems: &[String]) -> String {
    let mut out = format!("Results for \"{}\" (via {}):\n", truncate(&collapse(query), 200), provider.label());
    for (i, h) in hits.iter().enumerate() {
        out.push_str(&format!("\n{}. {}\n   {}", i + 1, if h.title.is_empty() { "(untitled)" } else { &h.title }, h.url));
        if let Some(d) = &h.date {
            out.push_str(&format!(" ({d})"));
        }
        if !h.snippet.is_empty() {
            out.push_str(&format!("\n   {}", h.snippet));
        }
    }
    if !problems.is_empty() {
        out.push_str(&format!("\n\n(Skipped: {}.)", problems.join("; ")));
    }
    out.push_str("\n\nRead a page with web_fetch(url) before relying on a snippet.");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{Reply, Server};

    fn cfg(server: &Server) -> SearchConfig {
        SearchConfig {
            endpoints: Endpoints {
                exa_mcp: server.url("/exa/mcp"),
                brave: server.url("/brave/search"),
                tavily: server.url("/tavily/search"),
                wikipedia: server.url("/w/api.php"),
            },
            ..Default::default()
        }
    }

    const EXA_TEXT: &str = "Title: Releases · ggml-org/llama.cpp\nURL: https://github.com/ggml-org/llama.cpp/releases\nPublished: 2026-11-11T02:52:55Z\nAuthor: N/A\nHighlights:\n# Releases\n\nb12292\n\nwebgpu: add PAD_REFLECT_1D support\n\n---\n\nTitle: llama.cpp\nURL: https://github.com/ggml-org/llama.cpp\nPublished: N/A\nAuthor: N/A\nHighlights:\nLLM inference in C/C++\nURL: https://not-the-url.example\n";

    #[test]
    fn exa_text_becomes_hits() {
        let hits = parse_exa(EXA_TEXT);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "Releases · ggml-org/llama.cpp");
        assert_eq!(hits[0].url, "https://github.com/ggml-org/llama.cpp/releases");
        assert_eq!(hits[0].date.as_deref(), Some("2026-11-11"));
        assert_eq!(hits[0].snippet, "Releases b12292 webgpu: add PAD_REFLECT_1D support");
        assert_eq!(hits[1].url, "https://github.com/ggml-org/llama.cpp", "a URL line in the page text isn't the result's");
        assert_eq!(hits[1].date, None);
        assert!(parse_exa("garbage\nURL: x\n---").is_empty());
    }

    #[test]
    fn exa_is_the_keyless_default_over_mcp() {
        let server = Server::start(|req| {
            assert_eq!((req.method.as_str(), req.target.as_str()), ("POST", "/exa/mcp"));
            let v: Value = serde_json::from_str(&req.body).unwrap();
            assert_eq!(v["method"], "tools/call");
            assert_eq!(v["params"]["arguments"]["query"], "llama.cpp release");
            assert_eq!(v["params"]["arguments"]["numResults"], 3);
            let msg = json!({"jsonrpc": "2.0", "id": 1, "result": {"content": [{"type": "text", "text": EXA_TEXT}]}});
            Reply::new(200, "text/event-stream", format!("event: message\ndata: {msg}\n\n"))
        });
        let (p, hits, problems) = search(&Http::new(), &cfg(&server), "llama.cpp release", 3).unwrap();
        assert_eq!((p, hits.len(), problems.len()), (Provider::Exa, 2, 0));
        let text = format("llama.cpp release", p, &hits, &problems);
        assert!(text.starts_with("Results for \"llama.cpp release\" (via Exa):\n\n1. Releases · ggml-org/llama.cpp\n   https://github.com/ggml-org/llama.cpp/releases (2026-11-11)\n   Releases b12292"), "{text}");
    }

    #[test]
    fn a_failing_provider_falls_through_to_wikipedia() {
        let server = Server::start(|req| {
            if req.target.starts_with("/exa") {
                return Reply::new(429, "text/plain", "slow down");
            }
            assert!(req.target.starts_with("/w/api.php?action=query&list=search&format=json&srlimit=5&srsearch=rust+lang"), "{}", req.target);
            assert!(req.header("user-agent").unwrap().starts_with("byob/"));
            Reply::json(&json!({"query": {"search": [
                {"title": "Rust (programming language)", "snippet": "<span class=\"searchmatch\">Rust</span> is a &quot;language&quot;"},
                {"title": "Rust (programming language)", "snippet": "duplicate"}
            ]}}))
        });
        let (p, hits, problems) = search(&Http::new(), &cfg(&server), "rust lang", 5).unwrap();
        assert_eq!(p, Provider::Wikipedia);
        assert_eq!(hits.len(), 1, "duplicates go");
        assert_eq!(hits[0].url, format!("{}/wiki/Rust_%28programming_language%29", server.base));
        assert_eq!(hits[0].snippet, "Rust is a \"language\"");
        assert_eq!(problems, ["Exa: HTTP 429 (rate limited; an EXA_API_KEY raises the limit)"]);
        assert!(format("q", p, &hits, &problems).contains("(Skipped: Exa: HTTP 429"));
    }

    #[test]
    fn configured_providers_come_first_with_their_keys() {
        let server = Server::start(|req| {
            if req.target.starts_with("/brave") {
                assert_eq!(req.header("x-subscription-token"), Some("bk"));
                assert!(req.target.contains("q=a+b") && req.target.contains("count=2"), "{}", req.target);
                return Reply::new(500, "text/plain", "down");
            }
            if req.target.starts_with("/tavily") {
                assert_eq!(req.header("authorization"), Some("Bearer tk"));
                let v: Value = serde_json::from_str(&req.body).unwrap();
                assert_eq!((v["query"].as_str(), v["max_results"].as_u64()), (Some("a b"), Some(2)));
                return Reply::json(
                    &json!({"results": [{"title": "T", "url": "https://t.example/", "content": "tavily  says", "published_date": "2026-01-02"}]}),
                );
            }
            panic!("unexpected {}", req.target)
        });
        let c = SearchConfig { brave_key: Some("bk".into()), tavily_key: Some("tk".into()), ..cfg(&server) };
        assert_eq!(c.order(), [Provider::Brave, Provider::Tavily, Provider::Exa, Provider::Wikipedia]);
        let (p, hits, problems) = search(&Http::new(), &c, "a b", 2).unwrap();
        assert_eq!(p, Provider::Tavily);
        assert_eq!(hits, [Hit { title: "T".into(), url: "https://t.example/".into(), snippet: "tavily says".into(), date: Some("2026-01-02".into()) }]);
        assert_eq!(problems, ["Brave Search: HTTP 500: down"]);
        // an explicit choice goes first
        let c = SearchConfig { provider: Some(Provider::Wikipedia), ..c };
        assert_eq!(c.order()[0], Provider::Wikipedia);
    }

    #[test]
    fn searxng_json() {
        let server = Server::start(|req| {
            assert!(req.target.starts_with("/sx/search?q=hello&format=json"), "{}", req.target);
            Reply::json(&json!({"results": [
                {"title": "Hello", "url": "https://hello.example/", "content": "a\n greeting", "publishedDate": "2025-05-05T00:00:00"},
                {"title": "No url"}
            ]}))
        });
        let c = SearchConfig { searxng_url: Some(server.url("/sx/")), ..cfg(&server) };
        let (p, hits, _) = search(&Http::new(), &c, "hello", 5).unwrap();
        assert_eq!(p, Provider::Searxng);
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].snippet.as_str(), hits[0].date.as_deref()), ("a greeting", Some("2025-05-05")));
    }

    #[test]
    fn nothing_anywhere_is_an_error_naming_each_try() {
        let server = Server::start(|_| Reply::new(200, "text/html", "<html>not json</html>"));
        let e = search(&Http::new(), &cfg(&server), "x", 5).unwrap_err();
        assert!(e.starts_with("No search results. Exa: the answer wasn't a JSON-RPC message; Wikipedia: the answer wasn't JSON"), "{e}");
    }
}
