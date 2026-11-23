//! Browser entry point. Build with `cargo xtask web`; the desktop app serves
//! `apps/byob-web/dist/` with `byob --serve`.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

#[cfg(target_arch = "wasm32")]
fn main() {
    use wasm_bindgen::JsCast as _;

    // Route `log` to the browser console; eframe also installs a panic hook that reports there.
    let _ = eframe::WebLogger::init(log::LevelFilter::Info);
    wasm_bindgen_futures::spawn_local(async {
        let Some(document) = web_sys::window().and_then(|w| w.document()) else {
            log::error!("no document");
            return;
        };
        let Some(canvas) = document.get_element_by_id("byob").and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok()) else {
            return;
        };
        keep_ctrl_f(&document);
        let result = eframe::WebRunner::new()
            .start(
                canvas,
                eframe::WebOptions::default(),
                Box::new(|cc| {
                    let conn = byob_ui_egui::ws::WsConnection::connect(&ws_url(), cc.egui_ctx.clone());
                    let link = conn.status_handle();
                    let mut ui = byob_ui_egui::ByobApp::new(Box::new(conn), "browser");
                    // Appearance is per device: kept in this browser's localStorage.
                    if let Some(prefs) = cc.storage.and_then(|s| eframe::get_value::<byob_ui_egui::theme::Prefs>(s, byob_ui_egui::theme::Prefs::STORAGE_KEY)) {
                        ui.restore_prefs(prefs);
                    }
                    let mut compositor = None;
                    if let Some(rs) = &cc.wgpu_render_state {
                        ui.enable_backdrops(rs);
                        // With a backdrop, byob runs in a context of its own, composited
                        // onto the canvas (frosted glass).
                        if byob_ui_egui::cockpit::compositor::wanted(&ui.prefs().cockpit) {
                            compositor = Some(byob_ui_egui::cockpit::compositor::Compositor::new(rs, &cc.egui_ctx));
                        }
                    }
                    // So is the artifact pane: width, collapsed, open tabs per chat.
                    if let Some(pane) =
                        cc.storage.and_then(|s| eframe::get_value::<byob_ui_egui::pane::PanePrefs>(s, byob_ui_egui::pane::PanePrefs::STORAGE_KEY))
                    {
                        ui.artifacts_mut().set_pane_prefs(pane);
                    }
                    // And the chat list: collapsed workspaces, unsent drafts.
                    if let Some(list) =
                        cc.storage.and_then(|s| eframe::get_value::<byob_ui_egui::sidebar::SidebarPrefs>(s, byob_ui_egui::sidebar::SidebarPrefs::STORAGE_KEY))
                    {
                        ui.set_sidebar_prefs(&list);
                    }
                    Ok(Box::new(App { ui, link, panel: panel_from_hash(), redraws: 0, compositor, marked_ready: false }))
                }),
            )
            .await;
        if let Err(e) = result {
            log::error!("failed to start byob: {e:?}");
        }
    });
}

/// Ctrl/Cmd+F searches the chat list: keep the browser's find bar (which can't see text on
/// a canvas anyway) from opening too.
#[cfg(target_arch = "wasm32")]
fn keep_ctrl_f(document: &web_sys::Document) {
    use wasm_bindgen::JsCast as _;
    let on_key = wasm_bindgen::closure::Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(|e: web_sys::KeyboardEvent| {
        if (e.ctrl_key() || e.meta_key()) && !e.shift_key() && !e.alt_key() && e.key().eq_ignore_ascii_case("f") {
            e.prevent_default();
        }
    });
    let _ = document.add_event_listener_with_callback("keydown", on_key.as_ref().unchecked_ref());
    on_key.forget();
}

/// `ws(s)://<same host>/ws` — the page is always served by the byob server it talks to.
#[cfg(target_arch = "wasm32")]
fn ws_url() -> String {
    let location = web_sys::window().map(|w| w.location());
    let host = location.as_ref().and_then(|l| l.host().ok()).unwrap_or_default();
    let secure = location.as_ref().and_then(|l| l.protocol().ok()).is_some_and(|p| p == "https:");
    format!("{}://{host}/ws", if secure { "wss" } else { "ws" })
}

