//! The turn pipeline's hooks: plugins sit between a turn's stages as middleware.
//!
//! A turn reaches five hook points, always between rounds or calls, never in the middle of
//! a tool call:
//!
//! | event | when | a hook may |
//! |---|---|---|
//! | `before_prompt` | once, before a top-level turn's first request | add, change or drop system-prompt sections |
//! | `before_request` | before every model request (each round) | change the messages and the tools offered, or stop the turn (compaction lives here) |
//! | `before_tool_call` | before the permission gate of each call | change the arguments; allow (skip the gate), deny with a message, or ask a person |
//! | `after_tool_call` | after a call, before the model sees its result | change the result text, attach a card (a view tree) to the step |
//! | `on_turn_end` | after the turn ended | observe (the answer, the error) |
//!
//! A hook is **native** (in-tree Rust registered in [`crate::Natives::hooks`] by id, named by a
//! manifest's `hooks[].native`) or answered by the plugin's **backend** (a `hook` request
//! over its JSON lines, with the manifest's `timeout_ms`; `fail: "closed"` turns a fault or
//! timeout into a stop/deny, the default carries on). Hooks run by `priority` (lower first),
//! then precedence (chat > workspace > global > bundled), then plugin id. Each returns
//! continue, modify (in place for natives; fields of the reply for backends) or stop; for
//! `before_tool_call` the first allow/deny/ask decides and later hooks don't run.
//!
//! A native hook no plugin the chat sees names keeps its default place (engines without
//! plugins, tests); one that some plugin names runs only while one of those plugins is on.
//! Backend faults and timeouts go into the plugin's status (`plugin_status` shows the model).

use std::any::Any;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use byob_llm::{ChatMessage, ToolSpec, Usage};
use byob_plugins::Backend;
use byob_protocol::{ToolAccess, View};
use serde_json::{Value, json};

use crate::adapt;
use crate::turn::{AutoCompact, TurnEvent};

/// A hook point (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Event {
    BeforePrompt,
    BeforeRequest,
    BeforeToolCall,
    AfterToolCall,
    OnTurnEnd,
}

impl Event {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "before_prompt" => Self::BeforePrompt,
            "before_request" => Self::BeforeRequest,
            "before_tool_call" => Self::BeforeToolCall,
            "after_tool_call" => Self::AfterToolCall,
            "on_turn_end" => Self::OnTurnEnd,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::BeforePrompt => "before_prompt",
            Self::BeforeRequest => "before_request",
            Self::BeforeToolCall => "before_tool_call",
            Self::AfterToolCall => "after_tool_call",
            Self::OnTurnEnd => "on_turn_end",
        }
    }
}

/// What a hook says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Flow {
    /// Go on (a native may have changed things in place).
    Continue,
    /// No later hooks; for `before_request` the turn ends with this reason.
    Stop(String),
}

/// A `before_tool_call` hook's decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Next hook, then the permission gate.
    Continue,
    /// Run without asking (the gate is skipped; a call that must be confirmed still asks).
    Allow,
    /// Don't run; the model gets this message.
    Deny(String),
    /// A person decides on an approval card, whatever the policy (`--auto-approve` too).
    Ask(String),
}

/// One section of the system prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub id: String,
    pub text: String,
}

impl Section {
    pub fn new(id: &str, text: impl Into<String>) -> Self {
        Self { id: id.to_owned(), text: text.into() }
    }
}

/// The system prompt: its non-blank sections, each trimmed, joined by blank lines.
pub fn join(sections: &[Section]) -> String {
    sections.iter().map(|s| s.text.trim()).filter(|t| !t.is_empty()).collect::<Vec<_>>().join("\n\n")
}

/// What `before_prompt` contributors build the prompt from (gathered on the engine thread).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptInputs {
    pub chat: String,
    pub workspace: PathBuf,
    /// The engine's system prompt (`EngineConfig::system_prompt`).
    pub base: String,
    /// Switched-on plugins' `system_prompt` sections (artifacts, code running, …).
    pub plugin_sections: String,
    /// Plugin prompt lines, the skills list and the authoring line.
    pub plugin_lines: String,
    /// The workspace's AGENTS.md section, if it has one.
    pub agents_md: Option<String>,
    /// The user's instructions of every settings level.
    pub instructions: String,
    /// The subagents section, when the turn may delegate.
    pub delegation: Option<String>,
    /// The chat's artifacts by id.
    pub artifacts: String,
    /// What the chat's web surfaces said with `ui/update-model-context` (a section, or empty).
    pub app_context: String,
}

