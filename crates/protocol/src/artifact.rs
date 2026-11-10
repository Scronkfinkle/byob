//! Artifacts: files the model makes that live beside the conversation — a markdown report,
//! a single-file HTML app, a chart, an image, any other text file — and that are versioned
//! and exportable.
//!
//! What an artifact is, is an open **content type** ([`Artifact::content_type`], e.g.
//! `text/markdown`, `text/html`, [`TYPE_CHART`], `image/png`, `text/plain`), which the
//! engine resolves from the file name through its registry of artifact types (built-ins and,
//! later, plugins' registrations). Clients pick a renderer by it and fall back to showing the
//! text source for types they don't know. [`ArtifactKind`] is the closed set older clients
//! understand; the engine still sends it (unknown types as [`ArtifactKind::Text`]) and a
//! client of an older engine derives the type from it.
//!
//! Every version is a separate file in the chat's workspace (normally
//! `.byob/artifacts/<id>/v<n>.<ext>`; charts and images that tools made elsewhere are
//! adopted where they are). Steps that made or changed one carry an [`ArtifactRef`]; clients
//! get the chat's artifacts in [`crate::ServerMsg::Artifacts`] and fetch a version's bytes
//! with [`crate::ClientMsg::GetArtifact`] (for display and export).

use serde::{Deserialize, Serialize};

/// Largest artifact version sent to a client in one [`crate::ServerMsg::ArtifactFile`]
/// (base64 makes it 4/3 bigger, still well inside a frame).
pub const MAX_ARTIFACT_SEND_BYTES: usize = 2 * 1024 * 1024;

/// Content types of byob's built-in artifact types.
pub const TYPE_MARKDOWN: &str = "text/markdown";
pub const TYPE_HTML: &str = "text/html";
pub const TYPE_CHART: &str = "application/vnd.byob.chart+json";
pub const TYPE_PNG: &str = "image/png";
pub const TYPE_JPEG: &str = "image/jpeg";
/// Any other text file, and the fallback for types nobody registered.
pub const TYPE_TEXT: &str = "text/plain";

/// Longest content type.
pub const MAX_CONTENT_TYPE_CHARS: usize = 100;

/// A content type is `type/subtype`, lowercase ASCII letters, digits and `.+-_` (no
/// parameters): a lookup key, safe in a header.
pub fn valid_content_type(t: &str) -> bool {
    let ok = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'+' | b'-' | b'_'));
    t.len() <= MAX_CONTENT_TYPE_CHARS && t.split_once('/').is_some_and(|(a, b)| ok(a) && ok(b))
}

/// The `Content-Type` to serve or save a version of `content_type` with: text gets a
/// charset, structured JSON (`…+json`) is plain `application/json`.
pub fn mime(content_type: &str) -> String {
    if !valid_content_type(content_type) {
        "application/octet-stream".to_owned()
    } else if content_type.starts_with("text/") {
        format!("{content_type}; charset=utf-8")
    } else if content_type.ends_with("+json") {
        "application/json".to_owned()
    } else {
        content_type.to_owned()
    }
}

/// What an artifact is, as clients of protocol 13.0 knew it (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// `.md`: rendered, with a source view.
    Markdown,
    /// `.html`: a single-file app (HTML + CSS + JS), run sandboxed.
    Html,
    /// `.chart.json`: drawn natively.
    Chart,
    /// PNG or JPEG.
    Image,
    /// Any other text file (csv, json, py, svg source…), shown as source. Also any kind a
    /// newer engine sends that this client doesn't know.
    #[serde(other)]
    Text,
}

impl ArtifactKind {
    /// The content type this kind stands for (`filename` tells PNG from JPEG).
    pub fn content_type(self, filename: &str) -> &'static str {
        match self {
            Self::Markdown => TYPE_MARKDOWN,
            Self::Html => TYPE_HTML,
            Self::Chart => TYPE_CHART,
            Self::Image if filename.to_ascii_lowercase().ends_with(".png") => TYPE_PNG,
            Self::Image => TYPE_JPEG,
            Self::Text => TYPE_TEXT,
        }
    }
}

/// One saved version of an artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactVersion {
    /// 1, 2, 3… (never reused).
    pub n: u32,
    /// Workspace-relative path of this version's file.
    pub path: String,
    /// Milliseconds since the Unix epoch.
    pub created_at: i64,
    /// What changed, in the model's words ("button is blue now").
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    /// `[a-z0-9-]`, unique in the workspace.
    pub id: String,
    /// For clients of protocol 13.0; [`Artifact::content_type`] is what it is.
    pub kind: ArtifactKind,
    /// What it is (13.1+; empty from older engines and in older index files: then
    /// [`Artifact::content_type`] derives it from `kind`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub content_type: String,
    pub title: String,
    /// Name to export it as, e.g. `loan-calculator.html`.
    pub filename: String,
    /// Oldest first; never empty.
    pub versions: Vec<ArtifactVersion>,
}

