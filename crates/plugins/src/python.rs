//! A plugin's declared Python tools (`tools[].run`): the model's arguments go to the
//! script as JSON on stdin; what it prints (plain text, or `{"text": …, "files": […]}`)
//! goes back. Settings arrive as environment variables, plus `HOB_CHAT_DIR` and
//! `HOB_PLUGIN_DIR` (and `BYOB_*` aliases).
//!
//! Given a [`Sandbox`] ([`PythonTool::with_sandbox`]), scripts run confined by
//! `byob-sandbox` ([`run_script_sandboxed`]: workspace + temp writable, plugin folder
//! readable, network only if the plugin declares `permissions.network`). Without one
//! they're a plain child process ([`run_script_unsandboxed`]). Either way: timeout,
//! bounded output, process-group kill, and [`Access::Execute`] so the permission gate asks.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use byob_sandbox::{Limits, Policy, RunOptions, Sandbox, TempDir};
use byob_tools::{Access, FileChange, Tool, ToolContext, ToolOutput, ToolSpec};
use serde_json::Value;

use crate::PluginRef;
use crate::discover::Plugin;
use crate::manifest::{ManifestTool, ToolKind, path_inside};
use crate::names::{ToolNames, namespaced};
use crate::pyenv::PluginPython;

/// hob's limit for a plugin tool.
pub const SCRIPT_TIMEOUT: Duration = Duration::from_secs(60);
/// Most bytes kept from each of stdout and stderr.
pub const MAX_SCRIPT_OUTPUT: usize = 256 * 1024;
/// Most files compared before/after a run to spot new ones.
const MAX_SCANNED_FILES: usize = 5000;

/// The interpreter a Python tool runs with, as a command prefix.
pub fn default_python() -> Vec<OsString> {
    vec![OsString::from(if cfg!(windows) { "python" } else { "python3" })]
}

/// One script run.
#[derive(Debug, Clone)]
pub struct ScriptRun {
    /// Interpreter and its leading arguments, e.g. `["python3"]`; the script path follows.
    pub program: Vec<OsString>,
    pub script: PathBuf,
    /// Extra arguments after the script.
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub stdin: Vec<u8>,
    /// Added to the inherited environment.
    pub env: BTreeMap<String, String>,
    pub timeout: Duration,
    /// Per stream; the rest is read and dropped.
    pub max_output: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScriptOutcome {
    pub stdout: String,
    pub stderr: String,
    /// `None` when killed or ended by a signal.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stopped: bool,
    /// Some output was dropped for being over `max_output`.
    pub truncated: bool,
}

impl ScriptOutcome {
    pub fn success(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out && !self.stopped
    }
}

/// Run a script as a plain child process: **no sandbox**. Kills it (and its process
/// group) on timeout or when `cancelled()` turns true. `Err` only when it couldn't start.
pub fn run_script_unsandboxed(run: &ScriptRun, cancelled: &dyn Fn() -> bool) -> Result<ScriptOutcome, String> {
    let Some((program, lead)) = run.program.split_first() else { return Err("no interpreter configured".into()) };
    let mut cmd = Command::new(program);
    cmd.args(lead).arg(&run.script).args(&run.args).current_dir(&run.cwd).envs(&run.env);
    execute(cmd, run, cancelled)
}

/// Run a script in `sandbox`: read the system, the plugin folder and the interpreter;
/// read+write `run.cwd` (the workspace) and a private temp folder; network only if
/// `network`. The environment is reset to a minimal one plus `run.env`.
pub fn run_script_sandboxed(
    run: &ScriptRun,
    sandbox: &Sandbox,
    plugin_dir: &Path,
    network: bool,
    cancelled: &dyn Fn() -> bool,
) -> Result<ScriptOutcome, String> {
    run_script_sandboxed_reading(run, sandbox, &[plugin_dir.to_path_buf()], network, cancelled)
}

/// [`run_script_sandboxed`] that may also read `read` (the plugin folder, its venv, byob's
/// helper module).
pub fn run_script_sandboxed_reading(
    run: &ScriptRun,
    sandbox: &Sandbox,
    read: &[PathBuf],
    network: bool,
    cancelled: &dyn Fn() -> bool,
) -> Result<ScriptOutcome, String> {
    let Some((program, lead)) = run.program.split_first() else { return Err("no interpreter configured".into()) };
    let cwd = run.cwd.canonicalize().map_err(|e| format!("{}: {e}", run.cwd.display()))?;
    let tmp = TempDir::new("byob-plugin-")?;
    let mut policy = Policy::for_workspace(&cwd, tmp.path())
        .network(network)
        .limits(Limits::for_timeout(run.timeout.as_secs()))
        .allow_path_dirs(std::env::var_os("PATH").as_deref())
        // Where the OS can't confine, run anyway (as before the sandbox existed).
        .allow_unsandboxed(true);
    for p in read {
        policy = policy.allow_read(p);
    }
    for p in byob_sandbox::interpreter_read_paths(program) {
        policy = policy.allow_read(p);
    }
    let mut args: Vec<OsString> = lead.to_vec();
    args.push(run.script.clone().into_os_string());
    args.extend(run.args.iter().cloned());
    let mut cmd = sandbox.command(&policy, program, &args);
    cmd.current_dir(&cwd);
    byob_sandbox::clean_env(&mut cmd, tmp.path());
    cmd.envs(&run.env);
    let out = execute(cmd, run, cancelled)?;
    drop(tmp);
    Ok(out)
}

fn execute(cmd: Command, run: &ScriptRun, cancelled: &dyn Fn() -> bool) -> Result<ScriptOutcome, String> {
    let opts = RunOptions { stdin: run.stdin.clone(), timeout: run.timeout, max_output: run.max_output };
    let o = byob_sandbox::run(cmd, &opts, cancelled)?;
    if let Some(e) = o.sandbox_error() {
        return Err(format!("the sandbox couldn't start: {e}"));
    }
    Ok(ScriptOutcome {
        stdout: String::from_utf8_lossy(&o.stdout.bytes()).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr.bytes()).into_owned(),
        exit_code: o.exit_code,
        timed_out: o.timed_out,
        stopped: o.cancelled,
        truncated: o.stdout.truncated() || o.stderr.truncated(),
    })
}

