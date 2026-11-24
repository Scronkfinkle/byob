//! Desktop notifications: `notify(title, body, action)`. Clicking one brings the window back
//! and hands its action id to the app ([`super::Event::Clicked`]), e.g. to open a chat.
//!
//! - Linux and the BSDs: `org.freedesktop.Notifications` over D-Bus (zbus 5, which byob has
//!   already; notify-rust is on zbus 6), with a `default` action whose `ActionInvoked`
//!   signal is the click.
//! - macOS and Windows: notify-rust, best effort (shown, but a click doesn't come back).

use super::Bus;

/// Sends notifications; set up on first use, so a byob that never notifies never connects.
pub struct Notifier {
    bus: Bus,
    #[cfg(all(unix, not(target_os = "macos")))]
    worker: Option<std::sync::mpsc::Sender<Note>>,
}

/// One notification.
#[derive(Debug, Clone)]
pub struct Note {
    pub title: String,
    pub body: String,
    /// What a click means to the app (`None`: just show the window).
    pub action: Option<String>,
}

impl Notifier {
    pub fn new(bus: Bus) -> Self {
        Self {
            bus,
            #[cfg(all(unix, not(target_os = "macos")))]
            worker: None,
        }
    }

    /// Show a notification (in the background: this never blocks or fails; problems are logged).
    pub fn notify(&mut self, title: &str, body: &str, action: Option<&str>) {
        let note = Note { title: title.to_owned(), body: body.to_owned(), action: action.map(str::to_owned) };
        self.send(note);
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    fn send(&mut self, note: Note) {
        let worker = self.worker.get_or_insert_with(|| dbus::start(self.bus.clone()));
        if let Err(std::sync::mpsc::SendError(note)) = worker.send(note) {
            // The worker ended (no session bus?): try again once with a new one.
            let worker = dbus::start(self.bus.clone());
            let _ = worker.send(note);
            self.worker = Some(worker);
        }
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn send(&mut self, note: Note) {
        let _ = &self.bus;
        let spawned = std::thread::Builder::new().name("byob-notify".into()).spawn(move || {
            let shown = notify_rust::Notification::new().appname("byob").summary(&note.title).body(&note.body).show();
            if let Err(e) = shown {
                log::warn!("couldn't show a notification: {e}");
            }
        });
        if let Err(e) = spawned {
            log::warn!("couldn't show a notification: {e}");
        }
    }

    #[cfg(not(any(unix, windows)))]
    fn send(&mut self, note: Note) {
        let _ = &self.bus;
        log::info!("notification (no notifications here): {}: {}", note.title, note.body);
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod dbus {
    use std::collections::HashMap;
    use std::sync::mpsc::{Receiver, Sender};
    use std::sync::{Arc, Mutex, PoisonError};

    use zbus::blocking::{Connection, MessageIterator};
    use zbus::zvariant::Value;

    use super::Note;
    use crate::background::{Bus, Event};

    const DEST: &str = "org.freedesktop.Notifications";
    const PATH: &str = "/org/freedesktop/Notifications";
    /// Clicks remembered for notifications still up, at most (the oldest are forgotten).
    const MAX_PENDING: usize = 64;

    /// Notification id → its action (`""`: none, just show the window).
    type Pending = Arc<Mutex<HashMap<u32, String>>>;

    /// A thread that sends notes; it listens for their clicks on another.
    pub fn start(bus: Bus) -> Sender<Note> {
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new().name("byob-notify".into()).spawn(move || {
            let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&bus, &rx)));
            match run {
                Ok(Err(e)) => log::warn!("notifications: {e}"),
                Err(_) => log::warn!("notifications stopped after a panic"),
                Ok(Ok(())) => {}
            }
        });
        if let Err(e) = spawned {
            log::warn!("couldn't start notifications: {e}");
        }
        tx
    }

    fn run(bus: &Bus, rx: &Receiver<Note>) -> Result<(), String> {
        let conn = Connection::session().map_err(|e| format!("no session bus: {e}"))?;
        let pending: Pending = Arc::default();
        listen(&conn, bus.clone(), pending.clone());
        for note in rx {
            let mut hints: HashMap<&str, Value> = HashMap::new();
            hints.insert("desktop-entry", Value::from("byob"));
            let actions: Vec<&str> = vec!["default", "Open byob"];
            let reply = conn.call_method(
                Some(DEST),
                PATH,
                Some(DEST),
                "Notify",
                &("byob", 0u32, "byob", note.title.as_str(), note.body.as_str(), actions, hints, -1i32),
            );
            match reply.and_then(|m| m.body().deserialize::<u32>()) {
                Ok(id) => {
                    let mut map = pending.lock().unwrap_or_else(PoisonError::into_inner);
                    if map.len() >= MAX_PENDING
                        && let Some(oldest) = map.keys().min().copied()
                    {
                        map.remove(&oldest);
                    }
                    map.insert(id, note.action.unwrap_or_default());
                }
                Err(e) => log::warn!("couldn't show a notification: {e}"),
            }
        }
        Ok(())
    }

    /// Clicks (`ActionInvoked`) and closes (`NotificationClosed`) of our notifications.
    fn listen(conn: &Connection, bus: Bus, pending: Pending) {
        for (member, clicked) in [("ActionInvoked", true), ("NotificationClosed", false)] {
            let rule = zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .interface(DEST)
                .and_then(|b| b.member(member))
                .and_then(|b| b.path(PATH))
                .map(zbus::match_rule::Builder::build);
            let rule = match rule {
                Ok(r) => r,
                Err(e) => {
                    log::warn!("notification clicks won't open byob: {e}");
                    continue;
                }
            };
            let iter = match MessageIterator::for_match_rule(rule, conn, Some(16)) {
                Ok(i) => i,
                Err(e) => {
                    log::warn!("notification clicks won't open byob: {e}");
                    continue;
                }
            };
            let (bus, pending) = (bus.clone(), pending.clone());
            let spawned = std::thread::Builder::new().name("byob-notify-clicks".into()).spawn(move || {
                for msg in iter.flatten() {
                    let id = if clicked {
                        msg.body().deserialize::<(u32, String)>().ok().map(|(id, _key)| id)
                    } else {
                        msg.body().deserialize::<(u32, u32)>().ok().map(|(id, _reason)| id)
                    };
                    let Some(id) = id else { continue };
                    let action = pending.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
                    if clicked && let Some(action) = action {
                        bus.send(Event::Clicked(action));
                    }
                }
            });
            if let Err(e) = spawned {
                log::warn!("notification clicks won't open byob: {e}");
            }
        }
    }
}
