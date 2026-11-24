//! ntfy (<https://ntfy.sh>) as a channel: the bundled `ntfy` plugin's backend, in-process. A
//! one-way channel (`"two_way": false`): byob publishes each message to a topic the user's
//! phone subscribes to in the ntfy app; nothing comes back, so there's nothing to pair and
//! approvals say "open byob". The topic name is the password on a public server, so it's a
//! secret setting and never in logs or errors.
//!
//! The plugin's manifest (`crates/bundled/plugins/ntfy`) names it with `"backend": {"native":
//! "ntfy"}`; the app registers [`factory`] under [`ID`]. Blocking I/O on the backend's thread.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use byob_plugins::native::{NativeBackend, NativeFactory, Outbox};
use serde_json::{Value, json};

/// The id manifests name.
pub const ID: &str = "ntfy";
/// Its one channel.
pub const CHANNEL: &str = "ntfy";
/// The public server, when the user names none.
pub const DEFAULT_SERVER: &str = "https://ntfy.sh";
/// ntfy's message limit (bytes; longer becomes an attachment, which byob doesn't want).
pub const MAX_BYTES: usize = 4096;
/// Said until a topic is set.
pub const NO_TOPIC: &str = "Pick a topic name in the ntfy app, then type the same name under Settings → Plugins → ntfy.";

/// The backend: where it publishes.
#[derive(Default)]
pub struct Ntfy {
    /// `<server>/<topic>`, once set up.
    url: Option<String>,
    topic: String,
    agent: Option<ureq::Agent>,
}

pub fn factory() -> NativeFactory {
    Arc::new(|| Box::new(Ntfy::default()) as Box<dyn NativeBackend>)
}

fn setting(init: &Value, key: &str) -> Option<String> {
    ["secrets", "settings"].iter().find_map(|m| init.get(m)?.get(key)?.as_str().map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned))
}

