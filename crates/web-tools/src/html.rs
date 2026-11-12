//! A small, forgiving HTML reader: a tokenizer that never fails (any bytes are some tokens),
//! and a converter to readable Markdown-ish text that drops scripts, styles, navigation,
//! footers, forms and hidden parts, and keeps headings, paragraphs, lists, tables, code and
//! links. No recursion, no DOM: the input is hostile and may be malformed or huge, so the
//! work is linear in its size and the open-element stack is capped.

use url::Url;

/// One piece of an HTML document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token<'a> {
    /// Text between tags, entities not yet decoded.
    Text(&'a str),
    /// `<name attr=value …>`; `name` lowercased.
    Start { name: String, attrs: Vec<(String, String)>, self_closing: bool },
    /// `</name>`; `name` lowercased.
    End(String),
}

impl Token<'_> {
    pub fn attr(&self, key: &str) -> Option<&str> {
        match self {
            Token::Start { attrs, .. } => attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str()),
            _ => None,
        }
    }

    /// Whether its `class` has `class` among its words.
    pub fn has_class(&self, class: &str) -> bool {
        self.attr("class").is_some_and(|c| c.split_ascii_whitespace().any(|w| w == class))
    }
}

/// Elements whose content is raw text up to their end tag (not markup).
const RAW: &[&str] = &["script", "style", "textarea", "title", "xmp", "noscript", "iframe", "noembed", "noframes", "template", "svg", "math"];

/// Elements that never have content.
const VOID: &[&str] = &["area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr"];

/// Split `html` into tokens. Comments, doctypes and processing instructions are dropped;
/// the content of raw-text elements (`script`, `style`, `title`, …) comes as one text token.
pub fn tokenize(html: &str) -> Vec<Token<'_>> {
    let b = html.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut text_from = 0;
    while i < b.len() {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        let next = b.get(i + 1).copied().unwrap_or(0);
        if next == b'!' || next == b'?' {
            push_text(html, text_from, i, &mut out);
            i = if html.get(i..).is_some_and(|r| r.starts_with("<!--")) {
                html.get(i + 4..).and_then(|r| r.find("-->")).map_or(b.len(), |e| i + 4 + e + 3)
            } else {
                find_byte(b, i, b'>').map_or(b.len(), |e| e + 1)
            };
            text_from = i;
            continue;
        }
        let closing = next == b'/';
        let name_at = if closing { i + 2 } else { i + 1 };
        if !b.get(name_at).is_some_and(u8::is_ascii_alphabetic) {
            // a lone `<` (e.g. "a < b"): text
            i += 1;
            continue;
        }
        push_text(html, text_from, i, &mut out);
        let (tok, end) = tag(html, name_at, closing);
        i = end;
        if let Token::Start { name, self_closing: false, .. } = &tok
            && RAW.contains(&name.as_str())
        {
            let close = format!("</{name}");
            let rest = html.get(i..).unwrap_or_default();
            let stop = find_ci(rest, &close).unwrap_or(rest.len());
            let name = name.clone();
            out.push(tok);
            if stop > 0 {
                out.push(Token::Text(rest.get(..stop).unwrap_or_default()));
            }
            i += stop;
            i = find_byte(b, i, b'>').map_or(b.len(), |e| e + 1);
            out.push(Token::End(name));
        } else {
            out.push(tok);
        }
        text_from = i;
    }
    push_text(html, text_from, b.len(), &mut out);
    out
}

fn push_text<'a>(html: &'a str, from: usize, to: usize, out: &mut Vec<Token<'a>>) {
    if to > from
        && let Some(t) = html.get(from..to)
    {
        out.push(Token::Text(t));
    }
}

fn find_byte(b: &[u8], from: usize, c: u8) -> Option<usize> {
    b.get(from..)?.iter().position(|&x| x == c).map(|p| from + p)
}

