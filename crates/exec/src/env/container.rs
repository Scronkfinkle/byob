//! Containers: a workspace can run its commands inside one long-lived Docker or Podman
//! container instead of on this machine (opt-in per workspace; asked once when the
//! workspace has a devcontainer.json).
//!
//! - **One container per workspace**, named `byob-<folder>-<hash of the path>` and
//!   labelled `byob.managed=1`, `byob.workspace=<path>`, `byob.config=<stamp>`. It's
//!   created on first use, reused across commands and byob restarts, and recreated when
//!   devcontainer.json (or the image setting) changes. It idles in a `sleep` loop under
//!   `--init`.
//! - **The workspace is mounted at the same path** (`-v /ws:/ws`), so paths in output,
//!   scripts and changed-file reports mean the same inside and out. byob's per-run temp
//!   folders live under `<data>/envs/<ws>/ctmp`, mounted the same way.
//! - **Commands run as the host user** (`exec -u uid:gid`; Podman adds
//!   `--userns=keep-id`), so files they write aren't owned by root. `install_packages`
//!   runs as root (apt-get / apk / dnf, and pip for Python libraries).
//! - **Network follows the call**: the container is disconnected from its network after
//!   creation and connected only while a call with `network: true` (or an install) runs.
//! - **Stop and timeouts reach inside**: each run records its process group (the
//!   `docker exec` process leads one), which is killed in the container when the run is
//!   stopped or times out — killing the CLI alone would leave it running.
//! - byob's own sandbox (Landlock, Seatbelt) doesn't apply inside: the container is the
//!   isolation. A container can only reach the workspace and byob's temp folder.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use byob_sandbox::{Outcome, RunOptions};

use super::devcontainer::{Cmd, Image, Spec};
use super::{Installed, STOPPED, fnv64};

/// The image for workspaces without a devcontainer.json: small, Debian (apt), Python 3.
pub const DEFAULT_IMAGE: &str = "python:3-slim";
/// `docker` or `podman`: overrides the settings and the automatic choice.
pub const RUNTIME_ENV: &str = "BYOB_CONTAINER_RUNTIME";

const QUICK: Duration = Duration::from_secs(60);
const SLOW: Duration = Duration::from_secs(1800);
const MAX_CLI_OUTPUT: usize = 64 * 1024;
/// Keeps the container alive doing nothing until it's stopped.
const IDLE: &str = "trap 'exit 0' TERM INT; while sleep 1000; do :; done";
/// Records the run's process group, sets up the user's home, runs the command.
const WRAPPER: &str = r#"echo $$ > "$BYOB_PID"; mkdir -p "$HOME" 2>/dev/null; export PATH="$HOME/.local/bin:$PATH"; exec "$@""#;
const HOME: &str = "/tmp/byob-home";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeKind {
    Docker,
    Podman,
}

/// The container CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runtime {
    pub kind: RuntimeKind,
    pub program: PathBuf,
}

impl Runtime {
    /// `pref` ("docker", "podman"; [`RUNTIME_ENV`] wins over it) or, by default, Podman
    /// if it's installed (rootless: a container escape is only the user), else Docker.
    /// Not on Windows yet (workspace paths can't be mounted at the same path).
    pub fn detect(pref: Option<&str>) -> Option<Self> {
        if cfg!(windows) {
            return None;
        }
        let find = |kind: RuntimeKind| {
            let name = if kind == RuntimeKind::Docker { "docker" } else { "podman" };
            byob_sandbox::which(name.as_ref()).map(|program| Self { kind, program })
        };
        let env = std::env::var(RUNTIME_ENV).ok();
        match env.as_deref().or(pref).map(|p| p.trim().to_ascii_lowercase()).as_deref() {
            Some("docker") => find(RuntimeKind::Docker),
            Some("podman") => find(RuntimeKind::Podman),
            _ => find(RuntimeKind::Podman).or_else(|| find(RuntimeKind::Docker)),
        }
    }

