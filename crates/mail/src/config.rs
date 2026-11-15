//! The account, from the plugin's settings (see the manifest in `crates/bundled/plugins/mail`).

use serde_json::Value;

use crate::net::Security;

/// One server (IMAP or SMTP).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    pub host: String,
    pub port: u16,
    pub security: Security,
}

/// When to put a copy of sent mail in the Sent folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveSent {
    /// Unless the provider does it itself (Gmail, Outlook).
    Auto,
    Always,
    Never,
}

/// The account byob talks to.
#[derive(Clone, Default)]
pub struct Account {
    pub imap: Option<Server>,
    pub smtp: Option<Server>,
    pub username: String,
    pub password: String,
    /// `From:` address (the username when it looks like one).
    pub from_address: String,
    pub from_name: Option<String>,
    /// Overrides for the special folders (else found by their `\Drafts` / `\Sent` flags).
    pub drafts: Option<String>,
    pub sent: Option<String>,
    pub save_sent: Option<SaveSent>,
    /// What's wrong with the settings, for the model (and the plugin's faults).
    pub problems: Vec<String>,
}

impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account")
            .field("imap", &self.imap)
            .field("smtp", &self.smtp)
            .field("username", &self.username)
            .field("password", &if self.password.is_empty() { "" } else { "<set>" })
            .field("from_address", &self.from_address)
            .finish_non_exhaustive()
    }
}

/// Known providers: (name, IMAP host, SMTP host, SMTP security). IMAP is TLS on 993 for all.
const PRESETS: &[(&str, &str, &str, Security)] = &[
    ("fastmail", "imap.fastmail.com", "smtp.fastmail.com", Security::Tls),
    ("icloud", "imap.mail.me.com", "smtp.mail.me.com", Security::StartTls),
    ("gmail", "imap.gmail.com", "smtp.gmail.com", Security::Tls),
    ("outlook", "outlook.office365.com", "smtp.office365.com", Security::StartTls),
];

/// Hosts whose SMTP servers file sent mail in Sent themselves.
const SAVES_SENT: &[&str] = &["gmail.com", "googlemail.com", "office365.com", "outlook.com"];

/// A setting's value from `init` (`settings`, or `secrets` for secret ones), blank as unset.
fn setting(init: &Value, key: &str) -> Option<String> {
    ["settings", "secrets"].iter().find_map(|m| init.get(m)?.get(key)?.as_str().map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned))
}

fn default_port(security: Security, imap: bool) -> u16 {
    match (imap, security) {
        (true, Security::Tls) => 993,
        (true, _) => 143,
        (false, Security::Tls) => 465,
        (false, _) => 587,
    }
}

impl Account {
    pub fn from_init(init: &Value) -> Self {
        let mut a = Account::default();
        let preset = setting(init, "PROVIDER").map(|p| p.to_ascii_lowercase());
        let preset = match preset.as_deref() {
            None | Some("custom" | "other" | "") => None,
            Some(p) => match PRESETS.iter().find(|(n, ..)| *n == p) {
                Some(found) => Some(*found),
                None => {
                    a.problems.push(format!("PROVIDER {p:?} isn't one of fastmail, icloud, gmail, outlook, custom; using the hosts below"));
                    None
                }
            },
        };
        a.imap = a.server(init, "IMAP", true, preset.map(|(_, h, _, _)| (h, Security::Tls)));
        a.smtp = a.server(init, "SMTP", false, preset.map(|(_, _, h, s)| (h, s)));
        a.username = setting(init, "USERNAME").unwrap_or_default();
        a.password = setting(init, "PASSWORD").unwrap_or_default();
        a.from_address = setting(init, "FROM_ADDRESS").unwrap_or_else(|| if a.username.contains('@') { a.username.clone() } else { String::new() });
        a.from_name = setting(init, "FROM_NAME");
        a.drafts = setting(init, "DRAFTS_FOLDER");
        a.sent = setting(init, "SENT_FOLDER");
        a.save_sent = match setting(init, "SAVE_SENT").map(|s| s.to_ascii_lowercase()).as_deref() {
            None | Some("auto") => None,
            Some("true" | "yes" | "always" | "on" | "1") => Some(SaveSent::Always),
            Some("false" | "no" | "never" | "off" | "0") => Some(SaveSent::Never),
            Some(other) => {
                a.problems.push(format!("SAVE_SENT {other:?} isn't one of auto, always, never; using auto"));
                None
            }
        };
        a
    }

