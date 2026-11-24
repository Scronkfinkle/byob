//! The few Bot API calls byob makes (`getMe`, `getUpdates`, `sendMessage`,
//! `answerCallbackQuery`, `deleteWebhook`), as blocking JSON POSTs over ureq.
//!
//! The token is part of every URL (`/bot<token>/<method>`), so nothing here ever puts a URL or
//! a ureq error's own text into an error: errors are said in words, and [`Api::clean`] takes
//! the token out of anything else that might carry it.

use std::io::Read;
use std::time::Duration;

use serde_json::Value;

/// The real Bot API.
pub const DEFAULT_API: &str = "https://api.telegram.org";
/// Largest answer read (a getUpdates batch of 100 long messages fits easily).
const MAX_BODY: u64 = 4 * 1024 * 1024;
/// Longest text of Telegram's own (an error description) passed on.
const MAX_DESCRIPTION: usize = 200;

/// Why a call didn't work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// Telegram answered with an error (`error_code`, `description`, `parameters.retry_after`).
    Refused { status: u16, description: String, retry_after: Option<u64> },
    /// No answer (no connection, a timeout, a broken answer).
    Network(String),
}

impl ApiError {
    /// In words (never with the token or a URL).
    pub fn text(&self) -> String {
        match self {
            Self::Refused { status: 401 | 404, .. } => {
                "Telegram refused the bot token; check it under Settings → My phone (BotFather's /token shows it again).".into()
            }
            Self::Refused { status: 429, retry_after, .. } => {
                format!("Telegram asks byob to slow down (try again in {} seconds).", retry_after.unwrap_or(5))
            }
            Self::Refused { status, description, .. } if description.is_empty() => format!("Telegram said no (HTTP {status})."),
            Self::Refused { status, description, .. } => format!("Telegram said no (HTTP {status}): {description}"),
            Self::Network(why) => why.clone(),
        }
    }
}

/// A bot's API client.
#[derive(Clone)]
pub struct Api {
    base: String,
    token: String,
    /// For quick calls.
    agent: ureq::Agent,
    /// For long polls (their timeout plus a margin).
    poll_agent: ureq::Agent,
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(timeout))
        .timeout_connect(Some(Duration::from_secs(10)))
        .user_agent(concat!("byob/", env!("CARGO_PKG_VERSION"), " (Telegram)"))
        .build()
        .new_agent()
}

fn cut(s: &str, max: usize) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max { one_line } else { format!("{}…", one_line.chars().take(max).collect::<String>()) }
}

impl Api {
    /// `base` without the trailing slash; `poll_secs`: how long one getUpdates may wait.
    pub fn new(base: &str, token: &str, poll_secs: u64) -> Self {
        Self {
            base: base.trim().trim_end_matches('/').to_owned(),
            token: token.trim().to_owned(),
            agent: agent(Duration::from_secs(20)),
            poll_agent: agent(Duration::from_secs(poll_secs.saturating_add(10))),
        }
    }

    /// `text` with the token taken out (the last line of defense before anything is shown).
    pub fn clean(&self, text: &str) -> String {
        if self.token.is_empty() { text.to_owned() } else { text.replace(&self.token, "<token>") }
    }

    /// One call: its `result`, or why not.
    pub fn call(&self, method: &str, body: &Value) -> Result<Value, ApiError> {
        self.call_with(&self.agent, method, body)
    }

    /// A long poll (the slower agent).
    pub fn poll(&self, body: &Value) -> Result<Value, ApiError> {
        self.call_with(&self.poll_agent, "getUpdates", body)
    }

    fn call_with(&self, agent: &ureq::Agent, method: &str, body: &Value) -> Result<Value, ApiError> {
        let url = format!("{}/bot{}/{method}", self.base, self.token);
        let mut res = agent.post(&url).send_json(body).map_err(|e| ApiError::Network(describe(&e)))?;
        let status = res.status().as_u16();
        let mut bytes = Vec::new();
        res.body_mut().as_reader().take(MAX_BODY).read_to_end(&mut bytes).map_err(|_| ApiError::Network("Telegram's answer broke off.".into()))?;
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if status == 200 && v.get("ok").and_then(Value::as_bool) == Some(true) {
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
        }
        let code = v.get("error_code").and_then(Value::as_u64).and_then(|c| u16::try_from(c).ok()).unwrap_or(status);
        let description = self.clean(&cut(v.get("description").and_then(Value::as_str).unwrap_or_default(), MAX_DESCRIPTION));
        let retry_after = v.get("parameters").and_then(|p| p.get("retry_after")).and_then(Value::as_u64);
        Err(ApiError::Refused { status: if code == 0 { status } else { code }, description, retry_after })
    }
}

/// A ureq error in words, without its URL.
fn describe(e: &ureq::Error) -> String {
    match e {
        ureq::Error::Timeout(_) => "Telegram took too long to answer.".into(),
        ureq::Error::HostNotFound => "Telegram wasn't found (no internet connection?).".into(),
        ureq::Error::ConnectionFailed => "Couldn't connect to Telegram (no internet connection?).".into(),
        ureq::Error::Io(io) => format!("Couldn't reach Telegram ({:?}).", io.kind()),
        _ => "Couldn't talk to Telegram.".into(),
    }
}
