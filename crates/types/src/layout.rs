//! What byob's interface shows, and in what order: the **layout**.
//!
//! Every optional piece of the window's chrome has an id in [`ELEMENTS`] (`reply.tokens`,
//! `header.usage`, …) and a [`Show`] mode: always, only while the pointer is on it, or never.
//! A few regions ([`REGIONS`]) can be reordered: their elements, plus plugin views drawn
//! there (`view:<plugin>/<view>`), which are placed and hidden the same way as byob's own.
//!
//! The engine keeps one layout (settings.json `layout`) and sends it to every client, so a
//! change made in Settings or by the model (the `ui_layout` tool) shows everywhere at once.
//! Only what differs from the defaults is stored; elements a newer build adds get their
//! defaults, and ids this build doesn't know are kept (a newer build's, or a plugin's that
//! isn't loaded).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// How an element is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Show {
    /// Always there.
    Shown,
    /// Only while the pointer is on what it belongs to (the reply, the header, the row).
    Hover,
    /// Never.
    Hidden,
}

impl Show {
    pub const ALL: [Self; 3] = [Self::Shown, Self::Hover, Self::Hidden];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shown => "shown",
            Self::Hover => "hover",
            Self::Hidden => "hidden",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "shown" | "show" | "always" | "visible" | "on" => Some(Self::Shown),
            "hover" | "on_hover" | "on hover" | "hovered" => Some(Self::Hover),
            "hidden" | "hide" | "never" | "off" | "none" => Some(Self::Hidden),
            _ => None,
        }
    }

    /// Whether it is drawn now, `hovered` being whether the pointer is on its owner.
    pub fn visible(self, hovered: bool) -> bool {
        match self {
            Self::Shown => true,
            Self::Hover => hovered,
            Self::Hidden => false,
        }
    }
}

/// A region whose elements can be reordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub id: &'static str,
    /// Where it is, for people (and the model).
    pub describe: &'static str,
    /// Whether plugin views (`view:…`) are drawn here.
    pub plugin_views: bool,
}

/// An element of the interface that can be shown, hidden or moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Element {
    pub id: &'static str,
    /// Its name in Settings.
    pub label: &'static str,
    /// What it is, for people (and the model).
    pub describe: &'static str,
    /// The region it can be moved within, if any.
    pub region: Option<&'static str>,
    pub default: Show,
    /// Whether [`Show::Hover`] means something for it (else it acts as shown).
    pub hover: bool,
}

pub const HEADER: &str = "header";
pub const TOOLBAR: &str = "toolbar";
pub const REPLY: &str = "reply";

/// The regions that can be reordered, each listed left to right.
pub const REGIONS: &[Region] = &[
    Region { id: HEADER, describe: "the header after the chat's title, left to right", plugin_views: false },
    Region { id: TOOLBAR, describe: "the header's buttons on the right, left to right (the More menu always stays last)", plugin_views: true },
    Region { id: REPLY, describe: "the details under a reply, left to right", plugin_views: false },
];

const fn el(id: &'static str, label: &'static str, describe: &'static str, region: Option<&'static str>, default: Show, hover: bool) -> Element {
    Element { id, label, describe, region, default, hover }
}

