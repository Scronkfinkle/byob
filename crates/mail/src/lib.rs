//! byob's email: the bundled `mail` plugin's backend, in-process. One IMAP + SMTP account
//! from the plugin's settings (the password is a secret setting: it arrives in `init`'s
//! `secrets` and never leaves byob's process). Tools: `mail_folders`, `mail_search`,
//! `mail_read` (reads; `EXAMINE` and `BODY.PEEK`, so nothing is marked read), `mail_draft`
//! and `mail_mark` (writes), and `mail_send`, which the manifest marks `"confirm"`: a person
//! approves every call, whatever the approval mode.
//!
//! Email is hostile input: every network call has a deadline (`net`), a connection reads at
//! most [`net::MAX_READ`] bytes, message text is cleaned and capped (`parse`), and results
//! start with [`parse::UNTRUSTED`]. Links are never followed, remote images never loaded,
//! attachments only listed. Blocking I/O on the backend's own thread, no async.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

pub mod compose;
pub mod config;
pub mod mailbox;
pub mod net;
pub mod parse;
pub mod utf7;

#[cfg(test)]
mod fake;
#[cfg(test)]
mod tests;

use std::fmt::Write as _;
use std::sync::Arc;

use byob_plugins::native::{NativeBackend, NativeFactory, Outbox};
use imap::types::Flag;
use serde_json::Value;

use crate::compose::{Draft, split_addresses};
use crate::config::Account;
use crate::mailbox::{MsgId, PEEK_BYTES, READ_BYTES, Search, Session};
use crate::parse::{Parsed, UNTRUSTED, one_line, size, snippet};

/// The id manifests name (`"backend": {"native": "mail"}`).
pub const ID: &str = "mail";
/// The tools it serves.
pub const TOOLS: [&str; 6] = ["mail_folders", "mail_search", "mail_read", "mail_draft", "mail_send", "mail_mark"];

/// Messages listed by default, and at most.
const DEFAULT_LIMIT: usize = 10;
const MAX_LIMIT: usize = 50;
/// Characters of a body `mail_read` returns by default, at least and at most.
const DEFAULT_CHARS: usize = 12_000;
const MIN_CHARS: usize = 1_000;
const MAX_CHARS: usize = 40_000;
/// Messages one `mail_mark` changes.
const MAX_MARK: usize = 100;

pub fn factory() -> NativeFactory {
    Arc::new(|| Box::new(Mail::default()) as Box<dyn NativeBackend>)
}

/// The backend: the account from `init`.
#[derive(Default)]
pub struct Mail {
    account: Account,
}

