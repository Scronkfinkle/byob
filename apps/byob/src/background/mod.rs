//! Running in the background: closing the window can hide it instead of quitting (the
//! engine, the browser server and routines go on), a tray icon brings it back, one byob per
//! data folder, notifications, and starting at login.
//!
//! Everything that happens outside the window (the tray, the instance listener, notification
//! clicks) arrives as an [`Event`] on the [`Bus`], which wakes the window so `App::logic` (run
//! even while the window is hidden) handles it on the main thread.
//!
//! Where a window can't be hidden (Wayland: winit can't unmap one), it is closed instead and
//! byob waits without one ([`Background::wait_to_reopen`]); showing it opens a new window.

pub mod autostart;
pub mod instance;
pub mod notify;
pub mod tray;

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};

use byob_protocol::routines::{RoutineView, RunStatus};
use byob_protocol::{ClientMsg, Connection, RoutineEventKind, ServerMsg};
use byob_store::KeepRunning;
use byob_ui_egui::ByobApp;

/// Something for the window to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Show the window and bring it to the front (the tray, a second launch).
    Show,
    /// Quit for real (the tray's Quit byob).
    Quit,
    /// Pause or resume routines (the tray).
    TogglePause,
    /// Run a routine now, by id (the tray's Run ▸, `byob --run-routine`).
    Run(String),
    /// A notification was clicked: its action id (`""`: none).
    Clicked(String),
    /// Whether a tray host shows our icon (it can come and go).
    TrayHost(bool),
    /// The engine has something for the background's own connection.
    Engine,
}

/// Where [`Event`]s go; cheap to clone, usable from any thread.
#[derive(Clone)]
pub struct Bus {
    tx: Sender<Event>,
    /// The window to wake (none between windows).
    ctx: Arc<Mutex<Option<egui::Context>>>,
    /// The routines (id, name), for `run <name>` from another launch.
    routines: Arc<Mutex<Vec<(String, String)>>>,
}

impl Bus {
    pub fn new() -> (Self, Receiver<Event>) {
        let (tx, rx) = std::sync::mpsc::channel();
        (Self { tx, ctx: Arc::default(), routines: Arc::default() }, rx)
    }

    /// Wake this window for every event from now on (events before it wait for its first frame).
    pub fn attach(&self, ctx: &egui::Context) {
        *self.ctx.lock().unwrap_or_else(PoisonError::into_inner) = Some(ctx.clone());
        ctx.request_repaint();
    }