/// What a `before_request` hook sees (natives get the turn's internals).
pub struct RequestCtx<'a> {
    pub entry: u64,
    /// Round of the tool loop (0: the first request).
    pub round: usize,
    /// A subagent's turn.
    pub child: bool,
    pub chat: &'a str,
    pub workspace: &'a Path,
    pub messages: &'a mut Vec<ChatMessage>,
    pub specs: &'a mut Vec<ToolSpec>,
    pub llm: &'a byob_llm::Client,
    pub plan: &'a adapt::Plan,
    pub cancel: &'a AtomicBool,
    /// The entry each of the turn's first messages came from (see `compact::history`);
    /// empty once a hook replaced the messages (a native that does should empty it too).
    pub ids: &'a [u64],
    /// The chat's last entry before this turn.
    pub prior: u64,
    /// The chat's automatic compaction settings (`None`: off).
    pub compact: Option<AutoCompact>,
    /// The turn's token use and model requests (a hook that calls the model adds to them).
    pub usage: &'a mut Usage,
    pub model_calls: &'a mut u64,
    /// Per-turn state of each native hook, by hook id (`entry(id).or_insert_with(…)`, then
    /// `downcast_mut`).
    pub state: &'a mut BTreeMap<String, Box<dyn Any + Send>>,
    pub emit: &'a dyn Fn(TurnEvent),
}

/// A tool call as `before_tool_call` sees it.
pub struct ToolCallCtx<'a> {
    pub tool: &'a str,
    pub args: &'a mut Value,
    pub access: ToolAccess,
    pub chat: &'a str,
    pub workspace: &'a Path,
    pub child: bool,
}

/// A finished call as `after_tool_call` sees it.
pub struct AfterCtx<'a> {
    pub tool: &'a str,
    pub args: &'a Value,
    /// What the model will get.
    pub result: &'a mut String,
    pub failed: bool,
    /// The step's views (a hook may attach one).
    pub views: &'a mut Vec<View>,
    pub chat: &'a str,
    pub workspace: &'a Path,
    pub child: bool,
}

/// How a turn ended, for `on_turn_end`.
pub struct TurnEndCtx<'a> {
    pub entry: u64,
    pub chat: &'a str,
    pub workspace: &'a Path,
    pub answer: &'a str,
    pub error: Option<&'a str>,
    pub stopped: bool,
}

/// In-tree code at hook points; implement the events it is registered for.
pub trait NativeHook: Send + Sync {
    fn before_prompt(&self, _inputs: &PromptInputs, _sections: &mut Vec<Section>) -> Flow {
        Flow::Continue
    }
    fn before_request(&self, _ctx: &mut RequestCtx<'_>) -> Flow {
        Flow::Continue
    }
    fn before_tool_call(&self, _ctx: &mut ToolCallCtx<'_>) -> Decision {
        Decision::Continue
    }
    fn after_tool_call(&self, _ctx: &mut AfterCtx<'_>) -> Flow {
        Flow::Continue
    }
    fn on_turn_end(&self, _ctx: &TurnEndCtx<'_>) {}
}

/// A native hook and where it sits when no plugin names it.
#[derive(Clone)]
pub struct NativeHookReg {
    pub hook: Arc<dyn NativeHook>,
    /// (event, priority) of each default binding.
    pub defaults: Vec<(Event, i64)>,
}

/// Who answers a binding.
#[derive(Clone)]
pub(crate) enum Runner {
    Native(Arc<dyn NativeHook>),
    Backend { backend: Arc<Backend>, timeout: Duration, fail_closed: bool },
}

/// One hook in a chat's pipeline.
#[derive(Clone)]
pub(crate) struct Binding {
    /// The plugin that declared it (`byob` for a native nobody names).
    pub plugin: String,
    /// A native's id, or `backend`.
    pub id: String,
    pub event: Event,
    pub priority: i64,
    /// Precedence: 0 chat, 1 workspace, 2 global, 3 bundled, 4 byob's own default.
    pub rank: u8,
    /// Only these tools (tool events; empty: all).
    pub tools: Vec<String>,
    pub run: Runner,
}

impl Binding {
    fn applies_to(&self, tool: &str) -> bool {
        self.tools.is_empty() || self.tools.iter().any(|t| t == tool)
    }

