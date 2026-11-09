//! yah desktop entry point.
//!
//! ```text
//! yah                         desktop window
//! yah --serve [ADDR]          desktop window + browser access (default 127.0.0.1:7470)
//! yah --headless [ADDR]       browser access only, no window
//!     --web-dir DIR           built web bundle to serve (default: $YAH_WEB_DIR, else apps/yah-web/dist)
//!     --allow-user LOGIN      only admit this Tailscale login via `tailscale serve` (repeatable)
//!     --inference-url URL     OpenAI-compatible base URL
//!     --model NAME
//!     --workspace DIR         folder tools work in (default: current directory)
//!     --plugin DIR            load the plugin in DIR (repeatable; also <config>/plugins)
//!     --dev-tools             offer trivial test tools (get_time, set_reminder)
//!     --theme NAME            system | dark | light (overrides the saved preference)
//!
//! Testing (see scripts/headless.sh):
//!     --send TEXT             send a message on startup (repeatable)
//!     --screenshot FILE       PNG of the window once the conversation settles, then exit
//!     --screenshot-after SECS minimum wait before the screenshot (default 2)
//!     --open-settings         start with the settings panel open
//! ```
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

mod extensions;
mod gpu;
mod testing;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use yah_agent::{EngineConfig, EngineHandle};
use yah_types::InferenceConfig;

struct Args {
    serve: Option<SocketAddr>,
    headless: bool,
    web_dir: PathBuf,
    allowed_users: Vec<String>,
    inference: InferenceConfig,
    workspace: Option<PathBuf>,
    dev_tools: bool,
    plugins: Vec<PathBuf>,
    theme: Option<yah_ui_egui::theme::ThemeKind>,
    open_settings: bool,
    script: testing::Script,
}

fn parse_args() -> Result<Args, String> {
    let default_addr = SocketAddr::from(([127, 0, 0, 1], 7470));
    let mut args = Args {
        serve: None,
        headless: false,
        allowed_users: Vec::new(),
        web_dir: std::env::var_os("YAH_WEB_DIR").map_or_else(|| PathBuf::from("apps/yah-web/dist"), PathBuf::from),
        inference: InferenceConfig::default(),
        workspace: None,
        dev_tools: false,
        plugins: Vec::new(),
        theme: None,
        open_settings: false,
        script: testing::Script { after: std::time::Duration::from_secs(2), ..Default::default() },
    };
    if let Ok(key) = std::env::var("YAH_API_KEY") {
        args.inference.api_key = Some(key);
    }
    let mut it = std::env::args().skip(1).peekable();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--serve" | "--headless" => {
                args.headless |= arg == "--headless";
                let addr = match it.peek() {
                    Some(next) if !next.starts_with("--") => {
                        let v = it.next().unwrap_or_default();
                        v.parse().map_err(|e| format!("bad address {v:?}: {e}"))?
                    }
                    _ => default_addr,
                };
                args.serve = Some(addr);
            }
            "--web-dir" => args.web_dir = it.next().ok_or("--web-dir needs a path")?.into(),
            "--allow-user" => args.allowed_users.push(it.next().ok_or("--allow-user needs a Tailscale login")?),
            "--inference-url" => args.inference.base_url = it.next().ok_or("--inference-url needs a URL")?,
            "--model" => args.inference.model = it.next().ok_or("--model needs a name")?,
            "--workspace" => args.workspace = Some(it.next().ok_or("--workspace needs a folder")?.into()),
            "--dev-tools" => args.dev_tools = true,
            "--plugin" => args.plugins.push(it.next().ok_or("--plugin needs a folder")?.into()),
            "--theme" => {
                let v = it.next().ok_or("--theme needs a name")?;
                args.theme = Some(yah_ui_egui::theme::ThemeKind::from_name(&v).ok_or_else(|| format!("unknown theme {v:?} (system, dark, light)"))?);
            }
            "--open-settings" => args.open_settings = true,
            "--send" => args.script.send.push(it.next().ok_or("--send needs text")?),
            "--screenshot" => args.script.screenshot = Some(it.next().ok_or("--screenshot needs a file")?.into()),
            "--screenshot-after" => {
                let v = it.next().ok_or("--screenshot-after needs seconds")?;
                let secs: f64 = v.parse().map_err(|e| format!("bad seconds {v:?}: {e}"))?;
                args.script.after = std::time::Duration::try_from_secs_f64(secs).map_err(|e| format!("bad seconds {v:?}: {e}"))?;
            }
            "-h" | "--help" => {
                return Err(
                    "usage: yah [--serve [ADDR] | --headless [ADDR]] [--web-dir DIR] [--allow-user LOGIN]... [--inference-url URL] [--model NAME] [--workspace DIR] [--plugin DIR]... [--dev-tools] [--theme system|dark|light]"
                        .into(),
                );
            }
            other => return Err(format!("unknown argument {other:?} (try --help)")),
        }
    }
    Ok(args)
}

