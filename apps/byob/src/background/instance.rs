//! One byob per data folder. Two engines on the same files would mark each other's replies
//! interrupted (each one's startup cleans up after a crash it thinks happened), so the first
//! byob holds `<data>/instance.lock` (an OS file lock, released when the process ends, so a
//! crashed byob's lock is simply free again) and listens on a loopback port. A second launch
//! finds the lock taken, asks the first one to `show` its window, and exits.
//!
//! The port, with the pid and a token, is in `<data>/instance.addr` rather than in the lock
//! file itself: on Windows a locked file can't be read by anyone else.
//!
//! The protocol is one line each way: `<token> show` or `<token> run <routine>`, answered
//! with `ok` or `err <why>`. The token keeps other users' programs on this machine (who can
//! reach loopback too, but not our data folder) from driving it.

use std::fs::{File, OpenOptions, TryLockError};
use std::hash::{BuildHasher, Hasher};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The locked file.
pub const LOCK_FILE: &str = "instance.lock";
/// Where the running byob says how to reach it.
pub const ADDR_FILE: &str = "instance.addr";
/// Longest request line taken.
const MAX_LINE: u64 = 1024;
/// Longest routine name in `run`.
const MAX_NAME: usize = 200;
/// How long a second launch waits for a starting byob to write where it listens.
const WAIT_FOR_ADDR: Duration = Duration::from_secs(5);
/// How long one exchange may take (both sides).
const IO_TIMEOUT: Duration = Duration::from_secs(3);

/// What a launch can ask the running byob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Show the window and bring it to the front.
    Show,
    /// Run a routine now (by name).
    Run(String),
}

impl Request {
    /// The request part of a line (after the token).
    pub fn parse(text: &str) -> Option<Request> {
        let text = text.trim();
        let (verb, rest) = text.split_once(' ').map_or((text, ""), |(v, r)| (v, r.trim()));
        match verb {
            "show" if rest.is_empty() => Some(Request::Show),
            "run" if !rest.is_empty() && rest.chars().count() <= MAX_NAME && !rest.chars().any(char::is_control) => Some(Request::Run(rest.to_owned())),
            _ => None,
        }
    }

    fn to_line(&self) -> String {
        match self {
            Request::Show => "show".to_owned(),
            Request::Run(name) => format!("run {name}"),
        }
    }
}

/// This process is the byob for the data folder: it holds the lock until dropped.
#[derive(Debug)]
pub struct Instance {
    _lock: File,
    addr_file: PathBuf,
    token: String,
}

/// Another byob has the data folder: how to reach it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Running {
    pub pid: u32,
    pub port: u16,
    token: String,
}

#[derive(Debug)]
pub enum Claim {
    First(Instance),
    Taken(Running),
}

/// Take the data folder `dir` for this process, or find who has it.
pub fn claim(dir: &Path) -> Result<Claim, String> {
    claim_waiting(dir, WAIT_FOR_ADDR)
}

