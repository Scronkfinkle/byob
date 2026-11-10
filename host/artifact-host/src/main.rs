//! byob-artifact-host: renders one HTML app artifact with Servo, offscreen, for byob.
//!
//! byob spawns one of these per live app and talks to it over stdin/stdout with
//! `byob-artifact-wire` frames: `Load`/`Resize`/`Input`/`Theme`/`Ping` in, `Frame`s (CPU
//! readback of a software GL surface, only when the pixels changed), `ContentHeight`,
//! `Error`, `Title`, `Cursor`, `Pong` out. byob kills the process when the page stops
//! answering pings (a runaway script) or the app goes out of view, so this side keeps no
//! state worth saving and simply exits when stdin closes.
//!
//! Sandbox (proportionate; the app is the model's own code, not a website): the document is
//! a `data:` URL (opaque origin, so no storage that outlives the process); every navigation
//! after it and every resource load that isn't `data:`/`blob:`/`about:` is refused in
//! [`WebViewDelegate::load_web_resource`]; and Servo's HTTP(S) proxy points at a dead port
//! so anything that slips past that hook (e.g. `sendBeacon`) fails too. New windows,
//! permissions and dialogs are denied.

use std::cell::{Cell, RefCell};
use std::io::{self, BufReader, BufWriter, Write};
use std::rc::Rc;
use std::str::FromStr as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use euclid::Scale;
use servo::{
    Code, Cursor, DeviceIntRect, DevicePoint, EmbedderControl, EventLoopWaker, InputEvent, Key, KeyState, KeyboardEvent, Location, Modifiers, MouseButton,
    MouseButtonAction, MouseButtonEvent, MouseLeftViewportEvent, MouseMoveEvent, NamedKey, NavigationRequest, RenderingContext, Servo, ServoBuilder,
    SoftwareRenderingContext, WebResourceLoad, WebView, WebViewBuilder, WebViewDelegate, WebViewPoint, WheelDelta, WheelEvent, WheelMode,
};
use byob_artifact_wire::{self as wire, Event, Input, Request};

/// How often the document height is measured.
const MEASURE_EVERY: Duration = Duration::from_millis(300);
/// At most this many frames per second go to byob.
const MIN_FRAME_GAP: Duration = Duration::from_millis(16);
/// Longest the loop sleeps while hidden.
const HIDDEN_WAIT: Duration = Duration::from_millis(500);
/// What the page's error reports start with (see byob-server's artifact shim).
const ERROR_MARK: &str = "\u{1}byob-error ";

fn log(msg: impl AsRef<str>) {
    eprintln!("[byob-artifact-host {}] {}", std::process::id(), msg.as_ref());
}

enum Msg {
    Req(Request),
    /// byob closed our stdin (or sent garbage): exit.
    Gone,
    Wake,
}

/// Servo asks to be spun again; one pending wake-up is enough.
#[derive(Clone)]
struct Waker {
    tx: Sender<Msg>,
    pending: Arc<AtomicBool>,
}

impl EventLoopWaker for Waker {
    fn clone_box(&self) -> Box<dyn EventLoopWaker> {
        Box::new(self.clone())
    }
    fn wake(&self) {
        if !self.pending.swap(true, Ordering::AcqRel) {
            let _ = self.tx.send(Msg::Wake);
        }
    }
}

/// The page's delegate: what Servo tells us, queued for the main loop.
#[derive(Default)]
struct Page {
    frame_ready: Cell<bool>,
    /// The next `data:` navigation is ours (a `Load`); everything else is refused.
    allow_nav: Cell<bool>,
    out: RefCell<Vec<Event>>,
}

impl Page {
    fn push(&self, e: Event) {
        self.out.borrow_mut().push(e);
    }
}

fn cursor(c: Cursor) -> wire::Cursor {
    match c {
        Cursor::Pointer => wire::Cursor::Pointer,
        Cursor::Text | Cursor::VerticalText => wire::Cursor::Text,
        Cursor::Crosshair | Cursor::Cell => wire::Cursor::Crosshair,
        Cursor::Move | Cursor::AllScroll => wire::Cursor::Move,
        Cursor::Grab => wire::Cursor::Grab,
        Cursor::Grabbing => wire::Cursor::Grabbing,
        Cursor::NotAllowed | Cursor::NoDrop => wire::Cursor::NotAllowed,
        Cursor::None => wire::Cursor::None,
        _ => wire::Cursor::Default,
    }
}

