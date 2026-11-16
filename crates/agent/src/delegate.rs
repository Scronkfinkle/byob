//! Subagents: the `delegate` tool.
//!
//! The main model calls `delegate { agent?, task, tools?, model? }` ([`DelegateTool`], an ordinary
//! concurrent tool the engine adds to a turn's tools while the bundled `subagents` plugin is
//! on); through the turn's [`crate::turn::TurnHost`] it starts a **child turn**
//! with a fresh context (the agent's system prompt and the task, not the chat's history)
//! that runs the normal tool loop with the same tools (or fewer), and returns the child's
//! final reply as the tool result. Children share the chat's workspace, artifacts,
//! permission policy, grants and sandbox; their approval cards appear in the chat. Several
//! delegations in one round run side by side, as many at once as the endpoint has free
//! slots ([`Slots`]) and, per saved model, as many as its `max_parallel`; the rest queue.
//! Children can't delegate (depth 1).
//!
//! The model a subagent runs on, first that applies ([`Env::pick_model`]): the call's `model`
//! (only profiles marked "offered to subagents", listed in the schema and the system prompt
//! with their descriptions), the agent file's `model:` (a role, a profile id or name, or a
//! model id on the chat's endpoint), the level's subagent model (chat, workspace, or the
//! `subagents` role), the chat's own model.
//!
//! Agent definitions are Claude Code compatible Markdown files (see [`byob_plugins::agents`]).

use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use byob_llm::{ChatMessage, ToolSpec};
use byob_plugins::agents::{AgentDef, load_agents_dir, merge_agents, tool_aliases};
use byob_protocol::{Delegation, DelegationState, Step, StepStatus, ToolAccess};
use byob_tools::Registry;
use byob_types::InferenceConfig;
use serde_json::{Value, json};

use crate::limits::{DETAIL_CHARS, one_line, prefix};
use crate::slot_leases::SlotTable;
use crate::turn::{self, TurnEvent};
use crate::{Input, adapt};

/// The tool's name.
pub const TOOL: &str = "delegate";
/// The agent used when the call names none.
pub const DEFAULT_AGENT: &str = "general";
/// Longest task text accepted, in bytes.
const MAX_TASK: usize = 16 * 1024;
/// Longest report returned to the parent (before the turn's own output caps).
const MAX_REPORT: usize = 12 * 1024;
/// Agents listed in the main system prompt.
const MAX_LISTED: usize = 40;
/// Most plugin guidance added to a subagent's system prompt, in bytes.
pub const MAX_GUIDANCE: usize = 4 * 1024;
/// How often a waiting subagent checks for a free slot / its watchdog checks the clock.
const POLL: Duration = Duration::from_millis(50);

/// The general-purpose worker's prompt.
pub const WORKER_PROMPT: &str = "You are a capable general-purpose worker. Complete the task you are given using the tools available: \
look things up rather than guessing, and keep going until the task is done or clearly blocked.";

/// Added to every subagent's system prompt.
const SUBAGENT_RULES: &str = "You are running as a subagent inside byob, on behalf of the main assistant. Nobody reads your messages \
as they stream: your final reply is returned to the main assistant as your report. Work only on the task below; don't ask \
questions (if something blocks you, say so in the report). Finish with a concise report: one or two lines on what you did, then \
the result, with the specific facts, numbers, file names and quotes the main assistant needs.";

/// Subagent settings of an engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubagentOptions {
    /// Offer the `delegate` tool to the main model.
    pub enabled: bool,
    /// Most subagents running at once (all chats). `None`: the endpoint's slots minus one
    /// for the main thread (2 slots assumed when it doesn't say).
    pub max_parallel: Option<usize>,
    /// Most tool calls of one subagent.
    pub max_calls: usize,
    /// A subagent is stopped after this long.
    pub time_limit: Duration,
}

impl Default for SubagentOptions {
    fn default() -> Self {
        Self { enabled: true, max_parallel: None, max_calls: 25, time_limit: Duration::from_secs(15 * 60) }
    }
}

impl SubagentOptions {
    /// How many subagents may run at once on an endpoint with `slots` parallel slots.
    pub fn parallel(&self, slots: Option<u64>) -> usize {
        if let Some(n) = self.max_parallel {
            return n.max(1);
        }
        let slots = usize::try_from(slots.unwrap_or(2)).unwrap_or(usize::MAX).max(1);
        if slots > 1 { slots - 1 } else { 1 }
    }
}

/// A counting semaphore for inference slots, shared by every subagent of an engine.
#[derive(Debug, Default)]
pub struct Slots {
    /// (capacity, in use)
    state: Mutex<(usize, usize)>,
    freed: Condvar,
}

impl Slots {
    pub fn new(capacity: usize) -> Self {
        Self { state: Mutex::new((capacity.max(1), 0)), freed: Condvar::new() }
    }

    /// Change the capacity (the endpoint probe found out how many slots it has).
    pub fn set_capacity(&self, capacity: usize) {
        let mut s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        s.0 = capacity.max(1);
        self.freed.notify_all();
    }

    pub fn capacity(&self) -> usize {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).0
    }

    /// Wait for a free slot; `None` if `cancel` is set first.
    pub fn acquire(self: &Arc<Self>, cancel: &AtomicBool) -> Option<SlotGuard> {
        let mut s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            if s.1 < s.0 {
                s.1 = s.1.saturating_add(1);
                return Some(SlotGuard(Arc::clone(self)));
            }
            s = self.freed.wait_timeout(s, POLL).map(|(g, _)| g).unwrap_or_else(|e| e.into_inner().0);
        }
    }
}

