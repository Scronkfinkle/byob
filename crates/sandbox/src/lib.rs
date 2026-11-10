//! Run untrusted commands (model-written shell and Python, plugin scripts, local MCP
//! servers) confined to a workspace.
//!
//! The workspace forbids `unsafe`, so restrictions can't be applied between `fork` and
//! `exec` (`pre_exec`). Instead the app binary doubles as a **helper**: run as
//! `byob __sandbox-exec <policy-json> -- <cmd> <args…>` it confines itself and then becomes
//! (or, with a PID namespace, starts and supervises) the command. [`Sandbox`] builds such
//! command lines; [`dispatch_helper`] goes first in the app's `main`.
//!
//! ```no_run
//! # use std::path::Path;
//! use byob_sandbox::{Policy, RunOptions, Sandbox};
//! let sandbox = Sandbox::from_current_exe()?;
//! let policy = Policy::for_workspace(Path::new("/home/me/project"), Path::new("/tmp/byob-run-1"));
//! let mut cmd = sandbox.command(&policy, "sh", ["-c", "ls"]);
//! byob_sandbox::clean_env(&mut cmd, Path::new("/tmp/byob-run-1"));
//! let out = byob_sandbox::run(cmd, &RunOptions::default(), &|| false)?;
//! # Ok::<(), String>(())
//! ```
//!
//! What's enforced, per platform, is in [`linux`](crate#linux), the macOS profile
//! ([`seatbelt`]) and [`Enforcement`]. On anything else the helper refuses to run unless
//! the policy says `allow_unsandboxed` (an explicit user opt-in).
//!
//! # Linux
//! Landlock filesystem allow-list (required), Landlock TCP deny (ABI ≥ 4), abstract
//! unix socket + signal scoping (ABI ≥ 6), `no_new_privs`, rlimits, and — when
//! unprivileged user namespaces are allowed — an empty network namespace (no network at
//! all, UDP included) and a PID namespace (killing the run kills everything it started).
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

mod helper;
mod policy;
mod run;
pub mod seatbelt;
mod snapshot;

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod platform;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[path = "other.rs"]
mod platform;

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

pub use helper::{ERROR_PREFIX, Enforcement, HELPER_ARG, HelperArgs, PROBE_ARG, SETUP_FAILED, dispatch_helper, helper_main, parse_helper_args};
pub use policy::{Limits, MAX_PATHS, MAX_POLICY_LEN, Policy, system_read_paths, system_write_paths};
pub use run::{Captured, Outcome, RunOptions, kill_group, run};
pub use snapshot::{Changes, Snapshot};

/// Set to `off` to run commands without a sandbox (an explicit user opt-in; logged).
pub const DISABLE_ENV: &str = "BYOB_SANDBOX";

/// The user turned the sandbox off with `BYOB_SANDBOX=off` (or `0`, `false`, `no`).
pub fn disabled_by_user() -> bool {
    std::env::var(DISABLE_ENV).is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "off" | "0" | "false" | "no"))
}

/// Builds helper command lines. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Sandbox {
    helper: PathBuf,
    probed: Arc<OnceLock<Enforcement>>,
}

impl Sandbox {
    /// `helper` is a binary that calls [`dispatch_helper`] (or [`helper_main`]) first.
    pub fn new(helper: impl Into<PathBuf>) -> Self {
        Self { helper: helper.into(), probed: Arc::default() }
    }

    /// This binary as the helper (it must call [`dispatch_helper`] in `main`).
    pub fn from_current_exe() -> Result<Self, String> {
        std::env::current_exe().map(Self::new).map_err(|e| format!("can't find the app binary for the sandbox helper: {e}"))
    }

    pub fn helper(&self) -> &Path {
        &self.helper
    }

    /// What this machine can enforce (runs the helper once, then cached).
    pub fn probe(&self) -> Enforcement {
        self.probed.get_or_init(|| self.probe_now()).clone()
    }

    fn probe_now(&self) -> Enforcement {
        let mut cmd = Command::new(&self.helper);
        cmd.args([HELPER_ARG, PROBE_ARG]);
        let opts = RunOptions { timeout: Duration::from_secs(10), max_output: 16 * 1024, ..Default::default() };
        match run(cmd, &opts, &|| false) {
            Ok(o) if o.success() => serde_json::from_slice(&o.stdout.bytes()).unwrap_or_else(|e| Enforcement::none(format!("bad probe output: {e}"))),
            Ok(o) => Enforcement::none(format!("the sandbox helper failed: {}", o.stderr.text().trim())),
            Err(e) => Enforcement::none(e),
        }
    }

    /// Helper arguments (after the helper binary) that run `program args…` under `policy`.
    pub fn argv<I, S>(&self, policy: &Policy, program: impl AsRef<OsStr>, args: I) -> Vec<OsString>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut v = vec![OsString::from(HELPER_ARG), OsString::from(policy.to_json()), OsString::from("--"), program.as_ref().to_os_string()];
        v.extend(args.into_iter().map(|a| a.as_ref().to_os_string()));
        v
    }

    /// A [`Command`] that runs `program args…` under `policy` via the helper. Set the
    /// working folder and environment on it as usual; hand it to [`run`].
    pub fn command<I, S>(&self, policy: &Policy, program: impl AsRef<OsStr>, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = Command::new(&self.helper);
        cmd.args(self.argv(policy, program, args));
        cmd
    }
}

