//! The model's plan and its questions to the user: `update_plan` (the bundled `plan` plugin)
//! and `ask_user` (the bundled `ask-user` plugin).
//!
//! `update_plan(steps: [{text, status}])` replaces the whole plan of the chat. It is kept in
//! `<workspace>/.byob/plans/<chat>.json` (so the plugin's `before_request` hook can restate
//! it, after a restart too) and shown as a card on the call's step.
//!
//! `ask_user(question, options?)` puts a question card in the chat (through the turn's
//! [`crate::Asker`], like any approval card) and waits: the option the user picks, or the
//! text they type, is the tool's result.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Access, Tool, ToolContext, ToolOutput, ToolSpec, UserQuestion, UserReply};

/// Folder of the plans, in the workspace.
pub const PLAN_DIR: &str = ".byob/plans";
/// Most steps in a plan.
pub const MAX_STEPS: usize = 20;
/// Longest step text (characters).
pub const MAX_STEP_CHARS: usize = 200;
/// Longest question (characters).
pub const MAX_QUESTION_CHARS: usize = 1000;

/// Where a step stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    Doing,
    Done,
}

impl Status {
    /// Lenient: the names other agents use (`in_progress`, `completed`) too.
    fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().replace(['-', ' '], "_").as_str() {
            "pending" | "todo" | "to_do" | "not_started" | "" => Self::Pending,
            "doing" | "in_progress" | "active" | "current" | "started" => Self::Doing,
            "done" | "completed" | "complete" | "finished" => Self::Done,
            _ => return None,
        })
    }

    fn mark(self) -> &'static str {
        match self {
            Self::Pending => "[ ]",
            Self::Doing => "[>]",
            Self::Done => "[x]",
        }
    }
}

/// One step of a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    pub text: String,
    pub status: Status,
}

/// A chat's plan.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub steps: Vec<PlanStep>,
}

impl Plan {
    /// From `update_plan`'s (untrusted) arguments.
    pub fn from_args(args: &Value) -> Result<Self, String> {
        let steps = match args.get("steps") {
            Some(Value::Array(a)) => a,
            // Some models send the list as a JSON string.
            Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
                Ok(Value::Array(a)) => return Self::from_args(&json!({"steps": a})),
                _ => return Err("`steps` must be a list of {text, status}".into()),
            },
            None | Some(Value::Null) => return Err("`steps` is missing: give the whole list of {text, status}".into()),
            Some(_) => return Err("`steps` must be a list of {text, status}".into()),
        };
        if steps.len() > MAX_STEPS {
            return Err(format!("a plan has at most {MAX_STEPS} steps; merge some"));
        }
        let mut out = Vec::with_capacity(steps.len());
        for (i, s) in steps.iter().enumerate() {
            let (text, status) = match s {
                Value::String(t) => (t.as_str(), Status::Pending),
                Value::Object(o) => {
                    let text = o.get("text").or_else(|| o.get("step")).or_else(|| o.get("content")).and_then(Value::as_str).unwrap_or_default();
                    let status = match o.get("status").and_then(Value::as_str) {
                        None => Status::Pending,
                        Some(st) => Status::parse(st).ok_or_else(|| format!("step {}: status must be pending, doing or done, not {st:?}", i + 1))?,
                    };
                    (text, status)
                }
                _ => return Err(format!("step {} must be {{text, status}}", i + 1)),
            };
            let text = one_line(text, MAX_STEP_CHARS);
            if text.is_empty() {
                return Err(format!("step {} has no text", i + 1));
            }
            out.push(PlanStep { text, status });
        }
        Ok(Self { steps: out })
    }

    pub fn done(&self) -> usize {
        self.steps.iter().filter(|s| s.status == Status::Done).count()
    }

    /// Some step isn't done.
    pub fn unfinished(&self) -> bool {
        self.steps.iter().any(|s| s.status != Status::Done)
    }

    /// The plan for the model: a header line and one line per step.
    pub fn text(&self) -> String {
        if self.steps.is_empty() {
            return "Plan cleared.".into();
        }
        let mut out = format!("Plan ({}/{} done):", self.done(), self.steps.len());
        for s in &self.steps {
            out.push('\n');
            out.push_str(s.status.mark());
            out.push(' ');
            out.push_str(&s.text);
        }
        out
    }

    /// The card on the step: a view tree (kind `byob.tree`).
    pub fn view(&self) -> byob_protocol::View {
        let rows: Vec<Value> = self
            .steps
            .iter()
            .map(|s| {
                let (chip, tone, style) = match s.status {
                    Status::Done => ("done", "success", "body"),
                    Status::Doing => ("doing", "accent", "strong"),
                    Status::Pending => ("to do", "dim", "body"),
                };
                let text_tone = if s.status == Status::Done { "dim" } else { "normal" };
                json!({"type": "row", "children": [
                    {"type": "chip", "text": chip, "tone": tone},
                    {"type": "text", "text": s.text, "style": style, "tone": text_tone},
                ]})
            })
            .collect();
        let n = self.steps.len();
        #[allow(clippy::cast_precision_loss)] // ≤ 20 steps
        let progress = if n == 0 { 1.0 } else { self.done() as f64 / n as f64 };
        let mut children = vec![json!({"type": "progress", "value": progress, "label": format!("{}/{n} done", self.done())})];
        children.extend(rows);
        let tree = json!({"type": "card", "title": "Plan", "children": children});
        let fallback = self.steps.iter().map(|s| format!("- {} {}", s.status.mark(), s.text)).collect::<Vec<_>>().join("\n");
        byob_protocol::View {
            kind: byob_protocol::tree::KIND.into(),
            v: byob_protocol::tree::VERSION,
            data: tree,
            fallback: if fallback.is_empty() { "Plan cleared.".into() } else { format!("**Plan**\n\n{fallback}") },
            owner: None,
        }
    }
}