/// Case-insensitive ASCII search.
fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    (0..=h.len() - n.len()).find(|&i| h[i..i + n.len()].eq_ignore_ascii_case(n))
}

/// Parse a tag whose name starts at `at`; returns it and the index after its `>`.
fn tag(html: &str, at: usize, closing: bool) -> (Token<'_>, usize) {
    let b = html.as_bytes();
    let mut i = at;
    while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'>' && b[i] != b'/' {
        i += 1;
    }
    let name = html.get(at..i).unwrap_or_default().to_ascii_lowercase();
    let mut attrs = Vec::new();
    let mut self_closing = false;
    loop {
        while i < b.len() && (b[i].is_ascii_whitespace() || (b[i] == b'/' && b.get(i + 1) != Some(&b'>'))) {
            i += 1;
        }
        match b.get(i) {
            None => break,
            Some(b'>') => {
                i += 1;
                break;
            }
            Some(b'/') => {
                self_closing = true;
                i += 2;
                break;
            }
            Some(_) => {}
        }
        let k0 = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && !matches!(b[i], b'=' | b'>' | b'/') {
            i += 1;
        }
        let key = html.get(k0..i).unwrap_or_default().to_ascii_lowercase();
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if b.get(i) == Some(&b'=') {
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            match b.get(i) {
                Some(&q @ (b'"' | b'\'')) => {
                    let end = find_byte(b, i + 1, q).unwrap_or(b.len());
                    value = decode_entities(html.get(i + 1..end).unwrap_or_default());
                    i = (end + 1).min(b.len());
                }
                _ => {
                    let v0 = i;
                    while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'>' {
                        i += 1;
                    }
                    value = decode_entities(html.get(v0..i).unwrap_or_default());
                }
            }
        }
        if !key.is_empty() && attrs.len() < 64 {
            attrs.push((key, value));
        }
        if i == k0 {
            // no progress (shouldn't happen): skip a byte
            i += 1;
        }
    }
    let tok = if closing { Token::End(name) } else { Token::Start { name, attrs, self_closing } };
    (tok, i.min(b.len()))
}

/// Decode `&amp;`, `&#39;`, `&#x27;` and the common named entities; unknown ones stay.
pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        rest = &rest[p..];
        let end = match rest.char_indices().skip(1).take(32).find(|&(_, c)| c == ';' || !(c.is_ascii_alphanumeric() || c == '#')) {
            Some((i, c)) => Some((i, c == ';')),
            // the string ends inside the name (`… &amp`)
            None if rest.len() <= 33 && rest.len() > 1 => Some((rest.len(), false)),
            None => None,
        };
        let decoded = match end {
            Some((e, true)) => entity(&rest[1..e]).map(|c| (c, e + 1)),
            // `&amp` without `;` (common in sloppy HTML)
            Some((e, false)) => entity(&rest[1..e]).filter(|_| matches!(&rest[1..e], "amp" | "lt" | "gt" | "quot" | "nbsp")).map(|c| (c, e)),
            None => None,
        };
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn entity(name: &str) -> Option<char> {
    if let Some(num) = name.strip_prefix('#') {
        let n = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => num.parse().ok()?,
        };
        return Some(match n {
            0 => '\u{FFFD}',
            // Windows-1252 in numeric references (&#146; for ’), as browsers do
            0x80..=0x9F => cp1252(n as u8),
            _ => char::from_u32(n).unwrap_or('\u{FFFD}'),
        });
    }
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "ndash" => '–',
        "mdash" => '—',
        "hellip" => '…',
        "lsquo" => '‘',
        "rsquo" => '’',
        "ldquo" => '“',
        "rdquo" => '”',
        "laquo" => '«',
        "raquo" => '»',
        "bull" => '•',
        "middot" => '·',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "deg" => '°',
        "times" => '×',
        "divide" => '÷',
        "plusmn" => '±',
        "euro" => '€',
        "pound" => '£',
        "yen" => '¥',
        "cent" => '¢',
        "sect" => '§',
        "para" => '¶',
        "larr" => '←',
        "rarr" => '→',
        "uarr" => '↑',
        "darr" => '↓',
        "le" => '≤',
        "ge" => '≥',
        "ne" => '≠',
        "minus" => '−',
        "shy" => '\u{AD}',
        "zwj" => '\u{200D}',
        "zwnj" => '\u{200C}',
        "thinsp" | "ensp" | "emsp" => ' ',
        "eacute" => 'é',
        "egrave" => 'è',
        "aacute" => 'á',
        "agrave" => 'à',
        "uuml" => 'ü',
        "ouml" => 'ö',
        "auml" => 'ä',
        "szlig" => 'ß',
        "ccedil" => 'ç',
        "ntilde" => 'ñ',
        _ => return None,
    })
}

