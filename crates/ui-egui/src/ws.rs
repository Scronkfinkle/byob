//! [`Connection`] over a WebSocket, via `ewebsock` (browser `WebSocket` on wasm, a
//! background thread natively). Used by the web build and, later, by the desktop app
//! when attaching to a remote byob.
//!
//! The connection heals itself: when the socket closes or errors (a phone going to sleep,
//! a Wi-Fi blip, the server restarting) it redials with exponential backoff and replays
//! the client's `Hello`, so the engine resends `State` + `Transcript` and the UI resyncs.
//! Progress is reported through [`LinkStatus`] rather than a notice per retry.
//!
//! No threads or timers of our own: retries are driven from [`Connection::try_recv`], which
//! the UI polls every frame, and we ask egui for a repaint when the next retry is due.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use web_time::Instant;
use byob_protocol::{ClientMsg, Connection, ServerMsg};

/// First retry delay; doubles per failed attempt up to [`MAX_BACKOFF`].
const MIN_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(10);
/// A dial that hasn't opened by then is abandoned (dead networks can hang a handshake).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// A connection must stay open this long before the backoff resets, so a server that
/// accepts and immediately drops us is not hammered at the minimum delay.
const STABLE_AFTER: Duration = Duration::from_secs(5);
/// Messages typed while offline are queued up to this many, then the oldest are dropped.
const MAX_PENDING: usize = 64;

/// What the UI should show about the link to the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkStatus {
    /// First connection attempt in progress.
    Connecting,
    /// Socket is open.
    Connected,
    /// The link dropped; `attempt` redials so far, next one in `retry_in` (zero while dialing).
    Reconnecting { attempt: u32, retry_in: Duration },
}

impl LinkStatus {
    /// Short human-readable label, or `None` when everything is fine.
    pub fn label(self) -> Option<String> {
        match self {
            Self::Connected => None,
            Self::Connecting => Some("connecting…".into()),
            Self::Reconnecting { retry_in, .. } if retry_in.is_zero() => Some("reconnecting…".into()),
            Self::Reconnecting { retry_in, .. } => Some(format!("disconnected — reconnecting in {}s…", retry_in.as_secs().saturating_add(1))),
        }
    }
}

/// A shareable view of a [`WsConnection`]'s [`LinkStatus`], readable after the connection
/// has been boxed into the UI.
#[derive(Debug, Clone)]
pub struct LinkStatusHandle(Arc<Mutex<LinkStatus>>);

impl LinkStatusHandle {
    pub fn get(&self) -> LinkStatus {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set(&self, status: LinkStatus) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = status;
    }
}

/// What the reconnect machine wants the caller to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Nothing,
    /// Open a new socket (dropping any old one) and call [`Reconnect::dialing`].
    Dial,
    /// The current dial timed out: drop the socket; a retry is scheduled.
    Abandon,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Dialing { since: Instant },
    Open { since: Instant },
    Waiting { until: Instant },
}

/// The reconnect state machine, free of I/O so it can be unit-tested with synthetic time.
#[derive(Debug, Clone)]
pub struct Reconnect {
    phase: Phase,
    /// Consecutive failed attempts (drives the backoff).
    failures: u32,
    ever_opened: bool,
}

impl Reconnect {
    /// Starts in the dialing phase: the caller has just opened the first socket.
    pub fn new(now: Instant) -> Self {
        Self { phase: Phase::Dialing { since: now }, failures: 0, ever_opened: false }
    }

    /// Delay before retry number `failures` (1-based): 0.5 s, 1 s, 2 s, … capped at 10 s.
    pub fn backoff(failures: u32) -> Duration {
        let shift = failures.saturating_sub(1).min(16);
        MIN_BACKOFF.saturating_mul(1u32 << shift).min(MAX_BACKOFF)
    }

    pub fn is_open(&self) -> bool {
        matches!(self.phase, Phase::Open { .. })
    }

    /// The socket reported `Opened`.
    pub fn opened(&mut self, now: Instant) {
        self.phase = Phase::Open { since: now };
        self.ever_opened = true;
    }

