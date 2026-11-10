//! Plain text: csv, json, code… anything without a type of its own (shown as source).

use byob_protocol::ArtifactKind;
use byob_protocol::artifact::TYPE_TEXT;

use super::{ArtifactType, ArtifactTypes};

pub fn register(types: &ArtifactTypes) {
    types.register(
        ArtifactType::new(TYPE_TEXT, "file", ArtifactKind::Text).describe(".csv/.json/.py/… (data or code, shown as source)").pick("data → .csv or .json"),
    );
}
