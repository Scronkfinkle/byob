//! A plugin's backend: one long-lived process the engine talks to in JSON lines (the wire
//! format is in `docs/plugins-backend.md`).
//!
//! [`Backend`] owns the process. It starts lazily on first use (an event or a request) on a
//! thread of its own, so nothing here blocks the caller: messages queue until it runs. A
//! writer thread feeds stdin, a reader thread reads stdout (lines over [`MAX_LINE`] are
//! dropped with a fault), a third keeps the last [`STDERR_KEEP`] bytes of stderr. Replies
//! go straight to the waiting [`Backend::request`]; everything else goes to the [`Sink`]
//! (the engine's inbox). A backend that exits is started again after a backoff (0.5 s,
//! doubling to 30 s), at once when someone uses it, and given up on after [`MAX_CRASHES`]
//! crashes in a row. [`Backend::stop`] (reload, switched off, removed, dropped) closes stdin
//! and kills it if it doesn't exit within half a second. A waiter thread notices the process
//! exiting even when something it started still holds its stdout; on Unix it runs in a
//! process group of its own, killed with it. Faults reach the sink at most once a second
//! (the rest are counted). Every thread runs under `catch_unwind`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError, channel, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use byob_tools::{Access, FileChange, Tool, ToolContext, ToolOutput, ToolSpec};
use serde_json::{Value, json};

use crate::PluginRef;
use crate::manifest::{ManifestTool, path_inside};
use crate::native::{NativeBackend, Outbox, dispatch, panic_text};

/// The wire format's version (`init.v`).
pub const WIRE_VERSION: u32 = 1;
/// Longest line either way (one message).
pub const MAX_LINE: usize = 1024 * 1024;
/// Bytes of stderr kept.
pub const STDERR_KEEP: usize = 16 * 1024;
/// Faults kept (the newest).
pub const MAX_FAULTS: usize = 8;
/// Characters kept of one fault.
const MAX_FAULT: usize = 2000;
/// Messages kept while it starts, and waiting for a running one to read them; older
/// ones are dropped while it starts, newer ones once it runs.
const MAX_QUEUED: usize = 256;
/// Bytes kept while it starts.
const MAX_QUEUED_BYTES: usize = 16 * 1024 * 1024;
/// Faults passed to the sink at most this often (the rest are counted in the next one).
const FAULT_EVERY: Duration = Duration::from_secs(1);
/// How often the waiter checks whether the process exited.
const WAIT_POLL: Duration = Duration::from_millis(20);
/// How long its output gets to be read after it exited.
const DRAIN_GRACE: Duration = Duration::from_millis(500);
/// How long a tool call waits for its reply.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Crashes in a row after which it isn't started again (until its plugin reloads).
pub const MAX_CRASHES: u32 = 5;
const FIRST_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Running this long resets the crash count.
const STABLE: Duration = Duration::from_secs(30);
/// How long a stopped backend gets to exit before it is killed.
const STOP_GRACE: Duration = Duration::from_millis(500);
/// What the engine supports, sent in `init.caps`.
pub const CAPS: [&str; 9] = ["event", "request", "reply", "view", "notify", "notice", "log", "fault", "web"];

/// How to start the process (worked out on the start thread: it may build a venv).
#[derive(Debug, Clone, Default)]
pub struct Launch {
    /// Program and arguments.
    pub argv: Vec<OsString>,
    pub cwd: PathBuf,
    /// Added to the inherited environment.
    pub env: BTreeMap<String, String>,
    /// Removed from it.
    pub remove_env: Vec<String>,
}

/// What a backend runs as: a process, or in-tree code on a thread ([`crate::native`]).
pub enum Start {
    Process(Launch),
    Native(Box<dyn NativeBackend>),
}

impl From<Launch> for Start {
    fn from(l: Launch) -> Self {
        Self::Process(l)
    }
}

/// Works out how to start it; its argument says the start was called off (the backend
/// stopped), so a long preparation (a venv build) can give up.
pub type Prepare = Box<dyn Fn(&dyn Fn() -> bool) -> Result<Start, String> + Send + Sync>;

/// What a backend says that isn't a reply, and its lifecycle.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    /// A message (`view`, `notify`, `notice`, `log`, `fault`, or a newer type).
    Msg(Value),
    Started,
    /// It exited or couldn't start: why.
    Exited(String),
}

pub type Sink = Arc<dyn Fn(Out) + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Not started yet.
    Idle,
    Starting,
    Running,
    /// Exited; starts again after a backoff or on the next use.
    Crashed,
    /// Couldn't start, or crashed too often: stays down until its plugin reloads.
    Failed,
    Stopped,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Crashed => "crashed",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }
}

/// A snapshot for status displays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub phase: Phase,
    pub pid: Option<u32>,
    pub starts: u32,
    /// Crashes in a row.
    pub crashes: u32,
    /// The kept stderr (and `log` lines).
    pub stderr: String,
    pub faults: Vec<String>,
    /// Why it isn't running (exit, start failure).
    pub error: Option<String>,
    pub subscribed: Vec<String>,
}

type Reply = Result<Value, String>;

struct Inner {
    phase: Phase,
    /// Bumped for every process started.
    generation: u64,
    writer: Option<SyncSender<Vec<u8>>>,
    queued: VecDeque<Vec<u8>>,
    queued_bytes: usize,
    child: Option<Child>,
    pid: Option<u32>,
    pending: BTreeMap<u64, Sender<Reply>>,
    next_id: u64,
    stderr: Ring,
    faults: VecDeque<String>,
    subscribed: BTreeSet<String>,
    starts: u32,
    crashes: u32,
    started_at: Option<Instant>,
    error: Option<String>,
    /// When a fault last went to the sink.
    fault_posted: Option<Instant>,
    /// Faults recorded since, not passed on yet.
    fault_held: u32,
    /// A thread will pass them on.
    fault_flush: bool,
}