/// A Windows-1252 byte as a char (bytes 0x80–0x9F differ from Latin-1).
pub fn cp1252(b: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8D}', 'Ž', '\u{8F}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™',
        'š', '›', 'œ', '\u{9D}', 'ž', 'Ÿ',
    ];
    match b {
        0x80..=0x9F => HIGH.get(usize::from(b - 0x80)).copied().unwrap_or('\u{FFFD}'),
        _ => char::from(b),
    }
}

/// Plain text of an HTML fragment (tags dropped, entities decoded, whitespace collapsed).
pub fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    for t in tokenize(s) {
        if let Token::Text(t) = t {
            out.push_str(&decode_entities(t));
            out.push(' ');
        } else if let Token::Start { name, .. } = &t
            && matches!(name.as_str(), "br" | "p" | "div" | "li")
        {
            out.push(' ');
        }
    }
    collapse(&out)
}

/// Whitespace runs as one space, trimmed.
pub fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A page as readable text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Readable {
    pub title: String,
    pub text: String,
}

/// Dropped with their content wherever they are.
const DROP: &[&str] = &[
    "script", "style", "noscript", "template", "svg", "math", "canvas", "iframe", "object", "embed", "form", "button", "select", "textarea", "input", "nav",
    "dialog", "head", "title", "video", "audio", "map", "picture",
];

/// Page chrome: dropped outside an article.
const CHROME: &[&str] = &["header", "footer", "aside", "menu"];

/// `class`/`id` words that mark page chrome (matched as parts of words: `site-footer`).
const CHROME_WORDS: &[&str] = &[
    "cookie",
    "consent",
    "sidebar",
    "breadcrumb",
    "navbar",
    "nav-",
    "-nav",
    "menu",
    "footer",
    "header-",
    "masthead",
    "advert",
    "promo",
    "newsletter",
    "subscribe",
    "share",
    "social",
    "related",
    "comments",
    "popup",
    "modal",
    "banner",
    "skip-link",
    "visually-hidden",
    "sr-only",
    "toc-",
    "editsection",
];

/// Block elements: their content goes on lines of its own.
const BLOCK: &[&str] = &[
    "p",
    "div",
    "section",
    "article",
    "main",
    "header",
    "footer",
    "aside",
    "blockquote",
    "figure",
    "figcaption",
    "address",
    "details",
    "summary",
    "dl",
    "dt",
    "dd",
    "ul",
    "ol",
    "table",
    "thead",
    "tbody",
    "tfoot",
    "caption",
    "center",
    "fieldset",
    "body",
    "html",
];

const MAX_DEPTH: usize = 256;

/// The open elements and what each one means for the output.
struct Open {
    name: String,
    /// Content is dropped.
    drop: bool,
    /// Inside a `pre`.
    pre: bool,
    /// An `a` with this target (written after its text).
    href: Option<String>,
    /// Where the link's text starts in the output.
    link_at: usize,
}

