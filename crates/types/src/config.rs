//! Layered settings: built-in defaults → global (`settings.json`) → workspace → chat.
//!
//! Each level is a [`Layer`] of optional values; [`resolve`] turns the levels into the
//! [`Effective`] settings of one chat and says where each value came from. Only some
//! settings exist at each level ([`Field::levels`]):
//!
//! | setting | global | workspace | chat |
//! |---|---|---|---|
//! | endpoint (URL + model; keys stay in the engine) | ✓ | ✓ | ✓ |
//! | tool calling mode | ✓ | ✓ | ✓ |
//! | approvals | ✓ | ✓ | ✓ |
//! | compaction (auto, threshold) | ✓ | ✓ | ✓ |
//! | subagents on/off | ✓ | ✓ | ✓ |
//! | extra instructions (appended, never replaced) | ✓ | ✓ | ✓ |
//! | environment (Auto, Host, Nix, Container) | | ✓ | Host / Container only |
//! | container image | | ✓ | |
//!
//! Global-only settings (theme, storage, plugin folders, container runtime and default
//! image, the workspaces folder, model roles, subagent limits) stay in `settings.json` and
//! are not layered. The most specific level that sets a value wins, except instructions,
//! which are joined in order global, workspace, chat. Command-line testing flags
//! (`--auto-approve`, `--tool-mode`) beat every level ([`Level::Flag`]).
//!
//! This module is pure (no I/O) so the engine and the UI resolve the same way.

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// Longest base URL a level may set.
pub const MAX_URL_CHARS: usize = 2048;
/// Longest model name.
pub const MAX_MODEL_CHARS: usize = 256;
/// Longest extra instructions per level.
pub const MAX_INSTRUCTIONS_CHARS: usize = 8000;
/// Longest container image reference.
pub const MAX_IMAGE_CHARS: usize = 255;

/// The built-in endpoint when nothing is configured.
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8080/v1";
pub const DEFAULT_MODEL: &str = "default";
pub const DEFAULT_COMPACT_PERCENT: u8 = 80;
/// JPEG quality images sent to the model get by default: a light pass, often much smaller
/// than a PNG screenshot and hard to tell apart.
pub const DEFAULT_IMAGE_QUALITY: u8 = 85;
/// Tool calls one turn may make by default (`max_tool_calls`; 0 is no limit).
pub const DEFAULT_TOOL_CALLS: u32 = 30;
/// Largest limit that can be set short of none.
pub const MAX_TOOL_CALLS: u32 = 100_000;

/// Where a value comes from, least to most specific.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// Built into byob.
    #[default]
    Default,
    /// `settings.json` (or `--inference-url` / `--model` for this run).
    Global,
    Workspace,
    Chat,
    /// A command-line testing flag for this run (`--auto-approve`, `--tool-mode`).
    Flag,
}

impl Level {
    /// For the UI: "default", "global settings", "workspace", "chat", "command line".
    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Global => "global settings",
            Self::Workspace => "workspace",
            Self::Chat => "chat",
            Self::Flag => "command line",
        }
    }
}

/// How turns pass tools to the model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolMode {
    /// Follow the endpoint probe; native first when it is inconclusive.
    #[default]
    Auto,
    /// The server's structured `tools` / `tool_calls`.
    Native,
    /// Tools described in the system prompt; calls parsed from the reply text.
    Prompt,
}

impl ToolMode {
    pub const ALL: [Self; 3] = [Self::Auto, Self::Native, Self::Prompt];

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Self::Auto),
            "native" => Some(Self::Native),
            "prompt" => Some(Self::Prompt),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Automatic",
            Self::Native => "Native tool calls",
            Self::Prompt => "Tools in the prompt",
        }
    }
}

/// Which tool calls run without asking.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Approvals {
    /// Reads run; writes, network and commands ask.
    #[default]
    Ask,
    /// Reads and file edits run; network and commands ask.
    AllowEdits,
    /// Everything runs without asking (calls a person must confirm still ask).
    AllowAll,
    /// Only reads: writes, network and commands are refused.
    ReadOnly,
}

impl Approvals {
    pub const ALL: [Self; 4] = [Self::Ask, Self::AllowEdits, Self::AllowAll, Self::ReadOnly];

    pub fn label(self) -> &'static str {
        match self {
            Self::Ask => "Ask before changes",
            Self::AllowEdits => "Allow file edits",
            Self::AllowAll => "Allow everything",
            Self::ReadOnly => "Read only",
        }
    }
}