fn claim_waiting(dir: &Path, wait: Duration) -> Result<Claim, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let lock_path = dir.join(LOCK_FILE);
    let lock = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&lock_path).map_err(|e| format!("{}: {e}", lock_path.display()))?;
    let addr_file = dir.join(ADDR_FILE);
    match lock.try_lock() {
        Ok(()) => {
            // Whatever it says is from a byob that's gone.
            let _ = std::fs::remove_file(&addr_file);
            Ok(Claim::First(Instance { _lock: lock, addr_file, token: new_token() }))
        }
        Err(TryLockError::WouldBlock) => {
            // Taken: the other byob may still be starting, before it wrote its port.
            let until = Instant::now() + wait;
            loop {
                if let Some(running) = std::fs::read_to_string(&addr_file).ok().and_then(|s| parse_addr(&s)) {
                    return Ok(Claim::Taken(running));
                }
                if Instant::now() >= until {
                    return Err(format!("byob is already running on {} but doesn't say how to reach it ({} is missing)", dir.display(), addr_file.display()));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        Err(TryLockError::Error(e)) => Err(format!("couldn't lock {}: {e}", lock_path.display())),
    }
}

/// 128 bits nobody else can guess: std's hasher keys come from the OS's randomness.
fn new_token() -> String {
    let word = |salt: u64| {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(salt);
        h.write_u32(std::process::id());
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or_default());
        h.finish()
    };
    format!("{:016x}{:016x}", word(1), word(2))
}

fn addr_text(pid: u32, port: u16, token: &str) -> String {
    format!("pid {pid}\nport {port}\ntoken {token}\n")
}

fn parse_addr(text: &str) -> Option<Running> {
    let (mut pid, mut port, mut token) = (None, None, None);
    for line in text.lines() {
        match line.split_once(' ') {
            Some(("pid", v)) => pid = v.trim().parse().ok(),
            Some(("port", v)) => port = v.trim().parse().ok().filter(|p| *p != 0),
            Some(("token", v)) if !v.trim().is_empty() => token = Some(v.trim().to_owned()),
            _ => {}
        }
    }
    Some(Running { pid: pid?, port: port?, token: token? })
}

impl Instance {
    /// Listen on a loopback port for other launches; `handle` answers each request (on the
    /// listener's thread). Returns the port.
    pub fn listen(&self, handle: impl Fn(Request) -> Result<(), String> + Send + 'static) -> Result<u16, String> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|e| format!("couldn't listen on loopback: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        write_private(&self.addr_file, &addr_text(std::process::id(), port, &self.token)).map_err(|e| format!("{}: {e}", self.addr_file.display()))?;
        let token = self.token.clone();
        std::thread::Builder::new()
            .name("byob-instance".into())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    // One bad exchange mustn't end the listener.
                    let served = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| serve(stream, &token, &handle)));
                    if served.is_err() {
                        log::warn!("the instance listener recovered from a panic");
                    }
                }
            })
            .map_err(|e| format!("couldn't start the instance listener: {e}"))?;
        Ok(port)
    }
}

fn serve(stream: TcpStream, token: &str, handle: &dyn Fn(Request) -> Result<(), String>) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let mut line = String::new();
    let Ok(reader) = stream.try_clone() else { return };
    if BufReader::new(reader.take(MAX_LINE)).read_line(&mut line).is_err() {
        return;
    }
    let reply = match line.trim().split_once(' ') {
        Some((t, rest)) if t == token => match Request::parse(rest) {
            Some(req) => match handle(req) {
                Ok(()) => "ok".to_owned(),
                Err(e) => format!("err {}", e.replace(['\r', '\n'], " ")),
            },
            None => "err unknown request".to_owned(),
        },
        _ => "err wrong token".to_owned(),
    };
    let mut stream = stream;
    let _ = writeln!(stream, "{reply}");
}

impl Running {
    /// Ask the running byob; `Ok` once it said ok.
    pub fn send(&self, req: &Request) -> Result<(), String> {
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, self.port));
        let mut stream = TcpStream::connect_timeout(&addr, IO_TIMEOUT).map_err(|e| format!("couldn't reach byob (pid {}) at {addr}: {e}", self.pid))?;
        let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
        let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
        writeln!(stream, "{} {}", self.token, req.to_line()).map_err(|e| e.to_string())?;
        let mut reply = String::new();
        BufReader::new((&stream).take(MAX_LINE)).read_line(&mut reply).map_err(|e| e.to_string())?;
        match reply.trim() {
            "ok" => Ok(()),
            other => Err(other.strip_prefix("err ").unwrap_or(other).to_owned()),
        }
    }
}

