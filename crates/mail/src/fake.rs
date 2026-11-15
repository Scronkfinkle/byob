//! In-process fake IMAP and SMTP servers on loopback (plain TCP) for the tests: enough of
//! each protocol for what the backend sends, with the state in memory for assertions.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// One stored message.
#[derive(Debug, Clone)]
pub struct Msg {
    pub uid: u32,
    pub flags: Vec<String>,
    pub raw: Vec<u8>,
}

/// A folder: its special-use flag, messages, next UID.
#[derive(Debug, Clone, Default)]
pub struct Box_ {
    pub special: Option<&'static str>,
    pub msgs: Vec<Msg>,
    pub next: u32,
}

#[derive(Debug, Default)]
pub struct State {
    pub boxes: BTreeMap<String, Box_>,
    /// Every command line the IMAP server got (tags stripped).
    pub commands: Vec<String>,
    /// Mail the SMTP server took: (MAIL FROM, RCPT TOs, data).
    pub sent: Vec<(String, Vec<String>, String)>,
    /// AUTH lines the SMTP server got.
    pub smtp_auth: Vec<String>,
    /// Capabilities offered (MOVE, UIDPLUS).
    pub caps: Vec<&'static str>,
}

impl State {
    pub fn add(&mut self, folder: &str, flags: &[&str], raw: &str) -> u32 {
        let b = self.boxes.entry(folder.to_owned()).or_default();
        b.next += 1;
        let uid = b.next;
        b.msgs.push(Msg { uid, flags: flags.iter().map(|f| (*f).to_owned()).collect(), raw: raw.replace("\r\n", "\n").replace('\n', "\r\n").into_bytes() });
        uid
    }
}

pub struct Servers {
    pub state: Arc<Mutex<State>>,
    pub imap_port: u16,
    pub smtp_port: u16,
}

pub const USER: &str = "me@example.com";
pub const PASS: &str = "secret pass";

impl Servers {
    pub fn start() -> Self {
        let mut st = State { caps: vec!["MOVE", "UIDPLUS"], ..Default::default() };
        for (name, special) in [("INBOX", None), ("Drafts", Some("\\Drafts")), ("Sent", Some("\\Sent")), ("Archive", Some("\\Archive")), ("Entw&APw-rfe", None)]
        {
            st.boxes.insert(name.into(), Box_ { special, ..Default::default() });
        }
        let state = Arc::new(Mutex::new(st));
        let imap = TcpListener::bind("127.0.0.1:0").unwrap();
        let smtp = TcpListener::bind("127.0.0.1:0").unwrap();
        let (imap_port, smtp_port) = (imap.local_addr().unwrap().port(), smtp.local_addr().unwrap().port());
        let s = Arc::clone(&state);
        std::thread::spawn(move || {
            for c in imap.incoming().flatten() {
                let s = Arc::clone(&s);
                std::thread::spawn(move || imap_conn(c, &s));
            }
        });
        let s = Arc::clone(&state);
        std::thread::spawn(move || {
            for c in smtp.incoming().flatten() {
                let s = Arc::clone(&s);
                std::thread::spawn(move || smtp_conn(c, &s));
            }
        });
        Self { state, imap_port, smtp_port }
    }

    /// `init` for the backend pointing at these servers.
    pub fn init(&self) -> serde_json::Value {
        serde_json::json!({
            "settings": {
                "IMAP_HOST": "127.0.0.1", "IMAP_PORT": self.imap_port.to_string(), "IMAP_SECURITY": "none",
                "SMTP_HOST": "127.0.0.1", "SMTP_PORT": self.smtp_port.to_string(), "SMTP_SECURITY": "none",
                "USERNAME": USER, "FROM_NAME": "Me Myself",
            },
            "secrets": {"PASSWORD": PASS},
        })
    }
}

