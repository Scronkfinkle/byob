//! My phone in the UI: the channel byob texts you on (Settings → Plugins → My phone), its
//! setup steps, pairing and status, and whether routines can offer "Text me". The engine
//! keeps all of it ([`byob_protocol::phone`]); this is what a client shows and the messages
//! its buttons send. The channel's own setting (a bot token) is the plugin's ordinary secret
//! setting: it goes to the engine once and is never shown again.

use byob_protocol::phone::{ChannelOption, PhoneState};
use byob_protocol::{ClientMsg, ConfigScope, ServerMsg};
use egui::{RichText, Ui};

use crate::form;
use crate::theme::Tokens;

/// What the client knows of My phone, and the section's own fields.
#[derive(Default)]
pub struct Phone {
    /// The engine's last word (`None`: an engine without My phone).
    pub state: Option<PhoneState>,
    /// The channel picked (its key); the first one when none is.
    pub picked: Option<String>,
    /// The token being typed (sent on Save, then cleared).
    pub token: String,
    /// Change the bot token: the field again, though one is set.
    pub changing: bool,
}

/// The setup steps for a channel, in words for someone who has never made a bot.
fn steps(c: &ChannelOption) -> Vec<String> {
    if c.plugin == "telegram" {
        return vec![
            "Install Telegram on your phone and sign in (it's free).".into(),
            "In Telegram, search for @BotFather, open it and send /newbot.".into(),
            "Pick a name (like \"My byob\"), then a username ending in \"bot\" (like my_byob_bot).".into(),
            "BotFather answers with a token (it looks like 123456789:AAEh…): copy it and paste it below.".into(),
        ];
    }
    c.setting_help.clone().into_iter().collect()
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

    fn channel_in<'a>(&self, s: &'a PhoneState) -> Option<&'a ChannelOption> {
        let key = s.phone.as_ref().map(|p| &p.channel).or(self.picked.as_ref());
        key.and_then(|k| s.channels.iter().find(|c| &c.key == k)).or_else(|| s.channels.first())
    }

    /// The channel the section is about.
    pub fn channel(&self) -> Option<&ChannelOption> {
        self.state.as_ref().and_then(|s| self.channel_in(s))
    }

    /// Save: the token goes to the plugin's secret setting, and the plugin is switched on.
    pub fn save_token(&mut self) -> Vec<ClientMsg> {
        let token = std::mem::take(&mut self.token).trim().to_owned();
        let Some(c) = self.channel().cloned() else { return Vec::new() };
        let Some(key) = c.setting.clone() else { return Vec::new() };
        if token.is_empty() {
            return Vec::new();
        }
        self.changing = false;
        vec![
            ClientMsg::SetPluginSetting { plugin: c.plugin.clone(), key, value: Some(token) },
            ClientMsg::SetPluginEnabled { scope: ConfigScope::Global, plugin: c.plugin, enabled: Some(true) },
        ]
    }

    /// Pair my phone.
    pub fn pair(&self) -> Vec<ClientMsg> {
        self.channel().map(|c| ClientMsg::PairPhone { channel: c.key.clone() }).into_iter().collect()
    }

    /// Draw the section; the messages its buttons sent.
    pub fn ui(&mut self, ui: &mut Ui, t: &Tokens) -> Vec<ClientMsg> {
        let mut out = Vec::new();
        let Some(state) = self.state.clone() else {
            ui.label(RichText::new("This byob can't text you (its engine is older).").color(t.text_faint));
            return out;
        };
        if state.channels.is_empty() {
            ui.label(RichText::new("No plugin here offers a way to text you.").color(t.text_faint));
            return out;
        }
        ui.spacing_mut().item_spacing.y = t.pad(8.0);
        let Some(c) = self.channel_in(&state).cloned() else { return out };
        // Which channel (one now; a choice once there are more).
        if state.channels.len() > 1 && state.phone.is_none() {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Text me on").color(t.text));
                let mut i = state.channels.iter().position(|x| x.key == c.key).unwrap_or(0);
                let names: Vec<(usize, &str)> = state.channels.iter().enumerate().map(|(i, c)| (i, c.title.as_str())).collect();
                if form::select(ui, t, &mut i, &names, None) {
                    self.picked = state.channels.get(i).map(|x| x.key.clone());
                }
            });
        }
        let what = if c.two_way && c.buttons {
            "free; you can reply, and answer questions with a tap"
        } else if c.two_way {
            "you can reply"
        } else {
            "one way: byob tells you, you answer in byob"
        };
        ui.label(RichText::new(format!("{}: {what}.", c.title)).color(t.text_dim));

        // Paired: who, status, test, unpair.
        if let Some(p) = &state.phone {
            let who = if !c.two_way {
                format!("Texts go to {}", p.title)
            } else if p.name.is_empty() {
                format!("Paired on {}", p.title)
            } else {
                format!("Paired with {} on {}", p.name, p.title)
            };
            ui.label(RichText::new(format!("✓ {who}")).color(t.text_strong));
            status_line(ui, t, &c);
            ui.horizontal(|ui| {
                if form::button(ui, t, "Send a test message", true).clicked() {
                    out.push(ClientMsg::TestPhone);
                }
                let (unpair, tip) = if c.two_way { ("Unpair", "Forget this phone: it can't reach byob any more") } else { ("Stop", "byob stops sending here") };
                if form::danger_button(ui, t, unpair).on_hover_text(tip).clicked() {
                    out.push(ClientMsg::UnpairPhone);
                }
            });
            ui.add(
                egui::Label::new(
                    RichText::new("Routines can text you now: tick Text me under Tell me on a routine's page, or ask byob (\"text me when it finds one\").")
                        .small()
                        .color(t.text_faint),
                )
                .wrap(),
            );
            return out;
        }

        // Not set up: the steps and the token.
        if !c.configured || self.changing {
            ui.label(RichText::new(format!("Set up {} (about 3 minutes):", c.title)).color(t.text));
            for (i, s) in steps(&c).iter().enumerate() {
                ui.add(egui::Label::new(RichText::new(format!("{}. {s}", i + 1)).color(t.text)).wrap());
            }
            ui.horizontal(|ui| {
                let w = (ui.available_width().min(380.0) - 90.0).max(120.0);
                let field = ui.add(
                    form::field(&mut self.token, t)
                        .password(true)
                        .desired_width(w)
                        .hint_text(format!("Paste the {} here", c.setting_label.as_deref().unwrap_or("token").to_lowercase()))
                        .char_limit(byob_protocol::plugins::MAX_SETTING_VALUE_CHARS),
                );
                let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if form::button(ui, t, "Save", !self.token.trim().is_empty()).clicked() || enter {
                    out.extend(self.save_token());
                }
            });
            let what = c.setting_label.as_deref().unwrap_or("token").to_lowercase();
            ui.add(
                egui::Label::new(
                    RichText::new(format!("The {what} stays on this computer, in byob's secrets file; it's never shown again.")).small().color(t.text_faint),
                )
                .wrap(),
            );
            if self.changing && ui.link(RichText::new("Cancel").small()).clicked() {
                self.changing = false;
                self.token.clear();
            }
            return out;
        }
        if !c.enabled {
            ui.label(RichText::new(format!("{} is switched off.", c.title)).color(t.text));
            if form::button(ui, t, &format!("Switch {} on", c.title), true).clicked() {
                out.push(ClientMsg::SetPluginEnabled { scope: ConfigScope::Global, plugin: c.plugin.clone(), enabled: Some(true) });
            }
            return out;
        }

        // Set up, not paired: status, then pairing.
        status_line(ui, t, &c);
        match &state.pairing {
            Some(p) if p.channel == c.key => {
                if let Some(link) = &p.link {
                    ui.label(RichText::new("On your phone, open this link and tap Start:").color(t.text));
                    ui.horizontal_wrapped(|ui| {
                        ui.hyperlink_to(RichText::new(link).color(t.accent), link);
                        if ui.small_button("Copy").on_hover_text("Copy the link").clicked() {
                            ui.ctx().copy_text(link.clone());
                        }
                    });
                }
                let to = p.name.clone().unwrap_or_else(|| "byob's bot".into());
                ui.label(RichText::new(format!("Or send {to} this code:")).color(t.text));
                ui.label(RichText::new(&p.code).monospace().size(t.text_size(20.0)).color(t.text_strong));
                ui.horizontal(|ui| {
                    ui.label(RichText::new("The code works once, for an hour.").small().color(t.text_faint));
                    if ui.link(RichText::new("Cancel").small()).clicked() {
                        out.push(ClientMsg::CancelPairing);
                    }
                });
            }
            _ if !c.two_way => {
                if form::button(ui, t, "Use for My phone", true).on_hover_text("byob sends a first message there").clicked() {
                    out.extend(self.pair());
                }
            }
            _ => {
                if form::button(ui, t, "Pair my phone", true).on_hover_text("Shows a link and a code to send from your phone").clicked() {
                    out.extend(self.pair());
                }
            }
        }
        let change = format!("Change the {}", c.setting_label.as_deref().unwrap_or("setting").to_lowercase());
        if c.setting.is_some() && ui.link(RichText::new(change).small()).clicked() {
            self.changing = true;
        }
        out
    }
}

