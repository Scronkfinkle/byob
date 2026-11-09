//! Attaching files in the composer: an Attach button (a file picker), drag and drop onto
//! the window, upload in chunks over the protocol, and chips for what is attached.
//!
//! Picking and reading files can't block a frame (and can't block at all in the browser),
//! so both report back through a channel polled every frame.
//! - Browser: a file `<input>` and egui's dropped-file futures, driven by
//!   `wasm_bindgen_futures::spawn_local` (the browser's own event loop; no runtime).
//! - Desktop: dropped files are read from their path; the picker is `rfd`'s native dialog
//!   (GTK3 on Linux), on a thread.

use std::sync::mpsc::{Receiver, Sender, channel};

use egui::{Stroke, vec2};
use yah_protocol::media::{self, MAX_ATTACHMENTS, MAX_UPLOAD_BYTES, UPLOAD_CHUNK_BYTES};
use yah_protocol::{ClientMsg, Connection, MediaRef};

use crate::theme::Tokens;

/// A file read on the client, or why it couldn't be.
pub type Picked = Result<(String, Vec<u8>), String>;

struct Pending {
    id: u64,
    name: String,
}

pub struct Uploads {
    next_id: u64,
    pending: Vec<Pending>,
    ready: Vec<MediaRef>,
    tx: Sender<Picked>,
    rx: Receiver<Picked>,
    /// A picker dialog is open.
    picking: bool,
}

impl Default for Uploads {
    fn default() -> Self {
        let (tx, rx) = channel();
        Self { next_id: 1, pending: Vec::new(), ready: Vec::new(), tx, rx, picking: false }
    }
}

impl Uploads {
    /// Start uploading `bytes` as `name`. All chunks are queued at once; the engine answers
    /// with `Uploaded` when it has the whole file.
    pub fn add(&mut self, conn: &mut dyn Connection, name: &str, bytes: &[u8]) -> Result<(), String> {
        let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if size > MAX_UPLOAD_BYTES {
            return Err(format!("{name} is too big to attach ({}; the limit is {})", media::size_label(size), media::size_label(MAX_UPLOAD_BYTES)));
        }
        if self.pending.len() + self.ready.len() >= MAX_ATTACHMENTS {
            return Err(format!("at most {MAX_ATTACHMENTS} files per message"));
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let pieces: Vec<&[u8]> = if bytes.is_empty() { vec![&[][..]] } else { bytes.chunks(UPLOAD_CHUNK_BYTES).collect() };
        let mut offset = 0u64;
        let last = pieces.len().saturating_sub(1);
        for (i, piece) in pieces.into_iter().enumerate() {
            conn.send(ClientMsg::Upload { upload: id, name: name.to_owned(), size, offset, data: media::encode(piece), done: i == last });
            offset = offset.saturating_add(u64::try_from(piece.len()).unwrap_or(0));
        }
        self.pending.push(Pending { id, name: name.to_owned() });
        Ok(())
    }

    /// Apply a `ServerMsg::Uploaded`; returns an error to show, if it failed.
    pub fn uploaded(&mut self, upload: u64, attachment: Option<MediaRef>, error: Option<String>) -> Option<String> {
        let pos = self.pending.iter().position(|p| p.id == upload)?;
        let p = self.pending.remove(pos);
        match (attachment, error) {
            (Some(a), _) => {
                self.ready.push(a);
                None
            }
            (None, e) => Some(format!("Couldn't attach {}: {}", p.name, e.unwrap_or_else(|| "unknown error".into()))),
        }
    }

    /// Uploads still on their way.
    pub fn busy(&self) -> bool {
        !self.pending.is_empty()
    }

    /// The attachments for the message being sent; clears the chips.
    pub fn take(&mut self) -> Vec<String> {
        std::mem::take(&mut self.ready).into_iter().map(|r| r.path).collect()
    }

    pub fn has_ready(&self) -> bool {
        !self.ready.is_empty()
    }

    /// Per frame: start reading dropped files, upload what the picker or drops produced.
    /// Returns errors to show.
    pub fn poll(&mut self, ctx: &egui::Context, conn: &mut dyn Connection) -> Vec<String> {
        let dropped = ctx.input_mut(|i| std::mem::take(&mut i.raw.dropped_files));
        for file in dropped {
            read_dropped(ctx, file, self.tx.clone());
        }
        let mut errors = Vec::new();
        while let Ok(picked) = self.rx.try_recv() {
            self.picking = false;
            match picked.and_then(|(name, bytes)| self.add(conn, &name, &bytes)) {
                Ok(()) => {}
                Err(e) if e.is_empty() => {}
                Err(e) => errors.push(e),
            }
        }
        errors
    }

    /// The Attach button.
    pub fn button(&mut self, ui: &mut egui::Ui) {
        let b = ui.add_enabled(!self.picking, egui::Button::new("Attach")).on_hover_text("Attach files (or drop them onto the window)");
        if b.clicked() {
            // A browser doesn't tell us reliably when its dialog is cancelled, so the button
            // only waits for the desktop dialog.
            self.picking = cfg!(not(target_arch = "wasm32"));
            pick_files(ui.ctx(), self.tx.clone());
        }
    }

    /// Chips for attached and uploading files; × removes one.
    pub fn chips(&mut self, ui: &mut egui::Ui, t: &Tokens) {
        if self.pending.is_empty() && self.ready.is_empty() {
            return;
        }
        let mut remove = None;
        ui.horizontal_wrapped(|ui| {
            for (i, r) in self.ready.iter().enumerate() {
                chip_frame(t).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(r.name()).small().color(t.text));
                        ui.label(egui::RichText::new(media::size_label(r.size)).small().color(t.text_dim));
                        if ui.add(egui::Button::new(egui::RichText::new("×").small()).frame(false)).on_hover_text("Remove").clicked() {
                            remove = Some(i);
                        }
                    });
                });
            }
            for p in &self.pending {
                chip_frame(t).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        crate::theme::spinner(ui, t, None);
                        ui.label(egui::RichText::new(&p.name).small().color(t.text_dim));
                    });
                });
            }
        });
        if let Some(i) = remove.filter(|i| *i < self.ready.len()) {
            self.ready.remove(i);
        }
    }
}

