//! What the desktop app does on its computer, for the settings that only make sense there
//! (keep running in the background, start at login). The app plugs one in with
//! [`ByobApp::set_desktop`](crate::ByobApp::set_desktop); browsers have none, so those
//! settings aren't shown there.

pub use byob_types::desktop::KeepRunning;

pub trait Desktop {
    /// Whether closing the window keeps byob running (`settings.json` `keep_running`).
    fn keep_running(&self) -> KeepRunning;
    fn set_keep_running(&mut self, value: KeepRunning) -> Result<(), String>;
    /// Whether byob starts (hidden) when you log in; `None`: this system can't.
    fn start_at_login(&self) -> Option<bool>;
    fn set_start_at_login(&mut self, on: bool) -> Result<(), String>;
}
