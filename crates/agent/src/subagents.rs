//! The engine side of subagents: building a turn's [`delegate::Env`], keeping each running
//! subagent's transcript in its `delegate` step, recording it as an agent run in the store,
//! and routing its events (text, steps, approvals, usage) to the chat's viewers.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError};
use std::time::Instant;

use byob_protocol::{DelegationState, DelegationUpdate, NoticeLevel, ServerMsg, Step};
use byob_store::{MessageStats, NewAgentRun};
use byob_types::config::ToolMode;
use byob_types::profile::{self, Profile};

use crate::workspaces::ChatSettings;
use crate::{ChildRun, Engine, adapt, chats, delegate, turn};

/// A fresh, store-wide unique run id.
fn new_run_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("r{:x}{n:x}", chrono::Utc::now().timestamp_millis())
}

impl Engine {
    /// What a turn in `workspace` needs to delegate, if subagents are on and there are tools.
    /// `settings` are the chat's (its endpoint is the model of subagents nothing else picks a
    /// model for, its effective subagent model the level's); `llm` and `plan` are the chat's;
    /// `guidance` is the plugins' prompt lines with their tools (`Composed::tool_prompts`).
    pub(crate) fn delegation_env(
        &self,
        settings: &ChatSettings,
        llm: &byob_llm::Client,
        plan: &adapt::Plan,
        workspace: &Path,
        plugin_agents: &[byob_plugins::AgentDef],
        guidance: Vec<delegate::PluginPrompt>,
    ) -> Option<delegate::Env> {
        if !self.subagents.enabled || self.tools.is_empty() {
            return None;
        }
        let eff = &settings.effective;
        let chat_profile = settings.profile.as_deref().and_then(|id| profile::find(&self.profiles, Some(id)));
        let mut main = delegate::Model::new(settings.inference.clone(), llm.clone(), plan.clone());
        main.profile = chat_profile.map(|p| p.label().to_owned());
        main.limit = chat_profile.and_then(|p| self.profile_limit(p));
        // The levels' tool mode, unless the chat's profile chose it (then it is that model's).
        let level_mode = if chat_profile.is_some_and(|p| p.tool_mode.is_some()) { ToolMode::Auto } else { eff.tool_mode.value };
        let profiles: Vec<delegate::Choice> = self.profiles.values().filter(|p| p.usable()).map(|p| self.choice(p, level_mode, plan.image_quality)).collect();
        let mut roles = BTreeMap::new();
        for (name, config) in &self.models {
            // A role that points at a profile is that profile (its name, its cap).
            let of_profile = self.roles.get(name).and_then(|id| profiles.iter().find(|c| c.id == *id)).and_then(delegate::Choice::model);
            if let Some(m) = of_profile {
                roles.insert(name.clone(), m);
                continue;
            }
            match byob_llm::Client::new(config.clone()) {
                Ok(llm) => {
                    roles.insert(name.clone(), delegate::Model::new(config.clone(), llm, self.endpoints.plan(config)));
                }
                Err(e) => log::warn!("model role {name}: {e}"),
            }
        }
        let default = eff.subagent_profile.as_ref().and_then(|s| profiles.iter().find(|c| c.id == s.value)).cloned();
        let configured = byob_plugins::merge_agents([self.agents.clone(), plugin_agents.to_vec()]);
        Some(delegate::Env {
            agents: delegate::agents_for(workspace, &configured),
            main,
            roles,
            profiles,
            default,
            slots: Arc::clone(&self.slots),
            leases: Arc::clone(&self.leases),
            opts: self.subagents,
            guidance,
        })
    }

    /// Profile `p` as a subagent model: its endpoint and key, a plan for it (its own tool
    /// mode, else `mode`), and its cap.
    fn choice(&self, p: &Profile, mode: ToolMode, image_quality: u8) -> delegate::Choice {
        let config = p.config(self.profile_key(&p.id));
        let mut plan = self.endpoints.plan_with(&config, p.tool_mode.unwrap_or(mode));
        plan.image_quality = image_quality;
        delegate::Choice {
            id: p.id.clone(),
            name: p.label().to_owned(),
            description: p.description.clone(),
            offered: p.subagents,
            config,
            plan,
            limit: self.profile_limit(p),
        }
    }

