//! Nix: the workspace's own dev shell, packages installed on request, and a `nix` CLI that
//! works inside the sandbox.
//!
//! What a sandboxed `nix` needs (verified on NixOS with Nix 2.34):
//! - read `/nix/store`, `/nix/var/nix` and `/etc/nix`, and connect to the daemon socket.
//!   The daemon downloads and builds outside the sandbox, so a run without network can
//!   still get packages from the binary cache — but the client sees no network, warns
//!   "you don't have Internet access" and turns substitution off; `substitute = true` in
//!   `NIX_CONFIG` turns it back on.
//! - the flake registry: the global one is downloaded by the *client* (fails offline), so
//!   byob points `flake-registry` at its own file pinning `nixpkgs` (seeded from
//!   `/etc/nix/registry.json`, else from the first install). Flake inputs that aren't in
//!   the store yet are fetched by the client too, so loading a dev shell runs with network.
//! - a writable cache: `NIX_CACHE_HOME` is `<data>/nix/cache`, shared by all runs, so
//!   evaluation and fetcher caches survive the per-run temp home.
//! - `nix-command flakes` enabled even if only the user's own `nix.conf` (unreadable from
//!   the sandbox) enables them.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Installed, Layer, Project, Runner, keep_var, write_atomic};
use crate::Job;

pub const DAEMON_SOCKET: &str = "/nix/var/nix/daemon-socket/socket";
/// What `nix` says when the sandbox has no network (left out of tool output: with
/// `substitute = true` the daemon still downloads).
pub const OFFLINE_WARNING: &str = "warning: you don't have Internet access; disabling some network-dependent features\n";
const LOAD_TIMEOUT: Duration = Duration::from_secs(900);
const EVAL_TIMEOUT: Duration = Duration::from_secs(300);
const HOOK_TIMEOUT: Duration = Duration::from_secs(300);
/// Largest dev environment JSON read back from the profile.
const MAX_ENV_JSON: u64 = 64 << 20;
/// Most packages per workspace.
const MAX_INSTALLED: usize = 100;

/// Nix on this machine.
#[derive(Debug, Clone)]
pub struct Nix {
    bin: PathBuf,
    /// The daemon's socket (multi-user install); `None` for a single-user install.
    socket: Option<PathBuf>,
    /// `<data>/nix`: shared cache, registry pin.
    state: Option<PathBuf>,
}

impl Nix {
    /// `nix` on `PATH` and a `/nix/store`. Prepares `<data>/nix` (cache folder, registry).
    pub fn detect(data_dir: Option<&Path>) -> Option<Self> {
        if !Path::new("/nix/store").is_dir() {
            return None;
        }
        let bin = byob_sandbox::which("nix".as_ref())?;
        let socket = Path::new(DAEMON_SOCKET).exists().then(|| PathBuf::from(DAEMON_SOCKET));
        let state = data_dir.map(|d| d.join("nix")).and_then(|s| match std::fs::create_dir_all(s.join("cache")) {
            Ok(()) => s.canonicalize().ok(),
            Err(e) => {
                log::warn!("nix: can't create {}: {e}", s.display());
                None
            }
        });
        let nix = Self { bin, socket, state };
        nix.seed_registry();
        Some(nix)
    }

    pub fn binary(&self) -> &Path {
        &self.bin
    }

    pub fn daemon(&self) -> bool {
        self.socket.is_some()
    }

    fn registry_file(&self) -> Option<PathBuf> {
        self.state.as_ref().map(|s| s.join("registry.json"))
    }

    fn pin_file(&self) -> Option<PathBuf> {
        self.state.as_ref().map(|s| s.join("nixpkgs.json"))
    }

    /// Start byob's registry from the system's `nixpkgs` pin (NixOS sets one), so `nixpkgs#…`
    /// works offline from the first run.
    fn seed_registry(&self) {
        let Some(file) = self.registry_file() else { return };
        if file.exists() {
            return;
        }
        let Ok(text) = std::fs::read_to_string("/etc/nix/registry.json") else { return };
        if let Some(entry) = nixpkgs_entry(&text)
            && let Err(e) = write_atomic(&file, registry_json(entry).as_bytes())
        {
            log::warn!("nix: {e}");
        }
    }

    /// `NIX_CONFIG` for every run.
    pub fn config(&self) -> String {
        let mut c = String::from("extra-experimental-features = nix-command flakes\nsubstitute = true\nwarn-dirty = false\n");
        if let Some(r) = self.registry_file().filter(|r| r.is_file()) {
            c.push_str(&format!("flake-registry = {}\n", r.display()));
        }
        c
    }

