//! What a sandboxed process may do. Built by the app, serialized to JSON, handed to the
//! helper on its command line, applied there.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Longest policy (JSON) the helper accepts.
pub const MAX_POLICY_LEN: usize = 256 * 1024;
/// Most paths in each of `read` / `write`.
pub const MAX_PATHS: usize = 512;

/// Filesystem, network and resource rules for one run.
///
/// Everything not listed is denied: on Linux the filesystem rules are an allow-list
/// (Landlock), so a path in neither `read` nor `write` can't be opened at all.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// Readable and executable (directories: everything beneath). Missing paths are skipped.
    #[serde(default)]
    pub read: Vec<PathBuf>,
    /// Readable, writable, executable; files may be created and deleted beneath.
    #[serde(default)]
    pub write: Vec<PathBuf>,
    /// Allow network access. Denied by default.
    #[serde(default)]
    pub network: bool,
    #[serde(default)]
    pub limits: Limits,
    /// Environment variables removed before the command starts (e.g. API keys the
    /// helper inherited because the caller couldn't clear its environment).
    #[serde(default)]
    pub remove_env: Vec<String>,
    /// The user opted into running without a sandbox where the OS can't provide one.
    /// Without this, the helper refuses to run anything it can't confine.
    #[serde(default)]
    pub allow_unsandboxed: bool,
    /// Linux: don't use user/PID/network namespaces, only Landlock (weaker: TCP-only
    /// network denial, process-group-only kill). For tests and odd environments.
    #[serde(default)]
    pub skip_namespaces: bool,
}

/// `setrlimit` limits, applied to the helper before it starts the command (inherited by
/// every process the command starts). `None` keeps the inherited limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// CPU seconds per process (`RLIMIT_CPU`).
    #[serde(default)]
    pub cpu_seconds: Option<u64>,
    /// Address space per process (`RLIMIT_AS`).
    #[serde(default)]
    pub memory_bytes: Option<u64>,
    /// Largest file a process may write (`RLIMIT_FSIZE`).
    #[serde(default)]
    pub file_size_bytes: Option<u64>,
    /// Open files per process (`RLIMIT_NOFILE`).
    #[serde(default)]
    pub open_files: Option<u64>,
    /// Processes (`RLIMIT_NPROC`). Linux counts these per user, across the whole system,
    /// so it is only applied when the run gets its own user namespace (where the count
    /// starts at zero); otherwise it would fail for users who already run many processes.
    #[serde(default)]
    pub processes: Option<u64>,
}

impl Limits {
    /// Sensible limits for model-written code that runs for at most `timeout_secs`.
    pub fn for_timeout(timeout_secs: u64) -> Self {
        Self {
            cpu_seconds: Some(timeout_secs.saturating_add(5)),
            memory_bytes: Some(8 << 30),
            file_size_bytes: Some(2 << 30),
            open_files: Some(1024),
            processes: Some(512),
        }
    }
}

impl Policy {
    /// The usual policy for code working on a folder: read the system, read+write
    /// `workspace` and the run's private `tmp` folder, nothing else; no network.
    pub fn for_workspace(workspace: &Path, tmp: &Path) -> Self {
        let mut p = Self { read: system_read_paths(), write: vec![workspace.to_path_buf(), tmp.to_path_buf()], ..Default::default() };
        p.write.extend(system_write_paths());
        p
    }

    pub fn network(mut self, on: bool) -> Self {
        self.network = on;
        self
    }

    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Add a readable (and executable) path, canonicalized when possible.
    pub fn allow_read(mut self, path: impl AsRef<Path>) -> Self {
        let p = path.as_ref();
        self.read.push(p.canonicalize().unwrap_or_else(|_| p.to_path_buf()));
        self
    }

    /// Add a writable path, canonicalized when possible.
    pub fn allow_write(mut self, path: impl AsRef<Path>) -> Self {
        let p = path.as_ref();
        self.write.push(p.canonicalize().unwrap_or_else(|_| p.to_path_buf()));
        self
    }

