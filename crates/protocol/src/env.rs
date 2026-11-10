//! Workspace environments as clients see them: whether byob loads the workspace's own
//! environment (flake.nix, pixi.toml, …), and whether its commands run in a container.

use serde::{Deserialize, Serialize};

/// Ask for the [`WorkspaceEnv`] of the chat in view, optionally changing it first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EnvAction {
    /// Remember whether to load the workspace's project environment (`None`: forget the
    /// answer, so byob asks again next time).
    ProjectEnv { load: Option<bool> },
    /// Run this workspace's commands in a container (`None`: forget, ask again if it has a
    /// devcontainer.json).
    Container { on: Option<bool> },
    /// Stop the workspace's container (it starts again on the next command).
    StopContainer,
    /// Remove the workspace's container (packages installed in it are lost).
    RemoveContainer,
}

/// One workspace's environment settings and state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceEnv {
    pub workspace: String,
    /// Project files byob can load an environment from, e.g. `flake.nix`, `pixi.toml`.
    #[serde(default)]
    pub project_files: Vec<String>,
    /// The remembered answer: load it (`true`), don't (`false`), or ask (`None`).
    #[serde(default)]
    pub project_env: Option<bool>,
    /// `BYOB_WORKSPACE_ENV` overrides the answer (`off`/`on`), for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_env_forced: Option<bool>,
    /// What `install_packages` uses here ("Nix", "pixi"), if anything.
    #[serde(default)]
    pub installer: Option<String>,
    #[serde(default)]
    pub container: ContainerEnv,
    /// What the last [`EnvAction`] did, or why it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default)]
    pub failed: bool,
}

/// The container side of a [`WorkspaceEnv`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerEnv {
    /// "docker" or "podman", if one is installed.
    #[serde(default)]
    pub runtime: Option<String>,
    /// The workspace's `.devcontainer/devcontainer.json` (or `.devcontainer.json`), if any.
    #[serde(default)]
    pub devcontainer: Option<String>,
    /// The remembered choice: commands run in the container (`true`), on this machine
    /// (`false`), or not decided (`None`; asked if there's a devcontainer.json).
    #[serde(default)]
    pub use_container: Option<bool>,
    /// The image it uses (from devcontainer.json, else the default).
    #[serde(default)]
    pub image: String,
    /// The container's state ("running", "exited", …); `None` if there is none.
    #[serde(default)]
    pub state: Option<String>,
}
