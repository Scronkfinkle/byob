//! Workspaces and layered settings in the engine.
//!
//! A workspace is a folder with a name and its own settings (`byob_store::Workspaces`);
//! chats belong to the workspace of the folder they were created in. Every place that
//! needs a chat's settings (the turn, compaction, the usage meter, the header) asks
//! [`Engine::chat_settings`], which resolves global → workspace → chat with
//! `byob_types::config::resolve` and turns the result into the endpoint, the permission
//! policy and the environment plan for `run_command`.

use std::path::{Path, PathBuf};

use byob_protocol::workspace::{MAX_LISTED_FOLDERS, NewFolder};
use byob_protocol::{ConfigScope, ConfigView, FolderList, NoticeLevel, ServerMsg, WorkspaceSummary};
use byob_store::{Chat, Settings, WorkspaceRow};
use byob_tools::EnvPlan;
use byob_types::InferenceConfig;
use byob_types::config::{Effective, EnvKind, Keys, Layer, Level, Levels, origin, resolve};

use crate::{ClientId, CompactOptions, Engine, Input, Policy, chats, spawn_worker};

/// What one chat (or a client's new chat) runs with.
#[derive(Debug, Clone)]
pub(crate) struct ChatSettings {
    pub effective: Effective,
    /// The endpoint, with the key that belongs to it.
    pub inference: InferenceConfig,
    /// Where its commands run.
    pub env: EnvPlan,
}

/// Longest API key a client may send.
const MAX_KEY_CHARS: usize = byob_protocol::connect::MAX_KEY_CHARS;

fn env_plan(e: &Effective) -> EnvPlan {
    EnvPlan { container: e.container(), project: e.load_project, image: e.image.clone() }
}

/// The key a level keeps after its settings change: a new one if given (`""` removes it),
/// else the old one while the level still points at the same server.
fn next_key(old_url: Option<&str>, new_url: Option<&str>, given: Option<&str>, existing: Option<&str>) -> Option<String> {
    match given {
        Some(k) => Some(k.trim().to_owned()).filter(|k| !k.is_empty()),
        None => match (old_url, new_url) {
            (Some(old), Some(new)) if origin(old) == origin(new) => existing.map(str::to_owned),
            _ => None,
        },
    }
}

/// A folder named by a client: absolute (a leading `~/` is the home folder), existing,
/// and not the filesystem root.
pub(crate) fn existing_folder(path: &str) -> Result<PathBuf, String> {
    let p = byob_store::expand_home(Path::new(path.trim()));
    if !p.is_absolute() {
        return Err(format!("{path:?} isn't a full path (like /home/me/project)"));
    }
    let real = p.canonicalize().map_err(|e| format!("{}: {e}", p.display()))?;
    if !real.is_dir() {
        return Err(format!("{} isn't a folder", real.display()));
    }
    if real.parent().is_none() {
        return Err("the root of the filesystem can't be a workspace".into());
    }
    Ok(real)
}

/// Create a fresh folder for workspace `name` in `root`: its slug, or slug-2, slug-3, …
pub(crate) fn fresh_folder(root: &Path, name: &str) -> Result<PathBuf, String> {
    let slug = byob_store::folder_slug(name);
    std::fs::create_dir_all(root).map_err(|e| format!("couldn't create {}: {e}", root.display()))?;
    for n in 1..=99u32 {
        let candidate = if n == 1 { root.join(&slug) } else { root.join(format!("{slug}-{n}")) };
        match std::fs::create_dir(&candidate) {
            Ok(()) => return candidate.canonicalize().map_err(|e| e.to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("couldn't create {}: {e}", candidate.display())),
        }
    }
    Err(format!("{} already has folders called {slug} up to {slug}-99", root.display()))
}

