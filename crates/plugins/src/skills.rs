//! Agent Skills (`skills/<name>/SKILL.md`): frontmatter + Markdown steps, as in hob,
//! Claude and Codex. Only names and one-line descriptions go in the system prompt
//! ([`skills_prompt`]); the model loads the full text with the [`UseSkillTool`].

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use yah_tools::{Access, Tool, ToolContext, ToolOutput, ToolSpec, parse_args};

use crate::LoadError;

/// Biggest SKILL.md we read.
const MAX_SKILL_BYTES: u64 = 256 * 1024;
/// Files listed after a skill's body.
const MAX_LISTED_FILES: usize = 30;
/// save_skill limits (as hob).
pub const MAX_SKILL_FILES: usize = 10;
pub const MAX_SKILL_FILE_BYTES: usize = 100_000;

/// Instructions for one kind of task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Skill {
    /// `[a-z0-9][a-z0-9-]{0,63}`; what the user types after `/`.
    pub name: String,
    pub description: String,
    /// The Markdown steps, frontmatter removed.
    pub body: String,
    /// The folder holding SKILL.md (scripts, templates sit next to it).
    pub dir: PathBuf,
    /// Its scripts may use the internet (`network: true`).
    pub network: bool,
    /// PyPI packages its scripts need, e.g. `yfinance==0.2.54`.
    pub packages: Vec<String>,
    /// Other frontmatter keys (`license`, `allowed-tools`, …), kept as text.
    pub meta: BTreeMap<String, String>,
}

/// `[a-z0-9][a-z0-9-]{0,63}`
pub fn skill_name_ok(n: &str) -> bool {
    let b = n.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

/// `"Weekly report!"` → `"weekly-report"`.
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in s.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            if dash && !out.is_empty() {
                out.push('-');
            }
            dash = false;
            out.push(c);
        } else {
            dash = true;
        }
    }
    out.truncate(64);
    out.trim_end_matches('-').to_string()
}

/// A name as typed (`/Weekly Report`) → a valid skill name.
pub fn skill_name(n: &str) -> String {
    let n = n.trim().trim_start_matches('/');
    if skill_name_ok(n) { n.to_string() } else { slug(n) }
}

fn unquote(v: &str) -> &str {
    let b = v.as_bytes();
    if b.len() >= 2 && (b[0] == b'"' || b[0] == b'\'') && b[b.len() - 1] == b[0] { &v[1..v.len() - 1] } else { v }
}

/// Split `---` frontmatter from the body. The small YAML subset skills use:
/// `key: value`, quoted values, folded/literal blocks (`>`/`|`) and `- item` lists
/// (joined with `, `).
pub fn frontmatter(text: &str) -> (BTreeMap<String, String>, &str) {
    let mut meta = BTreeMap::new();
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = text.strip_prefix("---\n").or_else(|| text.strip_prefix("---\r\n")) else { return (meta, text) };
    // the closing fence: a line that is exactly `---`
    let mut end = None;
    let mut pos = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            end = Some((pos, pos + line.len()));
            break;
        }
        pos += line.len();
    }
    let Some((head_end, body_start)) = end else { return (meta, text) };
    let lines: Vec<&str> = rest[..head_end].lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        let Some((key, value)) = key_value(line) else { continue };
        let mut v = value.trim().to_string();
        let continues = |l: &str| l.trim().is_empty() || l.starts_with([' ', '\t']);
        if matches!(v.as_str(), ">" | "|" | ">-" | "|-" | ">+" | "|+") {
            let mut block = Vec::new();
            while i < lines.len() && continues(lines[i]) {
                block.push(lines[i].trim());
                i += 1;
            }
            v = block.join(if v.starts_with('>') { " " } else { "\n" }).trim().to_string();
        } else if v.is_empty() {
            let mut items = Vec::new();
            while i < lines.len() && (continues(lines[i]) || lines[i].starts_with('-')) {
                if let Some(item) = lines[i].trim().strip_prefix('-') {
                    items.push(unquote(item.trim()).to_string());
                }
                i += 1;
            }
            v = items.join(", ");
        } else if let Some(inner) = v.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            v = inner.split(',').map(|s| unquote(s.trim())).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(", ");
        }
        meta.insert(key.to_string(), unquote(&v).to_string());
    }
    (meta, &rest[body_start..])
}