    /// What every sandboxed run gets so `nix` works inside it.
    pub fn support(&self) -> Layer {
        let mut l = Layer { read: vec!["/nix/var/nix".into(), "/etc/nix".into()], ..Default::default() };
        if let Some(dir) = self.bin.canonicalize().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
            l.read.push(dir);
        }
        match &self.socket {
            Some(s) => l.sockets.push(s.clone()),
            // Single-user: the client writes the store itself.
            None => l.write.extend([PathBuf::from("/nix/store"), PathBuf::from("/nix/var/nix")]),
        }
        l.vars.push(("NIX_CONFIG".into(), self.config()));
        if let Some(s) = &self.state {
            let cache = s.join("cache");
            l.vars.push(("NIX_CACHE_HOME".into(), cache.to_string_lossy().into_owned()));
            l.write.push(cache);
            if let Some(r) = self.registry_file() {
                l.read.push(r);
            }
        }
        // Legacy `nix-shell -p` and `<nixpkgs>` (NixOS sets NIX_PATH=nixpkgs=flake:nixpkgs).
        for k in ["NIX_PATH", "NIX_REMOTE"] {
            if let Some(v) = std::env::var_os(k) {
                l.vars.push((k.into(), v.to_string_lossy().into_owned()));
            }
        }
        l
    }

    // ---- packages installed with install_packages --------------------------------------

    /// The installed packages of the workspace whose state folder is `dir`.
    pub fn installed(&self, dir: &Path) -> Option<Layer> {
        let target = dir.join("tools").canonicalize().ok()?;
        let bin = target.join("bin");
        bin.is_dir().then(|| Layer { path: vec![bin], ..Default::default() })
    }

    /// Install `packages` (nixpkgs attribute names, or Python libraries) for the workspace
    /// whose state folder is `dir`: pin nixpkgs, check each name, build one environment.
    pub(crate) fn install(&self, dir: &Path, packages: &[String], run: &dyn Runner) -> Result<Installed, String> {
        let requests = packages.iter().map(|p| Request::parse(p)).collect::<Result<Vec<_>, _>>()?;
        std::fs::create_dir_all(dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
        let dir = dir.canonicalize().map_err(|e| format!("{}: {e}", dir.display()))?;
        let state_file = dir.join("packages.json");
        let mut state: Tools = std::fs::read_to_string(&state_file).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        if state.nixpkgs.is_empty() {
            state.nixpkgs = self.pin(&dir, run)?;
        }
        let found = self.classify(&dir, &state.nixpkgs, &requests, run)?;
        let mut added = Vec::new();
        let mut missing = Vec::new();
        let mut not_found = Vec::new();
        for (req, f) in requests.iter().zip(&found) {
            match f {
                Found::Package { attr, name } => {
                    if !state.packages.contains(attr) {
                        state.packages.push(attr.clone());
                    }
                    added.push(name.clone());
                }
                Found::Python { attr, name } => {
                    if !state.python.contains(attr) {
                        state.python.push(attr.clone());
                    }
                    added.push(format!("{name} (Python library)"));
                }
                Found::Missing { suggestions } => {
                    missing.push(missing_text(&req.query, suggestions));
                    not_found.push(req.query.clone());
                }
            }
        }
        if state.packages.len() + state.python.len() > MAX_INSTALLED {
            return Err(format!("a workspace can have at most {MAX_INSTALLED} installed packages"));
        }
        let mut text = String::new();
        if !added.is_empty() {
            self.build(&dir, &state, run)?;
            write_atomic(&state_file, serde_json::to_string_pretty(&state).unwrap_or_default().as_bytes())?;
            text.push_str(&format!(
                "Installed with Nix for this workspace: {}.\nrun_command and run_python can use them now (they are on PATH; Python libraries are in run_python's python3).",
                added.join(", ")
            ));
        }
        if !missing.is_empty() {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&format!(
                "Not found in nixpkgs: {}.\nCall install_packages again with the exact name (search: https://search.nixos.org/packages).",
                missing.join("; ")
            ));
        }
        Ok(Installed { text, failed: !missing.is_empty(), missing: not_found })
    }

    /// The locked `nixpkgs` every install uses: byob's pin, else the registry's `nixpkgs`
    /// resolved now (and pinned, also in byob's registry).
    fn pin(&self, dir: &Path, run: &dyn Runner) -> Result<String, String> {
        let Some(pin_file) = self.pin_file() else { return Err("byob has no data folder for Nix".into()) };
        if let Some(url) =
            std::fs::read_to_string(&pin_file).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()).and_then(|v| v["url"].as_str().map(str::to_owned))
        {
            return Ok(url);
        }
        let mut job = self.job(vec!["flake".into(), "metadata".into(), "nixpkgs".into(), "--json".into()], EVAL_TIMEOUT);
        job.network = true;
        let out = in_tmp(run, job, dir)?;
        if !out.success() {
            return Err(format!("couldn't resolve nixpkgs: {}", nix_error(&out.stderr.text())));
        }
        let meta: Value = serde_json::from_slice(&out.stdout.bytes()).map_err(|e| format!("bad `nix flake metadata` output: {e}"))?;
        let url = meta["url"].as_str().filter(|u| !u.is_empty()).ok_or("`nix flake metadata nixpkgs` gave no locked URL")?.to_owned();
        write_atomic(&pin_file, json!({"url": url}).to_string().as_bytes())?;
        if let Some(reg) = self.registry_file().filter(|r| !r.exists())
            && let Some(locked) = meta.get("locked").and_then(Value::as_object)
        {
            let mut to = locked.clone();
            to.remove("__final");
            write_atomic(&reg, registry_json(json!({"exact": true, "from": {"id": "nixpkgs", "type": "indirect"}, "to": to})).as_bytes())?;
        }
        Ok(url)
    }

    fn classify(&self, dir: &Path, nixpkgs: &str, requests: &[Request], run: &dyn Runner) -> Result<Vec<Found>, String> {
        let mut job = self.job(vec!["eval".into(), "--impure".into(), "--json".into(), "--expr".into(), CLASSIFY.into()], EVAL_TIMEOUT);
        job.network = true;
        job.env.push(("BYOB_NIXPKGS".into(), nixpkgs.into()));
        job.env.push(("BYOB_REQ".into(), serde_json::to_string(requests).unwrap_or_default().into()));
        let out = in_tmp(run, job, dir)?;
        if !out.success() {
            return Err(format!("checking the package names failed: {}", nix_error(&out.stderr.text())));
        }
        let found: Vec<Found> = serde_json::from_slice(&out.stdout.bytes()).map_err(|e| format!("bad `nix eval` output: {e}"))?;
        if found.len() != requests.len() {
            return Err("bad `nix eval` output: wrong length".into());
        }
        Ok(found)
    }

    fn build(&self, dir: &Path, state: &Tools, run: &dyn Runner) -> Result<(), String> {
        let link = dir.join("tools");
        let args = vec!["build".into(), "--impure".into(), "--out-link".into(), link.into_os_string(), "--expr".into(), BUILD.into()];
        let mut job = self.job(args, LOAD_TIMEOUT);
        job.network = true;
        job.layer.write.push(dir.to_path_buf());
        job.env.push(("BYOB_NIXPKGS".into(), state.nixpkgs.clone().into()));
        job.env.push(("BYOB_REQ".into(), json!({"packages": state.packages, "python": state.python}).to_string().into()));
        let out = in_tmp(run, job, dir)?;
        if out.cancelled {
            return Err("Stopped by the user.".into());
        }
        if !out.success() {
            return Err(format!("building the packages failed: {}", nix_error(&out.stderr.text())));
        }
        Ok(())
    }

    fn job(&self, args: Vec<std::ffi::OsString>, timeout: Duration) -> Job {
        Job::new(self.bin.clone().into_os_string(), args, timeout)
    }

    /// Load the dev shell of `ws`'s `flake.nix` / `shell.nix` with `nix print-dev-env`
    /// (GC-rooted by a profile in `dir`), then run its `shellHook` once.
    fn load_shell(&self, ws: &Path, dir: &Path, kind: ShellKind, run: &dyn Runner) -> Result<Layer, String> {
        let profile = dir.join("devshell-profile");
        let mut args: Vec<std::ffi::OsString> = vec!["print-dev-env".into(), "--profile".into(), profile.clone().into_os_string(), "--json".into()];
        match kind {
            ShellKind::Flake => args.push(".".into()),
            ShellKind::ShellNix => args.extend(["--impure".into(), "--file".into(), "shell.nix".into()]),
        }
        let mut job = self.job(args, LOAD_TIMEOUT);
        // Flake inputs not yet in the store are fetched by the client.
        job.network = true;
        job.layer.write.extend([ws.to_path_buf(), dir.to_path_buf()]);
        job.layer.read.extend(git_dirs(ws));
        let tmp = byob_sandbox::TempDir::new("byob-nix-")?;
        let out = run.run(job, ws, tmp.path())?;
        if out.cancelled {
            return Err("Stopped by the user.".into());
        }
        if !out.success() {
            let err = nix_error(&out.stderr.text());
            let hint = if err.contains("is not tracked by Git") { "\n(Nix only sees files git tracks: `git add flake.nix` and try again.)" } else { "" };
            return Err(format!("`nix print-dev-env` failed: {err}{hint}"));
        }
        let target = profile.canonicalize().map_err(|e| format!("no dev shell profile: {e}"))?;
        let size = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
        if size > MAX_ENV_JSON {
            return Err(format!("the dev shell environment is too large ({size} bytes)"));
        }
        let text = std::fs::read_to_string(&target).map_err(|e| format!("can't read {}: {e}", target.display()))?;
        let env = DevEnv::parse(&text)?;
        match env.shell_hook.as_deref().filter(|h| !h.trim().is_empty()) {
            None => Ok(env.layer),
            Some(hook) => {
                let bash = env.bash.as_deref().map(PathBuf::from).filter(|b| b.is_file()).or_else(|| byob_sandbox::which("bash".as_ref()));
                let Some(bash) = bash else { return Ok(env.layer) };
                let mut layer = run_hook(&bash, hook, &env.layer, ws, run).map_err(|e| format!("its shellHook failed: {e}"))?;
                layer.stack(&env.layer);
                Ok(layer)
            }
        }
    }
}