    /// The shared counter of subagents running on `p`, if it caps them (`max_parallel`).
    fn profile_limit(&self, p: &Profile) -> Option<Arc<delegate::Slots>> {
        let n = usize::from(p.max_parallel.filter(|n| *n > 0)?);
        let mut all = self.profile_slots.lock().unwrap_or_else(PoisonError::into_inner);
        let slots = all.entry(p.id.clone()).or_insert_with(|| Arc::new(delegate::Slots::new(n)));
        if slots.capacity() != n {
            slots.set_capacity(n);
        }
        Some(Arc::clone(slots))
    }

    /// A `delegate` step was added or changed: record its run when it first appears, and
    /// finish it in the store once the delegation has ended.
    pub(crate) fn track_delegation(&mut self, entry: u64, step: &Step) {
        let Some(d) = step.delegation.as_deref() else { return };
        let Some(run) = self.turns.get_mut(&entry) else { return };
        if !run.children.contains_key(&step.id) {
            let chat = run.chat.clone();
            let recorded = run.steps.get(&step.id).copied().and_then(|seq| {
                let new = NewAgentRun {
                    id: new_run_id(),
                    parent: chats::message_id(entry),
                    step_seq: seq,
                    agent: d.agent.clone(),
                    task: d.task.clone(),
                    model: d.model.clone(),
                };
                match self.store.add_agent_run(&chat, &new) {
                    Ok(()) => Some(new.id),
                    Err(e) => {
                        log::error!("recording a subagent run: {e}");
                        None
                    }
                }
            });
            let Some(run) = self.turns.get_mut(&entry) else { return };
            run.children.insert(
                step.id,
                ChildRun { run: recorded, unsaved: Default::default(), steps: BTreeMap::new(), started: Instant::now(), model_calls: 0, ended: false },
            );
        }
        if !d.state.finished() {
            // Time counts from when it got a slot, not from when it queued.
            if d.state == DelegationState::Running
                && let Some(child) = self.turns.get_mut(&entry).and_then(|r| r.children.get_mut(&step.id))
            {
                child.started = Instant::now();
            }
            return;
        }
        let Some(run) = self.turns.get_mut(&entry) else { return };
        let chat = run.chat.clone();
        let Some(child) = run.children.get_mut(&step.id).filter(|c| !c.ended) else { return };
        child.ended = true;
        let (content, reasoning) = std::mem::take(&mut child.unsaved);
        let stats = MessageStats {
            prompt_tokens: d.prompt_tokens,
            completion_tokens: d.completion_tokens,
            elapsed_ms: u64::try_from(child.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            model_calls: child.model_calls,
            context_tokens: 0,
        };
        let Some(id) = child.run.clone() else { return };
        if let Err(e) = self.store.append_agent_text(&chat, &id, &content, &reasoning) {
            log::error!("saving a subagent's streamed text: {e}");
        }
        if let Err(e) = self.store.finish_agent_run(&chat, &id, d.error.as_deref(), Some(&stats)) {
            log::error!("saving the end of a subagent run: {e}");
        }
    }

    /// An event of the subagent run shown by step `parent`.
    pub(crate) fn handle_child(&mut self, parent: u64, event: turn::TurnEvent) {
        match event {
            turn::TurnEvent::Delta { id, content, reasoning } => {
                let Some(run) = self.turns.get_mut(&id) else { return };
                if let Some(child) = run.children.get_mut(&parent) {
                    child.unsaved.0.push_str(&content);
                    child.unsaved.1.push_str(&reasoning);
                }
                self.delegation_update(id, parent, DelegationUpdate::Delta { content, reasoning });
            }
            turn::TurnEvent::Step { id, step } => {
                if !self.pending.contains_key(&step.id) {
                    self.update_child_step(id, parent, step);
                }
            }
            turn::TurnEvent::Gate { id, step, reply } => self.gate(id, step, reply, Some(parent)),
            // A subagent's file changes belong to the reply that delegated: one undo for all.
            turn::TurnEvent::Undo { id, ops, complete } => self.add_undo(id, ops, complete),
            turn::TurnEvent::Usage { id, prompt, completion, .. } => {
                let Some(run) = self.turns.get_mut(&id) else { return };
                let chat = run.chat.clone();
                if let Some(child) = run.children.get_mut(&parent) {
                    child.model_calls = child.model_calls.saturating_add(1);
                }
                if let Some(live) = self.live.get_mut(&chat) {
                    live.tally.agent_call(prompt, completion);
                }
                let status = self.reply_mut(id).and_then(|e| e.steps.iter_mut().find(|s| s.id == parent)).and_then(|s| s.delegation.as_deref_mut()).map(|d| {
                    d.prompt_tokens = d.prompt_tokens.saturating_add(prompt);
                    d.completion_tokens = d.completion_tokens.saturating_add(completion);
                    DelegationUpdate::Status { state: d.state, prompt_tokens: d.prompt_tokens, completion_tokens: d.completion_tokens, error: d.error.clone() }
                });
                if let Some(update) = status {
                    self.send_delegation(id, parent, update);
                }
                self.send_usage(&chat);
            }
            turn::TurnEvent::Notice { id, text } => {
                let Some(chat) = self.turns.get(&id).map(|t| t.chat.clone()) else { return };
                self.send_viewers(&chat, ServerMsg::Notice { level: NoticeLevel::Info, text });
            }
            // Child turns don't compact and are finished by their `delegate` step.
            turn::TurnEvent::Finished { .. } | turn::TurnEvent::Compacting { .. } | turn::TurnEvent::Compacted { .. } => {}
        }
    }

    /// Add or replace a step of subagent run `parent` (store, transcript, viewers).
    pub(crate) fn update_child_step(&mut self, entry: u64, parent: u64, step: Step) {
        self.persist_child_step(entry, parent, &step);
        let Some(chat) = self.turns.get(&entry).map(|t| t.chat.clone()) else { return };
        if step.status.finished() && !step.artifacts.is_empty() {
            self.artifacts_changed(&chat, &step.artifacts);
        }
        self.delegation_update(entry, parent, DelegationUpdate::Step(Box::new(step)));
    }

    fn persist_child_step(&mut self, entry: u64, parent: u64, step: &Step) {
        let Some(run) = self.turns.get_mut(&entry) else { return };
        let chat = run.chat.clone();
        let Some(child) = run.children.get_mut(&parent) else { return };
        let Some(id) = child.run.clone() else { return };
        let seq = match child.steps.get(&step.id) {
            Some(seq) => *seq,
            None => match self.store.add_agent_step(&chat, &id, &chats::new_step(step)) {
                Ok(seq) => {
                    child.steps.insert(step.id, seq);
                    seq
                }
                Err(e) => {
                    log::error!("saving subagent step {}: {e}", step.tool);
                    return;
                }
            },
        };
        if step.status.finished()
            && let Err(e) = self.store.finish_agent_step(&chat, &id, seq, &chats::step_result(step))
        {
            log::error!("saving subagent step result {}: {e}", step.tool);
        }
    }

    /// Apply an update to the live transcript and send it to the chat's viewers.
    fn delegation_update(&mut self, entry: u64, parent: u64, update: DelegationUpdate) {
        if let Some(e) = self.reply_mut(entry) {
            e.apply_delegation(parent, &update);
        }
        self.send_delegation(entry, parent, update);
    }

    fn send_delegation(&mut self, entry: u64, step: u64, update: DelegationUpdate) {
        let Some(chat) = self.turns.get(&entry).map(|t| t.chat.clone()) else { return };
        self.send_viewers(&chat, ServerMsg::Delegation { entry, step, update });
    }
}
