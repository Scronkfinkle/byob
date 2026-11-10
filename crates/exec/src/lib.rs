//! Tools that run code: [`RunCommand`] (`run_command`, a shell command), [`RunPython`]
//! (`run_python`, Python code or a script), [`InstallPackages`] (`install_packages`,
//! programs and Python libraries for the workspace, with Nix or pixi, or in the workspace's
//! container), [`InstallPythonPackages`] (`install_python_packages`, into byob's managed
//! venv with `uv`, where there is no package manager for `install_packages`) and
//! [`InstallSystemPackages`] (`install_system_packages`, Homebrew/winget, the last resort a
//! person approves every time).
//!
//! Every run is prepared by [`env::Envs`]: it runs in the workspace's container if the user
//! opted in; otherwise installed packages, the workspace's `.venv` and (once the user agreed)
//! its dev shell go on `PATH`, and the `nix` CLI works inside the sandbox.
//!
//! Everything runs through [`byob_sandbox`]: read the system, read+write the workspace and
//! a private temp folder, no network unless the call asks (and the user approves), a
//! minimal environment without inherited secrets, wall-clock timeout, bounded output,
//! and the whole process tree is killed on timeout or Stop. Where the OS can't sandbox
//! (no Landlock), commands still run, with a warning in the log and in the tool output;
//! `BYOB_SANDBOX=off` turns the sandbox off explicitly ([`Mode::Unsandboxed`]).
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

pub mod env;
pub mod python;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use env::{Envs, Layer, Runner, hints};

use byob_sandbox::{Limits, Outcome, Policy, RunOptions, Sandbox, Snapshot, TempDir};
use byob_tools::{Access, FileChange, Registry, Tool, ToolContext, ToolOutput, ToolSpec, parse_args};
use serde::Deserialize;
use serde_json::{Value, json};

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
    /// The user turned the sandbox off (`BYOB_SANDBOX=off`): plain child processes, still
    /// with the clean environment, timeout, output caps and group kill.
    Unsandboxed,
}

impl Mode {
    /// `Unsandboxed` if the user opted out (logged), else the sandbox via this binary.
    /// Logs what the sandbox can enforce here (a warning if it can't confine anything).
    pub fn from_env() -> Result<Self, String> {
        if byob_sandbox::disabled_by_user() {
            log::warn!("{}=off: commands and Python run WITHOUT a sandbox", byob_sandbox::DISABLE_ENV);
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
            Self::Unsandboxed => Some(format!("ran without a sandbox: {}=off", byob_sandbox::DISABLE_ENV)),
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
    /// byob's data folder (holds the managed Python venv and installed packages); `None`
    /// disables them.
    pub data_dir: Option<PathBuf>,
    /// Package managers and workspace environments ([`Envs::none`] for neither).
    pub envs: Arc<Envs>,
}

impl Exec {
    /// Detects the package managers on this machine.
    pub fn new(mode: Mode, data_dir: Option<PathBuf>) -> Self {
        let envs = Arc::new(Envs::detect(data_dir.as_deref()));
        Self { mode, data_dir, envs }
    }

    /// Like [`Exec::new`], with the container settings (default image; `docker` or `podman`).
    pub fn with_containers(mode: Mode, data_dir: Option<PathBuf>, image: Option<String>, runtime: Option<&str>) -> Self {
        let found = env::Found::detect_with(data_dir.as_deref(), image, runtime);
        let envs = Arc::new(Envs::new(data_dir.as_deref(), found, env::ProjectMode::from_env()));
        Self { mode, data_dir, envs }
    }

    /// What the system prompt should say about getting software, if anything.
    pub fn prompt(&self) -> Option<String> {
        self.envs.prompt()
    }

    fn fix(&self) -> hints::Fix {
        if self.envs.installer().is_some() {
            hints::Fix::Packages
        } else if self.data_dir.is_some() && python::which("uv".as_ref()).is_some() {
            hints::Fix::PythonOnly
        } else {
            hints::Fix::None
        }
    }
}

/// Register `run_command`, `run_python`, and — when there's a data folder — either
/// `install_packages` (a package manager like Nix is installed) or, failing that,
/// `install_python_packages` (`uv` is installed).
pub fn register(reg: &mut Registry, exec: Exec) {
    let exec = Arc::new(exec);
    reg.register(Arc::new(RunCommand(Arc::clone(&exec))));
    reg.register(Arc::new(RunPython(Arc::clone(&exec))));
    match exec.fix() {
        hints::Fix::Packages => reg.register(Arc::new(InstallPackages(Arc::clone(&exec)))),
        hints::Fix::PythonOnly => reg.register(Arc::new(InstallPythonPackages(Arc::clone(&exec)))),
        hints::Fix::None => {}
    }
    // Workspaces that run in a container install there.
    if exec.envs.containers().is_some() && reg.get("install_packages").is_none() {
        reg.register(Arc::new(InstallPackages(Arc::clone(&exec))));
    }
    if exec.envs.system().is_some() {
        reg.register(Arc::new(InstallSystemPackages(exec)));
    }
}

/// One sandboxed invocation.
pub(crate) struct Job {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub network: bool,
    pub timeout: Duration,
    /// `PATH` entries, variables and sandbox access on top of the clean environment.
    pub layer: Layer,
    /// Variables set last (after the layer's).
    pub env: Vec<(String, OsString)>,
    /// No address-space limit: package managers (pixi) map far more than they use.
    pub big_memory: bool,
}

impl Job {
    pub fn new(program: OsString, args: Vec<OsString>, timeout: Duration) -> Self {
        Self { program, args, network: false, timeout, layer: Layer::default(), env: Vec::new(), big_memory: false }
    }
}

/// The variables [`byob_sandbox::clean_env`] sets for a run with temp folder `tmp`.
pub(crate) fn clean_env_vars(tmp: &Path) -> Vec<(String, String)> {
    let mut cmd = Command::new("true");
    byob_sandbox::clean_env(&mut cmd, tmp);
    cmd.get_envs().filter_map(|(k, v)| Some((k.to_str()?.to_owned(), v?.to_str()?.to_owned()))).collect()
}

/// Runs provider jobs (dev shell loading, installs) in the tool call's sandbox.
struct CallRunner<'a> {
    exec: &'a Exec,
    ctx: &'a ToolContext<'a>,
}

impl Runner for CallRunner<'_> {
    fn run(&self, job: Job, cwd: &Path, tmp: &Path) -> Result<Outcome, String> {
        self.exec.run(job, cwd, tmp, self.ctx)
    }

