//! Project environments from tools that print their environment as JSON, so they work the
//! same on Linux, macOS and Windows:
//!
//! - **pixi** (`pixi.toml`, or `pyproject.toml` with `[tool.pixi…]`): conda-forge packages
//!   in `<ws>/.pixi`; `pixi shell-hook --json` installs the environment if needed and prints
//!   `{"environment_variables": {…}}`.
//! - **mise** (`mise.toml`, `.mise.toml`, `.tool-versions`): language runtimes and CLI tools;
//!   `mise install` then `mise env --json` (`{"PATH": …, …}`). The workspace's config is
//!   trusted for these runs only (`MISE_TRUSTED_CONFIG_PATHS`).
//!
//! Both only run when their CLI is on `PATH`. Their downloads go to `<data>/<tool>/` so they
//! survive the per-run temp home. Checked against pixi 0.68 and mise 2026.5 output.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use super::{Layer, Project, Runner, STOPPED, keep_var};
use crate::Job;

const LOAD_TIMEOUT: Duration = Duration::from_secs(900);
/// Largest environment JSON read back.
const MAX_JSON: u64 = 4 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Pixi,
    Mise,
}

/// A project tool found on this machine.
#[derive(Debug, Clone)]
pub struct JsonEnvTool {
    kind: Kind,
    program: PathBuf,
    /// `<data>/pixi` or `<data>/mise`.
    state: Option<PathBuf>,
}

impl JsonEnvTool {
    /// `program` is the tool's binary; its downloads go to `<data>/pixi` or `<data>/mise`.
    pub fn new(kind: Kind, program: PathBuf, data_dir: Option<&Path>) -> Self {
        let name = match kind {
            Kind::Pixi => "pixi",
            Kind::Mise => "mise",
        };
        Self { kind, program, state: data_dir.map(|d| d.join(name)) }
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    /// Variables that keep the tool's downloads in byob's data folder.
    fn vars(&self, ws: &Path) -> Vec<(String, OsString)> {
        let Some(s) = &self.state else { return Vec::new() };
        let mut v: Vec<(String, OsString)> = match self.kind {
            Kind::Pixi => vec![("PIXI_CACHE_DIR".into(), s.join("cache").into()), ("RATTLER_CACHE_DIR".into(), s.join("cache").into())],
            Kind::Mise => vec![("MISE_DATA_DIR".into(), s.join("data").into()), ("MISE_CACHE_DIR".into(), s.join("cache").into())],
        };
        if self.kind == Kind::Mise {
            v.push(("MISE_TRUSTED_CONFIG_PATHS".into(), ws.into()));
            v.push(("MISE_YES".into(), "1".into()));
        }
        v
    }

    /// Run the tool with `args`; with `capture`, its stdout goes to a file (not the
    /// bounded tool output) and is returned.
    fn run_tool(&self, args: &[&str], ws: &Path, run: &dyn Runner, capture: bool) -> Result<Option<String>, String> {
        let mut write = vec![ws.to_path_buf()];
        write.extend(self.state.clone());
        let t = ToolRun { program: &self.program, name: self.label_cmd(), cwd: ws, write, env: self.vars(ws) };
        run_tool(&t, args, run, capture)
    }

    fn label_cmd(&self) -> &'static str {
        match self.kind {
            Kind::Pixi => "pixi",
            Kind::Mise => "mise",
        }
    }
}

/// One sandboxed, networked run of a package tool (pixi, mise).
pub(crate) struct ToolRun<'a> {
    pub program: &'a Path,
    /// For messages: "pixi".
    pub name: &'a str,
    pub cwd: &'a Path,
    /// Folders it may write (created if missing).
    pub write: Vec<PathBuf>,
    pub env: Vec<(String, OsString)>,
}

/// Run `t` with `args`; with `capture`, its stdout goes to a file (not the bounded tool
/// output) and is returned. A failure's error is the tool's stderr.
pub(crate) fn run_tool(t: &ToolRun<'_>, args: &[&str], run: &dyn Runner, capture: bool) -> Result<Option<String>, String> {
    let tmp = byob_sandbox::TempDir::new("byob-env-")?;
    let out_file = tmp.path().join("env.json");
    // `sh -c 'exec "$@" > "$BYOB_OUT"' sh tool args…`: the file avoids the output cap.
    let mut argv: Vec<OsString> = vec!["-c".into(), if capture { r#"exec "$@" > "$BYOB_OUT""# } else { r#"exec "$@""# }.into(), "sh".into()];
    argv.push(t.program.as_os_str().to_owned());
    argv.extend(args.iter().map(OsString::from));
    let mut job = Job::new("sh".into(), argv, LOAD_TIMEOUT);
    job.network = true;
    job.big_memory = true;
    for w in &t.write {
        std::fs::create_dir_all(w).map_err(|e| format!("can't create {}: {e}", w.display()))?;
        job.layer.write.push(w.clone());
    }
    job.env.clone_from(&t.env);
    job.env.push(("BYOB_OUT".into(), out_file.clone().into_os_string()));
    let out = run.run(job, t.cwd, tmp.path())?;
    if out.cancelled {
        return Err(STOPPED.into());
    }
    if !out.success() {
        let err = out.stderr.text();
        return Err(format!("`{} {}` failed: {}", t.name, args.join(" "), err.trim()));
    }
    if !capture {
        return Ok(None);
    }
    let size = std::fs::metadata(&out_file).map(|m| m.len()).unwrap_or(0);
    if size > MAX_JSON {
        return Err(format!("the environment is too large ({size} bytes)"));
    }
    std::fs::read_to_string(&out_file).map(Some).map_err(|e| format!("no environment from {}: {e}", t.name))
}

impl Project for JsonEnvTool {
    fn id(&self) -> &'static str {
        self.label_cmd()
    }

    fn label(&self) -> &'static str {
        match self.kind {
            Kind::Pixi => "pixi environment",
            Kind::Mise => "mise tools",
        }
    }

    fn file(&self, ws: &Path) -> Option<&'static str> {
        project_file(self.kind, ws)
    }

    fn inputs(&self) -> &'static [&'static str] {
        match self.kind {
            Kind::Pixi => &["pixi.toml", "pixi.lock", "pyproject.toml"],
            Kind::Mise => &["mise.toml", ".mise.toml", ".tool-versions", "mise.lock"],
        }
    }

    fn load(&self, ws: &Path, _dir: &Path, run: &dyn Runner) -> Result<Layer, String> {
        let text = match self.kind {
            Kind::Pixi => self.run_tool(&["shell-hook", "--json"], ws, run, true)?,
            Kind::Mise => {
                self.run_tool(&["install"], ws, run, false)?;
                self.run_tool(&["env", "--json"], ws, run, true)?
            }
        };
        let host: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
        let mut layer = parse_env(self.kind, text.as_deref().unwrap_or_default(), &host)?;
        // Tools it installed outside the workspace (mise's data folder) load their libraries from there.
        layer.read.extend(self.state.clone());
        Ok(layer)
    }
}

