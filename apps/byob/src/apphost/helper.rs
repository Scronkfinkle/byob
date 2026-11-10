//! One `byob-artifact-host` child process: spawn, talk, watch, kill.
//!
//! Requests go through a writer thread (a helper that stops reading must never block the
//! UI), events come back through a reader thread. The newest frame is kept in a slot of its
//! own rather than queued, so a window that isn't repainting doesn't pile up frames.
//! Liveness: the UI pings every [`PING_EVERY`]; the helper answers from the page's script
//! thread, so a runaway script means no pongs and [`Helper::check`] reports it.

use std::io::{self, BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use byob_artifact_wire::{self as wire, Event, Frame, Request};

pub const PING_EVERY: Duration = Duration::from_millis(500);
/// No pong for this long after the page loaded: it is stuck.
pub const PONG_TIMEOUT: Duration = Duration::from_secs(3);
/// Servo start-up plus the first load may take this long before pongs are expected.
pub const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);

/// Where the helper binary is: `$BYOB_ARTIFACT_HOST`, else next to this executable.
pub fn locate() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("BYOB_ARTIFACT_HOST").filter(|p| !p.is_empty()) {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
        log::warn!("BYOB_ARTIFACT_HOST={} is not a file; HTML apps open in the browser", p.display());
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    let p = exe.parent()?.join(if cfg!(windows) { "byob-artifact-host.exe" } else { "byob-artifact-host" });
    p.is_file().then_some(p)
}

/// Why a helper is no longer running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gone {
    /// It exited or closed its output (crash, or a bad frame).
    Exited(String),
    /// It stopped answering pings.
    Unresponsive,
}

pub struct Helper {
    child: Option<Child>,
    out: Sender<Request>,
    events: Receiver<Event>,
    frame: Arc<Mutex<Option<Frame>>>,
    started: Instant,
    /// When the helper said `Ready` (pongs are expected from shortly after).
    ready_at: Option<Instant>,
    last_pong: Option<Instant>,
    next_ping: Instant,
    seq: u64,
    frames: u64,
    /// What it announced with `Event::Features` (none until then).
    features: wire::Features,
    gone: Option<Gone>,
}

impl Helper {
    /// Start `path`; `wake` is called whenever something arrives (repaint).
    pub fn spawn(path: &Path, wake: impl Fn() + Send + 'static) -> io::Result<Self> {
        let mut cmd = Command::new(path);
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit());
        // Mesa's software GL (llvmpipe) otherwise starts a render thread per core, per app.
        if std::env::var_os("LP_NUM_THREADS").is_none() {
            cmd.env("LP_NUM_THREADS", "2");
        }
        // Its own process group, so a kill takes anything it started with it.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        let mut child = cmd.spawn()?;
        let (stdin, stdout) = match (child.stdin.take(), child.stdout.take()) {
            (Some(i), Some(o)) => (i, o),
            _ => {
                kill(&mut child);
                return Err(io::Error::other("no pipes to the helper"));
            }
        };
        let (out, events, frame) = match pipes(stdin, stdout, wake) {
            Ok(p) => p,
            Err(e) => {
                kill(&mut child);
                let _ = child.wait();
                return Err(e);
            }
        };
        Ok(Self {
            child: Some(child),
            out,
            events,
            frame,
            started: Instant::now(),
            ready_at: None,
            last_pong: None,
            next_ping: Instant::now(),
            seq: 0,
            frames: 0,
            features: wire::Features::default(),
            gone: None,
        })
    }
}

/// The writer and reader threads for a helper's stdin/stdout.
fn pipes(
    stdin: std::process::ChildStdin,
    stdout: std::process::ChildStdout,
    wake: impl Fn() + Send + 'static,
) -> io::Result<(Sender<Request>, Receiver<Event>, Arc<Mutex<Option<Frame>>>)> {
    let (out, requests) = mpsc::channel::<Request>();
    std::thread::Builder::new().name("artifact-host-in".into()).spawn(move || {
        let mut w = BufWriter::new(stdin);
        for req in requests {
            if wire::send(&mut w, &req).is_err() {
                break;
            }
        }
    })?;
    let (tx, events) = mpsc::channel();
    let frame: Arc<Mutex<Option<Frame>>> = Arc::default();
    let slot = Arc::clone(&frame);
    std::thread::Builder::new().name("artifact-host-out".into()).spawn(move || {
        let mut r = BufReader::new(stdout);
        loop {
            match wire::recv::<Event>(&mut r) {
                Ok(Some(Event::Frame(f))) => *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(f),
                Ok(Some(e)) => {
                    if tx.send(e).is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    log::warn!("artifact host: {e}");
                    break;
                }
            }
            wake();
        }
        // Dropping `tx` tells the UI the helper is gone.
        wake();
    })?;
    Ok((out, events, frame))
}

impl Helper {
    pub fn send(&self, req: Request) {
        let _ = self.out.send(req);
    }