/// `"<line> <message>"` after [`ERROR_MARK`].
fn parse_error(console: &str) -> Option<Event> {
    let rest = console.strip_prefix(ERROR_MARK)?;
    let (line, message) = rest.split_once(' ').unwrap_or(("0", rest));
    Some(Event::Error { message: message.chars().take(2000).collect(), line: line.parse().unwrap_or(0) })
}

impl WebViewDelegate for Page {
    fn notify_new_frame_ready(&self, _: WebView) {
        self.frame_ready.set(true);
    }
    fn notify_cursor_changed(&self, _: WebView, c: Cursor) {
        self.push(Event::Cursor(cursor(c)));
    }
    fn notify_page_title_changed(&self, _: WebView, title: Option<String>) {
        if let Some(t) = title {
            self.push(Event::Title(t.chars().take(200).collect()));
        }
    }
    fn show_console_message(&self, _: WebView, level: servo::ConsoleLogLevel, message: String) {
        match parse_error(&message) {
            Some(e) => self.push(e),
            None if std::env::var_os("BYOB_ARTIFACT_HOST_CONSOLE").is_some() => log(format!("console {level:?}: {message}")),
            None => {}
        }
    }
    fn request_navigation(&self, _: WebView, req: NavigationRequest) {
        if req.url.scheme() == "data" && self.allow_nav.replace(false) {
            req.allow();
        } else {
            log(format!("refused navigation to {}", req.url));
            req.deny();
        }
    }
    fn load_web_resource(&self, _: WebView, load: WebResourceLoad) {
        let url = load.request().url.clone();
        if matches!(url.scheme(), "data" | "blob" | "about") {
            return; // not intercepted: loads normally
        }
        log(format!("refused load of {url}"));
        let resp = servo::WebResourceResponse::new(url.into_url());
        load.intercept(resp).cancel();
    }
    fn request_create_new(&self, _: WebView, _: servo::CreateNewWebViewRequest) {
        log("refused window.open");
    }
    fn request_permission(&self, _: WebView, req: servo::PermissionRequest) {
        req.deny();
    }
    fn show_embedder_control(&self, _: WebView, control: EmbedderControl) {
        if let EmbedderControl::SimpleDialog(d) = control {
            d.dismiss();
        }
    }
    fn notify_crashed(&self, _: WebView, reason: String, _: Option<String>) {
        self.push(Event::Error { message: format!("The app crashed: {reason}"), line: 0 });
    }
}

/// Works around a Servo (0.8) bug: a *collapsed* selection (a caret) whose container is a
/// replaced element — `<canvas>`, `<img>`, `<video>`, … — marks that element as selected, so it
/// is painted with the selection tint, as if the whole thing were highlighted. A plain click
/// on a `<canvas>` does exactly that: Servo's mousedown default action, finding no text under
/// the pointer, puts the caret at offset 0 *inside* the clicked node (`Document::
/// handle_mousedown_event` → `RootedSelectionBoundary::start_of`), and the visible-selection
/// update then flags that node (`VisibleSelectionFlagUpdate`, `Inside(canvas)`). Browsers never
/// paint a caret as a highlight. So: when the caret lands in such an element, move it to just
/// before the element, which Servo doesn't flag. Real (non-collapsed) selections, and carets
/// anywhere else, are left alone, so text selection in apps works as before.
const SELECTION_FIX: &str = "<script>document.addEventListener('selectionchange',()=>{\
    const s=getSelection(),n=s&&s.isCollapsed&&s.anchorNode;if(!n)return;\
    const e=(n.nodeType===1?n:n.parentElement)?.closest('canvas,img,video,audio,iframe,embed,object,svg'),p=e&&e.parentNode;\
    if(p)s.collapse(p,Array.prototype.indexOf.call(p.childNodes,e))})</script>";

/// The document with [`SELECTION_FIX`] first (after the doctype, so standards mode stays).
fn with_fixes(html: &str) -> String {
    let start = html.trim_start();
    if start.get(..9).is_some_and(|s| s.eq_ignore_ascii_case("<!doctype")) {
        let end = start.find('>').map_or(start.len(), |i| i.saturating_add(1));
        format!("{}{SELECTION_FIX}{}", start.get(..end).unwrap_or_default(), start.get(end..).unwrap_or_default())
    } else {
        format!("{SELECTION_FIX}{html}")
    }
}

