//! `GET /artifact/<chat>/<id>/<version>`: one artifact version, for an `<iframe>` in the
//! browser UI or a tab opened from the desktop app.
//!
//! The response is locked down by its own CSP (an iframe `srcdoc` would inherit the host
//! page's instead): `sandbox allow-scripts allow-forms` gives the document an opaque origin
//! even as a top-level tab, so it can't read byob's storage or talk to `/ws` (its `Origin` is
//! `null`); `connect-src 'none'` and friends keep it off the network; `frame-ancestors
//! 'self'` means only byob may frame it. Only artifacts that belong to that chat are served.
//!
//! HTML apps get a small helper script injected first: it applies theme variables passed in
//! the URL hash (or posted later by the host), reports script errors to the host page (or,
//! when it is the top-level page as in the desktop's Servo renderer, as a marked
//! `console.error` that `byob-artifact-host` picks up), and removes WebRTC (which can reach
//! the network around `connect-src`).
//! `?download` serves any version as an attachment instead.

use std::io::{self, Write};

use byob_agent::EngineHandle;
use byob_agent::artifact;
use byob_protocol::ArtifactKind;

use crate::http::{self, Request};

/// Every artifact response carries this.
pub const CSP: &str = "sandbox allow-scripts allow-forms; default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; \
    img-src data: blob:; font-src data:; media-src data: blob:; connect-src 'none'; form-action 'none'; base-uri 'none'; frame-ancestors 'self'";

/// Theme defaults (dark, and light for light-mode browsers), overridden by the URL hash.
/// Apps written before the rename to byob use `--yah-*` variables: alias each one to its
/// `--byob-*` twin (derived from [`DEFAULT_THEME`], so the lists can't drift).
fn legacy_aliases() -> String {
    // Definitions only (`--byob-name:`), not uses like `var(--byob-bg)`.
    let mut names: Vec<&str> = DEFAULT_THEME
        .split("--byob-")
        .skip(1)
        .filter_map(|rest| rest.split_once(':').map(|(name, _)| name))
        .filter(|name| !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c == '-'))
        .collect();
    names.sort_unstable();
    names.dedup();
    let body: String = names.iter().map(|n| format!("--yah-{n}:var(--byob-{n});")).collect();
    format!(":root{{{body}}}")
}

const DEFAULT_THEME: &str = ":root{--byob-bg:#16171a;--byob-surface:#1f2125;--byob-fg:#e4e4e7;--byob-muted:#9a9aa3;--byob-accent:#6d8cff;--byob-on-accent:#ffffff;\
    --byob-border:#34363c;--byob-danger:#f06a6a;--byob-success:#4cc38a;--byob-warning:#e8b75a;--byob-link:#6d8cff;--byob-strong:#ffffff;--byob-faint:#75787f;\
    --byob-panel:#1c1d21;--byob-field:#121316;--byob-code-bg:#141518;--byob-code-fg:#e4e5e8;--byob-radius:6px;\
    --byob-chart-a:#3987e5;--byob-chart-b:#d95926;--byob-chart-c:#199e70;--byob-chart-d:#c98500;--byob-chart-e:#d55181;--byob-chart-f:#008300;\
    --byob-chart-g:#9085e9;--byob-chart-h:#e66767;color-scheme:light dark}\
    @media (prefers-color-scheme: light){:root{--byob-bg:#ffffff;--byob-surface:#f4f4f6;--byob-fg:#1d1d22;--byob-muted:#686872;--byob-accent:#3f5fe0;\
    --byob-on-accent:#ffffff;--byob-border:#d9d9de;--byob-danger:#c53030;--byob-success:#1f8a55;--byob-warning:#8a5600;--byob-link:#3f5fe0;--byob-strong:#000000;\
    --byob-faint:#80838a;--byob-panel:#efefec;--byob-field:#ffffff;--byob-code-bg:#eeeff0;--byob-code-fg:#1d1e21;\
    --byob-chart-a:#2a78d6;--byob-chart-b:#eb6834;--byob-chart-c:#1baf7a;--byob-chart-d:#eda100;--byob-chart-e:#e87ba4;--byob-chart-f:#008300;\
    --byob-chart-g:#6250d6;--byob-chart-h:#e34948}}\
    html,body{background:var(--byob-bg);color:var(--byob-fg)}";

