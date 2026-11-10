//! Chats on disk: mapping between `yah-store` rows and protocol types, chat ids, and titles
//! (a first-line heuristic at once, then a short model request after the first reply).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use yah_llm::{ChatMessage, StreamEvent};
use yah_protocol::{
    ArtifactRef, ChatSummary, Delegation, DelegationState, Entry, EntryRole, FileChange, MediaKind, MediaRef, Step, StepStatus, ToolAccess, TurnStats,
};
use yah_store::{AgentRun, Chat, MessageStats, NewStep, Role, StepAccess, StepChange, StepResult, StoredMessage};

use crate::Input;
use crate::limits::{DETAIL_CHARS, one_line, prefix};

/// Title of a chat before anything better is known.
pub const UNTITLED: &str = "New chat";
/// Longest chat title, in characters.
pub const MAX_TITLE_CHARS: usize = 80;
/// Error recorded on replies that were streaming when yah stopped.
pub const INTERRUPTED: &str = "yah stopped before this reply finished";
/// How long the title request may take before it is abandoned.
const TITLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Stop reading the title reply after this much text (it should be a few words).
const TITLE_MAX_BYTES: usize = 400;
/// Reasoning models think first; give up if they think for this long.
const TITLE_MAX_REASONING: usize = 8 * 1024;

pub fn summary(chat: &Chat, busy: bool) -> ChatSummary {
    ChatSummary { id: chat.id.clone(), title: chat.title.clone(), updated_at: chat.updated_at, pinned_at: chat.pinned_at, busy }
}

pub fn store_cursor(c: &yah_protocol::ChatCursor) -> yah_store::ChatCursor {
    yah_store::ChatCursor { updated_at: c.updated_at, id: c.id.clone() }
}

pub fn protocol_cursor(c: &yah_store::ChatCursor) -> yah_protocol::ChatCursor {
    yah_protocol::ChatCursor { updated_at: c.updated_at, id: c.id.clone() }
}

/// A fresh chat id: time-based and readable, unique within this process.
pub fn new_chat_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("c{:x}{n:x}", chrono::Utc::now().timestamp_millis())
}

/// Entry id for a stored message. Message row ids are positive and monotonic.
pub fn entry_id(id: yah_store::MessageId) -> u64 {
    u64::try_from(id.0).unwrap_or(0)
}

pub fn message_id(entry: u64) -> yah_store::MessageId {
    yah_store::MessageId(i64::try_from(entry).unwrap_or(i64::MAX))
}

pub fn to_stats(s: &TurnStats) -> MessageStats {
    MessageStats { prompt_tokens: s.prompt_tokens, completion_tokens: s.completion_tokens, elapsed_ms: s.elapsed_ms, model_calls: s.model_calls }
}

/// A stored compaction for clients.
pub fn compaction(c: &yah_store::Compaction) -> yah_protocol::Compaction {
    yah_protocol::Compaction {
        id: u64::try_from(c.id).unwrap_or(0),
        after_entry: entry_id(c.through),
        tokens_before: c.tokens_before,
        tokens_after: c.tokens_after,
        summary: c.summary.clone(),
        manual: c.manual,
        focus: c.focus.clone(),
        created_at: c.created_at,
    }
}

/// A stored message as a transcript entry. System and tool rows are not shown.
pub fn entry(m: StoredMessage, step_ids: &AtomicU64) -> Option<Entry> {
    let role = match m.role {
        Role::User => EntryRole::User,
        Role::Assistant => EntryRole::Assistant,
        Role::System | Role::Tool => return None,
    };
    let stats = m.stats.map(|s| TurnStats {
        prompt_tokens: s.prompt_tokens,
        completion_tokens: s.completion_tokens,
        elapsed_ms: s.elapsed_ms,
        model_calls: s.model_calls,
    });
    let steps = m.steps.into_iter().map(|s| step(s, step_ids)).collect();
    Some(Entry {
        id: entry_id(m.id),
        role,
        content: m.content,
        reasoning: m.reasoning,
        done: m.done,
        error: m.error,
        stats,
        steps,
        undoable: false,
        undone: false,
        // Attachments and step media aren't stored yet (ROADMAP).
        attachments: Vec::new(),
    })
}

/// A stored step for display. A step without a result was cut off when yah stopped.
fn step(s: yah_store::Step, step_ids: &AtomicU64) -> Step {
    let (summary, at) = step_args(&s.args);
    let (status, detail, changes) = match s.result {
        None => (StepStatus::Failed, "interrupted".to_owned(), Vec::new()),
        Some(r) => {
            let status = if r.failed { StepStatus::Failed } else { StepStatus::Done };
            (status, one_line(&r.output, DETAIL_CHARS), r.changes.iter().map(from_change).collect())
        }
    };
    let detail = if detail.is_empty() { summary } else { detail };
    Step {
        id: step_ids.fetch_add(1, Ordering::Relaxed),
        tool: s.tool,
        title: s.title,
        access: from_access(s.access),
        status,
        detail,
        changes,
        at,
        preview: None,
        media: Vec::new(),
        artifacts: Vec::new(),
        delegation: None,
    }
}

