//! Render sites: where plugins' live views show in a client (13.3+, cap
//! [`CAP_VIEW_SITES`]).
//!
//! A plugin declares its views in its manifest (`views: [{site, id, title, icon}]`), and
//! clients learn them from [`crate::PluginInfo::views`]. A site is a place in the client's UI:
//! the side **pane** (a tab beside the chat), a **settings** section (under the plugin in
//! Settings → Plugins), a header **status** item (a chip or a short text) and step **card**s
//! (a backend tool attaches one to its step).
//!
//! The engine owns each view's tree; clients keep only transient state (typed text, scroll)
//! — the ipywidgets rule. A client that starts showing a view sends
//! [`crate::ClientMsg::OpenView`]; the engine answers with a [`LiveView`] carrying the whole
//! tree (from its cache, or after asking the plugin's backend), then sends patches whenever
//! the plugin changes it, to every client that has it open. What the user does in it is a
//! [`crate::ClientMsg::ViewEvent`], which the engine hands to the backend; the backend
//! answers with a new view (or patches), which the engine diffs against the last one.
//! A [`ViewRef`] names a view: the plugin, the declared view id, and a key (`""` for the
//! single instance at pane, settings and status sites; one per card).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::tree::{Event, Node, Patch};

/// Cap: the peer handles live views at render sites ([`crate::ClientMsg::OpenView`],
/// [`crate::ServerMsg::LiveView`], [`crate::View::owner`]).
pub const CAP_VIEW_SITES: &str = "view_sites";

/// Most views one plugin declares.
pub const MAX_VIEWS: usize = 16;
/// Longest view id.
pub const MAX_VIEW_ID_CHARS: usize = 48;
/// Longest view key.
pub const MAX_VIEW_KEY_CHARS: usize = 128;
/// Largest event (its JSON).
pub const MAX_EVENT_BYTES: usize = 256 * 1024;
/// Longest view title.
pub const MAX_TITLE_CHARS: usize = 80;

/// Where a view shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Site {
    /// A tab of the side pane beside the chat.
    Pane,
    /// A section under the plugin in Settings → Plugins.
    Settings,
    /// A small item in the header (a chip, a few words).
    Status,
    /// A card on a step (a backend tool attaches it).
    Card,
    /// An artifact's body (13.5): the view a renderer `{"renderer": "view:<id>"}` names,
    /// drawn in the artifact's card and pane, one instance per version (key
    /// `"<artifact id>@<version>"`, see [`artifact_key`]).
    Artifact,
    /// A site a newer engine knows (not shown).
    #[serde(other)]
    Other,
}

impl Site {
    pub const ALL: [Site; 5] = [Site::Pane, Site::Settings, Site::Status, Site::Card, Site::Artifact];

    pub fn label(self) -> &'static str {
        match self {
            Self::Pane => "pane",
            Self::Settings => "settings",
            Self::Status => "status",
            Self::Card => "card",
            Self::Artifact => "artifact",
            Self::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|x| x.label() == s)
    }
}

/// A view a plugin declares (`views` in its manifest).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewDecl {
    /// `[a-z0-9_-]{1,48}`, unique in the plugin.
    pub id: String,
    pub site: Site,
    #[serde(default)]
    pub title: String,
    /// A lucide icon name (may be empty).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub icon: String,
}

/// Names one live view: the plugin, its declared view id and an instance key.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ViewRef {
    pub plugin: String,
    /// The declared view's id.
    pub view: String,
    /// `""`: the only instance (pane, settings, status); a card's own key.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,
}

impl ViewRef {
    pub fn new(plugin: impl Into<String>, view: impl Into<String>, key: impl Into<String>) -> Self {
        Self { plugin: plugin.into(), view: view.into(), key: key.into() }
    }

    /// Check an untrusted reference's ids and sizes.
    pub fn check(&self) -> Result<(), String> {
        if !byob_types::config::plugin_id_ok(&self.plugin) {
            return Err(format!("{:?} isn't a plugin id", self.plugin.chars().take(60).collect::<String>()));
        }
        if !view_id_ok(&self.view) {
            return Err(format!("{:?} isn't a view id", self.view.chars().take(60).collect::<String>()));
        }
        if self.key.chars().count() > MAX_VIEW_KEY_CHARS {
            return Err(format!("a view key can be at most {MAX_VIEW_KEY_CHARS} characters"));
        }
        Ok(())
    }
}

/// The key of an artifact-site view showing version `version` of artifact `id`.
pub fn artifact_key(id: &str, version: u32) -> String {
    format!("{id}@{version}")
}

/// An artifact-site view key back to (artifact id, version).
pub fn parse_artifact_key(key: &str) -> Option<(&str, u32)> {
    let (id, v) = key.rsplit_once('@')?;
    let v: u32 = v.parse().ok()?;
    (crate::artifact::valid_id(id) && v > 0).then_some((id, v))
}

/// A view id: `[a-z0-9_-]{1,48}`.
pub fn view_id_ok(id: &str) -> bool {
    (1..=MAX_VIEW_ID_CHARS).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}

/// A live view's tree or a change to it ([`crate::ServerMsg::LiveView`]). Exactly one of:
/// `tree` (replace what the client has), `patch` (apply to it, in order), `error` (the
/// view couldn't be had: show it at the site), `reset` (forget it; open it again to get
/// the new one, e.g. after the plugin reloaded). `values` force field values over what the
/// user typed (only to the client whose event caused the reply). `faults` are what the
/// engine had to fix in the plugin's tree (also in the plugin's status for the model).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LiveView {
    pub view: ViewRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<Node>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub patch: Vec<Patch>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub values: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub faults: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub reset: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Size checks of the view messages (see [`crate::ClientMsg::check_sizes`]).