/// A held slot; released on drop.
pub struct SlotGuard(Arc<Slots>);

impl Drop for SlotGuard {
    fn drop(&mut self) {
        let mut s = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        s.1 = s.1.saturating_sub(1);
        self.0.freed.notify_all();
    }
}

/// A model a subagent can run on.
#[derive(Clone)]
pub struct Model {
    pub config: InferenceConfig,
    pub llm: byob_llm::Client,
    pub plan: adapt::Plan,
    /// The saved model it is, by name (shown on the delegation card).
    pub profile: Option<String>,
    /// Subagents on its profile at once (`max_parallel`); `None`: no cap of its own.
    pub limit: Option<Arc<Slots>>,
}

impl Model {
    pub fn new(config: InferenceConfig, llm: byob_llm::Client, plan: adapt::Plan) -> Self {
        Self { config, llm, plan, profile: None, limit: None }
    }
}

/// A saved model (profile) subagents can run on; its client is made when one does.
#[derive(Clone)]
pub struct Choice {
    pub id: String,
    pub name: String,
    pub description: String,
    /// Offered to the main model as a `delegate` `model`.
    pub offered: bool,
    /// Its endpoint, with its key.
    pub config: InferenceConfig,
    pub plan: adapt::Plan,
    pub limit: Option<Arc<Slots>>,
}

impl Choice {
    /// The model, or `None` (logged) if its URL can't be used.
    pub fn model(&self) -> Option<Model> {
        match byob_llm::Client::new(self.config.clone()) {
            Ok(llm) => Some(Model { config: self.config.clone(), llm, plan: self.plan.clone(), profile: Some(self.name.clone()), limit: self.limit.clone() }),
            Err(e) => {
                log::warn!("subagent model {:?}: {e}; trying the next choice", self.name);
                None
            }
        }
    }

    fn is(&self, wanted: &str) -> bool {
        self.id == wanted || self.name.trim().eq_ignore_ascii_case(wanted)
    }
}

/// What a top-level turn needs to delegate.
pub struct Env {
    /// Agents this chat can use (workspace, user, plugins), by precedence.
    pub agents: Vec<AgentDef>,
    /// The chat's own model.
    pub main: Model,
    /// Model roles from settings (`fast`, …).
    pub roles: BTreeMap<String, Model>,
    /// The usable saved models, for the call's `model` (the offered ones) and agents'
    /// `model:` (any, by id or name).
    pub profiles: Vec<Choice>,
    /// The level's subagent model (chat, workspace, or the `subagents` role), if any.
    pub default: Option<Choice>,
    pub slots: Arc<Slots>,
    /// Server slots: each subagent runs in one no running turn holds.
    pub leases: Arc<SlotTable>,
    pub opts: SubagentOptions,
    /// The switched-on plugins' manifest `prompt` texts with the tools each contributed, in
    /// the main prompt's order.
    pub guidance: Vec<PluginPrompt>,
}

/// A plugin's manifest `prompt` and the tools it contributed to the turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginPrompt {
    pub plugin: String,
    pub tools: BTreeSet<String>,
    pub text: String,
}

/// A checked delegation, ready to start.
pub struct Job {
    pub agent: String,
    pub task: String,
    pub model: Model,
    pub tools: Registry,
    pub system: String,
}

/// Where a delegation runs and reports.
pub struct Launch {
    pub entry: u64,
    /// The id of the parent step that shows the delegation.
    pub step_id: u64,
    /// Offset of the step in the parent's reply.
    pub at: usize,
    pub inbox: Sender<Input>,
    /// The parent turn's flag: stopping the chat stops its subagents.
    pub cancel: Arc<AtomicBool>,
    pub workspace: Arc<PathBuf>,
    pub chat: String,
    pub step_ids: Arc<AtomicU64>,
    /// Where the chat's commands run (subagents run theirs there too).
    pub env: Arc<byob_tools::EnvPlan>,
    /// The parent's hooks (approval policies apply to subagents' calls too).
    pub hooks: crate::hooks::Hooks,
    /// The `vision` model role, for images the subagent's tools show.
    pub vision: Option<byob_llm::Client>,
}

/// The `delegate` tool of one turn: checks the call, starts the subagent through the turn
/// (its step becomes the delegation card) and waits for the report. Concurrent, so several
/// delegations of a round run side by side.
pub struct DelegateTool {
    pub env: Arc<Env>,
}

impl byob_tools::Tool for DelegateTool {
    fn spec(&self) -> byob_tools::ToolSpec {
        let s = self.env.spec();
        byob_tools::ToolSpec { name: s.name, description: s.description, parameters: s.parameters }
    }

    fn title(&self, args: &Value) -> String {
        self.pending_title(args)
    }

    fn pending_title(&self, args: &Value) -> String {
        let agent = args.get("agent").and_then(Value::as_str).map(str::trim).filter(|a| !a.is_empty()).unwrap_or(DEFAULT_AGENT);
        let task = args.get("task").and_then(Value::as_str).unwrap_or_default();
        format!("{agent} · {task}")
    }

    fn access(&self, _args: &Value) -> byob_tools::Access {
        byob_tools::Access::Read
    }

    fn concurrent(&self) -> bool {
        true
    }