/// `key: value` with `key` = `[A-Za-z][\w-]*`.
fn key_value(line: &str) -> Option<(&str, &str)> {
    let (k, v) = line.split_once(':')?;
    let mut chars = k.chars();
    let ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic()) && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    ok.then_some((k, v))
}

/// Parse a SKILL.md. `dir` is the folder it's in (its name is the fallback skill name).
pub fn parse_skill(text: &str, dir: &Path) -> Result<Skill, String> {
    let folder = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (mut meta, body) = frontmatter(text);
    let name = meta.remove("name").filter(|n| !n.is_empty()).unwrap_or_else(|| folder.clone());
    if !skill_name_ok(&name) {
        return Err(format!("{folder}/SKILL.md: name must be lowercase words with dashes, like \"plan-trip\""));
    }
    let description = meta.remove("description").unwrap_or_default();
    if description.trim().is_empty() {
        return Err(format!("{folder}/SKILL.md: add a description in the frontmatter so the assistant knows when to use it"));
    }
    let network = meta.remove("network").is_some_and(|n| n == "true");
    let packages = meta.remove("packages").map(|p| p.split([',', ' ', '\t', '\n']).filter(|s| !s.is_empty()).map(str::to_string).collect()).unwrap_or_default();
    Ok(Skill { name, description, body: body.trim().to_string(), dir: dir.to_path_buf(), network, packages, meta })
}

/// Read one `<dir>/SKILL.md`.
pub fn load_skill(dir: &Path) -> Result<Skill, String> {
    let file = dir.join("SKILL.md");
    let text = read_capped(&file, MAX_SKILL_BYTES)?;
    parse_skill(&text, dir)
}

pub(crate) fn read_capped(file: &Path, max: u64) -> Result<String, String> {
    let len = fs::metadata(file).map_err(|e| format!("{}: {e}", file.display()))?.len();
    if len > max {
        return Err(format!("{}: too big ({len} bytes, at most {max})", file.display()));
    }
    fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))
}

/// Every `<root>/<name>/SKILL.md`, sorted by folder name. Bad skills become errors;
/// the others still load. A missing `root` is simply empty.
pub fn load_skills_dir(root: &Path) -> (Vec<Skill>, Vec<LoadError>) {
    let mut skills = Vec::new();
    let mut errors = Vec::new();
    for dir in crate::discover::subdirs(root) {
        if !dir.join("SKILL.md").is_file() {
            continue;
        }
        match load_skill(&dir) {
            Ok(s) => skills.push(s),
            Err(message) => errors.push(LoadError { path: dir, message }),
        }
    }
    (skills, errors)
}

/// The system-prompt lines listing skills (hob's wording). Empty when there are none.
pub fn skills_prompt(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = skills
        .iter()
        .map(|s| {
            let d: String = s.description.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(160).collect();
            format!("- {}: {d}", s.name)
        })
        .collect();
    format!(
        "Skills (step-by-step know-how for certain tasks). When a request matches one, call use_skill with its name first and follow it:\n{}",
        lines.join("\n")
    )
}

/// Files next to SKILL.md (relative, `/`-separated), dot files skipped, at most `max`.
fn files_in(dir: &Path, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), String::new(), 0usize)];
    while let Some((d, prefix, depth)) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries.into_iter().rev() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || (depth == 0 && name == "SKILL.md") {
                continue;
            }
            let rel = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                if depth < 4 {
                    stack.push((e.path(), rel, depth + 1));
                }
            } else {
                out.push(rel);
            }
        }
    }
    out.sort();
    out.truncate(max);
    out
}

/// `use_skill`: returns a skill's full instructions. Rebuild it when skills change.
#[derive(Debug, Clone, Default)]
pub struct UseSkillTool {
    skills: Arc<Vec<Skill>>,
}

impl UseSkillTool {
    pub fn new(skills: Vec<Skill>) -> Self {
        Self { skills: Arc::new(skills) }
    }

    pub fn find(&self, name: &str) -> Option<&Skill> {
        let name = name.trim().trim_start_matches('/');
        self.skills.iter().find(|s| s.name == name)
    }
}

