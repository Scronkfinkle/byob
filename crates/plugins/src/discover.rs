//! Finding plugins on disk: folders with a `byob-plugin.json`,
//! their `skills/*/SKILL.md`, and checks that declared scripts exist. Nothing here is
//! fatal: a broken plugin becomes a [`LoadError`] and the rest still load.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::LoadError;
use crate::agents::{AgentDef, load_agents_dir};
use crate::manifest::{MANIFEST, Manifest, OLD_MANIFEST, ParseOptions, ToolKind, parse_manifest_str};
use crate::skills::{Skill, load_skills_dir, read_capped};

/// Biggest manifest we read.
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

/// A loaded plugin.
#[derive(Debug, Clone, PartialEq)]
pub struct Plugin {
    pub manifest: Manifest,
    pub dir: PathBuf,
    /// Ships with the app (may use `native` tools).
    pub builtin: bool,
    pub skills: Vec<Skill>,
    /// Subagent definitions from its `agents/*.md` (Claude Code format).
    pub agents: Vec<AgentDef>,
    /// Non-fatal problems for the plugin's card (a bad skill, a missing script). The parts
    /// that work are still used.
    pub problems: Vec<String>,
}

impl Plugin {
    pub fn id(&self) -> &str {
        &self.manifest.id
    }

    /// The manifest's `platforms` include this system (or it has none).
    pub fn supported_here(&self) -> bool {
        self.manifest.platforms.as_ref().is_none_or(|p| p.iter().any(|x| x == current_platform()))
    }

    /// Names of the app's own tools this (built-in) plugin turns on.
    pub fn native_tools(&self) -> Vec<&str> {
        if !self.builtin {
            return Vec::new();
        }
        self.manifest
            .tools
            .iter()
            .filter_map(|t| match &t.kind {
                ToolKind::Native(n) => Some(n.as_str()),
                ToolKind::Script(_) => None,
            })
            .collect()
    }
}

/// This system as Node's `process.platform` names it (what manifests use).
pub fn current_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

/// The manifest file in `dir`, if any (`byob-plugin.json`).
pub fn manifest_path(dir: &Path) -> Option<PathBuf> {
    let file = dir.join(MANIFEST);
    if file.is_file() {
        return Some(file);
    }
    if dir.join(OLD_MANIFEST).is_file() {
        log::warn!("{}: rename {OLD_MANIFEST} to {MANIFEST}; the old name isn't read any more", dir.display());
    }
    None
}

/// Load the plugin in `dir`. `None` when there's no manifest there.
pub fn load_plugin(dir: &Path, builtin: bool) -> Option<Result<Plugin, LoadError>> {
    let file = manifest_path(dir)?;
    let file_name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| MANIFEST.into());
    let err = |message: String| LoadError { path: dir.to_path_buf(), message };
    let text = match read_capped(&file, MAX_MANIFEST_BYTES) {
        Ok(t) => t,
        Err(e) => return Some(Err(err(e))),
    };
    let manifest = match parse_manifest_str(&text, ParseOptions { builtin, file: Some(&file_name) }) {
        Ok(m) => m,
        Err(e) => return Some(Err(err(e.to_string()))),
    };
    let mut problems = Vec::new();
    for t in &manifest.tools {
        if let ToolKind::Script(run) = &t.kind
            && !dir.join(run).is_file()
        {
            problems.push(format!("{file_name}: {run} is missing"));
        }
    }
    let (skills, skill_errors) = load_skills_dir(&dir.join("skills"));
    problems.extend(skill_errors.into_iter().map(|e| e.message));
    let (agents, agent_errors) = load_agents_dir(&dir.join("agents"));
    problems.extend(agent_errors.into_iter().map(|e| e.to_string()));
    Some(Ok(Plugin { manifest, dir: dir.to_path_buf(), builtin, skills, agents, problems }))
}

/// Where to look for plugins.
#[derive(Debug, Clone, Default)]
pub struct PluginSources {
    /// Folders whose subfolders are built-in plugins.
    pub builtin_roots: Vec<PathBuf>,
    /// Folders whose subfolders are installed plugins, e.g. `<config>/plugins`.
    pub roots: Vec<PathBuf>,
    /// Plugin folders linked one by one (a plugin under development).
    pub linked: Vec<PathBuf>,
}

