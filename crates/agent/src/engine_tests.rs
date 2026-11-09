//! Engine tests: clients, chats, persistence and routing, against fake inference servers.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

use yah_protocol::{ChatSummary, ClientMsg, Connection, Entry, NoticeLevel, PROTOCOL_VERSION, ServerMsg};
use yah_store::{NewMessage, Paths, Store};
use yah_types::InferenceConfig;

use crate::{EngineConfig, EngineHandle, LocalConnection, chats};

fn recv(conn: &mut LocalConnection) -> ServerMsg {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(m) = conn.try_recv() {
            return m;
        }
        assert!(Instant::now() < deadline, "timed out waiting for engine");
        thread::sleep(Duration::from_millis(2));
    }
}

/// Wait for a message matching `f`, returning it and everything skipped on the way.
fn recv_until(conn: &mut LocalConnection, f: impl Fn(&ServerMsg) -> bool) -> (ServerMsg, Vec<ServerMsg>) {
    let mut skipped = Vec::new();
    loop {
        let m = recv(conn);
        if f(&m) {
            return (m, skipped);
        }
        skipped.push(m);
    }
}

/// Everything queued right now (after letting the engine catch up).
fn drain(conn: &mut LocalConnection) -> Vec<ServerMsg> {
    thread::sleep(Duration::from_millis(100));
    std::iter::from_fn(|| conn.try_recv()).collect()
}

struct Hello {
    state: yah_protocol::EngineState,
    chats: Vec<ChatSummary>,
    chat: Option<ChatSummary>,
    entries: Vec<Entry>,
}

/// Say hello and collect the answer: chat list, State, ChatOpened, Transcript, Artifacts.
fn hello(conn: &mut LocalConnection) -> Hello {
    conn.send(ClientMsg::Hello { protocol: PROTOCOL_VERSION, client: "test".into() });
    let (mut state, mut chats, mut chat, mut transcript) = (None, Vec::new(), None, None);
    loop {
        match recv(conn) {
            ServerMsg::State(s) => state = Some(s),
            ServerMsg::Chats { pinned, recent, .. } => chats = pinned.into_iter().chain(recent).collect(),
            ServerMsg::ChatOpened { chat: c } => chat = c,
            ServerMsg::Transcript { entries } => transcript = Some(entries),
            ServerMsg::Artifacts { .. } if transcript.is_some() => {
                return Hello { state: state.unwrap(), chats, chat, entries: transcript.unwrap_or_default() };
            }
            _ => {} // e.g. updates about chats from before the hello
        }
    }
}

/// Wait for the transcript after NewChat/OpenChat.
fn opened(conn: &mut LocalConnection) -> (Option<ChatSummary>, Vec<Entry>) {
    let (ServerMsg::ChatOpened { chat }, _) = recv_until(conn, |m| matches!(m, ServerMsg::ChatOpened { .. })) else { unreachable!() };
    let (ServerMsg::Transcript { entries }, _) = recv_until(conn, |m| matches!(m, ServerMsg::Transcript { .. })) else { unreachable!() };
    recv_until(conn, |m| matches!(m, ServerMsg::Artifacts { .. }));
    (chat, entries)
}

fn finish(conn: &mut LocalConnection) -> (Option<String>, Vec<ServerMsg>) {
    let (ServerMsg::EntryFinished { error, .. }, skipped) = recv_until(conn, |m| matches!(m, ServerMsg::EntryFinished { .. })) else { unreachable!() };
    (error, skipped)
}

fn text_of(msgs: &[ServerMsg]) -> String {
    msgs.iter().filter_map(|m| if let ServerMsg::EntryDelta { content, .. } = m { Some(content.as_str()) } else { None }).collect()
}

fn respond(s: &mut std::net::TcpStream, chunks: &str) {
    let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{chunks}");
}

fn sse_words(text: &str) -> String {
    text.split_inclusive(' ').map(|w| format!("data: {}\n\n", serde_json::json!({"choices":[{"delta":{"content": w}}]}))).collect()
}

