//! The tool interface. Everything the model can call — built-in file tools, plugin
//! skills, Python tools, MCP server tools — implements [`Tool`] and is collected in a
//! [`Registry`]. The agent loop (`byob-agent`) owns calling them, the permission gate, and
//! approvals; tools only describe themselves and do the work.
//!
//! Tools are synchronous and run on the turn's worker thread. Long-running tools should
//! poll [`ToolContext::cancelled`].
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

pub mod artifact;
pub mod artifact_types;
pub mod chart;
mod fs;
pub mod images;
pub mod preview;
pub mod sandbox;
pub mod undo;

pub use artifact::{CreateArtifact, ReadArtifact, UpdateArtifact};
pub use artifact_types::{ArtifactHooks, ArtifactType, ArtifactTypes};
pub use chart::MakeChart;
pub use fs::{DEFAULT_TRASH_DIR, DeleteFile, ListFiles, MoveFile, MoveFiles, ReadFile, WriteFile};
pub use preview::Preview;
pub use undo::{Journal, UndoOp};

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
    journal: Option<&'a Journal>,
    chat: Option<&'a str>,
    asker: Option<&'a dyn Asker>,
    env: Option<&'a EnvPlan>,
    host: Option<&'a (dyn std::any::Any + Sync)>,
}

/// Where a chat's commands run, as its workspace and chat settings decide (the engine
/// resolves them; see `byob_types::config`). The default leaves everything to the answers
/// the user gave for the folder (asked once).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvPlan {
    /// In the workspace's container (`Some(true)`), on this machine (`Some(false)`), or as
    /// the user answered for the folder (`None`). Chosen in the settings, so never asked.
    pub container: Option<bool>,
    /// Load the folder's project environment (flake.nix, …) without asking (`Some(true)`),
    /// never (`Some(false)`), or as answered (`None`).
    pub project: Option<bool>,
    /// The container's image; `None`: the folder's devcontainer.json, else the default.
    pub image: Option<String>,
    /// The environment providers the chat's plugins switch on (`None`: every one byob has;
    /// see `byob_exec::env::EnvProvider`).
    pub providers: Option<EnvProviders>,
}

/// Which workspace environment providers a chat uses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvProviders {
    /// Registered providers left out, by id (byob's own: `nix`, `pixi`, `mise`, `container`).
    pub off: Vec<String>,
    /// Command providers plugins declare (`environments` in a manifest).
    pub external: Vec<CommandEnv>,
}

/// A plugin's environment provider: when the workspace has one of `detect`, run `program`
/// with `args` in it (sandboxed, with network) and read the environment it prints as JSON
/// (`{"PATH": "...", "NAME": "value"}`, or pixi's `{"environment_variables": {...}}`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandEnv {
    /// Unique among providers (names the cache file): `<plugin>-<id>`.
    pub id: String,
    /// For messages: "conda environment".
    pub label: String,
    pub detect: Vec<String>,
    /// Files whose contents key the cache (besides `detect`).
    pub inputs: Vec<String>,
    pub program: PathBuf,
    pub args: Vec<String>,
    /// The plugin's folder (readable by the run).
    pub dir: PathBuf,
}

impl<'a> ToolContext<'a> {
    pub fn new(workspace: &'a Path, cancel: &'a AtomicBool) -> Self {
        Self { workspace, cancel, journal: None, chat: None, asker: None, env: None, host: None }
    }

    /// What the program running the tool offers beyond this context (the engine's turn
    /// offers a handle to start child turns; see `byob_agent`'s `delegate`). Tools that need
    /// it look it up by type with [`ToolContext::host`].
    #[must_use]
    pub fn with_host(mut self, host: &'a (dyn std::any::Any + Sync)) -> Self {
        self.host = Some(host);
        self
    }

    /// The host capability of type `T`, if the caller offered one.
    pub fn host<T: 'static>(&self) -> Option<&'a T> {
        self.host.and_then(|h| (h as &dyn std::any::Any).downcast_ref::<T>())
    }

