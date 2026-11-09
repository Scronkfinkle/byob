//! Blocking MCP (Model Context Protocol, <https://modelcontextprotocol.io>) client.
//!
//! Speaks JSON-RPC 2.0 to a server over stdio (a local program) or Streamable HTTP
//! (a URL), and does what an agent needs: the `initialize` handshake, `tools/list` and
//! `tools/call`. No async runtime: transports use plain threads and channels, and every
//! call blocks the caller's thread until it is answered, times out, or is cancelled.
//!
//! [`McpClient`] is `Send + Sync + Clone`, so turn threads can share one connection.
//! Everything the server sends is untrusted: sizes are capped, garbage is skipped, and
//! server→client requests (sampling, roots, ...) are declined.
//!
//! ```no_run
//! use std::time::Duration;
//! use yah_mcp::{McpClient, StdioConfig};
//!
//! let client = McpClient::connect_stdio(&StdioConfig::new("npx").args(["-y", "@modelcontextprotocol/server-everything"]))?;
//! for tool in client.list_tools()? {
//!     println!("{}: {}", tool.name, tool.description);
//! }
//! let out = client.call_tool("echo", serde_json::json!({ "message": "hi" }), Duration::from_secs(60))?;
//! println!("{} (error: {})", out.text, out.is_error);
//! # Ok::<(), yah_mcp::McpError>(())
//! ```
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

mod error;
mod http;
mod lines;
mod rpc;
mod stdio;
mod text;
mod types;

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

pub use error::McpError;
use http::HttpTransport;
pub use http::{HttpConfig, MAX_BODY, TokenProvider};
use rpc::Router;
use stdio::StdioTransport;
pub use stdio::{MAX_LINE, StdioConfig};
pub use types::{CallResult, Content, MAX_RESULT_TEXT, McpTool, ServerInfo, ToolAnnotations};

/// The MCP revision we speak.
pub const PROTOCOL_VERSION: &str = "2025-06-18";
/// Default deadline for `initialize` (first `npx -y ...` runs download the server).
pub const DEFAULT_INIT_TIMEOUT: Duration = Duration::from_secs(90);
/// Default deadline for `tools/list` pages.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Most `tools/list` pages we follow.
const MAX_PAGES: usize = 20;
/// Most tools we accept from one server.
const MAX_TOOLS: usize = 1000;
/// How often a waiting call checks its cancel flag.
const POLL: Duration = Duration::from_millis(50);

enum Transport {
    Stdio(StdioTransport),
    Http(HttpTransport),
}

impl Transport {
    fn send_request(&self, id: u64, msg: Value, timeout: Duration) -> Result<(), McpError> {
        match self {
            Self::Stdio(t) => t.send(&msg),
            Self::Http(t) => t.send_request(id, msg, timeout),
        }
    }

    fn notify(&self, msg: &Value) {
        match self {
            Self::Stdio(t) => {
                let _ = t.send(msg);
            }
            Self::Http(t) => t.notify(msg),
        }
    }

    fn close(&self) {
        match self {
            Self::Stdio(t) => t.close(),
            Self::Http(t) => t.close(),
        }
    }
}

struct Inner {
    transport: Transport,
    router: Arc<Router>,
    next_id: AtomicU64,
    server: OnceLock<ServerInfo>,
    request_timeout: Duration,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.router.shut("the connector was turned off");
        self.transport.close();
    }
}

/// A connection to one MCP server. Cheap to clone; the connection closes when the last
/// clone is dropped or [`close`](Self::close) is called. There is no automatic
/// reconnect: on [`McpError::is_disconnect`] errors, connect again.
#[derive(Clone)]
pub struct McpClient {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for McpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClient").field("server", &self.inner.server.get()).finish_non_exhaustive()
    }
}

impl McpClient {
    /// Start a local server and complete the handshake.
    pub fn connect_stdio(cfg: &StdioConfig) -> Result<Self, McpError> {
        let router = Arc::new(Router::default());
        let t = StdioTransport::spawn(cfg, Arc::clone(&router))?;
        Self::start(Transport::Stdio(t), router, cfg.init_timeout, cfg.request_timeout)
    }

    /// Connect to a Streamable HTTP server and complete the handshake.
    pub fn connect_http(cfg: &HttpConfig) -> Result<Self, McpError> {
        let router = Arc::new(Router::default());
        let t = HttpTransport::new(cfg, Arc::clone(&router))?;
        Self::start(Transport::Http(t), router, cfg.init_timeout, cfg.request_timeout)
    }

