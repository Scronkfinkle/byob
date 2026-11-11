//! Plugins in the engine: which plugin folders a chat sees, and the tools, prompt lines,
//! skills and subagents they add to its turns.
//!
//! Three sets of plugin folders, most specific first (the same id resolves chat > workspace
//! > global, see `byob_protocol::plugins`):
//! - a chat's **scratch** plugins, `<scratch root>/<chat>/plugins` (what the model made in it);
//! - a **workspace**'s `.byob/plugins` (+ standalone skills in `.byob/skills`);
//! - the **global** folders: `<config>/plugins`, `settings.json` `plugin_dirs`, `--plugin`
//!   (+ `<config>/skills`).
//!
//! Each set keeps a fingerprint of its files (names, sizes, times) and is read again only
//! when that changes ([`Extensions::refresh`], at turn start and after the model edits a
//! plugin). [`Extensions::compose`] then builds one turn's tool registry and prompt from the
//! base registry (built-in tools, registered once by the app) and the plugins switched on for
//! the chat. Plugins that came with a workspace folder run their code only once the user
//! trusts them; until then only their skills, prompt line and subagents are used. MCP servers
//! connect on worker threads ([`Pool`]); their tools join from the next round once connected.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::hash::Hasher;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use byob_plugins::authoring::{Fnv, fingerprint, fingerprint_into};
use byob_plugins::manifest::ToolKind;
use byob_plugins::manifest::{ManifestArtifactType, PromptSource};
use byob_plugins::mcp::McpSandbox;
use byob_plugins::{
    AgentDef, BackendTool, Command, Connector, ConnectorTool, Discovery, Embedded, LoadError, McpServerConfig, Plugin, PluginPython, PluginRef, PluginSources,
    PyEnvs, Skill, ToolNames, UseSkillTool,
};
use byob_protocol::{
    BackendInfo, PluginInfo, PluginLoadError, PluginOrigin, PluginScope, PluginSetting, RendererDecl, ServerInfo, ServerState, SlashItem, SlashKind,
    StarterDecl,
};
use byob_sandbox::Sandbox;
use byob_store::PluginTrust;
use byob_tools::{ArtifactType, ArtifactTypes, Registry, Tool};
use byob_types::config::Effective;

use crate::backends::{Backends, Make, SinkFor};
use crate::natives::Natives;

/// Longest system-prompt section a plugin file may add (bytes).
pub const MAX_SECTION_BYTES: usize = 16 * 1024;

/// The built-in skill on writing plugins.
pub const AUTHORING_SKILL: &str = include_str!("plugin_authoring.md");
/// Its name.
pub const AUTHORING_SKILL_NAME: &str = "plugin-authoring";
/// The one system prompt line about extending byob.
pub const AUTHORING_LINE: &str = "You can give yourself new tools: make_tool turns a Python `def run(...)` into a tool you can call from your next step (test it with try_args). For anything bigger (skills, MCP servers, subagents, settings, keeping a plugin) read the plugin-authoring skill first.";
/// A file byob leaves in plugin folders it wrote outside a workspace (global plugins).
pub const MADE_MARKER: &str = ".byob-made";
/// Most bytes of a workspace's AGENTS.md put in the system prompt.
pub const MAX_AGENTS_MD: usize = 16 * 1024;
/// MCP servers nobody used for this long are closed.
const IDLE_SERVER: Duration = Duration::from_secs(30 * 60);
/// Names no plugin tool may take (the turn adds them itself).
const RESERVED_NAMES: [&str; 2] = ["delegate", "use_skill"];

/// Where plugins come from (the app fills this in; `None` in `EngineConfig` turns plugins off).
#[derive(Clone, Default)]
pub struct PluginsConfig {
    /// Folders whose subfolders are global plugins. The first is where plugins kept
    /// "everywhere" go (`<config>/plugins`).
    pub global_roots: Vec<PathBuf>,
    /// Single plugin folders (`--plugin`).
    pub linked: Vec<PathBuf>,
    /// Folders whose subfolders ship with byob.
    pub builtin_roots: Vec<PathBuf>,
    /// Standalone skills kept everywhere (`<config>/skills`).
    pub global_skills: Option<PathBuf>,
    /// Where chats' scratch plugins live (`<data>/chats`); `None`: a temporary folder.
    pub scratch_root: Option<PathBuf>,
    /// The interpreter for Python tools, e.g. `["python3"]`.
    pub python: Vec<OsString>,
    /// Plugin scripts and local MCP servers run in this sandbox (`None`: unconfined).
    pub sandbox: Option<Sandbox>,
    /// Where plugins' Python environments and byob's helper module go
    /// (`<data>/plugin-python`); `None`: a temporary folder.
    pub python_root: Option<PathBuf>,
    /// Plugins inside the binary (scope `bundled`, the least specific; see
    /// [`crate::natives::bundled_plugins`]).
    pub bundled: Vec<Embedded>,
    /// What their native ids resolve to.
    pub natives: Natives,
}

/// One set of plugin folders (a scope) and what was found there.
pub(crate) struct ExtSet {
    scope: PluginScope,
    sources: PluginSources,
    skills_dir: Option<PathBuf>,
    /// `None`: never read.
    fp: Option<u64>,
    found: Discovery,
    /// Standalone skills (`skills_dir`).
    skills: Vec<Skill>,
    skill_errors: Vec<LoadError>,
    /// Each plugin's own fingerprint, by folder.
    plugin_fps: BTreeMap<PathBuf, u64>,
    /// Plugins in the binary instead of folders (the bundled set).
    embedded: Vec<Embedded>,
}

/// A fingerprint of an embedded plugin's files.
fn embedded_fp(files: &BTreeMap<String, String>) -> u64 {
    let mut h = Fnv::default();
    for (path, text) in files {
        h.write(path.as_bytes());
        h.write_u8(0);
        h.write(text.as_bytes());
    }
    h.finish()
}

