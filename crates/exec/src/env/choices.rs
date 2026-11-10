//! What the user decided per workspace: load its project environment or not, run its
//! commands in a container or not. A workspace is identified by its canonical path.
//!
//! Answers a person gave are saved in `<data>/envs/workspaces.json` (read on every lookup,
//! so another byob process or a hand edit is seen); answers the policy gave for them
//! (`--auto-approve`) only last for this process, so a test run never decides for good.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use super::write_atomic;

/// The decisions for one workspace (`None`: not decided, ask).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    /// Load the workspace's own environment (flake.nix, pixi.toml, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<bool>,
    /// Run its commands in a container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<bool>,
}

impl Choice {
    fn overlay(self, over: Choice) -> Choice {
        Choice { project: over.project.or(self.project), container: over.container.or(self.container) }
    }

    fn is_empty(self) -> bool {
        self == Choice::default()
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    workspaces: BTreeMap<PathBuf, Choice>,
}

/// Which part of a [`Choice`] to change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Project,
    Container,
}

/// The saved and the session-only decisions.
#[derive(Debug, Default)]
pub struct Choices {
    /// `None`: nothing is saved (no data folder).
    file: Option<PathBuf>,
    session: Mutex<BTreeMap<PathBuf, Choice>>,
    /// Serialises read-modify-write of the file.
    writing: Mutex<()>,
}

impl Choices {
    pub fn new(file: Option<PathBuf>) -> Self {
        Self { file, ..Self::default() }
    }

    fn load(&self) -> File {
        self.file.as_ref().and_then(|f| std::fs::read_to_string(f).ok()).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    /// The decisions for `ws`: this session's, then the saved ones.
    pub fn get(&self, ws: &Path) -> Choice {
        let saved = self.load().workspaces.get(ws).copied().unwrap_or_default();
        let session = self.session.lock().unwrap_or_else(PoisonError::into_inner).get(ws).copied().unwrap_or_default();
        saved.overlay(session)
    }

    /// Decide `field` for `ws` (`None`: forget). `save`: a person decided, keep it;
    /// otherwise it lasts for this process only. Forgetting clears both.
    pub fn set(&self, ws: &Path, field: Field, value: Option<bool>, save: bool) -> Result<(), String> {
        let put = |c: &mut Choice, v: Option<bool>| match field {
            Field::Project => c.project = v,
            Field::Container => c.container = v,
        };
        {
            let mut session = self.session.lock().unwrap_or_else(PoisonError::into_inner);
            let c = session.entry(ws.to_path_buf()).or_default();
            put(c, if save && self.file.is_some() { None } else { value });
            if c.is_empty() {
                session.remove(ws);
            }
        }
        if !save && value.is_some() {
            return Ok(());
        }
        let Some(path) = &self.file else { return Ok(()) };
        let _w = self.writing.lock().unwrap_or_else(PoisonError::into_inner);
        let mut f = self.load();
        let c = f.workspaces.entry(ws.to_path_buf()).or_default();
        put(c, value);
        if c.is_empty() {
            f.workspaces.remove(ws);
        }
        write_atomic(path, serde_json::to_string_pretty(&f).unwrap_or_default().as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_answers_persist_and_session_ones_do_not() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("envs/workspaces.json");
        let ws = Path::new("/home/me/proj");
        let c = Choices::new(Some(file.clone()));
        assert_eq!(c.get(ws), Choice::default());
        c.set(ws, Field::Project, Some(true), false).unwrap();
        assert_eq!(c.get(ws).project, Some(true));
        assert!(!file.exists(), "an automatic answer isn't saved");
        assert_eq!(Choices::new(Some(file.clone())).get(ws).project, None);

        c.set(ws, Field::Project, Some(false), true).unwrap();
        c.set(ws, Field::Container, Some(true), true).unwrap();
        let again = Choices::new(Some(file.clone()));
        assert_eq!(again.get(ws), Choice { project: Some(false), container: Some(true) });
        assert_eq!(again.get(Path::new("/other")), Choice::default());

        again.set(ws, Field::Project, None, true).unwrap();
        assert_eq!(Choices::new(Some(file.clone())).get(ws), Choice { project: None, container: Some(true) });
        again.set(ws, Field::Container, None, true).unwrap();
        assert!(!std::fs::read_to_string(&file).unwrap().contains("proj"), "forgotten workspaces are dropped");
    }

    #[test]
    fn without_a_data_folder_everything_is_per_session() {
        let c = Choices::new(None);
        let ws = Path::new("/w");
        c.set(ws, Field::Container, Some(true), true).unwrap();
        assert_eq!(c.get(ws).container, Some(true), "kept for the session");
        c.set(ws, Field::Container, None, true).unwrap();
        assert_eq!(c.get(ws).container, None);
    }
}