    pub fn name(&self) -> &'static str {
        match self.kind {
            RuntimeKind::Docker => "docker",
            RuntimeKind::Podman => "podman",
        }
    }

    /// The network a container joins for network access.
    fn network(&self) -> &'static str {
        match self.kind {
            RuntimeKind::Docker => "bridge",
            RuntimeKind::Podman => "podman",
        }
    }

    /// Run the CLI (outside byob's sandbox: it talks to the daemon, or is Podman itself).
    fn call<S: AsRef<std::ffi::OsStr>>(&self, args: &[S], timeout: Duration, cancelled: &dyn Fn() -> bool) -> Result<Outcome, String> {
        let mut cmd = Command::new(&self.program);
        cmd.args(args);
        let opts = RunOptions { timeout, max_output: MAX_CLI_OUTPUT, ..Default::default() };
        byob_sandbox::run(cmd, &opts, cancelled).map_err(|e| format!("can't run {}: {e}", self.name()))
    }

    /// Run the CLI and require success; returns stdout, trimmed.
    fn ok<S: AsRef<std::ffi::OsStr>>(&self, args: &[S], timeout: Duration, cancelled: &dyn Fn() -> bool) -> Result<String, String> {
        let out = self.call(args, timeout, cancelled)?;
        if out.cancelled {
            return Err(STOPPED.into());
        }
        if out.timed_out {
            return Err(format!("`{} {}` took too long", self.name(), first_args(args)));
        }
        if !out.success() {
            return Err(format!("`{} {}` failed: {}", self.name(), first_args(args), out.stderr.text().trim()));
        }
        Ok(out.stdout.text().trim().to_owned())
    }

    /// Whether the daemon (or Podman) answers; the error says what to do.
    pub fn check(&self) -> Result<(), String> {
        let never = || false;
        match self.call(&["info", "--format", "{{.ServerVersion}}"], QUICK, &never) {
            Ok(out) if out.success() => Ok(()),
            Ok(out) => Err(unreachable_message(self, out.stderr.text().trim())),
            Err(e) => Err(unreachable_message(self, &e)),
        }
    }
}