/// The workspace's `flake.nix` / `shell.nix` dev shell as a `Project`.
#[derive(Debug)]
pub struct NixShell(pub Nix);

impl Project for NixShell {
    fn id(&self) -> &str {
        "devshell"
    }

    fn label(&self) -> &str {
        "Nix dev shell"
    }

    fn file(&self, ws: &Path) -> Option<String> {
        ShellKind::find(ws).map(|k| k.file().to_owned())
    }

    fn inputs(&self) -> Vec<String> {
        ["flake.nix", "flake.lock", "shell.nix", "default.nix"].map(str::to_owned).to_vec()
    }

    fn load(&self, ws: &Path, dir: &Path, run: &dyn Runner) -> Result<Layer, String> {
        let kind = ShellKind::find(ws).ok_or("no flake.nix or shell.nix")?;
        self.0.load_shell(ws, dir, kind, run)
    }
}

/// Run the dev shell's `shellHook` once in its environment and keep what it changed.
fn run_hook(bash: &Path, hook: &str, env: &Layer, ws: &Path, run: &dyn Runner) -> Result<Layer, String> {
    let tmp = byob_sandbox::TempDir::new("byob-hook-")?;
    let dump = tmp.path().join("env.dump");
    let script = r#"eval "$shellHook" >&2 </dev/null; env -0 > "$1""#;
    let mut job = Job::new(
        bash.as_os_str().to_owned(),
        vec!["--noprofile".into(), "--norc".into(), "-c".into(), script.into(), "bash".into(), dump.clone().into_os_string()],
        HOOK_TIMEOUT,
    );
    job.network = true;
    job.layer = env.clone();
    job.layer.write.push(ws.to_path_buf());
    job.env.push(("shellHook".into(), hook.into()));
    let out = run.run(job, ws, tmp.path())?;
    let bytes = std::fs::read(&dump).map_err(|_| format!("exit code {:?}: {}", out.exit_code, nix_error(&out.stderr.text())))?;
    let mut before = crate::clean_env_vars(tmp.path());
    before.extend(env.vars.iter().cloned());
    let host = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).unwrap_or_default();
    Ok(hook_changes(&bytes, &before, &env.path, &host))
}