fn chip_frame(t: &Tokens) -> egui::Frame {
    egui::Frame::new().fill(t.button_bg).stroke(Stroke::new(1.0, t.border)).corner_radius(t.round(6.0)).inner_margin(vec2(t.pad(8.0), t.pad(3.0)))
}

/// Shown while files are dragged over the window.
pub fn drop_overlay(ctx: &egui::Context, t: &Tokens) {
    if ctx.input(|i| i.raw.hovered_files.is_empty()) {
        return;
    }
    let rect = ctx.content_rect();
    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("yah-drop")));
    painter.rect_filled(rect, 0.0, t.app_bg.gamma_multiply(0.85));
    painter.rect_stroke(rect.shrink(24.0), 12.0, Stroke::new(2.0, t.accent), egui::StrokeKind::Inside);
    painter.text(rect.center(), egui::Align2::CENTER_CENTER, "Drop files to attach", egui::FontId::proportional(t.text_size(22.0)), t.text_strong);
}

/// Read a native file with a size cap (never trust a drop to be small).
#[cfg(not(target_arch = "wasm32"))]
pub fn read_path(path: &std::path::Path) -> Picked {
    use std::io::Read as _;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
    let meta = std::fs::metadata(path).map_err(|e| format!("Couldn't read {name}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{name} is a folder; attach files instead"));
    }
    if meta.len() > MAX_UPLOAD_BYTES {
        return Err(format!("{name} is too big to attach ({}; the limit is {})", media::size_label(meta.len()), media::size_label(MAX_UPLOAD_BYTES)));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(MAX_UPLOAD_BYTES.saturating_add(1)).read_to_end(&mut bytes))
        .map_err(|e| format!("Couldn't read {name}: {e}"))?;
    Ok((name, bytes))
}