    fn ask(&self, q: &byob_tools::Question) -> byob_tools::Answer {
        self.ctx.ask(q)
    }

    fn cancelled(&self) -> bool {
        self.ctx.cancelled()
    }

    fn plan(&self) -> byob_tools::EnvPlan {
        self.ctx.env()
    }

    fn chat(&self) -> Option<String> {
        self.ctx.chat().map(str::to_owned)
    }
}

impl Exec {
    /// Run `job` with `tmp` as its private temp folder.
    fn run(&self, job: Job, cwd: &Path, tmp: &Path, ctx: &ToolContext<'_>) -> Result<Outcome, String> {
        let secs = job.timeout.as_secs().max(1);
        let mut layer = job.layer;
        layer.stack(&self.envs.support());
        let host_path = std::env::var_os("PATH");
        let path = (!layer.path.is_empty()).then(|| env::join_path(&layer.path, host_path.as_deref()));
        let mut cmd = match &self.mode {
            Mode::Sandboxed(sb) => {
                let mut policy = Policy::for_workspace(cwd, tmp)
                    .network(job.network)
                    .limits(Limits { memory_bytes: if job.big_memory { None } else { Limits::for_timeout(secs).memory_bytes }, ..Limits::for_timeout(secs) })
                    // Where the OS can't confine, run anyway (warned at startup and in the output).
                    .allow_unsandboxed(true)
                    .allow_path_dirs(path.as_deref().or(host_path.as_deref()));
                for p in &layer.read {
                    policy = policy.allow_read(p);
                }
                for p in &layer.write {
                    policy = policy.allow_write(p);
                }
                for p in &layer.sockets {
                    policy = policy.allow_unix_socket(p);
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
        byob_sandbox::clean_env(&mut cmd, tmp);
        if let Some(p) = path {
            cmd.env("PATH", p);
        }
        for (k, v) in &layer.vars {
            if !env::keep_var(k) {
                cmd.env(k, v);
            }
        }
        cmd.envs(job.env);
        let opts = RunOptions { timeout: job.timeout, max_output: MAX_OUTPUT, ..Default::default() };
        let out = byob_sandbox::run(cmd, &opts, &|| ctx.cancelled())?;
        match out.sandbox_error() {
            Some(e) => Err(format!(
                "the sandbox couldn't start: {e}. Nothing ran. (The user can set {}=off to run commands without a sandbox.)",
                byob_sandbox::DISABLE_ENV
            )),
            None => Ok(out),
        }
    }

    /// Run in the workspace and describe the result (output + changed files) for the model.
    /// `build` makes the job from the workspace environment's layer; `prepare` adds
    /// arguments once the run's temp folder exists.
    fn run_in_workspace(
        &self,
        build: impl FnOnce(&Layer, Where) -> Result<Job, String>,
        prepare: impl FnOnce(&Path) -> Result<Vec<OsString>, String>,
        command: Option<&str>,
        ctx: &ToolContext<'_>,
    ) -> ToolOutput {
        if ctx.cancelled() {
            return ToolOutput::err("Stopped by the user.");
        }
        let root = match byob_tools::sandbox::root(ctx.workspace) {
            Ok(r) => r,
            Err(e) => return ToolOutput::err(e),
        };
        let mut notes = Vec::new();
        if let Some(ready) = self.envs.container(&root, &CallRunner { exec: self, ctx }, &mut notes) {
            let out = match ready {
                Ok(r) => self.run_in_container(&r, build, prepare, command, &root, ctx),
                Err(e) => ToolOutput::err(e),
            };
            return with_notes(out, &notes);
        }
        let mut prepared = self.envs.prepare(&root, &CallRunner { exec: self, ctx });
        notes.append(&mut prepared.notes);
        prepared.notes = notes;
        if ctx.cancelled() {
            return ToolOutput::err("Stopped by the user.");
        }
        let mut job = match build(&prepared.layer, Where::Host) {
            Ok(j) => j,
            Err(e) => return with_notes(ToolOutput::err(e), &prepared.notes),
        };
        job.layer.stack(&prepared.layer);
        let tmp = match TempDir::new("byob-run-") {
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
        let mut res = describe(&out, &changes, &root, timeout, self.mode.unconfined_note());
        if res.failed
            && let Some(h) =
                hints::hint(&out.stdout.text(), &out.stderr.text(), command, self.fix(), self.envs.system().map(env::system::SystemInstaller::name))
        {
            res.text.push_str(&format!("\n\n{h}"));
        }
        with_notes(res, &prepared.notes)
    }
}

impl Exec {
    /// [`Exec::run_in_workspace`] inside the workspace's container.
    fn run_in_container(
        &self,
        ready: &env::container::Ready,
        build: impl FnOnce(&Layer, Where) -> Result<Job, String>,
        prepare: impl FnOnce(&Path) -> Result<Vec<OsString>, String>,
        command: Option<&str>,
        root: &Path,
        ctx: &ToolContext<'_>,
    ) -> ToolOutput {
        let Some(containers) = self.envs.containers() else { return ToolOutput::err("no container runtime") };
        let job = match build(&Layer::default(), Where::Container) {
            Ok(j) => j,
            Err(e) => return ToolOutput::err(e),
        };
        let tmp = match env::container::RunTmp::new(&ready.tmp_base) {
            Ok(t) => t,
            Err(e) => return ToolOutput::err(e),
        };
        let mut argv = vec![job.program];
        argv.extend(job.args);
        match prepare(tmp.path()) {
            Ok(extra) => argv.extend(extra),
            Err(e) => return ToolOutput::err(e),
        }
        let timeout = job.timeout;
        let run = env::container::Exec { argv, env: job.env, network: job.network, timeout };
        let before = Snapshot::take(root);
        let (out, problem) = match containers.exec(ready, run, root, tmp.path(), &|| ctx.cancelled()) {
            Ok(o) => o,
            Err(e) => return ToolOutput::err(e),
        };
        let changes = before.changes(&Snapshot::take(root));
        let mut res = describe(&out, &changes, root, timeout, problem);
        if res.failed
            && let Some(h) = hints::hint(&out.stdout.text(), &out.stderr.text(), command, hints::Fix::Packages, None)
        {
            res.text.push_str(&format!("\n\n{h}"));
        }
        res
    }
}

/// Where a job runs: here (in the sandbox) or in the workspace's container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Where {
    Host,
    Container,
}

/// Put the environment's one-time notes in front of a tool's output.
fn with_notes(mut out: ToolOutput, notes: &[String]) -> ToolOutput {
    if !notes.is_empty() {
        out.text = format!("({})\n\n{}", notes.join(")\n("), out.text);
    }
    out
}

fn describe(out: &Outcome, changes: &byob_sandbox::Changes, root: &Path, timeout: Duration, unconfined: Option<String>) -> ToolOutput {
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
        // Nix's warning in a run without network is misleading: the daemon still downloads.
        let t = if name == "stderr" { t.replace(env::nix::OFFLINE_WARNING, "").trim_start_matches('\n').to_owned() } else { t };
        if !t.trim().is_empty() {
            text.push_str(&format!("\n{name}:\n{}\n", t.trim_end()));
        }
    }
    let rel = |p: &PathBuf| byob_tools::sandbox::slash(p.strip_prefix(root).unwrap_or(p));
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
    if !text.contains("\nstdout:\n") && !text.contains("\nstderr:\n") && changes.is_empty() && out.success() {
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
            description: format!(
                "Run a shell command (`sh -c`) in the workspace folder and get its exit code, output and the files it created or changed. \
                Sandboxed: it can read system files and read/write only the workspace (plus a private temp folder); no network unless `network` is true. \
                Use it to inspect, search, convert or process files (e.g. `wc -l data.csv`, `head -5 data.csv`, `ls -R`). No interactive programs.{}",
                self.0.envs.installer().map_or(String::new(), |_| {
                    " Packages from install_packages are on PATH, and so is the workspace's own environment (flake.nix, pixi.toml, …) once the user agrees to load it. \
                    If a program is missing, call install_packages instead of pip, apt or sudo."
                        .into()
                })
            ),
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

    fn pending_title(&self, args: &Value) -> String {
        self.title(args)
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
        let command = a.command.clone();
        let build = |_: &Layer, _: Where| {
            let mut job = Job::new("sh".into(), vec!["-c".into(), a.command.into()], timeout_of(a.timeout_s));
            job.network = a.network;
            Ok(job)
        };
        self.0.run_in_workspace(build, |_| Ok(Vec::new()), Some(&command), ctx)
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

    fn pending_title(&self, args: &Value) -> String {
        self.title(args)
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
        let script = match (a.code, a.script) {
            (Some(code), None) if code.len() <= MAX_CODE_LEN && !code.contains('\0') => Source::Code(code),
            (Some(_), None) => return ToolOutput::err(format!("code must be text of at most {MAX_CODE_LEN} bytes")),
            (None, Some(s)) => match byob_tools::sandbox::resolve_follow(ctx.workspace, &s) {
                Ok(r) if r.abs.is_file() => Source::Script(r.abs),
                Ok(r) => return ToolOutput::err(format!("{} is not a file in the workspace", r.show())),
                Err(e) => return ToolOutput::err(e),
            },
            _ => return ToolOutput::err("give exactly one of `code` or `script`"),
        };
        let user_args: Vec<OsString> = a.args.into_iter().map(OsString::from).collect();
        let data_dir = self.0.data_dir.clone();
        let fix = self.0.fix();
        let build = move |layer: &Layer, at: Where| {
            if at == Where::Container {
                let mut job = Job::new("python3".into(), Vec::new(), timeout_of(a.timeout_s));
                job.network = a.network;
                return Ok(job);
            }
            let Some(py) = Python::resolve_in(data_dir.as_deref(), &layer.path) else {
                return Err(match fix {
                    hints::Fix::Packages => "No Python found. Call install_packages with {\"packages\": [\"python3\"]} (add the libraries you need, e.g. \"matplotlib\"), then try again.".to_owned(),
                    _ => format!("No Python found: install python3 (or set {}).", python::PYTHON_ENV),
                });
            };
            let mut job = Job::new(py.path.clone().into_os_string(), Vec::new(), timeout_of(a.timeout_s));
            job.network = a.network;
            job.layer.read = py.read_paths();
            Ok(job)
        };
        self.0.run_in_workspace(
            build,
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
            None,
            ctx,
        )
    }
}

enum Source {
    Code(String),
    Script(PathBuf),
}

/// `install_python_packages`: add packages to byob's managed venv (created on first use)
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
            description: "Install Python packages (from PyPI, prebuilt wheels only) into byob's own Python environment, which run_python then uses. \
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

    fn pending_title(&self, args: &Value) -> String {
        self.title(args)
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
        let Some(data) = self.0.data_dir.as_deref() else { return ToolOutput::err("byob has no data folder for a Python environment") };
        match install(&self.0, data, &a.packages, ctx) {
            Ok(text) => ToolOutput::ok(text),
            Err(e) => ToolOutput::err(e),
        }
    }
}

/// `install_packages`: programs and Python libraries for this workspace, from the machine's
/// package manager (Nix). They stay installed (per workspace) and are on `PATH` for every
/// later `run_command` / `run_python`.
pub struct InstallPackages(Arc<Exec>);

impl Tool for InstallPackages {
    fn spec(&self) -> ToolSpec {
        let by = self.0.envs.installer().unwrap_or("the package manager");
        ToolSpec {
            name: "install_packages".into(),
            description: format!(
                "Install programs and Python libraries for this workspace with {by}, so run_command and run_python can use them. \
                 Use it whenever a command is \"not found\" or Python says ModuleNotFoundError — not pip install, apt, brew or sudo, which don't work here. \
                 Give package names: programs like \"jq\", \"ffmpeg\", \"imagemagick\", \"ripgrep\", \"nodejs\", \"pandoc\"; Python libraries by their pip name, \
                 like \"matplotlib\", \"pandas\", \"scikit-learn\" (they go into run_python's python3). Packages stay installed for this workspace. \
                 Needs internet; the user is asked."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "packages": {"type": "array", "items": {"type": "string"}, "description": "Package names, e.g. [\"jq\", \"matplotlib\"]."}
                },
                "required": ["packages"]
            }),
        }
    }

    fn title(&self, args: &Value) -> String {
        let names: Vec<&str> = args.get("packages").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).take(6).collect()).unwrap_or_default();
        format!("Install packages: {}", short(&names.join(", "), 80))
    }

    fn pending_title(&self, args: &Value) -> String {
        self.title(args)
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
        let root = match byob_tools::sandbox::root(ctx.workspace) {
            Ok(r) => r,
            Err(e) => return ToolOutput::err(e),
        };
        match self.0.envs.install(&root, &a.packages, &CallRunner { exec: &self.0, ctx }) {
            Ok(done) => ToolOutput { text: done.text, failed: done.failed, changes: Vec::new() },
            Err(e) => ToolOutput::err(e),
        }
    }
}

