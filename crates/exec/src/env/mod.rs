//! Workspace environments: where the programs and libraries a run needs come from.
//!
//! Models fail most often not on the task but on a missing tool (`jq: not found`,
//! `No module named 'matplotlib'`, a `pip install` that can't work on NixOS). So every
//! `run_command` / `run_python` call is prepared by [`Envs`], which stacks **layers** onto
//! the clean sandbox environment, first match winning:
//!
//! 1. **Installed**: packages the model asked for with `install_packages`, kept per
//!    workspace under `<data>/envs/` (Nix: one `buildEnv`, a GC root, nixpkgs pinned).
//! 2. **Project venv**: a `.venv` / `venv` in the workspace (any OS).
//! 3. **Project environments** (`Project`): what the workspace declares itself, loaded
//!    once (sandboxed, with network) and cached until its files change — a `flake.nix` /
//!    `shell.nix` dev shell (`nix print-dev-env`, shellHook included), a pixi project
//!    (`pixi shell-hook --json`), mise tools (`mise env --json`).
//! 4. **Support**: what a package manager's own CLI needs inside the sandbox, so the model
//!    can also run `nix shell nixpkgs#foo --command …` itself (store, daemon socket,
//!    config, a persistent eval cache).
//!
//! Installing on request is Nix-only for now (the plan for macOS, Windows and other Linux
//! is in `docs/environments.md`). Failed output gets a [`hints`] line that names the one
//! tool to call, because small models follow an explicit next step far better than prose.

pub mod hints;
pub mod nix;
pub mod project;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use byob_sandbox::Outcome;
use serde::{Deserialize, Serialize};

use crate::Job;

/// Changes a run gets: `PATH` entries in front of the host's, variables, and sandbox access.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layer {
    /// Put in front of `PATH`, in this order.
    #[serde(default)]
    pub path: Vec<PathBuf>,
    /// Variables to set (never `PATH`; see [`Layer::path`]).
    #[serde(default)]
    pub vars: Vec<(String, String)>,
    #[serde(default)]
    pub read: Vec<PathBuf>,
    #[serde(default)]
    pub write: Vec<PathBuf>,
    /// Unix sockets the run may connect to without network (the Nix daemon's).
    #[serde(default)]
    pub sockets: Vec<PathBuf>,
}

impl Layer {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// Add `other` underneath: its `PATH` entries go after ours and it can't override our
    /// variables.
    pub fn stack(&mut self, other: &Layer) {
        for p in &other.path {
            if !self.path.contains(p) {
                self.path.push(p.clone());
            }
        }
        for (k, v) in &other.vars {
            if !self.vars.iter().any(|(have, _)| have == k) {
                self.vars.push((k.clone(), v.clone()));
            }
        }
        self.read.extend(other.read.iter().cloned());
        self.write.extend(other.write.iter().cloned());
        self.sockets.extend(other.sockets.iter().cloned());
    }

    pub fn var(&self, name: &str) -> Option<&str> {
        self.vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

/// Variables a layer must never set: the sandbox's own home, temp and cache folders, and
/// what only makes sense inside a Nix build (mirrors `nix develop`'s ignore list).
pub const KEEP_VARS: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "USERPROFILE",
    "TMPDIR",
    "TMP",
    "TEMP",
    "TEMPDIR",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "MPLCONFIGDIR",
    "TERM",
    "TZ",
    "PWD",
    "OLDPWD",
    "SHLVL",
    "_",
    "UID",
    "PPID",
    "BASHOPTS",
    "SHELLOPTS",
    "NIX_BUILD_TOP",
    "NIX_LOG_FD",
    "NIX_ENFORCE_PURITY",
    "NIX_REMOTE",
    "NIX_ATTRS_JSON_FILE",
    "NIX_ATTRS_SH_FILE",
    "SSL_CERT_FILE",
    "NIX_SSL_CERT_FILE",
    "SOURCE_DATE_EPOCH",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "out",
    "shellHook",
];

pub fn keep_var(name: &str) -> bool {
    KEEP_VARS.contains(&name)
}

/// Runs a sandboxed job for a provider (the tool's own sandbox, with the support layer).
pub(crate) trait Runner {
    /// Run `job` in `cwd` with `tmp` as its private temp folder.
    fn run(&self, job: Job, cwd: &Path, tmp: &Path) -> Result<Outcome, String>;
}

/// What [`Envs::prepare`] found for one run.
#[derive(Debug, Default)]
pub(crate) struct Prepared {
    pub layer: Layer,
    /// One-time messages for the model (a dev shell just loaded, or failed to).
    pub notes: Vec<String>,
}

/// The result of installing packages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub text: String,
    /// Some packages weren't found or the build failed.
    pub failed: bool,
}

