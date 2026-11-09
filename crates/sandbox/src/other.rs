//! Platforms without a sandbox yet (Windows, BSDs): refuse, unless the user opted into
//! running unsandboxed.

use std::ffi::{OsStr, OsString};
use std::process::{Command, ExitCode};

use crate::helper::{Enforcement, exec_failed, fail};
use crate::policy::Policy;

const WHY: &str = "sandboxing is not supported on this operating system yet";

pub fn probe() -> Enforcement {
    Enforcement::none(WHY)
}

/// The `--init` stage only exists on Linux.
pub fn init(_policy: &Policy, _program: &OsStr, _args: &[OsString]) -> ExitCode {
    fail("--init is only used on Linux")
}

pub fn run(policy: &Policy, program: &OsStr, args: &[OsString]) -> ExitCode {
    if !policy.allow_unsandboxed {
        return fail(WHY);
    }
    let mut cmd = Command::new(program);
    cmd.args(args);
    for name in &policy.remove_env {
        cmd.env_remove(name);
    }
    match cmd.status() {
        Ok(s) => ExitCode::from(s.code().and_then(|c| u8::try_from(c & 0xff).ok()).unwrap_or(1)),
        Err(e) => exec_failed(program, &e),
    }
}