/// Variables the hook set or changed (an `env -0` dump against what it started with), and
/// `PATH` entries it added in front.
pub fn hook_changes(dump: &[u8], before: &[(String, String)], had_path: &[PathBuf], host_path: &[PathBuf]) -> Layer {
    let mut l = Layer::default();
    for entry in dump.split(|b| *b == 0) {
        let entry = String::from_utf8_lossy(entry);
        let Some((k, v)) = entry.split_once('=') else { continue };
        if k == "PATH" {
            // The hook ran in Nix's bash: a Unix PATH whatever the host's rules.
            for p in v.split(':').filter(|p| p.starts_with('/')).map(PathBuf::from) {
                if !host_path.contains(&p) && !had_path.contains(&p) && !l.path.contains(&p) {
                    l.path.push(p);
                }
            }
            continue;
        }
        if k.is_empty() || keep_var(k) || before.iter().any(|(bk, bv)| bk == k && bv == v) {
            continue;
        }
        l.vars.push((k.to_owned(), v.to_owned()));
    }
    l
}

/// A dev environment as `nix print-dev-env --json` describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevEnv {
    pub layer: Layer,
    pub shell_hook: Option<String>,
    /// The bash the environment was made with (runs the hook).
    pub bash: Option<String>,
}

impl DevEnv {
    /// Exported variables become the layer (`PATH` split into entries), except those the
    /// sandbox or `nix develop` keep from outside ([`keep_var`]).
    pub fn parse(text: &str) -> Result<Self, String> {
        let v: Value = serde_json::from_str(text).map_err(|e| format!("bad dev shell JSON: {e}"))?;
        let vars = v.get("variables").and_then(Value::as_object).ok_or("dev shell JSON has no variables")?;
        let mut layer = Layer::default();
        let str_of = |name: &str| vars.get(name).and_then(|e| e.get("value")).and_then(Value::as_str).map(str::to_owned);
        for (k, e) in vars {
            if e.get("type").and_then(Value::as_str) != Some("exported") {
                continue;
            }
            let Some(val) = e.get("value").and_then(Value::as_str) else { continue };
            if k == "PATH" {
                layer.path = val.split(':').filter(|p| p.starts_with('/')).map(PathBuf::from).collect();
            } else if !keep_var(k) && !k.contains('=') && !k.contains('\0') && !val.contains('\0') {
                layer.vars.push((k.clone(), val.to_owned()));
            }
        }
        Ok(Self { layer, shell_hook: str_of("shellHook"), bash: str_of("BASH") })
    }
}

