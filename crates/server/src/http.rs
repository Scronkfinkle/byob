//! Just enough HTTP/1.1 to serve static files and accept WebSocket upgrades.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

/// Cap on the request head so a slow or hostile client can't make us buffer forever.
const MAX_HEAD: usize = 16 * 1024;
const MAX_HEADERS: usize = 64;

pub struct Request {
    pub method: String,
    pub path: String,
    headers: Vec<(String, String)>,
}

impl Request {
    #[cfg(test)]
    pub fn for_test(headers: &[(&str, &str)]) -> Self {
        Self { method: "GET".into(), path: "/ws".into(), headers: headers.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect() }
    }

    /// Case-insensitive header lookup.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn is_websocket_upgrade(&self) -> bool {
        self.header("upgrade").is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
    }

    /// Browsers always send `Origin` on WebSocket handshakes; it must name this server, or
    /// any website you visit could drive the harness. Native clients send no `Origin`.
    ///
    /// `tailscale serve` passes the browser's `Host` through unchanged and also sets
    /// `X-Forwarded-Host`, so either matches. `X-Forwarded-Host` is only honoured from a
    /// loopback proxy (`trusted_proxy`).
    pub fn origin_allowed(&self, trusted_proxy: bool) -> bool {
        let Some(origin) = self.header("origin") else { return true };
        let origin_host = origin.split_once("://").map_or(origin, |(_, rest)| rest).trim_end_matches('/');
        let forwarded = if trusted_proxy { self.header("x-forwarded-host") } else { None };
        [self.header("host"), forwarded].into_iter().flatten().any(|h| h.eq_ignore_ascii_case(origin_host))
    }

    /// Does the client accept a gzip body? Honours `q=0` ("not acceptable").
    pub fn accepts_gzip(&self) -> bool {
        self.header("accept-encoding").is_some_and(|v| {
            v.split(',').any(|item| {
                let mut parts = item.split(';').map(str::trim);
                let coding = parts.next().unwrap_or_default();
                let q_zero = parts.filter_map(|p| p.strip_prefix("q=")).any(|q| q.parse::<f32>().is_ok_and(|q| q <= 0.0));
                (coding.eq_ignore_ascii_case("gzip") || coding.eq_ignore_ascii_case("x-gzip")) && !q_zero
            })
        })
    }
}

/// Read and parse a request head, which must arrive completely within `timeout` (a
/// per-read timeout alone would let a slowloris client trickle a byte at a time forever).
/// Returns any bytes read past the head.
pub fn read_request(stream: &mut TcpStream, timeout: Duration) -> io::Result<(Request, Vec<u8>)> {
    let deadline = Instant::now().checked_add(timeout).unwrap_or_else(Instant::now);
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "request head not received in time"));
        }
        stream.set_read_timeout(Some(remaining))?;
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "closed before request head"));
        }
        buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
        let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
        let mut req = httparse::Request::new(&mut headers);
        match req.parse(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))? {
            httparse::Status::Complete(len) => {
                let request = Request {
                    method: req.method.unwrap_or_default().to_owned(),
                    path: req.path.unwrap_or("/").to_owned(),
                    headers: req.headers.iter().map(|h| (h.name.to_owned(), String::from_utf8_lossy(h.value).into_owned())).collect(),
                };
                let leftover = buf.get(len..).unwrap_or_default().to_vec();
                return Ok((request, leftover));
            }
            httparse::Status::Partial if buf.len() > MAX_HEAD => {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "request head too large"));
            }
            httparse::Status::Partial => {}
        }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        304 => "Not Modified",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

