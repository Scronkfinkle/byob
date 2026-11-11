//! Engine tests of web surfaces: an MCP App against a fake MCP Apps server (document,
//! gated tools/call, app-only and model-only tools, resources, model context, ui/message),
//! and a plugin's web view (document, its backend's tools, messages both ways).

use std::fs;
use std::time::{Duration, Instant};

use byob_protocol::web::{McpApp, WebError, WebOwner};
use byob_protocol::{ApprovalAnswer, ClientMsg, Connection, ServerMsg, ServerState};
use serde_json::{Value, json};

use crate::LocalConnection;
use crate::extensions_tests::{harness, hello, recv, step, system, turn, wait_ext};
use crate::loop_tests::{Reply, call};

const FAKE_APPS: &str = r#"
import json, sys
HTML = "<!doctype html><title>Weather</title><p id=t>weather app</p>"
TOOLS = [
    {"name": "show_weather", "description": "Show the weather", "inputSchema": {"type": "object", "properties": {"city": {"type": "string"}}},
     "_meta": {"ui": {"resourceUri": "ui://weather/dash"}}},
    {"name": "refresh", "description": "Refresh (for the app)", "inputSchema": {"type": "object"}, "_meta": {"ui": {"visibility": ["app"]}}},
    {"name": "secret", "description": "Model only", "inputSchema": {"type": "object"}, "_meta": {"ui": {"visibility": ["model"]}}},
]
for line in sys.stdin:
    try:
        m = json.loads(line)
    except Exception:
        continue
    mid, meth, p = m.get("id"), m.get("method"), m.get("params") or {}
    if mid is None:
        continue
    if meth == "initialize":
        ext = p.get("capabilities", {}).get("extensions", {}).get("io.modelcontextprotocol/ui", {})
        sys.stderr.write("apps ext: %s\n" % json.dumps(ext))
        r = {"protocolVersion": p.get("protocolVersion"), "capabilities": {"tools": {}, "resources": {}}, "serverInfo": {"name": "weather", "version": "1"}}
    elif meth == "tools/list":
        r = {"tools": TOOLS}
    elif meth == "tools/call":
        a = p.get("arguments") or {}
        r = {"content": [{"type": "text", "text": "%s: %s" % (p["name"], a.get("city", "-"))}], "structuredContent": {"tool": p["name"], "temp": 21}}
    elif meth == "resources/read":
        if p.get("uri") == "ui://weather/dash":
            r = {"contents": [{"uri": p["uri"], "mimeType": "text/html;profile=mcp-app", "text": HTML,
                 "_meta": {"ui": {"csp": {"connectDomains": ["https://api.weather.example"]}, "prefersBorder": True}}}]}
        else:
            r = {"contents": [{"uri": p.get("uri"), "mimeType": "text/plain", "text": "other"}]}
    else:
        r = {}
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": mid, "result": r}) + "\n")
    sys.stdout.flush()
"#;

/// Send a web call and wait for its answer (other messages are skipped).
fn web_call(c: &mut LocalConnection, n: u64, owner: &WebOwner, method: &str, params: Value, approved: bool) -> Result<Value, WebError> {
    c.send(ClientMsg::WebCall { call: n, owner: owner.clone(), method: method.into(), params, approved });
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "no answer to {method}");
        if let ServerMsg::WebReply { call, result, error } = recv(c)
            && call == n
        {
            return match (result, error) {
                (_, Some(e)) => Err(e),
                (r, None) => Ok(r.unwrap_or(Value::Null)),
            };
        }
    }
}

fn approve_all(s: &byob_protocol::Step, c: &mut LocalConnection) {
    c.send(ClientMsg::Approve { step: s.id, answer: ApprovalAnswer::Allow });
}