/// Everything the layout knows, in the order Settings lists them (and each region's default order).
pub const ELEMENTS: &[Element] = &[
    // Header, after the title.
    el("header.model", "Model", "the model chip: which model this chat uses; click it to switch", Some(HEADER), Show::Shown, true),
    el("header.usage", "Context meter", "the bar of how full the chat's context is; click it for details", Some(HEADER), Show::Shown, true),
    el("header.server", "Server address", "the server's address inside the model chip, on wide windows", None, Show::Shown, false),
    el("header.usage_numbers", "Context numbers", "tokens used out of the context window, next to the context meter", None, Show::Shown, true),
    el("header.wordmark", "Wordmark", "the \"byob\" name on the left of the header, on wide windows", None, Show::Shown, false),
    el("header.workspace", "Workspace name", "the workspace's name before the chat's title", None, Show::Shown, true),
    // Header, right.
    el("toolbar.panes", "Plugin panes", "buttons that open plugins' panes beside the chat", Some(TOOLBAR), Show::Shown, true),
    el("toolbar.artifacts", "Artifacts button", "the button that shows or hides the artifact pane", Some(TOOLBAR), Show::Shown, true),
    el("toolbar.plugins", "Plugins chip", "how many plugins are on, with a badge when one has trouble", Some(TOOLBAR), Show::Shown, true),
    // Messages.
    el("message.role", "Who said it", "\"You\" and \"byob\" above each message", None, Show::Shown, false),
    el(
        "message.thinking",
        "Thinking",
        "the \"Thought\" row that opens the model's thinking; hidden, it shows only while the model is still thinking",
        None,
        Show::Shown,
        false,
    ),
    el("message.thought_size", "Thinking length", "how long the model's thinking was, next to \"Thought\" (\"1.1k chars\")", None, Show::Shown, false),
    el(
        "message.steps",
        "Tool steps",
        "a row for each tool the model ran (\"Read notes.md\", \"Made Rain haiku\"); hidden, they show only while running (failed ones are hidden too; questions and approvals always show)",
        None,
        Show::Shown,
        false,
    ),
    el("message.step_files", "Files changed", "\"1 file\" (\"3 files\") on a tool's row when it changed files", None, Show::Shown, false),
    el("message.subagent_details", "Subagent details", "the model and tokens on a subagent's row", None, Show::Shown, false),
    el("message.edit", "Edit button", "\"Edit\" on your messages: change one and start the chat over from there", None, Show::Hover, true),
    el("message.undo", "Undo button", "\"Undo changes\" under a reply that changed files", None, Show::Shown, true),
    el(
        "reply.tokens",
        "Tokens",
        "tokens under each reply: how full the context was at its end, what the model wrote, and what several requests read together",
        Some(REPLY),
        Show::Shown,
        true,
    ),
    el("reply.time", "Time", "how long the reply took, under each reply", Some(REPLY), Show::Shown, true),
    el("reply.speed", "Speed", "tokens per second the model wrote, under each reply", Some(REPLY), Show::Hidden, true),
    // Artifacts and code in replies.
    el("artifact.kind", "Artifact kind", "the kind badge on an artifact's card in a reply (\"DOCUMENT\", \"APP\", \"CHART\")", None, Show::Shown, false),
    el("artifact.version", "Artifact version", "the version on an artifact's card in a reply (\"v1\") and its note", None, Show::Shown, false),
    el(
        "artifact.shown",
        "Artifact shown note",
        "\"Shown beside the chat\" on an artifact's card in a reply while it's open in the artifact pane",
        None,
        Show::Shown,
        true,
    ),
    el("code.language", "Code language", "the language (\"rust\", \"code\") at the top left of a code block", None, Show::Shown, true),
    el("code.copy", "Code copy button", "\"Copy\" at the top right of a code block", None, Show::Shown, true),
    // Elsewhere.
    el("chat.starters", "Suggestions", "the suggested prompts in an empty chat (\"Ask anything to get started, or try:\")", None, Show::Shown, false),
    el(
        "composer.attach",
        "Attach button",
        "the paperclip that attaches files, left of the message box (dropping or pasting files still works)",
        None,
        Show::Shown,
        false,
    ),
    el("composer.keys", "Composer keys", "\"Enter to send, Shift+Enter for a new line\" in the empty message box", None, Show::Shown, false),
    el("sidebar.shortcuts", "Sidebar shortcuts", "the keyboard shortcuts on New chat and the search field", None, Show::Shown, true),
];

/// Where elements that aren't in a region are, by id prefix, in the order Settings lists them.
pub const GROUPS: &[(&str, &str)] = &[
    ("header", "Header"),
    ("message", "Messages"),
    ("artifact", "Artifacts in replies"),
    ("code", "Code blocks"),
    ("chat", "Empty chat"),
    ("composer", "Message box"),
    ("sidebar", "Sidebar"),
];

/// The heading of the group element `id` is in ([`GROUPS`]).
pub fn group(id: &str) -> &'static str {
    let prefix = id.split('.').next().unwrap_or_default();
    GROUPS.iter().find(|(p, _)| *p == prefix).map_or("Other", |(_, name)| name)
}

/// The element with id `id`.
pub fn element(id: &str) -> Option<&'static Element> {
    ELEMENTS.iter().find(|e| e.id == id)
}

/// The region with id `id`.
pub fn region(id: &str) -> Option<&'static Region> {
    REGIONS.iter().find(|r| r.id == id)
}

/// The id of plugin `plugin`'s view `view` in a layout.
pub fn view_id(plugin: &str, view: &str) -> String {
    format!("view:{plugin}/{view}")
}

/// Ready-made layouts.
pub const PRESETS: &[(&str, &str)] = &[
    ("everything", "every detail shown (byob's default)"),
    ("balanced", "details of replies and the context numbers on hover; labels and shortcut hints hidden"),
    ("minimal", "only what you need: details on hover, the rest hidden"),
];

/// Most entries kept per map (ids are few; the rest is a hostile or broken file).
const MAX_ENTRIES: usize = 256;
const MAX_ID: usize = 160;