/// Tokens of a command: quoted strings (unescaped), parenthesised groups (as written), atoms.
fn tokens(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = line.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c == ' ' {
            chars.next();
            continue;
        }
        let mut t = String::new();
        if c == '"' {
            chars.next();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => t.extend(chars.next()),
                    '"' => break,
                    c => t.push(c),
                }
            }
        } else if c == '(' {
            let mut depth = 0;
            for c in chars.by_ref() {
                t.push(c);
                match c {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == ' ' {
                    break;
                }
                t.push(c);
                chars.next();
            }
        }
        out.push(t);
    }
    out
}

fn uid_set(s: &str) -> Vec<u32> {
    s.split(',').filter_map(|p| p.parse().ok()).collect()
}

fn matches(m: &Msg, crit: &[String]) -> bool {
    let raw = String::from_utf8_lossy(&m.raw).to_lowercase();
    let mut i = 0;
    while i < crit.len() {
        let key = crit[i].to_ascii_uppercase();
        let arg = crit.get(i + 1).map(|a| a.to_lowercase()).unwrap_or_default();
        let ok = match key.as_str() {
            "ALL" => true,
            "UNSEEN" => !m.flags.iter().any(|f| f == "\\Seen"),
            "SEEN" => m.flags.iter().any(|f| f == "\\Seen"),
            "FLAGGED" => m.flags.iter().any(|f| f == "\\Flagged"),
            "CHARSET" => {
                i += 2;
                continue;
            }
            "FROM" | "TO" | "SUBJECT" => {
                i += 1;
                let h = format!("{}:", key.to_lowercase());
                raw.lines().any(|l| l.starts_with(&h) && l.contains(&arg))
            }
            "TEXT" => {
                i += 1;
                raw.contains(&arg)
            }
            "SINCE" | "BEFORE" => {
                i += 1;
                true
            }
            _ => true,
        };
        if !ok {
            return false;
        }
        i += 1;
    }
    true
}

