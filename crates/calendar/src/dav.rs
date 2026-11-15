//! A blocking CalDAV client (RFC 4791 over WebDAV, RFC 4918): discovery (well-known URL,
//! current-user-principal, calendar-home-set, the home's calendars), calendar-query REPORTs
//! with a time range, and GET/PUT/DELETE of calendar objects with ETags. Basic auth over
//! HTTPS; plain HTTP only to this machine. Every answer is capped and treated as hostile.

use std::io::Read;
use std::time::Duration;

use base64::Engine;
use url::Url;

use crate::xml::{self, APPLE, CALDAV, DAV, El};

/// Most bytes of one answer read (a month of a busy calendar fits easily).
pub const MAX_BODY: usize = 8 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;
const TIMEOUT: Duration = Duration::from_secs(30);

/// One calendar collection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Calendar {
    /// Short id for the model: the collection's last path segment.
    pub id: String,
    /// The collection's URL.
    pub url: Url,
    pub name: String,
    pub color: Option<String>,
    pub read_only: bool,
}

/// An answer.
#[derive(Debug, Clone, Default)]
pub struct Reply {
    pub status: u16,
    pub etag: Option<String>,
    pub body: String,
}

impl Reply {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// The account, discovered.
#[derive(Debug, Clone, Default)]
pub struct Account {
    pub calendars: Vec<Calendar>,
    /// The user's address (calendar-user-address-set), for ORGANIZER.
    pub email: Option<String>,
}

pub struct Client {
    agent: ureq::Agent,
    base: Url,
    auth: Option<String>,
}

/// `url` is allowed: https anywhere, http only to this machine.
pub fn check_url(url: &Url) -> Result<(), String> {
    match url.scheme() {
        "https" => Ok(()),
        "http" if is_loopback(url) => Ok(()),
        "http" => Err(format!(
            "{} uses plain http, which would send your password unencrypted: use https (plain http is only allowed to localhost).",
            url.host_str().unwrap_or("the server")
        )),
        other => Err(format!("{other}: isn't a CalDAV address; use https://")),
    }
}

fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme() && a.host_str() == b.host_str() && a.port_or_known_default() == b.port_or_known_default()
}

impl Client {
    pub fn new(base: &str, user: &str, password: &str) -> Result<Self, String> {
        let raw = base.trim();
        let with_scheme = if raw.contains("://") { raw.to_owned() } else { format!("https://{raw}") };
        let base = Url::parse(&with_scheme).map_err(|e| format!("The calendar server address {raw:?} isn't a URL ({e})."))?;
        check_url(&base)?;
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .allow_non_standard_methods(true)
            .timeout_global(Some(TIMEOUT))
            .timeout_connect(Some(Duration::from_secs(10)))
            .user_agent(concat!("byob/", env!("CARGO_PKG_VERSION"), " (CalDAV)"))
            .build()
            .new_agent();
        let auth = (!user.is_empty() || !password.is_empty())
            .then(|| format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"))));
        Ok(Self { agent, base, auth })
    }

    pub fn base(&self) -> &Url {
        &self.base
    }

    /// One request, following redirects by hand (same method; auth only to the first origin;
    /// never down to plain http elsewhere).
    pub fn send(&self, method: &str, url: &Url, headers: &[(&str, &str)], body: Option<&str>) -> Result<Reply, String> {
        let mut url = url.clone();
        for _ in 0..=MAX_REDIRECTS {
            check_url(&url)?;
            let m = ureq::http::Method::from_bytes(method.as_bytes()).map_err(|e| e.to_string())?;
            let mut req = ureq::http::Request::builder().method(m).uri(url.as_str());
            if let Some(a) = self.auth.as_ref().filter(|_| same_origin(&url, &self.base)) {
                req = req.header("Authorization", a.as_str());
            }
            for (k, v) in headers {
                req = req.header(*k, *v);
            }
            let res = match body {
                Some(b) => self.agent.run(req.body(b.as_bytes().to_vec()).map_err(|e| e.to_string())?),
                None => self.agent.run(req.body(()).map_err(|e| e.to_string())?),
            };
            let mut res = res.map_err(|e| describe(&e, &url))?;
            let status = res.status().as_u16();
            let header = |n: &str| res.headers().get(n).and_then(|v| v.to_str().ok()).map(str::to_owned);
            if matches!(status, 301 | 302 | 303 | 307 | 308)
                && let Some(loc) = header("location")
            {
                url = url.join(&loc).map_err(|e| format!("the server redirected to a bad address ({e})"))?;
                continue;
            }
            let etag = header("etag");
            let mut bytes = Vec::new();
            let mut reader = res.body_mut().as_reader().take(u64::try_from(MAX_BODY).unwrap_or(u64::MAX).saturating_add(1));
            reader.read_to_end(&mut bytes).map_err(|e| format!("couldn't read the answer from {}: {e}", host(&url)))?;
            if bytes.len() > MAX_BODY {
                return Err(format!("{} sent more than {} MB; ask for a shorter time range.", host(&url), MAX_BODY / (1024 * 1024)));
            }
            return Ok(Reply { status, etag, body: String::from_utf8_lossy(&bytes).into_owned() });
        }
        Err(format!("{} redirected more than {MAX_REDIRECTS} times", host(&url)))
    }

    fn propfind(&self, url: &Url, depth: &str, props: &str) -> Result<Vec<Response>, String> {
        let body = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<d:propfind xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\" xmlns:a=\"http://apple.com/ns/ical/\"><d:prop>{props}</d:prop></d:propfind>"
        );
        let r = self.send("PROPFIND", url, &[("Depth", depth), ("Content-Type", "application/xml; charset=utf-8")], Some(&body))?;
        if r.status == 401 || r.status == 403 {
            return Err(format!("{} refused the user name or password (HTTP {}). For iCloud and Fastmail use an app-specific password.", host(url), r.status));
        }
        if r.status != 207 {
            return Err(format!("{} answered HTTP {} to PROPFIND {}", host(url), r.status, url.path()));
        }
        multistatus(&r.body, url)
    }