struct Shared {
    plugin: String,
    init: Value,
    prepare: Prepare,
    sink: Sink,
    inner: Mutex<Inner>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One plugin backend (see the module docs). Stops its process when dropped.
pub struct Backend {
    shared: Arc<Shared>,
}

impl Backend {
    /// `init` is the init message (sent first to every process, with `restart` added);
    /// `events` the engine events it gets from the start.
    pub fn new(plugin: &str, init: Value, events: &[String], prepare: Prepare, sink: Sink) -> Self {
        let inner = Inner {
            phase: Phase::Idle,
            generation: 0,
            writer: None,
            queued: VecDeque::new(),
            queued_bytes: 0,
            child: None,
            pid: None,
            pending: BTreeMap::new(),
            next_id: 1,
            stderr: Ring::default(),
            faults: VecDeque::new(),
            subscribed: events.iter().cloned().collect(),
            starts: 0,
            crashes: 0,
            started_at: None,
            error: None,
            fault_posted: None,
            fault_held: 0,
            fault_flush: false,
        };
        Self { shared: Arc::new(Shared { plugin: plugin.to_owned(), init, prepare, sink, inner: Mutex::new(inner) }) }
    }

    pub fn plugin(&self) -> &str {
        &self.shared.plugin
    }

    /// It asked for engine event `event`.
    pub fn wants(&self, event: &str) -> bool {
        self.shared.lock().subscribed.contains(event)
    }

    pub fn status(&self) -> Status {
        let g = self.shared.lock();
        Status {
            phase: g.phase,
            pid: g.pid,
            starts: g.starts,
            crashes: g.crashes,
            stderr: g.stderr.text(),
            faults: g.faults.iter().cloned().collect(),
            error: g.error.clone(),
            subscribed: g.subscribed.iter().cloned().collect(),
        }
    }

    /// Start it now if it isn't running (normally it starts on first use).
    pub fn start(&self) {
        let mut g = self.shared.lock();
        if matches!(g.phase, Phase::Idle | Phase::Crashed) {
            begin_start(&self.shared, &mut g);
        }
    }

    /// Send an `event` message (`{"type": "event", "event": …, …}`), starting the backend if
    /// needed. `false` if it can't take it (stopped, failed, too big).
    pub fn event(&self, msg: Value) -> bool {
        let Ok(line) = serde_json::to_vec(&msg) else { return false };
        if line.len() > MAX_LINE {
            log::warn!("plugin {}: an event over {MAX_LINE} bytes wasn't sent", self.shared.plugin);
            return false;
        }
        let mut g = self.shared.lock();
        if matches!(g.phase, Phase::Idle | Phase::Crashed) {
            begin_start(&self.shared, &mut g);
        }
        push_line(&self.shared, &mut g, line).is_ok()
    }

    /// Ask it something and wait for the `reply` (or `timeout`, or `cancelled()`).
    pub fn request(&self, method: &str, params: Value, timeout: Duration, cancelled: &dyn Fn() -> bool) -> Result<Value, String> {
        let (tx, rx) = channel();
        let id = {
            let mut g = self.shared.lock();
            match g.phase {
                Phase::Stopped => return Err("the backend was stopped (its plugin changed, or was switched off or removed); try again".into()),
                Phase::Failed => return Err(g.error.clone().unwrap_or_else(|| "the backend couldn't start".into())),
                Phase::Idle | Phase::Crashed => begin_start(&self.shared, &mut g),
                Phase::Starting | Phase::Running => {}
            }
            let id = g.next_id;
            g.next_id = g.next_id.wrapping_add(1);
            let line = serde_json::to_vec(&json!({"type": "request", "id": id, "method": method, "params": params})).map_err(|e| e.to_string())?;
            if line.len() > MAX_LINE {
                return Err(format!("the request is over {MAX_LINE} bytes"));
            }
            g.pending.insert(id, tx);
            if let Err(why) = push_line(&self.shared, &mut g, line) {
                g.pending.remove(&id);
                return Err(why.into());
            }
            id
        };
        let started = Instant::now();
        loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(r) => return r,
                Err(RecvTimeoutError::Disconnected) => return Err("the backend went away".into()),
                Err(RecvTimeoutError::Timeout) => {}
            }
            let gave_up = if cancelled() {
                Some("Stopped by the user.".to_owned())
            } else if started.elapsed() >= timeout {
                Some(if timeout < Duration::from_secs(1) {
                    format!("the backend didn't answer within {} ms", timeout.as_millis())
                } else {
                    format!("the backend didn't answer within {} seconds", timeout.as_secs())
                })
            } else {
                None
            };
            if let Some(why) = gave_up {
                let mut g = self.shared.lock();
                g.pending.remove(&id);
                if let Ok(line) = serde_json::to_vec(&json!({"type": "cancel", "id": id})) {
                    let _ = push_line(&self.shared, &mut g, line);
                }
                return Err(why);
            }
        }
    }

    /// A problem byob found in what it sent (e.g. a view tree it had to fix): kept in its
    /// status (`faults`) for the model and the user, and passed to the sink as a `fault`.
    pub fn report_fault(&self, text: &str) {
        fault(&self.shared, text.to_owned());
    }

    /// Hand `msg` to the sink as if the backend had sent it (the tool host uses this to pass
    /// a tool's card on to the engine).
    pub fn post(&self, msg: Value) {
        (self.shared.sink)(Out::Msg(msg));
    }

    /// Stop the process for good (this object won't start it again).
    pub fn stop(&self, why: &str) {
        stop(&self.shared, why);
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        stop(&self.shared, "the backend was stopped");
    }
}

