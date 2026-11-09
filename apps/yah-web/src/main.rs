//! Browser entry point. Build with `cargo xtask web`; the desktop app serves
//! `apps/yah-web/dist/` with `yah --serve`.
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
        let Some(canvas) = document.get_element_by_id("yah").and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok()) else {
            return;
        };
        let result = eframe::WebRunner::new()
            .start(
                canvas,
                eframe::WebOptions::default(),
                Box::new(|cc| {
                    let ctx = cc.egui_ctx.clone();
                    let url = ws_url();
                    let conn = yah_ui_egui::ws::WsConnection::connect(&url, move || ctx.request_repaint())?;
                    let mut app = yah_ui_egui::YahApp::new(Box::new(conn), "browser");
                    // Appearance is per device: kept in this browser's localStorage.
                    if let Some(prefs) = cc.storage.and_then(|s| eframe::get_value::<yah_ui_egui::theme::Prefs>(s, yah_ui_egui::theme::Prefs::STORAGE_KEY)) {
                        app.set_prefs(prefs);
                    }
                    Ok(Box::new(App(app)))
                }),
            )
            .await;
        if let Err(e) = result {
            log::error!("failed to start yah: {e:?}");
        }
    });
}

/// `ws(s)://<same host>/ws` — the page is always served by the yah server it talks to.
#[cfg(target_arch = "wasm32")]
fn ws_url() -> String {
    let location = web_sys::window().map(|w| w.location());
    let host = location.as_ref().and_then(|l| l.host().ok()).unwrap_or_default();
    let secure = location.as_ref().and_then(|l| l.protocol().ok()).is_some_and(|p| p == "https:");
    format!("{}://{host}/ws", if secure { "wss" } else { "ws" })
}

#[cfg(target_arch = "wasm32")]
struct App(yah_ui_egui::YahApp);

#[cfg(target_arch = "wasm32")]
impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.0.ui(ui);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, yah_ui_egui::theme::Prefs::STORAGE_KEY, &self.0.prefs());
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("yah-web only runs in the browser: build it with `cargo xtask web`");
}
