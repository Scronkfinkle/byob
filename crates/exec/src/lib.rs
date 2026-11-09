//! Tools that run code: [`RunCommand`] (`run_command`, a shell command), [`RunPython`]
//! (`run_python`, Python code or a script) and [`InstallPythonPackages`]
//! (`install_python_packages`, into yah's managed venv with `uv`).
//!
//! Everything runs through [`yah_sandbox`]: read the system, read+write the workspace and
//! a private temp folder, no network unless the call asks (and the user approves), a
//! minimal environment without inherited secrets, wall-clock timeout, bounded output,
//! and the whole process tree is killed on timeout or Stop. Where the OS can't sandbox
//! (no Landlock), commands still run, with a warning in the log and in the tool output;
//! `YAH_SANDBOX=off` turns the sandbox off explicitly ([`Mode::Unsandboxed`]).
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

pub mod python;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use yah_sandbox::{Limits, Outcome, Policy, RunOptions, Sandbox, Snapshot, TempDir};
use yah_tools::{Access, FileChange, Registry, Tool, ToolContext, ToolOutput, ToolSpec, parse_args};

pub use python::Python;

pub const DEFAULT_TIMEOUT_S: u64 = 120;
pub const MAX_TIMEOUT_S: u64 = 600;
const INSTALL_TIMEOUT: Duration = Duration::from_secs(900);
/// Bytes kept per stream (head and tail halves) for the model.
pub const MAX_OUTPUT: usize = 24 * 1024;
const MAX_COMMAND_LEN: usize = 64 * 1024;
const MAX_CODE_LEN: usize = 512 * 1024;
const MAX_ARGS: usize = 64;
const MAX_PACKAGES: usize = 20;
/// Most changed files listed in the text the model sees.
const MAX_LISTED: usize = 30;

/// How commands run.
#[derive(Debug, Clone)]
pub enum Mode {
    Sandboxed(Sandbox),
    /// The user turned the sandbox off (`YAH_SANDBOX=off`): plain child processes, still
    /// with the clean environment, timeout, output caps and group kill.
    Unsandboxed,
}

impl Mode {
    /// `Unsandboxed` if the user opted out (logged), else the sandbox via this binary.
    /// Logs what the sandbox can enforce here (a warning if it can't confine anything).
    pub fn from_env() -> Result<Self, String> {
        if yah_sandbox::disabled_by_user() {
            log::warn!("{}=off: commands and Python run WITHOUT a sandbox", yah_sandbox::DISABLE_ENV);
            return Ok(Self::Unsandboxed);
        }
        let sb = Sandbox::from_current_exe()?;
        let e = sb.probe();
        if e.confined() {
            log::info!("sandbox: filesystem {}, network {}, process tree {}", e.filesystem, e.network, e.process_tree);
        } else {
            log::warn!("sandbox unavailable ({}); commands and Python will run WITHOUT one", e.notes.join("; "));
        }
        Ok(Self::Sandboxed(sb))
    }

    /// A note for the model/user when runs aren't actually confined.
    fn unconfined_note(&self) -> Option<String> {
        match self {
            Self::Unsandboxed => Some(format!("ran without a sandbox: {}=off", yah_sandbox::DISABLE_ENV)),
            Self::Sandboxed(sb) => {
                let e = sb.probe();
                (!e.confined()).then(|| format!("ran without a sandbox: {}", e.notes.join("; ")))
            }
        }
    }
}

/// Shared by the tools.
#[derive(Debug, Clone)]
pub struct Exec {
    pub mode: Mode,
    /// yah's data folder (holds the managed Python venv); `None` disables it.
    pub data_dir: Option<PathBuf>,
}

/// Register `run_command`, `run_python`, and — when `uv` is installed and there's a data
/// folder — `install_python_packages`.
pub fn register(reg: &mut Registry, exec: Exec) {
    let exec = Arc::new(exec);
    reg.register(Arc::new(RunCommand(Arc::clone(&exec))));
    reg.register(Arc::new(RunPython(Arc::clone(&exec))));
    if exec.data_dir.is_some() && python::which("uv".as_ref()).is_some() {
        reg.register(Arc::new(InstallPythonPackages(exec)));
    }
}

