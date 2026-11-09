//! Parent side: start a command in its own process group, feed stdin, capture bounded
//! stdout/stderr, enforce a wall-clock timeout, honour cancellation, and kill the whole
//! group when it ends (so `cmd &` stragglers don't outlive the run).

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::helper::{ERROR_PREFIX, SETUP_FAILED};

const POLL: Duration = Duration::from_millis(20);
/// How long to wait for output pipes after the process ended (a process outside our
/// group, e.g. one that called `setsid` without a PID namespace, may hold them open).
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// How to run.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Written to stdin on a separate thread; empty means stdin is `/dev/null`.
    pub stdin: Vec<u8>,
    pub timeout: Duration,
    /// Bytes kept per stream: the first and last halves; the middle is counted and dropped.
    pub max_output: usize,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self { stdin: Vec::new(), timeout: Duration::from_secs(120), max_output: 64 * 1024 }
    }
}

/// One captured stream: its start and end, with the dropped middle counted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Captured {
    pub head: Vec<u8>,
    pub tail: Vec<u8>,
    /// Bytes between `head` and `tail` that were dropped.
    pub dropped: u64,
}

impl Captured {
    pub fn truncated(&self) -> bool {
        self.dropped > 0
    }

    /// Kept bytes, head then tail, without a marker.
    pub fn bytes(&self) -> Vec<u8> {
        let mut v = self.head.clone();
        v.extend_from_slice(&self.tail);
        v
    }

    /// Lossy text with a `[… N bytes cut …]` marker where the middle was dropped.
    pub fn text(&self) -> String {
        if self.dropped == 0 {
            return String::from_utf8_lossy(&self.bytes()).into_owned();
        }
        format!("{}\n[… {} bytes cut …]\n{}", String::from_utf8_lossy(&self.head), self.dropped, String::from_utf8_lossy(&self.tail))
    }

    /// Read `r` to the end, keeping at most `max` bytes.
    pub fn read(mut r: impl Read, max: usize) -> Self {
        let head_max = max - max / 2;
        let tail_max = max / 2;
        let mut head = Vec::new();
        let mut tail: VecDeque<u8> = VecDeque::new();
        let mut dropped = 0u64;
        let mut buf = [0u8; 8192];
        loop {
            let n = match r.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            let mut chunk = buf.get(..n).unwrap_or_default();
            let room = head_max.saturating_sub(head.len());
            if room > 0 {
                let take = room.min(chunk.len());
                head.extend_from_slice(chunk.get(..take).unwrap_or_default());
                chunk = chunk.get(take..).unwrap_or_default();
            }
            tail.extend(chunk);
            let over = tail.len().saturating_sub(tail_max);
            if over > 0 {
                tail.drain(..over);
                dropped = dropped.saturating_add(over as u64);
            }
        }
        Self { head, tail: tail.into(), dropped }
    }
}

/// What happened.
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    pub stdout: Captured,
    pub stderr: Captured,
    /// `None` when killed (timeout, cancel) or ended by a signal.
    pub exit_code: Option<i32>,
    /// The signal that ended it, if any (Unix).
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub elapsed: Duration,
}

impl Outcome {
    pub fn success(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out && !self.cancelled
    }

    /// The sandbox helper refused to start the command: its message.
    pub fn sandbox_error(&self) -> Option<String> {
        if self.exit_code != Some(i32::from(SETUP_FAILED)) {
            return None;
        }
        let err = String::from_utf8_lossy(&self.stderr.bytes()).into_owned();
        err.lines().find_map(|l| l.strip_prefix(ERROR_PREFIX)).map(str::to_owned)
    }
}

