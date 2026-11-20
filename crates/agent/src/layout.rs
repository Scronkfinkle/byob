//! The layout (what the interface shows, `byob_types::layout`): the engine keeps the one
//! copy, saves it in settings.json, sends it to every client that lists the `layout` cap,
//! and offers it to the model as the `ui_layout` tool (named by the bundled `appearance`
//! plugin), so "hide the token counts" or "put the context meter first" just works.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use byob_protocol::layout::{CAP_LAYOUT, ELEMENTS, GROUPS, Layout, PRESETS, REGIONS, Show, element, group, region, view_id};
use byob_protocol::{ServerMsg, Site};
use byob_tools::{Access, Tool, ToolContext, ToolOutput, ToolSpec};
use serde_json::{Value, json};

use crate::{ClientId, Engine, Input, plugins};

/// The tool's name.
pub const TOOL: &str = "ui_layout";

/// A change the tool asks the engine to make (`None`: only read it).
pub(crate) type Change = Option<Layout>;

/// How long a tool waits for the engine.
const TIMEOUT: Duration = Duration::from_secs(10);

impl Engine {
    /// The saved layout (settings.json), or the default.
    pub(crate) fn load_layout(paths: Option<&byob_store::Paths>) -> Layout {
        paths.map(|p| byob_store::Settings::load(&p.settings()).value.layout.sanitized()).unwrap_or_default()
    }

    fn wants_layout(&self, client: ClientId) -> bool {
        self.clients.get(&client).is_some_and(|c| c.caps.iter().any(|c| c == CAP_LAYOUT))
    }

    /// After the Welcome: the layout, to a client that knows it.
    pub(crate) fn send_layout(&mut self, client: ClientId) {
        if self.wants_layout(client) {
            self.send_to(client, ServerMsg::Layout(self.layout.clone()));
        }
    }

    /// Replace the layout: saved, then sent to every client that knows it.
    pub(crate) fn set_layout(&mut self, layout: Layout) -> Result<(), String> {
        let layout = layout.sanitized();
        if layout == self.layout {
            return Ok(());
        }
        self.layout = layout;
        let saved = match &self.paths {
            Some(paths) => {
                let mut s = byob_store::Settings::load(&paths.settings()).value;
                s.layout = self.layout.clone();
                s.save(&paths.settings()).map_err(|e| format!("couldn't save the layout: {e}"))
            }
            None => Ok(()),
        };
        let ids: Vec<ClientId> = self.clients.iter().filter(|(_, c)| c.attached && c.caps.iter().any(|c| c == CAP_LAYOUT)).map(|(id, _)| *id).collect();
        for id in ids {
            self.send_to(id, ServerMsg::Layout(self.layout.clone()));
        }
        saved
    }

    /// `ClientMsg::SetLayout`.
    pub(crate) fn client_set_layout(&mut self, client: ClientId, layout: Layout) {
        if let Err(e) = self.set_layout(layout) {
            self.notice(client, e);
        }
    }

    /// `Input::Layout` (the tool): apply `change`, answer with the layout now (or why not).
    pub(crate) fn tool_layout(&mut self, change: Change, reply: &Sender<Result<Layout, String>>) {
        let result = match change {
            Some(l) => self.set_layout(l).map(|()| self.layout.clone()),
            None => Ok(self.layout.clone()),
        };
        let _ = reply.send(result);
    }
}

/// Ask the engine for the layout, changing it first if `change` is set.
fn request(inbox: &Sender<Input>, change: Change, cancel: &AtomicBool) -> Result<Layout, String> {
    let (reply, answer) = channel();
    inbox.send(Input::Layout { change, reply }).map_err(|_| "byob is shutting down".to_owned())?;
    let started = Instant::now();
    loop {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("stopped".into());
        }
        match answer.recv_timeout(Duration::from_millis(50)) {
            Ok(r) => return r,
            Err(RecvTimeoutError::Timeout) if started.elapsed() < TIMEOUT => {}
            Err(_) => return Err("the layout isn't available here".into()),
        }
    }
}

