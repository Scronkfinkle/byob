use std::cell::RefCell;
use std::rc::Rc;

use super::*;
use crate::appview::{AppViewResponse, theme_vars};

/// A runtime that runs nothing: records what is posted, lets the test play the page.
#[derive(Default)]
struct Rec {
    posted: Rc<RefCell<Vec<(String, u32, Value)>>>,
    from_page: Rc<RefCell<Vec<(String, u32, String)>>>,
    loaded: Rc<RefCell<Vec<(String, u32, String)>>>,
    closed: Rc<RefCell<Vec<(String, u32)>>>,
    /// The largest message it takes (0: the default).
    max: usize,
}

impl AppView for Rec {
    fn show(&mut self, ui: &mut egui::Ui, _t: &Tokens, doc: &AppDoc<'_>) -> AppViewResponse {
        let _ = self.card(ui, _t, doc);
        AppViewResponse::default()
    }
    fn card(&mut self, ui: &mut egui::Ui, _t: &Tokens, doc: &AppDoc<'_>) -> Option<AppViewResponse> {
        assert!(doc.web);
        let mut l = self.loaded.borrow_mut();
        if !l.iter().any(|(i, v, _)| i == doc.id && *v == doc.version) {
            l.push((doc.id.to_owned(), doc.version, doc.source.unwrap_or_default().to_owned()));
        }
        ui.label("app");
        Some(AppViewResponse::default())
    }
    fn runs_inline(&self) -> bool {
        true
    }
    fn post(&mut self, id: &str, version: u32, msg: &str) -> bool {
        assert!(msg.len() <= self.max_message(), "posted {} bytes", msg.len());
        self.posted.borrow_mut().push((id.to_owned(), version, serde_json::from_str(msg).unwrap()));
        true
    }
    fn max_message(&self) -> usize {
        if self.max == 0 { byob_protocol::web::MAX_CALL_BYTES * 4 } else { self.max }
    }
    fn take_messages(&mut self) -> Vec<(String, u32, String)> {
        std::mem::take(&mut *self.from_page.borrow_mut())
    }
    fn close(&mut self, id: &str, version: u32) {
        self.closed.borrow_mut().push((id.to_owned(), version));
    }
}

struct Harness {
    web: Web,
    ctx: egui::Context,
    posted: Rc<RefCell<Vec<(String, u32, Value)>>>,
    from_page: Rc<RefCell<Vec<(String, u32, String)>>>,
    loaded: Rc<RefCell<Vec<(String, u32, String)>>>,
    closed: Rc<RefCell<Vec<(String, u32)>>>,
    time: f64,
}

const KEY: &str = "web:mcp:7:0";

fn owner() -> WebOwner {
    WebOwner::Mcp { plugin: "weather".into(), server: "main".into(), uri: "ui://weather/dash".into() }
}

fn init() -> Init {
    Init {
        tool_input: Some(json!({"city": "Lisbon"})),
        tool_result: Some(json!({"content": [{"type": "text", "text": "21°C"}], "structuredContent": {"t": 21}})),
        tool_def: Some(json!({"name": "get_weather"})),
        notify: None,
    }
}

impl Harness {
    fn new() -> Self {
        Self::with_max(0)
    }

    /// With a runtime that takes messages of at most `max` bytes (0: the default).
    fn with_max(max: usize) -> Self {
        let rec = Rec { max, ..Rec::default() };
        let (posted, from_page, loaded, closed) = (Rc::clone(&rec.posted), Rc::clone(&rec.from_page), Rc::clone(&rec.loaded), Rc::clone(&rec.closed));
        let mut web = Web::new(Box::new(rec));
        let w = byob_protocol::Welcome::current();
        assert!(!web.apply(&ServerMsg::Welcome(w)));
        web.set_theme(&theme_vars(&Tokens::dark()));
        Self { web, ctx: egui::Context::default(), posted, from_page, loaded, closed, time: 0.0 }
    }