/// Where a workspace's commands (`run_command`, `run_python`) run and what they get.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvKind {
    /// Today's behaviour: on this machine; byob asks once before loading the folder's
    /// project environment or using its devcontainer.json.
    #[default]
    Auto,
    /// Always on this machine (byob's sandbox), never in a container. A project
    /// environment is still asked about once.
    Host,
    /// On this machine with the folder's Nix dev shell (flake.nix / shell.nix, or another
    /// project environment) loaded without asking.
    Nix,
    /// In the workspace's Docker/Podman container (devcontainer.json or an image).
    Container,
}

impl EnvKind {
    pub const ALL: [Self; 4] = [Self::Auto, Self::Host, Self::Nix, Self::Container];

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Host => "This machine",
            Self::Nix => "Nix dev shell",
            Self::Container => "Container",
        }
    }

    /// May a chat choose this (a chat only picks where its commands run)?
    pub fn per_chat(self) -> bool {
        matches!(self, Self::Host | Self::Container)
    }
}

/// Deserialize a field, falling back to `None` when the stored value has the wrong shape:
/// one bad value shouldn't reset a whole layer.
fn lenient<'de, D: Deserializer<'de>, T: DeserializeOwned>(d: D) -> Result<Option<T>, D::Error> {
    let v = Value::deserialize(d)?;
    Ok(serde_json::from_value(v).ok())
}

/// One level's own settings; `None` means "inherit".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layer {
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub tool_mode: Option<ToolMode>,
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub approvals: Option<Approvals>,
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub auto_compact: Option<bool>,
    /// Compact when a request passes this percentage of the context window (10..=95).
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub compact_percent: Option<u8>,
    /// Offer the `delegate` tool.
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub subagents: Option<bool>,
    /// Images sent to the model are re-encoded as JPEG at this quality (1..=100) when that
    /// makes them smaller; 0 sends them as they are (downscaled only).
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub image_quality: Option<u8>,
    /// Most tool calls in one turn; 0: no limit.
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<u32>,
    /// Appended to the system prompt after the less specific levels' instructions.
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub env: Option<EnvKind>,
    /// The container image (workspace only). `None`: the folder's devcontainer.json if it
    /// has one, else the global default image.
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Plugins turned on (`true`) or off (`false`) at this level, by plugin id; plugins not
    /// named inherit (on by default). Every level has it; it changes through its own message
    /// (`SetPluginEnabled`), not the settings form.
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub plugins: Option<BTreeMap<String, bool>>,
}

/// Most plugins one level may turn on or off.
pub const MAX_PLUGIN_SWITCHES: usize = 500;

/// A plugin id as manifests have it: `[a-z0-9][a-z0-9-]{0,47}`.
pub fn plugin_id_ok(id: &str) -> bool {
    let b = id.as_bytes();
    let alnum = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    !b.is_empty() && b.len() <= 48 && alnum(b[0]) && b.iter().all(|&c| alnum(c) || c == b'-')
}

/// The groups of settings, for [`Field::levels`] and the UI's reset buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Field {
    /// `base_url` and `model` together.
    Endpoint,
    ToolMode,
    Approvals,
    /// `auto_compact` and `compact_percent`.
    Compaction,
    Subagents,
    /// `image_quality`.
    ModelImages,
    /// `max_tool_calls`.
    ToolCalls,
    Instructions,
    Env,
    Image,
}

impl Field {
    pub const ALL: [Self; 10] = [
        Self::Endpoint,
        Self::ToolMode,
        Self::Approvals,
        Self::Compaction,
        Self::Subagents,
        Self::ToolCalls,
        Self::ModelImages,
        Self::Instructions,
        Self::Env,
        Self::Image,
    ];

    /// The levels this setting exists at (besides [`Level::Default`]).
    pub fn levels(self) -> &'static [Level] {
        const ALL: &[Level] = &[Level::Global, Level::Workspace, Level::Chat];
        match self {
            Self::Endpoint
            | Self::ToolMode
            | Self::Approvals
            | Self::Compaction
            | Self::Subagents
            | Self::ToolCalls
            | Self::ModelImages
            | Self::Instructions => ALL,
            Self::Env => &[Level::Workspace, Level::Chat],
            Self::Image => &[Level::Workspace],
        }
    }

    pub fn at(self, level: Level) -> bool {
        self.levels().contains(&level)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Endpoint => "Model",
            Self::ToolMode => "Tool calling",
            Self::Approvals => "Approvals",
            Self::Compaction => "Compaction",
            Self::Subagents => "Subagents",
            Self::ModelImages => "Images to the model",
            Self::ToolCalls => "Tool calls per turn",
            Self::Instructions => "Instructions",
            Self::Env => "Environment",
            Self::Image => "Container image",
        }
    }
}