fn stop(shared: &Arc<Shared>, why: &str) {
    let mut g = shared.lock();
    if g.phase == Phase::Stopped {
        return;
    }
    let was = g.phase;
    g.phase = Phase::Stopped;
    if let Some(tx) = g.writer.take()
        && let Ok(line) = serde_json::to_vec(&json!({"type": "shutdown"}))
    {
        let _ = tx.try_send(line);
    }
    take_queued(&mut g);
    g.pid = None;
    let child = g.child.take();
    for (_, tx) in std::mem::take(&mut g.pending) {
        let _ = tx.send(Err(why.to_owned()));
    }
    drop(g);
    if let Some(child) = child {
        log::info!("plugin {}: stopping its backend ({why})", shared.plugin);
        guarded("byob-backend-stop", move || {
            reap(child, STOP_GRACE);
        });
    } else if was != Phase::Idle {
        log::info!("plugin {}: backend stopped ({why})", shared.plugin);
    }
}

/// Wait up to `grace` for `child` to exit, then kill it; either way kill what is left of
/// its process group. Returns how it ended.
fn reap(mut child: Child, grace: Duration) -> String {
    let pid = child.id();
    let deadline = Instant::now() + grace;
    let how = loop {
        match child.try_wait() {
            Ok(Some(status)) => break exit_text(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            _ => {
                // the group before the leader is reaped, so its id can't be reused yet
                byob_sandbox::kill_group(pid);
                let _ = child.kill();
                break child.wait().map_or_else(|e| e.to_string(), exit_text);
            }
        }
    };
    byob_sandbox::kill_group(pid);
    how
}

fn exit_text(status: std::process::ExitStatus) -> String {
    match status.code() {
        Some(c) => format!("exit code {c}"),
        None => "killed by a signal".into(),
    }
}

/// Queue one line: to the writer when running (dropped, with a fault, when it isn't
/// reading), kept while starting (the oldest dropped past [`MAX_QUEUED`] or
/// [`MAX_QUEUED_BYTES`]).
fn push_line(shared: &Arc<Shared>, g: &mut Inner, line: Vec<u8>) -> Result<(), &'static str> {
    const NOT_RUNNING: &str = "the backend isn't running";
    match g.phase {
        Phase::Running => match &g.writer {
            Some(tx) => match tx.try_send(line) {
                Ok(()) => Ok(()),
                Err(TrySendError::Full(_)) => {
                    record_fault(shared, g, format!("it isn't reading its input: {MAX_QUEUED} messages wait, so one was dropped"), false);
                    Err("the backend isn't reading its input (too many messages wait); the message was dropped")
                }
                Err(TrySendError::Disconnected(_)) => Err(NOT_RUNNING),
            },
            None => Err(NOT_RUNNING),
        },
        Phase::Starting => {
            let mut dropped = false;
            while g.queued.len() >= MAX_QUEUED || (!g.queued.is_empty() && g.queued_bytes.saturating_add(line.len()) > MAX_QUEUED_BYTES) {
                let Some(old) = g.queued.pop_front() else { break };
                g.queued_bytes = g.queued_bytes.saturating_sub(old.len());
                dropped = true;
            }
            if dropped {
                log::warn!("plugin {}: its backend is slow to start; an old message was dropped", shared.plugin);
            }
            g.queued_bytes = g.queued_bytes.saturating_add(line.len());
            g.queued.push_back(line);
            Ok(())
        }
        Phase::Idle | Phase::Crashed | Phase::Failed | Phase::Stopped => Err(NOT_RUNNING),
    }
}

/// Empty the start queue.
fn take_queued(g: &mut Inner) -> VecDeque<Vec<u8>> {
    g.queued_bytes = 0;
    std::mem::take(&mut g.queued)
}

fn begin_start(shared: &Arc<Shared>, g: &mut Inner) {
    g.phase = Phase::Starting;
    let shared = Arc::clone(shared);
    guarded("byob-backend-start", move || start_now(&shared));
}

/// What [`start_now`] got going.
enum Running {
    Process(Spawned),
    Native(Box<dyn NativeBackend>),
}

