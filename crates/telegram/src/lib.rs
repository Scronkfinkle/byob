//! byob's Telegram channel: the bundled `telegram` plugin's backend, in-process. The user
//! makes a bot of their own with @BotFather and pastes its token (`BOT_TOKEN`); byob then
//! reads the bot's messages by long polling `getUpdates` on a thread of its own (no webhook,
//! no open port) and sends with `sendMessage`.
//!
//! The engine owns pairing, the allowlist and routing (see the channel messages in
//! `docs/plugins-backend.md`); this backend only moves messages:
//!
//! - **In**: text from private chats (never groups, never bots) becomes `inbound`; a button
//!   press (`callback_query`) becomes `inbound` with `button`; `/start CODE` (the pairing deep
//!   link) or a message that is just a pairing code becomes `pair_claim`. The engine checks
//!   every code and every sender. Once it has said who is paired (`channel_peers`), messages
//!   from anyone else are dropped here too, apart from pairing codes; nobody unknown ever gets
//!   an answer. At most [`MAX_PER_MINUTE`] messages per sender per minute are passed on.
//! - **Out**: `deliver` sends plain text (no Markdown, so nothing needs escaping), cut to
//!   [`MAX_TEXT`], with inline Yes/No buttons when asked; `pair_start` makes the deep link.
//! - **Trouble** (a refused token, another program reading the same bot, rate limits, no
//!   connection) backs off and says so in `channel_status`, in words. The token is never in a
//!   log line, status or error.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

pub mod api;
/// A scripted Telegram Bot API server for tests (also the engine's, with `fake-server`).
#[cfg(any(test, feature = "fake-server"))]
pub mod fake;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use byob_plugins::native::{NativeBackend, NativeFactory, Outbox};
use serde_json::{Value, json};

use crate::api::{Api, ApiError, DEFAULT_API};

/// The id manifests name (`"backend": {"native": "telegram"}`).
pub const ID: &str = "telegram";
/// The channel it declares.
pub const CHANNEL: &str = "telegram";
/// The environment variable that points it at another Bot API server (a fake, in tests).
pub const API_ENV: &str = "BYOB_TELEGRAM_API";
/// Telegram's own limit on a message's text (characters).
pub const MAX_TEXT: usize = 4096;
/// Messages passed on per sender per minute; more are dropped.
pub const MAX_PER_MINUTE: usize = 20;
/// The commands byob reads (byob-agent's `messaging`), for Telegram's "/" menu:
/// (command, description).
pub const COMMANDS: [(&str, &str); 2] = [("new", "Start a new chat"), ("help", "What you can do from here")];
/// Longest button data Telegram takes (bytes).
pub const MAX_BUTTON_ID: usize = 64;
/// Most buttons on one message.
const MAX_BUTTONS: usize = 8;
/// Longest sender name passed on (characters).
const MAX_NAME: usize = 64;
/// Most paired peers kept for the filter.
const MAX_PEERS: usize = 1000;
/// How long one long poll waits.
pub const POLL_SECS: u64 = 25;
/// The setup hint when there's no token.
pub const NO_TOKEN: &str = "Add your bot's token: in Telegram, talk to @BotFather, send /newbot, then paste the token it gives you here.";

/// What the poller and the backend share.
struct Shared {
    stop: AtomicBool,
    /// Paired peers as byob last said (`None`: not said yet; everything is passed on).
    peers: Mutex<Option<BTreeSet<String>>>,
    /// The last status sent (sent again only when it changes).
    status: Mutex<Option<(bool, String)>>,
}

impl Shared {
    fn new() -> Arc<Self> {
        Arc::new(Self { stop: AtomicBool::new(false), peers: Mutex::new(None), status: Mutex::new(None) })
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Sleep `d` in small slices; `false` if stopped meanwhile.
    fn nap(&self, d: Duration) -> bool {
        let end = Instant::now() + d;
        while Instant::now() < end {
            if self.stopped() {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50).min(end.saturating_duration_since(Instant::now())));
        }
        !self.stopped()
    }

    /// The paired peers say this sender may be passed on (`true` until byob has said who).
    fn known(&self, peer: &str) -> Option<bool> {
        self.peers.lock().unwrap_or_else(PoisonError::into_inner).as_ref().map(|p| p.contains(peer))
    }

