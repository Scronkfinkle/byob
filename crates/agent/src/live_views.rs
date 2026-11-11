//! Live views at render sites (`byob_protocol::sites`): the engine owns each view's tree,
//! clients show it and send what the user did.
//!
//! A view is keyed by (plugin folder, declared view id, instance key), so a chat's scratch
//! copy of a plugin and the global copy have their own. When a client opens one
//! (`OpenView`), it gets the cached tree, or the engine asks the plugin's backend (`request
//! view`, on a thread of its own so the engine never waits) and sends the tree when it comes.
//! A backend changes a view by sending a whole `view` (or patches); the engine applies it to
//! the cached tree, diffs old against new (`tree::diff`) and sends only the patches to every
//! client that has it open. `values` (forced field values, e.g. clearing an input) go only to
//! the client whose event caused the change. Problems in a tree are fixed on the way in
//! (`tree::decode`) and reported to the backend's status (`Backend::report_fault`), so
//! `plugin_status` shows them to the model; the client gets them too, for a small warning at
//! the site. The cache is dropped (clients get `reset` and open it again) when the backend
//! the tree came from is replaced or restarted: a reload, a switch, a crash.

use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use byob_plugins::backend::{Backend, Phase};
use byob_protocol::sites::{CAP_VIEW_SITES, LiveView};
use byob_protocol::tree::{self, Fault, Node, Reply};
use byob_protocol::{ServerMsg, Site, View, ViewRef};
use serde_json::{Map, Value, json};

use crate::{ClientId, Engine, Input};

/// How long a view request waits for the backend.
const VIEW_TIMEOUT: Duration = Duration::from_secs(20);
/// Characters of one fault report kept for the plugin's status.
const FAULT_CHARS: usize = 1500;
/// Faults sent to clients with a view.
const CLIENT_FAULTS: usize = 8;

type Key = (PathBuf, String, String);

/// One live view.
struct Live {
    plugin: String,
    tree: Option<Node>,
    /// Why it couldn't be had (the last view request failed).
    error: Option<String>,
    faults: Vec<String>,
    /// Clients showing it.
    subs: BTreeSet<ClientId>,
    /// A view request is on its way.
    asked: bool,
    /// The backend process it came from, and how many times it had started then: a tree
    /// from an earlier process (or another backend object) is stale.
    backend: Weak<Backend>,
    starts: u32,
}

/// Every live view the engine knows.
#[derive(Default)]
pub(crate) struct LiveViews {
    map: BTreeMap<Key, Live>,
}

impl LiveViews {
    /// Client `id` is gone or looks at another chat now: it shows none of them.
    pub(crate) fn forget_client(&mut self, id: ClientId) {
        for l in self.map.values_mut() {
            l.subs.remove(&id);
        }
    }

    /// A view client `id` has open shows the workspace file `path` (an `image` node), so it
    /// may fetch it like a conversation's media.
    pub(crate) fn shows_media(&self, id: ClientId, path: &str) -> bool {
        fn has(n: &Node, path: &str) -> bool {
            match &n.kind {
                tree::Kind::Image { media: Some(m), .. } => m == path,
                _ => n.children().iter().any(|c| has(c, path)),
            }
        }
        self.map.values().any(|l| l.subs.contains(&id) && l.tree.as_ref().is_some_and(|t| has(t, path)))
    }

    /// The folder of the view `r` client `id` has open.
    fn dir_for(&self, id: ClientId, r: &ViewRef) -> Option<PathBuf> {
        self.map.iter().find(|((_, v, k), l)| l.plugin == r.plugin && *v == r.view && *k == r.key && l.subs.contains(&id)).map(|((d, ..), _)| d.clone())
    }
}

/// What a client's `ViewRef` resolves to.
struct Target {
    dir: PathBuf,
    site: Site,
    backend: Arc<Backend>,
    chat: Option<String>,
}

impl Engine {
    fn takes_views(&self, id: ClientId) -> bool {
        self.clients.get(&id).is_some_and(|c| c.attached && c.caps.iter().any(|c| c == CAP_VIEW_SITES))
    }

    fn send_live(&mut self, id: ClientId, msg: LiveView) {
        if self.takes_views(id) {
            self.send_to(id, ServerMsg::LiveView(msg));
        }
    }