impl ExtSet {
    fn new(scope: PluginScope, sources: PluginSources, skills_dir: Option<PathBuf>) -> Self {
        Self {
            scope,
            sources,
            skills_dir,
            fp: None,
            found: Discovery::default(),
            skills: Vec::new(),
            skill_errors: Vec::new(),
            plugin_fps: BTreeMap::new(),
            embedded: Vec::new(),
        }
    }

    fn embedded(list: Vec<Embedded>) -> Self {
        let mut set = Self::new(PluginScope::Bundled, PluginSources::default(), None);
        set.embedded = list;
        set
    }

    fn fingerprint(&self) -> u64 {
        let mut h = Fnv::default();
        for e in &self.embedded {
            h.write(e.id.as_bytes());
            h.write_u64(embedded_fp(&e.files));
        }
        for root in self.sources.builtin_roots.iter().chain(&self.sources.roots) {
            h.write(root.as_os_str().as_encoded_bytes());
            fingerprint_into(&mut h, root, 4);
        }
        for dir in &self.sources.linked {
            h.write(dir.as_os_str().as_encoded_bytes());
            fingerprint_into(&mut h, dir, 3);
        }
        if let Some(dir) = &self.skills_dir {
            h.write_u8(7);
            fingerprint_into(&mut h, dir, 2);
        }
        h.finish()
    }

    /// Read the folders again if anything changed. Returns the folders of plugins that were
    /// loaded before and changed (or went away); `None` when nothing changed at all.
    fn refresh(&mut self) -> Option<Vec<PathBuf>> {
        let fp = self.fingerprint();
        if self.fp == Some(fp) {
            return None;
        }
        let first = self.fp.is_none();
        self.fp = Some(fp);
        self.found = byob_plugins::discover(&self.sources);
        if !self.embedded.is_empty() {
            // The binary's own first, then any `builtin_roots` folders.
            let mut found = byob_plugins::discover_embedded(&self.embedded);
            for p in std::mem::take(&mut self.found.plugins) {
                if found.plugins.iter().any(|q| q.id() == p.id()) {
                    found.errors.push(LoadError { path: p.dir.clone(), message: format!("id \"{}\" is already a plugin built into byob", p.id()) });
                } else {
                    found.plugins.push(p);
                }
            }
            found.errors.append(&mut self.found.errors);
            self.found = found;
        }
        for e in &self.found.errors {
            log::warn!("plugin {}: {}", e.path.display(), e.message);
        }
        (self.skills, self.skill_errors) = match &self.skills_dir {
            Some(dir) => byob_plugins::load_skills_dir(dir),
            None => (Vec::new(), Vec::new()),
        };
        let fp_of = |p: &Plugin| p.embedded.as_deref().map_or_else(|| fingerprint(&p.dir), embedded_fp);
        let fps: BTreeMap<PathBuf, u64> = self.found.plugins.iter().map(|p| (p.dir.clone(), fp_of(p))).collect();
        let changed =
            if first { Vec::new() } else { self.plugin_fps.iter().filter(|(dir, fp)| fps.get(*dir) != Some(*fp)).map(|(dir, _)| dir.clone()).collect() };
        self.plugin_fps = fps;
        Some(changed)
    }
}

/// A connection to one MCP server of a plugin.
pub(crate) enum Conn {
    Connecting,
    Ready { connector: Arc<dyn Connector>, tools: Vec<ConnectorTool> },
    Failed(String),
}

struct PoolEntry {
    conn: Conn,
    used: Instant,
    /// The plugin's folder (to retry its servers when it changes).
    dir: PathBuf,
}

/// MCP connections by (plugin, server, configuration, workspace): a server whose
/// configuration didn't change survives reloads.
#[derive(Default)]
pub(crate) struct Pool {
    entries: BTreeMap<String, PoolEntry>,
}

impl Pool {
    /// A worker finished connecting.
    pub(crate) fn connected(&mut self, key: &str, result: Result<(Arc<dyn Connector>, Vec<ConnectorTool>), String>) {
        if let Some(e) = self.entries.get_mut(key) {
            e.conn = match result {
                Ok((connector, tools)) => Conn::Ready { connector, tools },
                Err(e) => Conn::Failed(e),
            };
        }
    }

    /// Forget failed connections (of the plugin in `dir`, or all) so they are tried again.
    fn retry_failed(&mut self, dir: Option<&Path>) {
        self.entries.retain(|_, e| !matches!(e.conn, Conn::Failed(_)) || dir.is_some_and(|d| e.dir != d));
    }

    /// Close connections of the plugin in `dir` (it changed or went away).
    fn drop_dir(&mut self, dir: &Path) {
        self.entries.retain(|_, e| e.dir != dir);
    }

    fn gc(&mut self) {
        self.entries.retain(|_, e| matches!(e.conn, Conn::Connecting) || e.used.elapsed() < IDLE_SERVER);
    }
}

/// An MCP server to start on a worker thread.
pub(crate) struct ConnectJob {
    pub key: String,
    pub plugin: String,
    pub server: String,
    pub config: McpServerConfig,
    pub dir: PathBuf,
    pub sandbox: Option<McpSandbox>,
}

