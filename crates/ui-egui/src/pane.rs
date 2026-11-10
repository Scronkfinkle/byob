//! The artifact pane's state, as plain data and pure transitions (drawn by
//! [`crate::artifact`]): which artifacts are open as tabs and which is in front, which are
//! minimized to a one-line chip, whether the pane is open or collapsed to a rail, and its
//! width.
//!
//! Width and open/collapsed are per device; tabs and minimized artifacts per chat (the
//! [`PanePrefs::chats`] list, most recently used last, capped). The app stores [`PanePrefs`]
//! next to the appearance prefs under [`PanePrefs::STORAGE_KEY`].
//!
//! On narrow screens (phones) the pane is full screen when open, so it is never opened by
//! itself there: it opens from the floating button and "← Chat" closes it again. That flag
//! is per session ([`Pane::narrow_open`]).
//!
//! On wide windows the open pane can be maximized to take the whole window below the header
//! (chat and chat list hidden); that too is per session, and only while the pane is open
//! ([`Pane::maximized`]).

use serde::{Deserialize, Serialize};

/// Open tabs per chat; opening another drops the least recently opened.
pub const MAX_TABS: usize = 12;
/// Chats whose tabs are remembered.
pub const MAX_CHATS: usize = 32;
/// Minimized artifacts remembered per chat.
pub const MAX_MINIMIZED: usize = 256;
/// Pane widths kept (points).
pub const MIN_WIDTH: f32 = 300.0;
pub const MAX_WIDTH: f32 = 4000.0;
/// Longest artifact or chat id kept from storage (real ones are short).
const MAX_ID: usize = 128;

/// One chat's tabs and minimized artifacts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ChatTabs {
    pub chat: String,
    /// Artifact ids, left to right.
    pub tabs: Vec<String>,
    /// The tab in front.
    pub active: Option<String>,
    /// Artifacts shown as chips (in the chat and the pane) instead of in full.
    pub minimized: Vec<String>,
    /// Order the tabs were opened in, oldest first (which one to drop at [`MAX_TABS`]).
    #[serde(skip)]
    opened: Vec<String>,
}

/// What the device remembers about the pane.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PanePrefs {
    /// Width the user dragged the pane to (`None`: about half the window).
    pub width: Option<f32>,
    /// Collapsed to the rail (when it has tabs).
    pub collapsed: bool,
    /// Per chat, most recently used last.
    pub chats: Vec<ChatTabs>,
}

/// Equal when what is stored is (the open order is rebuilt on load).
impl PartialEq for ChatTabs {
    fn eq(&self, o: &Self) -> bool {
        (&self.chat, &self.tabs, &self.active, &self.minimized) == (&o.chat, &o.tabs, &o.active, &o.minimized)
    }
}

fn ok_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_ID
}

fn dedup(list: &mut Vec<String>, cap: usize) {
    let mut seen = std::collections::HashSet::new();
    list.retain(|id| ok_id(id) && seen.insert(id.clone()));
    list.truncate(cap);
}

impl ChatTabs {
    #[must_use]
    fn sanitized(mut self) -> Self {
        dedup(&mut self.tabs, MAX_TABS);
        dedup(&mut self.minimized, MAX_MINIMIZED);
        if self.active.as_ref().is_some_and(|a| !self.tabs.contains(a)) {
            self.active = None;
        }
        if self.active.is_none() {
            self.active = self.tabs.first().cloned();
        }
        self.opened.clone_from(&self.tabs);
        self
    }

    fn is_empty(&self) -> bool {
        self.tabs.is_empty() && self.minimized.is_empty()
    }
}

impl PanePrefs {
    /// Storage key used by the apps.
    pub const STORAGE_KEY: &'static str = "byob.pane";

    /// Clamp what came from (untrusted) storage.
    #[must_use]
    pub fn sanitized(mut self) -> Self {
        self.width = self.width.filter(|w| w.is_finite()).map(|w| w.clamp(MIN_WIDTH, MAX_WIDTH));
        let mut seen = std::collections::HashSet::new();
        // Keep the most recent entry per chat.
        let mut chats: Vec<ChatTabs> =
            self.chats.into_iter().rev().filter(|c| ok_id(&c.chat) && seen.insert(c.chat.clone())).map(ChatTabs::sanitized).filter(|c| !c.is_empty()).collect();
        chats.truncate(MAX_CHATS);
        chats.reverse();
        self.chats = chats;
        self
    }
}

