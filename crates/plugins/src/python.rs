//! A plugin's declared Python tools (`tools[].run`): the model's arguments go to the
//! script as JSON on stdin; what it prints (plain text, or `{"text": …, "files": […]}`)
//! goes back. Settings arrive as environment variables, plus `HOB_CHAT_DIR` and
//! `HOB_PLUGIN_DIR` (and `YAH_*` aliases).
//!
//! **Not sandboxed yet.** hob ran these in its Python sandbox; yah will once
//! `yah-sandbox` exists. Until then [`run_script_unsandboxed`] is a plain child process
//! with a timeout and bounded output, and the tools report [`Access::Execute`] so the
//! agent's permission gate asks.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;
use yah_tools::{Access, FileChange, Tool, ToolContext, ToolOutput, ToolSpec};

use crate::PluginRef;
use crate::discover::Plugin;
use crate::manifest::{ManifestTool, ToolKind, path_inside};
use crate::names::{ToolNames, namespaced};

/// hob's limit for a plugin tool.
pub const SCRIPT_TIMEOUT: Duration = Duration::from_secs(60);
/// Most bytes kept from each of stdout and stderr.
pub const MAX_SCRIPT_OUTPUT: usize = 256 * 1024;
/// Most files compared before/after a run to spot new ones.
const MAX_SCANNED_FILES: usize = 5000;
const POLL: Duration = Duration::from_millis(20);
/// How long to wait for output pipes after the process ended (a grandchild may hold them).
const DRAIN_GRACE: Duration = Duration::from_secs(2);

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

/// Run a script as a plain child process: **no sandbox**. Kills it on timeout or when
/// `cancelled()` turns true. `Err` only when it couldn't start.
pub fn run_script_unsandboxed(run: &ScriptRun, cancelled: &dyn Fn() -> bool) -> Result<ScriptOutcome, String> {
    let Some((program, lead)) = run.program.split_first() else { return Err("no interpreter configured".into()) };
    let mut child = Command::new(program)
        .args(lead)
        .arg(&run.script)
        .args(&run.args)
        .current_dir(&run.cwd)
        .envs(&run.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("couldn't start {}: {e}", program.to_string_lossy()))?;

    if let Some(mut stdin) = child.stdin.take() {
        let input = run.stdin.clone();
        // a script that never reads stdin must not block us: write on a thread
        thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
    }
    let (tx, rx) = mpsc::channel();
    let readers = [child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>), child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>)];
    for (i, r) in readers.into_iter().enumerate() {
        let Some(r) = r else { continue };
        let tx = tx.clone();
        let max = run.max_output;
        thread::spawn(move || {
            let _ = tx.send((i, read_bounded(r, max)));
        });
    }
    drop(tx);

    let start = Instant::now();
    let mut out = ScriptOutcome::default();
    let status: Option<ExitStatus> = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {}
            Err(e) => {
                log::warn!("waiting for script {}: {e}", run.script.display());
                let _ = child.kill();
                break child.wait().ok();
            }
        }
        if cancelled() {
            out.stopped = true;
        } else if start.elapsed() >= run.timeout {
            out.timed_out = true;
        }
        if out.stopped || out.timed_out {
            let _ = child.kill();
            break child.wait().ok();
        }
        thread::sleep(POLL);
    };
    out.exit_code = if out.stopped || out.timed_out { None } else { status.and_then(|s| s.code()) };

    let deadline = Instant::now() + DRAIN_GRACE;
    for _ in 0..2 {
        let left = deadline.saturating_duration_since(Instant::now());
        let Ok((i, (bytes, cut))) = rx.recv_timeout(left) else { break };
        out.truncated |= cut;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        if i == 0 {
            out.stdout = text;
        } else {
            out.stderr = text;
        }
    }
    Ok(out)
}

/// Read everything, keeping the first `max` bytes. Returns (kept, something was dropped).
fn read_bounded(mut r: impl Read, max: usize) -> (Vec<u8>, bool) {
    let mut kept = Vec::new();
    let mut cut = false;
    let mut buf = [0u8; 8192];
    loop {
        match r.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = max.saturating_sub(kept.len());
                let take = n.min(room);
                kept.extend_from_slice(&buf[..take]);
                cut |= take < n;
            }
        }
    }
    (kept, cut)
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
    timeout: Duration,
}

impl PythonTool {
    /// `decl` must be a script tool ([`ToolKind::Script`]); `None` otherwise.
    pub fn new(name: String, plugin: PluginRef, decl: ManifestTool, interpreter: Vec<OsString>) -> Option<Self> {
        let ToolKind::Script(script) = &decl.kind else { return None };
        let script = script.clone();
        Some(Self { name, plugin, decl, script, interpreter, timeout: SCRIPT_TIMEOUT })
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn plugin(&self) -> &PluginRef {
        &self.plugin
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
            None => format!("Used {}", self.plugin.name),
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
        for (k, v) in [("HOB_CHAT_DIR", &chat), ("YAH_CHAT_DIR", &chat), ("HOB_PLUGIN_DIR", &dir), ("YAH_PLUGIN_DIR", &dir)] {
            env.insert(k.into(), v.clone());
        }
        let run = ScriptRun {
            program: self.interpreter.clone(),
            script,
            args: Vec::new(),
            cwd: ctx.workspace.to_path_buf(),
            stdin: serde_json::to_vec(&args).unwrap_or_else(|_| b"{}".to_vec()),
            env,
            timeout: self.timeout,
            max_output: MAX_SCRIPT_OUTPUT,
        };
        let before = snapshot(ctx.workspace);
        match run_script_unsandboxed(&run, &|| ctx.cancelled()) {
            Ok(r) => self.finish(&r, ctx.workspace, &before),
            Err(e) => ToolOutput::err(format!("{} couldn't run: {e}", self.plugin.name)),
        }
    }
}

/// The Python tools `plugin` declares, named as declared (so the plugin's `prompt` still
/// matches) or `<plugin>__<tool>` when that name is taken. Tools whose script is missing
/// are left out (the plugin's `problems` say so).
pub fn python_tools(plugin: &Plugin, settings: &BTreeMap<String, String>, interpreter: &[OsString], names: &mut ToolNames) -> Vec<Arc<PythonTool>> {
    let r = PluginRef::new(plugin, settings);
    plugin
        .manifest
        .tools
        .iter()
        .filter(|t| matches!(&t.kind, ToolKind::Script(s) if plugin.dir.join(s).is_file()))
        .filter_map(|t| {
            let name = names.claim(&t.name, &namespaced(plugin.id(), &t.name));
            PythonTool::new(name, r.clone(), t.clone(), interpreter.to_vec()).map(Arc::new)
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
    fn bounded_reads() {
        let (kept, cut) = read_bounded(&b"hello world"[..], 5);
        assert_eq!((kept.as_slice(), cut), (&b"hello"[..], true));
        let (kept, cut) = read_bounded(&b"hi"[..], 5);
        assert_eq!((kept.as_slice(), cut), (&b"hi"[..], false));
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
