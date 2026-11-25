//! Start byob when you log in, hidden (`byob --background`).
//!
//! - Linux and the BSDs: `$XDG_CONFIG_HOME/autostart/byob.desktop` (default `~/.config`), the
//!   XDG autostart spec every desktop follows.
//! - macOS: a LaunchAgent, `~/Library/LaunchAgents/io.github.scronkfinkle.byob.plist`.
//! - Windows: the `byob` value under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
//!   (through `reg.exe`, so no registry crate).

use std::path::{Path, PathBuf};

/// The flag login launches pass.
pub const FLAG: &str = "--background";

/// Where Nix keeps packages; a path in it changes with every update.
const NIX_STORE: &str = "/nix/store";

/// The program to start: the AppImage if byob runs from one (its own path is a temporary
/// mount); a Nix install's stable profile path; else this executable.
fn program() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("couldn't tell where byob is: {e}"))?;
    Ok(stable_program(&exe, std::env::var_os("APPIMAGE"), std::env::var_os("PATH"), Path::new(NIX_STORE)))
}

/// `exe` as login should start it. Under the Nix store (`store`) the path is gone after the
/// next update and garbage collection, so it's the `byob` on `PATH` that a profile links into
/// the store (e.g. `~/.nix-profile/bin/byob`, `/run/current-system/sw/bin/byob`), kept as
/// that link rather than followed; `PATH` folders inside the store itself (`nix shell`) are
/// skipped. Nothing like that: `exe`.
fn stable_program(exe: &Path, appimage: Option<std::ffi::OsString>, path: Option<std::ffi::OsString>, store: &Path) -> PathBuf {
    if let Some(appimage) = appimage.filter(|p| !p.is_empty()) {
        return PathBuf::from(appimage);
    }
    if !exe.starts_with(store) {
        return exe.to_owned();
    }
    let name = exe.file_name().unwrap_or_else(|| "byob".as_ref());
    let linked = path.iter().flat_map(std::env::split_paths).map(|dir| dir.join(name)).find(|candidate| {
        candidate.is_absolute()
            && !candidate.starts_with(store)
            && executable(candidate)
            && std::fs::canonicalize(candidate).is_ok_and(|real| real.starts_with(store))
    });
    linked.unwrap_or_else(|| exe.to_owned())
}

#[cfg(unix)]
fn executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn executable(p: &Path) -> bool {
    p.is_file()
}

/// Whether byob starts at login now; `None` where it can't.
pub fn enabled() -> Option<bool> {
    imp::enabled()
}

/// Turn starting at login on or off.
pub fn set(on: bool) -> Result<(), String> {
    imp::set(on)
}

// ---------------------------------------------------------------------------------------
// Linux and the BSDs.

/// `$XDG_CONFIG_HOME/autostart/byob.desktop`, else `$HOME/.config/autostart/byob.desktop`.
/// (A relative `XDG_CONFIG_HOME` is invalid by the spec and ignored.)
#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
pub fn desktop_file_path(xdg_config_home: Option<&std::ffi::OsStr>, home: Option<&Path>) -> Option<PathBuf> {
    let base = xdg_config_home.map(PathBuf::from).filter(|p| p.is_absolute()).or_else(|| home.map(|h| h.join(".config")))?;
    Some(base.join("autostart").join("byob.desktop"))
}

/// A desktop entry's `Exec` argument: quoted when it needs to be, with the escapes of the
/// spec (`"`, `` ` ``, `$` and `\` inside quotes, then every `\` once more for the string
/// value; `%` doubled so it isn't a field code).
#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
fn exec_arg(arg: &str) -> String {
    let arg = arg.replace('%', "%%");
    let plain = !arg.is_empty() && arg.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+,:@%".contains(c));
    if plain {
        return arg;
    }
    let mut quoted = String::from("\"");
    for c in arg.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted.replace('\\', "\\\\")
}

