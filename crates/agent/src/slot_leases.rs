//! Which llama.cpp server slot each model request runs in (`"id_slot"`).
//!
//! A llama.cpp server with `total_slots` parallel slots keeps one prompt cache per slot.
//! Left to itself it may put a subagent into the slot of a chat whose turn is waiting on
//! `delegate` (no request in flight) and throw that chat's cache away. So the engine leases
//! slots, per endpoint ([`adapt::key`]): each chat gets a slot it keeps across turns
//! (sticky), and a subagent gets one no running turn or subagent holds, preferring slots no
//! chat is sticky to. A lease is held for the whole turn or subagent run and released on
//! drop (so also on error, stop or panic). When every slot is leased the request carries no
//! slot and the server picks; nothing waits.
//!
//! Only endpoints whose probe reported `total_slots` get a slot ([`adapt::Plan::slots`]),
//! and only when the request goes to the endpoint the plan was made for: hosted APIs reject
//! unknown fields.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use byob_types::InferenceConfig;

use crate::adapt;

/// Most slots tracked per endpoint (the probe already ignores silly values).
const MAX_SLOTS: usize = 1024;

#[derive(Debug, Default, Clone)]
struct Slot {
    leased: bool,
    /// The chat that last ran here (its cache is in this slot).
    owner: Option<String>,
    /// When it was last leased (a tick), to reuse the least recently used one.
    used: u64,
}

#[derive(Debug, Default)]
struct State {
    /// Slots by endpoint key.
    endpoints: BTreeMap<String, Vec<Slot>>,
    tick: u64,
}

/// The engine's slot leases, shared with turn and subagent threads.
#[derive(Debug, Default)]
pub struct SlotTable {
    state: Mutex<State>,
}

/// Who wants a slot.
enum For<'a> {
    Chat(&'a str),
    Subagent,
}

impl SlotTable {
    /// A slot for a turn of `chat`: the one it had before if free, else a free one.
    pub fn for_chat(self: &Arc<Self>, plan: &adapt::Plan, config: &InferenceConfig, chat: &str) -> Option<SlotLease> {
        self.lease(plan, config, For::Chat(chat))
    }

    /// A slot for a subagent: one nobody holds now, preferably no chat's.
    pub fn for_subagent(self: &Arc<Self>, plan: &adapt::Plan, config: &InferenceConfig) -> Option<SlotLease> {
        self.lease(plan, config, For::Subagent)
    }

    fn lease(self: &Arc<Self>, plan: &adapt::Plan, config: &InferenceConfig, who: For<'_>) -> Option<SlotLease> {
        let total = usize::try_from(plan.slots?).ok()?.min(MAX_SLOTS);
        // A plan made for another endpoint (e.g. a subagent model id on the main URL) says
        // nothing about this one.
        if total == 0 || plan.key != adapt::key(config) {
            return None;
        }
        let mut guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let state = &mut *guard;
        state.tick = state.tick.saturating_add(1);
        let slots = state.endpoints.entry(plan.key.clone()).or_default();
        slots.resize(total, Slot::default());
        // Free slots: no chat's first, then the least recently used.
        let pick = |slots: &mut [Slot]| slots.iter().enumerate().filter(|(_, s)| !s.leased).min_by_key(|(_, s)| (s.owner.is_some(), s.used)).map(|(i, _)| i);
        let index = match who {
            For::Chat(chat) => {
                let mine = slots.iter().position(|s| s.owner.as_deref() == Some(chat) && !s.leased);
                let i = mine.or_else(|| pick(slots))?;
                for s in slots.iter_mut().filter(|s| s.owner.as_deref() == Some(chat)) {
                    s.owner = None;
                }
                slots.get_mut(i)?.owner = Some(chat.to_owned());
                i
            }
            For::Subagent => pick(slots)?,
        };
        let slot = slots.get_mut(index)?;
        slot.leased = true;
        slot.used = state.tick;
        Some(SlotLease { table: Arc::clone(self), key: plan.key.clone(), index })
    }

    #[cfg(test)]
    fn leased(&self, key: &str) -> Vec<usize> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.endpoints.get(key).map(|s| s.iter().enumerate().filter(|(_, s)| s.leased).map(|(i, _)| i).collect()).unwrap_or_default()
    }
}

/// A leased slot; released on drop.
#[derive(Debug)]
pub struct SlotLease {
    table: Arc<SlotTable>,
    key: String,
    index: usize,
}

impl SlotLease {
    /// The slot's `id_slot`.
    pub fn id(&self) -> u64 {
        u64::try_from(self.index).unwrap_or(u64::MAX)
    }
}

impl Drop for SlotLease {
    fn drop(&mut self) {
        let mut state = self.table.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(s) = state.endpoints.get_mut(&self.key).and_then(|v| v.get_mut(self.index)) {
            s.leased = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> InferenceConfig {
        InferenceConfig { base_url: "http://h/v1".into(), model: "m".into(), api_key: None, api: None, ..Default::default() }
    }

    fn plan(slots: Option<u64>) -> adapt::Plan {
        let mut e = adapt::Endpoints::default();
        let caps = byob_llm::Capabilities { slots, ..Default::default() };
        e.handle(adapt::Event::Probed { key: adapt::key(&config()), caps: Box::new(caps) });
        e.plan(&config())
    }

    #[test]
    fn chats_keep_their_slot_and_subagents_avoid_them() {
        let t = Arc::new(SlotTable::default());
        let (p, c) = (plan(Some(4)), config());
        let a = t.for_chat(&p, &c, "a").unwrap();
        assert_eq!(a.id(), 0);
        drop(a);
        let b = t.for_chat(&p, &c, "b").unwrap();
        assert_eq!(b.id(), 1, "a free slot no chat has");
        let a = t.for_chat(&p, &c, "a").unwrap();
        assert_eq!(a.id(), 0, "sticky");
        drop(b);
        // b is idle and its slot free: subagents still take the slots nobody's cache is in.
        let s1 = t.for_subagent(&p, &c).unwrap();
        let s2 = t.for_subagent(&p, &c).unwrap();
        assert_eq!((s1.id(), s2.id()), (2, 3));
        // The last free slot is b's; a third subagent gets it, a fourth none.
        let s3 = t.for_subagent(&p, &c).unwrap();
        assert_eq!(s3.id(), 1);
        assert!(t.for_subagent(&p, &c).is_none(), "all leased: let the server pick");
        drop((s1, s2, s3));
        assert_eq!(t.leased(&p.key), [0]);
        // b's cache is still in slot 1 (subagents don't take over a chat's slot).
        assert_eq!(t.for_chat(&p, &c, "b").unwrap().id(), 1);
    }

    #[test]
    fn no_slot_without_total_slots_or_for_another_endpoint() {
        let t = Arc::new(SlotTable::default());
        assert!(t.for_chat(&plan(None), &config(), "a").is_none());
        let other = InferenceConfig { model: "other".into(), ..config() };
        assert!(t.for_subagent(&plan(Some(4)), &other).is_none());
        assert!(t.for_chat(&plan(Some(4)), &config(), "a").is_some());
    }
}
