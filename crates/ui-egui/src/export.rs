//! Exporting an artifact (saving its file) and opening one in the system browser.
//!
//! - Desktop: a save dialog from the platform's dialog tool (`zenity`/`kdialog`, `osascript`
//!   on macOS) on a thread; without one, the file goes to the Downloads folder. Opening runs
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
    let _ = std::thread::Builder::new().name("yah-export".into()).spawn(move || {
        let outcome = match save_dialog(&name) {
            Ok(Some(path)) => {
                std::fs::write(&path, &bytes).map(|()| format!("Saved {}", path.display())).map_err(|e| format!("Couldn't save {}: {e}", path.display()))
            }
            // cancelled
            Ok(None) => Ok(String::new()),
            Err(NoDialog) => match downloads_target(&name) {
                Some(path) => {
                    std::fs::write(&path, &bytes).map(|()| format!("Saved to {}", path.display())).map_err(|e| format!("Couldn't save {}: {e}", path.display()))
                }
                None => Err("No file dialog is available and there is no Downloads folder to save to".into()),
            },
        };
        let _ = tx.send(outcome);
        ctx.request_repaint();
    });
}

#[cfg(not(target_arch = "wasm32"))]
struct NoDialog;

/// Ask where to save. `Ok(None)`: cancelled.
#[cfg(not(target_arch = "wasm32"))]
fn save_dialog(name: &str) -> Result<Option<std::path::PathBuf>, NoDialog> {
    use std::process::Command;
    let start = downloads_dir().map(|d| d.join(name)).unwrap_or_else(|| std::path::PathBuf::from(name));
    let start = start.to_string_lossy().into_owned();
    let script = format!("POSIX path of (choose file name with prompt \"Export\" default name \"{}\")", name.replace(['"', '\\'], "_"));
    let tries: Vec<(&str, Vec<String>)> = if cfg!(target_os = "macos") {
        vec![("osascript", vec!["-e".into(), script])]
    } else {
        vec![
            ("zenity", vec!["--file-selection".into(), "--save".into(), "--confirm-overwrite".into(), "--title=Export".into(), format!("--filename={start}")]),
            ("kdialog", vec!["--getsavefilename".into(), start]),
        ]
    };
    for (cmd, args) in tries {
        match Command::new(cmd).args(&args).output() {
            Ok(out) => {
                let text = String::from_utf8_lossy(&out.stdout);
                let path = text.lines().map(str::trim).find(|l| !l.is_empty());
                return Ok(path.map(std::path::PathBuf::from));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(NoDialog),
        }
    }
    Err(NoDialog)
}

#[cfg(not(target_arch = "wasm32"))]
fn downloads_dir() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(std::path::PathBuf::from)?;
    let downloads = std::env::var_os("XDG_DOWNLOAD_DIR").map(std::path::PathBuf::from).unwrap_or_else(|| home.join("Downloads"));
    Some(if downloads.is_dir() { downloads } else { home })
}

#[cfg(not(target_arch = "wasm32"))]
fn downloads_target(name: &str) -> Option<std::path::PathBuf> {
    unique_in(&downloads_dir()?, name)
}

/// `dir/name`, or `name (2)`, … — the first that doesn't exist.
#[cfg(not(target_arch = "wasm32"))]
fn unique_in(dir: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (name.get(..i).unwrap_or(name), name.get(i..).unwrap_or("")),
        _ => (name, ""),
    };
    (1..1000u32).map(|n| if n == 1 { dir.join(name) } else { dir.join(format!("{stem} ({n}){ext}")) }).find(|p| std::fs::symlink_metadata(p).is_err())
}

#[cfg(not(target_arch = "wasm32"))]
fn open_url(ctx: &egui::Context, tx: Sender<Outcome>, url: &str) {
    use std::process::Command;
    let (ctx, url) = (ctx.clone(), url.to_owned());
    let _ = std::thread::Builder::new().name("yah-open".into()).spawn(move || {
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

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn downloads_never_overwrite() {
        let dir = std::env::temp_dir().join(format!("yah-export-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(unique_in(&dir, "a.md"), Some(dir.join("a.md")));
        std::fs::write(dir.join("a.md"), "x").unwrap();
        assert_eq!(unique_in(&dir, "a.md"), Some(dir.join("a (2).md")));
        assert_eq!(unique_in(&dir, "noext"), Some(dir.join("noext")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
