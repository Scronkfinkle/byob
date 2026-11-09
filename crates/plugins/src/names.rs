//! Tool names every OpenAI-compatible server accepts: `[a-zA-Z0-9_-]{1,64}`, unique.

use std::collections::BTreeSet;

pub const MAX_TOOL_NAME: usize = 64;

/// Replace characters outside `[a-zA-Z0-9_-]` with `_`; `tool` if nothing is left.
pub fn sanitize(name: &str) -> String {
    let s: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
    if s.is_empty() { "tool".into() } else { s }
}

/// FNV-1a, for short stable suffixes.
fn fnv1a(s: &str) -> u32 {
    s.bytes().fold(0x811c_9dc5u32, |h, b| (h ^ u32::from(b)).wrapping_mul(0x0100_0193))
}

/// Fit `name` (already sanitized, ASCII) into 64 characters: long names keep their start
/// and get a hash of the whole name, so different long names stay different.
pub fn fit(name: &str) -> String {
    if name.len() <= MAX_TOOL_NAME {
        return name.to_string();
    }
    let hash = format!("_{:08x}", fnv1a(name));
    let keep = MAX_TOOL_NAME - hash.len();
    format!("{}{hash}", &name[..keep])
}

/// `<plugin>__<tool>`, sanitized and fitted. E.g. `linear__create_issue`.
pub fn namespaced(plugin_id: &str, tool: &str) -> String {
    fit(&format!("{}__{}", sanitize(plugin_id), sanitize(tool)))
}

/// Hands out unique tool names. Reserve the built-in tools' names first.
#[derive(Debug, Clone, Default)]
pub struct ToolNames {
    taken: BTreeSet<String>,
}

impl ToolNames {
    pub fn new<I: IntoIterator<Item = S>, S: Into<String>>(reserved: I) -> Self {
        Self { taken: reserved.into_iter().map(Into::into).collect() }
    }

    pub fn reserve(&mut self, name: &str) {
        self.taken.insert(name.to_string());
    }

    pub fn is_taken(&self, name: &str) -> bool {
        self.taken.contains(name)
    }

    /// `preferred` if free, else `fallback`, else `fallback_2`, `_3`, … (all fitted).
    pub fn claim(&mut self, preferred: &str, fallback: &str) -> String {
        let preferred = fit(&sanitize(preferred));
        let name = if self.taken.contains(&preferred) {
            let base = fit(&sanitize(fallback));
            let mut name = base.clone();
            let mut i = 2u32;
            while self.taken.contains(&name) {
                let suffix = format!("_{i}");
                let keep = base.len().min(MAX_TOOL_NAME - suffix.len());
                name = format!("{}{suffix}", &base[..keep]);
                i = i.saturating_add(1);
            }
            name
        } else {
            preferred
        };
        self.taken.insert(name.clone());
        name
    }
}

/// Whether `name` is valid for OpenAI-compatible servers.
pub fn valid(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_TOOL_NAME && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_valid_and_unique() {
        assert_eq!(namespaced("linear", "create.issue"), "linear__create_issue");
        assert_eq!(sanitize("ünï"), "_n_");
        assert_eq!(sanitize(""), "tool");
        let long_a = namespaced("some-plugin", &"a".repeat(80));
        let long_b = namespaced("some-plugin", &format!("{}b", "a".repeat(79)));
        assert!(valid(&long_a) && valid(&long_b) && long_a.len() == 64);
        assert_ne!(long_a, long_b);

        let mut names = ToolNames::new(["read_file"]);
        assert_eq!(names.claim("count", "p__count"), "count");
        assert_eq!(names.claim("count", "p__count"), "p__count");
        assert_eq!(names.claim("count", "p__count"), "p__count_2");
        assert_eq!(names.claim("read_file", "p__read_file"), "p__read_file");
        let long = "x".repeat(64);
        assert_eq!(names.claim(&long, &long), long);
        let next = names.claim(&long, &long);
        assert!(valid(&next) && next.ends_with("_2"));
    }
}
