//! Workspaces as clients see them, layered settings (global → workspace → chat), and the
//! engine's folder browser for picking a workspace folder from a browser.
//!
//! The settings types and their resolver live in [`byob_types::config`], so the engine and
//! the UI resolve the same way.

use serde::{Deserialize, Serialize};

pub use byob_types::config::{Approvals, Effective, EnvKind, Field, Layer, Level, Sourced, ToolMode};

use crate::{ClientMsg, Link};

/// Longest id (chat, workspace) a client may send.
pub const MAX_ID_CHARS: usize = 128;
/// Longest workspace name a client may send (the engine trims it to 80 characters).
pub const MAX_NAME_CHARS: usize = 200;
/// Longest folder path a client may send.
pub const MAX_PATH_CHARS: usize = 4096;
/// Most folders listed by one [`ServerMsg::Folders`](crate::ServerMsg::Folders).
pub const MAX_LISTED_FOLDERS: usize = 500;

/// A workspace in the chat list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSummary {
    pub id: String,
    pub name: String,
    /// Its folder on the engine's machine.
    pub folder: String,
    /// How many chats it has.
    #[serde(default)]
    pub chats: u64,
    /// Latest activity of its chats, else when it was created (ms since the epoch).
    #[serde(default)]
    pub updated_at: i64,
    /// Its environment setting, for a small marker in the list.
    #[serde(default)]
    pub env: EnvKind,
}

/// Which level's settings a [`ClientMsg::GetConfig`] / [`ClientMsg::SetConfig`] means.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConfigScope {
    /// `settings.json` (the endpoint itself changes with [`ClientMsg::SetInference`]).
    Global,
    Workspace {
        id: String,
    },
    /// The chat this client looks at, or the new chat it is about to start.
    Chat,
}

/// One level's settings, what applies there, and where each value comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigView {
    pub scope: ConfigScope,
    /// What this level sets itself.
    pub own: Layer,
    /// What applies at this level (to the chat, or to new chats of the workspace).
    pub effective: Effective,
    /// This level stores its own API key (the key itself never leaves the engine).
    #[serde(default)]
    pub has_key: bool,
    /// For the heading: the workspace's or chat's name.
    #[serde(default)]
    pub title: String,
    /// The workspace this level belongs to (none for global).
    #[serde(default)]
    pub workspace: Option<WorkspaceSummary>,
}

/// Where a new workspace's folder comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NewFolder {
    /// A folder that exists on the engine's machine.
    Existing { path: String },
    /// A fresh folder named after the workspace, in the workspaces folder (settings.json
    /// `workspaces_root`, default `~/byob-workspaces`).
    Create,
}

/// A folder's subfolders on the engine's machine (answer to [`ClientMsg::ListFolders`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderList {
    /// The folder listed (absolute, canonical).
    pub path: String,
    #[serde(default)]
    pub parent: Option<String>,
    /// Names of its subfolders (hidden ones left out), sorted, at most [`MAX_LISTED_FOLDERS`].
    #[serde(default)]
    pub dirs: Vec<String>,
    /// More subfolders than listed.
    #[serde(default)]
    pub truncated: bool,
    /// Why it couldn't be listed (`path` is then what was asked for).
    #[serde(default)]
    pub error: Option<String>,
}

/// The endpoint of the chat in view, when its workspace or the chat itself picked another
/// one than the global settings.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatEndpoint {
    pub base_url: String,
    pub model: String,
    /// Which level chose it.
    pub from: Level,
    pub has_api_key: bool,
    #[serde(default)]
    pub link: Link,
    #[serde(default)]
    pub info: Option<crate::EndpointInfo>,
}

fn long(s: &str, max: usize) -> bool {
    s.chars().count() > max
}