    fn run(&self, args: &Value, ctx: &byob_tools::ToolContext<'_>) -> byob_tools::ToolOutput {
        // Only a top-level turn offers what a child turn needs (subagents can't delegate).
        let Some(host) = ctx.host::<turn::TurnHost>().filter(|h| !h.child) else {
            return byob_tools::ToolOutput::err("delegation is not available here");
        };
        let job = match self.env.prepare(args, &host.tools) {
            Ok(job) => job,
            Err(e) => return byob_tools::ToolOutput::err(e),
        };
        let launch = Launch {
            entry: host.entry,
            step_id: host.step_id,
            at: host.at,
            inbox: host.inbox.clone(),
            cancel: Arc::clone(&host.cancel),
            workspace: Arc::clone(&host.workspace),
            chat: host.chat.clone(),
            step_ids: Arc::clone(&host.step_ids),
            env: Arc::clone(&host.env),
            hooks: host.hooks.clone(),
            vision: host.vision.clone(),
        };
        match launch_job(&self.env, job, launch) {
            Ok(report) => {
                // The delegation card is the step now; the turn doesn't finish it.
                host.owns_step.store(true, Ordering::Relaxed);
                // The subagent thread always answers; a closed channel means it crashed.
                let text = report.recv().unwrap_or_else(|_| "Error: the subagent stopped unexpectedly; no report.".to_owned());
                byob_tools::ToolOutput::ok(text)
            }
            Err(e) => {
                host.owns_step.store(true, Ordering::Relaxed);
                byob_tools::ToolOutput::err(format!("couldn't start the subagent: {e}"))
            }
        }
    }
}

impl Env {
    /// The `delegate` tool definition. `model` is there only when some profile is offered.
    pub fn spec(&self) -> ToolSpec {
        let names: Vec<&str> = std::iter::once(DEFAULT_AGENT).chain(self.agents.iter().map(|a| a.name.as_str()).filter(|n| *n != DEFAULT_AGENT)).collect();
        let mut properties = json!({
            "agent": {"type": "string", "enum": names, "description": format!("Which subagent (default {DEFAULT_AGENT}).")},
            "task": {"type": "string", "description": "The complete task, self-contained, and what to put in the report."},
            "tools": {"type": "array", "items": {"type": "string"}, "description": "Optional: only these tools (names), e.g. to keep a subagent read-only."}
        });
        let offered = self.offered();
        if !offered.is_empty()
            && let Some(map) = properties.as_object_mut()
        {
            let ids: Vec<&str> = offered.iter().map(|c| c.id.as_str()).collect();
            map.insert(
                "model".into(),
                json!({"type": "string", "enum": ids, "description": "Optional: which model the subagent runs on (see Subagent models in the system prompt). Leave it out for the usual one."}),
            );
        }
        ToolSpec {
            name: TOOL.into(),
            description: "Hand a self-contained task to a subagent that works with its own fresh context and the same tools, and get back \
its report. Use it for research or busywork that would fill your context (reading many files, searching, summarising), and to do \
independent things at once: several delegate calls in one reply run in parallel. The subagent sees only `task`, not this \
conversation, so say everything it needs (paths, names, what to report back)."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": properties,
                "required": ["task"]
            }),
        }
    }

    /// The profiles the main model may pick with `model`, at most [`MAX_LISTED`].
    pub fn offered(&self) -> Vec<&Choice> {
        self.profiles.iter().filter(|c| c.offered).take(MAX_LISTED).collect()
    }

    fn agent(&self, name: &str) -> Option<&AgentDef> {
        self.agents.iter().find(|a| a.name == name)
    }

    /// The model a subagent runs on, first that applies: the call's `model` (an offered
    /// profile, by id or name; anything else is an error naming the allowed ones), the
    /// agent's `model:` ([`Self::model_for`]), the level's subagent model, the chat's model.
    /// A choice whose URL can't be used falls through to the next one (logged).
    pub fn pick_model(&self, asked: Option<&Value>, agent: Option<&str>) -> Result<Model, String> {
        let asked = match asked {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if s.trim().is_empty() => None,
            Some(Value::String(s)) => Some(s.trim()),
            Some(_) => return Err("`model` must be a string".into()),
        };
        let offered = self.offered();
        // Not in the schema when nothing is offered: a stray value is ignored.
        if let Some(w) = asked.filter(|_| !offered.is_empty()) {
            let Some(c) = offered.iter().find(|c| c.is(w)) else {
                let allowed: Vec<&str> = offered.iter().map(|c| c.id.as_str()).collect();
                return Err(format!("there is no model {w:?} to delegate to. Allowed `model` values: {} (or leave it out).", allowed.join(", ")));
            };
            if let Some(m) = c.model() {
                return Ok(m);
            }
        } else if let Some(w) = asked {
            log::info!("delegate: ignored model {w:?} (no models are offered to subagents)");
        }
        if let Some(m) = self.model_for(agent) {
            return Ok(m);
        }
        Ok(self.default.as_ref().and_then(Choice::model).unwrap_or_else(|| self.main.clone()))
    }

    /// The model an agent's `model:` names: a role; `inherit`/`main`: the chat's model; a
    /// profile id or name (any case); Claude Code's other aliases (`sonnet`, `default`, …):
    /// no preference (`None`); else a model id on the chat's endpoint.
    pub fn model_for(&self, wanted: Option<&str>) -> Option<Model> {
        let w = wanted.map(str::trim).filter(|w| !w.is_empty())?;
        if let Some(m) = self.roles.get(w) {
            return Some(m.clone());
        }
        if matches!(w, "inherit" | "main") {
            return Some(self.main.clone());
        }
        if let Some(c) = self.profiles.iter().find(|c| c.id == w).or_else(|| self.profiles.iter().find(|c| c.is(w))) {
            return c.model();
        }
        if matches!(w, "default" | "sonnet" | "opus" | "haiku") {
            return None;
        }
        let config = InferenceConfig { model: w.to_owned(), ..self.main.config.clone() };
        match byob_llm::Client::new(config.clone()) {
            Ok(llm) => Some(Model::new(config, llm, self.main.plan.clone())),
            Err(e) => {
                log::warn!("subagent model {w:?}: {e}; using the next choice");
                None
            }
        }
    }

    /// Check a call's (untrusted) arguments and pick the agent, model and tools.
    pub fn prepare(&self, args: &Value, all: &Registry) -> Result<Job, String> {
        let task = args.get("task").and_then(Value::as_str).map(str::trim).unwrap_or_default();
        if task.is_empty() {
            return Err("delegate needs a `task`: the complete, self-contained instructions for the subagent.".into());
        }
        if task.len() > MAX_TASK {
            return Err(format!("the task is too long ({} bytes, at most {MAX_TASK}); shorten it or point at files instead.", task.len()));
        }
        let name = match args.get("agent") {
            None | Some(Value::Null) => DEFAULT_AGENT.to_owned(),
            Some(Value::String(s)) if s.trim().is_empty() => DEFAULT_AGENT.to_owned(),
            Some(Value::String(s)) => s.trim().to_owned(),
            Some(_) => return Err("`agent` must be a string".into()),
        };
        let def = match self.agent(&name) {
            Some(d) => Some(d),
            None if name == DEFAULT_AGENT => None,
            None => {
                let known: Vec<&str> = std::iter::once(DEFAULT_AGENT).chain(self.agents.iter().map(|a| a.name.as_str())).collect();
                return Err(format!("there is no agent called {name:?}. Available: {}.", known.join(", ")));
            }
        };
        let asked = match args.get("tools") {
            None | Some(Value::Null) => None,
            Some(Value::Array(list)) => Some(list.iter().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<_>>()),
            Some(_) => return Err("`tools` must be a list of tool names".into()),
        };
        let tools = child_tools(all, def, asked.as_deref())?;
        let body = def.map_or(WORKER_PROMPT, |d| d.prompt.as_str());
        // The guidance of the plugins whose tools it has (the main prompt has all of them).
        let guidance = guidance_for(&self.guidance, &tools, &name);
        let system = if guidance.is_empty() { format!("{body}\n\n{SUBAGENT_RULES}") } else { format!("{body}\n\n{guidance}\n\n{SUBAGENT_RULES}") };
        let model = self.pick_model(args.get("model"), def.and_then(|d| d.model.as_deref()))?;
        Ok(Job { agent: name, task: task.to_owned(), model, tools, system })
    }
}

