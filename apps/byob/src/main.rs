//! byob desktop entry point.
//!
//! ```text
//! byob                         desktop window
//! byob --serve [ADDR]          desktop window + browser access (default 127.0.0.1:7470)
//! byob --headless [ADDR]       browser access only, no window
//! byob --background            start with the window hidden (at login), if closing it would
//!                             hide it too (Settings → System → Keep running in the background)
//!     --web-dir DIR           serve the browser UI from DIR (or $BYOB_WEB_DIR) instead of the built-in copy
//!     --allow-user LOGIN      only admit this Tailscale login via `tailscale serve` (repeatable)
//!     --inference-url URL     OpenAI-compatible base URL (default: settings.json)
//!     --model NAME            (default: settings.json)
//!     --workspace DIR         the workspace new chats start in: the one for DIR, made if there is
//!                             none (default: settings.json `workspace`, else the current directory)
//!     --plugin DIR            load the plugin in DIR (repeatable; also <config>/plugins)
//!     --auto-approve          allow every tool call without asking (testing); loads workspace
//!                             environments for this run only; system-wide installs still ask
//!     --yolo                  never ask, for this run: like --auto-approve, and what a person is
//!                             normally asked to confirm runs too (system-wide installs included)
//!     --dev-tools             offer trivial test tools (get_time, set_reminder)
//!     --storage KIND          sqlite | folder: chat storage for this run (default: settings.json `storage`)
//!     --chats-folder DIR      root of the folder storage (default: settings.json `chats_folder`,
//!                             else <data>/chats-folder)
//!     --export-chats-to KIND  copy every chat from the current storage into KIND (sqlite |
//!                             folder), then exit; chats already there are skipped
//!     --theme NAME            system | dark | light, or a theme: a preset id (catppuccin-mocha,
//!                             gruvbox-dark, nord, …) or the name of a file in <config>/themes
//!                             (overrides the saved preference)
//!     --backdrop FILE         a shader behind glass panels (experimental): a .wgsl file with
//!                             fn scene (see byob_ui_egui::cockpit::wgsl::GUIDE), or off
//!                             (overrides the saved preference; BYOB_NO_BACKDROP=1 disables them)
//!     --arrangement NAME      where the window's parts are (experimental): docked, floating
//!                             or focus (overrides the saved preference)
//!     --arranging             start with the free arrangement's handles showing (testing)
//!     --look KEY=VALUE        change one appearance setting as the model's ui_look would
//!                             (repeatable; e.g. --look controls=hud --look accent=orange)
//!
//! Testing (see scripts/headless.sh):
//!     --send TEXT             send a message on startup (repeatable)
//!     --attach FILE           attach (upload) a file to the first message (repeatable)
//!     --screenshot FILE       PNG of the window once the conversation settles, then exit
//!     --screenshot-after SECS minimum wait before the screenshot (default 2)
//!     --open-settings         start with the settings panel open
//!     --settings-section NAME open the settings on a tab (models, defaults, plugins, appearance,
//!                             text, colors; chats, knobs and advanced still work), at one
//!                             setting (its id: accent, approvals, zoom, …) or at model
//!                             profile ID's form, Advanced expanded (model:ID)
//!     --settings-search TEXT  open the settings with TEXT typed in their search field
//!     --window-size WxH       window size in points (default 1100x750)
//!     --tool-mode MODE        auto | native | prompt: force how tools are passed to the model
//!     --compact-at TOKENS     compact the context when a request passes TOKENS (default:
//!                             settings.json `compaction`, 80% of the window)
//!     --test-connection URL MODEL  fill the connection form and press Test connection
//!     --new-workspace NAME ENV  create workspace NAME (auto | host | nix | container) in a fresh
//!                             folder through the New workspace form, then send the messages in it
//!     --open-panel NAME       open new-workspace, new-workspace-container, workspace-settings,
//!                             chat-settings, more-menu (the header's ⋯ menu) or plugins
//!                             (Settings → Plugins) once the messages are done; the chat
//!                             list's new-chat-menu, workspace-menu, sidebar-hover (rows as
//!                             if hovered), chat-list (open, phones too), new-chat,
//!                             search:TEXT, palette[:TEXT] (the search palette), footer-menu,
//!                             projects, projects-grid, new-project-empty,
//!                             new-workspace-folder:PATH (the form as if that
//!                             folder was dropped); also
//!                             plugin-pane[:PLUGIN[/VIEW]] (a plugin's pane) and plugin:ID
//!                             (Settings → Plugins open on that plugin), artifact:PREFIX (the
//!                             newest artifact whose id, title or file name starts so, in the pane)
//!     --draft TEXT            type TEXT into the composer once the messages are done, unsent
//! ```
//!
//! One byob runs per data folder: launching it again shows the running one's window
//! (`<data>/instance.lock`, see `background::instance`). Scripted (`--screenshot`) runs don't take part.
//!
//! Chats live in `byob.db` (or, with `storage = "folder"`, in plain files); preferences in `settings.json` and the API key in `secrets.json`
//! (all under `$BYOB_HOME` if set, else the platform config/data directories). Flags and
//! `BYOB_API_KEY` override the files for this run; changing the endpoint in the UI saves it.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

mod apphost;
mod background;
mod extensions;
mod gpu;
mod launch_env;
mod testing;
mod tokens;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use byob_agent::{EngineConfig, EngineHandle};
use byob_store::{Paths, Secrets, Settings, StorageKind, Theme};
use byob_types::InferenceConfig;

