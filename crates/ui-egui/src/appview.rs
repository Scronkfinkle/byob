//! Where HTML app artifacts run: the [`AppView`] seam.
//!
//! - Browser: [`IframeView`] lays a sandboxed `<iframe>` over the egui canvas, positioned
//!   from Rust every frame (web-sys only), pointing at the app's own URL
//!   (`/artifact/<chat>/<id>/<v>`, served with its own strict CSP — a `srcdoc` would inherit
//!   the page's). egui windows and popups above it are cut out with an even-odd
//!   `clip-path`. The theme goes in the URL hash and, when it changes, by `postMessage`;
//!   errors the app reports come back the same way.
//! - Desktop: the app binary plugs in a renderer that runs each app in a Servo helper
//!   process (`apps/byob/src/apphost`) via [`crate::ByobApp::set_app_view`]; it also shows
//!   apps live in chat cards ([`AppView::card`]). Without the helper, [`Placeholder`] shows a
//!   summary and points at Open (system browser) and Export.

use egui::{RichText, Stroke};
use byob_protocol::artifact::THEME_VARS;

use crate::theme::Tokens;

/// One HTML app version to show.
pub struct AppDoc<'a> {
    /// The artifact's id (with `version`, identifies the document).
    pub id: &'a str,
    /// Where the app is served (with the theme in the hash), if a server is reachable and
    /// the view [wants it](AppView::wants_url).
    pub url: Option<&'a str>,
    pub title: &'a str,
    pub version: u32,
    pub size: u64,
    /// The HTML, when loaded and the view [wants it](AppView::wants_source).
    pub source: Option<&'a str>,
}

/// What the user did in an app view.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AppViewResponse {
    /// Asked to open the app in the system browser.
    pub open: bool,
}

/// Whether an app version is running, for chips and the rail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppStatus {
    /// This view doesn't run apps, or hasn't seen this one.
    Unknown,
    /// Running (on screen, or minimized and still alive).
    Running,
    /// Not running: a snapshot (or nothing) is kept; showing it starts it afresh.
    Paused,
    /// Killed or crashed; waits for Reload.
    Stopped,
}

impl AppStatus {
    pub fn label(self) -> Option<&'static str> {
        match self {
            Self::Unknown => None,
            Self::Running => Some("running"),
            Self::Paused => Some("paused"),
            Self::Stopped => Some("stopped"),
        }
    }
}