/// The tools a subagent gets: the turn's tools, cut down to the agent definition's list
/// (Claude Code names mapped, unknown names ignored) and then to what the call asked for.
/// `delegate` itself is never among them.
pub fn child_tools(all: &Registry, def: Option<&AgentDef>, asked: Option<&[String]>) -> Result<Registry, String> {
    let mut names: Vec<String> = all.specs().into_iter().map(|s| s.name).filter(|n| n != TOOL).collect();
    if let Some(listed) = def.and_then(|d| d.tools.as_ref()) {
        let allowed: Vec<&str> = listed.iter().flat_map(|t| tool_aliases(t)).collect();
        names.retain(|n| allowed.contains(&n.as_str()));
        if names.is_empty() {
            log::warn!("agent {}: none of its tools ({}) exist here; it runs without tools", def.map_or("", |d| d.name.as_str()), listed.join(", "));
        }
    }
    if let Some(asked) = asked.filter(|a| !a.is_empty()) {
        let before = names.clone();
        let wanted: Vec<&str> = asked.iter().flat_map(|t| tool_aliases(t.trim())).collect();
        names.retain(|n| wanted.contains(&n.as_str()));
        if names.is_empty() {
            return Err(format!("none of the tools asked for ({}) are available to this agent. Available: {}.", asked.join(", "), before.join(", ")));
        }
    }
    let mut out = Registry::default();
    for n in &names {
        if let Some(t) = all.get(n) {
            out.register(Arc::clone(t));
        }
    }
    Ok(out)
}

/// The prompt texts of the plugins that contributed at least one of `tools`, in order, as
/// many whole texts as fit in [`MAX_GUIDANCE`] bytes (the others are left out and logged).
pub fn guidance_for(prompts: &[PluginPrompt], tools: &Registry, agent: &str) -> String {
    let names: BTreeSet<String> = tools.specs().into_iter().map(|s| s.name).collect();
    let mut out = String::new();
    let mut dropped = Vec::new();
    for p in prompts.iter().filter(|p| !p.tools.is_disjoint(&names)) {
        let text = p.text.trim();
        if text.is_empty() {
            continue;
        }
        let sep = usize::from(!out.is_empty());
        if out.len().saturating_add(sep).saturating_add(text.len()) > MAX_GUIDANCE {
            dropped.push(p.plugin.as_str());
            continue;
        }
        if sep > 0 {
            out.push('\n');
        }
        out.push_str(text);
    }
    if !dropped.is_empty() {
        log::warn!("subagent {agent}: plugin guidance past {} KB left out of its prompt: {}", MAX_GUIDANCE / 1024, dropped.join(", "));
    }
    out
}

