use byob_protocol::phone::{ChannelOption, Pairing, PhoneState, PhoneView};
use byob_protocol::routines::{Routine, RoutineView, Trigger};
use byob_types::channel::CHANNEL_PHONE;

use super::*;
use crate::settings::Tab;

const H: f32 = 1600.0;

fn texts(ctx: &egui::Context, app: &mut ByobApp, width: f32, events: Vec<egui::Event>) -> plugins::tests::Texts {
    let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, H))), events, ..Default::default() };
    let mut out = ctx.run_ui(input, |ui| app.ui(ui));
    out.textures_delta.clear();
    plugins::tests::texts_of(&out)
}

fn click_text(ctx: &egui::Context, app: &mut ByobApp, width: f32, text: &str) -> plugins::tests::Texts {
    let drawn = texts(ctx, app, width, vec![]);
    let at = drawn.iter().find(|(t, _)| t == text).or_else(|| drawn.iter().find(|(t, _)| t.contains(text))).map(|(_, r)| r.center());
    let at = at.unwrap_or_else(|| panic!("no {text:?} in {:?}", drawn.iter().map(|t| &t.0).collect::<Vec<_>>()));
    let button = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
    for events in [vec![egui::Event::PointerMoved(at)], vec![button(true)], vec![button(false)]] {
        texts(ctx, app, width, events);
    }
    texts(ctx, app, width, vec![])
}

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

fn paired() -> PhoneState {
    PhoneState {
        channels: vec![telegram(true)],
        phone: Some(PhoneView { channel: "telegram:telegram".into(), title: "Telegram".into(), name: "@jesse".into(), paired_at: 1 }),
        pairing: None,
    }
}

/// An app whose engine knows My phone (13.23), on Settings → Plugins.
fn with_phone(state: PhoneState) -> (Fake, ByobApp) {
    let fake = Fake::default();
    let mut app = crowded_with(fake.clone());
    fake.inbox.lock().unwrap().extend([ServerMsg::Welcome(Welcome::current()), ServerMsg::Phone(state)]);
    app.open_settings_at(Tab::Plugins);
    (fake, app)
}

fn sent(fake: &Fake) -> Vec<ClientMsg> {
    std::mem::take(&mut *fake.sent.lock().unwrap())
}

fn shown(t: &plugins::tests::Texts) -> Vec<&String> {
    t.iter().map(|x| &x.0).collect()
}

/// Not set up: the steps (BotFather, /newbot), a token field; Save sends the token as the
/// plugin's secret setting and switches Telegram on. An older engine: no section.
#[test]
fn my_phone_walks_through_setting_up_telegram() {
    let ctx = egui::Context::default();
    let fake = Fake::default();
    let mut old = crowded_with(fake.clone());
    fake.inbox.lock().unwrap().push(ServerMsg::Welcome(byob_protocol::Welcome { caps: vec![], ..Welcome::current() }));
    old.open_settings_at(Tab::Plugins);
    texts(&ctx, &mut old, 1100.0, vec![]);
    let page = texts(&ctx, &mut old, 1100.0, vec![]);
    assert!(!plugins::tests::has(&page, "My phone"), "an engine without it: {:?}", shown(&page));

    let (fake, mut app) = with_phone(PhoneState { channels: vec![telegram(false)], ..Default::default() });
    texts(&ctx, &mut app, 1100.0, vec![]);
    let page = texts(&ctx, &mut app, 1100.0, vec![]);
    for want in ["My phone", "Set up Telegram (about 3 minutes):", "@BotFather", "/newbot", "Save"] {
        assert!(plugins::tests::has(&page, want), "{want}: {:?}", shown(&page));
    }
    assert!(!plugins::tests::has(&page, "Pair my phone"), "no pairing before there's a bot");
    sent(&fake);
    app.plugins.phone.token = "123456:AAE-secret".into();
    click_text(&ctx, &mut app, 1100.0, "Save");
    let msgs = sent(&fake);
    assert!(
        msgs.contains(&ClientMsg::SetPluginSetting { plugin: "telegram".into(), key: "BOT_TOKEN".into(), value: Some("123456:AAE-secret".into()) }),
        "{msgs:?}"
    );
    assert!(msgs.contains(&ClientMsg::SetPluginEnabled { scope: byob_protocol::ConfigScope::Global, plugin: "telegram".into(), enabled: Some(true) }));
    let page = texts(&ctx, &mut app, 1100.0, vec![]);
    assert!(!shown(&page).iter().any(|t| t.contains("AAE-secret")), "the token isn't drawn");
}

