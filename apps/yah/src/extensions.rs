//! Load what the model can use: built-in file tools, then plugins (skills, Python tools,
//! MCP connectors) from the config folder, `settings.json` `plugin_dirs`, and `--plugin`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use yah_plugins::mcp::McpConnector;
use yah_plugins::{PluginSources, ToolNames, UseSkillTool};
use yah_tools::Registry;

/// Register everything into `tools`; returns text to append to the system prompt.
pub fn load(tools: &mut Registry, linked: Vec<PathBuf>) -> String {
    yah_tools::register_builtins(tools);

    let mut sources = PluginSources { linked, ..Default::default() };
    match yah_store::Paths::resolve() {
        Ok(paths) => {
            sources.roots.push(paths.config_dir.join("plugins"));
            // Settings::load logs its own warnings.
            sources.roots.extend(yah_store::Settings::load(&paths.settings()).value.plugin_dirs);
        }
        Err(e) => log::warn!("no config folder ({e}); only --plugin folders are loaded"),
    }

    let found = yah_plugins::discover(&sources);
    for e in &found.errors {
        log::warn!("plugin {}: {}", e.path.display(), e.message);
    }
    let mut names = ToolNames::new(tools.specs().into_iter().map(|s| s.name));
    let no_settings = BTreeMap::new();
    let python = yah_plugins::default_python();
    for plugin in found.active() {
        for p in &plugin.problems {
            log::warn!("plugin {}: {p}", plugin.id());
        }
        for tool in yah_plugins::python_tools(plugin, &no_settings, &python, &mut names) {
            tools.register(tool);
        }
        let plugin_ref = yah_plugins::PluginRef::new(plugin, &no_settings);
        for (server, config) in &plugin.manifest.mcp_servers {
            let config = config.with_settings(&plugin_ref.env);
            let connected = McpConnector::connect(&config, Some(&plugin.dir))
                .and_then(|c| yah_plugins::connector_tools(&plugin_ref, config.is_local(), Arc::new(c), &mut names));
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
    let mut prompt = found.prompt();
    if !skills.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(&yah_plugins::skills_prompt(&skills));
        tools.register(Arc::new(UseSkillTool::new(skills)));
    }
    prompt.trim().to_owned()
}
