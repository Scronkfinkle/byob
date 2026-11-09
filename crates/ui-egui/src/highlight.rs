//! A small, language-agnostic syntax highlighter for fenced code blocks: comments, strings,
//! numbers, keywords, function calls and type-like names, coloured with the theme's
//! `syntax_*` roles. Not a parser — it only has to look right for common languages and
//! never misbehave on anything else (unknown or plain-text fences stay plain).

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId};

use crate::theme::Tokens;

/// Larger blocks are drawn plain (highlighting runs every frame).
const MAX_CHARS: usize = 20_000;

const KEYWORDS: &[&str] = &[
    "as",
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "def",
    "default",
    "defer",
    "del",
    "do",
    "elif",
    "else",
    "enum",
    "except",
    "export",
    "extends",
    "false",
    "finally",
    "fn",
    "for",
    "from",
    "func",
    "function",
    "go",
    "if",
    "impl",
    "import",
    "in",
    "interface",
    "is",
    "lambda",
    "let",
    "loop",
    "match",
    "mod",
    "mut",
    "new",
    "nil",
    "None",
    "not",
    "null",
    "or",
    "and",
    "package",
    "pass",
    "pub",
    "raise",
    "return",
    "self",
    "Self",
    "static",
    "struct",
    "super",
    "switch",
    "this",
    "throw",
    "trait",
    "True",
    "False",
    "true",
    "try",
    "type",
    "typeof",
    "undefined",
    "use",
    "var",
    "void",
    "where",
    "while",
    "with",
    "yield",
    "select",
    "insert",
    "update",
    "delete",
    "where",
    "join",
    "on",
    "group",
    "order",
    "by",
    "limit",
    "values",
    "into",
    "create",
    "table",
    "begin",
    "end",
    "then",
    "local",
    "elseif",
    "echo",
    "fi",
    "esac",
    "done",
    "unsafe",
    "dyn",
    "ref",
    "move",
    "crate",
    "extern",
    "goto",
    "sizeof",
    "private",
    "protected",
    "public",
    "final",
    "override",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Comments {
    /// `//` and `/* */`
    CLike,
    /// `#`
    Hash,
    /// `--`
    Dash,
    /// No comments (JSON).
    None,
}

/// Whether a fence language gets highlighted, and how its comments look.
fn comments_for(lang: &str) -> Option<Comments> {
    let l = lang.to_ascii_lowercase();
    Some(match l.as_str() {
        "" | "text" | "txt" | "plain" | "plaintext" | "output" | "console" | "log" | "md" | "markdown" | "csv" | "diff" => return None,
        "py" | "python" | "sh" | "bash" | "zsh" | "shell" | "fish" | "toml" | "yaml" | "yml" | "rb" | "ruby" | "r" | "perl" | "pl" | "nix" | "make"
        | "makefile" | "dockerfile" | "ini" | "conf" | "ps1" | "powershell" => Comments::Hash,
        "sql" | "lua" | "hs" | "haskell" | "elm" => Comments::Dash,
        "json" => Comments::None,
        _ => Comments::CLike,
    })
}

/// The code as a coloured layout job (plain `code_text` when the language isn't one we
/// highlight or the block is very large).
pub fn job(t: &Tokens, lang: &str, code: &str, font: &FontId) -> LayoutJob {
    let mut job = LayoutJob::default();
    let plain = |job: &mut LayoutJob, s: &str| job.append(s, 0.0, TextFormat::simple(font.clone(), t.code_text));
    let Some(comments) = comments_for(lang).filter(|_| code.len() <= MAX_CHARS) else {
        plain(&mut job, code);
        return job;
    };
    let push = |job: &mut LayoutJob, s: &str, c: Color32| job.append(s, 0.0, TextFormat::simple(font.clone(), c));
    let bytes = code.as_bytes();
    let mut i = 0usize;
    let mut plain_from = 0usize;
    // Flush the plain run before `i`, then colour `i..end`.
    let emit = |job: &mut LayoutJob, plain_from: &mut usize, start: usize, end: usize, c: Color32| {
        if let Some(p) = code.get(*plain_from..start).filter(|p| !p.is_empty()) {
            plain(job, p);
        }
        if let Some(s) = code.get(start..end) {
            push(job, s, c);
        }
        *plain_from = end;
    };
    while i < bytes.len() {
        let b = bytes.get(i).copied().unwrap_or(0);
        let rest = code.get(i..).unwrap_or_default();
        let line_comment = match comments {
            Comments::CLike => rest.starts_with("//"),
            Comments::Hash => b == b'#',
            Comments::Dash => rest.starts_with("--"),
            Comments::None => false,
        };
        if line_comment {
            let end = rest.find('\n').map_or(bytes.len(), |n| i + n);
            emit(&mut job, &mut plain_from, i, end, t.syntax_comment);
            i = end;
        } else if comments == Comments::CLike && rest.starts_with("/*") {
            let end = rest.get(2..).and_then(|r| r.find("*/")).map_or(bytes.len(), |n| i + 2 + n + 2);
            emit(&mut job, &mut plain_from, i, end, t.syntax_comment);
            i = end;
        } else if b == b'"' || b == b'\'' || b == b'`' {
            // A quote: up to the matching one on the same line (backticks may span lines).
            let mut j = i + 1;
            let mut closed = false;
            while let Some(&c) = bytes.get(j) {
                if c == b'\\' {
                    j += 2;
                    continue;
                }
                if c == b || (c == b'\n' && b != b'`') {
                    closed = c == b;
                    break;
                }
                j += 1;
            }
            // A lone apostrophe (Rust lifetimes, "don't" in a comment-less language) isn't a string.
            if b == b'\'' && !closed {
                i += 1;
                continue;
            }
            let mut end = (j + usize::from(closed)).min(bytes.len());
            while !code.is_char_boundary(end) {
                end += 1;
            }
            emit(&mut job, &mut plain_from, i, end, t.syntax_string);
            i = end;
        } else if b.is_ascii_digit() && !prev_is_word(bytes, i) {
            let mut j = i;
            while bytes.get(j).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'.' || *c == b'_') {
                j += 1;
            }
            emit(&mut job, &mut plain_from, i, j, t.syntax_number);
            i = j;
        } else if (b.is_ascii_alphabetic() || b == b'_') && !prev_is_word(bytes, i) {
            let mut j = i;
            while bytes.get(j).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_') {
                j += 1;
            }
            let word = code.get(i..j).unwrap_or_default();
            let next = bytes.get(j).copied();
            let color = if KEYWORDS.contains(&word) {
                Some(t.syntax_keyword)
            } else if next == Some(b'(') || next == Some(b'!') {
                Some(t.syntax_function)
            } else if word.as_bytes().first().is_some_and(u8::is_ascii_uppercase) && word.bytes().any(|c| c.is_ascii_lowercase()) {
                Some(t.syntax_type)
            } else {
                None
            };
            if let Some(c) = color {
                emit(&mut job, &mut plain_from, i, j, c);
            }
            i = j;
        } else {
            // Step over one whole character (multi-byte UTF-8 included).
            i += rest.chars().next().map_or(1, char::len_utf8);
        }
    }
    if let Some(p) = code.get(plain_from..).filter(|p| !p.is_empty()) {
        plain(&mut job, p);
    }
    job
}