/// A plugin view the layout can place: its id (`view:<plugin>/<view>`) and a description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PluginView {
    pub id: String,
    pub describe: String,
}

/// The `status` views of the chat's plugins that are on: they sit in the toolbar.
fn plugin_views(inbox: &Sender<Input>, ctx: &ToolContext<'_>) -> Vec<PluginView> {
    let Some(chat) = ctx.chat() else { return Vec::new() };
    let Ok(reply) = plugins::request(inbox, chat, plugins::PluginOp::Status, ctx.cancel_flag()) else { return Vec::new() };
    let mut out = Vec::new();
    for p in reply.infos.iter().filter(|p| p.enabled && p.shadowed_by.is_none()) {
        for v in p.views.iter().filter(|v| v.site == Site::Status) {
            let title = if v.title.is_empty() { v.id.as_str() } else { v.title.as_str() };
            out.push(PluginView { id: view_id(&p.id, &v.id), describe: format!("{}: {title} (plugin {})", p.name, p.id) });
        }
    }
    out
}

/// The model's tool: read the layout, or change modes and orders.
pub(crate) struct UiLayout {
    pub inbox: Sender<Input>,
    /// The last layout this tool saw (for titles; the engine has the real one).
    seen: Mutex<Option<Layout>>,
    /// Layouts from before each change this tool made, newest last (for undo).
    history: Mutex<Vec<Layout>>,
}

/// Most layouts kept to undo to.
const HISTORY: usize = 20;

impl UiLayout {
    pub(crate) fn new(inbox: Sender<Input>) -> Arc<Self> {
        Arc::new(Self { inbox, seen: Mutex::new(None), history: Mutex::new(Vec::new()) })
    }
}

/// What a call asks for, checked.
#[derive(Debug, Default, PartialEq)]
struct Request {
    preset: Option<Layout>,
    reset: bool,
    /// Back to the layout before the tool's last change (the rest is ignored).
    undo: bool,
    show: Vec<(String, Show)>,
    order: Vec<(String, Vec<String>)>,
    /// What of the call was skipped, and why (the rest still applies).
    problems: Vec<String>,
}

/// The element `guess` most likely meant, by the words they share (`header.context_meter`
/// → `header.usage`, "Context meter").
fn closest(guess: &str, views: &[PluginView]) -> Option<String> {
    let words = |s: &str| -> Vec<String> { s.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 2).map(str::to_owned).collect() };
    let want = words(guess);
    let score = |text: String| want.iter().filter(|w| text.contains(w.as_str())).count();
    let mut best: Option<(usize, String)> = None;
    let candidates = ELEMENTS
        .iter()
        .map(|e| (e.id.to_owned(), format!("{} {} {}", e.id, e.label, e.describe).to_lowercase()))
        .chain(views.iter().map(|v| (v.id.clone(), format!("{} {}", v.id, v.describe).to_lowercase())));
    for (id, text) in candidates {
        let n = score(text);
        if n > 0 && best.as_ref().is_none_or(|(b, _)| n > *b) {
            best = Some((n, id));
        }
    }
    best.map(|(_, id)| id)
}

impl Request {
    fn is_read(&self) -> bool {
        !self.undo && self.preset.is_none() && !self.reset && self.show.is_empty() && self.order.is_empty()
    }

