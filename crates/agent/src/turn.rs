//! One assistant turn on a worker thread: stream a reply; while the model asks for tools,
//! run them (through the engine's permission gate) and call the model again with the
//! results. Everything visible is reported back into the engine inbox as [`TurnEvent`]s.

use std::any::Any;
use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use byob_llm::{ChatMessage, LlmError, StreamEvent, ToolCall, ToolCallFilter, Usage, toolprompt};
use byob_protocol::{Ask, FileChange, Step, StepStatus, ToolAccess, TurnStats};
use byob_tools::{Access, Answer, Asker, Journal, Question, Registry, Tool, ToolContext, ToolOutput, UndoOp};
use serde_json::Value;

use crate::hooks::{self, Decision, Flow, Hooks, PromptInputs};
use crate::{Input, adapt};

use crate::limits::{DETAIL_CHARS, MAX_OUTPUT, MAX_REPEATS, MAX_ROUNDS, REPEAT_QUOTE, TURN_OUTPUT_BUDGET, cap, one_line, prefix};

pub const DEFAULT_SYSTEM_PROMPT: &str = "You are byob, a helpful assistant running inside a desktop harness. Be concise and accurate. \
Use the provided tools when they help; when a tool fails or the user declines it, explain instead of retrying the same call.";

/// How often a turn blocked on an approval re-checks its cancel flag.
const APPROVAL_POLL: Duration = Duration::from_millis(50);

/// The engine's answer to a [`TurnEvent::Gate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Allowed without asking (policy or grant).
    Allow,
    /// Not run; the reason is shown on the step.
    Deny(String),
    /// A person answered the approval card: allowed or not.
    Answered(bool),
}

impl Verdict {
    /// Why it may not run, if it may not.
    pub fn denied(&self) -> Option<String> {
        match self {
            Self::Allow | Self::Answered(true) => None,
            Self::Deny(reason) => Some(reason.clone()),
            Self::Answered(false) => Some(USER_SAID_NO.into()),
        }
    }
}

pub(crate) const USER_SAID_NO: &str = "the user said no";

pub enum TurnEvent {
    Delta {
        id: u64,
        content: String,
        reasoning: String,
    },
    /// Add or replace a step of entry `id`.
    Step {
        id: u64,
        step: Step,
    },
    /// Ask the engine whether `step` may run. The engine answers on `reply`, at once or
    /// after a client answers the approval card. Dropping `reply` means deny.
    Gate {
        id: u64,
        step: Step,
        reply: Sender<Verdict>,
    },
    Finished {
        id: u64,
        error: Option<String>,
        stats: Option<TurnStats>,
    },
    /// Undo information for one call's file changes in entry `id`. `complete` is false if
    /// some change can't be undone (a tool that keeps no journal modified or deleted files).
    Undo {
        id: u64,
        ops: Vec<UndoOp>,
        complete: bool,
    },
    /// A model request of entry `id` finished (a compaction's summary request included).
    /// `context` is what the chat's context holds now; `estimated` if that is our estimate.
    Usage {
        id: u64,
        prompt: u64,
        completion: u64,
        context: u64,
        estimated: bool,
    },
    /// A compaction started (`true`) or ended in entry `id`'s turn.
    Compacting {
        id: u64,
        on: bool,
    },
    /// Entry `id`'s turn compacted the context; the engine stores it.
    Compacted {
        id: u64,
        made: Compacted,
    },
    /// Something the user should know that is not an error of the turn.
    Notice {
        id: u64,
        text: String,
    },
}

/// A compaction a turn made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compacted {
    /// Last entry the summary covers (0: none of the stored ones).
    pub through: u64,
    pub summary: String,
    pub tokens_before: u64,
    pub tokens_after: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

/// Automatic compaction settings of one turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoCompact {
    /// Compact when a request is estimated above this many tokens.
    pub at: usize,
    pub keep_turns: usize,
}

/// Everything a turn thread needs.
pub struct Setup {
    pub llm: byob_llm::Client,
    pub history: Vec<ChatMessage>,
    /// The assistant entry this turn writes into.
    pub entry: u64,
    pub cancel: Arc<AtomicBool>,
    pub inbox: Sender<Input>,
    /// The tools offered; the engine may swap them while the turn runs (a plugin changed),
    /// and each round takes the set current when it starts.
    pub tools: Arc<ToolSet>,
    /// Before the first request, ask the user whether the workspace's plugins (which came
    /// with its folder) may run their code.
    pub trust_ask: Option<TrustAsk>,
    pub workspace: Arc<PathBuf>,
    /// The chat the turn runs in (artifacts belong to it).
    pub chat: String,
    pub step_ids: Arc<AtomicU64>,
    /// Tool-calling mode, history budget and vision for this endpoint.
    pub plan: adapt::Plan,
    /// The entry each `history` message came from (0: none), see [`compact::history`].
    pub ids: Vec<u64>,
    /// The chat's last entry before this turn (0: none); a compaction in the middle of the
    /// tool loop covers up to it.
    pub prior: u64,
    /// Compact automatically (`None`: never).
    pub compact: Option<AutoCompact>,
    /// This is the child turn of `delegate` step `child` (its events go to that step).
    pub child: Option<u64>,
    /// A child turn also stops when its parent's flag is set (checked between steps; a
    /// watchdog copies it into `cancel` for the model stream).
    pub parent_cancel: Option<Arc<AtomicBool>>,
    /// Most tool calls this turn may make.
    pub max_calls: usize,
    /// Where `run_command` and friends run, as the chat's settings decide.
    pub env: Arc<byob_tools::EnvPlan>,
    /// The chat's hooks (see [`crate::hooks`]).
    pub hooks: Hooks,
    /// What the `before_prompt` hooks build the system prompt from, before the first
    /// request (`None`: keep `history`'s, as child turns do).
    pub prompt: Option<PromptInputs>,
}

