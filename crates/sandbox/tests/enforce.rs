//! Real enforcement tests. This binary is its own sandbox helper: run as
//! `enforce __sandbox-exec …` it behaves like `yah __sandbox-exec …`; otherwise it runs
//! the tests below (custom harness: libtest would reject the helper's arguments).
//!
//! On a system that can't sandbox (no Landlock) the tests print why and pass.

use std::net::{TcpListener, UdpSocket};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use yah_sandbox::{Enforcement, Limits, Outcome, Policy, RunOptions, Sandbox, TempDir, clean_env, dispatch_helper, run};

fn main() -> ExitCode {
    if let Some(code) = dispatch_helper() {
        return code;
    }
    let tests: &[(&str, fn(&Enforcement))] = &[
        ("write_outside_workspace_is_denied", write_outside_workspace_is_denied),
        ("read_outside_allowed_dirs_is_denied", read_outside_allowed_dirs_is_denied),
        ("network_is_denied_by_default", network_is_denied_by_default),
        ("network_can_be_allowed", network_can_be_allowed),
        ("udp_is_denied_with_namespaces", udp_is_denied_with_namespaces),
        ("timeout_kills_the_whole_tree", timeout_kills_the_whole_tree),
        ("setsid_escapees_die_with_pid_namespace", setsid_escapees_die_with_pid_namespace),
        ("stragglers_die_after_normal_exit", stragglers_die_after_normal_exit),
        ("output_is_capped", output_is_capped),
        ("cancel_stops_quickly", cancel_stops_quickly),
        ("file_size_limit", file_size_limit),
        ("exit_codes_and_signals", exit_codes_and_signals),
        ("garbage_helper_args_fail_cleanly", garbage_helper_args_fail_cleanly),
        ("env_is_scrubbed", env_is_scrubbed),
        ("no_new_privs_is_set", no_new_privs_is_set),
        ("refuses_when_requirements_unmet_is_not_silent", refuses_when_requirements_unmet_is_not_silent),
        ("landlock_only_fallback", landlock_only_fallback),
        ("killing_only_the_helper_ends_the_tree", killing_only_the_helper_ends_the_tree),
    ];
    // Enough of libtest's command line for `cargo test` and `cargo nextest`
    // (`--list --format terse`, `<name> --exact`).
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
    let e = sandbox().probe();
    println!("sandbox here: {e:?}");
    if e.filesystem == "none" {
        println!("skipped: this system can't sandbox");
        return ExitCode::SUCCESS;
    }
    let mut failed = 0;
    for (name, test) in tests {
        if filter.is_some_and(|f| if exact { name != f } else { !name.contains(f.as_str()) }) {
            continue;
        }
        let r = catch_unwind(AssertUnwindSafe(|| test(&e)));
        println!("test {name} ... {}", if r.is_ok() { "ok" } else { "FAILED" });
        failed += usize::from(r.is_err());
    }
    if failed > 0 {
        println!("{failed} failed");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn sandbox() -> Sandbox {
    Sandbox::new(std::env::current_exe().unwrap())
}

struct Run {
    ws: TempDir,
    tmp: TempDir,
}

impl Run {
    fn new() -> Self {
        Self { ws: TempDir::new("yah-sb-ws-").unwrap(), tmp: TempDir::new("yah-sb-tmp-").unwrap() }
    }

    fn policy(&self) -> Policy {
        Policy::for_workspace(self.ws.path(), self.tmp.path()).allow_path_dirs(std::env::var_os("PATH").as_deref()).limits(Limits::for_timeout(30))
    }

    fn sh(&self, policy: &Policy, script: &str, args: &[&str], opts: &RunOptions) -> Outcome {
        let mut a = vec!["-c", script, "sh"];
        a.extend_from_slice(args);
        let mut cmd = sandbox().command(policy, "sh", a);
        cmd.current_dir(self.ws.path());
        clean_env(&mut cmd, self.tmp.path());
        run(cmd, opts, &|| false).unwrap()
    }

    fn quick(&self, script: &str, args: &[&str]) -> Outcome {
        self.sh(&self.policy(), script, args, &RunOptions { timeout: Duration::from_secs(20), ..Default::default() })
    }
}

fn has(cmd: &str) -> bool {
    Command::new("sh").args(["-c", &format!("command -v {cmd}")]).output().is_ok_and(|o| o.status.success())
}

fn write_outside_workspace_is_denied(_: &Enforcement) {
    let r = Run::new();
    let outside = TempDir::new("yah-sb-out-").unwrap();
    let o = r.quick("echo hi > in.txt; mkdir -p sub && echo x > sub/y; echo x > \"$1/f\" || echo denied", &[outside.path().to_str().unwrap()]);
    assert!(o.success(), "{o:?}");
    assert_eq!(std::fs::read_to_string(r.ws.path().join("in.txt")).unwrap(), "hi\n");
    assert!(r.ws.path().join("sub/y").exists());
    assert!(!outside.path().join("f").exists());
    assert!(o.stdout.text().contains("denied"), "{o:?}");
    // the run's own temp folder is writable
    let o = r.quick("echo t > \"$TMPDIR/t\" && cat \"$TMPDIR/t\"", &[]);
    assert_eq!(o.stdout.text(), "t\n");
}

fn read_outside_allowed_dirs_is_denied(_: &Enforcement) {
    let r = Run::new();
    let secret_dir = TempDir::new("yah-sb-secret-").unwrap();
    let secret = secret_dir.path().join("id_rsa");
    std::fs::write(&secret, "TOP SECRET").unwrap();
    let o = r.quick("cat \"$1\"; ls \"$2\"", &[secret.to_str().unwrap(), secret_dir.path().to_str().unwrap()]);
    assert!(!o.stdout.text().contains("TOP SECRET") && !o.stdout.text().contains("id_rsa"), "{o:?}");
    assert!(o.stderr.text().contains("Permission denied"), "{o:?}");
    // the real home folder is not readable either
    if let Some(home) = std::env::var_os("HOME") {
        let o = r.quick("ls \"$1\" >/dev/null && echo readable", &[home.to_str().unwrap()]);
        assert!(!o.stdout.text().contains("readable"), "{o:?}");
    }
}

/// `bash -c 'echo > /dev/tcp/…'`: a TCP connect without needing curl or python.
fn tcp_script(port: u16) -> String {
    format!("exec bash -c 'echo hi > /dev/tcp/127.0.0.1/{port}' && echo connected")
}

fn network_is_denied_by_default(e: &Enforcement) {
    if !has("bash") {
        println!("  (skipped: no bash)");
        return;
    }
    assert_ne!(e.network, "none");
    let r = Run::new();
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    l.set_nonblocking(true).unwrap();
    let o = r.quick(&tcp_script(port), &[]);
    assert!(!o.stdout.text().contains("connected"), "{o:?}");
    assert!(l.accept().is_err(), "a connection got through");
}

fn network_can_be_allowed(_: &Enforcement) {
    if !has("bash") {
        return;
    }
    let r = Run::new();
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let o = r.sh(&r.policy().network(true), &tcp_script(port), &[], &RunOptions::default());
    assert!(o.success(), "{o:?}");
    l.set_nonblocking(true).unwrap();
    assert!(l.accept().is_ok());
}

fn udp_is_denied_with_namespaces(e: &Enforcement) {
    if e.network != "namespace" || !has("bash") {
        println!("  (skipped: network isolation is {}; UDP is not covered by Landlock)", e.network);
        return;
    }
    let r = Run::new();
    let u = UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = u.local_addr().unwrap().port();
    let _ = r.quick(&format!("bash -c 'echo hi > /dev/udp/127.0.0.1/{port}'"), &[]);
    u.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
    let mut buf = [0u8; 16];
    assert!(u.recv(&mut buf).is_err(), "a UDP packet got through");
}

fn timeout_kills_the_whole_tree(_: &Enforcement) {
    let r = Run::new();
    let opts = RunOptions { timeout: Duration::from_millis(800), ..Default::default() };
    let start = Instant::now();
    let o = r.sh(&r.policy(), "(sleep 2; echo leaked > leak) & sh -c 'sleep 2; echo leaked > leak2' & sleep 30", &[], &opts);
    assert!(o.timed_out && o.exit_code.is_none(), "{o:?}");
    assert!(start.elapsed() < Duration::from_secs(5), "{:?}", start.elapsed());
    std::thread::sleep(Duration::from_millis(2500));
    assert!(!r.ws.path().join("leak").exists() && !r.ws.path().join("leak2").exists(), "a grandchild survived the timeout");
}

fn setsid_escapees_die_with_pid_namespace(e: &Enforcement) {
    if !e.process_tree || !has("setsid") {
        println!("  (skipped: no PID namespace; processes that call setsid can outlive a timeout)");
        return;
    }
    let r = Run::new();
    let opts = RunOptions { timeout: Duration::from_millis(800), ..Default::default() };
    let o = r.sh(&r.policy(), "setsid sh -c 'sleep 2; echo leaked > leak' >/dev/null 2>&1 & sleep 30", &[], &opts);
    assert!(o.timed_out);
    std::thread::sleep(Duration::from_millis(2500));
    assert!(!r.ws.path().join("leak").exists(), "a setsid process survived");
}

fn stragglers_die_after_normal_exit(_: &Enforcement) {
    let r = Run::new();
    let o = r.quick("(sleep 1; echo leaked > leak) >/dev/null 2>&1 & echo done", &[]);
    assert!(o.success() && o.stdout.text() == "done\n", "{o:?}");
    std::thread::sleep(Duration::from_millis(1500));
    assert!(!r.ws.path().join("leak").exists(), "a background process outlived the run");
}

fn output_is_capped(_: &Enforcement) {
    let r = Run::new();
    let opts = RunOptions { max_output: 1000, ..Default::default() };
    let o = r.sh(&r.policy(), "head -c 1000000 /dev/zero | tr '\\0' a; echo end >&2", &[], &opts);
    assert!(o.success(), "{o:?}");
    assert_eq!((o.stdout.head.len(), o.stdout.tail.len(), o.stdout.dropped), (500, 500, 999_000));
    assert_eq!(o.stderr.text(), "end\n");
}

fn cancel_stops_quickly(_: &Enforcement) {
    let r = Run::new();
    let flag = Arc::new(AtomicBool::new(false));
    let f = Arc::clone(&flag);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        f.store(true, Ordering::Relaxed);
    });
    let mut cmd = sandbox().command(&r.policy(), "sh", ["-c", "sleep 30"]);
    cmd.current_dir(r.ws.path());
    clean_env(&mut cmd, r.tmp.path());
    let start = Instant::now();
    let o = run(cmd, &RunOptions::default(), &|| flag.load(Ordering::Relaxed)).unwrap();
    assert!(o.cancelled && !o.timed_out, "{o:?}");
    assert!(start.elapsed() < Duration::from_secs(3));
}

