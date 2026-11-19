//! Model profiles: a saved model (endpoint, model id, what it is good at) that the global
//! level, a workspace, a chat or a model role can pick by id.
//!
//! A profile lives in `settings.json` `profiles.<id>`; its API key in `secrets.json`
//! `profile_keys.<id>` (never here, never sent to clients). Roles (`settings.json`
//! `roles.<role>` → profile id) say which profile does what: [`ROLE_MAIN`] is the default
//! model of every chat, [`ROLE_VISION`] looks at images for a model that can't.
//!
//! [`ROLE_UTILITY`] writes chat titles and compaction summaries.
//!
//! A profile's [`Profile::params`] go with every chat request made with it, and its
//! [`Profile::context`] replaces the probed window (both through [`Profile::config`]).
//! Unknown fields are kept in [`Profile::extra`] so a newer byob's additions survive.
//!
//! This module is pure (no I/O): the store, the engine and the UI share it.

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::config::{Effective, Forced, Keys, Layer, Level, Levels, MAX_MODEL_CHARS, MAX_URL_CHARS, Sourced, ToolMode, resolve};
use crate::{ApiKind, InferenceConfig};

/// The default model of every chat (the "Connection" of older builds).
pub const ROLE_MAIN: &str = "main";
/// The model subagents use unless the `delegate` call, their agent or a workspace or chat
/// names another (the global level's subagent model).
pub const ROLE_SUBAGENTS: &str = "subagents";
/// Looks at images for a chat whose model can't.
pub const ROLE_VISION: &str = "vision";
/// Writes chat titles and compaction summaries (unset: the chat's own model).
pub const ROLE_UTILITY: &str = "utility";
/// The roles byob knows, in the order the UI lists them. Other role names (from
/// `settings.json` `models.<name>` of older builds) are kept and work as agents' `model:`.
pub const ROLES: [&str; 4] = [ROLE_MAIN, ROLE_SUBAGENTS, ROLE_VISION, ROLE_UTILITY];

/// Longest profile id.
pub const MAX_ID_CHARS: usize = 64;
/// Longest profile name.
pub const MAX_NAME_CHARS: usize = 80;
/// Longest description.
pub const MAX_DESCRIPTION_CHARS: usize = 1000;
/// Largest `params.extra`, as JSON.
pub const MAX_EXTRA_BYTES: usize = 16 * 1024;
/// Most profiles one settings file may hold.
pub const MAX_PROFILES: usize = 200;
/// Longest role name.
pub const MAX_ROLE_CHARS: usize = 64;

/// A field that falls back to `None` when the stored value has the wrong shape.
fn lenient<'de, D: Deserializer<'de>, T: DeserializeOwned>(d: D) -> Result<Option<T>, D::Error> {
    let v = Value::deserialize(d)?;
    Ok(serde_json::from_value(v).ok())
}

/// A field that falls back to its default when the stored value has the wrong shape.
fn lenient_or_default<'de, D: Deserializer<'de>, T: DeserializeOwned + Default>(d: D) -> Result<T, D::Error> {
    let v = Value::deserialize(d)?;
    Ok(serde_json::from_value(v).unwrap_or_default())
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Request body fields byob sets itself: [`Params::extra`] can't set them (the engine
/// refuses a profile that tries, and byob-llm skips them).
pub const PROTECTED_FIELDS: [&str; 7] = ["model", "messages", "tools", "stream", "stream_options", "id_slot", "system"];

/// How hard a Claude model thinks (Anthropic `output_config.effort`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub const ALL: [Effort; 5] = [Effort::Low, Effort::Medium, Effort::High, Effort::Xhigh, Effort::Max];

    /// The API's name for it.
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Xhigh => "xhigh",
            Effort::Max => "max",
        }
    }
}

