//! Just enough HTTP/1.1 to serve static files and accept WebSocket upgrades.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::{Component, Path};

/// Cap on the request head so a slow or hostile client can't make us buffer forever.
const MAX_HEAD: usize = 16 * 1024;
const MAX_HEADERS: usize = 64;

pub struct Request {
    pub method: String,
    pub path: String,
    headers: Vec<(String, String)>,
}

impl Request {
    /// Case-insensitive header lookup.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn is_websocket_upgrade(&self) -> bool {
        self.header("upgrade").is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
    }

    /// Browsers always send `Origin` on WebSocket handshakes; it must name this server, or
    /// any website you visit could drive the harness. Native clients send no `Origin`.
    pub fn origin_allowed(&self) -> bool {
        let Some(origin) = self.header("origin") else { return true };
        let origin_host = origin.split_once("://").map_or(origin, |(_, rest)| rest).trim_end_matches('/');
        [self.header("host"), self.header("x-forwarded-host")].into_iter().flatten().any(|h| h.eq_ignore_ascii_case(origin_host))
    }
}

/// Read and parse a request head. Returns any bytes read past the head.
pub fn read_request(stream: &mut TcpStream) -> io::Result<(Request, Vec<u8>)> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
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

pub fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}

pub fn serve_static(stream: &mut TcpStream, web_dir: Option<&Path>, path: &str) -> io::Result<()> {
    let Some(web_dir) = web_dir else {
        return respond(stream, 404, "text/plain", b"web UI not configured: build it with `cargo xtask web` (or pass --web-dir)");
    };
    let path = path.split(['?', '#']).next().unwrap_or("/");
    let rel = Path::new(path.trim_start_matches('/'));
    // Only plain names: no `..`, no absolute paths, no prefixes.
    if !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        return respond(stream, 404, "text/plain", b"not found");
    }
    let file = if rel.as_os_str().is_empty() { web_dir.join("index.html") } else { web_dir.join(rel) };
    match std::fs::read(&file) {
        Ok(body) => respond(stream, 200, content_type(&file), &body),
        Err(_) => respond(stream, 404, "text/plain", b"not found"),
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
        Request { method: "GET".into(), path: "/ws".into(), headers: headers.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect() }
    }

    #[test]
    fn origin_rules() {
        assert!(req(&[("Host", "box.tail1234.ts.net")]).origin_allowed());
        assert!(req(&[("Host", "box.tail1234.ts.net"), ("Origin", "https://box.tail1234.ts.net")]).origin_allowed());
        assert!(req(&[("Host", "127.0.0.1:7470"), ("X-Forwarded-Host", "box.ts.net"), ("Origin", "https://box.ts.net")]).origin_allowed());
        assert!(!req(&[("Host", "127.0.0.1:7470"), ("Origin", "https://evil.example")]).origin_allowed());
        assert!(!req(&[("Origin", "https://evil.example")]).origin_allowed());
    }
}