impl Tool for UseSkillTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "use_skill".into(),
            description: "Get the step-by-step instructions of one of your skills (listed in your instructions) before doing that kind of task.".into(),
            parameters: json!({"type": "object", "properties": {"name": {"type": "string", "description": "The skill name, e.g. plan-trip"}}, "required": ["name"]}),
        }
    }

    fn title(&self, args: &Value) -> String {
        let n: String = args.get("name").and_then(Value::as_str).unwrap_or("").replace('-', " ").chars().take(64).collect();
        format!("Used the {n} skill")
    }

    fn access(&self, _args: &Value) -> Access {
        Access::Read
    }

    fn run(&self, args: &Value, _ctx: &ToolContext<'_>) -> ToolOutput {
        let asked = args.get("name").and_then(Value::as_str).unwrap_or("");
        let Some(s) = self.find(asked) else {
            let have: Vec<&str> = self.skills.iter().map(|s| s.name.as_str()).collect();
            let have = if have.is_empty() { "none".to_string() } else { have.join(", ") };
            let asked: String = asked.chars().take(64).collect();
            return ToolOutput::err(format!("There's no skill called “{asked}”. Skills you have: {have}."));
        };
        let files = files_in(&s.dir, MAX_LISTED_FILES);
        let files = if files.is_empty() { String::new() } else { format!("; files: {}", files.join(", ")) };
        ToolOutput::ok(format!("{}\n\n(Skill folder: {}{files})", s.body, s.dir.display()))
    }
}

/// What to write for a new skill.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct NewSkill {
    pub name: String,
    pub description: String,
    pub instructions: String,
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    #[serde(default)]
    pub packages: Vec<String>,
    #[serde(default)]
    pub network: bool,
    #[serde(default)]
    pub replace: bool,
}

/// A SKILL.md for `s` (hob's layout).
pub fn skill_file(name: &str, s: &NewSkill) -> String {
    let desc = s.description.split_whitespace().collect::<Vec<_>>().join(" ");
    let desc = serde_json::to_string(&desc).unwrap_or_else(|_| "\"\"".into());
    let network = if s.network { "network: true\n" } else { "" };
    let packages = if s.packages.is_empty() { String::new() } else { format!("packages: {}\n", s.packages.join(", ")) };
    format!("---\nname: {name}\ndescription: {desc}\n{network}{packages}---\n{}\n", s.instructions.trim())
}

/// Why `files` can't be kept with a skill; `None` if they can.
pub fn skill_files_problem(files: &BTreeMap<String, String>) -> Option<String> {
    if files.len() > MAX_SKILL_FILES {
        return Some(format!("A skill can keep up to {MAX_SKILL_FILES} files."));
    }
    for (n, text) in files {
        if n == "SKILL.md" || !skill_file_name_ok(n) {
            return Some(format!("“{n}” can't be saved with a skill: use a plain file name ending in .py, .md, .txt, .json or .csv."));
        }
        if text.len() > MAX_SKILL_FILE_BYTES {
            return Some(format!("“{n}” is too big to keep with a skill (100 KB at most)."));
        }
    }
    None
}

/// `[A-Za-z0-9_][A-Za-z0-9_.-]{0,63}\.(py|md|txt|json|csv)`
fn skill_file_name_ok(n: &str) -> bool {
    let Some((stem, ext)) = n.rsplit_once('.') else { return false };
    let b = stem.as_bytes();
    matches!(ext, "py" | "md" | "txt" | "json" | "csv")
        && !b.is_empty()
        && b.len() <= 64
        && (b[0].is_ascii_alphanumeric() || b[0] == b'_')
        && b.iter().all(|&c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'))
}

/// A PyPI requirement we accept: `name` or `name==version` with plain characters.
fn package_ok(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= 100
        && p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '=' | '[' | ']' | ',' | '<' | '>' | '~' | '!'))
}