fn first_args<S: AsRef<std::ffi::OsStr>>(args: &[S]) -> String {
    args.iter().take(2).map(|a| a.as_ref().to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ")
}

fn unreachable_message(rt: &Runtime, detail: &str) -> String {
    let what = if rt.kind == RuntimeKind::Docker { "Docker isn't running or can't be reached" } else { "Podman doesn't work here" };
    let detail: String = detail.lines().take(3).collect::<Vec<_>>().join(" ");
    format!(
        "{what} (`{} info`: {detail}). Nothing ran. Start it, or turn off \"Run commands in a container\" for this workspace in \
         Settings → Workspace environment.",
        rt.name()
    )
}

/// A container ready to run commands in.
#[derive(Debug, Clone)]
pub struct Ready {
    pub name: String,
    /// For messages: the image, or what it was built from.
    pub image: String,
    pub remote_env: Vec<(String, String)>,
    /// Per-run temp folders go here (mounted at the same path).
    pub tmp_base: PathBuf,
    /// Created (or recreated) just now.
    pub fresh: bool,
    /// Problems worth telling the model (a failed postCreateCommand, …).
    pub notes: Vec<String>,
}

/// The containers of all workspaces, through one runtime.
#[derive(Debug)]
pub struct Containers {
    pub runtime: Runtime,
    pub default_image: String,
    /// One creation at a time.
    creating: Mutex<()>,
    /// Runs using the network, per container.
    online: Mutex<BTreeMap<String, usize>>,
}

/// `byob-<folder>-<hash>`: stable per workspace path, valid as a container name.
pub fn container_name(ws: &Path) -> String {
    let folder: String = ws
        .file_name()
        .map(|n| n.to_string_lossy().chars().filter(|c| c.is_ascii_alphanumeric() || "-_".contains(*c)).take(24).collect::<String>().to_ascii_lowercase())
        .unwrap_or_default();
    let hash = fnv64(ws.as_os_str().as_encoded_bytes());
    format!("byob-{}-{:012x}", if folder.is_empty() { "ws" } else { &folder }, hash & 0xffff_ffff_ffff)
}

/// What the container was made from, as a label: recreate when it changes.
fn stamp(spec: &Spec, rt: &Runtime) -> String {
    let mut s = format!("v1\0{:?}\0{:?}\0{:?}\0", rt.kind, spec.env, spec.on_create);
    match &spec.image {
        Image::Pull(i) => s.push_str(i),
        Image::Build { dockerfile, context, args } => {
            s.push_str(&format!("{}\0{}\0{args:?}\0", dockerfile.display(), context.display()));
            s.push_str(&String::from_utf8_lossy(&std::fs::read(dockerfile).unwrap_or_default()));
        }
    }
    format!("{:016x}", fnv64(s.as_bytes()))
}

/// `uid:gid` of whoever owns `p` (a folder byob just made: this process's user).
#[cfg(unix)]
fn user_of(p: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(p).ok()?;
    Some(format!("{}:{}", m.uid(), m.gid()))
}

#[cfg(not(unix))]
fn user_of(_p: &Path) -> Option<String> {
    None
}

/// The variables every run gets (like the sandbox's clean environment).
fn run_env(tmp: &Path) -> Vec<(String, String)> {
    let t = tmp.to_string_lossy().into_owned();
    let mut v: Vec<(String, String)> = [
        ("HOME", HOME),
        ("LANG", "C.UTF-8"),
        ("MPLBACKEND", "Agg"),
        ("PYTHONIOENCODING", "utf-8"),
        ("PYTHONUNBUFFERED", "1"),
        ("PYTHONDONTWRITEBYTECODE", "1"),
        ("PIP_DISABLE_PIP_VERSION_CHECK", "1"),
        ("TERM", "dumb"),
        ("NO_COLOR", "1"),
        ("PAGER", "cat"),
        ("GIT_PAGER", "cat"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    for k in ["TMPDIR", "TMP", "TEMP"] {
        v.push((k.to_owned(), t.clone()));
    }
    v.push(("MPLCONFIGDIR".into(), format!("{t}/.matplotlib")));
    v
}

/// One command to run in a container.
#[derive(Debug)]
pub struct Exec {
    pub argv: Vec<OsString>,
    pub env: Vec<(String, OsString)>,
    pub network: bool,
    pub timeout: Duration,
}

impl Containers {
    pub fn new(runtime: Runtime, default_image: Option<String>) -> Self {
        let default_image = default_image.filter(|i| super::devcontainer::valid_image(i)).unwrap_or_else(|| DEFAULT_IMAGE.to_owned());
        Self { runtime, default_image, creating: Mutex::new(()), online: Mutex::new(BTreeMap::new()) }
    }

    /// The container's state ("running", "exited", …), `None` if there is none (or the
    /// daemon doesn't answer).
    pub fn state(&self, ws: &Path) -> Option<String> {
        let never = || false;
        self.runtime.ok(&["container", "inspect", "--format", "{{.State.Status}}", &container_name(ws)], QUICK, &never).ok().filter(|s| !s.is_empty())
    }

    pub fn stop(&self, ws: &Path) -> Result<String, String> {
        let never = || false;
        let name = container_name(ws);
        self.runtime.ok(&["stop", "-t", "3", &name], QUICK, &never)?;
        Ok(format!("Stopped {name}; it starts again on the next command."))
    }

    pub fn remove(&self, ws: &Path) -> Result<String, String> {
        let never = || false;
        let name = container_name(ws);
        self.runtime.ok(&["rm", "-f", &name], QUICK, &never)?;
        Ok(format!("Removed {name}; the next command creates a new one."))
    }

    /// The workspace's container, created or started as needed. `tmp_base` is the folder
    /// for per-run temp folders (in byob's data folder).
    pub fn ensure(&self, ws: &Path, spec: &Spec, tmp_base: &Path, cancelled: &dyn Fn() -> bool) -> Result<Ready, String> {
        let _one = self.creating.lock().unwrap_or_else(PoisonError::into_inner);
        let rt = &self.runtime;
        let name = container_name(ws);
        let want = stamp(spec, rt);
        std::fs::create_dir_all(tmp_base).map_err(|e| format!("can't create {}: {e}", tmp_base.display()))?;
        let tmp_base = tmp_base.canonicalize().map_err(|e| format!("{}: {e}", tmp_base.display()))?;
        let mut ready = Ready {
            name: name.clone(),
            image: spec.describe(ws),
            remote_env: spec.remote_env.clone(),
            tmp_base: tmp_base.clone(),
            fresh: false,
            notes: Vec::new(),
        };
        let inspect = rt.call(&["container", "inspect", "--format", "{{.State.Status}}|{{index .Config.Labels \"byob.config\"}}", &name], QUICK, cancelled)?;
        if inspect.success() {
            let text = inspect.stdout.text();
            let (status, label) = text.trim().split_once('|').unwrap_or((text.trim(), ""));
            if label == want {
                if status != "running" {
                    rt.ok(&["start", &name], QUICK, cancelled)?;
                }
                return Ok(ready);
            }
            rt.ok(&["rm", "-f", &name], QUICK, cancelled)?;
            ready.notes.push("The workspace's container was recreated because its configuration changed; packages installed in the old one are gone.".into());
        } else {
            let err = inspect.stderr.text();
            if !err.to_ascii_lowercase().contains("no such") {
                rt.check()?;
                return Err(format!("`{} container inspect` failed: {}", rt.name(), err.trim()));
            }
        }
        let image = self.image(ws, spec, cancelled)?;
        let mut args: Vec<String> = vec!["run".into(), "-d".into(), "--init".into(), "--name".into(), name.clone()];
        for (k, v) in [("byob.managed", "1".to_owned()), ("byob.workspace", ws.display().to_string()), ("byob.config", want)] {
            args.extend(["--label".into(), format!("{k}={v}")]);
        }
        for dir in [ws, tmp_base.as_path()] {
            args.extend(["-v".into(), format!("{0}:{0}", dir.display())]);
        }
        args.extend(["-w".into(), ws.display().to_string(), "--network".into(), rt.network().into()]);
        if rt.kind == RuntimeKind::Podman {
            args.push("--userns=keep-id".into());
        }
        for (k, v) in &spec.env {
            args.extend(["-e".into(), format!("{k}={v}")]);
        }
        args.extend(["--entrypoint".into(), "/bin/sh".into(), image, "-c".into(), IDLE.into()]);
        rt.ok(&args, SLOW, cancelled)?;
        ready.fresh = true;
        // Lifecycle commands, as root, with network (they usually install things).
        for c in &spec.on_create {
            let mut a: Vec<String> = vec!["exec".into(), "-u".into(), "0".into(), "-w".into(), ws.display().to_string(), name.clone()];
            match c {
                Cmd::Shell(s) => a.extend(["/bin/sh".into(), "-c".into(), s.clone()]),
                Cmd::Args(v) => a.extend(v.iter().cloned()),
            }
            match rt.call(&a, SLOW, cancelled) {
                Ok(o) if o.success() => {}
                Ok(o) if o.cancelled => return Err(STOPPED.into()),
                Ok(o) => ready.notes.push(format!("The devcontainer's setup command failed (exit {:?}): {}", o.exit_code, o.stderr.text().trim())),
                Err(e) => ready.notes.push(format!("The devcontainer's setup command didn't run: {e}")),
            }
        }
        // No network until a call asks for it.
        if let Err(e) = rt.ok(&["network", "disconnect", rt.network(), &name], QUICK, cancelled) {
            log::warn!("containers: {name} keeps its network: {e}");
            ready.notes.push(format!("byob couldn't take the container's network away ({e}), so every command has internet access."));
        }
        Ok(ready)
    }

    /// The image to run: pulled if missing, or built.
    fn image(&self, ws: &Path, spec: &Spec, cancelled: &dyn Fn() -> bool) -> Result<String, String> {
        let rt = &self.runtime;
        match &spec.image {
            Image::Pull(i) => {
                if !rt.call(&["image", "inspect", "--format", "{{.Id}}", i], QUICK, cancelled)?.success() {
                    rt.ok(&["pull", i.as_str()], SLOW, cancelled).map_err(|e| format!("couldn't get the image {i}: {e}"))?;
                }
                Ok(i.clone())
            }
            Image::Build { dockerfile, context, args } => {
                let tag = format!("{}:{}", container_name(ws).replacen("byob-", "byob-image-", 1), stamp(spec, rt));
                let mut a: Vec<String> = vec!["build".into(), "-t".into(), tag.clone(), "-f".into(), dockerfile.display().to_string()];
                for (k, v) in args {
                    a.extend(["--build-arg".into(), format!("{k}={v}")]);
                }
                a.push(context.display().to_string());
                rt.ok(&a, SLOW, cancelled).map_err(|e| format!("building the dev container image failed: {e}"))?;
                Ok(tag)
            }
        }
    }

    /// Connect `name` to the network while `f` runs (counted, so overlapping runs share).
    fn online<T>(&self, name: &str, on: bool, f: impl FnOnce() -> T) -> (T, Option<String>) {
        if !on {
            return (f(), None);
        }
        let never = || false;
        let mut problem = None;
        {
            let mut users = self.online.lock().unwrap_or_else(PoisonError::into_inner);
            let n = users.entry(name.to_owned()).or_insert(0);
            if *n == 0
                && let Err(e) = self.runtime.ok(&["network", "connect", self.runtime.network(), name], QUICK, &never)
                && !e.contains("already exists")
            {
                problem = Some(format!("couldn't give the container network access: {e}"));
            }
            *n += 1;
        }
        let out = f();
        let mut users = self.online.lock().unwrap_or_else(PoisonError::into_inner);
        let n = users.entry(name.to_owned()).or_insert(1);
        *n = n.saturating_sub(1);
        if *n == 0 {
            users.remove(name);
            if let Err(e) = self.runtime.ok(&["network", "disconnect", self.runtime.network(), name], QUICK, &never) {
                log::warn!("containers: {name}: {e}");
            }
        }
        (out, problem)
    }

    /// Run `job` in the container as this user, with `cwd` (the workspace) and `tmp`
    /// (under `ready.tmp_base`) as its temp folder.
    pub fn exec(&self, ready: &Ready, job: Exec, cwd: &Path, tmp: &Path, cancelled: &dyn Fn() -> bool) -> Result<(Outcome, Option<String>), String> {
        let pid_file = tmp.join(".byob-pid");
        let mut args: Vec<OsString> = vec!["exec".into()];
        if let Some(u) = user_of(tmp) {
            args.extend(["-u".into(), u.into()]);
        }
        args.extend(["-w".into(), cwd.as_os_str().to_owned()]);
        let mut env: Vec<(String, OsString)> = run_env(tmp).into_iter().map(|(k, v)| (k, v.into())).collect();
        env.extend(ready.remote_env.iter().map(|(k, v)| (k.clone(), v.into())));
        env.extend(job.env);
        env.push(("BYOB_PID".into(), pid_file.clone().into_os_string()));
        for (k, v) in env {
            let mut kv = OsString::from(format!("{k}="));
            kv.push(v);
            args.extend(["-e".into(), kv]);
        }
        args.extend([ready.name.clone().into(), "/bin/sh".into(), "-c".into(), WRAPPER.into(), "sh".into()]);
        args.extend(job.argv);
        let (out, problem) = self.online(&ready.name, job.network, || self.runtime.call(&args, job.timeout, cancelled));
        let out = out?;
        if out.timed_out || out.cancelled {
            self.kill(ready, &pid_file);
        }
        Ok((out, problem))
    }

    /// Kill the run whose process group is in `pid_file`, inside the container.
    fn kill(&self, ready: &Ready, pid_file: &Path) {
        let Some(pid) = std::fs::read_to_string(pid_file).ok().and_then(|s| s.trim().parse::<u32>().ok()).filter(|p| *p > 1) else { return };
        let never = || false;
        let script = format!("kill -9 -{pid} 2>/dev/null; kill -9 {pid} 2>/dev/null; true");
        if let Err(e) = self.runtime.ok(&["exec", "-u", "0", &ready.name, "/bin/sh", "-c", &script], QUICK, &never) {
            log::warn!("containers: couldn't stop a run in {}: {e}", ready.name);
        }
    }

    /// Install `packages` in the container as root: the distribution's package manager for
    /// programs, pip for Python libraries (and for names it doesn't know).
    pub fn install(&self, ready: &Ready, ws: &Path, packages: &[String], cancelled: &dyn Fn() -> bool) -> Result<Installed, String> {
        let names: Vec<String> = packages.iter().map(|p| install_name(p)).collect::<Result<_, _>>()?;
        let mut args: Vec<String> = vec!["exec".into(), "-u".into(), "0".into(), "-w".into(), ws.display().to_string()];
        for (k, v) in [
            ("DEBIAN_FRONTEND", "noninteractive"),
            ("PIP_BREAK_SYSTEM_PACKAGES", "1"),
            ("PIP_ROOT_USER_ACTION", "ignore"),
            ("PIP_DISABLE_PIP_VERSION_CHECK", "1"),
        ] {
            args.extend(["-e".into(), format!("{k}={v}")]);
        }
        args.extend([ready.name.clone(), "/bin/sh".into(), "-c".into(), INSTALL.into(), "sh".into()]);
        args.extend(names);
        let (out, problem) = self.online(&ready.name, true, || self.runtime.call(&args, SLOW, cancelled));
        let out = out?;
        if out.cancelled {
            return Err(STOPPED.into());
        }
        let report = parse_install(&out.stdout.text());
        if let Some(p) = problem {
            return Err(format!("{p}\n{}", out.stderr.text().trim()));
        }
        if report.manager.is_empty() && report.pip.is_empty() && report.missing.is_empty() {
            return Err(format!("installing failed: {}", out.stderr.text().trim()));
        }
        Ok(report.describe(&out.stderr.text()))
    }
}

/// Shell script run as root in the container: `$@` are names, `py:` marks Python libraries.
/// Prints `BYOB-PM <manager>`, `BYOB-OS <names>` / `BYOB-OSFAIL <names>`, `BYOB-PIP <name>`,
/// `BYOB-MISSING <name>` on stdout; the managers' output goes to stderr.
const INSTALL: &str = r#"
pm=""
for m in apt-get apk dnf microdnf yum; do command -v $m >/dev/null 2>&1 && { pm=$m; break; }; done
echo "BYOB-PM $pm"
case "$pm" in apt-get) apt-get update -qq >&2 ;; apk) apk update -q >&2 ;; esac
os=""; py=""
for n in "$@"; do
  case "$n" in py:*) py="$py ${n#py:}"; continue ;; esac
  case "$pm" in
    apt-get) apt-cache show "$n" >/dev/null 2>&1 && os="$os $n" || py="$py $n" ;;
    apk) apk search -qe "$n" 2>/dev/null | grep -qx "$n" && os="$os $n" || py="$py $n" ;;
    dnf|microdnf|yum) $pm info "$n" >/dev/null 2>&1 && os="$os $n" || py="$py $n" ;;
    *) py="$py $n" ;;
  esac
