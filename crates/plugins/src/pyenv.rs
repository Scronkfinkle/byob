//! The Python a plugin's scripts and backend run with.
//!
//! Every plugin gets byob's helper module (`byob_plugin.py`, [`HELPER`]) on `PYTHONPATH`.
//! A plugin that lists `python.requirements` gets its own virtual environment, cached under
//! byob's data folder and keyed by a hash of the interpreter and the requirements (so two
//! plugins with the same list share one, and changing the list makes a new one). It is
//! built the first time something needs it, never on the engine thread: with `uv` when it
//! is on `PATH` (`uv venv` + `uv pip install`), else `python -m venv` + `pip install`. A
//! failure is kept (with the installer's output) until the plugin is reloaded, and shows
//! as one of the plugin's problems. A build can be called off (the tool call stopped, the
//! backend stopped); it isn't kept as a failure then.
//!
//! Two byob processes may share the data folder: a build holds a lock file next to the
//! environment (`<venv>.lock`), and the other waits for it and uses what it built. A lock
//! whose holder stopped refreshing it (a byob that died mid-build) is taken over.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::hash::Hasher;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock, PoisonError, TryLockError};
use std::time::{Duration, Instant, SystemTime};

use byob_sandbox::RunOptions;

use crate::authoring::Fnv;

/// byob's Python helper for plugins (`import byob_plugin`).
pub const HELPER: &str = include_str!("byob_plugin.py");
/// Its file name.
pub const HELPER_FILE: &str = "byob_plugin.py";
/// Longest an environment may take to build (downloads included).
const BUILD_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Installer output kept per stream.
const MAX_BUILD_OUTPUT: usize = 64 * 1024;
/// Characters of the installer's output in a problem message.
const PROBLEM_OUTPUT: usize = 3000;
/// The file that says an environment is complete.
const READY: &str = ".byob-ready";
/// What a build that was called off says.
pub const BUILD_CALLED_OFF: &str = "the Python environment wasn't built: stopped";
/// How often a build refreshes its lock file.
const LOCK_BEAT: Duration = Duration::from_secs(20);
/// A lock file not refreshed for this long is left over from a byob that died.
const LOCK_STALE: Duration = Duration::from_secs(120);
/// How often a waiting build looks at the lock again.
const LOCK_POLL: Duration = Duration::from_millis(100);

/// How to start Python for one plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Python {
    /// Interpreter and leading arguments.
    pub program: Vec<OsString>,
    /// Folders a sandbox must let it read (the venv, the helper).
    pub read: Vec<PathBuf>,
    /// Variables to set (`PYTHONPATH`).
    pub env: BTreeMap<String, String>,
}

/// Every plugin environment of one engine.
pub struct PyEnvs {
    root: PathBuf,
    base: Vec<OsString>,
    /// `None`: look for `uv` on `PATH`; `Some(None)`: never use it.
    uv: Option<Option<PathBuf>>,
    offline: bool,
    helper: OnceLock<Result<PathBuf, String>>,
    envs: Mutex<BTreeMap<String, Arc<PyEnv>>>,
}

impl PyEnvs {
    /// Environments go under `root` (e.g. `<data>/plugin-python`); `base` is the
    /// interpreter they are made from (and the one plugins without requirements use).
    pub fn new(root: impl Into<PathBuf>, base: Vec<OsString>) -> Self {
        Self { root: root.into(), base, uv: None, offline: false, helper: OnceLock::new(), envs: Mutex::new(BTreeMap::new()) }
    }

    /// Use this `uv` (`None`: pip only) instead of looking for one.
    pub fn with_uv(mut self, uv: Option<PathBuf>) -> Self {
        self.uv = Some(uv);
        self
    }

    /// Install without a package index (local wheels and folders only; tests).
    pub fn offline(mut self) -> Self {
        self.offline = true;
        self
    }

    pub fn base(&self) -> &[OsString] {
        &self.base
    }

    /// The folder holding `byob_plugin.py`, written there the first time it is asked for.
    pub fn helper_dir(&self) -> Result<PathBuf, String> {
        self.helper
            .get_or_init(|| {
                let dir = self.root.join("lib");
                let file = dir.join(HELPER_FILE);
                if fs::read_to_string(&file).ok().as_deref() != Some(HELPER) {
                    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
                    let tmp = dir.join(format!(".{HELPER_FILE}.{}", std::process::id()));
                    fs::write(&tmp, HELPER).and_then(|()| fs::rename(&tmp, &file)).map_err(|e| format!("{}: {e}", file.display()))?;
                }
                Ok(dir)
            })
            .clone()
    }

