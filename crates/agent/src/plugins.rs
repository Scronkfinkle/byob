//! The engine's side of plugins: composing each turn's tools and prompt
//! ([`Engine::compose_for`]), the requests authoring tools make from turn threads
//! ([`PluginOp`]), MCP servers connecting on worker threads, plugin cards on steps, and the
//! clients' plugin messages (switches, settings, keep, remove, trust).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::time::Duration;

use byob_plugins::mcp::McpConnector;
use byob_plugins::{Connector, ConnectorTool};
use byob_protocol::{ConfigScope, Extensions as ExtMsg, NoticeLevel, PluginInfo, PluginLoadError, PluginScope, ServerMsg, Step};
use byob_store::{GrantScope, PluginTrust, Secrets, Settings};
use byob_tools::Registry;
use byob_types::config::Effective;

use crate::extensions::{ComposeIn, Composed, ConnectJob, Owner};
use crate::{ClientId, Engine, Input, spawn_worker};

/// How long a tool waits for the engine to answer a plugin request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// What an authoring tool (or a turn) asks the engine to do with the chat's plugins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PluginOp {
    /// Just describe them.
    Status,
    /// Read the chat's plugin folders again and give the running turn the new tools.
    Reload,
    /// byob wrote plugin `id` into the chat's workspace: its code may run. Then reload.
    Made { id: String },
    /// Plugin `id` was removed from the chat's workspace: forget what was known. Then reload.
    Forget { id: String },
    /// The user answered whether the workspace's own plugins may run. Then reload.
    Trust { plugins: Vec<String>, trust: bool },
}

/// The engine's answer: the chat's plugins after the request.
pub(crate) struct PluginReply {
    pub infos: Vec<PluginInfo>,
    pub errors: Vec<PluginLoadError>,
    /// The chat's tools now (base + plugins).
    pub tools: Registry,
    pub owners: BTreeMap<String, Owner>,
    /// Where this chat's scratch plugins live.
    pub scratch_dir: PathBuf,
    /// Where plugins kept everywhere go.
    pub global_dir: Option<PathBuf>,
    /// The bundled plugins' files by id (they aren't on disk; reading or forking one uses these).
    pub bundled: BTreeMap<String, Arc<BTreeMap<String, String>>>,
}

impl PluginReply {
    /// The copy of plugin `id` the chat uses (not shadowed).
    pub fn find(&self, id: &str) -> Option<&PluginInfo> {
        self.infos.iter().find(|p| p.id == id && p.shadowed_by.is_none())
    }

    /// The model-facing name of the tool `declared` in the plugin in `dir`.
    pub fn tool_named(&self, dir: &Path, declared: &str) -> Option<String> {
        self.owners.iter().find(|(_, o)| o.dir == dir && o.declared == declared).map(|(n, _)| n.clone())
    }
}

/// Ask the engine from a turn thread and wait for the answer (or `cancel`).
pub(crate) fn request(inbox: &Sender<Input>, chat: &str, op: PluginOp, cancel: &AtomicBool) -> Result<PluginReply, String> {
    let (reply, answer) = channel();
    inbox.send(Input::Plugins { chat: chat.to_owned(), op, reply }).map_err(|_| "byob is shutting down".to_owned())?;
    let started = std::time::Instant::now();
    loop {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("stopped".into());
        }
        match answer.recv_timeout(Duration::from_millis(50)) {
            Ok(r) => return Ok(r),
            Err(RecvTimeoutError::Timeout) if started.elapsed() < REQUEST_TIMEOUT => {}
            Err(_) => return Err("plugins aren't available here".into()),
        }
    }
}

/// Connect to an MCP server and list its tools (on a worker thread).
fn connect(job: &ConnectJob) -> Result<(Arc<dyn Connector>, Vec<ConnectorTool>), String> {
    let c = McpConnector::connect_in(&job.config, Some(&job.dir), job.sandbox.as_ref())?;
    let tools = c.list_tools()?;
    Ok((Arc::new(c), tools))
}

/// `.byob/plugins/<id>/byob-plugin.json` (as a step shows it): the plugin a file tool created.
fn created_plugin(path: &str) -> Option<&str> {
    let rest = path.strip_prefix(".byob/plugins/")?;
    let (id, file) = rest.split_once('/')?;
    (file == byob_plugins::MANIFEST && byob_types::config::plugin_id_ok(id)).then_some(id)
}