    /// The window is gone.
    fn detach(&self) {
        *self.ctx.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// What wakes the background when the engine sends its connection something.
    pub fn engine_waker(&self) -> byob_agent::Waker {
        let bus = self.clone();
        Arc::new(move || bus.send(Event::Engine))
    }

    /// The routine `name` (or id) stands for, ignoring case.
    pub fn find_routine(&self, name: &str) -> Option<String> {
        let name = name.trim();
        let list = self.routines.lock().unwrap_or_else(PoisonError::into_inner);
        list.iter().find(|(id, n)| id == name || n.eq_ignore_ascii_case(name)).map(|(id, _)| id.clone())
    }

    fn set_routines(&self, items: &[RoutineView]) {
        *self.routines.lock().unwrap_or_else(PoisonError::into_inner) = items.iter().map(|v| (v.routine.id.clone(), v.routine.name.clone())).collect();
    }

    pub fn send(&self, event: Event) {
        let _ = self.tx.send(event);
        if let Some(ctx) = &*self.ctx.lock().unwrap_or_else(PoisonError::into_inner) {
            ctx.request_repaint();
        }
    }
}

/// Whether any routine is on and scheduled (the engine's state, as the window has it).
fn routines_on(ui: &ByobApp) -> bool {
    ui.routines_active()
}

/// A routine event as a notification: its title and body. A report is "<routine>: <what it
/// found>"; waiting and failing say so plainly (the engine's titles do), with a short body.
pub fn note_for(kind: RoutineEventKind, name: &str, title: &str, body: &str) -> (String, String) {
    let short = |s: &str, max: usize| {
        let line = s.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.chars().count() <= max { line } else { format!("{}…", line.chars().take(max.saturating_sub(1)).collect::<String>()) }
    };
    match kind {
        RoutineEventKind::Waiting if title.is_empty() => (format!("{name} is waiting for you"), short(body, 200)),
        RoutineEventKind::Waiting | RoutineEventKind::Failed => (short(title, 120), short(body, 200)),
        _ if title.starts_with(name) => (short(title, 120), short(body, 300)),
        _ => (short(&format!("{name}: {title}"), 120), short(body, 300)),
    }
}

/// Shows notifications (the desktop's; tests record them).
pub trait Notify {
    fn notify(&mut self, title: &str, body: &str, action: Option<&str>);
}

impl Notify for notify::Notifier {
    fn notify(&mut self, title: &str, body: &str, action: Option<&str>) {
        notify::Notifier::notify(self, title, body, action);
    }
}

/// The background's own engine connection: routine events become notifications and the
/// tray's menu follows the routines, whether or not a window is open.
#[derive(Default)]
struct EngineLink {
    conn: Option<Box<dyn Connection>>,
    routines: Vec<RoutineView>,
    /// The engine's Pause routines.
    paused: bool,
}

/// What a close of the window does now: hide it (byob goes on) or quit.
pub fn hides_on_close(keep: KeepRunning, routines_on: bool) -> bool {
    keep.hides_on_close(routines_on)
}

/// Which "byob is still running" notifications were shown already (kept with the window's state).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Told {
    /// The one that says it's in the tray.
    #[serde(default)]
    pub tray: bool,
    /// The one that says how to get the window back without a tray.
    #[serde(default)]
    pub no_tray: bool,
}

impl Told {
    pub const STORAGE_KEY: &'static str = "byob.background.told";
}

const TITLE: &str = "byob is still running";
const IN_TRAY: &str = "It keeps working in the background. Open it again from its icon in the tray, or quit it there.";
const NO_TRAY: &str = "It keeps working in the background. To get its window back, open byob again. To quit, press Ctrl+Q in its window.";

/// The window's side of running in the background (see the module docs). Outlives each
/// window: where windows can't hide, byob goes on between them.
pub struct Background {
    bus: Bus,
    rx: Receiver<Event>,
    /// Off for scripted runs: closing quits, no tray, no notifications.
    enabled: bool,
    /// `--background`: hide the window at the first frame (if closing would hide it too).
    start_hidden: bool,
    /// This window can be hidden (not on Wayland): else it's closed and byob waits for a Show.
    can_hide: bool,
    hidden: bool,
    /// The window is closing to the background (where it can't hide): let the close through.
    closing_to_background: bool,
    /// Quit byob was chosen: let the close through.
    quitting: bool,
    tray: Option<tray::Tray>,
    /// A tray host shows the icon.
    tray_up: bool,
    /// The tray couldn't be made; not tried again until closing stops hiding.
    tray_failed: bool,
    notifier: Box<dyn Notify>,
    engine: EngineLink,
    /// A clicked notification's chat, to open in the next window's logic.
    open_chat: Option<String>,
    told: Told,
}

/// What the window has to do about an event.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    Show,
    Quit,
}

impl Background {
    pub fn new(bus: Bus, rx: Receiver<Event>, enabled: bool, start_hidden: bool) -> Self {
        let notifier = Box::new(notify::Notifier::new(bus.clone()));
        Self {
            bus,
            rx,
            enabled,
            start_hidden,
            can_hide: true,
            hidden: false,
            closing_to_background: false,
            quitting: false,
            tray: None,
            tray_up: false,
            tray_failed: false,
            notifier,
            engine: EngineLink::default(),
            open_chat: None,
            told: Told::default(),
        }
    }