    /// The environment for a plugin in `dir` that needs `requirements` (relative paths,
    /// like `./wheels/x.whl`, are taken from the plugin folder).
    pub fn for_plugin(&self, requirements: &[String], dir: &Path) -> Arc<PyEnv> {
        let reqs: Vec<String> =
            requirements.iter().map(|r| if r.starts_with("./") || r.starts_with("../") { dir.join(r).display().to_string() } else { r.clone() }).collect();
        let mut h = Fnv::default();
        for p in &self.base {
            h.write(p.as_encoded_bytes());
            h.write_u8(0);
        }
        let mut sorted = reqs.clone();
        sorted.sort();
        for r in &sorted {
            h.write(r.as_bytes());
            h.write_u8(1);
        }
        let key = format!("{:016x}", h.finish());
        let mut envs = self.envs.lock().unwrap_or_else(PoisonError::into_inner);
        let env = envs.entry(key.clone()).or_insert_with(|| {
            let uv = match &self.uv {
                Some(u) => u.clone(),
                None => byob_sandbox::which(std::ffi::OsStr::new("uv")),
            };
            Arc::new(PyEnv {
                dir: self.root.join("venvs").join(&key),
                reqs,
                base: self.base.clone(),
                uv,
                offline: self.offline,
                state: Mutex::new(State::Unknown),
                build: Mutex::new(()),
            })
        });
        Arc::clone(env)
    }

    /// Forget failed builds so they are tried again (a reload).
    pub fn forget_failed(&self) {
        self.envs.lock().unwrap_or_else(PoisonError::into_inner).retain(|_, e| e.failure().is_none());
    }

    /// How to start Python for a plugin with this environment (`None`: no requirements).
    /// Builds the environment if needed, until `cancelled()`: call it off the engine thread.
    pub fn python(&self, env: Option<&PyEnv>, cancelled: &dyn Fn() -> bool) -> Result<Python, String> {
        let helper = self.helper_dir();
        let mut read = Vec::new();
        let mut vars = BTreeMap::new();
        match &helper {
            Ok(dir) => {
                let mut path = dir.display().to_string();
                if let Some(old) = std::env::var_os("PYTHONPATH").filter(|p| !p.is_empty()) {
                    path.push(if cfg!(windows) { ';' } else { ':' });
                    path.push_str(&old.to_string_lossy());
                }
                vars.insert("PYTHONPATH".to_owned(), path);
                read.push(dir.clone());
            }
            Err(e) => log::warn!("the byob_plugin helper isn't available: {e}"),
        }
        vars.insert("PYTHONUNBUFFERED".to_owned(), "1".to_owned());
        // UTF-8 on stdin/stdout and in files even where the locale says otherwise (Windows)
        vars.insert("PYTHONUTF8".to_owned(), "1".to_owned());
        vars.insert("PYTHONIOENCODING".to_owned(), "utf-8".to_owned());
        let program = match env {
            None => self.base.clone(),
            Some(env) => {
                let python = env.ensure(cancelled)?;
                read.push(env.dir.clone());
                vec![python.into_os_string()]
            }
        };
        Ok(Python { program, read, env: vars })
    }
}

/// One plugin's Python: byob's helper, and its own environment if it has requirements.
#[derive(Clone)]
pub struct PluginPython {
    pub envs: Arc<PyEnvs>,
    pub env: Option<Arc<PyEnv>>,
}

impl PluginPython {
    /// For a plugin in `dir` with these requirements.
    pub fn new(envs: &Arc<PyEnvs>, requirements: &[String], dir: &Path) -> Self {
        let env = (!requirements.is_empty()).then(|| envs.for_plugin(requirements, dir));
        Self { envs: Arc::clone(envs), env }
    }

    /// See [`PyEnvs::python`] (may build the environment: not on the engine thread).
    pub fn python(&self) -> Result<Python, String> {
        self.python_until(&|| false)
    }

    /// [`Self::python`] whose build gives up once `cancelled()`.
    pub fn python_until(&self, cancelled: &dyn Fn() -> bool) -> Result<Python, String> {
        self.envs.python(self.env.as_deref(), cancelled)
    }

