//! Codex (`codex exec`), signed in with a ChatGPT plan.
//!
//! Run with `--json` output, a read-only sandbox, no session files, no user config or
//! rules, and the extra features (browser, computer use, apps, image generation, subagents,
//! hooks) disabled; byob's instructions go at the top of the prompt, since `codex exec` has
//! no system-prompt flag. **Limit:** Codex has no switch that turns its shell off, and the
//! read-only sandbox still lets that shell read any file. byob logs every command it runs.
//! Flags checked against `codex exec --help` and a real run (codex-cli 0.149.1, 2026-11-18).
//!
//! A model must be passed: the config default may be one a ChatGPT account can't use
//! (`gpt-5.6-sol` was refused with `model_not_found`).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde_json::Value;

use super::{AgentEvent, Detection, ErrorKind, Provider, RunRequest, classify, proc};
use crate::Usage;

/// The model used when none is set.
const DEFAULT_MODEL: &str = "gpt-5.6-luna";
/// Features turned off for every run (`codex features list`).
const DISABLED: &[&str] = &["browser_use", "computer_use", "apps", "image_generation", "multi_agent", "hooks", "goals"];

pub struct Codex;

impl Provider for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }
    fn name(&self) -> &'static str {
        "Codex"
    }
    fn binary_name(&self) -> &'static str {
        "codex"
    }
    fn binary_env(&self) -> &'static str {
        "BYOB_CODEX_BIN"
    }

    fn detect_uncached(&self, binary: Option<PathBuf>) -> Detection {
        let mut d = Detection {
            login_command: "codex login",
            limits: "Codex can't turn its read-only shell off, so it can read files outside byob's rules.",
            ..Default::default()
        };
        let Some(bin) = binary else { return d };
        d.found = true;
        d.version = proc::output(&bin, &["--version"], Duration::from_secs(10)).map(|(_, out)| out.trim().to_owned()).filter(|v| !v.is_empty());
        if let Some((code, out)) = proc::output(&bin, &["login", "status"], Duration::from_secs(10)) {
            let text = out.trim();
            d.logged_in = code == 0 && text.to_ascii_lowercase().starts_with("logged in");
            if d.logged_in {
                d.auth_method = text.rsplit_once("using ").map(|(_, m)| m.trim().to_owned());
            }
        }
        d.binary = Some(bin);
        d
    }

    fn models(&self, binary: &Path) -> Vec<String> {
        let listed = proc::output(binary, &["debug", "models"], Duration::from_secs(15))
            .and_then(|(_, out)| serde_json::from_str::<Value>(&out).ok())
            .map(|v| model_slugs(&v))
            .unwrap_or_default();
        if listed.is_empty() { vec![DEFAULT_MODEL.to_owned()] } else { listed }
    }

    fn run(&self, req: &RunRequest<'_>, stop: &dyn Fn() -> bool, on_event: &mut dyn FnMut(AgentEvent)) {
        let mut cmd = Command::new(req.binary);
        cmd.current_dir(req.cwd).env("PATH", proc::child_path(req.binary));
        cmd.args([
            "exec",
            "--json",
            "--sandbox",
            "read-only",
            "--ephemeral",
            "--skip-git-repo-check",
            "--ignore-user-config",
            "--ignore-rules",
            "--color",
            "never",
        ]);
        cmd.arg("-C").arg(req.cwd);
        let model = if req.model.is_empty() || req.model == "default" { DEFAULT_MODEL } else { req.model };
        cmd.args(["-m", model]);
        if let Some(e) = req.effort {
            // Codex has no "max": its highest is "xhigh".
            let e = if e == "max" { "xhigh" } else { e };
            cmd.args(["-c", &format!("model_reasoning_effort={e}")]);
        }
        for f in DISABLED {
            cmd.args(["--disable", f]);
        }
        cmd.arg("-");
        let stdin = format!(
            "<instructions>\n{}\n</instructions>\n\nDo not run shell commands or read files yourself: use only the tools described in the instructions.\n\n{}",
            req.system, req.prompt
        );
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
    }
}

/// Model slugs Codex lists to users (`visibility: "list"`).
fn model_slugs(v: &Value) -> Vec<String> {
    let models = v.get("models").unwrap_or(v);
    models
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|m| m.get("visibility").and_then(Value::as_str) == Some("list"))
                .filter_map(|m| m.get("slug").and_then(Value::as_str))
                .filter(|s| s.len() <= 128)
                .take(64)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Reading `--json` lines.
#[derive(Default)]
struct Stream {
    finished: bool,
}