    /// Find the account's calendars from the base URL.
    pub fn discover(&self) -> Result<Account, String> {
        const START: &str = "<d:current-user-principal/><d:resourcetype/><c:calendar-home-set/><c:calendar-user-address-set/>";
        let mut tried: Vec<String> = Vec::new();
        let mut candidates = vec![self.base.clone()];
        if let Ok(w) = self.base.join("/.well-known/caldav") {
            candidates.push(w);
        }
        let mut home: Option<Url> = None;
        let mut email = None;
        for start in candidates {
            let rs = match self.propfind(&start, "0", START) {
                Ok(rs) => rs,
                Err(e) => {
                    tried.push(e);
                    continue;
                }
            };
            let Some(first) = rs.first() else { continue };
            if first.is_calendar() {
                // the address is a calendar itself
                let cal = self.calendars_at(&start, "0")?;
                return Ok(Account { calendars: cal, email: None });
            }
            if let Some(h) = first.href_prop(CALDAV, "calendar-home-set") {
                email = first.address();
                home = Some(h);
                break;
            }
            if let Some(p) = first.href_prop(DAV, "current-user-principal") {
                let rs = self.propfind(&p, "0", "<c:calendar-home-set/><c:calendar-user-address-set/>")?;
                let r = rs.first();
                email = r.and_then(Response::address);
                home = r.and_then(|r| r.href_prop(CALDAV, "calendar-home-set"));
                if home.is_none() {
                    // some servers keep calendars right under the principal
                    home = Some(p);
                }
                break;
            }
            // no principal: maybe this is the home itself
            home = Some(start);
            break;
        }
        let Some(home) = home else {
            return Err(format!("Couldn't find a CalDAV account at {}: {}", self.base, tried.join("; ")));
        };
        let calendars = self.calendars_at(&home, "1")?;
        if calendars.is_empty() {
            return Err(format!("No calendars at {home} (is this the CalDAV address of the account?)."));
        }
        Ok(Account { calendars, email })
    }

    fn calendars_at(&self, url: &Url, depth: &str) -> Result<Vec<Calendar>, String> {
        const PROPS: &str = "<d:resourcetype/><d:displayname/><a:calendar-color/><c:supported-calendar-component-set/><d:current-user-privilege-set/>";
        let rs = self.propfind(url, depth, PROPS)?;
        let mut out: Vec<Calendar> = Vec::new();
        for r in rs.iter().filter(|r| r.is_calendar()) {
            // task lists (VTODO only) aren't calendars of events
            let comps: Vec<&str> = r
                .prop(CALDAV, "supported-calendar-component-set")
                .map(|c| c.children_named(CALDAV, "comp").filter_map(|c| c.attr("name")).collect())
                .unwrap_or_default();
            if !comps.is_empty() && !comps.iter().any(|c| c.eq_ignore_ascii_case("VEVENT")) {
                continue;
            }
            let id = last_segment(&r.href);
            let name = r.prop(DAV, "displayname").map(|d| crate::event::clip(d.trimmed(), 100)).filter(|n| !n.is_empty()).unwrap_or_else(|| id.clone());
            let color = r.prop(APPLE, "calendar-color").map(|c| c.trimmed().chars().take(7).collect::<String>()).filter(|c| c.starts_with('#'));
            let read_only = r.prop(DAV, "current-user-privilege-set").is_some_and(|p| {
                let privs: Vec<&str> = p.children_named(DAV, "privilege").flat_map(|x| x.children.iter().map(|c| c.name.as_str())).collect();
                !privs.iter().any(|n| matches!(*n, "all" | "write" | "write-content" | "bind"))
            });
            if id.is_empty() || out.iter().any(|c| c.id == id) {
                continue;
            }
            out.push(Calendar { id, url: r.href.clone(), name, color, read_only });
        }
        Ok(out)
    }