    fn server(&mut self, init: &Value, which: &str, imap: bool, preset: Option<(&str, Security)>) -> Option<Server> {
        let security = match setting(init, &format!("{which}_SECURITY")) {
            None => preset.map_or(Security::Tls, |(_, s)| s),
            Some(s) => Security::parse(&s).unwrap_or_else(|| {
                self.problems.push(format!("{which}_SECURITY {s:?} isn't one of tls, starttls, none; using tls"));
                Security::Tls
            }),
        };
        let host = setting(init, &format!("{which}_HOST")).or_else(|| preset.map(|(h, _)| h.to_owned()))?;
        let port = match setting(init, &format!("{which}_PORT")) {
            None => default_port(security, imap),
            // numbers come as "993" or "993.0"
            Some(p) => match p.parse::<f64>().ok().filter(|n| n.fract() == 0.0 && *n >= 1.0 && *n <= 65535.0) {
                Some(n) => n as u16,
                None => {
                    self.problems.push(format!("{which}_PORT {p:?} isn't a port number"));
                    default_port(security, imap)
                }
            },
        };
        Some(Server { host, port, security })
    }

    /// The settings IMAP needs, or what to fill in.
    pub fn imap(&self) -> Result<&Server, String> {
        let s = self.imap.as_ref().ok_or("Mail isn't set up: fill in the IMAP server (or a provider) under Settings → Plugins → Mail.")?;
        self.login_ready()?;
        Ok(s)
    }

    pub fn smtp(&self) -> Result<&Server, String> {
        let s = self.smtp.as_ref().ok_or("Sending isn't set up: fill in the SMTP server (or a provider) under Settings → Plugins → Mail.")?;
        self.login_ready()?;
        if self.from_address.is_empty() {
            return Err("Sending isn't set up: fill in the From address under Settings → Plugins → Mail.".into());
        }
        Ok(s)
    }

    fn login_ready(&self) -> Result<(), String> {
        if self.username.is_empty() || self.password.is_empty() {
            return Err("Mail isn't set up: fill in the username and password under Settings → Plugins → Mail.".into());
        }
        Ok(())
    }

    /// Whether to append a sent message to Sent ourselves.
    pub fn should_save_sent(&self) -> bool {
        match self.save_sent.unwrap_or(SaveSent::Auto) {
            SaveSent::Always => true,
            SaveSent::Never => false,
            SaveSent::Auto => {
                let host = self.smtp.as_ref().map(|s| s.host.to_ascii_lowercase()).unwrap_or_default();
                !SAVES_SENT.iter().any(|d| host == *d || host.ends_with(&format!(".{d}")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_preset_fills_the_servers_and_settings_override_it() {
        let a = Account::from_init(&json!({"settings": {"PROVIDER": "Fastmail", "USERNAME": "me@fastmail.com"}, "secrets": {"PASSWORD": "pw"}}));
        assert_eq!(a.imap, Some(Server { host: "imap.fastmail.com".into(), port: 993, security: Security::Tls }));
        assert_eq!(a.smtp, Some(Server { host: "smtp.fastmail.com".into(), port: 465, security: Security::Tls }));
        assert_eq!(a.from_address, "me@fastmail.com");
        assert!(a.problems.is_empty() && a.should_save_sent());
        assert!(!format!("{a:?}").contains("pw"), "the password never shows in logs");

        let a = Account::from_init(&json!({"settings": {"PROVIDER": "icloud", "SMTP_PORT": "2525.0", "IMAP_SECURITY": "none", "IMAP_PORT": "3143"}}));
        assert_eq!(a.imap, Some(Server { host: "imap.mail.me.com".into(), port: 3143, security: Security::Plain }));
        assert_eq!(a.smtp, Some(Server { host: "smtp.mail.me.com".into(), port: 2525, security: Security::StartTls }));
        assert!(a.imap().unwrap_err().contains("username and password"));

        let g = Account::from_init(&json!({"settings": {"PROVIDER": "gmail"}}));
        assert!(!g.should_save_sent(), "Gmail files sent mail itself");
    }

    #[test]
    fn bad_values_are_problems_not_crashes() {
        let a = Account::from_init(&json!({"settings": {"PROVIDER": "aol", "IMAP_HOST": "h", "IMAP_PORT": "99999", "SMTP_SECURITY": "?", "SAVE_SENT": "x"}}));
        assert_eq!(a.problems.len(), 4, "{:?}", a.problems);
        assert_eq!(a.imap.as_ref().map(|s| s.port), Some(993));
        assert!(a.smtp.is_none());
        assert!(Account::from_init(&json!({})).imap().unwrap_err().contains("IMAP server"));
    }
}
