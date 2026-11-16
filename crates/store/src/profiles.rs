//! Model profiles in `settings.json` / `secrets.json`, and staying compatible with builds
//! before them (0.3.x).
//!
//! - `settings.json` `profiles.<id>` holds each [`Profile`], `roles.<role>` the profile id a
//!   role uses; `secrets.json` `profile_keys.<id>` the keys.
//! - **Copies for older builds** ([`mirror`]): `inference.{base_url,model}` and `api_key`
//!   always mirror the `main` profile, `models.<role>` and `model_keys.<role>` every other
//!   role. A level that picks a profile also keeps its `base_url`/`model` (and scoped key),
//!   written by the engine. A build without profiles reads those and works as before.
//! - **Migration** ([`migrate`]), on every load and idempotent: the main connection, each
//!   `models.<role>`, and (in the engine) each workspace or chat with its own endpoint
//!   become profiles, deduplicated by URL + model + key. When an older build changed one of
//!   the copies, the copy wins: a profile matching it is found or made and the role points
//!   there. Nothing is deleted.

use std::collections::BTreeMap;

use byob_types::config::{DEFAULT_BASE_URL, DEFAULT_MODEL};
use byob_types::profile::{self, Profile, ROLE_MAIN};

use crate::Paths;
use crate::error::Result;
use crate::settings::{ModelRole, Secrets, Settings};

fn text(v: Option<&String>) -> Option<&str> {
    v.map(|s| s.trim()).filter(|s| !s.is_empty())
}

/// `http://127.0.0.1:8080/v1` → `127.0.0.1:8080`.
fn host(url: &str) -> &str {
    let u = url.trim();
    let u = u.split_once("://").map_or(u, |(_, rest)| rest);
    u.split(['/', '?', '#']).next().unwrap_or(u)
}

/// The profile for this endpoint and key: an existing one that matches exactly, else a new
/// one named after the model (and its server, if the name is taken). Returns its id.
pub fn ensure(
    profiles: &mut BTreeMap<String, Profile>,
    keys: &mut BTreeMap<String, String>,
    base_url: &str,
    model: &str,
    key: Option<&str>,
    description: &str,
) -> String {
    let key = key.map(str::trim).filter(|k| !k.is_empty());
    if let Some(p) = profiles.values().find(|p| p.points_at(base_url, model) && text(keys.get(&p.id)) == key) {
        return p.id.clone();
    }
    let model = model.trim();
    let taken_name = profiles.values().any(|p| p.label() == model);
    let name = if taken_name { format!("{model} · {}", host(base_url)) } else { model.to_owned() };
    let id = profile::new_id(&name, |id| profiles.contains_key(id));
    let p = Profile {
        id: id.clone(),
        name,
        base_url: base_url.trim().to_owned(),
        model: model.to_owned(),
        description: description.to_owned(),
        ..Profile::default()
    };
    profiles.insert(id.clone(), p);
    if let Some(k) = key {
        keys.insert(id.clone(), k.to_owned());
    }
    id
}

/// Save profiles, roles and their keys, with the copies older builds read ([`mirror`]).
pub fn save(paths: &Paths, profiles: &BTreeMap<String, Profile>, roles: &BTreeMap<String, String>, keys: &BTreeMap<String, String>) -> Result<()> {
    let mut s = Settings::load(&paths.settings()).value;
    let mut k = Secrets::load(&paths.secrets()).value;
    s.profiles.clone_from(profiles);
    s.roles.clone_from(roles);
    k.profile_keys.clone_from(keys);
    mirror(&mut s, &mut k);
    // Keys first: settings that name a profile whose key isn't saved would lose it.
    k.save(&paths.secrets())?;
    s.save(&paths.settings())
}

/// [`migrate`] the files in `paths`, saving them if anything changed.
pub fn migrate_files(paths: &Paths) -> Result<bool> {
    let mut s = Settings::load(&paths.settings()).value;
    let mut k = Secrets::load(&paths.secrets()).value;
    if !migrate(&mut s, &mut k) {
        return Ok(false);
    }
    paths.ensure()?;
    k.save(&paths.secrets())?;
    s.save(&paths.settings())?;
    Ok(true)
}

/// Does role `role` point at a usable profile for this endpoint, with one of these keys?
fn role_matches(settings: &Settings, secrets: &Secrets, role: &str, base_url: &str, model: &str, keys: &[Option<&str>]) -> bool {
    let p = profile::find(&settings.profiles, settings.roles.get(role).map(String::as_str));
    p.is_some_and(|p| p.points_at(base_url, model) && keys.iter().any(|k| text(secrets.profile_keys.get(&p.id)) == k.map(str::trim).filter(|k| !k.is_empty())))
}

/// The main connection as settings.json has it (built-in defaults filling gaps), if the
/// user ever set one.
fn main_endpoint(settings: &Settings) -> Option<(String, String)> {
    let (url, model) = (text(settings.inference.base_url.as_ref()), text(settings.inference.model.as_ref()));
    if url.is_none() && model.is_none() {
        return None;
    }
    Some((url.unwrap_or(DEFAULT_BASE_URL).to_owned(), model.unwrap_or(DEFAULT_MODEL).to_owned()))
}