fn imap_conn(c: TcpStream, state: &Mutex<State>) {
    let mut w = c.try_clone().unwrap();
    let mut r = BufReader::new(c);
    let _ = w.write_all(b"* OK fake IMAP ready\r\n");
    let mut selected: Option<String> = None;
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let l = line.trim_end().to_owned();
        let Some((tag, rest)) = l.split_once(' ') else { continue };
        let t = tokens(rest);
        let mut st = state.lock().unwrap();
        st.commands.push(if rest.to_ascii_uppercase().starts_with("LOGIN") { "LOGIN ***".into() } else { rest.to_owned() });
        let cmd = t.first().map(|s| s.to_ascii_uppercase()).unwrap_or_default();
        let uid_cmd = t.get(1).map(|s| s.to_ascii_uppercase()).unwrap_or_default();
        let mut out = String::new();
        let mut ok = format!("{tag} OK done\r\n");
        match (cmd.as_str(), uid_cmd.as_str()) {
            ("CAPABILITY", _) => out.push_str(&format!("* CAPABILITY IMAP4rev1 {}\r\n", st.caps.join(" "))),
            ("LOGIN", _) => {
                if t.get(1).map(String::as_str) != Some(USER) || t.get(2).map(String::as_str) != Some(PASS) {
                    ok = format!("{tag} NO [AUTHENTICATIONFAILED] bad login\r\n");
                }
            }
            ("LIST", _) => {
                for (name, b) in &st.boxes {
                    let attrs = b.special.map_or_else(|| "\\HasNoChildren".to_owned(), |s| format!("\\HasNoChildren {s}"));
                    out.push_str(&format!("* LIST ({attrs}) \"/\" \"{name}\"\r\n"));
                }
            }
            ("STATUS", _) => match st.boxes.get(t.get(1).map(String::as_str).unwrap_or_default()) {
                Some(b) => {
                    let unseen = b.msgs.iter().filter(|m| !m.flags.iter().any(|f| f == "\\Seen")).count();
                    out.push_str(&format!("* STATUS \"{}\" (MESSAGES {} UNSEEN {unseen})\r\n", t[1], b.msgs.len()));
                }
                None => ok = format!("{tag} NO no such mailbox\r\n"),
            },
            ("SELECT" | "EXAMINE", _) => {
                let name = t.get(1).cloned().unwrap_or_default();
                match st.boxes.get(&name) {
                    Some(b) => {
                        out.push_str(&format!(
                            "* {} EXISTS\r\n* 0 RECENT\r\n* OK [UIDVALIDITY 7] ok\r\n* FLAGS (\\Seen \\Flagged \\Draft \\Answered \\Deleted)\r\n",
                            b.msgs.len()
                        ));
                        selected = Some(name);
                    }
                    None => ok = format!("{tag} NO [NONEXISTENT] no such mailbox\r\n"),
                }
            }
            ("UID", "SEARCH") => {
                let b = selected.as_ref().and_then(|s| st.boxes.get(s));
                let hits: Vec<String> = b.map(|b| b.msgs.iter().filter(|m| matches(m, &t[2..])).map(|m| m.uid.to_string()).collect()).unwrap_or_default();
                out.push_str(&format!("* SEARCH {}\r\n", hits.join(" ")).replace("SEARCH \r\n", "SEARCH\r\n"));
            }
            ("UID", "FETCH") => {
                let want = uid_set(&t[2]);
                let max: usize = t[3].split("<0.").nth(1).and_then(|s| s.split('>').next()).and_then(|n| n.parse().ok()).unwrap_or(usize::MAX);
                let mut bytes: Vec<u8> = Vec::new();
                if let Some(b) = selected.as_ref().and_then(|s| st.boxes.get(s)) {
                    for (seq, m) in b.msgs.iter().enumerate() {
                        if !want.contains(&m.uid) {
                            continue;
                        }
                        let body = &m.raw[..m.raw.len().min(max)];
                        bytes.extend(
                            format!(
                                "* {} FETCH (UID {} FLAGS ({}) INTERNALDATE \"03-Sep-2024 10:00:00 +0000\" RFC822.SIZE {} BODY[]<0> {{{}}}\r\n",
                                seq + 1,
                                m.uid,
                                m.flags.join(" "),
                                m.raw.len(),
                                body.len()
                            )
                            .as_bytes(),
                        );
                        bytes.extend_from_slice(body);
                        bytes.extend_from_slice(b")\r\n");
                    }
                }
                drop(st);
                let _ = w.write_all(&bytes).and_then(|()| w.write_all(ok.as_bytes()));
                continue;
            }
            ("APPEND", _) => {
                let name = t.get(1).cloned().unwrap_or_default();
                let flags: Vec<String> = t
                    .iter()
                    .find(|x| x.starts_with('('))
                    .map(|f| f.trim_matches(['(', ')']).split(' ').filter(|x| !x.is_empty()).map(str::to_owned).collect())
                    .unwrap_or_default();
                let n: usize = t.last().and_then(|x| x.trim_matches(['{', '}']).parse().ok()).unwrap_or(0);
                drop(st);
                let _ = w.write_all(b"+ go ahead\r\n");
                let mut raw = vec![0u8; n];
                if r.read_exact(&mut raw).is_err() {
                    return;
                }
                let mut crlf = String::new();
                let _ = r.read_line(&mut crlf);
                let mut st = state.lock().unwrap();
                let Some(b) = st.boxes.get_mut(&name) else {
                    let _ = w.write_all(format!("{tag} NO [TRYCREATE] no such mailbox\r\n").as_bytes());
                    continue;
                };
                b.next += 1;
                let uid = b.next;
                b.msgs.push(Msg { uid, flags, raw });
                let _ = w.write_all(format!("{tag} OK [APPENDUID 7 {uid}] appended\r\n").as_bytes());
                continue;
            }
            ("UID", "STORE") => {
                let uids = uid_set(&t[2]);
                let op = t[3].to_ascii_uppercase();
                let flags: Vec<String> = t[4].trim_matches(['(', ')']).split(' ').map(str::to_owned).collect();
                if let Some(b) = selected.clone().and_then(|s| st.boxes.get_mut(&s)) {
                    for m in b.msgs.iter_mut().filter(|m| uids.contains(&m.uid)) {
                        for f in &flags {
                            m.flags.retain(|x| x != f);
                            if op.starts_with('+') {
                                m.flags.push(f.clone());
                            }
                        }
                    }
                }
            }
            ("UID", "MOVE" | "COPY") => {
                let uids = uid_set(&t[2]);
                let dest = t[3].clone();
                let src = selected.clone().unwrap_or_default();
                if !st.boxes.contains_key(&dest) {
                    ok = format!("{tag} NO [TRYCREATE] no such mailbox\r\n");
                } else {
                    let moving: Vec<Msg> = st.boxes.get(&src).map(|b| b.msgs.iter().filter(|m| uids.contains(&m.uid)).cloned().collect()).unwrap_or_default();
                    if uid_cmd == "MOVE"
                        && let Some(b) = st.boxes.get_mut(&src)
                    {
                        b.msgs.retain(|m| !uids.contains(&m.uid));
                    }
                    let d = st.boxes.get_mut(&dest).unwrap();
                    for mut m in moving {
                        d.next += 1;
                        m.uid = d.next;
                        d.msgs.push(m);
                    }
                }
            }
            ("UID", "EXPUNGE") => {
                let uids = uid_set(&t[2]);
                if let Some(b) = selected.clone().and_then(|s| st.boxes.get_mut(&s)) {
                    b.msgs.retain(|m| !(uids.contains(&m.uid) && m.flags.iter().any(|f| f == "\\Deleted")));
                }
            }
            ("LOGOUT", _) => {
                let _ = w.write_all(format!("* BYE bye\r\n{tag} OK logged out\r\n").as_bytes());
                return;
            }
            ("NOOP", _) => {}
            _ => ok = format!("{tag} BAD unknown command\r\n"),
        }
        drop(st);
        let _ = w.write_all(out.as_bytes()).and_then(|()| w.write_all(ok.as_bytes()));
    }
}

