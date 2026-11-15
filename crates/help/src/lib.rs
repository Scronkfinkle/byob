//! byob's user guide for the model: the bundled `help` plugin's backend, in-process. One
//! read-only tool, `byob_help(topic?, section?, start?, query?)`:
//!
//! - no arguments: the topics, one line each;
//! - `topic`: that doc, a window of [`MAX_CHARS`] at a time (`start`), or one `section`;
//! - `query`: the best-matching sections across every doc (keyword scoring).
//!
//! The docs are copies of byob's user docs (README.md's Install/Use parts, docs/mail.md, ...)
//! kept in this crate's `docs/` so a published crate carries them. `cargo xtask help-docs`
//! writes them from the repo's files (the one source of truth) and `cargo xtask ci` fails
//! when a copy is stale. They are byob's own text, so results aren't framed as outside data.
//!
//! The plugin's manifest (`crates/bundled/plugins/help`) names it with
//! `"backend": {"native": "help"}`; the app registers [`factory`] under [`ID`].
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

use std::fmt::Write as _;
use std::sync::Arc;

use byob_plugins::native::{NativeBackend, NativeFactory, Outbox};
use serde_json::Value;

/// The id manifests name (`"backend": {"native": "help"}`).
pub const ID: &str = "help";
/// The tool it serves.
pub const TOOL: &str = "byob_help";
/// Most characters of doc text one call returns.
pub const MAX_CHARS: usize = 8000;
/// Most sections a query returns.
pub const MAX_HITS: usize = 3;
/// Longest query looked at (characters).
const MAX_QUERY: usize = 300;

/// One doc.
pub struct Topic {
    /// What the model passes as `topic`.
    pub name: &'static str,
    /// One line for the topic list.
    pub summary: &'static str,
    /// Other words that pick it (`topic: "gmail"` is the mail doc).
    pub aliases: &'static [&'static str],
    /// The Markdown (from `docs/`, written by `cargo xtask help-docs`).
    pub text: &'static str,
}

/// Every doc, in the order they're listed.
pub static TOPICS: &[Topic] = &[
    Topic {
        name: "byob",
        summary: "Installing byob, connecting a model (Connect a model, presets, API keys), the command line, the browser UI and Tailscale, workspaces, plugins.",
        aliases: &["readme", "install", "setup", "use", "usage", "start", "getting-started"],
        text: include_str!("../docs/byob.md"),
    },
    Topic {
        name: "mail",
        summary: "The Mail plugin (IMAP/SMTP, one account): settings, Gmail (app password), Fastmail, iCloud, what each tool does, what always asks first.",
        aliases: &["email", "e-mail", "gmail", "imap", "smtp"],
        text: include_str!("../docs/mail.md"),
    },
    Topic {
        name: "calendar",
        summary: "The Calendar plugin (CalDAV, one account): settings for Fastmail, iCloud, Nextcloud, Radicale; tools, time zones, recurrence; why Google Calendar isn't supported.",
        aliases: &["calendars", "caldav", "events"],
        text: include_str!("../docs/calendar.md"),
    },
    Topic {
        name: "environments",
        summary: "Where commands run and how tools get installed: install_packages (Nix, pixi), project environments (flake.nix, pixi, mise, .venv, devcontainer), Docker/Podman containers.",
        aliases: &["environment", "containers", "docker", "podman", "nix", "pixi", "packages"],
        text: include_str!("../docs/environments.md"),
    },
    Topic {
        name: "views",
        summary: "For plugin authors: view trees, the UI plugins draw (panes, header chips, cards, tables, grids, charts), events and patches.",
        aliases: &["view", "view-trees", "ui", "panes"],
        text: include_str!("../docs/views.md"),
    },
    Topic {
        name: "plugin-backends",
        summary: "For plugin authors: long-lived plugin backends (Python or Rust), their lifecycle and wire format, hooks, artifacts and web views, the Python helper.",
        aliases: &["plugins", "plugin", "backends", "backend", "hooks", "plugins-backend", "writing-plugins"],
        text: include_str!("../docs/plugin-backends.md"),
    },
];

pub fn factory() -> NativeFactory {
    Arc::new(|| Box::new(Help) as Box<dyn NativeBackend>)
}

