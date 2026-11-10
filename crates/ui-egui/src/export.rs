//! Exporting an artifact (saving its file) and opening one in the system browser.
//!
//! - Desktop: `rfd`'s native save dialog (the XDG desktop portal on Linux) on a thread. Opening runs
//!   `xdg-open`/`open`/`start` with the artifact's URL.
//! - Browser: a `Blob` and a clicked `<a download>`; opening is `window.open` in a new tab.
//!
//! Results come back on a channel, polled every frame, as text for a notice.

use std::sync::mpsc::{Receiver, Sender, channel};

/// What a finished export or open has to say: `Ok` info, `Err` a problem.
pub type Outcome = Result<String, String>;

pub struct Exports {
    tx: Sender<Outcome>,
    rx: Receiver<Outcome>,
}

impl Default for Exports {
    fn default() -> Self {
        let (tx, rx) = channel();
        Self { tx, rx }
    }
}

impl Exports {
    /// Save `bytes` as (by default) `filename`.
    pub fn save(&self, ctx: &egui::Context, filename: &str, mime: &str, bytes: Vec<u8>) {
        save_file(ctx, self.tx.clone(), filename, mime, bytes);
    }

    /// Open `url` in the system browser (a new tab in the browser client).
    pub fn open(&self, ctx: &egui::Context, url: &str) {
        open_url(ctx, self.tx.clone(), url);
    }

    /// Outcomes that arrived since the last frame (empty `Ok`s are silent).
    pub fn poll(&self) -> Vec<Outcome> {
        std::iter::from_fn(|| self.rx.try_recv().ok()).filter(|o| !matches!(o, Ok(s) if s.is_empty())).collect()
    }
}

/// A file name from an artifact (untrusted) safe to suggest or create.
pub fn safe_filename(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String = last.chars().map(|c| if c.is_control() || matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c }).take(100).collect();
    let clean = clean.trim().trim_start_matches('.').to_owned();
    if clean.is_empty() { "artifact".into() } else { clean }
}

#[cfg(not(target_arch = "wasm32"))]
fn save_file(ctx: &egui::Context, tx: Sender<Outcome>, filename: &str, _mime: &str, bytes: Vec<u8>) {
    let ctx = ctx.clone();
    let name = safe_filename(filename);
    let _ = std::thread::Builder::new().name("byob-export".into()).spawn(move || {
        if let Some(why) = dialog_unavailable_reason() {
            let _ = tx.send(Err(format!("No file dialog is available here ({why})")));
            ctx.request_repaint();
            return;
        }
        let outcome = match save_dialog(&name) {
            Some(path) => {
                std::fs::write(&path, &bytes).map(|()| format!("Saved {}", path.display())).map_err(|e| format!("Couldn't save {}: {e}", path.display()))
            }
            // cancelled
            None => Ok(String::new()),
        };
        let _ = tx.send(outcome);
        ctx.request_repaint();
    });
}

/// Why no native file dialog can open here, for the "drop files instead" message. `None`:
/// it can. On Linux rfd asks the XDG desktop portal over the D-Bus session bus, then falls
/// back to zenity.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn dialog_unavailable_reason() -> Option<&'static str> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let env = |k: &str| std::env::var_os(k);
    let on_path = |name: &str| env("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()));
    let bus = env("DBUS_SESSION_BUS_ADDRESS").is_some() || env("XDG_RUNTIME_DIR").is_some_and(|d| std::path::Path::new(&d).join("bus").exists());
    (!bus && !on_path("zenity")).then_some("no D-Bus session for the desktop portal, and no zenity")
}

/// Ask where to save (`rfd`'s native dialog). `None`: cancelled.
#[cfg(not(target_arch = "wasm32"))]
fn save_dialog(name: &str) -> Option<std::path::PathBuf> {
    let mut dialog = rfd::AsyncFileDialog::new().set_title("Export").set_file_name(name);
    if let Some(dir) = downloads_dir() {
        dialog = dialog.set_directory(dir);
    }
    pollster::block_on(dialog.save_file()).map(|f| f.path().to_path_buf())
}

/// Where to start the export dialog: the user's Downloads folder (on Linux from
/// `~/.config/user-dirs.dirs`, so a renamed or moved one counts), else home.
#[cfg(not(target_arch = "wasm32"))]
fn downloads_dir() -> Option<std::path::PathBuf> {
    let dirs = directories::UserDirs::new()?;
    Some(dirs.download_dir().filter(|d| d.is_dir()).unwrap_or(dirs.home_dir()).to_owned())
}

#[cfg(not(target_arch = "wasm32"))]
fn open_url(ctx: &egui::Context, tx: Sender<Outcome>, url: &str) {
    use std::process::Command;
    let (ctx, url) = (ctx.clone(), url.to_owned());
    let _ = std::thread::Builder::new().name("byob-open".into()).spawn(move || {
        let result = if cfg!(target_os = "macos") {
            Command::new("open").arg(&url).status()
        } else if cfg!(target_os = "windows") {
            Command::new("cmd").args(["/C", "start", ""]).arg(&url).status()
        } else {
            Command::new("xdg-open").arg(&url).status()
        };
        let outcome = match result {
            Ok(s) if s.success() => Ok(String::new()),
            Ok(_) | Err(_) => Err(format!("Couldn't open a browser; the app is at {url}")),
        };
        let _ = tx.send(outcome);
        ctx.request_repaint();
    });
}

#[cfg(target_arch = "wasm32")]
fn save_file(_ctx: &egui::Context, tx: Sender<Outcome>, filename: &str, mime: &str, bytes: Vec<u8>) {
    use wasm_bindgen::JsCast as _;
    let result = (|| {
        let document = web_sys::window()?.document()?;
        let parts = js_sys::Array::new();
        parts.push(&js_sys::Uint8Array::from(bytes.as_slice()));
        let options = web_sys::BlobPropertyBag::new();
        options.set_type(mime);
        let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &options).ok()?;
        let url = web_sys::Url::create_object_url_with_blob(&blob).ok()?;
        let a = document.create_element("a").ok()?.dyn_into::<web_sys::HtmlAnchorElement>().ok()?;
        a.set_href(&url);
        a.set_download(&safe_filename(filename));
        a.click();
        let _ = web_sys::Url::revoke_object_url(&url);
        Some(())
    })();
    let _ = tx.send(if result.is_some() { Ok(String::new()) } else { Err("Couldn't start the download".into()) });
}

#[cfg(target_arch = "wasm32")]
fn open_url(_ctx: &egui::Context, tx: Sender<Outcome>, url: &str) {
    let opened = web_sys::window().and_then(|w| w.open_with_url_and_target_and_features(url, "_blank", "noopener").ok());
    // With noopener the browser returns null even when it opened the tab.
    let _ = opened;
    let _ = tx.send(Ok(String::new()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_made_safe() {
        assert_eq!(safe_filename("../../etc/passwd"), "passwd");
        assert_eq!(safe_filename("a:b?.md"), "a_b_.md");
        assert_eq!(safe_filename(".."), "artifact");
        assert_eq!(safe_filename(".hidden.md"), "hidden.md");
    }
}
