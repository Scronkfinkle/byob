//! The permission gate: which tool calls run on their own, which ask the user, and which
//! are refused outright. The engine thread owns the session's "always allow" set.

use std::collections::BTreeSet;

use yah_protocol::ToolAccess;

/// What to do with calls of one [`ToolAccess`] level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// Run without asking.
    Allow,
    /// Show an approval card and wait for an answer.
    Ask,
    /// Never run; the model is told why.
    Deny,
}

/// Rule per access level. Default: reads run on their own, everything else asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub read: Rule,
    pub write: Rule,
    pub network: Rule,
    pub execute: Rule,
}

impl Default for Policy {
    fn default() -> Self {
        Self { read: Rule::Allow, write: Rule::Ask, network: Rule::Ask, execute: Rule::Ask }
    }
}

impl Policy {
    pub fn rule(&self, access: ToolAccess) -> Rule {
        match access {
            ToolAccess::Read => self.read,
            ToolAccess::Write => self.write,
            ToolAccess::Network => self.network,
            ToolAccess::Execute => self.execute,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    Allow,
    Ask,
    Deny(String),
}

/// Decide one call. "Always allow" (per tool, per session) skips the question but never
/// overrides a [`Rule::Deny`].
pub fn decide(policy: &Policy, always: &BTreeSet<String>, tool: &str, access: ToolAccess) -> Gate {
    match policy.rule(access) {
        Rule::Allow => Gate::Allow,
        Rule::Deny => Gate::Deny(format!("{} tools are turned off", access.label())),
        Rule::Ask if always.contains(tool) => Gate::Allow,
        Rule::Ask => Gate::Ask,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_allows_reads_and_asks_for_the_rest() {
        let p = Policy::default();
        let none = BTreeSet::new();
        assert_eq!(decide(&p, &none, "read_file", ToolAccess::Read), Gate::Allow);
        for a in [ToolAccess::Write, ToolAccess::Network, ToolAccess::Execute] {
            assert_eq!(decide(&p, &none, "x", a), Gate::Ask);
        }
    }

    #[test]
    fn always_skips_asking_but_not_deny() {
        let always = BTreeSet::from(["write_file".to_owned()]);
        let p = Policy { write: Rule::Ask, execute: Rule::Deny, ..Policy::default() };
        assert_eq!(decide(&p, &always, "write_file", ToolAccess::Write), Gate::Allow);
        assert_eq!(decide(&p, &always, "other", ToolAccess::Write), Gate::Ask);
        let always = BTreeSet::from(["run".to_owned()]);
        assert!(matches!(decide(&p, &always, "run", ToolAccess::Execute), Gate::Deny(_)));
    }
}
