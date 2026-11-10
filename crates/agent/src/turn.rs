//! One assistant turn on a worker thread: stream a reply; while the model asks for tools,
//! run them (through the engine's permission gate) and call the model again with the
//! results. Everything visible is reported back into the engine inbox as [`TurnEvent`]s.

use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

use byob_llm::{ChatMessage, LlmError, StreamEvent, ToolCall, ToolCallFilter, Usage, toolprompt};
use byob_protocol::{Ask, FileChange, Step, StepStatus, ToolAccess, TurnStats};
use byob_tools::{Access, Answer, Asker, Journal, Question, Registry, Tool, ToolContext, ToolOutput, UndoOp};
use serde_json::Value;

use crate::{Input, adapt, compact, delegate};

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
    pub tools: Registry,
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
    /// The turn may hand tasks to subagents (top-level turns only: subagents can't delegate).
    pub delegate: Option<Arc<delegate::Env>>,
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
    /// Compactions made, and whether one failed (then this turn stops trying).
    compactions: usize,
    compact_failed: bool,
    /// Estimated request tokens right after this turn's last compaction.
    compacted_to: usize,
    /// The final answer (text of the round that made no tool calls).
    answer: String,
    /// All visible text streamed (kept for child turns' reports).
    content: String,
    /// Tools that ran, with counts.
    ran: BTreeMap<String, usize>,
}

/// A `delegate` call as the tool loop sees it.
enum Delegated {
    /// Running in the background; the report arrives on the receiver.
    Started { key: String, report: Receiver<String> },
    /// Refused or failed at once; this is the result.
    Done(String),
}