/// What the interface shows: the modes and orders that differ from the defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Layout {
    /// Element id → how it is shown.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub show: BTreeMap<String, Show>,
    /// Region id → element ids in the order wanted. Ids a region doesn't have are skipped;
    /// those it has but the list doesn't name follow, in their default order.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub order: BTreeMap<String, Vec<String>>,
}

impl Layout {
    pub fn is_default(&self) -> bool {
        self.show.is_empty() && self.order.is_empty()
    }

    /// How element (or plugin view) `id` is shown. Unknown ids are shown.
    pub fn show(&self, id: &str) -> Show {
        let mode = self.show.get(id).copied().or_else(|| element(id).map(|e| e.default)).unwrap_or(Show::Shown);
        match element(id) {
            Some(e) if !e.hover && mode == Show::Hover => Show::Shown,
            _ => mode,
        }
    }

    /// Whether `id` is drawn now (`hovered`: the pointer is on what it belongs to).
    pub fn visible(&self, id: &str, hovered: bool) -> bool {
        self.show(id).visible(hovered)
    }

    /// Set how `id` is shown; back to the default stores nothing.
    pub fn set_show(&mut self, id: &str, mode: Show) {
        if element(id).is_some_and(|e| e.default == mode) {
            self.show.remove(id);
        } else {
            self.show.insert(id.to_owned(), mode);
        }
    }

    /// Region `region`'s elements in order: byob's own (in [`ELEMENTS`] order) and then
    /// `extra` (plugin views drawn there, as found), arranged by the stored order. Hidden
    /// ones are included; ask [`Self::show`].
    pub fn arrange(&self, region: &str, extra: &[String]) -> Vec<String> {
        let mut present: Vec<String> = ELEMENTS.iter().filter(|e| e.region == Some(region)).map(|e| e.id.to_owned()).collect();
        for id in extra {
            if !present.contains(id) {
                present.push(id.clone());
            }
        }
        let Some(wanted) = self.order.get(region) else { return present };
        let mut out: Vec<String> = Vec::with_capacity(present.len());
        for id in wanted {
            if present.contains(id) && !out.contains(id) {
                out.push(id.clone());
            }
        }
        for id in present {
            if !out.contains(&id) {
                out.push(id);
            }
        }
        out
    }

    /// Put region `region` in this order (see [`Self::order`]); the default order stores nothing.
    pub fn set_order(&mut self, region: &str, ids: Vec<String>) {
        let default: Vec<&str> = ELEMENTS.iter().filter(|e| e.region == Some(region)).map(|e| e.id).collect();
        let mut ids: Vec<String> = ids.into_iter().filter(|id| !id.is_empty()).collect();
        ids.dedup();
        if ids.is_empty() || ids.iter().map(String::as_str).eq(default.iter().copied()) {
            self.order.remove(region);
        } else {
            self.order.insert(region.to_owned(), ids);
        }
    }

    /// Move `id` one place earlier (`dir` < 0) or later within region `region`, among `extra`.
    pub fn nudge(&mut self, region: &str, extra: &[String], id: &str, dir: i32) {
        let mut ids = self.arrange(region, extra);
        let Some(i) = ids.iter().position(|x| x == id) else { return };
        let j = if dir < 0 { i.checked_sub(1) } else { Some(i + 1).filter(|j| *j < ids.len()) };
        if let Some(j) = j {
            ids.swap(i, j);
            self.set_order(region, ids);
        }
    }

    /// A ready-made layout by name ([`PRESETS`]).
    pub fn preset(name: &str) -> Option<Self> {
        let mut l = Self::default();
        let hover = |l: &mut Self, ids: &[&str]| ids.iter().for_each(|id| l.set_show(id, Show::Hover));
        let hide = |l: &mut Self, ids: &[&str]| ids.iter().for_each(|id| l.set_show(id, Show::Hidden));
        match name.trim().to_ascii_lowercase().as_str() {
            "everything" | "default" | "full" => {}
            "balanced" => {
                hover(&mut l, &["reply.tokens", "reply.time", "header.usage_numbers", "sidebar.shortcuts"]);
                hide(&mut l, &["message.role", "message.thought_size", "composer.keys"]);
            }
            "minimal" => {
                hover(&mut l, &["reply.tokens", "reply.time", "sidebar.shortcuts", "toolbar.plugins", "artifact.shown", "message.undo", "code.copy"]);
                hide(
                    &mut l,
                    &[
                        "message.role",
                        "message.thought_size",
                        "message.step_files",
                        "message.subagent_details",
                        "artifact.version",
                        "code.language",
                        "composer.keys",
                        "header.usage_numbers",
                        "header.server",
                        "header.wordmark",
                    ],
                );
            }
            _ => return None,
        }
        Some(l)
    }

