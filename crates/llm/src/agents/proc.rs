//! Running an agent CLI as a child process: the prompt goes in on stdin, stdout comes back
//! line by line, and the child is killed (its whole process group, on Unix) on cancel or
//! timeout. Standard library only. The child's output is untrusted: lines and totals are
//! capped and nothing here panics.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Longest stdout line we keep (longer ones are cut).
const MAX_LINE: usize = 4 * 1024 * 1024;
/// Most stdout bytes read from one run; the child is stopped past this.
const MAX_STDOUT: usize = 32 * 1024 * 1024;
/// Most stderr bytes kept for error messages.
const MAX_STDERR: u64 = 64 * 1024;
/// How often the caller's stop flag is checked.
const POLL: Duration = Duration::from_millis(50);

/// How a run ended.
#[derive(Debug)]
pub struct Exit {
    /// The exit code; `None` when killed by a signal.
    pub code: Option<i32>,
    /// The start of stderr, trimmed.
    pub stderr: String,
    /// We stopped it: the caller cancelled.
    pub cancelled: bool,
    /// We stopped it: the deadline passed.
    pub timed_out: bool,
    /// We stopped it: it wrote more than [`MAX_STDOUT`].
    pub too_long: bool,
}

/// Why a run couldn't start.
#[derive(Debug)]
pub struct SpawnError(pub std::io::Error);

/// The PATH a child gets: the usual install folders first, then the inherited one. A desktop
/// app opened from Finder or the Dock has a bare PATH, and CLIs such as `gemini` are scripts
/// whose `#!/usr/bin/env node` needs `node` on it.
pub fn child_path(binary: &Path) -> std::ffi::OsString {
    let mut dirs: Vec<std::path::PathBuf> = binary.parent().map(Path::to_path_buf).into_iter().collect();
    dirs.extend(super::install_dirs());
    if let Some(inherited) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&inherited));
    }
    std::env::join_paths(dirs).unwrap_or_default()
}