/// What one chat's turn gets from its plugins.
#[derive(Default)]
pub(crate) struct Composed {
    /// The base tools plus the plugins' tools and `use_skill`.
    pub tools: Registry,
    /// Plugin prompt lines, the skills list and the authoring line (no AGENTS.md).
    pub prompt: String,
    pub agents: Vec<AgentDef>,
    pub infos: Vec<PluginInfo>,
    pub errors: Vec<PluginLoadError>,
    /// Tool name → (plugin folder, plugin id, declared name).
    pub owners: BTreeMap<String, Owner>,
    /// Servers to start.
    pub connect: Vec<ConnectJob>,
    /// Plugins of the workspace that carry code and nobody decided on yet.
    pub undecided: Vec<String>,
    /// The slash commands of switched-on plugins (most specific first; a name once).
    pub commands: Vec<Command>,
    /// The `/` menu: built-in commands, plugin commands, skills.
    pub slash: Vec<SlashItem>,
    /// The switched-on plugins' system-prompt sections (bundled ones first, in their
    /// order), between the base prompt and [`Self::prompt`].
    pub sections: String,
    /// The chat's artifact types: the engine's, less those of plugins that are off, with
    /// the switched-on plugins' declarations (and overrides) applied.
    pub artifact_types: ArtifactTypes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Owner {
    pub dir: PathBuf,
    pub plugin: String,
    pub scope: PluginScope,
    /// The tool's name in the manifest (or the server's).
    pub declared: String,
}

/// What [`Extensions::compose`] needs to know about the chat.
pub(crate) struct ComposeIn<'a> {
    pub chat: Option<&'a str>,
    pub workspace: &'a Path,
    pub effective: &'a Effective,
    pub trust: &'a BTreeMap<String, PluginTrust>,
    pub values: &'a BTreeMap<String, BTreeMap<String, String>>,
    pub base: &'a Registry,
    /// The engine's artifact types (what native type ids resolve against).
    pub types: &'a ArtifactTypes,
}

/// Every set of plugin folders the engine has seen, and the MCP connections.
pub(crate) struct Extensions {
    pub cfg: PluginsConfig,
    scratch_root: PathBuf,
    /// The plugins inside the binary (and any `builtin_roots`), least specific.
    bundled: ExtSet,
    global: ExtSet,
    workspaces: BTreeMap<PathBuf, ExtSet>,
    chats: BTreeMap<String, ExtSet>,
    pub pool: Pool,
    /// The last failed run of a plugin's tool, by plugin folder.
    pub last_error: BTreeMap<PathBuf, String>,
    /// Tool names of each plugin at its last composition, by plugin folder.
    pub tools_of: BTreeMap<PathBuf, BTreeSet<String>>,
    builtin_skill: Skill,
    /// Plugins' Python (helper module, requirements environments).
    pub pyenvs: Arc<PyEnvs>,
    /// Plugin backends by plugin folder.
    pub backends: Backends,
    /// Where backends' messages go (the engine's inbox); `None`: no backends.
    pub sink: Option<SinkFor>,
}

/// The scratch root used when byob has no data folder (tests, throwaway runs).
fn temp_scratch() -> PathBuf {
    std::env::temp_dir().join(format!("byob-scratch-{}", std::process::id()))
}

impl Extensions {
    pub(crate) fn new(cfg: PluginsConfig) -> Self {
        let scratch_root = cfg.scratch_root.clone().unwrap_or_else(temp_scratch);
        let global = ExtSet::new(
            PluginScope::Global,
            PluginSources { roots: cfg.global_roots.clone(), linked: cfg.linked.clone(), ..Default::default() },
            cfg.global_skills.clone(),
        );
        let mut bundled = ExtSet::embedded(cfg.bundled.clone());
        bundled.sources.builtin_roots = cfg.builtin_roots.clone();
        let builtin_skill = byob_plugins::skills::parse_skill(AUTHORING_SKILL, Path::new(AUTHORING_SKILL_NAME)).unwrap_or_else(|e| {
            log::error!("the built-in plugin-authoring skill doesn't parse: {e}");
            Skill {
                name: AUTHORING_SKILL_NAME.into(),
                description: "How to write byob plugins".into(),
                body: AUTHORING_SKILL.into(),
                dir: PathBuf::from(AUTHORING_SKILL_NAME),
                network: false,
                packages: Vec::new(),
                meta: BTreeMap::new(),
            }
        });
        let python_root = cfg.python_root.clone().unwrap_or_else(|| std::env::temp_dir().join(format!("byob-python-{}", std::process::id())));
        let pyenvs = Arc::new(PyEnvs::new(python_root, cfg.python.clone()));
        Self {
            pyenvs,
            backends: Backends::default(),
            sink: None,
            cfg,
            scratch_root,
            global,
            bundled,
            workspaces: BTreeMap::new(),
            chats: BTreeMap::new(),
            pool: Pool::default(),
            last_error: BTreeMap::new(),
            tools_of: BTreeMap::new(),
            builtin_skill,
        }
    }

    /// The folder holding chat `chat`'s scratch plugins.
    pub(crate) fn scratch_dir(&self, chat: &str) -> PathBuf {
        self.scratch_root.join(chat).join("plugins")
    }

    /// Where plugins kept everywhere go.
    pub(crate) fn global_dir(&self) -> Option<&Path> {
        self.cfg.global_roots.first().map(PathBuf::as_path)
    }

    /// The files of bundled plugin `id` (what a fork copies).
    pub(crate) fn bundled_files(&mut self, id: &str) -> Option<Arc<BTreeMap<String, String>>> {
        self.bundled.refresh();
        self.bundled.found.plugins.iter().find(|p| p.id() == id).and_then(|p| p.embedded.clone())
    }

    /// Every bundled plugin's files, by id.
    pub(crate) fn all_bundled_files(&mut self) -> BTreeMap<String, Arc<BTreeMap<String, String>>> {
        self.bundled.refresh();
        self.bundled.found.plugins.iter().filter_map(|p| Some((p.id().to_owned(), p.embedded.clone()?))).collect()
    }

    /// Plugin `id` can't be switched off (a locked bundled plugin, or a fork of one).
    pub(crate) fn is_locked(&mut self, id: &str) -> bool {
        self.bundled.refresh();
        self.bundled.found.plugins.iter().any(|p| p.id() == id && p.manifest.locked)
    }

