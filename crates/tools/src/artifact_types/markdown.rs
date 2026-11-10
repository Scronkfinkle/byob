//! Markdown documents (`.md`): reports, summaries, notes. Clients render them with a source
//! view.

use byob_protocol::ArtifactKind;
use byob_protocol::artifact::TYPE_MARKDOWN;

use super::{ArtifactType, ArtifactTypes};

pub fn register(types: &ArtifactTypes) {
    types.register(
        ArtifactType::new(TYPE_MARKDOWN, "document", ArtifactKind::Markdown)
            .suffixes(&[".md", ".markdown"])
            .describe(".md (document, report, summary)")
            .pick("summary/report/notes → .md"),
    );
}