/// `install_system_packages`: the last resort, Homebrew or winget, system-wide. Every call
/// needs a person's approval (it is never auto-approved), and while there is a workspace
/// installer it only takes packages `install_packages` couldn't find.
pub struct InstallSystemPackages(Arc<Exec>);

impl InstallSystemPackages {
    fn names(args: &Value) -> Vec<String> {
        args.get("packages")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).take(MAX_PACKAGES).map(str::to_owned).collect())
            .unwrap_or_default()
    }

    fn by(&self) -> &'static str {
        self.0.envs.system().map_or("the system package manager", env::system::SystemInstaller::name)
    }
}

impl Tool for InstallSystemPackages {
    fn spec(&self) -> ToolSpec {
        let by = self.by();
        let first = match self.0.envs.installer() {
            Some(_) => "Only for packages install_packages couldn't find: always try install_packages first.",
            None => "Only when nothing else works.",
        };
        ToolSpec {
            name: "install_system_packages".into(),
            description: format!(
                "Last resort: install programs SYSTEM-WIDE with {by}, outside the workspace and the sandbox. {first} \
                 The user must approve every call and byob can't undo it, so say why you need it."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "packages": {"type": "array", "items": {"type": "string"}, "description": format!("{by} package names, e.g. [\"jq\"].")}
                },
                "required": ["packages"]
            }),
        }
    }

    fn title(&self, args: &Value) -> String {
        format!("Install system-wide with {}: {}", self.by(), short(&Self::names(args).join(", "), 80))
    }

    fn pending_title(&self, args: &Value) -> String {
        self.title(args)
    }

    fn access(&self, _args: &Value) -> Access {
        Access::Execute
    }

    fn always_ask(&self, args: &Value) -> Option<String> {
        let names = Self::names(args);
        Some(self.0.envs.system().map_or_else(|| "Installs system-wide.".to_owned(), |s| s.warning(&names)))
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        let a: InstallArgs = match parse_args(args) {
            Ok(a) => a,
            Err(e) => return ToolOutput::err(e),
        };
        if a.packages.is_empty() || a.packages.len() > MAX_PACKAGES {
            return ToolOutput::err(format!("give 1 to {MAX_PACKAGES} packages"));
        }
        let Some(system) = self.0.envs.system() else { return ToolOutput::err("no system package manager here") };
        let root = match byob_tools::sandbox::root(ctx.workspace) {
            Ok(r) => r,
            Err(e) => return ToolOutput::err(e),
        };
        if let Err(e) = self.0.envs.system_allowed(&root, &a.packages, &ctx.env()) {
            return ToolOutput::err(e);
        }
        match system.install(&a.packages, &|| ctx.cancelled()) {
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
    let tmp = TempDir::new("byob-install-")?;
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
    let job = |args: Vec<OsString>| {
        let mut j = Job::new(uv.clone().into_os_string(), args, INSTALL_TIMEOUT);
        j.network = true;
        j.layer.read = read.clone();
        j.layer.write = vec![root.clone()];
        j.env = env.clone();
        j
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
        Arc::new(Exec { mode: Mode::Unsandboxed, data_dir: None, envs: Arc::new(Envs::none()) })
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

    /// A fake `brew` that logs its arguments next to itself and fails for `bad-pkg`.
    #[cfg(unix)]
    fn fake_brew(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let brew = bin.join("brew");
        std::fs::write(
            &brew,
            "#!/bin/sh\necho \"$@\" >> \"$0.log\"\ncase \"$*\" in *bad-pkg*) echo 'Error: No formula' >&2; exit 1;; esac\necho \"==> Pouring $2\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&brew, std::fs::Permissions::from_mode(0o755)).unwrap();
        brew
    }

    #[cfg(unix)]
    #[test]
    fn system_installs_are_a_confirmed_last_resort() {
        use env::system::{Kind, SystemInstaller};
        let d = tempfile::tempdir().unwrap();
        let brew = fake_brew(d.path());
        let found = env::Found { system: Some(SystemInstaller { kind: Kind::Brew, program: brew.clone() }), ..Default::default() };
        let envs = Arc::new(Envs::new(Some(d.path()), found, env::ProjectMode::Ask));
        let mut reg = Registry::default();
        register(&mut reg, Exec { mode: Mode::Unsandboxed, data_dir: Some(d.path().to_path_buf()), envs });
        let tool = reg.get("install_system_packages").expect("offered where brew exists");
        let args = json!({"packages": ["jq"]});
        let warning = tool.always_ask(&args).expect("every call needs a person");
        assert!(warning.contains("SYSTEM-WIDE") && warning.contains("brew uninstall jq"), "{warning}");
        assert_eq!(tool.title(&args), "Install system-wide with Homebrew: jq");
        assert!(tool.spec().description.contains("Last resort"));

        let ws = tempfile::tempdir().unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let ctx = ToolContext::new(ws.path(), &cancel);
        // No workspace installer here (no Nix, no pixi): it may install anything.
        let out = tool.run(&args, &ctx);
        assert!(!out.failed && out.text.contains("Installed system-wide with Homebrew: jq"), "{}", out.text);
        assert_eq!(std::fs::read_to_string(brew.with_extension("log")).unwrap(), "install jq\n");
        let out = tool.run(&json!({"packages": ["--force"]}), &ctx);
        assert!(out.failed && out.text.contains("not a package name"), "{}", out.text);
        let out = tool.run(&json!({"packages": ["bad-pkg"]}), &ctx);
        assert!(out.failed && out.text.contains("No formula"), "{}", out.text);
        assert_eq!(std::fs::read_to_string(brew.with_extension("log")).unwrap().lines().count(), 2, "the bad name never reached brew");
        // A failed run names the last resort next to pixi.
        let h = hints::hint("", "sh: 1: jq: not found", None, hints::Fix::None, Some("Homebrew")).unwrap();
        assert!(h.contains("install_system_packages"), "{h}");
    }

    /// With a workspace installer (a fake pixi that finds nothing), the system installer
    /// only takes what install_packages reported missing.
    #[cfg(unix)]
    #[test]
    fn system_installs_wait_for_the_workspace_installer() {
        use env::system::{Kind, SystemInstaller};
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let brew = fake_brew(d.path());
        let pixi = d.path().join("bin/pixi");
        std::fs::write(&pixi, "#!/bin/sh\nfor a; do last=$a; done\nprintf 'Error: Cannot solve the request because of: No candidates were found for\\n    %s *.\\n' \"$last\" >&2\nexit 1\n").unwrap();
        std::fs::set_permissions(&pixi, std::fs::Permissions::from_mode(0o755)).unwrap();
        let found = env::Found { pixi: Some(pixi), system: Some(SystemInstaller { kind: Kind::Brew, program: brew.clone() }), ..Default::default() };
        let envs = Arc::new(Envs::new(Some(d.path()), found, env::ProjectMode::Ask));
        assert_eq!(envs.installer(), Some("pixi"));
        let mut reg = Registry::default();
        register(&mut reg, Exec { mode: Mode::Unsandboxed, data_dir: Some(d.path().to_path_buf()), envs });
        assert!(reg.get("install_python_packages").is_none());
        let ws = tempfile::tempdir().unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let ctx = ToolContext::new(ws.path(), &cancel);
        let system = reg.get("install_system_packages").unwrap();
        assert!(system.spec().description.contains("try install_packages first"));
        let out = system.run(&json!({"packages": ["sometool"]}), &ctx);
        assert!(out.failed && out.text.contains("Call install_packages with {\"packages\":[\"sometool\"]} first"), "{}", out.text);
        assert!(!brew.with_extension("log").exists());
        let out = reg.get("install_packages").unwrap().run(&json!({"packages": ["sometool"]}), &ctx);
        assert!(out.failed && out.text.contains("Not found on conda-forge: \"sometool\""), "{}", out.text);
        assert!(out.text.contains("install_system_packages can install [\"sometool\"] system-wide with Homebrew"), "{}", out.text);
        let out = system.run(&json!({"packages": ["sometool"]}), &ctx);
        assert!(!out.failed, "{}", out.text);
        assert_eq!(std::fs::read_to_string(brew.with_extension("log")).unwrap(), "install sometool\n");
        assert!(std::fs::read_dir(ws.path()).unwrap().next().is_none(), "the workspace is untouched");
    }

    /// Answers every question with `.0`.
    struct Says(byob_tools::Answer);
    impl byob_tools::Asker for Says {
        fn ask(&self, _q: &byob_tools::Question) -> byob_tools::Answer {
            self.0
        }
    }

    /// A workspace with containers through a fake `docker` whose daemon is down.
    #[cfg(unix)]
    fn fake_docker_world(d: &Path, runtime: bool) -> (Registry, Arc<Envs>, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let docker = d.join("bin/docker");
        std::fs::create_dir_all(d.join("bin")).unwrap();
        std::fs::write(&docker, "#!/bin/sh\necho \"$@\" >> \"$0.log\"\necho 'Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?' >&2\nexit 1\n").unwrap();
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
        let rt = env::container::Runtime { kind: env::container::RuntimeKind::Docker, program: docker.clone() };
        let found = env::Found { containers: runtime.then_some(rt), ..Default::default() };
        let envs = Arc::new(Envs::new(Some(d), found, env::ProjectMode::Ask));
        let mut reg = Registry::default();
        register(&mut reg, Exec { mode: Mode::Unsandboxed, data_dir: Some(d.to_path_buf()), envs: Arc::clone(&envs) });
        (reg, envs, docker.with_extension("log"))
    }

    #[cfg(unix)]
    #[test]
    fn container_problems_are_clear_and_nothing_runs() {
        use byob_tools::EnvControl;
        let d = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let (reg, envs, log) = fake_docker_world(d.path(), true);
        assert!(reg.get("install_packages").is_some(), "containers install there");
        let run = |reg: &Registry, answer| {
            let says = Says(answer);
            let ctx = ToolContext::new(&ws, &cancel).with_asker(&says);
            reg.get("run_command").unwrap().run(&json!({"command": "touch ran.txt"}), &ctx)
        };
        // A devcontainer.json the user declines: runs here, no docker call, told once.
        std::fs::write(ws.join(".devcontainer.json"), r#"{"image": "alpine"}"#).unwrap();
        let out = run(&reg, byob_tools::Answer::No);
        assert!(!out.failed && out.text.contains("chose to run commands on their machine"), "{}", out.text);
        assert!(!log.exists(), "docker wasn't called");
        assert!(!run(&reg, byob_tools::Answer::Yes).text.contains("chose to run"), "told once, and the answer stuck");
        assert_eq!(envs.describe(&ws, &byob_tools::EnvPlan::default()).container.use_container, Some(false));
        std::fs::remove_file(ws.join("ran.txt")).unwrap();
        // Turned on, but the daemon is down: a clear error, nothing ran.
        envs.apply(&ws, &byob_protocol::EnvAction::Container { on: Some(true) }).unwrap();
        let out = run(&reg, byob_tools::Answer::Yes);
        assert!(out.failed && out.text.contains("Docker isn't running") && out.text.contains("Cannot connect"), "{}", out.text);
        assert!(!ws.join("ran.txt").exists());
        let d = envs.describe(&ws, &byob_tools::EnvPlan::default()).container;
        assert_eq!((d.runtime.as_deref(), d.use_container, d.state, d.image.as_str()), (Some("docker"), Some(true), None, "alpine"));
        // Turned on where there's no runtime at all.
        let d2 = tempfile::tempdir().unwrap();
        let (reg2, envs2, _) = fake_docker_world(d2.path(), false);
        envs2.apply(&ws, &byob_protocol::EnvAction::Container { on: Some(true) }).unwrap();
        let out = run(&reg2, byob_tools::Answer::Yes);
        assert!(out.failed && out.text.contains("neither Docker nor Podman is installed"), "{}", out.text);
        assert!(envs2.apply(&ws, &byob_protocol::EnvAction::StopContainer).is_err());
    }

    /// The workspace's (or chat's) environment setting decides where commands run, with no
    /// question, and names the image; the folder's own answers apply only without one.
    #[cfg(unix)]
    #[test]
    fn environment_settings_decide_without_asking() {
        use byob_tools::{EnvControl, EnvPlan};
        let d = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        std::fs::write(ws.join(".devcontainer.json"), r#"{"image": "alpine"}"#).unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let (reg, envs, log) = fake_docker_world(d.path(), true);
        /// Fails the test if anything is asked.
        struct NoQuestions;
        impl byob_tools::Asker for NoQuestions {
            fn ask(&self, q: &byob_tools::Question) -> byob_tools::Answer {
                panic!("asked {q:?}")
            }
        }
        let run = |plan: &EnvPlan| {
            let ctx = ToolContext::new(&ws, &cancel).with_asker(&NoQuestions).with_env(plan).with_chat("c1");
            reg.get("run_command").unwrap().run(&json!({"command": "echo hi"}), &ctx)
        };
        // Container: straight to docker (whose daemon is down here), with the chosen image.
        let container = EnvPlan { container: Some(true), image: Some("debian:stable-slim".into()), ..EnvPlan::default() };
        let out = run(&container);
        assert!(out.failed && out.text.contains("Docker isn't running"), "{}", out.text);
        assert!(std::fs::read_to_string(&log).unwrap().contains("info"), "docker was asked");
        let shown = envs.describe(&ws, &container).container;
        assert_eq!((shown.use_container, shown.image.as_str()), (Some(true), "debian:stable-slim"));
        // A chat that runs on this machine: no docker, no devcontainer question.
        std::fs::remove_file(&log).unwrap();
        let host = EnvPlan { container: Some(false), ..EnvPlan::default() };
        let out = run(&host);
        assert!(!out.failed && out.text.contains("hi") && out.text.contains("chose to run commands on their machine"), "{}", out.text);
        assert!(!log.exists(), "docker wasn't called");
        // The folder's own answer is untouched by settings.
        assert_eq!(envs.describe(&ws, &EnvPlan::default()).container.use_container, None);
        // Forgetting the workspace clears what byob kept for it.
        envs.apply(&ws, &byob_protocol::EnvAction::Container { on: Some(false) }).unwrap();
        assert!(envs.forget(&ws).unwrap().contains("forgot its answers"));
        assert_eq!(envs.describe(&ws, &EnvPlan::default()).container.use_container, None);
        assert_eq!(EnvControl::support(&*envs).runtime.as_deref(), Some("docker"));
        assert!(envs.check_containers().unwrap_err().contains("Docker isn't running"));
    }

    #[test]
    fn specs_are_valid() {
        let mut reg = Registry::default();
        register(&mut reg, Exec { mode: Mode::Unsandboxed, data_dir: None, envs: Arc::new(Envs::none()) });
        assert!(reg.get("run_command").is_some() && reg.get("run_python").is_some());
        assert!(reg.get("install_python_packages").is_none(), "no data dir, no installer");
        for s in reg.specs() {
            assert_eq!(s.parameters["type"], "object");
        }
    }
}