/// A turn's tools, swappable between rounds (never during a call: a round works on the
/// [`ToolSet::snapshot`] it took when it started).
#[derive(Default)]
pub struct ToolSet(Mutex<Registry>);

impl ToolSet {
    pub fn new(tools: Registry) -> Arc<Self> {
        Arc::new(Self(Mutex::new(tools)))
    }

    /// The tools as they are now (cheap: the registry holds `Arc`s).
    pub fn snapshot(&self) -> Registry {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Offer `tools` from the next round on.
    pub fn swap(&self, tools: Registry) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = tools;
    }
}

/// The question about a workspace's plugins that came with its folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustAsk {
    pub plugins: Vec<String>,
    /// What saying yes turns on (for the card).
    pub text: String,
}

/// Send a turn's event to the engine, as a child's event if `child` is set.
pub(crate) fn emit_to(inbox: &Sender<Input>, child: Option<u64>, event: TurnEvent) {
    let _ = inbox.send(match child {
        Some(step) => Input::Child { step, event },
        None => Input::Turn(event),
    });
}

/// How a child turn ended, for its report.
pub(crate) struct ChildOutcome {
    /// `None`: it answered. Otherwise why not, and whether it was stopped (rather than failed).
    pub failure: Option<(String, bool)>,
    /// Its final answer (the last round's text); empty if it never got there.
    pub answer: String,
    /// Everything it streamed.
    pub content: String,
    /// Tools it ran, by name, with counts.
    pub ran: BTreeMap<String, usize>,
    pub calls: usize,
    pub usage: Usage,
}

/// Run a child turn on this thread (a `delegate` step's subagent).
pub(crate) fn run_child(setup: Setup) -> ChildOutcome {
    let mut turn = Turn::new(setup);
    let result = catch_unwind(AssertUnwindSafe(|| turn.run()));
    let failure = match result {
        Ok(Ok(())) => None,
        Ok(Err(End::Stopped)) => Some(("stopped".to_owned(), true)),
        Ok(Err(End::Error(e))) => Some((e, false)),
        Err(_) => Some(("internal error: the subagent crashed".to_owned(), false)),
    };
    ChildOutcome {
        failure,
        answer: std::mem::take(&mut turn.answer),
        content: std::mem::take(&mut turn.content),
        ran: std::mem::take(&mut turn.ran),
        calls: turn.calls,
        usage: turn.usage,
    }
}

pub fn spawn(setup: Setup) -> std::io::Result<()> {
    thread::Builder::new().name("byob-turn".into()).spawn(move || {
        let started = Instant::now();
        let id = setup.entry;
        let inbox = setup.inbox.clone();
        let mut turn = Turn::new(setup);
        let result = catch_unwind(AssertUnwindSafe(|| turn.run()));
        let end = hooks::TurnEndCtx {
            entry: id,
            chat: &turn.s.chat,
            workspace: turn.s.workspace.as_path(),
            answer: &turn.answer,
            error: match &result {
                Ok(Err(End::Error(e))) => Some(e.as_str()),
                Err(_) => Some("internal error: the turn crashed"),
                _ => None,
            },
            stopped: matches!(result, Ok(Err(End::Stopped))),
        };
        let never = AtomicBool::new(false);
        turn.s.hooks.on_turn_end(&end, &never);
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let stats =
            TurnStats { prompt_tokens: turn.usage.prompt_tokens, completion_tokens: turn.usage.completion_tokens, elapsed_ms, model_calls: turn.model_calls };
        let (error, stats) = match result {
            Ok(Ok(())) => (None, Some(stats)),
            Ok(Err(End::Stopped)) => (Some("stopped".to_owned()), Some(stats)),
            Ok(Err(End::Error(e))) => (Some(e), turn.rounds_done.then_some(stats)),
            Err(_) => (Some("internal error: the turn crashed".to_owned()), None),
        };
        let _ = inbox.send(Input::Turn(TurnEvent::Finished { id, error, stats }));
    })?;
    Ok(())
}

enum End {
    Stopped,
    Error(String),
}

struct Turn {
    s: Setup,
    /// This round's tools (taken from `s.tools` when the round starts).
    tools: Registry,
    messages: Vec<ChatMessage>,
    usage: Usage,
    /// At least one model request completed (so `usage` means something).
    rounds_done: bool,
    /// Bytes of visible content streamed so far (for step offsets).
    content_len: usize,
    /// Newlines the visible content ends with.
    tail_newlines: usize,
    /// Tool calls attempted this turn (including refused ones).
    calls: usize,
    /// Bytes of tool output given to the model this turn.
    output_used: usize,
    /// `name:args` → result text, for repeated-call detection.
    seen: BTreeMap<String, String>,
    repeats: usize,
    /// Undo journal for the file changes of this turn.
    journal: Journal,
    /// Model requests made (compactions included).
    model_calls: u64,
    /// Native hooks' per-turn state, by hook id.
    hook_state: BTreeMap<String, Box<dyn Any + Send>>,
    /// The final answer (text of the round that made no tool calls).
    answer: String,
    /// All visible text streamed (kept for child turns' reports).
    content: String,
    /// Tools that ran, with counts.
    ran: BTreeMap<String, usize>,
}

