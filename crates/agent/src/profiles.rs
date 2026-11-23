//! Model profiles in the engine: the saved models (`settings.json` `profiles`, keys in
//! `secrets.json` `profile_keys`), the roles that point at them, and the levels that pick one.
//!
//! The engine owns them: clients list them ([`ServerMsg::Profiles`]), change them through
//! the profile messages, and never see a key. Every change is saved with the copies older
//! builds read (`byob_store::profiles::mirror`): the main profile as `inference` + `api_key`,
//! other roles as `models.<role>`, and each level that picked a profile keeps its
//! `base_url`/`model` and scoped key in step with it.

use byob_protocol::profiles::{ProfileList, ProfileTest, ProfileView};
use byob_protocol::workspace::Field;
use byob_protocol::{ConfigScope, InferenceTest, Link, NoticeLevel, Problem, ServerMsg};
use byob_types::InferenceConfig;
use byob_types::config::{ToolMode, origin};
use byob_types::profile::{self, MAX_PROFILES, Profile, ROLE_MAIN};

use crate::{ClientId, Engine, adapt, connect};

impl Engine {
    /// The stored key of profile `id`.
    pub(crate) fn profile_key(&self, id: &str) -> Option<String> {
        self.profile_keys.get(id).filter(|k| !k.trim().is_empty()).cloned()
    }

    /// The endpoint of profile `id`, with its key.
    pub(crate) fn profile_config(&self, id: &str) -> Option<InferenceConfig> {
        profile::find(&self.profiles, Some(id)).map(|p| p.config(self.profile_key(id)))
    }

    /// The main profile, if the global endpoint is it (not, say, `--inference-url`).
    pub(crate) fn main_profile(&self) -> Option<&Profile> {
        profile::find(&self.profiles, self.roles.get(ROLE_MAIN).map(String::as_str)).filter(|p| p.points_at(&self.config.base_url, &self.config.model))
    }

    /// Model roles by name for agents' `model:` and the vision role: the configured ones
    /// (`models`), then every role but `main` through its profile.
    pub(crate) fn rebuild_models(&mut self) {
        let mut models = self.base_models.clone();
        for (role, id) in &self.roles {
            if role == ROLE_MAIN {
                continue;
            }
            match self.profile_config(id) {
                Some(c) => {
                    models.insert(role.clone(), c);
                }
                None => log::warn!("model role {role} points at profile {id:?}, which doesn't exist"),
            }
        }
        self.models = models;
    }

    /// Log once that a level points at a profile that doesn't exist.
    pub(crate) fn warn_missing(&self, ids: &[String]) {
        let mut warned = self.missing_warned.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for id in ids {
            if warned.insert(id.clone()) {
                log::warn!("a workspace or chat uses model profile {id:?}, which doesn't exist (deleted?); it inherits its model instead");
            }
        }
    }

    fn link_of(&self, config: &InferenceConfig) -> Link {
        let key = adapt::key(config);
        if key == adapt::key(&self.config) { self.link.clone() } else { self.links.get(&key).cloned().unwrap_or_default() }
    }

    pub(crate) fn profiles_msg(&self) -> ServerMsg {
        let mut items: Vec<ProfileView> = self
            .profiles
            .values()
            .map(|p| {
                let config = p.config(self.profile_key(&p.id));
                let info = p.usable().then(|| self.endpoints.info_with(&config, p.tool_mode.unwrap_or(ToolMode::Auto)));
                ProfileView { profile: p.clone(), has_key: config.api_key.is_some(), link: self.link_of(&config), info }
            })
            .collect();
        items.sort_by(|a, b| a.profile.label().to_lowercase().cmp(&b.profile.label().to_lowercase()).then_with(|| a.profile.id.cmp(&b.profile.id)));
        ServerMsg::Profiles(Box::new(ProfileList { items, roles: self.roles.clone() }))
    }

    /// Send the profiles to every client that knows them.
    pub(crate) fn broadcast_profiles(&mut self) {
        let msg = self.profiles_msg();
        let ids: Vec<ClientId> =
            self.clients.iter().filter(|(_, c)| c.attached && c.caps.iter().any(|c| c == byob_protocol::CAP_PROFILES)).map(|(id, _)| *id).collect();
        for id in ids {
            self.send_to(id, msg.clone());
        }
    }