/// Renders an HTML app artifact inside the pane.
pub trait AppView {
    /// Draw `doc` into the space `ui` has left.
    fn show(&mut self, ui: &mut egui::Ui, t: &Tokens, doc: &AppDoc<'_>) -> AppViewResponse;
    /// Draw a live preview of `doc` in a chat card (full width, the app's own height).
    /// `None`: this view has no card previews (see [`AppView::live_cards`]).
    fn card(&mut self, _ui: &mut egui::Ui, _t: &Tokens, _doc: &AppDoc<'_>) -> Option<AppViewResponse> {
        None
    }
    /// Cards should fetch the source and call [`AppView::card`].
    fn live_cards(&self) -> bool {
        false
    }
    /// [`AppDoc::source`] is needed (fetched with `GetArtifact`).
    fn wants_source(&self) -> bool {
        true
    }
    /// [`AppDoc::url`] is needed (on the desktop that starts a loopback server).
    fn wants_url(&self) -> bool {
        false
    }
    /// Once per frame, after everything else is drawn (overlays sync to their rect here,
    /// and hide when [`AppView::show`] wasn't called this frame).
    fn end_frame(&mut self, _ctx: &egui::Context) {}
    /// Errors the app on show has reported (untrusted text; cleared when another loads).
    fn errors(&self) -> Vec<String> {
        Vec::new()
    }
    /// The app runs inline (otherwise it only runs in a browser via Open).
    fn runs_inline(&self) -> bool;
    /// The theme changed; running apps should restyle.
    fn set_theme(&mut self, _vars: &[(&'static str, String)]) {}
    /// This frame `id`/`version` isn't drawn but is wanted in the background (minimized, a
    /// background tab, the collapsed pane): keep it running without rendering for a while,
    /// then pause it. Never starts an app.
    fn hold(&mut self, _id: &str, _version: u32) {}
    fn status(&self, _id: &str, _version: u32) -> AppStatus {
        AppStatus::Unknown
    }
}

/// The default view for this platform.
pub fn default_view() -> Box<dyn AppView> {
    #[cfg(target_arch = "wasm32")]
    {
        Box::new(IframeView::new())
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        Box::new(Placeholder)
    }
}

fn hex(c: egui::Color32) -> String {
    format!("#{:02x}{:02x}{:02x}", c.r(), c.g(), c.b())
}

/// The theme as the CSS variables apps are told to use (in [`THEME_VARS`] order).
pub fn theme_vars(t: &Tokens) -> Vec<(&'static str, String)> {
    let colors = [
        t.app_bg,
        t.assistant_bubble,
        t.text,
        t.text_dim,
        t.accent,
        t.on_accent,
        t.border,
        t.danger,
        t.success,
        t.warning,
        t.link,
        t.text_strong,
        t.text_faint,
        t.panel_bg,
        t.field_bg,
        t.code_bg,
        t.code_text,
    ];
    let mut values: Vec<String> = colors.into_iter().map(hex).collect();
    values.push(format!("{}px", t.radius.round()));
    values.extend(t.chart.iter().map(|c| hex(*c)));
    THEME_VARS.iter().zip(values).map(|(k, v)| (*k, v)).collect()
}

/// `#--byob-bg=%2316171a&…`, read by the helper script in served apps.
pub fn theme_hash(vars: &[(&'static str, String)]) -> String {
    let parts: Vec<String> = vars.iter().map(|(k, v)| format!("{k}={}", v.replace('#', "%23"))).collect();
    format!("#{}", parts.join("&"))
}

/// `<title>` of an HTML document, if it has one.
pub fn html_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<title")?;
    let open_end = lower.get(start..)?.find('>')?.saturating_add(start).saturating_add(1);
    let close = lower.get(open_end..)?.find("</title")?.saturating_add(open_end);
    let title = html.get(open_end..close)?.trim();
    (!title.is_empty()).then(|| title.chars().take(100).collect())
}

/// Desktop stand-in until an inline renderer lands: a summary card and Open.
pub struct Placeholder;

impl AppView for Placeholder {
    fn show(&mut self, ui: &mut egui::Ui, t: &Tokens, doc: &AppDoc<'_>) -> AppViewResponse {
        let mut resp = AppViewResponse::default();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            egui::Frame::new().fill(t.assistant_bubble).stroke(Stroke::new(1.0, t.border)).corner_radius(t.round(10.0)).inner_margin(t.pad(16.0)).show(
                ui,
                |ui| {
                    ui.set_width(ui.available_width());
                    let name = doc.source.and_then(html_title).unwrap_or_else(|| doc.title.to_owned());
                    ui.label(RichText::new(name).strong().size(t.text_size(17.0)).color(t.text_strong));
                    ui.label(
                        RichText::new(format!("Interactive app · version {} · {}", doc.version, byob_protocol::media::size_label(doc.size)))
                            .small()
                            .color(t.text_dim),
                    );
                    ui.add_space(8.0);
                    ui.label(RichText::new("Apps run in your browser for now. Open it there, or export the file to keep it.").color(t.text));
                    ui.add_space(8.0);
                    if ui.add(egui::Button::new(RichText::new("Open in browser").color(t.on_accent)).fill(t.accent)).clicked() {
                        resp.open = true;
                    }
                },
            );
            if let Some(src) = doc.source {
                ui.add_space(8.0);
                egui::CollapsingHeader::new(RichText::new("Source").color(t.text_dim)).id_salt(("app-source", doc.title, doc.version)).show(ui, |ui| {
                    let mut text = src;
                    ui.add(egui::TextEdit::multiline(&mut text).code_editor().desired_width(f32::INFINITY));
                });
            }
        });
        resp
    }

    fn runs_inline(&self) -> bool {
        false
    }
}

#[cfg(target_arch = "wasm32")]
pub use web::IframeView;

#[cfg(target_arch = "wasm32")]
mod web {
    use std::cell::RefCell;
    use std::rc::Rc;

    use wasm_bindgen::JsCast as _;
    use wasm_bindgen::prelude::*;

    use super::{AppDoc, AppStatus, AppView, AppViewResponse};
    use crate::theme::Tokens;

    /// Errors kept from one app.
    const MAX_ERRORS: usize = 20;
    /// Not shown (minimized, another tab, pane collapsed) this long, in seconds: the frame is
    /// emptied so the app stops and its memory goes; showing it again reloads it.
    const PAUSE_AFTER: f64 = 60.0;
    /// Paused apps remembered for their chip status.
    const MAX_PAUSED: usize = 32;
    /// Out of view: `display:none` (the document stays loaded and keeps its state).
    const HIDDEN: &str = "display:none";

    /// A sandboxed iframe over the canvas.
    #[derive(Default)]
    pub struct IframeView {
        frame: Option<web_sys::HtmlIFrameElement>,
        /// The app's URL without the hash (a theme change alone doesn't reload it).
        src: String,
        /// Where to put it this frame (`None`: not drawn this frame, hide it).
        rect: Option<(egui::Rect, egui::Rect)>,
        last_style: String,
        /// The app version loaded in the frame, and when (egui time) it was last shown.
        loaded: Option<(String, u32)>,
        last_shown: f64,
        /// Versions that ran here and were unloaded since.
        paused: Vec<(String, u32)>,
        errors: Rc<RefCell<Vec<String>>>,
        _on_message: Option<Closure<dyn FnMut(web_sys::MessageEvent)>>,
    }

    impl IframeView {
        pub fn new() -> Self {
            Self::default()
        }

        fn pause_mark(&mut self, key: (String, u32)) {
            if !self.paused.contains(&key) {
                if self.paused.len() >= MAX_PAUSED {
                    self.paused.remove(0);
                }
                self.paused.push(key);
            }
        }

        fn frame(&mut self) -> Option<web_sys::HtmlIFrameElement> {
            if let Some(f) = &self.frame {
                return Some(f.clone());
            }
            let document = web_sys::window()?.document()?;
            let f = document.create_element("iframe").ok()?.dyn_into::<web_sys::HtmlIFrameElement>().ok()?;
            // Scripts and forms only: opaque origin, no top navigation, popups or modals.
            let _ = f.set_attribute("sandbox", "allow-scripts allow-forms");
            let _ = f.set_attribute("referrerpolicy", "no-referrer");
            let _ = f.set_attribute(
                "allow",
                "camera 'none'; microphone 'none'; geolocation 'none'; clipboard-read 'none'; display-capture 'none'; fullscreen 'none'; payment 'none'; usb 'none'; serial 'none'; bluetooth 'none'",
            );
            let _ = f.set_attribute("style", HIDDEN);
            document.body()?.append_child(&f).ok()?;
            // Errors come back by postMessage; only from our frame's window, read as text.
            let errors = Rc::clone(&self.errors);
            let ours = f.clone();
            let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
                let from_ours = match (e.source(), ours.content_window()) {
                    (Some(src), Some(win)) => js_sys::Object::is(&src, &win),
                    _ => false,
                };
                if !from_ours {
                    return;
                }
                let data = e.data();
                let get = |k: &str| js_sys::Reflect::get(&data, &JsValue::from_str(k)).ok();
                if get("t").and_then(|v| v.as_string()).as_deref() != Some("error") {
                    return;
                }
                let msg: String = get("msg").and_then(|v| v.as_string()).unwrap_or_default().chars().take(2000).collect();
                let line = get("line").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let text = if line > 0.0 { format!("{msg} (line {line})") } else { msg };
                let mut list = errors.borrow_mut();
                if !list.contains(&text) && list.len() < MAX_ERRORS {
                    list.push(text);
                }
            });
            if let Some(w) = web_sys::window() {
                let _ = w.add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref());
            }
            self._on_message = Some(on_message);
            self.frame = Some(f.clone());
            Some(f)
        }
    }