/// Everything found.
#[derive(Debug, Clone, Default)]
pub struct Discovery {
    /// In load order: built-in, installed, linked. Ids are unique.
    pub plugins: Vec<Plugin>,
    /// Plugins that couldn't load at all.
    pub errors: Vec<LoadError>,
}

impl Discovery {
    /// Plugins that apply to this system.
    pub fn active(&self) -> impl Iterator<Item = &Plugin> {
        self.plugins.iter().filter(|p| p.supported_here())
    }

    /// Skills of active plugins; the first plugin to define a name wins.
    pub fn skills(&self) -> Vec<Skill> {
        let mut seen = BTreeMap::new();
        for s in self.active().flat_map(|p| &p.skills) {
            seen.entry(s.name.clone()).or_insert_with(|| s.clone());
        }
        seen.into_values().collect()
    }

    /// Subagents of active plugins; the first plugin to define a name wins.
    pub fn agents(&self) -> Vec<AgentDef> {
        crate::agents::merge_agents(self.active().map(|p| p.agents.clone()))
    }

    /// The `prompt` lines of active plugins, one per line.
    pub fn prompt(&self) -> String {
        self.active().filter_map(|p| p.manifest.prompt.as_deref()).collect::<Vec<_>>().join("\n")
    }
}

/// Non-hidden subfolders of `dir`, sorted. Missing or unreadable → empty.
pub(crate) fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<PathBuf> = rd.flatten().filter(|e| !e.file_name().to_string_lossy().starts_with('.')).map(|e| e.path()).filter(|p| p.is_dir()).collect();
    out.sort();
    out
}

/// Scan every source. Folders without a manifest are skipped silently.
pub fn discover(sources: &PluginSources) -> Discovery {
    let mut d = Discovery::default();
    let mut ids: BTreeMap<String, PathBuf> = BTreeMap::new();
    let candidates = sources
        .builtin_roots
        .iter()
        .flat_map(|r| subdirs(r).into_iter().map(|p| (p, true)))
        .chain(sources.roots.iter().flat_map(|r| subdirs(r).into_iter().map(|p| (p, false))))
        .chain(sources.linked.iter().map(|p| (p.clone(), false)));
    for (dir, builtin) in candidates {
        match load_plugin(&dir, builtin) {
            None => {}
            Some(Err(e)) => d.errors.push(e),
            Some(Ok(p)) => {
                if let Some(first) = ids.get(p.id()) {
                    d.errors.push(LoadError { path: dir, message: format!("id \"{}\" is already used by {}", p.id(), first.display()) });
                } else {
                    ids.insert(p.id().to_string(), dir);
                    d.plugins.push(p);
                }
            }
        }
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_names_match_node() {
        let p = current_platform();
        assert!(!p.is_empty() && p != "macos" && p != "windows");
    }

    #[test]
    fn plugins_bring_agents() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("helpers");
        fs::create_dir_all(dir.join("agents")).unwrap();
        fs::write(dir.join(MANIFEST), r#"{"id":"helpers","name":"Helpers","version":"1.0.0"}"#).unwrap();
        fs::write(dir.join("agents/explorer.md"), "---\nname: explorer\ndescription: Finds things\ntools: Read, Glob\n---\nExplore.").unwrap();
        fs::write(dir.join("agents/broken.md"), "---\nname: broken\n---\n").unwrap();
        let d = discover(&PluginSources { roots: vec![root.path().to_path_buf()], ..Default::default() });
        assert!(d.errors.is_empty(), "{:?}", d.errors);
        let agents = d.agents();
        assert_eq!(agents.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["explorer"]);
        assert_eq!(d.plugins[0].problems.len(), 1, "the broken agent is a problem, not an error");
    }

    #[test]
    fn missing_roots_are_empty() {
        let d = discover(&PluginSources { roots: vec![PathBuf::from("/definitely/not/here")], ..Default::default() });
        assert!(d.plugins.is_empty() && d.errors.is_empty());
    }
}