/// A call that passed the checks and the gate, about to run.
struct Opened {
    tool: Arc<dyn Tool>,
    name: String,
    args: Value,
    /// For repeated-call detection.
    key: String,
    step: Step,
}

/// A concurrent call running on its own thread.
struct Started {
    opened: Opened,
    done: Receiver<(ToolOutput, Vec<UndoOp>, bool)>,
}

/// What a running tool can reach of its turn ([`ToolContext::host`]): enough to start a
/// child turn whose events land on the call's step (see `delegate`).
pub(crate) struct TurnHost {
    pub entry: u64,
    /// The call's step.
    pub step_id: u64,
    /// The step's offset in the reply.
    pub at: usize,
    pub inbox: Sender<Input>,
    /// The turn's stop flag.
    pub cancel: Arc<AtomicBool>,
    pub workspace: Arc<PathBuf>,
    pub chat: String,
    pub step_ids: Arc<AtomicU64>,
    pub env: Arc<byob_tools::EnvPlan>,
    /// The round's tools.
    pub tools: Registry,
    /// The turn's hooks (a child turn runs the tool hooks too).
    pub hooks: Hooks,
    /// This is a child turn (children can't start children).
    pub child: bool,
    /// The tool reported the step itself (the turn doesn't finish it).
    pub owns_step: AtomicBool,
}

impl Turn {
    fn new(s: Setup) -> Self {
        let messages = s.history.clone();
        let started = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        let tag = s.child.map(|c| format!("-a{c}")).unwrap_or_default();
        let journal = Journal::new(s.workspace.as_path(), &format!("{}-{started}{tag}", s.entry));
        let tools = s.tools.snapshot();
        Self {
            s,
            tools,
            messages,
            usage: Usage::default(),
            rounds_done: false,
            content_len: 0,
            tail_newlines: 0,
            calls: 0,
            output_used: 0,
            seen: BTreeMap::new(),
            repeats: 0,
            journal,
            model_calls: 0,
            hook_state: BTreeMap::new(),
            answer: String::new(),
            content: String::new(),
            ran: BTreeMap::new(),
        }
    }

    fn cancelled(&self) -> bool {
        self.s.cancel.load(Ordering::Relaxed) || self.s.parent_cancel.as_ref().is_some_and(|p| p.load(Ordering::Relaxed))
    }

    fn emit(&self, event: TurnEvent) {
        emit_to(&self.s.inbox, self.s.child, event);
    }

    fn tools_on(&self) -> bool {
        !self.tools.is_empty() && self.calls < self.s.max_calls && self.repeats < MAX_REPEATS
    }

    fn run(&mut self) -> Result<(), End> {
        if let Some(ask) = self.s.trust_ask.take() {
            self.ask_trust(&ask);
        }
        // The system prompt: every before_prompt hook, in order (plugins' backends included).
        if let Some(inputs) = self.s.prompt.take() {
            let text = self.s.hooks.before_prompt(&inputs, &self.s.cancel);
            if let Some(first) = self.messages.first_mut().filter(|m| m.role == "system") {
                *first = ChatMessage::system(text);
            }
        }
        for round in 0..MAX_ROUNDS {
            // Plugins may have changed since the last round (the model made a tool).
            self.tools = self.s.tools.snapshot();
            let tools_on = self.tools_on();
            let mut specs = if tools_on { specs(&self.tools) } else { Vec::new() };
            self.before_request(&mut specs, round)?;
            let out = self.model_call(&specs)?;
            if out.cancelled {
                return Err(End::Stopped);
            }
            let prompt = self.s.plan.prompt_tools;
            let (text, calls) = if prompt && tools_on {
                let mut parsed = toolprompt::parse_tool_calls(&out.content, &specs);
                // Servers with a tool parser may turn call text into structured calls anyway.
                parsed.calls.extend(out.tool_calls);
                (parsed.text, parsed.calls)
            } else {
                (out.content, out.tool_calls)
            };
            let calls = usable_calls(calls, self.s.entry);
            // With tools off, calls the model makes anyway are ignored: its text is the answer.
            if calls.is_empty() || !tools_on {
                self.answer = text;
                return Ok(());
            }
            if prompt {
                self.messages.push(toolprompt::fallback_assistant_message(&text, &calls));
            } else {
                let text = Some(text).filter(|c| !c.trim().is_empty());
                self.messages.push(ChatMessage::assistant_tool_calls(text, &calls));
            }
            for (call, result) in calls.iter().zip(self.call_all(&calls)) {
                self.messages.push(if prompt { toolprompt::fallback_tool_result(call, &result) } else { ChatMessage::tool_result(call.id.clone(), result) });
            }
            if self.cancelled() {
                return Err(End::Stopped);
            }
        }
        Err(End::Error(format!("stopped after {MAX_ROUNDS} model requests without an answer")))
    }