    /// A chat was deleted: forget its scratch plugins (and delete them).
    pub(crate) fn forget_chat(&mut self, chat: &str) {
        self.chats.remove(chat);
        self.backends.stop_chat(chat);
        if !chat.is_empty() && !chat.contains(['/', '\\', '.']) {
            let _ = std::fs::remove_dir_all(self.scratch_root.join(chat));
        }
    }

    fn workspace_set(&mut self, ws: &Path) -> &mut ExtSet {
        self.workspaces.entry(ws.to_path_buf()).or_insert_with(|| {
            let dot = ws.join(".byob");
            ExtSet::new(PluginScope::Workspace, PluginSources { roots: vec![dot.join("plugins")], ..Default::default() }, Some(dot.join("skills")))
        })
    }

    fn chat_set(&mut self, chat: &str) -> &mut ExtSet {
        let dir = self.scratch_dir(chat);
        self.chats.entry(chat.to_owned()).or_insert_with(|| ExtSet::new(PluginScope::Chat, PluginSources { roots: vec![dir], ..Default::default() }, None))
    }

    /// Read the sets a chat sees again where their files changed. Returns the folders of
    /// plugins that changed (their "always allow" grants go) and whether anything changed.
    pub(crate) fn refresh(&mut self, chat: Option<&str>, ws: &Path) -> (Vec<(PluginScope, PathBuf)>, bool) {
        let mut changed = Vec::new();
        let mut any = false;
        let mut sets: Vec<&mut ExtSet> = Vec::new();
        // Borrow each set in turn (they live in different maps).
        let global = &mut self.global;
        sets.push(&mut self.bundled);
        let wset = self.workspaces.entry(ws.to_path_buf()).or_insert_with(|| {
            let dot = ws.join(".byob");
            ExtSet::new(PluginScope::Workspace, PluginSources { roots: vec![dot.join("plugins")], ..Default::default() }, Some(dot.join("skills")))
        });
        sets.push(global);
        sets.push(wset);
        if let Some(c) = chat {
            let dir = self.scratch_root.join(c).join("plugins");
            sets.push(
                self.chats
                    .entry(c.to_owned())
                    .or_insert_with(|| ExtSet::new(PluginScope::Chat, PluginSources { roots: vec![dir], ..Default::default() }, None)),
            );
        }
        for set in sets {
            if let Some(dirs) = set.refresh() {
                any = true;
                changed.extend(dirs.into_iter().map(|d| (set.scope, d)));
            }
        }
        for (_, dir) in &changed {
            self.pool.drop_dir(dir);
            self.last_error.remove(dir);
            self.backends.stop_dir(dir, "its plugin changed");
        }
        if any {
            self.pool.retry_failed(None);
            self.pyenvs.forget_failed();
        }
        (changed, any)
    }

    /// Forget every set's fingerprint so the next refresh reads everything again; backends
    /// start afresh and failed Python environments are tried again.
    pub(crate) fn invalidate(&mut self) {
        self.global.fp = None;
        self.bundled.fp = None;
        for s in self.workspaces.values_mut().chain(self.chats.values_mut()) {
            s.fp = None;
        }
        self.pool.retry_failed(None);
        self.backends.stop_all("plugins were reloaded");
        self.pyenvs.forget_failed();
    }

    /// The plugins a chat sees in `scope` (loaded sets only).
    pub(crate) fn plugins_in(&mut self, scope: PluginScope, chat: Option<&str>, ws: &Path) -> Vec<Plugin> {
        let set = match (scope, chat) {
            (PluginScope::Bundled, _) => &self.bundled,
            (PluginScope::Global, _) => &self.global,
            (PluginScope::Workspace, _) => self.workspace_set(ws),
            (PluginScope::Chat, Some(c)) => self.chat_set(c),
            (PluginScope::Chat, None) => return Vec::new(),
        };
        set.found.plugins.clone()
    }

