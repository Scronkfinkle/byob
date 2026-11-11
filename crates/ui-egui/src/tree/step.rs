//! `byob.tree` views on steps: decoded once per change of their data, each with its own
//! [`TreeState`], their events queued for the app to send.

use std::collections::HashMap;

use byob_protocol::View;
use byob_protocol::tree::{self, Event, Node};
use serde_json::Value;

use super::{TreeState, show_tree};
use crate::theme::Tokens;

/// Trees kept decoded (the least recently drawn go first).
const MAX_CACHED: usize = 64;
/// Events kept until taken.
const MAX_EVENTS: usize = 256;

/// Something the user did in a tree on a step.
#[derive(Debug, Clone, PartialEq)]
pub struct StepEvent {
    pub step: u64,
    /// The view's index on the step.
    pub view: usize,
    pub event: Event,
}

struct Cached {
    data: Value,
    tree: Node,
    state: TreeState,
    drawn: u64,
}

/// The trees on steps.
#[derive(Default)]
pub struct StepTrees {
    trees: HashMap<(u64, usize), Cached>,
    events: Vec<StepEvent>,
}

impl StepTrees {
    /// Draw a `byob.tree` view; `false` if this client can't (a newer schema).
    pub fn show(&mut self, ui: &mut egui::Ui, t: &Tokens, step: u64, index: usize, view: &View) -> bool {
        if view.kind != tree::KIND || view.v > tree::VERSION {
            return false;
        }
        let pass = ui.ctx().cumulative_pass_nr();
        let c = self.trees.entry((step, index)).or_insert_with(|| Cached {
            data: Value::Null,
            tree: Node::new(tree::Kind::Column { children: Vec::new() }),
            state: TreeState::with_id(egui::Id::unique(("byob-step-tree", step, index))),
            drawn: pass,
        });
        if c.data != view.data {
            let (tree, faults) = tree::decode(view.data.clone());
            if !faults.is_empty() {
                log::warn!("view tree on step {step}: {}", tree::Fault::report(&faults));
            }
            c.data = view.data.clone();
            c.tree = tree;
            c.state.replied(&serde_json::Map::new());
        }
        c.drawn = pass;
        let events = show_tree(ui, t, &c.tree, &mut c.state);
        for event in events {
            if self.events.len() >= MAX_EVENTS {
                self.events.remove(0);
            }
            self.events.push(StepEvent { step, view: index, event });
        }
        if self.trees.len() > MAX_CACHED
            && let Some(oldest) = self.trees.iter().min_by_key(|(_, c)| c.drawn).map(|(k, _)| *k)
        {
            self.trees.remove(&oldest);
        }
        true
    }

    /// What the user did since the last call, oldest first.
    pub fn take_events(&mut self) -> Vec<StepEvent> {
        std::mem::take(&mut self.events)
    }
}