/// `"Converted {amount} {from}"` + args → `"Converted 5 EUR"`; unknown keys stay.
pub fn fill_title(template: &str, args: &Value) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let key_len = after.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(after.len());
        if key_len > 0 && after[key_len..].starts_with('}') {
            let key = &after[..key_len];
            match args.get(key) {
                Some(Value::String(s)) => out.push_str(s),
                Some(v) => out.push_str(&v.to_string()),
                None => out.push_str(&rest[open..open + key_len + 2]),
            }
            rest = &after[key_len + 1..];
        } else {
            out.push('{');
            rest = after;
        }
    }
    out.push_str(rest);
    out.chars().take(120).collect()
}

/// Files under `dir` (two levels down, dot files skipped) with their modification times.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Option<SystemTime>> {
    let mut m = BTreeMap::new();
    let mut stack = vec![(dir.to_path_buf(), 0u8)];
    while let Some((d, depth)) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            if m.len() >= MAX_SCANNED_FILES {
                return m;
            }
            if e.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_dir() {
                if depth < 2 {
                    stack.push((e.path(), depth + 1));
                }
            } else {
                m.insert(e.path(), meta.modified().ok());
            }
        }
    }
    m
}

/// A plugin's Python tool.
pub struct PythonTool {
    name: String,
    plugin: PluginRef,
    decl: ManifestTool,
    script: String,
    interpreter: Vec<OsString>,
    /// The plugin's own Python (helper module, requirements venv); beats `interpreter`.
    python: Option<PluginPython>,
    sandbox: Option<Sandbox>,
    timeout: Duration,
}

impl PythonTool {
    /// `decl` must be a script tool ([`ToolKind::Script`]); `None` otherwise.
    pub fn new(name: String, plugin: PluginRef, decl: ManifestTool, interpreter: Vec<OsString>) -> Option<Self> {
        let ToolKind::Script(script) = &decl.kind else { return None };
        let script = script.clone();
        Some(Self { name, plugin, decl, script, interpreter, python: None, sandbox: None, timeout: SCRIPT_TIMEOUT })
    }

