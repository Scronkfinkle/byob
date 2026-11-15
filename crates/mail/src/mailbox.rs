//! IMAP: one short session per tool call (connect, log in, do it, log out). Reads use
//! `EXAMINE` and `BODY.PEEK`, so looking at mail never marks it read.

use std::collections::BTreeMap;

use imap::types::Flag;
use imap_proto::NameAttribute;

use crate::config::Server;
use crate::net::{self, Security, Stream};
use crate::parse::one_line;
use crate::utf7;

/// Bytes of a message read for a list entry (headers and the start of the body).
pub const PEEK_BYTES: usize = 24 * 1024;
/// Bytes of a message read by `mail_read`.
pub const READ_BYTES: usize = 8 * 1024 * 1024;
/// Folders listed (and counted).
pub const MAX_FOLDERS: usize = 200;

/// A message's place: its folder and UID, shown to the model as `INBOX:42`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MsgId {
    pub folder: String,
    pub uid: u32,
}

impl MsgId {
    /// `Folder:UID` (the folder may itself hold `:`, so the last one splits).
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let (folder, uid) = s.rsplit_once(':').ok_or_else(|| format!("{s:?} isn't a message id: ids look like INBOX:42 (from mail_search)"))?;
        let uid: u32 =
            uid.trim().parse().ok().filter(|u| *u > 0).ok_or_else(|| format!("{s:?} isn't a message id: ids look like INBOX:42 (from mail_search)"))?;
        let folder = folder.trim();
        if folder.is_empty() {
            return Err(format!("{s:?} has no folder: ids look like INBOX:42"));
        }
        Ok(Self { folder: folder.to_owned(), uid })
    }
}

impl std::fmt::Display for MsgId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.folder, self.uid)
    }
}

/// A folder: its readable name, special use, counts (when the server told).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    pub name: String,
    pub special: Option<&'static str>,
    pub messages: Option<u32>,
    pub unseen: Option<u32>,
}

/// One fetched message (or its first bytes).
#[derive(Debug, Clone, Default)]
pub struct Fetched {
    pub uid: u32,
    pub flags: Vec<String>,
    /// Server's arrival date (RFC 3339), a fallback for a missing `Date:`.
    pub internal_date: Option<String>,
    pub size: Option<u32>,
    pub raw: Vec<u8>,
}

/// What `mail_search` looks for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Search {
    pub from: Option<String>,
    pub to: Option<String>,
    pub subject: Option<String>,
    pub text: Option<String>,
    /// IMAP dates (`1-Feb-2024`).
    pub since: Option<String>,
    pub before: Option<String>,
    pub unread: Option<bool>,
    pub flagged: Option<bool>,
}

/// A folder name the model gave, for the wire (modified UTF-7). Quotes, backslashes and
/// control characters are refused: some commands put the name in quotes as it is.
pub fn wire(folder: &str) -> Result<String, String> {
    if folder.is_empty() || folder.len() > 300 || folder.chars().any(|c| c.is_control() || c == '"' || c == '\\') {
        return Err(format!("{:?} isn't a usable folder name (mail_folders lists them)", one_line(folder, 100)));
    }
    Ok(utf7::encode(folder))
}