    pub(crate) fn send_profiles(&mut self, client: ClientId) {
        if self.clients.get(&client).is_some_and(|c| c.caps.iter().any(|c| c == byob_protocol::CAP_PROFILES)) {
            let msg = self.profiles_msg();
            self.send_to(client, msg);
        }
    }

    fn save_profiles(&self) -> Result<(), String> {
        let Some(paths) = &self.paths else { return Ok(()) };
        byob_store::profiles::save(paths, &self.profiles, &self.roles, &self.profile_keys).map_err(|e| format!("couldn't save the models: {e}"))
    }

    /// Probe every profile's endpoint in the background (once each; see [`adapt::Endpoints`]).
    pub(crate) fn probe_profiles(&mut self) {
        let configs: Vec<InferenceConfig> = self.profiles.values().filter(|p| p.usable()).map(|p| p.config(self.profile_key(&p.id))).collect();
        for c in configs {
            self.probe_endpoint(&c);
        }
    }

    fn probe_endpoint(&mut self, config: &InferenceConfig) {
        if let Ok(client) = byob_llm::Client::new(config.clone()) {
            self.endpoints.probe(&client, &self.inbox);
        }
    }

    /// After profiles or roles changed: the role models, the global endpoint (if it follows
    /// the main profile), the levels' copies, the files, and every client.
    fn profiles_changed(&mut self, main_before: Option<InferenceConfig>, touched: &[String]) -> Result<(), String> {
        self.rebuild_models();
        for id in touched {
            self.mirror_levels(id);
        }
        // Saved before anyone hears of it.
        let saved = self.save_profiles();
        // The global endpoint follows the main profile unless something else set it (a
        // command-line flag): it did before, or nothing was set up yet (the built-in default).
        let following = match &main_before {
            Some(b) => adapt::key(b) == adapt::key(&self.config) && b.api_key == self.config.api_key,
            None => adapt::key(&self.config) == adapt::key(&InferenceConfig::default()),
        };
        match self.main_config().filter(|n| following && *n != self.config) {
            Some(now) => self.switch_global(now),
            None => self.broadcast_state(),
        }
        self.broadcast_profiles();
        self.broadcast_workspaces();
        saved
    }

    /// The endpoint the main profile has now, with its key.
    fn main_config(&self) -> Option<InferenceConfig> {
        self.roles.get(ROLE_MAIN).and_then(|id| self.profile_config(id))
    }

    /// Bring the copies of every level that picked profile `id` up to date: its URL and model
    /// (and key, for older builds), or, if it is gone, inherit again.
    fn mirror_levels(&mut self, id: &str) {
        let p = profile::find(&self.profiles, Some(id)).cloned();
        let key = self.profile_key(id);
        let fix = |l: &mut byob_protocol::workspace::Layer| match &p {
            Some(p) => {
                l.base_url = Some(p.base_url.trim().to_owned());
                l.model = Some(p.model.trim().to_owned());
            }
            None => l.reset(Field::Endpoint),
        };
        let workspaces: Vec<(String, byob_protocol::workspace::Layer)> =
            self.workspaces.list().iter().filter(|w| w.config.profile.as_deref() == Some(id)).map(|w| (w.id.clone(), w.config.clone())).collect();
        for (ws, mut layer) in workspaces {
            fix(&mut layer);
            if let Err(e) = self.workspaces.set_config(&ws, layer) {
                log::warn!("updating the model of workspace {ws}: {e}");
            }
            self.set_scoped_key(&format!("workspace:{ws}"), key.clone());
        }
        let chats: Vec<(String, byob_protocol::workspace::Layer)> =
            self.workspaces.chat_configs().iter().filter(|(_, l)| l.profile.as_deref() == Some(id)).map(|(c, l)| (c.clone(), l.clone())).collect();
        for (chat, mut layer) in chats {
            fix(&mut layer);
            if let Err(e) = self.workspaces.set_chat_config(&chat, layer) {
                log::warn!("updating the model of chat {chat}: {e}");
            }
            self.set_scoped_key(&format!("chat:{chat}"), key.clone());
        }
        for c in self.clients.values_mut().filter(|c| c.new_layer.profile.as_deref() == Some(id)) {
            fix(&mut c.new_layer);
            c.new_key.clone_from(&key);
        }
        if p.is_none() {
            self.forget_subagent_profile(id);
        }
    }