    /// Why its environment couldn't be built (never blocks).
    pub fn failure(&self) -> Option<String> {
        self.env.as_ref().and_then(|e| e.failure())
    }
}

#[derive(Debug, Clone)]
enum State {
    Unknown,
    Ready(PathBuf),
    Failed(String),
}

/// One virtual environment.
pub struct PyEnv {
    dir: PathBuf,
    reqs: Vec<String>,
    base: Vec<OsString>,
    uv: Option<PathBuf>,
    offline: bool,
    state: Mutex<State>,
    /// Held while building, so one build runs at a time (in this process; the lock file
    /// covers others).
    build: Mutex<()>,
}

impl PyEnv {
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn requirements(&self) -> &[String] {
        &self.reqs
    }

    /// Why the last build failed (never blocks).
    pub fn failure(&self) -> Option<String> {
        match &*self.state.lock().unwrap_or_else(PoisonError::into_inner) {
            State::Failed(e) => Some(e.clone()),
            _ => None,
        }
    }

    /// It is built (never blocks).
    pub fn ready(&self) -> bool {
        matches!(&*self.state.lock().unwrap_or_else(PoisonError::into_inner), State::Ready(_)) || self.dir.join(READY).is_file()
    }

    fn set(&self, s: State) {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner) = s;
    }

    fn get(&self) -> State {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// The environment's interpreter, building it first if needed (blocks while it builds,
    /// or another byob does, until `cancelled()`: then [`BUILD_CALLED_OFF`]).
    pub fn ensure(&self, cancelled: &dyn Fn() -> bool) -> Result<PathBuf, String> {
        match self.get() {
            State::Ready(p) => return Ok(p),
            State::Failed(e) => return Err(e),
            State::Unknown => {}
        }
        let _building = loop {
            match self.build.try_lock() {
                Ok(g) => break g,
                Err(TryLockError::Poisoned(p)) => break p.into_inner(),
                Err(TryLockError::WouldBlock) if cancelled() => return Err(BUILD_CALLED_OFF.into()),
                Err(TryLockError::WouldBlock) => std::thread::sleep(LOCK_POLL),
            }
        };
        // Someone else may have finished while we waited.
        match self.get() {
            State::Ready(p) => return Ok(p),
            State::Failed(e) => return Err(e),
            State::Unknown => {}
        }
        let python = venv_python(&self.dir);
        if self.built(&python) {
            self.set(State::Ready(python.clone()));
            return Ok(python);
        }
        if let Some(parent) = self.dir.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        let lock = BuildLock::take(&self.dir.with_extension("lock"), cancelled)?;
        // Another byob may have built it while we waited.
        if self.built(&python) {
            self.set(State::Ready(python.clone()));
            return Ok(python);
        }
        log::info!("building a Python environment for {} in {}", self.reqs.join(" "), self.dir.display());
        let called_off = || {
            lock.beat();
            cancelled()
        };
        match self.build_now(&python, &called_off) {
            Ok(()) => {
                self.set(State::Ready(python.clone()));
                Ok(python)
            }
            Err(e) => {
                let _ = fs::remove_dir_all(&self.dir);
                if cancelled() {
                    // not a failure: the next use builds it again
                    return Err(BUILD_CALLED_OFF.into());
                }
                let e = format!("python.requirements couldn't be installed: {e}");
                self.set(State::Failed(e.clone()));
                Err(e)
            }
        }
    }

    fn built(&self, python: &Path) -> bool {
        self.dir.join(READY).is_file() && python.exists()
    }

    fn build_now(&self, python: &Path, cancelled: &dyn Fn() -> bool) -> Result<(), String> {
        let _ = fs::remove_dir_all(&self.dir);
        let Some((base, lead)) = self.base.split_first() else { return Err("no Python interpreter configured".into()) };
        match &self.uv {
            Some(uv) => {
                let mut c = Command::new(uv);
                c.arg("venv").arg("--quiet").arg("--python").arg(base).arg(&self.dir);
                run(c, "uv venv", cancelled)?;
                let mut c = Command::new(uv);
                c.args(["pip", "install", "--python"]).arg(python).args(self.offline.then_some("--offline")).args(&self.reqs);
                run(c, "uv pip install", cancelled)?;
            }
            None => {
                let mut c = Command::new(base);
                c.args(lead).args(["-m", "venv"]).arg(&self.dir);
                run(c, "python -m venv", cancelled)?;
                let mut c = Command::new(python);
                c.args(["-m", "pip", "install", "--disable-pip-version-check", "--no-input"]).args(self.offline.then_some("--no-index")).args(&self.reqs);
                run(c, "pip install", cancelled)?;
            }
        }
        fs::write(self.dir.join(READY), self.reqs.join("\n")).map_err(|e| format!("{}: {e}", self.dir.display()))
    }
}

