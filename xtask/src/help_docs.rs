//! `cargo xtask help-docs`: the docs `byob_help` answers from (crate `byob-help`) are copies
//! of byob's user docs (README.md and docs/*.md), kept inside the crate so `cargo publish`
//! packages them. The repo's files are the one source of truth: this writes the copies, and
//! `--check` (part of `cargo xtask ci`) fails when a copy no longer matches its source.

use std::path::Path;

/// Where the copies go, relative to the repo root.
pub const DIR: &str = "crates/help/docs";

/// Which `## ` sections of a source to copy.
pub enum Pick {
    /// Only these (in the source's order).
    Keep(&'static [&'static str]),
    /// All but these (developer notes: crates, tests, measurements, plans).
    Drop(&'static [&'static str]),
}

/// One copy: its file name in [`DIR`], its source (relative to the root), what to keep.
pub struct Doc {
    pub file: &'static str,
    pub source: &'static str,
    pub pick: Pick,
}

/// The user-facing docs. docs/architecture.md and docs/releasing.md are for developers and
/// stay out; so do the README's Develop section and demos/ (they need a source checkout).
pub const DOCS: &[Doc] = &[
    Doc { file: "byob.md", source: "README.md", pick: Pick::Keep(&["What it does", "Install", "Use", "Plugins"]) },
    Doc { file: "mail.md", source: "docs/mail.md", pick: Pick::Drop(&["Crates", "Tests"]) },
    Doc { file: "calendar.md", source: "docs/calendar.md", pick: Pick::Drop(&["Testing"]) },
    Doc { file: "environments.md", source: "docs/environments.md", pick: Pick::Drop(&["Done", "Plan for the rest", "Decisions for the user"]) },
    Doc { file: "views.md", source: "docs/views.md", pick: Pick::Drop(&[]) },
    Doc { file: "plugin-backends.md", source: "docs/plugins-backend.md", pick: Pick::Drop(&["Measured"]) },
];

/// The copy of `text` (from `source`): a generated-file note, then the picked sections, without
/// HTML blocks or images (they don't read as text).
pub fn render(source: &str, text: &str, pick: &Pick) -> String {
    let mut out = format!("<!-- Generated from {source} by `cargo xtask help-docs`. Edit {source}, not this copy. -->\n");
    // a Keep source's preamble (before its title) goes too
    let mut keep = matches!(pick, Pick::Drop(_));
    let mut fence = false;
    // a README's title is replaced by a plain one, written whether or not it has a title
    let mut first = !matches!(pick, Pick::Keep(_));
    if !first {
        out.push_str("# byob\n\n");
    }
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with("```") {
            fence = !fence;
        }
        if !fence && !t.starts_with("```") {
            if let Some(h) = line.strip_prefix("## ") {
                let h = h.trim();
                keep = match pick {
                    Pick::Keep(names) => names.contains(&h),
                    Pick::Drop(names) => !names.contains(&h),
                };
            } else if first && line.starts_with("# ") {
                // the title, always
                first = false;
                out.push_str(line);
                out.push('\n');
                keep = true;
                continue;
            }
            if html_line(t) || t.starts_with("![") || t.starts_with("> [!") {
                continue;
            }
        }
        if keep {
            out.push_str(line);
            out.push('\n');
        }
    }
    // no runs of blank lines where blocks were left out
    while out.contains("\n\n\n") {
        out = out.replace("\n\n\n", "\n\n");
    }
    out
}

/// A line of an HTML block (`<table>`, `<p align=…>`, `</td>`, …), not prose that starts
/// with a `<placeholder>`.
fn html_line(t: &str) -> bool {
    const TAGS: &[&str] = &["table", "tr", "td", "th", "p", "img", "sub", "br", "div", "picture", "source", "details", "summary", "!--"];
    let Some(rest) = t.strip_prefix('<') else { return false };
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    TAGS.iter().any(|tag| rest.strip_prefix(tag).is_some_and(|after| after.starts_with([' ', '>', '/']) || (*tag == "!--")))
}

/// What is out of date under `root`: one line per stale, missing or extra copy.
pub fn stale(root: &Path) -> Result<Vec<String>, String> {
    let mut problems = Vec::new();
    for d in DOCS {
        let src = std::fs::read_to_string(root.join(d.source)).map_err(|e| format!("read {}: {e}", d.source))?;
        let want = render(d.source, &src, &d.pick);
        match std::fs::read_to_string(root.join(DIR).join(d.file)) {
            Ok(have) if have == want => {}
            Ok(_) => problems.push(format!("{DIR}/{} is out of date with {}", d.file, d.source)),
            Err(_) => problems.push(format!("{DIR}/{} is missing (from {})", d.file, d.source)),
        }
    }
    if let Ok(rd) = std::fs::read_dir(root.join(DIR)) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !DOCS.iter().any(|d| d.file == name) {
                problems.push(format!("{DIR}/{name} isn't one of xtask's help docs"));
            }
        }
    }
    problems.sort();
    Ok(problems)
}

