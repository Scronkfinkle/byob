//! `settings.json` (user preferences) and `secrets.json` (API keys) in the config dir.
//!
//! Settings are forgiving: a missing file means defaults, a field with the wrong type falls
//! back to its default (logged), unknown fields are kept and written back, and a corrupt
//! file is copied aside and replaced by defaults with a warning. Secrets never go in
//! `settings.json`; `secrets.json` is written with mode 0600 on unix.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub use byob_types::config::{Approvals, Layer, ToolMode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::error::Result;
use crate::files::{Loaded, load_json, save_json};

/// Deserialize a field, falling back to its default when the stored value has the wrong
/// shape — one bad field shouldn't reset every other setting.
fn lenient<'de, D: Deserializer<'de>, T: DeserializeOwned + Default>(d: D) -> Result<T, D::Error> {
    let v = Value::deserialize(d)?;
    Ok(serde_json::from_value(v.clone()).unwrap_or_else(|e| {
        log::warn!("byob-store: ignoring settings value {v}: {e}");
        T::default()
    }))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    /// Follow the operating system.
    #[default]
    System,
    Light,
    Dark,
}

/// Where to send inference requests. `None` means "use the built-in default".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InferenceSettings {
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The base URL that last answered (a connection check or a reply worked). Set once
    /// byob has been connected to something, so returning users skip the welcome screen.
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub last_good: Option<String>,
    /// Fields this build doesn't know (written by a newer byob, or by hand); preserved.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl InferenceSettings {
    fn is_empty(&self) -> bool {
        self.base_url.is_none() && self.model.is_none() && self.last_good.is_none() && self.extra.is_empty()
    }

    /// An endpoint has worked before.
    pub fn onboarded(&self) -> bool {
        self.last_good.as_deref().is_some_and(|u| !u.trim().is_empty())
    }
}

/// Which chat storage backend to use (`"storage"` in settings.json).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageKind {
    /// One SQLite file, `<data>/byob.db`.
    #[default]
    Sqlite,
    /// Plain files: one folder per chat with an append-only `events.jsonl` and a generated
    /// `chat.md` (see [`crate::FolderStore`]).
    Folder,
}

impl StorageKind {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// When the engine summarises a conversation to free context (see `byob-agent`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompactionSettings {
    /// Compact automatically when a request would pass the threshold.
    pub auto: bool,
    /// Threshold, in percent of the context window (clamped to 10..=95).
    pub threshold_percent: u8,
    /// Most recent turns kept word for word after a compaction.
    pub keep_turns: u8,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        Self { auto: true, threshold_percent: 80, keep_turns: 2 }
    }
}

