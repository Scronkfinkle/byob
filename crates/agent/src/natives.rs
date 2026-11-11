//! Native bindings: what plugin manifests may name by id and in-tree Rust implements.
//!
//! byob's built-in features are bundled plugins (`byob-bundled`) whose manifests point at
//! in-tree code instead of scripts:
//!
//! - **tools** (`tools[].native`): a tool of the engine's base registry by name
//!   (`EngineConfig::tools`, filled by the app: file tools, `make_chart`, the artifact tools,
//!   the code-running tools). Tools whose instance depends on the chat's artifact types (the
//!   artifact tools) are rebuilt per chat from [`Natives::typed_tools`].
//! - **artifact types** (`artifact_types[].native`): one of the engine's
//!   [`ArtifactTypes`] by id ([`Natives::artifact_types`]: id → content type), with the
//!   manifest's overrides (suffixes, label, describe, pick, guidance file).
//! - **prompt sections** (`system_prompt: {"native": id}`): text an in-tree builder makes
//!   from the chat's artifact types ([`Natives::prompts`]).
//!
//! Clients resolve `renderers[].renderer` (`native:<id>`, `web`) against their own registry
//! (`byob_ui_egui::renderers::ArtifactRenderers`). An id nobody registered is a problem of the
//! plugin, never a crash. A base tool, type or section no plugin names is simply always
//! there (third-party in-tree code, tests without bundled plugins).

use std::collections::BTreeMap;
use std::sync::Arc;

use byob_tools::{ArtifactTypes, Tool};

/// Makes a tool for a chat's artifact types.
pub type TypedTool = Arc<dyn Fn(&ArtifactTypes) -> Arc<dyn Tool> + Send + Sync>;
/// Makes a system-prompt section for a chat's artifact types.
pub type PromptBuilder = Arc<dyn Fn(&ArtifactTypes) -> String + Send + Sync>;

/// The registries manifests' native ids resolve against (see the module docs).
#[derive(Clone, Default)]
pub struct Natives {
    /// Tools rebuilt for each chat's artifact types, by tool name.
    pub typed_tools: BTreeMap<String, TypedTool>,
    /// Prompt section builders by id.
    pub prompts: BTreeMap<String, PromptBuilder>,
    /// Artifact type ids → content types registered in the engine's [`ArtifactTypes`].
    pub artifact_types: BTreeMap<String, String>,
}

impl Natives {
    /// byob's own: the artifact tools, the `artifacts` prompt section and the built-in
    /// artifact types. The app adds `exec` (the code-running section) with [`Self::prompt`].
    pub fn builtin() -> Self {
        let mut n = Self::default();
        let create: TypedTool = Arc::new(|types: &ArtifactTypes| Arc::new(byob_tools::CreateArtifact { types: types.clone() }));
        let update: TypedTool = Arc::new(|types: &ArtifactTypes| Arc::new(byob_tools::UpdateArtifact { types: types.clone() }));
        let read: TypedTool = Arc::new(|types: &ArtifactTypes| Arc::new(byob_tools::ReadArtifact { types: types.clone() }));
        n.typed_tools.insert("create_artifact".into(), create);
        n.typed_tools.insert("update_artifact".into(), update);
        n.typed_tools.insert("read_artifact".into(), read);
        n.prompts.insert("artifacts".into(), Arc::new(ArtifactTypes::prompt));
        for (id, ct) in ArtifactTypes::native_ids() {
            n.artifact_types.insert(id.to_owned(), ct.to_owned());
        }
        n
    }

    /// Add (or replace) prompt section `id`.
    pub fn prompt(mut self, id: &str, build: impl Fn(&ArtifactTypes) -> String + Send + Sync + 'static) -> Self {
        self.prompts.insert(id.to_owned(), Arc::new(build));
        self
    }
}

/// The bundled plugins of `byob-bundled`, as the engine loads them.
pub fn bundled_plugins() -> Vec<byob_plugins::Embedded> {
    byob_bundled::plugins()
        .into_iter()
        .map(|(id, files)| byob_plugins::Embedded { id: id.to_owned(), files: files.into_iter().map(|(p, t)| (p.to_owned(), t.to_owned())).collect() })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_tools_are_the_artifact_tools() {
        let n = Natives::builtin();
        let types = ArtifactTypes::builtin();
        for name in ["create_artifact", "update_artifact", "read_artifact"] {
            assert_eq!(n.typed_tools[name](&types).spec().name, name);
        }
        assert!(n.prompts["artifacts"](&types).starts_with("Artifacts: "));
        assert_eq!(n.artifact_types["html_app"], byob_protocol::artifact::TYPE_HTML);
    }

    #[test]
    fn bundled_html_guidance_is_the_native_default() {
        let html = byob_bundled::file("html-apps", "guidance.md").unwrap_or_default();
        assert_eq!(html.trim(), byob_tools::artifact_types::html_app::GUIDANCE);
    }
}
