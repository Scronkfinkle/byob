//! `web_fetch`: a page as readable text, a window of it at a time.
//!
//! HTML becomes Markdown-ish text (`crate::html`), JSON is pretty-printed, other text comes as
//! it is; binary types are refused. Downloads are capped (`MAX_BYTES`), and so is what one
//! call returns (`max_chars`): the rest is a `start=` away, from a small cache, so reading on
//! doesn't fetch again. Pages that come back empty or behind a bot wall (sites that need
//! JavaScript, or block scripts) go through a reader service (Jina Reader) when that is on.

use std::time::{Duration, Instant};

use url::Url;

use crate::html::{collapse, readable, truncate};
use crate::http::{Http, Limits, Response};

/// Bytes downloaded per page (after decompression).
pub const MAX_BYTES: usize = 4 * 1024 * 1024;
/// Characters kept per page (after conversion).
pub const MAX_TEXT: usize = 400_000;
pub const DEFAULT_CHARS: usize = 8_000;
pub const MIN_CHARS: usize = 500;
pub const MAX_CHARS: usize = 40_000;
const CACHE: usize = 8;
const CACHE_TTL: Duration = Duration::from_secs(600);
const LIMITS: Limits = Limits { timeout: Duration::from_secs(20), max_bytes: MAX_BYTES };
/// The reader service is slower (it renders the page).
const READER_LIMITS: Limits = Limits { timeout: Duration::from_secs(30), max_bytes: MAX_BYTES };

/// Phrases of pages that stand between a script and the content.
const BOT_WALL: &[&str] = &[
    "just a moment...",
    "checking your browser",
    "enable javascript",
    "javascript is required",
    "javascript is disabled",
    "attention required",
    "verify you are human",
    "verify you are a human",
    "are you a robot",
    "captcha",
    "access denied",
    "unusual traffic",
];

/// A page as text.
#[derive(Debug, Clone)]
pub struct Page {
    /// What was asked for, and where it ended up.
    pub url: String,
    pub final_url: String,
    pub title: String,
    pub text: String,
    /// The download hit [`MAX_BYTES`] (or the text [`MAX_TEXT`]).
    pub cut: bool,
    /// Read through the reader service.
    pub via_reader: bool,
    /// Links written as `[text](url)`.
    pub links: bool,
    at: Instant,
}

/// The reader service (Jina Reader: `https://r.jina.ai/<url>`, keyless at a low rate).
#[derive(Debug, Clone)]
pub struct Reader {
    pub base: String,
    pub key: Option<String>,
}

impl Default for Reader {
    fn default() -> Self {
        Self { base: "https://r.jina.ai/".into(), key: None }
    }
}

/// How a page read through the reader service is marked in the text the model gets.
pub const VIA_READER: &str = "(Read through r.jina.ai";

#[derive(Default)]
pub struct Fetcher {
    cache: Vec<Page>,
    /// `None`: never send pages to it.
    pub reader: Option<Reader>,
}

/// What the model asked for.
#[derive(Debug, Clone)]
pub struct FetchArgs {
    pub url: String,
    pub start: usize,
    pub max_chars: usize,
    /// Keep link URLs (`[text](url)`); off, only link text (much shorter).
    pub links: bool,
}

impl Fetcher {
    /// One window of the page at `args.url`, as the model reads it.
    pub fn fetch(&mut self, http: &Http, args: &FetchArgs) -> Result<String, String> {
        let url = normalize(&args.url)?;
        self.cache.retain(|p| p.at.elapsed() < CACHE_TTL);
        let page = match self.cache.iter().position(|p| (p.url == url || p.final_url == url) && (p.links == args.links || p.via_reader)) {
            Some(i) => self.cache.remove(i),
            None => self.load(http, &url, args.links)?,
        };
        let out = window(&page, args.start, args.max_chars.clamp(MIN_CHARS, MAX_CHARS));
        self.cache.push(page);
        if self.cache.len() > CACHE {
            self.cache.remove(0);
        }
        Ok(out)
    }

