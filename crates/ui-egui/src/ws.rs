//! [`Connection`] over a WebSocket, via `ewebsock` (browser `WebSocket` on wasm, a
//! background thread natively). Used by the web build and, later, by the desktop app
//! when attaching to a remote yah.

use yah_protocol::{ClientMsg, Connection, NoticeLevel, ServerMsg};

pub struct WsConnection {
    tx: ewebsock::WsSender,
    rx: ewebsock::WsReceiver,
    open: bool,
    /// Messages sent before the socket opened.
    pending: Vec<ClientMsg>,
}

impl WsConnection {
    /// `wake` is called whenever a message arrives (typically `ctx.request_repaint()`).
    pub fn connect(url: &str, wake: impl Fn() + Send + Sync + 'static) -> Result<Self, String> {
        let (tx, rx) = ewebsock::connect_with_wakeup(url, ewebsock::Options::default(), wake)?;
        Ok(Self { tx, rx, open: false, pending: Vec::new() })
    }
}

impl Connection for WsConnection {
    fn send(&mut self, msg: ClientMsg) {
        if self.open {
            self.tx.send(ewebsock::WsMessage::Text(msg.to_json()));
        } else {
            self.pending.push(msg);
        }
    }

    fn try_recv(&mut self) -> Option<ServerMsg> {
        while let Some(event) = self.rx.try_recv() {
            match event {
                ewebsock::WsEvent::Opened => {
                    self.open = true;
                    for msg in std::mem::take(&mut self.pending) {
                        self.tx.send(ewebsock::WsMessage::Text(msg.to_json()));
                    }
                }
                ewebsock::WsEvent::Message(ewebsock::WsMessage::Text(text)) => match ServerMsg::from_json(&text) {
                    Ok(msg) => return Some(msg),
                    Err(e) => log::warn!("bad frame from engine: {e}"),
                },
                ewebsock::WsEvent::Message(_) => {}
                ewebsock::WsEvent::Error(e) => {
                    return Some(ServerMsg::Notice { level: NoticeLevel::Error, text: format!("connection error: {e}") });
                }
                ewebsock::WsEvent::Closed => {
                    self.open = false;
                    return Some(ServerMsg::Notice { level: NoticeLevel::Error, text: "disconnected from engine; reload to reconnect".into() });
                }
            }
        }
        None
    }
}