done
if [ -n "$os" ]; then
  case "$pm" in
    apt-get) apt-get install -y -qq --no-install-recommends $os >&2 ;;
    apk) apk add --no-cache $os >&2 ;;
    *) $pm install -y $os >&2 ;;
  esac && echo "BYOB-OS$os" || echo "BYOB-OSFAIL$os"
fi
if [ -n "$py" ]; then
  if ! python3 -m pip --version >/dev/null 2>&1; then
    case "$pm" in
      apt-get) apt-get install -y -qq --no-install-recommends python3 python3-pip >&2 ;;
      apk) apk add --no-cache python3 py3-pip >&2 ;;
      dnf|microdnf|yum) $pm install -y python3 python3-pip >&2 ;;
    esac
  fi
  for p in $py; do
    if python3 -m pip install -q "$p" >&2; then echo "BYOB-PIP $p"; else echo "BYOB-MISSING $p"; fi
  done
fi
"#;

/// What [`INSTALL`] reported.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub manager: String,
    pub os: Vec<String>,
    pub os_failed: Vec<String>,
    pub pip: Vec<String>,
    pub missing: Vec<String>,
}

pub fn parse_install(stdout: &str) -> Report {
    let mut r = Report::default();
    for line in stdout.lines() {
        let words = |rest: &str| rest.split_whitespace().map(str::to_owned).collect::<Vec<_>>();
        if let Some(rest) = line.strip_prefix("BYOB-PM") {
            rest.trim().clone_into(&mut r.manager);
        } else if let Some(rest) = line.strip_prefix("BYOB-OSFAIL") {
            r.os_failed.extend(words(rest));
        } else if let Some(rest) = line.strip_prefix("BYOB-OS") {
            r.os.extend(words(rest));
        } else if let Some(rest) = line.strip_prefix("BYOB-PIP") {
            r.pip.extend(words(rest));
        } else if let Some(rest) = line.strip_prefix("BYOB-MISSING") {
            r.missing.extend(words(rest));
        }
    }
    r
}

