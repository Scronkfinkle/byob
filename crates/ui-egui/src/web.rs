//! Web surfaces: HTML that talks to byob (MCP Apps, plugin web views, `web:<file>` artifact
//! renderers), and the client's half of their bridge (`byob_protocol::web`).
//!
//! The page runs on the [`AppView`] runtime — the Servo helper on the desktop, a sandboxed
//! `srcdoc` iframe in the browser — and speaks JSON-RPC 2.0 to its "parent", the MCP Apps
//! way (`window.parent.postMessage`). In the browser that reaches this page's window; in
//! the Servo helper the page is top-level, so [`BRIDGE_JS`] (put first in every document)
//! turns its posts into marked console lines the helper forwards (`Event::Message`), and
//! the helper delivers byob's messages as `message` events (`Request::Message`).
//!
//! [`Web`] is the host: it answers `ui/initialize` (host context with the theme as CSS
//! variables — the MCP standard names and byob's `--byob-*` ones), `ui/open-link`,
//! `ui/request-display-mode`, size changes and `ping` itself, sends the tool's input and
//! result after `ui/notifications/initialized`, theme changes as
//! `ui/notifications/host-context-changed`, and `ui/resource-teardown` before a surface goes
//! away. Everything else goes to the engine as `ClientMsg::WebCall` and its `WebReply` back
//! to the page; a `tools/call` the permission gate asks about shows an Allow/Deny bar on
//! the surface first. Owners' pushes (`WebPush`) reach their pages as `byob/message`.
//!
//! Sandbox (kept as it is for artifacts): no same-origin, no top navigation, popups or
//! permissions; a CSP meta tag with no network (`connect-src 'none'`, nothing from URLs).
//! The CSP domains and permissions an MCP App declares are not granted (logged by the
//! engine).

use std::collections::HashMap;

use byob_protocol::web::{McpApp, WebApproval, WebCard, WebDocument, WebError, WebOwner};
use byob_protocol::{ClientMsg, ServerMsg, View, ViewData as _};
use egui::{RichText, Stroke, vec2};
use serde_json::{Value, json};

use crate::appview::{AppDoc, AppView};
use crate::theme::Tokens;

/// Not drawn this long (seconds): the page is told it goes away, then closed.
const TEARDOWN_AFTER: f64 = 2.5;
/// How long a page may take to answer `ui/resource-teardown` (seconds).
const TEARDOWN_WAIT: f64 = 0.5;
/// Card height before the page says (points), and the range it may pick.
const CARD_HEIGHT: f32 = 300.0;
const CARD_MIN: f32 = 80.0;
const CARD_MAX: f32 = 640.0;
/// Calls to the engine waiting for an answer, at most.
const MAX_PENDING: usize = 256;
/// Surfaces remembered.
const MAX_SURFACES: usize = 64;
/// A surface opens at most one link this often (seconds).
const LINK_INTERVAL: f64 = 2.0;

/// The document's sandbox policy (a meta tag; the iframe's `sandbox` and the Servo helper's
/// load filter do the rest).
pub const CSP: &str = "default-src 'none'; script-src 'unsafe-inline' data: blob:; style-src 'unsafe-inline' data:; img-src data: blob:; \
    font-src data:; media-src data: blob:; connect-src 'none'; frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'";

/// Goes first in every web surface's document. Plain data both ways; byob treats what the
/// page says as untrusted. `AUTO` (replaced): start the handshake for byob's own API.
pub const BRIDGE_JS: &str = r"<script>(()=>{
const T=window.parent===window,AUTO=__AUTO__;
if(T){const L=console.log.bind(console),O=window.postMessage.bind(window);try{Object.defineProperty(window,'postMessage',{configurable:true,writable:true,value:function(m,...a){if(!m||typeof m!=='object'||m.jsonrpc!=='2.0')return O(m,...a);try{L('\x01byob-msg '+JSON.stringify(m))}catch(e){}}})}catch(e){}}
const P=m=>{try{(T?window:parent).postMessage(m,'*')}catch(e){}};
const V=o=>{if(!o||typeof o!=='object')return;for(const[k,v]of Object.entries(o))if(/^--[a-z0-9-]{1,48}$/.test(k)&&typeof v==='string'&&v.length<200&&!/[;{}<>]/.test(v))document.documentElement.style.setProperty(k,v)};
const C=c=>{if(!c||typeof c!=='object')return;if(c.styles)V(c.styles.variables);if(c.theme==='dark'||c.theme==='light')document.documentElement.style.colorScheme=c.theme};
let n=1e9;const W={},S=[];
addEventListener('message',e=>{if(e.source!==parent&&!(T&&e.source===window))return;const d=e.data;if(!d||typeof d!=='object')return;
if(d.result&&d.result.hostContext)C(d.result.hostContext);
if(d.method==='ui/notifications/host-context-changed')C(d.params);
if(d.id in W&&!d.method){const[ok,no]=W[d.id];delete W[d.id];d.error?no(new Error(d.error.message||'failed')):ok(d.result)}
else if(typeof d.method==='string'&&d.method.startsWith('byob/'))for(const f of S)try{f(d.params,d.method)}catch(x){console.error(x)}});
const call=(method,params)=>{const id=++n;P({jsonrpc:'2.0',id,method,params:params||{}});return new Promise((ok,no)=>{W[id]=[ok,no]})};
const B={call,
send:msg=>P({jsonrpc:'2.0',method:'byob/send',params:{msg}}),
ask:msg=>call('byob/call',{msg}),
tool:(name,args)=>call('tools/call',{name,arguments:args||{}}),
message:text=>call('ui/message',{role:'user',content:[{type:'text',text:String(text)}]}),
context:(text,data)=>call('ui/update-model-context',{content:text?[{type:'text',text:String(text)}]:[],structuredContent:data}),
openLink:url=>call('ui/open-link',{url:String(url)}),
on:f=>{if(typeof f==='function')S.push(f)}};
B.ready=AUTO?call('ui/initialize',{protocolVersion:'2026-01-26',appInfo:{name:document.title||'byob view',version:'1'},appCapabilities:{}}).then(r=>{P({jsonrpc:'2.0',method:'ui/notifications/initialized',params:{}});return r&&r.hostContext}):Promise.resolve(null);
if(AUTO&&typeof ResizeObserver==='function')addEventListener('DOMContentLoaded',()=>{const b=document.body;let h=0;new ResizeObserver(()=>{const v=Math.ceil(b.getBoundingClientRect().bottom+(parseFloat(getComputedStyle(b).marginBottom)||0)+scrollY);if(v!==h){h=v;P({jsonrpc:'2.0',method:'ui/notifications/size-changed',params:{width:innerWidth,height:v}})}}).observe(b)});
window.byob=B;
})()</script>";

