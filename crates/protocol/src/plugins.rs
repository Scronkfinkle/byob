//! Plugins as clients see them: what a chat can use, where each plugin comes from, whether
//! it is on, which settings it still needs, and what went wrong loading it.
//!
//! A plugin is a folder with a `byob-plugin.json` (tools, skills, subagents, MCP servers,
//! settings). It lives in one of three scopes; the same id resolves chat > workspace >
//! global, and the less specific copy is listed with [`PluginInfo::shadowed_by`]:
//!
//! - **chat** (scratch): `<data>/chats/<chat>/plugins/<id>`; what the model made in this
//!   chat with `make_tool` and friends. Never asked about; gone with the chat.
//! - **workspace**: `<workspace>/.byob/plugins/<id>`, committed with the repo.
//! - **global**: `<config>/plugins/<id>` (plus `settings.json` `plugin_dirs` and `--plugin`).
//!
//! The engine sends [`ServerMsg::Extensions`](crate::ServerMsg::Extensions) for the chat a
//! client looks at after [`ServerMsg::ChatOpened`](crate::ServerMsg::ChatOpened), and again
//! whenever something changes (a reload, a switch, a setting, a server connecting). Setting
//! values never leave the engine: clients only learn whether each one is set.
//!
//! [`Extensions::slash`] lists what a message may start with (`/name args`): the engine
//! expands it when the turn starts (see [`SlashKind`]).

use serde::{Deserialize, Serialize};

use crate::workspace::{ConfigScope, Level};

/// Most characters of a plugin setting's value.
pub const MAX_SETTING_VALUE_CHARS: usize = 8192;
/// Most plugins one [`crate::ClientMsg::TrustPlugins`] may name.
pub const MAX_TRUSTED_AT_ONCE: usize = 200;

/// Where a plugin lives (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginScope {
    Global,
    Workspace,
    Chat,
}

impl PluginScope {
    pub fn label(self) -> &'static str {
        match self {
            Self::Global => "everywhere",
            Self::Workspace => "this workspace",
            Self::Chat => "this chat",
        }
    }
}

/// Who wrote a plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginOrigin {
    /// Ships with byob.
    Bundled,
    /// The user installed or wrote it (global plugins byob didn't make).
    User,
    /// byob made it: the model in a chat, possibly kept into a workspace or everywhere.
    Agent,
    /// It arrived with the workspace folder (a clone or pull). Its tools and servers wait
    /// for the user's OK ([`PluginInfo::trusted`]).
    Repo,
}

/// The state of one of a plugin's MCP servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerState {
    /// Starting in the background; its tools come in a later round.
    Connecting,
    Ready,
    Failed,
    /// Not started: the plugin is off, not trusted, or misses a setting.
    Off,
}

/// One of a plugin's MCP servers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    pub state: ServerState,
    /// Tools it offers (when ready).
    #[serde(default)]
    pub tools: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A plugin's long-lived backend process (13.2+; see `docs/plugins-backend.md`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendInfo {
    /// `idle` (starts on first use), `starting`, `running`, `crashed` (restarting),
    /// `failed` (given up until the plugin reloads), `stopped`, `off` (missing settings),
    /// or a state a newer engine knows.
    pub state: String,
    /// Times it was started in this engine run.
    #[serde(default)]
    pub starts: u32,
    /// Why it isn't running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The newest problems it reported (`fault`) or byob found in its output.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub faults: Vec<String>,
    /// The end of its stderr (≤ 4 KiB).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stderr: String,
}

/// A value a plugin asks the user for (an API key, a folder). The value itself never
/// reaches clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginSetting {
    /// The environment variable it becomes, e.g. `WEATHER_API_KEY`.
    pub key: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    /// Kept in secrets.json; show a password field.
    pub secret: bool,
    pub required: bool,
    /// It has a value (the user's or a default).
    pub set: bool,
}