    fn load(&self, http: &Http, url: &str, links: bool) -> Result<Page, String> {
        let direct = http.get(
            url,
            &[("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,text/plain;q=0.8,*/*;q=0.5"), ("Accept-Language", "en-US,en;q=0.9")],
            LIMITS,
        );
        let problem = match direct {
            Ok(r) => match convert(&r, url, links) {
                Ok(page) if !thin(&page.text) => return Ok(page),
                Ok(page) => Some((Some(page), "it came back nearly empty or behind a bot check (it may need JavaScript)".to_owned())),
                Err(Refusal::Final(e)) => return Err(e),
                Err(Refusal::Retry(e)) => Some((None, e)),
            },
            Err(e) => Some((None, e)),
        };
        let Some((thin_page, why)) = problem else { return Err("couldn't read the page".into()) };
        if let Some(reader) = &self.reader {
            match read_via(http, reader, url) {
                Ok(page) if !thin(&page.text) => return Ok(page),
                Ok(_) => {}
                Err(e) => log::debug!("web_fetch: reader for {url}: {e}"),
            }
        }
        match thin_page {
            // a short page is still an answer (a tiny text file, an empty listing)
            Some(page) if !page.text.trim().is_empty() && !walled(&page.text) => Ok(page),
            _ => Err(format!("Couldn't read {url}: {why}. Don't retry it; try another source.")),
        }
    }
}

enum Refusal {
    /// Not worth another way (a PDF reader could still try, see `convert`).
    Final(String),
    /// Another way (the reader service) may still work.
    Retry(String),
}

/// `example.com/x` → `https://example.com/x`; only http(s).
pub fn normalize(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("Give a URL to read (url).".into());
    }
    if raw.len() > 4096 {
        return Err("That URL is too long.".into());
    }
    let with_scheme = if raw.contains("://") { raw.to_owned() } else { format!("https://{raw}") };
    let u = Url::parse(&with_scheme).map_err(|e| format!("{raw:?} isn't a URL ({e})."))?;
    if !matches!(u.scheme(), "http" | "https") {
        return Err(format!("Only http and https pages can be read, not {}:.", u.scheme()));
    }
    if u.host_str().is_none_or(str::is_empty) {
        return Err(format!("{raw:?} has no host."));
    }
    let mut u = u;
    u.set_fragment(None);
    Ok(u.to_string())
}

fn mime(content_type: &str) -> &str {
    content_type.split(';').next().unwrap_or_default().trim()
}

fn convert(r: &Response, url: &str, links: bool) -> Result<Page, Refusal> {
    let host = Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_owned)).unwrap_or_default();
    if !r.ok() {
        let why = match r.status {
            401 | 403 => format!("{host} refused it (HTTP {})", r.status),
            404 | 410 => return Err(Refusal::Final(format!("{url} wasn't found (HTTP {}).", r.status))),
            429 => format!("{host} is rate limiting (HTTP 429)"),
            s => format!("{host} answered HTTP {s}"),
        };
        return Err(Refusal::Retry(why));
    }
    let mime = mime(&r.content_type);
    let sniff_html = || {
        let head = String::from_utf8_lossy(r.body.get(..512).unwrap_or(&r.body)).to_ascii_lowercase();
        let head = head.trim_start();
        head.starts_with("<!doctype html") || head.starts_with("<html") || head.contains("<head")
    };
    let kind = if mime.contains("html") || ((mime.is_empty() || mime == "application/octet-stream") && sniff_html()) {
        Kind::Html
    } else if mime == "application/json" || mime.ends_with("+json") {
        Kind::Json
    } else if mime.starts_with("text/")
        || mime.ends_with("xml")
        || mime.is_empty()
        || mime == "application/javascript"
        || mime == "application/x-yaml"
        || mime == "application/yaml"
        || mime == "application/toml"
    {
        Kind::Text
    } else if mime == "application/pdf" {
        return Err(Refusal::Retry(format!("{url} is a PDF, which byob can't read from the web by itself")));
    } else {
        return Err(Refusal::Final(format!("{url} isn't a text page (it's {mime}).")));
    };
    let raw = r.text();
    let base = Url::parse(&r.url).ok().or_else(|| Url::parse(url).ok());
    let (title, mut text) = match kind {
        Kind::Html => {
            let doc = readable(&raw, base.as_ref().filter(|_| links));
            (doc.title, doc.text)
        }
        Kind::Json => (String::new(), serde_json::from_str::<serde_json::Value>(&raw).ok().and_then(|v| serde_json::to_string_pretty(&v).ok()).unwrap_or(raw)),
        Kind::Text => (String::new(), raw),
    };
    let mut cut = r.truncated;
    if let Some((i, _)) = text.char_indices().nth(MAX_TEXT) {
        text.truncate(i);
        cut = true;
    }
    Ok(Page {
        url: url.to_owned(),
        final_url: if r.url.is_empty() { url.to_owned() } else { r.url.clone() },
        title,
        text,
        cut,
        via_reader: false,
        links,
        at: Instant::now(),
    })
}