/// Bring profiles up to date with the copies older builds read and write (see the module
/// docs): the main connection and each `models.<role>` get a profile and a role. True if
/// anything changed (then both files are worth saving). Running it again changes nothing.
pub fn migrate(settings: &mut Settings, secrets: &mut Secrets) -> bool {
    let before = (settings.profiles.clone(), settings.roles.clone(), secrets.profile_keys.clone());
    let main = main_endpoint(settings);
    let main_key = text(secrets.api_key.as_ref()).map(str::to_owned);
    if let Some((url, model)) = &main
        && !role_matches(settings, secrets, ROLE_MAIN, url, model, &[main_key.as_deref()])
    {
        let id = ensure(&mut settings.profiles, &mut secrets.profile_keys, url, model, main_key.as_deref(), "");
        settings.roles.insert(ROLE_MAIN.to_owned(), id);
    }
    let (main_url, main_model) = main.unwrap_or_else(|| (DEFAULT_BASE_URL.to_owned(), DEFAULT_MODEL.to_owned()));
    let roles: Vec<(String, ModelRole)> = settings.models.iter().map(|(n, r)| (n.clone(), r.clone())).collect();
    for (name, role) in roles {
        if !profile::role_ok(&name) || name == ROLE_MAIN {
            continue;
        }
        // As older builds resolve a role: gaps from the main connection, the main key on
        // the main server.
        let url = text(role.base_url.as_ref()).unwrap_or(&main_url).to_owned();
        let model = text(role.model.as_ref()).unwrap_or(&main_model).to_owned();
        let same_server = profile::same_url(&url, &main_url);
        let own = text(secrets.model_keys.get(&name)).map(str::to_owned);
        let key = own.clone().or_else(|| main_key.clone().filter(|_| same_server));
        // Older builds can't say "no key" for a role on the main server (they fall back to
        // the main key), so a keyless profile there matches too.
        let fits = if own.is_none() && same_server { vec![key.as_deref(), None] } else { vec![key.as_deref()] };
        if !role_matches(settings, secrets, &name, &url, &model, &fits) {
            let id = ensure(&mut settings.profiles, &mut secrets.profile_keys, &url, &model, key.as_deref(), "");
            settings.roles.insert(name, id);
        }
    }
    (settings.profiles.clone(), settings.roles.clone(), secrets.profile_keys.clone()) != before
}