fn text(args: &Value, key: &str) -> Option<String> {
    match args.get(key)? {
        Value::String(s) => Some(s.trim().to_owned()).filter(|s| !s.is_empty()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A body: kept as written (only trailing whitespace trimmed).
fn body(args: &Value) -> String {
    args.get("body").and_then(Value::as_str).map(str::trim_end).unwrap_or_default().to_owned()
}

fn flag(args: &Value, key: &str) -> Option<bool> {
    match args.get(key)? {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" => Some(true),
            "false" | "no" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// A non-negative whole number argument (models send `5`, `5.0` or `"5"`).
fn uint(args: &Value, key: &str) -> Option<usize> {
    let v = args.get(key)?;
    let n = v.as_u64().or_else(|| v.as_f64().filter(|f| f.is_finite() && *f >= 0.0).map(|f| f as u64)).or_else(|| v.as_str()?.trim().parse().ok())?;
    usize::try_from(n).ok()
}

/// Addresses from a string ("a@x, b@y") or a list.
fn addresses(args: &Value, key: &str) -> Vec<String> {
    match args.get(key) {
        Some(Value::String(s)) => split_addresses(s),
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).flat_map(split_addresses).collect(),
        _ => Vec::new(),
    }
}

/// Message ids from `id`/`ids`: a string, comma-separated ids, or a list.
fn ids(args: &Value) -> Result<Vec<MsgId>, String> {
    let mut raw: Vec<String> = Vec::new();
    for key in ["ids", "id"] {
        match args.get(key) {
            Some(Value::String(s)) => raw.extend(s.split([',', ' ']).map(str::to_owned)),
            Some(Value::Array(a)) => raw.extend(a.iter().filter_map(Value::as_str).map(str::to_owned)),
            _ => {}
        }
    }
    raw.retain(|s| !s.trim().is_empty());
    if raw.is_empty() {
        return Err("Give the message id (id), like INBOX:42 from mail_search.".into());
    }
    if raw.len() > MAX_MARK {
        return Err(format!("at most {MAX_MARK} messages at once"));
    }
    raw.iter().map(|s| MsgId::parse(s)).collect()
}

/// `2024-02-01` (or `2024-02-01T…`) as an IMAP date (`1-Feb-2024`).
fn imap_date(s: &str, key: &str) -> Result<String, String> {
    let day = s.trim().get(..10).unwrap_or(s.trim());
    chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").map(|d| d.format("%-d-%b-%Y").to_string()).map_err(|_| format!("{key} must be a date like 2024-02-01"))
}

/// A date for lists: `2024-09-03 10:00` (the sender's clock).
fn short_date(rfc3339: &str) -> String {
    rfc3339.get(..16).map(|d| d.replace('T', " ")).unwrap_or_else(|| rfc3339.to_owned())
}

/// The flags worth telling the model.
fn flag_words(flags: &[String]) -> Vec<&'static str> {
    let has = |f: &str| flags.iter().any(|x| x == f);
    let mut out = Vec::new();
    if !has("seen") {
        out.push("unread");
    }
    for (f, word) in [("flagged", "flagged"), ("answered", "replied"), ("draft", "draft")] {
        if has(f) {
            out.push(word);
        }
    }
    out
}

impl Mail {
    /// Apply the plugin's settings (see the manifest).
    pub fn configure(&mut self, init: &Value, out: Option<&Outbox>) {
        self.account = Account::from_init(init);
        if let Some(out) = out {
            for p in &self.account.problems {
                out.fault(p);
            }
        }
    }

    fn session(&self) -> Result<Session, String> {
        let server = self.account.imap()?;
        mailbox::open(server, &self.account.username, &self.account.password)
    }

    /// A tool call: the text for the model, or the error sentence.
    pub fn tool(&mut self, name: &str, args: &Value) -> Result<Value, String> {
        let text = match name {
            "mail_folders" => self.folders()?,
            "mail_search" => self.search(args)?,
            "mail_read" => self.read(args)?,
            "mail_draft" => self.draft(args)?,
            "mail_send" => self.send(args)?,
            "mail_mark" => self.mark(args)?,
            other => return Err(format!("the mail backend has no tool {other:?}")),
        };
        Ok(Value::String(text))
    }

    fn folders(&self) -> Result<String, String> {
        let mut s = self.session()?;
        let folders = s.folders();
        s.logout();
        let folders = folders?;
        let mut out = format!("{} folders:\n", folders.len());
        for f in &folders {
            let _ = write!(out, "- {}", one_line(&f.name, 200));
            if let Some(sp) = f.special {
                let _ = write!(out, " ({sp})");
            }
            match (f.messages, f.unseen) {
                (Some(m), Some(u)) if u > 0 => {
                    let _ = write!(out, ": {m} messages, {u} unread");
                }
                (Some(m), _) => {
                    let _ = write!(out, ": {m} messages");
                }
                _ => {}
            }
            out.push('\n');
        }
        Ok(out.trim_end().to_owned())
    }

    fn search(&self, args: &Value) -> Result<String, String> {
        let folder = text(args, "folder").unwrap_or_else(|| "INBOX".into());
        let q = Search {
            from: text(args, "from"),
            to: text(args, "to"),
            subject: text(args, "subject"),
            text: text(args, "text").or_else(|| text(args, "query")),
            since: text(args, "since").map(|d| imap_date(&d, "since")).transpose()?,
            before: text(args, "before").map(|d| imap_date(&d, "before")).transpose()?,
            unread: flag(args, "unread"),
            flagged: flag(args, "flagged"),
        };
        let limit = uint(args, "limit").unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let mut s = self.session()?;
        let found = s.search(&folder, &q).and_then(|uids| {
            let total = uids.len();
            let page: Vec<u32> = uids.into_iter().take(limit).collect();
            Ok((total, s.fetch(&page, PEEK_BYTES)?))
        });
        s.logout();
        let (total, messages) = found?;
        let mut out = format!("{UNTRUSTED}\n");
        if total == 0 {
            let _ = write!(out, "No messages in {folder} match.");
            return Ok(out);
        }
        let _ = writeln!(out, "{folder}: {} of {total} matching messages, newest first.", messages.len());
        for (i, m) in messages.iter().enumerate() {
            let p = parse::parse(&m.raw);
            let date = if p.headers.date.is_empty() { m.internal_date.clone().unwrap_or_default() } else { p.headers.date.clone() };
            let id = MsgId { folder: folder.clone(), uid: m.uid };
            let _ = write!(out, "{}. id {id} | {} | From: {} | Subject: {}", i + 1, short_date(&date), or_none(&p.headers.from), or_none(&p.headers.subject));
            let words = flag_words(&m.flags);
            if !words.is_empty() {
                let _ = write!(out, " | {}", words.join(", "));
            }
            if !p.attachments.is_empty() {
                let _ = write!(out, " | {} attachment(s)", p.attachments.len());
            }
            let snip = snippet(&p.body);
            if !snip.is_empty() {
                let _ = write!(out, "\n   {snip}");
            }
            out.push('\n');
        }
        if total > messages.len() {
            let _ = write!(out, "[{} more; narrow the search or raise limit (at most {MAX_LIMIT}).]", total - messages.len());
        }
        Ok(out.trim_end().to_owned())
    }

    /// One message, whole (up to [`READ_BYTES`]).
    fn fetch_one(&self, s: &mut Session, id: &MsgId, max: usize) -> Result<mailbox::Fetched, String> {
        s.open_folder(&id.folder, false)?;
        s.fetch(&[id.uid], max)?.into_iter().next().ok_or_else(|| format!("there's no message {id} (it may have been moved or deleted; search again)"))
    }

    fn read(&self, args: &Value) -> Result<String, String> {
        let id = ids(args)?.into_iter().next().ok_or("Give the message id (id).")?;
        let start = uint(args, "start").unwrap_or(0);
        let max_chars = uint(args, "max_chars").unwrap_or(DEFAULT_CHARS).clamp(MIN_CHARS, MAX_CHARS);
        let mut s = self.session()?;
        let m = self.fetch_one(&mut s, &id, READ_BYTES);
        s.logout();
        let m = m?;
        let p = parse::parse(&m.raw);
        Ok(format_read(&id, &m, &p, start, max_chars))
    }

    /// The threading headers (and defaults) for a reply to `id`.
    fn reply_to(&self, s: &mut Session, id: &MsgId) -> Result<Parsed, String> {
        let m = self.fetch_one(s, id, 256 * 1024)?;
        Ok(parse::parse(&m.raw))
    }

    fn compose_args(&self, args: &Value) -> Draft {
        Draft {
            to: addresses(args, "to"),
            cc: addresses(args, "cc"),
            bcc: addresses(args, "bcc"),
            subject: text(args, "subject").unwrap_or_default(),
            body: body(args),
            ..Default::default()
        }
    }

    fn draft(&self, args: &Value) -> Result<String, String> {
        let mut d = self.compose_args(args);
        let reply = text(args, "in_reply_to").map(|r| MsgId::parse(&r)).transpose()?;
        let mut s = self.session()?;
        let result = (|| {
            if let Some(r) = &reply {
                let orig = self.reply_to(&mut s, r)?;
                thread(&mut d, &orig);
                if d.to.is_empty() {
                    let back = if orig.headers.reply_to.is_empty() { &orig.headers.from } else { &orig.headers.reply_to };
                    d.to = split_addresses(back);
                }
                if d.subject.is_empty() {
                    d.subject = re(&orig.headers.subject);
                }
            }
            let (raw, _) = compose::build(&self.account, &d, true)?;
            let folder = s.special("drafts", self.account.drafts.as_deref())?;
            let uid = s.append(&folder, &raw, &[Flag::Draft, Flag::Seen])?;
            Ok::<_, String>((folder, uid))
        })();
        s.logout();
        let (folder, uid) = result?;
        let mut out = String::from("Saved a draft");
        match uid {
            Some(uid) => {
                let _ = write!(out, " as {}", MsgId { folder: folder.clone(), uid });
            }
            None => {
                let _ = write!(out, " in {folder}");
            }
        }
        let _ = write!(out, ". To: {} | Subject: {}", or_none(&d.to.join(", ")), or_none(&d.subject));
        if !d.cc.is_empty() {
            let _ = write!(out, " | Cc: {}", d.cc.join(", "));
        }
        if d.in_reply_to.is_some() {
            out.push_str(" | threaded as a reply");
        }
        out.push_str(". The user can review and send it from their mail app, or you can send it with mail_send (always asks the user).");
        Ok(out)
    }

    fn send(&self, args: &Value) -> Result<String, String> {
        let mut d = self.compose_args(args);
        if d.to.is_empty() && d.cc.is_empty() && d.bcc.is_empty() {
            return Err("Give the recipients (to).".into());
        }
        if d.body.trim().is_empty() {
            return Err("Give the message text (body).".into());
        }
        // Check the settings before touching anything.
        self.account.smtp()?;
        let reply = text(args, "in_reply_to").map(|r| MsgId::parse(&r)).transpose()?;
        let draft = text(args, "draft").map(|r| MsgId::parse(&r)).transpose()?;
        let save = self.account.should_save_sent();
        let mut session = if reply.is_some() || draft.is_some() || save { Some(self.session()?) } else { None };
        if let (Some(r), Some(s)) = (&reply, session.as_mut()) {
            let orig = self.reply_to(s, r);
            match orig {
                Ok(orig) => thread(&mut d, &orig),
                Err(e) => {
                    if let Some(s) = session.take() {
                        s.logout();
                    }
                    return Err(format!("Not sent: couldn't read the message replied to ({e})."));
                }
            }
        }
        let sent = compose::send(&self.account, &d);
        let (raw, message_id) = match sent {
            Ok(x) => x,
            Err(e) => {
                if let Some(s) = session.take() {
                    s.logout();
                }
                return Err(format!("Not sent: {e}"));
            }
        };
        // Sent: from here on problems are notes, never errors (a retry would send it again).
        let mut out = format!("Sent to {}", d.to.iter().chain(&d.cc).chain(&d.bcc).cloned().collect::<Vec<_>>().join(", "));
        let _ = write!(out, " (Message-ID {message_id}).");
        if let Some(mut s) = session {
            if save {
                match s.special("sent", self.account.sent.as_deref()).and_then(|f| s.append(&f, &raw, &[Flag::Seen]).map(|_| f)) {
                    Ok(f) => {
                        let _ = write!(out, " Saved a copy in {f}.");
                    }
                    Err(e) => {
                        let _ = write!(out, " Note: no copy was saved in Sent ({e}).");
                    }
                }
            }
            if let Some(r) = &reply
                && s.open_folder(&r.folder, true).and_then(|()| s.store(&[r.uid], true, "\\Answered")).is_err()
            {
                out.push_str(" Note: couldn't mark the original as replied.");
            }
            if let Some(dr) = &draft {
                match s.open_folder(&dr.folder, true).and_then(|()| s.delete(dr.uid)) {
                    Ok(()) => {
                        let _ = write!(out, " Removed the draft {dr}.");
                    }
                    Err(e) => {
                        let _ = write!(out, " Note: the draft {dr} is still there ({e}).");
                    }
                }
            }
            s.logout();
        }
        Ok(out)
    }

    fn mark(&self, args: &Value) -> Result<String, String> {
        let ids = ids(args)?;
        let action = text(args, "action").map(|a| a.to_ascii_lowercase()).unwrap_or_default();
        let (add, flag) = match action.as_str() {
            "read" | "seen" => (true, "\\Seen"),
            "unread" | "unseen" => (false, "\\Seen"),
            "flag" | "flagged" | "star" => (true, "\\Flagged"),
            "unflag" | "unflagged" | "unstar" => (false, "\\Flagged"),
            "move" => (true, ""),
            _ => return Err("Give an action: read, unread, flag, unflag or move (with folder).".into()),
        };
        let to = if action == "move" { Some(text(args, "folder").ok_or("Give the folder to move to (folder).")?) } else { None };
        // by folder, in the order given
        let mut groups: Vec<(String, Vec<u32>)> = Vec::new();
        for id in &ids {
            match groups.iter_mut().find(|(f, _)| *f == id.folder) {
                Some((_, uids)) => uids.push(id.uid),
                None => groups.push((id.folder.clone(), vec![id.uid])),
            }
        }
        let mut s = self.session()?;
        let mut notes = Vec::new();
        let result = (|| {
            for (folder, uids) in &groups {
                s.open_folder(folder, true)?;
                match &to {
                    Some(dest) => {
                        if !s.move_to(uids, dest)? {
                            notes.push(format!("the server can't remove just these from {folder}: they are copied and marked deleted there"));
                        }
                    }
                    None => s.store(uids, add, flag)?,
                }
            }
            Ok::<_, String>(())
        })();
        s.logout();
        result?;
        let n = ids.len();
        let mut out = match &to {
            Some(dest) => format!("Moved {n} message(s) to {dest}."),
            None => {
                let word = if flag == "\\Seen" { "read" } else { "flagged" };
                format!("Marked {n} message(s) {}{word}.", if add { "" } else { "un" })
            }
        };
        for note in notes {
            let _ = write!(out, " Note: {note}.");
        }
        Ok(out)
    }
}

fn or_none(s: &str) -> &str {
    if s.is_empty() { "(none)" } else { s }
}

/// `Re: subject` (once).
fn re(subject: &str) -> String {
    let s = subject.trim();
    if s.len() >= 3 && s.get(..3).is_some_and(|p| p.eq_ignore_ascii_case("re:")) { s.to_owned() } else { format!("Re: {s}") }
}

/// Thread `d` as a reply to `orig`: In-Reply-To its id, References its references and id.
fn thread(d: &mut Draft, orig: &Parsed) {
    if let Some(id) = &orig.headers.message_id {
        d.in_reply_to = Some(id.clone());
        let mut refs = orig.headers.references.clone();
        refs.push(id.clone());
        // the first (thread root) and the newest ones
        if refs.len() > 20 {
            let first = refs.first().cloned();
            let tail = refs.split_off(refs.len() - 19);
            refs = first.into_iter().chain(tail).collect();
        }
        d.references = refs;
    }
}

/// `mail_read`'s text.
fn format_read(id: &MsgId, m: &mailbox::Fetched, p: &Parsed, start: usize, max_chars: usize) -> String {
    let h = &p.headers;
    let mut out = format!("{UNTRUSTED}\nid: {id}\n");
    for (k, v) in [("From", &h.from), ("To", &h.to), ("Cc", &h.cc)] {
        if !v.is_empty() {
            let _ = writeln!(out, "{k}: {v}");
        }
    }
    if !h.reply_to.is_empty() && h.reply_to != h.from {
        let _ = writeln!(out, "Reply-To: {}", h.reply_to);
    }
    let date = if h.date.is_empty() { m.internal_date.clone().unwrap_or_default() } else { h.date.clone() };
    let _ = writeln!(out, "Date: {date}\nSubject: {}", or_none(&h.subject));
    let words = flag_words(&m.flags);
    if !words.is_empty() {
        let _ = writeln!(out, "Flags: {}", words.join(", "));
    }
    if !p.attachments.is_empty() {
        let list: Vec<String> = p.attachments.iter().map(|a| format!("{} ({}, {})", a.name, a.mime, size(a.bytes))).collect();
        let _ = write!(out, "Attachments ({}, not opened): {}", p.attachments.len() + p.more_attachments, list.join("; "));
        if p.more_attachments > 0 {
            let _ = write!(out, "; and {} more", p.more_attachments);
        }
        out.push('\n');
    }
    let cut = m.size.is_some_and(|s| s as usize > m.raw.len());
    if cut {
        let _ = writeln!(
            out,
            "[The message is {}; only its first {} were read, so attachments may be missing above.]",
            size(m.size.unwrap_or(0) as usize),
            size(m.raw.len())
        );
    }
    if p.from_html {
        out.push_str("[The body was HTML, shown as text.]\n");
    }
    out.push('\n');
    let total = p.body.chars().count();
    if total == 0 {
        out.push_str("(no text)");
        return out;
    }
    if start >= total {
        let _ = write!(out, "[Nothing at start={start}: the body has {total} characters.]");
        return out;
    }
    let chunk: String = p.body.chars().skip(start).take(max_chars).collect();
    let next = start + chunk.chars().count();
    if start > 0 || next < total {
        let _ = writeln!(out, "[Characters {start}–{next} of {total}]");
    }
    out.push_str(chunk.trim_end());
    if next < total {
        let _ = write!(out, "\n\n[Truncated: {} more characters. Call mail_read with start={next} to read on.]", total - next);
    }
    out
}

impl NativeBackend for Mail {
    fn init(&mut self, init: &Value, out: &Outbox) {
        self.configure(init, Some(out));
    }

    fn request(&mut self, method: &str, params: &Value, _out: &Outbox) -> Result<Value, String> {
        match method {
            "tool" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
                self.tool(name, params.get("args").unwrap_or(&Value::Null))
            }
            "hook" => Ok(serde_json::json!({"action": "continue"})),
            other => Err(format!("the mail backend doesn't answer {other:?} requests")),
        }
    }
}