fn file_size_limit(_: &Enforcement) {
    let r = Run::new();
    let mut p = r.policy();
    p.limits.file_size_bytes = Some(1 << 20);
    let o = r.sh(&p, "head -c 3000000 /dev/zero > big", &[], &RunOptions::default());
    assert!(!o.success(), "{o:?}");
    assert!(std::fs::metadata(r.ws.path().join("big")).unwrap().len() <= 1 << 20);
}

fn exit_codes_and_signals(_: &Enforcement) {
    let r = Run::new();
    assert_eq!(r.quick("exit 7", &[]).exit_code, Some(7));
    let o = r.quick("kill -9 $$", &[]);
    // exec mode: the signal itself; PID-namespace mode: the supervisor's 128+9
    assert!(o.signal == Some(9) || o.exit_code == Some(137), "{o:?}");
    let o = r.quick("exec /definitely/not/here", &[]);
    assert!(matches!(o.exit_code, Some(126 | 127)), "{o:?}");
    let mut cmd = sandbox().command(&r.policy(), "/definitely/not/here", Vec::<String>::new());
    cmd.current_dir(r.ws.path());
    let o = run(cmd, &RunOptions::default(), &|| false).unwrap();
    assert_eq!(o.exit_code, Some(127), "{o:?}");
    assert!(o.sandbox_error().is_none());
}

