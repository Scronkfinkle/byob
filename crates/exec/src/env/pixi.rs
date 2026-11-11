//! pixi as the `install_packages` backend where there's no Nix (Linux and macOS without
//! it, Windows): conda-forge packages, per workspace, without root.
//!
//! Each workspace gets a byob-owned manifest, `<data>/envs/<ws>/pixi/pixi.toml` — the
//! workspace itself is never touched. `pixi add --manifest-path …` solves and records the
//! packages; `pixi shell-hook --json` installs the environment (in `…/pixi/.pixi/envs/
//! default`) and prints its variables, which are cached as a [`Layer`] until the manifest
//! changes. Downloads are shared by all workspaces in `<data>/pixi/cache`.
//!
//! conda-forge mostly uses the names models already reach for (`jq`, `ffmpeg`,
//! `imagemagick`, `nodejs`, and Python libraries under their pip names), so only a few
//! names are translated ([`conda_name`]). Checked against pixi 0.68.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::project::{Kind, ToolRun, parse_env, run_tool};
use super::{Installed, Layer, Runner, write_atomic};

/// Most packages per workspace.
const MAX_INSTALLED: usize = 100;

/// The `pixi` program and byob's shared pixi folder.
#[derive(Debug, Clone)]
pub struct Pixi {
    program: PathBuf,
    /// `<data>/pixi` (download cache).
    state: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
struct Cache {
    stamp: String,
    layer: Layer,
}

impl Pixi {
    pub fn new(program: PathBuf, data_dir: &Path) -> Self {
        Self { program, state: data_dir.join("pixi") }
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    fn dir(ws_dir: &Path) -> PathBuf {
        ws_dir.join("pixi")
    }

    fn tool<'a>(&'a self, dir: &'a Path) -> ToolRun<'a> {
        let cache: OsString = self.state.join("cache").into();
        ToolRun {
            program: &self.program,
            name: "pixi",
            cwd: dir,
            write: vec![dir.to_path_buf(), self.state.clone()],
            read: Vec::new(),
            env: vec![("PIXI_CACHE_DIR".into(), cache.clone()), ("RATTLER_CACHE_DIR".into(), cache), ("PIXI_NO_UPDATE_CHECK".into(), "1".into())],
        }
    }

    /// The installed packages of the workspace whose state folder is `ws_dir`, from the
    /// cache written by the last install (no pixi run).
    pub fn installed(&self, ws_dir: &Path) -> Option<Layer> {
        let dir = Self::dir(ws_dir);
        let cache: Cache = serde_json::from_str(&std::fs::read_to_string(dir.join("layer.json")).ok()?).ok()?;
        (cache.stamp == stamp(&dir) && cache.layer.path.iter().all(|p| p.exists())).then_some(cache.layer)
    }

    /// Add `packages` to the workspace's manifest and install them.
    pub(crate) fn install(&self, ws_dir: &Path, packages: &[String], run: &dyn Runner) -> Result<Installed, String> {
        let names: Vec<String> = packages.iter().map(|p| conda_name(p)).collect::<Result<_, _>>()?;
        let dir = Self::dir(ws_dir);
        std::fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
        let dir = dir.canonicalize().map_err(|e| format!("{}: {e}", dir.display()))?;
        let manifest = dir.join("pixi.toml");
        if !manifest.is_file() {
            write_atomic(&manifest, manifest_text().as_bytes())?;
        }
        let have = std::fs::read_to_string(&manifest).unwrap_or_default().lines().filter(|l| l.contains(" = ")).count();
        if have.saturating_add(names.len()) > MAX_INSTALLED + 3 {
            return Err(format!("a workspace can have at most {MAX_INSTALLED} installed packages"));
        }
        let t = self.tool(&dir);
        let m = manifest.to_string_lossy().into_owned();
        // A name conda-forge lacks fails the whole solve: drop the ones pixi names, retry.
        let mut todo = names.clone();
        let mut missing: Vec<String> = Vec::new();
        while !todo.is_empty() {
            let mut args = vec!["add", "--manifest-path", m.as_str()];
            args.extend(todo.iter().map(String::as_str));
            match run_tool(&t, &args, run, false) {
                Ok(_) => break,
                Err(e) => {
                    let named: Vec<String> = not_found(&e).into_iter().filter(|n| todo.contains(n)).collect();
                    if named.is_empty() {
                        return Err(e);
                    }
                    todo.retain(|n| !named.contains(n));
                    missing.extend(named);
                }
            }
        }
        if !todo.is_empty() {
            let text = run_tool(&t, &["shell-hook", "--json", "--manifest-path", m.as_str()], run, true)?.unwrap_or_default();
            let host: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
            let mut layer = parse_env(Kind::Pixi, &text, &host)?;
            // The environment's libraries, not just its bin folder on PATH.
            layer.read.push(dir.clone());
            let cache = Cache { stamp: stamp(&dir), layer };
            write_atomic(&dir.join("layer.json"), serde_json::to_string(&cache).unwrap_or_default().as_bytes())?;
        }
        let mut text = String::new();
        if !todo.is_empty() {
            text.push_str(&format!(
                "Installed with pixi (conda-forge) for this workspace: {}.\nrun_command and run_python can use them now (they are on PATH; Python libraries are in run_python's python3).",
                todo.join(", ")
            ));
        }
        if !missing.is_empty() {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&format!(
                "Not found on conda-forge: {}.\nCall install_packages again with the exact conda-forge name (search: https://prefix.dev/channels/conda-forge).",
                missing.iter().map(|m| format!("{m:?}")).collect::<Vec<_>>().join(", ")
            ));
        }
        // Report the names the model used.
        let asked: Vec<String> = packages.iter().zip(&names).filter(|(_, n)| missing.contains(n)).map(|(p, _)| p.clone()).collect();
        Ok(Installed { text, failed: !missing.is_empty(), missing: asked })
    }
}

/// The manifest byob starts each workspace's installs from.
fn manifest_text() -> String {
    format!("[workspace]\nname = \"byob-workspace\"\nchannels = [\"conda-forge\"]\nplatforms = [\"{}\"]\n\n[dependencies]\n", platform())
}

/// This machine's conda platform.
pub fn platform() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "aarch64") => "linux-aarch64",
        ("linux", _) => "linux-64",
        ("macos", "aarch64") => "osx-arm64",
        ("macos", _) => "osx-64",
        ("windows", "aarch64") => "win-arm64",
        _ => "win-64",
    }
}