#[derive(Default)]
struct Args {
    serve: Option<SocketAddr>,
    headless: bool,
    /// `--background`: start with the window hidden (login launches).
    background: bool,
    /// Explicit `--web-dir` / `$BYOB_WEB_DIR`; otherwise the embedded UI.
    web_dir: Option<PathBuf>,
    allowed_users: Vec<String>,
    inference_url: Option<String>,
    model: Option<String>,
    api_key: Option<String>,
    workspace: Option<PathBuf>,
    dev_tools: bool,
    /// Allow every tool call without asking (scripted test runs).
    auto_approve: bool,
    yolo: bool,
    plugins: Vec<PathBuf>,
    theme: Option<String>,
    backdrop: Option<String>,
    arrangement: Option<String>,
    arranging: bool,
    look: Vec<(String, String)>,
    open_settings: bool,
    settings_section: Option<String>,
    settings_search: Option<String>,
    window_size: Option<[f32; 2]>,
    tool_mode: byob_agent::ToolMode,
    compact_at: Option<usize>,
    storage: Option<StorageKind>,
    chats_folder: Option<PathBuf>,
    export_to: Option<StorageKind>,
    script: testing::Script,
    /// `--help`: print [`USAGE`] and exit.
    help: bool,
}

const USAGE: &str = "usage: byob [--serve [ADDR] | --headless [ADDR]] [--background] [--web-dir DIR] [--allow-user LOGIN]... [--inference-url URL] [--model NAME] [--workspace DIR] [--storage sqlite|folder] [--chats-folder DIR] [--export-chats-to sqlite|folder] [--plugin DIR]... [--auto-approve | --yolo] [--dev-tools] [--theme system|dark|light|THEME] [--backdrop FILE|off] [--arrangement docked|floating|focus] [--look KEY=VALUE]...";

fn storage_kind(flag: &str, v: Option<String>) -> Result<StorageKind, String> {
    match v.as_deref() {
        Some("sqlite") => Ok(StorageKind::Sqlite),
        Some("folder") => Ok(StorageKind::Folder),
        Some(other) => Err(format!("unknown storage {other:?} for {flag} (sqlite, folder)")),
        None => Err(format!("{flag} needs sqlite or folder")),
    }
}

/// Where the browser UI comes from: an explicit `--web-dir`/`$BYOB_WEB_DIR`; else the copy
/// compiled into this binary (feature `web`); else, in a dev checkout without it, the
/// `cargo xtask web` output.
fn web_dir(explicit: Option<PathBuf>) -> Option<PathBuf> {
    if explicit.is_some() || byob_server::embedded_web_available() {
        return explicit;
    }
    let dev = PathBuf::from("apps/byob-web/dist");
    dev.join("index.html").is_file().then_some(dev)
}

fn parse_args() -> Result<Args, String> {
    parse_args_from(std::env::args().skip(1))
}

