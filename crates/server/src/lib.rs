//! HTTP + WebSocket front door for remote clients (e.g. a browser over Tailscale).
//!
//! - `GET /ws` upgrades to a WebSocket carrying [`yah_protocol`] JSON frames.
//! - `GET /artifact/<chat>/<id>/<version>` serves an artifact version under its own strict
//!   CSP (see [`artifact`]).
//! - Any other `GET` serves the static web bundle (`apps/yah-web`, built by `cargo xtask web`).
//!
//! Deliberately simple and blocking: one thread per connection. Binds to loopback by
//! default; expose it with `tailscale serve`, which terminates TLS and limits access to
//! your tailnet. Never bind it to a public interface.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

pub mod artifact;
mod auth;
mod http;

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use tungstenite::protocol::{Role, WebSocketConfig};
use tungstenite::{Message, WebSocket};
use yah_agent::EngineHandle;
use yah_protocol::{ClientMsg, Connection, MAX_FRAME_BYTES, NoticeLevel, ServerMsg};

/// How often a connection thread checks for outgoing engine messages.
const POLL: Duration = Duration::from_millis(20);

#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Defaults to `127.0.0.1:7470`.
    pub addr: SocketAddr,
    /// Directory containing the built web bundle (`index.html`, `*.wasm`, `*.js`).
    pub web_dir: Option<PathBuf>,
    /// Tailscale logins (`Tailscale-User-Login`) allowed in. Empty: no per-user check.
    pub allowed_users: Vec<String>,
    /// Concurrent connections (page loads + WebSockets); extra ones get a 503.
    pub max_connections: usize,
    /// The whole request head must arrive within this (slowloris guard).
    pub head_timeout: Duration,
    /// Blocked writes to a client that stopped reading give up after this.
    pub write_timeout: Duration,
    /// Send a WebSocket ping when the client has been quiet this long.
    pub ping_interval: Duration,
    /// Drop a WebSocket that has sent nothing (not even a pong) for this long.
    pub idle_timeout: Duration,
    /// Serve only `/artifact/…` (the desktop app's on-demand server for "Open in browser").
    pub artifacts_only: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            addr: SocketAddr::from(([127, 0, 0, 1], 7470)),
            web_dir: None,
            allowed_users: Vec::new(),
            max_connections: 64,
            head_timeout: Duration::from_secs(10),
            write_timeout: Duration::from_secs(10),
            ping_interval: Duration::from_secs(20),
            idle_timeout: Duration::from_secs(60),
            artifacts_only: false,
        }
    }
}

/// Counts live connections; the slot is released when dropped (even on panic).
struct Slot(Arc<AtomicUsize>);

impl Slot {
    fn take(count: &Arc<AtomicUsize>, max: usize) -> Option<Self> {
        count.try_update(Ordering::AcqRel, Ordering::Acquire, |n| (n < max).then(|| n.saturating_add(1))).ok().map(|_| Self(Arc::clone(count)))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Bind and start accepting connections on a background thread. Returns the bound address.
pub fn spawn(engine: EngineHandle, config: ServerConfig) -> io::Result<SocketAddr> {
    let listener = TcpListener::bind(config.addr)?;
    let addr = listener.local_addr()?;
    if !config.allowed_users.is_empty() {
        log::info!("only admitting Tailscale users: {}", config.allowed_users.join(", "));
    }
    let config = Arc::new(config);
    let live = Arc::new(AtomicUsize::new(0));
    thread::Builder::new().name("yah-server".into()).spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let Some(slot) = Slot::take(&live, config.max_connections) else {
                refuse_busy(stream);
                continue;
            };
            let (engine, config) = (engine.clone(), Arc::clone(&config));
            let spawned = thread::Builder::new().name("yah-conn".into()).spawn(move || {
                let _slot = slot;
                let result = catch_unwind(AssertUnwindSafe(|| handle(stream, &engine, &config)));
                match result {
                    Ok(Err(e)) => log::debug!("connection ended: {e}"),
                    Err(_) => log::error!("connection thread panicked"),
                    Ok(Ok(())) => {}
                }
            });
            if let Err(e) = spawned {
                log::error!("could not spawn connection thread: {e}");
            }
        }
    })?;
    log::info!("yah server listening on http://{addr}");
    Ok(addr)
}

