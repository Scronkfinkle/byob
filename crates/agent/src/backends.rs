//! Plugin backends in the engine: one long-lived process per plugin copy (its folder), made
//! when a composition first sees the plugin switched on and trusted, started on first use
//! (`byob_plugins::backend`). The engine routes the plugin channel to it, sends it the
//! engine events it subscribed to, forwards what it says to clients, and stops it when its
//! plugin changes, is switched off, removed, distrusted or reloaded (the next composition
//! makes a fresh one; tools pick it up from the next round).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use byob_plugins::backend::{Backend, CAPS, Launch, Out, Phase, Sink, WIRE_VERSION};
use byob_plugins::manifest::{BACKEND_EVENTS, BackendDecl, BackendRun};
use byob_plugins::{Plugin, PluginPython, PluginRef};
use byob_protocol::{BackendInfo, NoticeLevel, PluginScope, ServerMsg};
use serde_json::{Value, json};

use crate::{ClientId, Engine};

/// Bytes of stderr shown to clients and the model.
const STDERR_SHOWN: usize = 4096;

/// Makes the sink of the backend of the plugin in a folder.
pub(crate) type SinkFor = Arc<dyn Fn(&Path, &str) -> Sink + Send + Sync>;

/// One plugin copy's backend and where it applies.
pub(crate) struct Entry {
    pub backend: Arc<Backend>,
    pub plugin: String,
    /// Chat plugins: their chat.
    pub chat: Option<String>,
    /// Workspace plugins: their workspace.
    pub workspace: Option<PathBuf>,
    /// The plugin's fingerprint it was made for.
    fp: u64,
    /// The views it declares: (id, site).
    pub views: Vec<(String, byob_protocol::Site)>,
}

/// Backends by plugin folder.
#[derive(Default)]
pub(crate) struct Backends {
    map: BTreeMap<PathBuf, Entry>,
}

/// What [`Backends::ensure`] needs to make one.
pub(crate) struct Make<'a> {
    pub plugin: &'a Plugin,
    pub decl: &'a BackendDecl,
    pub scope: PluginScope,
    pub chat: Option<&'a str>,
    pub workspace: &'a Path,
    pub fp: u64,
    pub plugin_ref: &'a PluginRef,
    pub python: &'a PluginPython,
    pub sink: &'a SinkFor,
}

