//! byob.chat (`site/`) names the release in its download links; `cargo xtask ci` fails when
//! that falls behind the workspace version.

use std::path::Path;

pub const PAGE: &str = "site/index.html";

/// What in `root`'s site doesn't match the workspace version: one line per stale version.
pub fn stale(root: &Path) -> Result<Vec<String>, String> {
    let cargo = std::fs::read_to_string(root.join("Cargo.toml")).map_err(|e| format!("read Cargo.toml: {e}"))?;
    let version = workspace_version(&cargo).ok_or("Cargo.toml has no [workspace.package] version")?;
    let page = std::fs::read_to_string(root.join(PAGE)).map_err(|e| format!("read {PAGE}: {e}"))?;
    let mut problems: Vec<String> =
        versions(&page).into_iter().filter(|(_, v)| *v != version).map(|(line, v)| format!("{PAGE}:{line} says {v}, Cargo.toml says {version}")).collect();
    if !versions(&page).iter().any(|(_, v)| *v == version) {
        problems.push(format!("{PAGE} doesn't mention {version}"));
    }
    Ok(problems)
}

/// `version = "…"` under `[workspace.package]`.
fn workspace_version(cargo: &str) -> Option<String> {
    let mut inside = false;
    for line in cargo.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            inside = t == "[workspace.package]";
        } else if inside && let Some(v) = t.strip_prefix("version").map(str::trim_start).and_then(|r| r.strip_prefix('=')) {
            return Some(v.trim().trim_matches('"').to_owned());
        }
    }
    None
}

/// Every `X.Y.Z` in `text` with its line number; longer dotted numbers (an IP address) aren't
/// versions.
fn versions(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let b = line.as_bytes();
        let mut i = 0;
        while i < b.len() {
            let starts = b[i].is_ascii_digit() && (i == 0 || !(b[i - 1].is_ascii_digit() || b[i - 1] == b'.'));
            if !starts {
                i += 1;
                continue;
            }
            let mut j = i;
            while j < b.len() && (b[j].is_ascii_digit() || (b[j] == b'.' && b.get(j + 1).is_some_and(u8::is_ascii_digit))) {
                j += 1;
            }
            let s = &line[i..j];
            if s.split('.').count() == 3 {
                out.push((n + 1, s.to_owned()));
            }
            i = j.max(i + 1);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_release_versions_but_not_addresses() {
        let page = "<a href=\"download/v0.3.5/byob-0.3.5-x86_64.tar.gz\">\nbyob 0.3.4 on http://127.0.0.1:7470 and 10.0.0.1\n";
        assert_eq!(versions(page), [(1, "0.3.5".to_owned()), (1, "0.3.5".to_owned()), (2, "0.3.4".to_owned())]);
        let cargo = "[workspace]\nmembers = []\n\n[workspace.package]\nedition = \"2024\"\nversion = \"0.3.5\"\n\n[package]\nversion = \"9.9.9\"\n";
        assert_eq!(workspace_version(cargo).as_deref(), Some("0.3.5"));
    }
}