/// Over the connection cap: answer 503 without blocking the accept loop.
fn refuse_busy(mut stream: TcpStream) {
    log::warn!("connection limit reached; refusing {:?}", stream.peer_addr().ok());
    if stream.set_nonblocking(true).is_err() {
        return;
    }
    // Closing with unread input makes the kernel send RST, which can destroy our response
    // before the client reads it; drain what has arrived (never blocking) before and after.
    let drain = |s: &mut TcpStream| {
        let mut buf = [0u8; 4096];
        for _ in 0..4 {
            if !matches!(s.read(&mut buf), Ok(n) if n > 0) {
                break;
            }
        }
    };
    drain(&mut stream);
    let _ = http::respond(&mut stream, 503, "text/plain", b"too many connections");
    let _ = stream.shutdown(std::net::Shutdown::Write);
    drain(&mut stream);
}

fn handle(mut stream: TcpStream, engine: &EngineHandle, config: &ServerConfig) -> io::Result<()> {
    let peer = stream.peer_addr()?;
    stream.set_write_timeout(Some(config.write_timeout))?;
    let (req, leftover) = http::read_request(&mut stream, config.head_timeout)?;
    if req.method != "GET" {
        return http::respond(&mut stream, 405, "text/plain", b"method not allowed");
    }
    let who = match auth::authorize(peer.ip(), &req, &config.allowed_users) {
        auth::Access::Allow(who) => who,
        auth::Access::Deny(why) => {
            log::warn!("refused {} {} from {peer}: {why}", req.method, req.path);
            return if req.is_websocket_upgrade() {
                http::respond(&mut stream, 403, "text/plain", why.as_bytes())
            } else {
                http::forbidden_page(&mut stream, &why)
            };
        }
    };
    if req.path.starts_with("/artifact/") {
        log::debug!("GET {} by {who} from {peer}", req.path);
        return artifact::serve(&mut stream, engine, &req);
    }
    if config.artifacts_only {
        return http::respond(&mut stream, 404, "text/plain", b"not found");
    }
    if req.path == "/ws" && req.is_websocket_upgrade() {
        if !req.origin_allowed(peer.ip().to_canonical().is_loopback()) {
            log::warn!("rejected cross-origin WebSocket from {:?} ({who}, {peer})", req.header("origin"));
            return http::respond(&mut stream, 403, "text/plain", b"cross-origin WebSocket rejected");
        }
        let Some(key) = req.header("sec-websocket-key") else {
            return http::respond(&mut stream, 400, "text/plain", b"missing Sec-WebSocket-Key");
        };
        let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
        write!(stream, "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n")?;
        stream.flush()?;
        log::info!("client connected: {who} from {peer}");
        let result = websocket(stream, leftover, engine, config);
        log::info!("client disconnected: {who} from {peer}");
        return result;
    }
    log::debug!("GET {} by {who} from {peer}", req.path);
    http::serve_static(&mut stream, config.web_dir.as_deref(), &req)
}