/// The subfolders of `path` (default: the home folder), hidden ones left out.
pub(crate) fn folder_list(path: Option<&str>) -> FolderList {
    let asked = path.map(str::trim).filter(|p| !p.is_empty()).unwrap_or("~");
    let fail = |e: String| FolderList { path: asked.to_owned(), error: Some(e), ..FolderList::default() };
    let p = byob_store::expand_home(Path::new(asked));
    if !p.is_absolute() {
        return fail(format!("{asked:?} isn't a full path"));
    }
    let real = match p.canonicalize() {
        Ok(r) if r.is_dir() => r,
        Ok(r) => return fail(format!("{} isn't a folder", r.display())),
        Err(e) => return fail(format!("{}: {e}", p.display())),
    };
    let entries = match std::fs::read_dir(&real) {
        Ok(e) => e,
        Err(e) => return fail(format!("{}: {e}", real.display())),
    };
    let mut dirs: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter(|n| !n.starts_with('.'))
        .collect();
    dirs.sort_by_key(|d| d.to_lowercase());
    let truncated = dirs.len() > MAX_LISTED_FOLDERS;
    dirs.truncate(MAX_LISTED_FOLDERS);
    FolderList { path: real.display().to_string(), parent: real.parent().map(|p| p.display().to_string()), dirs, truncated, error: None }
}

impl Engine {
    /// A chat as the chat list shows it, with its workspace.
    pub(crate) fn chat_summary(&self, c: &Chat) -> byob_protocol::ChatSummary {
        chats::summary(c, self.chat_busy(&c.id), self.workspaces.by_folder(&c.workspace).map(|w| w.id.clone()))
    }

    /// Make `folder` a workspace if it isn't one; true if it is new.
    pub(crate) fn ensure_workspace(&mut self, folder: &Path) -> bool {
        match self.workspaces.ensure(folder) {
            Ok((_, new)) => new,
            Err(e) => {
                log::warn!("registering the workspace {}: {e}", folder.display());
                false
            }
        }
    }

    /// Recount chats per folder; folders seen on chats become workspaces.
    pub(crate) fn refresh_folder_stats(&mut self) {
        match self.store.folder_stats() {
            Ok(stats) => {
                for s in &stats {
                    self.ensure_workspace(&s.folder);
                }
                self.folder_stats = stats;
            }
            Err(e) => log::error!("counting chats per workspace: {e}"),
        }
    }

    fn summary_of(&self, w: &WorkspaceRow) -> WorkspaceSummary {
        let stats = self.folder_stats.iter().find(|s| s.folder == w.folder);
        WorkspaceSummary {
            id: w.id.clone(),
            name: w.name.clone(),
            folder: w.folder.display().to_string(),
            chats: stats.map_or(0, |s| s.chats),
            updated_at: stats.map_or(w.created_at, |s| s.updated_at.max(w.created_at)),
            env: w.config.env.unwrap_or_default(),
        }
    }

    /// The default workspace (where new chats go unless a client picks one), if registered.
    fn default_workspace_row(&self) -> Option<&WorkspaceRow> {
        self.workspaces.by_folder(&self.default_workspace)
    }