    /// Run with the plugin's own Python (built on first use if it has requirements).
    pub fn with_python(mut self, python: Option<PluginPython>) -> Self {
        self.python = python;
        self
    }

    /// Run the script in `sandbox` (see [`run_script_sandboxed`]); `None`: unsandboxed.
    pub fn with_sandbox(mut self, sandbox: Option<Sandbox>) -> Self {
        self.sandbox = sandbox;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn plugin(&self) -> &PluginRef {
        &self.plugin
    }

    /// The tool as the manifest declares it (its `name` may differ from the name the model
    /// sees when that one was taken).
    pub fn declared(&self) -> &ManifestTool {
        &self.decl
    }

    fn finish(&self, r: &ScriptOutcome, workspace: &Path, before: &BTreeMap<PathBuf, Option<SystemTime>>) -> ToolOutput {
        let name = &self.plugin.name;
        if r.stopped {
            return ToolOutput::err("Stopped by the user.");
        }
        if r.timed_out {
            return ToolOutput::err(format!("{name} took longer than {} seconds and was stopped.", self.timeout.as_secs()));
        }
        if r.exit_code != Some(0) {
            let output = [r.stdout.trim(), r.stderr.trim()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join("\n");
            return ToolOutput::err(format!("{name} ran into a problem:\n{output}"));
        }
        let out = r.stdout.trim();
        let mut text = out.to_string();
        let mut listed: Vec<PathBuf> = Vec::new();
        if out.starts_with('{')
            && let Ok(Value::Object(j)) = serde_json::from_str::<Value>(out)
        {
            if let Some(t) = j.get("text").and_then(Value::as_str) {
                text = t.to_string();
            }
            if let Some(files) = j.get("files").and_then(Value::as_array) {
                listed = files.iter().filter_map(Value::as_str).filter(|f| path_inside(f)).map(|f| workspace.join(f)).filter(|p| p.is_file()).collect();
            }
        }
        if r.truncated {
            text.push_str("\n…(output cut: it was too long)");
        }
        let after = snapshot(workspace);
        let mut changes: Vec<FileChange> = after
            .iter()
            .filter(|(f, m)| before.get(*f) != Some(*m))
            .map(|(f, _)| if before.contains_key(f) { FileChange::Modified(f.clone()) } else { FileChange::Created(f.clone()) })
            .collect();
        for f in listed {
            if !changes.iter().any(|c| matches!(c, FileChange::Created(p) | FileChange::Modified(p) if *p == f)) {
                changes.push(FileChange::Modified(f));
            }
        }
        if text.is_empty() {
            text = "(no result)".into();
        }
        ToolOutput { text, failed: false, changes }
    }
}

impl Tool for PythonTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec { name: self.name.clone(), description: self.decl.description.clone(), parameters: self.decl.params.clone() }
    }

    fn title(&self, args: &Value) -> String {
        match &self.decl.title {
            Some(t) => fill_title(t, args),
            None => format!("Used {}: {}", self.plugin.name, self.decl.name),
        }
    }