/// The backend (stateless: the docs are compiled in).
pub struct Help;

impl NativeBackend for Help {
    fn request(&mut self, method: &str, params: &Value, _out: &Outbox) -> Result<Value, String> {
        match method {
            "tool" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
                if name != TOOL {
                    return Err(format!("the help backend has no tool {name:?}"));
                }
                help(params.get("args").unwrap_or(&Value::Null)).map(Value::String)
            }
            "hook" => Ok(serde_json::json!({"action": "continue"})),
            other => Err(format!("the help backend doesn't answer {other:?} requests")),
        }
    }
}

/// A string argument, trimmed; blank is unset.
fn arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)?.as_str().map(str::trim).filter(|s| !s.is_empty())
}

/// A non-negative whole number argument (models send `5`, `5.0` or `"5"`).
fn uint(args: &Value, key: &str) -> Option<usize> {
    let v = args.get(key)?;
    let n = v.as_u64().or_else(|| v.as_f64().filter(|f| f.is_finite() && *f >= 0.0).map(|f| f as u64)).or_else(|| v.as_str()?.trim().parse().ok())?;
    usize::try_from(n).ok()
}

/// The tool: its text for the model, or the error sentence.
pub fn help(args: &Value) -> Result<String, String> {
    let start = uint(args, "start").unwrap_or(0);
    // `section` may be a number or a heading
    let section = args.get("section").and_then(|v| match v {
        Value::String(s) => Some(s.trim().to_owned()).filter(|s| !s.is_empty()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    });
    match (arg(args, "topic"), arg(args, "query")) {
        (Some(t), _) => read(find_topic(t)?, section.as_deref(), start),
        (None, Some(q)) => Ok(search(q)),
        (None, None) if section.is_some() => Err(format!("Say which topic the section is in (topic). {}", topic_list())),
        (None, None) => Ok(topic_list()),
    }
}

/// "Topics: ..." with a line each.
pub fn topic_list() -> String {
    let mut out = String::from("byob's help topics (read one with byob_help {\"topic\": \"<name>\"}, or search them all with {\"query\": \"<words>\"}):\n");
    for t in TOPICS {
        let _ = writeln!(out, "- {}: {}", t.name, t.summary);
    }
    out
}

fn find_topic(name: &str) -> Result<&'static Topic, String> {
    let n = name.trim().trim_end_matches(".md").to_lowercase().replace([' ', '_'], "-");
    let n = n.rsplit('/').next().unwrap_or(&n);
    TOPICS
        .iter()
        .find(|t| t.name == n)
        .or_else(|| TOPICS.iter().find(|t| t.aliases.contains(&n)))
        .ok_or_else(|| format!("There is no help topic {name:?}. {}", topic_list()))
}

/// A doc's text without the generated-file note at its top.
fn body(t: &Topic) -> &'static str {
    let text = t.text;
    match text.strip_prefix("<!--").and_then(|r| r.find("-->\n").map(|i| &r[i + 4..])) {
        Some(rest) => rest,
        None => text,
    }
}

/// One `## ` or `### ` section of a doc: its heading and its text (heading line included; a
/// `## ` section's text runs over its `### ` ones).
#[derive(Debug)]
pub struct Section {
    pub level: u8,
    pub heading: String,
    pub text: &'static str,
    /// Just its own part (up to its first subsection), for search.
    pub own: &'static str,
}

/// The sections of `text`, in order; the part before the first `## ` is a level-1 section
/// headed by the title. Headings inside code fences don't count.
pub fn sections(text: &'static str) -> Vec<Section> {
    // (byte offset, level, heading) of every heading line
    let mut heads: Vec<(usize, u8, String)> = Vec::new();
    let mut fence = false;
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        let t = line.trim_end();
        if t.trim_start().starts_with("```") {
            fence = !fence;
        } else if !fence {
            if let Some(h) = t.strip_prefix("### ") {
                heads.push((at, 3, h.trim().to_owned()));
            } else if let Some(h) = t.strip_prefix("## ") {
                heads.push((at, 2, h.trim().to_owned()));
            } else if let Some(h) = t.strip_prefix("# ")
                && heads.is_empty()
            {
                heads.push((at, 1, h.trim().to_owned()));
            }
        }
        at += line.len();
    }
    if heads.first().is_none_or(|h| h.0 != 0) {
        heads.insert(0, (0, 1, String::new()));
    }
    let mut out = Vec::with_capacity(heads.len());
    for (i, (from, level, heading)) in heads.iter().enumerate() {
        let next = heads.get(i + 1).map_or(text.len(), |h| h.0);
        // the section runs to the next heading of its level or higher
        let end = heads.iter().skip(i + 1).find(|h| h.1 <= *level || *level == 1).map_or(text.len(), |h| h.0);
        let (Some(t), Some(own)) = (text.get(*from..end), text.get(*from..next)) else { continue };
        out.push(Section { level: *level, heading: heading.clone(), text: t, own });
    }
    out
}