/// One plugin a chat can see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    /// A lucide icon name.
    #[serde(default)]
    pub icon: String,
    pub scope: PluginScope,
    pub origin: PluginOrigin,
    /// Its folder on the engine's machine, for display.
    pub dir: String,
    /// Switched on for this chat (the plugin switches, global → workspace → chat).
    pub enabled: bool,
    /// The level whose switch decides [`Self::enabled`] (`Default`: nobody switched it; on).
    pub enabled_from: Level,
    /// Its tools and servers may run. False for a [`PluginOrigin::Repo`] plugin until the
    /// user trusts it ([`crate::ClientMsg::TrustPlugins`]); until then only its skills,
    /// prompt line and subagents are used.
    pub trusted: bool,
    /// The user declined to trust it (not asked again).
    #[serde(default)]
    pub declined: bool,
    /// A plugin with the same id in a more specific scope is used instead of this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadowed_by: Option<PluginScope>,
    /// Tool names as the model sees them (Python tools and MCP tools), when in use.
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    /// Subagent names.
    #[serde(default)]
    pub agents: Vec<String>,
    /// Slash commands (`commands/*.md`, Claude Code's format).
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub servers: Vec<ServerInfo>,
    /// Its backend process, if it declares one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<BackendInfo>,
    #[serde(default)]
    pub settings: Vec<PluginSetting>,
    /// Its tools may use the internet (`permissions.network`).
    #[serde(default)]
    pub network: bool,
    /// What doesn't work (a missing script, a bad skill, a missing setting); the parts that
    /// work are still used. Messages are the loader's, for the plugin's author.
    #[serde(default)]
    pub problems: Vec<String>,
    /// The last failed run of one of its tools in this engine run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Changes whenever a file of the plugin changes (hex).
    #[serde(default)]
    pub fingerprint: String,
}

/// A plugin folder that couldn't load at all (bad manifest, duplicate id).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginLoadError {
    pub scope: PluginScope,
    /// The folder.
    pub path: String,
    pub message: String,
}

/// Every plugin the chat in view can see (also the new chat a client is about to start).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extensions {
    /// `None`: the new chat this client is about to start.
    pub chat: Option<String>,
    /// Its workspace, if registered.
    pub workspace: Option<String>,
    /// Chat scope first, then workspace, then global; by id within a scope.
    pub plugins: Vec<PluginInfo>,
    #[serde(default)]
    pub errors: Vec<PluginLoadError>,
    /// What the composer's `/` menu offers: built-in commands, then plugin commands, then
    /// skills (the same name only once).
    #[serde(default)]
    pub slash: Vec<SlashItem>,
}

/// What a `/name` at the start of a message is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlashKind {
    /// Built into byob (`/compact`).
    Builtin,
    /// A plugin's `commands/<name>.md`: its prompt template, with the arguments filled in,
    /// is what the model gets.
    Command,
    /// A skill: the model is told to use it, the arguments are the task.
    Skill,
    /// A kind a newer engine sends that this client doesn't know (offered like a command).
    #[serde(other)]
    Other,
}

/// One entry of the `/` menu.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlashItem {
    /// What follows the `/`.
    pub name: String,
    pub kind: SlashKind,
    #[serde(default)]
    pub description: String,
    /// What to type after the name, e.g. `<file> [focus]`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub hint: String,
    /// Where it comes from (`None`: built in).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<PluginScope>,
    /// The plugin that brings it (`None`: built in, or a standalone skill).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
}

/// What a step did to a plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginAction {
    /// `make_tool`: a tool was added or replaced.
    Made,
    /// `edit_plugin` / `write_plugin_file`.
    Edited,
    /// `keep_plugin`: copied into a workspace or everywhere.
    Kept,
    /// `remove_plugin`.
    Removed,
    /// An action a newer engine reports that this client doesn't know ("changed").
    #[serde(other)]
    Other,
}