/// Runs inside the sandboxed app before its own code. Messages to the host are plain data
/// (the host treats them as untrusted text).
const SHIM: &str = r"<script>(()=>{
for(const k of['RTCPeerConnection','webkitRTCPeerConnection','RTCDataChannel','RTCSessionDescription','RTCIceCandidate'])try{Object.defineProperty(window,k,{value:undefined,writable:false,configurable:false})}catch(e){}
const P=m=>{try{if(parent!==window)parent.postMessage(m,'*');else if(m.t==='error')console.error('\x01byob-error '+(m.line|0)+' '+m.msg)}catch(e){}};
const S=(v,n)=>String(v==null?'':v).slice(0,n);
const setv=o=>{if(!o||typeof o!=='object')return;for(const[k,v]of Object.entries(o))if(/^--byob-[a-z-]{1,30}$/.test(k)&&/^[#a-zA-Z0-9(),. %-]{0,60}$/.test(String(v)))document.documentElement.style.setProperty(k,String(v))};
try{setv(Object.fromEntries(new URLSearchParams(location.hash.slice(1))))}catch(e){}
addEventListener('error',e=>P({t:'error',msg:S(e.message,2000),line:e.lineno|0,col:e.colno|0}));
addEventListener('unhandledrejection',e=>P({t:'error',msg:'Unhandled promise rejection: '+S(e.reason&&e.reason.message||e.reason,2000)}));
addEventListener('securitypolicyviolation',e=>P({t:'error',msg:'Blocked by the sandbox: '+S(e.violatedDirective,100)+' '+S(e.blockedURI,200)}));
addEventListener('message',e=>{if(e.source===parent&&e.data&&e.data.t==='theme')setv(e.data.vars)});
})()</script>";

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

/// Put the theme defaults and the helper script before anything the app wrote, right
/// after its doctype (keeps standards mode; the parser hoists them into `<head>`).
pub fn wrap_html(html: &str) -> String {
    let head = format!("<meta charset=\"utf-8\"><style>{DEFAULT_THEME}{}</style>{SHIM}", legacy_aliases());
    let start = html.trim_start();
    let lower = start.get(..9).unwrap_or_default().to_ascii_lowercase();
    if lower == "<!doctype" {
        let end = start.find('>').map_or(start.len(), |i| i.saturating_add(1));
        format!("{}{head}{}", start.get(..end).unwrap_or_default(), start.get(end..).unwrap_or_default())
    } else {
        format!("<!doctype html>{head}{html}")
    }
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
    let mime = a.kind.mime(&a.filename);
    let mut extra =
        vec![format!("Content-Security-Policy: {CSP}"), "Cache-Control: no-store".to_owned(), "Cross-Origin-Resource-Policy: same-origin".to_owned()];
    if download {
        extra.push(format!("Content-Disposition: attachment; filename=\"{}\"", header_filename(&a.filename)));
        return http::respond_with(stream, 200, mime, &extra, &bytes);
    }
    if a.kind == ArtifactKind::Html {
        let body = wrap_html(&String::from_utf8_lossy(&bytes));
        return http::respond_with(stream, 200, mime, &extra, body.as_bytes());
    }
    http::respond_with(stream, 200, mime, &extra, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_yah_variables_alias_the_new_ones() {
        let a = legacy_aliases();
        assert!(a.contains("--yah-bg:var(--byob-bg);"));
        assert!(a.contains("--yah-chart-h:var(--byob-chart-h);"));
        assert_eq!(a.matches("--yah-bg:").count(), 1, "deduplicated");
        assert!(!a.contains(')') || a.matches(')').count() == a.matches("var(").count(), "only definitions: {a}");
    }

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
    fn helper_goes_first_and_keeps_the_doctype() {
        let w = wrap_html("<!DOCTYPE html><html><body>hi</body></html>");
        assert!(w.starts_with("<!DOCTYPE html><meta charset=\"utf-8\"><style>"), "{w}");
        assert!(w.ends_with("<html><body>hi</body></html>"));
        let bare = wrap_html("<p>hi</p>");
        assert!(bare.starts_with("<!doctype html><meta") && bare.ends_with("<p>hi</p>"));
        assert!(bare.contains("RTCPeerConnection") && bare.contains("--byob-accent"));
        assert_eq!(header_filename("my \"calc\".html"), "my__calc_.html");
        assert_eq!(header_filename("…"), "artifact");
    }
}
