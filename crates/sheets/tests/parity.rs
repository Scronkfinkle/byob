//! The native sheets backend against the Python plugin's own test suite
//! (`crates/bundled/sheets_tests`, a fake byob speaking JSON lines), and reply-for-reply
//! parity with the Python backend when a Python with `ironcalc` is at hand
//! (`BYOB_SHEETS_PYTHON`). Skipped without `python3`.

use std::path::Path;
use std::process::Command;

fn tests_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../bundled/sheets_tests")
}

fn python() -> Option<String> {
    let p = "python3".to_owned();
    Command::new(&p).args(["-c", "pass"]).status().ok().filter(|s| s.success()).map(|_| p)
}

fn run(args: &[&str], env: &[(&str, &str)]) -> (bool, String) {
    let Some(py) = python() else { return (true, "no python3: skipped".into()) };
    let mut cmd = Command::new(py);
    cmd.args(["-B", "-m", "unittest"]).args(args).current_dir(tests_dir()).env("PYTHONDONTWRITEBYTECODE", "1");
    for (k, v) in env {
        cmd.env(k, v);
    }
    match cmd.output() {
        Ok(out) => (out.status.success(), String::from_utf8_lossy(&out.stderr).into_owned()),
        Err(e) => (false, e.to_string()),
    }
}

#[test]
fn the_python_plugins_tests_pass_on_the_native_backend() {
    let (ok, err) = run(&["test_sheets"], &[("BYOB_SHEETS_BACKEND", env!("CARGO_BIN_EXE_byob-sheets-backend"))]);
    assert!(ok, "{err}");
    eprintln!("{}", err.lines().last().unwrap_or_default());
}

#[test]
fn replies_match_the_python_backend() {
    // needs BYOB_SHEETS_PYTHON (a Python with ironcalc==0.9.1); the test skips itself otherwise
    let (ok, err) = run(&["test_parity", "-v"], &[("BYOB_SHEETS_PARITY", env!("CARGO_BIN_EXE_byob-sheets-backend"))]);
    assert!(ok, "{err}");
    eprintln!("{}", err.lines().rev().take(3).collect::<Vec<_>>().join(" | "));
}