impl Engine {
    /// The folder a chat (or client `client`'s new chat) works in.
    pub(crate) fn ext_workspace(&self, chat: Option<&str>, client: Option<ClientId>) -> PathBuf {
        if let Some(c) = chat {
            if let Some(l) = self.live.get(c) {
                return l.workspace.as_ref().clone();
            }
            if let Some(w) = self.store.chat(c).ok().flatten() {
                return w.workspace;
            }
        }
        client.and_then(|id| self.new_chat_workspace(id)).map_or_else(|| self.default_workspace.clone(), |w| w.folder)
    }

    /// The plugins of `chat` (or of client `client`'s new chat): read again where files
    /// changed (`refresh`), then composed with the base tools. `None` without plugins.
    pub(crate) fn compose_for(&mut self, chat: Option<&str>, client: Option<ClientId>, refresh: bool) -> Option<Composed> {
        self.ext.as_ref()?;
        let ws = self.ext_workspace(chat, client);
        let effective = self.chat_settings(chat, client).effective;
        let mut changed_any = false;
        if refresh && let Some(ext) = self.ext.as_mut() {
            let (changed, any) = ext.refresh(chat, &ws);
            changed_any = any;
            let names: Vec<(PluginScope, Vec<String>)> =
                changed.iter().map(|(scope, dir)| (*scope, ext.tools_of.get(dir).map(|t| t.iter().cloned().collect()).unwrap_or_default())).collect();
            for (scope, tools) in names {
                self.revoke_grants(scope, chat, &ws, &tools);
            }
        }
        let composed = self.compose_now(chat, &ws, &effective);
        if changed_any {
            self.extensions_changed();
        }
        composed
    }

    fn compose_now(&mut self, chat: Option<&str>, ws: &Path, effective: &Effective) -> Option<Composed> {
        let trust = self.workspaces.by_folder(ws).map(|w| w.plugin_trust.clone()).unwrap_or_default();
        let ext = self.ext.as_mut()?;
        let input = ComposeIn { chat, workspace: ws, effective, trust: &trust, values: &self.plugin_values, base: &self.tools, types: &self.artifact_types };
        let mut composed = ext.compose(&input);
        for job in std::mem::take(&mut composed.connect) {
            let inbox = self.inbox.clone();
            log::info!("plugin {}: starting MCP server {}", job.plugin, job.server);
            spawn_worker("byob-mcp-connect", move || {
                let result = connect(&job);
                match &result {
                    Ok((_, tools)) => log::info!("plugin {}: MCP server {}: {} tools", job.plugin, job.server, tools.len()),
                    Err(e) => log::warn!("plugin {}: MCP server {}: {e}", job.plugin, job.server),
                }
                let _ = inbox.send(Input::Connected { key: job.key, result });
            });
        }
        Some(composed)
    }

    /// A plugin's code changed: "always allow" for its tools no longer stands.
    fn revoke_grants(&mut self, scope: PluginScope, chat: Option<&str>, ws: &Path, tools: &[String]) {
        if tools.is_empty() {
            return;
        }
        let chats: Vec<String> = match scope {
            PluginScope::Chat => chat.map(str::to_owned).into_iter().collect(),
            PluginScope::Workspace => self.chats_in(ws).unwrap_or_default(),
            PluginScope::Global | PluginScope::Bundled => self.all_chats(),
        };
        for tool in tools {
            log::info!("plugin tool {tool} changed; its \"always allow\" grants are withdrawn");
            let mut scopes = vec![GrantScope::Global];
            scopes.extend(chats.iter().cloned().map(GrantScope::Chat));
            for s in &scopes {
                if let Err(e) = self.store.revoke(s, tool) {
                    log::warn!("withdrawing the grant for {tool}: {e}");
                }
            }
        }
    }

    fn all_chats(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut after = None;
        loop {
            let Ok(page) = self.store.list_chats(byob_store::MAX_PAGE, after.as_ref()) else { return out };
            out.extend(page.chats.iter().map(|c| c.id.clone()));
            match page.next {
                Some(n) if !page.chats.is_empty() => after = Some(n),
                _ => return out,
            }
        }
    }