    fn native(&self) -> Option<&dyn NativeHook> {
        match &self.run {
            Runner::Native(h) => Some(h.as_ref()),
            Runner::Backend { .. } => None,
        }
    }
}

/// A chat's hooks in the order they run.
#[derive(Clone, Default)]
pub struct Hooks(Arc<Vec<Binding>>);

impl std::fmt::Debug for Hooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.0.iter().map(|b| format!("{}:{}@{}:{}", b.plugin, b.id, b.event.name(), b.priority))).finish()
    }
}

/// What a backend hook answered.
enum Reply {
    Continue,
    Modify(serde_json::Map<String, Value>),
    Stop(String),
    Allow,
    Deny(String),
    Ask(String),
    /// A fail-closed hook's backend faulted or timed out (why).
    Failed(String),
}

/// Longest message list sent to a backend hook (bytes of JSON); longer lists go as `null`.
const MAX_MESSAGES_JSON: usize = 512 * 1024;

impl Hooks {
    pub(crate) fn new(mut bindings: Vec<Binding>) -> Self {
        bindings.sort_by(|a, b| (a.priority, a.rank, &a.plugin).cmp(&(b.priority, b.rank, &b.plugin)));
        Self(Arc::new(bindings))
    }

    /// byob's native hooks in their default places (no plugins).
    pub fn defaults(natives: &BTreeMap<String, NativeHookReg>) -> Self {
        Self::new(default_bindings(natives, |_| true))
    }

    fn of(&self, event: Event) -> impl Iterator<Item = &Binding> {
        self.0.iter().filter(move |b| b.event == event)
    }

    /// `(plugin, id, event)` of every hook, in order (tests, status).
    pub fn describe(&self) -> Vec<(String, String, &'static str)> {
        self.0.iter().map(|b| (b.plugin.clone(), b.id.clone(), b.event.name())).collect()
    }

    /// The system prompt from native contributors only (the engine thread's estimates).
    pub fn native_prompt(&self, inputs: &PromptInputs) -> String {
        let mut sections = Vec::new();
        for b in self.of(Event::BeforePrompt) {
            if let Some(h) = b.native()
                && let Flow::Stop(_) = guard(|| h.before_prompt(inputs, &mut sections), Flow::Continue)
            {
                break;
            }
        }
        join(&sections)
    }

    /// The system prompt: every `before_prompt` hook in order. `Err` (why) if a fail-closed
    /// hook failed: the turn must not run without what it would have added.
    pub fn before_prompt(&self, inputs: &PromptInputs, cancel: &AtomicBool) -> Result<String, String> {
        let mut sections: Vec<Section> = Vec::new();
        for b in self.of(Event::BeforePrompt) {
            let flow = match &b.run {
                Runner::Native(h) => guard(|| h.before_prompt(inputs, &mut sections), Flow::Continue),
                Runner::Backend { .. } => {
                    let payload = json!({
                        "chat": inputs.chat,
                        "workspace": inputs.workspace.display().to_string(),
                        "sections": sections.iter().map(|s| json!({"id": s.id, "text": s.text})).collect::<Vec<_>>(),
                    });
                    match self.ask(b, payload, cancel) {
                        Reply::Modify(m) => {
                            apply_sections(b, &m, &mut sections);
                            Flow::Continue
                        }
                        Reply::Stop(r) => Flow::Stop(r),
                        Reply::Failed(why) => return Err(why),
                        _ => Flow::Continue,
                    }
                }
            };
            if let Flow::Stop(_) = flow {
                break;
            }
        }
        Ok(join(&sections))
    }

