//! Where yah keeps its files.
//!
//! By default the platform's config and data directories for the app `yah`
//! (e.g. `~/.config/yah` and `~/.local/share/yah` on Linux). Setting `YAH_HOME` — or
//! calling [`Paths::at`] — puts everything under one folder instead, which is what tests,
//! portable installs and side-by-side profiles use.

use std::path::{Path, PathBuf};

use crate::error::{Result, StoreError};

/// Environment variable that overrides every yah directory with one root folder.
pub const HOME_ENV: &str = "YAH_HOME";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// Small, user-editable files: `settings.json`, `secrets.json`.
    pub config_dir: PathBuf,
    /// Everything else: the database, undo backups.
    pub data_dir: PathBuf,
}

impl Paths {
    /// `YAH_HOME` if set (and non-empty), otherwise the platform directories.
    pub fn resolve() -> Result<Self> {
        match std::env::var_os(HOME_ENV) {
            Some(home) if !home.is_empty() => Ok(Self::at(home)),
            _ => Self::platform(),
        }
    }

    /// The platform's per-user directories for `yah`, ignoring `YAH_HOME`.
    pub fn platform() -> Result<Self> {
        let dirs = directories::ProjectDirs::from("", "", "yah").ok_or(StoreError::NoHome)?;
        Ok(Self { config_dir: dirs.config_dir().to_owned(), data_dir: dirs.data_dir().to_owned() })
    }

    /// Everything under one folder.
    pub fn at(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref().to_owned();
        Self { config_dir: root.clone(), data_dir: root }
    }

    pub fn db(&self) -> PathBuf {
        self.data_dir.join("yah.db")
    }

    pub fn settings(&self) -> PathBuf {
        self.config_dir.join("settings.json")
    }

    pub fn secrets(&self) -> PathBuf {
        self.config_dir.join("secrets.json")
    }

    /// User theme files (`*.json`), listed in the appearance settings.
    pub fn themes_dir(&self) -> PathBuf {
        self.config_dir.join("themes")
    }

    /// Backups the undo journal points at, one folder per task.
    pub fn undo_dir(&self) -> PathBuf {
        self.data_dir.join("undo")
    }

    /// Create the config and data directories (and the undo folder).
    pub fn ensure(&self) -> Result<()> {
        for dir in [&self.config_dir, &self.data_dir, &self.undo_dir()] {
            std::fs::create_dir_all(dir).map_err(|e| StoreError::io(dir.clone(), e))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_puts_everything_under_root() {
        let p = Paths::at("/tmp/yah-x");
        assert_eq!(p.db(), Path::new("/tmp/yah-x/yah.db"));
        assert_eq!(p.settings(), Path::new("/tmp/yah-x/settings.json"));
        assert_eq!(p.secrets(), Path::new("/tmp/yah-x/secrets.json"));
        assert_eq!(p.undo_dir(), Path::new("/tmp/yah-x/undo"));
        assert_eq!(p.themes_dir(), Path::new("/tmp/yah-x/themes"));
    }

    #[test]
    fn platform_dirs_are_named_yah() {
        // CI machines normally have a HOME; skip quietly when they don't.
        if let Ok(p) = Paths::platform() {
            assert!(p.config_dir.to_string_lossy().contains("yah"));
            assert!(p.data_dir.to_string_lossy().contains("yah"));
        }
    }
}