    fn start(transport: Transport, router: Arc<Router>, init_timeout: Duration, request_timeout: Duration) -> Result<Self, McpError> {
        let client = Self { inner: Arc::new(Inner { transport, router, next_id: AtomicU64::new(1), server: OnceLock::new(), request_timeout }) };
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "yah", "version": env!("CARGO_PKG_VERSION") },
        });
        // Dropping `client` on error stops the server.
        let result = client.request("initialize", params, init_timeout, None)?;
        if !result.is_object() {
            return Err(McpError::Protocol("initialize result is not an object".into()));
        }
        let info = ServerInfo::from_result(&result);
        if let Transport::Http(t) = &client.inner.transport
            && !info.protocol_version.is_empty()
        {
            t.set_protocol_version(&info.protocol_version);
        }
        log::info!("mcp: connected to {} {} (protocol {})", info.name, info.version, info.protocol_version);
        let _ = client.inner.server.set(info);
        client.inner.transport.notify(&rpc::notification("notifications/initialized", None));
        Ok(client)
    }

    /// What the server said about itself during `initialize`.
    pub fn server_info(&self) -> ServerInfo {
        self.inner.server.get().cloned().unwrap_or_default()
    }

    /// False once the server exited, the session ended, or `close()` was called.
    pub fn is_alive(&self) -> bool {
        self.inner.router.gone().is_none()
    }

    /// The end of a stdio server's stderr (empty for HTTP), for diagnostics.
    pub fn stderr_tail(&self) -> String {
        match &self.inner.transport {
            Transport::Stdio(t) => t.stderr_tail(),
            Transport::Http(_) => String::new(),
        }
    }

    /// All tools the server offers, following `nextCursor` pagination.
    pub fn list_tools(&self) -> Result<Vec<McpTool>, McpError> {
        self.list_tools_cancellable(&AtomicBool::new(false))
    }

    pub fn list_tools_cancellable(&self, cancel: &AtomicBool) -> Result<Vec<McpTool>, McpError> {
        let mut tools = Vec::new();
        let mut names = HashSet::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let page = self.request("tools/list", params, self.inner.request_timeout, Some(cancel))?;
            let Some(list) = page.get("tools").and_then(Value::as_array) else {
                return Err(McpError::Protocol("tools/list result has no tools array".into()));
            };
            for tool in list.iter().filter_map(McpTool::from_value) {
                if tools.len() >= MAX_TOOLS {
                    log::warn!("mcp: server offers over {MAX_TOOLS} tools; ignoring the rest");
                    return Ok(tools);
                }
                if names.insert(tool.name.clone()) {
                    tools.push(tool);
                }
            }
            let next = page.get("nextCursor").and_then(Value::as_str).filter(|c| !c.is_empty() && c.len() <= 4096).map(str::to_owned);
            if next.is_none() || next == cursor {
                return Ok(tools);
            }
            cursor = next;
        }
        log::warn!("mcp: tools/list still paginating after {MAX_PAGES} pages; stopping");
        Ok(tools)
    }

    /// Run a tool. `args` should be a JSON object (anything else is sent as `{}`).
    /// A tool that ran but failed returns `Ok` with `is_error: true`; `Err` means the
    /// call itself failed (timeout, disconnect, JSON-RPC error, ...).
    pub fn call_tool(&self, name: &str, args: Value, timeout: Duration) -> Result<CallResult, McpError> {
        self.call_tool_cancellable(name, args, timeout, &AtomicBool::new(false))
    }

    /// [`call_tool`](Self::call_tool) that gives up (and tells the server) once `cancel`
    /// is set. The flag is checked every 50 ms.
    pub fn call_tool_cancellable(&self, name: &str, args: Value, timeout: Duration, cancel: &AtomicBool) -> Result<CallResult, McpError> {
        let params = json!({ "name": name, "arguments": types::arguments(args) });
        let result = self.request("tools/call", params, timeout, Some(cancel))?;
        Ok(CallResult::from_value(&result))
    }

    /// Stop the server (stdio) or end the session (HTTP). Pending and later calls fail
    /// with [`McpError::Closed`]. Also happens when the last clone is dropped.
    pub fn close(&self) {
        self.inner.router.shut("the connector was turned off");
        self.inner.transport.close();
    }

    /// Send a request and block for its answer. `cancel: None` means not cancellable
    /// (`initialize` must not be cancelled per the spec).
    fn request(&self, method: &str, params: Value, timeout: Duration, cancel: Option<&AtomicBool>) -> Result<Value, McpError> {
        let inner = &self.inner;
        let id = inner.next_id.fetch_add(1, Ordering::Relaxed);
        let rx = inner.router.register(id)?;
        if let Err(e) = inner.transport.send_request(id, rpc::request(id, method, params), timeout) {
            inner.router.forget(id);
            return Err(e);
        }
        let deadline = Instant::now() + timeout;
        loop {
            if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                return Err(self.abandon(id, cancel.is_some(), "Stopped by the user", McpError::Cancelled));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(self.abandon(id, cancel.is_some(), "Timed out", McpError::Timeout(timeout)));
            }
            match rx.recv_timeout(left.min(POLL)) {
                Ok(reply) => return reply,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(McpError::Closed(inner.router.gone().unwrap_or_else(|| "the connection was lost".into())));
                }
            }
        }
    }

    /// Stop waiting for `id`; tell the server if it is still working on it.
    fn abandon(&self, id: u64, notify: bool, reason: &str, err: McpError) -> McpError {
        if self.inner.router.forget(id) && notify && self.is_alive() {
            let msg = rpc::notification("notifications/cancelled", Some(json!({ "requestId": id, "reason": reason })));
            self.inner.transport.notify(&msg);
        }
        err
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_is_shareable_across_threads() {
        fn assert_send_sync<T: Send + Sync + Clone>() {}
        assert_send_sync::<McpClient>();
    }

    #[test]
    fn spawn_failure_is_an_error() {
        let err = McpClient::connect_stdio(&StdioConfig::new("/nonexistent/yah-mcp-server")).unwrap_err();
        assert!(matches!(err, McpError::Spawn { .. }), "{err:?}");
    }

    #[test]
    fn bad_urls_are_rejected() {
        for url in ["not a url", "ftp://example.com/mcp", ""] {
            assert!(matches!(McpClient::connect_http(&HttpConfig::new(url)), Err(McpError::BadUrl(_))), "{url}");
        }
    }
}
