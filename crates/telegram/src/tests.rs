//! The backend against the fake Bot API (`fake`): nothing here reaches the real Telegram.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use byob_plugins::native::{NativeBackend, Outbox};
use serde_json::{Value, json};

use crate::fake::{BOT, Next, Server, TOKEN, text};
use crate::{MAX_PER_MINUTE, MAX_TEXT, Telegram, as_code};

struct H {
    server: Server,
    tg: Telegram,
    out: Outbox,
    seen: Arc<Mutex<Vec<Value>>>,
}

fn start_with(token: &str) -> H {
    let server = Server::start();
    let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
    let s = Arc::clone(&seen);
    let out = Outbox::new(move |m| s.lock().unwrap().push(m));
    let mut tg = Telegram::with_api(&server.base, 1);
    tg.init(&json!({"type": "init", "secrets": {"BOT_TOKEN": token}}), &out);
    H { server, tg, out, seen }
}

fn start() -> H {
    start_with(TOKEN)
}

impl H {
    /// Wait until the messages so far satisfy `f`.
    fn until(&self, what: &str, f: impl Fn(&[Value]) -> bool) -> Vec<Value> {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            let seen = self.seen.lock().unwrap().clone();
            if f(&seen) {
                return seen;
            }
            assert!(Instant::now() < end, "timed out waiting for {what}: {seen:#?}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn of(&self, kind: &str) -> Vec<Value> {
        self.seen.lock().unwrap().iter().filter(|m| m["type"] == kind).cloned().collect()
    }

    fn peers(&mut self, peers: &[&str]) {
        self.tg.event(&json!({"type": "event", "event": "channel_peers", "channel": "telegram", "peers": peers}), &self.out);
    }

    /// Let the poller take what was queued (and a little more).
    fn drain(&self) {
        let end = Instant::now() + Duration::from_secs(10);
        while !self.server.state.lock().unwrap().updates.is_empty() {
            assert!(Instant::now() < end, "the poller didn't take its updates");
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

impl Drop for H {
    fn drop(&mut self) {
        self.tg.shutdown();
    }
}

fn kinds(seen: &[Value], kind: &str) -> usize {
    seen.iter().filter(|m| m["type"] == kind).count()
}

#[test]
fn codes_are_read_the_way_people_type_them() {
    assert_eq!(as_code(" abcd-efgh ").as_deref(), Some("ABCDEFGH"));
    assert_eq!(as_code("ABCD EF23").as_deref(), Some("ABCDEF23"));
    assert_eq!(as_code("hello"), None);
    assert_eq!(as_code("ABCDEFG1"), None, "1 and 0 are never in a code");
    assert_eq!(as_code("hello there friend"), None);
}

#[test]
fn without_a_token_it_says_how_to_set_one_up() {
    let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
    let s = Arc::clone(&seen);
    let out = Outbox::new(move |m| s.lock().unwrap().push(m));
    let mut tg = Telegram::with_api("http://127.0.0.1:9", 1);
    tg.init(&json!({"type": "init", "secrets": {}}), &out);
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!((seen[0]["type"].as_str(), seen[0]["ok"].as_bool()), (Some("channel_status"), Some(false)));
    assert!(seen[0]["text"].as_str().unwrap().contains("@BotFather"));
    assert!(tg.request("deliver", &json!({"to": "1", "text": "hi"}), &out).unwrap_err().contains("@BotFather"));
}

#[test]
fn private_texts_are_passed_on_once_and_the_offset_moves_on() {
    let h = start();
    h.until("connected", |s| s.iter().any(|m| m["type"] == "channel_status" && m["ok"] == true));
    let status = h.of("channel_status");
    assert_eq!(status[0]["text"], format!("Connected as @{BOT}"));
    let mut reply = text(11, 777, "yes, buy it");
    reply["message"]["reply_to_message"] = json!({"message_id": 55});
    let group = json!({"update_id": 12, "message": {"message_id": 3, "from": {"id": 778, "is_bot": false, "first_name": "G"},
        "chat": {"id": -100, "type": "group"}, "text": "hello group"}});
    let bot = json!({"update_id": 13, "message": {"message_id": 4, "from": {"id": 779, "is_bot": true, "first_name": "B"},
        "chat": {"id": 779, "type": "private"}, "text": "beep"}});
    let sticker = json!({"update_id": 14, "message": {"message_id": 5, "from": {"id": 777, "is_bot": false, "first_name": "Ada"},
        "chat": {"id": 777, "type": "private"}, "sticker": {}}});
    h.server.push(Next::Updates(vec![text(10, 777, &"x".repeat(MAX_TEXT + 50)), reply, group, bot, sticker]));
    h.drain();
    let inbound = h.of("inbound");
    assert_eq!(inbound.len(), 2, "{inbound:#?}");
    assert_eq!((inbound[0]["peer"].as_str(), inbound[0]["peer_name"].as_str()), (Some("777"), Some("@ada")));
    assert_eq!(inbound[0]["text"].as_str().unwrap().chars().count(), MAX_TEXT);
    assert_eq!((inbound[1]["text"].as_str(), inbound[1]["reply_to"].as_str()), (Some("yes, buy it"), Some("55")));
    let polls = h.server.calls("getUpdates");
    assert!(polls.iter().any(|b| b["offset"] == 15), "the next poll asks for what comes after: {polls:?}");
    assert!(polls.last().unwrap()["allowed_updates"] == json!(["message", "callback_query"]));
    assert_eq!(h.of("channel_status").len(), 1, "status is sent when it changes, not every poll");
    let menu = h.server.calls("setMyCommands");
    assert_eq!(menu.len(), 1, "once, on connecting");
    assert_eq!(menu[0]["commands"][0], json!({"command": "new", "description": "Start a new chat"}));
}

#[test]
fn pairing_codes_go_to_byob_and_unknown_senders_are_dropped_once_peers_are_known() {
    let mut h = start();
    h.peers(&["777"]);
    h.server.push(Next::Updates(vec![
        text(1, 900, "/start ABCD2345"),
        text(2, 901, "abcd-2345"),
        text(3, 902, "hi, who are you?"),
        text(4, 903, "/start"),
        text(5, 777, "ABCDEFGH"),
    ]));
    h.drain();
    let claims = h.of("pair_claim");
    assert_eq!(claims.len(), 2, "{claims:#?}");
    assert_eq!((claims[0]["peer"].as_str(), claims[0]["code"].as_str()), (Some("900"), Some("ABCD2345")));
    assert_eq!((claims[1]["peer"].as_str(), claims[1]["code"].as_str()), (Some("901"), Some("ABCD2345")));
    let inbound = h.of("inbound");
    assert_eq!(inbound.len(), 1, "only the paired sender's message: {inbound:#?}");
    assert_eq!((inbound[0]["peer"].as_str(), inbound[0]["text"].as_str()), (Some("777"), Some("ABCDEFGH")), "a paired person's word is a message");
    assert!(h.server.calls("sendMessage").is_empty(), "nobody unknown gets an answer");
    assert!(h.of("log").iter().any(|l| l["text"].as_str().unwrap().contains("aren't paired")));
}

#[test]
fn button_presses_are_passed_on_and_answered() {
    let mut h = start();
    h.peers(&["777"]);
    let press = |update: i64, from: i64| {
        json!({"update_id": update, "callback_query": {"id": format!("cb{update}"), "from": {"id": from, "is_bot": false, "first_name": "Ada"},
            "message": {"message_id": 101, "chat": {"id": from, "type": "private"}}, "data": "a7:yes"}})
    };
    h.server.push(Next::Updates(vec![press(1, 777), press(2, 666)]));
    h.drain();
    let inbound = h.of("inbound");
    assert_eq!(inbound.len(), 1);
    assert_eq!((inbound[0]["button"].as_str(), inbound[0]["reply_to"].as_str(), inbound[0]["peer_name"].as_str()), (Some("a7:yes"), Some("101"), Some("Ada")));
    assert!(inbound[0].get("text").is_none());
    let answered = h.server.calls("answerCallbackQuery");
    assert_eq!(answered, [json!({"callback_query_id": "cb1"})]);
}

#[test]
fn deliver_sends_plain_text_with_buttons_and_says_which_message() {
    let mut h = start();
    let out = h.out.clone();
    let r =
        h.tg.request(
            "deliver",
            &json!({"channel": "telegram", "to": "777", "text": format!("Loom deals *wants* to [send] _mail_ {}", "y".repeat(MAX_TEXT)),
                "buttons": [{"id": "a1:yes", "label": "Yes"}, {"id": "a1:no", "label": "No"}], "reply_to": "55"}),
            &out,
        )
        .unwrap();
    assert_eq!(r, json!({"ref": "101"}));
    let sent = h.server.calls("sendMessage");
    let body = &sent[0];
    assert!(body.get("parse_mode").is_none(), "plain text: nothing to escape");
    assert_eq!(body["chat_id"], "777");
    assert!(body["text"].as_str().unwrap().starts_with("Loom deals *wants* to [send] _mail_"));
    assert_eq!(body["text"].as_str().unwrap().chars().count(), MAX_TEXT);
    assert_eq!(body["reply_markup"]["inline_keyboard"], json!([[{"text": "Yes", "callback_data": "a1:yes"}, {"text": "No", "callback_data": "a1:no"}]]));
    assert_eq!(body["reply_parameters"], json!({"message_id": 55, "allow_sending_without_reply": true}));
    let long = "z".repeat(65);
    assert!(h.tg.request("deliver", &json!({"to": "777", "text": "x", "buttons": [{"id": long, "label": "Yes"}]}), &out).is_err());
    assert!(h.tg.request("deliver", &json!({"to": "../etc", "text": "x"}), &out).is_err());
}

#[test]
fn a_rate_limited_send_waits_and_tries_again() {
    let mut h = start();
    h.server
        .state
        .lock()
        .unwrap()
        .send_errors
        .push_back((429, json!({"ok": false, "error_code": 429, "description": "Too Many Requests: retry after 1", "parameters": {"retry_after": 1}})));
    let out = h.out.clone();
    let t = Instant::now();
    let r = h.tg.request("deliver", &json!({"to": "777", "text": "hi"}), &out).unwrap();
    assert!(t.elapsed() >= Duration::from_secs(1), "it waited as Telegram asked");
    assert_eq!(r, json!({"ref": "101"}));
    assert_eq!(h.server.calls("sendMessage").len(), 2);
}

#[test]
fn polling_backs_off_as_long_as_telegram_asks() {
    let h = start();
    h.until("connected", |s| kinds(s, "channel_status") == 1);
    h.server
        .push(Next::Error(429, json!({"ok": false, "error_code": 429, "description": "Too Many Requests: retry after 1", "parameters": {"retry_after": 1}})));
    let asked = Instant::now();
    h.server.push(Next::Updates(vec![text(1, 777, "after the wait")]));
    h.until("the message after the wait", |s| kinds(s, "inbound") == 1);
    assert!(asked.elapsed() >= Duration::from_millis(900), "it waited before polling again");
}

#[test]
fn a_refused_token_is_said_in_words_and_never_shown() {
    let wrong = "999999:not-the-T0KEN-at-all";
    let mut h = start_with(wrong);
    h.until("the refusal", |s| s.iter().any(|m| m["type"] == "channel_status" && m["ok"] == false));
    let status = h.of("channel_status");
    assert!(status[0]["text"].as_str().unwrap().contains("refused the bot token"), "{status:?}");
    let out = h.out.clone();
    let e = h.tg.request("pair_start", &json!({"code": "ABCD2345"}), &out).unwrap_err();
    assert!(e.contains("refused the bot token"), "{e}");
    let e = h.tg.request("deliver", &json!({"to": "1", "text": "x"}), &out).unwrap_err();
    for m in h.seen.lock().unwrap().iter() {
        assert!(!m.to_string().contains(wrong) && !m.to_string().contains("not-the-T0KEN"), "the token leaked: {m}");
    }
    assert!(!e.contains(wrong));
}

#[test]
fn a_webhook_is_removed_so_polling_works() {
    let h = start();
    h.until("connected", |s| kinds(s, "channel_status") == 1);
    h.server.push(Next::Error(409, json!({"ok": false, "error_code": 409, "description": "Conflict: can't use getUpdates method while webhook is active; use deleteWebhook to delete the webhook first"})));
    h.server.push(Next::Updates(vec![text(1, 777, "hello")]));
    h.until("the message", |s| kinds(s, "inbound") == 1);
    assert_eq!(h.server.calls("deleteWebhook").len(), 1);
}

#[test]
fn a_flood_from_one_sender_is_capped() {
    let h = start();
    let many: Vec<Value> = (1..=30).map(|i| text(i, 777, &format!("flood message number {i}"))).collect();
    h.server.push(Next::Updates(many));
    h.push_other();
    h.until("the other sender", |s| s.iter().any(|m| m["peer"] == "888"));
    let inbound = h.of("inbound");
    assert_eq!(inbound.iter().filter(|m| m["peer"] == "777").count(), MAX_PER_MINUTE);
    assert_eq!(inbound.iter().filter(|m| m["peer"] == "888").count(), 1, "others still get through");
}

impl H {
    fn push_other(&self) {
        self.server.push(Next::Updates(vec![text(31, 888, "me too")]));
    }
}

#[test]
fn pair_start_links_to_the_bot() {
    let mut h = start();
    let out = h.out.clone();
    let r = h.tg.request("pair_start", &json!({"channel": "telegram", "code": "ABCD2345"}), &out).unwrap();
    assert_eq!(r["link"], format!("https://t.me/{BOT}?start=ABCD2345"));
    assert_eq!(r["name"], format!("@{BOT}"));
    assert!(r["instructions"].as_str().unwrap().contains("ABCD2345"));
    assert!(h.tg.request("pair_start", &json!({"code": "a b"}), &out).is_err());
}

#[test]
fn shutdown_stops_the_poller() {
    let mut h = start();
    h.until("connected", |s| kinds(s, "channel_status") == 1);
    h.tg.shutdown();
    // the poll in flight ends within its wait
    std::thread::sleep(Duration::from_millis(800));
    let n = h.server.calls("getUpdates").len();
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(h.server.calls("getUpdates").len(), n, "no more polls after shutdown");
}

#[test]
fn unpair_drops_a_peer_from_the_filter() {
    let mut h = start();
    h.peers(&["777"]);
    let out = h.out.clone();
    h.tg.request("unpair", &json!({"channel": "telegram", "peer": "777"}), &out).unwrap();
    h.server.push(Next::Updates(vec![text(1, 777, "still there?")]));
    h.drain();
    assert!(h.of("inbound").is_empty());
}