    /// The `before_request` hooks (compaction among them) may change the messages and
    /// the tools of this request, or end the turn.
    fn before_request(&mut self, specs: &mut Vec<byob_llm::ToolSpec>, round: usize) -> Result<(), End> {
        let inbox = self.s.inbox.clone();
        let child = self.s.child;
        let emit = move |e: TurnEvent| emit_to(&inbox, child, e);
        let mut ctx = hooks::RequestCtx {
            entry: self.s.entry,
            round,
            child: self.s.child.is_some(),
            messages: &mut self.messages,
            specs,
            llm: &self.s.llm,
            plan: &self.s.plan,
            cancel: &self.s.cancel,
            ids: &self.s.ids,
            prior: self.s.prior,
            compact: self.s.compact,
            usage: &mut self.usage,
            model_calls: &mut self.model_calls,
            state: &mut self.hook_state,
            emit: &emit,
        };
        match self.s.hooks.before_request(&mut ctx) {
            Flow::Continue => Ok(()),
            Flow::Stop(_) if self.cancelled() => Err(End::Stopped),
            Flow::Stop(reason) => Err(End::Error(reason)),
        }
    }

    /// Ask once whether the workspace's plugins that came with its folder may run their
    /// code; the engine records the answer and, on yes, adds their tools before the first
    /// request.
    fn ask_trust(&mut self, ask: &TrustAsk) {
        let asker = TurnAsker {
            inbox: self.s.inbox.clone(),
            child: self.s.child,
            entry: self.s.entry,
            step_ids: Arc::clone(&self.s.step_ids),
            cancel: Arc::clone(&self.s.cancel),
            tool: "plugins".into(),
            at: self.content_len,
        };
        let q = Question {
            title: format!("Turn on this workspace's plugins: {}", ask.plugins.join(", ")),
            text: ask.text.clone(),
            warning: Some("They came with the workspace folder and run their own code.".into()),
            access: Access::Execute,
            always_ask: false,
        };
        let trust = match asker.ask(&q) {
            Answer::Yes | Answer::AutoYes => true,
            Answer::No => false,
            // Nobody answered (stopped, or the policy refuses code): ask again next time.
            Answer::AutoNo => return,
        };
        let op = crate::plugins::PluginOp::Trust { plugins: ask.plugins.clone(), trust };
        let _ = crate::plugins::request(&self.s.inbox, &self.s.chat, op, &self.s.cancel);
    }

    /// One model request. If the server rejects native tools and the plan allows it, switch
    /// this turn (and, via the engine, the endpoint) to prompt-based tool calling and retry once.
    fn model_call(&mut self, specs: &[byob_llm::ToolSpec]) -> Result<byob_llm::ChatOutcome, End> {
        match self.stream(specs) {
            Err(e) if !specs.is_empty() && !self.s.plan.prompt_tools && self.s.plan.fallback && adapt::rejects_tools(&e) => {
                log::warn!("server rejected native tools ({e}); retrying with prompt-based tool calling");
                self.s.plan.prompt_tools = true;
                self.s.plan.fallback = false;
                self.messages = toolprompt::to_fallback(&self.messages);
                let _ = self.s.inbox.send(Input::Adapt(adapt::Event::ToolsRejected { key: self.s.plan.key.clone() }));
                self.stream(specs).map_err(|e| End::Error(e.to_string()))
            }
            result => result.map_err(|e| End::Error(e.to_string())),
        }
    }

    /// One model request; streams visible text to clients. In prompt mode, tool-call markup
    /// is hidden from the stream (the full text is still returned for parsing).
    fn stream(&mut self, specs: &[byob_llm::ToolSpec]) -> Result<byob_llm::ChatOutcome, LlmError> {
        let id = self.s.entry;
        let inbox = &self.s.inbox;
        let prompt = self.s.plan.prompt_tools;
        let request = self.s.plan.request(&self.messages, specs);
        let wire_specs = if prompt { &[][..] } else { specs };
        // Text of a later round starts a new paragraph.
        let mut shown = Shown::new(self.content_len);
        shown.tail_newlines = self.tail_newlines;
        let mut filter = (prompt && !specs.is_empty()).then(ToolCallFilter::default);
        let child = self.s.child;
        let streamed = &mut self.content;
        let result = self.s.llm.stream_chat(&request, wire_specs, &self.s.cancel, |ev| {
            let (content, reasoning) = match ev {
                StreamEvent::Content(t) => (t, String::new()),
                StreamEvent::Reasoning(t) => (String::new(), t),
            };
            let content = match filter.as_mut() {
                Some(f) => f.push(&content),
                None => content,
            };
            streamed.push_str(&content);
            shown.send(inbox, child, id, content, reasoning);
        });
        if let Some(f) = filter.as_mut() {
            let rest = f.finish();
            streamed.push_str(&rest);
            shown.send(inbox, child, id, rest, String::new());
        }
        self.content_len = shown.len;
        self.tail_newlines = shown.tail_newlines;
        let out = result?;
        self.rounds_done = true;
        self.model_calls = self.model_calls.saturating_add(1);
        let (prompt_tokens, completion, context, estimated) = match out.usage {
            Some(u) => (u.prompt_tokens, u.completion_tokens, u.prompt_tokens.saturating_add(u.completion_tokens), false),
            None => {
                let reply = byob_llm::estimate_tokens(&out.content).saturating_add(byob_llm::estimate_tokens(&out.reasoning));
                let sent = byob_llm::estimate_history_tokens(&request).saturating_add(byob_llm::budget::estimate_tools_tokens(wire_specs));
                (0, 0, to_u64(sent.saturating_add(reply)), true)
            }
        };
        self.add_usage(prompt_tokens, completion);
        self.emit(TurnEvent::Usage { id, prompt: prompt_tokens, completion, context, estimated });
        Ok(out)
    }