/// Which project file defines the dev shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    Flake,
    ShellNix,
}

impl ShellKind {
    pub fn find(ws: &Path) -> Option<Self> {
        if ws.join("flake.nix").is_file() {
            Some(Self::Flake)
        } else if ws.join("shell.nix").is_file() {
            Some(Self::ShellNix)
        } else {
            None
        }
    }

    pub fn file(self) -> &'static str {
        match self {
            Self::Flake => "flake.nix",
            Self::ShellNix => "shell.nix",
        }
    }
}

/// What a workspace has installed (`packages.json`).
#[derive(Debug, Default, Serialize, Deserialize)]
struct Tools {
    /// Locked nixpkgs URL.
    #[serde(default)]
    nixpkgs: String,
    /// Attribute paths, e.g. `jq`, `nodePackages.prettier`.
    #[serde(default)]
    packages: Vec<String>,
    /// `python3Packages` attributes, e.g. `matplotlib`.
    #[serde(default)]
    python: Vec<String>,
}

/// One name the model asked for, as the classify expression wants it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Request {
    pub query: String,
    /// Top-level attribute path to try first.
    pub top: Option<Vec<String>>,
    /// `python3Packages` attribute to try (after `top`).
    pub py: Option<String>,
}

impl Request {
    /// Accepts `jq`, `nixpkgs#jq`, `pkgs.jq`, `nodePackages.prettier`, `python3Packages.numpy`,
    /// `python:numpy`, and pip-style `numpy>=2` (the version is dropped: nixpkgs has one).
    /// Common command names map to their package (`convert` → `imagemagick`).
    pub fn parse(raw: &str) -> Result<Self, String> {
        let bad = || format!("{raw:?} is not a package name (like \"jq\", \"ffmpeg\" or \"matplotlib\")");
        let s = raw.trim();
        if s.is_empty() || s.len() > 100 {
            return Err(bad());
        }
        let s = s.strip_prefix("nixpkgs#").or_else(|| s.strip_prefix("nixpkgs.")).or_else(|| s.strip_prefix("pkgs.")).unwrap_or(s);
        let explicit_py = ["python:", "py:", "pip:", "pypi:"].iter().find_map(|p| s.strip_prefix(p)).or_else(|| {
            let (set, name) = s.split_once('.')?;
            let digits = set.strip_prefix("python")?.strip_suffix("Packages")?;
            digits.chars().all(|c| c.is_ascii_digit()).then_some(name)
        });
        if let Some(name) = explicit_py {
            let py = python_attr(name).ok_or_else(bad)?;
            return Ok(Self { query: s.to_owned(), top: None, py: Some(py) });
        }
        // A version constraint means a pip-style Python requirement.
        if s.contains(['<', '>', '=', '!', '~', '[']) {
            let py = python_attr(s).ok_or_else(bad)?;
            return Ok(Self { query: s.to_owned(), top: None, py: Some(py) });
        }
        let segments: Vec<&str> = s.split('.').collect();
        if segments.len() > 8 || !segments.iter().all(|seg| valid_attr(seg)) {
            return Err(bad());
        }
        if let [one] = segments.as_slice() {
            let top = command_package(one).unwrap_or(one);
            return Ok(Self { query: s.to_owned(), top: Some(top.split('.').map(str::to_owned).collect()), py: python_attr(one) });
        }
        Ok(Self { query: s.to_owned(), top: Some(segments.iter().map(|x| (*x).to_owned()).collect()), py: None })
    }
}

/// A Nix attribute name byob passes on (no quoting needed, nothing to inject).
fn valid_attr(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.len() <= 100
        && chars.all(|c| c.is_ascii_alphanumeric() || "_-'+".contains(c))
        && !s.contains("''")
}

/// The `python3Packages` attribute for a pip name or import name: lowercase, `_`/`.` → `-`,
/// version and extras dropped, well-known import names mapped (`sklearn` → `scikit-learn`).
pub fn python_attr(name: &str) -> Option<String> {
    let base = name.split(['<', '>', '=', '!', '~', '[', ';', ' ']).next()?.trim();
    let lower = base.to_ascii_lowercase().replace(['_', '.'], "-");
    let mapped = PYTHON_IMPORTS.iter().find(|(import, _)| import.eq_ignore_ascii_case(base)).map_or(lower.as_str(), |(_, pkg)| pkg).to_owned();
    valid_attr(&mapped).then_some(mapped)
}