    /// Plugins changed: running turns get the new tools from their next round, and every
    /// client the new list.
    pub(crate) fn extensions_changed(&mut self) {
        if self.ext.is_none() {
            return;
        }
        let running: Vec<(u64, String)> = self.turns.iter().map(|(id, t)| (*id, t.chat.clone())).collect();
        for (entry, chat) in running {
            if let Some(c) = self.compose_for(Some(&chat), None, false)
                && let Some(run) = self.turns.get_mut(&entry)
            {
                run.tools.swap(c.tools);
                run.owners = c.owners;
            }
        }
        let clients: Vec<ClientId> = self.clients.iter().filter(|(_, c)| c.attached).map(|(id, _)| *id).collect();
        for id in clients {
            self.send_extensions(id, false);
        }
        self.sweep_views();
    }

    /// Send client `id` the plugins of the chat it looks at.
    pub(crate) fn send_extensions(&mut self, id: ClientId, refresh: bool) {
        let chat = self.client_chat(id);
        let Some(c) = self.compose_for(chat.as_deref(), Some(id), refresh) else { return };
        let ws = self.ext_workspace(chat.as_deref(), Some(id));
        let workspace = self.workspaces.by_folder(&ws).map(|w| w.id.clone());
        let mut plugins = c.infos;
        let mut errors = c.errors;
        let mut slash = c.slash;
        // Clients before 13.4 don't know the bundled scope: to them it's global (with origin
        // bundled, which they do know).
        let knows_bundled = self.clients.get(&id).is_some_and(|c| c.caps.iter().any(|c| c == byob_protocol::version::CAP_BUNDLED_SCOPE));
        if !knows_bundled {
            let global = |s: &mut PluginScope| {
                if *s == PluginScope::Bundled {
                    *s = PluginScope::Global;
                }
            };
            for p in &mut plugins {
                global(&mut p.scope);
                if let Some(s) = p.shadowed_by.as_mut() {
                    global(s);
                }
            }
            for e in &mut errors {
                global(&mut e.scope);
            }
            for s in slash.iter_mut().filter_map(|s| s.scope.as_mut()) {
                global(s);
            }
        }
        self.send_to(id, ServerMsg::Extensions(Box::new(ExtMsg { chat, workspace, plugins, errors, slash })));
    }

    /// `Input::Plugins`: a request from an authoring tool or a turn.
    pub(crate) fn plugin_op(&mut self, chat: &str, op: PluginOp, reply: &Sender<PluginReply>) {
        let ws = self.ext_workspace(Some(chat), None);
        let record = |this: &mut Self, id: &str, trust: Option<PluginTrust>| {
            let row = match this.workspaces.ensure(&ws) {
                Ok((row, _)) => row,
                Err(e) => {
                    log::warn!("registering the workspace {}: {e}", ws.display());
                    return;
                }
            };
            if let Err(e) = this.workspaces.set_plugin_trust(&row.id, id, trust) {
                log::warn!("saving what byob knows about plugin {id}: {e}");
            }
        };
        match &op {
            PluginOp::Status | PluginOp::Reload => {}
            PluginOp::Made { id } => record(self, id, Some(PluginTrust::Made)),
            PluginOp::Forget { id } => record(self, id, None),
            PluginOp::Trust { plugins, trust } => {
                for p in plugins {
                    record(self, p, Some(if *trust { PluginTrust::Trusted } else { PluginTrust::Declined }));
                    if !trust && let Some(ext) = self.ext.as_mut() {
                        ext.backends.stop_plugin(p, "the user declined it");
                    }
                }
            }
        }
        let refresh = op != PluginOp::Status;
        let Some(c) = self.compose_for(Some(chat), None, refresh) else { return };
        if refresh {
            // Also when the files didn't change (a trust answer): switches and trust did.
            self.extensions_changed();
        }
        let Some(ext) = self.ext.as_mut() else { return };
        let bundled = ext.all_bundled_files();
        let _ = reply.send(PluginReply {
            infos: c.infos,
            errors: c.errors,
            tools: c.tools,
            owners: c.owners,
            scratch_dir: ext.scratch_dir(chat),
            global_dir: ext.global_dir().map(Path::to_path_buf),
            bundled,
        });
    }