    /// One frame drawing the card, then the end-of-frame pump.
    fn frame(&mut self) {
        self.time += 0.1;
        let input = egui::RawInput { time: Some(self.time), ..Default::default() };
        let web = &mut self.web;
        let mut out = self.ctx.run_ui(input, |ui| {
            web.card(ui, &Tokens::dark(), Spec { key: KEY, owner: &owner(), kind: Kind::McpApp, title: "Weather", init: Some(init()) });
        });
        out.textures_delta.clear();
        self.web.pump(self.time);
    }

    fn idle(&mut self, secs: f64) {
        self.time += secs;
        self.web.pump(self.time);
    }

    fn page(&mut self, msg: Value) {
        let v = self.loaded.borrow().last().map_or(1, |l| l.1);
        self.from_page.borrow_mut().push((KEY.into(), v, msg.to_string()));
        self.frame();
    }

    fn posted(&self) -> Vec<Value> {
        self.posted.borrow_mut().drain(..).map(|(_, _, v)| v).collect()
    }

    fn calls(&mut self) -> Vec<(u64, String, Value, bool)> {
        self.web
            .take()
            .into_iter()
            .map(|m| match m {
                ClientMsg::WebCall { call, owner: o, method, params, approved } => {
                    assert_eq!(o, owner());
                    (call, method, params, approved)
                }
                other => panic!("{other:?}"),
            })
            .collect()
    }
}

#[test]
fn documents_get_the_csp_the_bridge_and_the_theme_first() {
    let vars = theme_vars(&Tokens::dark());
    let d = document("<!DOCTYPE html><html><body><script>app()</script></body></html>", Kind::McpApp, &vars);
    assert!(d.starts_with("<!DOCTYPE html><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\""), "{d}");
    assert!(d.contains("connect-src 'none'") && d.contains("--color-background-primary:#") && d.contains("--byob-accent:#"));
    assert!(d.contains("AUTO=false") && !d.contains("__AUTO__"));
    let bridge = d.find("byob-msg").unwrap();
    assert!(bridge < d.find("app()").unwrap(), "the bridge runs before the page's code");
    // Top-level (the Servo helper): only JSON-RPC goes to byob, the page's own posts to the page.
    assert!(d.contains("m.jsonrpc!=='2.0')return O(m,...a)"), "{d}");
    let b = document("<p>hi</p>", Kind::Byob, &vars);
    assert!(b.starts_with("<!doctype html><meta") && b.ends_with("<p>hi</p>") && b.contains("AUTO=true"));
    let s = style_variables(&vars);
    assert_eq!(s["--color-text-primary"], s["--byob-fg"]);
    assert_eq!(s["--border-radius-md"], "6px");
}