    /// The socket closed, errored, or could not be created. Idempotent: browsers report
    /// both `error` and `close` for one failure, and that must count once.
    pub fn dropped(&mut self, now: Instant) {
        match self.phase {
            Phase::Waiting { .. } => return,
            Phase::Open { since } if now.saturating_duration_since(since) >= STABLE_AFTER => self.failures = 0,
            Phase::Open { .. } | Phase::Dialing { .. } => {}
        }
        self.failures = self.failures.saturating_add(1);
        self.phase = Phase::Waiting { until: now.checked_add(Self::backoff(self.failures)).unwrap_or(now) };
    }

    /// A new socket was just opened by the caller.
    pub fn dialing(&mut self, now: Instant) {
        self.phase = Phase::Dialing { since: now };
    }

    /// Called every poll: is a retry due, or has a dial hung?
    pub fn poll(&mut self, now: Instant) -> Action {
        match self.phase {
            Phase::Waiting { until } if now >= until => Action::Dial,
            Phase::Dialing { since } if now.saturating_duration_since(since) >= CONNECT_TIMEOUT => {
                self.dropped(now);
                Action::Abandon
            }
            _ => Action::Nothing,
        }
    }

    /// When the caller should be woken next even if no socket event arrives.
    pub fn next_deadline(&self) -> Option<Instant> {
        match self.phase {
            Phase::Waiting { until } => Some(until),
            Phase::Dialing { since } => Some(since.checked_add(CONNECT_TIMEOUT).unwrap_or(since)),
            Phase::Open { .. } => None,
        }
    }

    pub fn status(&self, now: Instant) -> LinkStatus {
        match self.phase {
            Phase::Open { .. } => LinkStatus::Connected,
            Phase::Dialing { .. } if !self.ever_opened && self.failures == 0 => LinkStatus::Connecting,
            Phase::Dialing { .. } => LinkStatus::Reconnecting { attempt: self.failures, retry_in: Duration::ZERO },
            Phase::Waiting { until } => LinkStatus::Reconnecting { attempt: self.failures, retry_in: until.saturating_duration_since(now) },
        }
    }
}

pub struct WsConnection {
    url: String,
    ctx: egui::Context,
    socket: Option<(ewebsock::WsSender, ewebsock::WsReceiver)>,
    link: Reconnect,
    status: LinkStatusHandle,
    /// The client's `Hello`, replayed on every (re)open.
    hello: Option<ClientMsg>,
    /// The chat the engine last showed us (as the message that reopens it), replayed after
    /// `Hello` so a reconnect lands back on the same chat.
    view: Option<ClientMsg>,
    /// Messages sent while the socket was down.
    pending: VecDeque<ClientMsg>,
}

impl WsConnection {
    /// Start connecting. Never fails: a socket that can't be created is retried like a
    /// dropped one. `ctx` is woken when messages arrive and when a retry is due.
    pub fn connect(url: &str, ctx: egui::Context) -> Self {
        let now = Instant::now();
        let mut conn = Self {
            url: url.to_owned(),
            ctx,
            socket: None,
            link: Reconnect::new(now),
            status: LinkStatusHandle(Arc::new(Mutex::new(LinkStatus::Connecting))),
            hello: None,
            view: None,
            pending: VecDeque::new(),
        };
        conn.dial(now);
        conn
    }

    /// A handle the app can keep to show the link status (e.g. a "reconnecting…" banner).
    pub fn status_handle(&self) -> LinkStatusHandle {
        self.status.clone()
    }

    fn dial(&mut self, now: Instant) {
        self.socket = None; // dropping the sender closes any previous socket
        self.link.dialing(now);
        let ctx = self.ctx.clone();
        match ewebsock::connect_with_wakeup(&self.url, ewebsock::Options::default(), move || ctx.request_repaint()) {
            Ok(socket) => self.socket = Some(socket),
            Err(e) => {
                log::warn!("cannot open WebSocket to {}: {e}", self.url);
                self.link.dropped(now);
            }
        }
    }

    fn send_now(&mut self, msg: &ClientMsg) {
        if let Some((tx, _)) = self.socket.as_mut() {
            tx.send(ewebsock::WsMessage::Text(msg.to_json()));
        }
    }