    /// Events since the last call (pongs are handled here).
    pub fn poll(&mut self) -> Vec<Event> {
        let mut list = Vec::new();
        loop {
            match self.events.try_recv() {
                Ok(Event::Pong(_)) => self.last_pong = Some(Instant::now()),
                Ok(Event::Ready) => self.ready_at = Some(Instant::now()),
                Ok(Event::Features(f)) => self.features = f,
                Ok(e) => list.push(e),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if self.gone.is_none() {
                        let status = self.child.as_mut().and_then(|c| c.try_wait().ok().flatten());
                        self.gone = Some(Gone::Exited(status.map_or_else(|| "it closed its output".to_owned(), |s| s.to_string())));
                    }
                    break;
                }
            }
        }
        list
    }

    /// It announced `bit` (a [`wire::Features`] constant).
    pub fn supports(&self, bit: u32) -> bool {
        self.features.has(bit)
    }

    /// The newest frame, if one arrived since the last call.
    pub fn take_frame(&mut self) -> Option<Frame> {
        let f = self.frame.lock().unwrap_or_else(PoisonError::into_inner).take();
        self.frames = self.frames.saturating_add(u64::from(f.is_some()));
        f
    }

    /// Whether a frame was taken before the one just taken.
    pub fn first_frame_seen(&self) -> bool {
        self.frames > 1
    }

    /// Time since the process started.
    pub fn age(&self) -> Duration {
        self.started.elapsed()
    }

    /// Ping when due; report (and kill) a helper that died or stopped answering.
    pub fn check(&mut self, now: Instant) -> Option<Gone> {
        if let Some(g) = &self.gone {
            return Some(g.clone());
        }
        if now >= self.next_ping {
            self.next_ping = now + PING_EVERY;
            self.seq = self.seq.wrapping_add(1);
            self.send(Request::Ping(self.seq));
        }
        let stuck = match (self.ready_at, self.last_pong) {
            (_, Some(p)) => now.saturating_duration_since(p) > PONG_TIMEOUT,
            // Ready but never answered: the page may still be loading; give it a moment more.
            (Some(r), None) => now.saturating_duration_since(r) > PONG_TIMEOUT * 2,
            (None, None) => now.saturating_duration_since(self.started) > STARTUP_TIMEOUT,
        };
        if stuck {
            self.kill();
            self.gone = Some(Gone::Unresponsive);
        }
        self.gone.clone()
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    /// Kill the process group now; reaped in the background.
    pub fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            kill(&mut child);
            let _ = std::thread::Builder::new().name("artifact-host-reap".into()).spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        self.kill();
    }
}

fn kill(child: &mut Child) {
    #[cfg(unix)]
    if let Ok(pid) = i32::try_from(child.id()) {
        use nix::sys::signal::{Signal, killpg};
        let _ = killpg(nix::unistd::Pid::from_raw(pid), Signal::SIGKILL);
    }
    let _ = child.kill();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in helper: a shell script that answers like one (or doesn't).
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    #[cfg(unix)]
    #[test]
    fn a_helper_that_never_answers_is_killed() {
        let dir = tempfile::tempdir().unwrap();
        // Says Ready (length 1, tag 0), then ignores every ping.
        let path = script(dir.path(), "silent", "printf '\\001\\000\\000\\000\\000'; exec sleep 600");
        let mut h = Helper::spawn(&path, || {}).unwrap();
        let pid = h.pid().unwrap();
        let start = Instant::now();
        while h.ready_at.is_none() && start.elapsed() < Duration::from_secs(5) {
            h.poll();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(h.ready_at.is_some(), "Ready arrived");
        assert_eq!(h.check(Instant::now()), None);
        // Pretend the timeout passed.
        let later = Instant::now() + PONG_TIMEOUT * 3;
        assert_eq!(h.check(later), Some(Gone::Unresponsive));
        // The process is gone (reaped in the background).
        let gone = (0..200).any(|_| {
            std::thread::sleep(Duration::from_millis(10));
            !Path::new(&format!("/proc/{pid}")).exists()
        });
        assert!(gone || !cfg!(target_os = "linux"), "pid {pid} still alive");
    }

    #[cfg(unix)]
    #[test]
    fn a_helper_that_exits_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = script(dir.path(), "crash", "exit 3");
        let mut h = Helper::spawn(&path, || {}).unwrap();
        let start = Instant::now();
        let mut gone = None;
        while gone.is_none() && start.elapsed() < Duration::from_secs(5) {
            h.poll();
            gone = h.check(Instant::now());
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(matches!(gone, Some(Gone::Exited(_))), "{gone:?}");
    }

    #[test]
    fn env_override_must_exist() {
        // Only the lookup logic; the variable is process-wide so keep it to the missing case.
        if std::env::var_os("BYOB_ARTIFACT_HOST").is_none() {
            assert!(locate().is_none_or(|p| p.ends_with("byob-artifact-host")));
        }
    }
}
