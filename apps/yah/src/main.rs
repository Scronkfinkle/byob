//! yah desktop entry point.
//!
//! ```text
//! yah                         desktop window
//! yah --serve [ADDR]          desktop window + browser access (default 127.0.0.1:7470)
//! yah --headless [ADDR]       browser access only, no window
//!     --web-dir DIR           built web bundle to serve (default: $YAH_WEB_DIR, else apps/yah-web/dist)
//!     --allow-user LOGIN      only admit this Tailscale login via `tailscale serve` (repeatable)
//!     --inference-url URL     OpenAI-compatible base URL (default: settings.json)
//!     --model NAME            (default: settings.json)
//!     --workspace DIR         folder new chats work in (default: settings.json, else current directory)
//!     --plugin DIR            load the plugin in DIR (repeatable; also <config>/plugins)
//!     --auto-approve          allow every tool call without asking (testing)
//!     --dev-tools             offer trivial test tools (get_time, set_reminder)
//!     --theme NAME            system | dark | light (overrides the saved preference)
//!
//! Testing (see scripts/headless.sh):
//!     --send TEXT             send a message on startup (repeatable)
//!     --attach FILE           attach (upload) a file to the first message (repeatable)
//!     --screenshot FILE       PNG of the window once the conversation settles, then exit
//!     --screenshot-after SECS minimum wait before the screenshot (default 2)
//!     --open-settings         start with the settings panel open
//!     --tool-mode MODE        auto | native | prompt: force how tools are passed to the model
//! ```
//!
//! Chats live in `yah.db`; preferences in `settings.json` and the API key in `secrets.json`
//! (all under `$YAH_HOME` if set, else the platform config/data directories). Flags and
//! `YAH_API_KEY` override the files for this run; changing the endpoint in the UI saves it.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

mod extensions;
mod gpu;
mod testing;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use yah_agent::{EngineConfig, EngineHandle};
use yah_store::{Paths, Secrets, Settings, Theme};
use yah_types::InferenceConfig;

#[derive(Default)]
struct Args {
    serve: Option<SocketAddr>,
    headless: bool,
    web_dir: PathBuf,
    allowed_users: Vec<String>,
    inference_url: Option<String>,
    model: Option<String>,
    api_key: Option<String>,
    workspace: Option<PathBuf>,
    dev_tools: bool,
    /// Allow every tool call without asking (scripted test runs).
    auto_approve: bool,
    plugins: Vec<PathBuf>,
    theme: Option<yah_ui_egui::theme::ThemeKind>,
    open_settings: bool,
    tool_mode: yah_agent::ToolMode,
    script: testing::Script,
}

