//! Streamable HTTP transport (MCP 2025-03-26+): every message is a POST; the answer comes
//! back as `application/json` or as a `text/event-stream` that ends with it.
//!
//! TODO: the older HTTP+SSE transport (GET an event stream that names a POST endpoint)
//! is not supported yet; hob falls back to it on 400/404/405 from `initialize`.

use std::fmt;
use std::io::{BufReader, Read};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use serde_json::Value;

use crate::lines::SseReader;
use crate::rpc::{Router, answer_id};
use crate::text::truncate;
use crate::{DEFAULT_INIT_TIMEOUT, DEFAULT_REQUEST_TIMEOUT, McpError, PROTOCOL_VERSION};

/// Largest JSON body or SSE event we accept.
pub const MAX_BODY: u64 = 16 * 1024 * 1024;
/// Most bytes of an error body quoted in errors.
const MAX_ERROR_BODY: u64 = 64 * 1024;
/// Deadline for notifications and replies to server requests.
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(10);
/// Extra time on top of a request's deadline before the HTTP call itself gives up.
const HTTP_SLACK: Duration = Duration::from_secs(5);

/// Supplies (and refreshes) a bearer token, e.g. from an OAuth sign-in.
///
/// TODO(oauth): byob has no OAuth flow yet; implement this on top of it. On HTTP 401 the
/// transport calls `refresh()` once and retries if it returns true; otherwise the call
/// fails with [`McpError::AuthRequired`] carrying the `WWW-Authenticate` header.
pub trait TokenProvider: Send + Sync {
    fn token(&self) -> Option<String>;
    fn refresh(&self) -> bool {
        false
    }
}

/// How to reach a remote MCP server.
#[derive(Clone)]
pub struct HttpConfig {
    pub url: String,
    /// Extra headers sent with every request (e.g. API keys).
    pub headers: Vec<(String, String)>,
    /// Sent as `Authorization: Bearer ...` (overrides any configured Authorization header).
    pub bearer_token: Option<String>,
    /// Takes precedence over `bearer_token` when it yields a token.
    pub auth: Option<Arc<dyn TokenProvider>>,
    pub init_timeout: Duration,
    pub request_timeout: Duration,
}

impl HttpConfig {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            headers: Vec::new(),
            bearer_token: None,
            auth: None,
            init_timeout: DEFAULT_INIT_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        }
    }

    pub fn bearer(mut self, token: impl Into<String>) -> Self {
        self.bearer_token = Some(token.into());
        self
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

impl fmt::Debug for HttpConfig {
    // Secrets stay out of logs: header values and tokens are not printed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpConfig")
            .field("url", &self.url)
            .field("headers", &self.headers.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>())
            .field("bearer_token", &self.bearer_token.as_ref().map(|_| "<redacted>"))
            .field("auth", &self.auth.as_ref().map(|_| "<provider>"))
            .field("init_timeout", &self.init_timeout)
            .field("request_timeout", &self.request_timeout)
            .finish()
    }
}

struct Shared {
    url: String,
    host: String,
    cfg: HttpConfig,
    agent: ureq::Agent,
    router: Arc<Router>,
    session: Mutex<Option<String>>,
    /// Negotiated protocol version; sent as `MCP-Protocol-Version` after `initialize`.
    version: Mutex<Option<String>>,
    closed: AtomicBool,
}

pub(crate) struct HttpTransport {
    shared: Arc<Shared>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl HttpTransport {
    pub fn new(cfg: &HttpConfig, router: Arc<Router>) -> Result<Self, McpError> {
        let url = url::Url::parse(cfg.url.trim()).map_err(|_| McpError::BadUrl(cfg.url.clone()))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(McpError::BadUrl(cfg.url.clone()));
        }
        let host = url.host_str().map_or_else(|| cfg.url.clone(), |h| url.port().map_or_else(|| h.to_owned(), |p| format!("{h}:{p}")));
        let agent = ureq::Agent::config_builder().http_status_as_error(false).timeout_connect(Some(Duration::from_secs(10))).build().new_agent();
        let shared = Shared {
            url: url.to_string(),
            host,
            cfg: cfg.clone(),
            agent,
            router,
            session: Mutex::new(None),
            version: Mutex::new(None),
            closed: AtomicBool::new(false),
        };
        Ok(Self { shared: Arc::new(shared) })
    }