    /// Build a chat's tools and prompt from the base registry and its plugins.
    pub(crate) fn compose(&mut self, input: &ComposeIn<'_>) -> Composed {
        let Self { cfg, global, bundled, workspaces, chats, pool, last_error, tools_of, builtin_skill, pyenvs, backends, sink, .. } = self;
        let mut out = Composed { tools: input.base.clone(), ..Default::default() };
        let mut names = ToolNames::new(input.base.specs().into_iter().map(|s| s.name).chain(RESERVED_NAMES.iter().map(|s| (*s).to_owned())));
        // Most specific set first; each keeps its folder errors.
        let mut order: Vec<&ExtSet> = Vec::new();
        if let Some(c) = input.chat.and_then(|c| chats.get(c)) {
            order.push(c);
        }
        if let Some(w) = workspaces.get(input.workspace) {
            order.push(w);
        }
        order.push(global);
        order.push(bundled);
        // Bundled plugins marked locked stay on, and so do their forks (by id).
        let locked: BTreeSet<String> = bundled.found.plugins.iter().filter(|p| p.manifest.locked).map(|p| p.id().to_owned()).collect();
        let mut native = NativeParts::new(&order, bundled);
        let mut winners: BTreeMap<String, PluginScope> = BTreeMap::new();
        let mut prompt_lines = Vec::new();
        let mut skills: Vec<Skill> = Vec::new();
        // Where each skill and command came from, for the `/` menu.
        let mut skill_from: BTreeMap<String, (PluginScope, Option<String>)> = BTreeMap::new();
        let mut command_from: Vec<(PluginScope, String)> = Vec::new();
        let mut agent_lists = Vec::new();
        let ws_local = input.workspace.to_path_buf();
        let empty = BTreeMap::new();
        for set in &order {
            for e in set.found.errors.iter().chain(&set.skill_errors) {
                out.errors.push(PluginLoadError { scope: set.scope, path: e.path.display().to_string(), message: e.message.clone() });
            }
            for plugin in set.found.active() {
                let id = plugin.id().to_owned();
                let fp = set.plugin_fps.get(&plugin.dir).copied().unwrap_or(0);
                let last = last_error.get(&plugin.dir).cloned();
                if let Some(winner) = winners.get(&id) {
                    let mut info = info(plugin, set.scope, input, &[], fp, last);
                    info.shadowed_by = Some(*winner);
                    out.infos.push(info);
                    continue;
                }
                winners.insert(id.clone(), set.scope);
                let (_, trusted, _) = origin_of(plugin, set.scope, input.trust);
                let is_locked = locked.contains(&id);
                let on = is_locked || input.effective.plugin_on(&id).value;
                let mut tool_names = Vec::new();
                let mut servers = Vec::new();
                let mut native_problems = Vec::new();
                let values = input.values.get(&id).unwrap_or(&empty);
                if on {
                    // Artifact types and the prompt section are data (like the prompt line),
                    // so they apply even before a workspace plugin is trusted.
                    native_problems.extend(native.data(plugin, &cfg.natives, input.types));
                    if let Some(p) = &plugin.manifest.prompt {
                        prompt_lines.push(p.clone());
                    }
                    for s in &plugin.skills {
                        if !skills.iter().any(|k| k.name == s.name) {
                            skills.push(s.clone());
                            skill_from.insert(s.name.clone(), (set.scope, Some(id.clone())));
                        }
                    }
                    for c in &plugin.commands {
                        if !out.commands.iter().any(|k| k.name == c.name) {
                            out.commands.push(c.clone());
                            command_from.push((set.scope, id.clone()));
                        }
                    }
                    agent_lists.push(plugin.agents.clone());
                }
                let has_code = !plugin.manifest.tools.is_empty() || !plugin.manifest.mcp_servers.is_empty() || plugin.manifest.backend.is_some();
                let mut backend_info = None;
                let mut python_problem = None;
                if on && trusted {
                    native_problems.extend(native.tools(plugin, input.base, &cfg.natives, &mut tool_names));
                    let python = PluginPython::new(pyenvs, &plugin.manifest.requirements, &plugin.dir);
                    python_problem = python.failure();
                    for tool in byob_plugins::python_tools_with(plugin, values, &cfg.python, &mut names, cfg.sandbox.as_ref(), Some(&python)) {
                        let name = tool.spec().name;
                        let declared = tool.declared().name.clone();
                        out.owners.insert(name.clone(), Owner { dir: plugin.dir.clone(), plugin: id.clone(), scope: set.scope, declared });
                        tool_names.push(name);
                        out.tools.register(tool);
                    }
                    let plugin_ref = PluginRef::new(plugin, values);
                    let missing = PluginRef::missing_settings(plugin, values);
                    if let (Some(decl), Some(sink)) = (&plugin.manifest.backend, sink.as_ref()) {
                        let make = Make {
                            plugin,
                            decl,
                            scope: set.scope,
                            chat: input.chat,
                            workspace: input.workspace,
                            fp,
                            plugin_ref: &plugin_ref,
                            python: &python,
                            sink,
                        };
                        let (bi, tools) = backend_tools(backends, &make, &missing, &mut names);
                        backend_info = Some(bi);
                        for tool in tools {
                            let name = tool.spec().name;
                            let declared = tool.declared().name.clone();
                            out.owners.insert(name.clone(), Owner { dir: plugin.dir.clone(), plugin: id.clone(), scope: set.scope, declared });
                            tool_names.push(name);
                            out.tools.register(tool);
                        }
                    }
                    for (server, config) in &plugin.manifest.mcp_servers {
                        if !missing.is_empty() {
                            servers.push(ServerInfo {
                                name: server.clone(),
                                state: ServerState::Off,
                                tools: 0,
                                error: Some(format!("fill in: {}", missing.join(", "))),
                            });
                            continue;
                        }
                        let config = config.with_settings(&plugin_ref.env);
                        let local = config.is_local();
                        let key = server_key(&id, server, &config, &plugin.dir, local.then_some(ws_local.as_path()));
                        let entry = pool.entries.entry(key.clone()).or_insert_with(|| {
                            out.connect.push(ConnectJob {
                                key: key.clone(),
                                plugin: id.clone(),
                                server: server.clone(),
                                config: config.clone(),
                                dir: plugin.dir.clone(),
                                sandbox: cfg.sandbox.clone().map(|sandbox| McpSandbox { sandbox, workspace: ws_local.clone() }),
                            });
                            PoolEntry { conn: Conn::Connecting, used: Instant::now(), dir: plugin.dir.clone() }
                        });
                        entry.used = Instant::now();
                        match &entry.conn {
                            Conn::Connecting => servers.push(ServerInfo { name: server.clone(), state: ServerState::Connecting, tools: 0, error: None }),
                            Conn::Failed(e) => servers.push(ServerInfo { name: server.clone(), state: ServerState::Failed, tools: 0, error: Some(e.clone()) }),
                            Conn::Ready { connector, tools } => {
                                let list = byob_plugins::connector_tools_from(&plugin_ref, local, Arc::clone(connector), tools.clone(), &mut names);
                                servers.push(ServerInfo {
                                    name: server.clone(),
                                    state: ServerState::Ready,
                                    tools: u32::try_from(list.len()).unwrap_or(u32::MAX),
                                    error: None,
                                });
                                for t in list {
                                    let name = t.spec().name;
                                    let declared = t.connector_tool().name.clone();
                                    out.owners.insert(name.clone(), Owner { dir: plugin.dir.clone(), plugin: id.clone(), scope: set.scope, declared });
                                    tool_names.push(name);
                                    out.tools.register(t);
                                }
                            }
                        }
                    }
                } else {
                    for server in plugin.manifest.mcp_servers.keys() {
                        servers.push(ServerInfo { name: server.clone(), state: ServerState::Off, tools: 0, error: None });
                    }
                    if has_code && set.scope == PluginScope::Workspace && !input.trust.contains_key(&id) {
                        out.undecided.push(id.clone());
                    }
                }
                tools_of.insert(plugin.dir.clone(), tool_names.iter().cloned().collect());
                let mut info = info(plugin, set.scope, input, &tool_names, fp, last);
                info.servers = servers;
                info.backend = backend_info;
                info.problems.extend(python_problem);
                info.problems.extend(native_problems);
                info.locked = is_locked;
                info.enabled |= is_locked;
                out.infos.push(info);
            }
        }
        native.finish(&mut out, &cfg.natives, input.types);
        // Standalone skills: the workspace's, then the global ones, then the built-in one.
        for set in &order {
            for s in &set.skills {
                if !skills.iter().any(|k| k.name == s.name) {
                    skills.push(s.clone());
                    skill_from.insert(s.name.clone(), (set.scope, None));
                }
            }
        }
        if !skills.iter().any(|k| k.name == builtin_skill.name) {
            skills.push(builtin_skill.clone());
        }
        out.slash = slash_menu(&out.commands, &command_from, &skills, &skill_from);
        out.tools.register(Arc::new(UseSkillTool::new(skills.clone())));
        let mut prompt = prompt_lines.join("\n");
        for part in [byob_plugins::skills_prompt(&skills), AUTHORING_LINE.to_owned()] {
            if !part.is_empty() {
                if !prompt.is_empty() {
                    prompt.push_str("\n\n");
                }
                prompt.push_str(&part);
            }
        }
        out.prompt = prompt;
        out.agents = byob_plugins::merge_agents(agent_lists);
        out.infos.sort_by(|a, b| b.scope.cmp(&a.scope).then_with(|| a.id.cmp(&b.id)));
        pool.gc();
        out
    }
}