/// The result of running a tool once with test arguments (`make_tool`'s `try_args`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginTest {
    pub ok: bool,
    /// The output, or the end of the traceback (bounded).
    pub output: String,
}

/// A "plugin card" on a step that changed a plugin ([`crate::Step::plugin`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginCard {
    pub plugin: String,
    pub scope: PluginScope,
    pub action: PluginAction,
    /// The plugin's tools after the change (model-facing names).
    #[serde(default)]
    pub tools: Vec<String>,
    /// Tools that appeared or went away with this change.
    #[serde(default)]
    pub added: Vec<String>,
    #[serde(default)]
    pub removed: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test: Option<PluginTest>,
    /// The loader's problems after the change.
    #[serde(default)]
    pub problems: Vec<String>,
    /// Labels of required settings without a value (show the settings field).
    #[serde(default)]
    pub missing_settings: Vec<String>,
}

impl crate::ViewData for PluginCard {
    const KIND: &'static str = "byob.plugin_card";
    const V: u32 = 1;

    fn fallback(&self) -> String {
        let action = match self.action {
            PluginAction::Made => "tool made",
            PluginAction::Edited => "edited",
            PluginAction::Kept => "kept",
            PluginAction::Removed => "removed",
            PluginAction::Other => "changed",
        };
        let mut out = format!("**Plugin {}** ({}) · {action}", self.plugin, self.scope.label());
        if !self.tools.is_empty() {
            out.push_str(&format!("\n\nTools: {}", self.tools.join(", ")));
        }
        if let Some(t) = &self.test {
            out.push_str(if t.ok { "\n\nTest run passed" } else { "\n\nTest run failed" });
        }
        for p in &self.problems {
            out.push_str(&format!("\n\n⚠ {p}"));
        }
        out
    }
}