enum Kind {
    Html,
    Json,
    Text,
}

fn walled(text: &str) -> bool {
    let head: String = text.chars().take(2000).collect::<String>().to_lowercase();
    BOT_WALL.iter().any(|p| head.contains(p))
}

/// Next to nothing, or a bot wall's text.
fn thin(text: &str) -> bool {
    let n = text.chars().filter(|c| !c.is_whitespace()).count();
    n < 150 || (n < 3000 && walled(text))
}

/// The page through the reader service (it renders JavaScript and reads PDFs).
fn read_via(http: &Http, reader: &Reader, url: &str) -> Result<Page, String> {
    let target = format!("{}{url}", reader.base);
    let auth = reader.key.as_ref().map(|k| format!("Bearer {k}"));
    let mut headers = vec![("Accept", "text/plain"), ("X-Return-Format", "markdown")];
    if let Some(a) = &auth {
        headers.push(("Authorization", a));
    }
    let r = http.get(&target, &headers, READER_LIMITS)?;
    if !r.ok() {
        return Err(format!("HTTP {}", r.status));
    }
    let raw = r.text();
    // "Title: …\nURL Source: …\nMarkdown Content:\n…"
    let (title, text) = match raw.split_once("Markdown Content:") {
        Some((head, body)) => (head.lines().find_map(|l| l.strip_prefix("Title:")).map(collapse).unwrap_or_default(), body.trim().to_owned()),
        None => (String::new(), raw.trim().to_owned()),
    };
    let mut text = text;
    let mut cut = r.truncated;
    if let Some((i, _)) = text.char_indices().nth(MAX_TEXT) {
        text.truncate(i);
        cut = true;
    }
    Ok(Page { url: url.to_owned(), final_url: url.to_owned(), title, text, cut, via_reader: true, links: true, at: Instant::now() })
}

/// `max` characters of the page from `start`, cut at a line break when one is near, with a
/// header and a marker saying how to read on.
fn window(page: &Page, start: usize, max: usize) -> String {
    let total = page.text.chars().count();
    let mut out = format!("URL: {}\n", page.final_url);
    if !page.title.is_empty() {
        out.push_str(&format!("Title: {}\n", truncate(&page.title, 200)));
    }
    if page.via_reader {
        out.push_str(VIA_READER);
        out.push_str(": the page needed JavaScript or refused scripts.)\n");
    }
    if start >= total && total > 0 {
        out.push_str(&format!("\n[Nothing at start={start}: the page has {total} characters.]"));
        return out;
    }
    let begin = byte_at(&page.text, start);
    let rest = &page.text[begin..];
    let mut end = byte_at(rest, max);
    if end < rest.len() {
        // prefer a paragraph or line break in the last fifth
        let floor = byte_at(rest, max * 4 / 5);
        if let Some(p) = rest[floor..end].rfind("\n\n").or_else(|| rest[floor..end].rfind('\n')) {
            end = floor + p;
        }
    }
    let chunk = &rest[..end];
    let shown = chunk.chars().count();
    let next = start + shown;
    if start > 0 || next < total {
        out.push_str(&format!("[Characters {start}–{next} of {total}]\n"));
    }
    out.push('\n');
    out.push_str(chunk.trim_end());
    if next < total {
        out.push_str(&format!("\n\n[Truncated: {} more characters. Call web_fetch with start={next} to read on.]", total - next));
    } else if page.cut {
        out.push_str("\n\n[The page was longer than byob reads; this is as far as it goes.]");
    }
    out
}

