//! The last resort: installing system-wide with Homebrew (macOS, Linuxbrew) or winget
//! (Windows), through `install_system_packages`. Never the default — it changes the
//! machine for every program and user, outside the workspace, and byob can't undo it —
//! so a person must approve every call (no grant or `--auto-approve` does), and only for
//! packages the workspace installer (Nix, pixi) couldn't provide, or where there is none.
//!
//! It runs outside the sandbox: Homebrew writes its prefix (`/opt/homebrew`,
//! `/home/linuxbrew/.linuxbrew`) and winget runs installers that may ask for elevation.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use byob_sandbox::{Outcome, RunOptions};

use super::Layer;

const TIMEOUT: Duration = Duration::from_secs(1800);
const MAX_OUTPUT: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Brew,
    Winget,
}

/// Homebrew or winget on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemInstaller {
    pub kind: Kind,
    pub program: PathBuf,
}

impl SystemInstaller {
    /// `winget` on Windows; elsewhere `brew` on `PATH` or in its usual places (a GUI app
    /// on macOS often starts without `/opt/homebrew/bin` on `PATH`).
    pub fn detect() -> Option<Self> {
        if cfg!(windows) {
            return byob_sandbox::which("winget".as_ref()).map(|program| Self { kind: Kind::Winget, program });
        }
        byob_sandbox::which("brew".as_ref())
            .or_else(|| {
                ["/opt/homebrew/bin/brew", "/usr/local/bin/brew", "/home/linuxbrew/.linuxbrew/bin/brew"].into_iter().map(PathBuf::from).find(|p| p.is_file())
            })
            .map(|program| Self { kind: Kind::Brew, program })
    }

    pub fn name(&self) -> &'static str {
        match self.kind {
            Kind::Brew => "Homebrew",
            Kind::Winget => "winget",
        }
    }

    /// The warning on the approval card.
    pub fn warning(&self, packages: &[String]) -> String {
        let list = packages.join(" ");
        match self.kind {
            Kind::Brew => format!(
                "Installs SYSTEM-WIDE with Homebrew, outside the workspace: it changes {} for every program and user on this computer, \
                 runs without byob's sandbox, and byob can't undo it (remove it later with `brew uninstall {list}`).",
                self.prefix().map_or_else(|| "Homebrew's folder".to_owned(), |p| p.display().to_string())
            ),
            Kind::Winget => format!(
                "Installs SYSTEM-WIDE with winget, outside the workspace: installers run without byob's sandbox, may ask for \
                 administrator rights, and byob can't undo it (remove it later with `winget uninstall {list}`)."
            ),
        }
    }

    /// Homebrew's prefix: the folder above `bin/brew`.
    fn prefix(&self) -> Option<PathBuf> {
        let bin = self.program.parent()?;
        (bin.file_name()? == "bin").then(|| bin.parent().map(Path::to_path_buf)).flatten()
    }

    /// What sandboxed runs need to find what it installed: Homebrew's `bin` on `PATH`.
    pub fn layer(&self) -> Layer {
        match (self.kind, self.prefix()) {
            (Kind::Brew, Some(prefix)) => Layer { path: vec![prefix.join("bin")], read: vec![prefix], ..Default::default() },
            _ => Layer::default(),
        }
    }

    /// The command lines that install `packages` (winget takes one package per call).
    pub fn commands(&self, packages: &[String]) -> Vec<Vec<OsString>> {
        match self.kind {
            Kind::Brew => {
                let mut args: Vec<OsString> = vec!["install".into()];
                args.extend(packages.iter().map(OsString::from));
                vec![args]
            }
            Kind::Winget => packages
                .iter()
                .map(|p| {
                    ["install", "--exact", "--accept-package-agreements", "--accept-source-agreements", "--disable-interactivity"]
                        .into_iter()
                        .map(OsString::from)
                        .chain([OsString::from(p)])
                        .collect()
                })
                .collect(),
        }
    }

    /// Install `packages` (names checked with [`valid_name`]), stopping when `cancelled`.
    pub fn install(&self, packages: &[String], cancelled: &dyn Fn() -> bool) -> Result<String, String> {
        if let Some(bad) = packages.iter().find(|p| !valid_name(p)) {
            return Err(format!("{bad:?} is not a package name"));
        }
        let mut log = String::new();
        for args in self.commands(packages) {
            let mut cmd = Command::new(&self.program);
            cmd.args(&args);
            if self.kind == Kind::Brew {
                cmd.env("NONINTERACTIVE", "1").env("HOMEBREW_NO_ENV_HINTS", "1").env("HOMEBREW_NO_AUTO_UPDATE", "1");
            }
            let opts = RunOptions { timeout: TIMEOUT, max_output: MAX_OUTPUT, ..Default::default() };
            let out: Outcome = byob_sandbox::run(cmd, &opts, cancelled)?;
            let text = [out.stdout.text(), out.stderr.text()].iter().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n");
            if out.cancelled {
                return Err(super::STOPPED.into());
            }
            if !out.success() {
                return Err(format!("`{} {}` failed:\n{text}", self.kind_cmd(), args.iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" ")));
            }
            log.push_str(&text);
            log.push('\n');
        }
        let after = match self.kind {
            Kind::Brew => "They are on PATH for run_command now.",
            Kind::Winget => "Programs installed this way may only be on PATH after byob restarts.",
        };
        Ok(format!("Installed system-wide with {}: {}. {after}\n{}", self.name(), packages.join(", "), log.trim_end()))
    }

    fn kind_cmd(&self) -> &'static str {
        match self.kind {
            Kind::Brew => "brew",
            Kind::Winget => "winget",
        }
    }
}

/// A package name or id (`jq`, `ffmpeg@6`, `Git.Git`, `homebrew/cask/foo`): no options,
/// no spaces, nothing a shell would interpret.
pub fn valid_name(p: &str) -> bool {
    !p.is_empty() && p.len() <= 100 && !p.starts_with('-') && p.chars().all(|c| c.is_ascii_alphanumeric() || "._+@/-".contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_commands_and_warnings() {
        assert!(valid_name("jq") && valid_name("ffmpeg@6") && valid_name("Git.Git") && valid_name("homebrew/cask/firefox"));
        assert!(!valid_name("--force") && !valid_name("a b") && !valid_name("x;rm") && !valid_name(""));
        let brew = SystemInstaller { kind: Kind::Brew, program: "/opt/homebrew/bin/brew".into() };
        assert_eq!(brew.commands(&["jq".into(), "rg".into()]), vec![vec![OsString::from("install"), "jq".into(), "rg".into()]]);
        let w = brew.warning(&["jq".into()]);
        assert!(w.contains("SYSTEM-WIDE") && w.contains("/opt/homebrew") && w.contains("brew uninstall jq"), "{w}");
        assert_eq!(brew.layer().path, vec![PathBuf::from("/opt/homebrew/bin")]);
        let winget = SystemInstaller { kind: Kind::Winget, program: "winget".into() };
        let cmds = winget.commands(&["jqlang.jq".into(), "Git.Git".into()]);
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[1].last(), Some(&OsString::from("Git.Git")));
        assert!(winget.warning(&["x".into()]).contains("winget uninstall x"));
        assert!(winget.layer().is_empty());
    }
}
