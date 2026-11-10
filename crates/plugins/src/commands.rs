//! Slash commands: a plugin's `commands/<name>.md`, in Claude Code's format. The file's
//! body is a prompt template; the user types `/<name> some arguments` and the model gets
//! the template with `$ARGUMENTS` (all of them) and `$1`…`$9` (split on whitespace) filled
//! in. Optional frontmatter: `description` (shown in the `/` menu) and `argument-hint`.
//! Other keys (`allowed-tools`, `model`) are kept but not used.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::LoadError;
use crate::skills::{frontmatter, read_capped, skill_name_ok};

/// Biggest command file read.
pub const MAX_COMMAND_BYTES: u64 = 64 * 1024;
/// Most command files read from one folder.
pub const MAX_COMMANDS_PER_DIR: usize = 100;

/// One slash command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// `[a-z0-9][a-z0-9-]{0,63}`, from the file name; what the user types after `/`.
    pub name: String,
    pub description: String,
    /// What to type after the name, e.g. `<file> [focus]`.
    pub argument_hint: String,
    /// The prompt template, frontmatter removed.
    pub body: String,
    pub file: PathBuf,
    /// Other frontmatter keys, kept as text.
    pub meta: BTreeMap<String, String>,
}

impl Command {
    /// The prompt for `/name args`: `$ARGUMENTS` and `$1`…`$9` filled in. Arguments the
    /// template doesn't mention are added at the end, as Claude Code does.
    pub fn expand(&self, args: &str) -> String {
        let args = args.trim();
        let words: Vec<&str> = args.split_whitespace().collect();
        let mut out = String::with_capacity(self.body.len() + args.len());
        let mut used = false;
        let mut rest = self.body.as_str();
        while let Some(i) = rest.find('$') {
            out.push_str(rest.get(..i).unwrap_or_default());
            let after = rest.get(i + 1..).unwrap_or_default();
            if let Some(tail) = after.strip_prefix("ARGUMENTS") {
                out.push_str(args);
                used = true;
                rest = tail;
            } else if let Some(d) = after.chars().next().and_then(|c| c.to_digit(10)).filter(|d| *d >= 1) {
                let n = usize::try_from(d).unwrap_or(1);
                out.push_str(words.get(n - 1).copied().unwrap_or_default());
                used = true;
                rest = after.get(1..).unwrap_or_default();
            } else {
                out.push('$');
                rest = after;
            }
        }
        out.push_str(rest);
        let mut out = out.trim().to_owned();
        if !used && !args.is_empty() {
            out.push_str("\n\nARGUMENTS: ");
            out.push_str(args);
        }
        out
    }
}

/// Parse a command file (`file` gives the name: `review-pr.md` → `review-pr`).
pub fn parse_command(text: &str, file: &Path) -> Result<Command, String> {
    let stem = file.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let shown = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let name = stem.to_lowercase();
    if !skill_name_ok(&name) {
        return Err(format!("commands/{shown}: name the file with lowercase words and dashes, like \"review-pr.md\""));
    }
    let (mut meta, body) = frontmatter(text);
    let body = body.trim().to_owned();
    if body.is_empty() {
        return Err(format!("commands/{shown}: the file has no prompt"));
    }
    let description = meta.remove("description").filter(|d| !d.trim().is_empty()).unwrap_or_else(|| first_line(&body));
    let argument_hint = meta.remove("argument-hint").unwrap_or_default();
    Ok(Command { name, description, argument_hint, body, file: file.to_path_buf(), meta })
}

/// The first non-empty line, without Markdown heading marks, at most 120 characters.
fn first_line(body: &str) -> String {
    let line = body.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
    line.trim_start_matches('#').trim().chars().take(120).collect()
}