struct App {
    ui: yah_ui_egui::YahApp,
    script: testing::Runner,
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.ui.ui(ui);
        let ctx = ui.ctx().clone();
        self.script.frame(&mut self.ui, &ctx);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, yah_ui_egui::theme::Prefs::STORAGE_KEY, &self.ui.prefs());
    }
}

fn main() -> ExitCode {
    // `yah __sandbox-exec …`: the sandbox helper. Must run before anything starts threads.
    if let Some(code) = yah_sandbox::dispatch_helper() {
        return code;
    }
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("yah: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    let mut config = EngineConfig::new(args.inference);
    if let Some(dir) = args.workspace {
        if !dir.is_dir() {
            return Err(format!("workspace {} is not a folder", dir.display()));
        }
        config.workspace = std::fs::canonicalize(&dir).map_err(|e| format!("workspace {}: {e}", dir.display()))?;
    }
    let extra = extensions::load(&mut config.tools, args.plugins, &config.workspace);
    if !extra.is_empty() {
        config.system_prompt = format!("{}\n\n{extra}", config.system_prompt);
    }
    if args.dev_tools {
        yah_agent::dev_tools::register(&mut config.tools);
    }
    log::info!("{} tools available in {}", config.tools.len(), config.workspace.display());
    let engine = EngineHandle::spawn_with(config).map_err(|e| format!("starting engine: {e}"))?;

    if let Some(addr) = args.serve {
        let config = yah_server::ServerConfig { addr, web_dir: Some(args.web_dir), allowed_users: args.allowed_users, ..Default::default() };
        let bound = yah_server::spawn(engine.clone(), config).map_err(|e| format!("binding {addr}: {e}"))?;
        eprintln!("yah: browser UI at http://{bound}  (expose on your tailnet with: tailscale serve {})", bound.port());
    }

    if args.headless {
        loop {
            std::thread::park();
        }
    }

    // Scripted (screenshot) runs must not read or overwrite the user's saved window/appearance.
    let scratch_state = args.script.screenshot.as_ref().map(|_| std::env::temp_dir().join(format!("yah-headless-{}.ron", std::process::id())));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 750.0]).with_title("yah"),
        wgpu_options: gpu::options(),
        persistence_path: scratch_state.clone(),
        ..Default::default()
    };
    let script = testing::Runner::new(args.script);
    let (theme, open_settings) = (args.theme, args.open_settings);
    let result = eframe::run_native(
        "yah",
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            let conn = engine.connect(Arc::new(move || ctx.request_repaint()));
            let mut ui = yah_ui_egui::YahApp::new(Box::new(conn), "desktop");
            let mut prefs: yah_ui_egui::theme::Prefs =
                cc.storage.and_then(|s| eframe::get_value(s, yah_ui_egui::theme::Prefs::STORAGE_KEY)).unwrap_or_default();
            if let Some(theme) = theme {
                prefs.theme = theme;
            }
            ui.set_prefs(prefs);
            ui.set_settings_open(open_settings);
            Ok(Box::new(App { ui, script }))
        }),
    )
    .map_err(|e| e.to_string());
    if let Some(path) = scratch_state {
        let _ = std::fs::remove_file(path);
    }
    result
}