/// What kind of page a surface runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// An MCP App: it brings its own client (the MCP Apps SDK or plain postMessage).
    McpApp,
    /// byob's own pages (plugin views, `web:` renderers): `window.byob` starts the handshake.
    Byob,
}

/// The theme as CSS variables for pages: MCP Apps' standard names and byob's own.
pub fn style_variables(vars: &[(&'static str, String)]) -> serde_json::Map<String, Value> {
    let get = |k: &str| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone()).unwrap_or_default();
    let (bg, surface, fg, muted, accent, border) =
        (get("--byob-bg"), get("--byob-surface"), get("--byob-fg"), get("--byob-muted"), get("--byob-accent"), get("--byob-border"));
    let (danger, success, warning, link, faint, panel, field) =
        (get("--byob-danger"), get("--byob-success"), get("--byob-warning"), get("--byob-link"), get("--byob-faint"), get("--byob-panel"), get("--byob-field"));
    let radius: f32 = get("--byob-radius").trim_end_matches("px").parse().unwrap_or(6.0);
    let px = |f: f32| format!("{}px", f.round());
    let mut m = serde_json::Map::new();
    let mut put = |k: &str, v: String| {
        if !v.is_empty() {
            m.insert(k.to_owned(), Value::String(v));
        }
    };
    for (k, v) in [
        ("--color-background-primary", &bg),
        ("--color-background-secondary", &surface),
        ("--color-background-tertiary", &panel),
        ("--color-background-inverse", &fg),
        ("--color-background-info", &accent),
        ("--color-background-danger", &danger),
        ("--color-background-success", &success),
        ("--color-background-warning", &warning),
        ("--color-background-disabled", &field),
        ("--color-text-primary", &fg),
        ("--color-text-secondary", &muted),
        ("--color-text-tertiary", &faint),
        ("--color-text-inverse", &bg),
        ("--color-text-info", &link),
        ("--color-text-danger", &danger),
        ("--color-text-success", &success),
        ("--color-text-warning", &warning),
        ("--color-text-disabled", &faint),
        ("--color-text-ghost", &muted),
        ("--color-border-primary", &border),
        ("--color-border-secondary", &border),
        ("--color-border-tertiary", &border),
        ("--color-border-inverse", &fg),
        ("--color-border-info", &accent),
        ("--color-border-danger", &danger),
        ("--color-border-success", &success),
        ("--color-border-warning", &warning),
        ("--color-border-disabled", &border),
        ("--color-ring-primary", &accent),
        ("--color-ring-secondary", &border),
        ("--color-ring-inverse", &fg),
        ("--color-ring-info", &accent),
        ("--color-ring-danger", &danger),
        ("--color-ring-success", &success),
        ("--color-ring-warning", &warning),
    ] {
        put(k, v.clone());
    }
    for k in ["--color-background-ghost", "--color-border-ghost"] {
        put(k, "transparent".into());
    }
    put("--font-sans", "system-ui, -apple-system, \"Segoe UI\", sans-serif".into());
    put("--font-mono", "ui-monospace, \"SF Mono\", Menlo, Consolas, monospace".into());
    for (k, v) in [("normal", "400"), ("medium", "500"), ("semibold", "600"), ("bold", "700")] {
        put(&format!("--font-weight-{k}"), v.into());
    }
    for (k, f) in [("xs", 0.34), ("sm", 0.67), ("md", 1.0), ("lg", 1.5), ("xl", 2.0)] {
        put(&format!("--border-radius-{k}"), px(radius * f));
    }
    put("--border-radius-full", "9999px".into());
    put("--border-width-regular", "1px".into());
    for (k, v) in vars {
        put(k, v.clone());
    }
    m
}

/// `html` ready to run as a web surface: the CSP, the theme as defaults, and [`BRIDGE_JS`]
/// before anything the page has (after its doctype, so standards mode stays).
pub fn document(html: &str, kind: Kind, vars: &[(&'static str, String)]) -> String {
    let defaults: String = style_variables(vars).iter().filter_map(|(k, v)| v.as_str().map(|v| format!("{k}:{v};"))).collect();
    let body = if kind == Kind::Byob { "html,body{background:var(--byob-bg);color:var(--byob-fg);font-family:var(--font-sans)}" } else { "" };
    let auto = if kind == Kind::Byob { "true" } else { "false" };
    let head = format!(
        "<meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"{CSP}\"><style>:root{{{defaults}color-scheme:light dark}}{body}</style>{}",
        BRIDGE_JS.replace("__AUTO__", auto)
    );
    let start = html.trim_start();
    if start.get(..9).is_some_and(|s| s.eq_ignore_ascii_case("<!doctype")) {
        let end = start.find('>').map_or(start.len(), |i| i.saturating_add(1));
        format!("{}{head}{}", start.get(..end).unwrap_or_default(), start.get(end..).unwrap_or_default())
    } else {
        format!("<!doctype html>{head}{html}")
    }
}

/// What a surface shows besides its page: the call that made it, or a card's data.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Init {
    /// MCP Apps: the tool's arguments and `CallToolResult` (sent after `initialized`), its
    /// definition (`hostContext.toolInfo`).
    pub tool_input: Option<Value>,
    pub tool_result: Option<Value>,
    pub tool_def: Option<Value>,
    /// byob pages: a notification sent after `initialized` (`byob/card`, `byob/artifact`).
    pub notify: Option<(String, Value)>,
}

/// One surface to draw: its key (stable while it is the same thing on screen), owner,
/// kind, title and what it starts with.
pub struct Spec<'a> {
    pub key: &'a str,
    pub owner: &'a WebOwner,
    pub kind: Kind,
    pub title: &'a str,
    /// `None`: the same as last frame (not built again each frame; see [`Web::known`]).
    pub init: Option<Init>,
}

enum Doc {
    /// `byob/document` asked (the call number).
    Fetching,
    Ready(String),
    Failed(String),
}

/// A call waiting for the user's OK before it goes to the engine.
struct Asking {
    page_id: Value,
    method: String,
    params: Value,
    approval: WebApproval,
}

struct Surface {
    owner: WebOwner,
    kind: Kind,
    title: String,
    doc: Doc,
    init: Init,
    /// The page finished its handshake (`ui/notifications/initialized`).
    initialized: bool,
    /// Bumped when the page is reloaded after a teardown (a new document key).
    generation: u32,
    /// The page's height from `ui/notifications/size-changed` (CSS pixels).
    height: Option<f32>,
    /// egui time it was last drawn.
    last_drawn: f64,
    /// Teardown sent at (egui time), waiting for its answer.
    tearing: Option<f64>,
    asking: Option<Asking>,
    /// Drawn in the pane (fullscreen) this frame or last.
    fullscreen: bool,
    /// Closed after a teardown, not drawn since.
    closed: bool,
    /// When it last had a link opened (egui time).
    last_link: Option<f64>,
    /// Its chat was closed: the record goes once the page does.
    gone: bool,
}

/// What a call to the engine was for.
enum Pending {
    Document(String),
    /// A page's request (its JSON-RPC id), or a notification (`None`: the answer is dropped).
    Page {
        key: String,
        page_id: Option<Value>,
        method: String,
        params: Value,
    },
}

/// The client's web surfaces and their bridge. Owns the [`AppView`] runtime.
pub struct Web {
    pub(crate) view: Box<dyn AppView>,
    surfaces: HashMap<String, Surface>,
    pending: HashMap<u64, Pending>,
    next_call: u64,
    out: Vec<ClientMsg>,
    /// The engine lists `web_bridge`.
    enabled: bool,
    vars: Vec<(&'static str, String)>,
    dark: bool,
    links: Vec<String>,
    now: f64,
    /// Step views' surfaces, decoded (by surface key).
    step_views: HashMap<String, StepView>,
    /// The surface shown over the window (an MCP App's "Maximize").
    maximized: Option<String>,
    /// The page to answer teardown with: (key, host request id).
    next_host_id: u64,
}

impl Web {
    pub fn new(view: Box<dyn AppView>) -> Self {
        Self {
            view,
            surfaces: HashMap::new(),
            pending: HashMap::new(),
            next_call: 0,
            out: Vec::new(),
            enabled: false,
            vars: Vec::new(),
            dark: true,
            links: Vec::new(),
            now: 0.0,
            step_views: HashMap::new(),
            maximized: None,
            next_host_id: 1,
        }
    }

    /// Messages for the engine since the last call.
    pub fn take(&mut self) -> Vec<ClientMsg> {
        std::mem::take(&mut self.out)
    }

    /// Links pages asked to open (`ui/open-link`).
    pub fn take_links(&mut self) -> Vec<String> {
        std::mem::take(&mut self.links)
    }

    /// A surface is shown over the window.
    pub fn maximized(&self) -> Option<&str> {
        self.maximized.as_deref()
    }

    /// The theme changed (or is first known).
    pub fn set_theme(&mut self, vars: &[(&'static str, String)]) {
        self.view.set_theme(vars);
        self.vars = vars.to_vec();
        self.dark = vars.iter().find(|(k, _)| *k == "--byob-bg").is_none_or(|(_, v)| crate::appview::luminance(v) < 0.5);
        let ctx = json!({"theme": self.theme_name(), "styles": {"variables": style_variables(&self.vars)}});
        let keys: Vec<String> = self.surfaces.iter().filter(|(_, s)| s.initialized).map(|(k, _)| k.clone()).collect();
        for k in keys {
            self.notify(&k, "ui/notifications/host-context-changed", ctx.clone());
        }
    }

    fn theme_name(&self) -> &'static str {
        if self.dark { "dark" } else { "light" }
    }

    /// Take in a server message; true if it was only for us.
    pub fn apply(&mut self, msg: &ServerMsg) -> bool {
        match msg {
            ServerMsg::Welcome(w) => {
                self.enabled = byob_protocol::version::shared(&byob_protocol::version::client_caps(), &w.caps, byob_protocol::web::CAP_WEB_BRIDGE);
                false
            }
            ServerMsg::ChatOpened { .. } => {
                // Another chat: its surfaces go (told first).
                let keys: Vec<String> = self.surfaces.keys().cloned().collect();
                for k in keys {
                    if let Some(s) = self.surfaces.get_mut(&k) {
                        s.gone = true;
                    }
                    self.teardown(&k, "the chat was closed");
                    // Already closed (out of view before): nothing to wait for.
                    if self.surfaces.get(&k).is_some_and(|s| s.closed) {
                        self.surfaces.remove(&k);
                    }
                }
                self.maximized = None;
                false
            }
            ServerMsg::WebReply { call, result, error } => {
                self.reply(*call, result.clone(), error.clone());
                true
            }
            ServerMsg::WebPush { owner, msg } => {
                let keys: Vec<String> = self.surfaces.iter().filter(|(_, s)| s.initialized && &s.owner == owner).map(|(k, _)| k.clone()).collect();
                for k in keys {
                    self.notify(&k, "byob/message", msg.clone());
                }
                true
            }
            _ => false,
        }
    }

    fn call(&mut self, owner: WebOwner, method: &str, params: Value, approved: bool, pending: Pending) -> bool {
        if !self.enabled || self.pending.len() >= MAX_PENDING {
            return false;
        }
        self.next_call = self.next_call.wrapping_add(1);
        let call = self.next_call;
        self.pending.insert(call, pending);
        self.out.push(ClientMsg::WebCall { call, owner, method: method.to_owned(), params, approved });
        true
    }

    fn reply(&mut self, call: u64, result: Option<Value>, error: Option<WebError>) {
        match self.pending.remove(&call) {
            Some(Pending::Document(key)) => {
                let Some(s) = self.surfaces.get_mut(&key) else { return };
                s.doc = match (result.and_then(|r| serde_json::from_value::<WebDocument>(r).ok()), error) {
                    (Some(d), _) => Doc::Ready(document(&d.html, s.kind, &self.vars)),
                    (None, Some(e)) => Doc::Failed(e.message),
                    (None, None) => Doc::Failed("the engine sent no document".into()),
                };
            }
            Some(Pending::Page { key, page_id, method, params }) => {
                let Some(id) = page_id else { return };
                if let Some(approval) = error.as_ref().and_then(|e| e.approval.clone())
                    && let Some(s) = self.surfaces.get_mut(&key)
                {
                    if s.asking.is_some() {
                        // One question at a time: the one asked keeps its bar (and its answer).
                        self.answer(&key, id, Err((-32000, "Another call is waiting for the user's OK; try again after it".into())));
                    } else {
                        s.asking = Some(Asking { page_id: id, method, params, approval });
                    }
                    return;
                }
                let msg = match (result, error) {
                    (_, Some(e)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": e.code, "message": e.message}}),
                    (r, None) => json!({"jsonrpc": "2.0", "id": id, "result": r.unwrap_or_else(|| json!({}))}),
                };
                self.post(&key, &msg);
            }
            None => {}
        }
    }

    fn doc_key(key: &str, s: &Surface) -> (String, u32) {
        (key.to_owned(), s.generation)
    }

    /// The largest message a page gets (bytes; the Servo helper's is smaller than the
    /// browser's).
    pub fn max_message(&self) -> usize {
        self.view.max_message()
    }

    /// Send the page a JSON-RPC message; whether it went. An answer too large for the
    /// runtime is answered with an error instead, so the page's call still settles.
    fn post(&mut self, key: &str, msg: &Value) -> bool {
        let Some(s) = self.surfaces.get(key) else { return false };
        let (id, v) = Self::doc_key(key, s);
        let text = msg.to_string();
        let max = self.view.max_message();
        if text.len() <= max {
            return self.view.post(&id, v, &text);
        }
        match msg.get("id") {
            Some(page_id) if msg.get("method").is_none() => {
                let message = format!("The answer is too large for the page ({} KB; at most {} KB)", text.len() / 1024, max / 1024);
                let err = json!({"jsonrpc": "2.0", "id": page_id, "error": {"code": -32000, "message": message}});
                self.view.post(&id, v, &err.to_string());
            }
            _ => log::warn!("web surface {key}: dropped a message of {} bytes (at most {max})", text.len()),
        }
        false
    }

    fn notify(&mut self, key: &str, method: &str, params: Value) {
        let _ = self.post(key, &json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    fn answer(&mut self, key: &str, id: Value, result: Result<Value, (i64, String)>) {
        let msg = match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err((code, message)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
        };
        let _ = self.post(key, &msg);
    }

    /// Make or refresh a surface's record; fetch its document the first time.
    fn ensure(&mut self, key: &str, owner: &WebOwner, kind: Kind, title: &str, init: Option<Init>) {
        if !self.surfaces.contains_key(key) {
            if self.surfaces.len() >= MAX_SURFACES {
                let oldest = self
                    .surfaces
                    .iter()
                    .filter(|(_, s)| s.last_drawn < self.now - TEARDOWN_AFTER)
                    .min_by(|a, b| a.1.last_drawn.total_cmp(&b.1.last_drawn))
                    .map(|(k, _)| k.clone());
                if let Some(k) = oldest {
                    self.close_now(&k);
                    self.surfaces.remove(&k);
                }
            }
            let mut s = Surface {
                owner: owner.clone(),
                kind,
                title: title.to_owned(),
                doc: Doc::Failed("this byob can't show apps (the engine is older)".into()),
                init: init.unwrap_or_default(),
                initialized: false,
                generation: 1,
                height: None,
                last_drawn: self.now,
                tearing: None,
                asking: None,
                fullscreen: false,
                closed: false,
                last_link: None,
                gone: false,
            };
            if self.enabled {
                s.doc = Doc::Fetching;
            }
            self.surfaces.insert(key.to_owned(), s);
            if self.enabled {
                let _ = self.call(owner.clone(), "byob/document", json!({}), false, Pending::Document(key.to_owned()));
            }
        } else if let Some(init) = init
            && let Some(s) = self.surfaces.get_mut(key)
            && s.init != init
        {
            let changed_result = s.init.tool_result != init.tool_result;
            s.init = init;
            if changed_result
                && s.initialized
                && let Some(r) = s.init.tool_result.clone()
            {
                self.notify(key, "ui/notifications/tool-result", r);
            }
        }
    }

    /// Surface `key` has a record (what it starts with needn't be built again).
    pub fn known(&self, key: &str) -> bool {
        self.surfaces.contains_key(key)
    }

    /// Draw surface `key` in a card (full width, the page's own height).
    pub fn card(&mut self, ui: &mut egui::Ui, t: &Tokens, spec: Spec<'_>) {
        let Spec { key, owner, kind, title, init } = spec;
        self.now = ui.input(|i| i.time);
        self.ensure(key, owner, kind, title, init);
        let maximized = self.maximized.as_deref() == Some(key);
        ui.horizontal(|ui| {
            let label = match owner {
                WebOwner::Mcp { plugin, server, .. } => format!("App · {plugin}/{server}"),
                WebOwner::Plugin { plugin, .. } => format!("View · {plugin}"),
                WebOwner::Other => "App".into(),
            };
            ui.label(RichText::new(label).small().color(t.text_dim));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (text, tip) = if maximized { ("Restore", "Back to the chat") } else { ("Maximize", "Show the app over the window") };
                if ui.small_button(text).on_hover_text(tip).clicked() {
                    self.maximized = if maximized { None } else { Some(key.to_owned()) };
                }
            });
        });
        if maximized {
            ui.label(RichText::new("Shown over the window").small().color(t.text_faint));
            return;
        }
        self.surface(ui, t, key, false);
    }

    /// Draw the maximized surface into the space left (with Restore).
    pub fn show_maximized(&mut self, ui: &mut egui::Ui, t: &Tokens) {
        let Some(key) = self.maximized.clone() else { return };
        self.now = ui.input(|i| i.time);
        if !self.surfaces.contains_key(&key) {
            self.maximized = None;
            return;
        }
        let title = self.surfaces.get(&key).map(|s| s.title.clone()).unwrap_or_default();
        ui.horizontal(|ui| {
            ui.label(RichText::new(title).strong().color(t.text_strong));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Restore").on_hover_text("Back to the chat (Esc)").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    self.maximized = None;
                }
            });
        });
        self.surface(ui, t, &key, true);
    }

    /// Draw surface `key` filling the space left (a plugin's pane).
    pub fn fill(&mut self, ui: &mut egui::Ui, t: &Tokens, spec: Spec<'_>) {
        let Spec { key, owner, kind, title, init } = spec;
        self.now = ui.input(|i| i.time);
        self.ensure(key, owner, kind, title, init);
        self.surface(ui, t, key, true);
    }

    fn surface(&mut self, ui: &mut egui::Ui, t: &Tokens, key: &str, fill: bool) {
        let now = self.now;
        let mut decided = None;
        let Some(s) = self.surfaces.get_mut(key) else { return };
        s.last_drawn = now;
        s.fullscreen = fill;
        s.closed = false;
        s.gone = false;
        if s.tearing.is_some() {
            // Came back while it was being told to go: it carries on.
            s.tearing = None;
        }
        if let Some(a) = &s.asking {
            egui::Frame::new().fill(t.code_bg).stroke(Stroke::new(1.0, t.warning)).corner_radius(t.round(6.0)).inner_margin(t.pad(6.0)).show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(format!("The app wants to run {} ({}).", a.approval.title, a.approval.access.label())).color(t.text));
                    if ui.button("Allow").clicked() {
                        decided = Some(true);
                    }
                    if ui.button("Deny").clicked() {
                        decided = Some(false);
                    }
                });
            });
        }
        let (id, version) = Self::doc_key(key, s);
        match &s.doc {
            Doc::Fetching => {
                ui.horizontal(|ui| {
                    crate::theme::spinner(ui, t, None);
                    ui.label(RichText::new("Loading the app…").small().color(t.text_dim));
                });
            }
            Doc::Failed(e) => {
                egui::Frame::new().stroke(Stroke::new(1.0, t.danger.gamma_multiply(0.6))).corner_radius(t.round(6.0)).inner_margin(t.pad(6.0)).show(ui, |ui| {
                    ui.add(egui::Label::new(RichText::new(format!("⚠ Couldn't show the app: {e}")).small().color(t.danger)).wrap());
                });
            }
            Doc::Ready(html) => {
                if self.view.bridged(&id, version) == Some(false) {
                    ui.label(RichText::new("⚠ This app needs a newer byob-artifact-host to talk to byob.").small().color(t.warning));
                }
                if !self.view.runs_inline() {
                    ui.label(RichText::new("Apps need byob's web runtime (byob-artifact-host) on this computer, or the browser UI.").small().color(t.text_dim));
                } else {
                    let height = s.height.map(|h| h.clamp(CARD_MIN, CARD_MAX));
                    let doc = AppDoc { id: &id, url: None, title: &s.title, version, size: html.len() as u64, source: Some(html), web: true, height };
                    if fill {
                        let _ = self.view.show(ui, t, &doc);
                    } else if self.view.card(ui, t, &doc).is_none() {
                        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), height.unwrap_or(CARD_HEIGHT)), egui::Sense::hover());
                        ui.put(rect, egui::Label::new(RichText::new("Open the app with Maximize").small().color(t.text_dim)));
                    }
                }
            }
        }
        if let Some(yes) = decided {
            self.decide(key, yes);
        }
    }

    /// The user answered the call surface `key` asked about: send it (approved) or refuse it.
    pub fn decide(&mut self, key: &str, allow: bool) {
        let Some(s) = self.surfaces.get_mut(key) else { return };
        let Some(a) = s.asking.take() else { return };
        let owner = s.owner.clone();
        if allow {
            let pending = Pending::Page { key: key.to_owned(), page_id: Some(a.page_id.clone()), method: a.method.clone(), params: a.params.clone() };
            if !self.call(owner, &a.method, a.params, true, pending) {
                self.answer(key, a.page_id, Err((-32000, "byob can't do that now".into())));
            }
        } else {
            self.answer(key, a.page_id, Err((-32000, "Denied by the user".into())));
        }
    }

    /// A call of surface `key` waits for the user's OK (its title).
    pub fn asking(&self, key: &str) -> Option<&str> {
        self.surfaces.get(key).and_then(|s| s.asking.as_ref()).map(|a| a.approval.title.as_str())
    }

    /// Once per frame, after the runtime's `end_frame`: what pages said, teardowns.
    pub fn pump(&mut self, now: f64) {
        self.now = now;
        for (id, version, text) in self.view.take_messages() {
            let Some((key, s)) = self.surfaces.iter().find(|(k, s)| **k == id && s.generation == version) else { continue };
            let _ = s;
            let key = key.clone();
            self.page_message(&key, &text);
        }
        let idle: Vec<String> = self
            .surfaces
            .iter()
            .filter(|(k, s)| !s.closed && s.tearing.is_none() && now - s.last_drawn > TEARDOWN_AFTER && self.maximized.as_deref() != Some(k.as_str()))
            .map(|(k, _)| k.clone())
            .collect();
        for k in idle {
            self.teardown(&k, "it is out of view");
        }
        let done: Vec<String> = self.surfaces.iter().filter(|(_, s)| s.tearing.is_some_and(|at| now - at >= TEARDOWN_WAIT)).map(|(k, _)| k.clone()).collect();
        for k in done {
            self.close_now(&k);
        }
    }

    /// Tell the page it goes away (if it talked to us), then close it.
    fn teardown(&mut self, key: &str, reason: &str) {
        let Some(s) = self.surfaces.get_mut(key) else { return };
        if s.tearing.is_some() || s.closed {
            return;
        }
        if !s.initialized {
            // Nothing running that would want to know (or it never loaded).
            s.tearing = Some(self.now - TEARDOWN_WAIT);
            return;
        }
        s.tearing = Some(self.now);
        let id = self.next_host_id;
        self.next_host_id = self.next_host_id.wrapping_add(1);
        self.post(key, &json!({"jsonrpc": "2.0", "id": format!("byob-{id}"), "method": "ui/resource-teardown", "params": {"reason": reason}}));
    }

    /// Stop the page now; the next draw starts it afresh (same document). A surface whose
    /// chat closed, or whose document failed, is forgotten.
    fn close_now(&mut self, key: &str) {
        let Some(s) = self.surfaces.get_mut(key) else { return };
        let (id, v) = (key.to_owned(), s.generation);
        s.tearing = None;
        s.initialized = false;
        s.asking = None;
        s.closed = true;
        s.generation = s.generation.wrapping_add(1);
        self.view.close(&id, v);
        if s.gone || matches!(s.doc, Doc::Failed(_)) {
            self.surfaces.remove(key);
        }
    }

    fn page_message(&mut self, key: &str, text: &str) {
        let Ok(msg) = serde_json::from_str::<Value>(text) else { return };
        let Some(o) = msg.as_object() else { return };
        let method = o.get("method").and_then(Value::as_str).map(str::to_owned);
        let id = o.get("id").filter(|i| i.is_string() || i.is_number()).cloned();
        let params = o.get("params").cloned().unwrap_or_else(|| json!({}));
        let Some(method) = method else {
            // An answer to one of ours: only teardown asks.
            if id.as_ref().and_then(Value::as_str).is_some_and(|i| i.starts_with("byob-"))
                && let Some(s) = self.surfaces.get_mut(key)
                && s.tearing.is_some()
            {
                s.tearing = Some(self.now - TEARDOWN_WAIT);
            }
            return;
        };
        let Some(s) = self.surfaces.get_mut(key) else { return };
        match (method.as_str(), id) {
            ("ui/initialize", Some(id)) => {
                s.initialized = false;
                let result = self.initialize_result(key);
                self.answer(key, id, Ok(result));
            }
            ("ui/notifications/initialized", None) => {
                s.initialized = true;
                let init = s.init.clone();
                if let Some(input) = init.tool_input {
                    self.notify(key, "ui/notifications/tool-input", json!({"arguments": input}));
                }
                if let Some(r) = init.tool_result {
                    self.notify(key, "ui/notifications/tool-result", r);
                }
                if let Some((m, p)) = init.notify {
                    self.notify(key, &m, p);
                }
            }
            ("ui/notifications/size-changed", None) => {
                if let Some(h) = params.get("height").and_then(Value::as_f64).filter(|h| h.is_finite() && *h > 0.0) {
                    #[allow(clippy::cast_possible_truncation)]
                    let h = h.min(100_000.0) as f32;
                    s.height = Some(h);
                }
            }
            ("ui/open-link", Some(id)) => {
                let url = params.get("url").and_then(Value::as_str).and_then(crate::export::browser_url);
                let now = self.now;
                if s.last_link.is_some_and(|at| now - at < LINK_INTERVAL && now >= at) {
                    self.answer(key, id, Err((-32000, "Too many links at once; try again in a moment".into())));
                } else if let Some(url) = url {
                    s.last_link = Some(now);
                    self.links.push(url);
                    self.answer(key, id, Ok(json!({})));
                } else {
                    self.answer(key, id, Err((-32602, "Invalid URL".into())));
                }
            }
            ("ui/request-display-mode", Some(id)) => {
                let want = params.get("mode").and_then(Value::as_str).unwrap_or("inline");
                let mode = match want {
                    "fullscreen" => {
                        self.maximized = Some(key.to_owned());
                        "fullscreen"
                    }
                    _ => {
                        if self.maximized.as_deref() == Some(key) {
                            self.maximized = None;
                        }
                        "inline"
                    }
                };
                self.answer(key, id, Ok(json!({"mode": mode})));
            }
            ("ping", Some(id)) => self.answer(key, id, Ok(json!({}))),
            ("notifications/message", None) => log::info!("web surface {key}: {}", params.to_string().chars().take(500).collect::<String>()),
            (_, page_id) => {
                let owner = s.owner.clone();
                let pending = Pending::Page { key: key.to_owned(), page_id: page_id.clone(), method: method.clone(), params: params.clone() };
                if !self.call(owner, &method, params, false, pending)
                    && let Some(id) = page_id
                {
                    self.answer(key, id, Err((-32000, "byob can't do that here (the engine is older, or too busy)".into())));
                }
            }
        }
    }

    fn initialize_result(&self, key: &str) -> Value {
        let s = self.surfaces.get(key);
        let mcp = matches!(s.map(|s| &s.owner), Some(WebOwner::Mcp { .. }));
        let mut ctx = json!({
            "theme": self.theme_name(),
            "styles": {"variables": style_variables(&self.vars)},
            "displayMode": if self.maximized.as_deref() == Some(key) || s.is_some_and(|s| s.fullscreen) { "fullscreen" } else { "inline" },
            "availableDisplayModes": ["inline", "fullscreen"],
            "platform": if cfg!(target_arch = "wasm32") { "web" } else { "desktop" },
            "userAgent": format!("byob/{}", env!("CARGO_PKG_VERSION")),
            "deviceCapabilities": {"touch": false, "hover": true},
        });
        if let Some(def) = s.and_then(|s| s.init.tool_def.clone()) {
            ctx["toolInfo"] = json!({"tool": def});
        }
        let mut caps = json!({"openLinks": {}, "logging": {}});
        if mcp {
            caps["serverTools"] = json!({});
            caps["serverResources"] = json!({});
        }
        json!({
            "protocolVersion": byob_protocol::web::MCP_APPS_VERSION,
            "hostCapabilities": caps,
            "hostInfo": {"name": "byob", "version": env!("CARGO_PKG_VERSION")},
            "hostContext": ctx,
        })
    }
}