impl ClientMsg {
    /// Cheap checks of an untrusted message's sizes (the engine checks meaning: unknown
    /// ids, folders that don't exist, settings that don't belong at a level).
    pub fn check_sizes(&self) -> Result<(), String> {
        let id = |s: &str| if long(s, MAX_ID_CHARS) || s.is_empty() { Err("bad id".to_owned()) } else { Ok(()) };
        let name = |s: &str| if long(s, MAX_NAME_CHARS) { Err("the name is too long".to_owned()) } else { Ok(()) };
        let path = |s: &str| if long(s, MAX_PATH_CHARS) { Err("the path is too long".to_owned()) } else { Ok(()) };
        match self {
            Self::NewChat { workspace: Some(w) } => id(w),
            Self::CreateWorkspace { name: n, folder, .. } => {
                name(n)?;
                match folder {
                    NewFolder::Existing { path: p } => path(p),
                    NewFolder::Create => Ok(()),
                }
            }
            Self::RenameWorkspace { id: i, name: n } => id(i).and_then(|()| name(n)),
            Self::RemoveWorkspace { id: i, .. } => id(i),
            Self::GetConfig { scope: ConfigScope::Workspace { id: i } } | Self::SetConfig { scope: ConfigScope::Workspace { id: i }, .. } => id(i),
            Self::ListFolders { path: Some(p) } => path(p),
            Self::WorkspaceEnv { workspace: Some(w), .. } => id(w),
            _ => crate::queue::check_sizes(self),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ServerMsg;

    #[test]
    fn workspace_messages_round_trip() {
        let layer = Layer { env: Some(EnvKind::Container), image: Some("python:3-slim".into()), ..Layer::default() };
        let msgs = vec![
            ClientMsg::NewChat { workspace: Some("w1".into()) },
            ClientMsg::CreateWorkspace { name: "Data".into(), folder: NewFolder::Create, config: layer.clone() },
            ClientMsg::CreateWorkspace { name: "Repo".into(), folder: NewFolder::Existing { path: "/home/me/repo".into() }, config: Layer::default() },
            ClientMsg::RenameWorkspace { id: "w1".into(), name: "New".into() },
            ClientMsg::RemoveWorkspace { id: "w1".into(), clean: true },
            ClientMsg::GetConfig { scope: ConfigScope::Chat },
            ClientMsg::SetConfig { scope: ConfigScope::Workspace { id: "w1".into() }, layer: layer.clone(), api_key: None },
            ClientMsg::ListFolders { path: None },
            ClientMsg::CheckContainers,
            ClientMsg::WorkspaceEnv { action: None, workspace: Some("w1".into()) },
        ];
        for m in msgs {
            assert_eq!(ClientMsg::from_json(&m.to_json()).unwrap(), m, "{}", m.to_json());
            assert!(m.check_sizes().is_ok());
        }
        // Older clients' messages still parse.
        assert_eq!(ClientMsg::from_json(r#"{"type":"new_chat"}"#).unwrap(), ClientMsg::NewChat { workspace: None });
        assert_eq!(ClientMsg::from_json(r#"{"type":"workspace_env"}"#).unwrap(), ClientMsg::WorkspaceEnv { action: None, workspace: None });
        let ws = WorkspaceSummary { id: "w1".into(), name: "Data".into(), folder: "/d".into(), chats: 2, updated_at: 5, env: EnvKind::Nix };
        let effective = byob_types::config::resolve(Default::default(), Default::default(), Default::default());
        let view = ConfigView { scope: ConfigScope::Global, own: layer, effective, has_key: false, title: "Defaults".into(), workspace: Some(ws.clone()) };
        let msgs = vec![
            ServerMsg::Workspaces { items: vec![ws], current: Some("w1".into()), root: "/home/me/byob-workspaces".into(), support: Default::default() },
            ServerMsg::Config(Box::new(view)),
            ServerMsg::Folders(FolderList { path: "/".into(), parent: None, dirs: vec!["home".into()], truncated: false, error: None }),
            ServerMsg::ContainersChecked { ok: false, message: "Docker isn't running".into() },
            ServerMsg::WorkspaceCreated { id: None, error: Some("no".into()) },
            ServerMsg::ChatOpened { chat: None, workspace: Some("w1".into()) },
        ];
        for m in msgs {
            assert_eq!(ServerMsg::from_json(&m.to_json()).unwrap(), m);
        }
        assert_eq!(ServerMsg::from_json(r#"{"type":"chat_opened","chat":null}"#).unwrap(), ServerMsg::ChatOpened { chat: None, workspace: None });
    }

    #[test]
    fn oversized_fields_are_refused() {
        let huge = "x".repeat(MAX_PATH_CHARS + 1);
        for m in [
            ClientMsg::CreateWorkspace { name: "n".into(), folder: NewFolder::Existing { path: huge.clone() }, config: Layer::default() },
            ClientMsg::CreateWorkspace { name: huge.clone(), folder: NewFolder::Create, config: Layer::default() },
            ClientMsg::RenameWorkspace { id: huge.clone(), name: "n".into() },
            ClientMsg::RemoveWorkspace { id: String::new(), clean: false },
            ClientMsg::ListFolders { path: Some(huge.clone()) },
            ClientMsg::NewChat { workspace: Some(huge.clone()) },
            ClientMsg::GetConfig { scope: ConfigScope::Workspace { id: huge.clone() } },
        ] {
            assert!(m.check_sizes().is_err(), "{m:?}");
        }
    }
}