    /// `Input::Connected`: an MCP server is up (or failed); its tools join from the next round.
    pub(crate) fn server_connected(&mut self, key: &str, result: Result<(Arc<dyn Connector>, Vec<ConnectorTool>), String>) {
        let Some(ext) = self.ext.as_mut() else { return };
        ext.pool.connected(key, result);
        self.extensions_changed();
    }

    /// A step of a running turn changed: plugin cards, last errors, and plugins the file
    /// tools wrote into the workspace.
    pub(crate) fn plugin_step(&mut self, entry: u64, step: &mut Step) {
        if self.ext.is_none() || !step.status.finished() {
            return;
        }
        let Some(run) = self.turns.get(&entry) else { return };
        let chat = run.chat.clone();
        let event = serde_json::json!({"entry": entry, "step": step.id, "tool": step.tool, "status": step.status, "title": step.title});
        self.emit(&chat, "step_done", event);
        let Some(run) = self.turns.get(&entry) else { return };
        if self.step_views.get(&chat).is_some_and(|(tool, _)| *tool == step.tool)
            && let Some((_, view)) = self.step_views.remove(&chat)
        {
            match view.check() {
                Ok(()) if step.views.len() < byob_protocol::view::MAX_STEP_VIEWS => step.views.push(view),
                Ok(()) => log::warn!("step {}: too many views; {} dropped", step.id, view.kind),
                Err(e) => log::warn!("step {}: view {} dropped: {e}", step.id, view.kind),
            }
        }
        if let Some(owner) = run.owners.get(&step.tool).cloned()
            && let Some(ext) = self.ext.as_mut()
        {
            match step.status {
                byob_protocol::StepStatus::Failed => {
                    ext.last_error.insert(owner.dir, format!("{}: {}", step.tool, step.detail));
                }
                byob_protocol::StepStatus::Done => {
                    ext.last_error.remove(&owner.dir);
                }
                _ => {}
            }
        }
        let touched: Vec<String> = step
            .changes
            .iter()
            .flat_map(|c| match c {
                byob_protocol::FileChange::Created { path } | byob_protocol::FileChange::Modified { path } | byob_protocol::FileChange::Deleted { path } => {
                    vec![path.clone()]
                }
                byob_protocol::FileChange::Moved { from, to } => vec![from.clone(), to.clone()],
                byob_protocol::FileChange::Other => vec![],
            })
            .filter(|p| p.starts_with(".byob/plugins/") || p.starts_with(".byob/skills/"))
            .collect();
        if touched.is_empty() {
            return;
        }
        // A manifest the file tools created is byob's own: its code may run.
        let made: Vec<String> = step
            .changes
            .iter()
            .filter_map(|c| match c {
                byob_protocol::FileChange::Created { path } => created_plugin(path).map(str::to_owned),
                _ => None,
            })
            .collect();
        let reply = channel().0;
        for id in made {
            self.plugin_op(&chat, PluginOp::Made { id }, &reply);
        }
        self.plugin_op(&chat, PluginOp::Reload, &reply);
    }

    // ---- client messages --------------------------------------------------------------

    /// `ClientMsg::SetPluginEnabled`: switch a plugin at one level.
    pub(crate) fn set_plugin_enabled(&mut self, client: ClientId, scope: ConfigScope, plugin: &str, enabled: Option<bool>) {
        if enabled == Some(false) && self.ext.as_mut().is_some_and(|e| e.is_locked(plugin)) {
            self.notice(client, format!("{plugin} is part of byob's core and can't be switched off"));
            return;
        }
        let set = |l: &mut byob_types::config::Layer| {
            let mut map = l.plugins.take().unwrap_or_default();
            match enabled {
                Some(on) => map.insert(plugin.to_owned(), on),
                None => map.remove(plugin),
            };
            l.plugins = Some(map).filter(|m| !m.is_empty());
        };
        let result: Result<(), String> = match &scope {
            ConfigScope::Global => {
                set(&mut self.defaults);
                match &self.paths {
                    Some(paths) => Settings::update(&paths.settings(), |s| s.plugins = self.defaults.plugins.clone().unwrap_or_default())
                        .map(|_| ())
                        .map_err(|e| format!("couldn't save the settings: {e}")),
                    None => Ok(()),
                }
            }
            ConfigScope::Workspace { id } => match self.workspaces.get(id).cloned() {
                Some(row) => {
                    let mut layer = row.config;
                    set(&mut layer);
                    self.workspaces.set_config(id, layer).map_err(|e| format!("couldn't save the workspace: {e}"))
                }
                None => Err("that workspace no longer exists".into()),
            },
            ConfigScope::Chat => match self.client_chat(client) {
                Some(chat) => {
                    let mut layer = self.workspaces.chat_config(&chat).cloned().unwrap_or_default();
                    set(&mut layer);
                    self.workspaces.set_chat_config(&chat, layer).map_err(|e| format!("couldn't save the chat's settings: {e}"))
                }
                None => {
                    if let Some(c) = self.clients.get_mut(&client) {
                        set(&mut c.new_layer);
                    }
                    Ok(())
                }
            },
        };
        if let Err(e) = result {
            self.notice(client, e);
        }
        if enabled != Some(true)
            && let Some(ext) = self.ext.as_mut()
        {
            // where it is still on, it starts again on first use
            ext.backends.stop_plugin(plugin, "it was switched off");
        }
        self.extensions_changed();
    }

