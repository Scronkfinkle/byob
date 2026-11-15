//! WebDAV XML (multistatus answers) as a small owned tree with resolved namespaces.
//! Hostile input: element count, depth and text are capped; nothing recurses on input.

use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;

pub const DAV: &str = "DAV:";
pub const CALDAV: &str = "urn:ietf:params:xml:ns:caldav";
pub const APPLE: &str = "http://apple.com/ns/ical/";
pub const CS: &str = "http://calendarserver.org/ns/";

const MAX_ELEMENTS: usize = 200_000;
const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct El {
    pub ns: String,
    pub name: String,
    pub children: Vec<El>,
    /// Its own text (CDATA and entities resolved), not its children's.
    pub text: String,
    /// Attributes (local name, value), at most a few.
    pub attrs: Vec<(String, String)>,
}

impl El {
    fn open(r: &ResolveResult<'_>, e: &quick_xml::events::BytesStart<'_>) -> Self {
        let attrs = e
            .attributes()
            .flatten()
            .take(8)
            .filter_map(|a| {
                Some((a.key.local_name().into_inner().to_owned(), a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok()?.chars().take(200).collect()))
            })
            .collect();
        Self { ns: ns_of(r), name: e.local_name().into_inner().to_owned(), attrs, ..Self::default() }
    }

    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    pub fn is(&self, ns: &str, name: &str) -> bool {
        self.ns == ns && self.name == name
    }

    pub fn child(&self, ns: &str, name: &str) -> Option<&El> {
        self.children.iter().find(|c| c.is(ns, name))
    }

    pub fn children_named<'a>(&'a self, ns: &'a str, name: &'a str) -> impl Iterator<Item = &'a El> + 'a {
        self.children.iter().filter(move |c| c.is(ns, name))
    }

    /// The first descendant (breadth-first, iterative) named so.
    pub fn find(&self, ns: &str, name: &str) -> Option<&El> {
        let mut queue: std::collections::VecDeque<&El> = self.children.iter().collect();
        while let Some(e) = queue.pop_front() {
            if e.is(ns, name) {
                return Some(e);
            }
            queue.extend(e.children.iter());
        }
        None
    }

    pub fn trimmed(&self) -> &str {
        self.text.trim()
    }
}

fn ns_of(r: &ResolveResult<'_>) -> String {
    match r {
        ResolveResult::Bound(ns) => ns.into_inner().to_owned(),
        _ => String::new(),
    }
}

/// Parse a document into its root element.
pub fn parse(text: &str) -> Result<El, String> {
    let mut reader = NsReader::from_str(text);
    let mut stack: Vec<El> = vec![El::default()];
    let mut count = 0usize;
    // Elements deeper than MAX_DEPTH are skipped (their depth is still tracked).
    let mut skipped = 0usize;
    loop {
        let ev = reader.read_resolved_event().map_err(|e| format!("the server's XML is broken: {e}"))?;
        match ev {
            (r, Event::Start(e)) => {
                count += 1;
                if skipped > 0 || stack.len() > MAX_DEPTH || count > MAX_ELEMENTS {
                    skipped += 1;
                    continue;
                }
                stack.push(El::open(&r, &e));
            }
            (r, Event::Empty(e)) => {
                count += 1;
                if skipped > 0 || stack.len() > MAX_DEPTH || count > MAX_ELEMENTS {
                    continue;
                }
                let el = El::open(&r, &e);
                if let Some(top) = stack.last_mut() {
                    top.children.push(el);
                }
            }
            (_, Event::End(_)) => {
                if skipped > 0 {
                    skipped -= 1;
                    continue;
                }
                if stack.len() > 1
                    && let Some(done) = stack.pop()
                    && let Some(top) = stack.last_mut()
                {
                    top.children.push(done);
                }
            }
            (_, Event::Text(t)) if skipped == 0 => push_text(&mut stack, &t.xml10_content()),
            (_, Event::CData(t)) if skipped == 0 => push_text(&mut stack, &t.xml10_content()),
            (_, Event::GeneralRef(r)) if skipped == 0 => {
                let resolved = match r.resolve_char_ref() {
                    Ok(Some(c)) => Some(c.to_string()),
                    _ => match r.xml10_content().as_ref() {
                        "lt" => Some("<".into()),
                        "gt" => Some(">".into()),
                        "amp" => Some("&".into()),
                        "quot" => Some("\"".into()),
                        "apos" => Some("'".into()),
                        _ => None,
                    },
                };
                if let Some(s) = resolved {
                    push_text(&mut stack, &s);
                }
            }
            (_, Event::Eof) => break,
            _ => {}
        }
    }
    // unclosed elements close at the end
    while stack.len() > 1 {
        if let Some(done) = stack.pop()
            && let Some(top) = stack.last_mut()
        {
            top.children.push(done);
        }
    }
    let doc = stack.pop().unwrap_or_default();
    doc.children.into_iter().next().ok_or_else(|| "the server sent no XML".to_owned())
}

fn push_text(stack: &mut [El], s: &str) {
    if let Some(top) = stack.last_mut()
        && top.text.len() < crate::ical::MAX_OBJECT_BYTES
    {
        top.text.push_str(s);
    }
}

/// `<` `>` `&` `"` escaped for XML text and attributes.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            c if c.is_control() && c != '\n' && c != '\t' => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_namespaces_entities_and_cdata() {
        let doc = r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><d:response><d:href>/a&amp;b/</d:href><d:propstat><d:prop><C:calendar-data>BEGIN:VCALENDAR&#13;
X:1 &lt;2&gt;</C:calendar-data><x:other xmlns:x="urn:x"><![CDATA[<raw>]]></x:other><d:resourcetype/></d:prop></d:propstat></d:response></d:multistatus>"#;
        let root = parse(doc).unwrap();
        assert!(root.is(DAV, "multistatus"));
        let r = root.child(DAV, "response").unwrap();
        assert_eq!(r.child(DAV, "href").unwrap().text, "/a&b/");
        assert_eq!(root.find(CALDAV, "calendar-data").unwrap().text, "BEGIN:VCALENDAR\r\nX:1 <2>");
        assert_eq!(root.find("urn:x", "other").unwrap().text, "<raw>");
        assert!(root.find(DAV, "resourcetype").is_some());
    }

    #[test]
    fn broken_or_deep_xml_is_an_error_or_capped() {
        assert!(parse("").is_err());
        assert!(parse("<a><b></a>").is_err());
        assert_eq!(parse(r#"<a x="1 &amp; 2"/>"#).unwrap().attr("x"), Some("1 & 2"));
        let deep = "<a>".repeat(1000) + &"</a>".repeat(1000);
        let root = parse(&deep).unwrap();
        let mut depth = 0;
        let mut e = Some(&root);
        while let Some(x) = e {
            depth += 1;
            e = x.children.first();
        }
        assert!(depth <= MAX_DEPTH + 1, "{depth}");
        assert_eq!(escape("a<b>&\"c\u{1}"), "a&lt;b&gt;&amp;&quot;c");
    }
}
