//! Workspaces (a folder, a name and its own settings) and per-chat settings.
//!
//! Kept in byob's own folders, never in the workspace folder:
//! - `<config>/workspaces.json`: `{"workspaces": [{id, name, folder, created_at, config}]}`;
//! - `<data>/chat-settings.json`: `{"chats": {"<chat id>": {…layer…}}}`.
//!
//! A chat belongs to the workspace whose folder it was created in (`Chat::workspace`); a
//! folder seen on a chat without a workspace becomes one ([`Workspaces::ensure`]), so older
//! chats group without any migration. Ids are derived from the folder path, so registering
//! the same folder twice (or after a lost file) gives the same id. Files are read once and
//! written atomically after each change; a damaged one is kept aside (see `files`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use byob_types::config::Layer;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::{Result, StoreError};
use crate::files::{Loaded, load_json, save_json};
use crate::model::Millis;
use crate::paths::Paths;

/// Longest workspace name.
pub const MAX_NAME_CHARS: usize = 80;
/// Longest folder path a client may name.
pub const MAX_PATH_CHARS: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRow {
    pub id: String,
    pub name: String,
    /// Canonical folder; chats created here belong to this workspace.
    pub folder: PathBuf,
    #[serde(default)]
    pub created_at: Millis,
    /// Its own settings (see [`byob_types::config`]).
    #[serde(default)]
    pub config: Layer,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct WorkspacesFile {
    #[serde(default)]
    workspaces: Vec<WorkspaceRow>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ChatsFile {
    #[serde(default)]
    chats: BTreeMap<String, Layer>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

/// Every workspace and every chat's own settings, in memory, saved on change.
#[derive(Debug, Default)]
pub struct Workspaces {
    file: Option<PathBuf>,
    chats_file: Option<PathBuf>,
    data: WorkspacesFile,
    chats: ChatsFile,
}

/// A stable id for a folder: `w` + 16 hex digits of its path's FNV-1a hash.
pub fn workspace_id(folder: &Path) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in folder.as_os_str().as_encoded_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("w{h:016x}")
}

/// What a folder is called when it becomes a workspace by itself: its last component.
pub fn default_name(folder: &Path) -> String {
    let name = folder.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    clean_name(&name).unwrap_or_else(|_| folder.display().to_string().chars().take(MAX_NAME_CHARS).collect())
}

/// A user-given name: trimmed, one line, not empty, at most [`MAX_NAME_CHARS`].
pub fn clean_name(name: &str) -> Result<String> {
    let name: String = name.trim().chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let name = name.trim().to_owned();
    if name.is_empty() {
        return Err(StoreError::Invalid("a workspace needs a name".into()));
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(StoreError::Invalid(format!("workspace names are limited to {MAX_NAME_CHARS} characters")));
    }
    Ok(name)
}

/// A folder name for a new workspace called `name`: ASCII letters, digits and dashes.
pub fn folder_slug(name: &str) -> String {
    let mut slug = String::new();
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    let slug: String = slug.trim_end_matches('-').chars().take(48).collect();
    if slug.is_empty() { "workspace".into() } else { slug }
}

fn now() -> Millis {
    chrono::Utc::now().timestamp_millis()
}

impl Workspaces {
    /// Read both files (missing → empty). Without `paths`, everything stays in memory.
    pub fn open(paths: Option<&Paths>) -> Loaded<Self> {
        let Some(paths) = paths else { return Loaded { value: Self::default(), warnings: Vec::new() } };
        let (file, chats_file) = (paths.workspaces(), paths.chat_settings());
        let data: Loaded<WorkspacesFile> = load_json(&file);
        let chats: Loaded<ChatsFile> = load_json(&chats_file);
        let mut warnings = data.warnings;
        warnings.extend(chats.warnings);
        let mut value = Self { file: Some(file), chats_file: Some(chats_file), data: data.value, chats: chats.value };
        // Hand edits may have duplicated a folder or an id: keep the first of each.
        let mut seen = std::collections::BTreeSet::new();
        value.data.workspaces.retain(|w| seen.insert(w.folder.clone()) && !w.id.is_empty());
        let mut ids = std::collections::BTreeSet::new();
        value.data.workspaces.retain(|w| ids.insert(w.id.clone()));
        for w in &warnings {
            log::warn!("byob-store: {w}");
        }
        Loaded { value, warnings }
    }

    fn save(&self) -> Result<()> {
        match &self.file {
            Some(f) => save_json(f, &self.data, None),
            None => Ok(()),
        }
    }

    fn save_chats(&self) -> Result<()> {
        match &self.chats_file {
            Some(f) => save_json(f, &self.chats, None),
            None => Ok(()),
        }
    }

    pub fn list(&self) -> &[WorkspaceRow] {
        &self.data.workspaces
    }

    pub fn get(&self, id: &str) -> Option<&WorkspaceRow> {
        self.data.workspaces.iter().find(|w| w.id == id)
    }

    pub fn by_folder(&self, folder: &Path) -> Option<&WorkspaceRow> {
        self.data.workspaces.iter().find(|w| w.folder == folder)
    }

    /// The workspace of `folder`, registering it (named after the folder, no settings of
    /// its own) if it has none yet. Returns it and whether it was new.
    pub fn ensure(&mut self, folder: &Path) -> Result<(WorkspaceRow, bool)> {
        if let Some(w) = self.by_folder(folder) {
            return Ok((w.clone(), false));
        }
        let row = WorkspaceRow {
            id: self.free_id(folder),
            name: default_name(folder),
            folder: folder.to_path_buf(),
            created_at: now(),
            config: Layer::default(),
            extra: Map::new(),
        };
        self.data.workspaces.push(row.clone());
        self.save()?;
        Ok((row, true))
    }

    /// The folder's id, or a variant if a hand edit gave it to another folder.
    fn free_id(&self, folder: &Path) -> String {
        let base = workspace_id(folder);
        let mut id = base.clone();
        let mut n = 1u32;
        while self.get(&id).is_some() {
            n = n.saturating_add(1);
            id = format!("{base}-{n}");
        }
        id
    }

    /// Register `folder` as a new workspace called `name` with settings `config` (already
    /// validated). Refused if the folder already is one.
    pub fn create(&mut self, name: &str, folder: &Path, config: Layer) -> Result<WorkspaceRow> {
        let name = clean_name(name)?;
        if let Some(w) = self.by_folder(folder) {
            return Err(StoreError::Exists(format!("{} is already the workspace {:?};", folder.display(), w.name)));
        }
        let row = WorkspaceRow { id: self.free_id(folder), name, folder: folder.to_path_buf(), created_at: now(), config, extra: Map::new() };
        self.data.workspaces.push(row.clone());
        self.save()?;
        Ok(row)
    }

    fn find_mut(&mut self, id: &str) -> Result<&mut WorkspaceRow> {
        self.data.workspaces.iter_mut().find(|w| w.id == id).ok_or_else(|| StoreError::NotFound(format!("workspace {id}")))
    }

    pub fn rename(&mut self, id: &str, name: &str) -> Result<()> {
        let name = clean_name(name)?;
        self.find_mut(id)?.name = name;
        self.save()
    }

    /// Replace a workspace's own settings (already validated).
    pub fn set_config(&mut self, id: &str, config: Layer) -> Result<()> {
        self.find_mut(id)?.config = config;
        self.save()
    }

    /// Forget a workspace (its folder is untouched). Its chats' settings are the caller's.
    pub fn remove(&mut self, id: &str) -> Result<WorkspaceRow> {
        let i = self.data.workspaces.iter().position(|w| w.id == id).ok_or_else(|| StoreError::NotFound(format!("workspace {id}")))?;
        let row = self.data.workspaces.remove(i);
        self.save()?;
        Ok(row)
    }

    /// A chat's own settings, if it has any.
    pub fn chat_config(&self, chat: &str) -> Option<&Layer> {
        self.chats.chats.get(chat)
    }

    /// Replace a chat's own settings (already validated); an empty layer removes them.
    pub fn set_chat_config(&mut self, chat: &str, config: Layer) -> Result<()> {
        if config.is_empty() {
            if self.chats.chats.remove(chat).is_none() {
                return Ok(());
            }
        } else {
            self.chats.chats.insert(chat.to_owned(), config);
        }
        self.save_chats()
    }

    /// A chat was deleted.
    pub fn forget_chat(&mut self, chat: &str) -> Result<()> {
        self.set_chat_config(chat, Layer::default())
    }
}

#[cfg(test)]
mod tests {
    use byob_types::config::{Approvals, EnvKind};

    use super::*;

    fn paths(dir: &Path) -> Paths {
        Paths::at(dir)
    }

    #[test]
    fn workspaces_persist_and_folders_register_once() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        let mut w = Workspaces::open(Some(&p)).value;
        assert!(w.list().is_empty());
        let (a, new) = w.ensure(Path::new("/home/me/proj")).unwrap();
        assert!(new && a.name == "proj" && a.id == workspace_id(Path::new("/home/me/proj")));
        let (again, new) = w.ensure(Path::new("/home/me/proj")).unwrap();
        assert!(!new && again.id == a.id);
        let cfg = Layer { env: Some(EnvKind::Container), image: Some("alpine".into()), ..Layer::default() };
        let b = w.create("  Data\nwork ", Path::new("/srv/data"), cfg.clone()).unwrap();
        assert_eq!(b.name, "Data work");
        assert!(matches!(w.create("dup", Path::new("/srv/data"), Layer::default()), Err(StoreError::Exists(_))));
        w.rename(&a.id, "Project").unwrap();
        assert!(w.rename(&a.id, "   ").is_err() && w.rename("nope", "x").is_err());
        w.set_config(&a.id, Layer { approvals: Some(Approvals::AllowEdits), ..Layer::default() }).unwrap();

        let back = Workspaces::open(Some(&p)).value;
        assert_eq!(back.list().len(), 2);
        assert_eq!(back.get(&a.id).unwrap().name, "Project");
        assert_eq!(back.get(&a.id).unwrap().config.approvals, Some(Approvals::AllowEdits));
        assert_eq!(back.by_folder(Path::new("/srv/data")).unwrap().config, cfg);

        let mut back = back;
        let gone = back.remove(&b.id).unwrap();
        assert_eq!(gone.folder, Path::new("/srv/data"));
        assert!(back.remove(&b.id).is_err());
        assert_eq!(Workspaces::open(Some(&p)).value.list().len(), 1);
    }

    #[test]
    fn chat_settings_persist_and_empty_ones_are_dropped() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        let mut w = Workspaces::open(Some(&p)).value;
        w.set_chat_config("c1", Layer { model: Some("small".into()), ..Layer::default() }).unwrap();
        w.set_chat_config("c2", Layer { env: Some(EnvKind::Host), ..Layer::default() }).unwrap();
        let mut back = Workspaces::open(Some(&p)).value;
        assert_eq!(back.chat_config("c1").and_then(|l| l.model.as_deref()), Some("small"));
        back.set_chat_config("c1", Layer::default()).unwrap();
        back.forget_chat("c2").unwrap();
        back.forget_chat("never").unwrap();
        let back = Workspaces::open(Some(&p)).value;
        assert!(back.chat_config("c1").is_none() && back.chat_config("c2").is_none());
        assert!(!std::fs::read_to_string(p.chat_settings()).unwrap().contains("c1"));
    }

    #[test]
    fn damaged_or_hand_edited_files_are_survivable() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        std::fs::write(p.workspaces(), "{not json").unwrap();
        let loaded = Workspaces::open(Some(&p));
        assert!(loaded.value.list().is_empty() && !loaded.warnings.is_empty());
        assert!(d.path().join("workspaces.json.corrupt").exists(), "the damaged file is kept");
        // Duplicate folders and ids, a bad config value and unknown keys.
        std::fs::write(
            p.workspaces(),
            r#"{"workspaces":[
                {"id":"w1","name":"A","folder":"/a","config":{"approvals":"maybe","model":"m"},"color":"red"},
                {"id":"w2","name":"A again","folder":"/a"},
                {"id":"w1","name":"B","folder":"/b"}
            ],"version":3}"#,
        )
        .unwrap();
        let mut w = Workspaces::open(Some(&p)).value;
        assert_eq!(w.list().len(), 1);
        assert_eq!(w.get("w1").unwrap().config, Layer { model: Some("m".into()), ..Layer::default() });
        w.rename("w1", "Renamed").unwrap();
        let text = std::fs::read_to_string(p.workspaces()).unwrap();
        assert!(text.contains("\"color\": \"red\"") && text.contains("\"version\": 3"), "unknown fields are kept: {text}");
    }

    #[test]
    fn names_and_slugs() {
        assert_eq!(folder_slug("My Data Project!"), "my-data-project");
        assert_eq!(folder_slug("  ünïcode  "), "n-code");
        assert_eq!(folder_slug("***"), "workspace");
        assert_eq!(default_name(Path::new("/")), "/");
        assert!(clean_name(&"x".repeat(MAX_NAME_CHARS + 1)).is_err());
        let mem = Workspaces::open(None).value;
        assert!(mem.list().is_empty());
    }
}