    /// Run `before_request` hooks; `Stop(reason)` ends the turn.
    pub fn before_request(&self, ctx: &mut RequestCtx<'_>) -> Flow {
        for b in self.of(Event::BeforeRequest) {
            let flow = match &b.run {
                Runner::Native(h) => guard(|| h.before_request(ctx), Flow::Continue),
                Runner::Backend { .. } => {
                    let messages = serde_json::to_value(&*ctx.messages).ok().filter(|v| v.to_string().len() <= MAX_MESSAGES_JSON).unwrap_or(Value::Null);
                    let payload = json!({
                        "entry": ctx.entry,
                        "round": ctx.round,
                        "child": ctx.child,
                        "messages": messages,
                        "tools": ctx.specs.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
                        "estimated_tokens": ctx.plan.raw_tokens(ctx.messages, ctx.specs),
                        "context_window": ctx.plan.context,
                    });
                    match self.ask(b, payload, ctx.cancel) {
                        Reply::Modify(m) => {
                            apply_request(b, &m, ctx);
                            Flow::Continue
                        }
                        Reply::Stop(r) | Reply::Failed(r) => Flow::Stop(r),
                        _ => Flow::Continue,
                    }
                }
            };
            if let Flow::Stop(r) = flow {
                return Flow::Stop(r);
            }
        }
        Flow::Continue
    }

    /// Run `before_tool_call` hooks until one decides.
    pub fn before_tool_call(&self, ctx: &mut ToolCallCtx<'_>, cancel: &AtomicBool) -> Decision {
        for b in self.of(Event::BeforeToolCall).filter(|b| b.applies_to(ctx.tool)) {
            let decision = match &b.run {
                Runner::Native(h) => guard(|| h.before_tool_call(ctx), Decision::Continue),
                Runner::Backend { fail_closed, .. } => {
                    let payload = json!({
                        "chat": ctx.chat,
                        "workspace": ctx.workspace.display().to_string(),
                        "tool": ctx.tool,
                        "args": ctx.args,
                        "access": ctx.access.label(),
                        "child": ctx.child,
                    });
                    match self.ask(b, payload, cancel) {
                        Reply::Continue => Decision::Continue,
                        Reply::Modify(m) => {
                            match m.get("args") {
                                Some(a @ Value::Object(_)) => *ctx.args = a.clone(),
                                Some(_) => report(b, "before_tool_call: \"args\" must be an object; the call keeps its arguments"),
                                None => {}
                            }
                            Decision::Continue
                        }
                        Reply::Allow => Decision::Allow,
                        Reply::Deny(m) => Decision::Deny(m),
                        Reply::Ask(m) => Decision::Ask(m),
                        Reply::Failed(r) => Decision::Deny(r),
                        Reply::Stop(r) if *fail_closed || !r.is_empty() => Decision::Deny(r),
                        Reply::Stop(_) => Decision::Deny(format!("the {} plugin stopped this call", b.plugin)),
                    }
                }
            };
            if decision != Decision::Continue {
                return decision;
            }
        }
        Decision::Continue
    }

    /// Run `after_tool_call` hooks.
    pub fn after_tool_call(&self, ctx: &mut AfterCtx<'_>, cancel: &AtomicBool) {
        for b in self.of(Event::AfterToolCall).filter(|b| b.applies_to(ctx.tool)) {
            let flow = match &b.run {
                Runner::Native(h) => guard(|| h.after_tool_call(ctx), Flow::Continue),
                Runner::Backend { .. } => {
                    let payload = json!({
                        "chat": ctx.chat,
                        "workspace": ctx.workspace.display().to_string(),
                        "tool": ctx.tool,
                        "args": ctx.args,
                        "result": ctx.result,
                        "failed": ctx.failed,
                        "child": ctx.child,
                    });
                    match self.ask(b, payload, cancel) {
                        Reply::Modify(m) => {
                            apply_after(b, &m, ctx);
                            Flow::Continue
                        }
                        Reply::Stop(r) | Reply::Failed(r) => Flow::Stop(r),
                        _ => Flow::Continue,
                    }
                }
            };
            if let Flow::Stop(_) = flow {
                break;
            }
        }
    }