    /// Profile `id` is gone: levels that ran their subagents on it use what they inherit.
    fn forget_subagent_profile(&mut self, id: &str) {
        let uses = |l: &byob_protocol::workspace::Layer| l.subagent_profile.as_deref() == Some(id);
        let workspaces: Vec<(String, byob_protocol::workspace::Layer)> =
            self.workspaces.list().iter().filter(|w| uses(&w.config)).map(|w| (w.id.clone(), w.config.clone())).collect();
        for (ws, mut layer) in workspaces {
            layer.subagent_profile = None;
            if let Err(e) = self.workspaces.set_config(&ws, layer) {
                log::warn!("updating the subagent model of workspace {ws}: {e}");
            }
        }
        let chats: Vec<(String, byob_protocol::workspace::Layer)> =
            self.workspaces.chat_configs().iter().filter(|(_, l)| uses(l)).map(|(c, l)| (c.clone(), l.clone())).collect();
        for (chat, mut layer) in chats {
            layer.subagent_profile = None;
            if let Err(e) = self.workspaces.set_chat_config(&chat, layer) {
                log::warn!("updating the subagent model of chat {chat}: {e}");
            }
        }
        for c in self.clients.values_mut().filter(|c| uses(&c.new_layer)) {
            c.new_layer.subagent_profile = None;
        }
    }

    /// Workspaces and chats with an endpoint of their own (set before profiles) become
    /// profiles, like the main connection and the roles did in the store. Runs at startup;
    /// a second run finds nothing to do.
    pub(crate) fn migrate_levels(&mut self) {
        let own = |l: &byob_protocol::workspace::Layer| l.profile.is_none() && l.base_url.is_some() && l.model.is_some();
        let mut changed = false;
        let workspaces: Vec<byob_store::WorkspaceRow> = self.workspaces.list().iter().filter(|w| own(&w.config)).cloned().collect();
        for ws in workspaces {
            let c = self.workspace_settings(&ws).inference;
            let id = byob_store::profiles::ensure(&mut self.profiles, &mut self.profile_keys, &c.base_url, &c.model, c.api_key.as_deref(), "");
            let layer = byob_protocol::workspace::Layer { profile: Some(id), ..ws.config.clone() };
            match self.workspaces.set_config(&ws.id, layer) {
                Ok(()) => changed = true,
                Err(e) => log::warn!("giving workspace {} a model profile: {e}", ws.id),
            }
        }
        let chats: Vec<(String, byob_protocol::workspace::Layer)> =
            self.workspaces.chat_configs().iter().filter(|(_, l)| own(l)).map(|(c, l)| (c.clone(), l.clone())).collect();
        for (chat, layer) in chats {
            let c = self.chat_settings(Some(&chat), None).inference;
            let id = byob_store::profiles::ensure(&mut self.profiles, &mut self.profile_keys, &c.base_url, &c.model, c.api_key.as_deref(), "");
            match self.workspaces.set_chat_config(&chat, byob_protocol::workspace::Layer { profile: Some(id), ..layer }) {
                Ok(()) => changed = true,
                Err(e) => log::warn!("giving chat {chat} a model profile: {e}"),
            }
        }
        if changed {
            log::info!("workspaces and chats with their own endpoint now use model profiles");
            if let Err(e) = self.save_profiles() {
                log::error!("{e}");
            }
            self.rebuild_models();
        }
    }

    // ---- client messages ----------------------------------------------------------------

    pub(crate) fn create_profile(&mut self, client: ClientId, profile: Profile, api_key: Option<String>, assign: Option<ConfigScope>, make_main: bool) {
        match self.try_create(profile, api_key, make_main) {
            Ok(id) => {
                if let Some(scope) = assign {
                    self.set_level_profile(client, scope, Some(id));
                }
            }
            Err(e) => self.notice(client, e),
        }
    }

    fn try_create(&mut self, profile: Profile, api_key: Option<String>, make_main: bool) -> Result<String, String> {
        if self.profiles.len() >= MAX_PROFILES {
            return Err(format!("byob keeps at most {MAX_PROFILES} models; delete one first"));
        }
        let mut p = profile.validated()?;
        let id = profile::new_id(&p.name, |id| self.profiles.contains_key(id));
        p.id.clone_from(&id);
        let main_before = self.main_config();
        let config = p.config(None);
        self.profiles.insert(id.clone(), p);
        if let Some(k) = api_key.map(|k| k.trim().to_owned()).filter(|k| !k.is_empty()) {
            self.profile_keys.insert(id.clone(), k);
        }
        if make_main || !self.roles.contains_key(ROLE_MAIN) {
            self.roles.insert(ROLE_MAIN.to_owned(), id.clone());
        }
        self.probe_endpoint(&InferenceConfig { api_key: self.profile_key(&id), ..config });
        self.profiles_changed(main_before, &[])?;
        Ok(id)
    }