/// Minimal OpenAI-style server that streams `reply` to every request.
fn fake_llm(reply: &'static str) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let mut s = stream.unwrap();
            let mut buf = [0u8; 65536];
            let _ = s.read(&mut buf);
            respond(&mut s, &format!("{}data: [DONE]\n\n", sse_words(reply)));
        }
    });
    format!("http://{addr}/v1")
}

/// A server that streams "first " at once, then waits for a signal before "second".
fn gated_llm() -> (String, Sender<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (release, gate): (Sender<()>, Receiver<()>) = channel();
    let gate = Arc::new(std::sync::Mutex::new(gate));
    thread::spawn(move || {
        for stream in listener.incoming() {
            let gate = Arc::clone(&gate);
            thread::spawn(move || {
                let mut s = stream.unwrap();
                let mut buf = [0u8; 65536];
                let _ = s.read(&mut buf);
                respond(&mut s, &sse_words("first "));
                let _ = s.flush();
                let _ = gate.lock().unwrap().recv_timeout(Duration::from_secs(10));
                let _ = write!(s, "{}data: [DONE]\n\n", sse_words("second"));
            });
        }
    });
    (format!("http://{addr}/v1"), release)
}

fn config(url: String, paths: Option<Paths>) -> EngineConfig {
    let mut c = EngineConfig::new(InferenceConfig { base_url: url, ..Default::default() });
    c.paths = paths;
    c.auto_title = false;
    c
}

fn engine(url: String) -> EngineHandle {
    EngineHandle::spawn_with(config(url, None)).unwrap()
}

#[test]
fn turn_streams_and_late_clients_get_the_chat() {
    let engine = engine(fake_llm("hello there friend"));
    let mut desktop = engine.connect(Arc::new(|| {}));
    let first = hello(&mut desktop);
    assert!(first.chat.is_none() && first.entries.is_empty() && first.chats.is_empty());
    desktop.send(ClientMsg::SendMessage { text: "hi\nmore".into(), attachments: vec![] });
    let (error, msgs) = finish(&mut desktop);
    assert_eq!(error, None);
    assert_eq!(text_of(&msgs), "hello there friend");

    // A client that connects later opens the most recent chat, with the whole conversation.
    let mut late = engine.connect(Arc::new(|| {}));
    let h = hello(&mut late);
    assert!(!h.state.busy);
    assert_eq!(h.chats.len(), 1);
    assert_eq!(h.chat.as_ref().map(|c| c.title.as_str()), Some("hi"));
    assert_eq!(h.entries.len(), 2);
    assert_eq!(h.entries[1].content, "hello there friend");
    assert!(h.entries[1].done);
}

#[test]
fn unreachable_endpoint_finishes_with_error() {
    let engine = engine("http://127.0.0.1:9/v1".into());
    let mut c = engine.connect(Arc::new(|| {}));
    hello(&mut c);
    c.send(ClientMsg::SendMessage { text: "hi".into(), attachments: vec![] });
    assert!(finish(&mut c).0.is_some());
}

#[test]
fn hello_returns_state_without_secrets() {
    let engine = EngineHandle::spawn(InferenceConfig { api_key: Some("sk-secret".into()), ..Default::default() }).unwrap();
    let mut c = engine.connect(Arc::new(|| {}));
    let state = hello(&mut c).state;
    assert!(state.has_api_key);
    assert!(!serde_json::to_string(&state).unwrap().contains("sk-secret"));
}

#[test]
fn inference_change_reaches_every_client_and_is_saved() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::at(dir.path());
    let engine = EngineHandle::spawn_with(config("http://127.0.0.1:9/v1".into(), Some(paths.clone()))).unwrap();
    let mut desktop = engine.connect(Arc::new(|| {}));
    let mut browser = engine.connect(Arc::new(|| {}));
    hello(&mut desktop);
    hello(&mut browser);
    desktop.send(ClientMsg::SetInference { base_url: "http://gpu-box:8000/v1".into(), model: "qwen".into(), api_key: Some("sk-new".into()) });
    let ServerMsg::State(state) = recv(&mut browser) else { panic!("expected state") };
    assert_eq!(state.endpoint.as_deref(), Some("http://gpu-box:8000/v1/chat/completions"));
    let settings = yah_store::Settings::load(&paths.settings()).value;
    assert_eq!(settings.inference.base_url.as_deref(), Some("http://gpu-box:8000/v1"));
    assert_eq!(settings.inference.model.as_deref(), Some("qwen"));
    // The key goes to secrets.json only.
    assert!(!std::fs::read_to_string(paths.settings()).unwrap().contains("sk-new"));
    assert_eq!(yah_store::Secrets::load(&paths.secrets()).value.api_key.as_deref(), Some("sk-new"));
}

