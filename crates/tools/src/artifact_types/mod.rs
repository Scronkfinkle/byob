//! Artifact types: what an artifact is (an open content type, see
//! [`byob_protocol::artifact`]) and what the engine does with it, as registrations.
//!
//! An [`ArtifactType`] says which file names are of the type (suffixes, longest match wins),
//! how it is named and described to the model, and its [`ArtifactHooks`]: checking content
//! before it is saved, the body served for viewing, making a too-big version fit a client.
//! The [`ArtifactTypes`] registry is shared by the artifact tools, the engine (prompt) and
//! the server; byob's own types register through it like anyone else's will
//! ([`ArtifactTypes::builtin`]): plain text (also the fallback for unknown types), markdown,
//! HTML apps, charts and images, each in its own module. A file name nothing matches is
//! plain text.

use std::sync::{Arc, PoisonError, RwLock};

use byob_protocol::artifact::TYPE_TEXT;
use byob_protocol::{Artifact, ArtifactKind};

pub mod chart;
pub mod html_app;
pub mod image;
pub mod markdown;
pub mod text;

/// What the engine does with an artifact type's content. Every method has a default that
/// suits plain text.
pub trait ArtifactHooks: Send + Sync {
    /// Check content `create_artifact` / `update_artifact` are about to save (already known
    /// to be non-empty and within the size limit), maybe normalise it. Returns the text to
    /// save and a note for the model (appended to the tool's result).
    fn check(&self, content: &str) -> Result<(String, String), String> {
        Ok((content.to_owned(), String::new()))
    }

    /// The body served to view a version (`/artifact/…` without `?download`, and what the
    /// desktop's app renderer loads). Default: the bytes as saved.
    fn serve(&self, bytes: Vec<u8>) -> Vec<u8> {
        bytes
    }

    /// A version is bigger than `max` bytes, too big to send a client in one message: make
    /// it fit, or say why not.
    fn shrink_for_client(&self, bytes: Vec<u8>, max: usize) -> Result<Vec<u8>, String> {
        let _ = max;
        Err(format!("too big to show here ({}); it's in the workspace", byob_protocol::media::size_label(bytes.len() as u64)))
    }
}

/// Hooks that keep every default (plain text).
pub struct Plain;

impl ArtifactHooks for Plain {}

/// One artifact type. Build with [`ArtifactType::new`] and the setters.
#[derive(Clone)]
pub struct ArtifactType {
    /// E.g. `text/markdown` (see [`byob_protocol::artifact::valid_content_type`]).
    pub content_type: String,
    /// File name endings (lowercase, with the dot, e.g. `.chart.json`) of this type.
    pub suffixes: Vec<String>,
    /// One word for it ("document", "app"), in tool results and the prompt's artifact list.
    pub label: String,
    /// What clients of protocol 13.0 are told it is (unknown types: text).
    pub kind: ArtifactKind,
    /// Its versions are text: `read_artifact` reads them and `update_artifact` edits them.
    pub text: bool,
    /// How `create_artifact`'s description and its file-name error name it, e.g. ".md
    /// (document, report, summary)". Empty: not something the model writes.
    pub describe: String,
    /// Its clause in the prompt's "pick the format that fits" list, e.g. "summary/report/notes
    /// → .md". Empty: none.
    pub pick: String,
    /// More system-prompt guidance for this type (e.g. the rules for HTML apps).
    pub guidance: String,
    pub hooks: Arc<dyn ArtifactHooks>,
}

impl ArtifactType {
    /// A text type with no suffixes, description or hooks yet.
    pub fn new(content_type: &str, label: &str, kind: ArtifactKind) -> Self {
        Self {
            content_type: content_type.to_owned(),
            suffixes: Vec::new(),
            label: label.to_owned(),
            kind,
            text: true,
            describe: String::new(),
            pick: String::new(),
            guidance: String::new(),
            hooks: Arc::new(Plain),
        }
    }

    pub fn suffixes(mut self, suffixes: &[&str]) -> Self {
        self.suffixes = suffixes.iter().map(|s| s.to_ascii_lowercase()).collect();
        self
    }