    fn add_usage(&mut self, prompt: u64, completion: u64) {
        self.usage.prompt_tokens = self.usage.prompt_tokens.saturating_add(prompt);
        self.usage.completion_tokens = self.usage.completion_tokens.saturating_add(completion);
    }

    /// Run one round's calls and return their results in order. Calls of concurrent tools
    /// (`Tool::concurrent`, e.g. `delegate`) start first, each on a thread of its own; the
    /// other calls run one after another meanwhile, in order.
    fn call_all(&mut self, calls: &[ToolCall]) -> Vec<String> {
        let mut results: Vec<Option<String>> = vec![None; calls.len()];
        let mut running: Vec<(usize, Started)> = Vec::new();
        for (i, call) in calls.iter().enumerate() {
            if self.tools.get(call.name.trim()).is_some_and(|t| t.concurrent()) {
                match self.open(call).and_then(|o| self.start(o)) {
                    Ok(started) => running.push((i, started)),
                    Err(text) => results[i] = Some(text),
                }
            }
        }
        for (i, call) in calls.iter().enumerate() {
            if results[i].is_none() && !running.iter().any(|r| r.0 == i) {
                results[i] = Some(self.call(call));
            }
        }
        for (i, started) in running {
            // The call's thread always answers; a closed channel means it crashed.
            let (output, ops, owned) = started.done.recv().unwrap_or_else(|_| (ToolOutput::err("the tool crashed"), Vec::new(), false));
            results[i] = Some(self.close(started.opened, output, ops, owned));
        }
        results.into_iter().map(Option::unwrap_or_default).collect()
    }

    /// Count a call against the turn's limit. `Some(refusal)` if it is over.
    fn count_call(&mut self) -> Option<String> {
        self.calls = self.calls.saturating_add(1);
        let max = self.s.max_calls;
        (self.calls > max).then(|| format!("Not run: the limit of {max} tool calls per turn is reached. Answer with what you have."))
    }

    /// Bound a result by the per-call cap and what is left of the turn's output budget.
    fn charge(&mut self, text: String) -> String {
        let text = cap(text, MAX_OUTPUT, "the tool's output is longer than one call may return");
        let remaining = TURN_OUTPUT_BUDGET.saturating_sub(self.output_used);
        let text = cap(text, remaining, "this turn's tool output budget is used up; answer with what you have");
        self.output_used = self.output_used.saturating_add(text.len());
        text
    }

    /// Run one call and return what the model sees.
    fn call(&mut self, call: &ToolCall) -> String {
        let opened = match self.open(call) {
            Ok(o) => o,
            Err(text) => return text,
        };
        let host = self.host(&opened.step);
        let asker = self.asker(&opened.name);
        let ctx = ToolContext::new(self.s.workspace.as_path(), &self.s.cancel)
            .with_journal(&self.journal)
            .with_chat(&self.s.chat)
            .with_asker(&asker)
            .with_env(&self.s.env)
            .with_host(&host);
        let output = catch_unwind(AssertUnwindSafe(|| opened.tool.run(&opened.args, &ctx))).unwrap_or_else(|_| ToolOutput::err("the tool crashed"));
        let ops = self.journal.take();
        let owned = host.owns_step.load(Ordering::Relaxed);
        self.close(opened, output, ops, owned)
    }

    /// Start a concurrent call on a thread of its own (with its own undo journal).
    fn start(&mut self, opened: Opened) -> Result<Started, String> {
        let host = self.host(&opened.step);
        let asker = self.asker(&opened.name);
        let (tool, args) = (Arc::clone(&opened.tool), opened.args.clone());
        let (workspace, cancel, chat, env) = (Arc::clone(&self.s.workspace), Arc::clone(&self.s.cancel), self.s.chat.clone(), Arc::clone(&self.s.env));
        let tag = format!("{}-c{}", self.s.entry, opened.step.id);
        let (tx, rx) = channel();
        let spawned = thread::Builder::new().name("byob-tool".into()).spawn(move || {
            let journal = Journal::new(workspace.as_path(), &tag);
            let ctx = ToolContext::new(workspace.as_path(), &cancel).with_journal(&journal).with_chat(&chat).with_asker(&asker).with_env(&env).with_host(&host);
            let output = catch_unwind(AssertUnwindSafe(|| tool.run(&args, &ctx))).unwrap_or_else(|_| ToolOutput::err("the tool crashed"));
            let _ = tx.send((output, journal.take(), host.owns_step.load(Ordering::Relaxed)));
        });
        match spawned {
            Ok(_) => Ok(Started { opened, done: rx }),
            Err(e) => Ok(Started { done: failed_now(format!("couldn't start the tool: {e}")), opened }),
        }
    }

    fn asker(&self, tool: &str) -> TurnAsker {
        TurnAsker {
            inbox: self.s.inbox.clone(),
            child: self.s.child,
            entry: self.s.entry,
            step_ids: Arc::clone(&self.s.step_ids),
            cancel: Arc::clone(&self.s.cancel),
            tool: tool.to_owned(),
            at: self.content_len,
        }
    }

    fn host(&self, step: &Step) -> TurnHost {
        TurnHost {
            entry: self.s.entry,
            step_id: step.id,
            at: step.at,
            inbox: self.s.inbox.clone(),
            cancel: Arc::clone(&self.s.cancel),
            workspace: Arc::clone(&self.s.workspace),
            chat: self.s.chat.clone(),
            step_ids: Arc::clone(&self.s.step_ids),
            env: Arc::clone(&self.s.env),
            tools: self.tools.clone(),
            hooks: self.s.hooks.clone(),
            child: self.s.child.is_some(),
            owns_step: AtomicBool::new(false),
        }
    }