/// Where each stored run belongs: (entry id, index into that entry's steps). Runs whose
/// step can't be found are left out (`None`).
pub fn run_links(messages: &[StoredMessage], runs: &[AgentRun]) -> Vec<Option<(u64, usize)>> {
    runs.iter()
        .map(|r| {
            let m = messages.iter().find(|m| m.id == r.parent)?;
            let i = m.steps.iter().position(|s| s.seq == r.step_seq)?;
            Some((entry_id(m.id), i))
        })
        .collect()
}

/// Put stored subagent runs into the `delegate` steps that started them.
pub fn attach_runs(entries: &mut [Entry], links: Vec<Option<(u64, usize)>>, runs: Vec<AgentRun>, step_ids: &AtomicU64) {
    for (link, run) in links.into_iter().zip(runs) {
        let Some((entry, i)) = link else { continue };
        let Some(step) = entries.iter_mut().find(|e| e.id == entry).and_then(|e| e.steps.get_mut(i)) else { continue };
        step.delegation = Some(Box::new(delegation(run, step_ids)));
    }
}

/// A stored run as a step's delegation. One that never finished was cut off when yah stopped.
fn delegation(run: AgentRun, step_ids: &AtomicU64) -> Delegation {
    let (state, error) = match (run.done, run.error) {
        (false, _) => (DelegationState::Stopped, Some("interrupted".to_owned())),
        (true, None) => (DelegationState::Done, None),
        (true, Some(e)) if e == "stopped" || e.starts_with("time limit") => (DelegationState::Stopped, Some(e)),
        (true, Some(e)) => (DelegationState::Failed, Some(e)),
    };
    let stats = run.stats.unwrap_or_default();
    Delegation {
        agent: run.agent,
        task: run.task,
        model: run.model,
        state,
        content: run.content,
        reasoning: run.reasoning,
        steps: run.steps.into_iter().map(|s| step(s, step_ids)).collect(),
        prompt_tokens: stats.prompt_tokens,
        completion_tokens: stats.completion_tokens,
        error,
    }
}

/// What we store as a step's "args": the one-line argument summary the user saw, and the
/// step's offset into the reply (for interleaving). Older or foreign rows may be anything.
pub fn new_step(step: &Step) -> NewStep {
    NewStep { tool: step.tool.clone(), title: step.title.clone(), access: to_access(step.access), args: json!({ "summary": step.detail, "at": step.at }) }
}

fn step_args(args: &Value) -> (String, usize) {
    let summary = match args.get("summary") {
        Some(Value::String(s)) => s.clone(),
        _ => one_line(&args.to_string(), DETAIL_CHARS),
    };
    let at = args.get("at").and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()).unwrap_or(0);
    (summary, at)
}

pub fn step_result(step: &Step) -> StepResult {
    StepResult { output: step.detail.clone(), failed: step.status != StepStatus::Done, changes: step.changes.iter().map(to_change).collect() }
}

fn to_access(a: ToolAccess) -> StepAccess {
    match a {
        ToolAccess::Read => StepAccess::Read,
        ToolAccess::Write => StepAccess::Write,
        ToolAccess::Network => StepAccess::Network,
        ToolAccess::Execute => StepAccess::Execute,
    }
}

fn from_access(a: StepAccess) -> ToolAccess {
    match a {
        StepAccess::Read => ToolAccess::Read,
        StepAccess::Write => ToolAccess::Write,
        StepAccess::Network => ToolAccess::Network,
        StepAccess::Execute => ToolAccess::Execute,
    }
}

fn to_change(c: &FileChange) -> StepChange {
    match c {
        FileChange::Created { path } => StepChange::Created { path: path.into() },
        FileChange::Modified { path } => StepChange::Modified { path: path.into() },
        FileChange::Deleted { path } => StepChange::Deleted { path: path.into() },
        FileChange::Moved { from, to } => StepChange::Moved { from: from.into(), to: to.into() },
    }
}

fn from_change(c: &StepChange) -> FileChange {
    let s = |p: &std::path::Path| p.display().to_string();
    match c {
        StepChange::Created { path } => FileChange::Created { path: s(path) },
        StepChange::Modified { path } => FileChange::Modified { path: s(path) },
        StepChange::Deleted { path } => FileChange::Deleted { path: s(path) },
        StepChange::Moved { from, to } => FileChange::Moved { from: s(from), to: s(to) },
    }
}