/// Write a skill to `<skills_root>/<name>/`. Returns the saved name.
pub fn save_skill(skills_root: &Path, s: &NewSkill) -> Result<String, String> {
    let name = skill_name(&s.name);
    if name.is_empty() {
        return Err("Give the skill a name with letters or numbers.".into());
    }
    let own = skills_root.join(&name);
    if !s.replace && own.join("SKILL.md").exists() {
        return Err(format!("There's already a skill called /{name}. Pick another name."));
    }
    if s.description.trim().is_empty() {
        return Err("Say when to use this skill.".into());
    }
    if s.instructions.trim().is_empty() {
        return Err("Write the instructions for this skill.".into());
    }
    if let Some(p) = skill_files_problem(&s.files) {
        return Err(p);
    }
    if let Some(p) = s.packages.iter().find(|p| !package_ok(p)) {
        return Err(format!("“{p}” isn't a package name like pandas or yfinance==0.2.54."));
    }
    fs::create_dir_all(&own).map_err(|e| format!("Couldn't save the skill: {e}"))?;
    for (f, text) in &s.files {
        fs::write(own.join(f), text).map_err(|e| format!("Couldn't save {f}: {e}"))?;
    }
    let instructions = with_run_line(&name, &s.instructions, &s.files);
    fs::write(own.join("SKILL.md"), skill_file(&name, &NewSkill { instructions, ..s.clone() })).map_err(|e| format!("Couldn't save the skill: {e}"))?;
    Ok(name)
}

/// Small models leave out how to run a skill's scripts; add a line saying so.
fn with_run_line(name: &str, instructions: &str, files: &BTreeMap<String, String>) -> String {
    let missing: Vec<String> =
        files.keys().filter(|f| f.ends_with(".py") && !instructions.contains(&format!("{name}/{f}"))).map(|f| format!("script \"{name}/{f}\"")).collect();
    if missing.is_empty() {
        return instructions.to_string();
    }
    format!("{}\n\nScripts in this skill: run them with run_python, passing {} and the inputs as args.", instructions.trim(), missing.join(" or "))
}

/// `save_skill`: writes a skill the user asked to keep. It changes later behaviour, so the
/// agent should always ask first (hob also refuses after untrusted web content in the
/// same message; that check belongs to the agent, which knows).
#[derive(Debug, Clone)]
pub struct SaveSkillTool {
    skills_root: PathBuf,
}

impl SaveSkillTool {
    /// `skills_root`: where saved skills go, e.g. `<config>/plugins/my-skills/skills`.
    pub fn new(skills_root: PathBuf) -> Self {
        Self { skills_root }
    }
}