/// A topic ntfy takes: `[A-Za-z0-9_-]{1,64}`.
pub fn topic_ok(t: &str) -> bool {
    (1..=64).contains(&t.len()) && t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The server's base URL: https, or http only to this machine.
fn server(raw: Option<&str>) -> Result<String, String> {
    let raw = raw.unwrap_or(DEFAULT_SERVER).trim().trim_end_matches('/');
    let u = url::Url::parse(raw).map_err(|_| format!("{raw:?} isn't a server address like https://ntfy.sh"))?;
    let local = matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    match u.scheme() {
        "https" => Ok(raw.to_owned()),
        "http" if local => Ok(raw.to_owned()),
        _ => Err("The ntfy server must be an https:// address (plain http only to this computer).".into()),
    }
}

/// The first `max` bytes of `s`, at a character boundary.
fn bytes(s: &str, max: usize) -> &str {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

impl Ntfy {
    fn configure(&mut self, init: &Value) -> Result<String, String> {
        let topic = setting(init, "TOPIC").ok_or(NO_TOPIC)?;
        if !topic_ok(&topic) {
            return Err("A topic name is letters, digits, - and _ (at most 64), like byob-7f3k2q9x.".into());
        }
        let server = server(setting(init, "SERVER").as_deref())?;
        self.url = Some(format!("{server}/{topic}"));
        self.topic = topic;
        self.agent = Some(
            ureq::Agent::config_builder()
                .http_status_as_error(false)
                .max_redirects(0)
                .timeout_global(Some(Duration::from_secs(20)))
                .user_agent(concat!("byob/", env!("CARGO_PKG_VERSION"), " (ntfy)"))
                .build()
                .new_agent(),
        );
        let host = url::Url::parse(&server).ok().and_then(|u| u.host_str().map(str::to_owned)).unwrap_or(server);
        Ok(format!("Ready: sends to your topic on {host}"))
    }

    /// Publish `text` (the topic stays out of every error).
    fn deliver(&self, params: &Value) -> Result<Value, String> {
        let (Some(url), Some(agent)) = (&self.url, &self.agent) else { return Err(NO_TOPIC.into()) };
        let text = params.get("text").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()).ok_or("deliver needs `text`")?;
        let res = agent.post(url).header("Markdown", "no").send(bytes(text, MAX_BYTES));
        let mut res = res.map_err(|e| match e {
            ureq::Error::Timeout(_) => "The ntfy server took too long to answer.".to_owned(),
            _ => "Couldn't reach the ntfy server (no internet connection?).".to_owned(),
        })?;
        let status = res.status().as_u16();
        let mut body = Vec::new();
        let _ = res.body_mut().as_reader().take(64 * 1024).read_to_end(&mut body);
        let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        match status {
            200 => Ok(json!({"ref": v.get("id").and_then(Value::as_str).unwrap_or_default()})),
            429 => Err("The ntfy server asks byob to slow down (too many messages).".into()),
            401 | 403 => Err("The ntfy server wants a login for that topic (byob can't log in yet); pick another topic or server.".into()),
            s => Err(format!("The ntfy server said no (HTTP {s}).")),
        }
        .map_err(|e: String| e.replace(&self.topic, "<topic>"))
    }
}

impl NativeBackend for Ntfy {
    fn init(&mut self, init: &Value, out: &Outbox) {
        let (ok, text) = match self.configure(init) {
            Ok(t) => (true, t),
            Err(e) => (false, e),
        };
        out.send(json!({"type": "channel_status", "channel": CHANNEL, "ok": ok, "text": text}));
    }

    fn request(&mut self, method: &str, params: &Value, _out: &Outbox) -> Result<Value, String> {
        match method {
            "deliver" => self.deliver(params),
            "unpair" => Ok(Value::Null),
            other => Err(format!("the ntfy backend has no {other:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::sync::Mutex;

    /// A fake ntfy server: records (path, headers, body), answers `status`.
    fn server(status: u16) -> (String, Arc<Mutex<Vec<(String, String, String)>>>) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in l.incoming() {
                let stream = stream.unwrap();
                let mut r = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                let (mut len, mut headers) = (0, String::new());
                loop {
                    let mut h = String::new();
                    r.read_line(&mut h).unwrap();
                    if h.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                    headers.push_str(&h);
                }
                let mut body = vec![0; len];
                r.read_exact(&mut body).unwrap();
                s.lock().unwrap().push((line.split_whitespace().nth(1).unwrap().to_owned(), headers, String::from_utf8(body).unwrap()));
                let answer = r#"{"id":"abc123","event":"message"}"#;
                let mut w = stream;
                write!(w, "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}", answer.len()).unwrap();
            }
        });
        (base, seen)
    }

    fn outbox() -> (Outbox, Arc<Mutex<Vec<Value>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = Arc::clone(&seen);
        (Outbox::new(move |m| s.lock().unwrap().push(m)), seen)
    }

    #[test]
    fn publishes_plain_text_to_the_topic() {
        let (base, seen) = server(200);
        let (out, said) = outbox();
        let mut n = Ntfy::default();
        n.init(&json!({"secrets": {"TOPIC": "byob-7f3k2q9x"}, "settings": {"SERVER": base}}), &out);
        assert_eq!(said.lock().unwrap()[0]["ok"], true);
        assert!(!said.lock().unwrap()[0].to_string().contains("7f3k2q9x"), "the topic isn't said");
        let r = n.request("deliver", &json!({"channel": "ntfy", "to": "me", "text": "Loom: 20% off ✓"}), &out).unwrap();
        assert_eq!(r, json!({"ref": "abc123"}));
        let (path, headers, body) = seen.lock().unwrap()[0].clone();
        assert_eq!((path.as_str(), body.as_str()), ("/byob-7f3k2q9x", "Loom: 20% off ✓"));
        assert!(headers.to_ascii_lowercase().contains("markdown: no"));
        // Long text is cut at a character, under ntfy's limit.
        n.request("deliver", &json!({"text": "é".repeat(5000)}), &out).unwrap();
        assert!(seen.lock().unwrap()[1].2.len() <= MAX_BYTES);
    }

    #[test]
    fn setup_problems_and_refusals_are_said_without_the_topic() {
        let (out, said) = outbox();
        let mut n = Ntfy::default();
        n.init(&json!({}), &out);
        assert_eq!(said.lock().unwrap()[0]["text"], NO_TOPIC);
        assert!(n.request("deliver", &json!({"text": "x"}), &out).unwrap_err().contains("Pick a topic"));
        n.init(&json!({"secrets": {"TOPIC": "bad topic!"}}), &out);
        assert_eq!(said.lock().unwrap()[1]["ok"], false);
        n.init(&json!({"secrets": {"TOPIC": "t1"}, "settings": {"SERVER": "http://example.com"}}), &out);
        assert!(said.lock().unwrap()[2]["text"].as_str().unwrap().contains("https://"), "no plain http to other machines");
        let (base, _) = server(403);
        n.init(&json!({"secrets": {"TOPIC": "secret-topic"}, "settings": {"SERVER": base}}), &out);
        let e = n.request("deliver", &json!({"text": "x"}), &out).unwrap_err();
        assert!(e.contains("wants a login") && !e.contains("secret-topic"), "{e}");
    }
}