    /// `ClientMsg::SetPluginSetting`: store a value the plugin declares (secret ones in
    /// secrets.json). The value is never sent back.
    pub(crate) fn set_plugin_setting(&mut self, client: ClientId, plugin: &str, key: &str, value: Option<String>) {
        let chat = self.client_chat(client);
        let ws = self.ext_workspace(chat.as_deref(), Some(client));
        let Some(ext) = self.ext.as_mut() else { return };
        let declared = [PluginScope::Chat, PluginScope::Workspace, PluginScope::Global, PluginScope::Bundled]
            .into_iter()
            .flat_map(|s| ext.plugins_in(s, chat.as_deref(), &ws))
            .find(|p| p.id() == plugin)
            .and_then(|p| p.manifest.settings.iter().find(|s| s.key == key).cloned());
        let Some(setting) = declared else {
            self.notice(client, format!("plugin {plugin} has no setting {key}"));
            return;
        };
        let value = value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
        let values = self.plugin_values.entry(plugin.to_owned()).or_default();
        match &value {
            Some(v) => values.insert(key.to_owned(), v.clone()),
            None => values.remove(key),
        };
        if values.is_empty() {
            self.plugin_values.remove(plugin);
        }
        if let Some(paths) = &self.paths {
            let saved = if setting.secret {
                let mut secrets = Secrets::load(&paths.secrets()).value;
                let scope = format!("plugin:{plugin}");
                let map = secrets.plugin_settings.entry(scope.clone()).or_default();
                match &value {
                    Some(v) => map.insert(key.to_owned(), v.clone()),
                    None => map.remove(key),
                };
                if map.is_empty() {
                    secrets.plugin_settings.remove(&scope);
                }
                secrets.save(&paths.secrets())
            } else {
                Settings::update(&paths.settings(), |s| {
                    let map = s.plugin_settings.entry(plugin.to_owned()).or_default();
                    match &value {
                        Some(v) => map.insert(key.to_owned(), v.clone()),
                        None => map.remove(key),
                    };
                    if map.is_empty() {
                        s.plugin_settings.remove(plugin);
                    }
                })
                .map(|_| ())
            };
            if let Err(e) = saved {
                self.notice(client, format!("the setting applies now but couldn't be saved: {e}"));
            }
        }
        if let Some(ext) = self.ext.as_mut() {
            // settings are its environment: start it again with the new ones
            ext.backends.stop_plugin(plugin, "its settings changed");
        }
        self.extensions_changed();
    }