/// The agents a chat in `workspace` can use: `<workspace>/.byob/agents`, then
/// `<workspace>/.claude/agents`, then `configured` (user folder and plugins), first name wins.
pub fn agents_for(workspace: &Path, configured: &[AgentDef]) -> Vec<AgentDef> {
    let mut lists = Vec::new();
    for dir in [workspace.join(".byob").join("agents"), workspace.join(".claude").join("agents")] {
        let (found, errors) = load_agents_dir(&dir);
        for e in errors {
            log::warn!("agent {e}");
        }
        lists.push(found);
    }
    lists.push(configured.to_vec());
    merge_agents(lists).into_iter().filter(|a| a.name != TOOL).collect()
}

/// The main system prompt's note on subagents: the agents, and the models offered for the
/// call's `model` with what each is good at.
pub fn prompt_section(agents: &[AgentDef], models: &[&Choice]) -> String {
    let mut out = String::from(
        "## Subagents\nWith the `delegate` tool you can hand a self-contained task to a subagent with its own fresh context; you get \
back only its report. Delegate research and busywork that would fill your context, and independent pieces of work (several \
delegate calls in one reply run in parallel). Give each subagent everything it needs in `task`.\nAgents:\n",
    );
    out.push_str(&format!("- {DEFAULT_AGENT}: a general-purpose worker with all your tools (the default)\n"));
    for a in agents.iter().filter(|a| a.name != DEFAULT_AGENT).take(MAX_LISTED) {
        let d: String = a.description.chars().take(300).collect();
        out.push_str(&format!("- {}: {d}\n", a.name));
    }
    if !models.is_empty() {
        out.push_str("Subagent models (`model`; leave it out for the usual one, pick one when the task suits it):\n");
        for c in models {
            let d = one_line(&c.description, 300);
            let d = if d.is_empty() { String::new() } else { format!(": {d}") };
            out.push_str(&format!("- {} ({}){d}\n", c.id, one_line(&c.name, 80)));
        }
    }
    out.trim_end().to_owned()
}

/// Start `job` on its own thread. The parent step appears at once (queued until a slot is
/// free); the report arrives on the returned receiver.
pub fn launch_job(env: &Env, job: Job, l: Launch) -> std::io::Result<Receiver<String>> {
    let (tx, rx) = channel();
    let mut step = Step {
        id: l.step_id,
        tool: TOOL.into(),
        title: one_line(&format!("{} · {}", job.agent, job.task), DETAIL_CHARS),
        access: ToolAccess::Read,
        status: StepStatus::Running,
        detail: one_line(&job.task, DETAIL_CHARS),
        changes: Vec::new(),
        at: l.at,
        preview: None,
        media: Vec::new(),
        artifacts: Vec::new(),
        ask: None,
        views: vec![],
        delegation: Some(Box::new(Delegation {
            agent: job.agent.clone(),
            task: job.task.clone(),
            model: job.model.config.model.clone(),
            profile: job.model.profile.clone(),
            state: DelegationState::Queued,
            ..Default::default()
        })),
    };
    emit_step(&l, &step);
    let (slots, leases, opts) = (Arc::clone(&env.slots), Arc::clone(&env.leases), env.opts);
    let (inbox, entry, mut failed) = (l.inbox.clone(), l.entry, step.clone());
    let spawned = thread::Builder::new().name("byob-subagent".into()).spawn(move || {
        let ran = catch_unwind(AssertUnwindSafe(|| run(job, &l, &mut step, (&slots, &leases), opts)));
        let report = ran.unwrap_or_else(|_| {
            set_state(&mut step, DelegationState::Failed, Some("internal error: the subagent crashed".into()));
            step.status = StepStatus::Failed;
            emit_step(&l, &step);
            "Error: the subagent crashed.".to_owned()
        });
        let _ = tx.send(report);
    });
    match spawned {
        Ok(_) => Ok(rx),
        Err(e) => {
            // The step is already shown: settle it.
            failed.status = StepStatus::Failed;
            failed.detail = format!("could not start: {e}");
            set_state(&mut failed, DelegationState::Failed, Some(e.to_string()));
            let _ = inbox.send(Input::Turn(TurnEvent::Step { id: entry, step: failed }));
            Err(e)
        }
    }
}

fn emit_step(l: &Launch, step: &Step) {
    let _ = l.inbox.send(Input::Turn(TurnEvent::Step { id: l.entry, step: step.clone() }));
}

fn set_state(step: &mut Step, state: DelegationState, error: Option<String>) {
    if let Some(d) = step.delegation.as_deref_mut() {
        d.state = state;
        d.error = error;
    }
}

