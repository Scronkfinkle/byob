//! The `events.jsonl` line format, and reading/appending it safely.
//!
//! Every line is one self-describing JSON object: `{"v":1,"ts":<ms>,"e":"<kind>",…}`.
//! Messages are referred to by `m`, their 1-based number within the chat (never reused).

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Result, StoreError};
use crate::model::{MessageStats, Millis, Role, StepAccess, StepChange, UndoOp};

/// Version written into every line. Readers accept any version and skip (with a warning)
/// lines they can't parse, so additive changes don't need a bump.
pub(crate) const EVENT_VERSION: u32 = 1;

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "e", rename_all = "snake_case")]
pub(crate) enum Event {
    /// Always the first event. `ts` is the creation time.
    ChatCreated {
        id: String,
        title: String,
        workspace: PathBuf,
    },
    Renamed {
        title: String,
    },
    WorkspaceSet {
        workspace: PathBuf,
    },
    /// The resulting pin time (`None` = unpinned).
    Pinned {
        pinned_at: Option<Millis>,
    },
    /// Sets the "last active" time exactly (written by imports).
    Touched {
        updated_at: Millis,
    },
    /// `ts` is the message's creation time; marks the chat active.
    MessageAdded {
        m: u32,
        role: Role,
        #[serde(default)]
        content: String,
        #[serde(default)]
        reasoning: String,
        done: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task: Option<String>,
    },
    /// One batch of streamed text, appended to the message.
    MessageDelta {
        m: u32,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        content: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        reasoning: String,
    },
    /// Text replaced wholesale.
    MessageUpdated {
        m: u32,
        content: String,
        reasoning: String,
        done: bool,
    },
    /// Reply done; marks the chat active. The lengths (bytes) let replay notice lost deltas.
    MessageFinished {
        m: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stats: Option<MessageStats>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_len: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_len: Option<usize>,
    },
    /// Closed at startup after a crash; keeps an existing error.
    MessageInterrupted {
        m: u32,
        error: String,
    },
    MessageUndone {
        m: u32,
        undone: bool,
    },
    /// Message `from` and every later one removed.
    MessagesDeleted {
        from: u32,
    },
    StepAdded {
        m: u32,
        seq: i64,
        tool: String,
        title: String,
        access: StepAccess,
        args: Value,
    },
    StepFinished {
        m: u32,
        seq: i64,
        output: String,
        #[serde(default, skip_serializing_if = "is_false")]
        failed: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        changes: Vec<StepChange>,
    },
    StepsDeleted {
        m: u32,
    },
    /// Always-allow `tool` in this chat (`allowed: false` = revoked).
    Grant {
        tool: String,
        allowed: bool,
    },
    UndoOp {
        task: String,
        op: UndoOp,
    },
    UndoCleared {
        task: String,
    },
    /// A context compaction: `summary` stands in for messages `1..=through` (`0`: none of
    /// them). `n` numbers the chat's compactions from 1 (never reused). Dropped again by a
    /// `messages_deleted` that removes message `through` (or an earlier one).
    CompactionAdded {
        n: u32,
        through: u32,
        summary: String,
        #[serde(default)]
        tokens_before: u64,
        #[serde(default)]
        tokens_after: u64,
        #[serde(default)]
        prompt_tokens: u64,
        #[serde(default)]
        completion_tokens: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        focus: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        manual: bool,
    },
    /// The chat was deleted (its folder is then moved to `.trash/`).
    Deleted {},
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Line {
    pub v: u32,
    pub ts: Millis,
    #[serde(flatten)]
    pub event: Event,
}

impl Line {
    pub(crate) fn new(ts: Millis, event: Event) -> Self {
        Self { v: EVENT_VERSION, ts, event }
    }

    /// The line as written, `\n` included.
    pub(crate) fn encode(&self) -> Result<String> {
        let mut s = serde_json::to_string(self)?;
        s.push('\n');
        Ok(s)
    }
}

/// What a line that didn't parse was meant to be, when that matters: a damaged undo entry
/// must make its task's journal an error rather than silently shorter.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Bad {
    Garbage,
    UndoOp { task: String },
}

/// Result of reading an events file.
#[derive(Debug, Default)]
pub(crate) struct Parsed {
    pub lines: Vec<Line>,
    pub bad: Vec<Bad>,
    pub warnings: Vec<String>,
}

