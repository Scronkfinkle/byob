//! Version negotiation and capabilities.
//!
//! A client opens with [`crate::ClientMsg::Hello`] carrying [`PROTOCOL_MAJOR`],
//! [`PROTOCOL_MINOR`] and its [`client_caps`]; the engine answers with
//! [`crate::ServerMsg::Welcome`] (its own version and caps) before anything else.
//!
//! Compatibility rules:
//! - **Major** must match. It is bumped only for changes an older peer can't survive
//!   (a renamed or removed field, a changed meaning). A client with another major gets one
//!   error notice and nothing else.
//! - **Minor** is informational: everything a minor adds is optional on the wire (serde
//!   defaults, skipped when empty) or behind a cap.
//! - **Caps** are feature strings. A feature that needs both sides to understand it is used
//!   only when both list its cap ([`shared`]). Unknown caps are ignored.
//! - Enums that grow decode unknown values into a fallback (`#[serde(other)]`), and a
//!   client skips a [`crate::ServerMsg`] it doesn't know ([`crate::ServerMsg::Unknown`]),
//!   so additions don't need a major bump.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Bumped on a change an older peer can't survive (see the module docs).
pub const PROTOCOL_MAJOR: u32 = 13;
/// Bumped on additive changes; peers with the same major but another minor still talk.
pub const PROTOCOL_MINOR: u32 = 23;
// Minors: 1 — `Artifact::content_type` (open artifact types; `kind` stays for 13.0 clients);
// 2 — `PluginInfo::backend` (plugin backend processes); 3 — render sites: `PluginInfo::views`,
// `View::owner`, `OpenView`/`CloseView`/`ViewEvent`, `LiveView` (cap `view_sites`); 4 — bundled
// plugins: `PluginScope::Bundled` (cap `bundled_scope`), `PluginInfo::{locked, renderers, starters}`;
// 5 — web surfaces: `WebCall`/`WebReply`/`WebPush`, `ViewDecl::html`, step views `byob.mcp_app`
// and `byob.web` (cap `web_bridge`).
// 6 — `Site::Artifact` and `view:<id>` renderers (an artifact's body is a plugin's live view),
// the `grid` tree node and `Event::grid`.
// 7 — `PluginInfo::superseded_by` (manifest `provides`: one of two plugins offering the same
// thing is used) and native plugin backends (no wire change).
// 8 — question cards: `Ask::choice` and `ClientMsg::AnswerQuestion` (the `ask_user` tool).
// 9 — model profiles: `GetProfiles`, `CreateProfile`, `UpdateProfile`, `DeleteProfile`,
// `DuplicateProfile`, `SetRole`, `TestProfile`, `SetLevelProfile`, `ServerMsg::Profiles`/
// `ProfileTested` (cap `profiles`), `Layer::profile`, `ChatEndpoint::profile`,
// `EndpointInfo::slots`.
// 10 — subagent models: `Layer::subagent_profile`, `Effective::subagent_profile`,
// `Delegation::profile`.
// 11 — the layout: `ClientMsg::SetLayout`, `ServerMsg::Layout` (cap `layout`).
// 12 — looks: `ClientMsg::ReportLook`, `ServerMsg::SetLook` (cap `look`).
// 13 — going back: `ClientMsg::Rewind` (cap `rewind`).
// 14 — HTML apps on the internet: `Artifact::network`, `ClientMsg::SetArtifactNetwork` (cap
// `artifact_network`); `LookReport::notes`.
// 15 — screenshots: `ServerMsg::Capture`, `ClientMsg::Captured` (cap `capture`).
// 16 — chats without a project: `ClientMsg::NewUnattachedChat`, `ClientMsg::ChatToProject`
// (cap `unattached`).
// 17 — archived chats: `ClientMsg::ArchiveChat`, `ChatSummary::archived_at` (cap `archive`);
// the Library: `ClientMsg::ListLibrary`, `ServerMsg::Library` (cap `library`); plugin logos:
// `PluginInfo::logo`, `ClientMsg::GetPluginLogo` (cap `plugin_logos`).
// 18 — projects beyond one folder: `WorkspaceSummary::{folders, goal}`,
// `CreateWorkspace::folders`, `ClientMsg::SetWorkspaceFolders` (cap `project_folders`).
// 19 — routines: `ClientMsg::{SaveRoutine, DeleteRoutine, RunRoutine, SetRoutineEnabled,
// ListRoutineRuns}`, `ServerMsg::{Routines, RoutineRuns, RoutineEvent}` (cap `routines`),
// `ChatSummary::origin`, `EngineState::routines_active`.
// 20 — keep running in the background: `EngineState::keep_running`, `ClientMsg::SetKeepRunning`
// (sent only to an engine whose state has it).
// 21 — pausing routines: `EngineState::routines_paused`, `ClientMsg::PauseRoutines` (sent only
// to an engine whose state has it).
// 22 — a tool's own card words: `Ask::card` (`CardWords`: the preview is complete, a plain
// access line, button labels).
// 23 — My phone: `ClientMsg::{PairPhone, CancelPairing, UnpairPhone, TestPhone}`,
// `ServerMsg::Phone` (cap `phone`).