    /// Run commands where `plan` says (see [`EnvPlan`]).
    #[must_use]
    pub fn with_env(mut self, plan: &'a EnvPlan) -> Self {
        self.env = Some(plan);
        self
    }

    /// Where this chat's commands run (the default when the engine didn't say).
    pub fn env(&self) -> EnvPlan {
        self.env.cloned().unwrap_or_default()
    }

    /// Let the tool ask the user questions while it runs (see [`ToolContext::ask`]).
    #[must_use]
    pub fn with_asker(mut self, asker: &'a dyn Asker) -> Self {
        self.asker = Some(asker);
        self
    }

    /// Ask the user `q` (an approval card), blocking until answered. Without anyone to ask
    /// (no asker: tests, direct calls) the answer is [`Answer::AutoNo`].
    pub fn ask(&self, q: &Question) -> Answer {
        if self.cancelled() {
            return Answer::AutoNo;
        }
        self.asker.map_or(Answer::AutoNo, |a| a.ask(q))
    }

    /// The chat this call runs in (artifacts belong to a chat).
    #[must_use]
    pub fn with_chat(mut self, chat: &'a str) -> Self {
        self.chat = Some(chat);
        self
    }

    pub fn chat(&self) -> Option<&'a str> {
        self.chat
    }

    /// Record undo information for this call's file changes in `journal`.
    #[must_use]
    pub fn with_journal(mut self, journal: &'a Journal) -> Self {
        self.journal = Some(journal);
        self
    }

    /// The turn's undo journal, if it keeps one. Tools that change files record an
    /// [`UndoOp`] per change, and back up a file with [`Journal::backup`] before
    /// overwriting it.
    pub fn journal(&self) -> Option<&'a Journal> {
        self.journal
    }

    /// Shorthand: record `op` if there is a journal.
    pub fn record(&self, op: UndoOp) {
        if let Some(j) = self.journal {
            j.record(op);
        }
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

/// A question a tool asks the user while it runs, e.g. "Load this workspace's Nix dev
/// shell?". Shown as an approval card with only Allow and Deny: the tool remembers the
/// answer itself, so "always allow" grants don't apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    /// Phrased like a pending title: "Load the workspace's Nix dev shell (flake.nix)".
    pub title: String,
    /// What saying yes means, for the card (a few short lines).
    pub text: String,
    /// Shown prominently, e.g. "installs system-wide".
    pub warning: Option<String>,
    /// Decides who answers: the permission policy for this level may answer for the user
    /// (e.g. `--auto-approve`), unless [`Question::always_ask`].
    pub access: Access,
    /// Only a person may say yes: not the policy, not `--auto-approve`.
    pub always_ask: bool,
}

/// The answer to a [`Question`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// A person said yes.
    Yes,
    /// A person said no.
    No,
    /// The policy allowed it without asking (`--auto-approve`).
    AutoYes,
    /// Nobody was asked: the policy refused, the turn stopped, or there's no one to ask.
    AutoNo,
}

impl Answer {
    pub fn allowed(self) -> bool {
        matches!(self, Self::Yes | Self::AutoYes)
    }

    /// A person answered (worth remembering).
    pub fn by_user(self) -> bool {
        matches!(self, Self::Yes | Self::No)
    }
}

/// Puts a [`Question`] to the user; the agent provides one per call.
pub trait Asker: Send + Sync {
    fn ask(&self, q: &Question) -> Answer;
}

/// Workspace environment settings the engine shows to clients and lets them change
/// (provided by the code-running tools, `byob-exec`). Calls may block for a while
/// (stopping a container): the engine makes them off its own thread.
pub trait EnvControl: Send + Sync {
    /// The settings and state of workspace `ws`, whose settings say `plan`.
    fn describe(&self, ws: &Path, plan: &EnvPlan) -> byob_protocol::WorkspaceEnv;
    /// Change them; returns what happened, for the user.
    fn apply(&self, ws: &Path, action: &byob_protocol::EnvAction) -> Result<String, String>;
    /// What this machine offers (Nix, a container runtime). Cheap.
    fn support(&self) -> byob_protocol::EnvSupport;
    /// Whether the container runtime answers; the error says what to do. May take seconds.
    fn check_containers(&self) -> Result<String, String>;
    /// Forget everything byob keeps for `ws` outside the folder: its answers, installed
    /// packages and container. The folder itself is never touched. May take seconds.
    fn forget(&self, ws: &Path) -> Result<String, String>;
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