/// The byte index of character `n` (the end when there are fewer).
fn byte_at(s: &str, n: usize) -> usize {
    s.char_indices().nth(n).map_or(s.len(), |(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{Reply, Server};

    fn args(url: &str) -> FetchArgs {
        FetchArgs { url: url.into(), start: 0, max_chars: DEFAULT_CHARS, links: false }
    }

    fn article(words: usize) -> String {
        format!(
            "<html><head><title>Doc</title></head><body><nav>Menu</nav><article><h1>Doc</h1><p>{}</p></article></body></html>",
            "lorem ipsum ".repeat(words)
        )
    }

    #[test]
    fn urls_are_checked_and_completed() {
        assert_eq!(normalize(" example.com/a#frag ").unwrap(), "https://example.com/a");
        assert!(normalize("file:///etc/passwd").unwrap_err().contains("not file:"));
        assert!(normalize("javascript:alert(1)").is_err());
        assert!(normalize("").is_err());
        assert!(normalize(&"a".repeat(5000)).is_err());
    }

    #[test]
    fn html_becomes_text_and_long_pages_come_in_windows() {
        let server = Server::start(|_| Reply::html(&article(2000)));
        let mut f = Fetcher::default();
        let http = Http::new();
        let first = f.fetch(&http, &FetchArgs { max_chars: 1000, ..args(&server.url("/doc")) }).unwrap();
        assert!(first.starts_with(&format!("URL: {}/doc\nTitle: Doc\n[Characters 0–", server.base)), "{first}");
        assert!(first.contains("# Doc\n\nlorem ipsum") && !first.contains("Menu"), "{first}");
        let next = first.rsplit("start=").next().unwrap().split(' ').next().unwrap().parse::<usize>().unwrap();
        assert!(next <= 1000 && next > 500, "{next}");
        assert!(first.contains("more characters. Call web_fetch with start="), "{first}");
        let second = f.fetch(&http, &FetchArgs { start: next, max_chars: 1000, ..args(&server.url("/doc")) }).unwrap();
        assert!(second.contains(&format!("[Characters {next}–")), "{second}");
        assert_eq!(server.requests().len(), 1, "reading on comes from the cache");
        let past = f.fetch(&http, &FetchArgs { start: 10_000_000, ..args(&server.url("/doc")) }).unwrap();
        assert!(past.contains("[Nothing at start=10000000"), "{past}");
    }

    #[test]
    fn link_urls_only_when_asked() {
        let body = format!("<main><p>See <a href=\"/next\">the next page</a>. {}</p><img alt=\"logo\" src=\"x.png\"></main>", "filler ".repeat(40));
        let server = Server::start(move |_| Reply::html(&body));
        let mut f = Fetcher::default();
        let http = Http::new();
        let plain = f.fetch(&http, &args(&server.url("/a"))).unwrap();
        assert!(plain.contains("See the next page. filler") && !plain.contains("logo"), "{plain}");
        let linked = f.fetch(&http, &FetchArgs { links: true, ..args(&server.url("/a")) }).unwrap();
        assert!(linked.contains(&format!("See [the next page]({}/next). filler", server.base)), "{linked}");
        assert_eq!(server.requests().len(), 2, "the cached copy had no links");
    }

    #[test]
    fn json_and_plain_text() {
        let server = Server::start(|req| match req.target.as_str() {
            "/j" => Reply::new(200, "application/json", r#"{"a":[1,2],"b":"x"}"#),
            "/t" => Reply::new(200, "text/plain; charset=utf-8", "just text, short"),
            "/bin" => Reply::new(200, "image/png", vec![0x89, b'P', b'N', b'G']),
            _ => Reply::new(404, "text/html", "<h1>nope</h1>"),
        });
        let mut f = Fetcher::default();
        let http = Http::new();
        let j = f.fetch(&http, &args(&server.url("/j"))).unwrap();
        assert!(j.ends_with("{\n  \"a\": [\n    1,\n    2\n  ],\n  \"b\": \"x\"\n}"), "{j}");
        assert!(f.fetch(&http, &args(&server.url("/t"))).unwrap().ends_with("\n\njust text, short"), "a short text file is still an answer");
        assert!(f.fetch(&http, &args(&server.url("/bin"))).unwrap_err().contains("isn't a text page (it's image/png)"));
        assert!(f.fetch(&http, &args(&server.url("/missing"))).unwrap_err().contains("wasn't found (HTTP 404)"));
    }

    #[test]
    fn downloads_are_capped_and_redirects_limited() {
        let server = Server::start(|req| match req.target.as_str() {
            "/big" => Reply::new(200, "text/plain", "x".repeat(MAX_BYTES + 10_000)),
            "/hop" => Reply::redirect("/doc"),
            "/loop" => Reply::redirect("/loop"),
            _ => Reply::html(&article(100)),
        });
        let mut f = Fetcher::default();
        let http = Http::new();
        let big = f.fetch(&http, &FetchArgs { start: MAX_TEXT - 100, ..args(&server.url("/big")) }).unwrap();
        assert!(big.contains(&format!("of {MAX_TEXT}]")) && big.contains("longer than byob reads"), "{}", &big[..200.min(big.len())]);
        let hop = f.fetch(&http, &args(&server.url("/hop"))).unwrap();
        assert!(hop.starts_with(&format!("URL: {}/doc\n", server.base)), "{hop}");
        let e = f.fetch(&http, &args(&server.url("/loop"))).unwrap_err();
        assert!(e.contains("redirected more than 5 times"), "{e}");
    }

    #[test]
    fn slow_servers_time_out() {
        let server = Server::start(|_| {
            let mut r = Reply::html("<p>late</p>");
            r.delay = Duration::from_secs(3);
            r
        });
        let http = Http::new();
        let started = Instant::now();
        let e = http.get(&server.url("/"), &[], Limits { timeout: Duration::from_millis(300), max_bytes: 1000 }).unwrap_err();
        assert!(e.contains("took too long"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn bot_walls_go_through_the_reader_when_it_is_on() {
        let server = Server::start(|req| {
            if let Some(target) = req.target.strip_prefix("/reader/") {
                assert!(target.ends_with("/walled"), "{target}");
                assert_eq!(req.header("x-return-format"), Some("markdown"));
                return Reply::new(200, "text/plain", format!("Title: Real page\nURL Source: x\n\nMarkdown Content:\n{}", "real content ".repeat(50)));
            }
            Reply::new(403, "text/html", "<title>Just a moment...</title><p>Checking your browser</p>")
        });
        let http = Http::new();
        let mut off = Fetcher::default();
        let e = off.fetch(&http, &args(&server.url("/walled"))).unwrap_err();
        assert!(e.contains("refused it (HTTP 403)") && e.contains("Don't retry"), "{e}");
        let mut on = Fetcher { reader: Some(Reader { base: server.url("/reader/"), key: None }), ..Default::default() };
        let page = on.fetch(&http, &args(&server.url("/walled"))).unwrap();
        assert!(page.contains("Title: Real page\n(Read through r.jina.ai") && page.contains("real content real"), "{page}");
    }

    #[test]
    fn windows_never_split_a_character() {
        let page = Page {
            url: "u".into(),
            final_url: "u".into(),
            title: String::new(),
            text: "é".repeat(3000),
            cut: false,
            via_reader: false,
            links: false,
            at: Instant::now(),
        };
        for (start, max) in [(0, 500), (1, 777), (2999, 500), (2500, 1000)] {
            let w = window(&page, start, max);
            assert!(w.contains('é'), "{start} {max}");
        }
    }
}