// ---- renderers --------------------------------------------------------------------------

/// What a step view's surface is, decoded once per change of its data.
pub(crate) struct StepView {
    data: Value,
    /// `None`: the data doesn't fit (the fallback shows).
    shown: Option<(WebOwner, Kind, String, Init)>,
}

/// Draw step view `view` as surface `key`, decoding it (`decode`) only when its data changed.
fn step_view(
    ui: &mut egui::Ui,
    t: &Tokens,
    web: &mut Web,
    key: String,
    view: &View,
    decode: impl FnOnce(&View) -> Option<(WebOwner, Kind, String, Init)>,
) -> bool {
    let fresh = web.step_views.get(&key).is_none_or(|c| c.data != view.data);
    if fresh {
        if web.step_views.len() >= MAX_SURFACES * 4 {
            let surfaces = &web.surfaces;
            web.step_views.retain(|k, _| surfaces.contains_key(k));
        }
        web.step_views.insert(key.clone(), StepView { data: view.data.clone(), shown: decode(view) });
    }
    let Some((owner, kind, title, init)) = web.step_views.get(&key).and_then(|c| c.shown.as_ref()) else { return false };
    let init = (fresh || !web.known(&key)).then(|| init.clone());
    let (owner, kind, title) = (owner.clone(), *kind, title.clone());
    web.card(ui, t, Spec { key: &key, owner: &owner, kind, title: &title, init });
    true
}