#[test]
fn bad_url_only_errors_the_sender() {
    let engine = EngineHandle::spawn(InferenceConfig::default()).unwrap();
    let mut a = engine.connect(Arc::new(|| {}));
    hello(&mut a);
    a.send(ClientMsg::SetInference { base_url: "not a url".into(), model: "m".into(), api_key: None });
    assert!(matches!(recv(&mut a), ServerMsg::Notice { level: NoticeLevel::Error, .. }));
}

#[test]
fn protocol_mismatch_is_reported() {
    let engine = EngineHandle::spawn(InferenceConfig::default()).unwrap();
    let mut c = engine.connect(Arc::new(|| {}));
    c.send(ClientMsg::Hello { protocol: PROTOCOL_VERSION + 1, client: "old tab".into() });
    assert!(matches!(recv(&mut c), ServerMsg::Notice { level: NoticeLevel::Error, .. }));
}

#[test]
fn restart_restores_chats_and_transcripts() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::at(dir.path());
    let url = fake_llm("restored reply");
    {
        let engine = EngineHandle::spawn_with(config(url.clone(), Some(paths.clone()))).unwrap();
        let mut c = engine.connect(Arc::new(|| {}));
        hello(&mut c);
        c.send(ClientMsg::SendMessage { text: "remember me".into(), attachments: vec![] });
        assert_eq!(finish(&mut c).0, None);
        c.send(ClientMsg::NewChat);
        opened(&mut c);
        c.send(ClientMsg::SendMessage { text: "second chat".into(), attachments: vec![] });
        assert_eq!(finish(&mut c).0, None);
    }
    let engine = EngineHandle::spawn_with(config(url, Some(paths))).unwrap();
    let mut c = engine.connect(Arc::new(|| {}));
    let h = hello(&mut c);
    let titles: Vec<&str> = h.chats.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(titles, ["second chat", "remember me"]);
    assert_eq!(h.entries.iter().map(|e| e.content.as_str()).collect::<Vec<_>>(), ["second chat", "restored reply"]);
    c.send(ClientMsg::OpenChat { id: h.chats[1].id.clone() });
    let (_, entries) = opened(&mut c);
    assert_eq!(entries[0].content, "remember me");
    assert_eq!((entries[1].content.as_str(), entries[1].done, entries[1].stats.is_some()), ("restored reply", true, true));
}

#[test]
fn interrupted_replies_are_closed_on_startup() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::at(dir.path());
    {
        let store = Store::open(paths.db()).unwrap();
        store.create_chat("c1", "Crashy", dir.path()).unwrap();
        store.append_message("c1", &NewMessage::user("go")).unwrap();
        let reply = store.append_message("c1", &NewMessage::assistant_streaming(None)).unwrap();
        store.append_message_text(reply, "half a rep", "").unwrap();
    }
    let engine = EngineHandle::spawn_with(config("http://127.0.0.1:9/v1".into(), Some(paths))).unwrap();
    let mut c = engine.connect(Arc::new(|| {}));
    let h = hello(&mut c);
    assert!(!h.state.busy);
    let reply = &h.entries[1];
    assert_eq!((reply.content.as_str(), reply.done, reply.error.as_deref()), ("half a rep", true, Some(chats::INTERRUPTED)));
}

