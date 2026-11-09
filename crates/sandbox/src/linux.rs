//! Linux: namespaces (where allowed), rlimits, Landlock, then the command.
//!
//! - **Namespaces** (best effort): a new user namespace (uid/gid mapped to themselves)
//!   holding a PID namespace and, when network is denied, an empty network namespace (only
//!   a down loopback: TCP, UDP, ICMP all fail). The helper then starts *itself* as the
//!   namespace's PID 1 (`--init`): a minimal init that applies Landlock, starts the
//!   command as PID 2, reaps orphans, and exits with the command's status. When PID 1
//!   exits the kernel kills everything left in the namespace — even processes that called
//!   `setsid` to leave the process group. The outer helper just waits and forwards the
//!   status.
//! - **Landlock** (required): filesystem allow-list from the policy; TCP bind/connect
//!   denied when network is denied (ABI ≥ 4); abstract-unix-socket and signal scoping
//!   (ABI ≥ 6, so the command can't signal or connect to abstract sockets of processes
//!   outside the sandbox). Also sets `no_new_privs`: setuid binaries can't gain privileges.
//!
//! Without user namespaces the helper applies Landlock itself and `exec`s the command:
//! network denial is Landlock's TCP-only one and the process tree is only as killable as
//! its process group.

use std::ffi::{OsStr, OsString};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Command, ExitCode};

use landlock::{
    ABI, Access, AccessFs, AccessNet, BitFlags, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus, Scope,
};
use nix::errno::Errno;
use nix::sched::{CloneFlags, unshare};
use nix::sys::resource::{Resource, getrlimit, setrlimit};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::Pid;

use crate::helper::{Enforcement, HELPER_ARG, INIT_ARG, exec_failed, fail};
use crate::policy::{Limits, Policy};

/// Newest Landlock ABI we ask for; older kernels get what they support.
const ABI_WANTED: ABI = ABI::V6;

pub fn probe() -> Enforcement {
    let policy = Policy { allow_unsandboxed: true, ..Default::default() };
    let ns = enter_namespaces(true);
    let mut e = assess(&policy, &ns).0;
    if e.filesystem != "none" {
        match landlock(&policy) {
            Ok(status) => e.filesystem = level(status).into(),
            Err(err) => {
                e.filesystem = "none".into();
                e.notes.push(err);
            }
        }
    }
    e
}

pub fn run(policy: &Policy, program: &OsStr, args: &[OsString]) -> ExitCode {
    let ns = if policy.skip_namespaces { Err("turned off by the policy".into()) } else { enter_namespaces(!policy.network) };
    let (_, verdict) = assess(policy, &ns);
    if let Err(e) = verdict {
        return fail(&e);
    }
    if let Err(e) = apply_limits(&policy.limits, ns.is_ok()) {
        return fail(&e);
    }
    if ns.is_ok() {
        // Our first child becomes PID 1 of the new namespace: make it our own init.
        let mut init = Command::new("/proc/self/exe");
        init.arg(HELPER_ARG).arg(INIT_ARG).arg(policy.to_json()).arg("--").arg(program).args(args);
        return match init.spawn().and_then(|mut c| c.wait()) {
            Ok(status) => exit_code(status.code(), status.signal()),
            Err(e) => fail(&format!("can't start the sandbox init: {e}")),
        };
    }
    if let Err(e) = landlock(policy).and_then(|s| require_fs(policy, s)) {
        return fail(&e);
    }
    let e = command(policy, program, args).exec(); // only returns on failure
    exec_failed(program, &e)
}

/// PID 1 inside the namespaces: confine, start the command, reap until it exits.
pub fn init(policy: &Policy, program: &OsStr, args: &[OsString]) -> ExitCode {
    // If the outer helper is killed on its own (e.g. a client that kills only the process
    // it started), take the namespace down with it: PID 1 dying kills everything inside.
    if let Err(e) = nix::sys::prctl::set_pdeathsig(nix::sys::signal::Signal::SIGKILL) {
        return fail(&format!("prctl(PR_SET_PDEATHSIG): {e}"));
    }
    if let Err(e) = landlock(policy).and_then(|s| require_fs(policy, s)) {
        return fail(&e);
    }
    let child = match command(policy, program, args).spawn() {
        Ok(c) => c,
        Err(e) => return exec_failed(program, &e),
    };
    let Ok(raw) = i32::try_from(child.id()) else { return fail("bad child pid") };
    let main = Pid::from_raw(raw);
    loop {
        match waitpid(None, None) {
            Ok(WaitStatus::Exited(pid, code)) if pid == main => return exit_code(Some(code), None),
            Ok(WaitStatus::Signaled(pid, sig, _)) if pid == main => return exit_code(None, Some(sig as i32)),
            Ok(_) | Err(Errno::EINTR) => {}
            Err(_) => return ExitCode::from(1),
        }
    }
}

fn command(policy: &Policy, program: &OsStr, args: &[OsString]) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(args);
    for name in &policy.remove_env {
        cmd.env_remove(name);
    }
    cmd
}

fn exit_code(code: Option<i32>, signal: Option<i32>) -> ExitCode {
    match (code, signal) {
        (Some(code), _) => ExitCode::from(u8::try_from(code & 0xff).unwrap_or(1)),
        (None, Some(sig)) => ExitCode::from(u8::try_from(128 + (sig & 0x7f)).unwrap_or(1)),
        (None, None) => ExitCode::from(1),
    }
}

fn level(s: RulesetStatus) -> &'static str {
    match s {
        RulesetStatus::FullyEnforced => "full",
        RulesetStatus::PartiallyEnforced => "partial",
        RulesetStatus::NotEnforced => "none",
    }
}