fn garbage_helper_args_fail_cleanly(_: &Enforcement) {
    let exe = std::env::current_exe().unwrap();
    for args in [
        vec!["__sandbox-exec"],
        vec!["__sandbox-exec", "garbage"],
        vec!["__sandbox-exec", "{\"read\":[\"rel\"]}", "--", "true"],
        vec!["__sandbox-exec", "{}", "--"],
    ] {
        let mut cmd = Command::new(&exe);
        cmd.args(&args);
        let o = run(cmd, &RunOptions::default(), &|| false).unwrap();
        assert_eq!(o.exit_code, Some(125), "{args:?}: {o:?}");
        assert!(o.sandbox_error().is_some(), "{args:?}");
    }
}

fn env_is_scrubbed(_: &Enforcement) {
    let r = Run::new();
    // clean_env drops inherited secrets...
    let o = r.quick("echo \"${YAH_API_KEY:-unset} $HOME $MPLBACKEND\"", &[]);
    assert_eq!(o.stdout.text().trim(), format!("unset {} Agg", r.tmp.path().display()));
    // ...and remove_env drops named ones even without it
    let mut cmd = sandbox().command(&r.policy().remove_env("SECRET_TOKEN"), "sh", ["-c", "echo ${SECRET_TOKEN:-unset}"]);
    cmd.env("SECRET_TOKEN", "x").current_dir(r.ws.path());
    let o = run(cmd, &RunOptions::default(), &|| false).unwrap();
    assert_eq!(o.stdout.text(), "unset\n");
}

