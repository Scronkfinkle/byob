//! A Mac app started from Finder, the Dock or Spotlight gets launchd's environment, not the
//! user's shell's: `PATH` is `/usr/bin:/bin:/usr/sbin:/sbin` (no Homebrew, no Nix, no pyenv)
//! and the current directory is `/`. byob runs the user's tools with `PATH` and uses the
//! current directory as the default workspace, so at startup inside `byob.app` (and not from
//! a terminal) it moves to the home folder and, if the login shell's `PATH` differs, execs
//! itself again with it. Elsewhere this does nothing.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// Set in the re-executed process so it doesn't look up the shell again.
const MARKER: &str = "BYOB_LOGIN_ENV";
/// Printed before `env`'s output so whatever the shell's rc files print is skipped.
const START: &str = "__byob_env_start__";

/// Call first thing in `main`, before any thread starts. Only returns if it didn't exec.
pub fn adopt() {
    #[cfg(target_os = "macos")]
    macos::adopt();
}

/// Inside an app bundle (`….app/Contents/MacOS/…`), not on a terminal, not done already.
fn wanted(exe: &Path, marker: Option<&OsStr>, on_terminal: bool) -> bool {
    let in_bundle = exe.parent().is_some_and(|d| d.ends_with("Contents/MacOS")) && exe.ancestors().any(|a| a.extension() == Some(OsStr::new("app")));
    in_bundle && marker.is_none() && !on_terminal
}

/// The `PATH` from `env` output that follows [`START`].
fn parse_path(out: &[u8]) -> Option<OsString> {
    let text = String::from_utf8_lossy(out);
    let (_, env) = text.split_once(START)?;
    env.lines().find_map(|l| l.strip_prefix("PATH=")).map(str::trim).filter(|p| !p.is_empty()).map(OsString::from)
}

/// The shell's `PATH` (or the current one), then the current one's other folders, then the
/// `extra` folders that exist: so a shell that couldn't be asked still finds Homebrew and Nix.
fn merged(shell: Option<&OsStr>, current: Option<&OsStr>, extra: &[PathBuf], exists: impl Fn(&Path) -> bool) -> OsString {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut add = |d: PathBuf| {
        if d.is_absolute() && !dirs.contains(&d) {
            dirs.push(d);
        }
    };
    for p in [shell, current].into_iter().flatten() {
        std::env::split_paths(p).for_each(&mut add);
    }
    extra.iter().filter(|d| exists(d)).cloned().for_each(add);
    std::env::join_paths(dirs).unwrap_or_default()
}

/// Where package managers put commands on a Mac.
fn well_known(home: Option<&Path>, user: Option<&OsStr>) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = ["/opt/homebrew/bin", "/opt/homebrew/sbin", "/usr/local/bin", "/nix/var/nix/profiles/default/bin", "/run/current-system/sw/bin"]
        .iter()
        .map(PathBuf::from)
        .collect();
    if let Some(u) = user {
        v.push(Path::new("/etc/profiles/per-user").join(u).join("bin"));
    }
    if let Some(h) = home {
        v.push(h.join(".nix-profile/bin"));
        v.push(h.join(".local/bin"));
        v.push(h.join(".cargo/bin"));
    }
    v
}

#[cfg(target_os = "macos")]
mod macos {
    use std::io::{IsTerminal, Read};
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    pub(super) fn adopt() {
        let Ok(exe) = std::env::current_exe() else { return };
        if !super::wanted(&exe, std::env::var_os(super::MARKER).as_deref(), std::io::stdin().is_terminal()) {
            return;
        }
        let home = std::env::var_os("HOME").map(PathBuf::from).filter(|h| h.is_dir());
        if let Some(h) = &home
            && std::env::current_dir().is_ok_and(|d| d.parent().is_none())
        {
            let _ = std::env::set_current_dir(h);
        }
        let current = std::env::var_os("PATH");
        let path =
            super::merged(login_shell_path().as_deref(), current.as_deref(), &super::well_known(home.as_deref(), std::env::var_os("USER").as_deref()), |d| {
                d.is_dir()
            });
        if path.is_empty() || current.as_deref() == Some(path.as_os_str()) {
            return;
        }
        // Only returns on failure; then carry on with what there is.
        let err = Command::new(&exe).args(std::env::args_os().skip(1)).env("PATH", &path).env(super::MARKER, "1").exec();
        eprintln!("byob: couldn't restart with the login shell's PATH: {err}");
    }

    /// `$SHELL -l -i -c env` (rc files run, as in a terminal), given a few seconds.
    fn login_shell_path() -> Option<std::ffi::OsString> {
        let shell = std::env::var_os("SHELL").filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/zsh".into());
        let mut child = Command::new(shell)
            .args(["-l", "-i", "-c"])
            .arg(format!("echo {}; /usr/bin/env", super::START))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let out = child.stdout.take()?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = out.take(1 << 20).read_to_end(&mut buf);
            let _ = tx.send(buf);
        });
        let got = rx.recv_timeout(Duration::from_secs(5)).ok();
        let _ = child.kill();
        let _ = child.wait();
        super::parse_path(&got?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_inside_a_bundle_off_a_terminal_and_once() {
        let app = Path::new("/Applications/byob.app/Contents/MacOS/byob");
        assert!(wanted(app, None, false));
        assert!(!wanted(app, None, true));
        assert!(!wanted(app, Some(OsStr::new("1")), false));
        assert!(!wanted(Path::new("/usr/local/bin/byob"), None, false));
        assert!(!wanted(Path::new("/x/Contents/MacOS/byob"), None, false));
    }

    #[test]
    fn path_comes_from_after_the_marker() {
        let out = format!("PATH=/from/rc/noise\nwelcome!\n{START}\nHOME=/Users/me\nPATH=/opt/homebrew/bin:/usr/bin:/bin\nSHELL=/bin/zsh\n");
        assert_eq!(parse_path(out.as_bytes()), Some(OsString::from("/opt/homebrew/bin:/usr/bin:/bin")));
        assert_eq!(parse_path(b"PATH=/x\n"), None);
        assert_eq!(parse_path(format!("{START}\nPATH=\n").as_bytes()), None);
    }

    #[test]
    fn merged_keeps_order_drops_duplicates_and_missing_extras() {
        let extra = [PathBuf::from("/opt/homebrew/bin"), PathBuf::from("/nix/var/nix/profiles/default/bin"), PathBuf::from("/missing")];
        let got = merged(Some(OsStr::new("/opt/homebrew/bin:/usr/bin")), Some(OsStr::new("/usr/bin:/bin:/usr/sbin:relative")), &extra, |d| {
            d != Path::new("/missing")
        });
        assert_eq!(got, OsString::from("/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/nix/var/nix/profiles/default/bin"));
        // No shell answer: the launchd PATH plus whatever package managers are installed.
        let got = merged(None, Some(OsStr::new("/usr/bin:/bin")), &extra, |d| d == Path::new("/opt/homebrew/bin"));
        assert_eq!(got, OsString::from("/usr/bin:/bin:/opt/homebrew/bin"));
    }

    #[test]
    fn well_known_includes_the_users_profiles() {
        let v = well_known(Some(Path::new("/Users/me")), Some(OsStr::new("me")));
        assert!(v.contains(&PathBuf::from("/opt/homebrew/bin")));
        assert!(v.contains(&PathBuf::from("/etc/profiles/per-user/me/bin")));
        assert!(v.contains(&PathBuf::from("/Users/me/.nix-profile/bin")));
    }
}