fn parse_args_from(argv: impl Iterator<Item = String>) -> Result<Args, String> {
    let default_addr = SocketAddr::from(([127, 0, 0, 1], 7470));
    let mut args = Args {
        serve: None,
        headless: false,
        background: false,
        allowed_users: Vec::new(),
        web_dir: std::env::var_os("BYOB_WEB_DIR").map(PathBuf::from),
        inference_url: None,
        model: None,
        api_key: std::env::var("BYOB_API_KEY").ok().filter(|k| !k.is_empty()),
        workspace: None,
        dev_tools: false,
        auto_approve: false,
        yolo: false,
        plugins: Vec::new(),
        theme: None,
        backdrop: None,
        arrangement: None,
        arranging: false,
        look: Vec::new(),
        open_settings: false,
        settings_section: None,
        settings_search: None,
        window_size: None,
        tool_mode: byob_agent::ToolMode::Auto,
        compact_at: None,
        storage: None,
        chats_folder: None,
        export_to: None,
        script: testing::Script { after: std::time::Duration::from_secs(2), ..Default::default() },
        help: false,
    };
    let mut it = argv.peekable();
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
            "--background" => args.background = true,
            "--web-dir" => args.web_dir = Some(it.next().ok_or("--web-dir needs a path")?.into()),
            "--allow-user" => args.allowed_users.push(it.next().ok_or("--allow-user needs a Tailscale login")?),
            "--inference-url" => args.inference_url = Some(it.next().ok_or("--inference-url needs a URL")?),
            "--model" => args.model = Some(it.next().ok_or("--model needs a name")?),
            "--workspace" => args.workspace = Some(it.next().ok_or("--workspace needs a folder")?.into()),
            "--storage" => args.storage = Some(storage_kind("--storage", it.next())?),
            "--chats-folder" => args.chats_folder = Some(it.next().ok_or("--chats-folder needs a folder")?.into()),
            "--export-chats-to" => args.export_to = Some(storage_kind("--export-chats-to", it.next())?),
            "--dev-tools" => args.dev_tools = true,
            "--auto-approve" => args.auto_approve = true,
            "--yolo" => args.yolo = true,
            "--plugin" => args.plugins.push(it.next().ok_or("--plugin needs a folder")?.into()),
            "--theme" => {
                // Checked once the themes folder is loaded (user themes count too).
                args.theme = Some(it.next().ok_or("--theme needs a name")?);
            }
            "--backdrop" => {
                let name = it.next().ok_or("--backdrop needs a .wgsl file or off")?;
                args.backdrop = Some(backdrop_source(&name)?);
            }
            "--arrangement" => {
                let name = it.next().ok_or("--arrangement needs a name (docked, floating, focus)")?;
                if name != "docked" && byob_ui_egui::cockpit::arrange::Arrangement::preset(&name).is_none() {
                    return Err(format!("--arrangement {name}: docked, floating or focus"));
                }
                args.arrangement = Some(name);
            }
            "--arranging" => args.arranging = true,
            "--look" => {
                let kv = it.next().ok_or("--look needs KEY=VALUE")?;
                let (k, v) = kv.split_once('=').ok_or_else(|| format!("--look {kv}: KEY=VALUE"))?;
                args.look.push((k.trim().to_owned(), v.to_owned()));
            }
            "--open-settings" => args.open_settings = true,
            "--settings-section" => {
                args.settings_section =
                    Some(it.next().ok_or("--settings-section needs a tab (models, defaults, plugins, appearance, text, colors) or model:<id>")?)
            }
            "--settings-search" => args.settings_search = Some(it.next().ok_or("--settings-search needs text")?),
            "--window-size" => {
                let v = it.next().ok_or("--window-size needs WxH")?;
                let size = v.split_once('x').and_then(|(w, h)| Some([w.parse::<f32>().ok()?, h.parse::<f32>().ok()?]));
                args.window_size = Some(
                    size.filter(|[w, h]| (200.0..=8000.0).contains(w) && (200.0..=8000.0).contains(h))
                        .ok_or_else(|| format!("bad window size {v:?} (e.g. 400x850)"))?,
                );
            }
            // Hidden, for testing: force native or prompt-based tool calling.
            "--tool-mode" => {
                let v = it.next().ok_or("--tool-mode needs auto, native or prompt")?;
                args.tool_mode = byob_agent::ToolMode::parse(&v).ok_or_else(|| format!("unknown tool mode {v:?} (auto, native, prompt)"))?;
            }
            // Hidden, for testing: a low compaction threshold in tokens.
            "--compact-at" => {
                let v = it.next().ok_or("--compact-at needs a token count")?;
                args.compact_at = Some(v.parse().map_err(|e| format!("bad token count {v:?}: {e}"))?);
            }
            "--send" => args.script.send.push(it.next().ok_or("--send needs text")?),
            // Hidden, for testing: send TEXT while the first reply runs, so it queues.
            "--queue" => args.script.queue.push(it.next().ok_or("--queue needs text")?),
            // Hidden, for testing: press Stop SECS after the queued messages went out.
            "--stop-after" => {
                let v = it.next().ok_or("--stop-after needs seconds")?;
                let secs: f64 = v.parse().map_err(|e| format!("bad seconds {v:?}: {e}"))?;
                args.script.stop_after = Some(std::time::Duration::try_from_secs_f64(secs).map_err(|e| format!("bad seconds {v:?}: {e}"))?);
            }
            // Hidden, for testing: screenshot as soon as the queued messages show.
            "--shoot-queued" => args.script.shoot_queued = true,
            // Hidden, for testing: with --shoot-queued, edit the first queued message first.
            "--edit-queued" => args.script.edit_queued = true,
            // Hidden, for testing: fill the connection form with URL and MODEL and test it.
            "--test-connection" => {
                let url = it.next().ok_or("--test-connection needs a URL and a model")?;
                let model = it.next().ok_or("--test-connection needs a URL and a model")?;
                args.script.test_connection = Some((url, model));
            }
            // Hidden, for testing: make a workspace through the New workspace form first.
            "--new-workspace" => {
                let name = it.next().ok_or("--new-workspace needs a name and auto, host, nix or container")?;
                let env = match it.next().as_deref() {
                    Some("auto") => byob_types::config::EnvKind::Auto,
                    Some("host") => byob_types::config::EnvKind::Host,
                    Some("nix") => byob_types::config::EnvKind::Nix,
                    Some("container") => byob_types::config::EnvKind::Container,
                    other => return Err(format!("--new-workspace {name}: unknown environment {other:?} (auto, host, nix, container)")),
                };
                args.script.new_workspace = Some((name, env));
            }
            // Hidden, for testing: open a workspace view (new-workspace, workspace-settings, …).
            "--open-panel" => {
                let v = it.next().ok_or("--open-panel needs a name")?;
                let plugin_panel = v == "plugin-pane"
                    || v.starts_with("plugin-pane:")
                    || v.starts_with("plugin:")
                    || v.starts_with("artifact:")
                    || v.starts_with("add-models:")
                    || v.starts_with("search:")
                    || v.starts_with("palette:")
                    || v.starts_with("new-workspace-folder:")
                    || v.starts_with("workspace-settings:")
                    || v.starts_with("edit-model:");
                if !plugin_panel && !byob_ui_egui::ByobApp::PANELS.split(", ").any(|p| p == v) {
                    return Err(format!(
                        "unknown panel {v:?} ({}, {}, {})",
                        byob_ui_egui::ByobApp::PANELS,
                        byob_ui_egui::ByobApp::ARG_PANELS,
                        byob_ui_egui::ByobApp::PLUGIN_PANELS
                    ));
                }
                args.script.open_panel = Some(v);
            }
            // Hidden, for testing: type into the composer without sending (`/` opens the menu).
            "--draft" => args.script.draft = Some(it.next().ok_or("--draft needs text")?),
            "--attach" => args.script.attach.push(it.next().ok_or("--attach needs a file")?.into()),
            "--screenshot" => args.script.screenshot = Some(it.next().ok_or("--screenshot needs a file")?.into()),
            // Hidden, for testing: act on the artifacts once the conversation settles.
            "--then" => {
                let v = it.next().ok_or("--then needs a command")?;
                args.script.then.push(testing::Command::parse(&v).ok_or_else(|| format!("unknown command {v:?} ({})", testing::Command::NAMES))?);
            }
            "--screenshot-after" => {
                let v = it.next().ok_or("--screenshot-after needs seconds")?;
                let secs: f64 = v.parse().map_err(|e| format!("bad seconds {v:?}: {e}"))?;
                args.script.after = std::time::Duration::try_from_secs_f64(secs).map_err(|e| format!("bad seconds {v:?}: {e}"))?;
            }
            // Asked for: printed to stdout, and not a failure.
            "-h" | "--help" => {
                args.help = true;
                return Ok(args);
            }
            other => return Err(format!("unknown argument {other:?} (try --help)")),
        }
    }
    Ok(args)
}