/// The first executable `name` on `PATH` (or `name` itself if it's a path).
pub fn which(name: &OsStr) -> Option<PathBuf> {
    let p = Path::new(name);
    if p.components().count() > 1 || p.is_absolute() {
        return is_executable(p).then(|| p.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).filter(|d| d.is_absolute()).map(|d| d.join(name)).find(|p| is_executable(p))
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

/// Folders a sandbox must let an interpreter (`python3`, a venv's `python`) read beyond
/// the system folders: its venv (`<venv>/pyvenv.cfg`) and its real installation prefix
/// (`<prefix>/lib/python3*`). Never the home folder itself or anything above it.
pub fn interpreter_read_paths(program: &OsStr) -> Vec<PathBuf> {
    let Some(path) = which(program) else { return Vec::new() };
    let mut out = Vec::new();
    if let Some(venv) = path.parent().and_then(Path::parent)
        && venv.join("pyvenv.cfg").is_file()
    {
        out.push(venv.canonicalize().unwrap_or_else(|_| venv.to_path_buf()));
    }
    if let Ok(real) = path.canonicalize()
        && let Some(prefix) = real.parent().and_then(Path::parent)
        && looks_like_python_prefix(prefix)
    {
        out.push(prefix.to_path_buf());
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).and_then(|h| h.canonicalize().ok());
    out.retain(|p| p.parent().is_some() && !home.as_ref().is_some_and(|h| h.starts_with(p)));
    out
}

fn looks_like_python_prefix(prefix: &Path) -> bool {
    prefix.join("Lib").is_dir()
        || std::fs::read_dir(prefix.join("lib")).is_ok_and(|rd| rd.flatten().any(|e| e.file_name().to_string_lossy().starts_with("python3")))
}

/// Inherited variables that are safe and useful to pass to sandboxed code.
const PASS_ENV: &[&str] = &[
    "PATH",
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "LC_CTYPE",
    "LC_NUMERIC",
    "LC_TIME",
    "LC_COLLATE",
    "LC_MONETARY",
    "LC_MESSAGES",
    "TZ",
    "TZDIR",
    "LOCALE_ARCHIVE",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NIX_SSL_CERT_FILE",
    "FONTCONFIG_FILE",
    "FONTCONFIG_PATH",
    "SYSTEMROOT",
];

/// Replace `cmd`'s environment with a minimal one: no inherited secrets (API keys,
/// tokens), `HOME` and temp folders pointing at `tmp`, matplotlib on the Agg backend.
pub fn clean_env(cmd: &mut Command, tmp: &Path) {
    cmd.env_clear();
    for k in PASS_ENV {
        if let Some(v) = std::env::var_os(k) {
            cmd.env(k, v);
        }
    }
    if std::env::var_os("PATH").is_none() {
        cmd.env("PATH", "/usr/local/bin:/usr/bin:/bin");
    }
    if std::env::var_os("LANG").is_none() && std::env::var_os("LC_ALL").is_none() {
        cmd.env("LANG", "C.UTF-8");
    }
    for k in ["HOME", "TMPDIR", "TMP", "TEMP", "USERPROFILE"] {
        cmd.env(k, tmp);
    }
    cmd.env("XDG_CACHE_HOME", tmp.join(".cache"))
        .env("XDG_CONFIG_HOME", tmp.join(".config"))
        .env("MPLCONFIGDIR", tmp.join(".matplotlib"))
        .env("MPLBACKEND", "Agg")
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("PYTHONNOUSERSITE", "1")
        .env("PYTHONUNBUFFERED", "1")
        .env("PIP_DISABLE_PIP_VERSION_CHECK", "1")
        .env("TERM", "dumb")
        .env("NO_COLOR", "1")
        .env("PAGER", "cat")
        .env("GIT_PAGER", "cat");
}

/// A private temp folder for one run, removed when dropped.
#[derive(Debug)]
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(prefix: &str) -> Result<Self, String> {
        let base = std::env::temp_dir();
        for attempt in 0..16u32 {
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
            let p = base.join(format!("{prefix}{}-{nanos:x}-{attempt}", std::process::id()));
            match std::fs::create_dir(&p) {
                Ok(()) => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700));
                    }
                    return Ok(Self(p.canonicalize().unwrap_or(p)));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("can't create a temp folder in {}: {e}", base.display())),
            }
        }
        Err("can't create a temp folder".into())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv_shape() {
        let sb = Sandbox::new("/bin/byob");
        let p = Policy::default();
        let v = sb.argv(&p, "sh", ["-c", "true"]);
        let parsed = parse_helper_args(&v[1..]).unwrap();
        assert_eq!(parsed, HelperArgs::Exec { policy: Box::new(p), program: "sh".into(), args: vec!["-c".into(), "true".into()] });
        assert_eq!(v[0], HELPER_ARG);
    }

    #[test]
    fn temp_dir_is_private_and_removed() {
        let t = TempDir::new("byob-test-").unwrap();
        let p = t.path().to_path_buf();
        assert!(p.is_dir());
        drop(t);
        assert!(!p.exists());
    }
}