#[test]
fn streamed_text_is_saved_in_batches_while_running() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::at(dir.path());
    let (url, release) = gated_llm();
    let engine = EngineHandle::spawn_with(config(url, Some(paths.clone()))).unwrap();
    let mut c = engine.connect(Arc::new(|| {}));
    hello(&mut c);
    c.send(ClientMsg::SendMessage { text: "go".into(), attachments: vec![] });
    recv_until(&mut c, |m| matches!(m, ServerMsg::EntryDelta { .. }));
    // Within a flush interval or two, the partial reply is on disk (as a crash would leave it).
    let store = Store::open(paths.db()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let chat = store.list_chats(1, None).unwrap().chats.remove(0);
        let msgs = store.messages(&chat.id).unwrap();
        if msgs[1].content == "first " {
            assert!(!msgs[1].done);
            break;
        }
        assert!(Instant::now() < deadline, "partial reply never saved");
        thread::sleep(Duration::from_millis(20));
    }
    release.send(()).unwrap();
    assert_eq!(finish(&mut c).0, None);
}

#[test]
fn clients_have_their_own_chat_and_deltas_go_only_to_viewers() {
    let (url, release) = gated_llm();
    let engine = engine(url);
    let mut a = engine.connect(Arc::new(|| {}));
    let mut b = engine.connect(Arc::new(|| {}));
    hello(&mut a);
    hello(&mut b);
    a.send(ClientMsg::SendMessage { text: "running chat".into(), attachments: vec![] });
    let (ServerMsg::ChatOpened { chat: Some(chat) }, _) = recv_until(&mut a, |m| matches!(m, ServerMsg::ChatOpened { .. })) else { panic!() };
    recv_until(&mut a, |m| matches!(m, ServerMsg::EntryDelta { .. }));
    // B still looks at its own new chat: it hears about the chat, not its entries.
    let seen = drain(&mut b);
    assert!(seen.iter().any(|m| matches!(m, ServerMsg::ChatUpdated(s) if s.id == chat.id && s.busy)));
    assert!(!seen.iter().any(|m| matches!(m, ServerMsg::EntryAdded(_) | ServerMsg::EntryDelta { .. } | ServerMsg::ChatOpened { .. })));

    // A switches away mid-turn; the turn keeps going.
    a.send(ClientMsg::NewChat);
    let (now, entries) = opened(&mut a);
    assert!(now.is_none() && entries.is_empty());
    // B opens the running chat and sees the partial reply.
    b.send(ClientMsg::OpenChat { id: chat.id.clone() });
    let (ServerMsg::State(state), _) = recv_until(&mut b, |m| matches!(m, ServerMsg::State(_))) else { panic!() };
    assert!(state.busy);
    let (_, entries) = opened(&mut b);
    assert_eq!((entries[1].content.as_str(), entries[1].done), ("first ", false));

    release.send(()).unwrap();
    let (error, msgs) = finish(&mut b);
    assert_eq!((error, text_of(&msgs).as_str()), (None, "second"));
    let a_saw = drain(&mut a);
    assert!(!a_saw.iter().any(|m| matches!(m, ServerMsg::EntryDelta { .. } | ServerMsg::EntryFinished { .. })), "{a_saw:?}");
    assert!(a_saw.iter().any(|m| matches!(m, ServerMsg::ChatUpdated(s) if s.id == chat.id && !s.busy)));

    // A comes back to it: the finished reply is all there.
    a.send(ClientMsg::OpenChat { id: chat.id });
    let (_, entries) = opened(&mut a);
    assert_eq!((entries[1].content.as_str(), entries[1].done), ("first second", true));
}

