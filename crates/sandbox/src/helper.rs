//! The helper: the app binary re-executed as `byob __sandbox-exec <policy> -- <cmd> <args…>`.
//! It confines *itself* (std's `pre_exec` would need `unsafe`), then becomes or starts the
//! command, which inherits every restriction.
//!
//! Exit codes: the command's own; [`SETUP_FAILED`] with a `byob-sandbox: …` line on stderr
//! when the sandbox couldn't be set up (nothing ran); 126/127 when the command couldn't
//! be started; 128+N when it died from signal N.

use std::ffi::OsString;
use std::process::ExitCode;

use serde::{Deserialize, Serialize};

use crate::policy::Policy;

/// The hidden subcommand that turns the app binary into the helper.
pub const HELPER_ARG: &str = "__sandbox-exec";
/// Ask the helper what it can enforce here; it prints an [`Enforcement`] as JSON.
pub const PROBE_ARG: &str = "--probe";
/// Exit code when the sandbox couldn't be set up.
pub const SETUP_FAILED: u8 = 125;
/// Prefix of the helper's own error line on stderr.
pub const ERROR_PREFIX: &str = "byob-sandbox: ";

/// What a run is confined by.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Enforcement {
    /// Filesystem allow-list: `full`, `partial` (older kernel: some operations, e.g.
    /// truncate or cross-folder renames, aren't covered) or `none`.
    pub filesystem: String,
    /// `allowed` (policy), `namespace` (no network at all: own empty network namespace),
    /// `denied` (macOS profile: all network denied), `tcp-only` (TCP connect/bind blocked
    /// by Landlock; UDP still works) or `none`.
    pub network: String,
    /// The command runs as PID 1 of its own PID namespace, so ending it ends every process
    /// it started — even ones that left its process group.
    pub process_tree: bool,
    /// Human-readable notes (why something isn't enforced).
    #[serde(default)]
    pub notes: Vec<String>,
}

impl Enforcement {
    pub fn none(why: impl Into<String>) -> Self {
        Self { filesystem: "none".into(), network: "none".into(), process_tree: false, notes: vec![why.into()] }
    }

    /// Filesystem and (if denied) network are both confined.
    pub fn confined(&self) -> bool {
        self.filesystem != "none" && self.network != "none"
    }
}

/// A parsed helper command line.
#[derive(Debug, Clone, PartialEq)]
pub enum HelperArgs {
    Probe,
    Exec {
        policy: Box<Policy>,
        program: OsString,
        args: Vec<OsString>,
    },
    /// Internal (Linux): PID 1 of a fresh PID namespace the helper already set up.
    Init {
        policy: Box<Policy>,
        program: OsString,
        args: Vec<OsString>,
    },
}

/// Marks the helper's internal init stage (Linux PID namespace).
pub const INIT_ARG: &str = "--init";

/// Parse what follows [`HELPER_ARG`]: `--probe`, or `[--init] <policy-json> -- <program> [args…]`.
pub fn parse_helper_args(args: &[OsString]) -> Result<HelperArgs, String> {
    let (init, rest) = match args.split_first() {
        Some((first, rest)) if first == INIT_ARG => (true, rest),
        _ => (false, args),
    };
    match rest {
        [p] if p == PROBE_ARG && !init => Ok(HelperArgs::Probe),
        [policy, sep, program, rest @ ..] if sep == "--" => {
            let policy = policy.to_str().ok_or("the policy is not valid UTF-8")?;
            let policy = Box::new(Policy::from_json(policy)?);
            if program.is_empty() {
                return Err("no command given".into());
            }
            let (program, args) = (program.clone(), rest.to_vec());
            Ok(if init { HelperArgs::Init { policy, program, args } } else { HelperArgs::Exec { policy, program, args } })
        }
        _ => Err(format!("usage: {HELPER_ARG} {PROBE_ARG} | {HELPER_ARG} <policy-json> -- <command> [args…]")),
    }
}

/// If this process was started as the helper (`argv[1] == __sandbox-exec`), run it and
/// return its exit code; otherwise `None`. Call this first thing in `main`, before any
/// threads start (Linux user namespaces require a single-threaded process).
pub fn dispatch_helper() -> Option<ExitCode> {
    let mut argv = std::env::args_os();
    let _exe = argv.next();
    if argv.next()? != HELPER_ARG {
        return None;
    }
    Some(helper_main(&argv.collect::<Vec<_>>()))
}

/// The helper's entry point; `args` follow [`HELPER_ARG`].
pub fn helper_main(args: &[OsString]) -> ExitCode {
    match parse_helper_args(args) {
        Ok(HelperArgs::Probe) => {
            let e = crate::platform::probe();
            println!("{}", serde_json::to_string(&e).unwrap_or_default());
            ExitCode::SUCCESS
        }
        Ok(HelperArgs::Exec { policy, program, args }) => crate::platform::run(&policy, &program, &args),
        Ok(HelperArgs::Init { policy, program, args }) => crate::platform::init(&policy, &program, &args),
        Err(e) => fail(&e),
    }
}

/// Report a setup failure the way the parent recognises.
pub(crate) fn fail(msg: &str) -> ExitCode {
    eprintln!("{ERROR_PREFIX}{msg}");
    ExitCode::from(SETUP_FAILED)
}

/// Exit code for a command that couldn't be started.
pub(crate) fn exec_failed(program: &std::ffi::OsStr, e: &std::io::Error) -> ExitCode {
    eprintln!("can't run {}: {e}", program.to_string_lossy());
    ExitCode::from(if e.kind() == std::io::ErrorKind::NotFound { 127 } else { 126 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_forms() {
        assert_eq!(parse_helper_args(&os(&["--probe"])).unwrap(), HelperArgs::Probe);
        let HelperArgs::Exec { policy, program, args } = parse_helper_args(&os(&["{}", "--", "sh", "-c", "x"])).unwrap() else { panic!() };
        assert_eq!((*policy, program, args), (Policy::default(), OsString::from("sh"), os(&["-c", "x"])));
        let HelperArgs::Init { program, .. } = parse_helper_args(&os(&["--init", "{}", "--", "sh"])).unwrap() else { panic!() };
        assert_eq!(program, "sh");
        for bad in [
            &[][..],
            &["{}"],
            &["{}", "--"],
            &["{}", "-", "sh"],
            &["nope", "--", "sh"],
            &["{}", "--", ""],
            &["--probe", "x"],
            &["--init", "--probe"],
            &["--init"],
        ] {
            assert!(parse_helper_args(&os(bad)).is_err(), "{bad:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_policy_is_refused() {
        use std::os::unix::ffi::OsStringExt;
        let args = vec![OsString::from_vec(vec![0xff, 0xfe]), "--".into(), "sh".into()];
        assert!(parse_helper_args(&args).is_err());
    }

    #[test]
    fn garbage_fails_cleanly() {
        // A bad command line must exit with the setup code, never run or panic.
        assert_eq!(helper_main(&os(&["garbage"])), ExitCode::from(SETUP_FAILED));
    }

    proptest! {
        #[test]
        fn arbitrary_args_never_panic(v in proptest::collection::vec(".{0,40}", 0..6)) {
            let _ = parse_helper_args(&v.iter().map(OsString::from).collect::<Vec<_>>());
        }

        #[test]
        fn arbitrary_policy_json_never_panics(s in ".{0,200}") {
            let _ = parse_helper_args(&[OsString::from(s), "--".into(), "true".into()]);
        }
    }
}