impl Backends {
    /// The backend of `m.plugin`, made if there is none (or its plugin changed).
    pub(crate) fn ensure(&mut self, m: &Make<'_>) -> Arc<Backend> {
        let dir = &m.plugin.dir;
        if let Some(e) = self.map.get(dir)
            && e.fp == m.fp
            && e.backend.status().phase != Phase::Stopped
        {
            return Arc::clone(&e.backend);
        }
        if let Some(old) = self.map.remove(dir) {
            old.backend.stop("its plugin changed");
        }
        let backend = Arc::new(make(m));
        self.map.insert(
            dir.clone(),
            Entry {
                backend: Arc::clone(&backend),
                plugin: m.plugin.id().to_owned(),
                chat: (m.scope == PluginScope::Chat).then(|| m.chat.map(str::to_owned)).flatten(),
                workspace: (m.scope == PluginScope::Workspace).then(|| m.workspace.to_path_buf()),
                fp: m.fp,
                views: m.plugin.manifest.views.iter().filter_map(|v| Some((v.id.clone(), byob_protocol::Site::parse(&v.site)?))).collect(),
            },
        );
        backend
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub(crate) fn get(&self, dir: &Path) -> Option<&Entry> {
        self.map.get(dir)
    }

    /// Stop the backend of the plugin in `dir`.
    pub(crate) fn stop_dir(&mut self, dir: &Path, why: &str) {
        if let Some(e) = self.map.remove(dir) {
            e.backend.stop(why);
        }
    }

    /// Stop every copy's backend of plugin `id` (they start again where still used).
    pub(crate) fn stop_plugin(&mut self, id: &str, why: &str) {
        self.map.retain(|_, e| {
            let keep = e.plugin != id;
            if !keep {
                e.backend.stop(why);
            }
            keep
        });
    }

    /// Stop the backends of chat `chat`'s scratch plugins.
    pub(crate) fn stop_chat(&mut self, chat: &str) {
        self.map.retain(|_, e| {
            let keep = e.chat.as_deref() != Some(chat);
            if !keep {
                e.backend.stop("its chat was deleted");
            }
            keep
        });
    }

    pub(crate) fn stop_all(&mut self, why: &str) {
        for (_, e) in std::mem::take(&mut self.map) {
            e.backend.stop(why);
        }
    }

    /// Backends that want `event` of a chat working in `ws`.
    pub(crate) fn subscribers(&self, chat: &str, ws: &Path, event: &str) -> Vec<Arc<Backend>> {
        self.map
            .values()
            .filter(|e| e.chat.as_deref().is_none_or(|c| c == chat) && e.workspace.as_deref().is_none_or(|w| w == ws))
            .filter(|e| e.backend.wants(event))
            .map(|e| Arc::clone(&e.backend))
            .collect()
    }
}

/// A new (idle) backend for `m.plugin`.
fn make(m: &Make<'_>) -> Backend {
    let plugin = m.plugin;
    let dir = plugin.dir.clone();
    let secret: Vec<&str> = plugin.manifest.settings.iter().filter(|s| s.secret).map(|s| s.key.as_str()).collect();
    let settings: serde_json::Map<String, Value> =
        m.plugin_ref.env.iter().filter(|(k, _)| !secret.contains(&k.as_str())).map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect();
    let init = json!({
        "type": "init",
        "v": WIRE_VERSION,
        "plugin": plugin.id(),
        "name": plugin.manifest.name,
        "version": plugin.manifest.version,
        "byob": env!("CARGO_PKG_VERSION"),
        "caps": CAPS,
        "events": BACKEND_EVENTS,
        "settings": settings,
        "dir": dir.display().to_string(),
        "scope": m.scope.label(),
        "chat": if m.scope == PluginScope::Chat { m.chat } else { None },
        "workspace": (m.scope == PluginScope::Workspace).then(|| m.workspace.display().to_string()),
        "views": plugin.manifest.views.iter().map(|v| json!({"id": v.id, "site": v.site, "title": v.title})).collect::<Vec<_>>(),
    });
    let mut env = m.plugin_ref.env.clone();
    let d = dir.display().to_string();
    env.insert("BYOB_PLUGIN_DIR".into(), d.clone());
    env.insert("HOB_PLUGIN_DIR".into(), d);
    let decl = m.decl.clone();
    let python = m.python.clone();
    // absolute: it is also the working folder, so a relative one (--plugin DIR) would resolve twice
    let prepare_dir = std::path::absolute(&dir).unwrap_or_else(|_| dir.clone());
    let prepare = Box::new(move || {
        let mut env = env.clone();
        let mut argv = match &decl.run {
            BackendRun::Python(script) => {
                let file = prepare_dir.join(script);
                if !file.is_file() {
                    return Err(format!("backend {script} is missing"));
                }
                let py = python.python()?;
                env.extend(py.env);
                let mut argv = py.program;
                argv.push(file.into_os_string());
                argv
            }
            BackendRun::Command(cmd) => {
                // byob's helper is on PYTHONPATH for Python run any other way too
                if let Ok(py) = python.python() {
                    env.extend(py.env);
                }
                let local = prepare_dir.join(cmd);
                let program =
                    if (cmd.contains('/') || cmd.contains('\\')) && byob_plugins::manifest::path_inside(cmd) { local.into_os_string() } else { cmd.into() };
                vec![program]
            }
        };
        argv.extend(decl.args.iter().map(Into::into));
        Ok(Launch { argv, cwd: prepare_dir.clone(), env, remove_env: vec!["BYOB_API_KEY".into()] })
    });
    let sink = (m.sink)(&dir, plugin.id());
    Backend::new(plugin.id(), init, &m.decl.events, prepare, sink)
}

/// How a client sees a backend.
pub(crate) fn info(backend: &Backend) -> BackendInfo {
    let s = backend.status();
    let mut stderr = s.stderr;
    if stderr.len() > STDERR_SHOWN {
        let mut cut = stderr.len() - STDERR_SHOWN;
        while !stderr.is_char_boundary(cut) {
            cut += 1;
        }
        stderr = stderr.split_off(cut);
    }
    BackendInfo { state: s.phase.label().to_owned(), starts: s.starts, error: s.error, faults: s.faults, stderr }
}

impl Engine {
    /// `Input::Backend`: the backend of the plugin in `dir` said something (or started/exited).
    pub(crate) fn backend_out(&mut self, dir: &Path, plugin: &str, out: Out) {
        let chat = self.ext.as_ref().and_then(|e| e.backends.get(dir)).map(|e| e.chat.clone());
        let Some(chat) = chat else { return };
        match out {
            Out::Started | Out::Exited(_) => self.extensions_changed(),
            Out::Msg(msg) => {
                let to = msg.get("client").and_then(Value::as_u64);
                match msg.get("type").and_then(Value::as_str).unwrap_or_default() {
                    "view" => self.backend_view(dir, &msg),
                    byob_plugins::backend::CARD_MSG => self.backend_card(dir, plugin, &msg),
                    "notify" => {
                        let inner = msg.get("msg").cloned().unwrap_or(Value::Null);
                        self.backend_send(to, chat.as_deref(), plugin, inner);
                    }
                    "notice" => {
                        let text: String = msg.get("text").and_then(Value::as_str).unwrap_or_default().chars().take(500).collect();
                        let level = if msg.get("level").and_then(Value::as_str) == Some("error") { NoticeLevel::Error } else { NoticeLevel::Info };
                        let notice = ServerMsg::Notice { level, text: format!("{plugin}: {text}") };
                        match to {
                            Some(id) => self.send_to(id, notice),
                            None => self.broadcast(notice),
                        }
                    }
                    "fault" => self.extensions_changed(),
                    "artifact" => self.backend_artifact(dir, chat.as_deref(), plugin, &msg),
                    other => log::debug!("plugin {plugin}: backend message {other:?} ignored"),
                }
            }
        }
    }