fn start_now(shared: &Arc<Shared>) {
    let called_off = || shared.lock().phase != Phase::Starting;
    let prepared = catch_unwind(AssertUnwindSafe(|| (shared.prepare)(&called_off)));
    let spawned = prepared.unwrap_or_else(|_| Err("preparing the backend crashed".into())).and_then(|s| match s {
        Start::Process(l) => spawn(&l).map(Running::Process),
        Start::Native(n) => Ok(Running::Native(n)),
    });
    let mut g = shared.lock();
    if g.phase != Phase::Starting {
        // stopped while it started
        if let Ok(Running::Process((child, ..))) = spawned {
            drop(g);
            reap(child, Duration::ZERO);
        }
        return;
    }
    let running = match spawned {
        Ok(parts) => parts,
        Err(e) => {
            g.phase = Phase::Failed;
            g.error = Some(e.clone());
            take_queued(&mut g);
            for (_, tx) in std::mem::take(&mut g.pending) {
                let _ = tx.send(Err(e.clone()));
            }
            drop(g);
            log::warn!("plugin {}: its backend couldn't start: {e}", shared.plugin);
            (shared.sink)(Out::Exited(e));
            return;
        }
    };
    g.generation = g.generation.wrapping_add(1);
    let generation = g.generation;
    // room for init, the start queue and a shutdown
    let (tx, rx) = sync_channel::<Vec<u8>>(MAX_QUEUED + 2);
    let mut init = shared.init.clone();
    if let Some(o) = init.as_object_mut() {
        o.insert("restart".into(), json!(g.starts));
    }
    if let Ok(line) = serde_json::to_vec(&init) {
        let _ = tx.try_send(line);
    }
    for line in take_queued(&mut g) {
        let _ = tx.try_send(line);
    }
    g.writer = Some(tx);
    g.phase = Phase::Running;
    g.starts = g.starts.saturating_add(1);
    g.started_at = Some(Instant::now());
    g.error = None;
    let (child, stdin, stdout, stderr) = match running {
        Running::Process(parts) => parts,
        Running::Native(backend) => {
            drop(g);
            log::info!("plugin {}: native backend started", shared.plugin);
            let s = Arc::clone(shared);
            guarded("byob-backend-native", move || {
                let how = native_loop(&s, backend, &rx);
                exited(&s, generation, &how);
            });
            (shared.sink)(Out::Started);
            return;
        }
    };
    let pid = child.id();
    g.pid = Some(pid);
    g.child = Some(child);
    drop(g);
    log::info!("plugin {}: backend started", shared.plugin);
    let out_done = Arc::new(AtomicBool::new(false));
    let err_done = Arc::new(AtomicBool::new(false));
    guarded("byob-backend-in", move || write_loop(stdin, &rx));
    let (s, done) = (Arc::clone(shared), Arc::clone(&err_done));
    guarded("byob-backend-err", move || {
        let _ = catch_unwind(AssertUnwindSafe(|| stderr_loop(stderr, &s, generation)));
        done.store(true, Ordering::SeqCst);
    });
    let (s, done) = (Arc::clone(shared), Arc::clone(&out_done));
    guarded("byob-backend-out", move || {
        let read = catch_unwind(AssertUnwindSafe(|| read_loop(stdout, &s, generation)));
        if read.is_err() {
            log::error!("plugin {}: the backend reader crashed", s.plugin);
        }
        done.store(true, Ordering::SeqCst);
    });
    let s = Arc::clone(shared);
    guarded("byob-backend-wait", move || wait_loop(&s, generation, pid, &out_done, &err_done));
    (shared.sink)(Out::Started);
}

/// Watch the process of `generation` until it exits (or closes its stdout, or is stopped),
/// then let the readers finish its output and report it [`exited`]. Its process group goes
/// too, so nothing it started keeps the pipes (and the reader threads) alive.
fn wait_loop(shared: &Arc<Shared>, generation: u64, pid: u32, out_done: &AtomicBool, err_done: &AtomicBool) {
    let ended = loop {
        thread::sleep(WAIT_POLL);
        let mut g = shared.lock();
        if g.generation != generation || g.phase != Phase::Running {
            return; // stopped: stop() reaps it
        }
        let Some(child) = g.child.as_mut() else { return };
        match child.try_wait() {
            Ok(Some(status)) => {
                g.child = None;
                break Ok(exit_text(status));
            }
            Ok(None) if !out_done.load(Ordering::SeqCst) => {}
            // it closed stdout (or can't be waited for): a moment to exit, then killed
            Ok(None) | Err(_) => break Err(g.child.take()),
        }
    };
    let how = match ended {
        Ok(how) => {
            byob_sandbox::kill_group(pid);
            how
        }
        Err(Some(child)) => reap(child, Duration::from_secs(2)),
        Err(None) => "it ended".to_owned(),
    };
    let deadline = Instant::now() + DRAIN_GRACE;
    while !(out_done.load(Ordering::SeqCst) && err_done.load(Ordering::SeqCst)) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    exited(shared, generation, &how);
}

type Spawned = (Child, ChildStdin, std::process::ChildStdout, std::process::ChildStderr);

fn spawn(l: &Launch) -> Result<Spawned, String> {
    let Some((program, args)) = l.argv.split_first() else { return Err("no command to start".into()) };
    let mut cmd = Command::new(program);
    cmd.args(args).current_dir(&l.cwd).envs(&l.env).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    // a group of its own, so what it starts is stopped with it
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    for k in &l.remove_env {
        cmd.env_remove(k);
    }
    let mut child = cmd.spawn().map_err(|e| format!("couldn't start {}: {e}", Path::new(program).display()))?;
    match (child.stdin.take(), child.stdout.take(), child.stderr.take()) {
        (Some(i), Some(o), Some(e)) => Ok((child, i, o, e)),
        _ => {
            reap(child, Duration::ZERO);
            Err("no pipes to the backend".into())
        }
    }
}

/// Run a native backend on the lines byob sends until `shutdown` (or the channel closes on
/// a stop), each call under `catch_unwind`; returns how it ended. Its messages go through
/// the same handling as a process's stdout lines.
fn native_loop(shared: &Arc<Shared>, mut backend: Box<dyn NativeBackend>, rx: &Receiver<Vec<u8>>) -> String {
    let s = Arc::clone(shared);
    let out = Outbox::new(move |msg: Value| {
        let size = serde_json::to_vec(&msg).map_or(0, |v| v.len());
        if size > MAX_LINE {
            fault(&s, format!("a message over {MAX_LINE} bytes was dropped"));
        } else if msg.is_object() {
            handle_msg(&s, msg);
        }
    });
    for line in rx {
        let Ok(msg) = serde_json::from_slice::<Value>(&line) else { continue };
        match catch_unwind(AssertUnwindSafe(|| dispatch(backend.as_mut(), &msg, &out))) {
            Ok(true) => {}
            Ok(false) => return "it shut down".into(),
            Err(p) => {
                let text = panic_text(p.as_ref());
                shared.lock().stderr.push(format!("(panic) {text}\n").as_bytes());
                fault(shared, format!("the native backend panicked: {text}"));
                return format!("it panicked: {text}");
            }
        }
    }
    "it ended".into()
}

fn write_loop(mut stdin: ChildStdin, rx: &Receiver<Vec<u8>>) {
    for mut line in rx {
        line.push(b'\n');
        if stdin.write_all(&line).and_then(|()| stdin.flush()).is_err() {
            return;
        }
    }
}

