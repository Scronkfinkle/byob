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
                        ui.set_prefs(prefs);
                    }
                    // So is the artifact pane: width, collapsed, open tabs per chat.
                    if let Some(pane) =
                        cc.storage.and_then(|s| eframe::get_value::<byob_ui_egui::pane::PanePrefs>(s, byob_ui_egui::pane::PanePrefs::STORAGE_KEY))
                    {
                        ui.artifacts_mut().set_pane_prefs(pane);
                    }
                    Ok(Box::new(App { ui, link, panel: panel_from_hash(), redraws: 0 }))
                }),
            )
            .await;
        if let Err(e) = result {
            log::error!("failed to start byob: {e:?}");
        }
    });
}

/// `ws(s)://<same host>/ws` — the page is always served by the byob server it talks to.
#[cfg(target_arch = "wasm32")]
fn ws_url() -> String {
    let location = web_sys::window().map(|w| w.location());
    let host = location.as_ref().and_then(|l| l.host().ok()).unwrap_or_default();
    let secure = location.as_ref().and_then(|l| l.protocol().ok()).is_some_and(|p| p == "https:");
    format!("{}://{host}/ws", if secure { "wss" } else { "ws" })
}

/// The `NAME` of a `#panel=NAME` URL hash, if it names a known view.
#[cfg(target_arch = "wasm32")]
fn panel_from_hash() -> Option<String> {
    let hash = web_sys::window()?.location().hash().ok()?;
    let name = hash.strip_prefix("#panel=")?;
    byob_ui_egui::ByobApp::PANELS.split(", ").any(|p| p == name).then(|| name.to_owned())
}

#[cfg(target_arch = "wasm32")]
struct App {
    ui: byob_ui_egui::ByobApp,
    link: byob_ui_egui::ws::LinkStatusHandle,
    /// `#panel=NAME` in the URL: a workspace view to open once connected (see
    /// `ByobApp::open_panel`), e.g. `#panel=new-workspace`.
    panel: Option<String>,
    redraws: u32,
}

#[cfg(target_arch = "wasm32")]
impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.ui.ui(ui);
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

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, byob_ui_egui::theme::Prefs::STORAGE_KEY, &self.ui.prefs());
        eframe::set_value(storage, byob_ui_egui::pane::PanePrefs::STORAGE_KEY, &self.ui.pane_prefs());
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("byob-web only runs in the browser: build it with `cargo xtask web`");
}