impl Turn {
    fn new(s: Setup) -> Self {
        let messages = s.history.clone();
        let started = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        let tag = s.child.map(|c| format!("-a{c}")).unwrap_or_default();
        let journal = Journal::new(s.workspace.as_path(), &format!("{}-{started}{tag}", s.entry));
        Self {
            s,
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
            compactions: 0,
            compact_failed: false,
            compacted_to: 0,
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
        (!self.s.tools.is_empty() || self.s.delegate.is_some()) && self.calls < self.s.max_calls && self.repeats < MAX_REPEATS
    }

    /// The tool definitions of a request: the registry's, plus `delegate` if allowed.
    fn specs(&self) -> Vec<byob_llm::ToolSpec> {
        let mut out = specs(&self.s.tools);
        if let Some(env) = &self.s.delegate {
            out.push(env.spec());
        }
        out
    }

    fn run(&mut self) -> Result<(), End> {
        for round in 0..MAX_ROUNDS {
            let tools_on = self.tools_on();
            let specs = if tools_on { self.specs() } else { Vec::new() };
            self.maybe_compact(&specs, round)?;
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

    /// Before a model request: if it would pass the threshold, summarise the conversation
    /// and continue from the summary. Before the first request the newest turns stay
    /// verbatim; later (inside the tool loop) everything is summarised and the request is
    /// repeated after the summary. A failed summary leaves the messages alone (fit_history
    /// still shortens them) and turns compaction off for the rest of the turn.
    fn maybe_compact(&mut self, specs: &[byob_llm::ToolSpec], round: usize) -> Result<(), End> {
        let Some(auto) = self.s.compact else { return Ok(()) };
        if self.compact_failed || self.compactions >= compact::MAX_PER_TURN {
            return Ok(());
        }
        let before = self.s.plan.raw_tokens(&self.messages, specs);
        // After a compaction in this turn, wait until the request has grown again by a
        // quarter of the threshold (a threshold just above the fixed prompt would churn).
        if before <= auto.at || before < self.compacted_to.saturating_add(auto.at / 4) {
            return Ok(());
        }
        let start = compact::content_start(&self.messages);
        let (covered, tail, through) = if round == 0 {
            let end = self.messages.len().saturating_sub(1);
            let Some(split) = compact::split_point(&self.messages, end, auto.keep_turns, auto.at / 4) else { return Ok(()) };
            let through = self.s.ids.get(..split).and_then(|ids| ids.iter().rev().find(|id| **id > 0)).copied().unwrap_or(0);
            (self.messages.get(start..split).unwrap_or_default().to_vec(), self.messages.get(split..).unwrap_or_default().to_vec(), through)
        } else {
            let Some(resume) = compact::resume_message(&self.messages) else { return Ok(()) };
            (self.messages.get(start..).unwrap_or_default().to_vec(), vec![resume], self.s.prior)
        };
        // Not worth a model call if little but an earlier summary would be summarised (the
        // threshold may sit just above the system prompt and tool definitions).
        if compact::fresh_tokens(&covered) < auto.at / 4 {
            return Ok(());
        }
        log::info!("compacting the context of entry {} (~{before} tokens, threshold {})", self.s.entry, auto.at);
        let id = self.s.entry;
        self.emit(TurnEvent::Compacting { id, on: true });
        let result = compact::summarize(&self.s.llm, &covered, None, &self.s.plan, &self.s.cancel);
        self.emit(TurnEvent::Compacting { id, on: false });
        let summary = match result {
            Ok(s) => s,
            Err(compact::Failure::Stopped) => return Err(End::Stopped),
            Err(compact::Failure::Error(e)) => {
                log::warn!("compaction failed: {e}");
                self.compact_failed = true;
                let text = format!("Couldn't summarise the conversation to free context ({e}); older messages were shortened instead.");
                self.emit(TurnEvent::Notice { id, text });
                return Ok(());
            }
        };
        self.model_calls = self.model_calls.saturating_add(1);
        self.compactions = self.compactions.saturating_add(1);
        let (prompt, completion) = summary.usage.map_or((0, 0), |u| (u.prompt_tokens, u.completion_tokens));
        self.add_usage(prompt, completion);
        let mut messages: Vec<ChatMessage> = self.messages.get(..start).unwrap_or_default().to_vec();
        messages.extend(compact::summary_messages(&summary.text));
        messages.extend(tail);
        let after = self.s.plan.raw_tokens(&messages, specs);
        if after >= before {
            // A summary longer than what it replaces: keep the messages, stop trying.
            self.compact_failed = true;
            self.emit(TurnEvent::Usage { id, prompt, completion, context: to_u64(before), estimated: true });
            let text = "The conversation summary came out longer than the conversation; older messages were shortened instead.".to_owned();
            self.emit(TurnEvent::Notice { id, text });
            return Ok(());
        }
        self.messages = messages;
        self.compacted_to = after;
        self.emit(TurnEvent::Usage { id, prompt, completion, context: to_u64(after), estimated: true });
        let made = Compacted {
            through,
            summary: summary.text,
            tokens_before: to_u64(before),
            tokens_after: to_u64(after),
            prompt_tokens: prompt,
            completion_tokens: completion,
        };
        self.emit(TurnEvent::Compacted { id, made });
        Ok(())
    }

    /// Run one round's calls and return their results in order. Delegations run in the
    /// background, side by side (as many at once as there are free inference slots); the
    /// other calls run one after another meanwhile, in order.
    fn call_all(&mut self, calls: &[ToolCall]) -> Vec<String> {
        let mut results: Vec<Option<String>> = vec![None; calls.len()];
        let mut waiting: Vec<(usize, String, Receiver<String>)> = Vec::new();
        for (i, call) in calls.iter().enumerate() {
            if self.s.delegate.is_some() && call.name.trim() == delegate::TOOL {
                match self.delegate(call) {
                    Delegated::Started { key, report } => waiting.push((i, key, report)),
                    Delegated::Done(text) => results[i] = Some(text),
                }
            }
        }
        for (i, call) in calls.iter().enumerate() {
            if results[i].is_none() && !waiting.iter().any(|w| w.0 == i) {
                results[i] = Some(self.call(call));
            }
        }
        for (i, key, report) in waiting {
            // The subagent thread always answers; a closed channel means it crashed.
            let text = report.recv().unwrap_or_else(|_| "Error: the subagent stopped unexpectedly; no report.".to_owned());
            let text = self.charge(text);
            self.seen.insert(key, text.clone());
            results[i] = Some(text);
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

    /// Start a `delegate` call: check it here, then run the subagent on its own thread.
    fn delegate(&mut self, call: &ToolCall) -> Delegated {
        if self.cancelled() {
            return Delegated::Done("Stopped by the user.".into());
        }
        if let Some(refused) = self.count_call() {
            return Delegated::Done(refused);
        }
        let Some(env) = self.s.delegate.clone() else { return Delegated::Done("Error: delegation is not available here.".into()) };
        let args = match parse_arguments(&call.arguments) {
            Ok(args) => args,
            Err(e) => {
                self.failed_step(delegate::TOOL, "delegate: unreadable arguments".into(), ToolAccess::Read, &e);
                return Delegated::Done(format!("Error: {e}. Call delegate again with a JSON object matching its parameters."));
            }
        };
        let key = format!("{}:{args}", delegate::TOOL);
        if let Some(earlier) = self.seen.get(&key) {
            self.repeats = self.repeats.saturating_add(1);
            return Delegated::Done(format!(
                "You already delegated exactly this in this turn. The report was:\n{}\nDon't repeat it; continue with something else or answer.",
                prefix(earlier, REPEAT_QUOTE)
            ));
        }
        let job = match env.prepare(&args, &self.s.tools) {
            Ok(job) => job,
            Err(e) => {
                self.failed_step(delegate::TOOL, "delegate: not started".into(), ToolAccess::Read, &e);
                return Delegated::Done(format!("Error: {e}"));
            }
        };
        let launch = delegate::Launch {
            entry: self.s.entry,
            step_id: self.s.step_ids.fetch_add(1, Ordering::Relaxed),
            at: self.content_len,
            inbox: self.s.inbox.clone(),
            cancel: Arc::clone(&self.s.cancel),
            workspace: Arc::clone(&self.s.workspace),
            chat: self.s.chat.clone(),
            step_ids: Arc::clone(&self.s.step_ids),
        };
        match delegate::launch(&env, job, launch) {
            Ok(report) => {
                *self.ran.entry(delegate::TOOL.to_owned()).or_default() += 1;
                Delegated::Started { key, report }
            }
            Err(e) => Delegated::Done(format!("Error: couldn't start the subagent: {e}")),
        }
    }

    /// Run one call and return what the model sees.
    fn call(&mut self, call: &ToolCall) -> String {
        if self.cancelled() {
            return "Stopped by the user.".into();
        }
        if let Some(refused) = self.count_call() {
            return refused;
        }
        let name = call.name.trim();
        let Some(tool) = self.s.tools.get(name).cloned() else {
            self.failed_step(name, format!("Unknown tool {name:?}"), ToolAccess::Read, "no such tool");
            let known: Vec<String> = self.specs().into_iter().map(|s| s.name).collect();
            return format!("Error: there is no tool called {name:?}. Available tools: {}.", known.join(", "));
        };
        let args = match parse_arguments(&call.arguments) {
            Ok(args) => args,
            Err(e) => {
                self.failed_step(name, format!("{name}: unreadable arguments"), ToolAccess::Read, &e);
                return format!("Error: {e}. Call {name} again with a JSON object matching its parameters.");
            }
        };
        let key = format!("{name}:{args}");
        if let Some(earlier) = self.seen.get(&key) {
            self.repeats = self.repeats.saturating_add(1);
            return format!(
                "You already called {name} with exactly these arguments in this turn. The result was:\n{}\nDon't repeat it; continue with something else or answer.",
                prefix(earlier, REPEAT_QUOTE)
            );
        }
        let text = self.run_tool(tool.as_ref(), name, &args);
        self.seen.insert(key, text.clone());
        text
    }

    fn run_tool(&mut self, tool: &dyn Tool, name: &str, args: &Value) -> String {
        // Tool code is not ours to trust either: a panic in title/access/preview/run fails the call.
        let title = catch_unwind(AssertUnwindSafe(|| tool.pending_title(args))).unwrap_or_else(|_| name.to_owned());
        // If access() can't say, assume the most dangerous.
        let access = catch_unwind(AssertUnwindSafe(|| tool.access(args))).map_or(ToolAccess::Execute, to_protocol);
        let preview = catch_unwind(AssertUnwindSafe(|| tool.preview(args, self.s.workspace.as_path()))).ok().flatten().map(crate::steps::bound_preview);
        let mut step = Step {
            id: self.s.step_ids.fetch_add(1, Ordering::Relaxed),
            tool: name.to_owned(),
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
            ask: catch_unwind(AssertUnwindSafe(|| tool.always_ask(args)))
                .unwrap_or_else(|_| Some("this tool must be confirmed".into()))
                .map(|w| Box::new(Ask { text: String::new(), warning: Some(prefix(&w, MAX_ASK_TEXT).to_owned()), always: true })),
        };
        self.emit(TurnEvent::Step { id: self.s.entry, step: step.clone() });
        if let Some(reason) = self.gate(&step).denied() {
            step.status = StepStatus::Denied;
            step.detail = one_line(&reason, DETAIL_CHARS);
            self.emit(TurnEvent::Step { id: self.s.entry, step });
            if self.cancelled() {
                return "Stopped by the user.".into();
            }
            return format!("Not run: {reason}. Don't retry this call; tell the user what you would have done.");
        }
        let asker = TurnAsker {
            inbox: self.s.inbox.clone(),
            child: self.s.child,
            entry: self.s.entry,
            step_ids: Arc::clone(&self.s.step_ids),
            cancel: Arc::clone(&self.s.cancel),
            tool: name.to_owned(),
            at: self.content_len,
        };
        let ctx = ToolContext::new(self.s.workspace.as_path(), &self.s.cancel).with_journal(&self.journal).with_chat(&self.s.chat).with_asker(&asker);
        let output = catch_unwind(AssertUnwindSafe(|| tool.run(args, &ctx))).unwrap_or_else(|_| ToolOutput::err("the tool crashed"));
        let (ops, complete) = crate::steps::undo_ops(self.journal.take(), &output.changes);
        if !ops.is_empty() || !complete {
            self.emit(TurnEvent::Undo { id: self.s.entry, ops, complete });
        }
        *self.ran.entry(name.to_owned()).or_default() += 1;
        let mut text = self.charge(output.text);
        step.status = if output.failed { StepStatus::Failed } else { StepStatus::Done };
        if !output.failed {
            let done = catch_unwind(AssertUnwindSafe(|| tool.title(args))).unwrap_or_else(|_| name.to_owned());
            step.title = one_line(&done, DETAIL_CHARS);
        }
        step.detail = one_line(&text, DETAIL_CHARS);
        step.changes = output.changes.iter().map(|c| display_change(c, &self.s.workspace)).collect();
        step.media = crate::attachments::media_for_changes(&output.changes, &self.s.workspace);
        if !output.failed {
            step.artifacts = byob_tools::artifact::for_call(&self.s.workspace, &self.s.chat, &output.changes, &step.media);
        }
        self.emit(TurnEvent::Step { id: self.s.entry, step });
        if output.failed && !text.starts_with("Error") {
            text.insert_str(0, "Error: ");
        }
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
        };
        self.emit(TurnEvent::Step { id: self.s.entry, step });
    }
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

pub(crate) fn specs(tools: &Registry) -> Vec<byob_llm::ToolSpec> {
    tools.specs().into_iter().map(|s| byob_llm::ToolSpec { name: s.name, description: s.description, parameters: s.parameters }).collect()
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
