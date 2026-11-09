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
                    let conn = yah_ui_egui::ws::WsConnection::connect(&ws_url(), cc.egui_ctx.clone());
                    let link = conn.status_handle();
                    Ok(Box::new(App { ui: yah_ui_egui::YahApp::new(Box::new(conn), "browser"), link }))
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
struct App {
    ui: yah_ui_egui::YahApp,
    link: yah_ui_egui::ws::LinkStatusHandle,
}

#[cfg(target_arch = "wasm32")]
impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.ui.ui(ui);
        // One quiet banner while the link is down, instead of a notice per retry.
        if let Some(label) = self.link.get().label() {
            egui::Area::new(egui::Id::unique("yah-link-status"))
                .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 8.0))
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

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("yah-web only runs in the browser: build it with `cargo xtask web`");
}