/// Readable text of a page: its title, and its main content (the `main`/`article` elements
/// if it has them, else the body without navigation, header, footer and asides) as
/// Markdown-ish text. With `base`, links are written as `[text](url)` (relative ones resolved
/// against it); without, only their text is kept.
pub fn readable(html: &str, base: Option<&Url>) -> Readable {
    let tokens = tokenize(html);
    let title = page_title(&tokens);
    // Pages that mark their content: only that, unless it is next to nothing.
    let scoped = tokens.iter().any(is_main);
    let mut text = convert(&tokens, base, scoped);
    if scoped && text.chars().count() < 200 {
        let all = convert(&tokens, base, false);
        if all.chars().count() > text.chars().count() {
            text = all;
        }
    }
    Readable { title, text }
}

fn is_main(t: &Token<'_>) -> bool {
    match t {
        Token::Start { name, .. } => name == "main" || name == "article" || t.attr("role") == Some("main"),
        _ => false,
    }
}

fn page_title(tokens: &[Token<'_>]) -> String {
    let mut in_title = false;
    let mut og = None;
    let mut h1 = None;
    let mut in_h1 = false;
    let mut h1_text = String::new();
    for t in tokens {
        match t {
            Token::Start { name, .. } if name == "title" => in_title = true,
            Token::End(name) if name == "title" => in_title = false,
            Token::Text(s) if in_title => {
                let s = collapse(&decode_entities(s));
                if !s.is_empty() {
                    return truncate(&s, 200);
                }
            }
            Token::Start { name, .. } if name == "meta" && t.attr("property") == Some("og:title") => og = t.attr("content").map(collapse),
            Token::Start { name, .. } if name == "h1" && h1.is_none() => in_h1 = true,
            Token::End(name) if name == "h1" && in_h1 => {
                in_h1 = false;
                h1 = Some(collapse(&h1_text));
            }
            Token::Text(s) if in_h1 => h1_text.push_str(&decode_entities(s)),
            _ => {}
        }
    }
    truncate(&og.or(h1).unwrap_or_default(), 200)
}

pub fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", s[..i].trim_end()),
        None => s.to_owned(),
    }
}

fn hidden(t: &Token<'_>) -> bool {
    if t.attr("hidden").is_some() || t.attr("aria-hidden") == Some("true") {
        return true;
    }
    t.attr("style").is_some_and(|s| {
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
        s.contains("display:none") || s.contains("visibility:hidden")
    })
}

fn chrome(name: &str, t: &Token<'_>) -> bool {
    if matches!(name, "html" | "body" | "main" | "article") || t.attr("role") == Some("main") {
        return false;
    }
    if matches!(t.attr("role"), Some("navigation" | "banner" | "contentinfo" | "complementary" | "search" | "dialog" | "alert")) {
        return true;
    }
    let words = format!("{} {}", t.attr("class").unwrap_or_default(), t.attr("id").unwrap_or_default()).to_ascii_lowercase();
    words.split_ascii_whitespace().any(|w| CHROME_WORDS.iter().any(|c| w.contains(c)))
}