/// Import names whose nixpkgs package is called something else.
pub const PYTHON_IMPORTS: &[(&str, &str)] = &[
    ("sklearn", "scikit-learn"),
    ("skimage", "scikit-image"),
    ("cv2", "opencv4"),
    ("opencv-python", "opencv4"),
    ("PIL", "pillow"),
    ("yaml", "pyyaml"),
    ("bs4", "beautifulsoup4"),
    ("dateutil", "python-dateutil"),
    ("dotenv", "python-dotenv"),
    ("docx", "python-docx"),
    ("pptx", "python-pptx"),
    ("magic", "python-magic"),
    ("serial", "pyserial"),
    ("usb", "pyusb"),
    ("Crypto", "pycryptodome"),
    ("jwt", "pyjwt"),
    ("git", "gitpython"),
    ("fitz", "pymupdf"),
    ("attr", "attrs"),
    ("google.protobuf", "protobuf"),
    ("mpl_toolkits", "matplotlib"),
];

/// Commands whose nixpkgs package has another name.
pub const COMMANDS: &[(&str, &str)] = &[
    ("convert", "imagemagick"),
    ("magick", "imagemagick"),
    ("identify", "imagemagick"),
    ("mogrify", "imagemagick"),
    ("rg", "ripgrep"),
    ("node", "nodejs"),
    ("npm", "nodejs"),
    ("npx", "nodejs"),
    ("ffprobe", "ffmpeg"),
    ("make", "gnumake"),
    ("cc", "gcc"),
    ("g++", "gcc"),
    ("pdftotext", "poppler-utils"),
    ("pdftoppm", "poppler-utils"),
    ("pdfinfo", "poppler-utils"),
    ("dot", "graphviz"),
    ("7z", "p7zip"),
    ("sqlite3", "sqlite"),
    ("psql", "postgresql"),
    ("java", "jdk"),
    ("javac", "jdk"),
    ("xmllint", "libxml2"),
    ("dig", "dnsutils"),
    ("nc", "netcat"),
    ("pip", "python3Packages.pip"),
    ("pip3", "python3Packages.pip"),
    ("python", "python3"),
    ("lualatex", "texliveMedium"),
    ("pdflatex", "texliveMedium"),
    ("latex", "texliveMedium"),
    ("xelatex", "texliveMedium"),
    ("rsvg-convert", "librsvg"),
    ("inkscape", "inkscape"),
    ("tesseract", "tesseract"),
    ("yq", "yq-go"),
    ("http", "httpie"),
    ("unzip", "unzip"),
    ("zip", "zip"),
];

/// The package for a command, if it differs from the command's name.
pub fn command_package(cmd: &str) -> Option<&'static str> {
    COMMANDS.iter().find(|(c, p)| *c == cmd && *p != cmd).map(|(_, p)| *p)
}

/// What the classify expression says about one request.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Found {
    Package {
        attr: String,
        name: String,
    },
    Python {
        attr: String,
        name: String,
    },
    Missing {
        #[serde(default)]
        suggestions: Vec<String>,
    },
}

fn missing_text(query: &str, suggestions: &[String]) -> String {
    let s: Vec<&str> = suggestions.iter().map(String::as_str).filter(|s| s.len() <= 100).take(8).collect();
    if s.is_empty() { format!("\"{query}\"") } else { format!("\"{query}\" (similar: {})", s.join(", ")) }
}

/// For each request: a top-level package, else a Python library, else similar names.
/// Inputs come from the environment (`BYOB_NIXPKGS`, `BYOB_REQ`), never spliced in.
const CLASSIFY: &str = r#"
let
  pkgs = (builtins.getFlake (builtins.getEnv "BYOB_NIXPKGS")).legacyPackages.${builtins.currentSystem};
  lib = pkgs.lib;
  req = builtins.fromJSON (builtins.getEnv "BYOB_REQ");
  nameAt = set: path:
    let r = builtins.tryEval (let v = lib.attrByPath path null set; in if v != null && lib.isDerivation v then v.name else null);
    in if r.success then r.value else null;
  similar = q: set: prefix:
    let lq = lib.toLower q;
    in map (n: prefix + n) (lib.take 5 (builtins.filter (n: lib.hasInfix lq (lib.toLower n)) (builtins.attrNames set)));
  one = r:
    let
      top = if r.top == null then null else nameAt pkgs r.top;
      py = if r.py == null then null else nameAt pkgs.python3Packages [ r.py ];
    in
      if top != null then { kind = "package"; attr = lib.concatStringsSep "." r.top; name = top; }
      else if py != null then { kind = "python"; attr = r.py; name = py; }
      else { kind = "missing"; suggestions = similar (if r.py != null then r.py else lib.last r.top) pkgs "" ++ (if r.py != null then similar r.py pkgs.python3Packages "python3Packages." else [ ]); };
in map one req
"#;

/// One environment with every installed package; Python libraries in one `python3`.
const BUILD: &str = r#"
let
  pkgs = (builtins.getFlake (builtins.getEnv "BYOB_NIXPKGS")).legacyPackages.${builtins.currentSystem};
  lib = pkgs.lib;
  req = builtins.fromJSON (builtins.getEnv "BYOB_REQ");
  python = lib.optional (req.python != [ ]) (pkgs.python3.withPackages (ps: map (n: ps.${n}) req.python));