/// One sandboxed invocation.
struct Job {
    program: OsString,
    args: Vec<OsString>,
    network: bool,
    timeout: Duration,
    read: Vec<PathBuf>,
    write: Vec<PathBuf>,
    env: Vec<(String, OsString)>,
}

impl Exec {
    /// Run `job` with `tmp` as its private temp folder.
    fn run(&self, job: Job, cwd: &Path, tmp: &Path, ctx: &ToolContext<'_>) -> Result<Outcome, String> {
        let secs = job.timeout.as_secs().max(1);
        let mut cmd = match &self.mode {
            Mode::Sandboxed(sb) => {
                let mut policy = Policy::for_workspace(cwd, tmp)
                    .network(job.network)
                    .limits(Limits::for_timeout(secs))
                    // Where the OS can't confine, run anyway (warned at startup and in the output).
                    .allow_unsandboxed(true)
                    .allow_path_dirs(std::env::var_os("PATH").as_deref());
                for p in job.read {
                    policy = policy.allow_read(p);
                }
                for p in job.write {
                    policy = policy.allow_write(p);
                }
                sb.command(&policy, &job.program, &job.args)
            }
            Mode::Unsandboxed => {
                let mut c = Command::new(&job.program);
                c.args(&job.args);
                c
            }
        };
        cmd.current_dir(cwd);
        yah_sandbox::clean_env(&mut cmd, tmp);
        cmd.envs(job.env);
        let opts = RunOptions { timeout: job.timeout, max_output: MAX_OUTPUT, ..Default::default() };
        let out = yah_sandbox::run(cmd, &opts, &|| ctx.cancelled())?;
        match out.sandbox_error() {
            Some(e) => Err(format!(
                "the sandbox couldn't start: {e}. Nothing ran. (The user can set {}=off to run commands without a sandbox.)",
                yah_sandbox::DISABLE_ENV
            )),
            None => Ok(out),
        }
    }

    /// Run in the workspace and describe the result (output + changed files) for the model.
    fn run_in_workspace(&self, mut job: Job, prepare: impl FnOnce(&Path) -> Result<Vec<OsString>, String>, ctx: &ToolContext<'_>) -> ToolOutput {
        if ctx.cancelled() {
            return ToolOutput::err("Stopped by the user.");
        }
        let root = match yah_tools::sandbox::root(ctx.workspace) {
            Ok(r) => r,
            Err(e) => return ToolOutput::err(e),
        };
        let tmp = match TempDir::new("yah-run-") {
            Ok(t) => t,
            Err(e) => return ToolOutput::err(e),
        };
        match prepare(tmp.path()) {
            Ok(extra) => job.args.extend(extra),
            Err(e) => return ToolOutput::err(e),
        }
        let timeout = job.timeout;
        let before = Snapshot::take(&root);
        let out = match self.run(job, &root, tmp.path(), ctx) {
            Ok(o) => o,
            Err(e) => return ToolOutput::err(e),
        };
        let changes = before.changes(&Snapshot::take(&root));
        describe(&out, &changes, &root, timeout, self.mode.unconfined_note())
    }
}