fn convert(tokens: &[Token<'_>], base: Option<&Url>, scoped: bool) -> String {
    let mut out = Out::default();
    let mut stack: Vec<Open> = Vec::new();
    // depth of main/article elements open (scoped mode emits only inside one)
    let mut mains = 0usize;
    let mut lists: Vec<Option<u32>> = Vec::new();
    for t in tokens {
        let dropping = stack.last().is_some_and(|o| o.drop);
        let pre = stack.last().is_some_and(|o| o.pre);
        match t {
            Token::Text(s) => {
                if dropping || (scoped && mains == 0) {
                    continue;
                }
                let s = decode_entities(s);
                if pre { out.raw(&s) } else { out.text(&s) }
            }
            Token::Start { name, self_closing, .. } => {
                let name = name.as_str();
                let drop = dropping || DROP.contains(&name) || hidden(t) || ((mains == 0 || !scoped) && CHROME.contains(&name)) || chrome(name, t);
                let main = is_main(t);
                let emit = !drop && !(scoped && mains == 0 && !main);
                if emit {
                    open_tag(name, t, base, &mut out, &mut lists);
                }
                if VOID.contains(&name) || *self_closing {
                    continue;
                }
                if stack.len() >= MAX_DEPTH {
                    continue;
                }
                if main && !drop {
                    mains += 1;
                }
                let href = if name == "a" && emit && base.is_some() { t.attr("href").and_then(|h| link(h, base)) } else { None };
                stack.push(Open { name: name.to_owned(), drop, pre: pre || name == "pre", href, link_at: out.buf.len() });
            }
            Token::End(name) => {
                // close up to the matching element; a stray end tag closes nothing
                let Some(pos) = stack.iter().rposition(|o| o.name == *name) else { continue };
                while stack.len() > pos {
                    let Some(o) = stack.pop() else { break };
                    let emit = !o.drop && !(scoped && mains == 0);
                    if o.name == "main" || o.name == "article" {
                        mains = mains.saturating_sub(usize::from(!o.drop));
                    }
                    if emit {
                        close_tag(&o, &mut out, &mut lists);
                    }
                }
            }
        }
    }
    out.finish()
}

fn link(href: &str, base: Option<&Url>) -> Option<String> {
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') || href.len() > 2000 {
        return None;
    }
    let u = match base {
        Some(b) => b.join(href).ok()?,
        None => Url::parse(href).ok()?,
    };
    matches!(u.scheme(), "http" | "https").then(|| u.to_string())
}

fn open_tag(name: &str, t: &Token<'_>, base: Option<&Url>, out: &mut Out, lists: &mut Vec<Option<u32>>) {
    match name {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            out.block();
            let level = name[1..].parse::<usize>().unwrap_or(1);
            out.raw(&"#".repeat(level));
            out.raw(" ");
        }
        "br" => out.newline(),
        "hr" => {
            out.block();
            out.raw("---");
            out.block();
        }
        "li" => {
            out.newline();
            let depth = lists.len().saturating_sub(1).min(6);
            out.raw(&"  ".repeat(depth));
            match lists.last_mut() {
                Some(Some(n)) => {
                    *n += 1;
                    let n = *n;
                    out.raw(&format!("{n}. "));
                }
                _ => out.raw("- "),
            }
        }
        "ul" | "menu" => {
            out.block();
            lists.push(None);
        }
        "ol" => {
            out.block();
            let start = t.attr("start").and_then(|s| s.trim().parse::<u32>().ok()).unwrap_or(1);
            lists.push(Some(start.saturating_sub(1)));
        }
        "tr" => out.newline(),
        "td" | "th" => {
            if !out.at_line_start() {
                out.raw(" | ");
            }
        }
        "pre" => {
            out.block();
            out.raw("```\n");
        }
        "code" | "kbd" | "samp" => out.text("`"),
        "blockquote" => {
            out.block();
            out.raw("> ");
        }
        "dt" => out.newline(),
        "dd" => {
            out.newline();
            out.raw("  ");
        }
        _ if BLOCK.contains(&name) => out.block(),
        _ => {
            let _ = base;
        }
    }
}

fn close_tag(o: &Open, out: &mut Out, lists: &mut Vec<Option<u32>>) {
    match o.name.as_str() {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => out.block(),
        "pre" => {
            out.newline();
            out.raw("```");
            out.block();
        }
        "code" | "kbd" | "samp" if !o.pre => out.text_glued("`"),
        "ul" | "ol" | "menu" => {
            lists.pop();
            out.block();
        }
        "a" => {
            if let Some(href) = &o.href {
                let text = out.buf.get(o.link_at..).unwrap_or_default().trim().to_owned();
                if text.is_empty() {
                    // an icon link: nothing to show
                } else if text == *href || text.trim_end_matches('/') == href.trim_end_matches('/') {
                    // the URL is its own text
                } else if let Some(start) = out.buf.get(o.link_at..).map(|s| o.link_at + (s.len() - s.trim_start().len())) {
                    out.buf.insert(start, '[');
                    let end = out.buf.trim_end().len();
                    out.buf.truncate(end);
                    out.buf.push_str("](");
                    out.buf.push_str(href);
                    out.buf.push(')');
                    out.space = false;
                }
            }
        }
        name if BLOCK.contains(&name) => out.block(),
        _ => {}
    }
}