    fn on_open(&mut self, now: Instant) {
        self.link.opened(now);
        if let Some(hello) = self.hello.clone() {
            self.send_now(&hello);
            if let Some(view) = self.view.clone() {
                self.send_now(&view);
            }
        }
        while let Some(msg) = self.pending.pop_front() {
            self.send_now(&msg);
        }
    }

    fn on_drop(&mut self, now: Instant, why: &str) {
        if !matches!(self.link.status(now), LinkStatus::Reconnecting { .. }) {
            log::info!("link to engine lost ({why}); reconnecting");
        }
        self.socket = None;
        self.link.dropped(now);
    }

    /// Drive timers and publish the status; called on every poll.
    fn tick(&mut self, now: Instant) {
        match self.link.poll(now) {
            Action::Dial => self.dial(now),
            Action::Abandon => self.socket = None,
            Action::Nothing => {}
        }
        self.status.set(self.link.status(now));
        if let Some(deadline) = self.link.next_deadline() {
            // Wake at the deadline, but at least once a second so the countdown updates.
            let wait = deadline.saturating_duration_since(now).min(Duration::from_secs(1));
            self.ctx.request_repaint_after(wait);
        }
    }
}

impl Connection for WsConnection {
    fn send(&mut self, msg: ClientMsg) {
        if matches!(msg, ClientMsg::Hello { .. }) {
            self.hello = Some(msg.clone());
        }
        if self.link.is_open() {
            self.send_now(&msg);
        } else if !matches!(msg, ClientMsg::Hello { .. }) {
            if self.pending.len() >= MAX_PENDING {
                self.pending.pop_front();
            }
            self.pending.push_back(msg);
        }
    }