/// Write every copy under `root` (and remove ones no longer listed).
pub fn write(root: &Path) -> Result<(), String> {
    let dir = root.join(DIR);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {DIR}: {e}"))?;
    for d in DOCS {
        let src = std::fs::read_to_string(root.join(d.source)).map_err(|e| format!("read {}: {e}", d.source))?;
        std::fs::write(dir.join(d.file), render(d.source, &src, &d.pick)).map_err(|e| format!("write {DIR}/{}: {e}", d.file))?;
    }
    for e in std::fs::read_dir(&dir).map_err(|e| format!("read {DIR}: {e}"))?.flatten() {
        if !DOCS.iter().any(|d| *d.file == *e.file_name().to_string_lossy()) {
            std::fs::remove_file(e.path()).map_err(|err| format!("remove {}: {err}", e.path().display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const README: &str = "> [!NOTE]\n> AI.\n\n# bring your own brain\n\nIntro.\n\n![hero](x.gif)\n\n## What it does\n\n- a\n\n<table>\n  <tr><td>x</td></tr>\n</table>\n\n## Install\n\n```sh\n# a comment, not a title\n## nor a section\ncargo install byob\n```\n\n## Develop\n\nnix develop\n";

    #[test]
    fn keeps_and_drops_sections_outside_code_fences() {
        let out = render("README.md", README, &Pick::Keep(&["What it does", "Install"]));
        assert_eq!(
            out,
            "<!-- Generated from README.md by `cargo xtask help-docs`. Edit README.md, not this copy. -->\n# byob\n\n## What it does\n\n- a\n\n## Install\n\n```sh\n# a comment, not a title\n## nor a section\ncargo install byob\n```\n\n"
        );
        // a README whose title is in its picture still gets one
        let untitled = README.replace("# bring your own brain\n\n", "<p align=\"center\"><img src=\"p.png\"></p>\n\n");
        assert_eq!(render("README.md", &untitled, &Pick::Keep(&["What it does", "Install"])), out);
        let doc = "# Mail\n\nIntro.\n\n## Settings\n\nset\n\n## Tests\n\ncargo test\n\n### Live\n\nlive\n\n## Gmail\n\napp password\n";
        let out = render("docs/mail.md", doc, &Pick::Drop(&["Tests"]));
        assert!(out.contains("# Mail\n\nIntro.\n\n## Settings\n\nset\n\n## Gmail\n\napp password\n"), "{out}");
        assert!(!out.contains("cargo test") && !out.contains("live"), "a dropped section takes its subsections: {out}");
        // prose that starts with a <placeholder> isn't an HTML block
        let out = render("docs/x.md", "# X\n\nsays (\"ask\",\n<message>\") here\n<p align=\"center\"><img src=\"a.png\"></p>\n", &Pick::Drop(&[]));
        assert!(out.ends_with("# X\n\nsays (\"ask\",\n<message>\") here\n"), "{out}");
    }

    #[test]
    fn the_check_finds_stale_missing_and_extra_copies() {
        let root = std::env::temp_dir().join(format!("byob-xtask-help-docs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("README.md"), README).unwrap();
        for d in DOCS.iter().filter(|d| d.source != "README.md") {
            std::fs::write(root.join(d.source), format!("# {}\n\n## Use\n\ntext\n", d.file)).unwrap();
        }
        assert_eq!(stale(&root).unwrap().len(), DOCS.len(), "all missing");
        write(&root).unwrap();
        assert!(stale(&root).unwrap().is_empty(), "{:?}", stale(&root));
        // the source changes: its copy is stale until written again
        std::fs::write(root.join("docs/mail.md"), "# Mail\n\n## Gmail\n\nUse an app password.\n").unwrap();
        std::fs::write(root.join(DIR).join("old.md"), "x").unwrap();
        assert_eq!(stale(&root).unwrap(), [format!("{DIR}/mail.md is out of date with docs/mail.md"), format!("{DIR}/old.md isn't one of xtask's help docs")]);
        write(&root).unwrap();
        assert!(stale(&root).unwrap().is_empty());
        assert!(std::fs::read_to_string(root.join(DIR).join("mail.md")).unwrap().contains("app password"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The repo's own copies are current (also checked by `cargo xtask ci`).
    #[test]
    fn the_repos_copies_are_current() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let problems = stale(&root).unwrap();
        assert!(problems.is_empty(), "run `cargo xtask help-docs`: {problems:?}");
    }
}
