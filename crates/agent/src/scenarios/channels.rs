//! Channels, end to end: a fake two-way channel plugin (a native backend that records what
//! byob asks it to send, and through which the scenario plays the person's phone) pairs with
//! a code, and then a routine's report reaches the phone, a reply continues the run's chat,
//! `/new` starts a chat of the phone's own (replies go on with it), anything else is told how,
//! a run waiting on an approval gets buttons and a tap lets it go on, a must-confirm call
//! says to open byob, strangers get nothing, `text_me` exists only in runs, and the caps hold.

use std::collections::BTreeMap;

use byob_plugins::Embedded;
use byob_plugins::native::{NativeBackend, NativeFactory, Outbox};
use byob_protocol::phone::PhoneState;
use byob_protocol::routines::{Routine, RoutineEventKind, RunStatus};
use byob_types::channel::{CHANNEL_PHONE, DAILY_CAP, MAX_INBOUND_PER_MINUTE};
use byob_types::routine::CHANNEL_DESKTOP;

use super::routines::{Time, daily};
use super::*;

const KEY: &str = "fakechat:chat";
/// The person's id on the fake channel.
const ME: &str = "4242";

/// What the fake channel saw, and its way back into byob.
#[derive(Clone, Default)]
struct Phone {
    /// Every `deliver`'s params, in order.
    sent: Arc<Mutex<Vec<Value>>>,
    /// Every event byob sent it (`channel_peers`).
    events: Arc<Mutex<Vec<Value>>>,
    /// `pair_start`'s codes.
    codes: Arc<Mutex<Vec<String>>>,
    out: Arc<Mutex<Option<Outbox>>>,
}

struct FakeChannel(Phone);

impl NativeBackend for FakeChannel {
    fn init(&mut self, _init: &Value, out: &Outbox) {
        *self.0.out.lock().unwrap() = Some(out.clone());
        out.send(json!({"type": "channel_status", "channel": "chat", "ok": true, "text": "Connected as @fake_bot"}));
    }

    fn event(&mut self, event: &Value, _out: &Outbox) {
        self.0.events.lock().unwrap().push(event.clone());
    }

    fn request(&mut self, method: &str, params: &Value, _out: &Outbox) -> Result<Value, String> {
        match method {
            "deliver" => {
                let mut sent = self.0.sent.lock().unwrap();
                sent.push(params.clone());
                Ok(json!({"ref": format!("m{}", sent.len())}))
            }
            "pair_start" => {
                let code = params["code"].as_str().unwrap().to_owned();
                self.0.codes.lock().unwrap().push(code.clone());
                Ok(json!({"link": format!("https://fake.example/start?c={code}"), "name": "@fake_bot", "instructions": "Open the link."}))
            }
            "unpair" => Ok(Value::Null),
            other => Err(format!("no {other}")),
        }
    }
}

impl Phone {
    fn factory(&self) -> NativeFactory {
        let me = self.clone();
        Arc::new(move || Box::new(FakeChannel(me.clone())) as Box<dyn NativeBackend>)
    }

    fn plugin() -> Embedded {
        let manifest = json!({
            "id": "fakechat", "name": "Fake chat", "backend": {"native": "fakechat"},
            "channels": [{"id": "chat", "title": "Fake chat", "two_way": true, "buttons": true, "max_len": 500}]
        });
        Embedded { id: "fakechat".into(), files: [("byob-plugin.json".to_owned(), manifest.to_string())].into_iter().collect::<BTreeMap<_, _>>() }
    }