/// Steps loaded from the store only have their file changes. Rebuild from those what a
/// live step carried: the charts and images it made, and the artifact versions.
pub fn restore_step_files(entries: &mut [Entry], artifacts: &[yah_protocol::Artifact]) {
    let same = |a: &str, b: &str| {
        let (a, b) = (a.replace('\\', "/"), b.replace('\\', "/"));
        a == b || a.ends_with(&format!("/{b}"))
    };
    for step in entries.iter_mut().flat_map(|e| e.steps.iter_mut()) {
        if step.status != StepStatus::Done {
            continue;
        }
        let made: Vec<&str> = step
            .changes
            .iter()
            .filter_map(|c| match c {
                FileChange::Created { path } | FileChange::Modified { path } | FileChange::Moved { to: path, .. } => Some(path.as_str()),
                FileChange::Deleted { .. } => None,
            })
            .collect();
        for path in made {
            let r = MediaRef::new(path, 0);
            if r.kind != MediaKind::File && !step.media.iter().any(|m| m.path == r.path) {
                step.media.push(r);
            }
            let found = yah_tools::artifact::parse_version_path(path).or_else(|| {
                artifacts.iter().find_map(|a| a.versions.iter().find(|v| same(path, &v.path)).map(|v| ArtifactRef { id: a.id.clone(), version: v.n }))
            });
            if let Some(r) = found.filter(|r| artifacts.iter().any(|a| a.id == r.id) && !step.artifacts.contains(r)) {
                step.artifacts.push(r);
            }
        }
    }
}

/// A title from the first message: its first non-empty line, shortened.
pub fn heuristic_title(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
    let title = clean_title(line, 48);
    if title.is_empty() { UNTITLED.to_owned() } else { title }
}

/// Normalise a title from a user or a model: one line, no wrapping quotes or "Title:"
/// prefix, at most `max` characters.
pub fn clean_title(raw: &str, max: usize) -> String {
    let line = raw.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
    let line = line.strip_prefix("Title:").or_else(|| line.strip_prefix("title:")).unwrap_or(line);
    let line = line.trim().trim_matches(|c: char| matches!(c, '"' | '\'' | '*' | '#' | '`' | '“' | '”')).trim();
    let line = line.strip_suffix('.').unwrap_or(line);
    one_line(line, max)
}

/// Ask the model for a short title in the background; the answer arrives as
/// [`Input::Titled`]. Never streams to clients and gives up quietly.
pub fn spawn_titler(llm: yah_llm::Client, chat: String, user: String, reply: String, inbox: Sender<Input>) {
    let spawned = thread::Builder::new().name("yah-title".into()).spawn(move || {
        let convo = format!("User: {}\n\nAssistant: {}", prefix(&user, 1500), prefix(&reply, 1500));
        let messages = vec![
            ChatMessage::system(
                "You name conversations. Reply with only a short title (2 to 6 words) for the conversation below: no quotes, no punctuation at the end.",
            ),
            ChatMessage::user(convo),
        ];
        let cancel = Arc::new(AtomicBool::new(false));
        let started = Instant::now();
        let (mut content, mut reasoning) = (0usize, 0usize);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            llm.stream_chat(&messages, &[], &cancel, |ev| {
                match ev {
                    StreamEvent::Content(t) => content = content.saturating_add(t.len()),
                    StreamEvent::Reasoning(t) => reasoning = reasoning.saturating_add(t.len()),
                }
                if content > TITLE_MAX_BYTES || reasoning > TITLE_MAX_REASONING || started.elapsed() > TITLE_TIMEOUT {
                    cancel.store(true, Ordering::Relaxed);
                }
            })
        }));
        let title = match result {
            Ok(Ok(out)) => Some(clean_title(&out.content, MAX_TITLE_CHARS)).filter(|t| !t.is_empty()),
            Ok(Err(e)) => {
                log::info!("chat title request failed: {e}");
                None
            }
            Err(_) => None,
        };
        if let Some(title) = title {
            let _ = inbox.send(Input::Titled { chat, title });
        }
    });
    if let Err(e) = spawned {
        log::warn!("could not start the title request: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_are_cleaned() {
        assert_eq!(heuristic_title("\n\n  Plan my trip to Rome\nwith details"), "Plan my trip to Rome");
        assert_eq!(heuristic_title("   "), UNTITLED);
        assert_eq!(clean_title("Title: \"Rome Trip Plans.\"", 80), "Rome Trip Plans");
        assert_eq!(clean_title("**Budget review**\nextra", 80), "Budget review");
        assert_eq!(heuristic_title(&"word ".repeat(40)).chars().count(), 48);
    }

    #[test]
    fn steps_round_trip_through_store_types() {
        let s = Step {
            id: 1,
            tool: "write_file".into(),
            title: "Write a.txt".into(),
            access: ToolAccess::Write,
            status: StepStatus::Done,
            detail: "ok".into(),
            changes: vec![FileChange::Moved { from: "a".into(), to: "b".into() }],
            at: 12,
            preview: None,
            media: vec![],
            artifacts: vec![],
            delegation: None,
        };
        let new = new_step(&s);
        let stored = yah_store::Step {
            message_id: yah_store::MessageId(3),
            seq: 1,
            tool: new.tool,
            title: new.title,
            access: new.access,
            args: new.args,
            result: Some(step_result(&s)),
        };
        let back = step(stored.clone(), &AtomicU64::new(7));
        assert_eq!(back, Step { id: 7, ..s });
        let cut = step(yah_store::Step { result: None, args: Value::String("raw".into()), ..stored }, &AtomicU64::new(0));
        assert_eq!((cut.status, cut.detail.as_str(), cut.at), (StepStatus::Failed, "interrupted", 0));
    }
}