pub(crate) fn check_sizes(msg: &crate::ClientMsg) -> Result<(), String> {
    use crate::ClientMsg;
    match msg {
        ClientMsg::OpenView { view } | ClientMsg::CloseView { view } => view.check(),
        ClientMsg::ViewEvent { view, event } => {
            view.check()?;
            check_event(event)
        }
        _ => Ok(()),
    }
}

fn check_event(e: &Event) -> Result<(), String> {
    if e.action.chars().count() > crate::tree::MAX_TEXT_CHARS {
        return Err("the event's action is too long".into());
    }
    if e.values.len() > crate::tree::MAX_VALUES {
        return Err(format!("an event carries at most {} values", crate::tree::MAX_VALUES));
    }
    let size = serde_json::to_string(e).map_or(usize::MAX, |s| s.len());
    if size > MAX_EVENT_BYTES {
        return Err(format!("event too large ({size} bytes, at most {MAX_EVENT_BYTES})"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Kind, Target};
    use crate::{ClientMsg, ServerMsg};

    #[test]
    fn view_messages_round_trip_and_are_checked() {
        let r = ViewRef::new("todo", "list", "");
        let event = Event { action: "add".into(), values: serde_json::json!({"new": "milk"}).as_object().cloned().unwrap_or_default(), edit: None, grid: None };
        for m in [ClientMsg::OpenView { view: r.clone() }, ClientMsg::CloseView { view: r.clone() }, ClientMsg::ViewEvent { view: r.clone(), event }] {
            assert_eq!(ClientMsg::from_json(&m.to_json()).unwrap(), m, "{}", m.to_json());
            assert!(m.check_sizes().is_ok());
        }
        assert_eq!(ClientMsg::OpenView { view: r.clone() }.to_json(), r#"{"type":"open_view","view":{"plugin":"todo","view":"list"}}"#);
        for bad in [ViewRef::new("Todo", "list", ""), ViewRef::new("todo", "a b", ""), ViewRef::new("todo", "list", "k".repeat(MAX_VIEW_KEY_CHARS + 1))] {
            assert!(ClientMsg::OpenView { view: bad }.check_sizes().is_err());
        }
        let big =
            Event { action: "x".into(), values: [("t".to_owned(), Value::String("x".repeat(MAX_EVENT_BYTES)))].into_iter().collect(), edit: None, grid: None };
        assert!(ClientMsg::ViewEvent { view: r.clone(), event: big }.check_sizes().is_err());

        let tree = Node::new(Kind::Text { text: "3 left".into(), style: Default::default(), tone: Default::default() });
        let msgs = [
            ServerMsg::LiveView(LiveView { view: r.clone(), tree: Some(tree), faults: vec!["/x: y".into()], ..Default::default() }),
            ServerMsg::LiveView(LiveView {
                view: r.clone(),
                patch: vec![Patch::Remove { at: Target::key("a") }],
                values: serde_json::json!({"new": ""}).as_object().cloned().unwrap_or_default(),
                ..Default::default()
            }),
            ServerMsg::LiveView(LiveView { view: r.clone(), error: Some("no backend".into()), ..Default::default() }),
            ServerMsg::LiveView(LiveView { view: r, reset: true, ..Default::default() }),
        ];
        for m in msgs {
            assert_eq!(ServerMsg::from_json(&m.to_json()).unwrap(), m, "{}", m.to_json());
        }
    }

    #[test]
    fn sites_and_ids() {
        for s in Site::ALL {
            assert_eq!(Site::parse(s.label()), Some(s));
        }
        assert_eq!(serde_json::from_str::<Site>(r#""sidebar""#).unwrap(), Site::Other);
        assert!(view_id_ok("todo_list-2") && !view_id_ok("") && !view_id_ok("Todo") && !view_id_ok(&"x".repeat(49)));
        // A 13.2 plugin list (no views) still parses, and views travel when there are some.
        let d: ViewDecl = serde_json::from_str(r#"{"id":"list","site":"pane"}"#).unwrap();
        assert_eq!((d.title.as_str(), d.icon.as_str()), ("", ""));
    }

    /// 13.3 additions are optional on the wire both ways: a 13.2 peer's messages parse here,
    /// and what 13.3 adds is left out when empty (so 13.2 peers see the same JSON as before).
    #[test]
    fn minor_3_is_compatible_with_minor_2() {
        let card = crate::PluginCard {
            plugin: "p".into(),
            scope: crate::PluginScope::Chat,
            action: crate::PluginAction::Made,
            tools: vec![],
            added: vec![],
            removed: vec![],
            test: None,
            problems: vec![],
            missing_settings: vec![],
        };
        let v = crate::View::of(&card);
        assert!(!serde_json::to_string(&v).unwrap().contains("owner"));
        let old: crate::View = serde_json::from_str(r#"{"kind":"byob.tree","v":1,"data":{"type":"text","text":"x"},"fallback":"x"}"#).unwrap();
        assert_eq!(old.owner, None);
        let mut owned = old.clone();
        owned.owner = Some(ViewRef::new("todo", "item", "3"));
        let back: crate::View = serde_json::from_str(&serde_json::to_string(&owned).unwrap()).unwrap();
        assert_eq!(back, owned);
        // a 13.2 client doesn't know LiveView: it skips it
        let lv = ServerMsg::LiveView(LiveView { view: ViewRef::new("todo", "list", ""), reset: true, ..Default::default() }).to_json();
        assert!(lv.starts_with(r#"{"type":"live_view""#), "{lv}");
    }
}