    /// Make the directories on `PATH` readable, so commands installed there (e.g.
    /// `~/.local/bin`) can run. `/` and the home folder itself (or anything above it)
    /// are never added.
    pub fn allow_path_dirs(mut self, path_var: Option<&std::ffi::OsStr>) -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from).and_then(|h| h.canonicalize().ok());
        for dir in path_var.map(std::env::split_paths).into_iter().flatten() {
            if !dir.is_absolute() {
                continue;
            }
            let Ok(dir) = dir.canonicalize() else { continue };
            if dir.parent().is_none() || home.as_ref().is_some_and(|h| h.starts_with(&dir)) {
                continue;
            }
            if !self.read.contains(&dir) {
                self.read.push(dir);
            }
        }
        self
    }

    pub fn remove_env(mut self, name: impl Into<String>) -> Self {
        self.remove_env.push(name.into());
        self
    }

    pub fn allow_unsandboxed(mut self, yes: bool) -> Self {
        self.allow_unsandboxed = yes;
        self
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Parse and check a policy from the helper's command line.
    pub fn from_json(s: &str) -> Result<Self, String> {
        if s.len() > MAX_POLICY_LEN {
            return Err(format!("policy is too long ({} bytes)", s.len()));
        }
        let p: Self = serde_json::from_str(s).map_err(|e| format!("bad policy: {e}"))?;
        if p.read.len() > MAX_PATHS || p.write.len() > MAX_PATHS || p.remove_env.len() > MAX_PATHS {
            return Err("policy lists too many paths".into());
        }
        if let Some(bad) = p.read.iter().chain(&p.write).find(|p| !p.is_absolute()) {
            return Err(format!("policy path {} is not absolute", bad.display()));
        }
        Ok(p)
    }
}

/// Folders and files every program needs to read: libraries, the dynamic linker,
/// interpreters, a few files from `/etc`, `/proc`, `/sys`, `/dev/urandom`.
///
/// Deliberately not here: `/home`, `/root`, `/tmp`, `/var`, `/run`, `/mnt`, `/media`, and
/// most of `/etc`. Symlinks *through* those (e.g. NixOS's `/run/current-system/sw/bin`)
/// still work: Landlock checks the file reached, not the folders walked through.
pub fn system_read_paths() -> Vec<PathBuf> {
    if cfg!(target_os = "macos") {
        return ["/usr", "/bin", "/sbin", "/System", "/Library", "/opt/homebrew", "/usr/local", "/private/etc", "/dev", "/nix/store"]
            .iter()
            .map(PathBuf::from)
            .collect();
    }
    let mut v: Vec<PathBuf> =
        ["/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32", "/opt", "/nix/store", "/proc", "/sys"].iter().map(PathBuf::from).collect();
    // /etc: only what programs commonly read (name resolution, TLS roots, time zones,
    // locales, the linker cache, fonts) — not ssh keys, shadow files or app configs.
    const ETC: &[&str] = &[
        "ld.so.cache",
        "ld.so.conf",
        "ld.so.conf.d",
        "localtime",
        "timezone",
        "zoneinfo",
        "nsswitch.conf",
        "passwd",
        "group",
        "hosts",
        "host.conf",
        "gai.conf",
        "resolv.conf",
        "services",
        "protocols",
        "ssl",
        "pki",
        "ca-certificates",
        "fonts",
        "mime.types",
        "magic",
        "os-release",
        "lsb-release",
        "alternatives",
        "locale.alias",
        "locale.conf",
        "terminfo",
        "inputrc",
        "matplotlibrc",
        "static",
    ];
    v.extend(ETC.iter().map(|f| Path::new("/etc").join(f)));
    v.extend(["/dev/urandom", "/dev/random"].iter().map(PathBuf::from));
    v
}

/// Device files that are safe to write.
pub fn system_write_paths() -> Vec<PathBuf> {
    ["/dev/null", "/dev/zero", "/dev/full"].iter().map(PathBuf::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_round_trip_and_checks() {
        let p = Policy::for_workspace(Path::new("/w"), Path::new("/t")).network(true).limits(Limits::for_timeout(10)).remove_env("YAH_API_KEY");
        assert_eq!(Policy::from_json(&p.to_json()).unwrap(), p);
        assert!(Policy::from_json("{}").unwrap().read.is_empty());
        assert!(Policy::from_json(r#"{"read":["relative"]}"#).is_err());
        assert!(Policy::from_json(r#"{"nope":1}"#).is_err());
        assert!(Policy::from_json("[").is_err());
        assert!(Policy::from_json(&" ".repeat(MAX_POLICY_LEN + 1)).is_err());
        let many = serde_json::json!({ "read": vec!["/x"; MAX_PATHS + 1] }).to_string();
        assert!(Policy::from_json(&many).is_err());
    }

    #[test]
    fn path_dirs_skip_root_home_and_relative() {
        let tmp = tempfile::tempdir().unwrap();
        let joined = std::env::join_paths(["/", "relative/bin", tmp.path().to_str().unwrap()]).unwrap();
        let p = Policy::default().allow_path_dirs(Some(&joined));
        assert_eq!(p.read, vec![tmp.path().canonicalize().unwrap()]);
    }

    #[test]
    fn workspace_policy_denies_network_and_home() {
        let p = Policy::for_workspace(Path::new("/w"), Path::new("/t"));
        assert!(!p.network && !p.allow_unsandboxed);
        assert!(p.read.iter().all(|r| !r.starts_with("/home") && !r.starts_with("/tmp") && r != Path::new("/etc")));
    }
}
