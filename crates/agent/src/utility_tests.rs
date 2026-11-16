//! Engine tests of the `utility` model role and per-profile request options: titles and
//! compaction summaries go to the utility profile (the chat's model once if it fails), a
//! profile's params reach its chats' requests and nobody else's, and the engine refuses
//! extra request JSON that would set byob's own fields.

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use byob_protocol::profiles::{Params, Profile};
use byob_protocol::{ClientMsg, Connection, NoticeLevel, ServerMsg};
use byob_types::InferenceConfig;
use serde_json::{Value, json};

use crate::compact::SYSTEM as SUMMARY_SYSTEM;
use crate::loop_tests::read_request;
use crate::{CompactOptions, EngineConfig, EngineHandle, LocalConnection};

type Seen = Arc<Mutex<Vec<Value>>>;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Title,
    Summary,
    Chat,
}

fn kind(body: &Value) -> Kind {
    let system = body["messages"][0]["content"].as_str().unwrap_or_default();
    if system == SUMMARY_SYSTEM {
        Kind::Summary
    } else if system.starts_with("You name conversations") {
        Kind::Title
    } else {
        Kind::Chat
    }
}

/// A fake server called `name`: chat requests get "answer", titles "<name> title",
/// summaries "SUMMARY by <name>"; with `fail`, titles and summaries get HTTP 500.
fn server(name: &'static str, fail: bool) -> (String, Seen) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Seen = Arc::default();
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let body = read_request(&mut s);
            let k = kind(&body);
            log.lock().unwrap().push(body);
            if fail && k != Kind::Chat {
                let body = r#"{"error":{"message":"utility model is down"}}"#;
                let _ = write!(s, "HTTP/1.1 500 X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                continue;
            }
            let text = match k {
                Kind::Title => format!("{name} title"),
                Kind::Summary => format!("## User goals\nSUMMARY by {name}"),
                Kind::Chat => "answer".into(),
            };
            let frame = |v: Value| format!("data: {v}\n\n");
            let out = frame(json!({"choices":[{"delta":{"content": text}}]}))
                + &frame(json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":7}}));
            let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{out}data: [DONE]\n\n");
        }
    });
    (format!("http://{addr}/v1"), seen)
}

fn of(seen: &Seen, k: Kind) -> Vec<Value> {
    seen.lock().unwrap().iter().filter(|b| kind(b) == k).cloned().collect()
}

fn profile(id: &str, url: &str, model: &str, params: Params) -> Profile {
    Profile { id: id.into(), name: id.into(), base_url: url.into(), model: model.into(), params, ..Profile::default() }
}

/// An engine whose default model is profile "chat" (with `params`) at `chat_url`, and whose
/// utility role is profile "util" at `util_url`.
fn engine(chat_url: &str, util_url: &str, params: Params, compaction: CompactOptions) -> (EngineHandle, tempfile::TempDir) {
    let ws = tempfile::tempdir().unwrap();
    let mut config = EngineConfig::new(InferenceConfig { base_url: chat_url.into(), model: "m".into(), ..Default::default() });
    config.workspace = ws.path().to_path_buf();
    config.auto_title = true;
    config.compaction = compaction;
    config.profiles =
        [("chat".to_owned(), profile("chat", chat_url, "m", params)), ("util".to_owned(), profile("util", util_url, "small", Params::default()))].into();
    config.roles = [("main".to_owned(), "chat".to_owned()), ("utility".to_owned(), "util".to_owned())].into();
    (EngineHandle::spawn_with(config).unwrap(), ws)
}

