//! My phone in the UI: the channel byob texts you on (Settings → Plugins → My phone), its
//! setup steps, pairing and status, and whether routines can offer "Text me". The engine
//! keeps all of it; this is what a client shows ([`byob_protocol::phone`]).

use byob_protocol::ServerMsg;
use byob_protocol::phone::PhoneState;

/// What the client knows of My phone.
#[derive(Default)]
pub struct Phone {
    /// The engine's last word (`None`: an engine without My phone).
    pub state: Option<PhoneState>,
}

impl Phone {
    /// Take in a server message; `true` if it was the phone's.
    pub fn apply(&mut self, msg: &ServerMsg) -> bool {
        match msg {
            ServerMsg::Phone(s) => {
                self.state = Some(s.clone());
                true
            }
            _ => false,
        }
    }

    /// My phone is set up (a routine can text).
    pub fn ready(&self) -> bool {
        self.state.as_ref().is_some_and(|s| s.phone.is_some())
    }
}