impl CompactionSettings {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// A model role (`settings.json` `models.<name>`): another endpoint or model that subagents
/// can name in their `model:`. Missing fields default to the main inference settings. Its
/// API key lives in `secrets.json` `model_keys.<name>` (without one, a role on the main
/// endpoint uses the main key).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelRole {
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Subagents (the `delegate` tool).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SubagentSettings {
    pub enabled: bool,
    /// Most subagents at once; unset: the endpoint's slots minus one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_parallel: Option<u16>,
    /// Most tool calls of one subagent.
    pub max_tool_calls: u16,
    /// A subagent is stopped after this many minutes.
    pub time_limit_minutes: u16,
}

impl Default for SubagentSettings {
    fn default() -> Self {
        Self { enabled: true, max_parallel: None, max_tool_calls: 25, time_limit_minutes: 15 }
    }
}

impl SubagentSettings {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Containers for workspaces that run their commands in one (opt-in per workspace).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContainerSettings {
    /// Image for workspaces without a devcontainer.json (default `python:3-slim`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// `docker` or `podman` (default: podman if installed, else docker).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
}

impl ContainerSettings {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "InferenceSettings::is_empty")]
    pub inference: InferenceSettings,
    #[serde(default, deserialize_with = "lenient")]
    pub theme: Theme,
    /// Folder new chats work in.
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,
    /// Extra folders to load plugins from.
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Vec::is_empty")]
    pub plugin_dirs: Vec<PathBuf>,
    /// Chat storage backend; SQLite unless set.
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "StorageKind::is_default")]
    pub storage: StorageKind,
    /// Root folder for `storage = "folder"` (a leading `~/` means the home directory).
    /// Defaults to [`crate::Paths::chats_folder`].
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub chats_folder: Option<PathBuf>,
    /// Automatic context compaction.
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "CompactionSettings::is_default")]
    pub compaction: CompactionSettings,
    /// Model roles by name (`fast`, …) for subagents.
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, ModelRole>,
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "SubagentSettings::is_default")]
    pub subagents: SubagentSettings,
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "ContainerSettings::is_default")]
    pub containers: ContainerSettings,
    /// How turns pass tools to the model (default: automatic). Workspaces and chats may override it.
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub tool_mode: Option<ToolMode>,
    /// Which tool calls run without asking (default: ask before changes).
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub approvals: Option<Approvals>,
    /// JPEG quality for images sent to the model (0: as they are; default
    /// [`byob_types::config::DEFAULT_IMAGE_QUALITY`]). Workspaces and chats may override it.
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub image_quality: Option<u8>,
    /// Most tool calls in one turn (0: no limit; default
    /// [`byob_types::config::DEFAULT_TOOL_CALLS`]). Workspaces and chats may override it.
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<u32>,
    /// Added to every chat's system prompt (workspaces and chats add their own after it).
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Where "New workspace" creates fresh folders (default `~/byob-workspaces`; `~/` allowed).
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub workspaces_root: Option<PathBuf>,
    /// Plugins turned on or off everywhere, by id (the global level of the plugin switches;
    /// workspaces and chats may override them).
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "BTreeMap::is_empty")]
    pub plugins: BTreeMap<String, bool>,
    /// Plugin settings that aren't secret, by plugin id, then setting key. Secret ones live
    /// in secrets.json ([`Secrets::plugin_settings`]).
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "BTreeMap::is_empty")]
    pub plugin_settings: BTreeMap<String, BTreeMap<String, String>>,
    /// Fields this build doesn't know; preserved across load/save.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Keys that must never live in settings.json.
const SECRET_KEYS: &[&str] = &["api_key", "apiKey"];

fn strip_secrets(map: &mut Map<String, Value>, at: &str, warnings: &mut Vec<String>) {
    for k in SECRET_KEYS {
        if map.remove(*k).is_some() {
            warnings.push(format!("ignored {at}{k} in settings.json: API keys belong in secrets.json"));
        }
    }
}

impl Settings {
    /// Read settings; never fails (see the module docs). Warnings are also logged.
    pub fn load(path: &Path) -> Loaded<Self> {
        let mut loaded: Loaded<Self> = load_json(path);
        strip_secrets(&mut loaded.value.extra, "", &mut loaded.warnings);
        strip_secrets(&mut loaded.value.inference.extra, "inference.", &mut loaded.warnings);
        for (name, role) in &mut loaded.value.models {
            strip_secrets(&mut role.extra, &format!("models.{name}."), &mut loaded.warnings);
        }
        for w in &loaded.warnings {
            log::warn!("byob-store: {w}");
        }
        loaded
    }

    /// Write atomically (temp file + rename).
    pub fn save(&self, path: &Path) -> Result<()> {
        let mut clean = self.clone();
        let mut ignored = Vec::new();
        strip_secrets(&mut clean.extra, "", &mut ignored);
        strip_secrets(&mut clean.inference.extra, "inference.", &mut ignored);
        for role in clean.models.values_mut() {
            strip_secrets(&mut role.extra, "", &mut ignored);
        }
        save_json(path, &clean, None)
    }

    /// Load, change, save; returns what was saved.
    pub fn update(path: &Path, change: impl FnOnce(&mut Self)) -> Result<Self> {
        let mut s = Self::load(path).value;
        change(&mut s);
        s.save(path)?;
        Ok(s)
    }