/// Write `text` to `path` through a temporary file, readable only by this user on unix.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("addr.tmp");
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut f = opts.open(&tmp)?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn requests_are_one_short_line() {
        assert_eq!(Request::parse("show"), Some(Request::Show));
        assert_eq!(Request::parse(" show \n"), Some(Request::Show));
        assert_eq!(Request::parse("run  morning news "), Some(Request::Run("morning news".into())));
        for bad in ["", "show me", "run", "run ", "quit", "SHOW", "run a\u{7}b"] {
            assert_eq!(Request::parse(bad), None, "{bad:?}");
        }
        assert_eq!(Request::parse(&format!("run {}", "x".repeat(MAX_NAME + 1))), None, "too long");
        for r in [Request::Show, Request::Run("weekly report".into())] {
            assert_eq!(Request::parse(&r.to_line()), Some(r));
        }
    }

    #[test]
    fn the_addr_file_needs_pid_port_and_token() {
        let r = parse_addr(&addr_text(42, 5000, "abc")).unwrap();
        assert_eq!((r.pid, r.port, r.token.as_str()), (42, 5000, "abc"));
        assert_eq!(parse_addr("pid 1\nport 0\ntoken a\n"), None, "port 0");
        assert_eq!(parse_addr("pid 1\nport 5\n"), None, "no token");
        assert_eq!(parse_addr(""), None);
        assert_ne!(new_token(), new_token());
        assert_eq!(new_token().len(), 32);
    }

    #[test]
    fn a_second_claim_finds_the_first_and_reaches_it() {
        let dir = tempfile::tempdir().unwrap();
        let Claim::First(first) = claim(dir.path()).unwrap() else { panic!("the folder was free") };
        let got: Arc<Mutex<Vec<Request>>> = Arc::default();
        let seen = got.clone();
        let port = first
            .listen(move |r| {
                let answer = if matches!(r, Request::Run(_)) { Err("no routines here\nyet".to_owned()) } else { Ok(()) };
                seen.lock().unwrap().push(r);
                answer
            })
            .unwrap();
        // The lock is per open file, so a second open in this process is refused like
        // another process would be.
        let Claim::Taken(other) = claim(dir.path()).unwrap() else { panic!("the folder is taken") };
        assert_eq!((other.pid, other.port), (std::process::id(), port));
        other.send(&Request::Show).unwrap();
        assert_eq!(other.send(&Request::Run("news".into())), Err("no routines here yet".into()));
        assert_eq!(*got.lock().unwrap(), [Request::Show, Request::Run("news".into())]);
        // Without the token, nothing happens.
        let stranger = Running { token: "guess".into(), ..other.clone() };
        assert_eq!(stranger.send(&Request::Show), Err("wrong token".into()));
        let mut raw = TcpStream::connect(("127.0.0.1", port)).unwrap();
        writeln!(raw, "{} dance", other.token).unwrap();
        let mut reply = String::new();
        BufReader::new(raw).read_line(&mut reply).unwrap();
        assert_eq!(reply, "err unknown request\n");
        assert_eq!(got.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_lock_left_behind_is_taken_over() {
        let dir = tempfile::tempdir().unwrap();
        // A byob that crashed: its files are there, its lock went with it.
        std::fs::write(dir.path().join(LOCK_FILE), "").unwrap();
        std::fs::write(dir.path().join(ADDR_FILE), addr_text(999_999, 1, "old")).unwrap();
        let Claim::First(first) = claim(dir.path()).unwrap() else { panic!("a dead byob's lock is free") };
        assert!(!dir.path().join(ADDR_FILE).exists(), "its address is gone");
        drop(first);
        // Released when its holder goes.
        assert!(matches!(claim(dir.path()).unwrap(), Claim::First(_)));
    }

    #[test]
    fn a_holder_that_never_says_where_it_is_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let Claim::First(_first) = claim(dir.path()).unwrap() else { panic!() };
        let err = claim_waiting(dir.path(), Duration::from_millis(200)).unwrap_err();
        assert!(err.contains("already running"), "{err}");
    }
}