/// The file of chat `chat`'s plan (`None` for a chat id that isn't a plain name).
pub fn plan_path(workspace: &Path, chat: &str) -> Option<PathBuf> {
    let ok = !chat.is_empty() && chat.len() <= 128 && chat.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    ok.then(|| workspace.join(PLAN_DIR).join(format!("{chat}.json")))
}

/// Chat `chat`'s plan, if it has one.
pub fn load(workspace: &Path, chat: &str) -> Option<Plan> {
    let text = std::fs::read_to_string(plan_path(workspace, chat)?).ok()?;
    serde_json::from_str::<Plan>(&text).ok().filter(|p| !p.steps.is_empty())
}

/// Keep `plan` as chat `chat`'s (an empty one removes the file).
pub fn save(workspace: &Path, chat: &str, plan: &Plan) -> Result<(), String> {
    let path = plan_path(workspace, chat).ok_or("this chat's id can't name a file")?;
    if plan.steps.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
            _ => Ok(()),
        };
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(plan).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| e.to_string())
}

/// `text` on one line (whitespace runs become one space), at most `max` characters.
fn one_line(text: &str, max: usize) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match joined.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &joined[..i]),
        None => joined,
    }
}

/// `update_plan`: replace the chat's plan.
pub struct UpdatePlan;

impl Tool for UpdatePlan {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "update_plan".into(),
            description: "Set your plan for a multi-step task: the whole list each time, one step doing at a time. [] clears it.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "steps": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "text": {"type": "string"},
                                "status": {"type": "string", "enum": ["pending", "doing", "done"]}
                            },
                            "required": ["text", "status"]
                        }
                    }
                },
                "required": ["steps"]
            }),
        }
    }

    fn title(&self, args: &Value) -> String {
        match Plan::from_args(args) {
            Ok(p) if p.steps.is_empty() => "Cleared the plan".into(),
            Ok(p) => format!("Plan: {}/{} done", p.done(), p.steps.len()),
            Err(_) => "Update the plan".into(),
        }
    }

    fn pending_title(&self, _args: &Value) -> String {
        "Update the plan".into()
    }

    fn preview(&self, _args: &Value, _workspace: &Path) -> Option<crate::Preview> {
        None
    }

    fn access(&self, _args: &Value) -> Access {
        Access::Read
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        let plan = match Plan::from_args(args) {
            Ok(p) => p,
            Err(e) => return ToolOutput::err(e),
        };
        if let Some(chat) = ctx.chat()
            && let Err(e) = save(ctx.workspace, chat, &plan)
        {
            log::warn!("keeping the plan of chat {chat}: {e}");
        }
        ctx.attach_view(plan.view());
        ToolOutput::ok(plan.text())
    }
}