/// One composition's native bindings (see [`crate::natives`]): what the plugins it sees
/// name, and what the switched-on ones offer.
struct NativeParts {
    /// Bundled plugin ids in order (their sections come first, in this order).
    bundled_order: Vec<String>,
    /// Native tools and artifact types some plugin here names: they are offered only while
    /// one of those plugins is on (the others stay as they are).
    claimed_tools: BTreeSet<String>,
    claimed_types: BTreeSet<String>,
    offered: BTreeSet<String>,
    declared_types: Vec<ArtifactType>,
    sections: Vec<(usize, String, Section)>,
}

impl NativeParts {
    fn new(order: &[&ExtSet], bundled: &ExtSet) -> Self {
        let mut claimed_tools = BTreeSet::new();
        let mut claimed_types = BTreeSet::new();
        for set in order {
            for p in set.found.active() {
                claimed_tools.extend(p.native_tools().into_iter().map(str::to_owned));
                claimed_types.extend(p.manifest.artifact_types.iter().map(|t| t.content_type.clone()));
            }
        }
        Self {
            bundled_order: bundled.found.plugins.iter().map(|p| p.id().to_owned()).collect(),
            claimed_tools,
            claimed_types,
            offered: BTreeSet::new(),
            declared_types: Vec::new(),
            sections: Vec::new(),
        }
    }

    /// A switched-on plugin's artifact types and prompt section. Returns its problems.
    fn data(&mut self, plugin: &Plugin, natives: &Natives, types: &ArtifactTypes) -> Vec<String> {
        let mut problems = Vec::new();
        for decl in &plugin.manifest.artifact_types {
            match artifact_type(plugin, decl, natives, types) {
                Ok((t, problem)) => {
                    problems.extend(problem);
                    self.declared_types.push(t);
                }
                Err(e) => problems.push(e),
            }
        }
        if let Some(src) = &plugin.manifest.system_prompt {
            let id = plugin.id().to_owned();
            let rank = self.bundled_order.iter().position(|b| *b == id).unwrap_or(self.bundled_order.len());
            match section(plugin, src) {
                Ok(Section::Native(n)) if !natives.prompts.contains_key(&n) => {
                    problems.push(format!("system_prompt: \"{n}\" isn't a prompt section built into this byob"));
                }
                Ok(s) => self.sections.push((rank, id, s)),
                Err(e) => problems.push(e),
            }
        }
        problems
    }

    /// A switched-on, trusted plugin's native tools (their names go into `names`). Returns
    /// its problems.
    fn tools(&mut self, plugin: &Plugin, base: &Registry, natives: &Natives, names: &mut Vec<String>) -> Vec<String> {
        let mut problems = Vec::new();
        for t in &plugin.manifest.tools {
            let ToolKind::Native(n) = &t.kind else { continue };
            if base.get(n).is_some() || natives.typed_tools.contains_key(n) {
                self.offered.insert(n.clone());
                names.push(n.clone());
            } else if !t.optional {
                problems.push(format!("tools: \"{n}\" isn't a tool built into this byob"));
            }
        }
        problems
    }

    /// Into `out`: the chat's artifact types, its native tools, the sections.
    fn finish(self, out: &mut Composed, natives: &Natives, types: &ArtifactTypes) {
        let Self { claimed_tools, claimed_types, offered, mut declared_types, mut sections, .. } = self;
        // The engine's order: unclaimed types stay, claimed ones only as a switched-on
        // plugin declares them; new types after.
        let chat_types = ArtifactTypes::empty();
        for t in types.all() {
            if let Some(i) = declared_types.iter().position(|d| d.content_type == t.content_type) {
                chat_types.register(declared_types.remove(i));
            } else if !claimed_types.contains(&t.content_type) {
                chat_types.register(t);
            }
        }
        for t in declared_types {
            chat_types.register(t);
        }
        // Native tools no switched-on plugin offers go; typed ones get the chat's types.
        for name in claimed_tools.difference(&offered) {
            out.tools.remove(name);
        }
        for name in &offered {
            if let Some(make) = natives.typed_tools.get(name) {
                out.tools.register(make(&chat_types));
            }
        }
        sections.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        let texts: Vec<String> = sections
            .into_iter()
            .filter_map(|(_, _, s)| match s {
                Section::Text(t) => Some(t),
                Section::Native(n) => natives.prompts.get(&n).map(|build| build(&chat_types)),
            })
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
            .collect();
        out.sections = texts.join("\n\n");
        out.artifact_types = chat_types;
    }
}

