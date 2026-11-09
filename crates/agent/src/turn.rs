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

use serde_json::Value;
use yah_llm::{ChatMessage, LlmError, StreamEvent, ToolCall, ToolCallFilter, Usage, toolprompt};
use yah_protocol::{Entry, EntryRole, FileChange, Step, StepStatus, ToolAccess, TurnStats};
use yah_tools::{Access, Journal, Registry, Tool, ToolContext, ToolOutput, UndoOp};

use crate::{Input, adapt};

use crate::limits::{DETAIL_CHARS, MAX_CALLS, MAX_OUTPUT, MAX_REPEATS, MAX_ROUNDS, REPEAT_QUOTE, TURN_OUTPUT_BUDGET, cap, one_line, prefix};

pub const DEFAULT_SYSTEM_PROMPT: &str = "You are yah, a helpful assistant running inside a desktop harness. Be concise and accurate. \
Use the provided tools when they help; when a tool fails or the user declines it, explain instead of retrying the same call.";

/// How often a turn blocked on an approval re-checks its cancel flag.
const APPROVAL_POLL: Duration = Duration::from_millis(50);

/// The engine's answer to a [`TurnEvent::Gate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    /// Not run; the reason is shown on the step.
    Deny(String),
}

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
}

/// Build the request transcript from finished entries plus the new user message.
/// Reasoning and earlier tool steps are not sent back; failed or empty replies are skipped.
pub fn history(transcript: &[Entry], new_user_text: &str, system_prompt: &str) -> Vec<ChatMessage> {
    let mut out = vec![ChatMessage::system(system_prompt)];
    for e in transcript.iter().filter(|e| e.done && e.error.is_none() && (!e.content.is_empty() || !e.attachments.is_empty())) {
        out.push(match e.role {
            EntryRole::User => ChatMessage::user(crate::attachments::with_names(&e.content, &e.attachments)),
            EntryRole::Assistant => ChatMessage::assistant(e.content.clone()),
        });
    }
    out.push(ChatMessage::user(new_user_text));
    out
}

/// Everything a turn thread needs.
pub struct Setup {
    pub llm: yah_llm::Client,
    pub history: Vec<ChatMessage>,
    /// The assistant entry this turn writes into.
    pub entry: u64,
    pub cancel: Arc<AtomicBool>,
    pub inbox: Sender<Input>,
    pub tools: Registry,
    pub workspace: Arc<PathBuf>,
    pub step_ids: Arc<AtomicU64>,
    /// Tool-calling mode, history budget and vision for this endpoint.
    pub plan: adapt::Plan,
}

