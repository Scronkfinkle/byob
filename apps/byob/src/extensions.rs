//! Load what the model can use: built-in file and artifact tools, the code-running tools (sandboxed),
//! then plugins (skills, Python tools, MCP connectors, subagents) from the config folder,
//! `settings.json` `plugin_dirs`, and `--plugin`; and the user's own subagents
//! (`<config>/agents/*.md`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use byob_exec::{Exec, Mode};
use byob_plugins::mcp::{McpConnector, McpSandbox};
use byob_plugins::{AgentDef, PluginSources, ToolNames, UseSkillTool};
use byob_tools::Registry;

/// What [`load`] found besides tools.
pub struct Loaded {
    /// Text to append to the system prompt.
    pub prompt: String,
    /// Subagents from `<config>/agents` and plugins, the user's first.
    pub agents: Vec<AgentDef>,
    /// Workspace environment settings for the engine (with run_command / run_python).
    pub envs: Option<Arc<dyn byob_tools::EnvControl>>,
}

/// Register everything into `tools`.
pub fn load(tools: &mut Registry, linked: Vec<PathBuf>, workspace: &Path) -> Loaded {
    byob_tools::register_builtins(tools);

    let paths = byob_store::Paths::resolve();
    // run_command / run_python (+ install_packages with Nix or pixi, else
    // install_python_packages with uv; install_system_packages where brew/winget is the
    // last resort; containers with Docker/Podman). The sandbox helper is this binary
    // (`byob __sandbox-exec`, see main).
    let mode = Mode::from_env();
    let mut exec_prompt = None;
    let mut envs: Option<Arc<dyn byob_tools::EnvControl>> = None;
    let containers = paths.as_ref().map(|p| byob_store::Settings::load(&p.settings()).value.containers).unwrap_or_default();
    match &mode {
        Ok(mode) => {
            let data = paths.as_ref().ok().map(|p| p.data_dir.clone());
            let exec = Exec::with_containers(mode.clone(), data, containers.image, containers.runtime.as_deref());
            exec_prompt = exec.prompt();
            envs = Some(exec.envs.clone());
            byob_exec::register(tools, exec);
        }
        Err(e) => log::warn!("no run_command/run_python: {e}"),
    }
    // Plugin scripts and local MCP servers go through the same sandbox (unless turned off).
    let sandbox = match mode {
        Ok(Mode::Sandboxed(sb)) => Some(sb),
        _ => None,
    };

    let mut sources = PluginSources { linked, ..Default::default() };
    match &paths {
        Ok(paths) => {
            sources.roots.push(paths.config_dir.join("plugins"));
            // Settings::load logs its own warnings.
            sources.roots.extend(byob_store::Settings::load(&paths.settings()).value.plugin_dirs);
        }
        Err(e) => log::warn!("no config folder ({e}); only --plugin folders are loaded"),
    }

    let found = byob_plugins::discover(&sources);
    for e in &found.errors {
        log::warn!("plugin {}: {}", e.path.display(), e.message);
    }
    let mut names = ToolNames::new(tools.specs().into_iter().map(|s| s.name));
    let no_settings = BTreeMap::new();
    let python = byob_plugins::default_python();
    let mcp_sandbox = sandbox.clone().map(|sandbox| McpSandbox { sandbox, workspace: workspace.to_path_buf() });
    for plugin in found.active() {
        for p in &plugin.problems {
            log::warn!("plugin {}: {p}", plugin.id());
        }
        for tool in byob_plugins::python_tools_in(plugin, &no_settings, &python, &mut names, sandbox.as_ref()) {
            tools.register(tool);
        }
        let plugin_ref = byob_plugins::PluginRef::new(plugin, &no_settings);
        for (server, config) in &plugin.manifest.mcp_servers {
            let config = config.with_settings(&plugin_ref.env);
            let connected = McpConnector::connect_in(&config, Some(&plugin.dir), mcp_sandbox.as_ref())
                .and_then(|c| byob_plugins::connector_tools(&plugin_ref, config.is_local(), Arc::new(c), &mut names));
            match connected {
                Ok(list) => {
                    log::info!("plugin {}: MCP server {server}: {} tools", plugin.id(), list.len());
                    for tool in list {
                        tools.register(tool);
                    }
                }
                Err(e) => log::warn!("plugin {}: MCP server {server}: {e}", plugin.id()),
            }
        }
        log::info!("plugin {} loaded", plugin.id());
    }

    let skills = found.skills();
    let mut prompt = format!("{}\n\n{}", byob_tools::artifact::PROMPT, found.prompt());
    if let Some(p) = exec_prompt {
        prompt.push_str("\n\n");
        prompt.push_str(&p);
    }
    if !skills.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(&byob_plugins::skills_prompt(&skills));
        tools.register(Arc::new(UseSkillTool::new(skills)));
    }
    let mine = match &paths {
        Ok(paths) => {
            let (agents, errors) = byob_plugins::load_agents_dir(&paths.config_dir.join("agents"));
            for e in errors {
                log::warn!("agent {e}");
            }
            agents
        }
        Err(_) => Vec::new(),
    };
    let agents = byob_plugins::merge_agents([mine, found.agents()]);
    if !agents.is_empty() {
        log::info!("{} subagent(s): {}", agents.len(), agents.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", "));
    }
    Loaded { prompt: prompt.trim().to_owned(), agents, envs }
}
