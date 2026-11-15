//! A small, careful iCalendar (RFC 5545) reader and writer: content lines are unfolded and
//! split into name, parameters and value; components nest by `BEGIN`/`END`. Everything a
//! server sends is hostile: sizes, nesting and counts are capped, nothing here panics, and
//! a broken line is skipped rather than failing the whole object. Writing keeps every
//! property it read (unknown ones included), so an update changes only what it means to.

/// Most bytes of one calendar object read.
pub const MAX_OBJECT_BYTES: usize = 1024 * 1024;
/// Deepest component nesting kept (VCALENDAR > VEVENT > VALARM is 3).
const MAX_DEPTH: usize = 8;
/// Most components in one object, and properties in one component.
const MAX_COMPONENTS: usize = 2000;
const MAX_PROPS: usize = 500;

/// One content line: `NAME;PARAM=value:VALUE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prop {
    /// Upper case.
    pub name: String,
    /// (upper-case name, raw value as written, quotes included).
    pub params: Vec<(String, String)>,
    /// Raw value (TEXT values are still escaped; see [`unescape`]).
    pub value: String,
}

impl Prop {
    pub fn new(name: &str, value: impl Into<String>) -> Self {
        Self { name: name.to_ascii_uppercase(), params: Vec::new(), value: value.into() }
    }

    /// A TEXT property, escaped.
    pub fn text(name: &str, text: &str) -> Self {
        Self::new(name, escape(text))
    }

    pub fn with_param(mut self, name: &str, value: &str) -> Self {
        self.params.push((name.to_ascii_uppercase(), quote_param(value)));
        self
    }

    /// A parameter's value, unquoted.
    pub fn param(&self, name: &str) -> Option<String> {
        self.params.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| unquote(v))
    }

    /// The value as text (unescaped).
    pub fn text_value(&self) -> String {
        unescape(&self.value)
    }
}

/// A component (`VCALENDAR`, `VEVENT`, `VTIMEZONE`, `VALARM`, ...).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Component {
    /// Upper case.
    pub name: String,
    pub props: Vec<Prop>,
    pub children: Vec<Component>,
}

impl Component {
    pub fn new(name: &str) -> Self {
        Self { name: name.to_ascii_uppercase(), ..Self::default() }
    }

    pub fn prop(&self, name: &str) -> Option<&Prop> {
        self.props.iter().find(|p| p.name.eq_ignore_ascii_case(name))
    }

    pub fn props_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Prop> + 'a {
        self.props.iter().filter(move |p| p.name.eq_ignore_ascii_case(name))
    }

    /// A TEXT property's value, unescaped.
    pub fn text(&self, name: &str) -> Option<String> {
        self.prop(name).map(Prop::text_value)
    }

    /// Replace every `name` property with `prop` (at the first one's place), or add it.
    pub fn set(&mut self, prop: Prop) {
        let at = self.props.iter().position(|p| p.name == prop.name);
        self.props.retain(|p| p.name != prop.name);
        let at = at.unwrap_or(self.props.len()).min(self.props.len());
        self.props.insert(at, prop);
    }

    pub fn remove(&mut self, name: &str) {
        self.props.retain(|p| !p.name.eq_ignore_ascii_case(name));
    }

    /// Children named `name`.
    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Component> + 'a {
        self.children.iter().filter(move |c| c.name.eq_ignore_ascii_case(name))
    }

    /// As iCalendar text (CRLF line ends, folded at 75 octets).
    pub fn to_ics(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        fold_into(out, &format!("BEGIN:{}", self.name));
        for p in &self.props {
            let mut line = p.name.clone();
            for (k, v) in &p.params {
                line.push(';');
                line.push_str(k);
                line.push('=');
                line.push_str(v);
            }
            line.push(':');
            line.push_str(&p.value);
            fold_into(out, &line);
        }
        for c in &self.children {
            c.write(out);
        }
        fold_into(out, &format!("END:{}", self.name));
    }
}

/// Append `line` folded to 75 octets per physical line (never inside a UTF-8 character).
fn fold_into(out: &mut String, line: &str) {
    let mut used = 0usize;
    for c in line.chars() {
        let n = c.len_utf8();
        if used + n > 75 {
            out.push_str("\r\n ");
            used = 1;
        }
        out.push(c);
        used += n;
    }
    out.push_str("\r\n");
}

/// Parse iCalendar text into its top-level components (usually one VCALENDAR). Oversized
/// input is cut at [`MAX_OBJECT_BYTES`]; lines that aren't content lines are skipped.
pub fn parse(text: &str) -> Vec<Component> {
    let text = cut(text, MAX_OBJECT_BYTES);
    let mut top: Vec<Component> = Vec::new();
    let mut stack: Vec<Component> = Vec::new();
    let mut count = 0usize;
    for line in unfold(text) {
        let Some(prop) = parse_line(&line) else { continue };
        if prop.name == "BEGIN" {
            let name = prop.value.trim().to_ascii_uppercase();
            if stack.len() >= MAX_DEPTH || count >= MAX_COMPONENTS || name.is_empty() {
                // too deep or too many: read on, but keep nothing of it
                stack.push(Component { name: format!("\u{0}{name}"), ..Component::default() });
                continue;
            }
            count += 1;
            stack.push(Component::new(&name));
        } else if prop.name == "END" {
            let name = prop.value.trim().to_ascii_uppercase();
            // close up to the matching BEGIN (an END without one is ignored)
            let Some(at) = stack.iter().rposition(|c| c.name.trim_start_matches('\u{0}') == name) else { continue };
            while stack.len() > at {
                let Some(done) = stack.pop() else { break };
                attach(&mut stack, &mut top, done);
            }
        } else if let Some(cur) = stack.last_mut()
            && cur.props.len() < MAX_PROPS
        {
            cur.props.push(prop);
        }
    }
    while let Some(done) = stack.pop() {
        attach(&mut stack, &mut top, done);
    }
    top
}