/// The subagent thread: wait for a slot, run the child turn, report.
fn run(job: Job, l: &Launch, step: &mut Step, (slots, leases): (&Arc<Slots>, &Arc<SlotTable>), opts: SubagentOptions) -> String {
    let started = Instant::now();
    // Its model's own cap first, so a subagent waiting for its model holds no global slot.
    let mine = match &job.model.limit {
        Some(limit) => limit.acquire(&l.cancel).map(Some),
        None => Some(None),
    };
    let Some((slot, mine)) = mine.and_then(|mine| slots.acquire(&l.cancel).map(|s| (s, mine))) else {
        set_state(step, DelegationState::Stopped, Some("stopped before it started".into()));
        step.status = StepStatus::Failed;
        "stopped before it started".clone_into(&mut step.detail);
        emit_step(l, step);
        return "Stopped by the user before the subagent started.".into();
    };
    set_state(step, DelegationState::Running, None);
    emit_step(l, step);

    // The child stops when the chat is stopped or its time is up.
    let cancel = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let timed_out = Arc::new(AtomicBool::new(false));
    let watchdog = {
        let (cancel, done, timed_out, parent) = (Arc::clone(&cancel), Arc::clone(&done), Arc::clone(&timed_out), Arc::clone(&l.cancel));
        let limit = opts.time_limit;
        thread::Builder::new().name("byob-subagent-watch".into()).spawn(move || {
            while !done.load(Ordering::Relaxed) {
                if parent.load(Ordering::Relaxed) {
                    cancel.store(true, Ordering::Relaxed);
                }
                if started.elapsed() > limit {
                    timed_out.store(true, Ordering::Relaxed);
                    cancel.store(true, Ordering::Relaxed);
                }
                thread::sleep(POLL);
            }
        })
    };
    if let Err(e) = watchdog {
        // Without the watchdog a stop would not reach the child; don't run unsupervised.
        drop((slot, mine));
        set_state(step, DelegationState::Failed, Some(e.to_string()));
        step.status = StepStatus::Failed;
        emit_step(l, step);
        return format!("Error: couldn't start the subagent: {e}");
    }
    let history = vec![ChatMessage::system(job.system.clone()), ChatMessage::user(job.task.clone())];
    let setup = turn::Setup {
        llm: job.model.llm.clone(),
        ids: vec![0; history.len()],
        history,
        entry: l.entry,
        cancel: Arc::clone(&cancel),
        inbox: l.inbox.clone(),
        tools: turn::ToolSet::new(job.tools),
        trust_ask: None,
        workspace: Arc::clone(&l.workspace),
        chat: l.chat.clone(),
        step_ids: Arc::clone(&l.step_ids),
        plan: job.model.plan.clone(),
        prior: 0,
        compact: None,
        child: Some(step.id),
        parent_cancel: Some(Arc::clone(&l.cancel)),
        max_calls: opts.max_calls,
        env: Arc::clone(&l.env),
        hooks: l.hooks.clone(),
        prompt: None,
        vision: l.vision.clone(),
        // Not the parent's slot (it holds that while it waits): its cache survives.
        slot: leases.for_subagent(&job.model.plan, job.model.llm.config()),
    };
    let out = turn::run_child(setup);
    done.store(true, Ordering::Relaxed);
    drop((slot, mine));

    let secs = started.elapsed().as_secs_f64();
    let tools = ran_summary(&out.ran, out.calls);
    let (state, error) = match &out.failure {
        None => (DelegationState::Done, None),
        Some(_) if timed_out.load(Ordering::Relaxed) => {
            (DelegationState::Stopped, Some(format!("time limit ({} min) reached", opts.time_limit.as_secs() / 60)))
        }
        Some((_, true)) => (DelegationState::Stopped, Some("stopped".to_owned())),
        Some((e, false)) => (DelegationState::Failed, Some(e.clone())),
    };
    let answer = if out.answer.trim().is_empty() { out.content.trim() } else { out.answer.trim() };
    if let Some(d) = step.delegation.as_deref_mut() {
        d.prompt_tokens = out.usage.prompt_tokens;
        d.completion_tokens = out.usage.completion_tokens;
    }
    set_state(step, state, error.clone());
    step.status = if state == DelegationState::Done { StepStatus::Done } else { StepStatus::Failed };
    step.detail = one_line(if answer.is_empty() { error.as_deref().unwrap_or("no report") } else { answer }, DETAIL_CHARS);
    emit_step(l, step);

    let head = match &error {
        None => format!("[subagent {} · {tools} · {secs:.1} s]", job.agent),
        Some(e) => format!("[subagent {} {} after {tools} · {secs:.1} s: {e}]", job.agent, state.label()),
    };
    let body = if answer.is_empty() { "(no report)".to_owned() } else { answer.to_owned() };
    let mut report = format!("{head}\n{body}");
    if report.len() > MAX_REPORT {
        let total = report.len();
        report = format!("{}\n[… report cut: {MAX_REPORT} of {total} bytes]", prefix(&report, MAX_REPORT));
    }
    if state != DelegationState::Done && !report.starts_with("Error") {
        report.insert_str(0, "Error: ");
    }
    report
}

