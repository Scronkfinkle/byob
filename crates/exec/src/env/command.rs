//! Environment providers plugins declare in their manifest (`environments`): when the
//! workspace has one of the provider's files, run its command in the workspace (sandboxed,
//! with network, may write the workspace) and read the environment it prints as JSON —
//! `{"PATH": "/x/bin:…", "NAME": "value"}` (mise's form) or `{"environment_variables": {…}}`
//! (pixi's). Asked once per workspace and cached until its files change, like byob's own.

use std::path::{Path, PathBuf};

use byob_tools::CommandEnv;

use super::project::{Kind, ToolRun, parse_env, run_tool};
use super::{EnvProvider, Layer, Runner};

/// A plugin's command provider (see the module docs).
#[derive(Debug, Clone)]
pub struct CommandProvider {
    decl: CommandEnv,
}

impl CommandProvider {
    pub fn new(decl: CommandEnv) -> Self {
        Self { decl }
    }
}

impl EnvProvider for CommandProvider {
    fn id(&self) -> &str {
        &self.decl.id
    }

    fn label(&self) -> &str {
        &self.decl.label
    }

    fn file(&self, ws: &Path) -> Option<String> {
        self.decl.detect.iter().find(|f| ws.join(f).is_file()).cloned()
    }

    fn inputs(&self) -> Vec<String> {
        let mut out = self.decl.detect.clone();
        out.extend(self.decl.inputs.iter().filter(|i| !self.decl.detect.contains(i)).cloned());
        out
    }

    fn load(&self, ws: &Path, _dir: &Path, run: &dyn Runner) -> Result<Layer, String> {
        // Files of the plugin are passed by their full path (the run's folder is the workspace).
        let args: Vec<String> = self
            .decl
            .args
            .iter()
            .map(|a| {
                let inside = self.decl.dir.join(a);
                if !a.starts_with('-') && byob_plugins_path_ok(a) && inside.is_file() { inside.display().to_string() } else { a.clone() }
            })
            .collect();
        let program = program(&self.decl.program, &self.decl.dir).ok_or_else(|| format!("{} isn't installed (not on PATH)", self.decl.program.display()))?;
        let name = self.decl.program.display().to_string();
        let t = ToolRun { program: &program, name: &name, cwd: ws, write: vec![ws.to_path_buf()], read: vec![self.decl.dir.clone()], env: Vec::new() };
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let text = run_tool(&t, &refs, run, true)?.unwrap_or_default();
        let host: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
        let kind = if serde_json::from_str::<serde_json::Value>(&text).ok().is_some_and(|v| v.get("environment_variables").is_some()) {
            Kind::Pixi
        } else {
            Kind::Mise
        };
        parse_env(kind, &text, &host).map_err(|e| format!("{e} (from `{name}`)"))
    }
}

/// A relative path that stays inside its folder.
fn byob_plugins_path_ok(p: &str) -> bool {
    let p = Path::new(p);
    !p.is_absolute() && p.components().all(|c| matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir))
}

/// The program to run: a file of the plugin, else a program on `PATH`.
fn program(p: &Path, dir: &Path) -> Option<PathBuf> {
    if p.is_absolute() {
        return p.is_file().then(|| p.to_path_buf());
    }
    let inside = dir.join(p);
    if p.components().count() > 1 || inside.is_file() {
        return inside.is_file().then_some(inside);
    }
    byob_sandbox::which(p.as_os_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_by_its_files_and_keys_the_cache_by_them() {
        let ws = tempfile::tempdir().unwrap();
        let p = CommandProvider::new(CommandEnv {
            id: "demo-conda".into(),
            label: "conda environment".into(),
            detect: vec!["environment.yml".into()],
            inputs: vec!["conda.lock".into(), "environment.yml".into()],
            program: "python3".into(),
            ..CommandEnv::default()
        });
        assert_eq!(p.file(ws.path()), None);
        std::fs::write(ws.path().join("environment.yml"), "name: x\n").unwrap();
        assert_eq!(p.file(ws.path()).as_deref(), Some("environment.yml"));
        assert_eq!(p.inputs(), ["environment.yml", "conda.lock"]);
        assert!(byob_plugins_path_ok("env.py") && !byob_plugins_path_ok("../x") && !byob_plugins_path_ok("/x"));
    }
}