fn smtp_conn(c: TcpStream, state: &Mutex<State>) {
    let mut w = c.try_clone().unwrap();
    let mut r = BufReader::new(c);
    let _ = w.write_all(b"220 fake ESMTP\r\n");
    let (mut from, mut to) = (String::new(), Vec::new());
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let l = line.trim_end().to_owned();
        let upper = l.to_ascii_uppercase();
        let reply: &[u8] = if upper.starts_with("EHLO") {
            b"250-fake\r\n250-AUTH PLAIN LOGIN\r\n250 8BITMIME\r\n"
        } else if upper.starts_with("AUTH") {
            state.lock().unwrap().smtp_auth.push(l.clone());
            b"235 ok\r\n"
        } else if let Some(a) = upper.strip_prefix("MAIL FROM:") {
            from = l[10..10 + a.len()].trim().to_owned();
            b"250 ok\r\n"
        } else if upper.starts_with("RCPT TO:") {
            to.push(l[8..].trim().to_owned());
            b"250 ok\r\n"
        } else if upper == "DATA" {
            let _ = w.write_all(b"354 go\r\n");
            let mut data = String::new();
            loop {
                let mut d = String::new();
                if r.read_line(&mut d).unwrap_or(0) == 0 {
                    return;
                }
                if d == ".\r\n" {
                    break;
                }
                data.push_str(&d);
            }
            state.lock().unwrap().sent.push((std::mem::take(&mut from), std::mem::take(&mut to), data));
            b"250 queued\r\n"
        } else if upper == "QUIT" {
            let _ = w.write_all(b"221 bye\r\n");
            return;
        } else {
            b"250 ok\r\n"
        };
        let _ = w.write_all(reply);
    }
}
