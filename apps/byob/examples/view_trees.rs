//! Draws the demo view trees with byob's theme and saves a screenshot, for looking at the
//! renderer: `view_trees OUT.png [--light]`. Run it on Xvfb (see `scripts/headless.sh`):
//!
//! ```sh
//! cargo build --example view_trees -p byob
//! xvfb-run -s '-screen 0 1600x1000x24' target/debug/examples/view_trees /tmp/trees.png
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use byob_protocol::tree::{self, Kind, Node, Patch, Reply, TableColumn};
use byob_ui_egui::theme::{self, Prefs, Tokens};
use byob_ui_egui::tree::TreeView;
use serde_json::json;

const DASHBOARD: &str = include_str!("../../../crates/protocol/tests/trees/dashboard.json");
const SETTINGS: &str = include_str!("../../../crates/protocol/tests/trees/settings.json");
const CANVAS: &str = include_str!("../../../crates/protocol/tests/trees/canvas.json");

fn todo() -> Node {
    let item = |i: usize, t: &str, done: bool| {
        json!({"type": "row", "key": format!("item-{i}"), "children": [
            {"type": "toggle", "name": format!("done-{i}"), "label": t, "value": done, "action": format!("toggle:{i}")},
            {"type": "button", "label": "Remove", "action": format!("remove:{i}"), "danger": true}]})
    };
    let items = ["Buy milk", "Write the report", "Call the bank", "Fix the bike"];
    tree::decode(json!({"type": "card", "key": "root", "title": "Todos", "children": [
        {"type": "row", "children": [
            {"type": "input", "name": "new", "placeholder": "What needs doing?", "submit": "add"},
            {"type": "button", "label": "Add", "action": "add", "primary": true},
            {"type": "chip", "key": "count", "text": "3 left", "tone": "accent"}]},
        {"type": "column", "key": "list", "children": items.iter().enumerate().map(|(i, t)| item(i, t, i == 1)).collect::<Vec<_>>()},
        {"type": "text", "text": "Double-click a Qty or Price cell to edit it.", "style": "small", "tone": "dim"}]}))
    .0
}

fn grid(n: usize) -> Node {
    let rows = (0..n)
        .map(|i| vec![json!(format!("SKU-{i:05}")), json!(format!("Widget {i}")), json!(i % 17 + 1), json!(((150 + (i % 50) * 37) as f64) / 100.0)])
        .collect();
    let col = |k: &str, l: &str, w: Option<f32>, editable: bool| TableColumn { key: k.into(), label: l.into(), width: w, editable };
    let columns = vec![
        col("sku", "SKU", Some(90.0), false),
        col("name", "Name", None, false),
        col("qty", "Qty", Some(60.0), true),
        col("price", "Price", Some(70.0), true),
    ];
    Node::new(Kind::Card {
        title: format!("{n} rows (virtualized)"),
        children: vec![Node::keyed("grid", Kind::Table { name: "grid".into(), columns, rows, height: Some(260.0), select: Some("pick".into()) })],
    })
}

struct App {
    t: Tokens,
    columns: Vec<Vec<TreeView>>,
    frame: u32,
    out: PathBuf,
    done: Arc<AtomicBool>,
    asked: bool,
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _f: &mut eframe::Frame) {
        self.frame += 1;
        let t = self.t;
        egui::Frame::new().fill(t.app_bg).inner_margin(16.0).show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            ui.columns(self.columns.len(), |cols| {
                for (ui, views) in cols.iter_mut().zip(self.columns.iter_mut()) {
                    for v in views.iter_mut() {
                        for e in v.show(ui, &t) {
                            eprintln!("event: {}", serde_json::to_string(&e).unwrap_or_default());
                        }
                        ui.add_space(12.0);
                    }
                }
            });
        });
        // A reply after a few frames: the chip changes, typed text stays.
        if self.frame == 4
            && let Some(v) = self.columns.get_mut(2).and_then(|c| c.first_mut())
        {
            let patch: Vec<Patch> =
                serde_json::from_value(json!([{"set": {"key": "count", "fields": {"text": "2 left", "tone": "success"}}}])).unwrap_or_default();
            let faults = v.reply(Reply { patch, ..Reply::default() });
            assert!(faults.is_empty(), "{faults:?}");
        }
        let ctx = ui.ctx().clone();
        if self.done.load(Ordering::Acquire) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else if self.frame >= 10 && !self.asked {
            self.asked = true;
            let (out, done) = (self.out.clone(), Arc::clone(&self.done));
            ctx.request_screenshot(move |image| {
                if let Err(e) = save_png(&out, &image) {
                    eprintln!("screenshot failed: {e}");
                }
                done.store(true, Ordering::Release);
            });
        }
        ctx.request_repaint();
    }
}

fn save_png(path: &PathBuf, image: &egui::ColorImage) -> Result<(), String> {
    let [w, h] = image.size;
    let (w, h) = (u32::try_from(w).map_err(|e| e.to_string())?, u32::try_from(h).map_err(|e| e.to_string())?);
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(image.as_raw()).map_err(|e| e.to_string())
}

fn main() -> eframe::Result {
    let mut args = std::env::args().skip(1);
    let out = PathBuf::from(args.next().unwrap_or_else(|| "view-trees.png".into()));
    let dark = !args.any(|a| a == "--light");
    let parse = |s: &str| {
        let (n, faults) = tree::decode_str(s);
        assert!(faults.is_empty(), "{faults:?}");
        TreeView::new(n)
    };
    let columns = vec![vec![parse(DASHBOARD)], vec![parse(SETTINGS), parse(CANVAS)], vec![TreeView::new(todo()), TreeView::new(grid(10_000))]];
    let t = Tokens::for_prefs(&Prefs::default(), Some(if dark { egui::Theme::Dark } else { egui::Theme::Light }), &[]);
    let opts = eframe::NativeOptions { viewport: egui::ViewportBuilder::default().with_inner_size([1500.0, 900.0]), ..Default::default() };
    eframe::run_native(
        "byob view trees",
        opts,
        Box::new(move |cc| {
            theme::install_fonts(&cc.egui_ctx, Default::default(), Default::default());
            t.apply(&cc.egui_ctx);
            Ok(Box::new(App { t, columns, frame: 0, out, done: Arc::new(AtomicBool::new(false)), asked: false }))
        }),
    )
}