fn describe(out: &Outcome, changes: &yah_sandbox::Changes, root: &Path, timeout: Duration, unconfined: Option<String>) -> ToolOutput {
    let mut text = String::new();
    if out.cancelled {
        text.push_str("Stopped by the user.\n");
    } else if out.timed_out {
        text.push_str(&format!("Timed out after {} s and was stopped (with everything it started).\n", timeout.as_secs()));
    } else {
        match (out.exit_code, out.signal) {
            (Some(c), _) => text.push_str(&format!("Exit code: {c}\n")),
            (None, Some(s)) => text.push_str(&format!("Killed by signal {s}\n")),
            (None, None) => text.push_str("Exit code: unknown\n"),
        }
    }
    for (name, cap) in [("stdout", &out.stdout), ("stderr", &out.stderr)] {
        let t = cap.text();
        if !t.trim().is_empty() {
            text.push_str(&format!("\n{name}:\n{}\n", t.trim_end()));
        }
    }
    let rel = |p: &PathBuf| p.strip_prefix(root).unwrap_or(p).display().to_string();
    for (label, list) in [("Files created", &changes.created), ("Files modified", &changes.modified), ("Files deleted", &changes.deleted)] {
        if list.is_empty() {
            continue;
        }
        let mut names: Vec<String> = list.iter().take(MAX_LISTED).map(rel).collect();
        if list.len() > MAX_LISTED {
            names.push(format!("… and {} more", list.len() - MAX_LISTED));
        }
        text.push_str(&format!("\n{label}: {}\n", names.join(", ")));
    }
    if out.stdout.text().trim().is_empty() && out.stderr.text().trim().is_empty() && changes.is_empty() && out.success() {
        text.push_str("(no output)\n");
    }
    if let Some(note) = unconfined {
        text.push_str(&format!("\n({note})\n"));
    }
    let mut fc: Vec<FileChange> = changes.created.iter().cloned().map(FileChange::Created).collect();
    fc.extend(changes.modified.iter().cloned().map(FileChange::Modified));
    fc.extend(changes.deleted.iter().cloned().map(FileChange::Deleted));
    ToolOutput { text: text.trim_end().to_owned(), failed: !out.success(), changes: fc }
}

fn timeout_of(secs: Option<f64>) -> Duration {
    let s = secs.filter(|s| s.is_finite() && *s > 0.0).unwrap_or(DEFAULT_TIMEOUT_S as f64);
    Duration::from_secs_f64(s.min(MAX_TIMEOUT_S as f64).max(1.0))
}

/// One line, at most `max` chars, for titles.
fn short(s: &str, max: usize) -> String {
    let line = s.trim().lines().next().unwrap_or_default();
    let mut out: String = line.chars().filter(|c| !c.is_control()).take(max).collect();
    if line.chars().count() > max || s.trim().lines().nth(1).is_some() {
        out.push('…');
    }
    out
}

fn with_network(title: String, args: &Value) -> String {
    if args.get("network").and_then(Value::as_bool) == Some(true) { format!("{title} — with internet access") } else { title }
}

const NETWORK_DOC: &str = "Allow internet access for this run (off by default; the user is asked). Only set it when the task needs to download something.";

/// `run_command`: a shell command in the workspace.
pub struct RunCommand(Arc<Exec>);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandArgs {
    command: String,
    #[serde(default)]
    timeout_s: Option<f64>,
    #[serde(default)]
    network: bool,
}

impl Tool for RunCommand {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_command".into(),
            description: "Run a shell command (`sh -c`) in the workspace folder and get its exit code, output and the files it created or changed. \
                Sandboxed: it can read system files and read/write only the workspace (plus a private temp folder); no network unless `network` is true. \
                Use it to inspect, search, convert or process files (e.g. `wc -l data.csv`, `head -5 data.csv`, `ls -R`). No interactive programs."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The shell command, e.g. `ls -la` or `grep -c error log.txt`."},
                    "timeout_s": {"type": "number", "description": format!("Stop it after this many seconds (default {DEFAULT_TIMEOUT_S}, max {MAX_TIMEOUT_S}).")},
                    "network": {"type": "boolean", "description": NETWORK_DOC}
                },
                "required": ["command"]
            }),
        }
    }

    fn title(&self, args: &Value) -> String {
        let cmd = args.get("command").and_then(Value::as_str).unwrap_or_default();
        with_network(format!("Run `{}`", short(cmd, 60)), args)
    }

    fn access(&self, _args: &Value) -> Access {
        Access::Execute
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        let a: CommandArgs = match parse_args(args) {
            Ok(a) => a,
            Err(e) => return ToolOutput::err(e),
        };
        if a.command.trim().is_empty() || a.command.len() > MAX_COMMAND_LEN || a.command.contains('\0') {
            return ToolOutput::err(format!("the command must be non-empty text of at most {MAX_COMMAND_LEN} bytes"));
        }
        let job = Job {
            program: "sh".into(),
            args: vec!["-c".into(), a.command.into()],
            network: a.network,
            timeout: timeout_of(a.timeout_s),
            read: Vec::new(),
            write: Vec::new(),
            env: Vec::new(),
        };
        self.0.run_in_workspace(job, |_| Ok(Vec::new()), ctx)
    }
}