/// The MCP Apps lifecycle against the host: document, handshake with host context, tool
/// input and result, a proxied tools/call, theme changes, teardown.
#[test]
#[allow(clippy::cognitive_complexity)] // one story, step by step
fn an_mcp_app_runs_through_its_lifecycle() {
    let mut h = Harness::new();
    h.frame();
    let calls = h.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, "byob/document");
    // Nothing runs before the document comes.
    assert!(h.loaded.borrow().is_empty());
    let doc = WebDocument { html: "<!doctype html><p id=x>app</p>".into(), ..Default::default() };
    assert!(h.web.apply(&ServerMsg::WebReply { call: calls[0].0, result: Some(serde_json::to_value(doc).unwrap()), error: None }));
    h.frame();
    let loaded = h.loaded.borrow().clone();
    assert_eq!(loaded.len(), 1);
    assert!(loaded[0].2.contains("byob-msg") && loaded[0].2.ends_with("<p id=x>app</p>"));

    // Handshake: the host answers with its context; nothing is sent before `initialized`.
    h.page(json!({"jsonrpc": "2.0", "id": 1, "method": "ui/initialize", "params": {"protocolVersion": "2026-01-26", "appCapabilities": {}}}));
    let p = h.posted();
    assert_eq!(p.len(), 1, "{p:?}");
    let r = &p[0]["result"];
    assert_eq!(p[0]["id"], 1);
    assert_eq!(r["hostInfo"]["name"], "byob");
    assert_eq!(r["hostContext"]["theme"], "dark");
    assert!(r["hostContext"]["styles"]["variables"]["--color-background-primary"].as_str().unwrap().starts_with('#'));
    assert_eq!(r["hostContext"]["toolInfo"]["tool"]["name"], "get_weather");
    assert!(r["hostCapabilities"].get("serverTools").is_some());
    h.page(json!({"jsonrpc": "2.0", "method": "ui/notifications/initialized", "params": {}}));
    let p = h.posted();
    let methods: Vec<&str> = p.iter().filter_map(|m| m["method"].as_str()).collect();
    assert_eq!(methods, ["ui/notifications/tool-input", "ui/notifications/tool-result"]);
    assert_eq!(p[0]["params"]["arguments"]["city"], "Lisbon");
    assert_eq!(p[1]["params"]["structuredContent"]["t"], 21);

    // tools/call goes to the engine and its answer back to the page, with the page's id.
    h.page(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "refresh", "arguments": {}}}));
    let c = h.calls();
    assert_eq!((c[0].1.as_str(), c[0].2["name"].as_str(), c[0].3), ("tools/call", Some("refresh"), false));
    h.web.apply(&ServerMsg::WebReply { call: c[0].0, result: Some(json!({"content": [{"type": "text", "text": "22°C"}]})), error: None });
    let p = h.posted();
    assert_eq!((p[0]["id"].clone(), p[0]["result"]["content"][0]["text"].clone()), (json!(2), json!("22°C")));

    // A call the gate asks about waits for the user; Allow sends it again, approved.
    h.page(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "delete_city"}}));
    let c = h.calls();
    let approval = WebApproval { title: "weather: delete_city".into(), access: byob_protocol::ToolAccess::Network };
    h.web.apply(&ServerMsg::WebReply { call: c[0].0, result: None, error: Some(WebError { code: -32000, message: "ask".into(), approval: Some(approval) }) });
    assert_eq!(h.web.asking(KEY), Some("weather: delete_city"));
    assert!(h.posted().is_empty(), "the page waits");
    h.web.decide(KEY, true);
    let c = h.calls();
    assert!(c[0].3, "sent again approved");
    h.web.apply(&ServerMsg::WebReply { call: c[0].0, result: None, error: Some(WebError::new(-32000, "gone")) });
    let p = h.posted();
    assert_eq!((p[0]["id"].clone(), p[0]["error"]["message"].clone()), (json!(3), json!("gone")));
    // Deny answers the page at once.
    h.page(json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "delete_city"}}));
    let c = h.calls();
    let approval = WebApproval { title: "t".into(), access: byob_protocol::ToolAccess::Network };
    h.web.apply(&ServerMsg::WebReply { call: c[0].0, result: None, error: Some(WebError { code: -32000, message: "ask".into(), approval: Some(approval) }) });
    h.web.decide(KEY, false);
    assert_eq!(h.posted()[0]["error"]["message"], "Denied by the user");
    assert!(h.calls().is_empty());

    // Host-side methods never reach the engine.
    h.page(json!({"jsonrpc": "2.0", "id": 5, "method": "ui/open-link", "params": {"url": "https://example.com/x"}}));
    h.page(json!({"jsonrpc": "2.0", "id": 6, "method": "ui/open-link", "params": {"url": "javascript:alert(1)"}}));
    h.page(json!({"jsonrpc": "2.0", "method": "ui/notifications/size-changed", "params": {"width": 300, "height": 222}}));
    h.page(json!({"jsonrpc": "2.0", "id": 7, "method": "ping"}));
    assert_eq!(h.web.take_links(), ["https://example.com/x"]);
    let p = h.posted();
    assert_eq!(p.iter().map(|m| m.get("error").is_some()).collect::<Vec<_>>(), [false, true, false]);
    assert!(h.calls().is_empty());
    // ui/message and model context are the engine's.
    h.page(json!({"jsonrpc": "2.0", "id": 8, "method": "ui/message", "params": {"role": "user", "content": {"type": "text", "text": "hi"}}}));
    assert_eq!(h.calls()[0].1, "ui/message");

    // A theme change reaches initialized pages.
    h.web.set_theme(&theme_vars(&Tokens::light()));
    let p = h.posted();
    assert_eq!(p[0]["method"], "ui/notifications/host-context-changed");
    assert_eq!(p[0]["params"]["theme"], "light");

    // Garbage from the page is ignored.
    h.page(json!("not an object"));
    h.from_page.borrow_mut().push((KEY.into(), 1, "{nope".into()));
    h.frame();
    assert!(h.posted().is_empty() && h.calls().is_empty());

    // Out of view: told first (teardown), then closed once it answers.
    h.idle(3.0);
    let p = h.posted();
    assert_eq!(p[0]["method"], "ui/resource-teardown");
    let id = p[0]["id"].clone();
    assert!(h.closed.borrow().is_empty());
    h.from_page.borrow_mut().push((KEY.into(), 1, json!({"jsonrpc": "2.0", "id": id, "result": {}}).to_string()));
    h.idle(0.0);
    h.idle(0.0);
    assert_eq!(*h.closed.borrow(), [(KEY.to_owned(), 1)]);
    // Back in view: the same document again under a new generation; no new fetch.
    h.frame();
    assert_eq!(h.loaded.borrow().last().map(|l| l.1), Some(2));
    assert!(h.calls().is_empty());
}

