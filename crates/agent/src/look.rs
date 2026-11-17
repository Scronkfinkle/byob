//! Windows' looks (`byob_protocol::look`): each client reports its own; the `ui_look` tool
//! (named by the bundled `appearance` plugin) shows the model the look of the window showing
//! its chat and changes it there, then reports what the window actually changed.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use byob_protocol::ServerMsg;
use byob_protocol::look::{CAP_LOOK, LookChanges, LookReport};
use byob_tools::{Access, Tool, ToolContext, ToolOutput, ToolSpec};
use serde_json::{Value, json};

use crate::{ClientId, Engine, Input};

/// The tool's name.
pub const TOOL: &str = "ui_look";

/// How long the tool waits for a window to answer.
const TIMEOUT: Duration = Duration::from_secs(10);

/// What the engine answers the tool: the look before, and after the change (if one was asked).
pub(crate) type Answer = Result<(LookReport, Option<LookReport>), String>;

/// A change sent to windows, waiting for the first to answer.
pub(crate) struct Waiting {
    before: LookReport,
    reply: Sender<Answer>,
}

const NO_WINDOW: &str = "no byob window is showing this chat right now (or it's too old to report its look), so there's no look to read or change";

impl Engine {
    fn look_clients(&self, chat: &str) -> Vec<ClientId> {
        self.clients.iter().filter(|(_, c)| c.attached && c.chat.as_deref() == Some(chat) && c.caps.iter().any(|c| c == CAP_LOOK)).map(|(id, _)| *id).collect()
    }

    /// `ClientMsg::ReportLook`: keep it; answer the tool that asked for the change.
    pub(crate) fn report_look(&mut self, client: ClientId, report: LookReport) {
        let report = report.sanitized();
        self.look_seq += 1;
        let seq = self.look_seq;
        if let Some(c) = self.clients.get_mut(&client) {
            c.look = Some((seq, report.clone()));
        }
        if let Some(w) = report.answered.and_then(|n| self.look_waits.remove(&n)) {
            let _ = w.reply.send(Ok((w.before, Some(report))));
        }
    }

    /// `Input::Look`: the look of the window showing `chat` that reported last, and with
    /// `changes`, send them to every window showing it.
    pub(crate) fn tool_look(&mut self, chat: &str, changes: Option<LookChanges>, reply: Sender<Answer>) {
        let ids = self.look_clients(chat);
        let latest = ids.iter().filter_map(|id| self.clients.get(id)?.look.clone()).max_by_key(|(seq, _)| *seq).map(|(_, r)| r);
        let Some(before) = latest else {
            let _ = reply.send(Err(NO_WINDOW.into()));
            return;
        };
        let Some(changes) = changes else {
            let _ = reply.send(Ok((before, None)));
            return;
        };
        self.look_seq += 1;
        let request = self.look_seq;
        // Forget requests nobody answered (their tool gave up long ago).
        while self.look_waits.len() > 16 {
            self.look_waits.pop_first();
        }
        self.look_waits.insert(request, Waiting { before, reply });
        for id in ids {
            self.send_to(id, ServerMsg::SetLook { request, changes: changes.clone() });
        }
    }
}

fn request(inbox: &Sender<Input>, chat: &str, changes: Option<LookChanges>, cancel: &AtomicBool) -> Answer {
    let (reply, answer) = channel();
    inbox.send(Input::Look { chat: chat.to_owned(), changes, reply }).map_err(|_| "byob is shutting down".to_owned())?;
    let started = Instant::now();
    loop {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("stopped".into());
        }
        match answer.recv_timeout(Duration::from_millis(50)) {
            Ok(r) => return r,
            Err(RecvTimeoutError::Timeout) if started.elapsed() < TIMEOUT => {}
            Err(_) => return Err("the window didn't answer".into()),
        }
    }
}

fn shown(v: &Value) -> String {
    match v {
        Value::String(s) => format!("{s:?}"),
        Value::Null => "(unset)".into(),
        v => v.to_string(),
    }
}

/// The look for the model.
fn describe(r: &LookReport) -> String {
    let mut out = String::from("The look of the byob window showing this chat (each device keeps its own):\n");
    for f in &r.fields {
        let _ = writeln!(out, "- {}: {} — {}", f.key, shown(&f.value), f.describe);
    }
    if !r.themes.is_empty() {
        out.push_str("\nThemes (for dark_theme / light_theme):\n");
        for t in &r.themes {
            let _ = writeln!(out, "- {}: {} ({})", t.id, t.name, if t.dark { "dark" } else { "light" });
        }
    }
    out
}

/// The window's setting names (the colours as one `colour.<role>`), for a model that guessed.
fn names(have: &LookReport) -> String {
    let mut names: Vec<&str> = have.fields.iter().map(|f| f.key.as_str()).filter(|k| !k.starts_with("colour.")).collect();
    if have.fields.iter().any(|f| f.key.starts_with("colour.")) {
        names.push("colour.<role>");
    }
    names.join(", ")
}

/// The `set` argument, checked against what the window has: the changes to send, and what
/// was left out and why (an error when nothing is left).
fn changes(args: &Value, have: &LookReport) -> Result<Option<(LookChanges, Vec<String>)>, String> {
    let Some(set) = args.get("set") else { return Ok(None) };
    let map = set.as_object().ok_or("set: an object of setting → new value")?;
    if map.is_empty() {
        return Ok(None);
    }
    let (mut out, mut skipped) = (LookChanges::new(), Vec::new());
    for (k, v) in map {
        if have.get(k).is_none() {
            skipped.push(format!("there's no setting {k:?}"));
        } else if matches!(v, Value::Object(_) | Value::Array(_)) {
            skipped.push(format!("{k}: one value (a string, number or true/false)"));
        } else {
            out.insert(k.clone(), v.clone());
        }
    }
    if skipped.iter().any(|s| s.starts_with("there's no setting")) {
        skipped.push(format!("the settings are: {}", names(have)));
    }
    if out.is_empty() {
        return Err(skipped.join("; "));
    }
    Ok(Some((out, skipped)))
}