    fn access(&self, _args: &Value) -> Access {
        Access::Execute
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        if ctx.cancelled() {
            return ToolOutput::err("Stopped by the user.");
        }
        let script = self.plugin.dir.join(&self.script);
        if !path_inside(&self.script) || !script.is_file() {
            return ToolOutput::err(format!("{}: {} is missing", self.plugin.name, self.script));
        }
        let args = if args.is_object() { args.clone() } else { Value::Object(serde_json::Map::new()) };
        let chat = ctx.workspace.to_string_lossy().into_owned();
        let dir = self.plugin.dir.to_string_lossy().into_owned();
        let mut env = self.plugin.env.clone();
        for (k, v) in [("HOB_CHAT_DIR", &chat), ("BYOB_CHAT_DIR", &chat), ("HOB_PLUGIN_DIR", &dir), ("BYOB_PLUGIN_DIR", &dir)] {
            env.insert(k.into(), v.clone());
        }
        let mut read = vec![self.plugin.dir.clone()];
        let program = match &self.python {
            None => self.interpreter.clone(),
            Some(p) => match p.python() {
                Ok(py) => {
                    read.extend(py.read);
                    env.extend(py.env);
                    py.program
                }
                Err(e) => return ToolOutput::err(format!("{} couldn't run: {e}", self.plugin.name)),
            },
        };
        let run = ScriptRun {
            program,
            script,
            args: Vec::new(),
            cwd: ctx.workspace.to_path_buf(),
            stdin: serde_json::to_vec(&args).unwrap_or_else(|_| b"{}".to_vec()),
            env,
            timeout: self.timeout,
            max_output: MAX_SCRIPT_OUTPUT,
        };
        let before = snapshot(ctx.workspace);
        let result = match &self.sandbox {
            Some(sb) => run_script_sandboxed_reading(&run, sb, &read, self.plugin.network, &|| ctx.cancelled()),
            None => run_script_unsandboxed(&run, &|| ctx.cancelled()),
        };
        match result {
            Ok(r) => self.finish(&r, ctx.workspace, &before),
            Err(e) => ToolOutput::err(format!("{} couldn't run: {e}", self.plugin.name)),
        }
    }
}

/// The Python tools `plugin` declares, named as declared (so the plugin's `prompt` still
/// matches) or `<plugin>__<tool>` when that name is taken. Tools whose script is missing
/// are left out (the plugin's `problems` say so).
pub fn python_tools(plugin: &Plugin, settings: &BTreeMap<String, String>, interpreter: &[OsString], names: &mut ToolNames) -> Vec<Arc<PythonTool>> {
    python_tools_in(plugin, settings, interpreter, names, None)
}

/// [`python_tools`] whose scripts run in `sandbox` (when given).
pub fn python_tools_in(
    plugin: &Plugin,
    settings: &BTreeMap<String, String>,
    interpreter: &[OsString],
    names: &mut ToolNames,
    sandbox: Option<&Sandbox>,
) -> Vec<Arc<PythonTool>> {
    python_tools_with(plugin, settings, interpreter, names, sandbox, None)
}

/// [`python_tools_in`] running with the plugin's own Python (`python`, see [`PluginPython`]).
pub fn python_tools_with(
    plugin: &Plugin,
    settings: &BTreeMap<String, String>,
    interpreter: &[OsString],
    names: &mut ToolNames,
    sandbox: Option<&Sandbox>,
    python: Option<&PluginPython>,
) -> Vec<Arc<PythonTool>> {
    let r = PluginRef::new(plugin, settings);
    plugin
        .manifest
        .tools
        .iter()
        .filter(|t| matches!(&t.kind, ToolKind::Script(s) if plugin.dir.join(s).is_file()))
        .filter_map(|t| {
            let name = names.claim(&t.name, &namespaced(plugin.id(), &t.name));
            PythonTool::new(name, r.clone(), t.clone(), interpreter.to_vec()).map(|t| Arc::new(t.with_sandbox(sandbox.cloned()).with_python(python.cloned())))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn titles() {
        let a = json!({"amount": 5, "from": "EUR", "x": null});
        assert_eq!(fill_title("Converted {amount} {from} {to} {x}", &a), "Converted 5 EUR {to} null");
        assert_eq!(fill_title("{ not a key } {", &a), "{ not a key } {");
        assert_eq!(fill_title(&"é".repeat(200), &a).chars().count(), 120);
    }

    #[test]
    fn missing_interpreter_is_an_error() {
        let run = ScriptRun {
            program: vec![OsString::from("/definitely/not/an/interpreter")],
            script: PathBuf::from("x"),
            args: vec![],
            cwd: std::env::temp_dir(),
            stdin: vec![],
            env: BTreeMap::new(),
            timeout: SCRIPT_TIMEOUT,
            max_output: 10,
        };
        assert!(run_script_unsandboxed(&run, &|| false).unwrap_err().starts_with("couldn't start"));
        let empty = ScriptRun { program: vec![], ..run };
        assert!(run_script_unsandboxed(&empty, &|| false).is_err());
    }
}
