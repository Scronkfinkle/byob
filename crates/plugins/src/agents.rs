//! Subagent definitions: Markdown files with YAML frontmatter, compatible with Claude Code's
//! `.claude/agents/*.md`. The frontmatter has `name`, `description`, optional `tools` (a
//! comma list or a YAML list) and optional `model`; the body is the subagent's system
//! prompt. Unknown keys (`color`, `permissionMode`, …) are kept in [`AgentDef::meta`] and
//! otherwise ignored, so files written for Claude Code load unchanged.
//!
//! byob looks in `<workspace>/.byob/agents`, `<workspace>/.claude/agents`, `<config>/agents`
//! and each plugin's `agents/` folder (see [`crate::Plugin::agents`]).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::LoadError;
use crate::skills::{frontmatter, read_capped, slug};

/// Biggest agent file we read.
const MAX_AGENT_BYTES: u64 = 256 * 1024;
/// Most agent files read from one folder.
const MAX_AGENTS_PER_DIR: usize = 200;
/// Longest description kept (they go into every system prompt).
const MAX_DESCRIPTION_CHARS: usize = 1000;

/// One subagent definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentDef {
    /// `[a-z0-9][a-z0-9_-]*`, at most 64 characters; what `delegate { agent }` names.
    pub name: String,
    /// When to use it (listed in the main system prompt).
    pub description: String,
    /// Tool names it may use as written (Claude Code names like `Read` included; see
    /// [`tool_aliases`]). `None`: every tool.
    pub tools: Option<Vec<String>>,
    /// A model role (`fast`), a raw model id, or Claude Code's `inherit`/`sonnet`/… .
    pub model: Option<String>,
    /// The system prompt (the file's body).
    pub prompt: String,
    /// The file it came from.
    pub source: PathBuf,
    /// Frontmatter keys byob doesn't use, as text.
    pub meta: BTreeMap<String, String>,
}

/// `[a-z0-9][a-z0-9_-]{0,63}`
pub fn agent_name_ok(n: &str) -> bool {
    let b = n.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_')
}

/// A name as written (`Code Reviewer`) → a valid agent name (`code-reviewer`).
pub fn agent_name(n: &str) -> String {
    let n = n.trim();
    if agent_name_ok(n) { n.to_owned() } else { slug(n) }
}

/// Parse an agent file. `file` names the fallback agent (its stem) when the frontmatter has
/// no `name`; a missing `description` falls back to the body's first line.
pub fn parse_agent(text: &str, file: &Path) -> Result<AgentDef, String> {
    let shown = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let stem = file.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (mut meta, body) = frontmatter(text);
    let body = body.trim();
    let name = agent_name(&meta.remove("name").filter(|n| !n.trim().is_empty()).unwrap_or(stem));
    if !agent_name_ok(&name) {
        return Err(format!("{shown}: the agent needs a name of letters, digits and dashes"));
    }
    if body.is_empty() {
        return Err(format!("{shown}: the file has no instructions after the frontmatter"));
    }
    let description = meta
        .remove("description")
        .map(|d| d.replace("\\n", " "))
        .filter(|d| !d.trim().is_empty())
        .or_else(|| body.lines().map(|l| l.trim().trim_start_matches('#').trim()).find(|l| !l.is_empty()).map(str::to_owned))
        .unwrap_or_default();
    let description: String = description.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(MAX_DESCRIPTION_CHARS).collect();
    let tools = meta.remove("tools").map(|t| parse_tools(&t)).filter(|t| !t.is_empty());
    let model = meta.remove("model").map(|m| m.trim().to_owned()).filter(|m| !m.is_empty());
    Ok(AgentDef { name, description, tools, model, prompt: body.to_owned(), source: file.to_path_buf(), meta })
}

/// `Read, Grep` / `[Read, Grep]` / a YAML list (already joined by [`frontmatter`]) / space
/// separated. `Bash(git:*)` is Claude Code's pattern syntax: only the tool name counts.
fn parse_tools(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in raw.split([',', ' ', '\t', '\n']) {
        let name = part.split('(').next().unwrap_or_default().trim().trim_matches(['"', '\'', '[', ']']);
        // A pattern's inside (`git:*)`) is not a tool.
        if name.is_empty() || part.trim_start().starts_with(['*', ')']) || name.contains([':', '*', ')']) {
            continue;
        }
        if !out.iter().any(|t| t == name) {
            out.push(name.to_owned());
        }
    }
    out
}

/// What a Claude Code tool name means in byob. Names byob has (`read_file`) map to
/// themselves; Claude Code's own tools map to the closest byob tools; tools with no
/// counterpart (`WebSearch`, `Task`) map to nothing.
pub fn tool_aliases(name: &str) -> Vec<&str> {
    match name {
        "Read" | "NotebookRead" => vec!["read_file", "read_artifact", "look_at_image"],
        "Write" => vec!["write_file", "create_artifact"],
        "Edit" | "MultiEdit" => vec!["edit_file", "update_artifact"],
        "NotebookEdit" => vec!["write_file", "update_artifact"],
        "Glob" | "LS" => vec!["list_files", "search_files"],
        "Grep" => vec!["search_files"],
        "Bash" | "BashOutput" => vec!["run_command"],
        "Task" | "Agent" | "WebFetch" | "WebSearch" | "TodoWrite" | "KillBash" | "KillShell" | "ExitPlanMode" | "SlashCommand" | "Skill" => Vec::new(),
        other => vec![other],
    }
}