/// `data:` URL for the document, theme in the hash for the shim to apply before first paint.
fn data_url(html: &str, theme: &wire::Theme) -> Option<url::Url> {
    let html = with_fixes(html);
    let body: String = url::form_urlencoded::byte_serialize(html.as_bytes()).collect();
    let hash: Vec<String> = safe_vars(theme).map(|(k, v)| format!("{k}={}", v.replace('#', "%23").replace(' ', "%20"))).collect();
    // form_urlencoded writes spaces as '+'; data: URLs need %20.
    url::Url::parse(&format!("data:text/html;charset=utf-8,{}#{}", body.replace('+', "%20"), hash.join("&"))).ok()
}

/// Theme variables that look like ours (byob sends its own colours; this keeps the JS below
/// from ever seeing anything else).
fn safe_vars(theme: &wire::Theme) -> impl Iterator<Item = (&str, &str)> {
    theme.vars.iter().map(|(k, v)| (k.as_str(), v.as_str())).filter(|(k, v)| {
        k.strip_prefix("--byob-").is_some_and(|n| (1..=30).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'))
            && v.len() <= 60
            && v.bytes().all(|b| b.is_ascii_alphanumeric() || b"#(),. %-".contains(&b))
    })
}

/// Script that applies the theme to the live page.
fn theme_js(theme: &wire::Theme) -> String {
    let sets: String = safe_vars(theme).map(|(k, v)| format!("s.setProperty('{k}','{v}');")).collect();
    let scheme = if theme.dark { "dark" } else { "light" };
    format!("(()=>{{const s=document.documentElement.style;{sets}s.colorScheme='{scheme}'}})()")
}

const MEASURE_JS: &str = "(()=>{const b=document.body,d=document.documentElement;if(!b)return 0;\
    const m=parseFloat(getComputedStyle(b).marginBottom)||0;return Math.ceil(Math.max(b.getBoundingClientRect().bottom+m+(scrollY||0),b.scrollHeight))})()";

fn point(x: f32, y: f32) -> WebViewPoint {
    WebViewPoint::Device(DevicePoint::new(x, y))
}

fn modifiers(m: wire::Mods) -> Modifiers {
    let mut r = Modifiers::empty();
    for (bit, flag) in
        [(wire::Mods::SHIFT, Modifiers::SHIFT), (wire::Mods::CTRL, Modifiers::CONTROL), (wire::Mods::ALT, Modifiers::ALT), (wire::Mods::META, Modifiers::META)]
    {
        if m.has(bit) {
            r |= flag;
        }
    }
    r
}

/// The physical key a typed character most likely came from (games read `code`).
fn char_code(c: char) -> Code {
    let name = match c {
        ' ' => "Space".to_owned(),
        c if c.is_ascii_alphabetic() => format!("Key{}", c.to_ascii_uppercase()),
        c if c.is_ascii_digit() => format!("Digit{c}"),
        _ => return Code::Unidentified,
    };
    Code::from_str(&name).unwrap_or(Code::Unidentified)
}

fn key_event(state: KeyState, key: Key, code: Code, m: Modifiers, repeat: bool) -> InputEvent {
    InputEvent::Keyboard(KeyboardEvent::new_without_event(state, key, code, Location::Standard, m, repeat, false))
}

fn input_events(input: Input) -> Vec<InputEvent> {
    match input {
        Input::MouseMove { x, y } => vec![InputEvent::MouseMove(MouseMoveEvent::new(point(x, y)))],
        Input::MouseButton { button, down, x, y } => {
            let b = match button {
                wire::Button::Primary => MouseButton::Primary,
                wire::Button::Secondary => MouseButton::Secondary,
                wire::Button::Middle => MouseButton::Auxiliary,
            };
            let act = if down { MouseButtonAction::Down } else { MouseButtonAction::Up };
            vec![InputEvent::MouseButton(MouseButtonEvent::new(act, b, point(x, y)))]
        }
        Input::Wheel { dx, dy, x, y } => {
            vec![InputEvent::Wheel(WheelEvent::new(WheelDelta { x: f64::from(dx), y: f64::from(dy), z: 0.0, mode: WheelMode::DeltaPixel }, point(x, y)))]
        }
        Input::MouseLeave => vec![InputEvent::MouseLeftViewport(MouseLeftViewportEvent::default())],
        Input::Key(k) => {
            let key = Key::from_str(&k.key).unwrap_or(Key::Named(NamedKey::Unidentified));
            let code = Code::from_str(&k.code).unwrap_or(Code::Unidentified);
            let state = if k.down { KeyState::Down } else { KeyState::Up };
            vec![key_event(state, key, code, modifiers(k.mods), k.repeat)]
        }
        Input::Text(text) => text
            .chars()
            .take(1000)
            .flat_map(|c| {
                let key = Key::Character(c.to_string());
                [KeyState::Down, KeyState::Up].map(|s| key_event(s, key.clone(), char_code(c), Modifiers::empty(), false))
            })
            .collect(),
    }
}

struct View {
    rc: Rc<SoftwareRenderingContext>,
    webview: WebView,
    scale: f32,
}

struct Host {
    servo: Servo,
    page: Rc<Page>,
    view: Option<View>,
    theme: wire::Theme,
    focused: bool,
    last_frame: Option<(u32, u32, Vec<u8>)>,
    last_paint: Instant,
    height: Rc<Cell<Option<f64>>>,
    sent_height: f32,
    measuring: Rc<Cell<bool>>,
    next_measure: Instant,
    /// Minimized or in the background (`Request::Visible(false)`): the page keeps running,
    /// nothing is painted, read back or measured.
    hidden: bool,
    quit: bool,
}

impl Host {
    fn handle(&mut self, req: Request) {
        match req {
            Request::Load(load) => self.load(*load),
            Request::Resize(vp) => {
                if let Some(v) = &mut self.view {
                    let (w, h) = (vp.width.clamp(1, 8192), vp.height.clamp(1, 8192));
                    v.webview.resize(dpi::PhysicalSize::new(w, h));
                    if (v.scale - vp.scale).abs() > f32::EPSILON && vp.scale > 0.0 {
                        v.scale = vp.scale;
                        v.webview.set_hidpi_scale_factor(Scale::new(vp.scale));
                    }
                }
            }
            Request::Input(input) => {
                if let Some(v) = &self.view {
                    for e in input_events(input) {
                        v.webview.notify_input_event(e);
                    }
                }
            }
            Request::Focus(f) => {
                self.focused = f;
                if let Some(v) = &self.view {
                    v.webview.set_focused(f);
                }
            }
            Request::Theme(theme) => {
                self.theme = theme;
                self.apply_theme();
            }
            Request::Ping(n) => match &self.view {
                // Answered by the page's script thread, so a busy loop there stops the pongs.
                Some(v) => {
                    let page = Rc::clone(&self.page);
                    v.webview.evaluate_javascript("0", move |_| page.push(Event::Pong(n)));
                }
                None => self.page.push(Event::Pong(n)),
            },
            Request::Shutdown => self.quit = true,
            Request::Visible(visible) => self.set_visible(visible),
        }
    }

    fn set_visible(&mut self, visible: bool) {
        if self.hidden != visible {
            return;
        }
        self.hidden = !visible;
        let Some(v) = &self.view else { return };
        if visible {
            v.webview.show();
            // Paint now: byob dropped nothing, but the page may have changed meanwhile.
            self.page.frame_ready.set(true);
            self.next_measure = Instant::now();
        } else {
            v.webview.hide();
        }
    }

    fn apply_theme(&self) {
        if let Some(v) = &self.view {
            v.webview.notify_theme_change(if self.theme.dark { servo::Theme::Dark } else { servo::Theme::Light });
            v.webview.evaluate_javascript(theme_js(&self.theme), |_| {});
        }
    }

    fn load(&mut self, load: wire::Load) {
        self.theme = load.theme;
        let Some(url) = data_url(&load.html, &self.theme) else {
            self.page.push(Event::Error { message: "couldn't encode the app as a data: URL".into(), line: 0 });
            return;
        };
        self.page.allow_nav.set(true);
        self.last_frame = None;
        self.sent_height = 0.0;
        let vp = load.viewport;
        let scale = if vp.scale > 0.0 { vp.scale } else { 1.0 };
        if let Some(v) = &self.view {
            v.webview.load(url);
            return;
        }
        let size = dpi::PhysicalSize::new(vp.width.clamp(1, 8192), vp.height.clamp(1, 8192));
        let rc = match SoftwareRenderingContext::new(size) {
            Ok(rc) => Rc::new(rc),
            Err(e) => {
                self.page.push(Event::Error { message: format!("no software rendering context: {e:?}"), line: 0 });
                return;
            }
        };
        let webview = WebViewBuilder::new(&self.servo, rc.clone()).url(url).hidpi_scale_factor(Scale::new(scale)).delegate(self.page.clone()).build();
        webview.set_focused(self.focused);
        if self.hidden {
            webview.hide();
        }
        self.view = Some(View { rc, webview, scale });
        self.apply_theme();
    }

    /// After spinning Servo: send a new frame if there is one, measure the height.
    fn tick(&mut self) {
        let Some(v) = &self.view else { return };
        if self.hidden {
            return;
        }
        if self.page.frame_ready.get() && self.last_paint.elapsed() >= MIN_FRAME_GAP {
            self.page.frame_ready.set(false);
            self.last_paint = Instant::now();
            v.webview.paint();
            let _ = v.rc.make_current();
            let size = v.rc.size();
            let rect = DeviceIntRect::from_origin_and_size(
                euclid::Point2D::zero(),
                euclid::Size2D::new(i32::try_from(size.width).unwrap_or(1), i32::try_from(size.height).unwrap_or(1)),
            );
            let painted = self.last_paint.elapsed();
            if let Some(img) = v.rc.read_to_image(rect) {
                v.rc.present();
                let (w, h) = (img.width(), img.height());
                let rgba = img.into_raw();
                let same = self.last_frame.as_ref().is_some_and(|(lw, lh, l)| *lw == w && *lh == h && *l == rgba);
                if std::env::var_os("BYOB_ARTIFACT_HOST_STATS").is_some() {
                    log(format!(
                        "frame {w}x{h}: paint {painted:?}, readback+compare {:?}, changed {}",
                        self.last_paint.elapsed().saturating_sub(painted),
                        !same
                    ));
                }
                if !same {
                    self.page.push(Event::Frame(wire::Frame { width: w, height: h, rgba: rgba.clone() }));
                    self.last_frame = Some((w, h, rgba));
                }
            }
        }
        if let Some(h) = self.height.take() {
            #[allow(clippy::cast_possible_truncation)]
            let h = h.clamp(0.0, 100_000.0) as f32;
            if (h - self.sent_height).abs() >= 1.0 {
                self.sent_height = h;
                self.page.push(Event::ContentHeight(h));
            }
        }
        if !self.measuring.get() && Instant::now() >= self.next_measure {
            self.next_measure = Instant::now() + MEASURE_EVERY;
            self.measuring.set(true);
            let (slot, busy) = (Rc::clone(&self.height), Rc::clone(&self.measuring));
            v.webview.evaluate_javascript(MEASURE_JS, move |r| {
                if let Ok(servo::JSValue::Number(n)) = r
                    && n.is_finite()
                {
                    slot.set(Some(n));
                }
                busy.set(false);
            });
        }
    }

    /// How long the loop may sleep.
    fn wait(&self) -> Duration {
        if self.hidden {
            // Requests (pings included) and Servo's own wake-ups still end the wait early.
            return HIDDEN_WAIT;
        }
        if self.page.frame_ready.get() {
            return MIN_FRAME_GAP.saturating_sub(self.last_paint.elapsed());
        }
        self.next_measure.saturating_duration_since(Instant::now()).max(Duration::from_millis(5))
    }
}

fn prefs() -> servo::Preferences {
    servo::Preferences {
        // Defence in depth: HTTP(S) that gets past load_web_resource hits a dead proxy.
        network_http_proxy_uri: "http://127.0.0.1:9".into(),
        network_https_proxy_uri: "http://127.0.0.1:9".into(),
        dom_geolocation_enabled: false,
        dom_notification_enabled: false,
        dom_serviceworker_enabled: false,
        dom_webrtc_enabled: false,
        dom_bluetooth_enabled: false,
        dom_indexeddb_enabled: false,
        // Small task tools, one process each: fewer threads, less memory.
        layout_threads: 1,
        thread_pool_workers_max: 2,
        thread_pool_async_runtime_workers_max: 1,
        thread_pool_canvas_workers: 0,
        thread_pool_webrender_workers_max: 2,
        perf_thread_boost_enabled: false,
        ..servo::Preferences::default()
    }
}

fn main() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let (tx, rx) = mpsc::channel();
    let input = tx.clone();
    let _ = std::thread::Builder::new().name("stdin".into()).spawn(move || {
        let mut r = BufReader::new(io::stdin().lock());
        loop {
            match wire::recv::<Request>(&mut r) {
                Ok(Some(req)) => {
                    if input.send(Msg::Req(req)).is_err() {
                        return;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    log(format!("stdin: {e}"));
                    break;
                }
            }
        }
        let _ = input.send(Msg::Gone);
    });
    let waker = Waker { tx, pending: Arc::default() };
    let pending = Arc::clone(&waker.pending);
    let servo = ServoBuilder::default().preferences(prefs()).event_loop_waker(Box::new(waker)).build();
    servo.setup_logging();
    let mut out = BufWriter::new(io::stdout().lock());
    if wire::send(&mut out, &Event::Ready).is_err() || wire::send(&mut out, &Event::Features(wire::Features::ALL)).is_err() {
        return;
    }
    let mut host = Host {
        servo,
        page: Rc::default(),
        view: None,
        theme: wire::Theme::default(),
        focused: false,
        last_frame: None,
        last_paint: Instant::now(),
        height: Rc::default(),
        sent_height: 0.0,
        measuring: Rc::default(),
        next_measure: Instant::now(),
        hidden: false,
        quit: false,
    };
    loop {
        let first = match rx.recv_timeout(host.wait()) {
            Ok(m) => Some(m),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        for m in first.into_iter().chain(rx.try_iter()) {
            match m {
                Msg::Req(r) => host.handle(r),
                Msg::Gone => host.quit = true,
                Msg::Wake => {}
            }
        }
        if host.quit {
            break;
        }
        pending.store(false, Ordering::Release);
        host.servo.spin_event_loop();
        host.tick();
        let events = std::mem::take(&mut *host.page.out.borrow_mut());
        for e in events {
            if wire::send(&mut out, &e).is_err() {
                // byob is gone.
                std::process::exit(0);
            }
        }
    }
    let _ = out.flush();
    // Nothing to save; don't wait for Servo's threads to wind down.
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_from_the_console() {
        assert_eq!(parse_error("\u{1}byob-error 12 x is not defined"), Some(Event::Error { message: "x is not defined".into(), line: 12 }));
        assert_eq!(parse_error("\u{1}byob-error oops"), Some(Event::Error { message: "oops".into(), line: 0 }));
        assert_eq!(parse_error("plain log"), None);
    }

    #[test]
    fn theme_vars_are_filtered() {
        let theme = wire::Theme {
            vars: vec![
                ("--byob-bg".into(), "#16171a".into()),
                ("--byob-x".into(), "red');alert(1);('".into()),
                ("color".into(), "red".into()),
                ("--byob-fg".into(), "rgb(1, 2, 3)".into()),
            ],
            dark: false,
        };
        let js = theme_js(&theme);
        assert!(js.contains("'--byob-bg','#16171a'") && js.contains("'--byob-fg','rgb(1, 2, 3)'") && js.contains("'light'"), "{js}");
        assert!(!js.contains("alert") && !js.contains("'color'"), "{js}");
        let url = data_url("<p>a b+c#d</p>", &theme).unwrap();
        let s = url.as_str();
        assert!(s.starts_with("data:text/html;charset=utf-8,%3Cscript%3E"), "{s}");
        assert!(s.ends_with("%3Cp%3Ea%20b%2Bc%23d%3C%2Fp%3E#--byob-bg=%2316171a&--byob-fg=rgb(1,%202,%203)"), "{s}");
    }

    #[test]
    fn the_selection_fix_goes_first() {
        assert!(with_fixes("<p>hi</p>").starts_with(SELECTION_FIX));
        let doc = with_fixes("  <!DOCTYPE html><html><p>hi</p>");
        assert!(doc.starts_with("<!DOCTYPE html><script>"), "{doc}");
        assert!(doc.ends_with("</script><html><p>hi</p>"), "{doc}");
        assert_eq!(with_fixes("<!doctype"), format!("<!doctype{SELECTION_FIX}"));
    }

    #[test]
    fn typed_text_becomes_key_presses() {
        let events = input_events(Input::Text("aZ 1".into()));
        assert_eq!(events.len(), 8);
        assert_eq!(char_code('z'), Code::KeyZ);
        assert_eq!(char_code('7'), Code::Digit7);
        assert_eq!(char_code('é'), Code::Unidentified);
    }
}