    /// Send a request on its own thread; the answer reaches the router.
    pub fn send_request(&self, id: u64, msg: Value, timeout: Duration) -> Result<(), McpError> {
        let s = Arc::clone(&self.shared);
        let spawned = thread::Builder::new().name("mcp-http".into()).spawn(move || {
            let outcome = catch_unwind(AssertUnwindSafe(|| s.post(&msg, Some(id), timeout + HTTP_SLACK)));
            match outcome {
                Ok(Ok(true)) => {}
                Ok(Ok(false)) => s.router.resolve(id, Err(McpError::Protocol("the connector closed before answering".into()))),
                Ok(Err(e)) => s.router.resolve(id, Err(e)),
                Err(_) => s.router.resolve(id, Err(McpError::Protocol("the HTTP worker crashed".into()))),
            }
        });
        spawned.map(|_| ()).map_err(|e| McpError::Transport { host: self.shared.host.clone(), message: e.to_string() })
    }

    /// Send a notification and wait (briefly) for the server to accept it.
    pub fn notify(&self, msg: &Value) {
        if let Err(e) = self.shared.post(msg, None, NOTIFY_TIMEOUT) {
            log::debug!("mcp: notification failed: {e}");
        }
    }

    pub fn set_protocol_version(&self, v: &str) {
        *lock(&self.shared.version) = Some(v.to_owned());
    }

    /// End the session (`DELETE`, best effort, in the background).
    pub fn close(&self) {
        if self.shared.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let Some(session) = lock(&self.shared.session).take() else { return };
        let s = Arc::clone(&self.shared);
        let _ = thread::Builder::new().name("mcp-http-close".into()).spawn(move || {
            let _ = catch_unwind(AssertUnwindSafe(|| {
                let mut req = s.agent.delete(&s.url).config().timeout_global(Some(Duration::from_secs(5))).build().header("Mcp-Session-Id", &session);
                if let Some(v) = lock(&s.version).clone() {
                    req = req.header("MCP-Protocol-Version", &v);
                }
                for (k, v) in s.auth_headers() {
                    req = req.header(&k, &v);
                }
                let _ = req.call();
            }));
        });
    }
}

impl Drop for HttpTransport {
    fn drop(&mut self) {
        self.close();
    }
}

impl Shared {
    fn transport_err(&self, e: impl fmt::Display) -> McpError {
        McpError::Transport { host: self.host.clone(), message: e.to_string() }
    }

    /// Configured headers plus Authorization, without duplicates.
    fn auth_headers(&self) -> Vec<(String, String)> {
        let token = self.cfg.auth.as_ref().and_then(|a| a.token()).or_else(|| self.cfg.bearer_token.clone());
        let mut h: Vec<(String, String)> =
            self.cfg.headers.iter().filter(|(k, _)| token.is_none() || !k.eq_ignore_ascii_case("authorization")).cloned().collect();
        if let Some(t) = token {
            h.push(("Authorization".into(), format!("Bearer {t}")));
        }
        h
    }

