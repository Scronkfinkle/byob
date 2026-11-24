//! Channels: ways byob reaches the user outside the app ("shoot me a text") and the user
//! answers. A channel is provided by a plugin (manifest `channels`, see
//! `docs/plugins-backend.md`, Channels); byob's engine owns everything that decides who may
//! talk to it: pairing codes, the paired peers (by their immutable platform id, never a
//! display name), "My phone" (the one channel and peer "text me" means), and the daily caps.
//!
//! The engine keeps them in `<config>/channels.json` ([`ChannelsFile`]), global (not per
//! project). This module is the pure part: keys, codes, limits, the file's shape.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The routine channel that means "My phone" (in `Routine::channels`).
pub const CHANNEL_PHONE: &str = "phone";

/// Pairing codes: 8 characters from an alphabet without look-alikes (no 0/O, 1/I).
pub const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
pub const CODE_LEN: usize = 8;
/// How long a pairing code works (ms): an hour.
pub const CODE_TTL_MS: i64 = 60 * 60 * 1000;
/// Most pairing codes waiting at once, per channel.
pub const MAX_PENDING_CODES: usize = 3;

/// Most texts one routine sends My phone in any 24 hours; beyond that they're held and go
/// out together, as one message, when the window allows.
pub const DAILY_CAP: usize = 10;
/// The window of [`DAILY_CAP`] (ms).
pub const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// Most held lines kept per routine (older ones are dropped, and the batch says so).
pub const MAX_HELD: usize = 50;
/// Most messages to My phone in any hour, from everything together (a safety net against a
/// loop; replies to the user's own messages count too).
pub const HOURLY_CAP: usize = 30;

/// Longest inbound text the engine keeps (characters); the rest is cut.
pub const MAX_INBOUND_CHARS: usize = 4000;
/// Most inbound messages a paired peer gets through per minute; the rest are dropped.
pub const MAX_INBOUND_PER_MINUTE: usize = 10;
/// Longest peer id or display name kept (characters).
pub const MAX_PEER_CHARS: usize = 128;
/// Most delivered messages remembered (to map a reply back to its chat).
pub const MAX_RECENT: usize = 200;
/// Most paired peers kept (one per channel in v1; room for later).
pub const MAX_PEERS: usize = 16;

/// A channel's key: `<plugin>:<channel>` (`telegram:telegram`).
pub fn key(plugin: &str, channel: &str) -> String {
    format!("{plugin}:{channel}")
}

/// A key's plugin and channel.
pub fn split_key(key: &str) -> Option<(&str, &str)> {
    let (p, c) = key.split_once(':')?;
    (!p.is_empty() && !c.is_empty()).then_some((p, c))
}

/// A pairing code as typed or sent (`abcd-efgh`, ` ABCD EFGH `) in its canonical form, if
/// it is one.
pub fn normalize_code(text: &str) -> Option<String> {
    let code: String = text.chars().filter(|c| !matches!(c, ' ' | '-' | '\t')).flat_map(char::to_uppercase).collect();
    (code.len() == CODE_LEN && code.bytes().all(|b| CODE_ALPHABET.contains(&b))).then_some(code)
}

/// A code made from `random` (64 random bits per call).
pub fn make_code(mut random: impl FnMut() -> u64) -> String {
    let n = u64::try_from(CODE_ALPHABET.len()).unwrap_or(32);
    let mut out = String::with_capacity(CODE_LEN);
    let mut bits = random();
    for i in 0..CODE_LEN {
        if i == 6 {
            bits = random();
        }
        let at = usize::try_from(bits % n).unwrap_or(0);
        out.push(char::from(CODE_ALPHABET.get(at).copied().unwrap_or(b'A')));
        bits /= n;
    }
    out
}

/// `ABCDEFGH` as people read it: `ABCD-EFGH`.
pub fn show_code(code: &str) -> String {
    match (code.get(..4), code.get(4..)) {
        (Some(a), Some(b)) if !b.is_empty() => format!("{a}-{b}"),
        _ => code.to_owned(),
    }
}

/// Text cut to `max` characters (with "…" when cut).
pub fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// A person paired on a channel: their immutable platform id (Telegram's numeric user id,
/// not the @username, which they can change and someone else can take).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peer {
    /// The channel's key.
    pub channel: String,
    pub peer: String,
    /// What the channel called them when they paired (for display only).
    #[serde(default)]
    pub name: String,
    /// ms since the epoch.
    #[serde(default)]
    pub paired_at: i64,
}

/// "My phone": the channel and peer that "text me" means.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhoneTarget {
    pub channel: String,
    pub peer: String,
}