impl Artifact {
    /// Its content type: the one the engine sent if it is well-formed, else the one its
    /// [`ArtifactKind`] stands for.
    pub fn content_type(&self) -> &str {
        if valid_content_type(&self.content_type) { &self.content_type } else { self.kind.content_type(&self.filename) }
    }

    pub fn latest(&self) -> Option<&ArtifactVersion> {
        self.versions.iter().max_by_key(|v| v.n)
    }

    pub fn version(&self, n: u32) -> Option<&ArtifactVersion> {
        self.versions.iter().find(|v| v.n == n)
    }
}

/// "This step made version `version` of artifact `id`."
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub id: String,
    pub version: u32,
}

/// An artifact id is safe to use as a folder name and a URL segment.
pub fn valid_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') && !id.starts_with('-')
}

/// The theme an HTML app is given, as CSS custom properties (`--byob-bg`, …). Apps are told
/// to style themselves with these so they match the window. Additive: names are only ever
/// appended (the artifact helper accepts lowercase letters and dashes only, so no digits).
pub const THEME_VARS: [&str; 26] = [
    "--byob-bg",
    "--byob-surface",
    "--byob-fg",
    "--byob-muted",
    "--byob-accent",
    "--byob-on-accent",
    "--byob-border",
    "--byob-danger",
    "--byob-success",
    "--byob-warning",
    "--byob-link",
    "--byob-strong",
    "--byob-faint",
    "--byob-panel",
    "--byob-field",
    "--byob-code-bg",
    "--byob-code-fg",
    "--byob-radius",
    "--byob-chart-a",
    "--byob-chart-b",
    "--byob-chart-c",
    "--byob-chart-d",
    "--byob-chart-e",
    "--byob-chart-f",
    "--byob-chart-g",
    "--byob-chart-h",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_types_and_their_fallbacks() {
        let a = |kind, content_type: &str, filename: &str| Artifact {
            id: "a".into(),
            kind,
            content_type: content_type.into(),
            title: "A".into(),
            filename: filename.into(),
            versions: vec![],
        };
        // From a 13.1 engine: the type it sent, whatever the kind says.
        assert_eq!(a(ArtifactKind::Text, "application/vnd.acme.sheet+json", "x.sheet").content_type(), "application/vnd.acme.sheet+json");
        // From an older engine (or index file): derived from the kind.
        assert_eq!(a(ArtifactKind::Image, "", "p.PNG").content_type(), TYPE_PNG);
        assert_eq!(a(ArtifactKind::Image, "", "p.jpg").content_type(), TYPE_JPEG);
        assert_eq!(a(ArtifactKind::Chart, "", "s.chart.json").content_type(), TYPE_CHART);
        // Garbage is ignored.
        assert_eq!(a(ArtifactKind::Html, "Text/HTML; x=<y>", "a.html").content_type(), TYPE_HTML);
        assert!(valid_content_type("text/x-python") && !valid_content_type("text") && !valid_content_type("/x"));
        assert!(!valid_content_type(&format!("a/{}", "b".repeat(100))));
        assert_eq!(mime(TYPE_MARKDOWN), "text/markdown; charset=utf-8");
        assert_eq!(mime(TYPE_CHART), "application/json");
        assert_eq!(mime(TYPE_PNG), "image/png");
        assert_eq!(mime("bad"), "application/octet-stream");
        // Not sent when empty; old JSON without it parses.
        let json = serde_json::to_string(&a(ArtifactKind::Text, "", "n")).unwrap();
        assert!(!json.contains("content_type"));
        let back: Artifact = serde_json::from_str(&json).unwrap();
        assert_eq!(back.content_type(), TYPE_TEXT);
    }

    #[test]
    fn ids() {
        assert!(valid_id("loan-calculator-3f2a"));
        for bad in ["", "-x", "A", "a/b", "..", "a b", &"x".repeat(65)] {
            assert!(!valid_id(bad), "{bad}");
        }
    }

    #[test]
    fn latest_and_lookup() {
        let v = |n| ArtifactVersion { n, path: format!("v{n}"), created_at: 0, note: String::new(), size: 1 };
        let a = Artifact {
            id: "a".into(),
            kind: ArtifactKind::Html,
            content_type: TYPE_HTML.into(),
            title: "A".into(),
            filename: "a.html".into(),
            versions: vec![v(1), v(3), v(2)],
        };
        assert_eq!(a.latest().map(|v| v.n), Some(3));
        assert_eq!(a.version(2).map(|v| v.path.as_str()), Some("v2"));
        assert!(a.version(9).is_none());
    }
}