    /// `ClientMsg::KeepPlugin`: the user keeps a chat's plugin in its workspace or everywhere.
    /// For a bundled plugin this is "fork to edit": its files are copied there and the copy
    /// wins by id.
    pub(crate) fn keep_plugin_msg(&mut self, client: ClientId, plugin: &str, scope: PluginScope, new_id: Option<String>) {
        let chat = self.client_chat(client);
        let ws = self.ext_workspace(chat.as_deref(), Some(client));
        let Some(c) = self.compose_for(chat.as_deref(), Some(client), true) else { return };
        let Some(info) = c.infos.iter().find(|p| p.id == plugin && p.shadowed_by.is_none()) else {
            self.notice(client, format!("this chat has no plugin {plugin}"));
            return;
        };
        if info.scope == PluginScope::Bundled {
            self.fork_plugin_msg(client, chat.as_deref(), plugin, scope, &ws, new_id);
            return;
        }
        let Some(chat) = chat else {
            self.notice(client, "open the chat whose plugin you want to keep".into());
            return;
        };
        let target = match scope {
            PluginScope::Workspace => ws.join(".byob").join("plugins"),
            PluginScope::Global => match self.ext.as_ref().and_then(|e| e.global_dir()) {
                Some(d) => d.to_path_buf(),
                None => {
                    self.notice(client, "there's no global plugins folder in this run".into());
                    return;
                }
            },
            PluginScope::Chat | PluginScope::Bundled => {
                self.notice(client, "a plugin is kept in a workspace or everywhere".into());
                return;
            }
        };
        let new_id = new_id.unwrap_or_else(|| plugin.to_owned());
        let moved = crate::authoring::keep_files(Path::new(&info.dir), &target, &new_id, scope == PluginScope::Global, info.scope == PluginScope::Chat);
        match moved {
            Ok(_) => {
                let reply = channel().0;
                let op = if scope == PluginScope::Workspace { PluginOp::Made { id: new_id.clone() } } else { PluginOp::Reload };
                self.plugin_op(&chat, op, &reply);
                let text = format!("Kept plugin {new_id} {}.", scope.label());
                self.send_to(client, ServerMsg::Notice { level: NoticeLevel::Info, text });
            }
            Err(e) => self.notice(client, e),
        }
    }

    /// Copy bundled plugin `plugin` to `scope` (global by default) so it can be edited.
    fn fork_plugin_msg(&mut self, client: ClientId, chat: Option<&str>, plugin: &str, scope: PluginScope, ws: &Path, new_id: Option<String>) {
        let Some(ext) = self.ext.as_mut() else { return };
        let Some(files) = ext.bundled_files(plugin) else {
            self.notice(client, format!("{plugin} isn't built into byob"));
            return;
        };
        let target = match scope {
            PluginScope::Workspace => ws.join(".byob").join("plugins"),
            _ => match ext.global_dir() {
                Some(d) => d.to_path_buf(),
                None => {
                    self.notice(client, "there's no global plugins folder in this run".into());
                    return;
                }
            },
        };
        let id = new_id.unwrap_or_else(|| plugin.to_owned());
        match crate::authoring::fork_files(&files, &target, &id) {
            Ok(_) => {
                let reply = channel().0;
                if scope == PluginScope::Workspace
                    && let Some(chat) = chat
                {
                    self.plugin_op(chat, PluginOp::Made { id: id.clone() }, &reply);
                }
                self.compose_for(chat, Some(client), true);
                self.extensions_changed();
                let place = if scope == PluginScope::Workspace { PluginScope::Workspace } else { PluginScope::Global };
                let text = format!(
                    "Forked {plugin} {} ({}); edit its files there — the copy is used instead of the built-in one.",
                    place.label(),
                    target.join(&id).display()
                );
                self.send_to(client, ServerMsg::Notice { level: NoticeLevel::Info, text });
            }
            Err(e) => self.notice(client, e),
        }
    }

    /// `ClientMsg::RemovePlugin`: delete one copy of a plugin the chat in view can see.
    pub(crate) fn remove_plugin_msg(&mut self, client: ClientId, plugin: &str, scope: PluginScope) {
        let chat = self.client_chat(client);
        let ws = self.ext_workspace(chat.as_deref(), Some(client));
        let Some(c) = self.compose_for(chat.as_deref(), Some(client), true) else { return };
        let Some(info) = c.infos.iter().find(|p| p.id == plugin && p.scope == scope) else {
            self.notice(client, format!("there's no plugin {plugin} in {}", scope.label()));
            return;
        };
        if info.origin == byob_protocol::PluginOrigin::Bundled {
            self.notice(client, format!("{plugin} ships with byob; switch it off instead"));
            return;
        }
        if let Some(ext) = self.ext.as_mut() {
            ext.backends.stop_dir(Path::new(&info.dir), "its plugin was removed");
        }
        if let Err(e) = std::fs::remove_dir_all(&info.dir) {
            self.notice(client, format!("couldn't remove {}: {e}", info.dir));
            return;
        }
        if scope == PluginScope::Workspace
            && let Some(id) = self.workspaces.by_folder(&ws).map(|w| w.id.clone())
            && let Err(e) = self.workspaces.set_plugin_trust(&id, plugin, None)
        {
            log::warn!("forgetting plugin {plugin}: {e}");
        }
        self.compose_for(chat.as_deref(), Some(client), true);
        self.extensions_changed();
    }