fn recv(c: &mut LocalConnection) -> ServerMsg {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(m) = c.try_recv() {
            return m;
        }
        assert!(Instant::now() < deadline, "timed out waiting for the engine");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn connect(e: &EngineHandle) -> LocalConnection {
    let mut c = e.connect(Arc::new(|| {}));
    c.send(ClientMsg::hello("test"));
    while !matches!(recv(&mut c), ServerMsg::Artifacts { .. }) {}
    c
}

/// Send `text` and wait for its reply; the chat's title, if it changed meanwhile, is kept.
fn turn(c: &mut LocalConnection, text: &str, title: &mut Option<String>) {
    c.send(ClientMsg::SendMessage { text: text.into(), attachments: vec![] });
    loop {
        match recv(c) {
            ServerMsg::EntryFinished { error, .. } => {
                assert_eq!(error, None);
                return;
            }
            ServerMsg::ChatUpdated(s) => *title = Some(s.title),
            _ => {}
        }
    }
}

fn wait_title(c: &mut LocalConnection, title: &mut Option<String>, want: &str) {
    while title.as_deref() != Some(want) {
        if let ServerMsg::ChatUpdated(s) = recv(c) {
            *title = Some(s.title);
        }
    }
}

/// ~150 estimated tokens.
fn question(i: usize) -> String {
    format!("question {i} {}", "word ".repeat(100))
}

#[test]
fn titles_and_summaries_go_to_the_utility_model_and_params_only_to_the_chats_own() {
    let (chat_url, chat) = server("chat", false);
    let (util_url, util) = server("util", false);
    let extra = json!({"chat_template_kwargs": {"enable_thinking": false}});
    let params = Params { temperature: Some(0.4), extra: extra.as_object().cloned().unwrap(), ..Params::default() };
    let (e, _ws) = engine(&chat_url, &util_url, params, CompactOptions::default());
    let mut c = connect(&e);
    let mut title = None;
    turn(&mut c, &question(1), &mut title);
    wait_title(&mut c, &mut title, "util title");
    turn(&mut c, &question(2), &mut title);
    c.send(ClientMsg::Compact { focus: None });
    while !matches!(recv(&mut c), ServerMsg::Compacted(_)) {}
    turn(&mut c, "third", &mut title);

    assert_eq!((of(&util, Kind::Title).len(), of(&util, Kind::Summary).len()), (1, 1));
    assert!(of(&chat, Kind::Title).is_empty() && of(&chat, Kind::Summary).is_empty());
    let asked = &of(&util, Kind::Summary)[0];
    assert_eq!(asked["model"], "small");
    // The chat's model got the summary written by the utility model.
    let last = of(&chat, Kind::Chat).pop().unwrap();
    assert!(last.to_string().contains("SUMMARY by util"), "{last}");
    // Every chat request carries the chat profile's params; the utility model's don't.
    for b in of(&chat, Kind::Chat) {
        assert_eq!((b["temperature"].as_f64(), &b["chat_template_kwargs"]), (Some(0.4), &json!({"enable_thinking": false})), "{b}");
    }
    for b in util.lock().unwrap().iter() {
        assert!(b.get("temperature").is_none() && b.get("chat_template_kwargs").is_none(), "{b}");
    }
}

#[test]
fn a_failing_utility_model_falls_back_to_the_chats_model_once() {
    let (chat_url, chat) = server("chat", false);
    let (util_url, util) = server("util", true);
    let compaction = CompactOptions { at_tokens: Some(700), keep_turns: 1, ..Default::default() };
    let (e, _ws) = engine(&chat_url, &util_url, Params::default(), compaction);
    let mut c = connect(&e);
    let mut title = None;
    for i in 1..=5 {
        turn(&mut c, &question(i), &mut title);
    }
    wait_title(&mut c, &mut title, "chat title");
    // One try each on the utility model, then one on the chat's.
    assert_eq!((of(&util, Kind::Title).len(), of(&chat, Kind::Title).len()), (1, 1));
    let (u, s) = (of(&util, Kind::Summary).len(), of(&chat, Kind::Summary).len());
    assert!(u >= 1 && u == s, "utility {u}, chat {s}");
    let last = of(&chat, Kind::Chat).pop().unwrap();
    assert!(last.to_string().contains("SUMMARY by chat"), "{last}");
}

#[test]
fn the_engine_refuses_extra_json_that_sets_byobs_fields() {
    let (chat_url, _chat) = server("chat", false);
    let (e, _ws) = engine(&chat_url, "http://127.0.0.1:9/v1", Params::default(), CompactOptions::default());
    let mut c = connect(&e);
    let mut p = profile("chat", &chat_url, "m", Params::default());
    p.params.extra.insert("messages".into(), json!([]));
    c.send(ClientMsg::UpdateProfile { profile: p, api_key: None });
    let text = loop {
        match recv(&mut c) {
            ServerMsg::Notice { level: NoticeLevel::Error, text } => break text,
            ServerMsg::Profiles(l) => assert!(l.get("chat").unwrap().profile.params.extra.is_empty(), "saved anyway"),
            _ => {}
        }
    };
    assert!(text.contains("messages"), "{text}");
}