    /// Check the model's arguments against what exists (`views`: the plugin views there
    /// are). What doesn't fit is skipped with a reason; the rest goes ahead.
    fn parse(args: &Value, views: &[PluginView]) -> Self {
        let known = |id: &str| element(id).is_some() || id.starts_with("view:") && (views.is_empty() || views.iter().any(|v| v.id == id));
        let unknown = |id: &str| match closest(id, views) {
            Some(c) => format!("there's no {id:?}; did you mean {c}? (call ui_layout without arguments for the list)"),
            None => format!("there's no {id:?} (call ui_layout without arguments for the list)"),
        };
        let flag = |k: &str| args.get(k).and_then(Value::as_bool).unwrap_or(false);
        let mut r = Self { reset: flag("reset"), undo: flag("undo"), ..Self::default() };
        if r.undo {
            return r;
        }
        if let Some(p) = args.get("preset").and_then(Value::as_str).filter(|p| !p.trim().is_empty()) {
            let names: Vec<&str> = PRESETS.iter().map(|(n, _)| *n).collect();
            match Layout::preset(p) {
                Some(l) => r.preset = Some(l),
                None => r.problems.push(format!("no preset {p:?}; there are: {}", names.join(", "))),
            }
        }
        match args.get("show") {
            None | Some(Value::Null) => {}
            Some(Value::Object(map)) => {
                for (id, mode) in map {
                    if !known(id) {
                        r.problems.push(unknown(id));
                    } else if let Some(m) = mode.as_str().and_then(Show::parse) {
                        r.show.push((id.clone(), m));
                    } else {
                        r.problems.push(format!("{id}: the mode is \"shown\", \"hover\" or \"hidden\", not {mode}"));
                    }
                }
            }
            Some(_) => r.problems.push("show: an object of element id → \"shown\", \"hover\" or \"hidden\"".into()),
        }
        match args.get("order") {
            None | Some(Value::Null) => {}
            Some(Value::Object(map)) => {
                for (reg, ids) in map {
                    let Some(region) = region(reg) else {
                        let names: Vec<&str> = REGIONS.iter().map(|r| r.id).collect();
                        r.problems.push(format!("no region {reg:?}; there are: {}", names.join(", ")));
                        continue;
                    };
                    let ids: Vec<String> = match ids {
                        Value::Array(a) => a.iter().filter_map(Value::as_str).map(str::to_owned).collect(),
                        Value::String(s) => s.split([',', ' ']).filter(|s| !s.is_empty()).map(str::to_owned).collect(),
                        _ => {
                            r.problems.push(format!("order.{reg}: a list of element ids"));
                            continue;
                        }
                    };
                    let mut here = Vec::new();
                    for id in ids {
                        if element(&id).is_some_and(|e| e.region == Some(region.id)) || region.plugin_views && id.starts_with("view:") && known(&id) {
                            here.push(id);
                        } else if known(&id) {
                            r.problems.push(format!("{id} isn't in the {reg} region, so it can't be moved there"));
                        } else {
                            r.problems.push(unknown(&id));
                        }
                    }
                    if !here.is_empty() {
                        r.order.push((reg.clone(), here));
                    }
                }
            }
            Some(_) => r.problems.push("order: an object of region → element ids, left to right".into()),
        }
        r
    }

    /// `layout` with this applied: the reset or preset first, then the modes and orders.
    fn apply(&self, layout: &Layout) -> Layout {
        // A reset is about what's shown: the person's own chat list sections stay.
        let mut l = if self.reset { Layout { chat_list: layout.chat_list.clone(), ..Layout::default() } } else { layout.clone() };
        if let Some(p) = &self.preset {
            l.show.clone_from(&p.show);
        }
        for (id, mode) in &self.show {
            l.set_show(id, *mode);
        }
        for (reg, ids) in &self.order {
            l.set_order(reg, ids.clone());
        }
        l
    }
}

/// What differs between `before` and `after`, one line each (modes, then orders).
fn changes(before: &Layout, after: &Layout, views: &[PluginView]) -> Vec<String> {
    let view_ids: Vec<String> = views.iter().map(|v| v.id.clone()).collect();
    let mut ids: Vec<String> = ELEMENTS.iter().map(|e| e.id.to_owned()).chain(view_ids.iter().cloned()).collect();
    ids.extend(before.show.keys().chain(after.show.keys()).filter(|k| !ids.contains(k)).cloned().collect::<Vec<_>>());
    let mut out: Vec<String> = ids
        .iter()
        .filter_map(|id| {
            let (a, b) = (before.show(id), after.show(id));
            (a != b).then(|| format!("{id}: {} → {}", a.as_str(), b.as_str()))
        })
        .collect();
    for r in REGIONS {
        let extra: &[String] = if r.plugin_views { &view_ids } else { &[] };
        let (a, b) = (before.arrange(r.id, extra), after.arrange(r.id, extra));
        if a != b {
            out.push(format!("{} order: {}", r.id, b.join(", ")));
        }
    }
    out
}