fn stderr_loop(mut stderr: impl Read, shared: &Shared, generation: u64) {
    let mut buf = [0u8; 4096];
    loop {
        match stderr.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                let mut g = shared.lock();
                if g.generation != generation {
                    return; // a newer process runs
                }
                g.stderr.push(buf.get(..n).unwrap_or_default());
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

fn read_loop(stdout: impl Read, shared: &Arc<Shared>, generation: u64) {
    let mut r = BufReader::new(stdout);
    let mut buf = Vec::new();
    loop {
        let line = read_line(&mut r, &mut buf, MAX_LINE);
        if shared.lock().generation != generation {
            return; // a newer process runs: this one's leftovers are stale
        }
        match line {
            Ok(LineRead::Line) => handle_line(shared, &buf),
            Ok(LineRead::TooLong) => fault(shared, format!("a message over {MAX_LINE} bytes was dropped")),
            Ok(LineRead::Eof) | Err(_) => return,
        }
    }
}

/// Keep a fault in the status and pass it to the sink, at most once per [`FAULT_EVERY`]:
/// faults in between are passed on together after it (the newest, with how many more).
fn fault(shared: &Arc<Shared>, text: String) {
    let post = record_fault(shared, &mut shared.lock(), text, true);
    if let Some(text) = post {
        post_fault(shared, &text);
    }
}

/// [`fault`] for a caller holding the lock: returns the text to post now (only if
/// `may_post`; else a thread posts it).
fn record_fault(shared: &Arc<Shared>, g: &mut Inner, text: String, may_post: bool) -> Option<String> {
    let text: String = text.chars().take(MAX_FAULT).collect();
    if g.faults.len() >= MAX_FAULTS {
        g.faults.pop_front();
    }
    g.faults.push_back(text.clone());
    let wait = g.fault_posted.map_or(Duration::ZERO, |t| FAULT_EVERY.saturating_sub(t.elapsed()));
    if may_post && wait.is_zero() && !g.fault_flush {
        g.fault_posted = Some(Instant::now());
        return Some(text);
    }
    g.fault_held = g.fault_held.saturating_add(1);
    if !g.fault_flush {
        g.fault_flush = true;
        let s = Arc::clone(shared);
        guarded("byob-backend-faults", move || {
            thread::sleep(wait);
            flush_faults(&s);
        });
    }
    None
}

/// Pass on the faults held back: the newest, and how many more there were.
fn flush_faults(shared: &Shared) {
    let text = {
        let mut g = shared.lock();
        g.fault_flush = false;
        let held = std::mem::take(&mut g.fault_held);
        if held == 0 {
            return;
        }
        g.fault_posted = Some(Instant::now());
        let last = g.faults.back().cloned().unwrap_or_default();
        if held > 1 { format!("{last} ({} more)", held - 1) } else { last }
    };
    post_fault(shared, &text);
}

fn post_fault(shared: &Shared, text: &str) {
    log::warn!("plugin {}: {text}", shared.plugin);
    (shared.sink)(Out::Msg(json!({"type": "fault", "text": text})));
}

fn handle_line(shared: &Arc<Shared>, line: &[u8]) {
    if line.iter().all(u8::is_ascii_whitespace) {
        return;
    }
    let msg = match serde_json::from_slice::<Value>(line) {
        Ok(v @ Value::Object(_)) => v,
        _ => {
            let text = String::from_utf8_lossy(line);
            shared.lock().stderr.push(format!("(stdout) {text}\n").as_bytes());
            let short: String = text.chars().take(200).collect();
            fault(shared, format!("stdout is for protocol messages (JSON objects, one per line); this isn't one: {short}"));
            return;
        }
    };
    handle_msg(shared, msg);
}

/// One message from the backend (a process's line, or a native backend's [`Outbox`]).
fn handle_msg(shared: &Arc<Shared>, msg: Value) {
    let kind = msg.get("type").and_then(Value::as_str).unwrap_or_default();
    match kind {
        "reply" => {
            let Some(id) = msg.get("id").and_then(Value::as_u64) else {
                fault(shared, "a reply without a numeric id".into());
                return;
            };
            let tx = shared.lock().pending.remove(&id);
            let Some(tx) = tx else { return };
            let result = match msg.get("error").filter(|e| !e.is_null()) {
                Some(Value::String(e)) => Err(e.clone()),
                Some(e) => Err(e.to_string()),
                None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = tx.send(result);
        }
        "fault" => {
            let text = msg.get("text").map(|t| t.as_str().map_or_else(|| t.to_string(), str::to_owned)).unwrap_or_default();
            fault(shared, text);
        }
        "log" => {
            let text = msg.get("text").and_then(Value::as_str).unwrap_or_default();
            shared.lock().stderr.push(format!("(log) {text}\n").as_bytes());
            log::info!("plugin {}: {}", shared.plugin, text.chars().take(500).collect::<String>());
        }
        "subscribe" | "unsubscribe" => {
            let events: Vec<String> = msg.get("events").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
            let unknown: Vec<&String> = events.iter().filter(|e| !crate::manifest::BACKEND_EVENTS.contains(&e.as_str())).collect();
            if !unknown.is_empty() {
                fault(shared, format!("no such engine events: {unknown:?} (there are {})", crate::manifest::BACKEND_EVENTS.join(", ")));
            }
            let mut g = shared.lock();
            for e in events.iter().filter(|e| crate::manifest::BACKEND_EVENTS.contains(&e.as_str())) {
                if kind == "subscribe" {
                    g.subscribed.insert(e.clone());
                } else {
                    g.subscribed.remove(e);
                }
            }
        }
        _ => (shared.sink)(Out::Msg(msg)),
    }
}

/// The backend of `generation` ended (`how`; its process, if any, is reaped).
fn exited(shared: &Arc<Shared>, generation: u64, how: &str) {
    let mut g = shared.lock();
    if g.generation != generation || g.phase != Phase::Running {
        return;
    }
    g.writer = None;
    g.pid = None;
    g.child = None;
    let tail = g.stderr.last_lines(15);
    let mut why = format!("the backend exited ({how})");
    if !tail.is_empty() {
        why.push_str(&format!("; its last output:\n{tail}"));
    }
    let stable = g.started_at.is_some_and(|t| t.elapsed() >= STABLE);
    g.crashes = if stable { 1 } else { g.crashes.saturating_add(1) };
    for (_, tx) in std::mem::take(&mut g.pending) {
        let _ = tx.send(Err(why.clone()));
    }
    let crashes = g.crashes;
    if crashes >= MAX_CRASHES {
        g.phase = Phase::Failed;
        why.push_str(&format!("\nIt exited {crashes} times in a row, so byob stopped starting it; fix it and reload the plugin."));
    } else {
        g.phase = Phase::Crashed;
        let backoff = FIRST_BACKOFF.saturating_mul(1 << crashes.saturating_sub(1).min(8)).min(MAX_BACKOFF);
        let s = Arc::clone(shared);
        guarded("byob-backend-retry", move || {
            thread::sleep(backoff);
            let mut g = s.lock();
            if g.phase == Phase::Crashed && g.generation == generation {
                begin_start(&s, &mut g);
            }
        });
    }
    g.error = Some(why.clone());
    drop(g);
    log::warn!("plugin {}: {why}", shared.plugin);
    (shared.sink)(Out::Exited(why));
}

fn guarded(name: &str, work: impl FnOnce() + Send + 'static) {
    let label = name.to_owned();
    let spawned = thread::Builder::new().name(name.into()).spawn(move || {
        if catch_unwind(AssertUnwindSafe(work)).is_err() {
            log::error!("{label} crashed");
        }
    });
    if let Err(e) = spawned {
        log::error!("could not start {name}: {e}");
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

    fn last_lines(&self, n: usize) -> String {
        last_lines(&self.text(), n)
    }
}

/// The last `n` lines of `text`, trimmed.
pub fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.trim_end().lines().collect();
    lines.get(lines.len().saturating_sub(n)..).unwrap_or_default().join("\n")
}

#[derive(Debug, PartialEq, Eq)]
enum LineRead {
    Line,
    TooLong,
    Eof,
}

/// Read one line into `buf` (cleared first), holding at most `max` bytes; longer lines are
/// consumed and dropped.
fn read_line(r: &mut impl BufRead, buf: &mut Vec<u8>, max: usize) -> io::Result<LineRead> {
    buf.clear();
    let mut overflow = false;
    let mut any = false;
    loop {
        let avail = match r.fill_buf() {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if avail.is_empty() {
            return Ok(match (overflow, any) {
                (true, _) => LineRead::TooLong,
                (false, true) => LineRead::Line,
                (false, false) => LineRead::Eof,
            });
        }
        any = true;
        let newline = avail.iter().position(|&b| b == b'\n');
        let (chunk, used) = match newline {
            Some(i) => (avail.split_at(i).0, i.saturating_add(1)),
            None => (avail, avail.len()),
        };
        if !overflow {
            if buf.len().saturating_add(chunk.len()) > max {
                overflow = true;
                buf.clear();
                buf.shrink_to(64 * 1024);
            } else {
                buf.extend_from_slice(chunk);
            }
        }
        r.consume(used);
        if newline.is_some() {
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            return Ok(if overflow { LineRead::TooLong } else { LineRead::Line });
        }
    }
}

/// A tool a plugin's backend serves (`"run": "backend"`): each call is a `tool` request.
pub struct BackendTool {
    name: String,
    plugin: PluginRef,
    decl: ManifestTool,
    backend: Arc<Backend>,
    timeout: Duration,
    /// The chat's artifact types, for the versions its results save.
    types: byob_tools::ArtifactTypes,
    /// The plugin's card views: (id, a web view).
    cards: Vec<(String, bool)>,
}

impl BackendTool {
    pub fn new(name: String, plugin: PluginRef, decl: ManifestTool, backend: Arc<Backend>) -> Self {
        Self { name, plugin, decl, backend, timeout: REQUEST_TIMEOUT, types: byob_tools::ArtifactTypes::builtin(), cards: Vec::new() }
    }

    /// The card views its plugin declares (`views` with `"site": "card"`): (id, web view).
    pub fn with_cards(mut self, cards: Vec<(String, bool)>) -> Self {
        self.cards = cards;
        self
    }

    /// Save the artifact versions its results carry with these types (the chat's).
    pub fn with_types(mut self, types: byob_tools::ArtifactTypes) -> Self {
        self.types = types;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn declared(&self) -> &ManifestTool {
        &self.decl
    }
}

impl Tool for BackendTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec { name: self.name.clone(), description: self.decl.description.clone(), parameters: self.decl.params.clone() }
    }

    fn title(&self, args: &Value) -> String {
        match &self.decl.title {
            Some(t) => crate::python::fill_title(t, args),
            None => format!("Used {}: {}", self.plugin.name, self.decl.name),
        }
    }

    fn access(&self, _args: &Value) -> Access {
        Access::Execute
    }

    fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> ToolOutput {
        let args = if args.is_object() { args.clone() } else { Value::Object(serde_json::Map::new()) };
        let params = json!({
            "name": self.decl.name,
            "args": args,
            "workspace": ctx.workspace.display().to_string(),
            "chat": ctx.chat(),
        });
        match self.backend.request("tool", params, self.timeout, &|| ctx.cancelled()) {
            Ok(v) => {
                self.attach_card(&v, ctx);
                let mut out = tool_output(v.clone(), ctx.workspace);
                save_artifacts(&v, &self.types, ctx, &mut out);
                out
            }
            Err(e) if e.starts_with("Stopped by the user") => ToolOutput::err(e),
            Err(e) => ToolOutput::err(format!("{} (backend) ran into a problem:\n{e}", self.plugin.name)),
        }
    }
}

impl BackendTool {
    /// A tool reply's card (`{"text": …, "card": <view tree>, "card_view"?: id, "card_key"?:
    /// k}`) goes onto the call's step (through `ctx`, so it lands on this call's step and
    /// nowhere else). A tree card is also posted to the engine (`CARD_MSG`) to be cached as
    /// the live view it owns; a web card's page gets the card's data.
    fn attach_card(&self, v: &Value, ctx: &ToolContext<'_>) {
        let Some(o) = v.as_object() else { return };
        let Some(tree) = o.get("card").filter(|c| c.is_object()) else { return };
        let card = match o.get("card_view").and_then(Value::as_str) {
            Some(want) => match self.cards.iter().find(|(id, _)| id == want) {
                Some(c) => c,
                None => {
                    let ids: Vec<&str> = self.cards.iter().map(|(id, _)| id.as_str()).collect();
                    self.backend.report_fault(&format!("card_view {want:?} isn't a card view in byob-plugin.json (card views: {})", ids.join(", ")));
                    return;
                }
            },
            None => match self.cards.first() {
                Some(c) => c,
                None => {
                    self.backend.report_fault("a tool returned a \"card\", but byob-plugin.json declares no view with \"site\": \"card\"");
                    return;
                }
            },
        };
        let (view, web) = (card.0.clone(), card.1);
        let key = match o.get("card_key").and_then(Value::as_str) {
            Some(k) if !k.is_empty() => k.chars().take(byob_protocol::sites::MAX_VIEW_KEY_CHARS).collect(),
            _ => format!("{}-{}", self.name, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())),
        };
        if web {
            let text = o.get("text").and_then(Value::as_str).unwrap_or_default().to_owned();
            let card = byob_protocol::web::WebCard { plugin: self.plugin.id.clone(), view, key, data: tree.clone(), text };
            ctx.attach_view(byob_protocol::View::of(&card));
            return;
        }
        let (node, _faults) = byob_protocol::tree::decode(tree.clone());
        let mut shown = byob_protocol::View::of(&node);
        shown.owner = Some(byob_protocol::ViewRef::new(&self.plugin.id, &view, &key));
        // Only while a step takes it: a call from the plugin's own page shows no card.
        if ctx.attach_view(shown) {
            self.backend.post(json!({"type": CARD_MSG, "view": view, "key": key, "tree": tree}));
        }
    }
}

/// The sink message type of a tree card a backend tool attached to its step (`{"type":
/// "_tool_card", "view", "key", "tree"}`): the engine caches it as a live view.
pub const CARD_MSG: &str = "_tool_card";

/// Most artifact versions one tool result saves.
const MAX_SAVED: usize = 4;

/// A tool reply's `artifact` (or `artifacts`, a list): `{"id"?, "title"?, "filename"?,
/// "file", "note"?}` — `file` is a path (absolute, or in the workspace) whose bytes become a
/// new version of artifact `id` (a new artifact without one). Each saved version is a file
/// change of the call (so the step shows the artifact and clients hear of it) and a line
/// for the model; a failure is a line too.
fn save_artifacts(v: &Value, types: &byob_tools::ArtifactTypes, ctx: &ToolContext<'_>, out: &mut ToolOutput) {
    let Some(o) = v.as_object() else { return };
    let items: Vec<&Value> = match (o.get("artifact"), o.get("artifacts")) {
        (Some(a), _) if a.is_object() => vec![a],
        (_, Some(Value::Array(list))) => list.iter().filter(|a| a.is_object()).take(MAX_SAVED).collect(),
        _ => return,
    };
    let chat = ctx.chat().unwrap_or_default();
    for a in items {
        let s = |k: &str| a.get(k).and_then(Value::as_str);
        let result = (|| {
            let file = s("file").filter(|f| !f.is_empty()).ok_or("the artifact has no \"file\" (the path of its bytes)")?;
            let path = if Path::new(file).is_absolute() { PathBuf::from(file) } else { ctx.workspace.join(file) };
            let bytes = std::fs::metadata(&path)
                .map_err(|e| format!("{file}: {e}"))
                .and_then(|m| if m.len() > byob_tools::artifact::MAX_FILE_BYTES { Err(format!("{file} is too big")) } else { Ok(()) })
                .and_then(|()| std::fs::read(&path).map_err(|e| format!("{file}: {e}")))?;
            let save = byob_tools::artifact::SaveBytes {
                chat,
                id: s("id"),
                title: s("title"),
                filename: s("filename"),
                bytes: &bytes,
                note: s("note").unwrap_or_default(),
                replace: None,
            };
            byob_tools::artifact::save_bytes(types, ctx.workspace, &save)
        })();
        match result {
            Ok(saved) => {
                out.text.push_str(&format!(
                    "\nSaved: artifact {}, version {} ({}){}.",
                    saved.artifact.id,
                    saved.version,
                    saved.artifact.filename,
                    if saved.created { "; a new artifact, use this id from now on" } else { "" }
                ));
                out.changes.push(FileChange::Created(saved.file));
            }
            Err(e) => {
                out.text.push_str(&format!("\nThe result couldn't be saved as an artifact: {e}"));
                out.failed = true;
            }
        }
    }
}

/// A tool reply as tool output: text, `{"text": …, "files": […]}`, or any JSON.
fn tool_output(v: Value, workspace: &Path) -> ToolOutput {
    let (text, files) = match v {
        Value::String(s) => (s, Vec::new()),
        Value::Null => (String::new(), Vec::new()),
        Value::Object(o) if ["text", "files", "card", "artifact", "artifacts"].iter().any(|k| o.contains_key(*k)) => {
            let text = o.get("text").map(|t| t.as_str().map_or_else(|| t.to_string(), str::to_owned)).unwrap_or_default();
            let files = o.get("files").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
            (text, files)
        }
        other => (other.to_string(), Vec::new()),
    };
    let changes = files.iter().filter(|f| path_inside(f)).map(|f| workspace.join(f)).filter(|p| p.is_file()).map(FileChange::Modified).collect();
    let text = if text.trim().is_empty() { "(no result)".to_owned() } else { text };
    ToolOutput { text, failed: false, changes }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(input: &[u8], max: usize) -> Vec<Result<String, LineRead>> {
        let mut r = io::BufReader::with_capacity(3, input);
        let mut buf = Vec::new();
        let mut out = Vec::new();
        loop {
            match read_line(&mut r, &mut buf, max).unwrap() {
                LineRead::Line => out.push(Ok(String::from_utf8(buf.clone()).unwrap())),
                LineRead::Eof => return out,
                other => out.push(Err(other)),
            }
        }
    }

    #[test]
    fn long_lines_are_dropped_not_fatal() {
        assert_eq!(lines(b"ab\r\nwaaaaaaaaaaay too long\nok", 8), vec![Ok("ab".into()), Err(LineRead::TooLong), Ok("ok".into())]);
    }

    #[test]
    fn ring_keeps_the_tail() {
        let mut r = Ring::default();
        r.push(&vec![b'a'; STDERR_KEEP]);
        r.push(b"\nlast line\n");
        assert_eq!(r.0.len(), STDERR_KEEP);
        assert_eq!(r.last_lines(1), "last line");
    }

    #[test]
    fn tool_replies() {
        let ws = std::env::temp_dir();
        assert_eq!(tool_output(json!("hi"), &ws).text, "hi");
        assert_eq!(tool_output(json!({"text": "t", "files": ["../x"]}), &ws).text, "t");
        assert_eq!(tool_output(json!({"n": 1}), &ws).text, "{\"n\":1}");
        assert_eq!(tool_output(Value::Null, &ws).text, "(no result)");
    }

    #[test]
    fn a_backend_that_cannot_start_fails_its_requests() {
        let sink: Sink = Arc::new(|_| {});
        let b = Backend::new("x", json!({"type": "init"}), &[], Box::new(|_: &dyn Fn() -> bool| Err("no python".into())), sink);
        let e = b.request("tool", json!({}), Duration::from_secs(5), &|| false).unwrap_err();
        assert_eq!(e, "no python");
        assert_eq!(b.status().phase, Phase::Failed);
        assert!(!b.event(json!({"type": "event"})));
    }

    #[test]
    fn faults_reach_the_sink_at_most_once_a_second() {
        let posted = Arc::new(Mutex::new(Vec::new()));
        let p = Arc::clone(&posted);
        let sink: Sink = Arc::new(move |o| {
            if let Out::Msg(m) = o {
                p.lock().unwrap().push(m["text"].as_str().unwrap_or_default().to_owned());
            }
        });
        let b = Backend::new("x", json!({"type": "init"}), &[], Box::new(|_: &dyn Fn() -> bool| Err("-".into())), sink);
        for i in 0..100 {
            b.report_fault(&format!("not json {i}"));
        }
        // every one is in the status (the newest), the sink heard of the first only
        assert_eq!(b.status().faults.last().map(String::as_str), Some("not json 99"));
        assert_eq!(b.status().faults.len(), MAX_FAULTS);
        assert_eq!(*posted.lock().unwrap(), ["not json 0"]);
        // then of the rest, once
        let deadline = Instant::now() + Duration::from_secs(5);
        while posted.lock().unwrap().len() < 2 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        thread::sleep(Duration::from_millis(200));
        assert_eq!(*posted.lock().unwrap(), ["not json 0", "not json 99 (98 more)"]);
    }

    /// A backend that stays starting until stopped; `true` in the flag once its
    /// preparation saw that.
    fn never_starts() -> (Backend, Arc<std::sync::atomic::AtomicBool>) {
        let saw = Arc::new(AtomicBool::new(false));
        let s = Arc::clone(&saw);
        let prepare = Box::new(move |called_off: &dyn Fn() -> bool| {
            while !called_off() {
                thread::sleep(Duration::from_millis(5));
            }
            s.store(true, Ordering::SeqCst);
            Err("called off".to_owned())
        });
        (Backend::new("x", json!({"type": "init"}), &[], prepare, Arc::new(|_| {})), saw)
    }

    #[test]
    fn a_stop_calls_off_a_long_preparation() {
        let (b, saw) = never_starts();
        b.start();
        thread::sleep(Duration::from_millis(50));
        assert_eq!(b.status().phase, Phase::Starting);
        b.stop("reloaded");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !saw.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "the preparation went on");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_slow_start_keeps_a_bounded_number_of_bytes() {
        let (b, _saw) = never_starts();
        let big = "x".repeat(MAX_LINE - 100);
        for _ in 0..40 {
            assert!(b.event(json!({"type": "event", "event": "client", "msg": big})));
        }
        let g = b.shared.lock();
        assert!(g.queued_bytes <= MAX_QUEUED_BYTES, "{}", g.queued_bytes);
        assert_eq!(g.queued_bytes, g.queued.iter().map(Vec::len).sum::<usize>());
        assert!(g.queued.len() < 40);
    }
}