/// `byob.mcp_app` on a step: the MCP App, live.
pub(crate) fn mcp_app_view(ui: &mut egui::Ui, t: &Tokens, web: &mut Web, step: u64, index: usize, view: &View) -> bool {
    if view.v > McpApp::V {
        return false;
    }
    step_view(ui, t, web, format!("web:mcp:{step}:{index}"), view, |view| {
        let app = view.get::<McpApp>()?;
        let title = if app.title.is_empty() { app.tool.clone() } else { app.title.clone() };
        let owner = app.owner();
        let init = Init { tool_input: Some(app.input), tool_result: Some(app.result), tool_def: Some(app.tool_def).filter(|d| !d.is_null()), notify: None };
        Some((owner, Kind::McpApp, title, init))
    })
}

/// `byob.web` on a step: a plugin's web card.
pub(crate) fn web_card_view(ui: &mut egui::Ui, t: &Tokens, web: &mut Web, step: u64, index: usize, view: &View) -> bool {
    if view.v > WebCard::V {
        return false;
    }
    step_view(ui, t, web, format!("web:card:{step}:{index}"), view, |view| {
        let card = view.get::<WebCard>()?;
        let owner = card.owner();
        let init = Init { notify: Some(("byob/card".into(), json!({"data": card.data, "key": card.key}))), ..Default::default() };
        Some((owner, Kind::Byob, card.view, init))
    })
}