    impl AppView for IframeView {
        fn show(&mut self, ui: &mut egui::Ui, t: &Tokens, doc: &AppDoc<'_>) -> AppViewResponse {
            let rect = ui.available_rect_before_wrap();
            ui.allocate_rect(rect, egui::Sense::hover());
            ui.painter().rect_filled(rect, 0.0, t.app_bg);
            let Some(url) = doc.url else { return AppViewResponse::default() };
            let page = url.split('#').next().unwrap_or(url);
            if self.src != page
                && let Some(f) = self.frame()
            {
                self.src = page.to_owned();
                let key = (doc.id.to_owned(), doc.version);
                self.paused.retain(|k| *k != key);
                if let Some(old) = self.loaded.replace(key) {
                    self.pause_mark(old);
                }
                self.errors.borrow_mut().clear();
                f.set_src(url);
                let _ = f.set_attribute("title", doc.title);
            }
            self.rect = Some((rect, ui.clip_rect()));
            self.last_shown = ui.input(|i| i.time);
            AppViewResponse::default()
        }

        fn status(&self, id: &str, version: u32) -> AppStatus {
            let key = (id.to_owned(), version);
            if self.loaded.as_ref() == Some(&key) {
                AppStatus::Running
            } else if self.paused.contains(&key) {
                AppStatus::Paused
            } else {
                AppStatus::Unknown
            }
        }

