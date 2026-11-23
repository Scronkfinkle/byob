//! `cargo xtask browser-smoke`: does the browser UI really run? `cargo xtask ci` only compiles
//! the wasm, so a UI that panics on start (`std::time::Instant` on wasm32, say) leaves a blank
//! page and a green gate. This builds the bundle, starts `byob --headless` on a free loopback
//! port (throwaway `BYOB_HOME`, an inference URL nothing listens on), opens the page in
//! headless Chromium at a desktop and a phone size, and fails unless, for each:
//! - the app marks `<html data-byob="ready">` (byob-web: running, connected, a frame drawn),
//! - nothing panicked or threw (the console, over the DevTools protocol), and
//! - the screenshot isn't blank (one color, or nearly).
//!
//! Screenshots go to `target/browser-smoke/`. Chromium is `$CHROMIUM`, else `chromium`,
//! `chromium-browser`, `google-chrome` or `google-chrome-stable` on PATH (in CI:
//! `nix shell --inputs-from . nixpkgs#chromium`). Not part of `cargo xtask ci`: it builds the
//! release wasm bundle and needs a browser, so CI runs it as a job of its own.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// How long the page may take to load, start the wasm and connect.
const READY_TIMEOUT: Duration = Duration::from_secs(60);
/// After `ready`: more frames, and time for a late panic to show.
const SETTLE: Duration = Duration::from_millis(1500);
const OUT_DIR: &str = "target/browser-smoke";

struct Viewport {
    name: &'static str,
    width: u32,
    height: u32,
    scale: f64,
    mobile: bool,
}

const VIEWPORTS: &[Viewport] = &[
    Viewport { name: "desktop", width: 1280, height: 800, scale: 1.0, mobile: false },
    Viewport { name: "phone", width: 390, height: 844, scale: 2.0, mobile: true },
];

