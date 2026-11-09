use std::time::Duration;

/// Everything that can go wrong talking to an MCP server.
///
/// Messages are meant to be shown to the user (and the model) as-is.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum McpError {
    /// The server program could not be started.
    #[error("couldn't start `{command}`: {message}")]
    Spawn { command: String, message: String },
    /// The configured URL is not a usable `http(s)` URL.
    #[error("invalid MCP server URL `{0}`")]
    BadUrl(String),
    /// The server could not be reached (DNS, connect, TLS, broken stream, ...).
    #[error("couldn't reach {host}: {message}")]
    Transport { host: String, message: String },
    /// The server answered with an HTTP error status.
    #[error("{host} answered with an error ({status}){}", fmt_detail(message))]
    Http { host: String, status: u16, message: String },
    /// The server wants the user to sign in (HTTP 401, or 403 `insufficient_scope`).
    /// `www_authenticate` is the raw `WWW-Authenticate` header, for a future OAuth flow.
    #[error("sign in to use this connector")]
    AuthRequired { www_authenticate: String },
    /// The server forgot our session (HTTP 404 with a session id): connect again.
    #[error("the connector session ended; connect again")]
    SessionExpired,
    /// No answer within the deadline. The server was sent `notifications/cancelled`.
    #[error("the connector took too long to answer ({}s)", .0.as_secs_f32())]
    Timeout(Duration),
    /// The caller cancelled. The server was sent `notifications/cancelled`.
    #[error("stopped by the user")]
    Cancelled,
    /// The connection is gone (server exited, stream closed, or `close()` was called).
    #[error("the connector stopped: {0}")]
    Closed(String),
    /// The server answered with a JSON-RPC error.
    #[error("the connector returned an error ({code}): {message}")]
    Rpc { code: i64, message: String },
    /// The server's answer did not make sense.
    #[error("the connector sent an invalid answer: {0}")]
    Protocol(String),
}

fn fmt_detail(message: &str) -> String {
    if message.is_empty() { String::new() } else { format!(": {message}") }
}

impl McpError {
    /// True if a fresh connection might succeed where this one failed
    /// (the server exited, the session expired, or the stream broke).
    pub fn is_disconnect(&self) -> bool {
        matches!(self, Self::Closed(_) | Self::SessionExpired | Self::Transport { .. })
    }
}
