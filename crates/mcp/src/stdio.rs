//! stdio transport: a child process speaking newline-delimited JSON-RPC.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufReader, Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::lines::{LineRead, read_line};
use crate::rpc::Router;
use crate::text::tail;
use crate::{DEFAULT_INIT_TIMEOUT, DEFAULT_REQUEST_TIMEOUT, McpError};

/// Longest stdout line (one JSON-RPC message) we accept; longer ones are skipped.
pub const MAX_LINE: usize = 16 * 1024 * 1024;
/// How much of the server's stderr we keep.
const STDERR_KEEP: usize = 16 * 1024;
/// How much stderr goes into "the server exited" messages.
const STDERR_IN_ERROR: usize = 600;

/// How to start a local MCP server.
#[derive(Debug, Clone)]
pub struct StdioConfig {
    pub command: String,
    pub args: Vec<String>,
    /// Added to (and overriding) the inherited environment.
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
    /// Deadline for the `initialize` handshake (first runs of `npx -y ...` download the server).
    pub init_timeout: Duration,
    /// Deadline for `tools/list` pages.
    pub request_timeout: Duration,
}

impl StdioConfig {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
            init_timeout: DEFAULT_INIT_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        }
    }

    pub fn args<I: IntoIterator<Item = S>, S: Into<String>>(mut self, args: I) -> Self {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }
}

/// The tail of a stream, bounded.
#[derive(Default)]
struct Ring(VecDeque<u8>);

impl Ring {
    fn push(&mut self, bytes: &[u8]) {
        let bytes = bytes.get(bytes.len().saturating_sub(STDERR_KEEP)..).unwrap_or_default();
        let overflow = (self.0.len() + bytes.len()).saturating_sub(STDERR_KEEP);
        self.0.drain(..overflow.min(self.0.len()));
        self.0.extend(bytes);
    }

    fn text(&self) -> String {
        let (a, b) = self.0.as_slices();
        let mut v = Vec::with_capacity(a.len() + b.len());
        v.extend_from_slice(a);
        v.extend_from_slice(b);
        String::from_utf8_lossy(&v).into_owned()
    }
}

type Shared<T> = Arc<Mutex<T>>;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) struct StdioTransport {
    /// Lines for the writer thread; `None` once closed (dropping it closes the child's stdin).
    tx: Shared<Option<Sender<Vec<u8>>>>,
    child: Shared<Option<Child>>,
    stderr: Shared<Ring>,
}

impl StdioTransport {
    pub fn spawn(cfg: &StdioConfig, router: Arc<Router>) -> Result<Self, McpError> {
        let mut cmd = Command::new(&cfg.command);
        cmd.args(&cfg.args).envs(&cfg.env).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        if let Some(dir) = &cfg.cwd {
            cmd.current_dir(dir);
        }
        let mut child = cmd.spawn().map_err(|e| McpError::Spawn { command: cfg.command.clone(), message: e.to_string() })?;
        let (Some(stdin), Some(stdout), Some(stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(McpError::Spawn { command: cfg.command.clone(), message: "no stdio pipes".into() });
        };
        let (line_tx, line_rx) = mpsc::channel::<Vec<u8>>();
        let t = Self { tx: Arc::new(Mutex::new(Some(line_tx))), child: Arc::new(Mutex::new(Some(child))), stderr: Arc::default() };

        let name = cfg.command.clone();
        spawn_guarded("mcp-stdin", move || write_loop(stdin, &line_rx));

        let ring = Arc::clone(&t.stderr);
        spawn_guarded("mcp-stderr", move || stderr_loop(stderr, &ring));

        let (tx, child, ring) = (Arc::clone(&t.tx), Arc::clone(&t.child), Arc::clone(&t.stderr));
        spawn_guarded("mcp-stdout", move || {
            let r = Arc::clone(&router);
            let ok = catch_unwind(AssertUnwindSafe(|| read_loop(stdout, &r, &tx)));
            let why = match ok {
                Ok(()) => exit_reason(&name, &child, &ring),
                Err(_) => "the connection reader crashed".to_owned(),
            };
            router.shut(&why);
        });
        Ok(t)
    }

    /// Queue one message. Never blocks on a stuck child (a writer thread does the I/O).
    pub fn send(&self, msg: &Value) -> Result<(), McpError> {
        send_line(&self.tx, msg)
    }

    pub fn stderr_tail(&self) -> String {
        lock(&self.stderr).text()
    }

    /// Close stdin, give the server a moment to exit, then kill it.
    pub fn close(&self) {
        lock(&self.tx).take();
        let Some(mut child) = lock(&self.child).take() else { return };
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = child.try_wait() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        self.close();
    }
}

fn spawn_guarded(name: &str, f: impl FnOnce() + Send + 'static) {
    let spawned = thread::Builder::new().name(name.to_owned()).spawn(move || {
        if catch_unwind(AssertUnwindSafe(f)).is_err() {
            log::error!("mcp: worker thread panicked");
        }
    });
    if let Err(e) = spawned {
        log::error!("mcp: couldn't start a thread: {e}");
    }
}

fn send_line(tx: &Mutex<Option<Sender<Vec<u8>>>>, msg: &Value) -> Result<(), McpError> {
    let mut line = serde_json::to_vec(msg).map_err(|e| McpError::Protocol(e.to_string()))?;
    line.push(b'\n');
    match lock(tx).as_ref() {
        Some(tx) => tx.send(line).map_err(|_| McpError::Closed("the connector's input is closed".into())),
        None => Err(McpError::Closed("the connector was turned off".into())),
    }
}

fn write_loop(mut stdin: ChildStdin, rx: &mpsc::Receiver<Vec<u8>>) {
    while let Ok(line) = rx.recv() {
        if let Err(e) = stdin.write_all(&line).and_then(|()| stdin.flush()) {
            log::debug!("mcp: writing to server failed: {e}");
            return;
        }
    }
}

fn stderr_loop(mut stderr: impl Read, ring: &Mutex<Ring>) {
    let mut buf = [0u8; 4096];
    loop {
        match stderr.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => lock(ring).push(buf.get(..n).unwrap_or_default()),
        }
    }
}