/// The layout for the model: every element with its mode, by region, and the presets.
fn describe(layout: &Layout, views: &[PluginView]) -> String {
    let mut out = String::from("byob's layout. Modes: shown, hover (only while the pointer is on what it belongs to), hidden.\n");
    let view_ids: Vec<String> = views.iter().map(|v| v.id.clone()).collect();
    for r in REGIONS {
        let _ = writeln!(out, "\nRegion {} ({}), in order:", r.id, r.describe);
        let extra: &[String] = if r.plugin_views { &view_ids } else { &[] };
        for id in layout.arrange(r.id, extra) {
            let what =
                element(&id).map(|e| e.describe.to_owned()).or_else(|| views.iter().find(|v| v.id == id).map(|v| v.describe.clone())).unwrap_or_default();
            let _ = writeln!(out, "- {id}: {} — {what}", layout.show(&id).as_str());
        }
    }
    for (_, name) in GROUPS {
        let mut members = ELEMENTS.iter().filter(|e| e.region.is_none() && group(e.id) == *name).peekable();
        if members.peek().is_none() {
            continue;
        }
        let _ = writeln!(out, "\n{name} (not reorderable):");
        for e in members {
            let hover = if e.hover { "" } else { " (no hover mode)" };
            let _ = writeln!(out, "- {}: {} — {}{hover}", e.id, layout.show(e.id).as_str(), e.describe);
        }
    }
    out.push_str("\nPresets:\n");
    for (name, what) in PRESETS {
        let _ = writeln!(out, "- {name}: {what}");
    }
    out.push_str(
        "\nOnly these can be changed. If the user names something that isn't listed, tell them it can't be hidden or moved yet; don't change something else in its place.",
    );
    out
}

/// The ids a change touched, for the step's title.
fn touched(r: &Request) -> String {
    if r.undo {
        return "Layout: undid the last change".into();
    }
    if r.reset {
        return "Reset the layout".into();
    }
    let mut parts: Vec<String> = Vec::new();
    if let Some((name, _)) = PRESETS.iter().find(|(n, _)| r.preset.as_ref() == Layout::preset(n).as_ref()) {
        parts.push(format!("{name} preset"));
    }
    for (id, mode) in r.show.iter().take(3) {
        let name = element(id).map_or(id.as_str(), |e| e.label);
        parts.push(format!(
            "{} {name}",
            match mode {
                Show::Shown => "showed",
                Show::Hover => "hover for",
                Show::Hidden => "hid",
            }
        ));
    }
    if r.show.len() > 3 {
        parts.push(format!("{} more", r.show.len() - 3));
    }
    for (reg, _) in &r.order {
        parts.push(format!("reordered the {reg}"));
    }
    let mut s = parts.join(", ");
    if let Some(first) = s.get(..1) {
        s = first.to_uppercase() + s.get(1..).unwrap_or_default();
    }
    format!("Layout: {s}")
}