in pkgs.buildEnv {
  name = "byob-workspace-tools";
  paths = python ++ map (a: lib.getAttrFromPath (lib.splitString "." a) pkgs) req.packages;
  ignoreCollisions = true;
}
"#;

/// The `nixpkgs` entry of a registry file.
pub fn nixpkgs_entry(registry: &str) -> Option<Value> {
    let v: Value = serde_json::from_str(registry).ok()?;
    v.get("flakes")?.as_array()?.iter().find(|f| f["from"]["id"].as_str() == Some("nixpkgs") && f["from"]["type"].as_str() == Some("indirect")).cloned()
}

fn registry_json(entry: Value) -> String {
    json!({"version": 2, "flakes": [entry]}).to_string()
}

/// Nix's stderr from the first `error:` on (skipping warnings about unreachable caches),
/// bounded.
pub fn nix_error(stderr: &str) -> String {
    let from = stderr.find("error:").map_or(stderr, |i| stderr.get(i..).unwrap_or(stderr));
    let t = from.trim();
    let max = 3000;
    if t.len() <= max {
        return t.to_owned();
    }
    let mut cut = max;
    while !t.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", t.get(..cut).unwrap_or_default())
}

/// A git worktree's `.git` is a file pointing elsewhere; Nix must read that folder (and the
/// main repository's) to see the flake.
fn git_dirs(ws: &Path) -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string(ws.join(".git")) else { return Vec::new() };
    let Some(dir) = text.trim().strip_prefix("gitdir:").map(|d| ws.join(d.trim())) else { return Vec::new() };
    let mut out = vec![dir.clone()];
    if let Ok(common) = std::fs::read_to_string(dir.join("commondir")) {
        out.push(dir.join(common.trim()));
    }
    out.into_iter().filter_map(|p| p.canonicalize().ok()).collect()
}

