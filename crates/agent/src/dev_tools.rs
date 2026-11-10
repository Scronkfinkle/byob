//! Trivial tools for trying the tool loop and approvals against a live model
//! (`byob --dev-tools`). They have no real side effects.

use std::sync::Arc;

use serde_json::{Value, json};
use byob_tools::{Access, Registry, Tool, ToolContext, ToolOutput, ToolSpec, parse_args};

/// Register `get_time` (read: runs without asking) and `set_reminder` (write: asks first).
pub fn register(reg: &mut Registry) {
    reg.register(Arc::new(GetTime));
    reg.register(Arc::new(SetReminder));
}

struct GetTime;

impl Tool for GetTime {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "get_time".into(),
            description: "Get the current local date and time.".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }
    fn title(&self, _args: &Value) -> String {
        "Check the time".into()
    }
    fn access(&self, _args: &Value) -> Access {
        Access::Read
    }
    fn run(&self, _args: &Value, _ctx: &ToolContext<'_>) -> ToolOutput {
        ToolOutput::ok(chrono::Local::now().format("%A %Y-%m-%d %H:%M:%S %:z").to_string())
    }
}

struct SetReminder;

#[derive(serde::Deserialize)]
struct ReminderArgs {
    text: String,
    #[serde(default)]
    when: String,
}

impl Tool for SetReminder {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "set_reminder".into(),
            description: "Set a reminder for the user. Needs the user's approval.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": {"type": "string", "description": "What to remind about"},
                    "when": {"type": "string", "description": "When, in the user's words"}
                },
                "required": ["text"]
            }),
        }
    }
    fn title(&self, args: &Value) -> String {
        match args.get("text").and_then(Value::as_str) {
            Some(t) => format!("Set a reminder: {t}"),
            None => "Set a reminder".into(),
        }
    }
    fn access(&self, _args: &Value) -> Access {
        Access::Write
    }
    fn run(&self, args: &Value, _ctx: &ToolContext<'_>) -> ToolOutput {
        match parse_args::<ReminderArgs>(args) {
            Ok(a) if a.when.is_empty() => ToolOutput::ok(format!("Reminder set: {} (dev tool; nothing was saved)", a.text)),
            Ok(a) => ToolOutput::ok(format!("Reminder set for {}: {} (dev tool; nothing was saved)", a.when, a.text)),
            Err(e) => ToolOutput::err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn dev_tools_run() {
        let mut reg = Registry::default();
        register(&mut reg);
        let cancel = AtomicBool::new(false);
        let ctx = ToolContext::new(Path::new("."), &cancel);
        assert!(!reg.get("get_time").unwrap().run(&json!({}), &ctx).failed);
        let r = reg.get("set_reminder").unwrap();
        assert_eq!(r.access(&json!({})), Access::Write);
        assert!(r.run(&json!({"text": "tea"}), &ctx).text.contains("tea"));
        assert!(r.run(&json!({}), &ctx).failed);
    }
}