    pub(crate) fn workspaces_msg(&self) -> ServerMsg {
        let mut items: Vec<WorkspaceSummary> = self.workspaces.list().iter().map(|w| self.summary_of(w)).collect();
        items.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| a.name.cmp(&b.name)));
        ServerMsg::Workspaces {
            items,
            current: self.default_workspace_row().map(|w| w.id.clone()),
            root: self.workspaces_root.as_ref().map(|r| r.display().to_string()).unwrap_or_default(),
            support: self.envs.as_ref().map(|e| e.support()).unwrap_or_default(),
        }
    }

    pub(crate) fn broadcast_workspaces(&mut self) {
        let msg = self.workspaces_msg();
        self.broadcast(msg);
    }

    /// The workspace client `id`'s next new chat goes into: the one it picked, else the
    /// default one (registered or not: `None` means the default folder).
    pub(crate) fn new_chat_workspace(&self, id: ClientId) -> Option<WorkspaceRow> {
        let picked = self.clients.get(&id).and_then(|c| c.new_in.as_deref()).and_then(|w| self.workspaces.get(w));
        picked.or_else(|| self.default_workspace_row()).cloned()
    }

    /// `ClientMsg::NewChat`: a new chat in `workspace`, else in the workspace of the chat
    /// in view, else in the default one.
    pub(crate) fn new_chat(&mut self, client: ClientId, workspace: Option<String>) {
        let ws = match workspace {
            Some(id) if self.workspaces.get(&id).is_none() => {
                self.notice(client, "that workspace no longer exists".into());
                return;
            }
            Some(id) => Some(id),
            None => self.client_chat(client).and_then(|c| self.chat_folder(&c)).and_then(|f| self.workspaces.by_folder(&f).map(|w| w.id.clone())),
        };
        if let Some(c) = self.clients.get_mut(&client) {
            c.new_in = ws;
            c.new_layer = Layer::default();
            c.new_key = None;
        }
        self.show_chat(client, None);
    }

    /// A client's new chat was just created as `chat`: it keeps the settings (and key) the
    /// client gave it beforehand.
    pub(crate) fn adopt_new_chat_settings(&mut self, client: ClientId, chat: &str) {
        let Some(c) = self.clients.get_mut(&client) else { return };
        let (layer, key) = (std::mem::take(&mut c.new_layer), c.new_key.take());
        if !layer.is_empty()
            && let Err(e) = self.workspaces.set_chat_config(chat, layer)
        {
            log::warn!("saving the settings of chat {chat}: {e}");
        }
        if key.is_some() {
            self.set_scoped_key(&format!("chat:{chat}"), key);
        }
    }

    /// The folder of `chat` (loaded or stored).
    fn chat_folder(&self, chat: &str) -> Option<PathBuf> {
        if let Some(l) = self.live.get(chat) {
            return Some(l.workspace.as_ref().clone());
        }
        self.store.chat(chat).ok().flatten().map(|c| c.workspace)
    }

    /// The global level: settings.json, with the endpoint the engine uses now.
    pub(crate) fn global_layer(&self) -> Layer {
        Layer { base_url: Some(self.config.base_url.clone()), model: Some(self.config.model.clone()), ..self.defaults.clone() }
    }

    fn scoped_key(&self, scope: &str) -> Option<&String> {
        self.scoped_keys.get(scope).filter(|k| !k.is_empty())
    }

    /// Resolve `levels` and pick the endpoint's key from the level the resolver names.
    fn settle(&self, levels: Levels<'_>, ws: Option<&str>, chat_key: Option<&str>, keys: Keys) -> ChatSettings {
        let effective = resolve(levels, keys, self.forced);
        let api_key = match effective.key_from {
            Some(Level::Global) => self.config.api_key.clone(),
            Some(Level::Workspace) => ws.and_then(|w| self.scoped_key(&format!("workspace:{w}"))).cloned(),
            Some(Level::Chat) => chat_key.map(str::to_owned),
            _ => None,
        };
        let inference = InferenceConfig { base_url: effective.base_url.value.clone(), model: effective.model.value.clone(), api_key };
        ChatSettings { env: env_plan(&effective), inference, effective }
    }

    /// The settings of `chat`, or (no chat) of the new chat client `client` is about to start.
    pub(crate) fn chat_settings(&self, chat: Option<&str>, client: Option<ClientId>) -> ChatSettings {
        let global = self.global_layer();
        let (ws, own, chat_key) = match chat {
            Some(c) => {
                let ws = self.chat_folder(c).and_then(|f| self.workspaces.by_folder(&f).cloned());
                (ws, self.workspaces.chat_config(c).cloned(), self.scoped_key(&format!("chat:{c}")).cloned())
            }
            None => {
                let ws = client.and_then(|id| self.new_chat_workspace(id)).or_else(|| self.default_workspace_row().cloned());
                let pending = client.and_then(|id| self.clients.get(&id));
                (ws, pending.map(|p| p.new_layer.clone()), pending.and_then(|p| p.new_key.clone()))
            }
        };
        let keys = Keys {
            global: self.config.api_key.is_some(),
            workspace: ws.as_ref().is_some_and(|w| self.scoped_key(&format!("workspace:{}", w.id)).is_some()),
            chat: chat_key.is_some(),
        };
        let levels = Levels { global: Some(&global), workspace: ws.as_ref().map(|w| &w.config), chat: own.as_ref() };
        self.settle(levels, ws.as_ref().map(|w| w.id.as_str()), chat_key.as_deref(), keys)
    }

    /// The settings a workspace gives its new chats.
    fn workspace_settings(&self, ws: &WorkspaceRow) -> ChatSettings {
        let global = self.global_layer();
        let keys = Keys { global: self.config.api_key.is_some(), workspace: self.scoped_key(&format!("workspace:{}", ws.id)).is_some(), chat: false };
        self.settle(Levels { global: Some(&global), workspace: Some(&ws.config), chat: None }, Some(&ws.id), None, keys)
    }

    /// Where a workspace's commands run (for its environment panel).
    pub(crate) fn workspace_plan(&self, id: &str) -> EnvPlan {
        self.workspaces.get(id).map(|w| self.workspace_settings(w).env).unwrap_or_default()
    }

    /// A client for `config`: the engine's own for the global endpoint, else a new one
    /// (probed once, in the background).
    pub(crate) fn llm_for(&mut self, config: &InferenceConfig) -> Result<byob_llm::Client, String> {
        if *config == self.config {
            return self.llm.clone();
        }
        let client = byob_llm::Client::new(config.clone()).map_err(|e| e.to_string())?;
        self.endpoints.probe(&client, &self.inbox);
        Ok(client)
    }

    /// The system prompt with the instructions of every level appended.
    pub(crate) fn prompt_with_instructions(&self, e: &Effective) -> String {
        let extra = e.instructions_text();
        if extra.is_empty() { self.system_prompt.clone() } else { format!("{}\n\nThe user's instructions for this chat:\n{extra}", self.system_prompt) }
    }

    /// The chat's compaction settings. The engine's own `compaction.auto: false` turns it off
    /// everywhere (tests); `--compact-at` turns it on everywhere.
    pub(crate) fn compact_options(&self, e: &Effective) -> CompactOptions {
        CompactOptions {
            auto: (self.compaction.auto && e.auto_compact.value) || self.compaction.at_tokens.is_some(),
            threshold_percent: e.compact_percent.value,
            at_tokens: self.compaction.at_tokens,
            keep_turns: self.compaction.keep_turns,
        }
    }

    pub(crate) fn policy_for(&self, e: &Effective) -> Policy {
        self.forced_policy.unwrap_or_else(|| Policy::for_approvals(e.approvals.value))
    }

    /// Change (or remove, `None`) the key of a scope and save secrets.json.
    pub(crate) fn set_scoped_key(&mut self, scope: &str, key: Option<String>) {
        let changed = match &key {
            Some(k) => self.scoped_keys.insert(scope.to_owned(), k.clone()).as_ref() != Some(k),
            None => self.scoped_keys.remove(scope).is_some(),
        };
        let Some(paths) = self.paths.as_ref().filter(|_| changed) else { return };
        let mut secrets = byob_store::Secrets::load(&paths.secrets()).value;
        match key {
            Some(k) => secrets.scoped_keys.insert(scope.to_owned(), k),
            None => secrets.scoped_keys.remove(scope),
        };
        if let Err(e) = secrets.save(&paths.secrets()) {
            log::error!("saving an API key: {e}");
        }
    }

    /// Record a workspace's environment choice as the folder's answers too, so its
    /// environment panel and the ask-once questions agree with it.
    fn record_env_choice(&self, folder: &Path, env: Option<EnvKind>) {
        use byob_protocol::EnvAction;
        let Some(envs) = self.envs.clone() else { return };
        let actions = match env {
            Some(EnvKind::Container) => vec![EnvAction::Container { on: Some(true) }],
            Some(EnvKind::Host) => vec![EnvAction::Container { on: Some(false) }],
            Some(EnvKind::Nix) => vec![EnvAction::Container { on: Some(false) }, EnvAction::ProjectEnv { load: Some(true) }],
            Some(EnvKind::Auto) | None => return,
        };
        let folder = folder.to_path_buf();
        spawn_worker("byob-env", move || {
            for a in &actions {
                if let Err(e) = envs.apply(&folder, a) {
                    log::warn!("recording the environment of {}: {e}", folder.display());
                }
            }
        });
    }

    // ---- workspace messages -----------------------------------------------------------

    pub(crate) fn create_workspace(&mut self, client: ClientId, name: &str, folder: NewFolder, config: Layer) {
        let result = (|| {
            let layer = config.validated(Level::Workspace)?;
            let name = byob_store::clean_name(name).map_err(|e| e.to_string())?;
            let folder = match folder {
                NewFolder::Existing { path } => existing_folder(&path)?,
                NewFolder::Create => {
                    let root = self.workspaces_root.clone().ok_or("byob has no folder for new workspaces here; pick an existing folder")?;
                    fresh_folder(&root, &name)?
                }
            };
            let row = self.workspaces.create(&name, &folder, layer).map_err(|e| match e {
                byob_store::StoreError::Exists(what) => format!("{what} pick it in the chat list"),
                other => format!("couldn't save the workspace: {other}"),
            })?;
            Ok::<_, String>(row)
        })();
        match result {
            Ok(row) => {
                log::info!("workspace {:?} at {}", row.name, row.folder.display());
                self.record_env_choice(&row.folder, row.config.env);
                self.broadcast_workspaces();
                self.send_to(client, ServerMsg::WorkspaceCreated { id: Some(row.id.clone()), error: None });
                self.new_chat(client, Some(row.id));
            }
            Err(e) => self.send_to(client, ServerMsg::WorkspaceCreated { id: None, error: Some(e) }),
        }
    }

    pub(crate) fn rename_workspace(&mut self, client: ClientId, id: &str, name: &str) {
        match self.workspaces.rename(id, name) {
            Ok(()) => self.broadcast_workspaces(),
            Err(e) => self.notice(client, format!("couldn't rename the workspace: {e}")),
        }
    }

    /// Every chat of `folder`, by id.
    fn chats_in(&self, folder: &Path) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        let mut after = None;
        loop {
            let page = self.store.list_chats(byob_store::MAX_PAGE, after.as_ref()).map_err(|e| e.to_string())?;
            out.extend(page.chats.iter().filter(|c| c.workspace == folder).map(|c| c.id.clone()));
            match page.next {
                Some(n) if !page.chats.is_empty() => after = Some(n),
                _ => return Ok(out),
            }
        }
    }

    /// `ClientMsg::RemoveWorkspace`: forget the workspace and delete its chats (the folder
    /// stays). `clean`: also what byob keeps for it elsewhere (packages, its container).
    pub(crate) fn remove_workspace(&mut self, client: ClientId, id: &str, clean: bool) {
        let Some(row) = self.workspaces.get(id).cloned() else {
            self.notice(client, "that workspace no longer exists".into());
            return;
        };
        let chats = match self.chats_in(&row.folder) {
            Ok(c) => c,
            Err(e) => {
                self.notice(client, format!("couldn't list the workspace's chats: {e}"));
                return;
            }
        };
        if chats.iter().any(|c| self.chat_busy(c)) {
            self.notice(client, "a reply is running in one of its chats; stop it first".into());
            return;
        }
        let mut failed = Vec::new();
        for c in &chats {
            if let Err(e) = self.remove_chat(c) {
                failed.push(e);
            }
        }
        if !failed.is_empty() {
            self.notice(client, format!("couldn't delete {} of its chats ({}); the workspace stays", failed.len(), failed.join("; ")));
            self.refresh_folder_stats();
            self.broadcast_workspaces();
            return;
        }
        if let Err(e) = self.workspaces.remove(id) {
            self.notice(client, format!("couldn't remove the workspace: {e}"));
            return;
        }
        self.set_scoped_key(&format!("workspace:{id}"), None);
        let mut moved = Vec::new();
        for (cid, c) in self.clients.iter_mut().filter(|(_, c)| c.new_in.as_deref() == Some(id)) {
            c.new_in = None;
            if c.chat.is_none() && c.attached {
                moved.push(*cid);
            }
        }
        self.refresh_folder_stats();
        self.broadcast_workspaces();
        // Their new chat now starts in the default workspace.
        for cid in moved {
            self.show_chat(cid, None);
        }
        let n = chats.len();
        let text = format!(
            "Removed the workspace {:?} and its {n} chat{} from byob; {} and its files are untouched.",
            row.name,
            if n == 1 { "" } else { "s" },
            row.folder.display()
        );
        self.send_to(client, ServerMsg::Notice { level: NoticeLevel::Info, text });
        if clean && let Some(envs) = self.envs.clone() {
            let (inbox, folder) = (self.inbox.clone(), row.folder.clone());
            spawn_worker("byob-env", move || {
                let (level, text) = match envs.forget(&folder) {
                    Ok(t) => (NoticeLevel::Info, t),
                    Err(e) => (NoticeLevel::Error, format!("couldn't clean up after the workspace: {e}")),
                };
                let _ = inbox.send(Input::Reply { client, msg: Box::new(ServerMsg::Notice { level, text }) });
            });
        }
    }

    fn view(&self, client: ClientId, scope: &ConfigScope) -> Result<ConfigView, String> {
        Ok(match scope {
            ConfigScope::Global => {
                let global = self.global_layer();
                let keys = Keys { global: self.config.api_key.is_some(), ..Keys::default() };
                ConfigView {
                    scope: scope.clone(),
                    effective: self.settle(Levels { global: Some(&global), ..Levels::default() }, None, None, keys).effective,
                    own: global,
                    has_key: self.config.api_key.is_some(),
                    title: "Defaults for every chat".into(),
                    workspace: None,
                }
            }
            ConfigScope::Workspace { id } => {
                let ws = self.workspaces.get(id).ok_or("that workspace no longer exists")?;
                ConfigView {
                    scope: scope.clone(),
                    own: ws.config.clone(),
                    effective: self.workspace_settings(ws).effective,
                    has_key: self.scoped_key(&format!("workspace:{id}")).is_some(),
                    title: ws.name.clone(),
                    workspace: Some(self.summary_of(ws)),
                }
            }
            ConfigScope::Chat => {
                let chat = self.client_chat(client);
                let pending = self.clients.get(&client);
                let (own, has_key, title, ws) = match &chat {
                    Some(c) => {
                        let row = self.store.chat(c).ok().flatten();
                        let ws = row.as_ref().and_then(|r| self.workspaces.by_folder(&r.workspace)).cloned();
                        let own = self.workspaces.chat_config(c).cloned().unwrap_or_default();
                        (own, self.scoped_key(&format!("chat:{c}")).is_some(), row.map(|r| r.title).unwrap_or_default(), ws)
                    }
                    None => (
                        pending.map(|p| p.new_layer.clone()).unwrap_or_default(),
                        pending.is_some_and(|p| p.new_key.is_some()),
                        "New chat".to_owned(),
                        self.new_chat_workspace(client),
                    ),
                };
                ConfigView {
                    scope: scope.clone(),
                    own,
                    effective: self.chat_settings(chat.as_deref(), Some(client)).effective,
                    has_key,
                    title,
                    workspace: ws.as_ref().map(|w| self.summary_of(w)),
                }
            }
        })
    }

    pub(crate) fn send_config(&mut self, client: ClientId, scope: ConfigScope) {
        match self.view(client, &scope) {
            Ok(v) => self.send_to(client, ServerMsg::Config(Box::new(v))),
            Err(e) => self.notice(client, e),
        }
    }

    /// `ClientMsg::SetConfig`: replace one level's own settings (and its key).
    pub(crate) fn set_config(&mut self, client: ClientId, scope: ConfigScope, layer: Layer, api_key: Option<String>) {
        if let Err(e) = self.apply_config(client, &scope, layer, api_key) {
            self.notice(client, e);
        }
        // Answer either way, so the form shows what is in effect.
        self.send_config(client, scope);
        self.broadcast_state();
        let viewed: Vec<String> = self.clients.values().filter_map(|c| c.chat.clone()).collect();
        for chat in viewed {
            self.send_usage(&chat);
        }
    }

    fn apply_config(&mut self, client: ClientId, scope: &ConfigScope, layer: Layer, api_key: Option<String>) -> Result<(), String> {
        if api_key.as_ref().is_some_and(|k| k.chars().count() > MAX_KEY_CHARS) {
            return Err("the API key is too long".into());
        }
        match scope {
            ConfigScope::Global => {
                let mut layer = layer.validated(Level::Global)?;
                // The global endpoint changes through the connection form (SetInference).
                layer.base_url = None;
                layer.model = None;
                if let Some(paths) = &self.paths {
                    Settings::update(&paths.settings(), |s| s.set_global_layer(&layer)).map_err(|e| format!("couldn't save the settings: {e}"))?;
                }
                self.defaults = layer;
            }
            ConfigScope::Workspace { id } => {
                let layer = layer.validated(Level::Workspace)?;
                let old = self.workspaces.get(id).ok_or("that workspace no longer exists")?.clone();
                let scope_key = format!("workspace:{id}");
                let key =
                    next_key(old.config.base_url.as_deref(), layer.base_url.as_deref(), api_key.as_deref(), self.scoped_key(&scope_key).map(String::as_str));
                let env_changed = old.config.env != layer.env;
                self.workspaces.set_config(id, layer.clone()).map_err(|e| format!("couldn't save the workspace: {e}"))?;
                self.set_scoped_key(&scope_key, key);
                if env_changed {
                    self.record_env_choice(&old.folder, layer.env);
                }
                self.broadcast_workspaces();
            }
            ConfigScope::Chat => {
                let layer = layer.validated(Level::Chat)?;
                match self.client_chat(client) {
                    Some(chat) => {
                        let scope_key = format!("chat:{chat}");
                        let old = self.workspaces.chat_config(&chat).cloned().unwrap_or_default();
                        let key =
                            next_key(old.base_url.as_deref(), layer.base_url.as_deref(), api_key.as_deref(), self.scoped_key(&scope_key).map(String::as_str));
                        self.workspaces.set_chat_config(&chat, layer).map_err(|e| format!("couldn't save the chat's settings: {e}"))?;
                        self.set_scoped_key(&scope_key, key);
                    }
                    None => {
                        let Some(c) = self.clients.get_mut(&client) else { return Ok(()) };
                        c.new_key = next_key(c.new_layer.base_url.as_deref(), layer.base_url.as_deref(), api_key.as_deref(), c.new_key.as_deref());
                        c.new_layer = layer;
                    }
                }
            }
        }
        Ok(())
    }

    /// `ClientMsg::ListFolders`, off the engine thread (a slow disk shouldn't stall it).
    pub(crate) fn list_folders(&mut self, client: ClientId, path: Option<String>) {
        let inbox = self.inbox.clone();
        spawn_worker("byob-folders", move || {
            let list = folder_list(path.as_deref());
            let _ = inbox.send(Input::Reply { client, msg: Box::new(ServerMsg::Folders(list)) });
        });
    }

    /// `ClientMsg::CheckContainers`: ask the runtime, off the engine thread.
    pub(crate) fn check_containers(&mut self, client: ClientId) {
        let Some(envs) = self.envs.clone() else {
            let msg = ServerMsg::ContainersChecked { ok: false, message: "Containers aren't available in this build.".into() };
            self.send_to(client, msg);
            return;
        };
        let inbox = self.inbox.clone();
        spawn_worker("byob-containers", move || {
            let (ok, message) = match envs.check_containers() {
                Ok(m) => (true, m),
                Err(e) => (false, e),
            };
            let _ = inbox.send(Input::Reply { client, msg: Box::new(ServerMsg::ContainersChecked { ok, message }) });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_stay_with_their_server() {
        assert_eq!(next_key(None, Some("http://a/v1"), Some(" sk "), None).as_deref(), Some("sk"));
        assert_eq!(next_key(Some("http://a/v1"), Some("http://a/v2"), None, Some("old")).as_deref(), Some("old"));
        assert_eq!(next_key(Some("http://a/v1"), Some("http://b/v1"), None, Some("old")), None, "a key never follows to another server");
        assert_eq!(next_key(Some("http://a/v1"), None, None, Some("old")), None, "no URL of its own, no key");
        assert_eq!(next_key(Some("http://a/v1"), Some("http://a/v1"), Some(""), Some("old")), None);
    }

    #[test]
    fn folders_from_clients_are_checked_and_fresh_ones_get_free_names() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().canonicalize().unwrap();
        assert_eq!(existing_folder(real.to_str().unwrap()).unwrap(), real);
        assert!(existing_folder("relative/path").is_err());
        assert!(existing_folder(real.join("missing").to_str().unwrap()).is_err());
        std::fs::write(real.join("file"), "x").unwrap();
        assert!(existing_folder(real.join("file").to_str().unwrap()).is_err());
        assert!(existing_folder("/").is_err());
        let a = fresh_folder(&real.join("root"), "My Data!").unwrap();
        let b = fresh_folder(&real.join("root"), "my data").unwrap();
        assert_eq!((a.file_name().unwrap().to_str(), b.file_name().unwrap().to_str()), (Some("my-data"), Some("my-data-2")));
        assert!(a.is_dir() && b.is_dir());
    }

    #[test]
    fn folder_lists_show_subfolders_only() {
        let d = tempfile::tempdir().unwrap();
        for f in ["b", "A", ".hidden"] {
            std::fs::create_dir(d.path().join(f)).unwrap();
        }
        std::fs::write(d.path().join("file.txt"), "x").unwrap();
        let l = folder_list(Some(d.path().to_str().unwrap()));
        assert_eq!((l.dirs, l.error, l.truncated), (vec!["A".to_owned(), "b".to_owned()], None, false));
        assert!(l.parent.is_some());
        assert!(folder_list(Some("not/absolute")).error.is_some());
        assert!(folder_list(Some(d.path().join("file.txt").to_str().unwrap())).error.is_some());
        assert!(folder_list(Some(d.path().join("nope").to_str().unwrap())).error.is_some());
    }
}