/// A plugin's system-prompt section, before the chat's artifact types are known.
enum Section {
    Text(String),
    /// Built by [`Natives::prompts`] from the chat's types.
    Native(String),
}

/// Read a plugin's `system_prompt`.
fn section(plugin: &Plugin, src: &PromptSource) -> Result<Section, String> {
    match src {
        PromptSource::Native(n) => Ok(Section::Native(n.clone())),
        PromptSource::File(f) => {
            let text = plugin.read(f).map_err(|e| format!("system_prompt: {e}"))?;
            if text.len() > MAX_SECTION_BYTES {
                return Err(format!("system_prompt: {f} is longer than {} KB", MAX_SECTION_BYTES / 1024));
            }
            Ok(Section::Text(text))
        }
    }
}

/// The artifact type a plugin declares: a native one (by id) with the manifest's overrides,
/// or a plain text type of its own. A guidance file that can't be read is a problem (the
/// native default stays); an unknown native id is an error for this type.
fn artifact_type(plugin: &Plugin, decl: &ManifestArtifactType, natives: &Natives, types: &ArtifactTypes) -> Result<(ArtifactType, Option<String>), String> {
    let mut t = match &decl.native {
        Some(n) => natives
            .artifact_types
            .get(n)
            .and_then(|ct| types.get(ct))
            .ok_or_else(|| format!("artifact_types: \"{n}\" isn't an artifact type built into this byob"))?,
        None => ArtifactType::new(&decl.content_type, decl.label.as_deref().unwrap_or("file"), byob_protocol::ArtifactKind::Text),
    };
    decl.content_type.clone_into(&mut t.content_type);
    if let Some(s) = &decl.suffixes {
        t.suffixes.clone_from(s);
    }
    if let Some(l) = &decl.label {
        l.clone_into(&mut t.label);
    }
    if let Some(d) = &decl.describe {
        d.clone_into(&mut t.describe);
    }
    if let Some(p) = &decl.pick {
        p.clone_into(&mut t.pick);
    }
    let mut problem = None;
    if let Some(g) = &decl.guidance {
        match plugin.read(g) {
            Ok(text) if text.len() <= MAX_SECTION_BYTES => t.guidance = text.trim().to_owned(),
            Ok(_) => problem = Some(format!("artifact_types: {g} is longer than {} KB", MAX_SECTION_BYTES / 1024)),
            Err(e) => problem = Some(format!("artifact_types: guidance: {e}")),
        }
    }
    Ok((t, problem))
}

/// The backend of `m.plugin` (made if needed) and the tools it serves; off while required
/// settings are `missing`.
fn backend_tools(backends: &mut Backends, m: &Make<'_>, missing: &[String], names: &mut ToolNames) -> (BackendInfo, Vec<Arc<BackendTool>>) {
    if !missing.is_empty() {
        return (BackendInfo { state: "off".into(), error: Some(format!("fill in: {}", missing.join(", "))), ..Default::default() }, Vec::new());
    }
    let backend = backends.ensure(m);
    let id = m.plugin.id();
    let tools = m
        .plugin
        .manifest
        .tools
        .iter()
        .filter(|t| t.kind == ToolKind::Backend)
        .map(|t| {
            let name = names.claim(&t.name, &byob_plugins::names::namespaced(id, &t.name));
            Arc::new(BackendTool::new(name, m.plugin_ref.clone(), t.clone(), Arc::clone(&backend)))
        })
        .collect();
    (crate::backends::info(&backend), tools)
}

/// The `/` menu: `/compact`, then commands, then skills; a name only once.
fn slash_menu(
    commands: &[Command],
    command_from: &[(PluginScope, String)],
    skills: &[Skill],
    skill_from: &BTreeMap<String, (PluginScope, Option<String>)>,
) -> Vec<SlashItem> {
    let one_line = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(160).collect::<String>();
    let mut out = vec![SlashItem {
        name: "compact".into(),
        kind: SlashKind::Builtin,
        description: "Summarize the conversation so far to free up context".into(),
        hint: "[what to keep]".into(),
        scope: None,
        plugin: None,
    }];
    for (c, (scope, plugin)) in commands.iter().zip(command_from) {
        if out.iter().any(|i| i.name == c.name) {
            continue;
        }
        out.push(SlashItem {
            name: c.name.clone(),
            kind: SlashKind::Command,
            description: one_line(&c.description),
            hint: one_line(&c.argument_hint),
            scope: Some(*scope),
            plugin: Some(plugin.clone()),
        });
    }
    for s in skills {
        if out.iter().any(|i| i.name == s.name) {
            continue;
        }
        let (scope, plugin) = skill_from.get(&s.name).cloned().map_or((None, None), |(scope, plugin)| (Some(scope), plugin));
        out.push(SlashItem { name: s.name.clone(), kind: SlashKind::Skill, description: one_line(&s.description), hint: String::new(), scope, plugin });
    }
    out
}