#[test]
fn owners_push_to_their_pages_and_chat_switches_tear_down() {
    let mut h = Harness::new();
    h.frame();
    let c = h.calls();
    h.web.apply(&ServerMsg::WebReply { call: c[0].0, result: Some(json!({"html": "<p>x</p>"})), error: None });
    h.frame();
    h.page(json!({"jsonrpc": "2.0", "id": 1, "method": "ui/initialize", "params": {}}));
    h.page(json!({"jsonrpc": "2.0", "method": "ui/notifications/initialized"}));
    h.posted();
    assert!(h.web.apply(&ServerMsg::WebPush { owner: owner(), msg: json!({"n": 1}) }));
    let other = WebOwner::Plugin { plugin: "x".into(), view: "v".into(), key: String::new() };
    h.web.apply(&ServerMsg::WebPush { owner: other, msg: json!({"n": 2}) });
    let p = h.posted();
    assert_eq!(p.len(), 1);
    assert_eq!((p[0]["method"].clone(), p[0]["params"]["n"].clone()), (json!("byob/message"), json!(1)));
    h.web.apply(&ServerMsg::ChatOpened { chat: None, workspace: None });
    assert_eq!(h.posted()[0]["method"], "ui/resource-teardown");
}

#[test]
fn without_the_cap_nothing_is_asked() {
    let rec = Rec::default();
    let mut web = Web::new(Box::new(rec));
    let ctx = egui::Context::default();
    let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
        web.card(ui, &Tokens::dark(), Spec { key: KEY, owner: &owner(), kind: Kind::McpApp, title: "W", init: Some(init()) })
    });
    out.textures_delta.clear();
    assert!(web.take().is_empty());
}

impl Harness {
    /// The document loaded and the handshake done; what was posted so far drained.
    fn ready(&mut self) {
        self.frame();
        let c = self.calls();
        self.web.apply(&ServerMsg::WebReply { call: c[0].0, result: Some(json!({"html": "<p>x</p>"})), error: None });
        self.frame();
        self.page(json!({"jsonrpc": "2.0", "id": 1, "method": "ui/initialize", "params": {}}));
        self.page(json!({"jsonrpc": "2.0", "method": "ui/notifications/initialized"}));
        self.posted();
    }