    /// The global level of the layered settings (see [`byob_types::config`]). Compaction and
    /// subagents count as set only where they differ from the built-in defaults.
    pub fn global_layer(&self) -> Layer {
        let text = |v: &Option<String>| v.clone().filter(|s| !s.trim().is_empty());
        let (compaction, subagents) = (CompactionSettings::default(), SubagentSettings::default());
        Layer {
            base_url: text(&self.inference.base_url),
            model: text(&self.inference.model),
            tool_mode: self.tool_mode,
            approvals: self.approvals,
            auto_compact: Some(self.compaction.auto).filter(|a| *a != compaction.auto),
            compact_percent: Some(self.compaction.threshold_percent).filter(|p| *p != compaction.threshold_percent),
            subagents: Some(self.subagents.enabled).filter(|s| *s != subagents.enabled),
            image_quality: self.image_quality.map(|q| q.min(100)),
            max_tool_calls: self.max_tool_calls,
            instructions: text(&self.instructions),
            env: None,
            image: None,
            plugins: Some(self.plugins.clone()).filter(|p| !p.is_empty()),
        }
    }

    /// Store the global level's layered settings. The endpoint is left alone: it changes
    /// through the connection form, which also checks it.
    pub fn set_global_layer(&mut self, l: &Layer) {
        self.tool_mode = l.tool_mode;
        self.approvals = l.approvals;
        let defaults = CompactionSettings::default();
        self.compaction.auto = l.auto_compact.unwrap_or(defaults.auto);
        self.compaction.threshold_percent = l.compact_percent.unwrap_or(defaults.threshold_percent);
        self.subagents.enabled = l.subagents.unwrap_or(SubagentSettings::default().enabled);
        self.image_quality = l.image_quality;
        self.max_tool_calls = l.max_tool_calls;
        self.instructions.clone_from(&l.instructions);
        self.plugins = l.plugins.clone().unwrap_or_default();
    }

    /// The folder new workspaces are created in.
    pub fn workspaces_root(&self) -> Option<PathBuf> {
        match &self.workspaces_root {
            Some(p) => Some(crate::backend::expand_home(p)),
            None => directories::BaseDirs::new().map(|d| d.home_dir().join("byob-workspaces")),
        }
    }
}

/// Credentials, kept apart from settings so settings.json can be shared or synced.
/// Later this may move to the OS keyring; callers only see this type.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Secrets {
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// API keys of model roles, by role name.
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "BTreeMap::is_empty")]
    pub model_keys: BTreeMap<String, String>,
    /// API keys of workspaces and chats that use their own endpoint, by scope
    /// (`workspace:<id>`, `chat:<id>`).
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "BTreeMap::is_empty")]
    pub scoped_keys: BTreeMap<String, String>,
    /// Secret plugin settings (API keys, tokens), by `plugin:<id>`, then setting key. They
    /// reach only the plugin's own processes, as environment variables.
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "BTreeMap::is_empty")]
    pub plugin_settings: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secrets")
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("model_keys", &self.model_keys.keys().collect::<Vec<_>>())
            .field("scoped_keys", &self.scoped_keys.keys().collect::<Vec<_>>())
            .field("plugin_settings", &self.plugin_settings.keys().collect::<Vec<_>>())
            .field("extra", &self.extra.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Owner read/write only.
pub const SECRETS_MODE: u32 = 0o600;

impl Secrets {
    /// Read secrets; never fails. On unix, a file readable by others is tightened to 0600
    /// (with a warning).
    pub fn load(path: &Path) -> Loaded<Self> {
        let mut loaded: Loaded<Self> = load_json(path);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(path) {
                let mode = meta.permissions().mode() & 0o777;
                if mode & 0o077 != 0 {
                    let fixed = std::fs::set_permissions(path, std::fs::Permissions::from_mode(SECRETS_MODE));
                    loaded.warnings.push(match fixed {
                        Ok(()) => format!("{} was readable by others (mode {mode:o}); changed it to 600", path.display()),
                        Err(e) => format!("{} is readable by others (mode {mode:o}) and couldn't be fixed: {e}", path.display()),
                    });
                }
            }
        }
        for w in &loaded.warnings {
            log::warn!("byob-store: {w}");
        }
        loaded
    }

    /// Write atomically, mode 0600 on unix.
    pub fn save(&self, path: &Path) -> Result<()> {
        save_json(path, self, Some(SECRETS_MODE))
    }
}