    /// Short human title for the finished call, e.g. "Wrote notes.md". `args` is untrusted.
    fn title(&self, args: &Value) -> String;

    /// Title for the call before and while it runs, and on approval cards, e.g.
    /// "Write notes.md". Defaults to "Run <tool name>".
    fn pending_title(&self, _args: &Value) -> String {
        format!("Run {}", self.spec().name)
    }

    /// A readable account of what the call would do, for approval cards: e.g. a diff for a
    /// file write. Reads only, no side effects; bounded. Defaults to the key arguments.
    fn preview(&self, args: &Value, _workspace: &Path) -> Option<Preview> {
        Some(preview::args(args))
    }

    /// What this call would do; drives the permission gate. Must not have side effects.
    fn access(&self, args: &Value) -> Access;

    /// `Some(warning)` if this call must be approved by a person every time: no policy,
    /// "always allow" grant or `--auto-approve` lets it run (e.g. installing system-wide).
    /// The warning is shown on the approval card.
    fn always_ask(&self, _args: &Value) -> Option<String> {
        None
    }

    /// Do it. `args` is untrusted model output: validate everything, never panic.
    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput;

    /// Calls of this tool start before the other calls of the same round and run side by
    /// side, each on a thread of its own (results still go back in call order). For tools
    /// that mostly wait, like `delegate`.
    fn concurrent(&self) -> bool {
        false
    }
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

    /// Take a tool out (a native tool whose plugin is off).
    pub fn remove(&mut self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.remove(name)
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
/// `move_files`, `delete_file`), `make_chart` and the artifact tools (for byob's own
/// artifact types). Deleted files go to [`DEFAULT_TRASH_DIR`].
pub fn register_builtins(reg: &mut Registry) {
    register_builtins_with(reg, &ArtifactTypes::builtin());
}

/// [`register_builtins`], with the artifact tools using `types` (the engine's registry).
pub fn register_builtins_with(reg: &mut Registry, types: &ArtifactTypes) {
    reg.register(Arc::new(ListFiles));
    reg.register(Arc::new(ReadFile));
    reg.register(Arc::new(WriteFile));
    reg.register(Arc::new(MoveFile));
    reg.register(Arc::new(MoveFiles));
    reg.register(Arc::new(DeleteFile::default()));
    reg.register(Arc::new(MakeChart));
    reg.register(Arc::new(CreateArtifact { types: types.clone() }));
    reg.register(Arc::new(UpdateArtifact { types: types.clone() }));
    reg.register(Arc::new(ReadArtifact { types: types.clone() }));
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
        assert_eq!(reg.len(), 10);
        for spec in reg.specs() {
            assert!(spec.name.len() <= 64 && spec.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
            assert_eq!(spec.parameters["type"], "object");
            let tool = reg.get(&spec.name).unwrap();
            let expected = if matches!(spec.name.as_str(), "list_files" | "read_file" | "make_chart" | "create_artifact" | "update_artifact" | "read_artifact")
            {
                Access::Read
            } else {
                Access::Write
            };
            assert_eq!(tool.access(&serde_json::json!({})), expected, "{}", spec.name);
        }
    }

    #[test]
    fn default_pending_title_and_preview() {
        assert_eq!(Echo.pending_title(&serde_json::json!({})), "Run echo");
        let p = Echo.preview(&serde_json::json!({"text": "hi"}), Path::new("/tmp"));
        assert_eq!(p, Some(Preview::Args { fields: vec![("text".into(), "hi".into())], code: None, more: 0 }));
    }

    #[test]
    fn access_is_ordered_by_danger() {
        assert!(Access::Read < Access::Write && Access::Write < Access::Network && Access::Network < Access::Execute);
    }
}