/// Run `cmd` (stdio and process group are set here). `Err` only if it couldn't start.
pub fn run(mut cmd: Command, opts: &RunOptions, cancelled: &dyn Fn() -> bool) -> Result<Outcome, String> {
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    cmd.stdin(if opts.stdin.is_empty() { Stdio::null() } else { Stdio::piped() }).stdout(Stdio::piped()).stderr(Stdio::piped());
    let start = Instant::now();
    let mut child = cmd.spawn().map_err(|e| format!("couldn't start {}: {e}", cmd.get_program().to_string_lossy()))?;
    let pid = child.id();

    if let Some(mut stdin) = child.stdin.take() {
        let input = opts.stdin.clone();
        // a command that never reads stdin must not block us
        thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
    }
    let (tx, rx) = mpsc::channel();
    let readers = [child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>), child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>)];
    for (i, r) in readers.into_iter().enumerate() {
        let Some(r) = r else { continue };
        let tx = tx.clone();
        let max = opts.max_output;
        thread::spawn(move || {
            let _ = tx.send((i, Captured::read(r, max)));
        });
    }
    drop(tx);

    let mut out = Outcome::default();
    let status: Option<ExitStatus> = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {}
            Err(e) => {
                log::warn!("waiting for {pid}: {e}");
                kill_group(pid);
                let _ = child.kill();
                break child.wait().ok();
            }
        }
        if cancelled() {
            out.cancelled = true;
        } else if start.elapsed() >= opts.timeout {
            out.timed_out = true;
        }
        if out.cancelled || out.timed_out {
            // Kill the group before reaping the leader, so the group id can't be reused.
            kill_group(pid);
            let _ = child.kill();
            break child.wait().ok();
        }
        thread::sleep(POLL);
    };
    // Leftover background processes in the group.
    kill_group(pid);
    out.elapsed = start.elapsed();
    if !(out.cancelled || out.timed_out) {
        out.exit_code = status.and_then(|s| s.code());
        #[cfg(unix)]
        {
            out.signal = status.and_then(|s| std::os::unix::process::ExitStatusExt::signal(&s));
        }
    }

    let deadline = Instant::now() + DRAIN_GRACE;
    for _ in 0..2 {
        let left = deadline.saturating_duration_since(Instant::now());
        let Ok((i, cap)) = rx.recv_timeout(left) else { break };
        if i == 0 {
            out.stdout = cap;
        } else {
            out.stderr = cap;
        }
    }
    Ok(out)
}

/// SIGKILL every process in the group led by `pid` (no-op if it's gone).
#[cfg(unix)]
pub fn kill_group(pid: u32) {
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;
    if let Ok(raw) = i32::try_from(pid)
        && raw > 0
    {
        let _ = killpg(Pid::from_raw(raw), Signal::SIGKILL);
    }
}

#[cfg(not(unix))]
pub fn kill_group(_pid: u32) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_keeps_head_and_tail() {
        let c = Captured::read(&b"0123456789abcdef"[..], 6);
        assert_eq!((c.head.as_slice(), c.tail.as_slice(), c.dropped), (&b"012"[..], &b"def"[..], 10));
        assert!(c.truncated());
        assert_eq!(c.text(), "012\n[… 10 bytes cut …]\ndef");
        let c = Captured::read(&b"hi"[..], 6);
        assert_eq!((c.bytes(), c.dropped), (b"hi".to_vec(), 0));
        assert_eq!(c.text(), "hi");
        let c = Captured::read(&b"abc"[..], 0);
        assert_eq!((c.bytes(), c.dropped), (Vec::new(), 3));
    }

    #[test]
    fn capture_across_chunks() {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let c = Captured::read(&data[..], 1001);
        assert_eq!(c.head, data[..501]);
        assert_eq!(c.tail, data[data.len() - 500..]);
        assert_eq!(c.dropped, (data.len() - 1001) as u64);
    }

    #[test]
    fn sandbox_error_needs_code_and_prefix() {
        let mut o = Outcome { exit_code: Some(125), ..Default::default() };
        o.stderr.head = b"yah-sandbox: no landlock\n".to_vec();
        assert_eq!(o.sandbox_error().as_deref(), Some("no landlock"));
        o.exit_code = Some(1);
        assert!(o.sandbox_error().is_none());
    }
}