/// The lock file of one environment's build, shared by every byob using the data folder:
/// created (exclusively) to build, refreshed while building, removed when done.
struct BuildLock {
    path: PathBuf,
    file: Option<fs::File>,
    beat: std::cell::Cell<Instant>,
}

impl BuildLock {
    /// Take the lock at `path`, waiting while another build holds it (until `cancelled()`).
    fn take(path: &Path, cancelled: &dyn Fn() -> bool) -> Result<Self, String> {
        let mut waited = false;
        loop {
            match fs::OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(mut file) => {
                    let _ = std::io::Write::write_all(&mut file, format!("{}\n", std::process::id()).as_bytes());
                    return Ok(Self { path: path.to_path_buf(), file: Some(file), beat: std::cell::Cell::new(Instant::now()) });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let age = fs::metadata(path).and_then(|m| m.modified()).ok().and_then(|t| SystemTime::now().duration_since(t).ok());
                    if age.is_some_and(|a| a > LOCK_STALE) {
                        log::warn!("{} is left over from a build that stopped; taking it over", path.display());
                        let _ = fs::remove_file(path);
                        continue;
                    }
                    if !waited {
                        log::info!("waiting for another byob to build the Python environment ({})", path.display());
                        waited = true;
                    }
                }
                Err(e) => return Err(format!("{}: {e}", path.display())),
            }
            if cancelled() {
                return Err(BUILD_CALLED_OFF.into());
            }
            std::thread::sleep(LOCK_POLL);
        }
    }

    /// Say the build is still going (at most every [`LOCK_BEAT`]).
    fn beat(&self) {
        if self.beat.get().elapsed() >= LOCK_BEAT {
            if let Some(f) = &self.file {
                let _ = f.set_modified(SystemTime::now());
            }
            self.beat.set(Instant::now());
        }
    }
}

impl Drop for BuildLock {
    fn drop(&mut self) {
        drop(self.file.take());
        let _ = fs::remove_file(&self.path);
    }
}

fn venv_python(dir: &Path) -> PathBuf {
    if cfg!(windows) { dir.join("Scripts").join("python.exe") } else { dir.join("bin").join("python") }
}