    /// A backend's `artifact` message: `{chat, id?, title?, filename?, file, note?, draft?}`
    /// saves `file`'s bytes as a version of the chat's artifact `id` (a new artifact without
    /// one). `draft` (the user's edits in its view): consecutive drafts replace one version
    /// until the model writes the artifact or a turn starts. Problems are faults.
    fn backend_artifact(&mut self, dir: &Path, own_chat: Option<&str>, plugin: &str, msg: &Value) {
        let fault = |me: &Self, text: String| {
            if let Some(e) = me.ext.as_ref().and_then(|e| e.backends.get(dir)) {
                e.backend.report_fault(&format!("artifact: {text}"));
            }
            log::warn!("plugin {plugin}: artifact: {text}");
        };
        let s = |k: &str| msg.get(k).and_then(Value::as_str).filter(|v| !v.is_empty());
        let Some(chat) = s("chat").map(str::to_owned) else { return fault(self, "a message without \"chat\"".into()) };
        if own_chat.is_some_and(|c| c != chat) {
            return fault(self, format!("a chat plugin can only save artifacts of its own chat, not {chat:?}"));
        }
        let Some(file) = s("file") else { return fault(self, "a message without \"file\" (the path of the bytes to save)".into()) };
        let Some(workspace) = self.chat_workspace(&chat) else { return fault(self, format!("no chat {chat:?}")) };
        let bytes = match std::fs::metadata(file) {
            Ok(m) if m.len() <= byob_tools::artifact::MAX_FILE_BYTES => std::fs::read(file).map_err(|e| e.to_string()),
            Ok(_) => Err("too big".into()),
            Err(e) => Err(e.to_string()),
        };
        let bytes = match bytes {
            Ok(b) => b,
            Err(e) => return fault(self, format!("{file}: {e}")),
        };
        let types = self.compose_for(Some(&chat), None, false).map(|c| c.artifact_types).unwrap_or_else(|| self.artifact_types.clone());
        let draft = msg.get("draft").and_then(Value::as_bool).unwrap_or(false);
        let id = s("id").map(str::to_owned);
        let replace = match (&id, draft) {
            (Some(id), true) => self.drafts.get(&(chat.clone(), id.clone())).copied(),
            _ => None,
        };
        let save = byob_tools::artifact::SaveBytes {
            chat: &chat,
            id: id.as_deref(),
            title: s("title"),
            filename: s("filename"),
            bytes: &bytes,
            note: s("note").unwrap_or_default(),
            replace,
        };
        match byob_tools::artifact::save_bytes(&types, &workspace, &save) {
            Ok(saved) => {
                let r = byob_protocol::ArtifactRef { id: saved.artifact.id.clone(), version: saved.version };
                let live = self.live.contains_key(&chat);
                self.artifacts_changed(&chat, std::slice::from_ref(&r));
                if !live {
                    self.send_viewers(&chat, ServerMsg::ArtifactUpdated(saved.artifact.clone()));
                }
                if draft {
                    self.drafts.insert((chat, r.id), r.version);
                }
            }
            Err(e) => fault(self, e),
        }
    }