/// What the model gets for a message that starts with `/name`: a command's prompt with the
/// arguments filled in, or the request to use a skill. `None`: not one of the chat's (the
/// text goes as typed; `/compact` is handled before a turn starts).
pub(crate) fn expand_slash(text: &str, composed: &Composed) -> Option<String> {
    let (name, args) = byob_plugins::split_slash(text)?;
    let item = composed.slash.iter().find(|i| i.name == name)?;
    match item.kind {
        SlashKind::Builtin | SlashKind::Other => None,
        SlashKind::Command => composed.commands.iter().find(|c| c.name == name).map(|c| c.expand(args)),
        SlashKind::Skill => {
            let task =
                if args.is_empty() { "Ask me what I'd like it applied to if that isn't clear from the conversation.".to_owned() } else { args.to_owned() };
            Some(format!("Use the \"{name}\" skill for this: call use_skill with name \"{name}\" first, then follow it.\n\n{task}"))
        }
    }
}

/// How a client sees `plugin` (`tools`: its tools as offered to the model).
fn info(plugin: &Plugin, scope: PluginScope, input: &ComposeIn<'_>, tools: &[String], fp: u64, last_error: Option<String>) -> PluginInfo {
    let m = &plugin.manifest;
    let (origin, trusted, declined) = origin_of(plugin, scope, input.trust);
    let empty = BTreeMap::new();
    let values = input.values.get(plugin.id()).unwrap_or(&empty);
    let switch = input.effective.plugin_on(plugin.id());
    let mut problems = plugin.problems.clone();
    let missing = PluginRef::missing_settings(plugin, values);
    if !missing.is_empty() {
        problems.push(format!("needs settings: {}", missing.join(", ")));
    }
    {
        PluginInfo {
            id: m.id.clone(),
            name: m.name.clone(),
            version: m.version.clone(),
            description: m.description.clone(),
            icon: m.icon.clone(),
            scope,
            origin,
            dir: plugin.dir.display().to_string(),
            enabled: switch.value,
            enabled_from: switch.from,
            trusted,
            declined,
            shadowed_by: None,
            tools: tools.to_vec(),
            skills: plugin.skills.iter().map(|s| s.name.clone()).collect(),
            agents: plugin.agents.iter().map(|a| a.name.clone()).collect(),
            commands: plugin.commands.iter().map(|c| c.name.clone()).collect(),
            servers: Vec::new(),
            backend: None,
            views: m
                .views
                .iter()
                .filter_map(|v| {
                    let site = byob_protocol::Site::parse(&v.site)?;
                    Some(byob_protocol::ViewDecl { id: v.id.clone(), site, title: v.title.clone(), icon: v.icon.clone() })
                })
                .collect(),
            settings: m
                .settings
                .iter()
                .map(|s| PluginSetting {
                    key: s.key.clone(),
                    label: s.label.clone(),
                    help: s.help.clone(),
                    secret: s.secret,
                    required: s.required,
                    set: values.get(&s.key).or(s.default.as_ref()).is_some_and(|v| !v.trim().is_empty()),
                })
                .collect(),
            network: m.permissions.network,
            problems,
            last_error,
            fingerprint: format!("{fp:016x}"),
            locked: false,
            renderers: m.renderers.iter().map(|r| RendererDecl { content_type: r.content_type.clone(), renderer: r.renderer.clone() }).collect(),
            starters: m.starters.iter().map(|s| StarterDecl { title: s.title.clone(), prompt: s.prompt.clone() }).collect(),
        }
    }
}

/// Who wrote a plugin, and whether its code may run: (origin, trusted, declined).
fn origin_of(plugin: &Plugin, scope: PluginScope, trust: &BTreeMap<String, PluginTrust>) -> (PluginOrigin, bool, bool) {
    match scope {
        PluginScope::Chat => (PluginOrigin::Agent, true, false),
        PluginScope::Workspace => match trust.get(plugin.id()) {
            Some(PluginTrust::Made) => (PluginOrigin::Agent, true, false),
            Some(PluginTrust::Trusted) => (PluginOrigin::Repo, true, false),
            Some(PluginTrust::Declined) => (PluginOrigin::Repo, false, true),
            None => (PluginOrigin::Repo, false, false),
        },
        PluginScope::Bundled => (PluginOrigin::Bundled, true, false),
        PluginScope::Global if plugin.builtin => (PluginOrigin::Bundled, true, false),
        PluginScope::Global if plugin.dir.join(MADE_MARKER).exists() => (PluginOrigin::Agent, true, false),
        PluginScope::Global => (PluginOrigin::User, true, false),
    }
}

/// The pool key of one server: unchanged configuration (settings filled in), folder and
/// (for local servers) workspace → the same connection.
fn server_key(plugin: &str, server: &str, config: &McpServerConfig, dir: &Path, ws: Option<&Path>) -> String {
    let mut h = Fnv::default();
    h.write(serde_json::to_string(config).unwrap_or_default().as_bytes());
    h.write(dir.as_os_str().as_encoded_bytes());
    if let Some(ws) = ws {
        h.write(ws.as_os_str().as_encoded_bytes());
    }
    format!("{plugin}/{server}/{:016x}", h.finish())
}

/// The workspace's AGENTS.md, capped, for the system prompt (`None` if there is none).
pub(crate) fn agents_md(ws: &Path) -> Option<String> {
    let file = ws.join("AGENTS.md");
    let meta = std::fs::metadata(&file).ok().filter(|m| m.is_file())?;
    let bytes = std::fs::read(&file).ok()?;
    let mut text = String::from_utf8_lossy(bytes.get(..MAX_AGENTS_MD.min(bytes.len())).unwrap_or_default()).into_owned();
    if meta.len() > MAX_AGENTS_MD as u64 {
        text.push_str("\n…(AGENTS.md is longer; read the file for the rest)");
    }
    let text = text.trim();
    (!text.is_empty()).then(|| format!("## Workspace instructions (AGENTS.md)\n{text}"))
}
