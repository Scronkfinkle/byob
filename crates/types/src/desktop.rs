//! Settings of the desktop app itself (not of chats): what closing the window does.

use serde::{Deserialize, Serialize};

/// Whether closing the desktop window keeps byob running in the background (`settings.json`
/// `keep_running`). The window hides; the engine, the browser server and routines go on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum KeepRunning {
    /// Only while a routine is on (nothing else needs byob when its window is closed).
    #[serde(rename = "when-routines-are-on", alias = "routines")]
    WhenRoutines,
    /// Closing hides to the tray, as chat and game launchers do.
    #[default]
    #[serde(rename = "always")]
    Always,
    #[serde(rename = "never")]
    Never,
}

impl KeepRunning {
    pub const ALL: [KeepRunning; 3] = [KeepRunning::Always, KeepRunning::WhenRoutines, KeepRunning::Never];

    pub const fn label(self) -> &'static str {
        match self {
            KeepRunning::WhenRoutines => "When routines are on",
            KeepRunning::Always => "Always",
            KeepRunning::Never => "Never",
        }
    }

    /// Closing the window hides it (instead of quitting), given whether any routine is on.
    pub const fn hides_on_close(self, routines_on: bool) -> bool {
        match self {
            KeepRunning::WhenRoutines => routines_on,
            KeepRunning::Always => true,
            KeepRunning::Never => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_hides_only_when_the_setting_says_so() {
        assert!(!KeepRunning::WhenRoutines.hides_on_close(false));
        assert!(KeepRunning::WhenRoutines.hides_on_close(true));
        assert!(KeepRunning::Always.hides_on_close(false));
        assert!(!KeepRunning::Never.hides_on_close(true));
    }

    #[test]
    fn stored_as_kebab_case_names() {
        let names: Vec<String> = KeepRunning::ALL.iter().map(|k| serde_json::to_string(k).unwrap()).collect();
        assert_eq!(names, ["\"always\"", "\"when-routines-are-on\"", "\"never\""]);
        assert_eq!(serde_json::from_str::<KeepRunning>("\"routines\"").unwrap(), KeepRunning::WhenRoutines);
        assert_eq!(KeepRunning::default(), KeepRunning::Always, "closing hides to the tray, as chat apps and game launchers do");
    }
}