/// A plugin's `web:<file>` artifact renderer: the page gets the artifact's text.
pub(crate) struct WebArtifact {
    pub plugin: String,
    pub file: String,
}

impl WebArtifact {
    fn draw(&self, ui: &mut egui::Ui, cx: &mut crate::renderers::ArtifactCx<'_>, fill: bool) {
        let (t, a, version) = (cx.t, cx.artifact, cx.version);
        let place = if fill { "pane" } else { "card" };
        // The key carries the version: a surface once made has its text (not copied again).
        let key = format!("web:artifact:{}:{}:{version}:{place}", self.plugin, a.id);
        let (known, max) = (cx.bridge().known(&key), cx.bridge().max_message());
        let init = match cx.bytes() {
            crate::renderers::Fetch::Ready(_) if known => None,
            crate::renderers::Fetch::Ready(b) => Some(artifact_init(a, version, &String::from_utf8_lossy(b), max)),
            crate::renderers::Fetch::Loading => {
                crate::theme::spinner(ui, t, Some(12.0));
                return;
            }
            crate::renderers::Fetch::Failed(e) => {
                ui.label(RichText::new(format!("Couldn't show it: {e}")).small().color(t.text_dim));
                return;
            }
        };
        let owner = WebOwner::Plugin { plugin: self.plugin.clone(), view: format!("artifact:{}", self.file), key: a.id.clone() };
        let web = cx.bridge();
        if fill {
            web.fill(ui, t, Spec { key: &key, owner: &owner, kind: Kind::Byob, title: &a.title, init });
        } else {
            web.card(ui, t, Spec { key: &key, owner: &owner, kind: Kind::Byob, title: &a.title, init });
        }
    }
}

