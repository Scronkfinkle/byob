//! Subagents: the `delegate` tool.
//!
//! The main model calls `delegate { agent?, task, tools? }`; the turn starts a **child turn**
//! with a fresh context (the agent's system prompt and the task, not the chat's history)
//! that runs the normal tool loop with the same tools (or fewer), and returns the child's
//! final reply as the tool result. Children share the chat's workspace, artifacts,
//! permission policy, grants and sandbox; their approval cards appear in the chat. Several
//! delegations in one round run side by side, as many at once as the endpoint has free
//! slots ([`Slots`]); the rest queue. Children can't delegate (depth 1).
//!
//! Agent definitions are Claude Code compatible Markdown files (see [`byob_plugins::agents`]).

use std::collections::BTreeMap;
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

    #[cfg(test)]
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
}

/// What a top-level turn needs to delegate.
pub struct Env {
    /// Agents this chat can use (workspace, user, plugins), by precedence.
    pub agents: Vec<AgentDef>,
    pub main: Model,
    /// Model roles from settings (`fast`, …).
    pub roles: BTreeMap<String, Model>,
    pub slots: Arc<Slots>,
    pub opts: SubagentOptions,
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
}

impl Env {
    /// The `delegate` tool definition.
    pub fn spec(&self) -> ToolSpec {
        let names: Vec<&str> = std::iter::once(DEFAULT_AGENT).chain(self.agents.iter().map(|a| a.name.as_str()).filter(|n| *n != DEFAULT_AGENT)).collect();
        ToolSpec {
            name: TOOL.into(),
            description: "Hand a self-contained task to a subagent that works with its own fresh context and the same tools, and get back \
its report. Use it for research or busywork that would fill your context (reading many files, searching, summarising), and to do \
independent things at once: several delegate calls in one reply run in parallel. The subagent sees only `task`, not this \
conversation, so say everything it needs (paths, names, what to report back)."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "agent": {"type": "string", "enum": names, "description": format!("Which subagent (default {DEFAULT_AGENT}).")},
                    "task": {"type": "string", "description": "The complete task, self-contained, and what to put in the report."},
                    "tools": {"type": "array", "items": {"type": "string"}, "description": "Optional: only these tools (names), e.g. to keep a subagent read-only."}
                },
                "required": ["task"]
            }),
        }
    }

    fn agent(&self, name: &str) -> Option<&AgentDef> {
        self.agents.iter().find(|a| a.name == name)
    }

    /// The model an agent's `model:` names: a role, else Claude Code's aliases (`inherit`,
    /// `sonnet`, …) mean the main model, else a model id on the main endpoint.
    pub fn model_for(&self, wanted: Option<&str>) -> Model {
        let Some(w) = wanted.map(str::trim).filter(|w| !w.is_empty()) else { return self.main.clone() };
        if let Some(m) = self.roles.get(w) {
            return m.clone();
        }
        if matches!(w, "inherit" | "main" | "default" | "sonnet" | "opus" | "haiku") {
            return self.main.clone();
        }
        let config = InferenceConfig { model: w.to_owned(), ..self.main.config.clone() };
        match byob_llm::Client::new(config.clone()) {
            Ok(llm) => Model { config, llm, plan: self.main.plan.clone() },
            Err(e) => {
                log::warn!("subagent model {w:?}: {e}; using the main model");
                self.main.clone()
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
        let system = format!("{body}\n\n{SUBAGENT_RULES}");
        let model = self.model_for(def.and_then(|d| d.model.as_deref()));
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

/// The main system prompt's note on subagents.
pub fn prompt_section(agents: &[AgentDef]) -> String {
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
    out.trim_end().to_owned()
}

/// Start `job` on its own thread. The parent step appears at once (queued until a slot is
/// free); the report arrives on the returned receiver.
pub fn launch(env: &Env, job: Job, l: Launch) -> std::io::Result<Receiver<String>> {
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
        delegation: Some(Box::new(Delegation {
            agent: job.agent.clone(),
            task: job.task.clone(),
            model: job.model.config.model.clone(),
            state: DelegationState::Queued,
            ..Default::default()
        })),
    };
    emit_step(&l, &step);
    let (slots, opts) = (Arc::clone(&env.slots), env.opts);
    let (inbox, entry, mut failed) = (l.inbox.clone(), l.entry, step.clone());
    let spawned = thread::Builder::new().name("byob-subagent".into()).spawn(move || {
        let ran = catch_unwind(AssertUnwindSafe(|| run(job, &l, &mut step, &slots, opts)));
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
fn run(job: Job, l: &Launch, step: &mut Step, slots: &Arc<Slots>, opts: SubagentOptions) -> String {
    let started = Instant::now();
    let Some(slot) = slots.acquire(&l.cancel) else {
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
        drop(slot);
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
        tools: job.tools,
        workspace: Arc::clone(&l.workspace),
        chat: l.chat.clone(),
        step_ids: Arc::clone(&l.step_ids),
        plan: job.model.plan.clone(),
        prior: 0,
        compact: None,
        child: Some(step.id),
        parent_cancel: Some(Arc::clone(&l.cancel)),
        max_calls: opts.max_calls,
        delegate: None,
        env: Arc::clone(&l.env),
    };
    let out = turn::run_child(setup);
    done.store(true, Ordering::Relaxed);
    drop(slot);

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
        let section = prompt_section(&[def(None)]);
        assert!(section.contains("- general:") && section.contains("- explorer: d"), "{section}");
    }
}