/// `run_python`: Python code (or a workspace script) in the workspace.
pub struct RunPython(Arc<Exec>);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PythonArgs {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    script: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    timeout_s: Option<f64>,
    #[serde(default)]
    network: bool,
}

impl Tool for RunPython {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_python".into(),
            description:
                "Run Python 3 code with the workspace as the current folder (so `pd.read_csv('data.csv')` works) and get its output and the files it created. \
                Use it to analyse data (pandas, numpy if installed) and to draw charts with matplotlib: save each figure as a PNG in the workspace, \
                e.g. `plt.savefig('sales.png', dpi=150, bbox_inches='tight')` — saved images are shown to the user. `print` the numbers you need. \
                Same sandbox as run_command (workspace only, no network unless `network` is true)."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "code": {"type": "string", "description": "Python source to run. Give this or `script`."},
                    "script": {"type": "string", "description": "A .py file in the workspace to run instead of `code`."},
                    "args": {"type": "array", "items": {"type": "string"}, "description": "Command-line arguments (sys.argv[1:])."},
                    "timeout_s": {"type": "number", "description": format!("Stop it after this many seconds (default {DEFAULT_TIMEOUT_S}, max {MAX_TIMEOUT_S}).")},
                    "network": {"type": "boolean", "description": NETWORK_DOC}
                }
            }),
        }
    }

    fn title(&self, args: &Value) -> String {
        let t = match args.get("script").and_then(Value::as_str) {
            Some(s) => format!("Run {}", short(s, 60)),
            None => "Run Python code".into(),
        };
        with_network(t, args)
    }

    fn access(&self, _args: &Value) -> Access {
        Access::Execute
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        let a: PythonArgs = match parse_args(args) {
            Ok(a) => a,
            Err(e) => return ToolOutput::err(e),
        };
        if a.args.len() > MAX_ARGS || a.args.iter().any(|s| s.contains('\0') || s.len() > 4096) {
            return ToolOutput::err(format!("at most {MAX_ARGS} arguments, each under 4096 bytes"));
        }
        let Some(py) = Python::resolve(self.0.data_dir.as_deref()) else {
            return ToolOutput::err(format!("No Python found: install python3 (or set {}).", python::PYTHON_ENV));
        };
        let script = match (a.code, a.script) {
            (Some(code), None) if code.len() <= MAX_CODE_LEN && !code.contains('\0') => Source::Code(code),
            (Some(_), None) => return ToolOutput::err(format!("code must be text of at most {MAX_CODE_LEN} bytes")),
            (None, Some(s)) => match yah_tools::sandbox::resolve_follow(ctx.workspace, &s) {
                Ok(r) if r.abs.is_file() => Source::Script(r.abs),
                Ok(r) => return ToolOutput::err(format!("{} is not a file in the workspace", r.show())),
                Err(e) => return ToolOutput::err(e),
            },
            _ => return ToolOutput::err("give exactly one of `code` or `script`"),
        };
        let user_args: Vec<OsString> = a.args.into_iter().map(OsString::from).collect();
        let job = Job {
            program: py.path.clone().into_os_string(),
            args: Vec::new(),
            network: a.network,
            timeout: timeout_of(a.timeout_s),
            read: py.read_paths(),
            write: Vec::new(),
            env: Vec::new(),
        };
        self.0.run_in_workspace(
            job,
            move |tmp| {
                let path = match script {
                    Source::Script(p) => p,
                    Source::Code(code) => {
                        let p = tmp.join("main.py");
                        std::fs::write(&p, code).map_err(|e| format!("can't write the code to a temp file: {e}"))?;
                        p
                    }
                };
                let mut v = vec![path.into_os_string()];
                v.extend(user_args);
                Ok(v)
            },
            ctx,
        )
    }
}

enum Source {
    Code(String),
    Script(PathBuf),
}

/// `install_python_packages`: add packages to yah's managed venv (created on first use)
/// with `uv`. Wheels only (`--only-binary :all:`), so installing never runs package code.
pub struct InstallPythonPackages(Arc<Exec>);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallArgs {
    packages: Vec<String>,
}