/// The autostart entry for `program`. Its newlines would break the file: such a path is refused.
#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
pub fn desktop_entry(program: &Path) -> Result<String, String> {
    let path = program.to_str().ok_or_else(|| format!("byob's path isn't UTF-8: {}", program.display()))?;
    if path.chars().any(char::is_control) {
        return Err(format!("byob's path has control characters: {path:?}"));
    }
    Ok(format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=byob\n\
         Comment=byob in the background, for your routines\n\
         Exec={} {FLAG}\n\
         Icon=byob\n\
         Terminal=false\n\
         Categories=Utility;\n\
         X-GNOME-Autostart-enabled=true\n",
        exec_arg(path)
    ))
}

#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
fn write_entry(path: &Path, program: &Path) -> Result<(), String> {
    let text = desktop_entry(program)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
fn remove_entry(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("{}: {e}", path.display())),
        _ => Ok(()),
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod imp {
    use super::{desktop_file_path, program, remove_entry, write_entry};

    fn path() -> Option<std::path::PathBuf> {
        let home = std::env::var_os("HOME").filter(|h| !h.is_empty()).map(std::path::PathBuf::from);
        desktop_file_path(std::env::var_os("XDG_CONFIG_HOME").as_deref(), home.as_deref())
    }

    pub fn enabled() -> Option<bool> {
        Some(path()?.is_file())
    }

    pub fn set(on: bool) -> Result<(), String> {
        let path = path().ok_or("no home folder to put the autostart entry in")?;
        if on { write_entry(&path, &program()?) } else { remove_entry(&path) }
    }
}

// ---------------------------------------------------------------------------------------
// macOS.

/// The LaunchAgent's label (also its file name).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const AGENT_LABEL: &str = "io.github.scronkfinkle.byob";

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The LaunchAgent that starts `program` hidden at login (once: not kept alive).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn launch_agent(program: &Path) -> Result<String, String> {
    let path = program.to_str().ok_or_else(|| format!("byob's path isn't UTF-8: {}", program.display()))?;
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{AGENT_LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{}</string>
		<string>{FLAG}</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>LimitLoadToSessionType</key>
	<string>Aqua</string>
	<key>ProcessType</key>
	<string>Interactive</string>
</dict>
</plist>
"#,
        xml_escape(path)
    ))
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{AGENT_LABEL, launch_agent, program, remove_entry};

    fn path() -> Option<std::path::PathBuf> {
        let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
        Some(std::path::PathBuf::from(home).join("Library/LaunchAgents").join(format!("{AGENT_LABEL}.plist")))
    }

    pub fn enabled() -> Option<bool> {
        Some(path()?.is_file())
    }

    pub fn set(on: bool) -> Result<(), String> {
        let path = path().ok_or("no home folder to put the LaunchAgent in")?;
        if !on {
            return remove_entry(&path);
        }
        let text = launch_agent(&program()?)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        // launchd reads the folder at the next login; nothing to load now.
        std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))
    }
}

// ---------------------------------------------------------------------------------------
// Windows.

#[cfg(windows)]
mod imp {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    use super::{FLAG, program};