        fn end_frame(&mut self, ctx: &egui::Context) {
            let Some(f) = self.frame.clone() else { return };
            if self.rect.is_none() && self.loaded.is_some() && ctx.input(|i| i.time) - self.last_shown > PAUSE_AFTER {
                f.set_src("about:blank");
                self.src.clear();
                self.errors.borrow_mut().clear();
                if let Some(old) = self.loaded.take() {
                    self.pause_mark(old);
                }
            }
            let style = match self.rect.take() {
                Some((rect, clip)) => {
                    let visible = clip.intersect(rect);
                    if visible.width() <= 0.5 || visible.height() <= 0.5 {
                        None
                    } else {
                        // Every visible egui layer above the background (windows, menus,
                        // tooltips) is cut out of the iframe.
                        let holes: Vec<egui::Rect> = ctx.memory(|m| {
                            m.areas().visible_layer_ids().into_iter().filter(|l| l.order != egui::Order::Background).filter_map(|l| m.area_rect(l.id)).collect()
                        });
                        let r = visible.translate(-rect.min.to_vec2());
                        let mut path = format!("M{} {}H{}V{}H{}Z", r.min.x, r.min.y, r.max.x, r.max.y, r.min.x);
                        for h in holes {
                            let h = h.intersect(visible);
                            if h.width() > 0.0 && h.height() > 0.0 {
                                let h = h.translate(-rect.min.to_vec2());
                                path.push_str(&format!("M{} {}H{}V{}H{}Z", h.min.x, h.min.y, h.max.x, h.max.y, h.min.x));
                            }
                        }
                        Some(format!(
                            "position:fixed;left:{}px;top:{}px;width:{}px;height:{}px;border:0;clip-path:path(evenodd,'{path}');z-index:1",
                            rect.min.x,
                            rect.min.y,
                            rect.width(),
                            rect.height()
                        ))
                    }
                }
                None => None,
            };
            let style = style.unwrap_or_else(|| HIDDEN.to_owned());
            if style != self.last_style {
                let _ = f.set_attribute("style", &style);
                self.last_style = style;
            }
        }

        fn errors(&self) -> Vec<String> {
            self.errors.borrow().clone()
        }

        fn runs_inline(&self) -> bool {
            true
        }

        fn wants_source(&self) -> bool {
            false
        }

        fn wants_url(&self) -> bool {
            true
        }

        fn set_theme(&mut self, vars: &[(&'static str, String)]) {
            let Some(win) = self.frame.as_ref().and_then(web_sys::HtmlIFrameElement::content_window) else { return };
            let msg = js_sys::Object::new();
            let v = js_sys::Object::new();
            for (k, val) in vars {
                let _ = js_sys::Reflect::set(&v, &JsValue::from_str(k), &JsValue::from_str(val));
            }
            let _ = js_sys::Reflect::set(&msg, &JsValue::from_str("t"), &JsValue::from_str("theme"));
            let _ = js_sys::Reflect::set(&msg, &JsValue::from_str("vars"), &v);
            let _ = win.post_message(&msg, "*");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_goes_in_the_hash() {
        let vars = theme_vars(&Tokens::dark());
        assert_eq!(vars.len(), THEME_VARS.len());
        let hash = theme_hash(&vars);
        assert!(hash.starts_with("#--byob-bg=%23") && hash.contains("&--byob-accent=%23"), "{hash}");
        assert!(!hash[1..].contains('#'));
        // The desktop helper only accepts `--byob-` + lowercase letters and dashes.
        for (k, v) in &vars {
            let name = k.strip_prefix("--byob-").unwrap_or_default();
            assert!(!name.is_empty() && name.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'), "{k}");
            assert!(v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'#'), "{k}={v}");
        }
        assert!(vars.contains(&("--byob-radius", "6px".to_owned())));
    }

    #[test]
    fn titles() {
        assert_eq!(html_title("<html><head><TITLE> Loan calc </title>").as_deref(), Some("Loan calc"));
        assert_eq!(html_title("<p>none</p>"), None);
        assert_eq!(html_title("<title></title>"), None);
        assert_eq!(html_title("<title>unclosed"), None);
    }

    #[test]
    fn placeholder_draws_and_offers_open() {
        let ctx = egui::Context::default();
        let mut view = Placeholder;
        let doc = AppDoc { id: "calc", url: None, title: "Calc", version: 2, size: 1200, source: Some("<title>Calc</title>") };
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            assert!(!view.show(ui, &Tokens::dark(), &doc).open);
            assert!(view.card(ui, &Tokens::dark(), &doc).is_none());
        });
        out.textures_delta.clear();
        assert!(!view.runs_inline() && view.wants_source() && !view.wants_url() && !view.live_cards());
    }
}
