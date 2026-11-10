//! Media in the conversation: charts and images that tools made, and files users attached.
//!
//! Entries and steps carry only small [`MediaRef`]s (a workspace-relative path). Clients
//! fetch the content with [`crate::ClientMsg::GetMedia`]; the engine answers with
//! [`crate::ServerMsg::Media`], and only for paths the transcript references, so a client
//! can never read arbitrary files. Uploads travel in [`crate::ClientMsg::Upload`] chunks
//! that each fit well inside [`crate::MAX_FRAME_BYTES`].

use base64::Engine as _;
use serde::{Deserialize, Serialize};
pub use byob_types::chart::ChartSpec;

/// Largest file a user may attach.
pub const MAX_UPLOAD_BYTES: u64 = 25 * 1024 * 1024;
/// Raw bytes per upload chunk (base64 makes it 4/3 bigger on the wire).
pub const UPLOAD_CHUNK_BYTES: usize = 1024 * 1024;
/// Largest encoded image sent to a client in one [`MediaContent::Image`]; bigger images are
/// downscaled by the engine first.
pub const MAX_IMAGE_BYTES: usize = 2_500_000;
/// Files attached to one message.
pub const MAX_ATTACHMENTS: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    /// A `.chart.json` drawn natively.
    Chart,
    /// A PNG or JPEG shown inline.
    Image,
    /// Anything else (shown as a file chip).
    File,
}

impl MediaKind {
    /// What a workspace path is shown as, by its name.
    pub fn for_path(path: &str) -> Self {
        let lower = path.to_ascii_lowercase();
        if lower.ends_with(".chart.json") {
            Self::Chart
        } else if [".png", ".jpg", ".jpeg"].iter().any(|e| lower.ends_with(e)) {
            Self::Image
        } else {
            Self::File
        }
    }
}

/// A file shown in the conversation. `path` is relative to the workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaRef {
    pub kind: MediaKind,
    pub path: String,
    /// Size in bytes, if known.
    #[serde(default)]
    pub size: u64,
}

impl MediaRef {
    pub fn new(path: impl Into<String>, size: u64) -> Self {
        let path = path.into();
        Self { kind: MediaKind::for_path(&path), path, size }
    }

    /// File name for display.
    pub fn name(&self) -> &str {
        self.path.rsplit(['/', '\\']).next().unwrap_or(&self.path)
    }
}

/// The content behind a [`MediaRef`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MediaContent {
    Chart {
        spec: ChartSpec,
    },
    /// An encoded PNG or JPEG, base64 (see [`encode`]).
    Image {
        mime: String,
        data: String,
    },
    /// The file is gone, too big, or not something we show.
    Error {
        message: String,
    },
}

/// Standard base64, as used for upload chunks and images.
pub fn encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Decode untrusted base64.
pub fn decode(text: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD.decode(text).map_err(|e| format!("bad base64: {e}"))
}

/// Human-readable size.
pub fn size_label(n: u64) -> String {
    #[allow(clippy::cast_precision_loss)] // display only
    let f = n as f64;
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.0} KB", f / 1024.0)
    } else {
        format!("{:.1} MB", f / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_by_name() {
        assert_eq!(MediaKind::for_path("charts/Sales.chart.json"), MediaKind::Chart);
        assert_eq!(MediaKind::for_path("out/plot.PNG"), MediaKind::Image);
        assert_eq!(MediaKind::for_path("a.jpeg"), MediaKind::Image);
        assert_eq!(MediaKind::for_path("data.json"), MediaKind::File);
        assert_eq!(MediaRef::new("uploads/sales.csv", 3).name(), "sales.csv");
        assert_eq!(MediaRef::new("x", 0).name(), "x");
    }

    #[test]
    fn base64_round_trip_and_garbage() {
        assert_eq!(decode(&encode(b"hello\0")).unwrap(), b"hello\0");
        assert!(decode("!!!").is_err());
    }

    #[test]
    fn upload_chunks_fit_in_a_frame() {
        let chunk = encode(&vec![0xffu8; UPLOAD_CHUNK_BYTES]);
        assert!(chunk.len() + 1024 < crate::MAX_FRAME_BYTES);
        assert!(encode(&vec![0u8; MAX_IMAGE_BYTES]).len() + 1024 < crate::MAX_FRAME_BYTES);
    }

    #[test]
    fn sizes() {
        assert_eq!(size_label(10), "10 B");
        assert_eq!(size_label(2048), "2 KB");
        assert_eq!(size_label(3 * 1024 * 1024), "3.0 MB");
    }
}