impl Report {
    fn describe(&self, stderr: &str) -> Installed {
        let mut text = Vec::new();
        if !self.os.is_empty() {
            text.push(format!("Installed in the container with {}: {}.", self.manager, self.os.join(", ")));
        }
        if !self.pip.is_empty() {
            text.push(format!("Installed in the container with pip: {} (run_python's python3 has them).", self.pip.join(", ")));
        }
        if !self.os.is_empty() || !self.pip.is_empty() {
            text.push("run_command and run_python can use them now; they stay in this workspace's container.".into());
        }
        if !self.os_failed.is_empty() {
            let tail: String = stderr.lines().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
            text.push(format!("{} couldn't install {}:\n{tail}", self.manager, self.os_failed.join(", ")));
        }
        if !self.missing.is_empty() {
            text.push(format!(
                "Not found{}: {}. Call install_packages again with the exact package name.",
                if self.manager.is_empty() { String::new() } else { format!(" with {} or pip", self.manager) },
                self.missing.iter().map(|m| format!("{m:?}")).collect::<Vec<_>>().join(", ")
            ));
        }
        let failed = !self.missing.is_empty() || !self.os_failed.is_empty();
        Installed { text: text.join("\n"), failed, missing: self.missing.clone() }
    }
}

/// Python libraries models commonly ask for: installed with pip, not as system packages.
const PIP_NAMES: &[&str] = &[
    "numpy",
    "pandas",
    "matplotlib",
    "scipy",
    "seaborn",
    "plotly",
    "requests",
    "httpx",
    "openpyxl",
    "xlrd",
    "pyyaml",
    "beautifulsoup4",
    "lxml",
    "sympy",
    "statsmodels",
    "pillow",
    "scikit-learn",
    "networkx",
    "tabulate",
    "polars",
    "pyarrow",
    "jinja2",
    "pytest",
    "flask",
    "fastapi",
    "torch",
    "tqdm",
    "rich",
    "click",
    "pydantic",
    "python-dateutil",
    "pytz",
    "markdown",
    "pypdf",
    "reportlab",
    "python-docx",
];