/// Set up, not paired: the bot's status and Pair my phone; pairing shows the link and the
/// code; paired: who, a test message, Unpair. At phone width too.
#[test]
fn my_phone_pairs_tests_and_unpairs() {
    let ctx = egui::Context::default();
    let (fake, mut app) = with_phone(PhoneState { channels: vec![telegram(true)], ..Default::default() });
    texts(&ctx, &mut app, 1100.0, vec![]);
    let page = texts(&ctx, &mut app, 1100.0, vec![]);
    assert!(plugins::tests::has(&page, "Connected as @my_byob_bot"), "{:?}", shown(&page));
    sent(&fake);
    click_text(&ctx, &mut app, 1100.0, "Pair my phone");
    assert_eq!(sent(&fake), [ClientMsg::PairPhone { channel: "telegram:telegram".into() }]);
    let link = "https://t.me/my_byob_bot?start=ABCDEFGH";
    fake.inbox.lock().unwrap().push(ServerMsg::Phone(PhoneState {
        channels: vec![telegram(true)],
        phone: None,
        pairing: Some(Pairing {
            channel: "telegram:telegram".into(),
            code: "ABCD-EFGH".into(),
            link: Some(link.into()),
            name: Some("@my_byob_bot".into()),
            instructions: String::new(),
            expires_at: i64::MAX,
        }),
    }));
    texts(&ctx, &mut app, 1100.0, vec![]);
    let page = texts(&ctx, &mut app, 1100.0, vec![]);
    for want in ["open this link and tap Start", link, "Or send @my_byob_bot this code:", "ABCD-EFGH", "Cancel"] {
        assert!(plugins::tests::has(&page, want), "{want}: {:?}", shown(&page));
    }
    fake.inbox.lock().unwrap().push(ServerMsg::Phone(paired()));
    for width in [1100.0, 390.0] {
        texts(&ctx, &mut app, width, vec![]);
        let page = texts(&ctx, &mut app, width, vec![]);
        for want in ["✓ Paired with @jesse on Telegram", "Send a test message", "Unpair"] {
            assert!(plugins::tests::has(&page, want), "{want} at {width}: {:?}", shown(&page));
        }
    }
    sent(&fake);
    click_text(&ctx, &mut app, 1100.0, "Send a test message");
    assert_eq!(sent(&fake), [ClientMsg::TestPhone]);
    click_text(&ctx, &mut app, 1100.0, "Unpair");
    assert_eq!(sent(&fake), [ClientMsg::UnpairPhone]);
}

/// A routine's page offers Text me only once My phone is set up; ticked, the routine is saved
/// with the phone channel.
#[test]
fn text_me_shows_only_with_my_phone() {
    let ctx = egui::Context::default();
    let routine = Routine {
        id: "loom".into(),
        name: "Loom deals".into(),
        instructions: "Find looms.".into(),
        when: vec![Trigger::Schedule { rule: "FREQ=DAILY".into(), at: "08:00".into(), zone: Some("UTC".into()), start: None }],
        ..Routine::default()
    };
    let view = RoutineView { routine: routine.clone(), schedule: vec!["Every day at 8:00 AM".into()], ..Default::default() };
    let fake = Fake::default();
    let mut app = crowded_with(fake.clone());
    fake.inbox.lock().unwrap().extend([ServerMsg::Welcome(Welcome::current()), ServerMsg::Routines { items: vec![view] }]);
    texts(&ctx, &mut app, 1100.0, vec![]);
    click_text(&ctx, &mut app, 1100.0, "Routines");
    let page = click_text(&ctx, &mut app, 1100.0, "Loom deals");
    assert!(plugins::tests::has(&page, "With a notification"));
    assert!(!shown(&page).iter().any(|t| t.starts_with("Text me")), "no phone, no Text me: {:?}", shown(&page));
    fake.inbox.lock().unwrap().push(ServerMsg::Phone(paired()));
    texts(&ctx, &mut app, 1100.0, vec![]);
    let page = texts(&ctx, &mut app, 1100.0, vec![]);
    assert!(plugins::tests::has(&page, "Text me (on Telegram)"), "{:?}", shown(&page));
    sent(&fake);
    click_text(&ctx, &mut app, 1100.0, "Text me (on Telegram)");
    click_text(&ctx, &mut app, 1100.0, "Save");
    let saved = sent(&fake).into_iter().find_map(|m| if let ClientMsg::SaveRoutine { routine } = m { Some(routine) } else { None }).expect("saved");
    assert!(saved.channels.iter().any(|c| c == CHANNEL_PHONE), "{:?}", saved.channels);
}