    /// Run `on_turn_end` hooks (observers: their answers are ignored).
    pub fn on_turn_end(&self, ctx: &TurnEndCtx<'_>, cancel: &AtomicBool) {
        for b in self.of(Event::OnTurnEnd) {
            match &b.run {
                Runner::Native(h) => guard(|| h.on_turn_end(ctx), ()),
                Runner::Backend { .. } => {
                    let payload = json!({
                        "entry": ctx.entry,
                        "chat": ctx.chat,
                        "workspace": ctx.workspace.display().to_string(),
                        "answer": ctx.answer,
                        "error": ctx.error,
                        "stopped": ctx.stopped,
                    });
                    let _ = self.ask(b, payload, cancel);
                }
            }
        }
    }

    /// A `hook` request to `b`'s backend. Faults and timeouts are reported to its status and
    /// become `Continue` (fail open) or `Stop` (fail closed).
    fn ask(&self, b: &Binding, mut payload: Value, cancel: &AtomicBool) -> Reply {
        let Runner::Backend { backend, timeout, fail_closed } = &b.run else { return Reply::Continue };
        if let Some(o) = payload.as_object_mut() {
            o.insert("event".into(), json!(b.event.name()));
        }
        let failed = |why: String| {
            report(b, &format!("{} hook: {why}", b.event.name()));
            if *fail_closed { Reply::Failed(format!("the {} plugin's {} hook failed ({why})", b.plugin, b.event.name())) } else { Reply::Continue }
        };
        match backend.request("hook", payload, *timeout, &|| cancel.load(std::sync::atomic::Ordering::Relaxed)) {
            Ok(v) => match parse_reply(&v) {
                Ok(r) => r,
                Err(e) => failed(e),
            },
            Err(e) if e.starts_with("Stopped by the user") => Reply::Stop(e),
            Err(e) => failed(e),
        }
    }
}

/// Report a problem with what a plugin's hook did to its backend's status.
fn report(b: &Binding, text: &str) {
    if let Runner::Backend { backend, .. } = &b.run {
        backend.report_fault(text);
    }
    log::warn!("plugin {}: {text}", b.plugin);
}

/// A native hook is in-tree but still must not take the turn down.
fn guard<T>(f: impl FnOnce() -> T, fallback: T) -> T {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|_| {
        log::error!("a native hook panicked; carrying on without it");
        fallback
    })
}

/// `null`, `"continue"`, `{}`, `{"action": "continue" | "modify" | "stop" | "allow" | "deny" | "ask", …}`.
fn parse_reply(v: &Value) -> Result<Reply, String> {
    let (action, o) = match v {
        Value::Null => return Ok(Reply::Continue),
        Value::String(s) => (s.as_str(), None),
        Value::Object(o) => (o.get("action").and_then(Value::as_str).unwrap_or(if o.is_empty() { "continue" } else { "modify" }), Some(o)),
        other => return Err(format!("the reply must be an object like {{\"action\": \"continue\"}}, not {other}")),
    };
    let text = |key: &str| o.and_then(|o| o.get(key)).and_then(Value::as_str).map(str::trim).unwrap_or_default().to_owned();
    let message = || {
        let m = text("message");
        if m.is_empty() { text("reason") } else { m }
    };
    Ok(match action {
        "continue" => Reply::Continue,
        "modify" => Reply::Modify(o.cloned().unwrap_or_default()),
        "stop" => Reply::Stop(message()),
        "allow" => Reply::Allow,
        "deny" => Reply::Deny({
            let m = message();
            if m.is_empty() { "a plugin refused this call".into() } else { m }
        }),
        "ask" => Reply::Ask(message()),
        other => return Err(format!("unknown action {other:?} (continue, modify, stop, allow, deny, ask)")),
    })
}

