//! What the model can use besides plugins: built-in file and artifact tools and the
//! code-running tools (sandboxed), registered once; the user's own subagents
//! (`<config>/agents/*.md`); and where the engine finds plugins (the config folder's
//! `plugins`, `settings.json` `plugin_dirs`, `--plugin`). The engine loads plugins itself,
//! per chat and workspace, and reloads them when their files change.

use std::path::PathBuf;
use std::sync::Arc;

use byob_agent::PluginsConfig;
use byob_exec::{Exec, Mode};
use byob_plugins::AgentDef;
use byob_tools::Registry;

/// What [`load`] found besides tools.
pub struct Loaded {
    /// Text to append to the system prompt (artifacts, code running).
    pub prompt: String,
    /// The user's own subagents (`<config>/agents`); plugins' come per chat.
    pub agents: Vec<AgentDef>,
    /// Workspace environment settings for the engine (with run_command / run_python).
    pub envs: Option<Arc<dyn byob_tools::EnvControl>>,
    /// Where the engine finds plugins.
    pub plugins: PluginsConfig,
}

/// Register the built-in tools into `tools`; `linked`: `--plugin` folders.
pub fn load(tools: &mut Registry, linked: Vec<PathBuf>) -> Loaded {
    byob_tools::register_builtins(tools);

    let paths = byob_store::Paths::resolve();
    // run_command / run_python (+ install_packages with Nix or pixi, else
    // install_python_packages with uv; install_system_packages where brew/winget is the
    // last resort; containers with Docker/Podman). The sandbox helper is this binary
    // (`byob __sandbox-exec`, see main).
    let mode = Mode::from_env();
    let mut exec_prompt = None;
    let mut envs: Option<Arc<dyn byob_tools::EnvControl>> = None;
    let settings = paths.as_ref().map(|p| byob_store::Settings::load(&p.settings()).value).unwrap_or_default();
    match &mode {
        Ok(mode) => {
            let data = paths.as_ref().ok().map(|p| p.data_dir.clone());
            let containers = settings.containers.clone();
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

    let mut plugins = PluginsConfig { linked, python: byob_plugins::default_python(), sandbox, ..Default::default() };
    match &paths {
        Ok(paths) => {
            plugins.global_roots.push(paths.config_dir.join("plugins"));
            plugins.global_roots.extend(settings.plugin_dirs.iter().cloned());
            plugins.global_skills = Some(paths.config_dir.join("skills"));
            plugins.scratch_root = Some(paths.data_dir.join("chats"));
        }
        Err(e) => log::warn!("no config folder ({e}); only --plugin folders are loaded"),
    }

    let mut prompt = byob_tools::artifact::PROMPT.to_owned();
    if let Some(p) = exec_prompt {
        prompt.push_str("\n\n");
        prompt.push_str(&p);
    }
    let agents = match &paths {
        Ok(paths) => {
            let (agents, errors) = byob_plugins::load_agents_dir(&paths.config_dir.join("agents"));
            for e in errors {
                log::warn!("agent {e}");
            }
            agents
        }
        Err(_) => Vec::new(),
    };
    if !agents.is_empty() {
        log::info!("{} subagent(s) of your own: {}", agents.len(), agents.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", "));
    }
    Loaded { prompt: prompt.trim().to_owned(), agents, envs, plugins }
}