/// The model's tool: read the window's look, or change it.
pub(crate) struct UiLook {
    pub inbox: Sender<Input>,
}

impl Tool for UiLook {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: TOOL.into(),
            description: "Read or change how the byob window showing this chat looks: dark or light, theme, accent colour, corner roundness, text size, zoom, density, message bubbles, fonts, any single colour. Without arguments: every setting with its value and what it may be. Only this device's window(s) change.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "set": {"type": "object", "description": "Setting → new value, e.g. {\"accent\": \"orange\", \"corners\": 12}. Call without arguments first for the settings there are.", "additionalProperties": true}
                }
            }),
        }
    }

    fn title(&self, args: &Value) -> String {
        match args.get("set").and_then(Value::as_object).filter(|m| !m.is_empty()) {
            Some(m) => {
                let keys: Vec<&str> = m.keys().map(String::as_str).take(4).collect();
                let more = if m.len() > 4 { ", …" } else { "" };
                format!("Look: changed {}{more}", keys.join(", "))
            }
            None => "Read the look".into(),
        }
    }

    fn pending_title(&self, args: &Value) -> String {
        if args.get("set").and_then(Value::as_object).is_some_and(|m| !m.is_empty()) { "Change the look".into() } else { "Read the look".into() }
    }

    fn preview(&self, _args: &Value, _workspace: &Path) -> Option<byob_tools::Preview> {
        None
    }

    /// Only how this window looks changes, visibly and at once, and Settings → Appearance
    /// changes it back: nothing to approve.
    fn access(&self, _args: &Value) -> Access {
        Access::Read
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        let Some(chat) = ctx.chat() else { return ToolOutput::err(NO_WINDOW) };
        let now = match request(&self.inbox, chat, None, ctx.cancel_flag()) {
            Ok((r, _)) => r,
            Err(e) => return ToolOutput::err(e),
        };
        let (set, skipped) = match changes(args, &now) {
            Ok(Some(c)) => c,
            Ok(None) => return ToolOutput::ok(describe(&now)),
            Err(e) => return ToolOutput::err(e),
        };
        let asked: Vec<String> = set.keys().cloned().collect();
        let (before, after) = match request(&self.inbox, chat, Some(set), ctx.cancel_flag()) {
            Ok((b, Some(a))) => (b, a),
            Ok((_, None)) => return ToolOutput::err("the window didn't answer"),
            Err(e) => return ToolOutput::err(e),
        };
        // What was asked for, setting by setting; colours that follow from it (an accent
        // changes links and selections too) only as a count.
        let changed = after.changes_since(&before);
        let mut out = String::from("In the window now:\n");
        for k in &asked {
            match changed.iter().find(|(c, _, _)| c == k) {
                Some((_, old, new)) => {
                    let _ = writeln!(out, "- {k}: {} → {}", shown(old), shown(new));
                }
                None if after.problems.iter().any(|p| p.starts_with(&format!("{k}:"))) => {}
                None => {
                    let _ = writeln!(out, "- {k}: already {}", after.get(k).map_or_else(|| "(unset)".to_owned(), shown));
                }
            }
        }
        let follow = changed.iter().filter(|(k, _, _)| !asked.contains(k)).count();
        if follow > 0 {
            let _ = writeln!(out, "({follow} colours that follow from these changed with them.)");
        }
        let problems: Vec<&String> = after.problems.iter().chain(&skipped).collect();
        if !problems.is_empty() {
            out.push_str("\nNot applied:\n");
            for p in &problems {
                let _ = writeln!(out, "- {p}");
            }
        }
        if changed.is_empty() && !problems.is_empty() { ToolOutput::err(out) } else { ToolOutput::ok(out) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use byob_protocol::look::LookField;

    fn report() -> LookReport {
        LookReport {
            fields: vec![
                LookField { key: "accent".into(), value: json!("theme"), describe: "theme, blue, orange or #rrggbb".into() },
                LookField { key: "colour.app_bg".into(), value: json!("#101010"), describe: "Background".into() },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn changes_are_checked_against_the_windows_settings() {
        assert_eq!(changes(&json!({}), &report()), Ok(None));
        assert_eq!(changes(&json!({"set": {}}), &report()), Ok(None));
        assert_eq!(changes(&json!({"set": {"accent": "orange"}}), &report()).unwrap().unwrap().0["accent"], "orange");
        let e = changes(&json!({"set": {"sparkle": true}}), &report()).unwrap_err();
        assert!(e.contains("no setting \"sparkle\"") && e.contains("the settings are: accent, colour.<role>"), "{e}");
        assert!(changes(&json!({"set": {"accent": ["a"]}}), &report()).is_err());
        // What it knows goes ahead; what it doesn't is said.
        let (set, skipped) = changes(&json!({"set": {"accent": "teal", "bubbles": "outlined"}}), &report()).unwrap().unwrap();
        assert_eq!((set.len(), skipped.len()), (1, 2));
        assert!(changes(&json!({"set": {"colour.app_bg": "#202020"}}), &report()).unwrap().is_some());
        assert!(describe(&report()).contains("- accent: \"theme\" — theme, blue"));
    }
}
