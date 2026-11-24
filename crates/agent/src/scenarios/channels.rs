//! Channels, end to end: a fake two-way channel plugin (a native backend that records what
//! byob asks it to send, and through which the scenario plays the person's phone) pairs with
//! a code, and then a routine's report reaches the phone, a reply continues the run's chat,
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

    // Anything that isn't a reply gets what the phone can do; nothing starts.
    phone.text(ME, "research spinning wheels", None);
    let help = phone.next(&mut seen);
    assert!(help["text"].as_str().unwrap().contains("Starting something new from your phone isn't possible yet"), "{help}");
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
    assert_eq!(buttons.iter().map(|b| b["label"].as_str().unwrap()).collect::<Vec<_>>(), ["Allow", "Deny"]);
    let allow = buttons[0]["id"].as_str().unwrap().to_owned();
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

    // Must-confirm: creating a routine from a run.
    w.model.calls("create_routine", json!({"name": "Another", "instructions": "Do more."}));
    w.model.says("Couldn't.");
    w.tell(ClientMsg::RunRoutine { id: r.id.clone() });
    let ask = phone.next(&mut seen);
    let text = ask["text"].as_str().unwrap();
    assert!(text.contains("open byob to confirm"), "{text}");
    assert!(ask.get("buttons").is_none(), "no tap for a must-confirm call: {ask}");
    // The person denies it in byob; the run ends.
    let chat = w.runs(&r.id)[0].chat.clone().unwrap();
    w.open_chat(&chat);
    let card = w.seen.entries.iter().flat_map(|e| &e.steps).find(|s| s.tool == "create_routine" && s.status == StepStatus::Waiting).map(|s| s.id).unwrap();
    w.tell(ClientMsg::Approve { step: card, answer: byob_protocol::ApprovalAnswer::Deny });
    assert_eq!(w.finished(&r.id)[0].status, RunStatus::Quiet);
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
    for i in 0..MAX_INBOUND_PER_MINUTE + 2 {
        phone.text(ME, &format!("hey {i}"), None);
    }
    w.settle();
    assert_eq!(phone.count() - seen, MAX_INBOUND_PER_MINUTE);
    assert_eq!(w.model.unused(), 0);
    w.model.check();
}
