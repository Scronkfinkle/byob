//! "My phone" on the wire (13.23+, cap [`CAP_PHONE`]): the channel byob texts you on and the
//! person paired on it (see [`byob_types::channel`]).
//!
//! The engine sends [`crate::ServerMsg::Phone`] after the Welcome and whenever it changes:
//! the channels plugins offer, whether My phone is set up, a pairing in progress. Setting a
//! channel's own settings (a bot token) and switching its plugin on are the ordinary plugin
//! messages (`SetPluginSetting`, `SetPluginEnabled`). Peer ids never reach clients: only the
//! name the channel gave the person.

use serde::{Deserialize, Serialize};

pub use byob_types::channel::{CHANNEL_PHONE, DAILY_CAP};

use crate::ClientMsg;

/// Cap: the peer handles the phone messages.
pub const CAP_PHONE: &str = "phone";

/// A channel a plugin offers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelOption {
    /// `<plugin>:<channel>`.
    pub key: String,
    pub plugin: String,
    /// "Telegram".
    pub title: String,
    /// The user can answer (replies, buttons).
    #[serde(default)]
    pub two_way: bool,
    /// Approvals can be answered with a tap.
    #[serde(default)]
    pub buttons: bool,
    /// Its plugin is switched on.
    #[serde(default)]
    pub enabled: bool,
    /// Its plugin's required settings are filled in.
    #[serde(default)]
    pub configured: bool,
    /// Its backend says it works (`channel_status` ok), as of `status`.
    #[serde(default)]
    pub ok: bool,
    /// What its backend last said about itself ("Connected as @byob_jesse_bot", "Telegram
    /// refused the bot token"), or why it isn't running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// The setting the user fills in to set it up (Telegram: `BOT_TOKEN`), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setting: Option<String>,
    /// That setting's label ("Bot token").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setting_label: Option<String>,
    /// That setting's help (the setup steps).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setting_help: Option<String>,
}

/// My phone, set up.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhoneView {
    pub channel: String,
    /// The channel's title.
    pub title: String,
    /// What the channel called the person ("@jesse").
    #[serde(default)]
    pub name: String,
    /// ms since the epoch.
    #[serde(default)]
    pub paired_at: i64,
}

/// A pairing in progress.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pairing {
    pub channel: String,
    /// The code, as people read it (`ABCD-EFGH`).
    pub code: String,
    /// A link that pairs with one tap (`https://t.me/<bot>?start=<code>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// Who to send the code to ("@byob_jesse_bot").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The channel's own steps.
    #[serde(default)]
    pub instructions: String,
    /// When the code stops working (ms).
    pub expires_at: i64,
}

/// Everything a client shows about My phone.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhoneState {
    #[serde(default)]
    pub channels: Vec<ChannelOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone: Option<PhoneView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pairing: Option<Pairing>,
}

/// Size checks of the phone messages.
pub(crate) fn check_sizes(msg: &ClientMsg) -> Result<(), String> {
    match msg {
        ClientMsg::PairPhone { channel } if channel.len() > 200 || byob_types::channel::split_key(channel).is_none() => Err("bad channel".into()),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ServerMsg;

    #[test]
    fn phone_messages_round_trip() {
        for m in [ClientMsg::PairPhone { channel: "telegram:telegram".into() }, ClientMsg::CancelPairing, ClientMsg::UnpairPhone, ClientMsg::TestPhone] {
            assert!(m.check_sizes().is_ok());
            assert_eq!(ClientMsg::from_json(&m.to_json()).unwrap(), m);
        }
        assert!(ClientMsg::PairPhone { channel: "nope".into() }.check_sizes().is_err());
        let s = ServerMsg::Phone(PhoneState {
            channels: vec![ChannelOption {
                key: "telegram:telegram".into(),
                plugin: "telegram".into(),
                title: "Telegram".into(),
                two_way: true,
                ..Default::default()
            }],
            phone: Some(PhoneView { channel: "telegram:telegram".into(), title: "Telegram".into(), name: "@jesse".into(), paired_at: 5 }),
            pairing: Some(Pairing { channel: "telegram:telegram".into(), code: "ABCD-EFGH".into(), expires_at: 9, ..Default::default() }),
        });
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(ServerMsg::from_json(&json).unwrap(), s, "{json}");
    }
}