pub fn run() -> Result<(), String> {
    let started = Instant::now();
    crate::web()?;
    crate::run(&["build", "-p", "byob"])?;
    let chromium = find_chromium()?;

    let tmp = std::env::temp_dir().join(format!("byob-browser-smoke-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
    std::fs::create_dir_all(OUT_DIR).map_err(|e| format!("create {OUT_DIR}: {e}"))?;
    let result = smoke(&chromium, &tmp);
    let _ = std::fs::remove_dir_all(&tmp);
    result?;
    println!("browser-smoke: ok in {:.0?} (screenshots in {OUT_DIR}/)", started.elapsed());
    Ok(())
}

fn smoke(chromium: &str, tmp: &Path) -> Result<(), String> {
    let port = free_port()?;
    let byob_log = tmp.join("byob.log");
    let log_file = std::fs::File::create(&byob_log).map_err(|e| format!("create {}: {e}", byob_log.display()))?;
    let log_err = log_file.try_clone().map_err(|e| format!("byob log: {e}"))?;
    let home = tmp.join("home");
    let byob = Command::new("target/debug/byob")
        .args(["--headless", &format!("127.0.0.1:{port}"), "--web-dir", "apps/byob-web/dist"])
        .args(["--inference-url", "http://127.0.0.1:9/v1", "--model", "x"])
        .env("BYOB_HOME", &home)
        .env("RUST_LOG", "info")
        .stdin(Stdio::null())
        .stdout(log_file)
        .stderr(log_err)
        .spawn()
        .map_err(|e| format!("start target/debug/byob: {e}"))?;
    let mut byob = Reaped(byob);
    let with_log = |e: String| format!("{e}\n--- byob's log ({}):\n{}", byob_log.display(), tail(&byob_log));

    wait_for_port(port, &mut byob.0).map_err(with_log)?;
    let url = format!("http://127.0.0.1:{port}/");
    println!("browser-smoke: byob serves {url}");

    let profile = tmp.join("chromium");
    let chrome_log = tmp.join("chromium.log");
    let mut cdp = Cdp::launch(chromium, &profile, &chrome_log).map_err(|e| format!("{e}\n--- chromium's log:\n{}", tail(&chrome_log)))?;
    for vp in VIEWPORTS {
        check_viewport(&mut cdp, &url, vp).map_err(with_log)?;
    }
    if let Ok(Some(status)) = byob.0.try_wait() {
        return Err(with_log(format!("byob exited during the test ({status})")));
    }
    Ok(())
}

fn check_viewport(cdp: &mut Cdp, url: &str, vp: &Viewport) -> Result<(), String> {
    let shot_path = PathBuf::from(OUT_DIR).join(format!("{}.png", vp.name));
    let _ = std::fs::remove_file(&shot_path);
    let target = cdp.call("Target.createTarget", json!({ "url": "about:blank" }), None)?;
    let target = target.get("targetId").and_then(Value::as_str).ok_or("Target.createTarget: no targetId")?.to_owned();
    let attached = cdp.call("Target.attachToTarget", json!({ "targetId": target, "flatten": true }), None)?;
    let session = attached.get("sessionId").and_then(Value::as_str).ok_or("Target.attachToTarget: no sessionId")?.to_owned();
    let s = Some(session.as_str());
    for domain in ["Runtime.enable", "Log.enable", "Page.enable"] {
        cdp.call(domain, json!({}), s)?;
    }
    cdp.call("Emulation.setDeviceMetricsOverride", json!({ "width": vp.width, "height": vp.height, "deviceScaleFactor": vp.scale, "mobile": vp.mobile }), s)?;
    if vp.mobile {
        cdp.call("Emulation.setTouchEmulationEnabled", json!({ "enabled": true, "maxTouchPoints": 5 }), s)?;
    }
    let label = format!("{} ({}x{})", vp.name, vp.width, vp.height);
    cdp.call("Page.navigate", json!({ "url": url }), s)?;

    let start = Instant::now();
    let fail = |cdp: &mut Cdp, why: String| -> String {
        let shot = screenshot(cdp, s, &shot_path).map_or_else(|e| format!("(no screenshot: {e})"), |_| format!("screenshot: {}", shot_path.display()));
        let console = cdp.console(&session);
        let console = if console.is_empty() { "(nothing)".to_owned() } else { console.join("\n") };
        format!("browser UI, {label}: {why}\n{shot}\n--- the page's console:\n{console}")
    };
    loop {
        if let Some(p) = cdp.problem(&session) {
            return Err(fail(cdp, format!("the page failed: {p}")));
        }
        let ready = cdp.call("Runtime.evaluate", json!({ "expression": "document.documentElement.dataset.byob || ''", "returnByValue": true }), s)?;
        if ready.pointer("/result/value").and_then(Value::as_str) == Some("ready") {
            break;
        }
        if start.elapsed() > READY_TIMEOUT {
            return Err(fail(
                cdp,
                format!("no <html data-byob=\"ready\"> after {READY_TIMEOUT:?}: the wasm didn't start, didn't connect, or stopped before a frame"),
            ));
        }
        cdp.pump(Duration::from_millis(200))?;
    }
    let to_ready = start.elapsed();
    cdp.pump(SETTLE)?;
    if let Some(p) = cdp.problem(&session) {
        return Err(fail(cdp, format!("the page failed after starting: {p}")));
    }
    let png = screenshot(cdp, s, &shot_path)?;
    let (colors, dominant) = color_stats(&png)?;
    // A blank page is one color (the page's background); byob's interface has text, icons and
    // panels: hundreds of colors with antialiasing.
    if colors < 16 || dominant > 0.98 {
        return Err(fail(cdp, format!("the screenshot looks blank: {colors} colors, the commonest covers {:.1}% of it", dominant * 100.0)));
    }
    let _ = cdp.call("Target.closeTarget", json!({ "targetId": target }), None);
    println!("browser-smoke: {label} ready after {to_ready:.1?}; {colors} colors, commonest {:.1}%; {}", dominant * 100.0, shot_path.display());
    Ok(())
}

fn screenshot(cdp: &mut Cdp, session: Option<&str>, path: &Path) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    let shot = cdp.call("Page.captureScreenshot", json!({ "format": "png" }), session)?;
    let data = shot.get("data").and_then(Value::as_str).ok_or("Page.captureScreenshot: no data")?;
    let png = base64::engine::general_purpose::STANDARD.decode(data).map_err(|e| format!("screenshot base64: {e}"))?;
    std::fs::write(path, &png).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(png)
}

/// Distinct colors, and the share of the commonest one.
fn color_stats(png: &[u8]) -> Result<(usize, f64), String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(|e| format!("screenshot png: {e}"))?;
    let mut buf = vec![0; reader.output_buffer_size().ok_or("screenshot png: too big")?];
    let info = reader.next_frame(&mut buf).map_err(|e| format!("screenshot png: {e}"))?;
    let channels = info.color_type.samples();
    let mut counts: HashMap<[u8; 3], usize> = HashMap::new();
    let mut total = 0usize;
    for px in buf.get(..info.buffer_size()).unwrap_or_default().chunks_exact(channels.max(1)) {
        let rgb = match px {
            [r, g, b, ..] => [*r, *g, *b],
            [v, ..] => [*v, *v, *v],
            [] => continue,
        };
        *counts.entry(rgb).or_default() += 1;
        total += 1;
    }
    let top = counts.values().copied().max().unwrap_or(0);
    let dominant = if total == 0 { 1.0 } else { top as f64 / total as f64 };
    Ok((counts.len(), dominant))
}

