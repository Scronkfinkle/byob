//! macOS: rlimits, then `exec /usr/bin/sandbox-exec -p <profile> -D… <command>`.
//! Compile-checked only; not exercised by the test suite (no macOS CI yet).

use std::ffi::{OsStr, OsString};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitCode};

use nix::sys::resource::{Resource, getrlimit, setrlimit};

use crate::helper::{Enforcement, exec_failed, fail};
use crate::policy::{Limits, Policy};

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

pub fn probe() -> Enforcement {
    if Path::new(SANDBOX_EXEC).exists() {
        Enforcement {
            filesystem: "partial".into(),
            network: "denied".into(),
            process_tree: false,
            notes: vec!["macOS sandbox-exec: writes are an allow-list, reads a deny-list (home, other users, volumes, temp)".into()],
        }
    } else {
        Enforcement::none("/usr/bin/sandbox-exec is missing")
    }
}

/// The `--init` stage only exists on Linux.
pub fn init(_policy: &Policy, _program: &OsStr, _args: &[OsString]) -> ExitCode {
    fail("--init is only used on Linux")
}

pub fn run(policy: &Policy, program: &OsStr, args: &[OsString]) -> ExitCode {
    if let Err(e) = apply_limits(&policy.limits) {
        return fail(&e);
    }
    let mut cmd = if Path::new(SANDBOX_EXEC).exists() {
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from).and_then(|h| h.canonicalize().ok());
        let (profile, params) = crate::seatbelt::profile(policy, home.as_deref());
        let mut c = Command::new(SANDBOX_EXEC);
        c.arg("-p").arg(profile);
        for (k, v) in params {
            c.arg("-D").arg(format!("{k}={v}"));
        }
        c.arg(program).args(args);
        c
    } else if policy.allow_unsandboxed {
        let mut c = Command::new(program);
        c.args(args);
        c
    } else {
        return fail("can't sandbox here: /usr/bin/sandbox-exec is missing");
    };
    for name in &policy.remove_env {
        cmd.env_remove(name);
    }
    let e = cmd.exec();
    exec_failed(program, &e)
}

fn apply_limits(l: &Limits) -> Result<(), String> {
    let set = |r: Resource, v: Option<u64>| -> Result<(), String> {
        let Some(v) = v else { return Ok(()) };
        let (_soft, hard) = getrlimit(r).map_err(|e| format!("getrlimit {r:?}: {e}"))?;
        let v = v.min(hard);
        setrlimit(r, v, v).map_err(|e| format!("setrlimit {r:?}: {e}"))
    };
    set(Resource::RLIMIT_CPU, l.cpu_seconds)?;
    set(Resource::RLIMIT_FSIZE, l.file_size_bytes)?;
    set(Resource::RLIMIT_NOFILE, l.open_files)?;
    set(Resource::RLIMIT_CORE, Some(0))?;
    // RLIMIT_AS is not enforced by macOS; RLIMIT_NPROC is per user system-wide.
    Ok(())
}