/// How the pane shows this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shown {
    /// No tabs: nothing on the right.
    Hidden,
    /// Collapsed: the rail (wide) or the floating button (narrow).
    Rail,
    Open,
}

/// The pane state for the chat on screen.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pane {
    prefs: PanePrefs,
    /// The current chat's entry (taken out of `prefs.chats` while it is current).
    cur: ChatTabs,
    /// Phones: the pane covers the chat (per session; it never opens by itself there).
    pub narrow_open: bool,
    /// Wide windows: the pane covers the chat and the chat list (per session).
    maximized: bool,
}

impl Pane {
    pub fn new(prefs: PanePrefs) -> Self {
        Self { prefs: prefs.sanitized(), ..Self::default() }
    }

    /// The prefs to store (the current chat included).
    pub fn prefs(&self) -> PanePrefs {
        let mut p = self.prefs.clone();
        if ok_id(&self.cur.chat) && !self.cur.is_empty() {
            p.chats.push(self.cur.clone());
        }
        p.sanitized()
    }

    /// Restore stored prefs (keeps the chat on screen).
    pub fn set_prefs(&mut self, prefs: PanePrefs) {
        let chat = std::mem::take(&mut self.cur.chat);
        self.prefs = prefs.sanitized();
        self.cur = ChatTabs::default();
        self.narrow_open = false;
        self.maximized = false;
        self.switch_chat((!chat.is_empty()).then_some(chat.as_str()));
    }

    pub fn chat(&self) -> Option<&str> {
        (!self.cur.chat.is_empty()).then_some(self.cur.chat.as_str())
    }

    /// Another chat came on screen: put this one's tabs away, bring that one's out.
    pub fn switch_chat(&mut self, chat: Option<&str>) {
        let chat = chat.unwrap_or_default();
        if chat == self.cur.chat {
            return;
        }
        let old = std::mem::take(&mut self.cur);
        if ok_id(&old.chat) && !old.is_empty() {
            self.prefs.chats.retain(|c| c.chat != old.chat);
            self.prefs.chats.push(old);
            let excess = self.prefs.chats.len().saturating_sub(MAX_CHATS);
            self.prefs.chats.drain(..excess);
        }
        self.cur = match self.prefs.chats.iter().position(|c| c.chat == chat) {
            Some(i) => self.prefs.chats.remove(i),
            None => ChatTabs { chat: chat.to_owned(), ..ChatTabs::default() },
        };
        self.narrow_open = false;
        self.maximized = false;
    }

    /// Forget tabs and minimized marks of artifacts the chat no longer has.
    pub fn retain(&mut self, exists: impl Fn(&str) -> bool) {
        self.cur.tabs.retain(|id| exists(id));
        self.cur.opened.retain(|id| exists(id));
        self.cur.minimized.retain(|id| exists(id));
        if self.cur.active.as_deref().is_some_and(|a| !exists(a)) {
            self.cur.active = self.cur.tabs.first().cloned();
        }
    }

    pub fn tabs(&self) -> &[String] {
        &self.cur.tabs
    }

    pub fn active(&self) -> Option<&str> {
        self.cur.active.as_deref()
    }

    pub fn width(&self) -> Option<f32> {
        self.prefs.width
    }

    pub fn set_width(&mut self, w: f32) {
        if w.is_finite() {
            self.prefs.width = Some(w.clamp(MIN_WIDTH, MAX_WIDTH));
        }
    }

    pub fn collapsed(&self) -> bool {
        self.prefs.collapsed
    }

    pub fn shown(&self, narrow: bool) -> Shown {
        if self.cur.tabs.is_empty() || self.cur.active.is_none() {
            Shown::Hidden
        } else if if narrow { self.narrow_open } else { !self.prefs.collapsed } {
            Shown::Open
        } else {
            Shown::Rail
        }
    }