fn prev_is_word(bytes: &[u8], i: usize) -> bool {
    i.checked_sub(1).and_then(|p| bytes.get(p)).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colors(lang: &str, code: &str) -> Vec<(String, Color32)> {
        let t = Tokens::dark();
        let j = job(&t, lang, code, &FontId::monospace(13.0));
        j.sections.iter().map(|s| (j.text.get(s.byte_range.start.0..s.byte_range.end.0).unwrap_or_default().to_owned(), s.format.color)).collect()
    }

    #[test]
    fn colours_common_tokens_and_keeps_the_text() {
        let t = Tokens::dark();
        let code = "fn main() {\n    let s = \"hi\"; // greet\n    println!(\"{}\", 42);\n}";
        let parts = colors("rust", code);
        assert_eq!(parts.iter().map(|(s, _)| s.as_str()).collect::<String>(), code);
        let find = |w: &str| parts.iter().find(|(s, _)| s == w).map(|(_, c)| *c);
        assert_eq!(find("fn"), Some(t.syntax_keyword));
        assert_eq!(find("main"), Some(t.syntax_function));
        assert_eq!(find("\"hi\""), Some(t.syntax_string));
        assert_eq!(find("// greet"), Some(t.syntax_comment));
        assert_eq!(find("42"), Some(t.syntax_number));
    }

    #[test]
    fn hash_comments_and_plain_fences() {
        let t = Tokens::dark();
        assert!(colors("python", "x = 1  # one").iter().any(|(s, c)| s == "# one" && *c == t.syntax_comment));
        assert_eq!(colors("text", "fn x // y"), vec![("fn x // y".to_owned(), t.code_text)]);
        assert_eq!(colors("", "let a"), vec![("let a".to_owned(), t.code_text)]);
    }

    #[test]
    fn hostile_input_never_panics() {
        for code in ["\"unterminated", "/* open", "'", "\\", "é'\"`\n`", "0x__.", "日本語 = 'ok'", "\"\\", "a'b'c\"d"] {
            for lang in ["rust", "python", "sql", "json", "weird"] {
                let joined: String = colors(lang, code).into_iter().map(|(s, _)| s).collect();
                assert_eq!(joined, code, "{lang}: {code}");
            }
        }
    }
}