fn parse_args() -> Result<Args, String> {
    let default_addr = SocketAddr::from(([127, 0, 0, 1], 7470));
    let mut args = Args {
        serve: None,
        headless: false,
        allowed_users: Vec::new(),
        web_dir: std::env::var_os("YAH_WEB_DIR").map_or_else(|| PathBuf::from("apps/yah-web/dist"), PathBuf::from),
        inference_url: None,
        model: None,
        api_key: std::env::var("YAH_API_KEY").ok().filter(|k| !k.is_empty()),
        workspace: None,
        dev_tools: false,
        auto_approve: false,
        plugins: Vec::new(),
        theme: None,
        open_settings: false,
        tool_mode: yah_agent::ToolMode::Auto,
        script: testing::Script { after: std::time::Duration::from_secs(2), ..Default::default() },
    };
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
            "--inference-url" => args.inference_url = Some(it.next().ok_or("--inference-url needs a URL")?),
            "--model" => args.model = Some(it.next().ok_or("--model needs a name")?),
            "--workspace" => args.workspace = Some(it.next().ok_or("--workspace needs a folder")?.into()),
            "--dev-tools" => args.dev_tools = true,
            "--auto-approve" => args.auto_approve = true,
            "--plugin" => args.plugins.push(it.next().ok_or("--plugin needs a folder")?.into()),
            "--theme" => {
                let v = it.next().ok_or("--theme needs a name")?;
                args.theme = Some(yah_ui_egui::theme::ThemeKind::from_name(&v).ok_or_else(|| format!("unknown theme {v:?} (system, dark, light)"))?);
            }
            "--open-settings" => args.open_settings = true,
            // Hidden, for testing: force native or prompt-based tool calling.
            "--tool-mode" => {
                let v = it.next().ok_or("--tool-mode needs auto, native or prompt")?;
                args.tool_mode = yah_agent::ToolMode::parse(&v).ok_or_else(|| format!("unknown tool mode {v:?} (auto, native, prompt)"))?;
            }
            "--send" => args.script.send.push(it.next().ok_or("--send needs text")?),
            "--attach" => args.script.attach.push(it.next().ok_or("--attach needs a file")?.into()),
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

/// What the settings files say; defaults when there is no usable home directory.
struct Saved {
    paths: Option<Paths>,
    settings: Settings,
    secrets: Secrets,
}

fn load_saved() -> Saved {
    match Paths::resolve() {
        Ok(paths) => {
            // Warnings (corrupt file kept aside, loose permissions fixed) are logged by the store.
            let settings = Settings::load(&paths.settings()).value;
            let secrets = Secrets::load(&paths.secrets()).value;
            Saved { paths: Some(paths), settings, secrets }
        }
        Err(e) => {
            log::warn!("{e}; chats and settings won't be saved this run");
            Saved { paths: None, settings: Settings::default(), secrets: Secrets::default() }
        }
    }
}

/// Flags (and `YAH_API_KEY`) beat settings.json / secrets.json, which beat built-in defaults.
fn inference_config(args: &Args, settings: &Settings, secrets: &Secrets) -> InferenceConfig {
    let defaults = InferenceConfig::default();
    let pick = |flag: &Option<String>, saved: &Option<String>, default: String| {
        flag.clone().or_else(|| saved.clone().filter(|s| !s.trim().is_empty())).unwrap_or(default)
    };
    InferenceConfig {
        base_url: pick(&args.inference_url, &settings.inference.base_url, defaults.base_url),
        model: pick(&args.model, &settings.inference.model, defaults.model),
        api_key: args.api_key.clone().or_else(|| secrets.api_key.clone().filter(|k| !k.is_empty())),
    }
}

/// `--workspace`, else the saved default, else the current directory.
fn workspace(args: &Args, settings: &Settings) -> Result<Option<PathBuf>, String> {
    let (dir, from_flag) = match (&args.workspace, &settings.workspace) {
        (Some(dir), _) => (dir.clone(), true),
        (None, Some(dir)) => (dir.clone(), false),
        (None, None) => return Ok(None),
    };
    if !dir.is_dir() {
        if from_flag {
            return Err(format!("workspace {} is not a folder", dir.display()));
        }
        log::warn!("the saved workspace {} is not a folder; using the current directory", dir.display());
        return Ok(None);
    }
    std::fs::canonicalize(&dir).map(Some).map_err(|e| format!("workspace {}: {e}", dir.display()))
}

fn theme_kind(theme: Theme) -> yah_ui_egui::theme::ThemeKind {
    use yah_ui_egui::theme::ThemeKind;
    match theme {
        Theme::System => ThemeKind::System,
        Theme::Light => ThemeKind::Light,
        Theme::Dark => ThemeKind::Dark,
    }
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    let saved = load_saved();
    let mut config = EngineConfig::new(inference_config(&args, &saved.settings, &saved.secrets));
    config.paths = saved.paths;
    config.adapt.tool_mode = args.tool_mode;
    if args.auto_approve {
        log::warn!("--auto-approve: every tool call runs without asking");
        let allow = yah_agent::Rule::Allow;
        config.policy = yah_agent::Policy { read: allow, write: allow, network: allow, execute: allow };
    }
    if let Some(dir) = workspace(&args, &saved.settings)? {
        config.workspace = dir;
    }
    let saved_theme = theme_kind(saved.settings.theme);
    let plugins = args.plugins.clone();
    let extra = extensions::load(&mut config.tools, plugins, &config.workspace);
    if !extra.is_empty() {
        config.system_prompt = format!("{}\n\n{extra}", config.system_prompt);
    }
    if args.dev_tools {
        yah_agent::dev_tools::register(&mut config.tools);
    }
    log::info!("{} tools available; new chats work in {}", config.tools.len(), config.workspace.display());
    let engine = EngineHandle::spawn_with(config).map_err(|e| format!("starting engine: {e}"))?;
    run_ui(args, engine, saved_theme)
}

/// Where "Open in browser" finds artifacts: the `--serve` server if there is one, else a
/// loopback server on a random port that serves only artifacts, started on first use.
fn link_base(engine: EngineHandle, serving: Option<SocketAddr>) -> yah_ui_egui::artifact::LinkBase {
    let started: std::sync::Mutex<Option<SocketAddr>> = std::sync::Mutex::new(serving.map(|mut a| {
        if a.ip().is_unspecified() {
            a.set_ip(std::net::Ipv4Addr::LOCALHOST.into());
        }
        a
    }));
    Box::new(move || {
        let mut slot = started.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let addr = match *slot {
            Some(a) => a,
            None => {
                let config = yah_server::ServerConfig { addr: SocketAddr::from(([127, 0, 0, 1], 0)), artifacts_only: true, ..Default::default() };
                let a = yah_server::spawn(engine.clone(), config).map_err(|e| format!("couldn't start the artifact server: {e}"))?;
                *slot = Some(a);
                a
            }
        };
        Ok(format!("http://{addr}"))
    })
}

fn run_ui(args: Args, engine: EngineHandle, saved_theme: yah_ui_egui::theme::ThemeKind) -> Result<(), String> {
    let mut serving = None;
    if let Some(addr) = args.serve {
        let config = yah_server::ServerConfig { addr, web_dir: Some(args.web_dir), allowed_users: args.allowed_users, ..Default::default() };
        let bound = yah_server::spawn(engine.clone(), config).map_err(|e| format!("binding {addr}: {e}"))?;
        eprintln!("yah: browser UI at http://{bound}  (expose on your tailnet with: tailscale serve {})", bound.port());
        serving = Some(bound);
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
            ui.set_link_base(link_base(engine.clone(), serving));
            // This device's saved appearance, else the theme from settings.json.
            let stored: Option<yah_ui_egui::theme::Prefs> = cc.storage.and_then(|s| eframe::get_value(s, yah_ui_egui::theme::Prefs::STORAGE_KEY));
            let mut prefs = stored.unwrap_or(yah_ui_egui::theme::Prefs { theme: saved_theme, ..Default::default() });
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

#[cfg(test)]
mod tests {
    use super::*;

    fn saved(url: Option<&str>, model: Option<&str>, key: Option<&str>) -> (Settings, Secrets) {
        let mut settings = Settings::default();
        settings.inference.base_url = url.map(Into::into);
        settings.inference.model = model.map(Into::into);
        let secrets = Secrets { api_key: key.map(Into::into), ..Default::default() };
        (settings, secrets)
    }

    #[test]
    fn flags_beat_saved_settings_which_beat_defaults() {
        let (settings, secrets) = saved(Some("http://saved:1/v1"), Some("saved-model"), Some("sk-saved"));
        let none = Args::default();
        let c = inference_config(&none, &settings, &secrets);
        assert_eq!((c.base_url.as_str(), c.model.as_str(), c.api_key.as_deref()), ("http://saved:1/v1", "saved-model", Some("sk-saved")));

        let flags = Args { inference_url: Some("http://flag:2/v1".into()), api_key: Some("sk-env".into()), ..Default::default() };
        let c = inference_config(&flags, &settings, &secrets);
        assert_eq!((c.base_url.as_str(), c.model.as_str(), c.api_key.as_deref()), ("http://flag:2/v1", "saved-model", Some("sk-env")));

        let (empty, no_key) = saved(Some("  "), None, Some(""));
        let c = inference_config(&none, &empty, &no_key);
        assert_eq!(c, InferenceConfig::default());
    }

    #[test]
    fn workspace_flag_beats_settings_and_a_missing_saved_folder_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let settings = Settings { workspace: Some(dir.path().into()), ..Default::default() };
        let real = |p: &std::path::Path| std::fs::canonicalize(p).unwrap();
        assert_eq!(workspace(&Args::default(), &settings).unwrap(), Some(real(dir.path())));
        let flag = Args { workspace: Some(other.path().into()), ..Default::default() };
        assert_eq!(workspace(&flag, &settings).unwrap(), Some(real(other.path())));
        let gone = Settings { workspace: Some(dir.path().join("missing")), ..Default::default() };
        assert_eq!(workspace(&Args::default(), &gone).unwrap(), None);
        let bad_flag = Args { workspace: Some(dir.path().join("missing")), ..Default::default() };
        assert!(workspace(&bad_flag, &settings).is_err());
    }
}