/// Every `<dir>/*.md`, sorted by name. Bad files become errors; the others still load. A
/// missing `dir` is simply empty.
pub fn load_commands_dir(dir: &Path) -> (Vec<Command>, Vec<LoadError>) {
    let mut commands: Vec<Command> = Vec::new();
    let mut errors = Vec::new();
    let Ok(rd) = fs::read_dir(dir) else { return (commands, errors) };
    let mut files: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x.eq_ignore_ascii_case("md")))
        .collect();
    files.sort();
    if files.len() > MAX_COMMANDS_PER_DIR {
        errors.push(LoadError { path: dir.to_path_buf(), message: format!("only the first {MAX_COMMANDS_PER_DIR} command files are read") });
        files.truncate(MAX_COMMANDS_PER_DIR);
    }
    for file in files {
        match read_capped(&file, MAX_COMMAND_BYTES).and_then(|text| parse_command(&text, &file)) {
            Ok(c) if commands.iter().any(|d| d.name == c.name) => {
                errors.push(LoadError { path: file, message: format!("command \"{}\" is defined twice", c.name) });
            }
            Ok(c) => commands.push(c),
            Err(message) => errors.push(LoadError { path: file, message }),
        }
    }
    (commands, errors)
}

/// `"/name some args"` → `("name", "some args")`; `None` if the text isn't a slash command
/// (no leading `/`, or not a valid name: `/usr/bin` is a path, not a command).
pub fn split_slash(text: &str) -> Option<(&str, &str)> {
    let rest = text.trim_start().strip_prefix('/')?;
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let (name, args) = rest.split_at(end);
    skill_name_ok(name).then_some((name, args.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(body: &str) -> Command {
        parse_command(body, Path::new("/p/commands/review.md")).unwrap()
    }

    #[test]
    fn templates_fill_in_arguments() {
        let c = cmd("---\ndescription: Review a file\nargument-hint: <file> [focus]\n---\nReview $1 with a focus on $2.\nAll: $ARGUMENTS; cost $5 stays.");
        assert_eq!((c.name.as_str(), c.description.as_str(), c.argument_hint.as_str()), ("review", "Review a file", "<file> [focus]"));
        assert_eq!(c.expand(" src/a.rs  errors "), "Review src/a.rs with a focus on errors.\nAll: src/a.rs  errors; cost  stays.");
        // No placeholders: the arguments go at the end; none given, nothing is added.
        let plain = cmd("# Summarize the chat\nBe brief.");
        assert_eq!(plain.description, "Summarize the chat");
        assert_eq!(plain.expand("in French"), "# Summarize the chat\nBe brief.\n\nARGUMENTS: in French");
        assert_eq!(plain.expand(""), "# Summarize the chat\nBe brief.");
        assert_eq!(cmd("price: $").expand("x"), "price: $\n\nARGUMENTS: x");
        assert!(parse_command("---\ndescription: x\n---\n", Path::new("/p/commands/empty.md")).is_err());
        assert!(parse_command("hi", Path::new("/p/commands/Bad Name.md")).is_err());
    }

    #[test]
    fn slash_text_splits_into_name_and_arguments() {
        assert_eq!(split_slash("/review src/a.rs now"), Some(("review", "src/a.rs now")));
        assert_eq!(split_slash("  /compact"), Some(("compact", "")));
        assert_eq!(split_slash("/review\nmore lines"), Some(("review", "more lines")));
        assert_eq!(split_slash("/usr/bin/env is a path"), None);
        assert_eq!(split_slash("no slash"), None);
        assert_eq!(split_slash("/"), None);
    }

    #[test]
    fn a_folder_loads_good_files_and_reports_bad_ones() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("b-one.md"), "Do B.").unwrap();
        fs::write(d.path().join("a-one.md"), "---\ndescription: A\n---\nDo A with $ARGUMENTS").unwrap();
        fs::write(d.path().join("Bad Name.md"), "x").unwrap();
        fs::write(d.path().join("notes.txt"), "ignored").unwrap();
        let (cmds, errors) = load_commands_dir(d.path());
        assert_eq!(cmds.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["a-one", "b-one"]);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(load_commands_dir(&d.path().join("missing")).0.is_empty());
    }
}