/// Cap: the engine takes [`crate::ClientMsg::Rewind`] (and the client offers it).
pub const CAP_REWIND: &str = "rewind";

/// Cap: the engine takes [`crate::ClientMsg::SetArtifactNetwork`] (and the client offers it).
pub const CAP_ARTIFACT_NETWORK: &str = "artifact_network";

/// Cap: the engine takes [`crate::ClientMsg::NewUnattachedChat`] and
/// [`crate::ClientMsg::ChatToProject`] (and the client offers them).
pub const CAP_UNATTACHED: &str = "unattached";

/// Cap (17+): the engine takes [`crate::ClientMsg::ArchiveChat`] and fills
/// [`crate::ChatSummary::archived_at`].
pub const CAP_ARCHIVE: &str = "archive";

/// Cap (17+): the engine answers [`crate::ClientMsg::ListLibrary`] with every chat's artifacts.
pub const CAP_LIBRARY: &str = "library";

/// Cap (17+): plugins say when they have a logo, and the engine serves it
/// ([`crate::ClientMsg::GetPluginLogo`]).
pub const CAP_PLUGIN_LOGOS: &str = "plugin_logos";

/// Cap: the peer handles [`crate::ClientMsg::Plugin`] / [`crate::ServerMsg::Plugin`].
pub const CAP_PLUGIN_CHANNEL: &str = "plugin_channel";
/// Cap: the peer understands [`crate::View`]s on steps (renders known kinds, shows the
/// fallback of the others). Part of major 13, listed so later view sites can be negotiated
/// the same way.
pub const CAP_VIEWS: &str = "views";

/// Cap: the engine keeps projects' other folders ([`crate::ClientMsg::SetWorkspaceFolders`],
/// `CreateWorkspace::folders`), and the client offers them.
pub const CAP_PROJECT_FOLDERS: &str = "project_folders";

/// Cap: the client decodes [`crate::PluginScope::Bundled`]. Clients without it get bundled
/// plugins as `global` (their origin already says `bundled`).
pub const CAP_BUNDLED_SCOPE: &str = "bundled_scope";

/// Most caps one side may list.
pub const MAX_CAPS: usize = 64;
/// Longest cap.
pub const MAX_CAP_CHARS: usize = 64;

/// The caps this build's clients announce.
pub fn client_caps() -> Vec<String> {
    vec![
        CAP_VIEWS.to_owned(),
        CAP_PLUGIN_CHANNEL.to_owned(),
        crate::sites::CAP_VIEW_SITES.to_owned(),
        CAP_BUNDLED_SCOPE.to_owned(),
        crate::web::CAP_WEB_BRIDGE.to_owned(),
        crate::profiles::CAP_PROFILES.to_owned(),
        crate::layout::CAP_LAYOUT.to_owned(),
        crate::look::CAP_LOOK.to_owned(),
        crate::look::CAP_CAPTURE.to_owned(),
        CAP_REWIND.to_owned(),
        CAP_ARTIFACT_NETWORK.to_owned(),
        CAP_UNATTACHED.to_owned(),
        CAP_ARCHIVE.to_owned(),
        CAP_PROJECT_FOLDERS.to_owned(),
        CAP_LIBRARY.to_owned(),
        CAP_PLUGIN_LOGOS.to_owned(),
        crate::routines::CAP_ROUTINES.to_owned(),
        crate::phone::CAP_PHONE.to_owned(),
    ]
}

