//! HTML apps (`.html`): a single self-contained file (HTML + CSS + JS) that runs sandboxed —
//! in an iframe in the browser, in the Servo helper on the desktop, or in a browser tab via
//! Open. Everything about them on the engine side is here: the prompt's rules, the content
//! check (warn about network use, which the sandbox blocks), and the document wrapper every
//! renderer loads ([`wrap_html`]: theme defaults, the helper script that applies the theme
//! and reports script errors, no WebRTC). The client side is `byob-ui-egui`'s `html_app`
//! renderer.

use std::sync::Arc;

use byob_protocol::ArtifactKind;
use byob_protocol::artifact::TYPE_HTML;

use super::{ArtifactHooks, ArtifactType, ArtifactTypes};

/// What the system prompt says about writing HTML apps.
pub const GUIDANCE: &str = "HTML apps: one self-contained file with inline <style> and <script>; nothing from the network (no CDNs, web fonts, fetch or images by URL — all blocked); \
style with the theme's CSS variables var(--byob-bg), var(--byob-surface), var(--byob-fg), var(--byob-muted), var(--byob-accent), var(--byob-on-accent), var(--byob-border), var(--byob-danger), var(--byob-success), var(--byob-warning), var(--byob-link), var(--byob-code-bg), var(--byob-code-fg), var(--byob-radius) \
and var(--byob-chart-a) … var(--byob-chart-h) for data series (give fallbacks); \
fit any container width (fluid layout, no fixed page widths).";

pub fn register(types: &ArtifactTypes) {
    types.register(
        ArtifactType::new(TYPE_HTML, "app", ArtifactKind::Html)
            .suffixes(&[".html", ".htm"])
            .describe(".html (interactive single-file app: HTML+CSS+JS in one file, no network)")
            .pick("interactive tool or app → a single-file .html app")
            .guidance(GUIDANCE)
            .hooks(Arc::new(HtmlApp)),
    );
}

struct HtmlApp;

impl ArtifactHooks for HtmlApp {
    fn check(&self, content: &str) -> Result<(String, String), String> {
        let lower = content.to_ascii_lowercase();
        let external = ["src=\"http", "src='http", "href=\"http", "href='http", "src=\"//", "@import url(", "url(http", "fetch(", "xmlhttprequest"]
            .iter()
            .any(|p| lower.contains(p));
        let note = if external {
            " Warning: it seems to load things from the network (a URL, fetch or a CDN). That is blocked in artifacts; inline everything instead."
        } else {
            ""
        };
        Ok((content.to_owned(), note.to_owned()))
    }

    fn serve(&self, bytes: Vec<u8>) -> Vec<u8> {
        wrap_html(&String::from_utf8_lossy(&bytes)).into_bytes()
    }
}

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
    fn helper_goes_first_and_keeps_the_doctype() {
        let w = wrap_html("<!DOCTYPE html><html><body>hi</body></html>");
        assert!(w.starts_with("<!DOCTYPE html><meta charset=\"utf-8\"><style>"), "{w}");
        assert!(w.ends_with("<html><body>hi</body></html>"));
        let bare = wrap_html("<p>hi</p>");
        assert!(bare.starts_with("<!doctype html><meta") && bare.ends_with("<p>hi</p>"));
        assert!(bare.contains("RTCPeerConnection") && bare.contains("--byob-accent"));
    }

    #[test]
    fn served_wrapped_and_network_use_warned_about() {
        let types = ArtifactTypes::builtin();
        let html = types.get(TYPE_HTML).unwrap();
        let served = String::from_utf8(html.hooks.serve(b"<p>x</p>".to_vec())).unwrap();
        assert!(served.contains("byob-error") && served.ends_with("<p>x</p>"));
        assert!(html.hooks.check("<script src=\"https://cdn.x/y.js\"></script>").unwrap().1.contains("Warning"));
        assert!(html.hooks.check("<p>ok</p>").unwrap().1.is_empty());
        // Other types are served as saved.
        let md = types.for_filename("a.md");
        assert_eq!(md.hooks.serve(b"# x".to_vec()), b"# x");
    }
}