    pub(crate) fn update_profile(&mut self, client: ClientId, profile: Profile, api_key: Option<String>) {
        if let Err(e) = self.try_update(profile, api_key) {
            self.notice(client, e);
        }
    }

    fn try_update(&mut self, profile: Profile, api_key: Option<String>) -> Result<(), String> {
        let id = profile.id.clone();
        let old = self.profiles.get(&id).cloned().ok_or("that model no longer exists")?;
        let mut p = profile.validated()?;
        p.id.clone_from(&id);
        // Fields this client doesn't know about stay as they were.
        for (k, v) in old.extra {
            p.extra.entry(k).or_insert(v);
        }
        let main_before = self.main_config();
        let key = match api_key {
            Some(k) => Some(k.trim().to_owned()).filter(|k| !k.is_empty()),
            // A key belongs to its server: it doesn't follow the URL elsewhere.
            None => self.profile_key(&id).filter(|_| origin(&old.base_url) == origin(&p.base_url)),
        };
        match key {
            Some(k) => self.profile_keys.insert(id.clone(), k),
            None => self.profile_keys.remove(&id),
        };
        let config = p.config(self.profile_key(&id));
        self.profiles.insert(id.clone(), p);
        self.probe_endpoint(&config);
        self.profiles_changed(main_before, &[id])
    }

    pub(crate) fn delete_profile(&mut self, client: ClientId, id: &str) {
        if self.roles.get(ROLE_MAIN).is_some_and(|m| m == id) {
            self.notice(client, "that is the default model; make another one the default first".into());
            return;
        }
        if self.profiles.remove(id).is_none() {
            self.notice(client, "that model no longer exists".into());
            return;
        }
        let main_before = self.main_config();
        self.profile_keys.remove(id);
        self.roles.retain(|_, p| p != id);
        if let Err(e) = self.profiles_changed(main_before, &[id.to_owned()]) {
            self.notice(client, e);
        }
    }

    pub(crate) fn duplicate_profile(&mut self, client: ClientId, id: &str) {
        let Some(old) = self.profiles.get(id).cloned() else {
            self.notice(client, "that model no longer exists".into());
            return;
        };
        let name: String = format!("{} (copy)", old.label()).chars().take(profile::MAX_NAME_CHARS).collect();
        let key = self.profile_key(id);
        if let Err(e) = self.try_create(Profile { name, ..old }, key, false) {
            self.notice(client, e);
        }
    }

    pub(crate) fn set_role(&mut self, client: ClientId, role: &str, id: Option<String>) {
        let main_before = self.main_config();
        match id {
            Some(id) if profile::find(&self.profiles, Some(&id)).is_none() => {
                self.notice(client, "that model no longer exists".into());
                return;
            }
            Some(id) => {
                self.roles.insert(role.trim().to_owned(), id);
            }
            None if role == ROLE_MAIN => {
                self.notice(client, "there is always a default model; pick another one instead".into());
                return;
            }
            None => {
                self.roles.remove(role.trim());
            }
        }
        if let Err(e) = self.profiles_changed(main_before, &[]) {
            self.notice(client, e);
        }
    }

    /// `ClientMsg::SetLevelProfile`: the level's settings with its model changed.
    pub(crate) fn set_level_profile(&mut self, client: ClientId, scope: ConfigScope, id: Option<String>) {
        if let Some(i) = &id
            && profile::find(&self.profiles, Some(i)).is_none()
        {
            self.notice(client, "that model no longer exists".into());
            return;
        }
        let mut layer = match &scope {
            ConfigScope::Global => {
                match id {
                    Some(id) => self.set_role(client, ROLE_MAIN, Some(id)),
                    None => self.notice(client, "every chat needs a default model".into()),
                }
                self.send_config(client, scope);
                return;
            }
            ConfigScope::Workspace { id: w } => match self.workspaces.get(w) {
                Some(row) => row.config.clone(),
                None => {
                    self.notice(client, "that workspace no longer exists".into());
                    return;
                }
            },
            ConfigScope::Chat => match self.client_chat(client) {
                Some(chat) => self.workspaces.chat_config(&chat).cloned().unwrap_or_default(),
                None => self.clients.get(&client).map(|c| c.new_layer.clone()).unwrap_or_default(),
            },
        };
        let clear_key = id.is_none().then(String::new);
        layer.reset(Field::Endpoint);
        layer.profile = id;
        self.set_config(client, scope, layer, clear_key);
    }