    /// The engine says the gate asks about the call it was sent as `call`.
    fn gate_asks(&mut self, call: u64) {
        let approval = WebApproval { title: "weather: delete_city".into(), access: byob_protocol::ToolAccess::Network };
        self.web.apply(&ServerMsg::WebReply { call, result: None, error: Some(WebError { code: -32000, message: "ask".into(), approval: Some(approval) }) });
    }
}

/// `byob.openLink("https://x/&calc")` ran calc on Windows (through `cmd /C start`); and a
/// page could open a thousand tabs a frame.
#[test]
fn links_are_encoded_and_one_per_surface_every_couple_of_seconds() {
    let mut h = Harness::new();
    h.ready();
    h.page(json!({"jsonrpc": "2.0", "id": 2, "method": "ui/open-link", "params": {"url": "https://x/\"&calc^|<>"}}));
    assert_eq!(h.web.take_links(), ["https://x/%22&calc%5E%7C%3C%3E"]);
    assert!(h.posted()[0].get("result").is_some());
    for id in 3..20 {
        h.page(json!({"jsonrpc": "2.0", "id": id, "method": "ui/open-link", "params": {"url": "https://example.com/"}}));
    }
    assert!(h.web.take_links().is_empty(), "too soon");
    let p = h.posted();
    assert_eq!(p.len(), 17);
    assert!(p.iter().all(|m| m.get("error").is_some()), "every call is answered");
    for _ in 0..20 {
        h.frame();
    }
    h.page(json!({"jsonrpc": "2.0", "id": 30, "method": "ui/open-link", "params": {"url": "https://example.com/"}}));
    assert_eq!(h.web.take_links(), ["https://example.com/"]);
}

/// An answer larger than the runtime takes (the Servo helper: 256 KB) was dropped, and the
/// page's promise never settled.
#[test]
fn answers_too_large_for_the_page_settle_with_an_error() {
    let mut h = Harness::with_max(2000);
    h.ready();
    h.page(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "big"}}));
    let c = h.calls();
    h.web.apply(&ServerMsg::WebReply { call: c[0].0, result: Some(json!({"text": "x".repeat(5000)})), error: None });
    let p = h.posted();
    assert_eq!(p.len(), 1);
    assert_eq!(p[0]["id"], 2);
    assert!(p[0]["error"]["message"].as_str().unwrap().contains("too large"), "{p:?}");
}

/// A `web:` renderer's artifact text is cut to what one message takes.
#[test]
fn artifact_text_is_cut_to_fit_one_message() {
    let a: byob_protocol::Artifact = serde_json::from_value(json!({
        "id": "notes", "kind": "markdown", "title": "Notes \"quoted\"", "filename": "notes.md",
        "versions": [{"n": 1, "path": "notes.md", "created_at": 0}]
    }))
    .unwrap();
    for (text, max) in [("a\"\\\n\u{1}é".repeat(50_000), 256 << 10), ("short".to_owned(), 256 << 10), ("x".repeat(300_000), 10_000)] {
        let init = artifact_init(&a, 1, &text, max);
        let (m, p) = init.notify.unwrap();
        let msg = json!({"jsonrpc": "2.0", "method": m, "params": p}).to_string();
        assert!(msg.len() <= max, "{} > {max}", msg.len());
        let sent = p["text"].as_str().unwrap();
        assert!(text.starts_with(sent));
        assert_eq!(p["truncated"], sent.len() < text.len());
        assert!(sent.len() * 2 > max.min(text.len()) / 6, "not cut more than it needs: {}", sent.len());
    }
    assert_eq!(fit_json("ab\"c", 5), ("ab", true));
    assert_eq!(fit_json("ab\"c", 6), ("ab\"", true));
    assert_eq!(fit_json("ab\"c", 8), ("ab\"c", false));
}