fn run(cmd: Command, what: &str, cancelled: &dyn Fn() -> bool) -> Result<(), String> {
    let opts = RunOptions { stdin: Vec::new(), timeout: BUILD_TIMEOUT, max_output: MAX_BUILD_OUTPUT };
    let o = byob_sandbox::run(cmd, &opts, cancelled).map_err(|e| format!("{what}: {e}"))?;
    if o.success() {
        return Ok(());
    }
    if o.cancelled {
        return Err(BUILD_CALLED_OFF.into());
    }
    let out = format!("{}\n{}", o.stdout.text().trim(), o.stderr.text().trim());
    let out = out.trim();
    let start = out.char_indices().rev().nth(PROBLEM_OUTPUT).map_or(0, |(i, _)| i);
    let why = if o.timed_out { "took too long".to_owned() } else { format!("exit code {}", o.exit_code.map_or_else(|| "none".into(), |c| c.to_string())) };
    Err(format!("{what} failed ({why}):\n{}", out.get(start..).unwrap_or(out)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_requirements_share_an_environment() {
        let root = tempfile::tempdir().unwrap();
        let envs = PyEnvs::new(root.path(), crate::default_python()).with_uv(None);
        let a = envs.for_plugin(&["x==1".into(), "y".into()], Path::new("/p/a"));
        let b = envs.for_plugin(&["y".into(), "x==1".into()], Path::new("/p/b"));
        let c = envs.for_plugin(&["x==2".into()], Path::new("/p/a"));
        assert_eq!(a.dir(), b.dir());
        assert_ne!(a.dir(), c.dir());
        let w = envs.for_plugin(&["./wheels/w.whl".into()], Path::new("/p/a"));
        assert_eq!(w.requirements(), ["/p/a/./wheels/w.whl"]);
    }

    #[test]
    fn helper_is_written_once_and_on_the_path() {
        let root = tempfile::tempdir().unwrap();
        let envs = PyEnvs::new(root.path(), crate::default_python());
        let py = envs.python(None, &|| false).unwrap();
        let dir = envs.helper_dir().unwrap();
        assert_eq!(fs::read_to_string(dir.join(HELPER_FILE)).unwrap(), HELPER);
        assert!(py.env["PYTHONPATH"].starts_with(&dir.display().to_string()));
        assert_eq!(py.program, crate::default_python());
    }

    #[test]
    fn python_speaks_utf8_whatever_the_locale() {
        let root = tempfile::tempdir().unwrap();
        let py = PyEnvs::new(root.path(), crate::default_python()).python(None, &|| false).unwrap();
        assert_eq!(py.env.get("PYTHONUTF8").map(String::as_str), Some("1"));
        assert_eq!(py.env.get("PYTHONIOENCODING").map(String::as_str), Some("utf-8"));
    }

    /// An environment whose interpreter doesn't exist: a build fails at once.
    fn broken(root: &Path) -> Arc<PyEnv> {
        PyEnvs::new(root, vec!["/definitely/not/python".into()]).with_uv(None).for_plugin(&["x".into()], Path::new("/p"))
    }

    #[test]
    fn another_byobs_build_is_waited_for_and_used_not_clobbered() {
        let root = tempfile::tempdir().unwrap();
        let env = broken(root.path());
        let lock = env.dir().with_extension("lock");
        fs::create_dir_all(env.dir()).unwrap();
        fs::write(&lock, "4242\n").unwrap();
        // the other byob finishes its build, then lets go of the lock
        let dir = env.dir().to_path_buf();
        let other = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            let python = venv_python(&dir);
            fs::create_dir_all(python.parent().unwrap()).unwrap();
            fs::write(&python, "").unwrap();
            fs::write(dir.join(READY), "x").unwrap();
            fs::remove_file(dir.with_extension("lock")).unwrap();
        });
        assert_eq!(env.ensure(&|| false), Ok(venv_python(env.dir())));
        other.join().unwrap();
        assert!(env.dir().join(READY).is_file(), "its build was left alone");
    }

    #[test]
    fn waiting_for_another_build_can_be_called_off() {
        let root = tempfile::tempdir().unwrap();
        let env = broken(root.path());
        fs::create_dir_all(env.dir()).unwrap();
        fs::write(env.dir().join("theirs"), "").unwrap();
        fs::write(env.dir().with_extension("lock"), "4242\n").unwrap();
        let t = Instant::now();
        assert_eq!(env.ensure(&|| t.elapsed() > Duration::from_millis(200)), Err(BUILD_CALLED_OFF.to_owned()));
        assert!(env.dir().join("theirs").exists(), "their half-built environment was left alone");
        assert!(env.failure().is_none(), "not a failure: tried again next time");
    }

    #[test]
    fn a_lock_left_by_a_byob_that_died_is_taken_over() {
        let root = tempfile::tempdir().unwrap();
        let env = broken(root.path());
        let lock = env.dir().with_extension("lock");
        fs::create_dir_all(env.dir()).unwrap();
        let f = fs::File::create(&lock).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(3600)).unwrap();
        drop(f);
        let e = env.ensure(&|| false).unwrap_err();
        assert!(e.starts_with("python.requirements couldn't be installed"), "{e}");
        assert!(!lock.exists(), "the lock goes with the build");
    }

    #[cfg(unix)]
    #[test]
    fn a_build_can_be_called_off() {
        let root = tempfile::tempdir().unwrap();
        // an "interpreter" whose `-m venv` takes half a minute
        let base: Vec<OsString> = vec!["sh".into(), "-c".into(), "sleep 30".into(), "sh".into()];
        let env = PyEnvs::new(root.path(), base).with_uv(None).for_plugin(&["x".into()], Path::new("/p"));
        let t = Instant::now();
        assert_eq!(env.ensure(&|| t.elapsed() > Duration::from_millis(300)), Err(BUILD_CALLED_OFF.to_owned()));
        assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
        assert!(env.failure().is_none());
        assert!(!env.dir().exists());
        assert!(!env.dir().with_extension("lock").exists());
    }
}
