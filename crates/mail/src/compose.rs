//! New messages (drafts and mail to send) with lettre's builder, and sending over SMTP with
//! its blocking transport.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lettre::message::header::ContentType;
use lettre::message::{Mailbox, Message};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{SmtpTransport, Transport};

use crate::config::{Account, Server};
use crate::net::Security;
use crate::parse::one_line;

/// Recipients on one message.
pub const MAX_RECIPIENTS: usize = 50;
/// Characters of a body we write.
pub const MAX_BODY: usize = 100_000;
/// One SMTP command's deadline.
const SMTP_TIMEOUT: Duration = Duration::from_secs(30);

/// What to write: addresses as typed, the reply's threading headers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Draft {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
    /// `<id>` of the message replied to.
    pub in_reply_to: Option<String>,
    /// Its `References:` plus its id.
    pub references: Vec<String>,
}

/// Split "a@x.com, Name <b@y.com>; c@z" into addresses (commas inside quotes or angle
/// brackets don't split).
pub fn split_addresses(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let (mut quoted, mut angle) = (false, false);
    for c in s.chars() {
        match c {
            '"' => quoted = !quoted,
            '<' if !quoted => angle = true,
            '>' if !quoted => angle = false,
            ',' | ';' | '\n' if !quoted && !angle => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    out.push(cur);
    out.into_iter().map(|a| a.trim().to_owned()).filter(|a| !a.is_empty()).collect()
}

fn mailbox(a: &str) -> Result<Mailbox, String> {
    a.parse::<Mailbox>().map_err(|_| format!("{:?} isn't an email address", one_line(a, 120)))
}

/// A Message-ID on the sender's domain.
fn new_message_id(from: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let domain = from.rsplit_once('@').map(|(_, d)| d.trim_end_matches('>')).filter(|d| !d.is_empty()).unwrap_or("byob.invalid");
    format!("<byob.{nanos:x}.{}.{n}@{domain}>", std::process::id())
}

/// The message as RFC 5322 bytes, and its Message-ID. `keep_bcc`: a draft keeps its Bcc
/// header (a sent copy doesn't).
pub fn build(account: &Account, d: &Draft, keep_bcc: bool) -> Result<(Vec<u8>, String), String> {
    let (msg, id) = message(account, d, keep_bcc)?;
    Ok((msg.formatted(), id))
}

fn message(account: &Account, d: &Draft, keep_bcc: bool) -> Result<(Message, String), String> {
    if account.from_address.is_empty() {
        return Err("Fill in the From address under Settings → Plugins → Mail first.".into());
    }
    let from = match &account.from_name {
        Some(name) => {
            Mailbox::new(Some(name.clone()), account.from_address.parse().map_err(|_| format!("the From address {:?} isn't valid", account.from_address))?)
        }
        None => mailbox(&account.from_address)?,
    };
    if d.to.len() + d.cc.len() + d.bcc.len() > MAX_RECIPIENTS {
        return Err(format!("at most {MAX_RECIPIENTS} recipients"));
    }
    if d.body.chars().count() > MAX_BODY {
        return Err(format!("the body is over {MAX_BODY} characters"));
    }
    let id = new_message_id(&account.from_address);
    let mut b = Message::builder().from(from).subject(one_line(&d.subject, 400)).message_id(Some(id.clone()));
    for a in &d.to {
        b = b.to(mailbox(a)?);
    }
    for a in &d.cc {
        b = b.cc(mailbox(a)?);
    }
    for a in &d.bcc {
        b = b.bcc(mailbox(a)?);
    }
    if let Some(irt) = &d.in_reply_to {
        b = b.in_reply_to(irt.clone());
    }
    if !d.references.is_empty() {
        b = b.references(d.references.join(" "));
    }
    if keep_bcc {
        b = b.keep_bcc();
    }
    b = b.user_agent("byob".into());
    let body = d.body.replace("\r\n", "\n");
    let msg = b.header(ContentType::TEXT_PLAIN).body(body).map_err(|e| one_line(&format!("couldn't write the message: {e}"), 300))?;
    Ok((msg, id))
}

/// Send `d` over SMTP: the message as sent (for the Sent folder) and its Message-ID.
pub fn send(account: &Account, d: &Draft) -> Result<(Vec<u8>, String), String> {
    let server = account.smtp()?;
    if d.to.is_empty() && d.cc.is_empty() && d.bcc.is_empty() {
        return Err("Give at least one recipient (to).".into());
    }
    // The envelope comes from all recipient headers (Bcc too); the Bcc header itself is
    // dropped from the bytes sent.
    let (msg, id) = message(account, d, false)?;
    transport(server, account)?.send(&msg).map_err(|e| one_line(&format!("the SMTP server didn't take the message: {e}"), 400))?;
    Ok((msg.formatted(), id))
}

fn transport(server: &Server, account: &Account) -> Result<SmtpTransport, String> {
    let params = || TlsParameters::new_rustls(server.host.clone()).map_err(|e| one_line(&format!("TLS setup for {}: {e}", server.host), 300));
    let tls = match server.security {
        Security::Tls => Tls::Wrapper(params()?),
        Security::StartTls => Tls::Required(params()?),
        Security::Plain => Tls::None,
    };
    Ok(SmtpTransport::builder_dangerous(server.host.clone())
        .port(server.port)
        .tls(tls)
        .credentials(Credentials::new(account.username.clone(), account.password.clone()))
        .timeout(Some(SMTP_TIMEOUT))
        .build())
}
