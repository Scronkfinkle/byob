//! A scripted CalDAV server for tests, on a local TcpListener (no live network): a
//! well-known redirect, a principal, a home with two calendars (one read-only) and a task
//! list, calendar-query REPORTs, and GET/PUT/DELETE with ETags and Basic auth. Test code:
//! it panics on what it can't do.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A request the server got.
#[derive(Debug, Clone, Default)]
pub struct Seen {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Seen {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

pub const USER: &str = "u";
pub const PASS: &str = "fake-s3cret-9271";
pub const WORK: &str = "/dav/cal/u/work/";
pub const HOLIDAYS: &str = "/dav/cal/u/holidays/";

#[derive(Default)]
pub struct State {
    /// path -> (etag, ics)
    pub objects: BTreeMap<String, (String, String)>,
    pub seen: Vec<Seen>,
    next: u32,
}

pub struct Server {
    pub base: String,
    pub state: Arc<Mutex<State>>,
}

impl Server {
    /// Serve a CalDAV account until the test process ends.
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State::default()));
        let st = Arc::clone(&state);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let st = Arc::clone(&st);
                std::thread::spawn(move || {
                    let Some(req) = read_request(&mut stream) else { return };
                    let (status, headers, body) = {
                        let mut s = st.lock().unwrap();
                        s.seen.push(req.clone());
                        answer(&mut s, &req)
                    };
                    let mut head = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
                    for (k, v) in &headers {
                        head.push_str(&format!("{k}: {v}\r\n"));
                    }
                    head.push_str("\r\n");
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(body.as_bytes());
                });
            }
        });
        Self { base, state }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// Store an object (in a calendar's path).
    pub fn put(&self, path: &str, ics: &str) {
        let mut s = self.state.lock().unwrap();
        s.next += 1;
        let etag = format!("\"e{}\"", s.next);
        s.objects.insert(path.to_owned(), (etag, ics.to_owned()));
    }

    pub fn object(&self, path: &str) -> Option<String> {
        self.state.lock().unwrap().objects.get(path).map(|(_, t)| t.clone())
    }

    pub fn requests(&self) -> Vec<Seen> {
        self.state.lock().unwrap().seen.clone()
    }
}

fn ms(responses: &str) -> (u16, Vec<(String, String)>, String) {
    (
        207,
        vec![("Content-Type".into(), "application/xml; charset=utf-8".into())],
        format!(
            "<?xml version=\"1.0\"?><d:multistatus xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\" xmlns:a=\"http://apple.com/ns/ical/\">{responses}</d:multistatus>"
        ),
    )
}

fn resp(href: &str, props: &str) -> String {
    format!("<d:response><d:href>{href}</d:href><d:propstat><d:prop>{props}</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>")
}

fn xml_text(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('\r', "&#13;")
}