    /// Send `channel_status` if it changed.
    fn status(&self, api: Option<&Api>, out: &Outbox, ok: bool, text: &str) {
        let text = api.map_or_else(|| text.to_owned(), |a| a.clean(text));
        let mut last = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        if last.as_ref().is_some_and(|(o, t)| *o == ok && *t == text) {
            return;
        }
        *last = Some((ok, text.clone()));
        drop(last);
        out.send(json!({"type": "channel_status", "channel": CHANNEL, "ok": ok, "text": text}));
    }
}

/// The backend: its API base, the client once there's a token, and the poller.
pub struct Telegram {
    base: String,
    poll_secs: u64,
    api: Option<Api>,
    shared: Arc<Shared>,
}

/// Makes a fresh backend (the real Bot API, or [`API_ENV`]'s).
pub fn factory() -> NativeFactory {
    let base = std::env::var(API_ENV).ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| DEFAULT_API.to_owned());
    Arc::new(move || Box::new(Telegram::with_api(&base, POLL_SECS)) as Box<dyn NativeBackend>)
}

/// A setting's value from `init` (`secrets` for secret ones, else `settings`), blank as unset.
fn setting(init: &Value, key: &str) -> Option<String> {
    ["secrets", "settings"].iter().find_map(|m| init.get(m)?.get(key)?.as_str().map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned))
}

/// A string or number field as text.
fn text_of(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.trim().to_owned()).filter(|s| !s.is_empty()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// A pairing code as the user may have typed it (spaces, dashes, lower case), if it is one:
/// 8 characters of A–Z and 2–9.
pub fn as_code(text: &str) -> Option<String> {
    let t = text.trim();
    if t.len() > 24 {
        return None;
    }
    let code: String = t.chars().filter(|c| !matches!(c, ' ' | '-')).flat_map(char::to_uppercase).collect();
    (code.chars().count() == 8 && code.chars().all(|c| c.is_ascii_uppercase() || ('2'..='9').contains(&c))).then_some(code)
}

/// A Telegram chat id byob may send to (a decimal integer).
fn chat_id_ok(s: &str) -> bool {
    let digits = s.strip_prefix('-').unwrap_or(s);
    !digits.is_empty() && digits.len() <= 20 && digits.bytes().all(|b| b.is_ascii_digit())
}

impl Telegram {
    /// A backend talking to the Bot API at `base`; `poll_secs`: each long poll's wait.
    pub fn with_api(base: &str, poll_secs: u64) -> Self {
        Self { base: base.to_owned(), poll_secs, api: None, shared: Shared::new() }
    }

    fn api(&self) -> Result<&Api, String> {
        self.api.as_ref().ok_or_else(|| NO_TOKEN.to_owned())
    }

    /// The bot's @username.
    fn username(&self) -> Result<String, String> {
        let api = self.api()?;
        let me = api.call("getMe", &json!({})).map_err(|e| e.text())?;
        me.get("username").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| "Telegram didn't say the bot's username.".to_owned())
    }

    fn deliver(&self, params: &Value) -> Result<Value, String> {
        let api = self.api()?;
        let to = text_of(params.get("to")).filter(|t| chat_id_ok(t)).ok_or("deliver needs `to`, a Telegram chat id")?;
        let text = params.get("text").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()).ok_or("deliver needs `text`")?;
        let mut body = json!({"chat_id": to, "text": chars(text, MAX_TEXT)});
        let buttons = params.get("buttons").and_then(Value::as_array).cloned().unwrap_or_default();
        if !buttons.is_empty() {
            let mut row = Vec::new();
            for b in buttons.iter().take(MAX_BUTTONS) {
                let id = b.get("id").and_then(Value::as_str).filter(|i| !i.is_empty()).ok_or("a button needs an `id`")?;
                if id.len() > MAX_BUTTON_ID {
                    return Err(format!("a button's id is at most {MAX_BUTTON_ID} bytes"));
                }
                let label = b.get("label").and_then(Value::as_str).map(str::trim).filter(|l| !l.is_empty()).unwrap_or(id);
                row.push(json!({"text": chars(label, 64), "callback_data": id}));
            }
            // Two to a row: room for each label on a phone.
            let rows: Vec<Vec<Value>> = row.chunks(2).map(<[Value]>::to_vec).collect();
            body["reply_markup"] = json!({"inline_keyboard": rows});
        }
        if let Some(r) = text_of(params.get("reply_to")).and_then(|r| r.parse::<i64>().ok()) {
            body["reply_parameters"] = json!({"message_id": r, "allow_sending_without_reply": true});
        }
        let sent = match api.call("sendMessage", &body) {
            Err(ApiError::Refused { status: 429, retry_after, .. }) if retry_after.unwrap_or(5) <= 30 => {
                if !self.shared.nap(Duration::from_secs(retry_after.unwrap_or(5))) {
                    return Err("stopped".into());
                }
                api.call("sendMessage", &body)
            }
            other => other,
        };
        let sent = sent.map_err(|e| api.clean(&e.text()))?;
        let id = text_of(sent.get("message_id")).ok_or("Telegram didn't say which message it sent")?;
        Ok(json!({"ref": id}))
    }

    fn pair_start(&self, params: &Value) -> Result<Value, String> {
        let code = params.get("code").and_then(Value::as_str).unwrap_or_default();
        if code.is_empty() || code.len() > 64 || !code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
            return Err("pair_start needs a `code` of letters and digits".into());
        }
        let bot = self.username()?;
        Ok(json!({
            "link": format!("https://t.me/{bot}?start={code}"),
            "name": format!("@{bot}"),
            "instructions": format!("On your phone, open the link and tap Start in Telegram. Or open Telegram, find @{bot} and send it this code: {code}"),
        }))
    }
}