/// The name to pass to [`INSTALL`]: `py:<name>` for Python libraries, a Debian-style name
/// otherwise; refuses anything that isn't a plain package name.
pub fn install_name(raw: &str) -> Result<String, String> {
    let p = raw.trim();
    let (py, p) = match p.strip_prefix("python3Packages.").or_else(|| p.strip_prefix("python3.pkgs.")) {
        Some(rest) => (true, rest),
        None => (false, p),
    };
    let ok = !p.is_empty() && p.len() <= 100 && !p.starts_with(['-', '.']) && p.chars().all(|c| c.is_ascii_alphanumeric() || "-_.+=<>!~,".contains(c));
    if !ok {
        return Err(format!("{raw:?} is not a package name (like \"jq\" or \"pandas\")"));
    }
    let lower = p.to_ascii_lowercase();
    let base = lower.split(['=', '<', '>', '!', '~']).next().unwrap_or(&lower);
    let pip = py || PIP_NAMES.contains(&base) || super::nix::PYTHON_IMPORTS.iter().any(|(_, pkg)| *pkg == base);
    if pip {
        return Ok(format!("py:{p}"));
    }
    let os = match base {
        "poppler_utils" | "poppler" => "poppler-utils",
        "gnumake" => "make",
        "python" => "python3",
        "fd" => "fd-find",
        "nodejs_22" | "nodejs_20" | "node" => "nodejs",
        _ => base,
    };
    Ok(os.to_owned())
}

