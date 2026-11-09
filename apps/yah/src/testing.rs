//! Scripted runs for headless testing (see `scripts/headless.sh`): send messages on start,
//! screenshot once the conversation settles, then exit.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Give up waiting for the conversation to settle after this long and shoot anyway.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Default)]
pub struct Script {
    /// Files attached (uploaded) before the first message is sent.
    pub attach: Vec<PathBuf>,
    pub send: Vec<String>,
    pub screenshot: Option<PathBuf>,
    /// Minimum time before the screenshot (lets the window settle).
    pub after: Duration,
}

pub struct Runner {
    script: Script,
    started: Instant,
    sent: bool,
    requested: bool,
    done: Arc<AtomicBool>,
    result: Arc<Mutex<Option<String>>>,
}

impl Runner {
    pub fn new(script: Script) -> Self {
        Self { script, started: Instant::now(), sent: false, requested: false, done: Arc::default(), result: Arc::default() }
    }

    pub fn frame(&mut self, app: &mut yah_ui_egui::YahApp, ctx: &egui::Context) {
        for path in std::mem::take(&mut self.script.attach) {
            match yah_ui_egui::upload::read_path(&path) {
                Ok((name, bytes)) => app.attach(&name, &bytes),
                Err(e) => eprintln!("yah: --attach {}: {e}", path.display()),
            }
        }
        // Messages go out once the attachments have uploaded, so they ride along.
        if !self.sent && !app.uploading() {
            self.sent = true;
            for text in std::mem::take(&mut self.script.send) {
                app.submit(text);
            }
        }
        let Some(path) = self.script.screenshot.clone() else { return };
        ctx.request_repaint_after(Duration::from_millis(100));
        if self.done.load(Ordering::Acquire) {
            if let Some(msg) = self.result.lock().unwrap_or_else(PoisonError::into_inner).take() {
                eprintln!("yah: {msg}");
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        let elapsed = self.started.elapsed();
        let ready = elapsed >= self.script.after && (app.settled() || elapsed >= SETTLE_TIMEOUT);
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