fn find_chromium() -> Result<String, String> {
    if let Some(c) = std::env::var_os("CHROMIUM") {
        return Ok(c.to_string_lossy().into_owned());
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    for name in ["chromium", "chromium-browser", "google-chrome", "google-chrome-stable"] {
        if std::env::split_paths(&path).any(|d| d.join(name).is_file()) {
            return Ok(name.to_owned());
        }
    }
    Err("no Chromium found: install it, put it on PATH or set CHROMIUM (nix: `nix shell --inputs-from . nixpkgs#chromium`)".into())
}

fn free_port() -> Result<u16, String> {
    let l = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("find a free port: {e}"))?;
    l.local_addr().map(|a| a.port()).map_err(|e| format!("find a free port: {e}"))
}

fn wait_for_port(port: u16, byob: &mut Child) -> Result<(), String> {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Ok(());
        }
        if let Ok(Some(status)) = byob.try_wait() {
            return Err(format!("byob exited before serving ({status})"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!("byob didn't listen on 127.0.0.1:{port} within 30s"))
}

/// The last lines of a log file.
fn tail(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines.get(lines.len().saturating_sub(40)..).unwrap_or_default().join("\n")
}

/// A child process killed when dropped.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Chromium, driven over its DevTools protocol (a WebSocket on loopback).
struct Cdp {
    ws: tungstenite::WebSocket<TcpStream>,
    next_id: u64,
    /// Events received so far (`method`, `params`, `sessionId`).
    events: Vec<Value>,
    _browser: Reaped,
}

impl Cdp {
    fn launch(chromium: &str, profile: &Path, log: &Path) -> Result<Self, String> {
        let log_file = std::fs::File::create(log).map_err(|e| format!("create {}: {e}", log.display()))?;
        let browser = Command::new(chromium)
            .args([
                "--headless=new",
                "--remote-debugging-port=0",
                "--use-angle=swiftshader",
                "--enable-unsafe-swiftshader",
                // CI runners may not allow the sandbox's user namespaces; the page is our own.
                "--no-sandbox",
                "--no-first-run",
                "--no-default-browser-check",
                "--hide-scrollbars",
                "--mute-audio",
            ])
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg("about:blank")
            // The Nix dev shell's library path breaks Chromium from nixpkgs.
            .env_remove("LD_LIBRARY_PATH")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log_file)
            .spawn()
            .map_err(|e| format!("start {chromium}: {e}"))?;
        let mut browser = Reaped(browser);
        // Chromium writes its port and the browser's WebSocket path here once it listens.
        let active = profile.join("DevToolsActivePort");
        let start = Instant::now();
        let (port, path) = loop {
            if let Ok(text) = std::fs::read_to_string(&active)
                && let [port, path, ..] = text.lines().collect::<Vec<_>>()[..]
                && let Ok(port) = port.trim().parse::<u16>()
            {
                break (port, path.trim().to_owned());
            }
            if let Ok(Some(status)) = browser.0.try_wait() {
                return Err(format!("{chromium} exited on start ({status})"));
            }
            if start.elapsed() > Duration::from_secs(30) {
                return Err(format!("{chromium} didn't open its DevTools port within 30s"));
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let stream = TcpStream::connect(("127.0.0.1", port)).map_err(|e| format!("connect to DevTools: {e}"))?;
        stream.set_read_timeout(Some(Duration::from_secs(30))).map_err(|e| format!("DevTools socket: {e}"))?;
        let (ws, _) = tungstenite::client::client(format!("ws://127.0.0.1:{port}{path}"), stream).map_err(|e| format!("DevTools handshake: {e}"))?;
        ws.get_ref().set_read_timeout(Some(Duration::from_millis(100))).map_err(|e| format!("DevTools socket: {e}"))?;
        Ok(Self { ws, next_id: 0, events: Vec::new(), _browser: browser })
    }

    /// Call a method and wait (30s at most) for its result.
    fn call(&mut self, method: &str, params: Value, session: Option<&str>) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let (Some(s), Some(obj)) = (session, msg.as_object_mut()) {
            obj.insert("sessionId".into(), Value::String(s.to_owned()));
        }
        self.ws.send(tungstenite::Message::text(msg.to_string())).map_err(|e| format!("DevTools {method}: {e}"))?;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(30) {
            let Some(reply) = self.read()? else { continue };
            if reply.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(err) = reply.get("error") {
                    return Err(format!("DevTools {method}: {err}"));
                }
                return Ok(reply.get("result").cloned().unwrap_or(Value::Null));
            }
        }
        Err(format!("DevTools {method}: no answer within 30s"))
    }

    /// Collect events for `dur`.
    fn pump(&mut self, dur: Duration) -> Result<(), String> {
        let start = Instant::now();
        while start.elapsed() < dur {
            self.read()?;
        }
        Ok(())
    }

    /// One message (events are kept), or None after a short wait.
    fn read(&mut self) -> Result<Option<Value>, String> {
        match self.ws.read() {
            Ok(tungstenite::Message::Text(t)) => {
                let v: Value = serde_json::from_str(t.as_str()).map_err(|e| format!("DevTools sent bad JSON: {e}"))?;
                if v.get("method").is_some() {
                    self.events.push(v.clone());
                }
                Ok(Some(v))
            }
            Ok(_) => Ok(None),
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => Ok(None),
            Err(e) => Err(format!("DevTools connection: {e}")),
        }
    }

    /// The first panic or uncaught exception in this page.
    fn problem(&self, session: &str) -> Option<String> {
        self.of(session).find_map(|(method, params)| match method {
            "Runtime.exceptionThrown" => Some(format!("uncaught exception: {}", brief(&exception_text(params)))),
            _ => console_line(method, params).filter(|l| l.contains("panicked")).map(|l| format!("panic: {}", brief(&l))),
        })
    }

    /// What the page logged (console and browser log), oldest first.
    fn console(&self, session: &str) -> Vec<String> {
        self.of(session)
            .filter_map(|(method, params)| match method {
                "Runtime.exceptionThrown" => Some(format!("[exception] {}", brief(&exception_text(params)))),
                _ => console_line(method, params).map(|l| brief(&l)),
            })
            .collect()
    }

    fn of<'a>(&'a self, session: &'a str) -> impl Iterator<Item = (&'a str, &'a Value)> + 'a {
        self.events.iter().filter(move |e| e.get("sessionId").and_then(Value::as_str) == Some(session)).filter_map(|e| {
            let method = e.get("method").and_then(Value::as_str)?;
            Some((method, e.get("params")?))
        })
    }
}