/// `before_prompt` modify: `sections` (the whole list) and/or `add` (texts or `{id, text}`).
fn apply_sections(b: &Binding, m: &serde_json::Map<String, Value>, sections: &mut Vec<Section>) {
    let one = |v: &Value, i: usize| -> Option<Section> {
        match v {
            Value::String(t) => Some(Section { id: format!("{}.{i}", b.plugin), text: t.clone() }),
            Value::Object(o) => Some(Section {
                id: o.get("id").and_then(Value::as_str).map_or_else(|| format!("{}.{i}", b.plugin), str::to_owned),
                text: o.get("text").and_then(Value::as_str)?.to_owned(),
            }),
            _ => None,
        }
    };
    if let Some(list) = m.get("sections") {
        match list.as_array() {
            Some(items) => *sections = items.iter().enumerate().filter_map(|(i, v)| one(v, i)).collect(),
            None => report(b, "before_prompt: \"sections\" must be a list of {id, text}"),
        }
    }
    if let Some(add) = m.get("add") {
        let items: Vec<Value> = match add {
            Value::Array(a) => a.clone(),
            v => vec![v.clone()],
        };
        let at = sections.len();
        sections.extend(items.iter().enumerate().filter_map(|(i, v)| one(v, at + i)));
    }
    let total: usize = sections.iter().map(|s| s.text.len()).sum();
    if total > MAX_PROMPT_BYTES {
        report(b, &format!("before_prompt: the system prompt grew past {} KB; later sections were dropped", MAX_PROMPT_BYTES / 1024));
        let mut used = 0;
        sections.retain(|s| {
            used += s.text.len();
            used <= MAX_PROMPT_BYTES
        });
    }
}

/// Longest system prompt hooks may build (bytes).
const MAX_PROMPT_BYTES: usize = 256 * 1024;

/// `before_request` modify: `messages` (the whole list) and/or `tools` (names to keep).
fn apply_request(b: &Binding, m: &serde_json::Map<String, Value>, ctx: &mut RequestCtx<'_>) {
    if let Some(v) = m.get("messages").filter(|v| !v.is_null()) {
        match serde_json::from_value::<Vec<ChatMessage>>(v.clone()) {
            Ok(list) if !list.is_empty() => {
                *ctx.messages = list;
                // Which entry each message came from is unknown now (a compaction then
                // covers none of the stored entries rather than the wrong ones).
                ctx.ids = &[];
            }
            Ok(_) => report(b, "before_request: \"messages\" can't be empty; the request keeps its messages"),
            Err(e) => report(b, &format!("before_request: \"messages\" isn't a list of messages ({e}); the request keeps its messages")),
        }
    }
    if let Some(v) = m.get("tools") {
        match v.as_array() {
            Some(names) => {
                let keep: Vec<&str> = names.iter().filter_map(Value::as_str).collect();
                ctx.specs.retain(|s| keep.contains(&s.name.as_str()));
            }
            None => report(b, "before_request: \"tools\" must be a list of tool names"),
        }
    }
}

/// `after_tool_call` modify: `result` (new text) and/or `card` (a view tree for the step).
fn apply_after(b: &Binding, m: &serde_json::Map<String, Value>, ctx: &mut AfterCtx<'_>) {
    // `null` (Python's `modify(result=None, card=…)`) leaves the result as it is.
    match m.get("result") {
        None | Some(Value::Null) => {}
        Some(Value::String(s)) => s.clone_into(ctx.result),
        Some(other) => *ctx.result = other.to_string(),
    }
    if let Some(card) = m.get("card").filter(|c| c.is_object()) {
        if ctx.views.len() >= byob_protocol::view::MAX_STEP_VIEWS {
            report(b, "after_tool_call: the step has as many views as it may");
            return;
        }
        let (tree, faults) = byob_protocol::tree::decode(card.clone());
        for f in faults.iter().take(8) {
            report(b, &format!("after_tool_call card: {f}"));
        }
        ctx.views.push(tree.to_view());
    }
}

/// The default bindings of the natives `keep` lets through.
pub(crate) fn default_bindings(natives: &BTreeMap<String, NativeHookReg>, keep: impl Fn(&str) -> bool) -> Vec<Binding> {
    let mut out = Vec::new();
    for (id, reg) in natives.iter().filter(|(id, _)| keep(id)) {
        for (event, priority) in &reg.defaults {
            out.push(Binding {
                plugin: "byob".into(),
                id: id.clone(),
                event: *event,
                priority: *priority,
                rank: 4,
                tools: Vec::new(),
                run: Runner::Native(Arc::clone(&reg.hook)),
            });
        }
    }
    out
}