/// A second call the gate asks about replaced the first one's bar: the first never got an
/// answer.
#[test]
fn a_second_gated_call_is_refused_while_one_waits() {
    let mut h = Harness::new();
    h.ready();
    h.page(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "delete_city"}}));
    h.page(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "delete_city"}}));
    let c = h.calls();
    h.gate_asks(c[0].0);
    h.gate_asks(c[1].0);
    let p = h.posted();
    assert_eq!((p.len(), p[0]["id"].clone()), (1, json!(3)), "the second is refused: {p:?}");
    assert!(p[0]["error"]["message"].as_str().unwrap().contains("waiting"));
    h.web.decide(KEY, false);
    let p = h.posted();
    assert_eq!((p[0]["id"].clone(), p[0]["error"]["message"].clone()), (json!(2), json!("Denied by the user")));
}

/// Surface records outlived their chat, and eviction didn't evict.
#[test]
fn surface_records_go_with_their_chat_and_are_capped() {
    let mut h = Harness::new();
    h.ready();
    h.web.apply(&ServerMsg::ChatOpened { chat: None, workspace: None });
    h.idle(TEARDOWN_WAIT + 0.1);
    assert!(h.web.surfaces.is_empty(), "{}", h.web.surfaces.len());
    for i in 0..=MAX_SURFACES {
        h.time += 0.1;
        let input = egui::RawInput { time: Some(h.time), ..Default::default() };
        let web = &mut h.web;
        let mut out = h.ctx.run_ui(input, |ui| {
            web.card(ui, &Tokens::dark(), Spec { key: &format!("k{i}"), owner: &owner(), kind: Kind::McpApp, title: "W", init: Some(init()) });
        });
        out.textures_delta.clear();
        if i + 1 == MAX_SURFACES {
            h.idle(TEARDOWN_AFTER + TEARDOWN_WAIT + 1.0);
        }
    }
    assert!(h.web.surfaces.len() <= MAX_SURFACES, "{}", h.web.surfaces.len());
}

/// Step views are decoded when their data changes (not every frame), and a new result still
/// reaches the page.
#[test]
fn step_views_follow_their_data() {
    let mut h = Harness::new();
    let app = |text: &str| View {
        kind: "byob.mcp_app".into(),
        v: McpApp::V,
        data: json!({"plugin": "weather", "server": "main", "tool": "get_weather", "uri": "ui://weather/dash", "result": {"t": text}}),
        fallback: String::new(),
        owner: None,
    };
    let draw = |h: &mut Harness, view: &View| {
        h.time += 0.1;
        let input = egui::RawInput { time: Some(h.time), ..Default::default() };
        let web = &mut h.web;
        let mut out = h.ctx.run_ui(input, |ui| assert!(mcp_app_view(ui, &Tokens::dark(), web, 7, 0, view)));
        out.textures_delta.clear();
        h.web.pump(h.time);
    };
    let one = app("21");
    draw(&mut h, &one);
    let c = h.calls();
    h.web.apply(&ServerMsg::WebReply { call: c[0].0, result: Some(json!({"html": "<p>x</p>"})), error: None });
    draw(&mut h, &one);
    h.from_page.borrow_mut().push((KEY.into(), 1, json!({"jsonrpc": "2.0", "id": 1, "method": "ui/initialize", "params": {}}).to_string()));
    draw(&mut h, &one);
    h.from_page.borrow_mut().push((KEY.into(), 1, json!({"jsonrpc": "2.0", "method": "ui/notifications/initialized"}).to_string()));
    draw(&mut h, &one);
    let p = h.posted();
    assert_eq!(p.last().unwrap()["params"]["t"], "21");
    draw(&mut h, &one);
    assert!(h.posted().is_empty());
    draw(&mut h, &app("22"));
    let p = h.posted();
    assert_eq!((p[0]["method"].clone(), p[0]["params"]["t"].clone()), (json!("ui/notifications/tool-result"), json!("22")));
    assert_eq!(h.web.step_views.len(), 1);
}