impl Layer {
    /// Is `field` set at this level?
    pub fn has(&self, field: Field) -> bool {
        match field {
            Field::Endpoint => self.base_url.is_some() || self.model.is_some(),
            Field::ToolMode => self.tool_mode.is_some(),
            Field::Approvals => self.approvals.is_some(),
            Field::Compaction => self.auto_compact.is_some() || self.compact_percent.is_some(),
            Field::Subagents => self.subagents.is_some(),
            Field::ModelImages => self.image_quality.is_some(),
            Field::ToolCalls => self.max_tool_calls.is_some(),
            Field::Instructions => self.instructions.is_some(),
            Field::Env => self.env.is_some(),
            Field::Image => self.image.is_some(),
        }
    }

    /// Forget `field` here (inherit it again).
    pub fn reset(&mut self, field: Field) {
        match field {
            Field::Endpoint => {
                self.base_url = None;
                self.model = None;
            }
            Field::ToolMode => self.tool_mode = None,
            Field::Approvals => self.approvals = None,
            Field::Compaction => {
                self.auto_compact = None;
                self.compact_percent = None;
            }
            Field::Subagents => self.subagents = None,
            Field::ModelImages => self.image_quality = None,
            Field::ToolCalls => self.max_tool_calls = None,
            Field::Instructions => self.instructions = None,
            Field::Env => self.env = None,
            Field::Image => self.image = None,
        }
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Check an untrusted layer for `level`: trims text, turns blank text into `None`,
    /// clamps the threshold, and refuses settings that don't exist at that level, values
    /// that are too long, and images that aren't plain references.
    pub fn validated(mut self, level: Level) -> Result<Self, String> {
        let clean = |v: Option<String>| v.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
        self.base_url = clean(self.base_url);
        self.model = clean(self.model);
        self.image = clean(self.image);
        self.instructions = self.instructions.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
        if self.base_url.as_ref().is_some_and(|u| u.chars().count() > MAX_URL_CHARS) {
            return Err("the URL is too long".into());
        }
        if let Some(u) = &self.base_url
            && !(u.starts_with("http://") || u.starts_with("https://"))
        {
            return Err(format!("{u:?} isn't an http:// or https:// URL"));
        }
        if self.model.as_ref().is_some_and(|m| m.chars().count() > MAX_MODEL_CHARS) {
            return Err("the model name is too long".into());
        }
        if self.instructions.as_ref().is_some_and(|i| i.chars().count() > MAX_INSTRUCTIONS_CHARS) {
            return Err(format!("instructions are limited to {MAX_INSTRUCTIONS_CHARS} characters"));
        }
        if let Some(i) = &self.image
            && !valid_image(i)
        {
            return Err(format!("{i:?} isn't a container image reference (like python:3-slim)"));
        }
        self.compact_percent = self.compact_percent.map(|p| p.clamp(10, 95));
        self.image_quality = self.image_quality.map(|q| q.min(100));
        self.max_tool_calls = self.max_tool_calls.map(|n| n.min(MAX_TOOL_CALLS));
        if let Some(p) = &self.plugins {
            if p.len() > MAX_PLUGIN_SWITCHES {
                return Err(format!("at most {MAX_PLUGIN_SWITCHES} plugins can be switched per level"));
            }
            if let Some(bad) = p.keys().find(|id| !plugin_id_ok(id)) {
                return Err(format!("{bad:?} isn't a plugin id"));
            }
        }
        self.plugins = self.plugins.filter(|p| !p.is_empty());
        for field in Field::ALL {
            if self.has(field) && !field.at(level) {
                return Err(format!("{} can't be set per {}", field.label().to_lowercase(), level.label()));
            }
        }
        if level == Level::Chat
            && let Some(e) = self.env.filter(|e| !e.per_chat())
        {
            return Err(format!("a chat can only run its commands on this machine or in the container, not {:?}", e.label()));
        }
        Ok(self)
    }
}

/// An image reference: no options, spaces or shell characters.
pub fn valid_image(i: &str) -> bool {
    !i.is_empty() && i.len() <= MAX_IMAGE_CHARS && !i.starts_with('-') && i.chars().all(|c| c.is_ascii_alphanumeric() || "._-/:@".contains(c))
}

/// A resolved value and the level it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sourced<T> {
    pub value: T,
    pub from: Level,
}