// ---- byob's own prompt contributors -----------------------------------------------------

/// A `before_prompt` native that appends what `pick` takes from the inputs.
pub(crate) struct PromptPart {
    pub id: &'static str,
    pub pick: fn(&PromptInputs) -> Vec<String>,
}

impl NativeHook for PromptPart {
    fn before_prompt(&self, inputs: &PromptInputs, sections: &mut Vec<Section>) -> Flow {
        for (i, text) in (self.pick)(inputs).into_iter().enumerate() {
            let id = if i == 0 { self.id.to_owned() } else { format!("{}.{i}", self.id) };
            sections.push(Section { id, text });
        }
        Flow::Continue
    }
}

/// `Today is Wednesday, 2026-11-11.` (local time; the prompt is built once per turn).
pub(crate) fn today() -> String {
    chrono::Local::now().format("Today is %A, %Y-%m-%d.").to_string()
}

/// The prompt contributors byob registers, `(id, default priority, pick)`, in prompt order:
/// the base prompt, the plugins' sections and lines, the workspace's AGENTS.md, the user's
/// instructions, the subagents section, the artifact list, the web surfaces' context, the date.
pub(crate) fn prompt_parts() -> Vec<(&'static str, i64, fn(&PromptInputs) -> Vec<String>)> {
    vec![
        ("base_prompt", 0, |i| vec![i.base.clone()]),
        ("plugin_prompts", 100, |i| vec![i.plugin_sections.clone(), i.plugin_lines.clone()]),
        ("agents_md", 300, |i| i.agents_md.clone().into_iter().collect()),
        ("instructions", 400, |i| if i.instructions.is_empty() { vec![] } else { vec![format!("The user's instructions for this chat:\n{}", i.instructions)] }),
        ("subagents", 500, |i| i.delegation.clone().into_iter().collect()),
        ("artifact_list", 600, |i| vec![i.artifacts.clone()]),
        ("app_context", 650, |i| if i.app_context.is_empty() { vec![] } else { vec![i.app_context.clone()] }),
        // models don't know the date, and "the latest X" needs it; last, so the rest of the
        // prompt stays the same from day to day
        ("date", 690, |_| vec![today()]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Push(&'static str, Flow);
    impl NativeHook for Push {
        fn before_prompt(&self, _: &PromptInputs, s: &mut Vec<Section>) -> Flow {
            s.push(Section::new(self.0, self.0));
            self.1.clone()
        }
    }

    fn native(plugin: &str, priority: i64, rank: u8, hook: Push) -> Binding {
        Binding { plugin: plugin.into(), id: hook.0.into(), event: Event::BeforePrompt, priority, rank, tools: vec![], run: Runner::Native(Arc::new(hook)) }
    }

    #[test]
    fn hooks_run_by_priority_then_precedence_and_stop_ends_the_chain() {
        let hooks = Hooks::new(vec![
            native("g", 10, 2, Push("global", Flow::Continue)),
            native("c", 10, 0, Push("chat", Flow::Continue)),
            native("b", 5, 3, Push("first", Flow::Continue)),
            native("z", 20, 0, Push("stops", Flow::Stop("enough".into()))),
            native("y", 30, 0, Push("never", Flow::Continue)),
        ]);
        let p = hooks.before_prompt(&PromptInputs::default(), &AtomicBool::new(false)).unwrap();
        assert_eq!(p, "first\n\nchat\n\nglobal\n\nstops");
        assert_eq!(hooks.native_prompt(&PromptInputs::default()), p);
    }

    #[test]
    fn backend_replies_parse_leniently() {
        assert!(matches!(parse_reply(&Value::Null), Ok(Reply::Continue)));
        assert!(matches!(parse_reply(&json!({})), Ok(Reply::Continue)));
        assert!(matches!(parse_reply(&json!("allow")), Ok(Reply::Allow)));
        assert!(matches!(parse_reply(&json!({"add": ["x"]})), Ok(Reply::Modify(_))));
        assert!(matches!(parse_reply(&json!({"action": "deny", "message": "no"})), Ok(Reply::Deny(m)) if m == "no"));
        assert!(matches!(parse_reply(&json!({"action": "deny"})), Ok(Reply::Deny(m)) if !m.is_empty()));
        assert!(matches!(parse_reply(&json!({"action": "stop", "reason": "r"})), Ok(Reply::Stop(r)) if r == "r"));
        assert!(parse_reply(&json!({"action": "explode"})).is_err());
        assert!(parse_reply(&json!([1])).is_err());
    }

    #[test]
    fn prompt_parts_join_like_the_old_concatenation() {
        let mut natives = BTreeMap::new();
        for (id, prio, pick) in prompt_parts() {
            natives.insert(id.to_owned(), NativeHookReg { hook: Arc::new(PromptPart { id, pick }), defaults: vec![(Event::BeforePrompt, prio)] });
        }
        let inputs = PromptInputs {
            base: "BASE".into(),
            plugin_sections: "SECTIONS".into(),
            plugin_lines: "LINES".into(),
            agents_md: Some("## AGENTS".into()),
            instructions: "be brief".into(),
            delegation: Some("## Subagents".into()),
            artifacts: String::new(),
            ..Default::default()
        };
        let p = Hooks::defaults(&natives).native_prompt(&inputs);
        assert_eq!(p, format!("BASE\n\nSECTIONS\n\nLINES\n\n## AGENTS\n\nThe user's instructions for this chat:\nbe brief\n\n## Subagents\n\n{}", today()));
        assert!(today().starts_with("Today is ") && today().len() >= "Today is Monday, 2026-11-11.".len(), "{}", today());
    }

    fn modify(v: Value) -> serde_json::Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn an_after_hook_with_a_null_result_keeps_the_result_and_adds_its_card() {
        let b = native("p", 0, 0, Push("x", Flow::Continue));
        let (mut result, mut views) = ("the tool's text".to_owned(), Vec::new());
        let mut ctx = AfterCtx {
            tool: "t",
            args: &Value::Null,
            result: &mut result,
            failed: false,
            views: &mut views,
            chat: "c",
            workspace: Path::new("/w"),
            child: false,
        };
        apply_after(&b, &modify(json!({"result": null, "card": {"type": "text", "text": "hi"}})), &mut ctx);
        apply_after(&b, &modify(json!({"card": {"type": "text", "text": "again"}})), &mut ctx);
        assert_eq!((result.as_str(), views.len()), ("the tool's text", 2));
    }

    #[test]
    fn replacing_the_messages_forgets_which_entries_they_came_from() {
        let b = native("p", 0, 0, Push("x", Flow::Continue));
        let llm = byob_llm::Client::new(byob_types::InferenceConfig::default()).unwrap();
        let plan = adapt::Plan { key: String::new(), prompt_tools: false, fallback: false, context: 1000, reserve: 0, vision: None, image_quality: 0 };
        let (mut messages, mut specs, mut usage, mut calls, mut state) =
            (vec![ChatMessage::system("s"), ChatMessage::user("u")], Vec::new(), Usage::default(), 0, BTreeMap::new());
        let ids = [0, 7];
        let mut ctx = RequestCtx {
            entry: 1,
            round: 0,
            child: false,
            chat: "c",
            workspace: Path::new("/w"),
            messages: &mut messages,
            specs: &mut specs,
            llm: &llm,
            plan: &plan,
            cancel: &AtomicBool::new(false),
            ids: &ids,
            prior: 7,
            compact: None,
            usage: &mut usage,
            model_calls: &mut calls,
            state: &mut state,
            emit: &|_| {},
        };
        apply_request(&b, &modify(json!({"tools": []})), &mut ctx);
        assert_eq!(ctx.ids, [0, 7], "only the tools changed");
        apply_request(&b, &modify(json!({"messages": [{"role": "user", "content": "other"}]})), &mut ctx);
        assert!(ctx.ids.is_empty());
    }
}