/// Run `job` in `cwd` with a fresh temp folder.
fn in_tmp(run: &dyn Runner, job: Job, cwd: &Path) -> Result<byob_sandbox::Outcome, String> {
    let tmp = byob_sandbox::TempDir::new("byob-nix-")?;
    run.run(job, cwd, tmp.path())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests() {
        let r = Request::parse("jq").unwrap();
        assert_eq!((r.top, r.py), (Some(vec!["jq".into()]), Some("jq".into())));
        let r = Request::parse("nixpkgs#ripgrep").unwrap();
        assert_eq!(r.top, Some(vec!["ripgrep".into()]));
        assert_eq!(Request::parse("convert").unwrap().top, Some(vec!["imagemagick".into()]));
        assert_eq!(Request::parse("pip").unwrap().top, Some(vec!["python3Packages".into(), "pip".into()]));
        let r = Request::parse("python3Packages.Scikit_Learn").unwrap();
        assert_eq!((r.top, r.py), (None, Some("scikit-learn".into())));
        assert_eq!(Request::parse("python312Packages.numpy").unwrap().py, Some("numpy".into()));
        assert_eq!(Request::parse("python:sklearn").unwrap().py, Some("scikit-learn".into()));
        let r = Request::parse("numpy>=2").unwrap();
        assert_eq!((r.top, r.py), (None, Some("numpy".into())));
        assert_eq!(Request::parse("xarray[io]").unwrap().py, Some("xarray".into()));
        let r = Request::parse("nodePackages.prettier").unwrap();
        assert_eq!((r.top, r.py), (Some(vec!["nodePackages".into(), "prettier".into()]), None));
        for bad in ["", "a b", "\"x\"", "${x}", "a;b", "../x", "x\\y", "a''b", "-rf", &"a".repeat(101), "a.b.c.d.e.f.g.h.i"] {
            assert!(Request::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn python_names() {
        assert_eq!(python_attr("PIL").as_deref(), Some("pillow"));
        assert_eq!(python_attr("typing_extensions").as_deref(), Some("typing-extensions"));
        assert_eq!(python_attr("ruamel.yaml").as_deref(), Some("ruamel-yaml"));
        assert_eq!(python_attr("pandas==2.0; python_version>'3'").as_deref(), Some("pandas"));
        assert_eq!(python_attr("$(x)"), None);
    }

    #[test]
    fn dev_env_json() {
        let text = json!({"variables": {
            "PATH": {"type": "exported", "value": "/nix/store/a-jq/bin:/nix/store/b-cowsay/bin"},
            "HELLO": {"type": "exported", "value": "world"},
            "HOME": {"type": "exported", "value": "/homeless-shelter"},
            "TMPDIR": {"type": "exported", "value": "/build"},
            "BASH": {"type": "var", "value": "/nix/store/c-bash/bin/bash"},
            "shellHook": {"type": "var", "value": "export X=1"},
            "buildInputs": {"type": "array", "value": ["a"]},
            "notExported": {"type": "var", "value": "x"}
        }})
        .to_string();
        let e = DevEnv::parse(&text).unwrap();
        assert_eq!(e.layer.path, vec![PathBuf::from("/nix/store/a-jq/bin"), PathBuf::from("/nix/store/b-cowsay/bin")]);
        assert_eq!(e.layer.vars, vec![("HELLO".to_owned(), "world".to_owned())]);
        assert_eq!(e.shell_hook.as_deref(), Some("export X=1"));
        assert_eq!(e.bash.as_deref(), Some("/nix/store/c-bash/bin/bash"));
        assert!(DevEnv::parse("[]").is_err() && DevEnv::parse("nope").is_err());
    }

    #[test]
    fn hook_changes_keep_only_what_changed() {
        let dump = b"A=1\0B=new\0HOME=/tmp/x\0PATH=/ws/.venv/bin:/nix/store/a/bin:/usr/bin\0C=same\0broken\0";
        let before = vec![("B".to_owned(), "old".to_owned()), ("C".to_owned(), "same".to_owned())];
        let l = hook_changes(dump, &before, &[PathBuf::from("/nix/store/a/bin")], &[PathBuf::from("/usr/bin")]);
        assert_eq!(l.path, vec![PathBuf::from("/ws/.venv/bin")]);
        assert_eq!(l.vars, vec![("A".to_owned(), "1".to_owned()), ("B".to_owned(), "new".to_owned())]);
    }

    #[test]
    fn registry_and_errors() {
        let reg = r#"{"flakes":[{"exact":true,"from":{"id":"other","type":"indirect"},"to":{"type":"path","path":"/x"}},{"exact":true,"from":{"id":"nixpkgs","type":"indirect"},"to":{"path":"/nix/store/q-source","type":"path"}}],"version":2}"#;
        let e = nixpkgs_entry(reg).unwrap();
        assert_eq!(e["to"]["path"], "/nix/store/q-source");
        assert!(nixpkgs_entry("{}").is_none() && nixpkgs_entry("x").is_none());
        let stderr = "warning: unable to download 'https://x/nix-cache-info': HTTP error 401\nerror: flake 'path:/w' does not provide attribute 'devShells'\n";
        assert_eq!(nix_error(stderr), "error: flake 'path:/w' does not provide attribute 'devShells'");
        assert!(nix_error(&"é".repeat(5000)).chars().count() < 3002);
        let found: Vec<Found> = serde_json::from_str(r#"[{"kind":"package","attr":"jq","name":"jq-1.8"},{"kind":"python","attr":"numpy","name":"python3-numpy"},{"kind":"missing","suggestions":["a"]}]"#).unwrap();
        assert_eq!(found[2], Found::Missing { suggestions: vec!["a".into()] });
        assert_eq!(missing_text("imagemagic", &["imagemagick".into()]), "\"imagemagic\" (similar: imagemagick)");
    }

    #[test]
    fn project_files() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(ShellKind::find(d.path()), None);
        std::fs::write(d.path().join("shell.nix"), "{}").unwrap();
        assert_eq!(ShellKind::find(d.path()), Some(ShellKind::ShellNix));
        std::fs::write(d.path().join("flake.nix"), "{}").unwrap();
        assert_eq!(ShellKind::find(d.path()), Some(ShellKind::Flake));
    }

    #[test]
    fn worktree_git_dirs_are_readable() {
        let d = tempfile::tempdir().unwrap();
        let main = d.path().join("repo/.git");
        std::fs::create_dir_all(main.join("worktrees/wt")).unwrap();
        std::fs::write(main.join("worktrees/wt/commondir"), "../..\n").unwrap();
        let ws = d.path().join("wt");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join(".git"), format!("gitdir: {}\n", main.join("worktrees/wt").display())).unwrap();
        let dirs = git_dirs(&ws);
        assert!(dirs.contains(&main.canonicalize().unwrap()), "{dirs:?}");
        assert!(git_dirs(d.path()).is_empty());
    }

    #[test]
    fn config_enables_flakes_and_substitution() {
        let d = tempfile::tempdir().unwrap();
        let n = Nix { bin: "/bin/nix".into(), socket: Some(DAEMON_SOCKET.into()), state: Some(d.path().to_path_buf()) };
        assert!(n.config().contains("nix-command flakes") && n.config().contains("substitute = true"));
        assert!(!n.config().contains("flake-registry"));
        std::fs::write(d.path().join("registry.json"), "{}").unwrap();
        assert!(n.config().contains(&format!("flake-registry = {}", d.path().join("registry.json").display())));
        let s = n.support();
        assert_eq!(s.sockets, vec![PathBuf::from(DAEMON_SOCKET)]);
        assert!(s.write.contains(&d.path().join("cache")) && s.var("NIX_CACHE_HOME").is_some());
        let single = Nix { socket: None, ..n };
        assert!(single.support().write.contains(&PathBuf::from("/nix/store")));
    }
}