/// What a chat (or a workspace's new chats) actually uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Effective {
    pub base_url: Sourced<String>,
    pub model: Sourced<String>,
    /// The level whose API key is used, if any (keys never leave the engine).
    #[serde(default)]
    pub key_from: Option<Level>,
    pub tool_mode: Sourced<ToolMode>,
    pub approvals: Sourced<Approvals>,
    pub auto_compact: Sourced<bool>,
    pub compact_percent: Sourced<u8>,
    pub subagents: Sourced<bool>,
    /// JPEG quality for images sent to the model; 0: as they are.
    pub image_quality: Sourced<u8>,
    /// Most tool calls in one turn; 0: no limit.
    pub max_tool_calls: Sourced<u32>,
    /// Every level's instructions, least specific first; all of them apply.
    #[serde(default)]
    pub instructions: Vec<(Level, String)>,
    pub env: Sourced<EnvKind>,
    /// Load the folder's project environment without asking (`Some(true)`: the workspace
    /// is Nix), or ask as usual (`None`). Follows the workspace even when a chat moves its
    /// commands to this machine.
    #[serde(default)]
    pub load_project: Option<bool>,
    /// The workspace's own image, if it set one.
    #[serde(default)]
    pub image: Option<String>,
    /// Plugins some level turned on or off, by id, with the level that decided. Plugins not
    /// listed are on.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub plugins: BTreeMap<String, Sourced<bool>>,
}

impl Effective {
    /// Commands in a container (`Some(true)`), on this machine (`Some(false)`), or as the
    /// workspace's answers say (`None`, asked once).
    pub fn container(&self) -> Option<bool> {
        match self.env.value {
            EnvKind::Auto => None,
            EnvKind::Host | EnvKind::Nix => Some(false),
            EnvKind::Container => Some(true),
        }
    }

    /// Is plugin `id` on (and which level says so; [`Level::Default`]: nobody switched it)?
    pub fn plugin_on(&self, id: &str) -> Sourced<bool> {
        self.plugins.get(id).cloned().unwrap_or(Sourced { value: true, from: Level::Default })
    }

    /// The instructions as one block for the system prompt (empty if there are none).
    pub fn instructions_text(&self) -> String {
        self.instructions.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>().join("\n\n")
    }
}

/// The levels of one chat.
#[derive(Debug, Clone, Copy, Default)]
pub struct Levels<'a> {
    pub global: Option<&'a Layer>,
    pub workspace: Option<&'a Layer>,
    pub chat: Option<&'a Layer>,
}

/// Which levels store an API key.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Keys {
    pub global: bool,
    pub workspace: bool,
    pub chat: bool,
}

/// Testing flags that beat every level for this run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Forced {
    pub approvals: Option<Approvals>,
    pub tool_mode: Option<ToolMode>,
}

fn pick<T: Clone>(levels: &Levels<'_>, default: T, get: impl Fn(&Layer) -> Option<T>) -> Sourced<T> {
    let ordered = [(Level::Chat, levels.chat), (Level::Workspace, levels.workspace), (Level::Global, levels.global)];
    for (level, layer) in ordered {
        if let Some(v) = layer.and_then(&get) {
            return Sourced { value: v, from: level };
        }
    }
    Sourced { value: default, from: Level::Default }
}

/// Scheme, host and port of a URL, lowercased ("http://127.0.0.1:8080").
pub fn origin(url: &str) -> String {
    let u = url.trim().to_ascii_lowercase();
    let (scheme, rest) = u.split_once("://").unwrap_or(("", u.as_str()));
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    format!("{scheme}://{host}")
}

