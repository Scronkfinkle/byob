//! One-line "what to do next" for failed runs that lack a program or Python library.
//! Small models recover far better from a concrete tool call than from general advice.

use super::nix::{PYTHON_IMPORTS, command_package};

/// Which tool can fix a missing dependency here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fix {
    /// `install_packages` (programs and Python libraries).
    Packages,
    /// Only `install_python_packages` (uv; Python libraries).
    PythonOnly,
    /// No package manager: tell the user how to get one.
    None,
}

/// How to get an installer where there is none (pixi works on Linux, macOS and Windows).
pub const GET_PIXI: &str = "byob has no package manager here. Tell the user that installing pixi (https://pixi.sh, e.g. \
`curl -fsSL https://pixi.sh/install.sh | sh`, `brew install pixi` or `winget install prefix-dev.pixi`) and restarting byob \
lets install_packages add programs and Python libraries for this workspace.";

/// A hint for a failed run, from its output and (for `run_command`) the command line.
/// `system` names the last-resort system installer (Homebrew, winget), if there is one.
pub fn hint(stdout: &str, stderr: &str, command: Option<&str>, fix: Fix, system: Option<&str>) -> Option<String> {
    let last_resort = |what: &str| {
        system.map_or_else(String::new, |s| {
            format!(" As a last resort, install_system_packages can install {what} system-wide with {s} (the user must approve).")
        })
    };
    let module = missing_module(stderr).or_else(|| missing_module(stdout));
    let cmd = missing_command(stderr);
    if fix == Fix::None || (fix == Fix::PythonOnly && module.is_none()) {
        let (what, pkg) = match (&module, &cmd) {
            (Some(m), _) => (format!("the Python library `{m}` is"), python_package(m)),
            (None, Some(c)) => (format!("`{c}` is"), command_package(c).unwrap_or(c).to_owned()),
            (None, None) => return None,
        };
        return Some(format!("Hint: {what} not installed, and {GET_PIXI}{}", last_resort(&pkg)));
    }
    if let Some(module) = module {
        let pkg = python_package(&module);
        return Some(match fix {
            Fix::Packages => {
                format!("Hint: the Python library `{module}` is not installed. Call install_packages with {{\"packages\": [\"{pkg}\"]}}, then run this again.")
            }
            _ => format!(
                "Hint: the Python library `{module}` is not installed. Call install_python_packages with {{\"packages\": [\"{pkg}\"]}}, then run this again."
            ),
        });
    }
    if fix != Fix::Packages {
        return None;
    }
    if let Some(cmd) = cmd {
        let pkg = command_package(&cmd).unwrap_or(&cmd);
        return Some(format!("Hint: `{cmd}` is not installed. Call install_packages with {{\"packages\": [\"{pkg}\"]}}, then run the command again."));
    }
    let c = command?;
    let installs = [
        "pip install",
        "pip3 install",
        "python -m pip",
        "python3 -m pip",
        "apt ",
        "apt-get",
        "sudo ",
        "brew install",
        "dnf ",
        "yum ",
        "pacman ",
        "nix-env",
        "nix profile install",
    ];
    installs.iter().any(|w| c.contains(w)).then(|| {
        "Hint: installing software that way doesn't work here. Call install_packages with the programs or Python libraries you need (e.g. {\"packages\": [\"jq\", \"matplotlib\"]}); they stay installed for this workspace.".to_owned()
    })
}

/// `jq` from `sh: 1: jq: not found`, `sh: jq: command not found`, `bash: line 1: jq: command not found`.
pub fn missing_command(stderr: &str) -> Option<String> {
    stderr.lines().find_map(|line| {
        let rest = line.trim_end().strip_suffix(": command not found").or_else(|| line.trim_end().strip_suffix(": not found"))?;
        let name = rest.rsplit(": ").next()?.trim();
        let ok = !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_alphanumeric() || "._+-".contains(c));
        ok.then(|| name.to_owned())
    })
}

/// `matplotlib` from `ModuleNotFoundError: No module named 'matplotlib.pyplot'`.
pub fn missing_module(text: &str) -> Option<String> {
    let (_, rest) = text.split_once("No module named '")?;
    let name = rest.split('\'').next()?;
    let ok = !name.is_empty() && name.len() <= 100 && name.chars().all(|c| c.is_ascii_alphanumeric() || "._".contains(c));
    ok.then(|| {
        // `google.protobuf` is one package; otherwise the top-level module names it.
        if PYTHON_IMPORTS.iter().any(|(import, _)| *import == name) { name.to_owned() } else { name.split('.').next().unwrap_or(name).to_owned() }
    })
}

/// The package that provides an import name (`PIL` → `pillow`).
fn python_package(module: &str) -> String {
    PYTHON_IMPORTS.iter().find(|(import, _)| *import == module).map_or_else(|| module.to_owned(), |(_, pkg)| (*pkg).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_commands_from_each_shell() {
        assert_eq!(missing_command("sh: 1: jq: not found").as_deref(), Some("jq"));
        assert_eq!(missing_command("sh: jq: command not found\n").as_deref(), Some("jq"));
        assert_eq!(missing_command("bash: line 1: rg: command not found").as_deref(), Some("rg"));
        assert_eq!(missing_command("zsh: command not found: jq"), None);
        assert_eq!(missing_command("cat: x.txt: No such file or directory"), None);
        assert_eq!(missing_command("sh: 1: $(evil): not found"), None);
    }

    #[test]
    fn missing_modules() {
        assert_eq!(missing_module("ModuleNotFoundError: No module named 'matplotlib.pyplot'").as_deref(), Some("matplotlib"));
        assert_eq!(missing_module("No module named 'PIL'").as_deref(), Some("PIL"));
        assert_eq!(missing_module("No module named 'google.protobuf'").as_deref(), Some("google.protobuf"));
        assert_eq!(missing_module("all fine"), None);
    }

    #[test]
    fn hints_name_the_tool_and_package() {
        let h = hint("", "sh: 1: convert: not found", Some("convert a.png b.jpg"), Fix::Packages, None).unwrap();
        assert!(h.contains("install_packages") && h.contains("[\"imagemagick\"]"), "{h}");
        let h = hint("", "ModuleNotFoundError: No module named 'sklearn'", None, Fix::Packages, None).unwrap();
        assert!(h.contains("[\"scikit-learn\"]"), "{h}");
        let h = hint("", "ModuleNotFoundError: No module named 'PIL'", None, Fix::PythonOnly, None).unwrap();
        assert!(h.contains("install_python_packages") && h.contains("[\"pillow\"]"), "{h}");
        assert!(hint("", "error: externally-managed-environment", Some("pip install pandas"), Fix::Packages, None).unwrap().contains("install_packages"));
        assert!(hint("", "some other failure", Some("ls"), Fix::Packages, None).is_none());
        assert!(hint("", "some other failure", Some("ls"), Fix::None, Some("Homebrew")).is_none());
    }

    #[test]
    fn without_an_installer_the_hint_says_how_to_get_pixi() {
        let h = hint("", "sh: 1: jq: not found", None, Fix::PythonOnly, None).unwrap();
        assert!(h.contains("`jq` is not installed") && h.contains("https://pixi.sh") && !h.contains("install_system_packages"), "{h}");
        let h = hint("", "sh: 1: convert: not found", None, Fix::None, Some("Homebrew")).unwrap();
        assert!(h.contains("pixi") && h.contains("install_system_packages can install imagemagick system-wide with Homebrew"), "{h}");
        let h = hint("", "No module named 'PIL'", None, Fix::None, None).unwrap();
        assert!(h.contains("`PIL`") && h.contains("pixi"), "{h}");
    }
}