/// `topic`'s text from `start` (or one section of it), at most [`MAX_CHARS`].
fn read(topic: &Topic, section: Option<&str>, start: usize) -> Result<String, String> {
    let all = sections(body(topic));
    let (text, label) = match section {
        None => (body(topic), None),
        Some(s) => {
            let tops: Vec<&Section> = all.iter().filter(|x| x.level == 2).collect();
            let want = s.trim_start_matches('#').trim().to_lowercase();
            let by_number = want.parse::<usize>().ok().and_then(|n| n.checked_sub(1)).and_then(|i| tops.get(i).copied());
            let by_name = || {
                all.iter()
                    .filter(|x| x.level > 1)
                    .find(|x| x.heading.to_lowercase() == want)
                    .or_else(|| all.iter().filter(|x| x.level > 1).find(|x| x.heading.to_lowercase().contains(&want)))
            };
            match by_number.or_else(by_name) {
                Some(x) => (x.text, Some(x.heading.clone())),
                None => return Err(format!("{:?} has no section {s:?}. Its sections: {}.", topic.name, contents(&all))),
            }
        }
    };
    let total = text.chars().count();
    if start >= total && start > 0 {
        return Err(format!("start {start} is past the end ({total} characters)."));
    }
    let (window, end) = window(text, start, MAX_CHARS);
    let mut out = String::new();
    if section.is_none() && start == 0 && end < total {
        let _ = writeln!(out, "[{:?} is long ({total} characters); its sections: {}.]\n", topic.name, contents(&all));
    }
    out.push_str(window.trim_end());
    if end < total {
        let sec = label.map(|l| format!(", \"section\": {}", Value::String(l))).unwrap_or_default();
        let _ = write!(out, "\n\n[Shown: characters {start}-{end} of {total}. Read on with byob_help {{\"topic\": {:?}{sec}, \"start\": {end}}}", topic.name);
        if section.is_none() {
            out.push_str(", or read one section with \"section\" (a name or number)");
        }
        out.push_str(".]");
    }
    Ok(out)
}

/// "1. Settings, 2. Tools, ..." (the `## ` sections).
fn contents(all: &[Section]) -> String {
    all.iter().filter(|x| x.level == 2).enumerate().map(|(i, x)| format!("{}. {}", i + 1, x.heading)).collect::<Vec<_>>().join(", ")
}

/// At most `max` characters of `text` from character `start`, cut at a line break when one is
/// near the end; and the character offset where it stopped.
fn window(text: &str, start: usize, max: usize) -> (&str, usize) {
    let mut idx = text.char_indices().map(|(i, _)| i).skip(start);
    let Some(from) = idx.next() else { return ("", start) };
    let total = text.chars().count();
    if total.saturating_sub(start) <= max {
        return (text.get(from..).unwrap_or_default(), total);
    }
    let to = text.char_indices().nth(start.saturating_add(max)).map_or(text.len(), |(i, _)| i);
    let mut slice = text.get(from..to).unwrap_or_default();
    // back up to the last line break in the second half, so lines aren't cut
    if let Some(nl) = slice.rfind('\n').filter(|&nl| nl > slice.len() / 2) {
        slice = slice.get(..=nl).unwrap_or(slice);
    }
    let end = start + slice.chars().count();
    (slice, end)
}