impl NativeBackend for Telegram {
    fn init(&mut self, init: &Value, out: &Outbox) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared = Shared::new();
        self.api = setting(init, "BOT_TOKEN").map(|t| Api::new(&self.base, &t, self.poll_secs));
        let Some(api) = self.api.clone() else {
            self.shared.status(None, out, false, NO_TOKEN);
            return;
        };
        let (shared, poll_out, secs) = (Arc::clone(&self.shared), out.clone(), self.poll_secs);
        let spawned = std::thread::Builder::new().name("byob-telegram-poll".into()).spawn(move || {
            let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| Poller::new(api, Arc::clone(&shared), poll_out.clone(), secs).run()));
            if let Err(p) = run {
                let why = byob_plugins::native::panic_text(p.as_ref());
                poll_out.fault(&format!("the Telegram poller stopped: {why}"));
            }
        });
        if let Err(e) = spawned {
            out.fault(&format!("couldn't start the Telegram poller: {e}"));
        }
    }

    fn event(&mut self, event: &Value, _out: &Outbox) {
        if event.get("event").and_then(Value::as_str) != Some("channel_peers") || event.get("channel").and_then(Value::as_str).is_some_and(|c| c != CHANNEL) {
            return;
        }
        let peers: BTreeSet<String> =
            event.get("peers").and_then(Value::as_array).into_iter().flatten().filter_map(|p| text_of(Some(p))).take(MAX_PEERS).collect();
        *self.shared.peers.lock().unwrap_or_else(PoisonError::into_inner) = Some(peers);
    }

    fn request(&mut self, method: &str, params: &Value, _out: &Outbox) -> Result<Value, String> {
        match method {
            "deliver" => self.deliver(params),
            "pair_start" => self.pair_start(params),
            "unpair" => {
                if let Some(peer) = text_of(params.get("peer"))
                    && let Some(p) = self.shared.peers.lock().unwrap_or_else(PoisonError::into_inner).as_mut()
                {
                    p.remove(&peer);
                }
                Ok(Value::Null)
            }
            other => Err(format!("the Telegram backend has no {other:?}")),
        }
    }

    fn shutdown(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for Telegram {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

/// The long-polling loop (on its own thread).
struct Poller {
    api: Api,
    shared: Arc<Shared>,
    out: Outbox,
    secs: u64,
    offset: i64,
    bot: Option<String>,
    /// Forwarded per sender, for the per-minute cap.
    recent: BTreeMap<String, VecDeque<Instant>>,
    /// Messages from unknown senders dropped since the last log line, and when that was.
    dropped: u64,
    dropped_logged: Option<Instant>,
}

impl Poller {
    fn new(api: Api, shared: Arc<Shared>, out: Outbox, secs: u64) -> Self {
        Self { api, shared, out, secs, offset: 0, bot: None, recent: BTreeMap::new(), dropped: 0, dropped_logged: None }
    }

    fn status(&self, ok: bool, text: &str) {
        self.shared.status(Some(&self.api), &self.out, ok, text);
    }

    fn log(&self, text: &str) {
        self.out.log(&self.api.clean(text));
    }

    fn run(mut self) {
        let mut network = 0u32;
        let mut conflicts = 0u32;
        let mut webhook_deleted = false;
        let mut backoff = Duration::from_secs(1);
        while !self.shared.stopped() {
            let result = match &self.bot {
                None => self.api.call("getMe", &json!({})).map(|me| {
                    self.bot = me.get("username").and_then(Value::as_str).map(|u| chars(u, MAX_NAME));
                    // Telegram's command menu ("/"); byob reads the commands, so this is a hint.
                    if let Err(e) = self.api.call(
                        "setMyCommands",
                        &json!({"commands": COMMANDS.map(|(command, description)| json!({"command": command, "description": description}))}),
                    ) {
                        self.log(&format!("couldn't set the bot's command menu: {}", e.text()));
                    }
                    Vec::new()
                }),
                Some(_) => self
                    .api
                    .poll(&json!({"offset": self.offset, "timeout": self.secs, "allowed_updates": ["message", "callback_query"]}))
                    .map(|v| v.as_array().cloned().unwrap_or_default()),
            };
            let wait = match result {
                Ok(updates) => {
                    (network, conflicts, backoff) = (0, 0, Duration::from_secs(1));
                    let name = self.bot.clone().unwrap_or_default();
                    self.status(true, &format!("Connected as @{name}"));
                    for u in &updates {
                        self.update(u);
                    }
                    continue;
                }
                Err(e @ ApiError::Refused { status: 401 | 404, .. }) => {
                    self.status(false, &e.text());
                    Duration::from_secs(300)
                }
                Err(ApiError::Refused { status: 409, description, .. }) if description.to_lowercase().contains("webhook") && !webhook_deleted => {
                    webhook_deleted = true;
                    self.log("the bot had a webhook set; removed it so byob can read its messages");
                    if let Err(e) = self.api.call("deleteWebhook", &json!({})) {
                        self.log(&format!("couldn't remove the webhook: {}", e.text()));
                    }
                    continue;
                }
                Err(ApiError::Refused { status: 409, .. }) => {
                    conflicts = conflicts.saturating_add(1);
                    if conflicts >= 2 {
                        self.status(
                            false,
                            "Another program is reading this bot's messages (Telegram allows only one). Stop it, or make byob a bot of its own with @BotFather.",
                        );
                    }
                    Duration::from_secs(5).saturating_mul(1 << conflicts.saturating_sub(1).min(4)).min(Duration::from_secs(60))
                }
                Err(ApiError::Refused { status: 429, retry_after, .. }) => Duration::from_secs(retry_after.unwrap_or(5).clamp(1, 300)),
                Err(e) => {
                    network = network.saturating_add(1);
                    if network >= 3 {
                        self.status(false, &e.text());
                    }
                    let w = backoff;
                    backoff = backoff.saturating_mul(2).min(Duration::from_secs(60));
                    w
                }
            };
            if !self.shared.nap(wait) {
                break;
            }
        }
    }

    /// One update: passed on, or dropped.
    fn update(&mut self, u: &Value) {
        if let Some(id) = u.get("update_id").and_then(Value::as_i64) {
            self.offset = self.offset.max(id.saturating_add(1));
        }
        if let Some(m) = u.get("message") {
            self.message(m);
        } else if let Some(q) = u.get("callback_query") {
            self.button(q);
        }
    }

    /// (peer id, name) of a human sender.
    fn sender(from: Option<&Value>) -> Option<(String, String)> {
        let from = from?;
        if from.get("is_bot").and_then(Value::as_bool).unwrap_or(false) {
            return None;
        }
        let id = from.get("id").and_then(Value::as_i64)?.to_string();
        let name = match from.get("username").and_then(Value::as_str).filter(|u| !u.is_empty()) {
            Some(u) => format!("@{}", chars(u, MAX_NAME)),
            None => chars(from.get("first_name").and_then(Value::as_str).unwrap_or_default().trim(), MAX_NAME),
        };
        Some((id, name))
    }

    fn message(&mut self, m: &Value) {
        if m.get("chat").and_then(|c| c.get("type")).and_then(Value::as_str) != Some("private") {
            return;
        }
        let Some((peer, name)) = Self::sender(m.get("from")) else { return };
        let Some(text) = m.get("text").or_else(|| m.get("caption")).and_then(Value::as_str).map(|t| chars(t, MAX_TEXT)) else { return };
        let known = self.shared.known(&peer);
        let trimmed = text.trim();
        let start = trimmed.strip_prefix("/start").filter(|rest| rest.is_empty() || rest.starts_with(' ')).map(str::trim);
        let code = match start {
            Some(rest) => as_code(rest),
            // A paired person's message that happens to look like a code is just a message.
            None if known != Some(true) => as_code(trimmed),
            None => None,
        };
        if let Some(code) = code {
            if self.allow(&peer) {
                self.out.send(json!({"type": "pair_claim", "channel": CHANNEL, "peer": peer, "peer_name": name, "code": code}));
            }
            return;
        }
        if known == Some(false) || (start == Some("") && known != Some(true)) {
            self.unknown();
            return;
        }
        if !self.allow(&peer) {
            return;
        }
        let mut msg = json!({"type": "inbound", "channel": CHANNEL, "peer": peer, "peer_name": name, "text": text});
        if let Some(r) = m.get("reply_to_message").and_then(|r| r.get("message_id")).and_then(Value::as_i64) {
            msg["reply_to"] = json!(r.to_string());
        }
        self.out.send(msg);
    }

    fn button(&mut self, q: &Value) {
        let Some((peer, name)) = Self::sender(q.get("from")) else { return };
        let message = q.get("message");
        if message.and_then(|m| m.get("chat")).and_then(|c| c.get("type")).and_then(Value::as_str).is_some_and(|t| t != "private") {
            return;
        }
        if self.shared.known(&peer) == Some(false) {
            self.unknown();
            return;
        }
        let Some(data) = q.get("data").and_then(Value::as_str).filter(|d| !d.is_empty() && d.len() <= MAX_BUTTON_ID) else { return };
        // The spinner on the button stops either way.
        if let Some(id) = q.get("id").and_then(Value::as_str)
            && let Err(e) = self.api.call("answerCallbackQuery", &json!({"callback_query_id": id}))
        {
            self.log(&format!("couldn't answer a button press: {}", e.text()));
        }
        if !self.allow(&peer) {
            return;
        }
        let mut msg = json!({"type": "inbound", "channel": CHANNEL, "peer": peer, "peer_name": name, "button": data});
        if let Some(r) = message.and_then(|m| m.get("message_id")).and_then(Value::as_i64) {
            msg["reply_to"] = json!(r.to_string());
        }
        self.out.send(msg);
    }

    /// Room under the per-minute cap for one more from `peer` (counted if so).
    fn allow(&mut self, peer: &str) -> bool {
        let now = Instant::now();
        if self.recent.len() > MAX_PEERS {
            self.recent.clear();
        }
        let q = self.recent.entry(peer.to_owned()).or_default();
        while q.front().is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60)) {
            q.pop_front();
        }
        if q.len() >= MAX_PER_MINUTE {
            self.log(&format!("more than {MAX_PER_MINUTE} messages a minute from one sender: dropped one"));
            return false;
        }
        q.push_back(now);
        true
    }

    /// A message from someone who isn't paired: dropped, counted, logged at most once a minute.
    fn unknown(&mut self) {
        self.dropped = self.dropped.saturating_add(1);
        if self.dropped_logged.is_none_or(|t| t.elapsed() >= Duration::from_secs(60)) {
            self.log(&format!("ignored {} message(s) from Telegram users who aren't paired with byob", self.dropped));
            self.dropped = 0;
            self.dropped_logged = Some(Instant::now());
        }
    }
}