/// The file that makes `ws` a project of this kind.
pub fn project_file(kind: Kind, ws: &Path) -> Option<&'static str> {
    match kind {
        Kind::Pixi => {
            if ws.join("pixi.toml").is_file() {
                return Some("pixi.toml");
            }
            let py = std::fs::read_to_string(ws.join("pyproject.toml")).ok()?;
            py.lines().any(|l| l.trim_start().starts_with("[tool.pixi")).then_some("pyproject.toml")
        }
        Kind::Mise => ["mise.toml", ".mise.toml", ".tool-versions"].into_iter().find(|f| ws.join(f).is_file()),
    }
}

/// The variables a tool printed, as a layer: `PATH` entries it put in front of the PATH it
/// was given (`host`), other variables except those the sandbox keeps ([`keep_var`]).
pub fn parse_env(kind: Kind, text: &str, host: &[PathBuf]) -> Result<Layer, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("bad environment JSON: {e}"))?;
    let vars = match kind {
        Kind::Pixi => v.get("environment_variables"),
        Kind::Mise => Some(&v),
    }
    .and_then(Value::as_object)
    .ok_or("the environment JSON has no variables")?;
    let mut l = Layer::default();
    for (k, val) in vars {
        let Some(val) = val.as_str() else { continue };
        if k == "PATH" {
            for p in std::env::split_paths(val) {
                if p.is_absolute() && !host.contains(&p) && !l.path.contains(&p) {
                    l.path.push(p);
                }
            }
        } else if !keep_var(k) && !k.is_empty() && !k.contains(['=', '\0']) && !val.contains('\0') {
            l.vars.push((k.clone(), val.to_owned()));
        }
    }
    Ok(l)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn pixi_and_mise_json() {
        let host = vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")];
        let pixi = r#"{"environment_variables":{"CONDA_PREFIX":"/w/.pixi/envs/default","PIXI_PROJECT_NAME":"t","PATH":"/w/.pixi/envs/default/bin:/usr/bin:/bin","HOME":"/x"}}"#;
        let l = parse_env(Kind::Pixi, pixi, &host).unwrap();
        assert_eq!(l.path, vec![PathBuf::from("/w/.pixi/envs/default/bin")]);
        assert_eq!(l.var("CONDA_PREFIX"), Some("/w/.pixi/envs/default"));
        assert_eq!(l.var("HOME"), None);
        let mise = r#"{"GREETING":"hi","PATH":"/d/installs/node/22/bin:/usr/bin","N":1}"#;
        let l = parse_env(Kind::Mise, mise, &host).unwrap();
        assert_eq!(l.path, vec![PathBuf::from("/d/installs/node/22/bin")]);
        assert_eq!(l.vars, vec![("GREETING".to_owned(), "hi".to_owned())]);
        assert!(parse_env(Kind::Pixi, "{}", &host).is_err() && parse_env(Kind::Mise, "[1]", &host).is_err() && parse_env(Kind::Mise, "x", &host).is_err());
    }

    #[test]
    fn project_files() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(project_file(Kind::Pixi, d.path()), None);
        assert_eq!(project_file(Kind::Mise, d.path()), None);
        std::fs::write(d.path().join("pyproject.toml"), "[project]\nname = \"x\"\n").unwrap();
        assert_eq!(project_file(Kind::Pixi, d.path()), None);
        std::fs::write(d.path().join("pyproject.toml"), "[project]\nname = \"x\"\n\n[tool.pixi.workspace]\nchannels = []\n").unwrap();
        assert_eq!(project_file(Kind::Pixi, d.path()), Some("pyproject.toml"));
        std::fs::write(d.path().join("pixi.toml"), "").unwrap();
        assert_eq!(project_file(Kind::Pixi, d.path()), Some("pixi.toml"));
        std::fs::write(d.path().join(".tool-versions"), "nodejs 22\n").unwrap();
        assert_eq!(project_file(Kind::Mise, d.path()), Some(".tool-versions"));
    }
}