/// A per-run temp folder under the container's temp base, removed when dropped.
#[derive(Debug)]
pub struct RunTmp(PathBuf);

impl RunTmp {
    pub fn new(base: &Path) -> Result<Self, String> {
        for attempt in 0..16u32 {
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
            let p = base.join(format!("run-{}-{nanos:x}-{attempt}", std::process::id()));
            match std::fs::create_dir(&p) {
                Ok(()) => return Ok(Self(p)),
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

impl Drop for RunTmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_stable_and_valid() {
        let a = container_name(Path::new("/home/me/My Project"));
        assert_eq!(a, container_name(Path::new("/home/me/My Project")));
        assert_ne!(a, container_name(Path::new("/home/you/My Project")));
        assert!(a.starts_with("byob-myproject-") && a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'), "{a}");
        assert!(container_name(Path::new("/")).starts_with("byob-ws-"));
    }

    #[test]
    fn install_names() {
        assert_eq!(install_name("jq").unwrap(), "jq");
        assert_eq!(install_name("pandas").unwrap(), "py:pandas");
        assert_eq!(install_name("scikit-learn").unwrap(), "py:scikit-learn");
        assert_eq!(install_name("python3Packages.six").unwrap(), "py:six");
        assert_eq!(install_name("numpy>=2").unwrap(), "py:numpy>=2");
        assert_eq!(install_name("poppler_utils").unwrap(), "poppler-utils");
        for bad in ["", "-y", "a b", "x;rm -rf /", "$(id)"] {
            assert!(install_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn install_reports() {
        let r = parse_install("BYOB-PM apt-get\nBYOB-OS jq ripgrep\nBYOB-PIP pandas\nBYOB-MISSING nosuch\n");
        assert_eq!(
            r,
            Report {
                manager: "apt-get".into(),
                os: vec!["jq".into(), "ripgrep".into()],
                os_failed: vec![],
                pip: vec!["pandas".into()],
                missing: vec!["nosuch".into()]
            }
        );
        let i = r.describe("");
        assert!(i.failed && i.missing == vec!["nosuch".to_owned()]);
        assert!(
            i.text.contains("with apt-get: jq, ripgrep") && i.text.contains("pip: pandas") && i.text.contains("Not found with apt-get or pip: \"nosuch\""),
            "{}",
            i.text
        );
        let ok = parse_install("BYOB-PM apk\nBYOB-OS jq\n").describe("");
        assert!(!ok.failed && ok.text.contains("with apk: jq"));
    }

    #[test]
    fn stamps_change_with_the_config() {
        let rt = Runtime { kind: RuntimeKind::Docker, program: "docker".into() };
        let a = Spec::default_image("python:3-slim");
        let b = Spec::default_image("debian:stable-slim");
        assert_eq!(stamp(&a, &rt), stamp(&a.clone(), &rt));
        assert_ne!(stamp(&a, &rt), stamp(&b, &rt));
        let podman = Runtime { kind: RuntimeKind::Podman, program: "podman".into() };
        assert_ne!(stamp(&a, &rt), stamp(&a, &podman));
        assert_eq!(Containers::new(rt.clone(), Some("--bad".into())).default_image, DEFAULT_IMAGE);
        assert_eq!(Containers::new(rt, Some("alpine:3".into())).default_image, "alpine:3");
    }

    #[test]
    fn the_daemon_being_down_says_what_to_do() {
        let rt = Runtime { kind: RuntimeKind::Docker, program: "docker".into() };
        let m = unreachable_message(&rt, "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?");
        assert!(m.contains("Docker isn't running") && m.contains("Cannot connect") && m.contains("Settings"), "{m}");
    }
}
