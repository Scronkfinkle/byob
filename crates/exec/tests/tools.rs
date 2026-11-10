//! The tools end to end, through the real sandbox. This binary is its own sandbox helper
//! (see byob-sandbox's tests/enforce.rs), hence the custom harness.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::json;
use byob_exec::{Exec, Mode, Python, register};
use byob_sandbox::{Sandbox, TempDir, dispatch_helper};
use byob_tools::{FileChange, Registry, ToolContext, ToolOutput};

fn main() -> ExitCode {
    if let Some(code) = dispatch_helper() {
        return code;
    }
    let tests: &[(&str, fn())] = &[
        ("command_output_and_created_files", command_output_and_created_files),
        ("command_failure_and_denied_write", command_failure_and_denied_write),
        ("command_timeout_and_cancel", command_timeout_and_cancel),
        ("python_code_and_script", python_code_and_script),
        ("python_chart_with_matplotlib", python_chart_with_matplotlib),
        ("install_packages_with_uv", install_packages_with_uv),
    ];
    // Enough of libtest's command line for `cargo test` and `cargo nextest`.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--list") {
        if !args.iter().any(|a| a == "--ignored") {
            for (name, _) in tests {
                println!("{name}: test");
            }
        }
        return ExitCode::SUCCESS;
    }
    let exact = args.iter().any(|a| a == "--exact");
    let filter = args.iter().find(|a| !a.starts_with('-'));
    if Sandbox::new(std::env::current_exe().unwrap()).probe().filesystem == "none" {
        println!("skipped: this system can't sandbox");
        return ExitCode::SUCCESS;
    }
    let mut failed = 0;
    for (name, test) in tests {
        if filter.is_some_and(|f| if exact { name != f } else { !name.contains(f.as_str()) }) {
            continue;
        }
        let r = catch_unwind(AssertUnwindSafe(test));
        println!("test {name} ... {}", if r.is_ok() { "ok" } else { "FAILED" });
        failed += usize::from(r.is_err());
    }
    if failed > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

fn tools() -> Registry {
    let mut reg = Registry::default();
    register(&mut reg, Exec { mode: Mode::Sandboxed(Sandbox::new(std::env::current_exe().unwrap())), data_dir: None });
    reg
}

fn call(ws: &TempDir, tool: &str, args: serde_json::Value) -> ToolOutput {
    let cancel = AtomicBool::new(false);
    let ctx = ToolContext::new(ws.path(), &cancel);
    tools().get(tool).unwrap().run(&args, &ctx)
}

fn command_output_and_created_files() {
    let ws = TempDir::new("byob-exec-ws-").unwrap();
    std::fs::write(ws.path().join("data.csv"), "a,b\n1,2\n3,4\n").unwrap();
    let out = call(&ws, "run_command", json!({"command": "wc -l < data.csv; mkdir -p out && echo hi > out/x.txt && echo more >> data.csv"}));
    assert!(!out.failed, "{out:?}");
    assert!(out.text.starts_with("Exit code: 0") && out.text.contains("stdout:\n3"), "{}", out.text);
    assert!(out.text.contains("Files created: out/x.txt") && out.text.contains("Files modified: data.csv"), "{}", out.text);
    assert!(out.changes.contains(&FileChange::Created(ws.path().join("out/x.txt"))));
    assert!(out.changes.contains(&FileChange::Modified(ws.path().join("data.csv"))));
}

fn command_failure_and_denied_write() {
    let ws = TempDir::new("byob-exec-ws-").unwrap();
    let out = call(&ws, "run_command", json!({"command": "echo oops >&2; exit 3"}));
    assert!(out.failed && out.text.contains("Exit code: 3") && out.text.contains("stderr:\noops"), "{}", out.text);
    let target = std::env::temp_dir().join(format!("byob-exec-escape-{}", std::process::id()));
    let out = call(&ws, "run_command", json!({"command": format!("echo x > {}", target.display())}));
    assert!(out.failed && !target.exists(), "{}", out.text);
}