impl Tool for InstallPythonPackages {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "install_python_packages".into(),
            description: "Install Python packages (from PyPI, prebuilt wheels only) into yah's own Python environment, which run_python then uses. \
                Use it when run_python fails with ModuleNotFoundError, e.g. packages: [\"pandas\", \"matplotlib\"]. Needs internet; the user is asked."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "packages": {"type": "array", "items": {"type": "string"}, "description": "Package names, optionally with versions, e.g. \"pandas\" or \"numpy>=2\"."}
                },
                "required": ["packages"]
            }),
        }
    }

    fn title(&self, args: &Value) -> String {
        let names: Vec<&str> = args.get("packages").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).take(6).collect()).unwrap_or_default();
        format!("Install Python packages: {}", short(&names.join(", "), 80))
    }

    fn access(&self, _args: &Value) -> Access {
        Access::Network
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        let a: InstallArgs = match parse_args(args) {
            Ok(a) => a,
            Err(e) => return ToolOutput::err(e),
        };
        if a.packages.is_empty() || a.packages.len() > MAX_PACKAGES {
            return ToolOutput::err(format!("give 1 to {MAX_PACKAGES} packages"));
        }
        if let Some(bad) = a.packages.iter().find(|p| !python::valid_requirement(p)) {
            return ToolOutput::err(format!("{bad:?} is not a package name (like \"pandas\" or \"numpy>=2\")"));
        }
        let Some(data) = self.0.data_dir.as_deref() else { return ToolOutput::err("yah has no data folder for a Python environment") };
        match install(&self.0, data, &a.packages, ctx) {
            Ok(text) => ToolOutput::ok(text),
            Err(e) => ToolOutput::err(e),
        }
    }
}

