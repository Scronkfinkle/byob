//! Scripted runs for headless testing (see `scripts/headless.sh`): send messages on start,
//! screenshot once the conversation settles, then exit.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Give up waiting for the conversation to settle after this long and shoot anyway.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(180);
/// A message that hasn't made the app busy after this long was refused or finished at once.
const START_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
pub struct Script {
    /// Files attached (uploaded) before the first message is sent.
    pub attach: Vec<PathBuf>,
    /// Sent one at a time, each once the previous reply finished.
    pub send: Vec<String>,
    pub screenshot: Option<PathBuf>,
    /// Minimum time before the screenshot (lets the window settle); counted from the
    /// commands when there are any.
    pub after: Duration,
    /// Done once the conversation has settled (`--then`).
    pub then: Vec<Command>,
}

/// Something done to the artifacts in a scripted run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    OpenAll,
    TogglePane,
    CollapseAll,
    ExpandAll,
    /// Let this many seconds pass before the next command.
    Wait(u64),
}

impl Command {
    pub const NAMES: &'static str = "open-all, toggle-pane, collapse-all, expand-all, wait:SECONDS";

    pub fn parse(s: &str) -> Option<Self> {
        if let Some(n) = s.strip_prefix("wait:") {
            return n.parse().ok().map(Self::Wait);
        }
        Some(match s {
            "open-all" => Self::OpenAll,
            "toggle-pane" => Self::TogglePane,
            "collapse-all" => Self::CollapseAll,
            "expand-all" => Self::ExpandAll,
            _ => return None,
        })
    }

    fn run(self, app: &mut byob_ui_egui::ByobApp) {
        let a = app.artifacts_mut();
        match self {
            Self::OpenAll => a.open_all(),
            Self::TogglePane => a.toggle_pane(),
            Self::CollapseAll => a.minimize_all(true),
            Self::ExpandAll => a.minimize_all(false),
            Self::Wait(_) => {}
        }
    }
}

pub struct Runner {
    script: Script,
    started: Instant,
    /// The last message sent: when, and whether the app has been busy since.
    waiting: Option<(Instant, bool)>,
    requested: bool,
    /// When the last `then` command ran; the next one runs no earlier than `next_at`.
    then_at: Option<Instant>,
    next_at: Option<Instant>,
    done: Arc<AtomicBool>,
    result: Arc<Mutex<Option<String>>>,
}

impl Runner {
    pub fn new(mut script: Script) -> Self {
        script.send.reverse();
        Self { script, started: Instant::now(), waiting: None, requested: false, then_at: None, next_at: None, done: Arc::default(), result: Arc::default() }
    }

    pub fn frame(&mut self, app: &mut byob_ui_egui::ByobApp, ctx: &egui::Context) {
        for path in std::mem::take(&mut self.script.attach) {
            match byob_ui_egui::upload::read_path(&path) {
                Ok((name, bytes)) => app.attach(&name, &bytes),
                Err(e) => eprintln!("byob: --attach {}: {e}", path.display()),
            }
        }
        // Messages go out once the attachments have uploaded (so they ride along with the
        // first), one at a time: the next once the app was busy and has settled again.
        if let Some((at, was_busy)) = self.waiting {
            let busy = was_busy || app.busy();
            let finished = at.elapsed() >= SETTLE_TIMEOUT || if busy { !app.busy() && app.settled() } else { at.elapsed() >= START_TIMEOUT };
            self.waiting = (!finished).then_some((at, busy));
        }
        if self.waiting.is_none()
            && !app.uploading()
            && let Some(text) = self.script.send.pop()
        {
            app.submit(text);
            self.waiting = Some((Instant::now(), false));
        }
        if self.waiting.is_some() || !self.script.send.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(100));
            return;
        }
        let Some(path) = self.script.screenshot.clone() else { return };
        ctx.request_repaint_after(Duration::from_millis(100));
        if self.done.load(Ordering::Acquire) {
            if let Some(msg) = self.result.lock().unwrap_or_else(PoisonError::into_inner).take() {
                eprintln!("byob: {msg}");
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        let elapsed = self.started.elapsed();
        let settled = app.settled() || elapsed >= SETTLE_TIMEOUT;
        if !self.script.then.is_empty() {
            // Give the chat a moment after settling so artifacts and apps have loaded.
            if !settled || elapsed < Duration::from_secs(2) || self.next_at.is_some_and(|t| Instant::now() < t) {
                return;
            }
            let c = self.script.then.remove(0);
            eprintln!("byob: {:.1}s: {c:?}", elapsed.as_secs_f64());
            c.run(app);
            let now = Instant::now();
            self.next_at = if let Command::Wait(s) = c { Some(now + Duration::from_secs(s)) } else { None };
            self.then_at = Some(now);
            return;
        }
        let waited = self.then_at.map_or(elapsed, |t| t.elapsed());
        let ready = waited >= self.script.after && settled;
        if ready && !self.requested {
            self.requested = true;
            let (done, result) = (Arc::clone(&self.done), Arc::clone(&self.result));
            ctx.request_screenshot(move |image| {
                let msg = match save_png(&path, &image) {
                    Ok(()) => format!("screenshot saved to {}", path.display()),
                    Err(e) => format!("screenshot failed: {e}"),
                };
                *result.lock().unwrap_or_else(PoisonError::into_inner) = Some(msg);
                done.store(true, Ordering::Release);
            });
        }
    }
}

fn save_png(path: &Path, image: &egui::ColorImage) -> Result<(), String> {
    let [w, h] = image.size;
    let (w, h) = (u32::try_from(w).map_err(|e| e.to_string())?, u32::try_from(h).map_err(|e| e.to_string())?);
    let file = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(image.as_raw()).map_err(|e| e.to_string())
}