/// What a delivered message was.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sent {
    /// A run's report (or a `text_me`): a reply continues its chat.
    #[default]
    Report,
    /// An approval question: answered with its buttons.
    Approval,
    /// byob's answer to something the user sent from the phone: a reply continues the chat.
    Answer,
    /// Pairing, help and test messages: nothing to continue.
    Note,
    #[serde(other)]
    Other,
}

/// A message byob delivered, remembered so a reply to it finds its chat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivered {
    pub channel: String,
    pub peer: String,
    /// The platform's id of the message (`deliver`'s `ref`).
    #[serde(rename = "ref")]
    pub reference: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routine: Option<String>,
    #[serde(default)]
    pub kind: Sent,
    #[serde(default)]
    pub at: i64,
}

/// `<config>/channels.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChannelsFile {
    /// Who is paired (the allowlist).
    #[serde(default)]
    pub peers: Vec<Peer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone: Option<PhoneTarget>,
    /// The latest messages delivered (newest last, at most [`MAX_RECENT`]).
    #[serde(default)]
    pub recent: Vec<Delivered>,
    /// When each routine texted My phone (ms), within the last day: the daily cap.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sent: BTreeMap<String, Vec<i64>>,
    /// Lines held back by the daily cap, by routine (they go out as one message).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub held: BTreeMap<String, Vec<String>>,
}

impl ChannelsFile {
    /// Is `peer` paired on `channel`?
    pub fn paired(&self, channel: &str, peer: &str) -> bool {
        self.peers.iter().any(|p| p.channel == channel && p.peer == peer)
    }

    /// The peer paired on `channel` (one per channel in v1).
    pub fn peer_of(&self, channel: &str) -> Option<&Peer> {
        self.peers.iter().find(|p| p.channel == channel)
    }

    /// Keep it to its limits after a hand edit or a newer build: valid keys, short ids,
    /// one peer per channel, the phone pointing at a paired peer.
    pub fn tidy(&mut self) {
        self.peers.retain(|p| split_key(&p.channel).is_some() && !p.peer.is_empty() && p.peer.chars().count() <= MAX_PEER_CHARS);
        let mut seen = Vec::new();
        self.peers.retain(|p| {
            let fresh = !seen.contains(&p.channel);
            seen.push(p.channel.clone());
            fresh
        });
        self.peers.truncate(MAX_PEERS);
        if self.phone.as_ref().is_some_and(|t| !self.paired(&t.channel, &t.peer)) {
            self.phone = None;
        }
        let extra = self.recent.len().saturating_sub(MAX_RECENT);
        self.recent.drain(..extra);
        for held in self.held.values_mut() {
            let extra = held.len().saturating_sub(MAX_HELD);
            held.drain(..extra);
        }
        self.held.retain(|_, v| !v.is_empty());
        self.sent.retain(|_, v| !v.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_have_no_look_alikes_and_read_back() {
        let mut n = 0u64;
        let code = make_code(|| {
            n = n.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            n
        });
        assert_eq!(code.len(), CODE_LEN);
        assert!(!code.contains(['0', 'O', '1', 'I']), "{code}");
        assert_eq!(normalize_code(&show_code(&code).to_lowercase()), Some(code.clone()));
        assert_eq!(normalize_code(" abcd efgh "), Some("ABCDEFGH".into()));
        assert_eq!(normalize_code("ABCD0FGH"), None, "0 isn't in the alphabet");
        assert_eq!(normalize_code("ABCDEFG"), None);
        assert_eq!(normalize_code("hello there"), None);
        assert_eq!(split_key("telegram:telegram"), Some(("telegram", "telegram")));
        assert_eq!(split_key("telegram"), None);
        assert_eq!(cut("abcdef", 4), "abc…");
    }

    #[test]
    fn tidy_keeps_one_peer_per_channel_and_a_paired_phone() {
        let peer = |c: &str, p: &str| Peer { channel: c.into(), peer: p.into(), name: String::new(), paired_at: 0 };
        let mut f = ChannelsFile {
            peers: vec![peer("tg:tg", "1"), peer("tg:tg", "2"), peer("bad", "3"), peer("x:y", "")],
            phone: Some(PhoneTarget { channel: "tg:tg".into(), peer: "2".into() }),
            ..ChannelsFile::default()
        };
        f.tidy();
        assert_eq!(f.peers, vec![peer("tg:tg", "1")]);
        assert_eq!(f.phone, None, "the phone named a peer that isn't paired");
        let back: ChannelsFile =
            serde_json::from_str(r#"{"peers":[{"channel":"a:b","peer":"9"}],"recent":[{"channel":"a:b","peer":"9","ref":"5","kind":"teleport"}]}"#).unwrap();
        assert_eq!(back.recent[0].kind, Sent::Other);
        assert!(back.paired("a:b", "9") && !back.paired("a:b", "8"));
    }
}