const STOP: &[&str] = &[
    "a", "about", "an", "and", "any", "are", "as", "at", "be", "byob", "byob's", "can", "could", "do", "does", "for", "from", "get", "have", "how", "i", "if",
    "in", "into", "is", "it", "its", "me", "my", "of", "on", "or", "please", "should", "so", "that", "the", "there", "this", "to", "up", "use", "using",
    "want", "way", "what", "when", "where", "which", "why", "will", "with", "would", "you", "your",
];

/// The words of a query worth matching, lowercased, without duplicates.
fn terms(q: &str) -> Vec<String> {
    let q: String = q.chars().take(MAX_QUERY).collect::<String>().to_lowercase();
    let mut out: Vec<String> = Vec::new();
    for w in q.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '\'')) {
        let w = w.trim_matches('\'').trim_end_matches("'s");
        if w.chars().count() < 2 || STOP.contains(&w) || out.iter().any(|o| o == w) {
            continue;
        }
        out.push(w.to_owned());
        if out.len() >= 12 {
            break;
        }
    }
    out
}

/// A search hit: the topic, the section, its score.
struct Hit<'a> {
    topic: &'static Topic,
    section: &'a Section,
    score: f64,
}

/// The best-matching sections across all docs.
fn search(query: &str) -> String {
    let q: String = query.chars().take(80).collect();
    let q = q.as_str();
    let words = terms(query);
    if words.is_empty() {
        return format!("Nothing to search for in {q:?}: give a few keywords, e.g. \"gmail app password\".\n\n{}", topic_list());
    }
    let docs: Vec<(&'static Topic, Vec<Section>)> = TOPICS.iter().map(|t| (t, sections(body(t)))).collect();
    // every section's own text, lowercased, with its topic and heading
    let units: Vec<(&'static Topic, &Section, String)> =
        docs.iter().flat_map(|(t, ss)| ss.iter().filter(|s| !s.own.trim().is_empty()).map(move |s| (*t, s, s.own.to_lowercase()))).collect();
    let n = units.len().max(1) as f64;
    let idf: Vec<f64> = words
        .iter()
        .map(|w| {
            let df = units.iter().filter(|u| u.2.contains(w.as_str())).count();
            if df == 0 { 0.0 } else { (1.0 + n / df as f64).ln() }
        })
        .collect();
    let pairs: Vec<String> = words.windows(2).map(|p| format!("{} {}", p[0], p[1])).collect();
    let mut hits: Vec<Hit> = Vec::new();
    for (topic, section, lower) in &units {
        let heading = section.heading.to_lowercase();
        let mut score = 0.0;
        // words found in the text (a topic named by a word scores, but covers nothing)
        let mut matched = 0;
        for (w, idf) in words.iter().zip(&idf) {
            let count = lower.matches(w.as_str()).count().min(10);
            let named = topic.name.contains(w.as_str()) || topic.aliases.iter().any(|a| a == w);
            if named {
                score += idf;
            }
            if count == 0 {
                continue;
            }
            matched += 1;
            score += idf * (1.0 + (count as f64).ln_1p());
            if heading.contains(w.as_str()) {
                score += 3.0 * idf;
            }
        }
        score += 2.0 * pairs.iter().filter(|p| lower.contains(p.as_str())).count() as f64;
        // sections that match more of the words first
        score *= 0.5 + 0.5 * matched as f64 / words.len() as f64;
        if matched > 0 && score > 0.0 {
            hits.push(Hit { topic, section, score });
        }
    }
    hits.sort_by(|a, b| b.score.total_cmp(&a.score));
    hits.truncate(MAX_HITS);
    if hits.is_empty() {
        return format!("Nothing in byob's docs matches {q:?}.\n\n{}", topic_list());
    }
    let per = MAX_CHARS / hits.len();
    let mut out = format!("byob's docs, best matches for {:?}:\n", words.join(" "));
    for h in &hits {
        let heading = if h.section.heading.is_empty() { h.topic.name.to_owned() } else { h.section.heading.clone() };
        let _ = writeln!(out, "\n--- topic {:?}, section {:?} ---", h.topic.name, heading);
        let (text, end) = window(h.section.text, 0, per);
        out.push_str(text.trim_end());
        out.push('\n');
        if end < h.section.text.chars().count() {
            let section = if h.section.level == 1 { String::new() } else { format!(", \"section\": {}", Value::String(heading)) };
            let _ = writeln!(out, "[cut; read it all with byob_help {{\"topic\": {:?}{section}}}]", h.topic.name);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(args: Value) -> Result<String, String> {
        help(&args)
    }

    #[test]
    fn no_arguments_list_the_topics() {
        let out = call(json!({})).unwrap();
        for t in TOPICS {
            assert!(out.contains(&format!("- {}: {}\n", t.name, t.summary)), "{out}");
        }
        assert_eq!(call(Value::Null).unwrap(), out);
        assert_eq!(call(json!({"topic": " ", "query": ""})).unwrap(), out, "blank is unset");
    }

    #[test]
    fn every_doc_is_embedded_and_listed() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs");
        let mut files: Vec<String> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        files.sort();
        let mut names: Vec<String> = TOPICS.iter().map(|t| format!("{}.md", t.name)).collect();
        names.sort();
        assert_eq!(files, names);
        for t in TOPICS {
            assert!(t.text.starts_with("<!-- Generated from "), "{}", t.name);
            assert!(body(t).starts_with("# "), "{}: {:?}", t.name, &body(t)[..40]);
        }
    }

    #[test]
    fn reads_a_topic_and_pages_a_long_one() {
        // the mail doc fits: whole, without the generated-file note
        let mail = call(json!({"topic": "mail"})).unwrap();
        assert!(mail.starts_with("# Mail\n") && mail.contains("## Gmail") && !mail.contains("<!--"), "{mail}");
        assert!(!mail.contains("## Tests") && !mail.contains("## Crates"), "developer sections are left out");
        assert_eq!(call(json!({"topic": "Gmail"})).unwrap(), mail, "aliases pick a topic");
        assert_eq!(call(json!({"topic": "docs/mail.md"})).unwrap(), mail);
        // a long one comes in windows, with its contents first and how to read on
        let first = call(json!({"topic": "plugin-backends"})).unwrap();
        assert!(first.starts_with("[\"plugin-backends\" is long"), "{}", &first[..200]);
        assert!(first.contains("1. Declaring one, 2. Lifecycle"), "{}", &first[..400]);
        let end = first.rsplit_once("\"start\": ").unwrap().1.split('}').next().unwrap().parse::<usize>().unwrap();
        assert!(end <= MAX_CHARS && end > MAX_CHARS / 2, "{end}");
        let next = call(json!({"topic": "plugin-backends", "start": end})).unwrap();
        assert!(!next.starts_with('['), "no contents after the first window");
        assert!(body(&TOPICS[5]).chars().skip(end).take(200).collect::<String>().starts_with(&next.chars().take(200).collect::<String>()));
        assert!(call(json!({"topic": "mail", "start": 1_000_000})).unwrap_err().contains("past the end"));
    }

    #[test]
    fn reads_one_section_by_name_or_number() {
        let gmail = call(json!({"topic": "mail", "section": "gmail"})).unwrap();
        assert!(gmail.starts_with("## Gmail\n") && gmail.contains("app password") && !gmail.contains("## Not supported yet"), "{gmail}");
        let first = call(json!({"topic": "mail", "section": 1})).unwrap();
        assert!(first.starts_with("## Settings\n"), "{first}");
        assert_eq!(call(json!({"topic": "mail", "section": "1"})).unwrap(), first);
        // a `##` section takes its `###` ones; a `###` one can be read alone
        let tools = call(json!({"topic": "mail", "section": "Tools"})).unwrap();
        assert!(tools.contains("### Sending always asks"), "{tools}");
        let asks = call(json!({"topic": "mail", "section": "sending always"})).unwrap();
        assert!(asks.starts_with("### Sending always asks") && !asks.contains("## Hostile input"), "{asks}");
        let err = call(json!({"topic": "mail", "section": "nope"})).unwrap_err();
        assert!(err.contains("has no section \"nope\"") && err.contains("1. Settings"), "{err}");
        assert!(call(json!({"section": "Gmail"})).unwrap_err().contains("which topic"));
    }

    #[test]
    fn unknown_topics_list_the_real_ones() {
        let err = call(json!({"topic": "telepathy"})).unwrap_err();
        assert!(err.starts_with("There is no help topic \"telepathy\"."), "{err}");
        for t in TOPICS {
            assert!(err.contains(&format!("- {}:", t.name)), "{err}");
        }
    }

    #[test]
    fn a_query_finds_the_gmail_app_password() {
        let out = call(json!({"query": "how do I connect my Gmail to byob?"})).unwrap();
        let first = out.split("\n--- ").nth(1).unwrap();
        assert!(first.starts_with("topic \"mail\", section \"Gmail\""), "{out}");
        assert!(first.contains("app password") && first.contains("2-Step Verification"), "{out}");
        let out = call(json!({"query": "app password"})).unwrap();
        assert!(out.contains("topic \"mail\""), "{out}");
        assert!(out.matches("\n--- topic ").count() <= MAX_HITS);
        // nothing to match
        assert!(call(json!({"query": "how do I"})).unwrap().starts_with("Nothing to search for"));
        assert!(call(json!({"query": "zzyzx quuxplumber"})).unwrap().starts_with("Nothing in byob's docs matches"));
    }

    #[test]
    fn outputs_stay_under_the_cap() {
        let cap = MAX_CHARS + 600; // the contents line and the read-on note
        for t in TOPICS {
            let out = call(json!({"topic": t.name})).unwrap();
            assert!(out.chars().count() <= cap, "{}: {}", t.name, out.chars().count());
            for (i, _) in sections(body(t)).iter().filter(|s| s.level == 2).enumerate() {
                let out = call(json!({"topic": t.name, "section": i + 1})).unwrap();
                assert!(out.chars().count() <= cap, "{} section {}: {}", t.name, i + 1, out.chars().count());
            }
        }
        for q in ["plugin", "settings tools hooks views events", "a b c d e f g h i j k l m n o p", &"password ".repeat(500)] {
            let out = call(json!({"query": q})).unwrap();
            assert!(out.chars().count() <= cap + MAX_QUERY, "{q}: {}", out.chars().count());
        }
        // hostile numbers and types don't panic
        for args in [
            json!({"topic": "mail", "start": -5}),
            json!({"topic": "mail", "start": 1e300}),
            json!({"topic": 7}),
            json!({"topic": "mail", "section": []}),
            json!([1, 2]),
        ] {
            let _ = call(args);
        }
        assert_eq!(window("héllo wörld", 3, 4), ("lo w", 7));
        assert_eq!(window("abc", 5, 4), ("", 5));
    }

    #[test]
    fn headings_in_code_fences_are_not_sections() {
        let ss = sections("# T\n\nintro\n\n## A\n\n```sh\n## not one\n# nor this\n```\n\n### B\n\nb\n\n## C\n\nc\n");
        let heads: Vec<(u8, &str)> = ss.iter().map(|s| (s.level, s.heading.as_str())).collect();
        assert_eq!(heads, [(1, "T"), (2, "A"), (3, "B"), (2, "C")]);
        assert!(ss[1].text.contains("### B") && !ss[1].text.contains("## C") && !ss[1].own.contains("### B"));
        assert_eq!(ss[0].text, "# T\n\nintro\n\n", "the intro stops at the first section");
    }

    #[test]
    fn served_through_the_wire_dispatch() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let s = Arc::clone(&seen);
        let out = Outbox::new(move |m| s.lock().unwrap().push(m));
        let mut h = Help;
        byob_plugins::native::dispatch(&mut h, &json!({"type": "init", "settings": {}, "secrets": {}}), &out);
        byob_plugins::native::dispatch(
            &mut h,
            &json!({"type": "request", "id": 1, "method": "tool", "params": {"name": "byob_help", "args": {"topic": "calendar", "section": "Settings"}}}),
            &out,
        );
        byob_plugins::native::dispatch(&mut h, &json!({"type": "request", "id": 2, "method": "tool", "params": {"name": "nope", "args": {}}}), &out);
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0]["type"], "reply");
        assert!(seen[0]["result"].as_str().unwrap().starts_with("## Settings"), "{:?}", seen[0]);
        assert!(seen[1]["error"].as_str().unwrap().contains("no tool"), "{:?}", seen[1]);
    }
}