fn answer(s: &mut State, req: &Seen) -> (u16, Vec<(String, String)>, String) {
    use base64::Engine;
    let want = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{USER}:{PASS}")));
    if req.header("authorization") != Some(want.as_str()) {
        return (401, vec![("WWW-Authenticate".into(), "Basic realm=\"x\"".into())], "no".into());
    }
    let path = req.target.split('?').next().unwrap_or_default().to_owned();
    let writable = |p: &str| p.starts_with(WORK);
    match (req.method.as_str(), path.as_str()) {
        ("PROPFIND", "/.well-known/caldav") => (301, vec![("Location".into(), "/dav/".into())], String::new()),
        ("PROPFIND", "/") => (404, vec![], String::new()),
        ("PROPFIND", "/dav/") => ms(&resp(
            "/dav/",
            "<d:current-user-principal><d:href>/dav/principals/u/</d:href></d:current-user-principal><d:resourcetype><d:collection/></d:resourcetype>",
        )),
        ("PROPFIND", "/dav/principals/u/") => ms(&resp(
            "/dav/principals/u/",
            "<c:calendar-home-set><d:href>/dav/cal/u/</d:href></c:calendar-home-set><c:calendar-user-address-set><d:href>/dav/principals/u/</d:href><d:href>mailto:me@example.com</d:href></c:calendar-user-address-set>",
        )),
        ("PROPFIND", "/dav/cal/u/") => {
            let rw = "<d:current-user-privilege-set><d:privilege><d:read/></d:privilege><d:privilege><d:write/></d:privilege></d:current-user-privilege-set>";
            let ro = "<d:current-user-privilege-set><d:privilege><d:read/></d:privilege></d:current-user-privilege-set>";
            let ev = "<c:supported-calendar-component-set><c:comp name=\"VEVENT\"/></c:supported-calendar-component-set>";
            ms(&[
                resp("/dav/cal/u/", "<d:resourcetype><d:collection/></d:resourcetype>"),
                resp(WORK, &format!("<d:resourcetype><d:collection/><c:calendar/></d:resourcetype><d:displayname>Work</d:displayname><a:calendar-color>#3366FFFF</a:calendar-color>{ev}{rw}")),
                resp(HOLIDAYS, &format!("<d:resourcetype><d:collection/><c:calendar/></d:resourcetype><d:displayname>Holidays &amp; more</d:displayname>{ev}{ro}")),
                resp("/dav/cal/u/tasks/", &format!("<d:resourcetype><d:collection/><c:calendar/></d:resourcetype><d:displayname>Tasks</d:displayname><c:supported-calendar-component-set><c:comp name=\"VTODO\"/></c:supported-calendar-component-set>{rw}")),
            ]
            .concat())
        }
        ("REPORT", cal) if cal == WORK || cal == HOLIDAYS => {
            if !req.body.contains("<c:time-range start=\"") || req.header("depth") != Some("1") {
                return (400, vec![], "want a time-range query, Depth 1".into());
            }
            let body: String = s
                .objects
                .iter()
                .filter(|(p, _)| p.starts_with(cal))
                .map(|(p, (etag, ics))| resp(p, &format!("<d:getetag>{}</d:getetag><c:calendar-data>{}</c:calendar-data>", xml_text(etag), xml_text(ics))))
                .collect();
            ms(&body)
        }
        ("GET", p) => match s.objects.get(p) {
            Some((etag, ics)) => (200, vec![("ETag".into(), etag.clone()), ("Content-Type".into(), "text/calendar".into())], ics.clone()),
            None => (404, vec![], String::new()),
        },
        ("PUT", p) => {
            if !writable(p) {
                return (403, vec![], "<error>read-only calendar</error>".into());
            }
            let current = s.objects.get(p).map(|(e, _)| e.clone());
            match (req.header("if-none-match"), req.header("if-match"), &current) {
                (Some("*"), _, Some(_)) => return (412, vec![], String::new()),
                (_, Some(m), Some(e)) if m != e => return (412, vec![], String::new()),
                (_, Some(_), None) => return (412, vec![], String::new()),
                _ => {}
            }
            s.next += 1;
            let etag = format!("\"e{}\"", s.next);
            s.objects.insert(p.to_owned(), (etag.clone(), req.body.clone()));
            (if current.is_some() { 204 } else { 201 }, vec![("ETag".into(), etag)], String::new())
        }
        ("DELETE", p) => {
            if !writable(p) {
                return (403, vec![], String::new());
            }
            match (s.objects.get(p), req.header("if-match")) {
                (None, _) => (404, vec![], String::new()),
                (Some((e, _)), Some(m)) if e != m => (412, vec![], String::new()),
                _ => {
                    s.objects.remove(p);
                    (204, vec![], String::new())
                }
            }
        }
        _ => (405, vec![], String::new()),
    }
}

fn read_request(stream: &mut std::net::TcpStream) -> Option<Seen> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_owned();
    let target = first.next()?.to_owned();
    let headers: Vec<(String, String)> = lines.filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))).collect();
    let len = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.parse::<usize>().ok()).unwrap_or(0);
    let mut body = buf[head_end..].to_vec();
    while body.len() < len {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Some(Seen { method, target, headers, body: String::from_utf8_lossy(&body).into_owned() })
}