fn command_timeout_and_cancel() {
    let ws = TempDir::new("byob-exec-ws-").unwrap();
    let start = Instant::now();
    let out = call(&ws, "run_command", json!({"command": "sleep 30", "timeout_s": 1}));
    assert!(out.failed && out.text.starts_with("Timed out after 1 s"), "{}", out.text);
    assert!(start.elapsed() < Duration::from_secs(5));

    let cancel = Arc::new(AtomicBool::new(false));
    let c = Arc::clone(&cancel);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        c.store(true, Ordering::Relaxed);
    });
    let ctx = ToolContext::new(ws.path(), &cancel);
    let start = Instant::now();
    let out = tools().get("run_command").unwrap().run(&json!({"command": "sleep 30"}), &ctx);
    assert!(out.failed && out.text.starts_with("Stopped by the user."), "{}", out.text);
    assert!(start.elapsed() < Duration::from_secs(3));
}

fn python_code_and_script() {
    if Python::resolve(None).is_none() {
        println!("  (skipped: no python)");
        return;
    }
    let ws = TempDir::new("byob-exec-ws-").unwrap();
    std::fs::write(ws.path().join("data.csv"), "x,y\n1,2\n3,4\n").unwrap();
    let code = "import csv, os, sys\nrows = list(csv.DictReader(open('data.csv')))\nprint(sum(int(r['y']) for r in rows))\nopen('total.txt','w').write('6')\nprint(os.environ.get('MPLBACKEND'))";
    let out = call(&ws, "run_python", json!({"code": code}));
    assert!(!out.failed, "{}", out.text);
    assert!(out.text.contains("stdout:\n6\nAgg") && out.text.contains("Files created: total.txt"), "{}", out.text);
    // a traceback comes back as a failure with stderr
    let out = call(&ws, "run_python", json!({"code": "import definitely_not_a_module"}));
    assert!(out.failed && out.text.contains("ModuleNotFoundError"), "{}", out.text);
    // a script from the workspace, with arguments
    std::fs::write(ws.path().join("s.py"), "import sys\nprint(sys.argv[1:])").unwrap();
    let out = call(&ws, "run_python", json!({"script": "s.py", "args": ["a", "b c"]}));
    assert!(!out.failed && out.text.contains("['a', 'b c']"), "{}", out.text);
    // reading outside the workspace is denied
    let out = call(&ws, "run_python", json!({"code": "import os\nprint(os.listdir(os.path.expanduser('~')))\nprint(open('/etc/shadow').read())"}));
    assert!(out.failed, "{}", out.text);
}

fn python_chart_with_matplotlib() {
    let Some(py) = Python::resolve(None) else { return };
    println!("  using {}", py.path.display());
    let has_mpl = std::process::Command::new(&py.path).args(["-c", "import matplotlib"]).output().is_ok_and(|o| o.status.success());
    if !has_mpl {
        println!("  (skipped: no matplotlib for {})", py.path.display());
        return;
    }
    let ws = TempDir::new("byob-exec-ws-").unwrap();
    let code = "import matplotlib.pyplot as plt\nplt.plot([1,2,3],[3,1,2])\nplt.savefig('chart.png')\nprint('saved')";
    let out = call(&ws, "run_python", json!({"code": code}));
    assert!(!out.failed && out.changes.contains(&FileChange::Created(ws.path().join("chart.png"))), "{}", out.text);
    let png = std::fs::read(ws.path().join("chart.png")).unwrap();
    assert!(png.starts_with(b"\x89PNG"));
}

/// Downloads from PyPI, so opt-in: `BYOB_TEST_UV_INSTALL=1` with `uv` on PATH.
fn install_packages_with_uv() {
    if std::env::var_os("BYOB_TEST_UV_INSTALL").is_none() || byob_sandbox::which("uv".as_ref()).is_none() {
        println!("  (skipped: set BYOB_TEST_UV_INSTALL=1 and put uv on PATH)");
        return;
    }
    let data = TempDir::new("byob-exec-data-").unwrap();
    let mut reg = Registry::default();
    register(&mut reg, Exec { mode: Mode::Sandboxed(Sandbox::new(std::env::current_exe().unwrap())), data_dir: Some(data.path().to_path_buf()) });
    let ws = TempDir::new("byob-exec-ws-").unwrap();
    let cancel = AtomicBool::new(false);
    let ctx = ToolContext::new(ws.path(), &cancel);
    let out = reg.get("install_python_packages").unwrap().run(&json!({"packages": ["six"]}), &ctx);
    println!("  {}", out.text.replace('\n', "\n  "));
    assert!(!out.failed);
    let out = reg.get("run_python").unwrap().run(&json!({"code": "import six, sys\nprint(six.__name__, sys.prefix)"}), &ctx);
    assert!(!out.failed && out.text.contains("six"), "{}", out.text);
}