/// `VALUE` of `KEY=VALUE` in the URL hash (`#panel=new-workspace&backdrop=aurora`).
#[cfg(target_arch = "wasm32")]
fn hash_value(key: &str) -> Option<String> {
    let hash = web_sys::window()?.location().hash().ok()?;
    hash.trim_start_matches('#').split('&').find_map(|kv| kv.strip_prefix(key)?.strip_prefix('=')).map(str::to_owned)
}

/// The `NAME` of a `#panel=NAME` URL hash, if it names a known view.
#[cfg(target_arch = "wasm32")]
fn panel_from_hash() -> Option<String> {
    let name = hash_value("panel")?;
    byob_ui_egui::ByobApp::PANELS.split(", ").any(|p| p == name).then_some(name)
}

#[cfg(target_arch = "wasm32")]
struct App {
    ui: byob_ui_egui::ByobApp,
    link: byob_ui_egui::ws::LinkStatusHandle,
    /// `#panel=NAME` in the URL: a workspace view to open once connected (see
    /// `ByobApp::open_panel`), e.g. `#panel=new-workspace`.
    panel: Option<String>,
    redraws: u32,
    /// byob in a context of its own, composited onto the canvas (with a backdrop).
    compositor: Option<byob_ui_egui::cockpit::compositor::Compositor>,
    /// `<html data-byob="ready">` is set: a frame was drawn with the engine's state.
    marked_ready: bool,
}

#[cfg(target_arch = "wasm32")]
impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        match self.compositor.take() {
            Some(mut c) => {
                c.run(ui, |ui| self.frame(ui));
                self.compositor = Some(c);
            }
            None => self.frame(ui),
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, byob_ui_egui::theme::Prefs::STORAGE_KEY, &self.ui.prefs());
        eframe::set_value(storage, byob_ui_egui::pane::PanePrefs::STORAGE_KEY, &self.ui.pane_prefs());
        eframe::set_value(storage, byob_ui_egui::sidebar::SidebarPrefs::STORAGE_KEY, &self.ui.sidebar_prefs());
    }
}

#[cfg(target_arch = "wasm32")]
impl App {
    /// byob's frame, and the page's own bits (the link banner, `#panel=`).
    fn frame(&mut self, ui: &mut egui::Ui) {
        self.ui.ui(ui);
        if !self.marked_ready && self.ui.connected() {
            mark_ready();
            self.marked_ready = true;
            // One more frame, so the page shows what this one drew with the state.
            ui.ctx().request_repaint();
        }
        if let Some(p) = &self.panel {
            // Once connected (the workspace views need the engine's lists).
            if self.ui.connected() && self.ui.open_panel(p) {
                self.panel = None;
                self.redraws = 20;
            }
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        } else if self.redraws > 0 {
            // A few more frames, so the view shows its answers from the engine.
            self.redraws -= 1;
            ui.ctx().request_repaint();
        }
        // One quiet banner while the link is down, instead of a notice per retry.
        if let Some(label) = self.link.get().label() {
            egui::Area::new(egui::Id::unique("byob-link-status"))
                .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 36.0))
                .fade_in(false)
                .order(egui::Order::Foreground)
                .interactable(false)
                .show(ui.ctx(), |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.label(egui::RichText::new(label).color(ui.visuals().warn_fg_color));
                    });
                });
        }
    }
}

/// `<html data-byob="ready">`: the app is running, connected and has drawn a frame with the
/// engine's state. `cargo xtask browser-smoke` waits for it.
#[cfg(target_arch = "wasm32")]
fn mark_ready() {
    if let Some(root) = web_sys::window().and_then(|w| w.document()).and_then(|d| d.document_element()) {
        let _ = root.set_attribute("data-byob", "ready");
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("byob-web only runs in the browser: build it with `cargo xtask web`");
}