fn no_new_privs_is_set(_: &Enforcement) {
    let r = Run::new();
    let o = r.quick("grep NoNewPrivs /proc/self/status", &[]);
    assert!(o.stdout.text().contains('1'), "{o:?}");
}

fn refuses_when_requirements_unmet_is_not_silent(_: &Enforcement) {
    // A workspace path that doesn't exist is skipped, but the run still happens confined:
    // writes elsewhere stay denied.
    let r = Run::new();
    let mut p = r.policy();
    p.write.push(Path::new("/definitely/not/here").to_path_buf());
    let o = r.sh(&p, "echo x > /definitely-not-writable 2>/dev/null || echo denied", &[], &RunOptions::default());
    assert_eq!(o.stdout.text(), "denied\n", "{o:?}");
}

/// Clients like yah-mcp kill just the process they started (the helper), not its group.
fn killing_only_the_helper_ends_the_tree(e: &Enforcement) {
    if !e.process_tree {
        println!("  (skipped: no PID namespace)");
        return;
    }
    let r = Run::new();
    let mut cmd = sandbox().command(&r.policy(), "sh", ["-c", "sleep 2; echo leaked > leak"]);
    cmd.current_dir(r.ws.path());
    let mut child = cmd.spawn().unwrap();
    std::thread::sleep(Duration::from_millis(400));
    child.kill().unwrap();
    child.wait().unwrap();
    std::thread::sleep(Duration::from_millis(2500));
    assert!(!r.ws.path().join("leak").exists(), "the command outlived its helper");
}

/// Without namespaces: Landlock alone still confines files and TCP; the group kill still
/// gets ordinary background processes.
fn landlock_only_fallback(_: &Enforcement) {
    let r = Run::new();
    let mut p = r.policy();
    p.skip_namespaces = true;
    let outside = TempDir::new("yah-sb-out-").unwrap();
    let o = r.sh(&p, "echo x > \"$1/f\" || echo denied; echo ok > in", &[outside.path().to_str().unwrap()], &RunOptions::default());
    assert!(o.stdout.text().contains("denied") && !outside.path().join("f").exists() && r.ws.path().join("in").exists(), "{o:?}");
    let o = r.sh(&p, "kill -9 $$", &[], &RunOptions::default());
    assert_eq!(o.signal, Some(9), "{o:?}");
    if has("bash") {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        l.set_nonblocking(true).unwrap();
        let o = r.sh(&p, &tcp_script(port), &[], &RunOptions::default());
        assert!(!o.stdout.text().contains("connected") && l.accept().is_err(), "{o:?}");
    }
    let opts = RunOptions { timeout: Duration::from_millis(500), ..Default::default() };
    let o = r.sh(&p, "(sleep 2; echo leaked > leak) & sleep 30", &[], &opts);
    assert!(o.timed_out);
    std::thread::sleep(Duration::from_millis(2500));
    assert!(!r.ws.path().join("leak").exists());
}