struct App {
    ui: byob_ui_egui::ByobApp,
    script: testing::Runner,
    /// Closing hides the window, the tray, notifications (shared with `run_ui`: it outlives the window).
    background: std::rc::Rc<std::cell::RefCell<background::Background>>,
    /// byob in a context of its own, composited onto the window (with a backdrop).
    compositor: Option<byob_ui_egui::cockpit::compositor::Compositor>,
}

impl eframe::App for App {
    // Before each frame, and alone while the window is hidden (eframe runs no frames then):
    // the engine's messages are taken here so look changes don't wait for the window.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.ui.logic(ctx);
        self.background.borrow_mut().logic(ctx, &self.ui);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let (app, script) = (&mut self.ui, &mut self.script);
        let mut frame = |ui: &mut egui::Ui| {
            app.ui(ui);
            let ctx = ui.ctx().clone();
            script.frame(app, &ctx);
        };
        match &mut self.compositor {
            Some(c) => c.run(ui, frame),
            None => frame(ui),
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, byob_ui_egui::theme::Prefs::STORAGE_KEY, &self.ui.prefs());
        eframe::set_value(storage, byob_ui_egui::pane::PanePrefs::STORAGE_KEY, &self.ui.pane_prefs());
        eframe::set_value(storage, byob_ui_egui::sidebar::SidebarPrefs::STORAGE_KEY, &self.ui.sidebar_prefs());
        eframe::set_value(storage, background::Told::STORAGE_KEY, &self.background.borrow().told());
    }
}

