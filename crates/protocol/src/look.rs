//! A window's look on the wire (13.12+, cap [`CAP_LOOK`]): its theme, accent, corners, text
//! size, bubbles and colors, which each device keeps for itself.
//!
//! The client is the authority on its own look: it sends [`crate::ClientMsg::ReportLook`]
//! once connected and after every change, listing each setting with its value and what it
//! may be. The engine keeps the latest report of each client and offers them to the model
//! (the `ui_look` tool); a change the model asks for goes to the windows showing the chat as
//! [`crate::ServerMsg::SetLook`], and each answers with a fresh report naming the request
//! (`answered`) and anything it couldn't apply (`problems`). Another kind of client can
//! report settings of its own: the engine only passes them on.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Cap: the peer reports and applies looks.
pub const CAP_LOOK: &str = "look";

/// Most settings in a report, and the longest key, description and value text.
pub const MAX_FIELDS: usize = 128;
pub const MAX_KEY: usize = 64;
pub const MAX_DESCRIBE: usize = 400;
pub const MAX_THEMES: usize = 200;
pub const MAX_PROBLEMS: usize = 32;
/// Most notes in a report, and the longest note.
pub const MAX_NOTES: usize = 8;
pub const MAX_NOTE: usize = 20 * 1024;

/// One setting of a window's look.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LookField {
    /// Its key in [`crate::ServerMsg::SetLook`] (`accent`, `corners`, `color.app_bg`, …).
    pub key: String,
    /// What it is now (a string, number or bool).
    pub value: Value,
    /// What it is and what it may be, for the model.
    pub describe: String,
}

/// A theme a window can use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThemeChoice {
    pub id: String,
    pub name: String,
    pub dark: bool,
}

/// A window's look: [`crate::ClientMsg::ReportLook`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LookReport {
    pub fields: Vec<LookField>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub themes: Vec<ThemeChoice>,
    /// The [`crate::ServerMsg::SetLook`] this report answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered: Option<u64>,
    /// What of that request couldn't be applied, and why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
    /// Longer texts for the model, by name: how to write a setting's value (a backdrop
    /// shader's guide), or a value too long for a field (the backdrop's source).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub notes: BTreeMap<String, String>,
}

impl LookReport {
    /// Cut what a client sent to size.
    #[must_use]
    pub fn sanitized(mut self) -> Self {
        let cut = |s: &mut String, n: usize| {
            if s.len() > n {
                let mut end = n;
                while !s.is_char_boundary(end) {
                    end -= 1;
                }
                s.truncate(end);
            }
        };
        self.fields.truncate(MAX_FIELDS);
        self.fields.retain(|f| !f.key.is_empty() && f.key.len() <= MAX_KEY && !matches!(f.value, Value::Object(_) | Value::Array(_)));
        for f in &mut self.fields {
            cut(&mut f.describe, MAX_DESCRIBE);
            if let Value::String(s) = &mut f.value {
                cut(s, MAX_DESCRIBE);
            }
        }
        self.themes.truncate(MAX_THEMES);
        for t in &mut self.themes {
            cut(&mut t.id, MAX_KEY * 2);
            cut(&mut t.name, MAX_KEY * 2);
        }
        self.problems.truncate(MAX_PROBLEMS);
        for p in &mut self.problems {
            cut(p, MAX_DESCRIBE);
        }
        let keys: Vec<String> = self.notes.keys().skip(MAX_NOTES).cloned().collect();
        for k in keys {
            self.notes.remove(&k);
        }
        self.notes.retain(|k, _| !k.is_empty() && k.len() <= MAX_KEY);
        for n in self.notes.values_mut() {
            cut(n, MAX_NOTE);
        }
        self
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.fields.iter().find(|f| f.key == key).map(|f| &f.value)
    }

    /// What differs from `before`: (key, before, now).
    pub fn changes_since(&self, before: &Self) -> Vec<(String, Value, Value)> {
        self.fields
            .iter()
            .filter_map(|f| {
                let old = before.get(&f.key).cloned().unwrap_or(Value::Null);
                (old != f.value).then(|| (f.key.clone(), old, f.value.clone()))
            })
            .collect()
    }
}

/// Changes to a look by key: [`crate::ServerMsg::SetLook`].
pub type LookChanges = BTreeMap<String, Value>;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn field(key: &str, value: Value) -> LookField {
        LookField { key: key.into(), value, describe: String::new() }
    }

    #[test]
    fn reports_are_cut_to_size_and_diffed() {
        let mut r =
            LookReport { fields: vec![field("accent", json!("theme")), field("corners", json!(8)), field("bad", json!({"a": 1}))], ..Default::default() };
        r.fields.push(LookField { key: "x".repeat(MAX_KEY + 1), value: json!(1), describe: String::new() });
        r.problems = vec!["é".repeat(MAX_DESCRIBE)];
        r.notes = (0..MAX_NOTES + 2).map(|i| (format!("n{i:02}"), "w".repeat(MAX_NOTE + 1))).collect();
        let r = r.sanitized();
        assert_eq!(r.fields.len(), 2);
        assert!(r.problems[0].len() <= MAX_DESCRIBE);
        assert_eq!(r.notes.len(), MAX_NOTES);
        assert!(r.notes.values().all(|n| n.len() == MAX_NOTE));
        let mut now = r.clone();
        now.fields[1].value = json!(12);
        assert_eq!(now.changes_since(&r), [("corners".to_owned(), json!(8), json!(12))]);
    }
}