    /// Check a call (stop, limit, known tool, arguments, repeats), show its step, run the
    /// `before_tool_call` hooks and the permission gate. `Err` is the result for the model.
    fn open(&mut self, call: &ToolCall) -> Result<Opened, String> {
        if self.cancelled() {
            return Err("Stopped by the user.".into());
        }
        if let Some(refused) = self.count_call() {
            return Err(refused);
        }
        let name = call.name.trim().to_owned();
        let Some(tool) = self.tools.get(&name).cloned() else {
            self.failed_step(&name, format!("Unknown tool {name:?}"), ToolAccess::Read, "no such tool");
            let known: Vec<String> = specs(&self.tools).into_iter().map(|s| s.name).collect();
            return Err(format!("Error: there is no tool called {name:?}. Available tools: {}.", known.join(", ")));
        };
        let mut args = match parse_arguments(&call.arguments) {
            Ok(args) => args,
            Err(e) => {
                self.failed_step(&name, format!("{name}: unreadable arguments"), ToolAccess::Read, &e);
                return Err(format!("Error: {e}. Call {name} again with a JSON object matching its parameters."));
            }
        };
        let key = format!("{name}:{args}");
        if let Some(earlier) = self.seen.get(&key) {
            self.repeats = self.repeats.saturating_add(1);
            return Err(format!(
                "You already called {name} with exactly these arguments in this turn. The result was:\n{}\nDon't repeat it; continue with something else or answer.",
                prefix(earlier, REPEAT_QUOTE)
            ));
        }
        let tool_ref = tool.as_ref();
        // Tool code is not ours to trust either: a panic in title/access/preview/run fails the call.
        let title = catch_unwind(AssertUnwindSafe(|| tool_ref.pending_title(&args))).unwrap_or_else(|_| name.clone());
        // If access() can't say, assume the most dangerous.
        let access = catch_unwind(AssertUnwindSafe(|| tool_ref.access(&args))).map_or(ToolAccess::Execute, to_protocol);
        let preview = catch_unwind(AssertUnwindSafe(|| tool_ref.preview(&args, self.s.workspace.as_path()))).ok().flatten().map(crate::steps::bound_preview);
        let mut step = Step {
            id: self.s.step_ids.fetch_add(1, Ordering::Relaxed),
            tool: name.clone(),
            title: one_line(&title, DETAIL_CHARS),
            access,
            status: StepStatus::Running,
            detail: one_line(&args.to_string(), DETAIL_CHARS),
            changes: Vec::new(),
            at: self.content_len,
            preview,
            media: Vec::new(),
            artifacts: Vec::new(),
            delegation: None,
            // A call a person must confirm every time (installing system-wide).
            ask: catch_unwind(AssertUnwindSafe(|| tool_ref.always_ask(&args)))
                .unwrap_or_else(|_| Some("this tool must be confirmed".into()))
                .map(|w| Box::new(Ask { text: String::new(), warning: Some(prefix(&w, MAX_ASK_TEXT).to_owned()), always: true })),
            views: vec![],
        };
        self.emit(TurnEvent::Step { id: self.s.entry, step: step.clone() });
        // Plugins first (they may change the arguments, decide, or ask), then the gate.
        let mut hook_ctx = hooks::ToolCallCtx {
            tool: &name,
            args: &mut args,
            access,
            chat: &self.s.chat,
            workspace: self.s.workspace.as_path(),
            child: self.s.child.is_some(),
        };
        let decision = self.s.hooks.before_tool_call(&mut hook_ctx, &self.s.cancel);
        if format!("{name}:{args}") != key {
            step.detail = one_line(&args.to_string(), DETAIL_CHARS);
        }
        let verdict = match decision {
            Decision::Deny(message) => Verdict::Deny(message),
            Decision::Allow if step.ask.as_ref().is_none_or(|a| !a.always) => Verdict::Allow,
            Decision::Ask(message) => {
                let text = if message.is_empty() { "A plugin asks you to confirm this call.".to_owned() } else { prefix(&message, MAX_ASK_TEXT).to_owned() };
                let warning = step.ask.take().and_then(|a| a.warning);
                step.ask = Some(Box::new(Ask { text, warning, always: true }));
                self.gate(&step)
            }
            Decision::Allow | Decision::Continue => self.gate(&step),
        };
        if let Some(reason) = verdict.denied() {
            step.status = StepStatus::Denied;
            step.detail = one_line(&reason, DETAIL_CHARS);
            self.emit(TurnEvent::Step { id: self.s.entry, step });
            if self.cancelled() {
                return Err("Stopped by the user.".into());
            }
            return Err(format!("Not run: {reason}. Don't retry this call; tell the user what you would have done."));
        }
        Ok(Opened { tool, name, args, key, step })
    }