#[test]
fn chats_can_be_renamed_pinned_listed_and_deleted() {
    let engine = engine(fake_llm("ok"));
    let mut a = engine.connect(Arc::new(|| {}));
    let mut b = engine.connect(Arc::new(|| {}));
    hello(&mut a);
    for text in ["one", "two", "three"] {
        a.send(ClientMsg::NewChat);
        opened(&mut a);
        a.send(ClientMsg::SendMessage { text: text.into(), attachments: vec![] });
        finish(&mut a);
    }
    let h = hello(&mut b);
    assert_eq!(h.chat.as_ref().map(|c| c.title.as_str()), Some("three"));
    let one = h.chats.iter().find(|c| c.title == "one").unwrap().id.clone();

    a.send(ClientMsg::RenameChat { id: one.clone(), title: "  \"Renamed\"\nignored ".into() });
    let (ServerMsg::ChatUpdated(s), _) = recv_until(&mut b, |m| matches!(m, ServerMsg::ChatUpdated(_))) else { panic!() };
    assert_eq!(s.title, "Renamed");
    a.send(ClientMsg::PinChat { id: one.clone(), pinned: true });
    let (ServerMsg::ChatUpdated(s), _) = recv_until(&mut b, |m| matches!(m, ServerMsg::ChatUpdated(_))) else { panic!() };
    assert!(s.pinned_at.is_some());

    // Paging: pinned chats come separately on the first page.
    b.send(ClientMsg::ListChats { after: None, limit: 1 });
    let (ServerMsg::Chats { pinned, recent, next, .. }, _) = recv_until(&mut b, |m| matches!(m, ServerMsg::Chats { .. })) else { panic!() };
    assert_eq!(pinned.len(), 1);
    assert_eq!(recent.iter().map(|c| c.title.as_str()).collect::<Vec<_>>(), ["three"]);
    b.send(ClientMsg::ListChats { after: next.clone(), limit: 10 });
    let (ServerMsg::Chats { pinned, recent, after, next: end }, _) = recv_until(&mut b, |m| matches!(m, ServerMsg::Chats { .. })) else { panic!() };
    assert!(pinned.is_empty() && end.is_none());
    assert_eq!(after, next);
    assert_eq!(recent.iter().map(|c| c.title.as_str()).collect::<Vec<_>>(), ["two"]);

    // Deleting the chat B looks at moves B to a new chat.
    let three = h.chat.unwrap().id;
    a.send(ClientMsg::DeleteChat { id: three.clone() });
    let (ServerMsg::ChatDeleted { id }, _) = recv_until(&mut b, |m| matches!(m, ServerMsg::ChatDeleted { .. })) else { panic!() };
    assert_eq!(id, three);
    let (now, entries) = opened(&mut b);
    assert!(now.is_none() && entries.is_empty());
    b.send(ClientMsg::OpenChat { id: three });
    let (ServerMsg::Notice { .. }, _) = recv_until(&mut b, |m| matches!(m, ServerMsg::Notice { .. })) else { panic!() };
}

#[test]
fn running_chats_cannot_be_deleted_and_other_chats_run_in_parallel() {
    let (url, release) = gated_llm();
    let engine = engine(url);
    let mut a = engine.connect(Arc::new(|| {}));
    hello(&mut a);
    a.send(ClientMsg::SendMessage { text: "slow".into(), attachments: vec![] });
    let (ServerMsg::ChatOpened { chat: Some(chat) }, _) = recv_until(&mut a, |m| matches!(m, ServerMsg::ChatOpened { .. })) else { panic!() };
    recv_until(&mut a, |m| matches!(m, ServerMsg::EntryDelta { .. }));
    a.send(ClientMsg::DeleteChat { id: chat.id.clone() });
    let (ServerMsg::Notice { text, .. }, _) = recv_until(&mut a, |m| matches!(m, ServerMsg::Notice { .. })) else { panic!() };
    assert!(text.contains("stop it first"), "{text}");
    // A second chat can run while the first one waits.
    a.send(ClientMsg::NewChat);
    opened(&mut a);
    a.send(ClientMsg::SendMessage { text: "fast".into(), attachments: vec![] });
    recv_until(&mut a, |m| matches!(m, ServerMsg::EntryDelta { .. }));
    release.send(()).unwrap();
    release.send(()).unwrap();
    assert_eq!(finish(&mut a).0, None);
}

#[test]
fn first_reply_names_the_chat() {
    let mut c = config(fake_llm("Title: \"Trip Planning.\""), None);
    c.auto_title = true;
    let engine = EngineHandle::spawn_with(c).unwrap();
    let mut a = engine.connect(Arc::new(|| {}));
    hello(&mut a);
    a.send(ClientMsg::SendMessage { text: "plan a trip".into(), attachments: vec![] });
    finish(&mut a);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (ServerMsg::ChatUpdated(s), _) = recv_until(&mut a, |m| matches!(m, ServerMsg::ChatUpdated(_))) else { panic!() };
        if s.title == "Trip Planning" {
            break;
        }
        assert!(Instant::now() < deadline);
    }
}
