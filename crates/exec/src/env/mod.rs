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
//! 3. **Project dev shell**: the workspace's own `flake.nix` or `shell.nix` (Nix), loaded with
//!    `nix print-dev-env` once and cached until those files change.
//! 4. **Support**: what a package manager's own CLI needs inside the sandbox, so the model
//!    can also run `nix shell nixpkgs#foo --command …` itself (store, daemon socket,
//!    config, a persistent eval cache).
//!
//! A [`Provider`] is one package manager (today: Nix; the plan for others is in
//! `docs/environments.md`). Failed output gets a [`hints`] line that names the one tool to
//! call, because small models follow an explicit next step far better than prose.

pub mod hints;
pub mod nix;

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

/// The package managers found on this machine, and the per-workspace state they keep.
#[derive(Debug, Default)]
pub struct Envs {
    nix: Option<nix::Nix>,
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
        Self { nix, state: data_dir.map(|d| d.join("envs")), load_project_shells: !off, busy: Mutex::new(()) }
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
        if self.load_project_shells
            && let (Some(nix), Some(dir)) = (&self.nix, &dir)
            && let Some(shell) = nix.project_shell(ws, dir, run)
        {
            match shell.result {
                Ok(l) => {
                    out.layer.stack(&l);
                    if shell.fresh {
                        out.notes
                            .push(format!("Loaded the workspace's Nix dev shell from {} (its programs are on PATH; cached until it changes).", shell.file));
                    }
                }
                Err(e) if shell.fresh => out.notes.push(format!("The workspace's {} dev shell failed to load, so this ran without it:\n{e}", shell.file)),
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
