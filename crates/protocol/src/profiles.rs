//! Model profiles on the wire (13.9+, cap [`CAP_PROFILES`]): the saved models, which role
//! uses which, and what the engine found out about each.
//!
//! The engine sends [`crate::ServerMsg::Profiles`] after the Welcome and whenever a profile,
//! a role or a probe changes. Keys never leave the engine: a profile comes with
//! [`ProfileView::has_key`] only, and a key a client sends is write-only.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use byob_types::profile::{
    Effort, MAX_DESCRIPTION_CHARS, MAX_ID_CHARS, MAX_NAME_CHARS, PROTECTED_FIELDS, Params, Profile, ROLE_MAIN, ROLE_SUBAGENTS, ROLE_UTILITY, ROLE_VISION,
    ROLES, id_ok, role_ok,
};

use crate::{ClientMsg, EndpointInfo, InferenceTest, Link};

/// Cap: the peer handles profile messages.
pub const CAP_PROFILES: &str = "profiles";

/// One profile as clients see it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfileView {
    pub profile: Profile,
    /// The engine stores a key for it (the key itself never leaves the engine).
    #[serde(default)]
    pub has_key: bool,
    /// How its endpoint did last (a test, a reply, the engine's own check).
    #[serde(default)]
    pub link: Link,
    /// What the probe found about its endpoint, if it ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info: Option<EndpointInfo>,
}

/// Every profile (sorted by name) and the role assignments (role → profile id).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProfileList {
    #[serde(default)]
    pub items: Vec<ProfileView>,
    #[serde(default)]
    pub roles: BTreeMap<String, String>,
}

impl ProfileList {
    pub fn get(&self, id: &str) -> Option<&ProfileView> {
        self.items.iter().find(|p| p.profile.id == id)
    }

    /// The profile of `role`, if it has one that exists.
    pub fn role(&self, role: &str) -> Option<&ProfileView> {
        self.roles.get(role).and_then(|id| self.get(id))
    }

    /// The roles `id` has.
    pub fn roles_of(&self, id: &str) -> Vec<&str> {
        self.roles.iter().filter(|(_, p)| *p == id).map(|(r, _)| r.as_str()).collect()
    }
}

/// The answer to [`ClientMsg::TestProfile`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProfileTest {
    pub id: String,
    pub test: InferenceTest,
}

/// Cheap checks of the profile messages' sizes (the engine checks the rest).
pub(crate) fn check_sizes(msg: &ClientMsg) -> Result<(), String> {
    let id = |s: &str| if id_ok(s) { Ok(()) } else { Err(format!("{:?} isn't a profile id", s.chars().take(80).collect::<String>())) };
    let key = |k: &Option<String>| match k {
        Some(k) if k.chars().count() > crate::connect::MAX_KEY_CHARS => Err("the API key is too long".to_owned()),
        _ => Ok(()),
    };
    match msg {
        ClientMsg::CreateProfile { profile, api_key, .. } | ClientMsg::UpdateProfile { profile, api_key } => {
            key(api_key)?;
            if matches!(msg, ClientMsg::UpdateProfile { .. }) {
                id(&profile.id)?;
            }
            profile.clone().validated().map(|_| ())
        }
        ClientMsg::DeleteProfile { id: i } | ClientMsg::DuplicateProfile { id: i } | ClientMsg::TestProfile { id: i } => id(i),
        ClientMsg::SetRole { role, profile } => {
            if !role_ok(role) {
                return Err("bad role name".into());
            }
            profile.as_deref().map_or(Ok(()), id)
        }
        ClientMsg::SetLevelProfile { profile, scope } => {
            if let crate::ConfigScope::Workspace { id: w } = scope
                && (w.is_empty() || w.chars().count() > crate::workspace::MAX_ID_CHARS)
            {
                return Err("bad id".into());
            }
            profile.as_deref().map_or(Ok(()), id)
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ConfigScope, ServerMsg};

    fn p() -> Profile {
        Profile { id: "qwen".into(), name: "Qwen".into(), base_url: "http://h/v1".into(), model: "qwen3".into(), ..Profile::default() }
    }

    #[test]
    fn profile_messages_round_trip_and_are_checked() {
        let msgs = vec![
            ClientMsg::GetProfiles,
            ClientMsg::CreateProfile { profile: p(), api_key: Some("sk".into()), assign: Some(ConfigScope::Chat), make_main: true },
            ClientMsg::UpdateProfile { profile: p(), api_key: None },
            ClientMsg::DeleteProfile { id: "qwen".into() },
            ClientMsg::DuplicateProfile { id: "qwen".into() },
            ClientMsg::TestProfile { id: "qwen".into() },
            ClientMsg::SetRole { role: "vision".into(), profile: Some("qwen".into()) },
            ClientMsg::SetLevelProfile { scope: ConfigScope::Workspace { id: "w1".into() }, profile: None },
        ];
        for m in msgs {
            assert_eq!(ClientMsg::from_json(&m.to_json()).unwrap(), m, "{}", m.to_json());
            assert!(m.check_sizes().is_ok(), "{}", m.to_json());
        }
        // A minimal create from a client.
        let m = ClientMsg::from_json(r#"{"type":"create_profile","profile":{"base_url":"http://h/v1","model":"m"}}"#).unwrap();
        assert!(matches!(m, ClientMsg::CreateProfile { assign: None, make_main: false, .. }));
        assert!(ClientMsg::DeleteProfile { id: "../x".into() }.check_sizes().is_err());
        assert!(
            ClientMsg::CreateProfile { profile: Profile { base_url: "ftp://x".into(), ..p() }, api_key: None, assign: None, make_main: false }
                .check_sizes()
                .is_err()
        );
        assert!(ClientMsg::UpdateProfile { profile: p(), api_key: Some("k".repeat(crate::connect::MAX_KEY_CHARS + 1)) }.check_sizes().is_err());
        assert!(ClientMsg::SetRole { role: " ".into(), profile: None }.check_sizes().is_err());
    }

    #[test]
    fn profile_lists_carry_no_keys() {
        let list = ProfileList {
            items: vec![ProfileView { profile: p(), has_key: true, link: Link::Ok, info: None }],
            roles: [("main".to_owned(), "qwen".to_owned())].into(),
        };
        let json = ServerMsg::Profiles(Box::new(list.clone())).to_json();
        assert!(!json.contains("api_key") && json.contains("\"has_key\":true"), "{json}");
        assert_eq!(ServerMsg::from_json(&json).unwrap(), ServerMsg::Profiles(Box::new(list.clone())));
        assert_eq!(list.role("main").map(|v| v.profile.name.as_str()), Some("Qwen"));
        assert_eq!(list.roles_of("qwen"), ["main"]);
        let t = ServerMsg::ProfileTested(ProfileTest { id: "qwen".into(), test: InferenceTest::default() });
        assert_eq!(ServerMsg::from_json(&t.to_json()).unwrap(), t);
    }
}