/// The channel's own word on itself: connected, or what's wrong.
fn status_line(ui: &mut Ui, t: &Tokens, c: &ChannelOption) {
    let (text, dot, color) = match (&c.status, c.ok) {
        (Some(s), true) => (s.clone(), t.success, t.text_dim),
        (Some(s), false) => (s.clone(), t.danger, t.danger),
        (None, _) => ("Starting…".to_owned(), t.text_faint, t.text_faint),
    };
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = t.pad(6.0);
        ui.label(RichText::new("●").color(dot));
        ui.add(egui::Label::new(RichText::new(text).color(color)).wrap());
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use byob_protocol::phone::{Pairing, PhoneView};

    fn telegram(configured: bool) -> ChannelOption {
        ChannelOption {
            key: "telegram:telegram".into(),
            plugin: "telegram".into(),
            title: "Telegram".into(),
            two_way: true,
            buttons: true,
            enabled: configured,
            configured,
            ok: configured,
            status: configured.then(|| "Connected as @my_byob_bot".into()),
            setting: Some("BOT_TOKEN".into()),
            setting_label: Some("Bot token".into()),
            setting_help: None,
        }
    }

    #[test]
    fn saving_the_token_sets_the_secret_and_switches_the_plugin_on() {
        let mut p = Phone::default();
        p.apply(&ServerMsg::Phone(PhoneState { channels: vec![telegram(false)], ..Default::default() }));
        assert!(!p.ready());
        assert!(p.save_token().is_empty(), "nothing typed, nothing sent");
        p.token = " 123:abc ".into();
        let sent = p.save_token();
        assert_eq!(
            sent,
            vec![
                ClientMsg::SetPluginSetting { plugin: "telegram".into(), key: "BOT_TOKEN".into(), value: Some("123:abc".into()) },
                ClientMsg::SetPluginEnabled { scope: ConfigScope::Global, plugin: "telegram".into(), enabled: Some(true) },
            ]
        );
        assert!(p.token.is_empty(), "the field doesn't keep it");
        assert_eq!(p.pair(), vec![ClientMsg::PairPhone { channel: "telegram:telegram".into() }]);
        p.apply(&ServerMsg::Phone(PhoneState {
            channels: vec![telegram(true)],
            phone: Some(PhoneView { channel: "telegram:telegram".into(), title: "Telegram".into(), name: "@jesse".into(), paired_at: 1 }),
            pairing: Some(Pairing::default()),
        }));
        assert!(p.ready());
    }
}