    /// Give the background a connection of its own to the engine (`engine.connect` with
    /// [`Bus::engine_waker`]): it says hello, wanting only routines, and views no chat.
    pub fn connect(&mut self, mut conn: Box<dyn Connection>) {
        if !self.enabled {
            return;
        }
        let ClientMsg::Hello { protocol, minor, .. } = ClientMsg::hello("background") else { return };
        conn.send(ClientMsg::Hello { protocol, minor, client: "background".into(), caps: vec![byob_protocol::routines::CAP_ROUTINES.to_owned()] });
        conn.send(ClientMsg::NewChat { workspace: None });
        self.engine.conn = Some(conn);
        self.drain();
    }

    /// Take what the engine sent the background's connection.
    fn drain(&mut self) {
        let mut notes = Vec::new();
        let Some(conn) = &mut self.engine.conn else { return };
        while let Some(msg) = conn.try_recv() {
            match msg {
                ServerMsg::State(s) => self.engine.paused = s.routines_paused.unwrap_or(false),
                ServerMsg::Routines { items } => {
                    self.bus.set_routines(&items);
                    self.engine.routines = items;
                }
                ServerMsg::RoutineEvent { name, chat, kind, title, body, .. } => {
                    let (title, body) = note_for(kind, &name, &title, &body);
                    notes.push((title, body, chat));
                }
                _ => {}
            }
        }
        for (title, body, chat) in notes {
            self.notify(&title, &body, chat.as_deref());
        }
    }

    fn tell_engine(&mut self, msg: ClientMsg) {
        match &mut self.engine.conn {
            Some(conn) => conn.send(msg),
            None => log::info!("no engine connection for {msg:?}"),
        }
    }

    /// A window opened: wake it for events; `can_hide`: it can be hidden (not on Wayland);
    /// `told`: the notifications its saved state says were shown.
    pub fn attach(&mut self, ctx: &egui::Context, can_hide: bool, told: Told) {
        self.bus.attach(ctx);
        if !can_hide {
            log::info!("this window can't be hidden (Wayland): closing it to the background closes it");
        }
        self.can_hide = can_hide;
        self.told = Told { tray: self.told.tray || told.tray, no_tray: self.told.no_tray || told.no_tray };
    }

    pub fn told(&self) -> Told {
        self.told
    }

    /// Show a notification; clicking it shows the window, and the app gets `action` back
    /// ([`Event::Clicked`]).
    pub fn notify(&mut self, title: &str, body: &str, action: Option<&str>) {
        if self.enabled {
            self.notifier.notify(title, body, action);
        }
    }

    fn menu(&self) -> tray::Menu {
        let routines = self.engine.routines.iter().map(|v| tray::Routine { id: v.routine.id.clone(), label: v.routine.name.clone() }).collect();
        let attention = self.engine.routines.iter().any(|v| v.last_run.as_ref().is_some_and(|r| r.status == RunStatus::Waiting));
        tray::Menu { routines, paused: self.engine.paused, attention }
    }

