//! Claude Code (`claude`), signed in with a claude.ai plan.
//!
//! Run as `claude -p` with `stream-json` output and **every tool, MCP server, hook, plugin
//! and settings file off** (`--tools "" --strict-mcp-config --safe-mode --setting-sources ""`):
//! it can't read or write anything, and the lean flags matter for cost. Without them a
//! "Reply with: ok" loaded the user's plugins and MCP servers into a 294k-token prompt; with
//! them it took 907 (measured 2026-11-18, Claude Code 2.1.316). `--bare` would be leaner
//! but never reads the claude.ai login. Flags checked against `claude --help` and a real run.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};

use super::{AgentEvent, Detection, ErrorKind, Part, PlanUsage, Provider, RunRequest, classify, proc};
use crate::Usage;

/// Largest system prompt passed as an argument; a longer one goes on stdin instead.
const MAX_ARG_SYSTEM: usize = 256 * 1024;

pub struct Claude;

/// The rate-limit windows from the latest run.
static USAGE: Mutex<Option<PlanUsage>> = Mutex::new(None);

impl Provider for Claude {
    fn id(&self) -> &'static str {
        "claude"
    }
    fn name(&self) -> &'static str {
        "Claude Code"
    }
    fn binary_name(&self) -> &'static str {
        "claude"
    }
    fn binary_env(&self) -> &'static str {
        "BYOB_CLAUDE_BIN"
    }

    fn detect_uncached(&self, binary: Option<PathBuf>) -> Detection {
        let mut d = Detection { login_command: "claude auth login", ..Default::default() };
        let Some(bin) = binary else { return d };
        d.found = true;
        d.version = proc::output(&bin, &["--version"], Duration::from_secs(10)).map(|(_, out)| out.trim().to_owned()).filter(|v| !v.is_empty());
        if let Some((_, out)) = proc::output(&bin, &["auth", "status"], Duration::from_secs(10))
            && let Ok(status) = serde_json::from_str::<Value>(out.trim())
        {
            d.logged_in = status.get("loggedIn").and_then(Value::as_bool).unwrap_or(false);
            d.plan = status.get("subscriptionType").and_then(Value::as_str).map(str::to_owned);
            d.auth_method = status.get("authMethod").and_then(Value::as_str).map(str::to_owned);
        }
        d.binary = Some(bin);
        d
    }

    fn takes_images(&self) -> bool {
        true
    }

    fn models(&self, _binary: &Path) -> Vec<String> {
        // Claude Code takes these aliases for the newest model of each family.
        ["sonnet", "opus", "haiku", "fable"].map(str::to_owned).to_vec()
    }

    fn run(&self, req: &RunRequest<'_>, stop: &dyn Fn() -> bool, on_event: &mut dyn FnMut(AgentEvent)) {
        let mut cmd = Command::new(req.binary);
        cmd.current_dir(req.cwd).env("PATH", proc::child_path(req.binary));
        cmd.args(["-p", "--output-format", "stream-json", "--verbose", "--include-partial-messages"]);
        cmd.args(["--tools", "", "--strict-mcp-config", "--safe-mode", "--setting-sources", "", "--no-session-persistence"]);
        if !req.model.is_empty() {
            cmd.args(["--model", req.model]);
        }
        if let Some(e) = req.effort {
            cmd.args(["--effort", e]);
        }
        let mut stdin = String::new();
        if req.system.len() <= MAX_ARG_SYSTEM {
            cmd.args(["--system-prompt", req.system]);
        } else {
            stdin.push_str(req.system);
            stdin.push_str("\n\n");
        }
        if req.parts.is_empty() {
            stdin.push_str(req.prompt);
        } else {
            // Images: one user message of text and image blocks, as the Messages API takes them.
            cmd.args(["--input-format", "stream-json"]);
            stdin = user_message(&stdin, req.parts);
        }
        let mut state = Stream::default();
        let exit = proc::run(cmd, &stdin, super::run_timeout(), stop, &mut |line| state.line(line, on_event));
        match exit {
            Err(e) => on_event(AgentEvent::Error { kind: ErrorKind::NotInstalled, message: e.0.to_string() }),
            Ok(exit) if exit.cancelled => {}
            Ok(exit) if exit.timed_out => on_event(AgentEvent::Error { kind: ErrorKind::Timeout, message: String::new() }),
            Ok(exit) if exit.too_long => on_event(AgentEvent::Error { kind: ErrorKind::Other, message: "the reply was too long".into() }),
            Ok(exit) if !state.finished => {
                let message = if exit.stderr.is_empty() { format!("exited ({:?}) without a reply", exit.code) } else { exit.stderr };
                on_event(AgentEvent::Error { kind: classify(&message), message });
            }
            Ok(_) => {}
        }
        if let Some(u) = state.usage.take() {
            *USAGE.lock().unwrap_or_else(PoisonError::into_inner) = Some(u);
        }
    }

    fn usage(&self) -> Option<PlanUsage> {
        USAGE.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

/// The `stream-json` input line for one user message: `lead` (the system prompt when it was
/// too long for an argument), then the parts, images as image blocks.
fn user_message(lead: &str, parts: &[Part]) -> String {
    let text = |t: &str| (!t.trim().is_empty()).then(|| json!({"type": "text", "text": t}));
    let mut content: Vec<Value> = text(lead).into_iter().collect();
    for p in parts {
        match p {
            Part::Text(t) => content.extend(text(t)),
            Part::Image(url) => content.push(crate::anthropic::image(url)),
        }
    }
    let mut line = json!({"type": "user", "message": {"role": "user", "content": content}}).to_string();
    line.push('\n');
    line
}

/// Reading `stream-json` lines.
#[derive(Default)]
struct Stream {
    finished: bool,
    rate_limited: bool,
    usage: Option<PlanUsage>,
}

impl Stream {
    fn line(&mut self, line: &str, on_event: &mut dyn FnMut(AgentEvent)) {
        let Ok(v) = serde_json::from_str::<Value>(line) else { return };
        let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
        match v.get("type").and_then(Value::as_str) {
            Some("stream_event") => {
                let Some(delta) = v.pointer("/event/delta") else { return };
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(t) = s(delta, "text") {
                            on_event(AgentEvent::Text(t));
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(t) = s(delta, "thinking") {
                            on_event(AgentEvent::Reasoning(t));
                        }
                    }
                    _ => {}
                }
            }
            Some("rate_limit_event") => {
                let info = v.get("rate_limit_info").unwrap_or(&Value::Null);
                self.rate_limited |= info.get("status").and_then(Value::as_str) == Some("rejected");
                if let Some(windows) = info.get("unifiedWindows").and_then(Value::as_object) {
                    let windows = windows
                        .iter()
                        .take(8)
                        .map(|(name, w)| {
                            (
                                name.clone(),
                                w.get("utilization").and_then(Value::as_f64).unwrap_or(0.0).clamp(0.0, 1.0),
                                w.get("resetsAt").and_then(Value::as_u64),
                            )
                        })
                        .collect();
                    self.usage = Some(PlanUsage { windows });
                }
            }
            Some("result") => {
                self.finished = true;
                let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                if is_error {
                    let message = s(&v, "result").or_else(|| s(&v, "subtype")).unwrap_or_default();
                    let kind = if self.rate_limited { ErrorKind::RateLimited } else { classify(&message) };
                    on_event(AgentEvent::Error { kind, message });
                    return;
                }
                let u = v.get("usage").unwrap_or(&Value::Null);
                let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
                let prompt_tokens = n("input_tokens").saturating_add(n("cache_creation_input_tokens")).saturating_add(n("cache_read_input_tokens"));
                on_event(AgentEvent::Done { usage: Some(Usage { prompt_tokens, completion_tokens: n("output_tokens") }) });
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Images reach Claude Code as image blocks of one stream-json user message.
    #[test]
    fn a_prompt_with_images_is_one_stream_json_message() {
        let parts = [Part::Text("<user>\nwhat color?\n".into()), Part::Image("data:image/png;base64,AAAA".into()), Part::Text("\n</user>".into())];
        let line = user_message("", &parts);
        assert!(line.ends_with('\n') && line.trim_end().lines().count() == 1);
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["type"], "user");
        let content = v["message"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 3);
        assert_eq!(content[1], json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}));
        // A system prompt too long for an argument leads.
        let v: Value = serde_json::from_str(&user_message("be brief", &parts)).unwrap();
        assert_eq!(v["message"]["content"][0]["text"], "be brief");
    }

    fn feed(lines: &[&str]) -> (Vec<AgentEvent>, Stream) {
        let mut s = Stream::default();
        let mut got = Vec::new();
        for l in lines {
            s.line(l, &mut |e| got.push(e));
        }
        (got, s)
    }

    #[test]
    fn reads_deltas_usage_and_limits_from_a_real_run() {
        // Trimmed from a real `claude -p --output-format stream-json --include-partial-messages` run.
        let (got, s) = feed(&[
            r#"{"type":"system","subtype":"init","tools":[]}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hm"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"o"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"k"}}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"ok"}]}}"#,
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","unifiedWindows":{"five_hour":{"utilization":0.04,"resetsAt":1794990000},"seven_day":{"utilization":0.2}}}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","usage":{"input_tokens":2,"cache_creation_input_tokens":907,"cache_read_input_tokens":0,"output_tokens":4}}"#,
        ]);
        assert_eq!(
            got,
            vec![
                AgentEvent::Reasoning("hm".into()),
                AgentEvent::Text("o".into()),
                AgentEvent::Text("k".into()),
                AgentEvent::Done { usage: Some(Usage { prompt_tokens: 909, completion_tokens: 4 }) }
            ]
        );
        assert!(s.finished);
        let u = s.usage.unwrap();
        assert!(u.windows.contains(&("five_hour".into(), 0.04, Some(1_794_990_000))));
    }

    #[test]
    fn errors_say_what_kind() {
        let (got, _) = feed(&[r#"{"type":"result","is_error":true,"result":"Invalid API key · Please run /login"}"#]);
        assert!(matches!(&got[..], [AgentEvent::Error { kind: ErrorKind::NotLoggedIn, .. }]));
        let (got, _) =
            feed(&[r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected"}}"#, r#"{"type":"result","is_error":true,"result":"limit reached"}"#]);
        assert!(matches!(&got[..], [AgentEvent::Error { kind: ErrorKind::RateLimited, .. }]));
    }

    #[test]
    fn garbage_lines_are_ignored() {
        let (got, s) =
            feed(&["", "not json", "{", r#"{"type":"stream_event"}"#, r#"{"type":"stream_event","event":{"delta":{"type":"text_delta","text":7}}}"#]);
        assert!(got.is_empty());
        assert!(!s.finished);
    }
}