    /// Drop what a broken or hostile file could hold: too many entries, absurd ids, empty orders.
    #[must_use]
    pub fn sanitized(mut self) -> Self {
        let ok = |id: &String| !id.is_empty() && id.len() <= MAX_ID && !id.chars().any(char::is_control);
        self.show.retain(|id, _| ok(id));
        while self.show.len() > MAX_ENTRIES {
            self.show.pop_last();
        }
        self.order.retain(|region, ids| {
            ids.retain(&ok);
            ids.truncate(MAX_ENTRIES);
            ok(region) && !ids.is_empty()
        });
        while self.order.len() > MAX_ENTRIES {
            self.order.pop_last();
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_show_everything_but_speed_and_store_nothing() {
        let l = Layout::default();
        for e in ELEMENTS {
            assert_eq!(l.show(e.id), e.default, "{}", e.id);
        }
        assert_eq!(l.show("reply.speed"), Show::Hidden);
        assert_eq!(l.show("view:todo/count"), Show::Shown);
        let mut l = Layout::default();
        l.set_show("reply.tokens", Show::Hover);
        l.set_show("reply.tokens", Show::Shown);
        assert!(l.is_default(), "back to the default stores nothing");
        assert_eq!(serde_json::to_string(&l).unwrap(), "{}");
    }

    #[test]
    fn hover_falls_back_to_shown_where_it_means_nothing() {
        let mut l = Layout::default();
        l.set_show("message.role", Show::Hover);
        assert_eq!(l.show("message.role"), Show::Shown);
        l.set_show("reply.time", Show::Hover);
        assert!(!l.visible("reply.time", false));
        assert!(l.visible("reply.time", true));
    }

    #[test]
    fn arranging_puts_named_ids_first_and_keeps_the_rest_in_their_order() {
        let views = vec!["view:todo/count".to_owned()];
        let mut l = Layout::default();
        assert_eq!(l.arrange(TOOLBAR, &views), ["toolbar.panes", "toolbar.artifacts", "toolbar.plugins", "view:todo/count"]);
        l.set_order(TOOLBAR, vec!["view:todo/count".into(), "nope".into(), "toolbar.plugins".into()]);
        assert_eq!(l.arrange(TOOLBAR, &views), ["view:todo/count", "toolbar.plugins", "toolbar.panes", "toolbar.artifacts"]);
        // A plugin view that isn't loaded is skipped (and kept for when it is).
        assert_eq!(l.arrange(TOOLBAR, &[]), ["toolbar.plugins", "toolbar.panes", "toolbar.artifacts"]);
        assert!(l.order[TOOLBAR].contains(&"view:todo/count".to_owned()));
    }

    #[test]
    fn nudging_moves_one_place_and_the_default_order_stores_nothing() {
        let mut l = Layout::default();
        l.nudge(REPLY, &[], "reply.time", -1);
        assert_eq!(l.arrange(REPLY, &[]), ["reply.time", "reply.tokens", "reply.speed"]);
        l.nudge(REPLY, &[], "reply.time", -1);
        assert_eq!(l.arrange(REPLY, &[]), ["reply.time", "reply.tokens", "reply.speed"], "already first");
        l.nudge(REPLY, &[], "reply.time", 1);
        assert!(l.is_default());
    }

    #[test]
    fn presets_are_named_and_known() {
        for (name, _) in PRESETS {
            assert!(Layout::preset(name).is_some(), "{name}");
        }
        assert!(Layout::preset("everything").unwrap().is_default());
        assert!(Layout::preset("sparkly").is_none());
        let min = Layout::preset("minimal").unwrap();
        assert!(!min.visible("reply.tokens", false) && min.visible("reply.tokens", true));
        for id in min.show.keys() {
            assert!(element(id).is_some(), "{id}");
        }
    }

    #[test]
    fn modes_parse_loosely_and_sanitizing_drops_junk() {
        assert_eq!(Show::parse(" Hide "), Some(Show::Hidden));
        assert_eq!(Show::parse("on hover"), Some(Show::Hover));
        assert_eq!(Show::parse("maybe"), None);
        let l: Layout = serde_json::from_str(r#"{"show": {"": "hidden", "a\u0007": "hidden", "reply.time": "hover"}, "order": {"reply": []}}"#).unwrap();
        let l = l.sanitized();
        assert_eq!(l.show.len(), 1);
        assert!(l.order.is_empty());
    }

    #[test]
    fn every_element_is_unique_and_its_region_exists() {
        for (i, e) in ELEMENTS.iter().enumerate() {
            assert!(ELEMENTS[..i].iter().all(|o| o.id != e.id), "{}", e.id);
            assert!(e.region.is_none_or(|r| region(r).is_some()), "{}", e.id);
        }
    }
}