/// Write the copies older builds read: `inference` + `api_key` from the main profile,
/// `models.<role>` + `model_keys.<role>` from the other roles (a role that no longer exists
/// loses its copy). Roles pointing at a profile that is gone are dropped.
pub fn mirror(settings: &mut Settings, secrets: &mut Secrets) {
    let profiles = settings.profiles.clone();
    settings.roles.retain(|role, id| profile::role_ok(role) && profile::find(&profiles, Some(id)).is_some());
    if let Some(p) = profile::find(&profiles, settings.roles.get(ROLE_MAIN).map(String::as_str)) {
        settings.inference.base_url = Some(p.base_url.trim().to_owned());
        settings.inference.model = Some(p.model.trim().to_owned());
        secrets.api_key = text(secrets.profile_keys.get(&p.id)).map(str::to_owned);
    }
    let roles: BTreeMap<&String, &Profile> =
        settings.roles.iter().filter(|(r, _)| *r != ROLE_MAIN).filter_map(|(r, id)| profile::find(&profiles, Some(id)).map(|p| (r, p))).collect();
    settings.models.retain(|name, _| roles.contains_key(name));
    secrets.model_keys.retain(|name, _| roles.contains_key(name));
    for (role, p) in roles {
        let entry = settings.models.entry(role.clone()).or_default();
        entry.base_url = Some(p.base_url.trim().to_owned());
        entry.model = Some(p.model.trim().to_owned());
        match text(secrets.profile_keys.get(&p.id)) {
            Some(k) => {
                secrets.model_keys.insert(role.clone(), k.to_owned());
            }
            None => {
                secrets.model_keys.remove(role);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::InferenceSettings;

    fn old() -> (Settings, Secrets) {
        let mut s = Settings {
            inference: InferenceSettings { base_url: Some("http://main:8080/v1".into()), model: Some("big".into()), ..Default::default() },
            ..Default::default()
        };
        s.models.insert("fast".into(), ModelRole { model: Some("small".into()), ..Default::default() });
        s.models.insert("vision".into(), ModelRole { base_url: Some("http://vl/v1".into()), model: Some("qwen-vl".into()), ..Default::default() });
        // Same endpoint as main, but its own key: a profile of its own.
        s.models.insert("keyed".into(), ModelRole { base_url: Some("http://main:8080/v1".into()), model: Some("big".into()), ..Default::default() });
        let secrets = Secrets {
            api_key: Some("sk-main".into()),
            model_keys: [("keyed".to_owned(), "sk-other".to_owned()), ("vision".to_owned(), "sk-vl".to_owned())].into(),
            ..Default::default()
        };
        (s, secrets)
    }

    #[test]
    fn old_settings_become_profiles_once() {
        let (mut s, mut k) = old();
        assert!(migrate(&mut s, &mut k));
        let main = &s.profiles[&s.roles["main"]];
        assert_eq!((main.base_url.as_str(), main.model.as_str(), main.name.as_str()), ("http://main:8080/v1", "big", "big"));
        assert_eq!(k.profile_keys.get(&main.id).map(String::as_str), Some("sk-main"));
        // A role without a URL is on the main server, with the main key.
        let fast = &s.profiles[&s.roles["fast"]];
        assert_eq!((fast.base_url.as_str(), fast.model.as_str()), ("http://main:8080/v1", "small"));
        assert_eq!(k.profile_keys.get(&fast.id).map(String::as_str), Some("sk-main"));
        let vision = &s.profiles[&s.roles["vision"]];
        assert_eq!(k.profile_keys.get(&vision.id).map(String::as_str), Some("sk-vl"));
        let keyed = &s.profiles[&s.roles["keyed"]];
        assert_ne!(keyed.id, main.id, "another key is another profile");
        assert_eq!(keyed.name, "big · main:8080");
        assert_eq!(s.profiles.len(), 4);
        // Again: nothing changes.
        let (s1, k1) = (s.clone(), k.clone());
        assert!(!migrate(&mut s, &mut k));
        assert_eq!((s, k), (s1, k1));
    }

    #[test]
    fn identical_endpoints_share_a_profile_and_nothing_is_lost() {
        let (mut s, mut k) = old();
        s.models.insert("twin".into(), ModelRole { model: Some("small".into()), ..Default::default() });
        migrate(&mut s, &mut k);
        assert_eq!(s.roles["twin"], s.roles["fast"]);
        // The old copies are still all there.
        let (o, ok) = old();
        assert_eq!(s.inference.base_url, o.inference.base_url);
        assert!(o.models.keys().all(|m| s.models.contains_key(m)));
        assert_eq!((k.api_key.clone(), k.model_keys.clone()), (ok.api_key, ok.model_keys));
    }

    #[test]
    fn the_copies_older_builds_read_follow_the_profiles() {
        let (mut s, mut k) = old();
        migrate(&mut s, &mut k);
        // The user edits the main profile and drops a role, in this build.
        let main = s.roles["main"].clone();
        if let Some(p) = s.profiles.get_mut(&main) {
            p.base_url = "https://api.example.com/v1".into();
            p.model = "gpt".into();
        }
        k.profile_keys.insert(main.clone(), "sk-new".into());
        s.roles.remove("keyed");
        mirror(&mut s, &mut k);
        assert_eq!((s.inference.base_url.as_deref(), s.inference.model.as_deref()), (Some("https://api.example.com/v1"), Some("gpt")));
        assert_eq!(k.api_key.as_deref(), Some("sk-new"));
        assert!(!s.models.contains_key("keyed") && !k.model_keys.contains_key("keyed"));
        let fast = &s.models["fast"];
        assert_eq!((fast.base_url.as_deref(), fast.model.as_deref()), (Some("http://main:8080/v1"), Some("small")));
        assert_eq!(k.model_keys.get("fast").map(String::as_str), Some("sk-main"));
        // Mirrored settings load as profiles unchanged.
        assert!(!migrate(&mut s, &mut k));
        // A keyless profile on the main server can't be written for older builds (they use
        // the main key there); it still loads as itself.
        let id = ensure(&mut s.profiles, &mut k.profile_keys, "https://api.example.com/v1", "mini", None, "");
        s.roles.insert("vision".into(), id.clone());
        mirror(&mut s, &mut k);
        assert!(!k.model_keys.contains_key("vision"));
        assert!(!migrate(&mut s, &mut k));
        assert_eq!(s.roles["vision"], id);
    }

    #[test]
    fn a_change_made_by_an_older_build_wins() {
        let (mut s, mut k) = old();
        migrate(&mut s, &mut k);
        let first_main = s.roles["main"].clone();
        // 0.3.5 changes the connection (it only knows `inference` and `api_key`).
        s.inference.model = Some("other".into());
        k.api_key = None;
        assert!(migrate(&mut s, &mut k));
        let main = &s.profiles[&s.roles["main"]];
        assert_ne!(main.id, first_main);
        assert_eq!((main.model.as_str(), k.profile_keys.get(&main.id)), ("other", None));
        assert!(s.profiles.contains_key(&first_main), "the old profile stays");
        assert!(!migrate(&mut s, &mut k));
    }

    #[test]
    fn nothing_configured_means_no_profiles() {
        let (mut s, mut k) = (Settings::default(), Secrets::default());
        assert!(!migrate(&mut s, &mut k));
        assert!(s.profiles.is_empty() && s.roles.is_empty());
        mirror(&mut s, &mut k);
        assert_eq!(s, Settings::default());
    }
}
