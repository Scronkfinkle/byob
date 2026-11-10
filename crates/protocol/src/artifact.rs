//! Artifacts: files the model makes that live beside the conversation — a markdown report,
//! a single-file HTML app, a chart, an image, any other text file — and that are versioned
//! and exportable.
//!
//! An artifact's kind follows its file name ([`ArtifactKind::for_filename`]). Every version
//! is a separate file in the chat's workspace (normally `.byob/artifacts/<id>/v<n>.<ext>`;
//! charts and images that tools made elsewhere are adopted where they are). Steps that made
//! or changed one carry an [`ArtifactRef`]; clients get the chat's artifacts in
//! [`crate::ServerMsg::Artifacts`] and fetch a version's bytes with
//! [`crate::ClientMsg::GetArtifact`] (for display and export).

use serde::{Deserialize, Serialize};

/// Largest artifact version sent to a client in one [`crate::ServerMsg::ArtifactFile`]
/// (base64 makes it 4/3 bigger, still well inside a frame).
pub const MAX_ARTIFACT_SEND_BYTES: usize = 2 * 1024 * 1024;

/// What an artifact is, which decides how it is shown.
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
    /// The kind of a file, by its name. Every name is something: unknown extensions are text.
    pub fn for_filename(name: &str) -> Self {
        let lower = name.to_ascii_lowercase();
        let ends = |exts: &[&str]| exts.iter().any(|e| lower.ends_with(e));
        if ends(&[".chart.json"]) {
            Self::Chart
        } else if ends(&[".md", ".markdown"]) {
            Self::Markdown
        } else if ends(&[".html", ".htm"]) {
            Self::Html
        } else if ends(&[".png", ".jpg", ".jpeg"]) {
            Self::Image
        } else {
            Self::Text
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Markdown => "document",
            Self::Html => "app",
            Self::Chart => "chart",
            Self::Image => "image",
            Self::Text => "file",
        }
    }

    /// Content type for serving or downloading a version.
    pub fn mime(self, filename: &str) -> &'static str {
        match self {
            Self::Markdown => "text/markdown; charset=utf-8",
            Self::Html => "text/html; charset=utf-8",
            Self::Chart => "application/json",
            Self::Image if filename.to_ascii_lowercase().ends_with(".png") => "image/png",
            Self::Image => "image/jpeg",
            Self::Text => "text/plain; charset=utf-8",
        }
    }

    /// Shown as text (the bytes are UTF-8 source).
    pub fn is_text(self) -> bool {
        !matches!(self, Self::Image)
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
    pub kind: ArtifactKind,
    pub title: String,
    /// Name to export it as, e.g. `loan-calculator.html`.
    pub filename: String,
    /// Oldest first; never empty.
    pub versions: Vec<ArtifactVersion>,
}

impl Artifact {
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
    fn kinds_follow_the_file_name() {
        assert_eq!(ArtifactKind::for_filename("Sales.chart.json"), ArtifactKind::Chart);
        assert_eq!(ArtifactKind::for_filename("data.json"), ArtifactKind::Text);
        assert_eq!(ArtifactKind::for_filename("summary.MD"), ArtifactKind::Markdown);
        assert_eq!(ArtifactKind::for_filename("calc.html"), ArtifactKind::Html);
        assert_eq!(ArtifactKind::for_filename("plot.jpeg"), ArtifactKind::Image);
        assert_eq!(ArtifactKind::for_filename("script.py"), ArtifactKind::Text);
        assert_eq!(ArtifactKind::for_filename("noext"), ArtifactKind::Text);
        assert_eq!(ArtifactKind::Image.mime("a.png"), "image/png");
        assert!(!ArtifactKind::Image.is_text() && ArtifactKind::Html.is_text());
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
        let a = Artifact { id: "a".into(), kind: ArtifactKind::Html, title: "A".into(), filename: "a.html".into(), versions: vec![v(1), v(3), v(2)] };
        assert_eq!(a.latest().map(|v| v.n), Some(3));
        assert_eq!(a.version(2).map(|v| v.path.as_str()), Some("v2"));
        assert!(a.version(9).is_none());
    }
}