/// Cache key: the manifest and lock file.
fn stamp(dir: &Path) -> String {
    let mut bytes = std::fs::read(dir.join("pixi.toml")).unwrap_or_default();
    bytes.push(0);
    bytes.extend(std::fs::read(dir.join("pixi.lock")).unwrap_or_default());
    format!("{:016x}", super::fnv64(&bytes))
}

/// The conda-forge name for a package as models (and byob's hints, which use nixpkgs
/// names) spell it; refuses anything that isn't a plain name with an optional version.
pub fn conda_name(raw: &str) -> Result<String, String> {
    let p = raw.trim();
    let p = p.strip_prefix("python3Packages.").or_else(|| p.strip_prefix("python3.pkgs.")).unwrap_or(p);
    let (name, version) = match p.find(['=', '<', '>', '!', '~']) {
        Some(i) => p.split_at(i),
        None => (p, ""),
    };
    let ok_name =
        !name.is_empty() && name.len() <= 100 && !name.starts_with(['-', '.']) && name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    let ok_version = version.len() <= 60 && version.chars().all(|c| c.is_ascii_alphanumeric() || "=<>!~.*,".contains(c));
    if !ok_name || !ok_version {
        return Err(format!("{raw:?} is not a package name (like \"jq\" or \"numpy>=2\")"));
    }
    let lower = name.to_ascii_lowercase();
    let mapped = match lower.as_str() {
        "python3" => "python",
        "poppler-utils" | "poppler_utils" => "poppler",
        "gnumake" => "make",
        "fd" => "fd-find",
        "opencv-python" | "opencv-python-headless" | "cv2" => "opencv",
        "torch" => "pytorch",
        "sklearn" => "scikit-learn",
        "pil" => "pillow",
        "yaml" => "pyyaml",
        "bs4" => "beautifulsoup4",
        "nodejs_22" | "nodejs_20" | "node" => "nodejs",
        _ => lower.as_str(),
    };
    Ok(format!("{mapped}{version}"))
}

/// The names in pixi's "No candidates were found for <name> <spec>." errors (the message
/// wraps lines, so whitespace is collapsed first).
pub fn not_found(err: &str) -> Vec<String> {
    let flat: String = err.split_whitespace().collect::<Vec<_>>().join(" ");
    flat.split("No candidates were found for ")
        .skip(1)
        .filter_map(|rest| {
            let name = rest.split([' ', '.']).next()?;
            let name = name.trim_end_matches(['.', ',']);
            (!name.is_empty()).then(|| name.to_owned())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_translated_and_checked() {
        assert_eq!(conda_name("jq").unwrap(), "jq");
        assert_eq!(conda_name("python3").unwrap(), "python");
        assert_eq!(conda_name("poppler_utils").unwrap(), "poppler");
        assert_eq!(conda_name("python3Packages.numpy").unwrap(), "numpy");
        assert_eq!(conda_name("numpy>=2").unwrap(), "numpy>=2");
        assert_eq!(conda_name("Pillow").unwrap(), "pillow");
        for bad in ["", "--manifest-path=/x", "a b", "x;y", "numpy>=2 ; rm"] {
            assert!(conda_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn missing_names_from_pixi_errors() {
        let e = "Error:   × failed to solve requirements of environment 'default' for platform 'linux-\n  │ 64'\n  ╰─▶ Cannot solve the request because of: No candidates were found for\n      nosuchpkg-xyz *.\n";
        assert_eq!(not_found(e), vec!["nosuchpkg-xyz".to_owned()]);
        assert!(not_found("network down").is_empty());
        assert!(manifest_text().contains(platform()) && manifest_text().contains("conda-forge"));
    }
}