fn console_line(method: &str, params: &Value) -> Option<String> {
    match method {
        "Runtime.consoleAPICalled" => {
            let kind = params.get("type").and_then(Value::as_str).unwrap_or("log");
            let args = params.get("args").and_then(Value::as_array).cloned().unwrap_or_default();
            let text: Vec<String> = args
                .iter()
                .map(|a| match a.get("value") {
                    Some(Value::String(s)) => s.clone(),
                    Some(v) => v.to_string(),
                    None => a.get("description").and_then(Value::as_str).unwrap_or("?").to_owned(),
                })
                .collect();
            Some(format!("[{kind}] {}", text.join(" ")))
        }
        "Log.entryAdded" => {
            let entry = params.get("entry")?;
            let level = entry.get("level").and_then(Value::as_str).unwrap_or("log");
            let text = entry.get("text").and_then(Value::as_str).unwrap_or_default();
            Some(format!("[{level}] {text}"))
        }
        _ => None,
    }
}

/// A console message without its stack trace (wasm frames say nothing in a release build).
fn brief(text: &str) -> String {
    text.lines().map(str::trim).take_while(|l| *l != "Stack:" && !l.starts_with("at ")).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ")
}

fn exception_text(params: &Value) -> String {
    let d = params.get("exceptionDetails");
    d.and_then(|d| d.pointer("/exception/description"))
        .or_else(|| d.and_then(|d| d.get("text")))
        .and_then(Value::as_str)
        .unwrap_or("(no description)")
        .to_owned()
}
