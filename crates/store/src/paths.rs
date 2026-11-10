//! Where byob keeps its files.
//!
//! By default the platform's config and data directories for the app `byob`
//! (e.g. `~/.config/byob` and `~/.local/share/byob` on Linux). Setting `BYOB_HOME` — or
//! calling [`Paths::at`] — puts everything under one folder instead, which is what tests,
//! portable installs and side-by-side profiles use.

use std::path::{Path, PathBuf};

use crate::error::{Result, StoreError};

/// Environment variable that overrides every byob directory with one root folder.
/// The database was `yah.db` before the rename: rename it (with its WAL and shared-memory
/// files) to `byob.db` unless a `byob.db` already exists.
pub fn migrate_db_name(data_dir: &Path) {
    if !data_dir.join("yah.db").is_file() || data_dir.join("byob.db").exists() {
        return;
    }
    for suffix in ["", "-wal", "-shm"] {
        let (from, to) = (data_dir.join(format!("yah.db{suffix}")), data_dir.join(format!("byob.db{suffix}")));
        if from.exists()
            && let Err(e) = std::fs::rename(&from, &to)
        {
            log::warn!("couldn't rename {} to {}: {e}", from.display(), to.display());
        }
    }
    log::info!("renamed yah.db to byob.db in {}", data_dir.display());
}

/// A workspace's own folder (artifacts, trash, undo backups). Before the rename it was
/// `.yah`: rename it the first time byob opens that workspace.
pub fn migrate_workspace(workspace: &Path) {
    let (old, new) = (workspace.join(".yah"), workspace.join(".byob"));
    if old.is_dir() && !new.exists() {
        match std::fs::rename(&old, &new) {
            Ok(()) => log::info!("renamed {} to .byob", old.display()),
            Err(e) => log::warn!("couldn't rename {}: {e}", old.display()),
        }
    }
}

pub const HOME_ENV: &str = "BYOB_HOME";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// Small, user-editable files: `settings.json`, `secrets.json`.
    pub config_dir: PathBuf,
    /// Everything else: the database, undo backups.
    pub data_dir: PathBuf,
}

impl Paths {
    /// `BYOB_HOME` if set (and non-empty), otherwise the platform directories.
    pub fn resolve() -> Result<Self> {
        match std::env::var_os(HOME_ENV) {
            Some(home) if !home.is_empty() => Ok(Self::at(home)),
            _ => Self::platform(),
        }
    }

    /// The platform's per-user directories for `byob`, ignoring `BYOB_HOME`.
    pub fn platform() -> Result<Self> {
        let dirs = directories::ProjectDirs::from("", "", "byob").ok_or(StoreError::NoHome)?;
        Ok(Self { config_dir: dirs.config_dir().to_owned(), data_dir: dirs.data_dir().to_owned() })
    }

    /// Everything under one folder.
    pub fn at(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref().to_owned();
        Self { config_dir: root.clone(), data_dir: root }
    }

    pub fn db(&self) -> PathBuf {
        self.data_dir.join("byob.db")
    }

    pub fn settings(&self) -> PathBuf {
        self.config_dir.join("settings.json")
    }

    pub fn secrets(&self) -> PathBuf {
        self.config_dir.join("secrets.json")
    }

    /// Workspaces: names, folders and their settings.
    pub fn workspaces(&self) -> PathBuf {
        self.config_dir.join("workspaces.json")
    }

    /// Each chat's own settings.
    pub fn chat_settings(&self) -> PathBuf {
        self.data_dir.join("chat-settings.json")
    }

    /// Default root of the folder chat backend (`storage = "folder"`).
    pub fn chats_folder(&self) -> PathBuf {
        self.data_dir.join("chats-folder")
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
    /// Before the rename to byob, the platform folders were named `yah`. If byob's don't
    /// exist yet but yah's do, move them over (once). Explicit `BYOB_HOME` roots are left alone.
    pub fn migrate_from_yah() {
        let (Some(new), Some(old)) = (directories::ProjectDirs::from("", "", "byob"), directories::ProjectDirs::from("", "", "yah")) else { return };
        let pairs = [(old.config_dir().to_owned(), new.config_dir().to_owned()), (old.data_dir().to_owned(), new.data_dir().to_owned())];
        for (from, to) in pairs {
            if from == to || to.exists() || !from.is_dir() {
                continue;
            }
            if let Some(parent) = to.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::rename(&from, &to) {
                Ok(()) => log::info!("moved {} to {} (yah is now byob)", from.display(), to.display()),
                Err(e) => log::warn!("couldn't move {} to {}: {e}", from.display(), to.display()),
            }
        }
        migrate_db_name(new.data_dir());
    }

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
    fn old_database_name_is_migrated_with_its_wal() {
        let dir = tempfile::tempdir().unwrap();
        for f in ["yah.db", "yah.db-wal", "yah.db-shm"] {
            std::fs::write(dir.path().join(f), f).unwrap();
        }
        migrate_db_name(dir.path());
        assert_eq!(std::fs::read_to_string(dir.path().join("byob.db-wal")).unwrap(), "yah.db-wal");
        assert!(!dir.path().join("yah.db").exists());
        // An existing byob.db is never overwritten.
        std::fs::write(dir.path().join("yah.db"), "old").unwrap();
        migrate_db_name(dir.path());
        assert_eq!(std::fs::read_to_string(dir.path().join("byob.db")).unwrap(), "yah.db");
    }

    #[test]
    fn at_puts_everything_under_root() {
        let p = Paths::at("/tmp/byob-x");
        assert_eq!(p.db(), Path::new("/tmp/byob-x/byob.db"));
        assert_eq!(p.settings(), Path::new("/tmp/byob-x/settings.json"));
        assert_eq!(p.secrets(), Path::new("/tmp/byob-x/secrets.json"));
        assert_eq!(p.undo_dir(), Path::new("/tmp/byob-x/undo"));
        assert_eq!(p.chats_folder(), Path::new("/tmp/byob-x/chats-folder"));
        assert_eq!(p.themes_dir(), Path::new("/tmp/byob-x/themes"));
    }

    #[test]
    fn platform_dirs_are_named_yah() {
        // CI machines normally have a HOME; skip quietly when they don't.
        if let Ok(p) = Paths::platform() {
            assert!(p.config_dir.to_string_lossy().contains("byob"));
            assert!(p.data_dir.to_string_lossy().contains("byob"));
        }
    }
}