/// `ask_user`: a question card; the answer is the result.
pub struct AskUser;

impl AskUser {
    fn question(args: &Value) -> Result<UserQuestion, String> {
        let question = args.get("question").and_then(Value::as_str).map(str::trim).unwrap_or_default();
        if question.is_empty() {
            return Err("`question` is missing".into());
        }
        let options: Vec<String> = match args.get("options") {
            Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(|o| one_line(o, 120)).filter(|o| !o.is_empty()).collect(),
            Some(Value::String(s)) => match serde_json::from_str::<Vec<String>>(s) {
                Ok(v) => v.iter().map(|o| one_line(o, 120)).filter(|o| !o.is_empty()).collect(),
                Err(_) => s.split(['\n', ',']).map(|o| one_line(o, 120)).filter(|o| !o.is_empty()).collect(),
            },
            _ => Vec::new(),
        };
        let mut seen = Vec::new();
        for o in options {
            if !seen.contains(&o) && seen.len() < byob_protocol::MAX_CHOICE_OPTIONS {
                seen.push(o);
            }
        }
        let q: String = question.chars().take(MAX_QUESTION_CHARS).collect();
        Ok(UserQuestion { question: q, options: seen, free_text: true })
    }
}

impl Tool for AskUser {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "ask_user".into(),
            description: "Ask the user a question and wait for the answer. Only when you can't go on without it.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "question": {"type": "string"},
                    "options": {"type": "array", "items": {"type": "string"}, "description": "Short choices (the user may also type their own)."}
                },
                "required": ["question"]
            }),
        }
    }

    fn title(&self, args: &Value) -> String {
        let q = args.get("question").and_then(Value::as_str).unwrap_or_default();
        format!("Asked: {}", one_line(q, 80))
    }

    fn pending_title(&self, args: &Value) -> String {
        self.title(args)
    }

    fn preview(&self, _args: &Value, _workspace: &Path) -> Option<crate::Preview> {
        None
    }

    fn access(&self, _args: &Value) -> Access {
        Access::Read
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        let q = match Self::question(args) {
            Ok(q) => q,
            Err(e) => return ToolOutput::err(e),
        };
        match ctx.ask_user(&q) {
            UserReply::Answered(a) => ToolOutput::ok(format!("The user answered: {a}")),
            UserReply::Skipped => ToolOutput::ok("The user skipped the question. Go on with your best judgement and say what you assumed."),
            UserReply::NoOne => ToolOutput::ok("Nobody could answer now. Go on with your best judgement and say what you assumed."),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn plans_parse_leniently_and_refuse_nonsense() {
        let p = Plan::from_args(&json!({"steps": [
            {"text": "Read  the\ndata", "status": "completed"},
            {"text": "Chart it", "status": "in_progress"},
            "Write it up",
        ]}))
        .unwrap();
        assert_eq!(p.steps.iter().map(|s| s.status).collect::<Vec<_>>(), [Status::Done, Status::Doing, Status::Pending]);
        assert_eq!(p.text(), "Plan (1/3 done):\n[x] Read the data\n[>] Chart it\n[ ] Write it up");
        assert!(p.unfinished());
        // A list sent as a string.
        assert_eq!(Plan::from_args(&json!({"steps": "[{\"text\": \"a\", \"status\": \"done\"}]"})).unwrap().done(), 1);
        for bad in [json!({}), json!({"steps": 3}), json!({"steps": [{"text": ""}]}), json!({"steps": [{"text": "a", "status": "maybe"}]})] {
            assert!(Plan::from_args(&bad).is_err(), "{bad}");
        }
        let many: Vec<Value> = (0..=MAX_STEPS).map(|i| json!(format!("s{i}"))).collect();
        assert!(Plan::from_args(&json!({"steps": many})).is_err());
        let long = "x".repeat(MAX_STEP_CHARS + 50);
        assert_eq!(Plan::from_args(&json!({"steps": [long]})).unwrap().steps[0].text.chars().count(), MAX_STEP_CHARS + 1);
    }

    #[test]
    fn update_plan_keeps_the_plan_per_chat_and_shows_a_card() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);
        let views = Mutex::new(Vec::new());
        let ctx = ToolContext::new(dir.path(), &cancel).with_chat("c-1").with_views(&views);
        let out = UpdatePlan.run(&json!({"steps": [{"text": "a", "status": "doing"}, {"text": "b", "status": "pending"}]}), &ctx);
        assert!(!out.failed, "{}", out.text);
        assert!(out.text.starts_with("Plan (0/2 done):"));
        let plan = load(dir.path(), "c-1").unwrap();
        assert_eq!(plan.steps.len(), 2);
        assert!(load(dir.path(), "other").is_none());
        let views = views.lock().unwrap().clone();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].kind, "byob.tree");
        assert!(views[0].check().is_ok());
        let (tree, faults) = byob_protocol::tree::decode(views[0].data.clone());
        assert!(faults.is_empty(), "{faults:?}");
        assert!(serde_json::to_string(&tree).unwrap().contains("\"text\":\"a\""));
        // Clearing removes the file.
        let out = UpdatePlan.run(&json!({"steps": []}), &ctx);
        assert_eq!(out.text, "Plan cleared.");
        assert!(load(dir.path(), "c-1").is_none());
        // Chat ids that aren't plain names never become paths.
        assert!(plan_path(dir.path(), "../x").is_none());
    }

    struct Says(UserReply, Mutex<Vec<UserQuestion>>);
    impl crate::Asker for Says {
        fn ask(&self, _: &crate::Question) -> crate::Answer {
            crate::Answer::AutoNo
        }
        fn ask_user(&self, q: &UserQuestion) -> UserReply {
            self.1.lock().unwrap().push(q.clone());
            self.0.clone()
        }
    }

    #[test]
    fn ask_user_returns_the_answer_and_copes_without_one() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);
        let says = Says(UserReply::Answered("Metric".into()), Mutex::new(Vec::new()));
        let ctx = ToolContext::new(dir.path(), &cancel).with_asker(&says);
        let out = AskUser.run(&json!({"question": "Units?", "options": ["Metric", "Imperial", "Metric", ""]}), &ctx);
        assert_eq!(out.text, "The user answered: Metric");
        let q = says.1.lock().unwrap()[0].clone();
        assert_eq!(q, UserQuestion { question: "Units?".into(), options: vec!["Metric".into(), "Imperial".into()], free_text: true });
        // Options as a string, too many options.
        let many: Vec<String> = (0..20).map(|i| format!("o{i}")).collect();
        assert_eq!(AskUser::question(&json!({"question": "q", "options": many})).unwrap().options.len(), byob_protocol::MAX_CHOICE_OPTIONS);
        assert_eq!(AskUser::question(&json!({"question": "q", "options": "a, b"})).unwrap().options, ["a", "b"]);
        assert!(AskUser.run(&json!({"options": ["a"]}), &ctx).failed);
        // Skipped, and nobody to ask.
        let skip = Says(UserReply::Skipped, Mutex::new(Vec::new()));
        assert!(AskUser.run(&json!({"question": "q"}), &ToolContext::new(dir.path(), &cancel).with_asker(&skip)).text.contains("skipped"));
        let alone = AskUser.run(&json!({"question": "q"}), &ToolContext::new(dir.path(), &cancel));
        assert!(alone.text.starts_with("Nobody could answer"));
    }
}