/// An IMAP quoted string:`"` and `\` escaped, line breaks and control characters out.
pub fn quote(s: &str) -> String {
    let mut out = String::from('"');
    for c in s.chars().filter(|c| !c.is_control()).take(200) {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

impl Search {
    /// The `UID SEARCH` criteria.
    pub fn criteria(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        for (key, v) in [("FROM", &self.from), ("TO", &self.to), ("SUBJECT", &self.subject), ("TEXT", &self.text)] {
            if let Some(v) = v {
                parts.push(format!("{key} {}", quote(v)));
            }
        }
        for (key, v) in [("SINCE", &self.since), ("BEFORE", &self.before)] {
            if let Some(v) = v {
                parts.push(format!("{key} {v}"));
            }
        }
        match self.unread {
            Some(true) => parts.push("UNSEEN".into()),
            Some(false) => parts.push("SEEN".into()),
            None => {}
        }
        match self.flagged {
            Some(true) => parts.push("FLAGGED".into()),
            Some(false) => parts.push("UNFLAGGED".into()),
            None => {}
        }
        if parts.is_empty() {
            parts.push("ALL".into());
        }
        let all = parts.join(" ");
        // Servers take UTF-8 in quoted strings once told the charset (Dovecot, Cyrus,
        // Fastmail, Gmail, GreenMail); strict ones would want literals.
        if all.is_ascii() { all } else { format!("CHARSET UTF-8 {all}") }
    }
}

/// A connected, logged-in IMAP session.
pub struct Session {
    s: imap::Session<Stream>,
    caps: Vec<String>,
}

/// A server error as one readable line.
fn err(what: &str, e: &imap::Error) -> String {
    let text = match e {
        imap::Error::No(n) => format!("the server said no: {}", n.information),
        imap::Error::Bad(b) => format!("the server rejected the command: {}", b.information),
        imap::Error::Bye(b) => format!("the server closed the session: {}", b.information),
        imap::Error::ConnectionLost => "the connection was lost".into(),
        other => other.to_string(),
    };
    one_line(&format!("{what}: {text}"), 400)
}

fn io_err(what: &str, host: &str, e: &std::io::Error) -> String {
    let hint = match e.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => " (timed out)",
        _ => "",
    };
    one_line(&format!("{what} {host}{hint}: {e}"), 400)
}

/// Connect and log in.
pub fn open(server: &Server, username: &str, password: &str) -> Result<Session, String> {
    let host = server.host.as_str();
    let tcp = net::connect(host, server.port).map_err(|e| io_err("couldn't connect to the IMAP server", &format!("{host}:{}", server.port), &e))?;
    let (stream, greeted) = match server.security {
        Security::Tls => (Stream::tls(tcp, host).map_err(|e| io_err("TLS with", host, &e))?, false),
        Security::Plain => (Stream::plain(tcp), false),
        Security::StartTls => {
            let mut tcp = tcp;
            starttls(&mut tcp).map_err(|e| one_line(&format!("STARTTLS with {host}: {e}"), 400))?;
            (Stream::tls(tcp, host).map_err(|e| io_err("TLS with", host, &e))?, true)
        }
    };
    let mut client = imap::Client::new(stream);
    if greeted {
        client.greeting_read = true;
    } else {
        let greeting = client.read_greeting().map_err(|e| err("the IMAP server didn't greet", &e))?;
        if !String::from_utf8_lossy(&greeting).to_ascii_uppercase().starts_with("* OK") {
            return Err(one_line(&format!("the IMAP server refused the connection: {}", String::from_utf8_lossy(&greeting)), 400));
        }
    }
    let mut s = client.login(username, password).map_err(|(e, _)| match e {
        imap::Error::No(_) => "The IMAP server refused the username or password (for Gmail and iCloud use an app password).".to_owned(),
        e => err("logging in", &e),
    })?;
    let caps = s
        .capabilities()
        .map(|c| {
            c.iter()
                .filter_map(|c| match c {
                    imap_proto::Capability::Atom(a) => Some(a.to_ascii_uppercase()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Session { s, caps })
}

/// The plain-text part of STARTTLS, before anything is buffered: greeting, `STARTTLS`, OK.
fn starttls(tcp: &mut std::net::TcpStream) -> std::io::Result<()> {
    use std::io::Write;
    let greeting = net::read_line(tcp, 8192)?;
    if !greeting.to_ascii_uppercase().starts_with("* OK") {
        return Err(std::io::Error::other(format!("unexpected greeting {:?}", one_line(&greeting, 200))));
    }
    tcp.write_all(b"S0 STARTTLS\r\n")?;
    for _ in 0..32 {
        let line = net::read_line(tcp, 8192)?;
        if let Some(rest) = line.strip_prefix("S0 ") {
            return if rest.to_ascii_uppercase().starts_with("OK") {
                Ok(())
            } else {
                Err(std::io::Error::other(format!("the server doesn't offer it: {}", one_line(rest, 200))))
            };
        }
    }
    Err(std::io::Error::other("no answer to STARTTLS"))
}

fn flag_name(f: &Flag<'_>) -> String {
    match f {
        Flag::Seen => "seen".into(),
        Flag::Answered => "answered".into(),
        Flag::Flagged => "flagged".into(),
        Flag::Deleted => "deleted".into(),
        Flag::Draft => "draft".into(),
        Flag::Recent => "recent".into(),
        Flag::MayCreate => "may-create".into(),
        Flag::Custom(c) => one_line(c, 40),
        _ => "other".into(),
    }
}

fn special_of(attrs: &[NameAttribute<'_>]) -> Option<&'static str> {
    attrs.iter().find_map(|a| match a {
        NameAttribute::Drafts => Some("drafts"),
        NameAttribute::Sent => Some("sent"),
        NameAttribute::Trash => Some("trash"),
        NameAttribute::Junk => Some("junk"),
        NameAttribute::Archive => Some("archive"),
        NameAttribute::All => Some("all"),
        NameAttribute::Flagged => Some("flagged"),
        _ => None,
    })
}

/// Common names of the special folders, for servers without special-use flags.
fn guess_special(name: &str) -> Option<&'static str> {
    let n = name.rsplit(['/', '.']).next().unwrap_or(name).to_ascii_lowercase();
    Some(match n.as_str() {
        "drafts" | "draft" | "entwürfe" | "brouillons" => "drafts",
        "sent" | "sent items" | "sent messages" | "sent mail" | "gesendet" | "envoyés" => "sent",
        "trash" | "deleted items" | "deleted messages" | "bin" => "trash",
        "junk" | "spam" | "junk e-mail" => "junk",
        "archive" => "archive",
        _ => return None,
    })
}

impl Session {
    pub fn has(&self, cap: &str) -> bool {
        self.caps.iter().any(|c| c.eq_ignore_ascii_case(cap))
    }

    /// The folders, as listed.
    fn list(&mut self) -> Result<Vec<Listed>, String> {
        let names = self.s.list(None, Some("*")).map_err(|e| err("listing folders", &e))?;
        let mut out = Vec::new();
        for n in names.iter().take(MAX_FOLDERS) {
            let wire = n.name().to_owned();
            let name = utf7::decode(&wire);
            let attrs = n.attributes();
            let flagged = special_of(attrs);
            let special = flagged.or_else(|| guess_special(&name));
            let selectable = !attrs.iter().any(|a| matches!(a, NameAttribute::NoSelect));
            out.push(Listed { wire, name, special, by_flag: flagged.is_some(), selectable });
        }
        Ok(out)
    }

    /// The folders, with counts.
    pub fn folders(&mut self) -> Result<Vec<Folder>, String> {
        let mut out = Vec::new();
        for Listed { wire, name, special, selectable, .. } in self.list()? {
            let (messages, unseen) = if selectable {
                match self.s.status(&wire, "(MESSAGES UNSEEN)") {
                    Ok(m) => (Some(m.exists), m.unseen),
                    Err(_) => (None, None),
                }
            } else {
                (None, None)
            };
            out.push(Folder { name, special, messages, unseen });
        }
        // INBOX first, then special folders, then the rest by name.
        out.sort_by_key(|f| (!f.name.eq_ignore_ascii_case("INBOX"), f.special.is_none(), f.name.to_lowercase()));
        Ok(out)
    }

    /// The folder for a special use (`drafts`, `sent`): the setting, the server's flag, or
    /// a common name.
    pub fn special(&mut self, kind: &str, setting: Option<&str>) -> Result<String, String> {
        if let Some(s) = setting {
            return Ok(s.to_owned());
        }
        let list = self.list()?;
        let of_kind = |by_flag: bool| list.iter().find(|l| l.selectable && l.special == Some(kind) && l.by_flag == by_flag);
        match of_kind(true).or_else(|| of_kind(false)) {
            Some(l) => Ok(l.name.clone()),
            None => Err(format!("found no {kind} folder; set one under Settings → Plugins → Mail")),
        }
    }

    /// Message UIDs matching `q` in `folder`, newest first.
    pub fn search(&mut self, folder: &str, q: &Search) -> Result<Vec<u32>, String> {
        self.s.examine(wire(folder)?).map_err(|e| err(&format!("opening {folder}"), &e))?;
        let uids = self.s.uid_search(q.criteria()).map_err(|e| err("searching", &e))?;
        let mut uids: Vec<u32> = uids.into_iter().collect();
        uids.sort_unstable_by(|a, b| b.cmp(a));
        Ok(uids)
    }

    /// The first `max` bytes of messages `uids` in the folder opened last (by `search`, or
    /// `open_folder`), in the order asked.
    pub fn fetch(&mut self, uids: &[u32], max: usize) -> Result<Vec<Fetched>, String> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let set = |uids: &[u32]| uids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
        // Sizes first: messages that fit come whole (`BODY.PEEK[]`), bigger ones as their
        // first `max` bytes. (Partial fetches only where needed: some servers, GreenMail
        // among them, answer those in a form the parser refuses.)
        let meta = self.s.uid_fetch(set(uids), "(UID FLAGS INTERNALDATE RFC822.SIZE)").map_err(|e| err("reading messages", &e))?;
        let mut by_uid: BTreeMap<u32, Fetched> = BTreeMap::new();
        for f in meta.iter() {
            let Some(uid) = f.uid else { continue };
            let fetched = Fetched {
                uid,
                flags: f.flags().iter().map(flag_name).collect(),
                internal_date: f.internal_date().map(|d| d.to_rfc3339()),
                size: f.size,
                raw: Vec::new(),
            };
            by_uid.insert(uid, fetched);
        }
        drop(meta);
        let (mut whole, mut cut) = (Vec::new(), Vec::new());
        for f in by_uid.values() {
            if f.size.is_some_and(|s| s as usize <= max) { whole.push(f.uid) } else { cut.push(f.uid) }
        }
        for (uids, query) in [(whole, "(UID BODY.PEEK[])".to_owned()), (cut, format!("(UID BODY.PEEK[]<0.{max}>)"))] {
            if uids.is_empty() {
                continue;
            }
            let bodies = self.s.uid_fetch(set(&uids), query).map_err(|e| err("reading messages", &e))?;
            for f in bodies.iter() {
                if let (Some(uid), Some(body)) = (f.uid, f.body())
                    && let Some(m) = by_uid.get_mut(&uid)
                {
                    m.raw = body.get(..body.len().min(max)).unwrap_or_default().to_vec();
                }
            }
        }
        Ok(uids.iter().filter_map(|u| by_uid.remove(u)).collect())
    }

    /// Open `folder` for reading (`write`: for changing flags or moving).
    pub fn open_folder(&mut self, folder: &str, write: bool) -> Result<(), String> {
        let wire = wire(folder)?;
        let r = if write { self.s.select(&wire).map(|_| ()) } else { self.s.examine(&wire).map(|_| ()) };
        r.map_err(|e| match e {
            imap::Error::No(_) => format!("there's no folder {folder:?} (mail_folders lists them)"),
            e => err(&format!("opening {folder}"), &e),
        })
    }

    /// Store a message in `folder`; its UID when the server says (UIDPLUS).
    pub fn append(&mut self, folder: &str, raw: &[u8], flags: &[Flag<'static>]) -> Result<Option<u32>, String> {
        let wire = wire(folder)?;
        let mut cmd = self.s.append(&wire, raw);
        for f in flags {
            cmd.flag(f.clone());
        }
        let appended = cmd.finish().map_err(|e| err(&format!("saving to {folder}"), &e))?;
        Ok(appended.uids.and_then(|u| {
            u.into_iter()
                .map(|m| match m {
                    imap_proto::UidSetMember::Uid(u) => u,
                    imap_proto::UidSetMember::UidRange(r) => *r.end(),
                })
                .next()
        }))
    }

    /// Add or remove a flag on `uids` in the folder opened (for writing) last.
    pub fn store(&mut self, uids: &[u32], add: bool, flag: &str) -> Result<(), String> {
        let set: Vec<String> = uids.iter().map(u32::to_string).collect();
        let op = if add { "+FLAGS.SILENT" } else { "-FLAGS.SILENT" };
        self.s.uid_store(set.join(","), format!("{op} ({flag})")).map(|_| ()).map_err(|e| err("changing flags", &e))
    }

    /// Move `uids` from the folder opened last to `to`. `Ok(false)`: copied and marked deleted,
    /// but the server can't expunge just them (no UIDPLUS), so they stay until it's emptied.
    pub fn move_to(&mut self, uids: &[u32], to: &str) -> Result<bool, String> {
        let set: Vec<String> = uids.iter().map(u32::to_string).collect();
        let set = set.join(",");
        let wire = wire(to)?;
        if self.has("MOVE") {
            self.s.uid_mv(&set, &wire).map_err(|e| err(&format!("moving to {to}"), &e))?;
            return Ok(true);
        }
        self.s.uid_copy(&set, &wire).map_err(|e| err(&format!("copying to {to}"), &e))?;
        self.store(uids, true, "\\Deleted")?;
        if self.has("UIDPLUS") {
            self.s.uid_expunge(&set).map_err(|e| err("removing the originals", &e))?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Delete `uid` from the folder opened (for writing) last, e.g. a draft once sent.
    pub fn delete(&mut self, uid: u32) -> Result<(), String> {
        self.store(&[uid], true, "\\Deleted")?;
        if self.has("UIDPLUS") {
            self.s.uid_expunge(uid.to_string()).map_err(|e| err("removing the draft", &e))?;
        }
        Ok(())
    }

    pub fn logout(mut self) {
        let _ = self.s.logout();
    }
}

/// A folder as LIST gave it.
struct Listed {
    wire: String,
    name: String,
    special: Option<&'static str>,
    /// `special` is the server's special-use flag (not guessed from the name).
    by_flag: bool,
    selectable: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_criteria() {
        assert_eq!(MsgId::parse(" INBOX:42 ").unwrap(), MsgId { folder: "INBOX".into(), uid: 42 });
        assert_eq!(MsgId::parse("Work:Clients:7").unwrap().folder, "Work:Clients");
        for bad in ["42", "INBOX:", "INBOX:0", ":5", "INBOX:x"] {
            assert!(MsgId::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(Search::default().criteria(), "ALL");
        let q = Search { from: Some("a\"b\\c\r\n".into()), since: Some("1-Feb-2024".into()), unread: Some(true), ..Default::default() };
        assert_eq!(q.criteria(), r#"FROM "a\"b\\c" SINCE 1-Feb-2024 UNSEEN"#);
        let q = Search { subject: Some("Grüße".into()), flagged: Some(false), ..Default::default() };
        assert_eq!(q.criteria(), "CHARSET UTF-8 SUBJECT \"Grüße\" UNFLAGGED");
        assert_eq!(guess_special("INBOX/Sent Items"), Some("sent"));
        assert_eq!(guess_special("[Gmail]/Drafts"), Some("drafts"));
    }
}