    /// After a call: undo information, the `after_tool_call` hooks, the step's final state,
    /// and the text the model gets. `owned`: the tool reported its step itself.
    fn close(&mut self, opened: Opened, output: ToolOutput, ops: Vec<UndoOp>, owned: bool) -> String {
        let Opened { tool, name, args, key, mut step } = opened;
        let (ops, complete) = crate::steps::undo_ops(ops, &output.changes);
        if !ops.is_empty() || !complete {
            self.emit(TurnEvent::Undo { id: self.s.entry, ops, complete });
        }
        *self.ran.entry(name.clone()).or_default() += 1;
        let mut text = self.charge(output.text);
        let mut hook_ctx = hooks::AfterCtx {
            tool: &name,
            args: &args,
            result: &mut text,
            failed: output.failed,
            views: &mut step.views,
            chat: &self.s.chat,
            workspace: self.s.workspace.as_path(),
            child: self.s.child.is_some(),
        };
        self.s.hooks.after_tool_call(&mut hook_ctx, &self.s.cancel);
        if !owned {
            step.status = if output.failed { StepStatus::Failed } else { StepStatus::Done };
            if !output.failed {
                let done = catch_unwind(AssertUnwindSafe(|| tool.title(&args))).unwrap_or_else(|_| name.clone());
                step.title = one_line(&done, DETAIL_CHARS);
            }
            step.detail = one_line(&text, DETAIL_CHARS);
            step.changes = output.changes.iter().map(|c| display_change(c, &self.s.workspace)).collect();
            step.media = crate::attachments::media_for_changes(&output.changes, &self.s.workspace);
            if !output.failed {
                step.artifacts = byob_tools::artifact::for_call(&self.s.workspace, &self.s.chat, &output.changes, &step.media);
            }
            self.emit(TurnEvent::Step { id: self.s.entry, step });
        }
        if output.failed && !text.starts_with("Error") {
            text.insert_str(0, "Error: ");
        }
        self.seen.insert(key, text.clone());
        text
    }

    /// Ask the engine for permission; blocks until answered or the turn is cancelled.
    fn gate(&self, step: &Step) -> Verdict {
        let (reply, answer) = channel();
        self.emit(TurnEvent::Gate { id: self.s.entry, step: step.clone(), reply });
        wait_verdict(&answer, &self.s.cancel)
    }

    /// A step that failed before the tool could run (unknown tool, bad arguments).
    fn failed_step(&self, tool: &str, title: String, access: ToolAccess, detail: &str) {
        let step = Step {
            id: self.s.step_ids.fetch_add(1, Ordering::Relaxed),
            tool: tool.to_owned(),
            title: one_line(&title, DETAIL_CHARS),
            access,
            status: StepStatus::Failed,
            detail: one_line(detail, DETAIL_CHARS),
            changes: Vec::new(),
            at: self.content_len,
            preview: None,
            media: Vec::new(),
            artifacts: Vec::new(),
            delegation: None,
            ask: None,
            views: vec![],
        };
        self.emit(TurnEvent::Step { id: self.s.entry, step });
    }
}

/// A receiver that already holds a failed output.
fn failed_now(text: String) -> Receiver<(ToolOutput, Vec<UndoOp>, bool)> {
    let (tx, rx) = channel();
    let _ = tx.send((ToolOutput::err(text), Vec::new(), false));
    rx
}

/// Most characters of a question's text or warning on an approval card.
const MAX_ASK_TEXT: usize = 1500;

/// Lets a running tool ask the user (a [`Question`]): an extra step with an approval card,
/// decided by the engine like any call, except that grants never answer it.
struct TurnAsker {
    inbox: Sender<Input>,
    child: Option<u64>,
    entry: u64,
    step_ids: Arc<AtomicU64>,
    cancel: Arc<AtomicBool>,
    /// The asking tool's name.
    tool: String,
    at: usize,
}

impl Asker for TurnAsker {
    fn ask(&self, q: &Question) -> Answer {
        let mut step = Step {
            id: self.step_ids.fetch_add(1, Ordering::Relaxed),
            tool: self.tool.clone(),
            title: one_line(&q.title, DETAIL_CHARS),
            access: to_protocol(q.access),
            status: StepStatus::Running,
            detail: one_line(&q.text, DETAIL_CHARS),
            changes: Vec::new(),
            at: self.at,
            preview: None,
            media: Vec::new(),
            artifacts: Vec::new(),
            delegation: None,
            ask: Some(Box::new(Ask {
                text: prefix(&q.text, MAX_ASK_TEXT).to_owned(),
                warning: q.warning.as_deref().map(|w| prefix(w, MAX_ASK_TEXT).to_owned()),
                always: q.always_ask,
            })),
            views: vec![],
        };
        emit_to(&self.inbox, self.child, TurnEvent::Step { id: self.entry, step: step.clone() });
        let (reply, answer) = channel();
        emit_to(&self.inbox, self.child, TurnEvent::Gate { id: self.entry, step: step.clone(), reply });
        let verdict = wait_verdict(&answer, &self.cancel);
        let answer = match &verdict {
            Verdict::Allow => Answer::AutoYes,
            Verdict::Deny(_) => Answer::AutoNo,
            Verdict::Answered(true) => Answer::Yes,
            Verdict::Answered(false) => Answer::No,
        };
        step.status = if answer.allowed() { StepStatus::Done } else { StepStatus::Denied };
        if let Some(reason) = verdict.denied() {
            step.detail = one_line(&reason, DETAIL_CHARS);
        }
        emit_to(&self.inbox, self.child, TurnEvent::Step { id: self.entry, step });
        answer
    }
}

/// Visible text streamed so far in a turn.
struct Shown {
    /// Bytes sent (for step offsets).
    len: usize,
    /// The next text starts a new paragraph (a later round's text).
    separate: bool,
    /// Newlines the text sent so far ends with (counts towards the paragraph break).
    tail_newlines: usize,
}

impl Shown {
    fn new(len: usize) -> Self {
        Self { len, separate: len > 0, tail_newlines: 0 }
    }