/// Request parameters for a profile's model, sent with every chat request made with it
/// (see `byob_llm::params`). Unset ones are left to the server.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Params {
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Most tokens of one reply (Anthropic: replaces byob's 64000, thinking included).
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Anthropic only: `output_config.effort`; unset is the API's default.
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    /// A JSON object merged into the request body last (e.g. `{"chat_template_kwargs": {…}}`):
    /// objects merge key by key, anything else replaces. It may override the fields above,
    /// never the [`PROTECTED_FIELDS`].
    #[serde(default, deserialize_with = "lenient_or_default", skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
    /// Fields this build doesn't know; preserved.
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

impl Params {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// A saved model. Its key is in `secrets.json` `profile_keys.<id>`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    /// Stable id (`[a-z0-9][a-z0-9._-]*`); levels and roles point at it. In settings.json
    /// the map key is authoritative.
    #[serde(default, deserialize_with = "lenient_or_default")]
    pub id: String,
    /// What the user calls it.
    #[serde(default, deserialize_with = "lenient_or_default")]
    pub name: String,
    #[serde(default, deserialize_with = "lenient_or_default")]
    pub base_url: String,
    #[serde(default, deserialize_with = "lenient_or_default")]
    pub model: String,
    /// Free text: what it is good at ("fast, good at code").
    #[serde(default, deserialize_with = "lenient_or_default", skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// The API it speaks; `None` decides from the URL (see `InferenceConfig::api_kind`).
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub api: Option<ApiKind>,
    /// How tools are passed to this model; when set it beats the levels' tool mode.
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub tool_mode: Option<ToolMode>,
    /// Offered to the main model as a `delegate` `model` choice (its description says when).
    #[serde(default, deserialize_with = "lenient_or_default", skip_serializing_if = "is_false")]
    pub subagents: bool,
    /// Most subagents running on this model at once (besides the global cap and the
    /// server's free slots).
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub max_parallel: Option<u16>,
    /// Context window, overriding what the probe found (budgeting and compaction).
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub context: Option<u32>,
    /// Compact when a request passes this percentage (10..=95) of the context window,
    /// instead of the global setting. A workspace's or chat's own threshold still wins.
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub compact_percent: Option<u8>,
    /// Request parameters, sent with every chat request made with it.
    #[serde(default, deserialize_with = "lenient_or_default", skip_serializing_if = "Params::is_empty")]
    pub params: Params,
    /// Fields this build doesn't know (a newer byob's, or hand edits); preserved.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A profile id as byob makes them: `[a-z0-9][a-z0-9._-]{0,63}`.
pub fn id_ok(id: &str) -> bool {
    let b = id.as_bytes();
    let ok = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    !b.is_empty() && b.len() <= MAX_ID_CHARS && ok(b[0]) && b.iter().all(|&c| ok(c) || matches!(c, b'-' | b'_' | b'.'))
}

/// A role name: like an id, a little more lenient (older `models.<name>` keys).
pub fn role_ok(role: &str) -> bool {
    !role.trim().is_empty() && role.chars().count() <= MAX_ROLE_CHARS && !role.chars().any(char::is_control)
}

/// A fresh id for a profile called `name`, not in `taken`: its slug, then slug-2, slug-3, …
pub fn new_id(name: &str, taken: impl Fn(&str) -> bool) -> String {
    let mut slug = String::new();
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if matches!(c, '.' | '_') && !slug.is_empty() {
            slug.push(c);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug: String = slug.trim_end_matches(['-', '.', '_']).chars().take(48).collect();
    let slug = if slug.is_empty() { "model".to_owned() } else { slug };
    if !taken(&slug) {
        return slug;
    }
    (2..u32::MAX).map(|n| format!("{slug}-{n}")).find(|c| !taken(c)).unwrap_or(slug)
}

/// Scheme, host and port of a URL, then the rest without a trailing slash: two URLs that
/// mean the same endpoint compare equal.
pub fn same_url(a: &str, b: &str) -> bool {
    a.trim().trim_end_matches('/').eq_ignore_ascii_case(b.trim().trim_end_matches('/'))
}

impl Profile {
    /// Is it usable (a URL and a model)?
    pub fn usable(&self) -> bool {
        !self.base_url.trim().is_empty() && !self.model.trim().is_empty()
    }

    /// The endpoint, with `key`.
    pub fn config(&self, key: Option<String>) -> InferenceConfig {
        InferenceConfig {
            base_url: self.base_url.trim().to_owned(),
            model: self.model.trim().to_owned(),
            api_key: key.filter(|k| !k.is_empty()),
            api: self.api,
            params: Params { rest: Map::new(), ..self.params.clone() },
            context: self.context,
        }
    }

    /// Does it speak Anthropic's Messages API (set, or decided from the URL)?
    pub fn anthropic(&self) -> bool {
        self.config(None).api_kind() == ApiKind::Anthropic
    }

    /// Does it point at this endpoint (same URL and model)?
    pub fn points_at(&self, base_url: &str, model: &str) -> bool {
        same_url(&self.base_url, base_url) && self.model.trim() == model.trim()
    }

    /// The name, or the model when it has none.
    pub fn label(&self) -> &str {
        if self.name.trim().is_empty() { self.model.trim() } else { self.name.trim() }
    }

    /// Check an untrusted profile: trims text, refuses what doesn't fit. The id is the
    /// caller's business (the engine makes new ones).
    pub fn validated(mut self) -> Result<Self, String> {
        self.name = self.name.trim().to_owned();
        self.base_url = self.base_url.trim().to_owned();
        self.model = self.model.trim().to_owned();
        self.description = self.description.trim().to_owned();
        if self.name.is_empty() {
            self.name.clone_from(&self.model);
        }
        if self.name.chars().count() > MAX_NAME_CHARS {
            return Err(format!("names are limited to {MAX_NAME_CHARS} characters"));
        }
        if self.base_url.chars().count() > MAX_URL_CHARS {
            return Err("the URL is too long".into());
        }
        if !crate::endpoint_url_ok(&self.base_url) {
            return Err(format!("{:?} isn't an http://, https:// or cli:// URL", self.base_url));
        }
        if self.model.is_empty() {
            return Err("a model needs a model id (as the server names it)".into());
        }
        if self.model.chars().count() > MAX_MODEL_CHARS {
            return Err("the model name is too long".into());
        }
        if self.description.chars().count() > MAX_DESCRIPTION_CHARS {
            return Err(format!("descriptions are limited to {MAX_DESCRIPTION_CHARS} characters"));
        }
        let finite = |v: Option<f64>, max: f64| v.is_none_or(|v| v.is_finite() && (0.0..=max).contains(&v));
        if !finite(self.params.temperature, 5.0) {
            return Err("temperature must be between 0 and 5".into());
        }
        if !finite(self.params.top_p, 1.0) {
            return Err("top_p must be between 0 and 1".into());
        }
        if self.params.max_tokens == Some(0) {
            return Err("max_tokens must be at least 1".into());
        }
        if let Some(k) = PROTECTED_FIELDS.iter().find(|k| self.params.extra.contains_key(**k)) {
            return Err(format!("the extra request JSON can't set {k:?}: byob sets it"));
        }
        let size = serde_json::to_string(&self.params).map_or(usize::MAX, |s| s.len()) + serde_json::to_string(&self.extra).map_or(usize::MAX, |s| s.len());
        if size > MAX_EXTRA_BYTES {
            return Err(format!("extra request fields are limited to {MAX_EXTRA_BYTES} bytes"));
        }
        self.max_parallel = self.max_parallel.filter(|n| *n > 0);
        self.context = self.context.filter(|n| *n > 0);
        if self.compact_percent.is_some_and(|p| !(10..=95).contains(&p)) {
            return Err("compact at must be between 10 and 95 (% of the context window)".into());
        }
        Ok(self)
    }
}

/// Read a `profiles` map leniently: an entry that isn't an object is dropped (with a
/// warning), every other one keeps what it can; the map key becomes the id.
pub fn profiles_from(v: Value, warnings: &mut Vec<String>) -> BTreeMap<String, Profile> {
    let Value::Object(map) = v else {
        if !v.is_null() {
            warnings.push("ignored settings.json profiles: not an object".into());
        }
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    for (id, entry) in map {
        if !id_ok(&id) {
            warnings.push(format!("ignored profile {id:?}: ids are lowercase letters, digits, '-', '_' and '.'"));
            continue;
        }
        match serde_json::from_value::<Profile>(entry) {
            Ok(mut p) if out.len() < MAX_PROFILES => {
                p.id.clone_from(&id);
                out.insert(id, p);
            }
            Ok(_) => warnings.push(format!("ignored profile {id:?}: at most {MAX_PROFILES} profiles")),
            Err(e) => warnings.push(format!("ignored profile {id:?}: {e}")),
        }
    }
    out
}

/// Pick `wanted` (a profile id) among `profiles`, if it exists and is usable.
pub fn find<'a>(profiles: &'a BTreeMap<String, Profile>, wanted: Option<&str>) -> Option<&'a Profile> {
    wanted.and_then(|id| profiles.get(id)).filter(|p| p.usable())
}

/// A level as the resolver sees it: the endpoint of its profile in place of its own
/// `base_url`/`model`. A profile that doesn't exist (deleted) or can't be used leaves the
/// level without an endpoint, so it inherits one; its id comes back as the error.
pub fn apply<'a>(layer: &Layer, profiles: &'a BTreeMap<String, Profile>) -> (Layer, Result<Option<&'a Profile>, String>) {
    let Some(id) = layer.profile.as_deref() else { return (layer.clone(), Ok(None)) };
    match find(profiles, Some(id)) {
        Some(p) => (Layer { base_url: Some(p.base_url.trim().to_owned()), model: Some(p.model.trim().to_owned()), ..layer.clone() }, Ok(Some(p))),
        None => (Layer { profile: None, base_url: None, model: None, ..layer.clone() }, Err(id.to_owned())),
    }
}

/// What [`resolve_with`] found besides the settings.
#[derive(Debug)]
pub struct Resolved<'a> {
    pub effective: Effective,
    /// The profile whose endpoint is used, and the level that picked it.
    pub profile: Option<(Level, &'a Profile)>,
    /// Profiles levels point at that don't exist (they inherit instead).
    pub missing: Vec<String>,
}

/// [`resolve`] with profiles: each level's profile stands in for its endpoint; the key of
/// the level whose profile is used is that profile's (`has_key`); a profile's tool mode,
/// when it sets one, beats every level's (not a command-line flag).
pub fn resolve_with<'a>(
    levels: Levels<'_>,
    keys: Keys,
    forced: Forced,
    profiles: &'a BTreeMap<String, Profile>,
    has_key: impl Fn(&str) -> bool,
) -> Resolved<'a> {
    let mut missing = Vec::new();
    // Per level: the layer as resolved, its profile, and whether its profile is gone.
    let mut picked: [(Option<Layer>, Option<&'a Profile>, bool); 3] = [(None, None, false), (None, None, false), (None, None, false)];
    for (slot, layer) in picked.iter_mut().zip([levels.global, levels.workspace, levels.chat]) {
        if let Some(l) = layer {
            let (applied, p) = apply(l, profiles);
            match p {
                Ok(p) => slot.1 = p,
                Err(id) => {
                    missing.push(id);
                    slot.2 = true;
                }
            }
            slot.0 = Some(applied);
        }
    }
    let [(g, gp, g_gone), (w, wp, w_gone), (c, cp, c_gone)] = &picked;
    let applied = Levels { global: g.as_ref(), workspace: w.as_ref(), chat: c.as_ref() };
    // A level with a profile has the key its profile has (none: none), whatever it stored;
    // the key a level kept for a profile that is gone isn't used.
    let own = |p: &Option<&Profile>, gone: bool, stored: bool| match p {
        Some(p) => has_key(&p.id),
        None => stored && !gone,
    };
    let keys = Keys {
        global: own(gp, *g_gone, keys.global) || (gp.is_some() && keys.global),
        workspace: own(wp, *w_gone, keys.workspace),
        chat: own(cp, *c_gone, keys.chat),
    };
    let mut effective = resolve(applied, keys, forced);
    // The subagent model: the most specific level's that still exists.
    effective.subagent_profile = None;
    for (from, layer) in [(Level::Chat, levels.chat), (Level::Workspace, levels.workspace), (Level::Global, levels.global)] {
        let Some(id) = layer.and_then(|l| l.subagent_profile.as_deref()) else { continue };
        if find(profiles, Some(id)).is_some() {
            effective.subagent_profile = Some(Sourced { value: id.to_owned(), from });
            break;
        }
        missing.push(id.to_owned());
    }
    let from = effective.base_url.from;
    let profile = match from {
        Level::Global => *gp,
        Level::Workspace => *wp,
        Level::Chat => *cp,
        Level::Default | Level::Flag => None,
    }
    .filter(|p| p.points_at(&effective.base_url.value, &effective.model.value))
    .map(|p| (from, p));
    if let Some((level, p)) = profile {
        effective.key_from = (has_key(&p.id) || (level == Level::Global && keys.global)).then_some(level);
        if let Some(m) = p.tool_mode
            && effective.tool_mode.from != Level::Flag
        {
            effective.tool_mode = Sourced { value: m, from: level };
        }
        // The model's threshold replaces the default and the global one, not a workspace's
        // or chat's own.
        if let Some(pc) = p.compact_percent
            && matches!(effective.compact_percent.from, Level::Default | Level::Global)
        {
            effective.compact_percent = Sourced { value: pc.clamp(10, 95), from: level };
        }
    }
    Resolved { effective, profile, missing }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(id: &str, url: &str, model: &str) -> Profile {
        Profile { id: id.into(), name: id.into(), base_url: url.into(), model: model.into(), ..Profile::default() }
    }

    #[test]
    fn a_levels_profile_beats_its_own_endpoint_and_the_most_specific_level_wins() {
        let mut ps = BTreeMap::new();
        ps.insert("big".to_owned(), Profile { tool_mode: Some(ToolMode::Prompt), ..profile("big", "http://big/v1", "large") });
        ps.insert("small".to_owned(), profile("small", "http://small/v1", "tiny"));
        let g = Layer { base_url: Some("http://g/v1".into()), model: Some("gm".into()), tool_mode: Some(ToolMode::Native), ..Layer::default() };
        // The workspace picked "big" and still carries an old copy of another endpoint.
        let w = Layer { profile: Some("big".into()), base_url: Some("http://stale/v1".into()), model: Some("stale".into()), ..Layer::default() };
        let has = |id: &str| id == "big";
        let r =
            resolve_with(Levels { global: Some(&g), workspace: Some(&w), chat: None }, Keys { global: true, ..Keys::default() }, Forced::default(), &ps, has);
        assert_eq!(r.effective.base_url, Sourced { value: "http://big/v1".into(), from: Level::Workspace });
        assert_eq!(r.effective.model.value, "large");
        assert_eq!(r.effective.key_from, Some(Level::Workspace));
        assert_eq!(r.effective.tool_mode, Sourced { value: ToolMode::Prompt, from: Level::Workspace }, "the profile's tool mode beats the levels'");
        assert_eq!(r.profile.map(|(l, p)| (l, p.id.as_str())), Some((Level::Workspace, "big")));
        // A chat's profile beats the workspace's; it has no key, so none is used.
        let c = Layer { profile: Some("small".into()), ..Layer::default() };
        let r = resolve_with(
            Levels { global: Some(&g), workspace: Some(&w), chat: Some(&c) },
            Keys { global: true, workspace: true, chat: true },
            Forced::default(),
            &ps,
            has,
        );
        assert_eq!((r.effective.base_url.value.as_str(), r.effective.model.from), ("http://small/v1", Level::Chat));
        assert_eq!(r.effective.key_from, None, "a profile without a key uses none, even if the level stored one");
        assert_eq!(r.effective.tool_mode, Sourced { value: ToolMode::Native, from: Level::Global });
        // A command-line tool mode still wins.
        let forced = Forced { tool_mode: Some(ToolMode::Native), ..Forced::default() };
        let r = resolve_with(Levels { global: Some(&g), workspace: Some(&w), chat: None }, Keys::default(), forced, &ps, has);
        assert_eq!(r.effective.tool_mode.from, Level::Flag);
    }

    /// A model's compaction threshold replaces the default and the global one; a workspace's
    /// or chat's own threshold still wins over it.
    #[test]
    fn a_models_compaction_threshold_overrides_the_default() {
        let mut ps = BTreeMap::new();
        ps.insert("big".to_owned(), Profile { compact_percent: Some(60), ..profile("big", "http://big/v1", "large") });
        let has = |_: &str| false;
        let at = |g: Layer, w: Layer| {
            resolve_with(Levels { global: Some(&g), workspace: Some(&w), chat: None }, Keys::default(), Forced::default(), &ps, has).effective.compact_percent
        };
        let pick = Layer { profile: Some("big".into()), ..Layer::default() };
        assert_eq!(at(Layer::default(), pick.clone()), Sourced { value: 60, from: Level::Workspace }, "over the default");
        assert_eq!(at(Layer { compact_percent: Some(85), ..Layer::default() }, pick.clone()).value, 60, "over the global setting");
        assert_eq!(at(Layer::default(), Layer { compact_percent: Some(40), ..pick }).value, 40, "the workspace's own wins");
        // Without the profile, the default.
        assert_eq!(at(Layer::default(), Layer::default()).value, crate::config::DEFAULT_COMPACT_PERCENT);
        assert!(Profile { compact_percent: Some(99), ..profile("x", "http://x/v1", "m") }.validated().is_err());
    }

    #[test]
    fn a_deleted_profile_falls_back_to_inheriting() {
        let ps = BTreeMap::new();
        let g = Layer { base_url: Some("http://g/v1".into()), model: Some("gm".into()), ..Layer::default() };
        let w = Layer { profile: Some("gone".into()), base_url: Some("http://copy/v1".into()), model: Some("copy".into()), ..Layer::default() };
        let r = resolve_with(
            Levels { global: Some(&g), workspace: Some(&w), chat: None },
            Keys { global: true, workspace: true, chat: false },
            Forced::default(),
            &ps,
            |_| true,
        );
        assert_eq!(r.effective.base_url, Sourced { value: "http://g/v1".into(), from: Level::Global });
        assert_eq!(r.effective.key_from, Some(Level::Global));
        assert_eq!(r.missing, ["gone"]);
        assert!(r.profile.is_none());
        // An unusable profile (no URL) counts as missing too.
        let ps = [("x".to_owned(), profile("x", "", "m"))].into();
        let c = Layer { profile: Some("x".into()), ..Layer::default() };
        let r = resolve_with(Levels { global: Some(&g), workspace: None, chat: Some(&c) }, Keys::default(), Forced::default(), &ps, |_| false);
        assert_eq!((r.effective.base_url.from, r.missing.len()), (Level::Global, 1));
    }

    #[test]
    fn profiles_round_trip_and_keep_unknown_fields() {
        let json = serde_json::json!({
            "id": "qwen",
            "name": "Qwen",
            "base_url": "http://h/v1",
            "model": "qwen3",
            "description": "fast, good at code",
            "tool_mode": "native",
            "subagents": true,
            "max_parallel": 2,
            "context": 32768,
            "params": {"temperature": 0.7, "top_p": 0.9, "max_tokens": 4096, "effort": "xhigh", "extra": {"chat_template_kwargs": {"enable_thinking": false}}, "min_p": 0.05},
            "api": "anthropic",
            "effort_hint": "low",
        });
        let p: Profile = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(p.tool_mode, Some(ToolMode::Native));
        assert_eq!((p.subagents, p.max_parallel, p.context), (true, Some(2), Some(32768)));
        assert_eq!(p.params.temperature, Some(0.7));
        assert_eq!(p.params.rest.get("min_p"), Some(&serde_json::json!(0.05)));
        assert_eq!(p.api, Some(ApiKind::Anthropic));
        assert_eq!(p.extra.get("effort_hint"), Some(&serde_json::json!("low")));
        assert_eq!(serde_json::to_value(&p).unwrap(), json);
        // Its endpoint carries its request options and context, not the fields it doesn't know.
        let c = p.config(None);
        assert_eq!((c.params.effort, c.params.max_tokens, c.context), (Some(Effort::Xhigh), Some(4096), Some(32768)));
        assert!(c.params.rest.is_empty() && c.params.extra.contains_key("chat_template_kwargs"));
        // An effort this build doesn't know reads as unset.
        let odd: Params = serde_json::from_value(serde_json::json!({"effort": "ludicrous", "top_p": 0.5})).unwrap();
        assert_eq!((odd.effort, odd.top_p), (None, Some(0.5)));
        // A minimal profile writes only what it has.
        let small = Profile { id: "a".into(), name: "A".into(), base_url: "http://h/v1".into(), model: "m".into(), ..Profile::default() };
        assert_eq!(serde_json::to_string(&small).unwrap(), r#"{"id":"a","name":"A","base_url":"http://h/v1","model":"m"}"#);
    }

    #[test]
    fn bad_values_fall_back_one_by_one() {
        let mut warnings = Vec::new();
        let v = serde_json::json!({
            "ok": {"base_url": "http://h/v1", "model": "m", "tool_mode": "sometimes", "max_parallel": -3, "params": "hot"},
            "Bad Id": {"base_url": "http://h/v1", "model": "m"},
            "worse": 5,
        });
        let ps = profiles_from(v, &mut warnings);
        assert_eq!(ps.len(), 1, "{warnings:?}");
        let p = &ps["ok"];
        assert_eq!((p.id.as_str(), p.tool_mode, p.max_parallel, p.params.is_empty()), ("ok", None, None, true));
        assert_eq!(warnings.len(), 2);
        assert!(profiles_from(Value::Null, &mut warnings).is_empty());
    }

    #[test]
    fn untrusted_profiles_are_checked() {
        let p = Profile { name: " ".into(), base_url: " http://h/v1 ".into(), model: " m ".into(), ..Profile::default() };
        let ok = p.clone().validated().unwrap();
        assert_eq!((ok.name.as_str(), ok.base_url.as_str(), ok.model.as_str()), ("m", "http://h/v1", "m"));
        assert!(Profile { base_url: "file:///etc".into(), ..p.clone() }.validated().is_err());
        assert!(Profile { model: String::new(), ..p.clone() }.validated().is_err());
        let hot = Profile { params: Params { temperature: Some(f64::NAN), ..Params::default() }, ..p.clone() };
        assert!(hot.validated().is_err());
        let mut big = p;
        big.params.extra.insert("x".into(), Value::String("y".repeat(MAX_EXTRA_BYTES)));
        assert!(big.validated().is_err());
        // The extra request JSON can't set the fields byob sets; anything else goes.
        let base = Profile { base_url: "http://h/v1".into(), model: "m".into(), ..Profile::default() };
        for k in PROTECTED_FIELDS {
            let mut p = base.clone();
            p.params.extra.insert(k.into(), Value::Bool(true));
            let err = p.validated().unwrap_err();
            assert!(err.contains(k), "{err}");
        }
        let mut ok = base.clone();
        ok.params.extra.insert("chat_template_kwargs".into(), serde_json::json!({"enable_thinking": false}));
        assert!(ok.validated().is_ok());
        assert!(Profile { params: Params { max_tokens: Some(0), ..Params::default() }, ..base }.validated().is_err());
    }

    #[test]
    fn ids_are_slugs_and_unique() {
        assert_eq!(new_id("Qwen 3 (fast)", |_| false), "qwen-3-fast");
        assert_eq!(new_id("gpt-4.1", |_| false), "gpt-4.1");
        assert_eq!(new_id("  ", |_| false), "model");
        assert_eq!(new_id("x", |id| id == "x" || id == "x-2"), "x-3");
        assert!(id_ok("qwen-3_b.1") && !id_ok("Qwen") && !id_ok("-a") && !id_ok(""));
        assert!(same_url("HTTP://h:1/v1/", "http://h:1/v1"));
    }

    #[test]
    fn the_subagent_model_is_the_most_specific_levels_that_exists() {
        let ps: BTreeMap<String, Profile> =
            [("role".to_owned(), profile("role", "http://r/v1", "r")), ("ws".to_owned(), profile("ws", "http://w/v1", "w"))].into();
        let g = Layer { subagent_profile: Some("role".into()), ..Layer::default() };
        let w = Layer { subagent_profile: Some("ws".into()), ..Layer::default() };
        let c = Layer { subagent_profile: Some("gone".into()), ..Layer::default() };
        let at = |levels: Levels<'_>| {
            let r = resolve_with(levels, Keys::default(), Forced::default(), &ps, |_| false);
            (r.effective.subagent_profile.map(|s| (s.value, s.from)), r.missing)
        };
        let some = |id: &str, l: Level| Some((id.to_owned(), l));
        assert_eq!(at(Levels { global: Some(&g), workspace: None, chat: None }), (some("role", Level::Global), vec![]));
        assert_eq!(at(Levels { global: Some(&g), workspace: Some(&w), chat: None }), (some("ws", Level::Workspace), vec![]));
        // The chat's is deleted: the workspace's applies, and the chat's is reported.
        assert_eq!(at(Levels { global: Some(&g), workspace: Some(&w), chat: Some(&c) }), (some("ws", Level::Workspace), vec!["gone".to_owned()]));
        assert_eq!(at(Levels { global: None, workspace: None, chat: Some(&c) }), (None, vec!["gone".to_owned()]));
        assert_eq!(at(Levels::default()), (None, vec![]));
        // Checked like a level's own profile.
        assert!(Layer { subagent_profile: Some("Not An Id".into()), ..Layer::default() }.validated(Level::Chat).is_err());
        assert_eq!(Layer { subagent_profile: Some(" ".into()), ..Layer::default() }.validated(Level::Chat).unwrap().subagent_profile, None);
    }
}
