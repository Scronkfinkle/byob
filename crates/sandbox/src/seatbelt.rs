//! macOS `sandbox-exec` (Seatbelt) profile for a [`Policy`], adapted from hob's.
//! Platform-independent so it can be tested anywhere; only used on macOS.
//!
//! Unlike Landlock, reads are a deny-list: the home folder, other users, external
//! volumes and shared temp folders are unreadable except for the policy's paths; the rest
//! of the system stays readable. Writes are an allow-list. Network is denied unless the
//! policy allows it. Paths go in as `-D` parameters, never spliced into the profile text.

use std::path::{Path, PathBuf};

use crate::policy::Policy;

/// The profile text and its `-D NAME=VALUE` parameters.
pub fn profile(policy: &Policy, home: Option<&Path>) -> (String, Vec<(String, String)>) {
    let mut params: Vec<(String, String)> = Vec::new();
    let mut add = |prefix: &str, paths: &[PathBuf]| -> Vec<String> {
        paths
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let name = format!("{prefix}{i}");
                params.push((name.clone(), p.to_string_lossy().into_owned()));
                name
            })
            .collect()
    };
    let reads = add("R", &policy.read);
    let writes = add("W", &policy.write);
    let above = add("A", &folders_above(policy.write.iter().chain(&policy.read)));
    let home_param = home.map(|h| add("HOME", &[h.to_path_buf()]));

    let sub = |names: &[String]| names.iter().map(|n| format!(" (subpath (param \"{n}\"))")).collect::<String>();
    let mut s = String::from("(version 1)\n(allow default)\n");
    if !policy.network {
        s.push_str("(deny network*)\n");
    }
    s.push_str("(deny appleevent-send)\n");
    s.push_str("(deny mach-lookup (global-name \"com.apple.coreservices.launchservicesd\") (global-name \"com.apple.lsd.mapdb\") (global-name \"com.apple.lsd.modifydb\") (global-name \"com.apple.coreservices.quarantine-resolver\"))\n");
    s.push_str("(deny file-write*)\n");
    s.push_str(&format!("(allow file-write*{})\n", sub(&writes)));
    let home_rule = home_param.as_deref().map(sub).unwrap_or_default();
    s.push_str(&format!(
        "(deny file-read*{home_rule} (subpath \"/Users\") (subpath \"/Volumes\") (subpath \"/private/tmp\") (subpath \"/private/var/folders\"))\n"
    ));
    let mut allowed = reads.clone();
    allowed.extend(writes.iter().cloned());
    s.push_str(&format!("(allow file-read*{})\n", sub(&allowed)));
    if !above.is_empty() {
        // SQLite and friends stat each folder on the way to a file.
        let lits = above.iter().map(|n| format!(" (literal (param \"{n}\"))")).collect::<String>();
        s.push_str(&format!("(allow file-read-metadata{lits})\n"));
    }
    (s, params)
}

/// Every folder above these paths, e.g. `/Users` and `/Users/me` for `/Users/me/chat`.
fn folders_above<'a>(paths: impl Iterator<Item = &'a PathBuf>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = paths.flat_map(|p| p.ancestors().skip(1).map(Path::to_path_buf).collect::<Vec<_>>()).filter(|a| a.parent().is_some()).collect();
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_shape() {
        let p = Policy { read: vec!["/usr".into()], write: vec!["/Users/me/ws".into()], ..Default::default() };
        let (s, params) = profile(&p, Some(Path::new("/Users/me")));
        assert!(s.contains("(deny network*)"));
        assert!(s.contains("(allow file-write* (subpath (param \"W0\")))"));
        assert!(s.contains("(allow file-read* (subpath (param \"R0\")) (subpath (param \"W0\")))"));
        assert!(s.contains("(subpath (param \"HOME0\"))"));
        assert!(params.contains(&("A0".into(), "/Users".into())) && params.contains(&("A1".into(), "/Users/me".into())));
        // paths never enter the profile text
        assert!(!s.contains("/Users/me"));
        let (s, _) = profile(&Policy { network: true, ..p }, None);
        assert!(!s.contains("(deny network*)"));
    }
}
