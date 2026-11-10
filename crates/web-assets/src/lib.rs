//! byob's browser UI, embedded at build time (see `build.rs`).
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

/// One file of the UI bundle.
#[derive(Debug)]
pub struct Asset {
    /// Path under the site root, e.g. `index.html`.
    pub path: &'static str,
    /// `bytes` are gzip-compressed (serve with `Content-Encoding: gzip`).
    pub gzip: bool,
    /// A strong ETag (quoted) derived from the bytes.
    pub etag: &'static str,
    pub bytes: &'static [u8],
}

include!(concat!(env!("OUT_DIR"), "/assets.rs"));

/// The asset at `path` (`""` or `/` is `index.html`).
pub fn get(path: &str) -> Option<&'static Asset> {
    let path = path.trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    ASSETS.iter().find(|a| a.path == path)
}

/// Whether this build has the UI (it doesn't when `cargo xtask web` hasn't run).
pub fn available() -> bool {
    get("index.html").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_is_by_path_and_root_is_index() {
        assert_eq!(get("/").map(|a| a.path), get("index.html").map(|a| a.path));
        assert!(get("../etc/passwd").is_none());
    }
}