/// The caps this build's engine announces.
pub fn engine_caps() -> Vec<String> {
    vec![
        CAP_VIEWS.to_owned(),
        CAP_PLUGIN_CHANNEL.to_owned(),
        crate::sites::CAP_VIEW_SITES.to_owned(),
        CAP_BUNDLED_SCOPE.to_owned(),
        crate::web::CAP_WEB_BRIDGE.to_owned(),
        crate::profiles::CAP_PROFILES.to_owned(),
        crate::layout::CAP_LAYOUT.to_owned(),
        crate::look::CAP_LOOK.to_owned(),
        crate::look::CAP_CAPTURE.to_owned(),
        CAP_REWIND.to_owned(),
        CAP_ARTIFACT_NETWORK.to_owned(),
        CAP_UNATTACHED.to_owned(),
        CAP_ARCHIVE.to_owned(),
        CAP_PROJECT_FOLDERS.to_owned(),
        CAP_LIBRARY.to_owned(),
        CAP_PLUGIN_LOGOS.to_owned(),
        crate::routines::CAP_ROUTINES.to_owned(),
        crate::phone::CAP_PHONE.to_owned(),
    ]
}

/// Both sides list `cap`.
pub fn shared(ours: &[String], theirs: &[String], cap: &str) -> bool {
    ours.iter().any(|c| c == cap) && theirs.iter().any(|c| c == cap)
}

/// A cap is ASCII lowercase letters, digits, `_`, `.`, `-` (e.g. `plugin_channel`,
/// `view.byob.plugin_card`).
pub fn cap_ok(cap: &str) -> bool {
    (1..=MAX_CAP_CHARS).contains(&cap.len()) && cap.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-'))
}

/// Check a peer's caps list.
pub fn check_caps(caps: &[String]) -> Result<(), String> {
    if caps.len() > MAX_CAPS {
        return Err(format!("too many caps (at most {MAX_CAPS})"));
    }
    match caps.iter().find(|c| !cap_ok(c)) {
        Some(bad) => Err(format!("bad cap {:?}", bad.chars().take(MAX_CAP_CHARS).collect::<String>())),
        None => Ok(()),
    }
}

/// Largest plugin channel message (its JSON).
pub const MAX_PLUGIN_MSG_BYTES: usize = 256 * 1024;

/// A plugin channel message ([`crate::ClientMsg::Plugin`] / [`crate::ServerMsg::Plugin`]):
/// plugin ids are plugin ids (`[a-z0-9-]`), the message any JSON up to
/// [`MAX_PLUGIN_MSG_BYTES`].
pub fn check_plugin_msg(plugin: &str, msg: &Value) -> Result<(), String> {
    if !byob_types::config::plugin_id_ok(plugin) {
        return Err(format!("{:?} isn't a plugin id", plugin.chars().take(60).collect::<String>()));
    }
    let size = serde_json::to_string(msg).map_or(usize::MAX, |s| s.len());
    if size > MAX_PLUGIN_MSG_BYTES {
        return Err(format!("plugin message too large ({size} bytes, at most {MAX_PLUGIN_MSG_BYTES})"));
    }
    Ok(())
}

/// The engine's side of the handshake ([`crate::ServerMsg::Welcome`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    pub protocol: u32,
    #[serde(default)]
    pub minor: u32,
    #[serde(default)]
    pub caps: Vec<String>,
}

impl Welcome {
    /// This build's engine.
    pub fn current() -> Self {
        Self { protocol: PROTOCOL_MAJOR, minor: PROTOCOL_MINOR, caps: engine_caps() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_are_shared_only_when_both_list_them() {
        let a = vec!["views".to_owned(), "plugin_channel".to_owned()];
        let b = vec!["views".to_owned(), "something_new".to_owned()];
        assert!(shared(&a, &b, CAP_VIEWS));
        assert!(!shared(&a, &b, CAP_PLUGIN_CHANNEL));
        assert!(!shared(&a, &b, "something_new"));
        assert!(check_caps(&a).is_ok() && check_caps(&client_caps()).is_ok() && check_caps(&engine_caps()).is_ok());
        assert!(check_caps(&["Bad Cap".to_owned()]).is_err());
        assert!(check_caps(&vec!["x".to_owned(); MAX_CAPS + 1]).is_err());
        assert!(check_caps(&["x".repeat(MAX_CAP_CHARS + 1)]).is_err());
    }

    #[test]
    fn plugin_messages_are_capped() {
        assert!(check_plugin_msg("sheets", &serde_json::json!({"op": "get"})).is_ok());
        assert!(check_plugin_msg("../x", &Value::Null).is_err());
        assert!(check_plugin_msg("sheets", &Value::String("x".repeat(MAX_PLUGIN_MSG_BYTES))).is_err());
    }
}
