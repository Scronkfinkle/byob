//! Subagents: a `delegate` tool step runs a child turn with its own context (a task and a
//! system prompt, not the chat's history). The step carries the child's live transcript
//! ([`Delegation`]); [`ServerMsg::Delegation`](crate::ServerMsg::Delegation) streams changes to it.
//! A child's tool steps can wait for approval like any other step and are answered with
//! the same [`ClientMsg::Approve`](crate::ClientMsg::Approve).

use serde::{Deserialize, Serialize};

use crate::{Step, StepStatus};

/// Where a subagent run is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationState {
    /// Waiting for a free inference slot.
    #[default]
    Queued,
    Running,
    Done,
    Failed,
    /// Stopped by the user, or cut off by a limit.
    Stopped,
}

impl DelegationState {
    pub fn finished(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Stopped)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }
}

/// The child thread of a `delegate` step.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Delegation {
    /// The agent definition's name (`general` for the default worker).
    pub agent: String,
    /// What the parent model asked it to do.
    pub task: String,
    /// The model it runs on.
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub state: DelegationState,
    /// The child's streamed reply text (all rounds).
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub reasoning: String,
    /// The child's tool calls, in order (ids unique within the engine, like any step).
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Delegation {
    /// "explorer · 3 tools · running".
    pub fn status_line(&self) -> String {
        let n = self.steps.len();
        let tools = if n == 1 { "1 tool".to_owned() } else { format!("{n} tools") };
        let state = if self.steps.iter().any(|s| s.status == StepStatus::Waiting) { "waiting for approval" } else { self.state.label() };
        format!("{} · {tools} · {state}", self.agent)
    }

    /// Apply one [`DelegationUpdate`].
    pub fn apply(&mut self, update: &DelegationUpdate) {
        match update {
            DelegationUpdate::Delta { content, reasoning } => {
                self.content.push_str(content);
                self.reasoning.push_str(reasoning);
            }
            DelegationUpdate::Step(step) => match self.steps.iter_mut().find(|s| s.id == step.id) {
                Some(existing) => *existing = (**step).clone(),
                None => self.steps.push((**step).clone()),
            },
            DelegationUpdate::Status { state, prompt_tokens, completion_tokens, error } => {
                self.state = *state;
                self.prompt_tokens = *prompt_tokens;
                self.completion_tokens = *completion_tokens;
                self.error.clone_from(error);
            }
        }
    }
}

/// A change to the [`Delegation`] of one step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DelegationUpdate {
    /// Streamed text appended to the child's reply.
    Delta { content: String, reasoning: String },
    /// A child step was added or changed (replace by id).
    Step(Box<Step>),
    /// State and token totals.
    Status {
        state: DelegationState,
        prompt_tokens: u64,
        completion_tokens: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}