#[test]
fn an_mcp_app_shows_on_its_step_and_its_calls_go_through_the_gate() {
    let script = vec![call("weather__show_weather", r#"{"city": "Lisbon"}"#), Reply::Text("shown"), Reply::Text("noted")];
    let h = harness(script, None, |home, _| {
        let dir = home.join("plugins/weather");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("server.py"), FAKE_APPS).unwrap();
        let python = byob_plugins::default_python()[0].to_string_lossy().into_owned();
        let m = json!({"id": "weather", "name": "Weather", "mcpServers": {"main": {"command": python, "args": ["server.py"]}}});
        fs::write(dir.join("byob-plugin.json"), m.to_string()).unwrap();
    });
    let (mut c, _) = hello(&h);
    let e = wait_ext(&mut c, |e| e.plugins.iter().any(|p| p.id == "weather" && p.servers.first().is_some_and(|s| s.state == ServerState::Ready)));
    // App-only tools are hidden from the model; the others are its tools.
    let tools = &e.plugins.iter().find(|p| p.id == "weather").unwrap().tools;
    assert_eq!(tools, &["weather__show_weather", "weather__secret"], "{tools:?}");

    let (r, _) = turn(&mut c, "weather please", approve_all);
    let s = step(&r, "weather__show_weather");
    let app = s.view::<McpApp>().expect("the step has the MCP App");
    assert_eq!((app.plugin.as_str(), app.server.as_str(), app.tool.as_str(), app.uri.as_str()), ("weather", "main", "show_weather", "ui://weather/dash"));
    assert_eq!(app.input["city"], "Lisbon");
    assert_eq!(app.result["structuredContent"]["temp"], 21);
    assert_eq!(app.tool_def["_meta"]["ui"]["resourceUri"], "ui://weather/dash");
    let sent = h.requests.lock().unwrap().clone();
    assert!(!crate::extensions_tests::tool_names(&sent[0]).iter().any(|t| t.contains("refresh")));

    let owner = app.owner();
    // The document is the server's ui:// resource (its CSP request is only reported).
    let doc = web_call(&mut c, 1, &owner, "byob/document", json!({}), false).unwrap();
    assert!(doc["html"].as_str().unwrap().contains("weather app"));
    assert_eq!(doc["prefers_border"], true);
    assert_eq!(doc["csp"]["connectDomains"][0], "https://api.weather.example");

    // An app-only tool: the gate asks first (a local server runs code), then it runs.
    let err = web_call(&mut c, 2, &owner, "tools/call", json!({"name": "refresh", "arguments": {}}), false).unwrap_err();
    let approval = err.approval.expect("asks");
    assert!(approval.title.contains("refresh"), "{}", approval.title);
    let ok = web_call(&mut c, 3, &owner, "tools/call", json!({"name": "refresh", "arguments": {"city": "Porto"}}), true).unwrap();
    assert_eq!(ok["content"][0]["text"], "refresh: Porto");
    assert_eq!(ok["structuredContent"]["temp"], 21);
    // Model-only tools and other servers' tools are refused.
    assert!(web_call(&mut c, 4, &owner, "tools/call", json!({"name": "secret"}), true).unwrap_err().message.contains("can't be called from an app"));
    assert!(web_call(&mut c, 5, &owner, "tools/call", json!({"name": "nope"}), true).is_err());
    let elsewhere = WebOwner::Mcp { plugin: "weather".into(), server: "other".into(), uri: "ui://weather/dash".into() };
    assert!(web_call(&mut c, 6, &elsewhere, "tools/call", json!({"name": "refresh"}), true).is_err());
    // Resources of the same server.
    let res = web_call(&mut c, 7, &owner, "resources/read", json!({"uri": "ui://weather/other"}), false).unwrap();
    assert_eq!(res["contents"][0]["text"], "other");
    // Unknown methods are JSON-RPC errors.
    assert_eq!(web_call(&mut c, 8, &owner, "sampling/createMessage", json!({}), false).unwrap_err().code, -32601);

    // Model context goes into the next system prompt.
    web_call(
        &mut c,
        9,
        &owner,
        "ui/update-model-context",
        json!({"content": [{"type": "text", "text": "The user picked Lisbon"}], "structuredContent": {"city": "Lisbon"}}),
        false,
    )
    .unwrap();
    // ui/message: a user message in the chat (it starts the next turn).
    web_call(&mut c, 10, &owner, "ui/message", json!({"role": "user", "content": {"type": "text", "text": "from the app"}}), false).unwrap();
    let mut user = None;
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        match recv(&mut c) {
            ServerMsg::EntryAdded(e) if e.content == "from the app" => user = Some(e),
            ServerMsg::EntryFinished { .. } if user.is_some() => break,
            _ => {}
        }
    }
    assert!(user.is_some(), "ui/message became a user message");
    let sent = h.requests.lock().unwrap().clone();
    let last = system(sent.last().unwrap());
    assert!(last.contains("The user picked Lisbon") && last.contains(r#"{"city":"Lisbon"}"#), "{last}");
    assert!(last.contains("weather/main app ui://weather/dash"));
    // An empty update clears it.
    web_call(&mut c, 11, &owner, "ui/update-model-context", json!({"content": []}), false).unwrap();
    assert!(web_call(&mut c, 12, &owner, "ui/message", json!({"role": "assistant", "content": {"type": "text", "text": "x"}}), false).is_err());
}

const PANEL_BACKEND: &str = r#"
import byob_plugin as byob

@byob.tool
def add(a: int = 0, b: int = 0):
    return str(a + b)

@byob.tool
def chart(n: int = 3):
    return {"text": "a chart of %d" % n, "card": {"values": list(range(n))}, "card_view": "chartcard"}

@byob.on_web("panel")
def panel(msg, event):
    if isinstance(msg, dict) and msg.get("push"):
        byob.web_send("panel", {"echo": msg["push"]}, client=event.get("client"))
        return None
    return {"got": msg, "view": event.get("view")}

byob.run()
"#;

#[test]
fn a_plugin_web_view_talks_to_its_backend() {
    let script = vec![call("chart", r#"{"n": 2}"#), Reply::Text("drawn")];
    let h = harness(script, Some(crate::Policy::ALLOW_ALL), |home, _| {
        let dir = home.join("plugins/panel");
        fs::create_dir_all(dir.join("ui")).unwrap();
        fs::write(dir.join("backend.py"), PANEL_BACKEND).unwrap();
        fs::write(dir.join("ui/index.html"), "<!doctype html><p>panel</p>").unwrap();
        fs::write(dir.join("ui/card.html"), "<p>card</p>").unwrap();
        let m = json!({"id": "panel", "name": "Panel", "backend": "backend.py",
            "views": [{"site": "pane", "id": "panel", "title": "Panel", "html": "ui/index.html"}, {"site": "card", "id": "chartcard", "html": "ui/card.html"}],
            "renderers": [{"type": "text/x-notes", "renderer": "web:ui/card.html"}],
            "tools": [{"name": "add", "description": "Add", "params": {"type": "object", "properties": {"a": {"type": "integer"}, "b": {"type": "integer"}}}, "run": "backend"},
                      {"name": "chart", "description": "Chart", "params": {"type": "object", "properties": {"n": {"type": "integer"}}}, "run": "backend"}]});
        fs::write(dir.join("byob-plugin.json"), m.to_string()).unwrap();
    });
    let (mut c, ext) = hello(&h);
    let p = ext.plugins.iter().find(|p| p.id == "panel").unwrap();
    assert_eq!(p.views[0].html, "ui/index.html");
    let owner = WebOwner::Plugin { plugin: "panel".into(), view: "panel".into(), key: String::new() };
    let doc = web_call(&mut c, 1, &owner, "byob/document", json!({}), false).unwrap();
    assert_eq!(doc["html"], "<!doctype html><p>panel</p>");
    // Its own backend's tools: no approval card (the plugin's own UI).
    let r = web_call(&mut c, 2, &owner, "tools/call", json!({"name": "add", "arguments": {"a": 2, "b": 3}}), false).unwrap();
    assert_eq!(r["content"][0]["text"], "5");
    // byob.call → a `web` request the backend answers.
    let r = web_call(&mut c, 3, &owner, "byob/call", json!({"msg": {"q": 1}}), false).unwrap();
    assert_eq!(r, json!({"got": {"q": 1}, "view": "panel"}));
    // byob.send → a `web` event; the backend pushes to the page.
    web_call(&mut c, 4, &owner, "byob/send", json!({"msg": {"push": "hello"}}), false).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let pushed = loop {
        assert!(Instant::now() < deadline, "no push");
        if let ServerMsg::WebPush { owner: o, msg } = recv(&mut c) {
            break (o, msg);
        }
    };
    assert_eq!(pushed, (owner.clone(), json!({"echo": "hello"})));
    // A view it doesn't declare has no document.
    let other = WebOwner::Plugin { plugin: "panel".into(), view: "nope".into(), key: String::new() };
    assert!(web_call(&mut c, 5, &other, "byob/document", json!({}), false).is_err());

    // A backend tool's card that names a web view: a `byob.web` view with the card's data.
    let (r, _) = turn(&mut c, "chart it", |_, _| {});
    let card = step(&r, "chart").view::<byob_protocol::web::WebCard>().expect("a web card");
    assert_eq!(
        (card.plugin.as_str(), card.view.as_str(), card.data.clone(), card.text.as_str()),
        ("panel", "chartcard", json!({"values": [0, 1]}), "a chart of 2")
    );
    let doc = web_call(&mut c, 6, &card.owner(), "byob/document", json!({}), false).unwrap();
    assert_eq!(doc["html"], "<p>card</p>");
    // A `web:<file>` artifact renderer's page; only files it declares.
    let renderer = WebOwner::Plugin { plugin: "panel".into(), view: "artifact:ui/card.html".into(), key: "notes-1".into() };
    assert_eq!(web_call(&mut c, 7, &renderer, "byob/document", json!({}), false).unwrap()["html"], "<p>card</p>");
    let undeclared = WebOwner::Plugin { plugin: "panel".into(), view: "artifact:backend.py".into(), key: String::new() };
    assert!(web_call(&mut c, 8, &undeclared, "byob/document", json!({}), false).is_err());
}

/// Bridge round trip through the engine: page call → engine → plugin backend → page.
#[test]
#[ignore = "measurement; run with --ignored --nocapture"]
fn measure_bridge_round_trip_through_the_engine() {
    let h = harness(vec![Reply::Text("ok")], None, |home, _| {
        let dir = home.join("plugins/panel");
        fs::create_dir_all(dir.join("ui")).unwrap();
        fs::write(dir.join("backend.py"), PANEL_BACKEND).unwrap();
        fs::write(dir.join("ui/index.html"), "<p>x</p>").unwrap();
        let m = json!({"id": "panel", "name": "Panel", "backend": "backend.py", "views": [{"site": "pane", "id": "panel", "html": "ui/index.html"}]});
        fs::write(dir.join("byob-plugin.json"), m.to_string()).unwrap();
    });
    let (mut c, _) = hello(&h);
    let owner = WebOwner::Plugin { plugin: "panel".into(), view: "panel".into(), key: String::new() };
    web_call(&mut c, 1, &owner, "byob/call", json!({"msg": 0}), false).unwrap();
    let n = 200;
    let start = Instant::now();
    for i in 0..n {
        web_call_spin(&mut c, 10 + i, &owner, "byob/call", json!({"msg": i}));
    }
    let per = start.elapsed() / u32::try_from(n).unwrap();
    let start = Instant::now();
    for i in 0..n {
        web_call_spin(&mut c, 1000 + i, &owner, "ping", json!({}));
    }
    let ping = start.elapsed() / u32::try_from(n).unwrap();
    println!("engine round trip: byob/call to a Python backend {per:?}, engine-only ping {ping:?} ({n} each)");
}

/// [`web_call`] polling without sleeping (for timing).
fn web_call_spin(c: &mut LocalConnection, n: u64, owner: &WebOwner, method: &str, params: Value) -> Value {
    c.send(ClientMsg::WebCall { call: n, owner: owner.clone(), method: method.into(), params, approved: false });
    loop {
        match c.try_recv() {
            Some(ServerMsg::WebReply { call, result, .. }) if call == n => return result.unwrap_or(Value::Null),
            Some(_) => {}
            None => std::thread::yield_now(),
        }
    }
}