    /// A backend message for clients: one client, or those looking at its chat (chat
    /// plugins), or everyone.
    fn backend_send(&mut self, to: Option<ClientId>, chat: Option<&str>, plugin: &str, msg: Value) {
        match (to, chat) {
            (Some(id), _) => self.send_plugin(Some(id), plugin, msg),
            (None, Some(chat)) => {
                let ids: Vec<ClientId> = self.clients.iter().filter(|(_, c)| c.chat.as_deref() == Some(chat)).map(|(id, _)| *id).collect();
                for id in ids {
                    self.send_plugin(Some(id), plugin, msg.clone());
                }
            }
            (None, None) => self.send_plugin(None, plugin, msg),
        }
    }

    /// Engine event `event` in `chat`: to the backends that subscribed.
    pub(crate) fn emit(&mut self, chat: &str, event: &str, data: Value) {
        if self.ext.as_ref().is_none_or(|e| e.backends.is_empty()) {
            return;
        }
        let ws = match self.live.get(chat) {
            Some(l) => l.workspace.as_ref().clone(),
            None => self.ext_workspace(Some(chat), None),
        };
        let subs = self.ext.as_ref().map(|e| e.backends.subscribers(chat, &ws, event)).unwrap_or_default();
        if subs.is_empty() {
            return;
        }
        let mut msg = json!({"type": "event", "event": event, "chat": chat});
        if let (Some(o), Value::Object(d)) = (msg.as_object_mut(), data) {
            o.extend(d);
        }
        for b in subs {
            b.event(msg.clone());
        }
    }

    /// A client's plugin-channel message for a plugin without a native handler: to its
    /// backend, as a `client` event. `false` if it has none.
    pub(crate) fn backend_message(&mut self, client: ClientId, plugin: &str, msg: Value) -> Result<(), String> {
        let chat = self.client_chat(client);
        let Some(c) = self.compose_for(chat.as_deref(), Some(client), false) else { return Err(format!("no plugin {plugin:?} takes messages in this engine")) };
        let Some(info) = c.infos.iter().find(|p| p.id == plugin && p.shadowed_by.is_none()) else {
            return Err(format!("no plugin {plugin:?} takes messages in this engine"));
        };
        let dir = PathBuf::from(&info.dir);
        let backend = self.ext.as_ref().and_then(|e| e.backends.get(&dir)).map(|e| Arc::clone(&e.backend));
        let Some(backend) = backend else {
            let why = match &info.backend {
                Some(b) => b.error.clone().unwrap_or_else(|| format!("its backend is {}", b.state)),
                None if !info.enabled => "it is switched off".into(),
                None if !info.trusted => "its code waits for your OK".into(),
                None => "it has no backend".into(),
            };
            return Err(format!("plugin {plugin} can't take messages: {why}"));
        };
        if backend.event(json!({"type": "event", "event": "client", "client": client, "chat": chat, "msg": msg})) {
            Ok(())
        } else {
            Err(format!("plugin {plugin}'s backend isn't running: {}", backend.status().error.unwrap_or_else(|| "stopped".into())))
        }
    }
}