#[cfg(not(target_arch = "wasm32"))]
fn read_dropped(ctx: &egui::Context, file: egui::DroppedFileHandle, tx: Sender<Picked>) {
    let path = file.path().to_path_buf();
    let ctx = ctx.clone();
    let _ = std::thread::Builder::new().name("yah-drop".into()).spawn(move || {
        let _ = tx.send(read_path(&path));
        ctx.request_repaint();
    });
}

#[cfg(target_arch = "wasm32")]
fn read_dropped(ctx: &egui::Context, file: egui::DroppedFileHandle, tx: Sender<Picked>) {
    let ctx = ctx.clone();
    wasm_bindgen_futures::spawn_local(async move {
        let name = file.path().file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
        let result = match file.bytes_async().await {
            Ok(bytes) => Ok((name, bytes)),
            Err(e) => Err(format!("Couldn't read {name}: {e}")),
        };
        let _ = tx.send(result);
        ctx.request_repaint();
    });
}

/// Open the platform's file dialog on a thread; each chosen file arrives on `tx`.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn pick_files(ctx: &egui::Context, tx: Sender<Picked>) {
    let ctx = ctx.clone();
    let _ = std::thread::Builder::new().name("yah-picker".into()).spawn(move || {
        match native_dialog() {
            Ok(paths) if paths.is_empty() => {
                // cancelled: an empty error just re-enables the button
                let _ = tx.send(Err(String::new()));
            }
            Ok(paths) => {
                for p in paths {
                    let _ = tx.send(read_path(&p));
                }
            }
            Err(e) => {
                let _ = tx.send(Err(e));
            }
        }
        ctx.request_repaint();
    });
}

#[cfg(not(target_arch = "wasm32"))]
fn native_dialog() -> Result<Vec<std::path::PathBuf>, String> {
    if !crate::export::dialogs_available() {
        return Err("No file dialog is available here (GTK can't find its settings schemas); drop files onto the window instead".into());
    }
    // rfd's async dialog runs GTK on its own thread (Linux) and the panel on the main thread
    // (macOS); we're on a background thread, so blocking on it here is fine.
    let picked = pollster::block_on(rfd::AsyncFileDialog::new().set_title("Attach files").pick_files());
    Ok(picked.map(|files| files.into_iter().map(|f| f.path().to_path_buf()).collect()).unwrap_or_default())
}

/// The browser's file dialog: a detached `<input type=file multiple>`, clicked. Each chosen
/// file is read with `Blob.arrayBuffer()` (size checked first) and arrives on `tx`.
#[cfg(target_arch = "wasm32")]
pub(crate) fn pick_files(ctx: &egui::Context, tx: Sender<Picked>) {
    use wasm_bindgen::JsCast as _;
    use wasm_bindgen::closure::Closure;
    let input =
        web_sys::window().and_then(|w| w.document()).and_then(|d| d.create_element("input").ok()).and_then(|e| e.dyn_into::<web_sys::HtmlInputElement>().ok());
    let Some(input) = input else {
        let _ = tx.send(Err("Couldn't open a file dialog; drop files onto the page instead".into()));
        return;
    };
    input.set_type("file");
    input.set_multiple(true);
    let (ctx, chosen) = (ctx.clone(), input.clone());
    let on_change = Closure::<dyn FnMut()>::once(move || {
        let files = chosen.files();
        let count = files.as_ref().map_or(0, web_sys::FileList::length);
        for i in 0..count {
            let Some(file) = files.as_ref().and_then(|f| f.get(i)) else { continue };
            let (tx, ctx) = (tx.clone(), ctx.clone());
            wasm_bindgen_futures::spawn_local(async move {
                let name = file.name();
                #[allow(clippy::cast_precision_loss)] // sizes far below 2^52
                let result = if file.size() > MAX_UPLOAD_BYTES as f64 {
                    Err(format!("{name} is too big to attach (the limit is {})", media::size_label(MAX_UPLOAD_BYTES)))
                } else {
                    match wasm_bindgen_futures::JsFuture::from(file.array_buffer()).await {
                        Ok(buf) => Ok((name, js_sys::Uint8Array::new(&buf).to_vec())),
                        Err(_) => Err(format!("Couldn't read {name}")),
                    }
                };
                let _ = tx.send(result);
                ctx.request_repaint();
            });
        }
    });
    input.set_onchange(Some(on_change.as_ref().unchecked_ref()));
    // The input lives until the browser drops it; so must its handler.
    on_change.forget();
    input.click();
}