    pub fn binary(mut self) -> Self {
        self.text = false;
        self
    }

    pub fn describe(mut self, text: &str) -> Self {
        text.clone_into(&mut self.describe);
        self
    }

    pub fn pick(mut self, text: &str) -> Self {
        text.clone_into(&mut self.pick);
        self
    }

    pub fn guidance(mut self, text: &str) -> Self {
        text.clone_into(&mut self.guidance);
        self
    }

    pub fn hooks(mut self, hooks: Arc<dyn ArtifactHooks>) -> Self {
        self.hooks = hooks;
        self
    }

    /// The longest of its suffixes `lower` (a lowercase file name) ends with.
    fn suffix_of(&self, lower: &str) -> Option<&str> {
        self.suffixes.iter().filter(|s| lower.len() > s.len() && lower.ends_with(s.as_str())).map(String::as_str).max_by_key(|s| s.len())
    }
}

/// The artifact types an engine knows. Cheap to clone; clones share registrations, so a
/// type registered later is seen by every holder (tools, engine, server).
#[derive(Clone)]
pub struct ArtifactTypes(Arc<RwLock<Vec<ArtifactType>>>);

impl Default for ArtifactTypes {
    fn default() -> Self {
        Self::builtin()
    }
}

impl std::fmt::Debug for ArtifactTypes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.all().iter().map(|t| t.content_type.clone())).finish()
    }
}

impl ArtifactTypes {
    /// No types: every file is plain text.
    pub fn empty() -> Self {
        Self(Arc::default())
    }

    /// byob's own types, in the order the prompt lists them.
    pub fn builtin() -> Self {
        let types = Self::empty();
        markdown::register(&types);
        html_app::register(&types);
        text::register(&types);
        chart::register(&types);
        image::register(&types);
        types
    }

    /// The ids plugin manifests name byob's own types by (`artifact_types[].native`), with
    /// their content types, in the order [`Self::builtin`] registers them.
    pub fn native_ids() -> Vec<(&'static str, &'static str)> {
        use byob_protocol::artifact::{TYPE_CHART, TYPE_HTML, TYPE_JPEG, TYPE_MARKDOWN, TYPE_PNG};
        vec![("markdown", TYPE_MARKDOWN), ("html_app", TYPE_HTML), ("text", TYPE_TEXT), ("chart", TYPE_CHART), ("png", TYPE_PNG), ("jpeg", TYPE_JPEG)]
    }

    /// Add a type, or replace the one with its content type (keeping its place).
    pub fn register(&self, t: ArtifactType) {
        let mut types = self.0.write().unwrap_or_else(PoisonError::into_inner);
        match types.iter_mut().find(|x| x.content_type == t.content_type) {
            Some(x) => *x = t,
            None => types.push(t),
        }
    }

