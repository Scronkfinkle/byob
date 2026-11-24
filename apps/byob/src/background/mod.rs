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
}

/// Where [`Event`]s go; cheap to clone, usable from any thread.
#[derive(Clone)]
pub struct Bus {
    tx: Sender<Event>,
    /// The window to wake (none between windows).
    ctx: Arc<Mutex<Option<egui::Context>>>,
}

impl Bus {
    pub fn new() -> (Self, Receiver<Event>) {
        let (tx, rx) = std::sync::mpsc::channel();
        (Self { tx, ctx: Arc::default() }, rx)
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

    pub fn send(&self, event: Event) {
        let _ = self.tx.send(event);
        if let Some(ctx) = &*self.ctx.lock().unwrap_or_else(PoisonError::into_inner) {
            ctx.request_repaint();
        }
    }
}

/// Whether any routine is on.
///
/// STUB until routines exist: always off, so "When routines are on" behaves like "Never".
/// The routines work plugs in here (from the engine's state, through what `ui` knows of it).
fn routines_on(_ui: &ByobApp) -> bool {
    false
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
    notifier: notify::Notifier,
    /// STUB: routines aren't paused by anything yet.
    paused: bool,
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
        let notifier = notify::Notifier::new(bus.clone());
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
            paused: false,
            told: Told::default(),
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
    #[allow(dead_code)] // for routines (a run that finished or waits for you)
    pub fn notify(&mut self, title: &str, body: &str, action: Option<&str>) {
        if self.enabled {
            self.notifier.notify(title, body, action);
        }
    }

    fn menu(&self) -> tray::Menu {
        // Routines and attention come with routines.
        tray::Menu { routines: Vec::new(), paused: self.paused, attention: false }
    }

    /// Every frame, and while the window is hidden whenever something wakes it.
    pub fn logic(&mut self, ctx: &egui::Context, ui: &ByobApp) {
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
                if !action.is_empty() {
                    // Routines: open the run's chat.
                    log::info!("notification {action:?} clicked");
                }
                Some(Action::Show)
            }
            Event::Quit => Some(Action::Quit),
            // STUB: routines pause here.
            Event::TogglePause => {
                self.paused = !self.paused;
                None
            }
            Event::Run(id) => {
                log::info!("run {id:?}: there are no routines yet");
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
        // "When routines are on" is off until routines exist.
        assert!(!hides_on_close(KeepRunning::default(), false));
        assert!(hides_on_close(KeepRunning::WhenRoutines, true));
        assert!(hides_on_close(KeepRunning::Always, false));
        assert!(!hides_on_close(KeepRunning::Never, true));
    }

    /// A window whose engine says `keep`.
    fn app(keep: KeepRunning) -> ByobApp {
        struct Engine(Vec<byob_protocol::ServerMsg>);
        impl byob_protocol::Connection for Engine {
            fn send(&mut self, _: byob_protocol::ClientMsg) {}
            fn try_recv(&mut self) -> Option<byob_protocol::ServerMsg> {
                self.0.pop()
            }
        }
        let state = byob_protocol::EngineState { keep_running: Some(keep), onboarded: true, ..Default::default() };
        let mut ui = ByobApp::new(Box::new(Engine(vec![byob_protocol::ServerMsg::State(state)])), "test");
        ui.set_desktop(Box::new(DesktopHost { login: None }));
        ui.logic(&egui::Context::default());
        assert_eq!(ui.keep_running(), Some(keep));
        ui
    }

    /// One logic pass, with a close request from the window manager if `close`.
    fn pass(ctx: &egui::Context, bg: &mut Background, ui: &ByobApp, close: bool) -> Vec<egui::ViewportCommand> {
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
        let ui = app(KeepRunning::Always);
        let cmds = pass(&ctx, &mut bg, &ui, true);
        assert!(cmds.contains(&egui::ViewportCommand::CancelClose) && cmds.contains(&egui::ViewportCommand::Visible(false)), "{cmds:?}");
        assert!(bg.is_hidden());
        bus.send(Event::Show);
        let cmds = pass(&ctx, &mut bg, &ui, false);
        assert!(cmds.contains(&egui::ViewportCommand::Visible(true)) && cmds.contains(&egui::ViewportCommand::Focus), "{cmds:?}");
        assert!(!bg.is_hidden());
        bus.send(Event::Quit);
        let cmds = pass(&ctx, &mut bg, &ui, false);
        assert!(cmds.contains(&egui::ViewportCommand::Close), "{cmds:?}");
        let cmds = pass(&ctx, &mut bg, &ui, true);
        assert!(!cmds.contains(&egui::ViewportCommand::CancelClose), "quitting: {cmds:?}");
    }

    #[test]
    fn otherwise_a_close_quits() {
        let ctx = egui::Context::default();
        for (keep, enabled) in [(KeepRunning::Never, true), (KeepRunning::WhenRoutines, true), (KeepRunning::Always, false)] {
            let (bus, rx) = Bus::new();
            let mut bg = Background::new(bus, rx, enabled, true);
            let cmds = pass(&ctx, &mut bg, &app(keep), true);
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
        let ui = app(KeepRunning::Always);
        let cmds = pass(&ctx, &mut bg, &ui, true);
        assert!(!cmds.contains(&egui::ViewportCommand::CancelClose), "{cmds:?}");
        // The window is gone; a Show (from another thread) opens a new one.
        let sender = bus.clone();
        let t = std::thread::spawn(move || {
            sender.send(Event::TogglePause);
            sender.send(Event::Show);
        });
        assert!(bg.wait_to_reopen());
        t.join().unwrap();
        assert!(bg.paused, "events between windows still count");
        // Quit while closed: no new window.
        bg.attach(&ctx, false, Told::default());
        pass(&ctx, &mut bg, &ui, true);
        bus.send(Event::Quit);
        assert!(!bg.wait_to_reopen());
        // A close that quits (Never) doesn't wait.
        let (bus, rx) = Bus::new();
        let mut never = Background::new(bus, rx, true, false);
        never.attach(&ctx, false, Told::default());
        pass(&ctx, &mut never, &app(KeepRunning::Never), true);
        assert!(!never.wait_to_reopen());
    }

    #[test]
    fn starting_in_the_background_hides_the_window_once() {
        let ctx = egui::Context::default();
        let (bus, rx) = Bus::new();
        let mut bg = Background::new(bus, rx, true, true);
        bg.tray_failed = true;
        let ui = app(KeepRunning::Always);
        assert!(pass(&ctx, &mut bg, &ui, false).contains(&egui::ViewportCommand::Visible(false)));
        assert!(pass(&ctx, &mut bg, &ui, false).is_empty());
        assert_eq!(bg.told(), Told::default(), "starting hidden isn't news");
    }
}