#[cfg(test)]
mod tests {
    use super::*;
    use yah_protocol::ServerMsg;

    #[derive(Default)]
    struct Recorder(Vec<ClientMsg>);

    impl Connection for Recorder {
        fn send(&mut self, msg: ClientMsg) {
            self.0.push(msg);
        }
        fn try_recv(&mut self) -> Option<ServerMsg> {
            None
        }
    }

    #[test]
    fn files_go_out_in_ordered_chunks() {
        let mut u = Uploads::default();
        let mut conn = Recorder::default();
        let bytes: Vec<u8> = (0..UPLOAD_CHUNK_BYTES * 2 + 5).map(|i| (i % 7) as u8).collect();
        u.add(&mut conn, "big.bin", &bytes).unwrap();
        assert_eq!(conn.0.len(), 3);
        let mut joined = Vec::new();
        for (i, m) in conn.0.iter().enumerate() {
            let ClientMsg::Upload { upload, size, offset, data, done, .. } = m else { panic!() };
            assert_eq!((*upload, *size, *offset as usize, *done), (1, bytes.len() as u64, joined.len(), i == 2));
            assert!(m.to_json().len() < yah_protocol::MAX_FRAME_BYTES);
            joined.extend(media::decode(data).unwrap());
        }
        assert_eq!(joined, bytes);
        assert!(u.busy());
        assert_eq!(u.uploaded(1, Some(MediaRef::new("uploads/big.bin", 9)), None), None);
        assert!(!u.busy());
        assert_eq!(u.take(), vec!["uploads/big.bin".to_owned()]);
        assert!(u.take().is_empty());
    }

    #[test]
    fn empty_file_is_one_chunk_and_failures_are_reported() {
        let mut u = Uploads::default();
        let mut conn = Recorder::default();
        u.add(&mut conn, "empty.txt", b"").unwrap();
        assert!(matches!(&conn.0[..], [ClientMsg::Upload { done: true, size: 0, .. }]));
        let err = u.uploaded(1, None, Some("disk full".into())).unwrap();
        assert!(err.contains("empty.txt") && err.contains("disk full"));
        assert_eq!(u.uploaded(77, None, None), None, "unknown ids are ignored");
    }

    #[test]
    fn too_many_or_too_big_is_refused_up_front() {
        let mut u = Uploads::default();
        let mut conn = Recorder::default();
        for i in 0..MAX_ATTACHMENTS {
            u.add(&mut conn, &format!("f{i}"), b"x").unwrap();
        }
        assert!(u.add(&mut conn, "one more", b"x").is_err());
        #[cfg(not(target_arch = "wasm32"))]
        {
            let d = std::env::temp_dir().join(format!("yah-upload-test-{}", std::process::id()));
            std::fs::create_dir_all(&d).unwrap();
            assert!(read_path(&d).unwrap_err().contains("folder"));
            std::fs::write(d.join("a.csv"), "a,b").unwrap();
            assert_eq!(read_path(&d.join("a.csv")).unwrap(), ("a.csv".to_owned(), b"a,b".to_vec()));
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn chips_render() {
        let mut u = Uploads::default();
        let mut conn = Recorder::default();
        u.add(&mut conn, "a.csv", b"x").unwrap();
        u.add(&mut conn, "b.csv", b"x").unwrap();
        u.uploaded(1, Some(MediaRef::new("uploads/a.csv", 1)), None);
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| u.chips(ui, &Tokens::light()));
        out.textures_delta.clear();
        assert!(u.has_ready() && u.busy());
    }
}