    /// Every frame, and while the window is hidden whenever something wakes it.
    pub fn logic(&mut self, ctx: &egui::Context, ui: &mut ByobApp) {
        if !self.enabled {
            return;
        }
        // The engine's setting (until its state arrives, closing quits).
        let keep = ui.keep_running();
        let hides = keep.is_some_and(|k| hides_on_close(k, routines_on(ui)));
        self.keep_tray(hides);
        if keep.is_some() && std::mem::take(&mut self.start_hidden) && hides {
            self.hide(ctx);
        }
        while let Ok(event) = self.rx.try_recv() {
            match self.apply(event) {
                Some(Action::Show) => self.show(ctx),
                Some(Action::Quit) => self.quit(ctx),
                None => {}
            }
        }
        // A notification about a run was clicked: its chat.
        if let Some(chat) = self.open_chat.take() {
            ui.open_chat(&chat);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::Q))) {
            self.quit(ctx);
        }
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting && !self.closing_to_background && hides {
            if self.can_hide {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            }
            self.hide(ctx);
            self.tell_once();
        }
        self.update_menu();
    }

    /// After the window closed: `true` if it closed to the background (where windows can't
    /// hide) and something asked for it again; `false`: quit.
    pub fn wait_to_reopen(&mut self) -> bool {
        self.bus.detach();
        if self.quitting || !std::mem::take(&mut self.closing_to_background) {
            return false;
        }
        log::info!("window closed; byob keeps running until it's opened again or quit");
        while let Ok(event) = self.rx.recv() {
            match self.apply(event) {
                Some(Action::Show) => {
                    self.hidden = false;
                    return true;
                }
                Some(Action::Quit) => return false,
                None => self.update_menu(),
            }
        }
        false
    }

    fn update_menu(&mut self) {
        let menu = self.menu();
        if let Some(t) = &mut self.tray {
            t.set_menu(menu);
        }
    }

    /// The tray is up while closing hides the window.
    fn keep_tray(&mut self, hides: bool) {
        if !hides {
            if self.tray.take().is_some() {
                log::info!("closing quits byob again: tray icon down");
            }
            (self.tray_up, self.tray_failed) = (false, false);
            return;
        }
        if self.tray.is_none() && !self.tray_failed {
            match tray::Tray::start(&self.bus, self.menu()) {
                Ok(t) => self.tray = Some(t),
                Err(e) => {
                    log::info!("no tray icon: {e}");
                    self.tray_failed = true;
                }
            }
        }
    }

    /// Take in an event; what the window should do about it.
    fn apply(&mut self, event: Event) -> Option<Action> {
        match event {
            Event::Show => Some(Action::Show),
            Event::Clicked(action) => {
                // A run's notification: open its chat.
                if !action.is_empty() {
                    self.open_chat = Some(action);
                }
                Some(Action::Show)
            }
            Event::Quit => Some(Action::Quit),
            Event::TogglePause => {
                let paused = !self.engine.paused;
                self.engine.paused = paused;
                self.tell_engine(ClientMsg::PauseRoutines { paused });
                None
            }
            Event::Run(id) => {
                self.tell_engine(ClientMsg::RunRoutine { id });
                None
            }
            Event::Engine => {
                self.drain();
                None
            }
            Event::TrayHost(up) => {
                self.tray_up = up;
                None
            }
        }
    }

    fn quit(&mut self, ctx: &egui::Context) {
        self.quitting = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        ctx.request_repaint();
    }

    fn hide(&mut self, ctx: &egui::Context) {
        self.hidden = true;
        if self.can_hide {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            log::info!("window hidden; byob keeps running");
        } else {
            // Closed instead; [`Self::wait_to_reopen`] takes over.
            self.closing_to_background = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            ctx.request_repaint();
        }
    }

    fn show(&mut self, ctx: &egui::Context) {
        self.hidden = false;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        ctx.request_repaint();
    }

    /// The first time the window hides (with a tray, and without one), say where byob went.
    fn tell_once(&mut self) {
        let (seen, body) = if self.tray_up { (&mut self.told.tray, IN_TRAY) } else { (&mut self.told.no_tray, NO_TRAY) };
        if !std::mem::replace(seen, true) {
            self.notifier.notify(TITLE, body, None);
        }
    }

    #[cfg(test)]
    fn is_hidden(&self) -> bool {
        self.hidden
    }
}

/// What the settings screen asks of this computer: the login entry (its presence is the
/// setting). Keep running in the background is the engine's (`ClientMsg::SetKeepRunning`).
pub struct DesktopHost {
    login: Option<bool>,
}

impl DesktopHost {
    pub fn new() -> Self {
        Self { login: autostart::enabled() }
    }
}

impl byob_ui_egui::desktop::Desktop for DesktopHost {
    fn start_at_login(&self) -> Option<bool> {
        self.login
    }