/// Create the managed venv if needed and install `packages` into it. Usable by the app
/// directly (e.g. a "set up Python for data analysis" button). Runs in the sandbox with
/// network on, writing only under `<data>/python`.
pub fn install(exec: &Exec, data_dir: &Path, packages: &[String], ctx: &ToolContext<'_>) -> Result<String, String> {
    let uv = python::which("uv".as_ref()).ok_or("uv is not installed (https://docs.astral.sh/uv/); it's needed to install Python packages")?;
    let root = python::managed_root(data_dir);
    std::fs::create_dir_all(&root).map_err(|e| format!("can't create {}: {e}", root.display()))?;
    let root = root.canonicalize().map_err(|e| format!("{}: {e}", root.display()))?;
    let venv = root.join("venv");
    let tmp = TempDir::new("yah-install-")?;
    let mut read: Vec<PathBuf> = uv.canonicalize().ok().and_then(|p| p.parent().map(Path::to_path_buf)).into_iter().collect();
    let base = python::system_python();
    if let Some(b) = &base {
        read.extend(Python { path: b.clone(), managed: false }.read_paths());
    }
    let env = vec![
        ("UV_CACHE_DIR".to_owned(), root.join("cache").into_os_string()),
        ("UV_PYTHON_INSTALL_DIR".to_owned(), root.join("interpreters").into_os_string()),
        ("UV_NO_CONFIG".to_owned(), "1".into()),
    ];
    let job = |args: Vec<OsString>| Job {
        program: uv.clone().into_os_string(),
        args,
        network: true,
        timeout: INSTALL_TIMEOUT,
        read: read.clone(),
        write: vec![root.clone()],
        env: env.clone(),
    };
    let mut log = String::new();
    let python_bin = python::venv_python(&venv);
    if !python_bin.exists() {
        let mut args: Vec<OsString> = vec!["venv".into(), "--quiet".into()];
        if let Some(b) = &base {
            args.extend(["--python".into(), b.clone().into_os_string()]);
        }
        args.push(venv.clone().into_os_string());
        let out = exec.run(job(args), &root, tmp.path(), ctx)?;
        if !out.success() {
            return Err(format!("creating the Python environment failed:\n{}", out.stderr.text().trim()));
        }
        log.push_str(&format!("Created a Python environment in {}.\n", venv.display()));
    }
    let mut args: Vec<OsString> = vec!["pip".into(), "install".into(), "--only-binary".into(), ":all:".into(), "--python".into(), python_bin.into_os_string()];
    args.extend(packages.iter().map(OsString::from));
    let out = exec.run(job(args), &root, tmp.path(), ctx)?;
    let detail = [out.stdout.text(), out.stderr.text()].iter().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n");
    if out.cancelled {
        return Err("Stopped by the user.".into());
    }
    if !out.success() {
        return Err(format!("installing failed:\n{detail}"));
    }
    log.push_str(&format!("Installed {}. run_python now uses this environment.\n{detail}", packages.join(", ")));
    Ok(log.trim_end().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exec() -> Arc<Exec> {
        Arc::new(Exec { mode: Mode::Unsandboxed, data_dir: None })
    }

    #[test]
    fn titles_show_network_and_stay_short() {
        let t = RunCommand(exec());
        assert_eq!(t.title(&json!({"command": "ls -la"})), "Run `ls -la`");
        assert_eq!(t.title(&json!({"command": "curl x", "network": true})), "Run `curl x` — with internet access");
        assert!(t.title(&json!({"command": "a\nb"})).ends_with("…`"));
        assert!(t.title(&json!({"command": "x".repeat(500)})).chars().count() < 80);
        assert_eq!(t.title(&json!(null)), "Run ``");
        let p = RunPython(exec());
        assert_eq!(p.title(&json!({"code": "print(1)"})), "Run Python code");
        assert_eq!(p.title(&json!({"script": "a.py", "network": true})), "Run a.py — with internet access");
        assert_eq!(InstallPythonPackages(exec()).title(&json!({"packages": ["pandas", "numpy"]})), "Install Python packages: pandas, numpy");
    }

    #[test]
    fn access_levels() {
        assert_eq!(RunCommand(exec()).access(&json!({})), Access::Execute);
        assert_eq!(RunPython(exec()).access(&json!({})), Access::Execute);
        assert_eq!(InstallPythonPackages(exec()).access(&json!({})), Access::Network);
    }

    #[test]
    fn timeouts_are_clamped() {
        assert_eq!(timeout_of(None), Duration::from_secs(DEFAULT_TIMEOUT_S));
        assert_eq!(timeout_of(Some(1e9)), Duration::from_secs(MAX_TIMEOUT_S));
        assert_eq!(timeout_of(Some(-3.0)), Duration::from_secs(DEFAULT_TIMEOUT_S));
        assert_eq!(timeout_of(Some(f64::NAN)), Duration::from_secs(DEFAULT_TIMEOUT_S));
        assert_eq!(timeout_of(Some(0.01)), Duration::from_secs(1));
    }

    #[test]
    fn bad_arguments_are_refused_before_running() {
        let d = tempfile::tempdir().unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let ctx = ToolContext::new(d.path(), &cancel);
        let e = exec();
        for args in [json!({}), json!({"command": ""}), json!({"command": 1}), json!({"command": "ls", "extra": 1})] {
            assert!(RunCommand(Arc::clone(&e)).run(&args, &ctx).failed, "{args}");
        }
        for args in [json!({}), json!({"code": "1", "script": "a.py"}), json!({"script": "../x.py"}), json!({"script": "missing.py"})] {
            let out = RunPython(Arc::clone(&e)).run(&args, &ctx);
            assert!(out.failed, "{args}");
        }
        for args in [json!({"packages": []}), json!({"packages": ["--index-url=http://evil"]}), json!({"packages": ["a b"]})] {
            assert!(InstallPythonPackages(Arc::clone(&e)).run(&args, &ctx).failed, "{args}");
        }
    }

    #[test]
    fn specs_are_valid() {
        let mut reg = Registry::default();
        register(&mut reg, Exec { mode: Mode::Unsandboxed, data_dir: None });
        assert!(reg.get("run_command").is_some() && reg.get("run_python").is_some());
        assert!(reg.get("install_python_packages").is_none(), "no data dir, no installer");
        for s in reg.specs() {
            assert_eq!(s.parameters["type"], "object");
        }
    }
}