impl Tool for UiLayout {
    fn spec(&self) -> ToolSpec {
        let regions: Vec<&str> = REGIONS.iter().map(|r| r.id).collect();
        let presets: Vec<&str> = PRESETS.iter().map(|(n, _)| *n).collect();
        ToolSpec {
            name: TOOL.into(),
            description: "Show, hide or reorder parts of byob's own interface (the details under replies, the header's model chip and context meter, labels, shortcut hints, plugins' header items). Without arguments: every element with its id and mode. The change shows in every byob window at once and is kept; the result lists what changed and anything it skipped.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "show": {"type": "object", "description": "Element id → \"shown\", \"hover\" (only while the pointer is on it) or \"hidden\", e.g. {\"reply.tokens\": \"hidden\"}.", "additionalProperties": {"type": "string", "enum": ["shown", "hover", "hidden"]}},
                    "order": {"type": "object", "description": format!("Region ({}) → its element ids left to right; ones left out follow in their usual order.", regions.join(", ")), "additionalProperties": {"type": "array", "items": {"type": "string"}}},
                    "preset": {"type": "string", "enum": presets, "description": "Replace every element's mode with a ready-made layout's (earlier changes are lost; `show` in the same call applies on top)."},
                    "reset": {"type": "boolean", "description": "Back to byob's default layout first."},
                    "undo": {"type": "boolean", "description": "Put back the layout from before the last change you made (again to go further back). For \"undo that\" or \"put it back\"."}
                }
            }),
        }
    }

    fn title(&self, args: &Value) -> String {
        let r = Request::parse(args, &[]);
        if r.is_read() && r.problems.is_empty() {
            "Read the layout".into()
        } else if r.is_read() {
            "Change the layout".into()
        } else {
            touched(&r)
        }
    }

    fn pending_title(&self, args: &Value) -> String {
        let r = Request::parse(args, &[]);
        if r.is_read() && r.problems.is_empty() { "Read the layout".into() } else { "Change the layout".into() }
    }

    fn preview(&self, _args: &Value, _workspace: &Path) -> Option<byob_tools::Preview> {
        None
    }

    /// Only how byob looks changes, at once and visibly, and Settings → Appearance (or
    /// asking again) changes it back: nothing to approve.
    fn access(&self, _args: &Value) -> Access {
        Access::Read
    }

    /// What it reads changes (the user changes it too), and undo steps further back each time.
    fn repeatable(&self, _args: &Value) -> bool {
        true
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        let views = plugin_views(&self.inbox, ctx);
        let req = Request::parse(args, &views);
        let current = match request(&self.inbox, None, ctx.cancel_flag()) {
            Ok(l) => l,
            Err(e) => return ToolOutput::err(e),
        };
        if req.is_read() && req.problems.is_empty() {
            *self.seen.lock().unwrap_or_else(PoisonError::into_inner) = Some(current.clone());
            return ToolOutput::ok(describe(&current, &views));
        }
        let history = || self.history.lock().unwrap_or_else(PoisonError::into_inner);
        let want = if req.undo {
            let Some(before) = history().pop() else {
                return ToolOutput::err(
                    "there's no earlier layout to go back to (only changes made with ui_layout are kept); reset: true goes back to byob's default",
                );
            };
            before
        } else {
            req.apply(&current)
        };
        let layout = if req.is_read() {
            current.clone()
        } else {
            match request(&self.inbox, Some(want), ctx.cancel_flag()) {
                Ok(l) => l,
                Err(e) => return ToolOutput::err(e),
            }
        };
        if !req.undo && layout != current {
            let mut h = history();
            h.push(current.clone());
            if h.len() > HISTORY {
                h.remove(0);
            }
        }
        *self.seen.lock().unwrap_or_else(PoisonError::into_inner) = Some(layout.clone());
        let changed = changes(&current, &layout, &views);
        let mut out = String::new();
        if changed.is_empty() {
            out.push_str("Nothing changed.\n");
        } else {
            out.push_str("Changed in every byob window:\n");
            for c in &changed {
                let _ = writeln!(out, "- {c}");
            }
        }
        if !req.problems.is_empty() {
            out.push_str("Not applied:\n");
            for p in &req.problems {
                let _ = writeln!(out, "- {p}");
            }
        }
        if req.preset.is_some() || req.reset {
            out.push_str(
                "(A preset or reset sets every element's mode, so earlier changes it covers are undone; add them again in `show` if they should stay.)\n",
            );
        }
        if changed.is_empty() && !req.problems.is_empty() { ToolOutput::err(out.trim_end().to_owned()) } else { ToolOutput::ok(out.trim_end().to_owned()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn views() -> Vec<PluginView> {
        vec![PluginView { id: "view:todo/count".into(), describe: "To-do: count (plugin todo)".into() }]
    }

    #[test]
    fn requests_are_checked_against_what_exists() {
        let r = Request::parse(&json!({"show": {"reply.tokens": "hide", "view:todo/count": "hover"}}), &views());
        assert_eq!(r.show, [("reply.tokens".to_owned(), Show::Hidden), ("view:todo/count".to_owned(), Show::Hover)]);
        assert!(r.problems.is_empty());
        let problem = |args: Value| Request::parse(&args, &views()).problems.join("\n");
        assert!(problem(json!({"show": {"reply.colour": "hidden"}})).contains("there's no \"reply.colour\""));
        assert!(problem(json!({"show": {"reply.time": "sometimes"}})).contains("mode"));
        assert!(problem(json!({"show": {"view:gone/x": "hidden"}})).contains("view:gone/x"));
        assert!(problem(json!({"order": {"footer": []}})).contains("no region"));
        assert!(problem(json!({"order": {"reply": ["header.model"]}})).contains("isn't in the reply region"));
        assert!(problem(json!({"order": {"reply": ["view:todo/count"]}})).contains("isn't in the reply region"), "plugin views only go in the toolbar");
        assert!(problem(json!({"preset": "sparkly"})).contains("minimal"));
        assert!(Request::parse(&json!({}), &views()).is_read());
    }

    /// Regression: one guessed id ("header.context_meter") threw away the whole call. The
    /// guesses are skipped with the likely element; the rest applies.
    #[test]
    fn guessed_ids_are_skipped_with_a_suggestion_and_the_rest_applies() {
        let r = Request::parse(
            &json!({"show": {"header.context_meter": "hidden", "reply.role_names": "hidden", "reply.undo": "hover", "code.copy": "hidden"}}),
            &views(),
        );
        assert_eq!(r.show, [("code.copy".to_owned(), Show::Hidden)]);
        let p = r.problems.join("\n");
        assert!(p.contains("did you mean header.usage?"), "{p}");
        assert!(p.contains("did you mean message.undo?"), "{p}");
        assert!(p.contains("\"reply.role_names\""), "{p}");
        let after = r.apply(&Layout::default());
        assert_eq!(changes(&Layout::default(), &after, &views()), ["code.copy: shown → hidden"]);
        // A preset sets every mode: what it undoes shows in the changes.
        let mut mine = Layout::default();
        mine.set_show("message.steps", Show::Hidden);
        let after = Request::parse(&json!({"preset": "everything"}), &[]).apply(&mine);
        assert_eq!(changes(&mine, &after, &[]), ["message.steps: hidden → shown"]);
    }

    #[test]
    fn a_change_applies_on_top_of_a_preset_and_reset() {
        let mut current = Layout::default();
        current.set_show("composer.keys", Show::Hidden);
        let r = Request::parse(
            &json!({"reset": true, "preset": "minimal", "show": {"reply.tokens": "shown"}, "order": {"reply": "reply.time, reply.tokens"}}),
            &[],
        );
        let l = r.apply(&current);
        assert_eq!(l.show("reply.tokens"), Show::Shown);
        assert_eq!(l.show("message.role"), Show::Hidden, "from the preset");
        assert_eq!(l.arrange("reply", &[]), ["reply.time", "reply.tokens", "reply.speed"]);
        assert_eq!(touched(&r), "Reset the layout");
        let r = Request::parse(&json!({"show": {"reply.tokens": "hidden"}, "order": {"header": ["header.usage"]}}), &[]);
        assert_eq!(touched(&r), "Layout: Hid Tokens, reordered the header");
    }

    #[test]
    fn the_description_lists_every_element_and_plugin_view_with_its_mode() {
        let mut l = Layout::default();
        l.set_show("view:todo/count", Show::Hidden);
        let text = describe(&l, &views());
        for e in ELEMENTS {
            assert!(text.contains(&format!("- {}: ", e.id)), "{}", e.id);
        }
        assert!(text.contains("- view:todo/count: hidden — To-do: count"));
        assert!(text.contains("- reply.speed: hidden"));
        assert!(text.contains("minimal:"));
    }
}