fn websocket(stream: TcpStream, leftover: Vec<u8>, engine: &EngineHandle, config: &ServerConfig) -> io::Result<()> {
    stream.set_read_timeout(Some(POLL))?;
    let ws_config = WebSocketConfig::default().max_message_size(Some(MAX_FRAME_BYTES)).max_frame_size(Some(MAX_FRAME_BYTES));
    let mut ws = WebSocket::from_partially_read(stream, leftover, Role::Server, Some(ws_config));
    let mut conn = engine.connect(Arc::new(|| {}));
    let mut last_heard = Instant::now();
    let mut last_ping = last_heard;
    loop {
        match ws.read() {
            Ok(msg) => {
                last_heard = Instant::now();
                match msg {
                    Message::Text(text) => match ClientMsg::from_json(text.as_str()) {
                        Ok(msg) => conn.send(msg),
                        Err(e) => {
                            let notice = ServerMsg::Notice { level: NoticeLevel::Error, text: format!("bad message: {e}") };
                            ws.send(Message::text(notice.to_json())).map_err(io::Error::other)?;
                        }
                    },
                    Message::Close(_) => return Ok(()),
                    _ => {}
                }
            }
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => return Ok(()),
            Err(e) => return Err(io::Error::other(e)),
        }
        let now = Instant::now();
        let quiet = now.saturating_duration_since(last_heard);
        if quiet >= config.idle_timeout {
            return Err(io::Error::new(io::ErrorKind::TimedOut, format!("client silent for {quiet:?}")));
        }
        if quiet >= config.ping_interval && now.saturating_duration_since(last_ping) >= config.ping_interval {
            last_ping = now;
            ws.send(Message::Ping(Default::default())).map_err(io::Error::other)?;
        }
        while let Some(msg) = conn.try_recv() {
            ws.send(Message::text(msg.to_json())).map_err(io::Error::other)?;
        }
        match ws.flush() {
            Ok(()) => {}
            Err(tungstenite::Error::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(io::Error::other(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use yah_protocol::PROTOCOL_VERSION;

    fn start_with(config: ServerConfig) -> SocketAddr {
        let engine = EngineHandle::spawn(yah_types::InferenceConfig::default()).unwrap();
        spawn(engine, ServerConfig { addr: SocketAddr::from(([127, 0, 0, 1], 0)), ..config }).unwrap()
    }

    fn start() -> SocketAddr {
        start_with(ServerConfig::default())
    }

    fn raw(addr: SocketAddr, request: &str) -> String {
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(request.as_bytes()).unwrap();
        let mut out = Vec::new();
        // A refusing server may reset after responding; keep whatever arrived.
        let _ = s.read_to_end(&mut out);
        String::from_utf8_lossy(&out).into_owned()
    }

    fn get(addr: SocketAddr, path: &str, extra: &str) -> String {
        raw(addr, &format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\n{extra}\r\n"))
    }

    fn upgrade(addr: SocketAddr, extra: &str) -> String {
        format!(
            "GET /ws HTTP/1.1\r\nHost: {addr}\r\n{extra}Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )
    }

    /// Sends `request` and reads only the response head (a 101 keeps the socket open).
    fn head(addr: SocketAddr, request: &str) -> (TcpStream, String) {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(request.as_bytes()).unwrap();
        let mut out = Vec::new();
        let mut byte = [0u8];
        while !out.ends_with(b"\r\n\r\n") && s.read(&mut byte).unwrap() == 1 {
            out.push(byte[0]);
        }
        (s, String::from_utf8_lossy(&out).into_owned())
    }

    #[test]
    fn websocket_round_trip() {
        let addr = start();
        let (mut ws, _) = tungstenite::connect(format!("ws://{addr}/ws")).unwrap();
        let hello = ClientMsg::Hello { protocol: PROTOCOL_VERSION, client: "test".into() };
        ws.send(Message::text(hello.to_json())).unwrap();
        let reply = ws.read().unwrap();
        let msg = ServerMsg::from_json(reply.to_text().unwrap()).unwrap();
        assert!(matches!(msg, ServerMsg::State(_)));
    }

    #[test]
    fn rejects_cross_origin_websocket() {
        let addr = start();
        assert!(raw(addr, &upgrade(addr, "Origin: https://evil.example\r\n")).starts_with("HTTP/1.1 403"));
    }

    #[test]
    fn static_path_traversal_is_rejected() {
        let addr = start();
        let out = get(addr, "/../../etc/passwd", "");
        assert!(out.starts_with("HTTP/1.1 404"), "{out}");
    }

    #[test]
    fn garbage_request_does_not_kill_server() {
        let addr = start();
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(b"\x00\x01garbage\r\n\r\n").unwrap();
        drop(s);
        assert!(get(addr, "/", "").starts_with("HTTP/1.1"));
    }

    #[test]
    fn allowlist_admits_listed_user_and_refuses_others() {
        let addr = start_with(ServerConfig { allowed_users: vec!["ann@example.com".into()], ..Default::default() });
        // What tailscale serve forwards for a tailnet user.
        let serve = |login: &str| format!("X-Forwarded-Host: {addr}\r\nTailscale-User-Login: {login}\r\nOrigin: https://{addr}\r\n");

        let (_ws, ok) = head(addr, &upgrade(addr, &serve("ann@example.com")));
        assert!(ok.starts_with("HTTP/1.1 101"), "{ok}");
        assert!(raw(addr, &upgrade(addr, &serve("bob@example.com"))).starts_with("HTTP/1.1 403"));

        let page = get(addr, "/", &serve("bob@example.com"));
        assert!(page.starts_with("HTTP/1.1 403"), "{page}");
        assert!(page.contains("text/html") && page.contains("bob@example.com is not allowed"), "{page}");
        // Forwarded by a proxy with no identity (e.g. a tagged device): refused.
        let tagged = get(addr, "/", &format!("X-Forwarded-Host: {addr}\r\n"));
        assert!(tagged.starts_with("HTTP/1.1 403"), "{tagged}");
        // Direct local access still works (404 only because no web dir is configured).
        assert!(get(addr, "/", "").starts_with("HTTP/1.1 404"));
    }

    #[test]
    fn funnel_traffic_is_refused() {
        let addr = start();
        let out = raw(addr, &upgrade(addr, &format!("X-Forwarded-Host: {addr}\r\nTailscale-Funnel-Request: ?1\r\n")));
        assert!(out.starts_with("HTTP/1.1 403"), "{out}");
    }

    #[test]
    fn connection_cap_answers_503_and_recovers() {
        let addr = start_with(ServerConfig { max_connections: 2, ..Default::default() });
        let idle1 = TcpStream::connect(addr).unwrap();
        let idle2 = TcpStream::connect(addr).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let busy = get(addr, "/", "");
        assert!(busy.starts_with("HTTP/1.1 503"), "{busy}");
        drop((idle1, idle2));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let out = get(addr, "/", "");
            if out.starts_with("HTTP/1.1 404") {
                break;
            }
            assert!(Instant::now() < deadline, "slots never released: {out}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn slowloris_head_is_cut_off() {
        let addr = start_with(ServerConfig { head_timeout: Duration::from_millis(300), ..Default::default() });
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        let started = Instant::now();
        s.write_all(b"GET / HTTP/1.1\r\n").unwrap();
        // Trickle a byte every ~50 ms: no single read ever waits long, so only a
        // whole-head deadline can stop this.
        let mut buf = [0u8; 64];
        let hung_up = loop {
            if started.elapsed() > Duration::from_secs(5) {
                break false;
            }
            if s.write_all(b"X").is_err() {
                break true;
            }
            match s.read(&mut buf) {
                Ok(_) => break true, // EOF (or an error response) — either way, it hung up
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
                Err(_) => break true,
            }
        };
        assert!(hung_up, "server never hung up");
        assert!(started.elapsed() < Duration::from_secs(2), "took {:?}", started.elapsed());
    }

    #[test]
    fn websocket_pings_and_drops_silent_client() {
        let addr = start_with(ServerConfig { ping_interval: Duration::from_millis(100), idle_timeout: Duration::from_millis(500), ..Default::default() });
        // A live client gets pinged and, by answering, outlives the idle timeout.
        let (mut ws, _) = tungstenite::connect(format!("ws://{addr}/ws")).unwrap();
        assert!(matches!(ws.read().unwrap(), Message::Ping(_)));
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(1000) {
            ws.read().unwrap(); // tungstenite queues a pong for each ping, flushed on the next read
        }
        // A client that never answers is dropped after the idle timeout.
        let (mut silent, ok) = head(addr, &upgrade(addr, ""));
        assert!(ok.starts_with("HTTP/1.1 101"), "{ok}");
        let started = Instant::now();
        let mut sink = Vec::new();
        let _ = silent.read_to_end(&mut sink);
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(400) && took < Duration::from_secs(3), "took {took:?}");
    }

    /// An engine whose store has chats `c1` and `c2` working in `ws`, with an HTML
    /// artifact in `c1` (returned: its id).
    fn with_artifact(ws: &std::path::Path, home: &std::path::Path, config: ServerConfig) -> (SocketAddr, String) {
        let paths = yah_store::Paths::at(home);
        paths.ensure().unwrap();
        let store = yah_store::Store::open(paths.db()).unwrap();
        store.create_chat("c1", "one", ws).unwrap();
        store.create_chat("c2", "two", ws).unwrap();
        drop(store);
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let ctx = yah_tools::ToolContext::new(ws, &cancel).with_chat("c1");
        let args = serde_json::json!({"title": "Calc", "filename": "calc.html", "content": "<!doctype html><button>Go</button>"});
        let out = yah_tools::Tool::run(&yah_tools::CreateArtifact, &args, &ctx);
        assert!(!out.failed, "{}", out.text);
        let id = yah_agent::artifact::list(ws, "c1")[0].id.clone();
        let mut engine_config = yah_agent::EngineConfig::new(yah_types::InferenceConfig::default());
        engine_config.paths = Some(paths);
        engine_config.workspace = ws.to_path_buf();
        let engine = EngineHandle::spawn_with(engine_config).unwrap();
        (spawn(engine, ServerConfig { addr: SocketAddr::from(([127, 0, 0, 1], 0)), ..config }).unwrap(), id)
    }

    #[test]
    fn artifacts_are_served_sandboxed_and_only_in_their_chat() {
        let (ws, home) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (addr, id) = with_artifact(ws.path(), home.path(), ServerConfig::default());
        let ok = get(addr, &format!("/artifact/c1/{id}/1"), "");
        assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
        assert!(ok.contains("Content-Type: text/html; charset=utf-8"), "{ok}");
        let csp = ok.lines().find_map(|l| l.strip_prefix("Content-Security-Policy: ")).unwrap();
        for part in ["sandbox allow-scripts allow-forms", "default-src 'none'", "connect-src 'none'", "frame-ancestors 'self'", "script-src 'unsafe-inline'"] {
            assert!(csp.contains(part), "{csp}");
        }
        assert!(!csp.contains("allow-same-origin"));
        assert!(ok.contains("Cache-Control: no-store") && ok.contains("X-Content-Type-Options: nosniff"), "{ok}");
        assert!(ok.contains("<button>Go</button>") && ok.contains("RTCPeerConnection"), "the helper is injected: {ok}");
        let dl = get(addr, &format!("/artifact/c1/{id}/1?download"), "");
        assert!(dl.contains("Content-Disposition: attachment; filename=\"calc.html\"") && dl.ends_with("<!doctype html><button>Go</button>"), "{dl}");
        // Another chat, a missing version, an unknown chat or id: all 404.
        for path in [
            format!("/artifact/c2/{id}/1"),
            format!("/artifact/c1/{id}/2"),
            format!("/artifact/nope/{id}/1"),
            "/artifact/c1/nope/1".into(),
            "/artifact/c1/../1".into(),
        ] {
            let out = get(addr, &path, "");
            assert!(out.starts_with("HTTP/1.1 404"), "{path}: {out}");
        }
        // The allowlist applies to artifacts too.
        let (addr, id) = with_artifact(
            tempfile::tempdir().unwrap().path(),
            tempfile::tempdir().unwrap().path(),
            ServerConfig { allowed_users: vec!["ann@example.com".into()], ..Default::default() },
        );
        let other = get(addr, &format!("/artifact/c1/{id}/1"), &format!("X-Forwarded-Host: {addr}\r\nTailscale-User-Login: bob@example.com\r\n"));
        assert!(other.starts_with("HTTP/1.1 403"), "{other}");
    }

    #[test]
    fn artifacts_only_server_serves_nothing_else() {
        let (ws, home) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (addr, id) = with_artifact(ws.path(), home.path(), ServerConfig { artifacts_only: true, ..Default::default() });
        assert!(get(addr, &format!("/artifact/c1/{id}/1"), "").starts_with("HTTP/1.1 200"));
        assert!(raw(addr, &upgrade(addr, "")).starts_with("HTTP/1.1 404"));
        assert!(get(addr, "/", "").starts_with("HTTP/1.1 404"));
    }

    #[test]
    fn serves_precompressed_gzip_with_revalidation() {
        let dir = std::env::temp_dir().join(format!("yah-web-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("app.js"), b"console.log(1)").unwrap();
        std::fs::write(dir.join("app.js.gz"), b"GZ").unwrap();
        let addr = start_with(ServerConfig { web_dir: Some(dir.clone()), ..Default::default() });

        let gz = get(addr, "/app.js", "Accept-Encoding: gzip, br\r\n");
        assert!(gz.starts_with("HTTP/1.1 200") && gz.contains("Content-Encoding: gzip") && gz.contains("text/javascript") && gz.ends_with("GZ"), "{gz}");
        assert!(gz.contains("Vary: Accept-Encoding") && gz.contains("Cache-Control: no-cache"), "{gz}");
        let plain = get(addr, "/app.js", "");
        assert!(!plain.contains("Content-Encoding") && plain.ends_with("console.log(1)"), "{plain}");

        let etag = gz.lines().find_map(|l| l.strip_prefix("ETag: ")).unwrap().to_owned();
        let cached = get(addr, "/app.js", &format!("Accept-Encoding: gzip\r\nIf-None-Match: {etag}\r\n"));
        assert!(cached.starts_with("HTTP/1.1 304") && cached.ends_with("\r\n\r\n"), "{cached}");
        // The gzip ETag must not validate the identity body.
        let other = get(addr, "/app.js", &format!("If-None-Match: {etag}\r\n"));
        assert!(other.starts_with("HTTP/1.1 200"), "{other}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