fn main() -> ExitCode {
    // `byob __sandbox-exec …`: the sandbox helper. Must run before anything starts threads.
    if let Some(code) = byob_sandbox::dispatch_helper() {
        return code;
    }
    // Started from Finder: the home folder and the login shell's PATH (may exec itself again).
    launch_env::adopt();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("byob: {e}");
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
            // The main connection and model roles of older builds become model profiles
            // (their copies stay, so older builds still read them).
            match byob_store::profiles::migrate_files(&paths) {
                Ok(true) => log::info!("saved the connection and model roles as model profiles"),
                Ok(false) => {}
                Err(e) => log::warn!("couldn't save model profiles: {e}"),
            }
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

/// Flags (and `BYOB_API_KEY`) beat settings.json / secrets.json, which beat built-in defaults.
fn inference_config(args: &Args, settings: &Settings, secrets: &Secrets) -> InferenceConfig {
    let defaults = InferenceConfig::default();
    let pick = |flag: &Option<String>, saved: &Option<String>, default: String| {
        flag.clone().or_else(|| saved.clone().filter(|s| !s.trim().is_empty())).unwrap_or(default)
    };
    let base_url = pick(&args.inference_url, &settings.inference.base_url, defaults.base_url);
    // The saved API belongs to the saved URL; a URL from the command line decides from its host.
    let saved_url = settings.inference.base_url.as_deref().map(|u| u.trim().trim_end_matches('/'));
    let api = settings.inference.api.filter(|_| saved_url == Some(base_url.trim().trim_end_matches('/')));
    InferenceConfig {
        base_url,
        model: pick(&args.model, &settings.inference.model, defaults.model),
        api_key: args.api_key.clone().or_else(|| secrets.api_key.clone().filter(|k| !k.is_empty())),
        api,
        ..Default::default()
    }
}

/// `settings.json` `models`: each role's endpoint, defaulting to the main one. A role's key
/// is `secrets.json` `model_keys.<role>`, else the main key if it is on the main endpoint.
fn model_roles(main: &InferenceConfig, settings: &Settings, secrets: &Secrets) -> std::collections::BTreeMap<String, InferenceConfig> {
    settings
        .models
        .iter()
        .filter(|(name, _)| !name.trim().is_empty())
        .map(|(name, role)| {
            let base_url = role.base_url.clone().filter(|u| !u.trim().is_empty()).unwrap_or_else(|| main.base_url.clone());
            let same = base_url.trim_end_matches('/') == main.base_url.trim_end_matches('/');
            let api_key = secrets.model_keys.get(name).filter(|k| !k.is_empty()).cloned().or_else(|| main.api_key.clone().filter(|_| same));
            let model = role.model.clone().filter(|m| !m.trim().is_empty()).unwrap_or_else(|| main.model.clone());
            let api = role.api.or_else(|| main.api.filter(|_| same));
            (name.clone(), InferenceConfig { base_url, model, api_key, api, ..Default::default() })
        })
        .collect()
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

fn theme_kind(theme: Theme) -> byob_ui_egui::theme::ThemeKind {
    use byob_ui_egui::theme::ThemeKind;
    match theme {
        Theme::System => ThemeKind::System,
        Theme::Light => ThemeKind::Light,
        Theme::Dark => ThemeKind::Dark,
    }
}

/// Flags beat settings.json for where chats live.
fn storage_settings(args: &Args, settings: &Settings) -> Settings {
    Settings {
        storage: args.storage.unwrap_or(settings.storage),
        chats_folder: args.chats_folder.clone().or_else(|| settings.chats_folder.clone()),
        ..settings.clone()
    }
}

/// `--export-chats-to KIND`: copy everything from the storage in use into KIND.
fn export_chats(to: StorageKind, paths: Option<&Paths>, settings: &Settings) -> Result<(), String> {
    let paths = paths.ok_or("no data directory to export from")?;
    if to == settings.storage {
        return Err(format!("chats are already stored in {to:?} storage; pass --storage to pick the source"));
    }
    paths.ensure().map_err(|e| e.to_string())?;
    let from = byob_store::open_backend(settings, paths).map_err(|e| format!("opening the chats: {e}"))?;
    let mut dest = byob_store::open_backend(&Settings { storage: to, ..settings.clone() }, paths).map_err(|e| format!("opening the destination: {e}"))?;
    let report = byob_store::export(&*from, &mut *dest).map_err(|e| format!("exporting: {e}"))?;
    eprintln!(
        "byob: copied {} chat(s) ({} messages, {} compactions, {} global grants) from {} to {}",
        report.chats,
        report.messages,
        report.compactions,
        report.global_grants,
        from.path().display(),
        dest.path().display()
    );
    if !report.skipped.is_empty() {
        eprintln!("byob: skipped {} chat(s) the destination already had", report.skipped.len());
    }
    let name = if to == StorageKind::Folder { "folder" } else { "sqlite" };
    eprintln!("byob: to use it, set \"storage\": \"{name}\" in {} (or run with --storage {name})", paths.settings().display());
    Ok(())
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    if args.help {
        println!("{USAGE}");
        return Ok(());
    }
    tokens::install();
    if std::env::var_os(byob_store::HOME_ENV).is_none() {
        byob_store::Paths::migrate_from_yah();
    }
    let saved = load_saved();
    let storage = storage_settings(&args, &saved.settings);
    if let Some(to) = args.export_to {
        return export_chats(to, saved.paths.as_ref(), &storage);
    }
    let (bus, events) = background::Bus::new();
    // Held until byob exits: the data folder is this process's.
    let _instance = match claim_instance(saved.paths.as_ref(), &args, &bus)? {
        Started::Alone(instance) => instance,
        Started::HandedOver => return Ok(()),
    };
    let mut config = EngineConfig::new(inference_config(&args, &saved.settings, &saved.secrets));
    // Returning users (an endpoint worked before) and explicit `--inference-url` runs go
    // straight to the chat; everyone else gets the welcome screen until something answers.
    config.onboarded = saved.settings.inference.onboarded() || args.inference_url.is_some();
    config.paths = saved.paths;
    config.storage = storage.storage;
    config.chats_folder = storage.chats_folder;
    // Beats every settings level for this run (testing).
    config.adapt.tool_mode = args.tool_mode;
    // The global level of the layered settings (tool mode, approvals, compaction,
    // subagents, instructions); workspaces and chats may override them.
    config.defaults = byob_agent::Layer { base_url: None, model: None, ..saved.settings.global_layer() };
    config.scoped_keys = saved.secrets.scoped_keys.clone();
    config.workspaces_root = saved.settings.workspaces_root();
    let c = saved.settings.compaction;
    // On here; each chat's settings decide (`--compact-at` forces it on everywhere).
    config.compaction =
        byob_agent::CompactOptions { auto: true, threshold_percent: c.threshold_percent, at_tokens: args.compact_at, keep_turns: usize::from(c.keep_turns) };
    if args.auto_approve {
        log::warn!("--auto-approve: every tool call runs without asking");
        config.policy = Some(byob_agent::Policy::ALLOW_ALL);
    }
    if args.yolo {
        log::warn!("--yolo: nothing asks; every tool call runs, also what a person is normally asked to confirm");
        config.policy = Some(byob_agent::Policy::YOLO);
    }
    if let Some(dir) = workspace(&args, &saved.settings)? {
        config.workspace = dir;
        // `--workspace DIR` (or the saved default) selects that folder's workspace,
        // making it one if it isn't yet.
        config.register_workspace = true;
    }
    let saved_theme = theme_kind(saved.settings.theme);
    let themes_dir = config.paths.as_ref().map(Paths::themes_dir);
    let plugins = args.plugins.clone();
    let extra = extensions::load(&mut config.tools, &config.artifact_types, plugins);
    config.agents = extra.agents;
    config.envs = extra.envs;
    config.plugins = Some(extra.plugins);
    config.models = model_roles(&config.inference, &saved.settings, &saved.secrets);
    config.profiles = saved.settings.profiles.clone();
    config.roles = saved.settings.roles.clone();
    config.profile_keys = saved.secrets.profile_keys.clone();
    let sub = saved.settings.subagents;
    config.subagents = byob_agent::SubagentOptions {
        // Whether a chat may delegate is a layered setting (global `subagents.enabled`).
        enabled: true,
        max_parallel: sub.max_parallel.map(usize::from),
        max_calls: usize::from(sub.max_tool_calls.max(1)),
        time_limit: std::time::Duration::from_secs(u64::from(sub.time_limit_minutes.max(1)) * 60),
    };
    if args.dev_tools {
        byob_agent::dev_tools::register(&mut config.tools);
    }
    log::info!("{} tools available; new chats work in {}", config.tools.len(), config.workspace.display());
    let engine = EngineHandle::spawn_with(config).map_err(|e| format!("starting engine: {e}"))?;
    run_ui(args, engine, saved_theme, themes_dir, Outside { bus, events })
}

enum Started {
    /// This process has the data folder (`None`: no single-instance check this run).
    Alone(Option<background::instance::Instance>),
    /// Another byob has it and was asked to show its window: nothing more to do.
    HandedOver,
}

/// One byob per data folder: take it, or ask the byob that has it to show its window.
/// Scripted runs (`--screenshot`) stay out, so they can run beside the real one.
fn claim_instance(paths: Option<&Paths>, args: &Args, bus: &background::Bus) -> Result<Started, String> {
    use background::instance::{Claim, Request, claim};
    let Some(paths) = paths.filter(|_| args.script.screenshot.is_none()) else { return Ok(Started::Alone(None)) };
    match claim(&paths.data_dir) {
        Ok(Claim::First(instance)) => {
            let (bus, headless) = (bus.clone(), args.headless);
            let answer = move |req: Request| match req {
                Request::Show if headless => Err("that byob runs without a window (--headless)".to_owned()),
                Request::Show => {
                    bus.send(background::Event::Show);
                    Ok(())
                }
                Request::Run(_) => Err("that byob has no routines yet".to_owned()),
            };
            if let Err(e) = instance.listen(answer) {
                log::warn!("{e}; launching byob again won't find this one");
            }
            Ok(Started::Alone(Some(instance)))
        }
        Ok(Claim::Taken(running)) if args.headless => {
            Err(format!("byob is already running on {} (pid {}); quit it first, or give this one its own BYOB_HOME", paths.data_dir.display(), running.pid))
        }
        Ok(Claim::Taken(running)) => {
            running.send(&Request::Show).map_err(|e| format!("byob is already running (pid {}), but: {e}", running.pid))?;
            eprintln!("byob: already running (pid {}); showed its window", running.pid);
            Ok(Started::HandedOver)
        }
        Err(e) => {
            log::warn!("{e}; not checking for another byob on this data folder");
            Ok(Started::Alone(None))
        }
    }
}

/// What the window gets from outside it: events (tray, other launches, notifications) and
/// the desktop's own settings.
struct Outside {
    bus: background::Bus,
    events: std::sync::mpsc::Receiver<background::Event>,
}

/// Where "Open in browser" finds artifacts: the `--serve` server if there is one, else a
/// loopback server on a random port that serves only artifacts, started on first use.
fn link_base(engine: EngineHandle, serving: Option<SocketAddr>) -> byob_ui_egui::artifact::LinkBase {
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
                let config = byob_server::ServerConfig { addr: SocketAddr::from(([127, 0, 0, 1], 0)), artifacts_only: true, ..Default::default() };
                let a = byob_server::spawn(engine.clone(), config).map_err(|e| format!("couldn't start the artifact server: {e}"))?;
                *slot = Some(a);
                a
            }
        };
        Ok(format!("http://{addr}"))
    })
}