/// Every `<dir>/*.md` (sorted by file name). Bad files become errors; the rest still load.
/// A missing `dir` is simply empty. Later files with an existing name are reported and skipped.
pub fn load_agents_dir(dir: &Path) -> (Vec<AgentDef>, Vec<LoadError>) {
    let mut agents: Vec<AgentDef> = Vec::new();
    let mut errors = Vec::new();
    let Ok(rd) = fs::read_dir(dir) else { return (agents, errors) };
    let mut files: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x.eq_ignore_ascii_case("md")))
        .collect();
    files.sort();
    if files.len() > MAX_AGENTS_PER_DIR {
        errors.push(LoadError { path: dir.to_path_buf(), message: format!("only the first {MAX_AGENTS_PER_DIR} agent files are read") });
        files.truncate(MAX_AGENTS_PER_DIR);
    }
    for file in files {
        let parsed = read_capped(&file, MAX_AGENT_BYTES).and_then(|text| parse_agent(&text, &file));
        match parsed {
            Ok(a) if agents.iter().any(|b| b.name == a.name) => {
                errors.push(LoadError { path: file, message: format!("agent \"{}\" is defined twice in this folder", a.name) });
            }
            Ok(a) => agents.push(a),
            Err(message) => errors.push(LoadError { path: file, message }),
        }
    }
    (agents, errors)
}

/// Merge agent lists, earlier lists winning by name (workspace before user before plugins).
pub fn merge_agents(lists: impl IntoIterator<Item = Vec<AgentDef>>) -> Vec<AgentDef> {
    let mut out: Vec<AgentDef> = Vec::new();
    for list in lists {
        for a in list {
            if !out.iter().any(|b| b.name == a.name) {
                out.push(a);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> AgentDef {
        parse_agent(text, Path::new("/x/agents/file-name.md")).unwrap()
    }

    #[test]
    fn claude_code_sample_loads_unchanged() {
        let text = "---\nname: code-reviewer\ndescription: Expert code review specialist. Proactively reviews code for quality, security, and maintainability. Use immediately after writing or modifying code.\ntools: Read, Grep, Glob, Bash\nmodel: inherit\ncolor: blue\n---\n\nYou are a senior code reviewer ensuring high standards of code quality and security.\n\nWhen invoked:\n1. Run git diff to see recent changes\n";
        let a = parse(text);
        assert_eq!(a.name, "code-reviewer");
        assert!(a.description.starts_with("Expert code review specialist."));
        assert_eq!(a.tools.as_deref(), Some(&["Read".to_owned(), "Grep".into(), "Glob".into(), "Bash".into()][..]));
        assert_eq!(a.model.as_deref(), Some("inherit"));
        assert!(a.prompt.starts_with("You are a senior code reviewer"));
        assert_eq!(a.meta.get("color").map(String::as_str), Some("blue"));
    }

    #[test]
    fn tools_in_every_list_form() {
        let yaml = parse("---\nname: a\ndescription: d\ntools:\n  - Read\n  - \"Bash\"\n---\nbody");
        assert_eq!(yaml.tools.unwrap(), ["Read", "Bash"]);
        let inline = parse("---\nname: a\ndescription: d\ntools: [read_file, list_files]\n---\nbody");
        assert_eq!(inline.tools.unwrap(), ["read_file", "list_files"]);
        let patterns = parse("---\nname: a\ndescription: d\ntools: Bash(git diff:*), Read\n---\nbody");
        assert_eq!(patterns.tools.unwrap(), ["Bash", "Read"]);
        let spaces = parse("---\nname: a\ndescription: d\ntools: Read Grep\n---\nbody");
        assert_eq!(spaces.tools.unwrap(), ["Read", "Grep"]);
        // No tools key, or an empty one: every tool.
        assert_eq!(parse("---\nname: a\ndescription: d\n---\nbody").tools, None);
        assert_eq!(parse("---\nname: a\ndescription: d\ntools:\n---\nbody").tools, None);
    }

    #[test]
    fn missing_frontmatter_falls_back_to_the_file() {
        let a = parse("# Summariser\n\nSummarise the files you are given.");
        assert_eq!(a.name, "file-name");
        assert_eq!(a.description, "Summariser");
        assert_eq!((a.tools, a.model), (None, None));
        // Names are normalised; empty bodies refused.
        assert_eq!(parse("---\nname: Code Reviewer\n---\nx").name, "code-reviewer");
        assert!(parse_agent("---\nname: a\ndescription: d\n---\n  \n", Path::new("a.md")).unwrap_err().contains("no instructions"));
        assert!(parse_agent("---\nname: '!!!'\n---\nx", Path::new("!!!.md")).is_err());
    }

    #[test]
    fn claude_tools_map_to_yah_tools() {
        assert_eq!(tool_aliases("Read"), ["read_file", "read_artifact", "look_at_image"]);
        assert_eq!(tool_aliases("Bash"), ["run_command"]);
        assert!(tool_aliases("WebSearch").is_empty());
        assert_eq!(tool_aliases("run_python"), ["run_python"]);
    }

    #[test]
    fn folders_load_sorted_with_errors_kept_apart() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("b.md"), "---\nname: beta\ndescription: B\n---\nDo b.").unwrap();
        fs::write(d.path().join("a.md"), "---\nname: alpha\ndescription: A\n---\nDo a.").unwrap();
        fs::write(d.path().join("c.md"), "---\nname: alpha\ndescription: again\n---\nDo a.").unwrap();
        fs::write(d.path().join("empty.md"), "---\nname: e\n---\n").unwrap();
        fs::write(d.path().join("notes.txt"), "not an agent").unwrap();
        let (agents, errors) = load_agents_dir(d.path());
        assert_eq!(agents.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["alpha", "beta"]);
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(load_agents_dir(&d.path().join("missing")).0.is_empty());
        let merged = merge_agents([vec![agents[1].clone()], agents.clone()]);
        assert_eq!(merged.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["beta", "alpha"]);
    }
}