    pub fn all(&self) -> Vec<ArtifactType> {
        self.0.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    pub fn get(&self, content_type: &str) -> Option<ArtifactType> {
        self.0.read().unwrap_or_else(PoisonError::into_inner).iter().find(|t| t.content_type == content_type).cloned()
    }

    /// What anything unknown is: the registered plain text type, else a bare one.
    pub fn fallback(&self) -> ArtifactType {
        self.get(TYPE_TEXT).unwrap_or_else(|| ArtifactType::new(TYPE_TEXT, "file", ArtifactKind::Text))
    }

    /// The type of a file, by its name: the longest matching suffix wins (`.chart.json` over
    /// `.json`); nothing matches → plain text.
    pub fn for_filename(&self, name: &str) -> ArtifactType {
        let lower = name.to_ascii_lowercase();
        let types = self.0.read().unwrap_or_else(PoisonError::into_inner);
        let best = types.iter().filter_map(|t| t.suffix_of(&lower).map(|s| (s.len(), t))).max_by_key(|(n, _)| *n).map(|(_, t)| t.clone());
        drop(types);
        best.unwrap_or_else(|| self.fallback())
    }

    /// The longest registered suffix `name` ends with, without its leading dot (what
    /// versions are saved with, e.g. `chart.json`), if it has more than one part.
    pub fn compound_extension(&self, name: &str) -> Option<String> {
        let lower = name.to_ascii_lowercase();
        let types = self.0.read().unwrap_or_else(PoisonError::into_inner);
        let best = types.iter().filter_map(|t| t.suffix_of(&lower)).max_by_key(|s| s.len())?;
        let ext = best.trim_start_matches('.');
        ext.contains('.').then(|| ext.to_owned())
    }

    /// An artifact's type (its content type's registration, else the fallback).
    pub fn of(&self, a: &Artifact) -> ArtifactType {
        self.get(a.content_type()).unwrap_or_else(|| self.fallback())
    }

    /// The types the model writes, as `create_artifact` names them.
    fn described(&self) -> String {
        self.all().iter().map(|t| t.describe.clone()).filter(|d| !d.is_empty()).collect::<Vec<_>>().join(", ")
    }

    /// `create_artifact`'s description.
    pub fn tool_description(&self) -> String {
        format!(
            "Make an artifact: a file shown beside the chat that the user can open, keep and export, with versions. \
             The file name's extension decides what it is: {}. Returns the artifact's id for update_artifact.",
            self.described()
        )
    }

    /// What a file name without a usable extension is told.
    pub fn extension_hint(&self) -> String {
        format!("needs an extension that says what it is: {}", self.described())
    }

    /// What the system prompt says about artifacts: the general rules, each type's place in
    /// "pick the format that fits", then each type's own guidance.
    pub fn prompt(&self) -> String {
        let all = self.all();
        let picks: Vec<&str> = all.iter().map(|t| t.pick.as_str()).filter(|p| !p.is_empty()).collect();
        let mut out = String::from(
            "Artifacts: when the user wants something to keep, reuse or export — a report, summary or document, an interactive tool, calculator, game or app, a data file — \
             make it an artifact with create_artifact rather than pasting it into the chat; it shows in a pane beside the conversation and can be exported.",
        );
        if !picks.is_empty() {
            out.push_str(&format!(" Pick the format that fits: {}.", picks.join("; ")));
        }
        out.push_str(
            " To change an artifact, call update_artifact with its id and small find/replace edits (read_artifact shows its current text) — never create a new one for a change.",
        );
        for t in all.iter().filter(|t| !t.guidance.is_empty()) {
            out.push(' ');
            out.push_str(&t.guidance);
        }
        out.push_str(" After making one, say in a sentence or two what it is; don't repeat its content.");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use byob_protocol::artifact::{TYPE_CHART, TYPE_HTML, TYPE_JPEG, TYPE_MARKDOWN, TYPE_PNG};

    /// The prompt as it was written before artifact types were registrations.
    const OLD_PROMPT: &str = "Artifacts: when the user wants something to keep, reuse or export — a report, summary or document, an interactive tool, calculator, game or app, a data file — \
make it an artifact with create_artifact rather than pasting it into the chat; it shows in a pane beside the conversation and can be exported. Pick the format that fits: \
summary/report/notes → .md; interactive tool or app → a single-file .html app; data → .csv or .json; a chart of numbers → make_chart (or .html for a custom visualisation). \
To change an artifact, call update_artifact with its id and small find/replace edits (read_artifact shows its current text) — never create a new one for a change. \
HTML apps: one self-contained file with inline <style> and <script>; nothing from the network (no CDNs, web fonts, fetch or images by URL — all blocked); \
style with the theme's CSS variables var(--byob-bg), var(--byob-surface), var(--byob-fg), var(--byob-muted), var(--byob-accent), var(--byob-on-accent), var(--byob-border), var(--byob-danger), var(--byob-success), var(--byob-warning), var(--byob-link), var(--byob-code-bg), var(--byob-code-fg), var(--byob-radius) \
and var(--byob-chart-a) … var(--byob-chart-h) for data series (give fallbacks); \
fit any container width (fluid layout, no fixed page widths). After making one, say in a sentence or two what it is; don't repeat its content.";

    #[test]
    fn builtins_resolve_by_file_name() {
        let t = ArtifactTypes::builtin();
        let ct = |name: &str| t.for_filename(name).content_type;
        assert_eq!(ct("Sales.chart.json"), TYPE_CHART);
        assert_eq!(ct("data.json"), TYPE_TEXT);
        assert_eq!(ct("summary.MD"), TYPE_MARKDOWN);
        assert_eq!(ct("calc.html"), TYPE_HTML);
        assert_eq!(ct("plot.jpeg"), TYPE_JPEG);
        assert_eq!(ct("plot.png"), TYPE_PNG);
        assert_eq!(ct("script.py"), TYPE_TEXT);
        assert_eq!(ct("noext"), TYPE_TEXT);
        // A suffix alone isn't a name of that type.
        assert_eq!(ct(".md"), TYPE_TEXT);
        assert_eq!(t.compound_extension("Sales.chart.json").as_deref(), Some("chart.json"));
        assert_eq!(t.compound_extension("a.md"), None);
        // The 13.0 view of each.
        let kinds: Vec<(String, ArtifactKind)> = t.all().into_iter().map(|t| (t.content_type, t.kind)).collect();
        assert!(kinds.contains(&(TYPE_HTML.into(), ArtifactKind::Html)) && kinds.contains(&(TYPE_PNG.into(), ArtifactKind::Image)));
        assert!(!t.get(TYPE_PNG).unwrap().text && t.get(TYPE_HTML).unwrap().text);
    }

    #[test]
    fn the_prompt_and_descriptions_are_unchanged() {
        let t = ArtifactTypes::builtin();
        assert_eq!(t.prompt(), OLD_PROMPT);
        assert_eq!(
            t.tool_description(),
            "Make an artifact: a file shown beside the chat that the user can open, keep and export, with versions. \
             The file name's extension decides what it is: .md (document, report, summary), .html (interactive single-file app: HTML+CSS+JS in one file, no network), \
             .csv/.json/.py/… (data or code, shown as source), .chart.json (a chart spec). Returns the artifact's id for update_artifact."
        );
    }

    struct Upper;

    impl ArtifactHooks for Upper {
        fn check(&self, content: &str) -> Result<(String, String), String> {
            Ok((content.to_uppercase(), " (shouted)".into()))
        }
    }

    #[test]
    fn third_party_types_register_and_replace() {
        let t = ArtifactTypes::builtin();
        let shared = t.clone();
        t.register(
            ArtifactType::new("text/csv", "table", ArtifactKind::Text)
                .suffixes(&[".csv"])
                .describe(".csv (a table)")
                .pick("a table → .csv")
                .guidance("Tables: comma-separated, a header row first.")
                .hooks(Arc::new(Upper)),
        );
        // Every clone sees it.
        let csv = shared.for_filename("prices.CSV");
        assert_eq!((csv.content_type.as_str(), csv.label.as_str()), ("text/csv", "table"));
        assert_eq!(csv.hooks.check("a,b").unwrap(), ("A,B".into(), " (shouted)".into()));
        let p = shared.prompt();
        assert!(p.contains("; a table → .csv.") && p.contains("Tables: comma-separated"), "{p}");
        assert!(shared.tool_description().contains(".csv (a table)"));
        // Replacing keeps the place; an unknown content type falls back to text.
        t.register(ArtifactType::new(TYPE_MARKDOWN, "notes", ArtifactKind::Markdown).suffixes(&[".md"]));
        assert_eq!(t.all()[0].label, "notes");
        let a = Artifact {
            id: "a".into(),
            kind: ArtifactKind::Text,
            content_type: "application/x-nobody".into(),
            title: "A".into(),
            filename: "a".into(),
            versions: vec![],
        };
        assert_eq!(t.of(&a).content_type, TYPE_TEXT);
        // Even with nothing registered.
        assert_eq!(ArtifactTypes::empty().for_filename("a.html").content_type, TYPE_TEXT);
    }
}