    /// The calendar objects of `cal` with an event in [start, end) (UTC `YYYYMMDDTHHMMSSZ`):
    /// (url, etag, iCalendar text).
    pub fn query(&self, cal: &Calendar, start: &str, end: &str) -> Result<Vec<(Url, Option<String>, String)>, String> {
        let body = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<c:calendar-query xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\"><d:prop><d:getetag/><c:calendar-data/></d:prop><c:filter><c:comp-filter name=\"VCALENDAR\"><c:comp-filter name=\"VEVENT\"><c:time-range start=\"{}\" end=\"{}\"/></c:comp-filter></c:comp-filter></c:filter></c:calendar-query>",
            xml::escape(start),
            xml::escape(end)
        );
        let r = self.send("REPORT", &cal.url, &[("Depth", "1"), ("Content-Type", "application/xml; charset=utf-8")], Some(&body))?;
        if r.status != 207 {
            return Err(format!("{} answered HTTP {} when asked for the events of {}", host(&cal.url), r.status, cal.name));
        }
        let rs = multistatus(&r.body, &cal.url)?;
        Ok(rs
            .into_iter()
            .filter_map(|r| {
                let data = r.prop(CALDAV, "calendar-data")?.text.clone();
                let etag = r.prop(DAV, "getetag").map(|e| e.trimmed().to_owned());
                Some((r.href, etag, data))
            })
            .collect())
    }

    /// One object: (etag, text).
    pub fn get(&self, url: &Url) -> Result<(Option<String>, String), String> {
        let r = self.send("GET", url, &[("Accept", "text/calendar")], None)?;
        match r.status {
            200 => Ok((r.etag, r.body)),
            404 | 410 => Err("There's no such event (it may have been deleted); list the events again for current ids.".into()),
            s => Err(format!("{} answered HTTP {s} when asked for the event", host(url))),
        }
    }

    /// Write an object: `etag` `Some` replaces that version (If-Match), `None` creates it
    /// only if nothing is there (If-None-Match: *). Returns the new etag if the server says.
    pub fn put(&self, url: &Url, ics: &str, etag: Option<&str>) -> Result<Option<String>, String> {
        let cond = match etag {
            Some(e) => ("If-Match", e),
            None => ("If-None-Match", "*"),
        };
        let r = self.send("PUT", url, &[("Content-Type", "text/calendar; charset=utf-8"), cond], Some(ics))?;
        match r.status {
            200..=299 => Ok(r.etag),
            412 => Err("The event changed on the server meanwhile (or the id is taken). Read it again, then retry.".into()),
            403 => Err(format!("{} refused to save it (HTTP 403): the calendar may be read-only. {}", host(url), server_says(&r.body))),
            s => Err(format!("{} refused to save it (HTTP {s}). {}", host(url), server_says(&r.body))),
        }
    }

    pub fn delete(&self, url: &Url, etag: Option<&str>) -> Result<(), String> {
        let headers: Vec<(&str, &str)> = etag.map(|e| ("If-Match", e)).into_iter().collect();
        let r = self.send("DELETE", url, &headers, None)?;
        match r.status {
            200..=299 => Ok(()),
            404 | 410 => Err("There's no such event (it may have been deleted already).".into()),
            412 => Err("The event changed on the server meanwhile. Read it again, then retry.".into()),
            s => Err(format!("{} refused to delete it (HTTP {s}). {}", host(url), server_says(&r.body))),
        }
    }
}

/// The start of an error body, for the model (servers explain refusals there).
fn server_says(body: &str) -> String {
    let flat = crate::event::clip(&body.replace(['<', '>'], " "), 200);
    if flat.is_empty() { String::new() } else { format!("The server said: {flat}") }
}

fn host(url: &Url) -> String {
    url.host_str().unwrap_or("the calendar server").to_owned()
}