    /// The plugin copy client `id` sees for `r`, its declared view and its backend.
    fn view_target(&mut self, id: ClientId, r: &ViewRef) -> Result<Target, String> {
        let chat = self.client_chat(id);
        let none = || format!("no plugin {:?} in this chat", r.plugin);
        let c = self.compose_for(chat.as_deref(), Some(id), false).ok_or_else(none)?;
        let info = c.infos.iter().find(|p| p.id == r.plugin && p.shadowed_by.is_none()).ok_or_else(none)?;
        let Some(decl) = info.views.iter().find(|v| v.id == r.view) else {
            let ids: Vec<&str> = info.views.iter().map(|v| v.id.as_str()).collect();
            return Err(format!("plugin {} has no view {:?} (its views: {})", r.plugin, r.view, if ids.is_empty() { "none".into() } else { ids.join(", ") }));
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
            return Err(format!("{} can't show {}: {why}", info.name, decl.title));
        };
        Ok(Target { dir, site: decl.site, backend, chat })
    }

    /// `OpenView`: client `id` shows `r` now.
    pub(crate) fn open_view(&mut self, id: ClientId, r: ViewRef) {
        if !self.takes_views(id) {
            return;
        }
        let t = match self.view_target(id, &r) {
            Ok(t) => t,
            Err(e) => {
                self.send_live(id, LiveView { view: r, error: Some(e), ..Default::default() });
                return;
            }
        };
        let key = (t.dir.clone(), r.view.clone(), r.key.clone());
        let fresh = || Live {
            plugin: r.plugin.clone(),
            tree: None,
            error: None,
            faults: Vec::new(),
            subs: BTreeSet::new(),
            asked: false,
            backend: Arc::downgrade(&t.backend),
            starts: 0,
        };
        let live = self.live_views.map.entry(key.clone()).or_insert_with(fresh);
        live.subs.insert(id);
        if let Some(tree) = live.tree.clone() {
            let faults = live.faults.clone();
            self.send_live(id, LiveView { view: r, tree: Some(tree), faults, ..Default::default() });
            return;
        }
        if live.asked {
            return;
        }
        live.asked = true;
        let params = json!({"site": t.site.label(), "view": r.view, "key": r.key, "client": id, "chat": t.chat});
        let inbox = self.inbox.clone();
        let backend = t.backend;
        let spawned = std::thread::Builder::new().name("byob-view-request".into()).spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| backend.request("view", params, VIEW_TIMEOUT, &|| false)))
                .unwrap_or_else(|_| Err("the view request crashed".into()));
            let starts = backend.status().starts;
            let _ = inbox.send(Input::ViewReply { dir: key.0, view: key.1, key: key.2, starts, result });
        });
        if let Err(e) = spawned {
            log::error!("could not start a view request: {e}");
        }
    }

    /// `CloseView`.
    pub(crate) fn close_view(&mut self, id: ClientId, r: &ViewRef) {
        for ((_, v, k), l) in &mut self.live_views.map {
            if l.plugin == r.plugin && *v == r.view && *k == r.key {
                l.subs.remove(&id);
            }
        }
    }

    /// `ViewEvent`: to the view's backend, as an `event` of kind `view`.
    pub(crate) fn view_event(&mut self, id: ClientId, r: ViewRef, event: tree::Event) {
        let open = self.live_views.dir_for(id, &r).and_then(|dir| {
            let e = self.ext.as_ref()?.backends.get(&dir)?;
            let site = e.views.iter().find(|(v, _)| *v == r.view).map_or(Site::Card, |(_, s)| *s);
            Some(Target { site, backend: Arc::clone(&e.backend), chat: self.client_chat(id), dir })
        });
        let t = match open.map_or_else(|| self.view_target(id, &r), Ok) {
            Ok(t) => t,
            Err(e) => {
                self.send_live(id, LiveView { view: r, error: Some(e), ..Default::default() });
                return;
            }
        };
        let msg = json!({
            "type": "event", "event": "view", "view": r.view, "site": t.site.label(), "key": r.key, "client": id, "chat": t.chat,
            "action": event.action, "values": event.values, "edit": event.edit,
        });
        if !t.backend.event(msg) {
            let why = t.backend.status().error.unwrap_or_else(|| "stopped".into());
            self.send_live(id, LiveView { view: r, error: Some(format!("{}'s backend isn't running: {why}", t.backend.plugin())), ..Default::default() });
        }
    }

    /// `Input::ViewReply`: the backend answered a view request.
    pub(crate) fn view_reply(&mut self, dir: &Path, view: &str, key: &str, starts: u32, result: Result<Value, String>) {
        let k = (dir.to_path_buf(), view.to_owned(), key.to_owned());
        let Some(live) = self.live_views.map.get_mut(&k) else { return };
        live.asked = false;
        match result {
            Ok(v) => {
                let (reply, faults) = parse_reply(v);
                self.set_view(dir, view, key, reply, faults, None, starts);
            }
            Err(e) => {
                let plugin = live.plugin.clone();
                live.error = Some(e.clone());
                let subs: Vec<ClientId> = live.subs.iter().copied().collect();
                let backend = live.backend.upgrade();
                let r = ViewRef::new(&plugin, view, key);
                for id in subs {
                    self.send_live(id, LiveView { view: r.clone(), error: Some(e.clone()), ..Default::default() });
                }
                if let Some(b) = backend {
                    b.report_fault(&cut(&format!("view {view}: the view request failed: {e}")));
                }
            }
        }
    }

    /// A backend's `view` message: `{"id"?, "site"?, "key"?, "view"?: tree, "patch"?: [..],
    /// "values"?: {..}, "client"?}`.
    pub(crate) fn backend_view(&mut self, dir: &Path, msg: &Value) {
        let Some(entry) = self.ext.as_ref().and_then(|e| e.backends.get(dir)) else { return };
        let starts = entry.backend.status().starts;
        let declared = entry.views.clone();
        let backend = Arc::clone(&entry.backend);
        let str_of = |k: &str| msg.get(k).and_then(Value::as_str).map(str::to_owned);
        let view = str_of("id").or_else(|| {
            let site = str_of("site");
            declared.iter().find(|(_, s)| site.as_deref() == Some(s.label())).or_else(|| (declared.len() == 1).then(|| &declared[0])).map(|(v, _)| v.clone())
        });
        let Some(view) = view.filter(|v| declared.iter().any(|(d, _)| d == v)) else {
            let ids: Vec<&str> = declared.iter().map(|(v, _)| v.as_str()).collect();
            backend.report_fault(&format!(
                "a view message for {:?}, which isn't one of the views in byob-plugin.json ({}); send {{\"type\": \"view\", \"id\": <view id>, \"view\": <tree>}}",
                str_of("id").or_else(|| str_of("site")).unwrap_or_default(),
                if ids.is_empty() { "none declared".to_owned() } else { ids.join(", ") }
            ));
            return;
        };
        let key = str_of("key").unwrap_or_default();
        let mut body = Map::new();
        for f in ["view", "patch", "values"] {
            if let Some(v) = msg.get(f) {
                body.insert(f.into(), v.clone());
            }
        }
        let (reply, faults) = parse_reply(Value::Object(body));
        let to = msg.get("client").and_then(Value::as_u64);
        self.set_view(dir, &view, &key, reply, faults, to, starts);
    }

    /// A backend tool's card (`byob_plugins::backend::CARD_MSG`): cached as a live view and
    /// attached to the tool's step.
    pub(crate) fn backend_card(&mut self, dir: &Path, plugin: &str, msg: &Value) {
        let Some(entry) = self.ext.as_ref().and_then(|e| e.backends.get(dir)) else { return };
        let starts = entry.backend.status().starts;
        let backend = Arc::clone(&entry.backend);
        let cards: Vec<String> = entry.views.iter().filter(|(_, s)| *s == Site::Card).map(|(v, _)| v.clone()).collect();
        let (Some(chat), Some(tool)) = (msg.get("chat").and_then(Value::as_str), msg.get("tool").and_then(Value::as_str)) else { return };
        let view = match msg.get("view").and_then(Value::as_str) {
            Some(v) if cards.iter().any(|c| c == v) => v.to_owned(),
            Some(v) => {
                backend.report_fault(&format!("card_view {v:?} isn't a card view in byob-plugin.json (card views: {})", cards.join(", ")));
                return;
            }
            None => match cards.first() {
                Some(v) => v.clone(),
                None => {
                    backend.report_fault("a tool returned a \"card\", but byob-plugin.json declares no view with \"site\": \"card\"");
                    return;
                }
            },
        };
        let key = match msg.get("key").and_then(Value::as_str) {
            Some(k) if !k.is_empty() => k.chars().take(byob_protocol::sites::MAX_VIEW_KEY_CHARS).collect(),
            _ => format!("{tool}-{}", SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos())),
        };
        let web = self.ext.as_ref().and_then(|e| e.backends.get(dir)).is_some_and(|e| e.web_views.contains(&view));
        if web {
            // A web card: the page gets the card's data (`byob/card`); no tree.
            let data = msg.get("tree").cloned().unwrap_or(Value::Null);
            let text = msg.get("text").and_then(Value::as_str).unwrap_or_default().to_owned();
            let card = byob_protocol::web::WebCard { plugin: plugin.to_owned(), view, key, data, text };
            self.step_views.insert(chat.to_owned(), (tool.to_owned(), View::of(&card)));
            return;
        }
        let (tree, faults) = tree::decode(msg.get("tree").cloned().unwrap_or(Value::Null));
        let mut view_data = View::of(&tree);
        view_data.owner = Some(ViewRef::new(plugin, &view, &key));
        self.set_view(dir, &view, &key, Reply { view: Some(tree), ..Default::default() }, faults, None, starts);
        self.step_views.insert(chat.to_owned(), (tool.to_owned(), view_data));
    }

    /// Apply `reply` to the cached view (made if needed) and send the change to the clients
    /// showing it; `values` only to `to` if given.
    #[allow(clippy::too_many_arguments)]
    fn set_view(&mut self, dir: &Path, view: &str, key: &str, mut reply: Reply, mut faults: Vec<Fault>, to: Option<ClientId>, starts: u32) {
        let Some(entry) = self.ext.as_ref().and_then(|e| e.backends.get(dir)) else { return };
        let (plugin, backend) = (entry.plugin.clone(), Arc::clone(&entry.backend));
        let k = (dir.to_path_buf(), view.to_owned(), key.to_owned());
        let live = self.live_views.map.entry(k).or_insert_with(|| Live {
            plugin: plugin.clone(),
            tree: None,
            error: None,
            faults: Vec::new(),
            subs: BTreeSet::new(),
            asked: false,
            backend: Arc::downgrade(&backend),
            starts,
        });
        let values = std::mem::take(&mut reply.values);
        if reply.view.is_none() && live.tree.is_none() && !reply.patch.is_empty() {
            faults.push(Fault::new("/patch", format!("patches for view {view:?} before its first view were dropped: send the whole view first")));
            reply.patch.clear();
        }
        let old = live.tree.clone();
        let mut new = old.clone().unwrap_or_else(|| Node::new(tree::Kind::Column { children: Vec::new() }));
        faults.extend(reply.apply_to(&mut new));
        let fault_lines: Vec<String> = faults.iter().take(CLIENT_FAULTS).map(Fault::to_string).collect();
        let faults_changed = live.faults != fault_lines;
        live.faults = fault_lines.clone();
        live.tree = Some(new.clone());
        live.error = None;
        live.starts = starts;
        live.backend = Arc::downgrade(&backend);
        let subs: Vec<ClientId> = live.subs.iter().copied().collect();
        if !faults.is_empty() {
            backend.report_fault(&cut(&format!("view {view}: byob had to fix its tree:\n{}", Fault::report(&faults))));
        }
        let r = ViewRef::new(&plugin, view, key);
        let base = match &old {
            Some(old) => {
                let patch = tree::diff(old, &new);
                if patch.is_empty() && values.is_empty() && !faults_changed {
                    return;
                }
                LiveView { view: r, patch, faults: fault_lines, ..Default::default() }
            }
            None => LiveView { view: r, tree: Some(new), faults: fault_lines, ..Default::default() },
        };
        for id in subs {
            let mut m = base.clone();
            if to.is_none_or(|t| t == id) {
                m.values.clone_from(&values);
            }
            if m.tree.is_none() && m.patch.is_empty() && m.values.is_empty() && !faults_changed {
                continue;
            }
            self.send_live(id, m);
        }
    }

    /// Drop views whose backend was replaced, stopped or restarted since their tree was
    /// made; their clients get `reset` (and open them again).
    pub(crate) fn sweep_views(&mut self) {
        let mut gone = Vec::new();
        for (k, l) in &self.live_views.map {
            let current = self.ext.as_ref().and_then(|e| e.backends.get(&k.0)).map(|e| Arc::clone(&e.backend));
            let same = match (current, l.backend.upgrade()) {
                (Some(c), Some(b)) => Arc::ptr_eq(&c, &b) && c.status().phase != Phase::Stopped && (l.tree.is_none() || c.status().starts == l.starts),
                _ => false,
            };
            if !same {
                gone.push(k.clone());
            }
        }
        for k in gone {
            let Some(l) = self.live_views.map.remove(&k) else { continue };
            let r = ViewRef::new(&l.plugin, &k.1, &k.2);
            for id in l.subs {
                self.send_live(id, LiveView { view: r.clone(), reset: true, ..Default::default() });
            }
        }
    }
}

/// A view request's result or a `view` message's body: a bare tree (`{"type": …}`), or
/// `{"view": tree, "patch": [..], "values": {..}}`.
fn parse_reply(v: Value) -> (Reply, Vec<Fault>) {
    let bare = v.get("type").is_some();
    let v = if bare { json!({ "view": v }) } else { v };
    if !v.is_object() {
        return (Reply::default(), vec![Fault::new("", "a view must be a JSON object like {\"type\": \"column\", \"children\": [...]}")]);
    }
    match Reply::parse(&v.to_string()) {
        Ok((reply, mut faults)) => {
            // a bare tree: paths into what the plugin sent, not into our wrapper
            if bare {
                for f in &mut faults {
                    if let Some(rest) = f.path.strip_prefix("/view") {
                        f.path = rest.to_owned();
                    }
                }
            }
            (reply, faults)
        }
        Err(f) => (Reply::default(), vec![f]),
    }
}

fn cut(s: &str) -> String {
    s.chars().take(FAULT_CHARS).collect()
}