/// Parse an events file. A last line without its `\n` is a write cut short by a crash and
/// is ignored; any other line that isn't a known event is skipped. Both are reported in
/// `warnings`, never errors.
pub(crate) fn parse(bytes: &[u8], path: &Path) -> Parsed {
    let mut out = Parsed::default();
    let mut rest = bytes;
    let mut n = 0usize;
    while !rest.is_empty() {
        n += 1;
        let (raw, complete) = match rest.iter().position(|&b| b == b'\n') {
            Some(i) => (rest.get(..i).unwrap_or_default(), true),
            None => (rest, false),
        };
        rest = rest.get(raw.len().saturating_add(1)..).unwrap_or_default();
        if !complete {
            out.warnings.push(format!("{}: ignoring unfinished last line {n} ({} bytes)", path.display(), raw.len()));
            break;
        }
        let text = String::from_utf8_lossy(raw);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        match serde_json::from_str::<Line>(text) {
            Ok(l) => out.lines.push(l),
            Err(e) => {
                let v = serde_json::from_str::<Value>(text).ok();
                let ver = v.as_ref().and_then(|v| v.get("v")).and_then(Value::as_u64);
                let bad = match v.as_ref().filter(|v| v.get("e").and_then(Value::as_str) == Some("undo_op")) {
                    Some(v) => Bad::UndoOp { task: v.get("task").and_then(Value::as_str).unwrap_or_default().to_owned() },
                    None => Bad::Garbage,
                };
                let newer = match ver {
                    Some(ver) if ver > u64::from(EVENT_VERSION) => format!(" (written by a newer byob, v{ver})"),
                    _ => String::new(),
                };
                out.warnings.push(format!("{}: skipping line {n}{newer}: {e}", path.display()));
                out.bad.push(bad);
            }
        }
    }
    out
}

/// Read and parse an events file. A missing file is an `Io` error.
pub(crate) fn read(path: &Path) -> Result<Parsed> {
    let bytes = std::fs::read(path).map_err(|e| StoreError::io(path, e))?;
    Ok(parse(&bytes, path))
}

/// File size and modification time, used to notice changes made by someone else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub(crate) struct Stamp {
    pub len: u64,
    /// Nanoseconds since the Unix epoch (0 if unknown).
    pub mtime_ns: u64,
}

impl Stamp {
    pub(crate) fn of(meta: &std::fs::Metadata) -> Self {
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        Self { len: meta.len(), mtime_ns }
    }

    pub(crate) fn read(path: &Path) -> Option<Self> {
        std::fs::metadata(path).ok().map(|m| Self::of(&m))
    }
}

/// Append encoded lines as one write. If the file's last byte isn't `\n` (a crash cut the
/// previous write short), a `\n` goes first so the new line stays parseable. `sync` makes
/// it durable (chat creation, finished replies); stream deltas only flush to the OS.
///
/// Returns the stamp before and after the write.
pub(crate) fn append(path: &Path, text: &str, sync: bool) -> Result<(Stamp, Stamp)> {
    let io = |e| StoreError::io(path, e);
    let mut f: File = OpenOptions::new().read(true).append(true).open(path).map_err(io)?;
    let before = Stamp::of(&f.metadata().map_err(io)?);
    let mut buf = String::with_capacity(text.len() + 1);
    if before.len > 0 {
        let mut last = [0u8; 1];
        f.seek(SeekFrom::End(-1)).map_err(io)?;
        f.read_exact(&mut last).map_err(io)?;
        if last[0] != b'\n' {
            log::warn!("byob-store: {} ended mid-line (crash?); starting a new line", path.display());
            buf.push('\n');
        }
    }
    buf.push_str(text);
    f.write_all(buf.as_bytes()).map_err(io)?;
    f.flush().map_err(io)?;
    if sync {
        f.sync_data().map_err(io)?;
    }
    let after = Stamp::of(&f.metadata().map_err(io)?);
    Ok((before, after))
}

/// Create a new events file holding `text`, durably. Fails if it exists.
pub(crate) fn create(path: &Path, text: &str) -> Result<Stamp> {
    let io = |e| StoreError::io(path, e);
    let mut f = OpenOptions::new().write(true).create_new(true).open(path).map_err(io)?;
    f.write_all(text.as_bytes()).map_err(io)?;
    f.sync_all().map_err(io)?;
    crate::files::sync_dir(path);
    Ok(Stamp::of(&f.metadata().map_err(io)?))
}
