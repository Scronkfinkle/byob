//! The tool interface. Everything the model can call — built-in file tools, plugin
//! skills, Python tools, MCP server tools — implements [`Tool`] and is collected in a
//! [`Registry`]. The agent loop (`yah-agent`) owns calling them, the permission gate, and
//! approvals; tools only describe themselves and do the work.
//!
//! Tools are synchronous and run on the turn's worker thread. Long-running tools should
//! poll [`ToolContext::cancelled`].
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

mod fs;
pub mod sandbox;

pub use fs::{DEFAULT_TRASH_DIR, DeleteFile, ListFiles, MoveFile, MoveFiles, ReadFile, WriteFile};

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::de::DeserializeOwned;
use serde_json::Value;

/// What the model sees: name, description, JSON Schema for the arguments.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    /// `[a-zA-Z0-9_-]{1,64}` — the constraint OpenAI-compatible servers enforce.
    pub name: String,
    pub description: String,
    /// JSON Schema object describing the arguments.
    pub parameters: Value,
}

/// What a call does, for the permission gate. Ordered from least to most dangerous.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Access {
    /// Reads inside the workspace only.
    Read,
    /// Creates, modifies, moves or deletes files.
    Write,
    /// Talks to the network.
    Network,
    /// Runs code or external programs.
    Execute,
}

/// Per-call environment provided by the agent.
pub struct ToolContext<'a> {
    /// Root folder the tool may touch. File tools must refuse paths outside it.
    pub workspace: &'a Path,
    cancel: &'a AtomicBool,
}

impl<'a> ToolContext<'a> {
    pub fn new(workspace: &'a Path, cancel: &'a AtomicBool) -> Self {
        Self { workspace, cancel }
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// The turn's cancel flag, for APIs that poll it themselves (e.g. MCP calls).
    pub fn cancel_flag(&self) -> &AtomicBool {
        self.cancel
    }
}

/// A file-system effect of a call, reported for the UI and the undo journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileChange {
    Created(PathBuf),
    Modified(PathBuf),
    Deleted(PathBuf),
    Moved { from: PathBuf, to: PathBuf },
}

/// Result of a call. `text` goes back to the model; keep it bounded.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolOutput {
    pub text: String,
    /// The call failed; `text` explains why (the model sees it and can recover).
    pub failed: bool,
    pub changes: Vec<FileChange>,
}

impl ToolOutput {
    pub fn ok(text: impl Into<String>) -> Self {
        Self { text: text.into(), ..Default::default() }
    }

    pub fn err(text: impl Into<String>) -> Self {
        Self { text: text.into(), failed: true, ..Default::default() }
    }
}

pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;

    /// Short human title for this call, e.g. "Read notes.md". `args` is untrusted.
    fn title(&self, args: &Value) -> String;

    /// What this call would do; drives the permission gate. Must not have side effects.
    fn access(&self, args: &Value) -> Access;

    /// Do it. `args` is untrusted model output: validate everything, never panic.
    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput;
}

/// Parse tool arguments into a typed struct, with a model-readable error.
pub fn parse_args<T: DeserializeOwned>(args: &Value) -> Result<T, String> {
    serde_json::from_value(args.clone()).map_err(|e| format!("invalid arguments: {e}"))
}

/// Tools available to a turn, by name.
#[derive(Default, Clone)]
pub struct Registry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl Registry {
    /// Add a tool. A later tool with the same name replaces the earlier one.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.spec().name, tool);
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|t| t.spec()).collect()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

/// Add the built-in file tools (`list_files`, `read_file`, `write_file`, `move_file`,
/// `move_files`, `delete_file`). Deleted files go to [`DEFAULT_TRASH_DIR`].
pub fn register_builtins(reg: &mut Registry) {
    reg.register(Arc::new(ListFiles));
    reg.register(Arc::new(ReadFile));
    reg.register(Arc::new(WriteFile));
    reg.register(Arc::new(MoveFile));
    reg.register(Arc::new(MoveFiles));
    reg.register(Arc::new(DeleteFile::default()));
}

/// A registry with just the built-in file tools.
pub fn builtin() -> Registry {
    let mut reg = Registry::default();
    register_builtins(&mut reg);
    reg
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    impl Tool for Echo {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "echo".into(),
                description: "Echo text".into(),
                parameters: serde_json::json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}),
            }
        }
        fn title(&self, _args: &Value) -> String {
            "Echo".into()
        }
        fn access(&self, _args: &Value) -> Access {
            Access::Read
        }
        fn run(&self, args: &Value, _ctx: &ToolContext<'_>) -> ToolOutput {
            #[derive(serde::Deserialize)]
            struct Args {
                text: String,
            }
            match parse_args::<Args>(args) {
                Ok(a) => ToolOutput::ok(a.text),
                Err(e) => ToolOutput::err(e),
            }
        }
    }

    #[test]
    fn registry_dispatch_and_bad_args() {
        let mut reg = Registry::default();
        reg.register(Arc::new(Echo));
        let cancel = AtomicBool::new(false);
        let ctx = ToolContext::new(Path::new("/tmp"), &cancel);
        let tool = reg.get("echo").unwrap();
        assert_eq!(tool.run(&serde_json::json!({"text":"hi"}), &ctx), ToolOutput::ok("hi"));
        assert!(tool.run(&serde_json::json!({"nope":1}), &ctx).failed);
        assert_eq!(reg.specs().len(), 1);
    }

    #[test]
    fn builtins_are_valid() {
        let reg = builtin();
        assert_eq!(reg.len(), 6);
        for spec in reg.specs() {
            assert!(spec.name.len() <= 64 && spec.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
            assert_eq!(spec.parameters["type"], "object");
            let tool = reg.get(&spec.name).unwrap();
            let expected = if matches!(spec.name.as_str(), "list_files" | "read_file") { Access::Read } else { Access::Write };
            assert_eq!(tool.access(&serde_json::json!({})), expected, "{}", spec.name);
        }
    }

    #[test]
    fn access_is_ordered_by_danger() {
        assert!(Access::Read < Access::Write && Access::Write < Access::Network && Access::Network < Access::Execute);
    }
}