    /// Add `id` as a tab (if it isn't one) and bring it to the front. Doesn't open the pane.
    pub fn add_tab(&mut self, id: &str) {
        if !ok_id(id) {
            return;
        }
        if !self.cur.tabs.iter().any(|t| t == id) {
            if self.cur.tabs.len() >= MAX_TABS {
                // Drop the least recently opened tab that isn't in front.
                let active = self.cur.active.clone();
                if let Some(old) = self.cur.opened.iter().find(|t| Some(*t) != active.as_ref()).cloned() {
                    self.close(&old);
                }
            }
            // Next to the tab in front, like browsers do.
            let at = self.cur.active.as_ref().and_then(|a| self.cur.tabs.iter().position(|t| t == a)).map_or(self.cur.tabs.len(), |i| i + 1);
            self.cur.tabs.insert(at.min(self.cur.tabs.len()), id.to_owned());
        }
        self.cur.opened.retain(|t| t != id);
        self.cur.opened.push(id.to_owned());
        self.cur.active = Some(id.to_owned());
    }

    /// Show `id` in the pane: a tab in front, the pane open (and `id` restored).
    pub fn show(&mut self, id: &str, narrow: bool) {
        self.add_tab(id);
        self.set_minimized(id, false);
        self.expand(narrow);
    }

    /// A new version of `id` arrived: bring it forward without covering the chat on a
    /// phone or undoing the user's collapse; opens the pane if it had nothing in it.
    pub fn reveal(&mut self, id: &str, narrow: bool) {
        let was_hidden = self.shown(narrow) == Shown::Hidden;
        self.add_tab(id);
        self.set_minimized(id, false);
        if was_hidden && !narrow {
            self.prefs.collapsed = false;
        }
    }

    pub fn activate(&mut self, id: &str) {
        if self.cur.tabs.iter().any(|t| t == id) {
            self.cur.active = Some(id.to_owned());
        }
    }

    /// Close a tab (the artifact stays); the neighbour to the right, else left, comes forward.
    pub fn close(&mut self, id: &str) {
        let Some(i) = self.cur.tabs.iter().position(|t| t == id) else { return };
        self.cur.tabs.remove(i);
        self.cur.opened.retain(|t| t != id);
        if self.cur.active.as_deref() == Some(id) {
            self.cur.active = self.cur.tabs.get(i).or_else(|| self.cur.tabs.get(i.wrapping_sub(1))).cloned();
        }
        if self.cur.tabs.is_empty() {
            self.narrow_open = false;
            self.maximized = false;
        }
    }

    pub fn collapse(&mut self, narrow: bool) {
        if narrow {
            self.narrow_open = false;
        } else {
            self.prefs.collapsed = true;
            self.maximized = false;
        }
    }

    pub fn expand(&mut self, narrow: bool) {
        if narrow {
            self.narrow_open = true;
        } else {
            self.prefs.collapsed = false;
        }
    }

    /// Open ↔ collapsed. With no tabs, opens `fallback` (e.g. the newest artifact) if any.
    pub fn toggle(&mut self, narrow: bool, fallback: Option<&str>) {
        match self.shown(narrow) {
            Shown::Open => self.collapse(narrow),
            Shown::Rail => self.expand(narrow),
            Shown::Hidden => {
                if let Some(id) = fallback {
                    self.show(id, narrow);
                }
            }
        }
    }

    /// The pane covers the whole window below the header (wide windows, pane open).
    pub fn maximized(&self, narrow: bool) -> bool {
        self.maximized && !narrow && self.shown(false) == Shown::Open
    }

    /// Maximize (opening the pane, if it has a tab to show) or restore.
    pub fn set_maximized(&mut self, maximized: bool) {
        if maximized && self.shown(false) == Shown::Hidden {
            return;
        }
        if maximized {
            self.prefs.collapsed = false;
        }
        self.maximized = maximized;
    }

    pub fn is_minimized(&self, id: &str) -> bool {
        self.cur.minimized.iter().any(|m| m == id)
    }

    pub fn set_minimized(&mut self, id: &str, minimized: bool) {
        let has = self.is_minimized(id);
        if minimized && !has && ok_id(id) && self.cur.minimized.len() < MAX_MINIMIZED {
            self.cur.minimized.push(id.to_owned());
        } else if !minimized && has {
            self.cur.minimized.retain(|m| m != id);
        }
    }

