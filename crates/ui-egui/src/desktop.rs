//! What only the desktop app can do on its computer, for the settings that only make sense
//! there (start at login). The app plugs one in with
//! [`ByobApp::set_desktop`](crate::ByobApp::set_desktop); browsers have none, so those
//! settings aren't shown there. Settings the engine keeps (keep running in the background)
//! go through it like any other, not through this.

pub use byob_types::desktop::KeepRunning;

pub trait Desktop {
    /// Whether byob starts (hidden) when you log in; `None`: this system can't.
    fn start_at_login(&self) -> Option<bool>;
    fn set_start_at_login(&mut self, on: bool) -> Result<(), String>;
}