fn run_ui(args: Args, engine: EngineHandle, saved_theme: byob_ui_egui::theme::ThemeKind, themes_dir: Option<PathBuf>, outside: Outside) -> Result<(), String> {
    use byob_ui_egui::theme;
    let mut serving = None;
    if let Some(addr) = args.serve {
        let config = byob_server::ServerConfig { addr, web_dir: web_dir(args.web_dir), allowed_users: args.allowed_users, ..Default::default() };
        if let Some(warning) = byob_server::webui::UiCheck::of(config.web_dir.as_deref()).warning() {
            eprintln!("byob: warning: {warning}");
        }
        let bound = byob_server::spawn(engine.clone(), config).map_err(|e| format!("binding {addr}: {e}"))?;
        eprintln!("byob: browser UI at http://{bound}  (expose on your tailnet with: tailscale serve {})", bound.port());
        serving = Some(bound);
    }

    if args.headless {
        loop {
            std::thread::park();
        }
    }

    // Scripted (screenshot) runs must not read or overwrite the user's saved window/appearance.
    let scratch_state = args.script.screenshot.as_ref().map(|_| std::env::temp_dir().join(format!("byob-headless-{}.ron", std::process::id())));
    // No smaller than the top bar fits (window buttons, title, badges, menu): smaller, it
    // broke apart and the chat was a sliver of wrapped words.
    let viewport =
        egui::ViewportBuilder::default().with_inner_size(args.window_size.unwrap_or([1100.0, 750.0])).with_min_inner_size([560.0, 520.0]).with_title("byob");
    // macOS: the content runs under a transparent title bar, so the chat list's toggle and
    // Back/Forward sit beside the window buttons (the UI leaves room for them and moves the
    // window from empty space). Windows and Linux keep the system's title bar.
    #[cfg(target_os = "macos")]
    let viewport = viewport.with_fullsize_content_view(true).with_titlebar_shown(false).with_title_shown(false);
    let options = eframe::NativeOptions {
        viewport,
        wgpu_options: gpu::options(),
        persistence_path: scratch_state.clone().or_else(|| window_state_in(std::env::var_os(byob_store::HOME_ENV))),
        ..Default::default()
    };
    // Fail fast on a --theme nothing could match (user themes are checked again below).
    if let Some(name) = &args.theme {
        let user = themes_dir.as_deref().map(|d| theme::file::load_dir(d).0).unwrap_or_default();
        if !theme::file::select(&mut theme::Prefs::default(), name, &user) {
            let ids: Vec<&str> = theme::presets::PRESETS.iter().map(|p| p.id).collect();
            return Err(format!("unknown theme {name:?} (system, dark, light, {}, or a file in the themes folder)", ids.join(", ")));
        }
    }
    // Scripted runs close when done and leave the tray and notifications alone.
    let in_background = args.script.screenshot.is_none();
    let start_hidden = args.background;
    let mut script = Some(testing::Runner::new(args.script));
    let Outside { bus, events } = outside;
    // Outlives each window: where a window can't hide (Wayland), closing it closes it and byob
    // waits for a Show to open another.
    let background = std::rc::Rc::new(std::cell::RefCell::new(background::Background::new(bus, events, in_background, start_hidden)));
    let (theme_arg, open_settings, section, search) = (args.theme, args.open_settings, args.settings_section, args.settings_search);
    let backdrop_arg = args.backdrop;
    let (arrangement_arg, arranging, look_args) = (args.arrangement, args.arranging, args.look);
    let result = loop {
        // The script runs in the first window only.
        let script = script.take().unwrap_or_else(|| testing::Runner::new(testing::Script::default()));
        let window = eframe::run_native(
            "byob",
            options.clone(),
            Box::new(|cc| {
                let ctx = cc.egui_ctx.clone();
                let conn = engine.connect(Arc::new(move || ctx.request_repaint()));
                let mut ui = byob_ui_egui::ByobApp::new(Box::new(conn), "desktop");
                ui.set_desktop(Box::new(background::DesktopHost::new()));
                let told = cc.storage.and_then(|s| eframe::get_value(s, background::Told::STORAGE_KEY)).unwrap_or_default();
                // winit can't hide a window on Wayland.
                let can_hide = {
                    use eframe::egui_wgpu::wgpu::rwh::{HasWindowHandle, RawWindowHandle};
                    !matches!(cc.window_handle().map(|h| h.as_raw()), Ok(RawWindowHandle::Wayland(_)))
                };
                background.borrow_mut().attach(&cc.egui_ctx, can_hide, told);
                if cfg!(target_os = "macos") {
                    ui.set_titlebar(Some(byob_ui_egui::sidebar::Titlebar::MACOS));
                }
                ui.set_link_base(link_base(engine.clone(), serving));
                if let Some(dir) = themes_dir.clone() {
                    ui.set_themes_dir(dir);
                }
                // This device's saved appearance, else the theme from settings.json.
                let stored: Option<theme::Prefs> = cc.storage.and_then(|s| eframe::get_value(s, theme::Prefs::STORAGE_KEY));
                let mut prefs = stored.unwrap_or(theme::Prefs { theme: saved_theme, ..Default::default() });
                if let Some(name) = &theme_arg {
                    theme::file::select(&mut prefs, name, ui.user_themes());
                }
                if let Some(arg) = &backdrop_arg {
                    // Asked for by name: trusted, not on trial.
                    prefs.cockpit.backdrop.clone_from(arg);
                    prefs.cockpit.trial = false;
                }
                if let Some(name) = &arrangement_arg {
                    match byob_ui_egui::cockpit::arrange::Arrangement::preset(name) {
                        Some(a) => prefs.cockpit.arrangement = a,
                        None => prefs.cockpit.arrangement.free = false,
                    }
                }
                if !look_args.is_empty() {
                    let changes: byob_protocol::look::LookChanges =
                        look_args.iter().map(|(k, v)| (k.clone(), serde_json::from_str(v).unwrap_or_else(|_| serde_json::Value::String(v.clone())))).collect();
                    for problem in byob_ui_egui::look::apply(&mut prefs, &changes, true, ui.user_themes(), true) {
                        log::warn!("--look {problem}");
                    }
                    prefs.cockpit.trial = false;
                }
                ui.restore_prefs(prefs);
                if arranging {
                    ui.arrange_by_hand();
                }
                let mut compositor = None;
                if let Some(rs) = &cc.wgpu_render_state
                    && std::env::var_os("BYOB_NO_BACKDROP").is_none()
                {
                    ui.enable_backdrops(rs);
                    // With a backdrop, byob runs in a context of its own, composited onto the
                    // window (frosted glass).
                    if byob_ui_egui::cockpit::compositor::wanted(&ui.prefs().cockpit) {
                        log::info!("compositing the window (frosted glass)");
                        compositor = Some(byob_ui_egui::cockpit::compositor::Compositor::new(rs, &cc.egui_ctx));
                    }
                }
                // HTML apps inline, if the Servo helper is installed; else the summary + Open.
                // Their pictures belong to the context byob draws in.
                let byob_ctx = compositor.as_ref().map_or(&cc.egui_ctx, |c| c.context());
                if let Some(view) = apphost::ServoView::new(byob_ctx) {
                    ui.set_app_view(Box::new(view));
                }
                if let Some(pane) = cc.storage.and_then(|s| eframe::get_value::<byob_ui_egui::pane::PanePrefs>(s, byob_ui_egui::pane::PanePrefs::STORAGE_KEY)) {
                    ui.artifacts_mut().set_pane_prefs(pane);
                }
                if let Some(list) =
                    cc.storage.and_then(|s| eframe::get_value::<byob_ui_egui::sidebar::SidebarPrefs>(s, byob_ui_egui::sidebar::SidebarPrefs::STORAGE_KEY))
                {
                    ui.set_sidebar_prefs(&list);
                }
                ui.set_settings_open(open_settings);
                if let Some(section) = &section
                    && !ui.reveal_settings(section)
                {
                    log::warn!("--settings-section {section}: no such settings tab or setting");
                }
                if let Some(text) = &search {
                    ui.set_settings_search(text);
                }
                Ok(Box::new(App { ui, script, compositor, background: background.clone() }))
            }),
        );
        if let Err(e) = window {
            break Err(e.to_string());
        }
        // Closed to the background: wait until something asks for the window again.
        if !background.borrow_mut().wait_to_reopen() {
            break Ok(());
        }
    };
    if let Some(path) = scratch_state {
        let _ = std::fs::remove_file(path);
    }
    result
}