    /// `ClientMsg::TestProfile`: check the profile's endpoint in the background.
    pub(crate) fn test_profile(&mut self, client: ClientId, id: String) {
        let answer =
            |detail: &str| ServerMsg::ProfileTested(ProfileTest { id: id.clone(), test: InferenceTest { detail: detail.into(), ..InferenceTest::default() } });
        let Some(config) = self.profile_config(&id) else {
            self.send_to(client, answer("that model no longer exists"));
            return;
        };
        if !self.profile_tests.insert(id.clone()) {
            self.send_to(client, answer("a test of this model is already running"));
            return;
        }
        let client_llm = match byob_llm::Client::new(config.clone()) {
            Ok(c) => c,
            Err(e) => {
                self.profile_tests.remove(&id);
                let test = InferenceTest {
                    base_url: config.base_url,
                    model: config.model,
                    problem: Some(Problem::BadUrl),
                    detail: e.to_string(),
                    ..InferenceTest::default()
                };
                self.send_to(client, ServerMsg::ProfileTested(ProfileTest { id, test }));
                return;
            }
        };
        self.endpoints.probe(&client_llm, &self.inbox);
        let key = adapt::key(&config);
        let crashed = connect::Event::ProfileTested {
            client,
            id: id.clone(),
            key: key.clone(),
            test: Box::new(InferenceTest { base_url: config.base_url, model: config.model, detail: connect::CRASHED.into(), ..InferenceTest::default() }),
        };
        let started = connect::spawn("byob-test", &self.inbox, crashed, move || connect::Event::ProfileTested {
            client,
            id: id.clone(),
            key,
            test: Box::new(connect::test(&client_llm)),
        });
        if !started {
            self.profile_tests.clear();
        }
    }

    /// A profile test finished: answer, and remember how its endpoint did.
    pub(crate) fn profile_tested(&mut self, client: ClientId, id: String, key: String, test: InferenceTest) {
        self.profile_tests.remove(&id);
        let link = if test.ok { Link::Ok } else { Link::Failed { problem: test.problem, detail: test.detail.clone() } };
        if key == adapt::key(&self.config) {
            self.set_link(link);
        } else {
            self.links.insert(key, link);
        }
        self.send_to(client, ServerMsg::ProfileTested(ProfileTest { id, test }));
        self.broadcast_profiles();
        self.broadcast_state();
    }

    /// `ClientMsg::SetInference` (the connection form, the welcome screen): the main profile
    /// becomes this endpoint. The main profile itself changes only its key when it is this
    /// endpoint already; otherwise a profile for it is found or made and becomes the main one.
    pub(crate) fn set_inference_profile(&mut self, client: ClientId, config: InferenceConfig) {
        let main = self
            .roles
            .get(ROLE_MAIN)
            .cloned()
            .filter(|id| profile::find(&self.profiles, Some(id)).is_some_and(|p| p.points_at(&config.base_url, &config.model)));
        let id = match main {
            Some(id) => {
                match &config.api_key {
                    Some(k) => self.profile_keys.insert(id.clone(), k.clone()),
                    None => self.profile_keys.remove(&id),
                };
                id
            }
            None => byob_store::profiles::ensure(&mut self.profiles, &mut self.profile_keys, &config.base_url, &config.model, config.api_key.as_deref(), ""),
        };
        // The API the endpoint speaks goes with it (set for the URL it was saved for).
        if let (Some(api), Some(p)) = (config.api, self.profiles.get_mut(&id)) {
            p.api = Some(api);
        }
        self.roles.insert(ROLE_MAIN.to_owned(), id.clone());
        // The form asked for this endpoint: switch even if a flag had set another.
        let following = Some(self.config.clone());
        if let Err(e) = self.profiles_changed(following, &[id]) {
            self.send_to(client, ServerMsg::Notice { level: NoticeLevel::Error, text: format!("the model applies now but {e}") });
        }
    }
}