fn describe(e: &ureq::Error, url: &Url) -> String {
    let host = host(url);
    match e {
        ureq::Error::Timeout(_) => format!("{host} took too long to answer"),
        ureq::Error::HostNotFound => format!("{host} wasn't found (no such host, or no connection)"),
        ureq::Error::ConnectionFailed => format!("couldn't connect to {host}"),
        ureq::Error::Io(io) => format!("couldn't reach {host}: {io}"),
        other => format!("couldn't talk to {host}: {other}"),
    }
}

/// The last path segment of a collection or object URL (`work` of `/cal/u/work/`).
pub fn last_segment(url: &Url) -> String {
    url.path_segments().and_then(|mut s| s.rfind(|p| !p.is_empty()).map(str::to_owned)).unwrap_or_default()
}

/// One `<d:response>` of a multistatus: its href (absolute) and its 2xx properties.
#[derive(Debug, Clone)]
pub struct Response {
    pub href: Url,
    pub props: Vec<El>,
}

impl Response {
    pub fn prop(&self, ns: &str, name: &str) -> Option<&El> {
        self.props.iter().find(|p| p.is(ns, name))
    }

    fn is_calendar(&self) -> bool {
        self.prop(DAV, "resourcetype").is_some_and(|t| t.child(CALDAV, "calendar").is_some())
    }

    /// A property holding a `<d:href>` (principal, home set), resolved against the response.
    fn href_prop(&self, ns: &str, name: &str) -> Option<Url> {
        let h = self.prop(ns, name)?.child(DAV, "href")?;
        let u = self.href.join(h.trimmed()).ok()?;
        check_url(&u).ok().map(|()| u)
    }

    /// The first mailto: in calendar-user-address-set.
    fn address(&self) -> Option<String> {
        self.prop(CALDAV, "calendar-user-address-set")?.children_named(DAV, "href").find_map(|h| {
            let t = h.trimmed();
            let t = t.get(..7).filter(|p| p.eq_ignore_ascii_case("mailto:")).and_then(|_| t.get(7..))?;
            (t.contains('@') && t.len() < 255).then(|| t.to_owned())
        })
    }
}

/// Parse a 207 answer. hrefs are resolved against `base`; responses whose href leaves its
/// origin are dropped (a hostile server can't point byob at another host with our password).
pub fn multistatus(body: &str, base: &Url) -> Result<Vec<Response>, String> {
    let root = xml::parse(body)?;
    if !root.is(DAV, "multistatus") {
        return Err("the server's answer isn't a WebDAV multistatus".into());
    }
    let mut out = Vec::new();
    for r in root.children_named(DAV, "response").take(10_000) {
        let Some(href) = r.child(DAV, "href").and_then(|h| base.join(h.trimmed()).ok()) else { continue };
        if !same_origin(&href, base) {
            continue;
        }
        let mut props = Vec::new();
        for ps in r.children_named(DAV, "propstat") {
            let ok = ps.child(DAV, "status").is_none_or(|s| s.trimmed().split_whitespace().nth(1).is_some_and(|c| c.starts_with('2')));
            if let (true, Some(p)) = (ok, ps.child(DAV, "prop")) {
                props.extend(p.children.iter().cloned());
            }
        }
        out.push(Response { href, props });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_http_only_to_this_machine() {
        let ok = |s: &str| check_url(&Url::parse(s).unwrap()).is_ok();
        assert!(ok("https://caldav.fastmail.com/dav/"));
        assert!(ok("http://localhost:5232/") && ok("http://127.0.0.1:5232/") && ok("http://[::1]:5232/"));
        assert!(!ok("http://example.com/dav/") && !ok("ftp://localhost/"));
        assert!(Client::new("http://cal.example.com", "u", "p").err().unwrap().contains("plain http"));
        assert_eq!(Client::new("cal.example.com/dav", "u", "p").unwrap().base().as_str(), "https://cal.example.com/dav");
    }

    #[test]
    fn multistatus_keeps_ok_props_on_the_same_origin() {
        let base = Url::parse("https://h.example/cal/").unwrap();
        let body = r#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/cal/work/</d:href><d:propstat><d:prop><d:displayname>Work</d:displayname></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat><d:propstat><d:prop><d:getctag/></d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat></d:response><d:response><d:href>https://evil.example/x/</d:href></d:response></d:multistatus>"#;
        let rs = multistatus(body, &base).unwrap();
        assert_eq!(rs.len(), 1);
        assert_eq!(rs[0].href.as_str(), "https://h.example/cal/work/");
        assert_eq!(rs[0].props.len(), 1);
        assert_eq!(last_segment(&rs[0].href), "work");
        assert!(multistatus("<html/>", &base).is_err());
    }
}