    /// `ClientMsg::ReloadPlugins`.
    pub(crate) fn reload_plugins(&mut self, client: ClientId) {
        if let Some(ext) = self.ext.as_mut() {
            ext.invalidate();
        }
        let chat = self.client_chat(client);
        self.compose_for(chat.as_deref(), Some(client), true);
        self.extensions_changed();
    }

    /// `ClientMsg::TrustPlugins`: the user decides on a workspace's own plugins.
    pub(crate) fn trust_plugins(&mut self, client: ClientId, workspace: &str, plugins: Vec<String>, trust: bool) {
        let Some(row) = self.workspaces.get(workspace).cloned() else {
            self.notice(client, "that workspace no longer exists".into());
            return;
        };
        let plugins = if plugins.is_empty() {
            let Some(ext) = self.ext.as_mut() else { return };
            ext.refresh(None, &row.folder);
            ext.plugins_in(PluginScope::Workspace, None, &row.folder)
                .into_iter()
                .map(|p| p.id().to_owned())
                .filter(|id| !row.plugin_trust.get(id).is_some_and(|t| *t == PluginTrust::Made))
                .collect()
        } else {
            plugins
        };
        for p in &plugins {
            let t = if trust { PluginTrust::Trusted } else { PluginTrust::Declined };
            if let Err(e) = self.workspaces.set_plugin_trust(&row.id, p, Some(t)) {
                self.notice(client, format!("couldn't save the decision: {e}"));
                return;
            }
            if !trust && let Some(ext) = self.ext.as_mut() {
                ext.backends.stop_plugin(p, "the user declined it");
            }
        }
        self.extensions_changed();
    }

    /// The workspace's plugins that came with its folder and wait for a decision, as the
    /// question a turn asks before it starts. `None` if there are none (or no one may say
    /// yes: commands are off).
    pub(crate) fn trust_question(&self, composed: &Composed, policy: &crate::Policy) -> Option<crate::turn::TrustAsk> {
        if composed.undecided.is_empty() || policy.execute == crate::Rule::Deny {
            return None;
        }
        let lines: Vec<String> = composed
            .infos
            .iter()
            .filter(|p| composed.undecided.contains(&p.id) && p.scope == PluginScope::Workspace)
            .map(|p| {
                let tools: Vec<&str> = p.servers.iter().map(|s| s.name.as_str()).collect();
                let what = if tools.is_empty() { String::new() } else { format!(" (servers: {})", tools.join(", ")) };
                format!("- {} {}: {}{what}", p.name, p.version, p.description)
            })
            .collect();
        Some(crate::turn::TrustAsk {
            plugins: composed.undecided.clone(),
            text: format!(
                "These plugins came with the workspace folder (byob didn't write them). Turning them on lets their tools and servers run (tool calls still follow your approvals):\n{}",
                lines.join("\n")
            ),
        })
    }

    /// Plugin settings saved in settings.json and secrets.json, by plugin id.
    pub(crate) fn load_plugin_values(paths: Option<&byob_store::Paths>) -> BTreeMap<String, BTreeMap<String, String>> {
        let Some(paths) = paths else { return BTreeMap::new() };
        let mut values = Settings::load(&paths.settings()).value.plugin_settings;
        for (scope, map) in Secrets::load(&paths.secrets()).value.plugin_settings {
            if let Some(id) = scope.strip_prefix("plugin:") {
                values.entry(id.to_owned()).or_default().extend(map);
            }
        }
        values
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifests_the_file_tools_create_are_recognised() {
        assert_eq!(created_plugin(".byob/plugins/weather/byob-plugin.json"), Some("weather"));
        assert_eq!(created_plugin(".byob/plugins/weather/tools/x.py"), None);
        assert_eq!(created_plugin(".byob/plugins/Bad Id/byob-plugin.json"), None);
        assert_eq!(created_plugin("x/.byob/plugins/w/byob-plugin.json"), None);
    }
}