/// `--backdrop`: a WGSL file (checked now), or off (`""`).
fn backdrop_source(name: &str) -> Result<String, String> {
    if name.eq_ignore_ascii_case("off") {
        return Ok(String::new());
    }
    let source = std::fs::read_to_string(name).map_err(|e| format!("--backdrop {name}: a .wgsl file I can read, or off: {e}"))?;
    byob_ui_egui::cockpit::wgsl::check(&source).map_err(|e| format!("--backdrop {name}: {e}"))?;
    Ok(source)
}

/// Where the window's saved state (size, appearance, pane) goes: under `BYOB_HOME` when it
/// is set, so a separate profile doesn't share it with the user's own; else eframe's default
/// (the platform data folder).
fn window_state_in(byob_home: Option<std::ffi::OsString>) -> Option<PathBuf> {
    byob_home.filter(|h| !h.is_empty()).map(|h| PathBuf::from(h).join("window.ron"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: `--help` came back as an error, so it went to stderr and byob exited 1.
    #[test]
    fn help_is_an_answer_not_an_error() {
        let argv = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>().into_iter();
        assert!(parse_args_from(argv(&["--help"])).is_ok_and(|a| a.help));
        assert!(parse_args_from(argv(&["--dev-tools", "-h"])).is_ok_and(|a| a.help));
        assert!(parse_args_from(argv(&["--dev-tools"])).is_ok_and(|a| !a.help && a.dev_tools));
        assert!(parse_args_from(argv(&["--nope"])).is_err());
        for flag in ["--backdrop", "--arrangement", "--look", "--theme", "--yolo"] {
            assert!(USAGE.contains(flag), "{flag} is in the usage");
        }
    }

    #[test]
    fn a_byob_home_profile_keeps_its_own_window_state() {
        assert_eq!(window_state_in(Some("/tmp/demo/home".into())), Some(PathBuf::from("/tmp/demo/home/window.ron")));
        assert_eq!(window_state_in(Some("".into())), None);
        assert_eq!(window_state_in(None), None);
    }

    fn saved(url: Option<&str>, model: Option<&str>, key: Option<&str>) -> (Settings, Secrets) {
        let mut settings = Settings::default();
        settings.inference.base_url = url.map(Into::into);
        settings.inference.model = model.map(Into::into);
        let secrets = Secrets { api_key: key.map(Into::into), ..Default::default() };
        (settings, secrets)
    }

    #[test]
    fn model_roles_default_to_the_main_endpoint() {
        let main = InferenceConfig { base_url: "http://main/v1".into(), model: "big".into(), api_key: Some("sk-main".into()), api: None, ..Default::default() };
        let mut settings = Settings::default();
        settings.models.insert("fast".into(), byob_store::ModelRole { model: Some("small".into()), ..Default::default() });
        settings.models.insert("remote".into(), byob_store::ModelRole { base_url: Some("http://other/v1".into()), ..Default::default() });
        settings.models.insert("keyed".into(), byob_store::ModelRole { base_url: Some("http://third/v1".into()), ..Default::default() });
        let secrets = Secrets { model_keys: [("keyed".to_owned(), "sk-third".to_owned())].into(), ..Default::default() };
        let roles = model_roles(&main, &settings, &secrets);
        assert_eq!(roles["fast"], InferenceConfig { model: "small".into(), ..main.clone() });
        // Another endpoint never gets the main key; its own comes from secrets.json.
        assert_eq!(
            roles["remote"],
            InferenceConfig { base_url: "http://other/v1".into(), model: "big".into(), api_key: None, api: None, ..Default::default() }
        );
        assert_eq!(roles["keyed"].api_key.as_deref(), Some("sk-third"));
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
    fn the_saved_api_goes_with_its_url_and_to_roles_there() {
        use byob_types::ApiKind;
        let (mut settings, secrets) = saved(Some("http://proxy:1/v1"), Some("claude-opus-5-5"), None);
        settings.inference.api = Some(ApiKind::Anthropic);
        let c = inference_config(&Args::default(), &settings, &secrets);
        assert_eq!(c.api, Some(ApiKind::Anthropic));
        // Another URL on the command line decides from its host.
        let flags = Args { inference_url: Some("http://flag:2/v1".into()), ..Default::default() };
        assert_eq!(inference_config(&flags, &settings, &secrets).api, None);
        settings.models.insert("same".into(), byob_store::ModelRole { model: Some("claude-haiku-5-5".into()), ..Default::default() });
        settings.models.insert("elsewhere".into(), byob_store::ModelRole { base_url: Some("http://llama/v1".into()), ..Default::default() });
        let own = byob_store::ModelRole { base_url: Some("http://gw/v1".into()), api: Some(ApiKind::Anthropic), ..Default::default() };
        settings.models.insert("own".into(), own);
        let roles = model_roles(&c, &settings, &secrets);
        assert_eq!(roles["same"].api, Some(ApiKind::Anthropic));
        assert_eq!(roles["elsewhere"].api, None);
        assert_eq!(roles["own"].api, Some(ApiKind::Anthropic));
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