/// The effective settings of a chat with these levels. `keys` says which levels store an
/// API key: a key belongs to the server its level points at, so it is used only when the
/// effective URL has the same scheme, host and port (the most specific such level wins).
pub fn resolve(levels: Levels<'_>, keys: Keys, forced: Forced) -> Effective {
    let base_url = pick(&levels, DEFAULT_BASE_URL.to_owned(), |l| l.base_url.clone());
    let model = pick(&levels, DEFAULT_MODEL.to_owned(), |l| l.model.clone());
    // The URL each level points at: its own, else what it inherits.
    let upto = |stop: Level| {
        let partial = Levels {
            global: levels.global,
            workspace: levels.workspace.filter(|_| stop >= Level::Workspace),
            chat: levels.chat.filter(|_| stop >= Level::Chat),
        };
        pick(&partial, DEFAULT_BASE_URL.to_owned(), |l| l.base_url.clone()).value
    };
    let want = origin(&base_url.value);
    let key_from = [(Level::Chat, keys.chat), (Level::Workspace, keys.workspace), (Level::Global, keys.global)]
        .into_iter()
        .find(|(level, has)| *has && origin(&upto(*level)) == want)
        .map(|(level, _)| level);
    let mut tool_mode = pick(&levels, ToolMode::Auto, |l| l.tool_mode);
    if let Some(m) = forced.tool_mode {
        tool_mode = Sourced { value: m, from: Level::Flag };
    }
    let mut approvals = pick(&levels, Approvals::Ask, |l| l.approvals);
    if let Some(a) = forced.approvals {
        approvals = Sourced { value: a, from: Level::Flag };
    }
    let instructions = [(Level::Global, levels.global), (Level::Workspace, levels.workspace), (Level::Chat, levels.chat)]
        .into_iter()
        .filter_map(|(level, layer)| layer.and_then(|l| l.instructions.clone()).filter(|s| !s.trim().is_empty()).map(|s| (level, s)))
        .collect();
    let ws_env = levels.workspace.and_then(|l| l.env).unwrap_or_default();
    let chat_env = levels.chat.and_then(|l| l.env).filter(|e| e.per_chat());
    let env = match (chat_env, levels.workspace.and_then(|l| l.env)) {
        (Some(e), _) => Sourced { value: e, from: Level::Chat },
        (None, Some(e)) => Sourced { value: e, from: Level::Workspace },
        (None, None) => Sourced { value: EnvKind::Auto, from: Level::Default },
    };
    Effective {
        base_url,
        model,
        key_from,
        tool_mode,
        approvals,
        auto_compact: pick(&levels, true, |l| l.auto_compact),
        compact_percent: pick(&levels, DEFAULT_COMPACT_PERCENT, |l| l.compact_percent.map(|p| p.clamp(10, 95))),
        subagents: pick(&levels, true, |l| l.subagents),
        image_quality: pick(&levels, DEFAULT_IMAGE_QUALITY, |l| l.image_quality.map(|q| q.min(100))),
        max_tool_calls: pick(&levels, DEFAULT_TOOL_CALLS, |l| l.max_tool_calls.map(|n| n.min(MAX_TOOL_CALLS))),
        instructions,
        env,
        load_project: (ws_env == EnvKind::Nix).then_some(true),
        image: levels.workspace.and_then(|l| l.image.clone()).filter(|i| valid_image(i)),
        plugins: plugin_switches(&levels),
    }
}