    /// "Collapse all": every artifact in `ids` minimized; `false` restores them all.
    pub fn set_all_minimized<'a>(&mut self, ids: impl IntoIterator<Item = &'a str>, minimized: bool) {
        if minimized {
            for id in ids {
                self.set_minimized(id, true);
            }
        } else {
            self.cur.minimized.clear();
        }
    }

    pub fn any_minimized(&self) -> bool {
        !self.cur.minimized.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(chat: &str) -> Pane {
        let mut p = Pane::default();
        p.switch_chat(Some(chat));
        p
    }

    #[test]
    fn tabs_open_switch_and_close() {
        let mut p = pane("c1");
        assert_eq!(p.shown(false), Shown::Hidden);
        p.show("a", false);
        p.show("b", false);
        p.activate("a");
        p.show("c", false);
        // A new tab goes right of the one in front.
        assert_eq!(p.tabs(), ["a", "c", "b"]);
        assert_eq!((p.active(), p.shown(false)), (Some("c"), Shown::Open));
        p.add_tab("a");
        assert_eq!((p.tabs().len(), p.active()), (3, Some("a")), "no duplicate tabs");
        // Closing the front tab brings its right neighbour forward, then the left one.
        p.close("a");
        assert_eq!((p.tabs(), p.active()), (&["c".to_owned(), "b".to_owned()][..], Some("c")));
        p.activate("b");
        p.close("b");
        assert_eq!(p.active(), Some("c"));
        p.activate("nope");
        assert_eq!(p.active(), Some("c"));
        p.close("c");
        assert_eq!((p.active(), p.shown(false)), (None, Shown::Hidden));
    }

    #[test]
    fn tabs_are_capped_dropping_the_oldest_in_back() {
        let mut p = pane("c");
        for i in 0..MAX_TABS {
            p.add_tab(&format!("t{i}"));
        }
        p.activate("t0");
        p.add_tab("new");
        assert_eq!(p.tabs().len(), MAX_TABS);
        assert!(p.tabs().iter().any(|t| t == "t0"), "the tab in front stays");
        assert!(!p.tabs().iter().any(|t| t == "t1"), "the oldest other one goes");
        assert_eq!(p.active(), Some("new"));
    }

    #[test]
    fn collapse_rail_and_toggle() {
        let mut p = pane("c");
        p.toggle(false, None);
        assert_eq!(p.shown(false), Shown::Hidden, "nothing to show");
        p.toggle(false, Some("latest"));
        assert_eq!((p.shown(false), p.active()), (Shown::Open, Some("latest")));
        p.toggle(false, Some("other"));
        assert_eq!(p.shown(false), Shown::Rail);
        assert!(p.collapsed());
        p.toggle(false, None);
        assert_eq!(p.shown(false), Shown::Open);
        // Phones: never open by itself; the floating button / "← Chat" flip a session flag.
        assert_eq!(p.shown(true), Shown::Rail);
        p.toggle(true, None);
        assert_eq!((p.shown(true), p.shown(false)), (Shown::Open, Shown::Open));
        p.collapse(true);
        assert_eq!((p.shown(true), p.shown(false)), (Shown::Rail, Shown::Open), "the desktop state is untouched");
    }

    #[test]
    fn maximize_and_restore() {
        let mut p = pane("c");
        p.set_maximized(true);
        assert!(!p.maximized(false), "nothing to maximize");
        p.show("a", false);
        let stored = p.prefs();
        p.set_maximized(true);
        assert!(p.maximized(false));
        assert!(!p.maximized(true), "phones: the open pane is full screen anyway");
        assert_eq!(p.prefs(), stored, "not stored");
        // Collapsing restores; maximizing from the rail opens the pane again.
        p.collapse(false);
        assert!(!p.maximized(false));
        p.set_maximized(true);
        assert_eq!((p.shown(false), p.maximized(false)), (Shown::Open, true));
        // Another tab keeps it; closing the last tab or switching chats ends it.
        p.show("b", false);
        p.close("a");
        assert!(p.maximized(false));
        p.close("b");
        assert!(!p.maximized(false));
        p.show("b", false);
        assert!(!p.maximized(false), "a new tab doesn't maximize by itself");
        p.set_maximized(true);
        p.switch_chat(Some("other"));
        p.switch_chat(Some("c"));
        assert_eq!((p.shown(false), p.maximized(false)), (Shown::Open, false));
        p.set_maximized(true);
        p.set_maximized(false);
        assert_eq!((p.shown(false), p.maximized(false)), (Shown::Open, false), "restore keeps the pane open");
    }

    #[test]
    fn reveal_respects_collapse_and_phones() {
        let mut p = pane("c");
        p.reveal("a", true);
        assert_eq!(p.shown(true), Shown::Rail, "a phone keeps the chat");
        let mut p = pane("c");
        p.reveal("a", false);
        assert_eq!(p.shown(false), Shown::Open, "an empty pane opens");
        p.collapse(false);
        p.set_minimized("b", true);
        p.reveal("b", false);
        assert_eq!((p.shown(false), p.active()), (Shown::Rail, Some("b")), "collapsed stays collapsed");
        assert!(!p.is_minimized("b"), "an update is shown in full");
    }

    #[test]
    fn minimize_and_collapse_all() {
        let mut p = pane("c");
        p.set_minimized("a", true);
        p.set_minimized("a", true);
        assert!(p.is_minimized("a") && !p.is_minimized("b"));
        p.set_all_minimized(["a", "b", "c"], true);
        assert_eq!(p.prefs().chats[0].minimized, ["a", "b", "c"]);
        p.show("b", false);
        assert!(!p.is_minimized("b"), "showing restores");
        p.set_all_minimized([], false);
        assert!(!p.any_minimized());
    }

    #[test]
    fn per_chat_state_and_retain() {
        let mut p = pane("c1");
        p.show("a", false);
        p.set_minimized("x", true);
        p.switch_chat(Some("c2"));
        assert_eq!((p.tabs().len(), p.shown(false)), (0, Shown::Hidden));
        p.show("b", false);
        p.switch_chat(Some("c1"));
        assert_eq!((p.tabs(), p.active()), (&["a".to_owned()][..], Some("a")));
        assert!(p.is_minimized("x"));
        // Artifacts an undo removed drop out.
        p.retain(|id| id != "a" && id != "x");
        assert!(p.tabs().is_empty() && !p.any_minimized());
        // A new chat (no id yet) keeps nothing.
        p.switch_chat(None);
        p.show("tmp", false);
        p.switch_chat(Some("c2"));
        assert!(!p.prefs().chats.iter().any(|c| c.chat.is_empty()));
        assert_eq!(p.tabs(), ["b"]);
    }

    #[test]
    fn prefs_round_trip_and_hostile_values() {
        let mut p = pane("c1");
        p.show("a", false);
        p.show("b", false);
        p.set_minimized("a", true);
        p.set_width(612.0);
        p.collapse(false);
        p.switch_chat(Some("c2"));
        p.show("z", false);
        let json = serde_json::to_string(&p.prefs()).unwrap();
        let back: PanePrefs = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p.prefs());
        let mut q = Pane::new(back);
        assert_eq!((q.width(), q.collapsed()), (Some(612.0), false), "showing z reopened it");
        q.switch_chat(Some("c1"));
        assert_eq!((q.tabs(), q.active(), q.is_minimized("a")), (&["a".to_owned(), "b".to_owned()][..], Some("b"), true));
        // Fields missing or garbage: defaults, clamped, capped, deduplicated.
        assert_eq!(serde_json::from_str::<PanePrefs>("{}").unwrap(), PanePrefs::default());
        let long = "x".repeat(MAX_ID + 1);
        let bad = PanePrefs {
            width: Some(f32::NAN),
            collapsed: true,
            chats: (0..MAX_CHATS + 5)
                .map(|i| ChatTabs {
                    chat: format!("c{i}"),
                    tabs: vec!["a".into(), "a".into(), long.clone(), String::new()],
                    active: Some("gone".into()),
                    ..ChatTabs::default()
                })
                .collect(),
        }
        .sanitized();
        assert_eq!(bad.width, None);
        assert_eq!(bad.chats.len(), MAX_CHATS);
        assert_eq!(bad.chats.last().map(|c| c.chat.as_str()), Some(format!("c{}", MAX_CHATS + 4).as_str()), "the most recent are kept");
        assert_eq!((bad.chats[0].tabs.clone(), bad.chats[0].active.clone()), (vec!["a".to_owned()], Some("a".to_owned())));
        assert_eq!(PanePrefs { width: Some(1.0), ..PanePrefs::default() }.sanitized().width, Some(MIN_WIDTH));
    }
}