fn read_loop(stdout: impl Read, router: &Router, tx: &Mutex<Option<Sender<Vec<u8>>>>) {
    let mut reader = BufReader::with_capacity(64 * 1024, stdout);
    let mut line = Vec::new();
    loop {
        match read_line(&mut reader, &mut line, MAX_LINE) {
            Ok(LineRead::Eof) => return,
            Err(e) => {
                log::debug!("mcp: reading from server failed: {e}");
                return;
            }
            Ok(LineRead::TooLong) => {
                log::warn!("mcp: skipped a message over {MAX_LINE} bytes");
            }
            Ok(LineRead::Line) => {
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                match serde_json::from_slice::<Value>(&line) {
                    Ok(msg) => {
                        for reply in router.deliver(msg) {
                            let _ = send_line(tx, &reply);
                        }
                    }
                    // Servers sometimes log to stdout.
                    Err(_) => log::debug!("mcp: ignoring non-JSON line from server"),
                }
            }
        }
    }
}

/// Why stdout ended: the exit status and the end of stderr, if we can get them quickly.
fn exit_reason(command: &str, child: &Mutex<Option<Child>>, ring: &Mutex<Ring>) -> String {
    let deadline = Instant::now() + Duration::from_millis(300);
    let mut status = None;
    while Instant::now() < deadline {
        match lock(child).as_mut().map(Child::try_wait) {
            Some(Ok(Some(s))) => {
                status = Some(s);
                break;
            }
            Some(Ok(None)) => {}
            // Already reaped by close(), or try_wait failed.
            None | Some(Err(_)) => break,
        }
        thread::sleep(Duration::from_millis(10));
    }
    // Let the stderr thread catch up with the last words.
    thread::sleep(Duration::from_millis(20));
    let mut why = match status.and_then(|s| s.code()) {
        Some(code) => format!("`{command}` exited (code {code})"),
        None if status.is_some() => format!("`{command}` was killed"),
        None => format!("`{command}` closed its output"),
    };
    let err = lock(ring).text();
    let err = tail(err.trim(), STDERR_IN_ERROR);
    if !err.is_empty() {
        why.push_str(": ");
        why.push_str(err);
    }
    why
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_keeps_the_tail() {
        let mut r = Ring::default();
        r.push(&vec![b'a'; STDERR_KEEP]);
        r.push(b"end");
        let t = r.text();
        assert_eq!(t.len(), STDERR_KEEP);
        assert!(t.ends_with("aend"));
        r.push(&vec![b'b'; STDERR_KEEP * 2]);
        assert_eq!(r.text(), "b".repeat(STDERR_KEEP));
    }
}
