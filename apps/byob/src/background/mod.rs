//! Running in the background: closing the window can hide it instead of quitting (the
//! engine, the browser server and routines go on), a tray icon brings it back, one byob per
//! data folder, notifications, and starting at login.
//!
//! Everything that happens outside the window (the tray, the instance listener, notification
//! clicks) arrives as an [`Event`] on the [`Bus`], which wakes the window so `App::logic` (run
//! even while the window is hidden) handles it on the main thread.

pub mod autostart;
pub mod instance;
pub mod notify;
pub mod tray;

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, OnceLock};

use byob_store::{KeepRunning, Settings};
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
    ctx: Arc<OnceLock<egui::Context>>,
}

impl Bus {
    pub fn new() -> (Self, Receiver<Event>) {
        let (tx, rx) = std::sync::mpsc::channel();
        (Self { tx, ctx: Arc::default() }, rx)
    }

    /// Wake this window for every event from now on (events before it wait for its first frame).
    pub fn attach(&self, ctx: &egui::Context) {
        let _ = self.ctx.set(ctx.clone());
        ctx.request_repaint();
    }

    pub fn send(&self, event: Event) {
        let _ = self.tx.send(event);
        if let Some(ctx) = self.ctx.get() {
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

/// The window's side of running in the background (see the module docs).
pub struct Background {
    bus: Bus,
    rx: Receiver<Event>,
    /// Off for scripted runs: closing quits, no tray, no notifications.
    enabled: bool,
    /// `--background`: hide the window at the first frame (if closing would hide it too).
    start_hidden: bool,
    hidden: bool,
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

impl Background {
    pub fn new(bus: Bus, rx: Receiver<Event>, enabled: bool, start_hidden: bool, told: Told) -> Self {
        let notifier = notify::Notifier::new(bus.clone());
        Self { bus, rx, enabled, start_hidden, hidden: false, quitting: false, tray: None, tray_up: false, tray_failed: false, notifier, paused: false, told }
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
        let keep = ui.desktop().map_or(KeepRunning::Never, |d| d.keep_running());
        let hides = hides_on_close(keep, routines_on(ui));
        self.keep_tray(hides);
        if std::mem::take(&mut self.start_hidden) && hides {
            self.hide(ctx);
        }
        while let Ok(event) = self.rx.try_recv() {
            self.handle(ctx, event);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::Q))) {
            self.handle(ctx, Event::Quit);
        }
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting && hides {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.hide(ctx);
            self.tell_once();
        }
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

    fn handle(&mut self, ctx: &egui::Context, event: Event) {
        match event {
            Event::Show => self.show(ctx),
            Event::Clicked(action) => {
                self.show(ctx);
                if !action.is_empty() {
                    // Routines: open the run's chat.
                    log::info!("notification {action:?} clicked");
                }
            }
            Event::Quit => {
                self.quitting = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                ctx.request_repaint();
            }
            // STUB: routines pause here.
            Event::TogglePause => self.paused = !self.paused,
            Event::Run(id) => log::info!("run {id:?}: there are no routines yet"),
            Event::TrayHost(up) => self.tray_up = up,
        }
    }

    fn hide(&mut self, ctx: &egui::Context) {
        self.hidden = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        log::info!("window hidden; byob keeps running");
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

/// The desktop's own settings for the settings screen: `keep_running` in settings.json, and
/// the login entry (its presence is the setting).
pub struct DesktopHost {
    settings: Option<PathBuf>,
    keep: KeepRunning,
    login: Option<bool>,
}

impl DesktopHost {
    pub fn new(settings: Option<PathBuf>, keep: KeepRunning) -> Self {
        Self { settings, keep, login: autostart::enabled() }
    }
}

impl byob_ui_egui::desktop::Desktop for DesktopHost {
    fn keep_running(&self) -> KeepRunning {
        self.keep
    }

    fn set_keep_running(&mut self, value: KeepRunning) -> Result<(), String> {
        self.keep = value;
        let path = self.settings.as_ref().ok_or("there's no settings file this run")?;
        Settings::update(path, |s| s.keep_running = value).map(drop).map_err(|e| e.to_string())
    }

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

    fn app(keep: KeepRunning) -> ByobApp {
        struct Null;
        impl byob_protocol::Connection for Null {
            fn send(&mut self, _: byob_protocol::ClientMsg) {}
            fn try_recv(&mut self) -> Option<byob_protocol::ServerMsg> {
                None
            }
        }
        let mut ui = ByobApp::new(Box::new(Null), "test");
        ui.set_desktop(Box::new(DesktopHost { settings: None, keep, login: None }));
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
        let mut bg = Background::new(bus.clone(), rx, true, false, Told { tray: true, no_tray: true });
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
            let mut bg = Background::new(bus, rx, enabled, true, Told::default());
            let cmds = pass(&ctx, &mut bg, &app(keep), true);
            assert!(cmds.is_empty(), "{keep:?} enabled {enabled}: {cmds:?}");
            assert!(!bg.is_hidden() && bg.tray.is_none());
        }
    }

    #[test]
    fn starting_in_the_background_hides_the_window_once() {
        let ctx = egui::Context::default();
        let (bus, rx) = Bus::new();
        let mut bg = Background::new(bus, rx, true, true, Told::default());
        bg.tray_failed = true;
        let ui = app(KeepRunning::Always);
        assert!(pass(&ctx, &mut bg, &ui, false).contains(&egui::ViewportCommand::Visible(false)));
        assert!(pass(&ctx, &mut bg, &ui, false).is_empty());
        assert_eq!(bg.told(), Told::default(), "starting hidden isn't news");
    }
}