/// The text being written: collapses whitespace and blank lines.
#[derive(Default)]
struct Out {
    buf: String,
    /// A space is due before the next word.
    space: bool,
    /// Newlines due before the next text (1: a line break, 2: a blank line).
    breaks: u8,
}

impl Out {
    fn at_line_start(&self) -> bool {
        self.buf.is_empty() || self.buf.ends_with('\n') || self.breaks > 0
    }

    fn flush_breaks(&mut self) {
        if self.breaks > 0 && !self.buf.is_empty() {
            let end = self.buf.trim_end_matches([' ', '\t']).len();
            self.buf.truncate(end);
            let have = self.buf.chars().rev().take_while(|&c| c == '\n').count();
            for _ in have..usize::from(self.breaks) {
                self.buf.push('\n');
            }
            self.space = false;
        }
        self.breaks = 0;
    }

    fn text(&mut self, s: &str) {
        if s.chars().all(char::is_whitespace) {
            if !s.is_empty() {
                self.space = true;
            }
            return;
        }
        let lead = s.starts_with(char::is_whitespace);
        let trail = s.ends_with(char::is_whitespace);
        self.flush_breaks();
        if (lead || self.space) && !self.at_line_start() && !self.buf.ends_with(' ') {
            self.buf.push(' ');
        }
        let mut first = true;
        for w in s.split_whitespace() {
            if !first {
                self.buf.push(' ');
            }
            self.buf.push_str(w);
            first = false;
        }
        self.space = trail;
    }

    /// Text that sticks to what came before (a closing backtick).
    fn text_glued(&mut self, s: &str) {
        self.buf.push_str(s);
    }

    /// Text as it is (in `pre`, markers).
    fn raw(&mut self, s: &str) {
        self.flush_breaks();
        if self.space && !self.at_line_start() && !self.buf.ends_with(' ') && !s.starts_with(char::is_whitespace) {
            self.buf.push(' ');
        }
        self.space = false;
        self.buf.push_str(s);
    }

    fn newline(&mut self) {
        self.breaks = self.breaks.max(1);
    }

    fn block(&mut self) {
        self.breaks = 2;
    }