pub fn spawn(setup: Setup) -> std::io::Result<()> {
    thread::Builder::new().name("yah-turn".into()).spawn(move || {
        let started = Instant::now();
        let id = setup.entry;
        let inbox = setup.inbox.clone();
        let mut turn = Turn::new(setup);
        let result = catch_unwind(AssertUnwindSafe(|| turn.run()));
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let stats = TurnStats { prompt_tokens: turn.usage.prompt_tokens, completion_tokens: turn.usage.completion_tokens, elapsed_ms };
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
}

impl Turn {
    fn new(s: Setup) -> Self {
        let messages = s.history.clone();
        let started = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        let journal = Journal::new(s.workspace.as_path(), &format!("{}-{started}", s.entry));
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
        }
    }

    fn cancelled(&self) -> bool {
        self.s.cancel.load(Ordering::Relaxed)
    }

    fn emit(&self, event: TurnEvent) {
        let _ = self.s.inbox.send(Input::Turn(event));
    }

    fn tools_on(&self) -> bool {
        !self.s.tools.is_empty() && self.calls < MAX_CALLS && self.repeats < MAX_REPEATS
    }

    fn run(&mut self) -> Result<(), End> {
        for _ in 0..MAX_ROUNDS {
            let tools_on = self.tools_on();
            let specs = if tools_on { specs(&self.s.tools) } else { Vec::new() };
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
                return Ok(());
            }
            if prompt {
                self.messages.push(toolprompt::fallback_assistant_message(&text, &calls));
            } else {
                let text = Some(text).filter(|c| !c.trim().is_empty());
                self.messages.push(ChatMessage::assistant_tool_calls(text, &calls));
            }
            for call in &calls {
                let result = self.call(call);
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
    fn model_call(&mut self, specs: &[yah_llm::ToolSpec]) -> Result<yah_llm::ChatOutcome, End> {
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
    fn stream(&mut self, specs: &[yah_llm::ToolSpec]) -> Result<yah_llm::ChatOutcome, LlmError> {
        let id = self.s.entry;
        let inbox = &self.s.inbox;
        let prompt = self.s.plan.prompt_tools;
        let request = self.s.plan.request(&self.messages, specs);
        let wire_specs = if prompt { &[][..] } else { specs };
        // Text of a later round starts a new paragraph.
        let mut shown = Shown::new(self.content_len);
        shown.tail_newlines = self.tail_newlines;
        let mut filter = (prompt && !specs.is_empty()).then(ToolCallFilter::default);
        let result = self.s.llm.stream_chat(&request, wire_specs, &self.s.cancel, |ev| {
            let (content, reasoning) = match ev {
                StreamEvent::Content(t) => (t, String::new()),
                StreamEvent::Reasoning(t) => (String::new(), t),
            };
            let content = match filter.as_mut() {
                Some(f) => f.push(&content),
                None => content,
            };
            shown.send(inbox, id, content, reasoning);
        });
        if let Some(f) = filter.as_mut() {
            shown.send(inbox, id, f.finish(), String::new());
        }
        self.content_len = shown.len;
        self.tail_newlines = shown.tail_newlines;
        let out = result?;
        self.rounds_done = true;
        if let Some(u) = out.usage {
            self.usage.prompt_tokens = self.usage.prompt_tokens.saturating_add(u.prompt_tokens);
            self.usage.completion_tokens = self.usage.completion_tokens.saturating_add(u.completion_tokens);
        }
        Ok(out)
    }

    /// Run one call and return what the model sees.
    fn call(&mut self, call: &ToolCall) -> String {
        if self.cancelled() {
            return "Stopped by the user.".into();
        }
        self.calls = self.calls.saturating_add(1);
        if self.calls > MAX_CALLS {
            return format!("Not run: the limit of {MAX_CALLS} tool calls per turn is reached. Answer with what you have.");
        }
        let name = call.name.trim();
        let Some(tool) = self.s.tools.get(name).cloned() else {
            self.failed_step(name, format!("Unknown tool {name:?}"), ToolAccess::Read, "no such tool");
            let known: Vec<String> = self.s.tools.specs().into_iter().map(|s| s.name).collect();
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
        };
        self.emit(TurnEvent::Step { id: self.s.entry, step: step.clone() });
        if let Verdict::Deny(reason) = self.gate(&step) {
            step.status = StepStatus::Denied;
            step.detail = one_line(&reason, DETAIL_CHARS);
            self.emit(TurnEvent::Step { id: self.s.entry, step });
            if self.cancelled() {
                return "Stopped by the user.".into();
            }
            return format!("Not run: {reason}. Don't retry this call; tell the user what you would have done.");
        }
        let ctx = ToolContext::new(self.s.workspace.as_path(), &self.s.cancel).with_journal(&self.journal);
        let output = catch_unwind(AssertUnwindSafe(|| tool.run(args, &ctx))).unwrap_or_else(|_| ToolOutput::err("the tool crashed"));
        let (ops, complete) = crate::steps::undo_ops(self.journal.take(), &output.changes);
        if !ops.is_empty() || !complete {
            self.emit(TurnEvent::Undo { id: self.s.entry, ops, complete });
        }
        let mut text = cap(output.text, MAX_OUTPUT, "the tool's output is longer than one call may return");
        let remaining = TURN_OUTPUT_BUDGET.saturating_sub(self.output_used);
        text = cap(text, remaining, "this turn's tool output budget is used up; answer with what you have");
        self.output_used = self.output_used.saturating_add(text.len());
        step.status = if output.failed { StepStatus::Failed } else { StepStatus::Done };
        if !output.failed {
            let done = catch_unwind(AssertUnwindSafe(|| tool.title(args))).unwrap_or_else(|_| name.to_owned());
            step.title = one_line(&done, DETAIL_CHARS);
        }
        step.detail = one_line(&text, DETAIL_CHARS);
        step.changes = output.changes.iter().map(|c| display_change(c, &self.s.workspace)).collect();
        step.media = crate::attachments::media_for_changes(&output.changes, &self.s.workspace);
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
        };
        self.emit(TurnEvent::Step { id: self.s.entry, step });
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

    fn send(&mut self, inbox: &Sender<Input>, id: u64, mut content: String, reasoning: String) {
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
        let _ = inbox.send(Input::Turn(TurnEvent::Delta { id, content, reasoning }));
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

fn specs(tools: &Registry) -> Vec<yah_llm::ToolSpec> {
    tools.specs().into_iter().map(|s| yah_llm::ToolSpec { name: s.name, description: s.description, parameters: s.parameters }).collect()
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

fn display_change(c: &yah_tools::FileChange, workspace: &Path) -> FileChange {
    let d = |p: &Path| display_path(p, workspace);
    match c {
        yah_tools::FileChange::Created(p) => FileChange::Created { path: d(p) },
        yah_tools::FileChange::Modified(p) => FileChange::Modified { path: d(p) },
        yah_tools::FileChange::Deleted(p) => FileChange::Deleted { path: d(p) },
        yah_tools::FileChange::Moved { from, to } => FileChange::Moved { from: d(from), to: d(to) },
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
        let c = display_change(&yah_tools::FileChange::Moved { from: "/w/a.txt".into(), to: "/elsewhere/b".into() }, ws);
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
