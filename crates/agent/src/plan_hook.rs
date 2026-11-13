//! The bundled `plan` plugin's hook: before each model request, restate the chat's plan (kept
//! by `update_plan`, `byob_tools::plan`) at the end of the newest message, so a small model
//! doesn't lose track of a long task. Cheap: one line per step, only while a step is open,
//! and never right after `update_plan` itself (its result is the plan). The text goes at the
//! end, so earlier messages (and the server's prompt cache) stay as they were.

use byob_tools::plan;

use crate::hooks::{Flow, NativeHook, RequestCtx};

/// The id manifests name (`hooks[].native`).
pub const HOOK: &str = "plan";
/// After compaction (100), so the note lands on what is actually sent.
pub const PRIORITY: i64 = 200;

/// Restates the plan (see the module docs).
pub struct PlanHook;

impl NativeHook for PlanHook {
    fn before_request(&self, ctx: &mut RequestCtx<'_>) -> Flow {
        if ctx.child {
            return Flow::Continue;
        }
        let Some(plan) = plan::load(ctx.workspace, ctx.chat) else { return Flow::Continue };
        if !plan.unfinished() {
            return Flow::Continue;
        }
        let text = plan.text();
        let Some(last) = ctx.messages.last_mut() else { return Flow::Continue };
        if !matches!(last.role.as_str(), "user" | "tool") {
            return Flow::Continue;
        }
        let content = last.content.get_or_insert_with(String::new);
        // update_plan's own result already is the plan.
        if content.contains(&text) {
            return Flow::Continue;
        }
        let note = if ctx.round == 0 {
            format!("\n\n(Your plan in this chat so far; keep it current with update_plan, or clear it with [] if it no longer applies.)\n{text}")
        } else {
            format!("\n\n[Current plan]\n{text}")
        };
        content.push_str(&note);
        Flow::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::atomic::AtomicBool;

    use byob_llm::{ChatMessage, Usage};
    use byob_tools::plan::{Plan, PlanStep, Status};

    fn run(ws: &Path, chat: &str, round: usize, child: bool, messages: &mut Vec<ChatMessage>) {
        let llm = byob_llm::Client::new(byob_types::InferenceConfig::default()).unwrap();
        let p = crate::adapt::Plan { key: String::new(), prompt_tools: false, fallback: false, context: 1000, reserve: 0, vision: None, image_quality: 0 };
        let (mut specs, mut usage, mut calls, mut state) = (Vec::new(), Usage::default(), 0, BTreeMap::new());
        let mut ctx = RequestCtx {
            entry: 1,
            round,
            child,
            chat,
            workspace: ws,
            messages,
            specs: &mut specs,
            llm: &llm,
            plan: &p,
            cancel: &AtomicBool::new(false),
            ids: &[],
            prior: 0,
            compact: None,
            usage: &mut usage,
            model_calls: &mut calls,
            state: &mut state,
            emit: &|_| {},
        };
        assert_eq!(PlanHook.before_request(&mut ctx), Flow::Continue);
    }

    fn steps(s: &[(&str, Status)]) -> Plan {
        Plan { steps: s.iter().map(|(t, st)| PlanStep { text: (*t).into(), status: *st }).collect() }
    }

    #[test]
    fn an_open_plan_is_restated_at_the_end_and_a_finished_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        let base = vec![ChatMessage::system("s"), ChatMessage::user("go on")];
        // No plan: nothing changes.
        let mut m = base.clone();
        run(ws, "c1", 0, false, &mut m);
        assert_eq!(m, base);
        plan::save(ws, "c1", &steps(&[("Read", Status::Done), ("Chart", Status::Doing)])).unwrap();
        let mut m = base.clone();
        run(ws, "c1", 0, false, &mut m);
        let last = m[1].content.clone().unwrap();
        assert!(last.starts_with("go on\n\n(Your plan in this chat so far"), "{last}");
        assert!(last.ends_with("Plan (1/2 done):\n[x] Read\n[>] Chart"), "{last}");
        assert_eq!(m[0], base[0], "the system prompt stays");
        // Later rounds: on the newest tool result, once.
        let mut m = vec![ChatMessage::system("s"), ChatMessage::user("u"), ChatMessage::tool_result("1", "file text")];
        run(ws, "c1", 2, false, &mut m);
        assert!(m[2].content.as_deref().unwrap().starts_with("file text\n\n[Current plan]\nPlan (1/2 done)"));
        let again = m.clone();
        run(ws, "c1", 3, false, &mut m);
        assert_eq!(m, again, "already there");
        // update_plan's own result is the plan: no second copy.
        let own = steps(&[("Read", Status::Done), ("Chart", Status::Doing)]).text();
        let mut m = vec![ChatMessage::user("u"), ChatMessage::tool_result("1", own.clone())];
        run(ws, "c1", 1, false, &mut m);
        assert_eq!(m[1].content.as_deref(), Some(own.as_str()));
        // Subagents and other chats don't get it; nor does a finished plan.
        let mut m = base.clone();
        run(ws, "c1", 0, true, &mut m);
        run(ws, "c2", 0, false, &mut m);
        assert_eq!(m, base);
        plan::save(ws, "c1", &steps(&[("Read", Status::Done)])).unwrap();
        run(ws, "c1", 0, false, &mut m);
        assert_eq!(m, base);
    }
}