    /// POST one message. For a request (`id` set), returns whether its answer arrived;
    /// every message found in the reply is routed.
    fn post(&self, msg: &Value, id: Option<u64>, timeout: Duration) -> Result<bool, McpError> {
        let body = serde_json::to_vec(msg).map_err(|e| McpError::Protocol(e.to_string()))?;
        let initialize = msg.get("method").and_then(Value::as_str) == Some("initialize");
        let mut retried = false;
        let resp = loop {
            let had_session = lock(&self.session).clone();
            let mut req = self
                .agent
                .post(&self.url)
                .config()
                .timeout_global(Some(timeout))
                .build()
                .header("Content-Type", "application/json")
                .header("Accept", "application/json, text/event-stream");
            for (k, v) in self.auth_headers() {
                req = req.header(&k, &v);
            }
            if let Some(sid) = &had_session {
                req = req.header("Mcp-Session-Id", sid);
            }
            if !initialize {
                let v = lock(&self.version).clone().unwrap_or_else(|| PROTOCOL_VERSION.to_owned());
                req = req.header("MCP-Protocol-Version", &v);
            }
            let resp = req.send(&body[..]).map_err(|e| self.transport_err(e))?;
            let status = resp.status().as_u16();
            let www = resp.headers().get("www-authenticate").and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
            if status == 401 || (status == 403 && www.contains("insufficient_scope")) {
                if !retried && self.cfg.auth.as_ref().is_some_and(|a| a.refresh()) {
                    retried = true;
                    continue;
                }
                return Err(McpError::AuthRequired { www_authenticate: truncate(&www, 4096) });
            }
            if status == 404 && had_session.is_some() {
                lock(&self.session).take();
                // This caller learns why; everyone else waiting sees the connection close.
                // Gone first, then woken: the caller may check is_alive() right away.
                self.router.shut_answering("the connector session ended", id.map(|id| (id, Err(McpError::SessionExpired))));
                return Err(McpError::SessionExpired);
            }
            if !(200..300).contains(&status) {
                let mut text = String::new();
                let _ = resp.into_body().into_with_config().limit(MAX_ERROR_BODY).reader().take(MAX_ERROR_BODY).read_to_string(&mut text);
                return Err(McpError::Http { host: self.host.clone(), status, message: truncate(text.trim(), 300) });
            }
            break resp;
        };
        if let Some(sid) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            let sid = sid.trim();
            if !sid.is_empty() && sid.len() <= 1024 {
                *lock(&self.session) = Some(sid.to_owned());
            }
        }
        if resp.status().as_u16() == 202 || id.is_none() {
            return Ok(id.is_none());
        }
        let is_sse = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|t| t.to_ascii_lowercase().contains("text/event-stream"));
        let reader = resp.into_body().into_with_config().limit(u64::MAX).reader();
        if is_sse { self.read_sse(reader, id) } else { self.read_json(reader, id) }
    }

    fn read_json(&self, reader: impl Read, id: Option<u64>) -> Result<bool, McpError> {
        let mut bytes = Vec::new();
        reader.take(MAX_BODY + 1).read_to_end(&mut bytes).map_err(|e| self.transport_err(e))?;
        if bytes.len() as u64 > MAX_BODY {
            return Err(McpError::Protocol(format!("answer larger than {} MiB", MAX_BODY / (1024 * 1024))));
        }
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(false);
        }
        let msg: Value = serde_json::from_slice(&bytes).map_err(|e| McpError::Protocol(format!("not JSON: {e}")))?;
        let answered = match &msg {
            Value::Array(items) => items.iter().any(|m| answer_id(m).is_some() && answer_id(m) == id),
            m => answer_id(m).is_some() && answer_id(m) == id,
        };
        self.route(msg);
        Ok(answered)
    }

    fn read_sse(&self, reader: impl Read, id: Option<u64>) -> Result<bool, McpError> {
        let mut events = SseReader::new(BufReader::new(reader), MAX_BODY as usize);
        loop {
            let event = match events.next_event() {
                Ok(Some(e)) => e,
                Ok(None) => return Ok(false),
                Err(e) => return Err(self.transport_err(e)),
            };
            if event.event != "message" || event.data.trim().is_empty() {
                continue;
            }
            let Ok(msg) = serde_json::from_str::<Value>(&event.data) else {
                log::debug!("mcp: ignoring non-JSON SSE event");
                continue;
            };
            let answered = answer_id(&msg).is_some() && answer_id(&msg) == id;
            self.route(msg);
            // The answer is in: stop reading even if the server keeps the stream open.
            if answered {
                return Ok(true);
            }
            if self.closed.load(Ordering::SeqCst) {
                return Ok(false);
            }
        }
    }

    /// Hand messages to the router and POST back its replies to server requests.
    fn route(&self, msg: Value) {
        for reply in self.router.deliver(msg) {
            if let Err(e) = self.post(&reply, None, NOTIFY_TIMEOUT) {
                log::debug!("mcp: replying to a server request failed: {e}");
            }
        }
    }
}