/// Turn workspace environments on or off: `BYOB_WORKSPACE_ENV=off` skips loading the
/// workspace's own dev shell (installed packages and the Nix CLI still work).
pub const WORKSPACE_ENV: &str = "BYOB_WORKSPACE_ENV";

/// An environment a workspace declares in a project file (`flake.nix`, `pixi.toml`,
/// `mise.toml`, …): loaded once (slow, may download), then cached until its files change.
pub(crate) trait Project: Send + Sync + std::fmt::Debug {
    /// Names the cache file.
    fn id(&self) -> &'static str;
    /// For messages: "Nix dev shell".
    fn label(&self) -> &'static str;
    /// The file in `ws` that declares it, if any.
    fn file(&self, ws: &Path) -> Option<&'static str>;
    /// Files whose contents key the cache (lock files included).
    fn inputs(&self) -> &'static [&'static str];
    /// Load it now: a sandboxed run with network that may write the workspace and `dir`.
    fn load(&self, ws: &Path, dir: &Path, run: &dyn Runner) -> Result<Layer, String>;
}

/// The result of [`load_project`].
#[derive(Debug)]
pub(crate) struct ProjectEnv {
    pub file: &'static str,
    pub result: Result<Layer, String>,
    /// Loaded just now (else from the cache).
    pub fresh: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct ProjectCache {
    stamp: String,
    #[serde(default)]
    layer: Option<Layer>,
    #[serde(default)]
    error: Option<String>,
}

/// Cache key: the project's input files' contents.
fn stamp(ws: &Path, p: &dyn Project) -> String {
    let mut bytes = format!("{}\0", p.id()).into_bytes();
    for f in p.inputs() {
        bytes.extend_from_slice(f.as_bytes());
        bytes.push(0);
        bytes.extend(std::fs::read(ws.join(f)).unwrap_or_default());
        bytes.push(0);
    }
    format!("{:016x}", fnv64(&bytes))
}

/// `p`'s environment for `ws`, from `<dir>/<id>.json` while its files are unchanged (and the
/// folders it put on `PATH` still exist), else loaded now. Failures are cached too, so a
/// broken project file is reported once rather than retried before every command.
pub(crate) fn load_project(p: &dyn Project, ws: &Path, dir: &Path, run: &dyn Runner) -> Option<ProjectEnv> {
    let file = p.file(ws)?;
    let cache_file = dir.join(format!("{}.json", p.id()));
    let now = stamp(ws, p);
    if let Some(c) = std::fs::read_to_string(&cache_file).ok().and_then(|s| serde_json::from_str::<ProjectCache>(&s).ok())
        && c.stamp == now
    {
        match (c.layer, c.error) {
            (Some(l), _) if l.path.iter().all(|p| p.exists()) => return Some(ProjectEnv { file, result: Ok(l), fresh: false }),
            (_, Some(e)) => return Some(ProjectEnv { file, result: Err(e), fresh: false }),
            _ => {}
        }
    }
    let result = std::fs::create_dir_all(dir)
        .map_err(|e| format!("can't create {}: {e}", dir.display()))
        .and_then(|()| dir.canonicalize().map_err(|e| e.to_string()))
        .and_then(|dir| p.load(ws, &dir, run));
    // Stamp after loading: the tool may just have written its lock file.
    let cache = match &result {
        Ok(l) => ProjectCache { stamp: stamp(ws, p), layer: Some(l.clone()), error: None },
        Err(e) => ProjectCache { stamp: stamp(ws, p), layer: None, error: Some(e.clone()) },
    };
    if result.as_ref().err().is_none_or(|e| e != STOPPED)
        && let Err(e) = write_atomic(&cache_file, serde_json::to_string(&cache).unwrap_or_default().as_bytes())
    {
        log::warn!("environments: {e}");
    }
    Some(ProjectEnv { file, result, fresh: true })
}

pub(crate) const STOPPED: &str = "Stopped by the user.";

/// The package managers found on this machine, and the per-workspace state they keep.
#[derive(Debug, Default)]
pub struct Envs {
    nix: Option<nix::Nix>,
    /// Project environments, in order of precedence.
    projects: Vec<Box<dyn Project>>,
    /// `<data>/envs`, holding one folder per workspace.
    state: Option<PathBuf>,
    load_project_shells: bool,
    /// One activation or install at a time (they share caches and GC roots).
    busy: Mutex<()>,
}

impl Envs {
    /// Detect what this machine has. `data_dir` holds caches and installed packages;
    /// without it only the package managers' CLIs are made to work inside the sandbox.
    pub fn detect(data_dir: Option<&Path>) -> Self {
        let nix = nix::Nix::detect(data_dir);
        if let Some(n) = &nix {
            log::info!("environments: Nix {} ({})", n.binary().display(), if n.daemon() { "daemon" } else { "single-user" });
        }
        let off = std::env::var(WORKSPACE_ENV).is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "off" | "0" | "false" | "no"));
        let mut projects: Vec<Box<dyn Project>> = Vec::new();
        if let Some(n) = &nix {
            projects.push(Box::new(nix::NixShell(n.clone())));
        }
        for p in project::detect(data_dir) {
            log::info!("environments: {} ({})", p.label(), p.program().display());
            projects.push(Box::new(p));
        }
        Self { nix, projects, state: data_dir.map(|d| d.join("envs")), load_project_shells: !off, busy: Mutex::new(()) }
    }

    /// No package managers (tests, or a machine without any).
    pub fn none() -> Self {
        Self::default()
    }

    /// The package manager `install_packages` uses here, if any.
    pub fn installer(&self) -> Option<&'static str> {
        self.nix.as_ref().filter(|_| self.state.is_some()).map(|_| "Nix")
    }

    /// A short paragraph for the system prompt.
    pub fn prompt(&self) -> Option<String> {
        let name = self.installer()?;
        Some(format!(
            "Software: run_command and run_python only have what is installed. When a program or a Python library is missing \
             (\"command not found\", ModuleNotFoundError), call install_packages — it uses {name} and keeps the packages for this \
             workspace — then run the command again. Don't use pip install, apt, brew or sudo: they don't work here."
        ))
    }

    /// Per-workspace state folder: `<data>/envs/<name>-<hash of the path>`.
    fn ws_dir(&self, ws: &Path) -> Option<PathBuf> {
        let base = self.state.as_ref()?;
        let name: String = ws
            .file_name()
            .map(|n| n.to_string_lossy().chars().filter(|c| c.is_ascii_alphanumeric() || "-_".contains(*c)).take(32).collect())
            .unwrap_or_default();
        let hash = fnv64(ws.as_os_str().as_encoded_bytes());
        Some(base.join(format!("{}-{hash:016x}", if name.is_empty() { "ws" } else { &name })))
    }

    /// The support layer every sandboxed run gets.
    pub(crate) fn support(&self) -> Layer {
        self.nix.as_ref().map(nix::Nix::support).unwrap_or_default()
    }

    /// Layers for one run in workspace `ws` (canonical path).
    pub(crate) fn prepare(&self, ws: &Path, run: &dyn Runner) -> Prepared {
        let _busy = self.busy.lock().unwrap_or_else(PoisonError::into_inner);
        let mut out = Prepared::default();
        let dir = self.ws_dir(ws);
        if let (Some(nix), Some(dir)) = (&self.nix, &dir)
            && let Some(l) = nix.installed(dir)
        {
            out.layer.stack(&l);
        }
        if let Some(l) = venv_layer(ws) {
            out.layer.stack(&l);
        }
        let Some(dir) = dir.filter(|_| self.load_project_shells) else { return out };
        for p in &self.projects {
            let Some(env) = load_project(p.as_ref(), ws, &dir, run) else { continue };
            match env.result {
                Ok(l) => {
                    out.layer.stack(&l);
                    if env.fresh {
                        out.notes.push(format!("Loaded the workspace's {} from {} (its programs are on PATH; cached until it changes).", p.label(), env.file));
                    }
                }
                Err(e) if env.fresh => out.notes.push(format!("The workspace's {} ({}) failed to load, so this ran without it:\n{e}", p.label(), env.file)),
                Err(_) => {}
            }
        }
        out
    }

    /// Install `packages` for workspace `ws`.
    pub(crate) fn install(&self, ws: &Path, packages: &[String], run: &dyn Runner) -> Result<Installed, String> {
        let _busy = self.busy.lock().unwrap_or_else(PoisonError::into_inner);
        let (Some(nix), Some(dir)) = (&self.nix, self.ws_dir(ws)) else { return Err("no package manager is available here".into()) };
        nix.install(&dir, packages, run)
    }
}