/// "3 tool calls: read_file ×2, list_files".
fn ran_summary(ran: &BTreeMap<String, usize>, calls: usize) -> String {
    if calls == 0 {
        return "no tool calls".into();
    }
    let parts: Vec<String> = ran.iter().map(|(n, c)| if *c > 1 { format!("{n} ×{c}") } else { n.clone() }).collect();
    let calls_text = if calls == 1 { "1 tool call".to_owned() } else { format!("{calls} tool calls") };
    if parts.is_empty() { calls_text } else { format!("{calls_text}: {}", parts.join(", ")) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use byob_tools::{Access, Tool, ToolContext, ToolOutput};

    struct Named(&'static str);
    impl Tool for Named {
        fn spec(&self) -> byob_tools::ToolSpec {
            byob_tools::ToolSpec { name: self.0.into(), description: String::new(), parameters: json!({"type":"object"}) }
        }
        fn title(&self, _: &Value) -> String {
            self.0.into()
        }
        fn access(&self, _: &Value) -> Access {
            Access::Read
        }
        fn run(&self, _: &Value, _: &ToolContext<'_>) -> ToolOutput {
            ToolOutput::ok("")
        }
    }

    fn registry() -> Registry {
        let mut r = Registry::default();
        for n in ["read_file", "list_files", "write_file", "run_command"] {
            r.register(Arc::new(Named(n)));
        }
        r
    }

    fn names(r: &Registry) -> Vec<String> {
        r.specs().into_iter().map(|s| s.name).collect()
    }

    fn def(tools: Option<&[&str]>) -> AgentDef {
        AgentDef {
            name: "explorer".into(),
            description: "d".into(),
            tools: tools.map(|t| t.iter().map(|s| (*s).to_owned()).collect()),
            model: None,
            prompt: "p".into(),
            source: PathBuf::new(),
            meta: BTreeMap::new(),
        }
    }

    #[test]
    fn tools_follow_the_definition_and_the_call() {
        let all = registry();
        assert_eq!(names(&child_tools(&all, None, None).unwrap()).len(), 4);
        // Claude Code names map to byob tools; unknown ones are ignored.
        let d = def(Some(&["Read", "Glob", "WebSearch"]));
        assert_eq!(names(&child_tools(&all, Some(&d), None).unwrap()), ["list_files", "read_file"]);
        assert_eq!(names(&child_tools(&all, Some(&d), Some(&["read_file".into()])).unwrap()), ["read_file"]);
        assert!(child_tools(&all, Some(&d), Some(&["write_file".into()])).err().unwrap().contains("none of the tools"));
        // A definition whose tools don't exist here runs without tools.
        assert!(child_tools(&all, Some(&def(Some(&["WebSearch"]))), None).unwrap().is_empty());
    }

    fn guidance(plugin: &str, tools: &[&str], text: String) -> PluginPrompt {
        PluginPrompt { plugin: plugin.into(), tools: tools.iter().map(|t| (*t).to_owned()).collect(), text }
    }

    #[test]
    fn plugin_guidance_follows_the_tools_and_stays_under_the_cap() {
        let all = registry();
        let reader = child_tools(&all, None, Some(&["read_file".into()])).unwrap();
        let prompts = [
            guidance("files", &["read_file", "list_files"], "FILES".into()),
            guidance("shell", &["run_command"], "SHELL".into()),
            guidance("huge", &["read_file"], "x".repeat(MAX_GUIDANCE)),
            guidance("small", &["read_file"], "SMALL".into()),
        ];
        // Only the plugins of its tools; whole texts in order; what doesn't fit is left out.
        let g = guidance_for(&prompts, &reader, "explorer");
        assert_eq!(g, "FILES\nSMALL");
        let g = guidance_for(&prompts, &all, "general");
        assert_eq!(g, "FILES\nSHELL\nSMALL");
        // However many plugins there are, the total stays under the cap.
        let many: Vec<PluginPrompt> = (0..50).map(|i| guidance(&format!("p{i}"), &["read_file"], "y".repeat(500))).collect();
        let g = guidance_for(&many, &reader, "explorer");
        assert!(g.len() <= MAX_GUIDANCE && g.len() > MAX_GUIDANCE - 501, "{}", g.len());
    }

    #[test]
    fn parallelism_follows_the_endpoint_slots() {
        let o = SubagentOptions::default();
        assert_eq!((o.parallel(None), o.parallel(Some(1)), o.parallel(Some(2)), o.parallel(Some(4))), (1, 1, 1, 3));
        assert_eq!(SubagentOptions { max_parallel: Some(5), ..o }.parallel(Some(2)), 5);
        assert_eq!(SubagentOptions { max_parallel: Some(0), ..o }.parallel(None), 1);
    }

    #[test]
    fn slots_queue_and_respect_cancel() {
        let slots = Arc::new(Slots::new(1));
        let never = AtomicBool::new(false);
        let first = slots.acquire(&never).unwrap();
        let cancel = AtomicBool::new(true);
        assert!(slots.acquire(&cancel).is_none(), "full, and cancelled while waiting");
        let s2 = Arc::clone(&slots);
        let waiter = thread::spawn(move || s2.acquire(&AtomicBool::new(false)).is_some());
        thread::sleep(Duration::from_millis(80));
        drop(first);
        assert!(waiter.join().unwrap());
        slots.set_capacity(3);
        assert_eq!(slots.capacity(), 3);
    }

    #[test]
    fn summaries_read_naturally() {
        let mut ran = BTreeMap::new();
        assert_eq!(ran_summary(&ran, 0), "no tool calls");
        ran.insert("read_file".to_owned(), 2);
        ran.insert("list_files".to_owned(), 1);
        assert_eq!(ran_summary(&ran, 3), "3 tool calls: list_files, read_file ×2");
        let section = prompt_section(&[def(None)], &[]);
        assert!(section.contains("- general:") && section.contains("- explorer: d"), "{section}");
    }

    fn plan(url: &str) -> adapt::Plan {
        adapt::Plan {
            key: adapt::key(&InferenceConfig { base_url: url.into(), ..InferenceConfig::default() }),
            prompt_tools: false,
            fallback: true,
            context: 8192,
            reserve: 1024,
            vision: None,
            image_quality: 85,
            slots: None,
        }
    }

    fn model(url: &str, id: &str) -> Model {
        let config = InferenceConfig { base_url: url.into(), model: id.into(), ..InferenceConfig::default() };
        Model::new(config.clone(), byob_llm::Client::new(config).unwrap(), plan(url))
    }

    fn choice(id: &str, name: &str, offered: bool, url: &str) -> Choice {
        Choice {
            id: id.into(),
            name: name.into(),
            description: format!("{name} is good at things"),
            offered,
            config: InferenceConfig { base_url: url.into(), model: format!("{id}-model"), api_key: Some(format!("key-{id}")), api: None },
            plan: plan(url),
            limit: None,
        }
    }

    fn env(profiles: Vec<Choice>, default: Option<Choice>) -> Env {
        Env {
            agents: vec![],
            main: model("http://main/v1", "chat-model"),
            roles: [("fast".to_owned(), model("http://role/v1", "role-model"))].into(),
            profiles,
            default,
            slots: Arc::new(Slots::new(2)),
            leases: Arc::default(),
            opts: SubagentOptions::default(),
            guidance: vec![],
        }
    }

    fn picked(e: &Env, asked: Option<Value>, agent: Option<&str>) -> Result<String, String> {
        e.pick_model(asked.as_ref(), agent).map(|m| format!("{} {}", m.config.base_url, m.config.model))
    }

    #[test]
    fn the_model_argument_exists_only_when_a_profile_is_offered() {
        let none = env(vec![choice("quiet", "Quiet", false, "http://q/v1")], None);
        let spec = none.spec();
        assert!(spec.parameters["properties"].get("model").is_none(), "{}", spec.parameters);
        assert!(!prompt_section(&none.agents, &none.offered()).contains("Subagent models"));
        let some = env(vec![choice("quiet", "Quiet", false, "http://q/v1"), choice("coder", "Fast Coder", true, "http://c/v1")], None);
        let spec = some.spec();
        assert_eq!(spec.parameters["properties"]["model"]["enum"], json!(["coder"]));
        let section = prompt_section(&some.agents, &some.offered());
        assert!(section.contains("- coder (Fast Coder): Fast Coder is good at things"), "{section}");
        assert!(!section.contains("quiet"), "{section}");
    }

    #[test]
    fn a_subagents_model_comes_from_the_call_then_the_agent_then_the_level_then_the_chat() {
        let profiles = vec![choice("coder", "Fast Coder", true, "http://c/v1"), choice("quiet", "Quiet One", false, "http://q/v1")];
        let level = choice("level", "Level", false, "http://l/v1");
        let e = env(profiles.clone(), Some(level));
        // a. The call's model, by id or name, beats the agent's.
        assert_eq!(picked(&e, Some(json!("coder")), Some("fast")), Ok("http://c/v1 coder-model".into()));
        assert_eq!(picked(&e, Some(json!("fast coder")), None), Ok("http://c/v1 coder-model".into()));
        // Not offered, unknown or not a string: an error that names the allowed values.
        let err = picked(&e, Some(json!("quiet")), None).unwrap_err();
        assert!(err.contains("Allowed `model` values: coder"), "{err}");
        assert!(picked(&e, Some(json!("gpt-9")), None).unwrap_err().contains("coder"));
        assert!(picked(&e, Some(json!(3)), None).is_err());
        // b. The agent's model: a role, a profile (any, by id or by name in any case), an
        // explicit inherit, a raw model id on the chat's endpoint.
        assert_eq!(picked(&e, None, Some("fast")), Ok("http://role/v1 role-model".into()));
        assert_eq!(picked(&e, None, Some("quiet")), Ok("http://q/v1 quiet-model".into()));
        assert_eq!(picked(&e, Some(json!("")), Some("QUIET one")), Ok("http://q/v1 quiet-model".into()));
        assert_eq!(picked(&e, None, Some("inherit")), Ok("http://main/v1 chat-model".into()));
        assert_eq!(picked(&e, None, Some("tiny")), Ok("http://main/v1 tiny".into()));
        // c. Claude Code aliases and no `model:` leave it to the level's subagent model.
        assert_eq!(picked(&e, None, Some("sonnet")), Ok("http://l/v1 level-model".into()));
        assert_eq!(picked(&e, None, None), Ok("http://l/v1 level-model".into()));
        // d. Without one, the chat's model.
        let e = env(profiles, None);
        assert_eq!(picked(&e, None, Some("haiku")), Ok("http://main/v1 chat-model".into()));
        assert_eq!(picked(&e, None, None), Ok("http://main/v1 chat-model".into()));
        // The profile's key and name go with it.
        let m = e.pick_model(Some(&json!("coder")), None).unwrap();
        assert_eq!((m.config.api_key.as_deref(), m.profile.as_deref()), (Some("key-coder"), Some("Fast Coder")));
        // A stray `model` when nothing is offered (it isn't in the schema) is ignored.
        let e = env(vec![choice("quiet", "Quiet", false, "http://q/v1")], None);
        assert_eq!(picked(&e, Some(json!("quiet")), None), Ok("http://main/v1 chat-model".into()));
    }

    #[test]
    fn an_unusable_choice_falls_through_to_the_next() {
        let broken = choice("broken", "Broken", true, "notaurl");
        let e = env(vec![broken.clone()], Some(broken));
        assert_eq!(picked(&e, Some(json!("broken")), Some("broken")), Ok("http://main/v1 chat-model".into()));
    }
}