    fn set_start_at_login(&mut self, on: bool) -> Result<(), String> {
        let done = autostart::set(on);
        self.login = autostart::enabled();
        done
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_hides_by_the_setting_and_routines() {
        assert!(!hides_on_close(KeepRunning::default(), false));
        assert!(hides_on_close(KeepRunning::WhenRoutines, true));
        assert!(hides_on_close(KeepRunning::Always, false));
        assert!(!hides_on_close(KeepRunning::Never, true));
    }

    /// A connection whose messages tests queue and whose sends they read.
    #[derive(Clone, Default)]
    struct Fake {
        inbox: Arc<Mutex<Vec<ServerMsg>>>,
        sent: Arc<Mutex<Vec<ClientMsg>>>,
    }

    impl Connection for Fake {
        fn send(&mut self, msg: ClientMsg) {
            self.sent.lock().unwrap().push(msg);
        }
        fn try_recv(&mut self) -> Option<ServerMsg> {
            let mut q = self.inbox.lock().unwrap();
            (!q.is_empty()).then(|| q.remove(0))
        }
    }

    /// A window whose engine says `keep`.
    fn app(keep: KeepRunning) -> ByobApp {
        app_on(keep, false, Fake::default())
    }

    /// A window whose engine says `keep` and whether routines are on, on `fake`.
    fn app_on(keep: KeepRunning, routines_active: bool, fake: Fake) -> ByobApp {
        let state = byob_protocol::EngineState { keep_running: Some(keep), routines_active, onboarded: true, ..Default::default() };
        fake.inbox.lock().unwrap().push(ServerMsg::State(state));
        let mut ui = ByobApp::new(Box::new(fake), "test");
        ui.set_desktop(Box::new(DesktopHost { login: None }));
        ui.logic(&egui::Context::default());
        assert_eq!(ui.keep_running(), Some(keep));
        ui
    }

    /// One logic pass, with a close request from the window manager if `close`.
    fn pass(ctx: &egui::Context, bg: &mut Background, ui: &mut ByobApp, close: bool) -> Vec<egui::ViewportCommand> {
        let mut input = egui::RawInput::default();
        if close {
            input.viewports.entry(egui::ViewportId::ROOT).or_default().events.push(egui::ViewportEvent::Close);
        }
        let out = ctx.run_logic(&input, |ctx| bg.logic(ctx, ui));
        out.viewport_commands.get(&egui::ViewportId::ROOT).cloned().unwrap_or_default()
    }

    #[test]
    fn with_always_a_close_hides_and_quit_lets_it_through() {
        let ctx = egui::Context::default();
        let (bus, rx) = Bus::new();
        // Enabled, but a tray here would go on the test machine's session bus: say it failed.
        let mut bg = Background::new(bus.clone(), rx, true, false);
        bg.told = Told { tray: true, no_tray: true };
        bg.tray_failed = true;
        let mut ui = app(KeepRunning::Always);
        let cmds = pass(&ctx, &mut bg, &mut ui, true);
        assert!(cmds.contains(&egui::ViewportCommand::CancelClose) && cmds.contains(&egui::ViewportCommand::Visible(false)), "{cmds:?}");
        assert!(bg.is_hidden());
        bus.send(Event::Show);
        let cmds = pass(&ctx, &mut bg, &mut ui, false);
        assert!(cmds.contains(&egui::ViewportCommand::Visible(true)) && cmds.contains(&egui::ViewportCommand::Focus), "{cmds:?}");
        assert!(!bg.is_hidden());
        bus.send(Event::Quit);
        let cmds = pass(&ctx, &mut bg, &mut ui, false);
        assert!(cmds.contains(&egui::ViewportCommand::Close), "{cmds:?}");
        let cmds = pass(&ctx, &mut bg, &mut ui, true);
        assert!(!cmds.contains(&egui::ViewportCommand::CancelClose), "quitting: {cmds:?}");
    }

    #[test]
    fn otherwise_a_close_quits() {
        let ctx = egui::Context::default();
        for (keep, enabled) in [(KeepRunning::Never, true), (KeepRunning::WhenRoutines, true), (KeepRunning::Always, false)] {
            let (bus, rx) = Bus::new();
            let mut bg = Background::new(bus, rx, enabled, true);
            let cmds = pass(&ctx, &mut bg, &mut app(keep), true);
            assert!(cmds.is_empty(), "{keep:?} enabled {enabled}: {cmds:?}");
            assert!(!bg.is_hidden() && bg.tray.is_none());
        }
    }

    #[test]
    fn where_windows_cant_hide_a_close_goes_through_and_byob_waits_for_show() {
        let ctx = egui::Context::default();
        let (bus, rx) = Bus::new();
        let mut bg = Background::new(bus.clone(), rx, true, false);
        bg.attach(&ctx, false, Told { tray: true, no_tray: true });
        bg.tray_failed = true;
        let mut ui = app(KeepRunning::Always);
        let cmds = pass(&ctx, &mut bg, &mut ui, true);
        assert!(!cmds.contains(&egui::ViewportCommand::CancelClose), "{cmds:?}");
        // The window is gone; a Show (from another thread) opens a new one.
        let sender = bus.clone();
        let t = std::thread::spawn(move || {
            sender.send(Event::TogglePause);
            sender.send(Event::Show);
        });
        assert!(bg.wait_to_reopen());
        t.join().unwrap();
        assert!(bg.engine.paused, "events between windows still count");
        // Quit while closed: no new window.
        bg.attach(&ctx, false, Told::default());
        pass(&ctx, &mut bg, &mut ui, true);
        bus.send(Event::Quit);
        assert!(!bg.wait_to_reopen());
        // A close that quits (Never) doesn't wait.
        let (bus, rx) = Bus::new();
        let mut never = Background::new(bus, rx, true, false);
        never.attach(&ctx, false, Told::default());
        pass(&ctx, &mut never, &mut app(KeepRunning::Never), true);
        assert!(!never.wait_to_reopen());
    }

    #[test]
    fn starting_in_the_background_hides_the_window_once() {
        let ctx = egui::Context::default();
        let (bus, rx) = Bus::new();
        let mut bg = Background::new(bus, rx, true, true);
        bg.tray_failed = true;
        let mut ui = app(KeepRunning::Always);
        assert!(pass(&ctx, &mut bg, &mut ui, false).contains(&egui::ViewportCommand::Visible(false)));
        assert!(pass(&ctx, &mut bg, &mut ui, false).is_empty());
        assert_eq!(bg.told(), Told::default(), "starting hidden isn't news");
    }

    /// Notifications a test can read.
    #[derive(Clone, Default)]
    struct Shown(Arc<Mutex<Vec<(String, String, Option<String>)>>>);

    impl Notify for Shown {
        fn notify(&mut self, title: &str, body: &str, action: Option<&str>) {
            self.0.lock().unwrap().push((title.into(), body.into(), action.map(str::to_owned)));
        }
    }

    fn loom(waiting: bool) -> RoutineView {
        let run = byob_protocol::routines::RunRecord { status: if waiting { RunStatus::Waiting } else { RunStatus::Quiet }, ..Default::default() };
        RoutineView {
            routine: byob_protocol::routines::Routine { id: "loom".into(), name: "Loom deals".into(), instructions: "x".into(), ..Default::default() },
            last_run: Some(run),
            ..Default::default()
        }
    }

    #[test]
    fn routine_events_read_plainly_as_notifications() {
        let n = |kind, title: &str, body: &str| note_for(kind, "Loom deals", title, body);
        assert_eq!(
            n(RoutineEventKind::Reported, "Schacht Wolf Pup 8S, 20% off", "$1,040 at The Woolery"),
            ("Loom deals: Schacht Wolf Pup 8S, 20% off".into(), "$1,040 at The Woolery".into())
        );
        assert_eq!(n(RoutineEventKind::Reported, "Loom deals: nothing new", "").0, "Loom deals: nothing new", "not named twice");
        assert_eq!(n(RoutineEventKind::Waiting, "Loom deals is waiting for you", "It wants to: send an email").0, "Loom deals is waiting for you");
        let (title, body) = n(RoutineEventKind::Failed, "Loom deals failed", &"the model server didn't answer ".repeat(20));
        assert_eq!(title, "Loom deals failed");
        assert!(body.chars().count() <= 200 && body.ends_with('…'), "a short error: {body}");
    }

    /// "When routines are on" follows the engine: closing hides only while one is on.
    #[test]
    fn closing_hides_while_the_engine_says_routines_are_on() {
        let ctx = egui::Context::default();
        for (on, hides) in [(true, true), (false, false)] {
            let (bus, rx) = Bus::new();
            let mut bg = Background::new(bus, rx, true, false);
            bg.told = Told { tray: true, no_tray: true };
            bg.tray_failed = true;
            let mut ui = app_on(KeepRunning::WhenRoutines, on, Fake::default());
            let cmds = pass(&ctx, &mut bg, &mut ui, true);
            assert_eq!(cmds.contains(&egui::ViewportCommand::Visible(false)), hides, "routines on: {on}: {cmds:?}");
        }
    }

    /// The background's own connection: it asks only for routines and views no chat; the
    /// tray lists the routines and calls out one waiting for you; events are notifications
    /// whose click opens the run's chat in the window; Run and Pause go to the engine; a
    /// second launch's `run <name>` finds the routine by name.
    #[test]
    fn the_background_link_feeds_the_tray_and_notifications() {
        let ctx = egui::Context::default();
        let (bus, rx) = Bus::new();
        let mut bg = Background::new(bus.clone(), rx, true, false);
        bg.tray_failed = true;
        let shown = Shown::default();
        bg.notifier = Box::new(shown.clone());
        let engine = Fake::default();
        bg.connect(Box::new(engine.clone()));
        let hello = engine.sent.lock().unwrap().clone();
        assert!(matches!(&hello[..], [ClientMsg::Hello { caps, .. }, ClientMsg::NewChat { workspace: None }] if caps == &["routines".to_owned()]), "{hello:?}");
        engine.inbox.lock().unwrap().extend([
            ServerMsg::Routines { items: vec![loom(true)] },
            ServerMsg::RoutineEvent {
                routine: "loom".into(),
                name: "Loom deals".into(),
                chat: Some("run-1".into()),
                kind: RoutineEventKind::Reported,
                title: "Schacht Wolf Pup 8S, 20% off".into(),
                body: "Until Sunday.".into(),
            },
        ]);
        bus.send(Event::Engine);
        let window = Fake::default();
        let mut ui = app_on(KeepRunning::Always, true, window.clone());
        pass(&ctx, &mut bg, &mut ui, false);
        let menu = bg.menu();
        assert_eq!(menu.routines, vec![tray::Routine { id: "loom".into(), label: "Loom deals".into() }]);
        assert!(menu.attention, "a run waits for you");
        assert_eq!(
            *shown.0.lock().unwrap(),
            vec![("Loom deals: Schacht Wolf Pup 8S, 20% off".to_owned(), "Until Sunday.".to_owned(), Some("run-1".to_owned()))]
        );
        // Its click opens the run's chat.
        window.sent.lock().unwrap().clear();
        bus.send(Event::Clicked("run-1".into()));
        let cmds = pass(&ctx, &mut bg, &mut ui, false);
        assert!(cmds.contains(&egui::ViewportCommand::Visible(true)));
        assert!(window.sent.lock().unwrap().contains(&ClientMsg::OpenChat { id: "run-1".into() }));
        // The tray's Run and Pause.
        engine.sent.lock().unwrap().clear();
        bus.send(Event::Run("loom".into()));
        bus.send(Event::TogglePause);
        pass(&ctx, &mut bg, &mut ui, false);
        assert_eq!(*engine.sent.lock().unwrap(), vec![ClientMsg::RunRoutine { id: "loom".into() }, ClientMsg::PauseRoutines { paused: true }]);
        assert!(bg.menu().paused);
        // Another launch's `run loom deals`.
        assert_eq!(bus.find_routine("loom deals"), Some("loom".into()));
        assert_eq!(bus.find_routine("loom"), Some("loom".into()));
        assert_eq!(bus.find_routine("weather"), None);
    }
}