/// Run `cmd`, feed it `stdin`, and hand each stdout line to `on_line` until it exits,
/// `stop()` turns true, or `timeout` passes. The child runs in its own process group so a
/// stop takes its children with it.
pub fn run(mut cmd: Command, stdin: &str, timeout: Duration, stop: &dyn Fn() -> bool, on_line: &mut dyn FnMut(&str)) -> Result<Exit, SpawnError> {
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(SpawnError)?;
    // stdin on its own thread: a large prompt must not block us while the child fills stdout.
    if let Some(mut pipe) = child.stdin.take() {
        let text = stdin.to_owned();
        let _ = std::thread::Builder::new().name("byob-agent-stdin".into()).spawn(move || {
            let _ = pipe.write_all(text.as_bytes());
            // Dropping the pipe closes it: the CLI sees end of input.
        });
    }
    let (tx, rx) = mpsc::channel::<String>();
    if let Some(out) = child.stdout.take() {
        let _ = std::thread::Builder::new().name("byob-agent-stdout".into()).spawn(move || {
            let mut reader = BufReader::new(out);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match (&mut reader).take(MAX_LINE as u64).read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if buf.last() != Some(&b'\n') && buf.len() >= MAX_LINE {
                            // Skip the rest of an over-long line.
                            let mut sink = Vec::new();
                            let _ = reader.read_until(b'\n', &mut sink);
                        }
                        if tx.send(String::from_utf8_lossy(&buf).into_owned()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }
    let (err_tx, err_rx) = mpsc::channel::<String>();
    if let Some(err) = child.stderr.take() {
        let _ = std::thread::Builder::new().name("byob-agent-stderr".into()).spawn(move || {
            let mut text = String::new();
            let mut limited = err.take(MAX_STDERR);
            let _ = limited.read_to_string(&mut text);
            // Drain the rest so the child never blocks on a full stderr pipe.
            let mut rest = limited.into_inner();
            let _ = std::io::copy(&mut rest, &mut std::io::sink());
            let _ = err_tx.send(text);
        });
    }
    let deadline = Instant::now() + timeout;
    let (mut cancelled, mut timed_out, mut too_long) = (false, false, false);
    let mut total = 0usize;
    loop {
        if stop() {
            cancelled = true;
            break;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            break;
        }
        match rx.recv_timeout(POLL) {
            Ok(line) => {
                total = total.saturating_add(line.len());
                if total > MAX_STDOUT {
                    too_long = true;
                    break;
                }
                on_line(line.trim_end_matches(['\n', '\r']));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            // stdout closed: the child is done (or closed it); wait for it below.
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    if cancelled || timed_out || too_long {
        kill(&mut child);
    }
    let code = wait(&mut child, deadline.max(Instant::now()) + Duration::from_secs(5));
    let stderr = err_rx.recv_timeout(Duration::from_secs(1)).unwrap_or_default();
    let stderr = stderr.trim().chars().take(4000).collect();
    Ok(Exit { code, stderr, cancelled, timed_out, too_long })
}

/// Stop the child and everything it started.
fn kill(child: &mut Child) {
    #[cfg(unix)]
    {
        // The child leads its own process group (see `run`): signal the whole group.
        let _ = Command::new("/bin/kill").arg("-KILL").arg(format!("-{}", child.id())).stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
    let _ = child.kill();
}

/// Reap the child; kill it if it hasn't exited by `until`.
fn wait(child: &mut Child, until: Instant) -> Option<i32> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code(),
            Ok(None) if Instant::now() >= until => {
                kill(child);
                return child.wait().ok().and_then(|s| s.code());
            }
            Ok(None) => std::thread::sleep(POLL),
            Err(_) => return None,
        }
    }
}

/// Run a short command (`--version`, `auth status`) and return its exit code and output
/// (stdout, then stderr: `codex login status` writes to stderr), or `None` if it failed to
/// start or took longer than `timeout`.
pub fn output(binary: &Path, args: &[&str], timeout: Duration) -> Option<(i32, String)> {
    let mut cmd = Command::new(binary);
    cmd.args(args).env("PATH", child_path(binary));
    let mut lines = String::new();
    let exit = run(cmd, "", timeout, &|| false, &mut |l| {
        if lines.len() < 1024 * 1024 {
            lines.push_str(l);
            lines.push('\n');
        }
    })
    .ok()?;
    if exit.timed_out {
        return None;
    }
    if lines.trim().is_empty() {
        lines = exit.stderr;
    }
    Some((exit.code.unwrap_or(-1), lines))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn sh(script: &str) -> Command {
        let mut c = Command::new("/bin/sh");
        c.arg("-c").arg(script);
        c
    }

    #[test]
    fn streams_lines_and_feeds_stdin() {
        let mut got = Vec::new();
        let exit = run(sh("while read l; do echo \"got $l\"; done; echo done >&2; exit 3"), "a\nb\n", Duration::from_secs(10), &|| false, &mut |l| {
            got.push(l.to_owned())
        })
        .unwrap();
        assert_eq!(got, ["got a", "got b"]);
        assert_eq!(exit.code, Some(3));
        assert_eq!(exit.stderr, "done");
        assert!(!exit.cancelled && !exit.timed_out);
    }

    #[test]
    fn cancel_kills_the_child_and_its_children() {
        let stop = AtomicBool::new(false);
        let start = Instant::now();
        let mut n = 0;
        let exit = run(sh("echo started; sleep 30 & sleep 30; echo never"), "", Duration::from_secs(60), &|| stop.load(Ordering::Relaxed), &mut |_| {
            n += 1;
            stop.store(true, Ordering::Relaxed);
        })
        .unwrap();
        assert!(exit.cancelled);
        assert_eq!(n, 1);
        assert!(start.elapsed() < Duration::from_secs(10), "took {:?}", start.elapsed());
    }

    #[test]
    fn timeout_stops_a_silent_child() {
        let exit = run(sh("sleep 30"), "", Duration::from_millis(300), &|| false, &mut |_| {}).unwrap();
        assert!(exit.timed_out);
    }

    #[test]
    fn a_missing_binary_is_a_spawn_error() {
        assert!(run(Command::new("/nonexistent/byob-agent"), "", Duration::from_secs(1), &|| false, &mut |_| {}).is_err());
    }
}