fn require_fs(policy: &Policy, status: RulesetStatus) -> Result<(), String> {
    if status == RulesetStatus::NotEnforced && !policy.allow_unsandboxed {
        return Err("Landlock is not enforced on this kernel".into());
    }
    Ok(())
}

/// What will be enforced, and whether that's enough for `policy`.
fn assess(policy: &Policy, ns: &Result<(), String>) -> (Enforcement, Result<(), String>) {
    let mut e = Enforcement { filesystem: "none".into(), network: if policy.network { "allowed" } else { "none" }.into(), ..Default::default() };
    if let Err(why) = ns {
        e.notes.push(format!("no namespaces ({why}): background processes that leave the process group can outlive a timeout"));
    }
    e.process_tree = ns.is_ok();
    let fs_ok = landlock_fs_supported(AccessFs::from_all(ABI::V1));
    if fs_ok {
        e.filesystem = if landlock_fs_supported(AccessFs::from_all(ABI_WANTED)) { "full" } else { "partial" }.into();
    } else {
        e.notes.push("Landlock is not available in this kernel (needs Linux 5.13+ with Landlock enabled)".into());
    }
    if !policy.network {
        if ns.is_ok() {
            e.network = "namespace".into();
        } else if fs_ok && landlock_tcp_supported() {
            e.network = "tcp-only".into();
            e.notes.push("only TCP is blocked (no network namespace): UDP still works".into());
        } else {
            e.notes.push("network can't be blocked here (needs user namespaces, or Landlock ABI 4 on Linux 6.7+)".into());
        }
    }
    let verdict = if policy.allow_unsandboxed || (e.filesystem != "none" && e.network != "none") {
        Ok(())
    } else {
        Err(format!("can't sandbox here: {}", e.notes.join("; ")))
    };
    (e, verdict)
}

/// New user + PID + IPC (+ network) namespaces, with our uid/gid mapped to themselves.
/// Must run while the process is single-threaded.
fn enter_namespaces(new_net: bool) -> Result<(), String> {
    let uid = nix::unistd::getuid();
    let gid = nix::unistd::getgid();
    let mut flags = CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWPID | CloneFlags::CLONE_NEWIPC;
    if new_net {
        flags |= CloneFlags::CLONE_NEWNET;
    }
    unshare(flags).map_err(|e| format!("unshare: {e}"))?;
    let write = |file: &str, what: String| std::fs::write(Path::new("/proc/self").join(file), what).map_err(|e| format!("writing {file}: {e}"));
    write("setgroups", "deny".into())?;
    write("uid_map", format!("{uid} {uid} 1"))?;
    write("gid_map", format!("{gid} {gid} 1"))?;
    Ok(())
}

fn apply_limits(l: &Limits, own_user_ns: bool) -> Result<(), String> {
    let set = |r: Resource, v: Option<u64>| -> Result<(), String> {
        let Some(v) = v else { return Ok(()) };
        // Only ever lower a limit (raising the hard limit isn't allowed anyway).
        let (_soft, hard) = getrlimit(r).map_err(|e| format!("getrlimit {r:?}: {e}"))?;
        let v = v.min(hard);
        setrlimit(r, v, v).map_err(|e| format!("setrlimit {r:?}: {e}"))
    };
    set(Resource::RLIMIT_CPU, l.cpu_seconds)?;
    set(Resource::RLIMIT_AS, l.memory_bytes)?;
    set(Resource::RLIMIT_FSIZE, l.file_size_bytes)?;
    set(Resource::RLIMIT_NOFILE, l.open_files)?;
    set(Resource::RLIMIT_CORE, Some(0))?;
    if own_user_ns {
        set(Resource::RLIMIT_NPROC, l.processes)?;
    }
    Ok(())
}

/// Does the kernel handle these filesystem rights? Creates (and drops) a ruleset.
fn landlock_fs_supported(access: BitFlags<AccessFs>) -> bool {
    Ruleset::default().set_compatibility(CompatLevel::HardRequirement).handle_access(access).and_then(|r| r.create()).is_ok()
}

/// Does the kernel handle TCP rights (ABI ≥ 4)?
fn landlock_tcp_supported() -> bool {
    Ruleset::default().set_compatibility(CompatLevel::HardRequirement).handle_access(AccessNet::ConnectTcp).and_then(|r| r.create()).is_ok()
}

/// Restrict this process (and everything it starts) to the policy's paths.
fn landlock(policy: &Policy) -> Result<RulesetStatus, String> {
    let abi = ABI_WANTED;
    let err = |e: landlock::RulesetError| format!("landlock: {e}");
    let mut ruleset = Ruleset::default().handle_access(AccessFs::from_all(abi)).map_err(err)?;
    if !policy.network {
        ruleset = ruleset.handle_access(AccessNet::BindTcp | AccessNet::ConnectTcp).map_err(err)?;
    }
    ruleset = ruleset.scope(Scope::AbstractUnixSocket | Scope::Signal).map_err(err)?;
    let mut created = ruleset.create().map_err(err)?;
    for (paths, access) in [(&policy.read, AccessFs::from_read(abi)), (&policy.write, AccessFs::from_all(abi))] {
        for path in paths {
            let Ok(meta) = std::fs::metadata(path) else { continue };
            let access = if meta.is_dir() { access } else { access & AccessFs::from_file(abi) };
            let Ok(fd) = PathFd::new(path) else { continue };
            created = created.add_rule(PathBeneath::new(fd, access)).map_err(err)?;
        }
    }
    let status = created.restrict_self().map_err(err)?;
    if !status.no_new_privs {
        return Err("couldn't set no_new_privs".into());
    }
    Ok(status.ruleset)
}