/// Room in a `byob/artifact` message for everything but the text (bytes).
const ARTIFACT_OVERHEAD: usize = 4096;

/// What a `web:` renderer's page starts with: the artifact and as much of its text as fits
/// one message of at most `max` bytes (`truncated` says if it was cut).
fn artifact_init(a: &byob_protocol::Artifact, version: u32, text: &str, max: usize) -> Init {
    let title: String = a.title.chars().take(500).collect();
    let budget = max.saturating_sub(ARTIFACT_OVERHEAD.saturating_add(title.len().saturating_mul(6)).saturating_add(a.id.len().saturating_mul(6)));
    let (text, cut) = fit_json(text, budget);
    let data = json!({"id": a.id, "version": version, "title": title, "content_type": a.content_type(), "text": text, "truncated": cut});
    Init { notify: Some(("byob/artifact".into(), data)), ..Default::default() }
}

/// The longest start of `text` whose JSON string is at most `budget` bytes; whether it was cut.
fn fit_json(text: &str, budget: usize) -> (&str, bool) {
    let mut used: usize = 2;
    for (i, c) in text.char_indices() {
        let n = match c {
            '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
            c if u32::from(c) < 0x20 => 6,
            c => c.len_utf8(),
        };
        used = used.saturating_add(n);
        if used > budget {
            return (text.get(..i).unwrap_or_default(), true);
        }
    }
    (text, false)
}

impl crate::renderers::ArtifactRenderer for WebArtifact {
    fn caps(&self) -> crate::renderers::RendererCaps {
        crate::renderers::RendererCaps { label: "view".into(), text: true, source_view: true, web_surface: false }
    }

    fn card(&self, ui: &mut egui::Ui, cx: &mut crate::renderers::ArtifactCx<'_>) {
        self.draw(ui, cx, false);
    }

    fn pane(&self, ui: &mut egui::Ui, cx: &mut crate::renderers::ArtifactCx<'_>) {
        self.draw(ui, cx, true);
    }
}

#[cfg(test)]
mod tests;
