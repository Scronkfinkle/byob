//! `GET /artifact/<chat>/<id>/<version>`: one artifact version, for an `<iframe>` in the
//! browser UI or a tab opened from the desktop app.
//!
//! The response is locked down by its own CSP (an iframe `srcdoc` would inherit the host
//! page's instead): `sandbox allow-scripts allow-forms` gives the document an opaque origin
//! even as a top-level tab, so it can't read byob's storage or talk to `/ws` (its `Origin` is
//! `null`); `connect-src 'none'` and friends keep it off the network; `frame-ancestors
//! 'self'` means only byob may frame it. Only artifacts that belong to that chat are served.
//!
//! The body is what the artifact's type serves ([`byob_tools::ArtifactHooks::serve`]): HTML
//! apps get a small helper script injected first (`byob_tools::artifact_types::html_app`):
//! it applies theme variables passed in the URL hash (or posted later by the host), reports
//! script errors to the host page (or, when it is the top-level page as in the desktop's
//! Servo renderer, as a marked `console.error` that `byob-artifact-host` picks up), and
//! removes WebRTC (which can reach the network around `connect-src`). The CSP is the same
//! for every type. `?download` serves any version as saved, as an attachment.

use std::io::{self, Write};

use byob_agent::EngineHandle;
use byob_agent::artifact;

use crate::http::{self, Request};

/// Every artifact response carries this.
pub const CSP: &str = "sandbox allow-scripts allow-forms; default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; \
    img-src data: blob:; font-src data:; media-src data: blob:; connect-src 'none'; form-action 'none'; base-uri 'none'; frame-ancestors 'self'";

/// `/artifact/<chat>/<id>/<version>[?download]` → its parts, if well-formed.
pub fn parse(path: &str) -> Option<(&str, &str, u32, bool)> {
    let (path, query) = path.split_once('?').map_or((path, ""), |(p, q)| (p, q));
    let path = path.split('#').next().unwrap_or_default();
    let mut parts = path.strip_prefix("/artifact/")?.split('/');
    let (chat, id, version) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let chat_ok = (1..=80).contains(&chat.len()) && chat.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    let version: u32 = version.parse().ok().filter(|n| *n > 0)?;
    let download = query.split('&').any(|q| q == "download" || q.starts_with("download="));
    (chat_ok && byob_protocol::artifact::valid_id(id)).then_some((chat, id, version, download))
}

/// A download file name safe for a header: ASCII letters, digits, `.-_` only.
fn header_filename(name: &str) -> String {
    let clean: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' }).take(100).collect();
    if clean.trim_matches(['.', '_']).is_empty() { "artifact".into() } else { clean }
}

pub fn serve(stream: &mut impl Write, engine: &EngineHandle, req: &Request) -> io::Result<()> {
    let Some((chat, id, version, download)) = parse(&req.path) else {
        return http::respond(stream, 404, "text/plain", b"not found");
    };
    let Some(workspace) = engine.chat_workspace(chat) else {
        return http::respond(stream, 404, "text/plain", b"no such artifact");
    };
    let (a, bytes) = match artifact::read_version(&workspace, chat, id, version, artifact::MAX_FILE_BYTES) {
        Ok(found) => found,
        Err(e) => {
            log::debug!("artifact {chat}/{id}/{version}: {e}");
            return http::respond(stream, 404, "text/plain", b"no such artifact");
        }
    };
    let mime = byob_protocol::artifact::mime(a.content_type());
    let mime = mime.as_str();
    let mut extra =
        vec![format!("Content-Security-Policy: {CSP}"), "Cache-Control: no-store".to_owned(), "Cross-Origin-Resource-Policy: same-origin".to_owned()];
    if download {
        extra.push(format!("Content-Disposition: attachment; filename=\"{}\"", header_filename(&a.filename)));
        return http::respond_with(stream, 200, mime, &extra, &bytes);
    }
    let body = engine.artifact_types().of(&a).hooks.serve(bytes);
    http::respond_with(stream, 200, mime, &extra, &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(parse("/artifact/c18f/calc-1a2b/3"), Some(("c18f", "calc-1a2b", 3, false)));
        assert_eq!(parse("/artifact/c18f/calc/1?download"), Some(("c18f", "calc", 1, true)));
        assert_eq!(parse("/artifact/c/a/2#--byob-bg=%23000"), Some(("c", "a", 2, false)));
        for bad in [
            "/artifact/c/a",
            "/artifact/c/a/0",
            "/artifact/c/a/x",
            "/artifact/c/../1",
            "/artifact/c/A/1",
            "/artifact/c/a/1/x",
            "/artifact/c%2F/a/1",
            "/artifacts/c/a/1",
        ] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn download_names() {
        assert_eq!(header_filename("my \"calc\".html"), "my__calc_.html");
        assert_eq!(header_filename("…"), "artifact");
    }
}
