//! Images (PNG, JPEG) that tools made (`make_chart`, `run_python`) and that were adopted as
//! artifacts. Not written by `create_artifact`; a version too big for one message is
//! downscaled for clients.

use std::sync::Arc;

use byob_protocol::ArtifactKind;
use byob_protocol::artifact::{TYPE_JPEG, TYPE_PNG};

use super::{ArtifactHooks, ArtifactType, ArtifactTypes};
use crate::images;

/// Longest side of an image sent to a client.
pub const CLIENT_IMAGE_SIDE: u32 = 2048;

pub fn register(types: &ArtifactTypes) {
    let image = |content_type: &str, suffixes: &[&str]| {
        ArtifactType::new(content_type, "image", ArtifactKind::Image).suffixes(suffixes).binary().hooks(Arc::new(Image))
    };
    types.register(image(TYPE_PNG, &[".png"]));
    types.register(image(TYPE_JPEG, &[".jpg", ".jpeg"]));
}

struct Image;

impl ArtifactHooks for Image {
    fn check(&self, _content: &str) -> Result<(String, String), String> {
        Err("create_artifact writes text files; images that tools make (make_chart, run_python) show up as artifacts on their own".into())
    }

    fn shrink_for_client(&self, bytes: Vec<u8>, max: usize) -> Result<Vec<u8>, String> {
        images::fit(bytes, CLIENT_IMAGE_SIDE, max).map(|(_, b)| b)
    }
}