fn attach(stack: &mut [Component], top: &mut Vec<Component>, done: Component) {
    if done.name.starts_with('\u{0}') {
        return;
    }
    match stack.last_mut() {
        Some(parent) if !parent.name.starts_with('\u{0}') => parent.children.push(done),
        Some(_) => {}
        None => top.push(done),
    }
}

/// At most `max` bytes of `s`, on a character boundary.
pub fn cut(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.get(..end).unwrap_or_default()
}

/// Logical lines: a line starting with a space or tab continues the one before.
fn unfold(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        match (raw.strip_prefix([' ', '\t']), out.last_mut()) {
            (Some(rest), Some(last)) => last.push_str(rest),
            _ if raw.is_empty() => {}
            _ => out.push(raw.to_owned()),
        }
    }
    out
}

/// `NAME *(;PARAM=value) : VALUE`, with quoted parameter values that may hold `;:,`.
fn parse_line(line: &str) -> Option<Prop> {
    let mut chars = line.char_indices().peekable();
    let mut name_end = None;
    while let Some(&(i, c)) = chars.peek() {
        if c == ';' || c == ':' {
            name_end = Some(i);
            break;
        }
        chars.next();
    }
    let name_end = name_end?;
    let name = line.get(..name_end)?.trim();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return None;
    }
    let mut params = Vec::new();
    let mut rest = line.get(name_end..)?;
    loop {
        if let Some(value) = rest.strip_prefix(':') {
            return Some(Prop { name: name.to_ascii_uppercase(), params, value: value.to_owned() });
        }
        let after = rest.strip_prefix(';')?;
        let eq = after.find('=')?;
        let pname = after.get(..eq)?.trim().to_ascii_uppercase();
        let mut i = eq + 1;
        let bytes = after.as_bytes();
        let mut quoted = false;
        while let Some(&b) = bytes.get(i) {
            match b {
                b'"' => quoted = !quoted,
                b';' | b':' if !quoted => break,
                _ => {}
            }
            i += 1;
        }
        let pvalue = after.get(eq + 1..i)?.to_owned();
        if params.len() < 32 {
            params.push((pname, pvalue));
        }
        rest = after.get(i..)?;
    }
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    v.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(v).to_owned()
}

/// A parameter value, quoted if it needs to be (quotes themselves can't be written).
fn quote_param(v: &str) -> String {
    let v: String = v.chars().filter(|c| *c != '"' && !c.is_control()).collect();
    if v.contains([';', ':', ',']) { format!("\"{v}\"") } else { v }
}

/// A TEXT value's escapes (`\\`, `\;`, `\,`, `\n`, `\N`) undone.
pub fn unescape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut it = v.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n' | 'N') => out.push('\n'),
            Some(o) => out.push(o),
            None => {}
        }
    }
    out
}

/// Escape text for a TEXT value (control characters other than newlines dropped).
pub fn escape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_folded_lines_params_and_nesting() {
        let text = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nSUMMARY:Long \r\n title\\, with\\ncomma\r\nATTENDEE;CN=\"Doe, Jane\";ROLE=REQ-PARTICIPANT:mailto:jane@example.com\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let top = parse(text);
        assert_eq!(top.len(), 1);
        let ev = top[0].children_named("VEVENT").next().unwrap();
        assert_eq!(ev.text("SUMMARY").as_deref(), Some("Long title, with\ncomma"));
        let att = ev.prop("ATTENDEE").unwrap();
        assert_eq!(att.param("CN").as_deref(), Some("Doe, Jane"));
        assert_eq!(att.value, "mailto:jane@example.com");
        assert_eq!(ev.children.len(), 1);
    }

    #[test]
    fn hostile_input_doesnt_break_it() {
        assert!(parse("").is_empty());
        assert!(parse("garbage\n:no name\nEND:VEVENT\n;;;\n").is_empty());
        // unclosed components are closed at the end
        let top = parse("BEGIN:VCALENDAR\nBEGIN:VEVENT\nSUMMARY:x");
        assert_eq!(top[0].children[0].text("SUMMARY").as_deref(), Some("x"));
        // deep nesting is dropped, not recursed into
        let deep = "BEGIN:X\n".repeat(100) + &"END:X\n".repeat(100);
        let top = parse(&deep);
        let mut depth = 0;
        let mut c = top.first();
        while let Some(x) = c {
            depth += 1;
            c = x.children.first();
        }
        assert_eq!(depth, MAX_DEPTH);
        assert_eq!(cut("héllo", 2), "h");
    }

    #[test]
    fn writes_what_it_read_folded() {
        let mut ev = Component::new("VEVENT");
        ev.set(Prop::text("SUMMARY", &"Ünïcode, long; ".repeat(10)));
        ev.set(Prop::new("ATTENDEE", "mailto:a@b").with_param("CN", "A, B"));
        let text = ev.to_ics();
        assert!(text.lines().all(|l| l.len() <= 76), "{text}");
        let back = parse(&text);
        assert_eq!(back[0], ev);
        assert_eq!(back[0].text("SUMMARY").unwrap(), "Ünïcode, long; ".repeat(10));
        assert_eq!(escape("a\\b;c,d\ne\u{7}"), "a\\\\b\\;c\\,d\\ne");
    }
}
