//! HTTP + WebSocket front door for remote clients (e.g. a browser over Tailscale).
//!
//! - `GET /ws` upgrades to a WebSocket carrying [`yah_protocol`] JSON frames.
//! - Any other `GET` serves the static web bundle (`apps/yah-web`, built by `cargo xtask web`).
//!
//! Deliberately simple and blocking: one thread per connection. Binds to loopback by
//! default; expose it with `tailscale serve`, which terminates TLS and limits access to
//! your tailnet. Never bind it to a public interface.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

mod http;

use std::io::{self, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

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
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self { addr: SocketAddr::from(([127, 0, 0, 1], 7470)), web_dir: None }
    }
}

/// Bind and start accepting connections on a background thread. Returns the bound address.
pub fn spawn(engine: EngineHandle, config: ServerConfig) -> io::Result<SocketAddr> {
    let listener = TcpListener::bind(config.addr)?;
    let addr = listener.local_addr()?;
    let config = Arc::new(config);
    thread::Builder::new().name("yah-server".into()).spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let (engine, config) = (engine.clone(), Arc::clone(&config));
            let spawned = thread::Builder::new().name("yah-conn".into()).spawn(move || {
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

fn handle(mut stream: TcpStream, engine: &EngineHandle, config: &ServerConfig) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let (req, leftover) = http::read_request(&mut stream)?;
    if req.method != "GET" {
        return http::respond(&mut stream, 405, "text/plain", b"method not allowed");
    }
    if req.path == "/ws" && req.is_websocket_upgrade() {
        if !req.origin_allowed() {
            log::warn!("rejected cross-origin WebSocket from {:?}", req.header("origin"));
            return http::respond(&mut stream, 403, "text/plain", b"cross-origin WebSocket rejected");
        }
        let Some(key) = req.header("sec-websocket-key") else {
            return http::respond(&mut stream, 400, "text/plain", b"missing Sec-WebSocket-Key");
        };
        let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
        write!(stream, "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n")?;
        stream.flush()?;
        return websocket(stream, leftover, engine);
    }
    http::serve_static(&mut stream, config.web_dir.as_deref(), &req.path)
}

fn websocket(stream: TcpStream, leftover: Vec<u8>, engine: &EngineHandle) -> io::Result<()> {
    stream.set_read_timeout(Some(POLL))?;
    let ws_config = WebSocketConfig::default().max_message_size(Some(MAX_FRAME_BYTES)).max_frame_size(Some(MAX_FRAME_BYTES));
    let mut ws = WebSocket::from_partially_read(stream, leftover, Role::Server, Some(ws_config));
    let mut conn = engine.connect(Arc::new(|| {}));
    loop {
        match ws.read() {
            Ok(Message::Text(text)) => match ClientMsg::from_json(text.as_str()) {
                Ok(msg) => conn.send(msg),
                Err(e) => {
                    let notice = ServerMsg::Notice { level: NoticeLevel::Error, text: format!("bad message: {e}") };
                    ws.send(Message::text(notice.to_json())).map_err(io::Error::other)?;
                }
            },
            Ok(Message::Close(_)) => return Ok(()),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => return Ok(()),
            Err(e) => return Err(io::Error::other(e)),
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

    fn start() -> SocketAddr {
        let engine = EngineHandle::spawn(yah_types::InferenceConfig::default()).unwrap();
        spawn(engine, ServerConfig { addr: SocketAddr::from(([127, 0, 0, 1], 0)), web_dir: None }).unwrap()
    }

    fn raw(addr: SocketAddr, request: &str) -> String {
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(request.as_bytes()).unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
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
        let req = format!(
            "GET /ws HTTP/1.1\r\nHost: {addr}\r\nOrigin: https://evil.example\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        assert!(raw(addr, &req).starts_with("HTTP/1.1 403"));
    }

    #[test]
    fn static_path_traversal_is_rejected() {
        let addr = start();
        let out = raw(addr, &format!("GET /../../etc/passwd HTTP/1.1\r\nHost: {addr}\r\n\r\n"));
        assert!(out.starts_with("HTTP/1.1 404"), "{out}");
    }

    #[test]
    fn garbage_request_does_not_kill_server() {
        let addr = start();
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(b"\x00\x01garbage\r\n\r\n").unwrap();
        drop(s);
        assert!(raw(addr, &format!("GET / HTTP/1.1\r\nHost: {addr}\r\n\r\n")).starts_with("HTTP/1.1"));
    }
}
