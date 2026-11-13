//! Blocking HTTP for the web tools (ureq, like the inference client): every request has a
//! deadline, a redirect limit and a cap on the bytes read (after decompression), and every
//! answer is treated as hostile.

use std::io::Read;
use std::time::Duration;

/// What a desktop browser sends: many sites refuse bare script requests.
pub const BROWSER_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";
/// For APIs whose rules ask for an honest one (Wikipedia).
pub const BYOB_UA: &str = concat!("byob/", env!("CARGO_PKG_VERSION"), " (https://github.com/Scronkfinkle/byob)");

pub const MAX_REDIRECTS: u32 = 5;

/// One request's limits.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// The whole request, connecting and reading the body included.
    pub timeout: Duration,
    /// Body bytes kept; the rest is not read.
    pub max_bytes: usize,
}

/// An answer (any status).
#[derive(Debug, Clone, Default)]
pub struct Response {
    pub status: u16,
    /// After redirects.
    pub url: String,
    /// `Content-Type`, lowercased (e.g. `text/html; charset=utf-8`).
    pub content_type: String,
    pub body: Vec<u8>,
    /// The body was longer than `max_bytes`.
    pub truncated: bool,
}

impl Response {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The body as text (UTF-8, or the charset its type names when that is Latin-1 /
    /// Windows-1252; anything else is read as UTF-8, invalid bytes replaced).
    pub fn text(&self) -> String {
        decode(&self.body, &self.content_type)
    }
}

/// `bytes` as text in the charset `content_type` (or a `<meta charset>` in its first 1 KiB)
/// names.
pub fn decode(bytes: &[u8], content_type: &str) -> String {
    let charset = charset_of(content_type).or_else(|| {
        let head = String::from_utf8_lossy(bytes.get(..1024).unwrap_or(bytes)).to_ascii_lowercase();
        let at = head.find("charset=")?;
        let rest = head.get(at + 8..)?.trim_start_matches(['"', '\'']);
        Some(rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect())
    });
    match charset.as_deref() {
        Some("iso-8859-1" | "latin1" | "latin-1" | "windows-1252" | "cp1252" | "us-ascii" | "ascii") if std::str::from_utf8(bytes).is_err() => {
            bytes.iter().map(|&b| crate::html::cp1252(b)).collect()
        }
        _ => String::from_utf8_lossy(bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes)).into_owned(),
    }
}

fn charset_of(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|p| {
        let (k, v) = p.split_once('=')?;
        (k.trim() == "charset").then(|| v.trim().trim_matches('"').to_ascii_lowercase())
    })
}

/// Shared HTTP client.
#[derive(Clone)]
pub struct Http {
    agent: ureq::Agent,
}

impl Default for Http {
    fn default() -> Self {
        Self::new()
    }
}

impl Http {
    pub fn new() -> Self {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(MAX_REDIRECTS)
            .timeout_connect(Some(Duration::from_secs(10)))
            .user_agent(BROWSER_UA)
            .build()
            .new_agent();
        Self { agent }
    }

    /// `GET url` with `headers`.
    pub fn get(&self, url: &str, headers: &[(&str, &str)], limits: Limits) -> Result<Response, String> {
        let mut req = self.agent.get(url).config().timeout_global(Some(limits.timeout)).build();
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        finish(req.call(), url, limits)
    }

    /// `POST url` with a JSON body.
    pub fn post_json(&self, url: &str, headers: &[(&str, &str)], body: &serde_json::Value, limits: Limits) -> Result<Response, String> {
        let mut req = self.agent.post(url).config().timeout_global(Some(limits.timeout)).build();
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let body = serde_json::to_vec(body).map_err(|e| e.to_string())?;
        finish(req.header("Content-Type", "application/json").send(&body[..]), url, limits)
    }
}

fn finish(res: Result<ureq::http::Response<ureq::Body>, ureq::Error>, url: &str, limits: Limits) -> Result<Response, String> {
    use ureq::ResponseExt;
    let mut resp = res.map_err(|e| describe(&e, url))?;
    let status = resp.status().as_u16();
    let final_url = resp.get_uri().to_string();
    let content_type = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or_default().trim().to_ascii_lowercase();
    let cap = limits.max_bytes;
    let mut body = Vec::with_capacity(cap.min(64 * 1024));
    let mut reader = resp.body_mut().as_reader().take(u64::try_from(cap).unwrap_or(u64::MAX).saturating_add(1));
    if let Err(e) = reader.read_to_end(&mut body) {
        // what arrived before a reset or a timeout is still worth showing
        if body.is_empty() {
            return Err(format!("couldn't read {}: {e}", host_of(url)));
        }
    }
    let truncated = body.len() > cap;
    body.truncate(cap);
    Ok(Response { status, url: final_url, content_type, body, truncated })
}

fn host_of(url: &str) -> String {
    url::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_owned)).unwrap_or_else(|| url.chars().take(80).collect())
}

/// A transport error as a sentence.
fn describe(e: &ureq::Error, url: &str) -> String {
    let host = host_of(url);
    match e {
        ureq::Error::Timeout(_) => format!("{host} took too long to answer"),
        ureq::Error::TooManyRedirects => format!("{host} redirected more than {MAX_REDIRECTS} times"),
        ureq::Error::HostNotFound => format!("{host} wasn't found (no such host, or no connection)"),
        ureq::Error::ConnectionFailed => format!("couldn't connect to {host}"),
        ureq::Error::Io(io) => format!("couldn't reach {host}: {io}"),
        other => format!("couldn't fetch from {host}: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_charsets() {
        assert_eq!(decode(b"caf\xe9", "text/html; charset=ISO-8859-1"), "café");
        assert_eq!(decode(b"\x93q\x94", "text/plain; charset=windows-1252"), "“q”");
        assert_eq!(decode(b"<meta charset=\"windows-1252\">caf\xe9", "text/html"), "<meta charset=\"windows-1252\">café");
        assert_eq!(decode("café".as_bytes(), "text/html; charset=iso-8859-1"), "café", "valid UTF-8 wins over a wrong label");
        assert_eq!(decode(b"\xEF\xBB\xBFhi \xff", ""), "hi \u{FFFD}");
    }
}