impl Tool for SaveSkillTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "save_skill".into(),
            description:
                "Save how to do a task as a skill the user can run later with /name: steps, plus the code that worked as script files. Only when the user asks."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Short name, lowercase words with dashes, e.g. weekly-report"},
                    "description": {"type": "string", "description": "One sentence: when to use this skill"},
                    "instructions": {"type": "string", "description": "Clear step-by-step instructions in Markdown. Say which script to run and what to pass it."},
                    "files": {"type": "object", "description": "Scripts and data to keep with the skill: file name → contents, e.g. {\"make_report.py\": \"...\"}. Use code that already worked in this chat. Scripts get their inputs from sys.argv and save results in the current folder.", "additionalProperties": {"type": "string"}},
                    "packages": {"type": "array", "items": {"type": "string"}, "description": "PyPI packages the scripts need that you installed with install_packages"},
                    "network": {"type": "boolean", "description": "true only if the scripts need the internet"},
                    "replace": {"type": "boolean", "description": "true to update an existing skill with this name"}
                },
                "required": ["name", "description", "instructions"]
            }),
        }
    }

    fn title(&self, args: &Value) -> String {
        format!("Saved the “{}” skill", skill_name(args.get("name").and_then(Value::as_str).unwrap_or("")))
    }

    /// Writes outside the workspace and changes later behaviour: always ask.
    fn access(&self, _args: &Value) -> Access {
        Access::Write
    }

    fn run(&self, args: &Value, _ctx: &ToolContext<'_>) -> ToolOutput {
        let s: NewSkill = match parse_args(args) {
            Ok(s) => s,
            Err(e) => return ToolOutput::err(e),
        };
        match save_skill(&self.skills_root, &s) {
            Ok(name) => ToolOutput::ok(format!("Saved. The user can run it with /{name}, and you'll see it in your skills list.")),
            Err(e) => ToolOutput::err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn frontmatter_subset() {
        let text = "---\nname: fridge\ndescription: >\n  Suggest recipes\n  from the fridge.\nnetwork: true\npackages:\n  - yfinance==0.2.54\n  - 'ta'\nallowed-tools: [a, \"b\"]\nquoted: \"x: y\"\n---\nSteps here.\n";
        let (meta, body) = frontmatter(text);
        assert_eq!(meta["name"], "fridge");
        assert_eq!(meta["description"], "Suggest recipes from the fridge.");
        assert_eq!(meta["packages"], "yfinance==0.2.54, ta");
        assert_eq!(meta["allowed-tools"], "a, b");
        assert_eq!(meta["quoted"], "x: y");
        assert_eq!(body, "Steps here.\n");
        let (meta, body) = frontmatter("no frontmatter");
        assert!(meta.is_empty());
        assert_eq!(body, "no frontmatter");
        // unterminated: whole text is the body
        assert_eq!(frontmatter("---\nname: x\n").1, "---\nname: x\n");
        // literal block, CRLF
        let (meta, _) = frontmatter("---\r\ndescription: |\r\n  a\r\n  b\r\n---\r\nbody");
        assert_eq!(meta["description"], "a\nb");
    }

    #[test]
    fn parse_skill_rules() {
        let dir = Path::new("/p/skills/fridge");
        let s = parse_skill("---\ndescription: Recipes.\npackages: a==1, b\nlicense: MIT\n---\n\n1. Do.\n", dir).unwrap();
        assert_eq!(s.name, "fridge");
        assert_eq!(s.packages, ["a==1", "b"]);
        assert_eq!(s.body, "1. Do.");
        assert_eq!(s.meta["license"], "MIT");
        assert!(!s.network);
        assert_eq!(
            parse_skill("---\nname: Bad Name\ndescription: d\n---\n", dir).unwrap_err(),
            "fridge/SKILL.md: name must be lowercase words with dashes, like \"plan-trip\""
        );
        assert!(parse_skill("---\nname: ok\n---\n", dir).unwrap_err().contains("add a description"));
    }

    #[test]
    fn slugs() {
        assert_eq!(slug("Weekly report!"), "weekly-report");
        assert_eq!(slug("  --Hi__there-- "), "hi-there");
        assert_eq!(skill_name("/plan-trip"), "plan-trip");
        assert_eq!(slug(&"a".repeat(100)).len(), 64);
    }

    #[test]
    fn prompt_lists_skills() {
        assert_eq!(skills_prompt(&[]), "");
        let s = Skill {
            name: "fridge".into(),
            description: "Recipes\n  from   fridge.".into(),
            body: String::new(),
            dir: PathBuf::new(),
            network: false,
            packages: vec![],
            meta: BTreeMap::new(),
        };
        let p = skills_prompt(&[s]);
        assert!(p.starts_with("Skills (step-by-step"));
        assert!(p.ends_with("\n- fridge: Recipes from fridge."));
    }

    #[test]
    fn save_then_use() {
        let root = std::env::temp_dir().join(format!("yah-plugins-save-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let new = NewSkill {
            name: "Weekly Report".into(),
            description: "Make the weekly report.".into(),
            instructions: "1. Run it.".into(),
            files: BTreeMap::from([("make.py".to_string(), "print(1)".to_string())]),
            packages: vec!["pandas".into()],
            ..Default::default()
        };
        assert_eq!(save_skill(&root, &new).unwrap(), "weekly-report");
        assert!(save_skill(&root, &new).unwrap_err().contains("already a skill"));
        assert!(save_skill(&root, &NewSkill { replace: true, ..new.clone() }).is_ok());
        let bad = NewSkill { files: BTreeMap::from([("../x.py".to_string(), String::new())]), replace: true, ..new.clone() };
        assert!(save_skill(&root, &bad).unwrap_err().contains("can't be saved"));

        let (skills, errors) = load_skills_dir(&root);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(skills[0].packages, ["pandas"]);
        assert!(skills[0].body.contains("script \"weekly-report/make.py\""));

        let tool = UseSkillTool::new(skills);
        let cancel = AtomicBool::new(false);
        let ctx = ToolContext::new(&root, &cancel);
        let out = tool.run(&json!({"name": "/weekly-report"}), &ctx);
        assert!(!out.failed);
        assert!(out.text.starts_with("1. Run it.") && out.text.contains("files: make.py"), "{}", out.text);
        let miss = tool.run(&json!({"name": 5}), &ctx);
        assert!(miss.failed && miss.text.contains("Skills you have: weekly-report"));
        assert_eq!(tool.access(&json!({})), Access::Read);
        assert_eq!(tool.title(&json!({"name": "plan-trip"})), "Used the plan trip skill");
        let _ = fs::remove_dir_all(&root);
    }
}