/// Write a complete response. `extra` lines must be `Name: value` without CRLF.
pub fn respond_with(stream: &mut impl Write, status: u16, content_type: &str, extra: &[String], body: &[u8]) -> io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n", reason(status), body.len());
    for line in extra {
        head.push_str(line);
        head.push_str("\r\n");
    }
    head.push_str("X-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// An uncacheable plain response (errors, refusals).
pub fn respond(stream: &mut impl Write, status: u16, content_type: &str, body: &[u8]) -> io::Result<()> {
    respond_with(stream, status, content_type, &["Cache-Control: no-store".into()], body)
}

/// A small self-explanatory HTML page for refusals a person will see in their browser.
pub fn forbidden_page(stream: &mut impl Write, why: &str) -> io::Result<()> {
    let why = why.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let body = format!(
        "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><title>byob: access denied</title>\
         <body style=\"font-family:system-ui,sans-serif;background:#16171a;color:#ddd;max-width:36em;margin:4em auto;padding:0 1em\">\
         <h1>Access denied</h1><p>{why}</p></body></html>"
    );
    respond(stream, 403, "text/html; charset=utf-8", body.as_bytes())
}

/// A static file chosen for a request: what to read, and how to label it.
#[derive(Debug, PartialEq, Eq)]
pub struct Chosen {
    pub file: PathBuf,
    pub content_type: &'static str,
    pub gzip: bool,
    pub etag: String,
}

/// Map a URL path to a file under `web_dir`, preferring a precompressed `<file>.gz` (made by
/// `cargo xtask web`) when the client accepts gzip and the `.gz` is not older than the file.
pub fn choose_static(web_dir: &Path, path: &str, accepts_gzip: bool) -> Option<Chosen> {
    let path = path.split(['?', '#']).next().unwrap_or("/");
    let rel = Path::new(path.trim_start_matches('/'));
    // Only plain names: no `..`, no absolute paths, no prefixes.
    if !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        return None;
    }
    let file = if rel.as_os_str().is_empty() { web_dir.join("index.html") } else { web_dir.join(rel) };
    let meta = std::fs::metadata(&file).ok().filter(std::fs::Metadata::is_file)?;
    let content_type = content_type(&file);
    if accepts_gzip {
        let mut gz = file.clone().into_os_string();
        gz.push(".gz");
        let gz = PathBuf::from(gz);
        if let Some(gz_meta) = std::fs::metadata(&gz).ok().filter(std::fs::Metadata::is_file)
            && gz_meta.modified().ok() >= meta.modified().ok()
        {
            return Some(Chosen { etag: etag(&gz_meta, "gz"), file: gz, content_type, gzip: true });
        }
    }
    Some(Chosen { etag: etag(&meta, "id"), file, content_type, gzip: false })
}

fn etag(meta: &std::fs::Metadata, variant: &str) -> String {
    let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
    format!("\"{:x}-{mtime:x}-{variant}\"", meta.len())
}

pub fn serve_static(stream: &mut TcpStream, web_dir: Option<&Path>, req: &Request) -> io::Result<()> {
    let Some(web_dir) = web_dir else {
        return serve_embedded(stream, req);
    };
    let Some(chosen) = choose_static(web_dir, &req.path, req.accepts_gzip()) else {
        return respond(stream, 404, "text/plain", b"not found");
    };
    // The bundle's file names are not content-hashed, so let browsers cache but always
    // revalidate (cheap 304s via ETag) — a rebuilt bundle is picked up on next load.
    let mut extra = vec!["Cache-Control: no-cache".to_owned(), format!("ETag: {}", chosen.etag), "Vary: Accept-Encoding".to_owned()];
    if req.header("if-none-match").is_some_and(|v| v.split(',').any(|t| t.trim() == chosen.etag || t.trim() == "*")) {
        return respond_with(stream, 304, chosen.content_type, &extra, b"");
    }
    if chosen.gzip {
        extra.push("Content-Encoding: gzip".to_owned());
    }
    match std::fs::read(&chosen.file) {
        Ok(body) => respond_with(stream, 200, chosen.content_type, &extra, &body),
        Err(_) => respond(stream, 404, "text/plain", b"not found"),
    }
}

/// The browser UI compiled into the binary (feature `embedded-web`).
#[cfg(feature = "embedded-web")]
fn serve_embedded(stream: &mut TcpStream, req: &Request) -> io::Result<()> {
    if !byob_web_assets::available() {
        return respond(stream, 404, "text/plain", b"this build has no web UI: run `cargo xtask web` and rebuild, or pass --web-dir");
    }
    let path = req.path.split(['?', '#']).next().unwrap_or("/");
    let Some(asset) = byob_web_assets::get(path) else {
        return respond(stream, 404, "text/plain", b"not found");
    };
    let content_type = content_type(Path::new(asset.path));
    let mut extra = vec!["Cache-Control: no-cache".to_owned(), format!("ETag: {}", asset.etag), "Vary: Accept-Encoding".to_owned()];
    if req.header("if-none-match").is_some_and(|v| v.split(',').any(|t| t.trim() == asset.etag || t.trim() == "*")) {
        return respond_with(stream, 304, content_type, &extra, b"");
    }
    if !asset.gzip {
        return respond_with(stream, 200, content_type, &extra, asset.bytes);
    }
    if req.accepts_gzip() {
        extra.push("Content-Encoding: gzip".to_owned());
        return respond_with(stream, 200, content_type, &extra, asset.bytes);
    }
    let mut body = Vec::new();
    match io::Read::read_to_end(&mut flate2::read::GzDecoder::new(asset.bytes), &mut body) {
        Ok(_) => respond_with(stream, 200, content_type, &extra, &body),
        Err(_) => respond(stream, 500, "text/plain", b"couldn't decompress the web UI"),
    }
}