    /// The person (or a stranger, `from`) sends something.
    fn says(&self, msg: Value) {
        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Some(out) = self.out.lock().unwrap().as_ref() {
                out.send(msg);
                return;
            }
            assert!(Instant::now() < deadline, "the channel's backend never started");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn text(&self, from: &str, text: &str, reply_to: Option<&str>) {
        self.says(json!({"type": "inbound", "channel": "chat", "peer": from, "peer_name": "@jesse", "text": text, "reply_to": reply_to}));
    }

    /// Wait for the `n`th message sent (1-based) and return it.
    fn nth(&self, n: usize) -> Value {
        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Some(m) = self.sent.lock().unwrap().get(n - 1) {
                return m.clone();
            }
            assert!(Instant::now() < deadline, "byob never sent message {n}; it sent {:?}", self.sent.lock().unwrap());
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn count(&self) -> usize {
        self.sent.lock().unwrap().len()
    }

    /// Wait for the next message after the `seen` already looked at.
    fn next(&self, seen: &mut usize) -> Value {
        *seen += 1;
        self.nth(*seen)
    }
}

fn world(time: &Time) -> (World, Phone) {
    let phone = Phone::default();
    let (t, p) = (time.clone(), phone.clone());
    let w = World::with(move |c| {
        c.clock = t.clock();
        if let Some(plugins) = c.plugins.as_mut() {
            plugins.bundled.push(Phone::plugin());
            plugins.natives = std::mem::take(&mut plugins.natives).backend("fakechat", p.factory());
        }
    });
    (w, phone)
}

impl World {
    /// My phone's state once `f` accepts it (the latest seen, or the next one that does).
    fn phone_where(&mut self, mut f: impl FnMut(&PhoneState) -> bool) -> PhoneState {
        if let Some(s) = self.seen.phone.clone().filter(|s| f(s)) {
            return s;
        }
        self.wait(|m| match m {
            ServerMsg::Phone(s) if f(s) => Some(s.clone()),
            _ => None,
        })
    }

    /// Pair the fake phone as My phone.
    fn pair(&mut self, phone: &Phone) {
        self.tell(ClientMsg::PairPhone { channel: KEY.into() });
        let state = self.phone_where(|s| s.pairing.is_some());
        let shown = state.pairing.unwrap();
        let code = phone.codes.lock().unwrap().last().cloned().unwrap();
        assert_eq!(shown.code.replace('-', ""), code);
        assert_eq!(shown.link.as_deref(), Some(format!("https://fake.example/start?c={code}").as_str()));
        // Sent the way people type it: lower case, with the dash.
        let typed = format!("{}-{}", &code[..4], &code[4..]).to_lowercase();
        phone.says(json!({"type": "pair_claim", "channel": "chat", "peer": ME, "peer_name": "@jesse", "code": typed}));
        let state = self.phone_where(|s| s.phone.is_some());
        assert_eq!(state.phone.as_ref().map(|p| p.name.as_str()), Some("@jesse"));
        assert!(state.pairing.is_none());
        // The welcome text is sent after the state says paired: wait for it, so a test that
        // counts messages from here doesn't count it as its own (it did on a slow runner).
        let deadline = Instant::now() + PATIENCE;
        while !phone.sent.lock().unwrap().iter().any(|m| m["to"] == ME && m["text"].as_str().is_some_and(|t| t.starts_with("Paired with"))) {
            assert!(Instant::now() < deadline, "no welcome text: {:?}", phone.sent.lock().unwrap());
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// Pairing: a wrong code pairs nobody (and gets one neutral line, as a pairing is going on);
/// the right one makes that person My phone, saved in channels.json; the backend learns who's
/// paired; at most three codes wait at once; a code an hour old is dead.
#[test]
fn pairing_takes_only_a_waiting_code_and_makes_my_phone() {
    let time = Time::new("2026-11-24 07:00");
    let (mut w, phone) = world(&time);
    let state = w.phone_where(|s| s.channels.iter().any(|c| c.key == KEY && c.ok));
    let c = state.channels.iter().find(|c| c.key == KEY).unwrap();
    assert_eq!((c.title.as_str(), c.two_way, c.buttons, c.status.as_deref()), ("Fake chat", true, true, Some("Connected as @fake_bot")));
    assert!(state.phone.is_none());

    w.tell(ClientMsg::PairPhone { channel: KEY.into() });
    w.phone_where(|s| s.pairing.is_some());
    phone.says(json!({"type": "pair_claim", "channel": "chat", "peer": "777", "code": "ZZZZZZZZ"}));
    let wrong = phone.nth(1);
    assert_eq!(wrong["to"], "777");
    assert!(wrong["text"].as_str().unwrap().starts_with("That code didn't work"), "{wrong}");
    // Nor does a stranger's message get through afterwards.
    phone.text("777", "hello?", None);
    w.settle();
    assert_eq!(phone.count(), 1, "strangers get nothing");

    w.pair(&phone);
    let hello = phone.nth(2);
    assert_eq!(hello["to"], ME);
    assert!(hello["text"].as_str().unwrap().starts_with("Paired with byob"), "{hello}");
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(w.home.path().join("channels.json")).unwrap()).unwrap();
    assert_eq!(saved["phone"], json!({"channel": KEY, "peer": ME}));
    assert_eq!(saved["peers"][0]["peer"], ME);
    let peers = phone.events.lock().unwrap().iter().rev().find(|e| e["event"] == "channel_peers").cloned().unwrap();
    assert_eq!(peers["peers"], json!([ME]));

    // Three codes may wait; a fourth is refused.
    for _ in 0..3 {
        w.tell(ClientMsg::PairPhone { channel: KEY.into() });
        w.phone_where(|s| s.pairing.is_some());
    }
    w.tell(ClientMsg::PairPhone { channel: KEY.into() });
    let refused = w.wait(|m| match m {
        ServerMsg::Notice { text, .. } => Some(text.clone()),
        _ => None,
    });
    assert!(refused.contains("3 pairing codes waiting"), "{refused}");
    // An hour later they're all dead: the last one doesn't pair the stranger.
    let last = phone.codes.lock().unwrap().last().cloned().unwrap();
    time.set("2026-11-24 08:01");
    w.settle();
    phone.says(json!({"type": "pair_claim", "channel": "chat", "peer": "888", "code": last}));
    w.settle();
    phone.text("888", "let me in", None);
    w.settle();
    assert!(phone.sent.lock().unwrap().iter().all(|m| m["to"] != "888"), "an expired code pairs nobody, and nobody answers them");
    w.restart();
    let state = w.phone_where(|s| s.phone.is_some());
    assert_eq!(state.phone.unwrap().name, "@jesse", "paired across a restart");

    // Unpaired: the phone is a stranger again.
    w.tell(ClientMsg::UnpairPhone);
    w.phone_where(|s| s.phone.is_none());
    let before = phone.count();
    phone.text(ME, "still there?", None);
    w.settle();
    assert_eq!(phone.count(), before);
}

/// The loom routine texts: its report reaches the phone; a reply to that text continues the
/// run's chat as a user message, and the answer goes back to the phone.
#[test]
fn a_report_reaches_the_phone_and_a_reply_continues_the_run() {
    let time = Time::new("2026-11-24 07:00");
    let (mut w, phone) = world(&time);
    w.pair(&phone);
    let mut seen = phone.count();
    let r = w.save(Routine { channels: vec![CHANNEL_DESKTOP.into(), CHANNEL_PHONE.into()], ..daily("Loom deals", "08:00") });
    w.model.calls("report", json!({"title": "Schacht Wolf Pup 8S, 20% off", "body": "At The Woolery, $1,040 until Sunday."}));
    w.model.says("Reported.");
    time.set("2026-11-24 08:00");
    let (_, chat, _, _) = w.event(RoutineEventKind::Reported);
    let chat = chat.unwrap();
    let text = phone.next(&mut seen);
    assert_eq!(text["to"], ME);
    assert_eq!(text["chat"], chat);
    let body = text["text"].as_str().unwrap();
    assert!(body.starts_with("Schacht Wolf Pup 8S, 20% off\n\nAt The Woolery"), "{body}");
    assert!(body.contains("— Loom deals"), "{body}");
    assert!(text.get("buttons").is_none());
    w.finished(&r.id);

    // The reply: a normal turn in the run's chat, told it came from the phone.
    w.model.answers(|req| {
        let users = req.user_texts();
        let last = users.last().unwrap();
        assert!(last.contains("from their phone") && last.ends_with("Which shop?"), "{last}");
        Answer::text("The Woolery, in Kentucky.")
    });
    phone.text(ME, "Which shop?", Some(&format!("m{seen}")));
    let answer = phone.next(&mut seen);
    assert_eq!(answer["text"], "The Woolery, in Kentucky.");
    w.open_chat(&chat);
    let texts: Vec<&str> = w.seen.entries.iter().map(|e| e.content.as_str()).collect();
    assert!(texts.contains(&"Which shop?") && texts.contains(&"The Woolery, in Kentucky."), "{texts:?}");

    // A message that isn't a reply starts nothing: reply, or /new.
    phone.text(ME, "Is it in stock?", None);
    let note = phone.next(&mut seen);
    assert!(note["text"].as_str().unwrap().starts_with("To keep talking about something, reply to my message"), "{note}");
    w.settle();
    assert_eq!(w.model.unused(), 0);
    w.model.check();
}

/// `/new TEXT` starts a chat with TEXT, and a reply to its answer goes on with it; `/new`
/// alone (what Telegram's command menu sends) makes the next message start one, once;
/// `/help` says what works.
#[test]
fn new_starts_a_chat_from_the_phone_and_replies_go_on_with_it() {
    let time = Time::new("2026-11-24 07:00");
    let (mut w, phone) = world(&time);
    w.pair(&phone);
    let mut seen = phone.count();

    w.model.answers(|req| {
        let users = req.user_texts();
        assert!(users.len() == 1 && users[0].contains("from their phone") && users[0].ends_with("Which spinning wheel for a beginner?"), "{users:?}");
        Answer::text("Try the Ashford Kiwi.")
    });
    phone.text(ME, "/new@fake_bot Which spinning wheel for a beginner?", None);
    let answer = phone.next(&mut seen);
    assert_eq!(answer["text"], "Try the Ashford Kiwi.");
    let first = answer["chat"].as_str().unwrap().to_owned();
    w.settle();

    w.model.answers(|req| {
        assert_eq!(req.user_texts().len(), 2, "the same chat");
        Answer::text("About $600.")
    });
    phone.text(ME, "How much?", Some(&format!("m{seen}")));
    let answer = phone.next(&mut seen);
    assert_eq!((answer["text"].as_str(), answer["chat"].as_str()), (Some("About $600."), Some(first.as_str())));
    w.settle();

    phone.text(ME, "/new", None);
    assert_eq!(phone.next(&mut seen)["text"], "OK, a new chat: what's it about?");
    w.model.answers(|req| {
        assert_eq!(req.user_texts().len(), 1, "a new chat");
        Answer::text("Sunny.")
    });
    phone.text(ME, "weather in Lisbon", None);
    let answer = phone.next(&mut seen);
    assert_eq!(answer["text"], "Sunny.");
    assert_ne!(answer["chat"], first.as_str());
    w.settle();
    // Once: the next message that isn't a reply starts nothing.
    phone.text(ME, "and tomorrow?", None);
    assert!(phone.next(&mut seen)["text"].as_str().unwrap().contains("send /new"));

    phone.text(ME, "/help", None);
    assert!(phone.next(&mut seen)["text"].as_str().unwrap().contains("/new to start a new chat"));
    w.settle();
    assert_eq!(w.model.unused(), 0);
    w.model.check();
}

/// A read-only routine's run wants to write a file: the phone gets Allow/Deny; Allow lets the
/// run go on. A must-confirm call (making a routine) gets no buttons: open byob to confirm.
#[test]
fn a_run_waiting_on_an_approval_is_answered_from_the_phone() {
    let time = Time::new("2026-11-24 07:00");
    let (mut w, phone) = world(&time);
    w.pair(&phone);
    let mut seen = phone.count();
    let r = w.save(Routine { when: vec![], channels: vec![CHANNEL_PHONE.into()], ..daily("Notes", "08:00") });
    w.model.calls("write_file", json!({"path": "found.md", "content": "a loom"}));
    w.model.says("Saved it.");
    w.tell(ClientMsg::RunRoutine { id: r.id.clone() });
    let ask = phone.next(&mut seen);
    let text = ask["text"].as_str().unwrap();
    assert!(text.starts_with("Notes wants to: ") && text.contains("found.md") && text.ends_with("Allow it?"), "{text}");
    let buttons = ask["buttons"].as_array().unwrap();
    assert_eq!(buttons.iter().map(|b| b["label"].as_str().unwrap()).collect::<Vec<_>>(), ["✅ Allow", "❌ Deny", "🔓 Always allow for this routine"]);
    let allow = buttons[0]["id"].as_str().unwrap().to_owned();
    let always = buttons[2]["id"].as_str().unwrap().to_owned();
    assert!(allow.len() <= 64);
    // A stranger pressing it does nothing.
    phone.says(json!({"type": "inbound", "channel": "chat", "peer": "777", "button": allow}));
    w.settle();
    assert_eq!(w.runs(&r.id)[0].status, RunStatus::Waiting);
    phone.says(json!({"type": "inbound", "channel": "chat", "peer": ME, "button": allow, "reply_to": format!("m{seen}")}));
    let runs = w.finished(&r.id);
    assert_eq!(runs[0].status, RunStatus::Quiet);
    let ack = phone.next(&mut seen);
    assert!(ack["text"].as_str().unwrap().starts_with("Allowed: "), "{ack}");
    let folder = w.store().chat(runs[0].chat.as_deref().unwrap()).unwrap().unwrap().workspace;
    assert!(folder.join("found.md").exists(), "the run went on and wrote it");
    // Pressed again: already answered.
    phone.says(json!({"type": "inbound", "channel": "chat", "peer": ME, "button": allow}));
    assert!(phone.next(&mut seen)["text"].as_str().unwrap().contains("already answered"));

    // Always allow: the routine's later runs write without asking.
    w.model.calls("write_file", json!({"path": "found.md", "content": "two looms"}));
    w.model.says("Saved again.");
    w.tell(ClientMsg::RunRoutine { id: r.id.clone() });
    let ask = phone.next(&mut seen);
    let always = ask["buttons"][2]["id"].as_str().map_or(always, str::to_owned);
    phone.says(json!({"type": "inbound", "channel": "chat", "peer": ME, "button": always}));
    assert!(phone.next(&mut seen)["text"].as_str().unwrap().contains("from now on without asking"));
    w.settle();
    w.model.calls("write_file", json!({"path": "found.md", "content": "three looms"}));
    w.model.says("Saved, unasked.");
    w.tell(ClientMsg::RunRoutine { id: r.id.clone() });
    let deadline = Instant::now() + PATIENCE;
    while w.model.unused() > 0 {
        assert!(Instant::now() < deadline, "the third run never went on");
        std::thread::sleep(Duration::from_millis(10));
    }
    w.settle();
    assert_eq!(phone.count(), seen, "no question the third time");

    // Must-confirm: creating a routine from a run. The text says all the card does, with the
    // tool's own words on the buttons, and a tap confirms it.
    w.model.calls("create_routine", json!({"name": "Another", "instructions": "Do more."}));
    w.model.says("Made it.");
    w.tell(ClientMsg::RunRoutine { id: r.id.clone() });
    let ask = phone.next(&mut seen);
    let text = ask["text"].as_str().unwrap();
    assert!(text.starts_with("Notes wants to: ") && text.contains("Another") && text.ends_with("This always needs you to say yes."), "{text}");
    let buttons = ask["buttons"].as_array().unwrap();
    assert_eq!(buttons.iter().map(|b| b["label"].as_str().unwrap()).collect::<Vec<_>>(), ["✅ Create routine", "❌ Not now"], "never always");
    let confirm = buttons[0]["id"].as_str().unwrap().to_owned();
    phone.says(json!({"type": "inbound", "channel": "chat", "peer": ME, "button": confirm}));
    assert!(phone.next(&mut seen)["text"].as_str().unwrap().starts_with("Allowed: "));
    // Confirmed from the phone, so it was made.
    w.routines_where(|items| items.iter().any(|v| v.routine.name == "Another"));
    w.settle();

    // A question: its choices are buttons; a reply to it is an answer of its own.
    w.model.calls("ask_user", json!({"question": "Which fiber?", "options": ["Wool", "Cotton"]}));
    w.model.answers(|req| {
        assert!(req.tool_results().last().unwrap().contains("Cotton"), "{:?}", req.tool_results());
        Answer::call("ask_user", json!({"question": "Which color?", "options": ["Red", "Blue"]}))
    });
    w.model.answers(|req| {
        assert!(req.tool_results().last().unwrap().contains("undyed, please"), "{:?}", req.tool_results());
        Answer::text("Noted.")
    });
    w.tell(ClientMsg::RunRoutine { id: r.id.clone() });
    let ask = phone.next(&mut seen);
    assert!(ask["text"].as_str().unwrap().starts_with("Notes asks: Which fiber?"), "{ask}");
    let buttons = ask["buttons"].as_array().unwrap();
    assert_eq!(buttons.iter().map(|b| b["label"].as_str().unwrap()).collect::<Vec<_>>(), ["Wool", "Cotton"]);
    phone.says(json!({"type": "inbound", "channel": "chat", "peer": ME, "button": buttons[1]["id"]}));
    assert_eq!(phone.next(&mut seen)["text"], "Answered: Cotton. It goes on.");
    let ask = phone.next(&mut seen);
    assert!(ask["text"].as_str().unwrap().contains("reply to this message with your own answer"), "{ask}");
    phone.text(ME, "undyed, please", Some(&format!("m{seen}")));
    assert_eq!(phone.next(&mut seen)["text"], "Answered: undyed, please. It goes on.");
    w.settle();
    assert_eq!(w.model.unused(), 0);
    w.model.check();
}

/// `text_me` is only in runs; there it texts, at most 10 a day per routine, and the rest go
/// out as one message once the day is up. A flood from the phone is cut off.
#[test]
fn text_me_only_in_runs_and_the_caps_hold() {
    let time = Time::new("2026-11-24 07:00");
    let (mut w, phone) = world(&time);
    w.pair(&phone);
    let mut seen = phone.count();
    w.model.answers(|r| {
        assert!(!r.tools().contains(&"text_me".to_owned()), "not in an ordinary chat: {:?}", r.tools());
        Answer::text("Hi.")
    });
    w.send("hello");
    let r = w.save(Routine { when: vec![], ..daily("Pinger", "08:00") });
    let mut calls = Answer::call("text_me", json!({"text": "ping 1"}));
    for i in 2..=DAILY_CAP + 2 {
        calls = calls.and("text_me", json!({"text": format!("ping {i}")}));
    }
    w.model.answers(move |r| {
        assert!(r.tools().contains(&"text_me".to_owned()), "{:?}", r.tools());
        calls
    });
    w.model.answers(|r| {
        let results = r.tool_results();
        assert_eq!(results.iter().filter(|t| t.starts_with("Sent to the user's phone")).count(), DAILY_CAP, "{results:?}");
        assert!(results.last().unwrap().starts_with("Not sent yet"), "{results:?}");
        Answer::text("Pinged.")
    });
    w.tell(ClientMsg::RunRoutine { id: r.id.clone() });
    assert_eq!(w.finished(&r.id)[0].status, RunStatus::Quiet);
    w.model.check();
    for i in 1..=DAILY_CAP {
        assert_eq!(phone.next(&mut seen)["text"], format!("ping {i}\n\n— Pinger"));
    }
    w.settle();
    assert_eq!(phone.count(), seen, "the rest are held");
    time.set("2026-11-25 07:01");
    let batch = phone.next(&mut seen);
    let text = batch["text"].as_str().unwrap();
    assert!(text.starts_with("2 more from Pinger") && text.contains("• ping 11\n• ping 12"), "{text}");

    // Twelve messages in a burst from the phone: ten get through (each answered with help).
    for _ in 0..MAX_INBOUND_PER_MINUTE + 2 {
        phone.text(ME, "/help", None);
    }
    w.settle();
    assert_eq!(phone.count() - seen, MAX_INBOUND_PER_MINUTE);
    assert_eq!(w.model.unused(), 0);
    w.model.check();
}

/// The real Telegram backend against a fake Bot API: the token set as the UI sets it, the
/// phone paired with the deep link's `/start CODE`, a run's approval sent with an inline
/// keyboard and answered with a button press, a stranger's message going nowhere.
#[test]
fn telegram_end_to_end_against_a_fake_bot_api() {
    use byob_telegram::fake::{self, Next};
    let api = fake::Server::start();
    let time = Time::new("2026-11-24 07:00");
    let (t, base) = (time.clone(), api.base.clone());
    let mut w = World::with(move |c| {
        c.clock = t.clock();
        let base = base.clone();
        let make: NativeFactory = Arc::new(move || Box::new(byob_telegram::Telegram::with_api(&base, 1)) as Box<dyn NativeBackend>);
        if let Some(p) = c.plugins.as_mut() {
            p.natives = std::mem::take(&mut p.natives).backend(byob_telegram::ID, make);
        }
    });
    // Off and without a token, it says what to do; with one (as Settings → My phone sends it), it connects.
    let state = w.phone_where(|s| s.channels.iter().any(|c| c.plugin == "telegram"));
    let tg = state.channels.iter().find(|c| c.plugin == "telegram").unwrap();
    assert!(!tg.enabled && !tg.configured && tg.setting.as_deref() == Some("BOT_TOKEN"), "{tg:?}");
    w.tell(ClientMsg::SetPluginSetting { plugin: "telegram".into(), key: "BOT_TOKEN".into(), value: Some(fake::TOKEN.into()) });
    w.tell(ClientMsg::SetPluginEnabled { scope: byob_protocol::ConfigScope::Global, plugin: "telegram".into(), enabled: Some(true) });
    let key = "telegram:telegram";
    w.phone_where(|s| s.channels.iter().any(|c| c.key == key && c.ok && c.status.as_deref() == Some(&format!("Connected as @{}", fake::BOT))));

    w.tell(ClientMsg::PairPhone { channel: key.into() });
    let pairing = w.phone_where(|s| s.pairing.is_some()).pairing.unwrap();
    let code = pairing.code.replace('-', "");
    assert_eq!(pairing.link.as_deref(), Some(format!("https://t.me/{}?start={code}", fake::BOT).as_str()));
    api.push(Next::Updates(vec![fake::text(1, 555, &format!("/start {code}"))]));
    let state = w.phone_where(|s| s.phone.is_some());
    assert_eq!(state.phone.unwrap().name, "@ada");
    let sent_to = |api: &fake::Server, who: i64| api.calls("sendMessage").into_iter().filter(|b| b["chat_id"] == json!(who.to_string())).collect::<Vec<_>>();
    let wait_for = |api: &fake::Server, who: i64, n: usize| {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let got = sent_to(api, who);
            if got.len() >= n {
                return got;
            }
            assert!(Instant::now() < deadline, "only {} messages to {who}", got.len());
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    let hello = wait_for(&api, 555, 1);
    assert!(hello[0]["text"].as_str().unwrap().starts_with("Paired with byob"));
    assert!(hello[0].get("parse_mode").is_none(), "plain text");

    // A stranger: nothing.
    api.push(Next::Updates(vec![fake::text(2, 777, "hi bot")]));
    // A run waiting on an approval: Allow / Deny on the phone; Allow is pressed.
    let r = w.save(Routine { when: vec![], channels: vec![CHANNEL_PHONE.into()], ..daily("Notes", "08:00") });
    w.model.calls("write_file", json!({"path": "found.md", "content": "a loom"}));
    w.model.says("Saved.");
    w.tell(ClientMsg::RunRoutine { id: r.id.clone() });
    let ask = wait_for(&api, 555, 2).pop().unwrap();
    let keyboard = &ask["reply_markup"]["inline_keyboard"][0];
    assert_eq!((keyboard[0]["text"].as_str(), keyboard[1]["text"].as_str()), (Some("✅ Allow"), Some("❌ Deny")), "{ask}");
    assert_eq!(ask["reply_markup"]["inline_keyboard"][1][0]["text"], "🔓 Always allow for this routine", "on a row of its own: {ask}");
    let allow = keyboard[0]["callback_data"].as_str().unwrap().to_owned();
    api.push(Next::Updates(vec![json!({"update_id": 3, "callback_query": {"id": "cb1", "from": {"id": 555, "is_bot": false, "username": "ada"},
        "message": {"message_id": 101, "chat": {"id": 555, "type": "private"}}, "data": allow}})]));
    assert_eq!(w.finished(&r.id)[0].status, RunStatus::Quiet);
    assert!(!api.calls("answerCallbackQuery").is_empty(), "the button's spinner stops");
    assert!(sent_to(&api, 777).is_empty(), "strangers get nothing");
    let all = api.state.lock().unwrap().calls.iter().map(|(m, b)| format!("{m} {b}")).collect::<Vec<_>>().join("\n");
    assert!(!all.contains(fake::TOKEN), "the token goes only in the URL");
}

/// A one-way channel (a push service: ntfy): nothing to pair, so choosing it makes it My
/// phone at once; reports arrive without "reply"; an approval says to open byob, no buttons.
#[test]
fn a_one_way_channel_gets_texts_and_no_buttons() {
    let time = Time::new("2026-11-24 07:00");
    let phone = Phone::default();
    let (t, p) = (time.clone(), phone.clone());
    let mut w = World::with(move |c| {
        c.clock = t.clock();
        if let Some(plugins) = c.plugins.as_mut() {
            let manifest =
                json!({"id": "fakepush", "name": "Fake push", "backend": {"native": "fakepush"}, "channels": [{"id": "push", "title": "Fake push"}]});
            plugins.bundled.push(Embedded { id: "fakepush".into(), files: [("byob-plugin.json".to_owned(), manifest.to_string())].into_iter().collect() });
            plugins.natives = std::mem::take(&mut plugins.natives).backend("fakepush", p.factory());
        }
    });
    w.phone_where(|s| s.channels.iter().any(|c| c.key == "fakepush:push" && !c.two_way));
    w.tell(ClientMsg::PairPhone { channel: "fakepush:push".into() });
    let state = w.phone_where(|s| s.phone.is_some());
    assert_eq!(state.phone.unwrap().channel, "fakepush:push");
    assert!(phone.codes.lock().unwrap().is_empty(), "nothing to pair");
    let mut seen = 0;
    assert!(phone.next(&mut seen)["text"].as_str().unwrap().starts_with("byob will send you messages here"));
    let r = w.save(Routine { when: vec![], channels: vec![CHANNEL_PHONE.into()], ..daily("Notes", "08:00") });
    w.model.calls("report", json!({"title": "A loom", "body": "Cheap."}));
    w.model.calls("write_file", json!({"path": "found.md", "content": "a loom"}));
    w.model.says("Saved.");
    w.tell(ClientMsg::RunRoutine { id: r.id.clone() });
    let ask = phone.next(&mut seen);
    assert!(ask["text"].as_str().unwrap().contains("Open byob to answer") && ask.get("buttons").is_none(), "{ask}");
    // Answered in byob, the run reports.
    let chat = w.runs(&r.id)[0].chat.clone().unwrap();
    w.open_chat(&chat);
    let card = w.seen.entries.iter().flat_map(|e| &e.steps).find(|s| s.tool == "write_file" && s.status == StepStatus::Waiting).map(|s| s.id).unwrap();
    w.tell(ClientMsg::Approve { step: card, answer: byob_protocol::ApprovalAnswer::Allow });
    assert_eq!(w.finished(&r.id)[0].status, RunStatus::Reported);
    let report = phone.next(&mut seen);
    assert_eq!(report["text"], "A loom\n\nCheap.\n\n— Notes (the details are in byob)");
    // Inbound on a one-way channel is ignored.
    phone.text("me", "hello", None);
    w.settle();
    assert_eq!(phone.count(), seen);
}