    fn finish(self) -> String {
        let mut out = String::with_capacity(self.buf.len());
        let mut blank = 0;
        for line in self.buf.lines() {
            let line = line.trim_end();
            if line.trim().is_empty() || line.trim() == "-" {
                blank += 1;
                continue;
            }
            if !out.is_empty() {
                out.push_str(if blank > 0 { "\n\n" } else { "\n" });
            }
            out.push_str(line);
            blank = 0;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_tags_attributes_and_raw_text() {
        let t = tokenize(r#"<p class="a b" data-x=1 hidden>Hi &amp; bye<br/><script>if (a < b) { "</p>" }</script></P><!-- c --><x"#);
        assert_eq!(
            t[0],
            Token::Start {
                name: "p".into(),
                attrs: vec![("class".into(), "a b".into()), ("data-x".into(), "1".into()), ("hidden".into(), String::new())],
                self_closing: false
            }
        );
        assert!(t[0].has_class("b") && !t[0].has_class("a b"));
        assert_eq!(t[1], Token::Text("Hi &amp; bye"));
        assert_eq!(t[2], Token::Start { name: "br".into(), attrs: vec![], self_closing: true });
        assert_eq!(t[4], Token::Text(r#"if (a < b) { "</p>" }"#), "raw text runs to its own end tag");
        assert!(t.contains(&Token::End("p".into())));
    }

    proptest::proptest! {
        /// Any text, and any soup of HTML-ish pieces, converts without a panic.
        #[test]
        fn any_input_converts(s in "\\PC{0,400}", parts in proptest::collection::vec(proptest::sample::select(vec![
            "<", ">", "</", "/>", "<!--", "-->", "<a href=\"", "\"", "'", "=", "&", "&#", "&amp", ";", "<script>", "</script>", "<pre>", "</pre>",
            "<main>", "</main>", "<li>", "<ol start=x>", "<table><tr><td>", "é", "\u{1F600}", " ", "\n", "text", "<div hidden>", "</div>",
        ]), 0..80)) {
            let soup: String = parts.concat();
            let base = Url::parse("https://example.com/a/").unwrap();
            for input in [s.as_str(), soup.as_str()] {
                let _ = tokenize(input);
                let _ = strip_tags(input);
                let _ = decode_entities(input);
                let _ = readable(input, Some(&base));
                let _ = readable(input, None);
            }
        }
    }

    #[test]
    fn never_panics_on_garbage() {
        for s in [
            "<",
            "<a",
            "<a href=",
            "<a href='x",
            "<!--",
            "<!",
            "</",
            "</>",
            "<<<>>>",
            "&",
            "&#",
            "&#x;",
            "&#99999999999;",
            "<p>\u{0}é&eacute",
            "<script>",
            "<a\n=\"\">",
        ] {
            let _ = tokenize(s);
            let _ = readable(s, None);
            let _ = strip_tags(s);
        }
        // deep nesting is capped, not recursed
        let deep = "<div>".repeat(100_000) + "deep" + &"</div>".repeat(100_000);
        assert_eq!(readable(&deep, None).text, "deep");
    }

    #[test]
    fn decodes_entities() {
        assert_eq!(decode_entities("a &amp; b &lt;c&gt; &#39;q&#x27; &rsquo; &#146; &bogus; &amp"), "a & b <c> 'q' ’ ’ &bogus; &");
    }

    #[test]
    fn keeps_the_article_and_drops_the_chrome() {
        let html = r#"<html><head><title>The &amp; Title</title><style>p{}</style></head><body>
            <nav><a href="/">Home</a></nav><header class="site-header">Site</header>
            <main><h1>Heading</h1><p>First   paragraph with <a href="/x">a link</a> and <code>code()</code>.</p>
            <ul><li>one</li><li>two <b>bold</b></li></ul><ol start="3"><li>three</li></ol>
            <pre>  keep
    spacing</pre><div style="display: none">secret</div><div class="cookie-banner">Accept cookies</div>
            <table><tr><th>A</th><th>B</th></tr><tr><td>1</td><td>2</td></tr></table></main>
            <footer>Footer</footer><script>alert(1)</script></body></html>"#;
        let base = Url::parse("https://example.com/dir/page").unwrap();
        let r = readable(html, Some(&base));
        assert_eq!(r.title, "The & Title");
        assert_eq!(
            r.text,
            "# Heading\n\nFirst paragraph with [a link](https://example.com/x) and `code()`.\n\n- one\n- two bold\n\n3. three\n\n```\n  keep\n    spacing\n```\n\nA | B\n1 | 2"
        );
    }

    #[test]
    fn without_a_main_element_the_body_minus_chrome() {
        let html = "<body><div id=\"sidebar\">Side</div><div><p>Body text.</p><aside>aside</aside></div><footer>f</footer></body>";
        assert_eq!(readable(html, None).text, "Body text.");
        // a tiny `main` (a skip link target, say) doesn't hide the rest of the page
        let html = format!("<body><main>x</main><div><p>{}</p></div></body>", "word ".repeat(60));
        assert!(readable(&html, None).text.contains("word word"));
    }

    #[test]
    fn strips_tags_for_snippets() {
        assert_eq!(strip_tags("A <strong>bold</strong> move&nbsp;&mdash; <em>now</em>"), "A bold move — now");
    }
}
