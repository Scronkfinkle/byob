//! Who is on the other end, and may they in?
//!
//! Behind `tailscale serve`, every proxied request from a tailnet user carries
//! `Tailscale-User-Login` / `Tailscale-User-Name`. Tailscale strips client-sent copies of
//! these headers and always sets `X-Forwarded-Host`; funnel (public internet) requests get
//! `Tailscale-Funnel-Request: ?1` and no identity; tagged devices get no identity.
//!
//! The headers are only meaningful when the TCP peer is loopback (tailscale serve proxies
//! from 127.0.0.1). From any other peer they are attacker-controlled and ignored.

use std::fmt;
use std::net::IpAddr;

use crate::http::Request;

/// The connecting party, as far as we can tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Who {
    /// A tailnet user, identified by `tailscale serve`.
    Tailnet { login: String, name: Option<String> },
    /// Someone on this machine talking to us directly (loopback, not via a proxy).
    Local,
    /// Unidentified: a direct non-loopback peer, or a proxy that sent no identity.
    Anonymous,
}

impl fmt::Display for Who {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tailnet { login, name: Some(name) } => write!(f, "{name} <{login}>"),
            Self::Tailnet { login, name: None } => write!(f, "{login}"),
            Self::Local => f.write_str("local user"),
            Self::Anonymous => f.write_str("anonymous"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    Allow(Who),
    Deny(String),
}

/// Decide whether `req` from `peer` may load the UI or open a WebSocket.
///
/// - Tailscale Funnel traffic is always refused: it is the public internet.
/// - No allowlist: everyone else is let in (the tailnet is the boundary), identified if possible.
/// - With an allowlist: a tailnet login on the list, or a direct local request, is let in;
///   everything else (other logins, tagged devices, non-loopback peers, other proxies) is not.
pub fn authorize(peer: IpAddr, req: &Request, allowed_users: &[String]) -> Access {
    let trusted = is_loopback(peer);
    if !trusted && req.header("tailscale-user-login").is_some() {
        log::warn!("ignoring Tailscale identity headers from non-loopback peer {peer}");
    }
    if trusted && req.header("tailscale-funnel-request").is_some() {
        return Access::Deny("byob does not accept Tailscale Funnel (public internet) traffic. Use `tailscale serve`, which stays inside your tailnet.".into());
    }
    let who = identify(trusted, req);
    if allowed_users.is_empty() {
        return Access::Allow(who);
    }
    match who {
        Who::Tailnet { ref login, .. } if allowed_users.iter().any(|u| u.eq_ignore_ascii_case(login)) => Access::Allow(who),
        Who::Tailnet { login, .. } => Access::Deny(format!("{login} is not allowed to use this byob. Ask its owner to add `--allow-user {login}`.")),
        Who::Local => Access::Allow(Who::Local),
        Who::Anonymous => Access::Deny("This byob only admits specific Tailscale users, and this request carries no Tailscale identity. Open it through `tailscale serve` from an untagged device.".into()),
    }
}

fn identify(trusted: bool, req: &Request) -> Who {
    if !trusted {
        return Who::Anonymous;
    }
    if let Some(login) = req.header("tailscale-user-login").map(decode_header_value).filter(|l| !l.trim().is_empty()) {
        let name = req.header("tailscale-user-name").map(decode_header_value).filter(|n| !n.trim().is_empty());
        return Who::Tailnet { login, name };
    }
    let proxied = ["x-forwarded-host", "x-forwarded-for", "forwarded", "tailscale-headers-info"].iter().any(|h| req.header(h).is_some());
    if proxied { Who::Anonymous } else { Who::Local }
}

fn is_loopback(ip: IpAddr) -> bool {
    ip.to_canonical().is_loopback()
}

/// Tailscale RFC 2047 Q-encodes non-ASCII values (`=?utf-8?q?J=C3=BCrgen?=`); plain ASCII
/// passes through. Anything malformed is returned as-is.
pub fn decode_header_value(raw: &str) -> String {
    let words: Vec<&str> = raw.split_whitespace().collect();
    if !words.iter().any(|w| is_encoded_word(w)) {
        return raw.to_owned();
    }
    let mut out = String::new();
    let mut prev_encoded = false;
    for word in words {
        let decoded = is_encoded_word(word).then(|| decode_q_word(word)).flatten();
        if !out.is_empty() && !(prev_encoded && decoded.is_some()) {
            out.push(' ');
        }
        prev_encoded = decoded.is_some();
        out.push_str(decoded.as_deref().unwrap_or(word));
    }
    out
}

fn is_encoded_word(w: &str) -> bool {
    w.starts_with("=?") && w.ends_with("?=") && w.len() > 4
}

fn decode_q_word(word: &str) -> Option<String> {
    let inner = word.get(2..word.len().checked_sub(2)?)?;
    let mut parts = inner.splitn(3, '?');
    let (charset, encoding, text) = (parts.next()?, parts.next()?, parts.next()?);
    if !charset.eq_ignore_ascii_case("utf-8") || !encoding.eq_ignore_ascii_case("q") {
        return None;
    }
    let mut bytes = Vec::with_capacity(text.len());
    let mut it = text.bytes();
    while let Some(b) = it.next() {
        match b {
            b'_' => bytes.push(b' '),
            b'=' => {
                let hex = [it.next()?, it.next()?];
                bytes.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
            }
            _ => bytes.push(b),
        }
    }
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
    const REMOTE: IpAddr = IpAddr::V4(Ipv4Addr::new(100, 96, 19, 117));

    /// A request as `tailscale serve` would forward it for `login`.
    fn via_serve(login: &str) -> Request {
        Request::for_test(&[
            ("Host", "box.tail1234.ts.net"),
            ("X-Forwarded-Host", "box.tail1234.ts.net"),
            ("Tailscale-User-Login", login),
            ("Tailscale-User-Name", "Ann"),
        ])
    }

    fn allow(users: &[&str]) -> Vec<String> {
        users.iter().map(|u| (*u).to_owned()).collect()
    }

    #[test]
    fn no_allowlist_admits_and_identifies() {
        let who = Who::Tailnet { login: "ann@example.com".into(), name: Some("Ann".into()) };
        assert_eq!(authorize(LOOPBACK, &via_serve("ann@example.com"), &[]), Access::Allow(who));
        assert_eq!(authorize(LOOPBACK, &Request::for_test(&[("Host", "127.0.0.1:7470")]), &[]), Access::Allow(Who::Local));
        assert_eq!(authorize(REMOTE, &Request::for_test(&[("Host", "10.0.0.2:7470")]), &[]), Access::Allow(Who::Anonymous));
    }

    #[test]
    fn allowlist_admits_listed_logins_only() {
        let list = allow(&["ann@example.com"]);
        assert!(matches!(authorize(LOOPBACK, &via_serve("ann@example.com"), &list), Access::Allow(Who::Tailnet { .. })));
        assert!(matches!(authorize(LOOPBACK, &via_serve("ANN@example.com"), &list), Access::Allow(_)));
        assert!(matches!(authorize(LOOPBACK, &via_serve("bob@example.com"), &list), Access::Deny(_)));
    }

    #[test]
    fn spoofed_identity_from_non_loopback_peer_is_ignored() {
        let list = allow(&["ann@example.com"]);
        assert!(matches!(authorize(REMOTE, &via_serve("ann@example.com"), &list), Access::Deny(_)));
        // Without an allowlist they're let in, but not as Ann.
        assert_eq!(authorize(REMOTE, &via_serve("ann@example.com"), &[]), Access::Allow(Who::Anonymous));
        let v6 = IpAddr::V6(Ipv6Addr::new(0xfd7a, 0x115c, 0xa1e0, 0, 0, 0, 0x9c37, 0x1377));
        assert!(matches!(authorize(v6, &via_serve("ann@example.com"), &list), Access::Deny(_)));
    }

    #[test]
    fn ipv4_mapped_loopback_is_loopback() {
        let mapped = IpAddr::V6(Ipv4Addr::LOCALHOST.to_ipv6_mapped());
        assert!(matches!(authorize(mapped, &via_serve("ann@example.com"), &allow(&["ann@example.com"])), Access::Allow(_)));
    }

    #[test]
    fn allowlist_rejects_unidentified_proxied_requests() {
        let list = allow(&["ann@example.com"]);
        // Tagged device via tailscale serve: forwarded, but no identity.
        let tagged = Request::for_test(&[("Host", "box.ts.net"), ("X-Forwarded-Host", "box.ts.net"), ("X-Forwarded-For", "100.1.2.3")]);
        assert!(matches!(authorize(LOOPBACK, &tagged, &list), Access::Deny(_)));
        // Direct local access still works.
        assert_eq!(authorize(LOOPBACK, &Request::for_test(&[("Host", "localhost:7470")]), &list), Access::Allow(Who::Local));
    }

    #[test]
    fn funnel_is_always_refused() {
        let funnel = Request::for_test(&[("Host", "box.ts.net"), ("X-Forwarded-Host", "box.ts.net"), ("Tailscale-Funnel-Request", "?1")]);
        assert!(matches!(authorize(LOOPBACK, &funnel, &[]), Access::Deny(_)));
    }

    #[test]
    fn decodes_q_encoded_names() {
        assert_eq!(decode_header_value("ann@example.com"), "ann@example.com");
        assert_eq!(decode_header_value("=?utf-8?q?J=C3=BCrgen_M=C3=BCller?="), "Jürgen Müller");
        assert_eq!(decode_header_value("=?utf-8?q?J=C3=BC?= =?utf-8?q?rgen?="), "Jürgen");
        assert_eq!(decode_header_value("=?utf-8?q?bad=Z?="), "=?utf-8?q?bad=Z?=");
        assert_eq!(decode_header_value("=?"), "=?");
    }
}