/// The plugin messages' size checks (see [`crate::ClientMsg::check_sizes`]).
pub(crate) fn check_sizes(msg: &crate::ClientMsg) -> Result<(), String> {
    use crate::ClientMsg;
    let id =
        |s: &str| if byob_types::config::plugin_id_ok(s) { Ok(()) } else { Err(format!("{:?} isn't a plugin id", s.chars().take(60).collect::<String>())) };
    match msg {
        ClientMsg::SetPluginEnabled { scope, plugin, .. } => {
            if let ConfigScope::Workspace { id: w } = scope
                && (w.is_empty() || w.len() > crate::workspace::MAX_ID_CHARS)
            {
                return Err("bad id".into());
            }
            id(plugin)
        }
        ClientMsg::SetPluginSetting { plugin, key, value } => {
            id(plugin)?;
            if key.is_empty() || key.len() > 64 {
                return Err("bad setting name".into());
            }
            if value.as_ref().is_some_and(|v| v.chars().count() > MAX_SETTING_VALUE_CHARS || v.contains('\0')) {
                return Err(format!("a setting's value can be at most {MAX_SETTING_VALUE_CHARS} characters"));
            }
            Ok(())
        }
        ClientMsg::KeepPlugin { plugin, new_id, .. } => {
            id(plugin)?;
            new_id.as_deref().map_or(Ok(()), id)
        }
        ClientMsg::RemovePlugin { plugin, .. } => id(plugin),
        ClientMsg::TrustPlugins { workspace, plugins, .. } => {
            if workspace.is_empty() || workspace.len() > crate::workspace::MAX_ID_CHARS {
                return Err("bad id".into());
            }
            if plugins.len() > MAX_TRUSTED_AT_ONCE {
                return Err("too many plugins".into());
            }
            plugins.iter().try_for_each(|p| id(p))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClientMsg, ServerMsg};

    fn info() -> PluginInfo {
        PluginInfo {
            id: "weather".into(),
            name: "Weather".into(),
            version: "0.1.0".into(),
            description: String::new(),
            icon: "puzzle".into(),
            scope: PluginScope::Chat,
            origin: PluginOrigin::Agent,
            dir: "/d/chats/c1/plugins/weather".into(),
            enabled: true,
            enabled_from: Level::Default,
            trusted: true,
            declined: false,
            shadowed_by: None,
            tools: vec!["weather".into()],
            skills: vec![],
            agents: vec![],
            commands: vec![],
            servers: vec![ServerInfo { name: "s".into(), state: ServerState::Connecting, tools: 0, error: None }],
            backend: None,
            settings: vec![PluginSetting { key: "API_KEY".into(), label: "API key".into(), help: None, secret: true, required: true, set: false }],
            network: true,
            problems: vec!["tools/x.py is missing".into()],
            last_error: Some("boom".into()),
            fingerprint: "00ff".into(),
        }
    }

    #[test]
    fn plugin_messages_round_trip_and_are_checked() {
        let ext = Extensions {
            chat: Some("c1".into()),
            workspace: Some("w1".into()),
            plugins: vec![info()],
            errors: vec![PluginLoadError { scope: PluginScope::Global, path: "/p".into(), message: "bad".into() }],
            slash: vec![SlashItem {
                name: "review".into(),
                kind: SlashKind::Command,
                description: "Review a file".into(),
                hint: "<file>".into(),
                scope: Some(PluginScope::Workspace),
                plugin: Some("code".into()),
            }],
        };
        let m = ServerMsg::Extensions(Box::new(ext));
        assert_eq!(ServerMsg::from_json(&m.to_json()).unwrap(), m);
        let ok = vec![
            ClientMsg::GetExtensions,
            ClientMsg::ReloadPlugins,
            ClientMsg::SetPluginEnabled { scope: ConfigScope::Chat, plugin: "weather".into(), enabled: Some(false) },
            ClientMsg::SetPluginEnabled { scope: ConfigScope::Workspace { id: "w1".into() }, plugin: "weather".into(), enabled: None },
            ClientMsg::SetPluginSetting { plugin: "weather".into(), key: "API_KEY".into(), value: Some("sk".into()) },
            ClientMsg::KeepPlugin { plugin: "scratch".into(), scope: PluginScope::Workspace, new_id: Some("weather".into()) },
            ClientMsg::RemovePlugin { plugin: "weather".into(), scope: PluginScope::Chat },
            ClientMsg::TrustPlugins { workspace: "w1".into(), plugins: vec![], trust: true },
        ];
        for m in ok {
            assert_eq!(ClientMsg::from_json(&m.to_json()).unwrap(), m);
            assert!(m.check_sizes().is_ok(), "{}", m.to_json());
        }
        for bad in [
            ClientMsg::SetPluginEnabled { scope: ConfigScope::Chat, plugin: "../x".into(), enabled: Some(true) },
            ClientMsg::SetPluginSetting { plugin: "weather".into(), key: "K".into(), value: Some("x".repeat(MAX_SETTING_VALUE_CHARS + 1)) },
            ClientMsg::KeepPlugin { plugin: "a".into(), scope: PluginScope::Global, new_id: Some("Bad".into()) },
            ClientMsg::TrustPlugins { workspace: "w1".into(), plugins: vec!["ok".into(); MAX_TRUSTED_AT_ONCE + 1], trust: true },
        ] {
            assert!(bad.check_sizes().is_err(), "{}", bad.to_json());
        }
        let card = PluginCard {
            plugin: "scratch".into(),
            scope: PluginScope::Chat,
            action: PluginAction::Made,
            tools: vec!["weather".into()],
            added: vec!["weather".into()],
            removed: vec![],
            test: Some(PluginTest { ok: false, output: "Traceback…".into() }),
            problems: vec![],
            missing_settings: vec!["API key".into()],
        };
        let json = serde_json::to_string(&card).unwrap();
        assert_eq!(serde_json::from_str::<PluginCard>(&json).unwrap(), card);
    }
}
