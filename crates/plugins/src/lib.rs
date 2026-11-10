//! Plugins: a folder with a `byob-plugin.json` that adds any mix of
//!
//! - **skills** (`skills/<name>/SKILL.md`, the Agent Skills format) → [`skills`],
//!   [`UseSkillTool`], [`skills_prompt`];
//! - **Python tools** (`tools[].run`) → [`python::PythonTool`];
//! - **subagents** (`agents/*.md`, Claude Code's format) → [`agents`];
//! - **slash commands** (`commands/*.md`, Claude Code's format) → [`commands`];
//! - **connectors** (`mcpServers`, MCP) → [`connectors::ConnectorToolAdapter`] over the
//!   [`connectors::Connector`] trait, which `byob-mcp`'s client implements.
//!
//! Typical use by the agent:
//! 1. [`discover`] the plugin folders; show `errors` and each plugin's `problems`.
//! 2. Reserve the built-in tool names in a [`ToolNames`], then add each active plugin's
//!    [`python_tools`] and, per `mcpServers` entry, [`connector_tools`].
//! 3. Add [`UseSkillTool`] over [`Discovery::skills`] and put [`skills_prompt`] and
//!    [`Discovery::prompt`] in the system prompt.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

pub mod agents;
pub mod authoring;
pub mod commands;
pub mod connectors;
pub mod discover;
pub mod manifest;
pub mod mcp;
pub mod names;
pub mod python;
pub mod skills;

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

pub use agents::{AgentDef, load_agents_dir, merge_agents, parse_agent};
pub use commands::{Command, load_commands_dir, split_slash};
pub use connectors::{
    Connector, ConnectorContent, ConnectorDraft, ConnectorResult, ConnectorTool, ConnectorToolAdapter, connector_tools, connector_tools_from, parse_connector,
};
pub use discover::{Discovery, Plugin, PluginSources, discover, load_plugin};
pub use manifest::{MANIFEST, Manifest, ManifestError, McpServerConfig, OLD_MANIFEST, parse_manifest};
pub use names::ToolNames;
pub use python::{PythonTool, default_python, python_tools, python_tools_in, run_script_sandboxed, run_script_unsandboxed};
pub use skills::{SaveSkillTool, Skill, UseSkillTool, load_skills_dir, skills_prompt};

/// Something that couldn't load, for the user (`path`: the plugin or skill folder).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadError {
    pub path: PathBuf,
    pub message: String,
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.message)
    }
}

impl std::error::Error for LoadError {}

/// What a plugin's tools need to know about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRef {
    pub id: String,
    pub name: String,
    pub dir: PathBuf,
    /// `permissions.network`.
    pub network: bool,
    /// Setting values (defaults filled in), passed as environment variables.
    pub env: BTreeMap<String, String>,
}

impl PluginRef {
    /// `settings`: the user's values by key; declared defaults fill the gaps.
    pub fn new(plugin: &Plugin, settings: &BTreeMap<String, String>) -> Self {
        let mut env: BTreeMap<String, String> = plugin.manifest.settings.iter().filter_map(|s| Some((s.key.clone(), s.default.clone()?))).collect();
        for s in &plugin.manifest.settings {
            if let Some(v) = settings.get(&s.key) {
                env.insert(s.key.clone(), v.clone());
            }
        }
        Self { id: plugin.manifest.id.clone(), name: plugin.manifest.name.clone(), dir: plugin.dir.clone(), network: plugin.manifest.permissions.network, env }
    }

    /// Required settings that have no value yet (labels), e.g. to show "Fill in API key".
    pub fn missing_settings(plugin: &Plugin, settings: &BTreeMap<String, String>) -> Vec<String> {
        plugin
            .manifest
            .settings
            .iter()
            .filter(|s| s.required && settings.get(&s.key).or(s.default.as_ref()).is_none_or(|v| v.trim().is_empty()))
            .map(|s| s.label.clone())
            .collect()
    }
}