/// Every level's plugin switches, the most specific level winning per plugin.
fn plugin_switches(levels: &Levels<'_>) -> BTreeMap<String, Sourced<bool>> {
    let mut out = BTreeMap::new();
    for (level, layer) in [(Level::Global, levels.global), (Level::Workspace, levels.workspace), (Level::Chat, levels.chat)] {
        for (id, on) in layer.and_then(|l| l.plugins.as_ref()).into_iter().flatten() {
            out.insert(id.clone(), Sourced { value: *on, from: level });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(f: impl FnOnce(&mut Layer)) -> Layer {
        let mut l = Layer::default();
        f(&mut l);
        l
    }

    #[test]
    fn nothing_set_gives_the_built_in_defaults() {
        let e = resolve(Levels::default(), Keys::default(), Forced::default());
        assert_eq!(e.base_url, Sourced { value: DEFAULT_BASE_URL.to_owned(), from: Level::Default });
        assert_eq!(e.model.value, DEFAULT_MODEL);
        assert_eq!((e.tool_mode.value, e.approvals.value, e.env.value), (ToolMode::Auto, Approvals::Ask, EnvKind::Auto));
        assert_eq!((e.auto_compact.value, e.compact_percent.value, e.subagents.value), (true, 80, true));
        assert!(e.instructions.is_empty() && e.key_from.is_none() && e.load_project.is_none() && e.container().is_none());
    }

    #[test]
    fn the_most_specific_level_wins_per_field() {
        let g = layer(|l| {
            l.base_url = Some("http://g/v1".into());
            l.model = Some("gm".into());
            l.approvals = Some(Approvals::AllowEdits);
            l.compact_percent = Some(70);
        });
        let w = layer(|l| {
            l.model = Some("wm".into());
            l.tool_mode = Some(ToolMode::Prompt);
            l.compact_percent = Some(60);
        });
        let c = layer(|l| {
            l.compact_percent = Some(50);
            l.subagents = Some(false);
        });
        let e = resolve(Levels { global: Some(&g), workspace: Some(&w), chat: Some(&c) }, Keys::default(), Forced::default());
        assert_eq!(e.base_url, Sourced { value: "http://g/v1".into(), from: Level::Global });
        assert_eq!(e.model, Sourced { value: "wm".into(), from: Level::Workspace });
        assert_eq!(e.tool_mode, Sourced { value: ToolMode::Prompt, from: Level::Workspace });
        assert_eq!(e.approvals, Sourced { value: Approvals::AllowEdits, from: Level::Global });
        assert_eq!(e.compact_percent, Sourced { value: 50, from: Level::Chat });
        assert_eq!(e.subagents, Sourced { value: false, from: Level::Chat });
        assert_eq!(e.auto_compact.from, Level::Default);
        // Without the chat, the workspace's values show through.
        let e = resolve(Levels { global: Some(&g), workspace: Some(&w), chat: None }, Keys::default(), Forced::default());
        assert_eq!(e.compact_percent, Sourced { value: 60, from: Level::Workspace });
        // Out-of-range stored thresholds are clamped.
        assert_eq!(
            resolve(Levels::default(), Keys::default(), Forced::default()).image_quality,
            Sourced { value: DEFAULT_IMAGE_QUALITY, from: Level::Default }
        );
        let as_is = layer(|l| l.image_quality = Some(0));
        let e = resolve(Levels { workspace: Some(&as_is), ..Default::default() }, Keys::default(), Forced::default());
        assert_eq!(e.image_quality, Sourced { value: 0, from: Level::Workspace });
        let wild_q = layer(|l| l.image_quality = Some(250));
        assert_eq!(resolve(Levels { chat: Some(&wild_q), ..Default::default() }, Keys::default(), Forced::default()).image_quality.value, 100);
        assert_eq!(wild_q.clone().validated(Level::Chat).unwrap().image_quality, Some(100));
        let calls = |n| layer(|l| l.max_tool_calls = Some(n));
        assert_eq!(resolve(Levels::default(), Keys::default(), Forced::default()).max_tool_calls.value, DEFAULT_TOOL_CALLS);
        let (none, many) = (calls(0), calls(u32::MAX));
        let e = resolve(Levels { global: Some(&many), chat: Some(&none), ..Default::default() }, Keys::default(), Forced::default());
        assert_eq!(e.max_tool_calls, Sourced { value: 0, from: Level::Chat });
        assert_eq!(resolve(Levels { global: Some(&many), ..Default::default() }, Keys::default(), Forced::default()).max_tool_calls.value, MAX_TOOL_CALLS);
        let wild = layer(|l| l.compact_percent = Some(200));
        assert_eq!(resolve(Levels { chat: Some(&wild), ..Default::default() }, Keys::default(), Forced::default()).compact_percent.value, 95);
    }

    #[test]
    fn plugin_switches_resolve_per_plugin() {
        let g = layer(|l| l.plugins = Some([("web".to_owned(), false), ("pdf".to_owned(), false)].into()));
        let w = layer(|l| l.plugins = Some([("web".to_owned(), true)].into()));
        let c = layer(|l| l.plugins = Some([("pdf".to_owned(), true), ("git".to_owned(), false)].into()));
        let e = resolve(Levels { global: Some(&g), workspace: Some(&w), chat: Some(&c) }, Keys::default(), Forced::default());
        assert_eq!(e.plugin_on("web"), Sourced { value: true, from: Level::Workspace });
        assert_eq!(e.plugin_on("pdf"), Sourced { value: true, from: Level::Chat });
        assert_eq!(e.plugin_on("git"), Sourced { value: false, from: Level::Chat });
        assert_eq!(e.plugin_on("other"), Sourced { value: true, from: Level::Default });
        assert!(layer(|l| l.plugins = Some([("Bad Id".to_owned(), true)].into())).validated(Level::Chat).is_err());
        assert_eq!(layer(|l| l.plugins = Some(BTreeMap::new())).validated(Level::Chat).unwrap().plugins, None);
    }

    #[test]
    fn instructions_are_appended_in_order() {
        let g = layer(|l| l.instructions = Some("Be brief.".into()));
        let w = layer(|l| l.instructions = Some("This repo uses Rust.".into()));
        let c = layer(|l| l.instructions = Some("  ".into()));
        let e = resolve(Levels { global: Some(&g), workspace: Some(&w), chat: Some(&c) }, Keys::default(), Forced::default());
        assert_eq!(e.instructions, vec![(Level::Global, "Be brief.".to_owned()), (Level::Workspace, "This repo uses Rust.".to_owned())]);
        assert_eq!(e.instructions_text(), "Be brief.\n\nThis repo uses Rust.");
    }

    #[test]
    fn keys_follow_only_their_own_server() {
        let g = layer(|l| l.base_url = Some("http://local:8080/v1".into()));
        let w = layer(|l| l.base_url = Some("https://api.example.com/v1".into()));
        let c_same = layer(|l| l.model = Some("other".into()));
        let c_back = layer(|l| l.base_url = Some("http://local:8080/v1/".into()));
        let all = Keys { global: true, workspace: true, chat: false };
        // The workspace's own key goes with its URL; the global key stays on the local server.
        let e = resolve(Levels { global: Some(&g), workspace: Some(&w), chat: Some(&c_same) }, all, Forced::default());
        assert_eq!(e.key_from, Some(Level::Workspace));
        let e = resolve(Levels { global: Some(&g), workspace: Some(&w), chat: Some(&c_same) }, Keys { global: true, ..Keys::default() }, Forced::default());
        assert_eq!(e.key_from, None, "the global key never follows the URL to another server");
        // A chat pointing back at the global server gets the global key again.
        let e = resolve(Levels { global: Some(&g), workspace: Some(&w), chat: Some(&c_back) }, all, Forced::default());
        assert_eq!(e.key_from, Some(Level::Global));
        // A chat's own key wins for its own URL.
        let e = resolve(Levels { global: Some(&g), workspace: Some(&w), chat: Some(&c_back) }, Keys { chat: true, ..all }, Forced::default());
        assert_eq!(e.key_from, Some(Level::Chat));
        // A workspace with a key but no URL of its own: it points at the global server.
        let w_key_only = layer(|l| l.model = Some("m".into()));
        let e = resolve(Levels { global: Some(&g), workspace: Some(&w_key_only), chat: None }, Keys { workspace: true, ..Keys::default() }, Forced::default());
        assert_eq!(e.key_from, Some(Level::Workspace));
        assert_eq!(origin("HTTP://Example.com:8080/v1/"), "http://example.com:8080");
        assert_ne!(origin("http://127.0.0.1:8080/v1"), origin("http://127.0.0.1:11434/v1"));
    }

    #[test]
    fn environments_come_from_the_workspace_and_a_chat_may_move_its_commands() {
        let none = resolve(Levels::default(), Keys::default(), Forced::default());
        assert_eq!((none.container(), none.load_project), (None, None));
        let nix = layer(|l| l.env = Some(EnvKind::Nix));
        let container = layer(|l| {
            l.env = Some(EnvKind::Container);
            l.image = Some("debian:stable-slim".into());
        });
        let host = layer(|l| l.env = Some(EnvKind::Host));
        let to_container = layer(|l| l.env = Some(EnvKind::Container));
        let e = resolve(Levels { workspace: Some(&container), ..Default::default() }, Keys::default(), Forced::default());
        assert_eq!(
            (e.env.value, e.env.from, e.container(), e.image.as_deref()),
            (EnvKind::Container, Level::Workspace, Some(true), Some("debian:stable-slim"))
        );
        // The chat runs on this machine instead.
        let e = resolve(Levels { workspace: Some(&container), chat: Some(&host), ..Default::default() }, Keys::default(), Forced::default());
        assert_eq!((e.env.value, e.env.from, e.container()), (EnvKind::Host, Level::Chat, Some(false)));
        // On this machine in a Nix workspace: the dev shell still loads.
        let e = resolve(Levels { workspace: Some(&nix), chat: Some(&host), ..Default::default() }, Keys::default(), Forced::default());
        assert_eq!((e.container(), e.load_project), (Some(false), Some(true)));
        let e = resolve(Levels { workspace: Some(&nix), chat: Some(&to_container), ..Default::default() }, Keys::default(), Forced::default());
        assert_eq!(e.container(), Some(true));
        // A stored chat value that isn't a placement is ignored.
        let bad_chat = layer(|l| l.env = Some(EnvKind::Nix));
        let e = resolve(Levels { workspace: Some(&container), chat: Some(&bad_chat), ..Default::default() }, Keys::default(), Forced::default());
        assert_eq!(e.env.from, Level::Workspace);
        // A bad stored image is ignored rather than handed to docker.
        let evil = layer(|l| l.image = Some("--privileged".into()));
        assert_eq!(resolve(Levels { workspace: Some(&evil), ..Default::default() }, Keys::default(), Forced::default()).image, None);
    }

    #[test]
    fn flags_beat_every_level() {
        let c = layer(|l| {
            l.approvals = Some(Approvals::ReadOnly);
            l.tool_mode = Some(ToolMode::Native);
        });
        let forced = Forced { approvals: Some(Approvals::AllowAll), tool_mode: Some(ToolMode::Prompt) };
        let e = resolve(Levels { chat: Some(&c), ..Default::default() }, Keys::default(), forced);
        assert_eq!(e.approvals, Sourced { value: Approvals::AllowAll, from: Level::Flag });
        assert_eq!(e.tool_mode, Sourced { value: ToolMode::Prompt, from: Level::Flag });
    }

    #[test]
    fn layers_from_clients_are_checked_per_level() {
        let ok = layer(|l| {
            l.base_url = Some("  http://h/v1 ".into());
            l.model = Some(" ".into());
            l.compact_percent = Some(3);
            l.instructions = Some("\n".into());
        })
        .validated(Level::Chat)
        .unwrap();
        assert_eq!((ok.base_url.as_deref(), ok.model.as_deref(), ok.compact_percent, ok.instructions.as_deref()), (Some("http://h/v1"), None, Some(10), None));
        assert!(layer(|l| l.base_url = Some("file:///etc/passwd".into())).validated(Level::Chat).is_err());
        assert!(layer(|l| l.base_url = Some(format!("http://{}", "a".repeat(MAX_URL_CHARS)))).validated(Level::Workspace).is_err());
        assert!(layer(|l| l.model = Some("m".repeat(MAX_MODEL_CHARS + 1))).validated(Level::Chat).is_err());
        assert!(layer(|l| l.instructions = Some("x".repeat(MAX_INSTRUCTIONS_CHARS + 1))).validated(Level::Global).is_err());
        // Settings that don't exist at a level.
        assert!(layer(|l| l.env = Some(EnvKind::Container)).validated(Level::Global).is_err());
        assert!(layer(|l| l.image = Some("alpine".into())).validated(Level::Chat).is_err());
        assert!(layer(|l| l.env = Some(EnvKind::Nix)).validated(Level::Chat).is_err());
        assert!(layer(|l| l.env = Some(EnvKind::Host)).validated(Level::Chat).is_ok());
        assert!(layer(|l| l.image = Some("alpine; rm -rf /".into())).validated(Level::Workspace).is_err());
        assert!(layer(|l| l.image = Some("ghcr.io/me/img:1.2@sha256:abc".into())).validated(Level::Workspace).is_ok());
    }

    #[test]
    fn layers_round_trip_and_bad_values_are_dropped_alone() {
        let l = layer(|l| {
            l.approvals = Some(Approvals::AllowEdits);
            l.env = Some(EnvKind::Container);
        });
        let json = serde_json::to_string(&l).unwrap();
        assert_eq!(json, r#"{"approvals":"allow_edits","env":"container"}"#);
        assert_eq!(serde_json::from_str::<Layer>(&json).unwrap(), l);
        let damaged: Layer = serde_json::from_str(r#"{"approvals":"sometimes","model":"m","compact_percent":"lots","future":1}"#).unwrap();
        assert_eq!(damaged, layer(|l| l.model = Some("m".into())));
        let mut reset = l.clone();
        reset.reset(Field::Env);
        assert!(!reset.has(Field::Env) && reset.has(Field::Approvals));
        reset.reset(Field::Approvals);
        assert!(reset.is_empty());
    }
}