#[cfg(not(feature = "embedded-web"))]
fn serve_embedded(stream: &mut TcpStream, _req: &Request) -> io::Result<()> {
    respond(stream, 404, "text/plain", b"this is the slim build of byob, without the browser UI: pass --web-dir, or install the full build")
}

/// Whether this binary carries the browser UI (feature `embedded-web`, built with `cargo xtask web`).
pub fn embedded_web_available() -> bool {
    #[cfg(feature = "embedded-web")]
    {
        byob_web_assets::available()
    }
    #[cfg(not(feature = "embedded-web"))]
    {
        false
    }
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript",
        Some("wasm") => "application/wasm",
        Some("css") => "text/css",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(headers: &[(&str, &str)]) -> Request {
        Request::for_test(headers)
    }

    #[test]
    fn origin_rules() {
        assert!(req(&[("Host", "box.tail1234.ts.net")]).origin_allowed(false));
        assert!(req(&[("Host", "box.tail1234.ts.net"), ("Origin", "https://box.tail1234.ts.net")]).origin_allowed(false));
        // What tailscale serve sends: Host passed through, X-Forwarded-Host set to it.
        let serve = req(&[("Host", "box.tail1234.ts.net"), ("X-Forwarded-Host", "box.tail1234.ts.net"), ("Origin", "https://box.tail1234.ts.net")]);
        assert!(serve.origin_allowed(true));
        let serve_port = req(&[("Host", "box.ts.net:8443"), ("X-Forwarded-Host", "box.ts.net:8443"), ("Origin", "https://box.ts.net:8443")]);
        assert!(serve_port.origin_allowed(true));
        // A proxy that rewrites Host but sets X-Forwarded-Host: honoured only from loopback.
        let rewritten = req(&[("Host", "127.0.0.1:7470"), ("X-Forwarded-Host", "box.ts.net"), ("Origin", "https://box.ts.net")]);
        assert!(rewritten.origin_allowed(true));
        assert!(!rewritten.origin_allowed(false));
        assert!(!req(&[("Host", "127.0.0.1:7470"), ("Origin", "https://evil.example")]).origin_allowed(true));
        assert!(!req(&[("Origin", "https://evil.example")]).origin_allowed(true));
    }

    #[test]
    fn gzip_negotiation() {
        assert!(req(&[("Accept-Encoding", "gzip, deflate, br")]).accepts_gzip());
        assert!(req(&[("accept-encoding", "br;q=1.0, GZIP;q=0.5")]).accepts_gzip());
        assert!(!req(&[("Accept-Encoding", "gzip;q=0")]).accepts_gzip());
        assert!(!req(&[("Accept-Encoding", "gzip; q=0.0, br")]).accepts_gzip());
        assert!(!req(&[("Accept-Encoding", "br, gzipx")]).accepts_gzip());
        assert!(!req(&[("Accept-Encoding", "identity")]).accepts_gzip());
        assert!(!req(&[]).accepts_gzip());
    }

    #[test]
    fn choose_static_prefers_fresh_gz() {
        let dir = std::env::temp_dir().join(format!("byob-static-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("app.wasm"), b"plain").unwrap();
        assert!(!choose_static(&dir, "/app.wasm", true).unwrap().gzip, "no .gz yet");
        std::fs::write(dir.join("app.wasm.gz"), b"gz").unwrap();
        let gz = choose_static(&dir, "/app.wasm?v=1", true).unwrap();
        assert!(gz.gzip);
        assert_eq!(gz.content_type, "application/wasm");
        let plain = choose_static(&dir, "/app.wasm", false).unwrap();
        assert!(!plain.gzip);
        assert_ne!(gz.etag, plain.etag, "variants need distinct ETags");
        // A .gz older than its source is stale and ignored.
        let old = std::time::SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options().write(true).open(dir.join("app.wasm.gz")).unwrap().set_modified(old).unwrap();
        assert!(!choose_static(&dir, "/app.wasm", true).unwrap().gzip);
        assert!(choose_static(&dir, "/../app.wasm", true).is_none());
        assert!(choose_static(&dir, "/missing", true).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