    fn send(&mut self, inbox: &Sender<Input>, child: Option<u64>, id: u64, mut content: String, reasoning: String) {
        if content.is_empty() && reasoning.is_empty() {
            return;
        }
        if !content.is_empty() && self.separate {
            self.separate = false;
            content.insert_str(0, &"\n".repeat(2usize.saturating_sub(self.tail_newlines)));
        }
        if !content.is_empty() {
            let trailing = content.len() - content.trim_end_matches('\n').len();
            self.tail_newlines = if trailing == content.len() { self.tail_newlines.saturating_add(trailing) } else { trailing };
        }
        self.len = self.len.saturating_add(content.len());
        emit_to(inbox, child, TurnEvent::Delta { id, content, reasoning });
    }
}

fn wait_verdict(answer: &Receiver<Verdict>, cancel: &AtomicBool) -> Verdict {
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Verdict::Deny("stopped".into());
        }
        match answer.recv_timeout(APPROVAL_POLL) {
            Ok(v) => return v,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Verdict::Deny("no answer".into()),
        }
    }
}

fn to_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// The tool definitions of a request, by name, except that concurrent tools (`delegate`)
/// come last.
pub(crate) fn specs(tools: &Registry) -> Vec<byob_llm::ToolSpec> {
    let mut out: Vec<byob_llm::ToolSpec> =
        tools.specs().into_iter().map(|s| byob_llm::ToolSpec { name: s.name, description: s.description, parameters: s.parameters }).collect();
    out.sort_by_key(|s| tools.get(&s.name).is_some_and(|t| t.concurrent()));
    out
}

/// Drop empty slots (gaps from sparse stream indices) and give every call a unique id,
/// since some servers omit or reuse them.
fn usable_calls(calls: Vec<ToolCall>, entry: u64) -> Vec<ToolCall> {
    let mut ids = BTreeSet::new();
    let mut out = Vec::new();
    for (i, mut call) in calls.into_iter().enumerate() {
        if call.name.trim().is_empty() && call.arguments.trim().is_empty() {
            continue;
        }
        if call.id.trim().is_empty() || ids.contains(&call.id) {
            call.id = format!("call_{entry}_{i}");
        }
        ids.insert(call.id.clone());
        out.push(call);
    }
    out
}

/// Tool-call arguments are untrusted model output. Empty means no arguments; anything
/// else must be a JSON object.
pub(crate) fn parse_arguments(raw: &str) -> Result<Value, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(Value::Object(serde_json::Map::new()));
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(v @ Value::Object(_)) => Ok(v),
        Ok(other) => Err(format!("the arguments must be a JSON object, not {}", kind(&other))),
        Err(e) => Err(format!("the arguments are not valid JSON ({e})")),
    }
}

fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn to_protocol(a: Access) -> ToolAccess {
    match a {
        Access::Read => ToolAccess::Read,
        Access::Write => ToolAccess::Write,
        Access::Network => ToolAccess::Network,
        Access::Execute => ToolAccess::Execute,
    }
}

fn display_path(p: &Path, workspace: &Path) -> String {
    p.strip_prefix(workspace).unwrap_or(p).display().to_string()
}

fn display_change(c: &byob_tools::FileChange, workspace: &Path) -> FileChange {
    let d = |p: &Path| display_path(p, workspace);
    match c {
        byob_tools::FileChange::Created(p) => FileChange::Created { path: d(p) },
        byob_tools::FileChange::Modified(p) => FileChange::Modified { path: d(p) },
        byob_tools::FileChange::Deleted(p) => FileChange::Deleted { path: d(p) },
        byob_tools::FileChange::Moved { from, to } => FileChange::Moved { from: d(from), to: d(to) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_are_parsed_defensively() {
        assert_eq!(parse_arguments("").unwrap(), serde_json::json!({}));
        assert_eq!(parse_arguments(" {\"a\":1} ").unwrap(), serde_json::json!({"a":1}));
        assert!(parse_arguments("{\"a\":").unwrap_err().contains("not valid JSON"));
        assert!(parse_arguments("[1]").unwrap_err().contains("an array"));
        assert!(parse_arguments("\"x\"").unwrap_err().contains("a string"));
    }

    #[test]
    fn calls_get_unique_ids_and_gaps_are_dropped() {
        let call = |id: &str, name: &str| ToolCall { id: id.into(), name: name.into(), arguments: String::new() };
        let out = usable_calls(vec![call("a", "x"), call("", ""), call("a", "y"), call("", "z")], 9);
        let ids: Vec<_> = out.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["a", "call_9_2", "call_9_3"]);
    }

    #[test]
    fn changes_are_shown_relative_to_the_workspace() {
        let ws = Path::new("/w");
        let c = display_change(&byob_tools::FileChange::Moved { from: "/w/a.txt".into(), to: "/elsewhere/b".into() }, ws);
        assert_eq!(c, FileChange::Moved { from: "a.txt".into(), to: "/elsewhere/b".into() });
    }

    #[test]
    fn cancel_while_waiting_denies() {
        let (_tx, rx) = channel();
        let cancel = AtomicBool::new(true);
        assert_eq!(wait_verdict(&rx, &cancel), Verdict::Deny("stopped".into()));
        let (tx, rx) = channel::<Verdict>();
        drop(tx);
        assert!(matches!(wait_verdict(&rx, &AtomicBool::new(false)), Verdict::Deny(_)));
    }
}