    const KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE: &str = "byob";
    /// No console window flashing up for reg.exe.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    fn reg(args: &[&str]) -> std::io::Result<std::process::Output> {
        Command::new("reg").args(args).creation_flags(CREATE_NO_WINDOW).output()
    }

    pub fn enabled() -> Option<bool> {
        Some(reg(&["query", KEY, "/v", VALUE]).ok()?.status.success())
    }

    pub fn set(on: bool) -> Result<(), String> {
        let out = if on {
            let exe = program()?;
            let line = format!("\"{}\" {FLAG}", exe.display());
            reg(&["add", KEY, "/v", VALUE, "/t", "REG_SZ", "/d", &line, "/f"])
        } else {
            if enabled() != Some(true) {
                return Ok(());
            }
            reg(&["delete", KEY, "/v", VALUE, "/f"])
        }
        .map_err(|e| format!("couldn't run reg.exe: {e}"))?;
        if out.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&out.stderr).trim().to_owned()) }
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    pub fn enabled() -> Option<bool> {
        None
    }

    pub fn set(_on: bool) -> Result<(), String> {
        Err("starting at login isn't supported here".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Linux and the BSDs (paths written the Unix way).
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn the_autostart_entry_goes_under_xdg_config_home() {
        let home = Path::new("/home/ada");
        assert_eq!(desktop_file_path(Some("/cfg".as_ref()), Some(home)), Some(PathBuf::from("/cfg/autostart/byob.desktop")));
        assert_eq!(desktop_file_path(None, Some(home)), Some(PathBuf::from("/home/ada/.config/autostart/byob.desktop")));
        assert_eq!(
            desktop_file_path(Some("relative".as_ref()), Some(home)),
            Some(PathBuf::from("/home/ada/.config/autostart/byob.desktop")),
            "invalid, ignored"
        );
        assert_eq!(desktop_file_path(None, None), None);
    }

    #[test]
    fn the_autostart_entry_starts_byob_hidden() {
        let entry = desktop_entry(Path::new("/opt/byob/byob")).unwrap();
        assert_eq!(
            entry,
            "[Desktop Entry]\nType=Application\nName=byob\nComment=byob in the background, for your routines\nExec=/opt/byob/byob --background\nIcon=byob\nTerminal=false\nCategories=Utility;\nX-GNOME-Autostart-enabled=true\n"
        );
        // Spaces and the spec's reserved characters are quoted and escaped.
        let odd = desktop_entry(Path::new("/home/a b/$x/100%/byob")).unwrap();
        assert!(odd.contains("Exec=\"/home/a b/\\\\$x/100%%/byob\" --background\n"), "{odd}");
        assert!(desktop_entry(Path::new("/tmp/a\nExec=evil")).is_err());
    }

    #[test]
    fn turning_it_on_and_off_writes_and_removes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = desktop_file_path(Some(dir.path().as_os_str()), None).unwrap();
        write_entry(&path, Path::new("/usr/bin/byob")).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("Exec=/usr/bin/byob --background"));
        remove_entry(&path).unwrap();
        assert!(!path.exists());
        remove_entry(&path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_nix_install_starts_from_its_profile_link_not_the_store() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        // Its real path: macOS's temporary folder is behind a symlink (/var → /private/var).
        let real = dir.path().canonicalize().unwrap();
        let root = real.as_path();
        let store = root.join("nix/store");
        let bin = |p: &Path| {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        let exe = store.join("aaa-byob-0.3.11/bin/byob");
        bin(&exe);
        // `nix shell` puts a store folder on PATH first; a profile links into the store.
        let shell = store.join("bbb-byob-0.3.12/bin/byob");
        bin(&shell);
        let profile = root.join("home/.nix-profile/bin");
        std::fs::create_dir_all(&profile).unwrap();
        symlink(&exe, profile.join("byob")).unwrap();
        let elsewhere = root.join("usr/bin/byob");
        bin(&elsewhere);
        let path = |dirs: &[&Path]| Some(std::env::join_paths(dirs).unwrap());
        let found = stable_program(&exe, None, path(&[root.join("empty").as_path(), shell.parent().unwrap(), &profile]), &store);
        assert_eq!(found, profile.join("byob"), "the link, not where it points");
        // A byob on PATH that isn't Nix's own isn't taken for it.
        assert_eq!(stable_program(&exe, None, path(&[elsewhere.parent().unwrap()]), &store), exe);
        assert_eq!(stable_program(&exe, None, None, &store), exe);
        // Outside the store, and from an AppImage, nothing changes.
        assert_eq!(stable_program(&elsewhere, None, path(&[&profile]), &store), elsewhere);
        assert_eq!(stable_program(&exe, Some("/apps/byob.AppImage".into()), path(&[&profile]), &store), PathBuf::from("/apps/byob.AppImage"));
    }

    #[test]
    fn the_launch_agent_runs_byob_hidden_at_load() {
        let plist = launch_agent(Path::new("/Applications/byob & co.app/Contents/MacOS/byob")).unwrap();
        assert!(plist.contains("<string>/Applications/byob &amp; co.app/Contents/MacOS/byob</string>\n\t\t<string>--background</string>"), "{plist}");
        assert!(plist.contains("<key>RunAtLoad</key>\n\t<true/>"));
        assert!(plist.contains(AGENT_LABEL));
    }
}
