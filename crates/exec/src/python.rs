//! Which Python runs model code, and the managed environment for data-analysis packages.
//!
//! Order: `$BYOB_PYTHON`, then the managed venv `<data>/python/venv` (created by
//! [`install_python_packages`](crate::InstallPythonPackages) with `uv`), then `python3` /
//! `python` on `PATH`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

pub use byob_sandbox::which;

/// Overrides the interpreter.
pub const PYTHON_ENV: &str = "BYOB_PYTHON";

/// The managed environment's folder under the byob data folder.
pub fn managed_root(data_dir: &Path) -> PathBuf {
    data_dir.join("python")
}

pub fn managed_venv(data_dir: &Path) -> PathBuf {
    managed_root(data_dir).join("venv")
}

pub fn venv_python(venv: &Path) -> PathBuf {
    if cfg!(windows) { venv.join("Scripts").join("python.exe") } else { venv.join("bin").join("python") }
}

/// A resolved interpreter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Python {
    pub path: PathBuf,
    /// It's byob's managed venv.
    pub managed: bool,
}

impl Python {
    /// Find an interpreter (see the module docs).
    pub fn resolve(data_dir: Option<&Path>) -> Option<Self> {
        Self::resolve_in(data_dir, &[])
    }

    /// Like [`Python::resolve`], but a `python3` in the workspace environment's `PATH`
    /// entries (installed packages, a `.venv`, the dev shell) comes right after `$BYOB_PYTHON`.
    pub fn resolve_in(data_dir: Option<&Path>, env_path: &[PathBuf]) -> Option<Self> {
        if let Some(p) = std::env::var_os(PYTHON_ENV).filter(|p| !p.is_empty()) {
            return which(&p).map(|path| Self { path, managed: false });
        }
        for dir in env_path {
            for name in ["python3", "python"] {
                let p = dir.join(if cfg!(windows) { format!("{name}.exe") } else { name.to_owned() });
                if which(p.as_os_str()).is_some() {
                    return Some(Self { path: p, managed: false });
                }
            }
        }
        if let Some(d) = data_dir {
            let p = venv_python(&managed_venv(d));
            if p.exists() {
                return Some(Self { path: p, managed: true });
            }
        }
        system_python().map(|path| Self { path, managed: false })
    }

    /// Folders the sandbox must let this interpreter read (see
    /// [`byob_sandbox::interpreter_read_paths`]).
    pub fn read_paths(&self) -> Vec<PathBuf> {
        byob_sandbox::interpreter_read_paths(self.path.as_os_str())
    }
}

/// `python3` (or `python`) on `PATH`, ignoring `$BYOB_PYTHON` and the managed venv.
pub fn system_python() -> Option<PathBuf> {
    which(OsStr::new("python3")).or_else(|| which(OsStr::new("python")))
}

/// A package requirement the model may ask for: a name with optional extras and version
/// constraints (`pandas`, `scikit-learn>=1.4`, `xarray[io]`). No URLs, paths or options.
pub fn valid_requirement(s: &str) -> bool {
    let Some(first) = s.chars().next() else { return false };
    s.len() <= 100 && first.is_ascii_alphanumeric() && s.chars().all(|c| c.is_ascii_alphanumeric() || "._-[],<>=!~".contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requirements() {
        for ok in ["pandas", "scikit-learn>=1.4", "xarray[io]", "numpy==2.1.0", "a.b_c"] {
            assert!(valid_requirement(ok), "{ok}");
        }
        for bad in ["", "-r", "--index-url=x", "../x", "git+https://x", "a b", "pkg@ https://x", "/abs", "x;rm", &"a".repeat(101)] {
            assert!(!valid_requirement(bad), "{bad}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn venv_read_paths() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let venv = d.path().join("venv");
        std::fs::create_dir_all(venv.join("bin")).unwrap();
        std::fs::write(venv.join("pyvenv.cfg"), "home = /usr/bin\n").unwrap();
        std::fs::write(venv.join("bin/python"), "").unwrap();
        std::fs::set_permissions(venv.join("bin/python"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let p = Python { path: venv.join("bin/python"), managed: true };
        assert!(p.read_paths().contains(&venv.canonicalize().unwrap()));
        // a stray binary in a plain folder grants nothing
        std::fs::create_dir_all(d.path().join("bin")).unwrap();
        std::fs::write(d.path().join("bin/python3"), "").unwrap();
        std::fs::set_permissions(d.path().join("bin/python3"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let p = Python { path: d.path().join("bin/python3"), managed: false };
        assert!(p.read_paths().is_empty());
    }
}
