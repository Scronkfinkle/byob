//! Open view payloads: something for a client to show that the protocol doesn't need to know.
//!
//! A [`View`] is a namespaced `kind` (`byob.plugin_card`, `<plugin>.<name>`), a schema
//! version `v` of its `data`, the `data` itself (any JSON) and a markdown `fallback`. A client
//! draws a kind it has a renderer for (and whose `v` it understands); any other view shows its
//! fallback. So a newer engine, or a plugin, can put new things on a step without a protocol
//! bump, and an older client still shows something sensible.
//!
//! Built-in views keep a typed struct as the `data` schema ([`ViewData`]): encode with
//! [`View::of`], decode with [`View::get`]. Bump a kind's `V` only for changes an older
//! renderer can't read (new optional fields don't need it); a renderer takes `v <= V`.
//!
//! `byob.*` kinds are byob's own.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Longest view kind.
pub const MAX_VIEW_KIND_CHARS: usize = 64;
/// Largest view `data` (its JSON).
pub const MAX_VIEW_DATA_BYTES: usize = 256 * 1024;
/// Longest fallback (characters).
pub const MAX_VIEW_FALLBACK_CHARS: usize = 16 * 1024;
/// Most views on one step.
pub const MAX_STEP_VIEWS: usize = 16;

/// Something to show, by kind; see the module docs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct View {
    pub kind: String,
    /// Schema version of `data` for this kind.
    #[serde(default)]
    pub v: u32,
    #[serde(default)]
    pub data: Value,
    /// Markdown shown by a client that can't draw this kind.
    #[serde(default)]
    pub fallback: String,
    /// A live view this is a snapshot of (13.3+): a client that can (cap
    /// [`crate::sites::CAP_VIEW_SITES`]) shows the live one and sends its events to the owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<crate::ViewRef>,
}

/// A typed view: the struct is the `data` schema of kind [`ViewData::KIND`].
pub trait ViewData: Serialize + DeserializeOwned {
    const KIND: &'static str;
    /// Current schema version.
    const V: u32;
    /// What a client without a renderer shows.
    fn fallback(&self) -> String;
}

/// A view kind is ASCII lowercase letters, digits, `_`, `-`, separated by dots, and starts
/// with a namespace: `byob.plugin_card`, `sheets.grid`.
pub fn kind_ok(kind: &str) -> bool {
    (3..=MAX_VIEW_KIND_CHARS).contains(&kind.len())
        && kind.contains('.')
        && kind.split('.').all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-')))
}

impl View {
    /// A typed view (unchecked: built-in data is small).
    pub fn of<T: ViewData>(data: &T) -> Self {
        Self { kind: T::KIND.to_owned(), v: T::V, data: serde_json::to_value(data).unwrap_or(Value::Null), fallback: data.fallback(), owner: None }
    }

    /// The typed data, if this view is a `T` this build can read.
    pub fn get<T: ViewData>(&self) -> Option<T> {
        (self.kind == T::KIND && self.v <= T::V).then(|| serde_json::from_value(self.data.clone()).ok()).flatten()
    }

    /// Check an untrusted view's kind and sizes.
    pub fn check(&self) -> Result<(), String> {
        if !kind_ok(&self.kind) {
            return Err(format!("bad view kind {:?}", self.kind.chars().take(MAX_VIEW_KIND_CHARS).collect::<String>()));
        }
        if self.fallback.chars().count() > MAX_VIEW_FALLBACK_CHARS {
            return Err(format!("a view's fallback can be at most {MAX_VIEW_FALLBACK_CHARS} characters"));
        }
        let size = serde_json::to_string(&self.data).map_or(usize::MAX, |s| s.len());
        if size > MAX_VIEW_DATA_BYTES {
            return Err(format!("view data too large ({size} bytes, at most {MAX_VIEW_DATA_BYTES})"));
        }
        Ok(())
    }

    /// The fallback, cut to [`MAX_VIEW_FALLBACK_CHARS`] (for clients drawing untrusted views).
    pub fn fallback_bounded(&self) -> &str {
        match self.fallback.char_indices().nth(MAX_VIEW_FALLBACK_CHARS) {
            Some((at, _)) => self.fallback.get(..at).unwrap_or_default(),
            None => &self.fallback,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Gauge {
        value: u32,
        #[serde(default)]
        label: String,
    }

    impl ViewData for Gauge {
        const KIND: &'static str = "test.gauge";
        const V: u32 = 2;
        fn fallback(&self) -> String {
            format!("**{}**: {}", self.label, self.value)
        }
    }

    #[test]
    fn typed_views_round_trip_and_other_kinds_or_versions_dont_decode() {
        let g = Gauge { value: 3, label: "load".into() };
        let v = View::of(&g);
        assert_eq!((v.kind.as_str(), v.v, v.fallback.as_str()), ("test.gauge", 2, "**load**: 3"));
        assert!(v.check().is_ok());
        let back: View = serde_json::from_str(&serde_json::to_string(&v).unwrap()).unwrap();
        assert_eq!(back.get::<Gauge>(), Some(g));
        // A newer schema than this build knows: fallback, not a misread.
        assert_eq!(View { v: 3, ..v.clone() }.get::<Gauge>(), None);
        assert_eq!(View { kind: "test.other".into(), ..v.clone() }.get::<Gauge>(), None);
        // Data that doesn't fit the schema doesn't decode.
        assert_eq!(View { data: serde_json::json!({"value": "x"}), ..v }.get::<Gauge>(), None);
        // A minimal view from anyone parses.
        let min: View = serde_json::from_str(r#"{"kind":"x.y"}"#).unwrap();
        assert_eq!((min.v, min.data.is_null(), min.fallback.is_empty()), (0, true, true));
    }

    #[test]
    fn kinds_and_sizes_are_checked() {
        for ok in ["byob.plugin_card", "sheets.grid", "a.b-c.d_2"] {
            assert!(kind_ok(ok), "{ok}");
        }
        for bad in ["", "plain", ".x", "x.", "a..b", "Byob.card", "a.b c", &format!("a.{}", "x".repeat(MAX_VIEW_KIND_CHARS))] {
            assert!(!kind_ok(bad), "{bad}");
        }
        let v = |data: Value, fallback: String| View { kind: "x.y".into(), v: 0, data, fallback, owner: None };
        assert!(v(Value::String("x".repeat(MAX_VIEW_DATA_BYTES)), String::new()).check().is_err());
        let long = "é".repeat(MAX_VIEW_FALLBACK_CHARS + 5);
        let big = v(Value::Null, long);
        assert!(big.check().is_err());
        assert_eq!(big.fallback_bounded().chars().count(), MAX_VIEW_FALLBACK_CHARS);
    }
}