/// A Python virtual environment in the workspace (`.venv` or `venv`): its scripts folder
/// goes first on `PATH`, as `activate` would do.
pub fn venv_layer(ws: &Path) -> Option<Layer> {
    for name in [".venv", "venv"] {
        let venv = ws.join(name);
        if !venv.join("pyvenv.cfg").is_file() {
            continue;
        }
        let bin = if cfg!(windows) { venv.join("Scripts") } else { venv.join("bin") };
        if bin.is_dir() {
            return Some(Layer { path: vec![bin], vars: vec![("VIRTUAL_ENV".into(), venv.to_string_lossy().into_owned())], ..Default::default() });
        }
    }
    None
}

/// `PATH` for a run: `prefix` then the host's.
pub fn join_path(prefix: &[PathBuf], host: Option<&std::ffi::OsStr>) -> OsString {
    let mut all: Vec<PathBuf> = prefix.to_vec();
    if let Some(h) = host {
        for p in std::env::split_paths(h) {
            if !all.contains(&p) {
                all.push(p);
            }
        }
    }
    std::env::join_paths(all).unwrap_or_default()
}

/// FNV-1a: a stable 64-bit hash for cache keys (std's hasher may change between releases).
pub fn fnv64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Write `bytes` to `path` via a temp file and rename, so readers never see half a file.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("can't create {}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path)).map_err(|e| format!("can't write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stacking_keeps_the_first_value_and_order() {
        let mut a = Layer { path: vec!["/a".into()], vars: vec![("X".into(), "1".into())], ..Default::default() };
        let b = Layer {
            path: vec!["/b".into(), "/a".into()],
            vars: vec![("X".into(), "2".into()), ("Y".into(), "3".into())],
            sockets: vec!["/s".into()],
            ..Default::default()
        };
        a.stack(&b);
        assert_eq!(a.path, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
        assert_eq!(a.var("X"), Some("1"));
        assert_eq!(a.var("Y"), Some("3"));
        assert_eq!(a.sockets, vec![PathBuf::from("/s")]);
    }

    #[cfg(unix)]
    #[test]
    fn path_is_prefix_then_host_without_duplicates() {
        let p = join_path(&["/env/bin".into(), "/usr/bin".into()], Some("/usr/bin:/bin".as_ref()));
        assert_eq!(p, "/env/bin:/usr/bin:/bin");
    }

    #[test]
    fn venv_in_the_workspace_goes_first() {
        let d = tempfile::tempdir().unwrap();
        assert!(venv_layer(d.path()).is_none());
        let bin = if cfg!(windows) { "Scripts" } else { "bin" };
        std::fs::create_dir_all(d.path().join(".venv").join(bin)).unwrap();
        assert!(venv_layer(d.path()).is_none(), "no pyvenv.cfg, not a venv");
        std::fs::write(d.path().join(".venv/pyvenv.cfg"), "home = /usr/bin\n").unwrap();
        let l = venv_layer(d.path()).unwrap();
        assert_eq!(l.path, vec![d.path().join(".venv").join(bin)]);
        assert!(l.var("VIRTUAL_ENV").is_some());
    }

    /// Loads by counting; fails while `bad` exists in the workspace.
    #[derive(Debug, Default)]
    struct Fake(std::sync::atomic::AtomicUsize);

    impl Project for Fake {
        fn id(&self) -> &'static str {
            "fake"
        }
        fn label(&self) -> &'static str {
            "fake env"
        }
        fn file(&self, ws: &Path) -> Option<&'static str> {
            ws.join("fake.toml").is_file().then_some("fake.toml")
        }
        fn inputs(&self) -> &'static [&'static str] {
            &["fake.toml", "fake.lock"]
        }
        fn load(&self, ws: &Path, _dir: &Path, _run: &dyn Runner) -> Result<Layer, String> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // Like `nix` writing flake.lock on first load.
            std::fs::write(ws.join("fake.lock"), "locked").map_err(|e| e.to_string())?;
            if ws.join("bad").exists() { Err("broken".into()) } else { Ok(Layer { vars: vec![("A".into(), "1".into())], ..Default::default() }) }
        }
    }

    struct NoRun;
    impl Runner for NoRun {
        fn run(&self, _job: Job, _cwd: &Path, _tmp: &Path) -> Result<Outcome, String> {
            Err("not in this test".into())
        }
    }

    #[test]
    fn project_envs_are_cached_until_their_files_change() {
        let ws = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let p = Fake::default();
        let loads = || p.0.load(std::sync::atomic::Ordering::SeqCst);
        assert!(load_project(&p, ws.path(), dir.path(), &NoRun).is_none(), "no project file");
        std::fs::write(ws.path().join("fake.toml"), "v1").unwrap();
        let first = load_project(&p, ws.path(), dir.path(), &NoRun).unwrap();
        assert!(first.fresh && first.result.unwrap().var("A") == Some("1") && loads() == 1);
        let second = load_project(&p, ws.path(), dir.path(), &NoRun).unwrap();
        assert!(!second.fresh && second.result.is_ok() && loads() == 1, "cached, though the load wrote the lock file");
        std::fs::write(ws.path().join("fake.toml"), "v2").unwrap();
        std::fs::write(ws.path().join("bad"), "").unwrap();
        let broken = load_project(&p, ws.path(), dir.path(), &NoRun).unwrap();
        assert!(broken.fresh && broken.result.is_err() && loads() == 2);
        let again = load_project(&p, ws.path(), dir.path(), &NoRun).unwrap();
        assert!(!again.fresh && again.result.is_err() && loads() == 2, "a failure is cached too");
    }

    #[test]
    fn workspace_folders_are_stable_and_distinct() {
        let e = Envs { state: Some("/data/envs".into()), ..Envs::default() };
        let a = e.ws_dir(Path::new("/home/me/My Project")).unwrap();
        assert_eq!(a, e.ws_dir(Path::new("/home/me/My Project")).unwrap());
        assert_ne!(a, e.ws_dir(Path::new("/home/you/My Project")).unwrap());
        assert!(a.file_name().unwrap().to_string_lossy().starts_with("MyProject-"));
        assert_eq!(fnv64(b""), 0xcbf2_9ce4_8422_2325);
        assert!(Envs::none().installer().is_none() && Envs::none().prompt().is_none());
    }
}
