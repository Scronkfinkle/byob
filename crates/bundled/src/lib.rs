//! byob's bundled plugins: the built-in features (artifacts, HTML apps, charts, markdown,
//! images, the file and code-running tools, the system prompt, compaction, subagents,
//! workspace environments, web search, memory, the plan and questions to the user) as plugins on the same manifest third parties
//! use. Their sources are `plugins/<id>/` in this crate, embedded at build time (`build.rs`),
//! so `cargo install` needs no extra files. Their native parts (tools, artifact types,
//! renderers, prompt builders) are in-tree Rust, named by id in the manifests and resolved
//! by the engine and the UI against registries the app fills at startup.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

/// One file of a bundled plugin.
#[derive(Debug)]
pub struct File {
    /// The plugin's folder name (its id).
    pub plugin: &'static str,
    /// Path inside the plugin, `/`-separated (e.g. `byob-plugin.json`, `guidance.md`).
    pub path: &'static str,
    pub text: &'static str,
}

include!(concat!(env!("OUT_DIR"), "/bundled.rs"));

/// The bundled plugins, by id, each with its files (path, text), sorted.
pub fn plugins() -> Vec<(&'static str, Vec<(&'static str, &'static str)>)> {
    let mut out: Vec<(&'static str, Vec<(&'static str, &'static str)>)> = Vec::new();
    for f in FILES {
        match out.iter_mut().find(|(id, _)| *id == f.plugin) {
            Some((_, files)) => files.push((f.path, f.text)),
            None => out.push((f.plugin, vec![(f.path, f.text)])),
        }
    }
    out.sort_by_key(|(id, _)| *id);
    out
}

/// A file of a bundled plugin.
pub fn file(plugin: &str, path: &str) -> Option<&'static str> {
    FILES.iter().find(|f| f.plugin == plugin && f.path == path).map(|f| f.text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_plugin_has_a_manifest() {
        let all = plugins();
        let ids: Vec<&str> = all.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            ids,
            [
                "artifacts",
                "ask-user",
                "charts",
                "compaction",
                "core-tools",
                "environments",
                "html-apps",
                "images",
                "markdown",
                "memory",
                "plan",
                "prompt",
                "sheets",
                "subagents",
                "web",
                "workspace-instructions"
            ]
        );
        for (id, _) in &all {
            assert!(file(id, "byob-plugin.json").is_some_and(|m| m.contains(&format!("\"id\": \"{id}\""))), "{id}");
        }
    }
}