impl Stream {
    fn line(&mut self, line: &str, on_event: &mut dyn FnMut(AgentEvent)) {
        let Ok(v) = serde_json::from_str::<Value>(line) else { return };
        let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
        match v.get("type").and_then(Value::as_str) {
            Some("item.completed") => {
                let item = v.get("item").unwrap_or(&Value::Null);
                match item.get("type").and_then(Value::as_str) {
                    Some("agent_message") => on_event(AgentEvent::Text(text(item, "text"))),
                    Some("reasoning") => on_event(AgentEvent::Reasoning(text(item, "text"))),
                    Some("command_execution") => {
                        on_event(AgentEvent::Tool { name: "shell".into(), summary: text(item, "command").chars().take(200).collect() })
                    }
                    Some(other) if other != "error" => on_event(AgentEvent::Tool { name: other.chars().take(40).collect(), summary: String::new() }),
                    // Codex's own warnings (e.g. missing model metadata): not the reply.
                    _ => {}
                }
            }
            Some("turn.completed") => {
                self.finished = true;
                let u = v.get("usage").unwrap_or(&Value::Null);
                let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
                on_event(AgentEvent::Done {
                    usage: Some(Usage { prompt_tokens: n("input_tokens"), completion_tokens: n("output_tokens").saturating_add(n("reasoning_output_tokens")) }),
                });
            }
            Some("turn.failed") | Some("error") if !self.finished => {
                self.finished = true;
                let raw = v.pointer("/error/message").or_else(|| v.get("message")).and_then(Value::as_str).unwrap_or_default();
                // The message is often the API's JSON error, as a string.
                let inner: Option<Value> = serde_json::from_str(raw).ok();
                let message = inner.as_ref().and_then(|i| i.pointer("/error/message")).and_then(Value::as_str).unwrap_or(raw).to_owned();
                let code = inner.as_ref().and_then(|i| i.pointer("/error/code")).and_then(Value::as_str).unwrap_or_default();
                let kind = match code {
                    "rate_limit_exceeded" => ErrorKind::RateLimited,
                    "usage_limit_reached" | "insufficient_quota" => ErrorKind::Quota,
                    _ => classify(&message),
                };
                on_event(AgentEvent::Error { kind, message });
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(lines: &[&str]) -> Vec<AgentEvent> {
        let mut s = Stream::default();
        let mut got = Vec::new();
        for l in lines {
            s.line(l, &mut |e| got.push(e));
        }
        got
    }

    #[test]
    fn reads_a_real_run() {
        let got = feed(&[
            r#"{"type":"thread.started","thread_id":"x"}"#,
            r#"{"type":"turn.started"}"#,
            r#"{"type":"item.completed","item":{"id":"item_0","type":"error","message":"Model metadata not found"}}"#,
            r#"{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"ok"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":12125,"cached_input_tokens":0,"output_tokens":5,"reasoning_output_tokens":0}}"#,
        ]);
        assert_eq!(got, vec![AgentEvent::Text("ok".into()), AgentEvent::Done { usage: Some(Usage { prompt_tokens: 12125, completion_tokens: 5 }) }]);
    }

    #[test]
    fn a_refused_model_is_one_clear_error() {
        // From a real run with a model a ChatGPT account can't use: `error`, then `turn.failed`.
        let body = r#"{\"type\":\"error\",\"status\":400,\"error\":{\"type\":\"invalid_request_error\",\"message\":\"The 'gpt-5.6-sol' model is not supported when using Codex with a ChatGPT account.\",\"code\":\"model_not_found\"}}"#;
        let got = feed(&[&format!(r#"{{"type":"error","message":"{body}"}}"#), &format!(r#"{{"type":"turn.failed","error":{{"message":"{body}"}}}}"#)]);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(matches!(&got[0], AgentEvent::Error { kind: ErrorKind::Other, message } if message.contains("not supported")), "{got:?}");
    }

    #[test]
    fn its_own_commands_show_as_tool_lines() {
        let got = feed(&[r#"{"type":"item.completed","item":{"type":"command_execution","command":"cat /etc/hosts"}}"#]);
        assert_eq!(got, vec![AgentEvent::Tool { name: "shell".into(), summary: "cat /etc/hosts".into() }]);
    }

    #[test]
    fn lists_only_visible_models() {
        let v: Value = serde_json::from_str(r#"{"models":[{"slug":"gpt-reserve","visibility":"hide"},{"slug":"gpt-5.6-luna","visibility":"list"}]}"#).unwrap();
        assert_eq!(model_slugs(&v), ["gpt-5.6-luna"]);
    }
}