    fn try_recv(&mut self) -> Option<ServerMsg> {
        loop {
            let now = Instant::now();
            let Some(event) = self.socket.as_ref().and_then(|(_, rx)| rx.try_recv()) else {
                self.tick(now);
                return None;
            };
            match event {
                ewebsock::WsEvent::Opened => {
                    self.on_open(now);
                    self.status.set(LinkStatus::Connected);
                }
                ewebsock::WsEvent::Message(ewebsock::WsMessage::Text(text)) => match ServerMsg::from_json(&text) {
                    Ok(msg) => {
                        if let ServerMsg::ChatOpened { chat } = &msg {
                            self.view = Some(chat.as_ref().map_or(ClientMsg::NewChat, |c| ClientMsg::OpenChat { id: c.id.clone() }));
                        }
                        return Some(msg);
                    }
                    Err(e) => log::warn!("bad frame from engine: {e}"),
                },
                ewebsock::WsEvent::Message(_) => {}
                ewebsock::WsEvent::Error(e) => self.on_drop(now, &e),
                ewebsock::WsEvent::Closed => self.on_drop(now, "closed"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: f64) -> Duration {
        Duration::from_secs_f64(s)
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let got: Vec<f64> = (1..=8).map(|n| Reconnect::backoff(n).as_secs_f64()).collect();
        assert_eq!(got, [0.5, 1.0, 2.0, 4.0, 8.0, 10.0, 10.0, 10.0]);
        assert_eq!(Reconnect::backoff(u32::MAX), MAX_BACKOFF);
    }

    #[test]
    fn first_connect_then_drop_then_redial() {
        let t0 = Instant::now();
        let mut r = Reconnect::new(t0);
        assert_eq!(r.status(t0), LinkStatus::Connecting);
        r.opened(t0);
        assert_eq!(r.status(t0), LinkStatus::Connected);
        assert_eq!(r.next_deadline(), None);

        let t1 = t0 + secs(60.0);
        r.dropped(t1);
        assert_eq!(r.status(t1), LinkStatus::Reconnecting { attempt: 1, retry_in: secs(0.5) });
        assert_eq!(r.poll(t1 + secs(0.4)), Action::Nothing);
        assert_eq!(r.poll(t1 + secs(0.5)), Action::Dial);
        r.dialing(t1 + secs(0.5));
        assert_eq!(r.status(t1 + secs(0.5)), LinkStatus::Reconnecting { attempt: 1, retry_in: Duration::ZERO });
        r.opened(t1 + secs(0.6));
        assert!(r.is_open());
    }

    #[test]
    fn error_and_close_for_one_failure_count_once() {
        let t0 = Instant::now();
        let mut r = Reconnect::new(t0);
        r.dropped(t0);
        r.dropped(t0);
        assert_eq!(r.status(t0), LinkStatus::Reconnecting { attempt: 1, retry_in: secs(0.5) });
    }

    #[test]
    fn repeated_failures_back_off_and_stable_link_resets() {
        let mut now = Instant::now();
        let mut r = Reconnect::new(now);
        for expected in [0.5, 1.0, 2.0, 4.0] {
            r.dropped(now);
            assert_eq!(r.next_deadline(), Some(now + secs(expected)));
            now += secs(expected);
            assert_eq!(r.poll(now), Action::Dial);
            r.dialing(now);
        }
        // Opens but drops again immediately: still backing off.
        r.opened(now);
        r.dropped(now + secs(1.0));
        assert_eq!(r.status(now + secs(1.0)), LinkStatus::Reconnecting { attempt: 5, retry_in: secs(8.0) });
        now += secs(9.0);
        assert_eq!(r.poll(now), Action::Dial);
        r.dialing(now);
        // Stays up long enough: the next drop starts from the minimum again.
        r.opened(now);
        now += STABLE_AFTER;
        r.dropped(now);
        assert_eq!(r.status(now), LinkStatus::Reconnecting { attempt: 1, retry_in: secs(0.5) });
    }

    #[test]
    fn hung_dial_is_abandoned() {
        let t0 = Instant::now();
        let mut r = Reconnect::new(t0);
        assert_eq!(r.poll(t0 + secs(9.0)), Action::Nothing);
        assert_eq!(r.poll(t0 + CONNECT_TIMEOUT), Action::Abandon);
        assert!(matches!(r.status(t0 + CONNECT_TIMEOUT), LinkStatus::Reconnecting { attempt: 1, .. }));
    }

    #[test]
    fn labels() {
        assert_eq!(LinkStatus::Connected.label(), None);
        assert_eq!(LinkStatus::Reconnecting { attempt: 2, retry_in: secs(1.5) }.label().unwrap(), "disconnected — reconnecting in 2s…");
    }

    /// End to end over a real socket: the server drops the first connection; the client
    /// must redial on its own and replay `Hello` plus anything typed while offline.
    #[test]
    fn reconnects_and_replays_hello() {
        use std::net::TcpListener;
        use std::sync::mpsc;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (seen_tx, seen_rx) = mpsc::channel::<(usize, String)>();
        std::thread::spawn(move || {
            for (n, stream) in listener.incoming().enumerate() {
                let mut ws = tungstenite::accept(stream.unwrap()).unwrap();
                while let Ok(msg) = ws.read() {
                    if let tungstenite::Message::Text(t) = msg {
                        seen_tx.send((n, t.to_string())).unwrap();
                        if n == 0 {
                            break; // drop the first connection right after Hello
                        }
                    }
                }
            }
        });

        let mut conn = WsConnection::connect(&format!("ws://{addr}/ws"), egui::Context::default());
        let status = conn.status_handle();
        conn.send(ClientMsg::Hello { protocol: 1, client: "test".into() });
        let deadline = Instant::now() + secs(10.0);
        let mut seen = Vec::new();
        let mut queued = false;
        while seen.iter().filter(|(n, _)| *n == 1).count() < 2 && Instant::now() < deadline {
            conn.try_recv();
            if !queued && matches!(status.get(), LinkStatus::Reconnecting { .. }) {
                conn.send(ClientMsg::Cancel); // typed while offline
                queued = true;
            }
            seen.extend(seen_rx.try_iter());
            std::thread::sleep(Duration::from_millis(10));
        }
        let first: Vec<_> = seen.iter().filter(|(n, _)| *n == 0).map(|(_, t)| t.as_str()).collect();
        let second: Vec<_> = seen.iter().filter(|(n, _)| *n == 1).map(|(_, t)| t.as_str()).collect();
        assert_eq!(first.len(), 1, "{seen:?}");
        assert!(first[0].contains("\"hello\""));
        assert_eq!(second.len(), 2, "{seen:?}");
        assert!(second[0].contains("\"hello\""), "Hello must be replayed first: {second:?}");
        assert!(second[1].contains("\"cancel\""));
        assert_eq!(status.get(), LinkStatus::Connected);
    }
}
