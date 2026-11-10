//! Nix through the real sandbox: the `nix` CLI inside `run_command`, `install_packages`,
//! and a workspace flake's dev shell. Skipped where Nix isn't installed (macOS/Windows CI).
//!
//! The quick test needs only `nix`. The others build packages, so they run when the
//! system registry pins nixpkgs to a store path (NixOS; nothing to download but binary
//! cache hits) or when `BYOB_TEST_NIX=1`; `BYOB_TEST_NIX=0` skips them.
//! Like tests/tools.rs, this binary is its own sandbox helper (custom harness).

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;

use byob_exec::{Exec, Mode, register};
use byob_sandbox::{Sandbox, TempDir, dispatch_helper};
use byob_tools::{Registry, ToolContext, ToolOutput};
use serde_json::json;

fn main() -> ExitCode {
    if let Some(code) = dispatch_helper() {
        return code;
    }
    let tests: &[(&str, fn(), bool)] = &[
        ("nix_cli_works_offline", nix_cli_works_offline, false),
        ("nix_shell_substitutes_without_network", nix_shell_substitutes_without_network, true),
        ("install_packages_program_and_python_library", install_packages_program_and_python_library, true),
        ("flake_dev_shell_is_loaded_and_cached", flake_dev_shell_is_loaded_and_cached, true),
    ];
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--list") {
        if !args.iter().any(|a| a == "--ignored") {
            for (name, ..) in tests {
                println!("{name}: test");
            }
        }
        return ExitCode::SUCCESS;
    }
    let exact = args.iter().any(|a| a == "--exact");
    let filter = args.iter().find(|a| !a.starts_with('-'));
    if byob_sandbox::which("nix".as_ref()).is_none() || !std::path::Path::new("/nix/store").is_dir() {
        println!("skipped: Nix is not installed");
        return ExitCode::SUCCESS;
    }
    if Sandbox::new(std::env::current_exe().unwrap()).probe().filesystem == "none" {
        println!("skipped: this system can't sandbox");
        return ExitCode::SUCCESS;
    }
    let heavy = match std::env::var("BYOB_TEST_NIX").as_deref() {
        Ok("1") => true,
        Ok("0") => false,
        _ => std::fs::read_to_string("/etc/nix/registry.json")
            .ok()
            .and_then(|r| byob_exec::env::nix::nixpkgs_entry(&r))
            .is_some_and(|e| e["to"]["type"] == "path"),
    };
    let mut failed = 0;
    for (name, test, needs_builds) in tests {
        if filter.is_some_and(|f| if exact { name != f } else { !name.contains(f.as_str()) }) {
            continue;
        }
        if *needs_builds && !heavy {
            println!("test {name} ... skipped (nixpkgs isn't pinned locally; BYOB_TEST_NIX=1 runs it)");
            continue;
        }
        let r = catch_unwind(AssertUnwindSafe(test));
        println!("test {name} ... {}", if r.is_ok() { "ok" } else { "FAILED" });
        failed += usize::from(r.is_err());
    }
    if failed > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

struct World {
    data: TempDir,
    ws: TempDir,
    reg: Registry,
}

impl World {
    fn new() -> Self {
        let data = TempDir::new("byob-nix-data-").unwrap();
        let ws = TempDir::new("byob-nix-ws-").unwrap();
        let mut reg = Registry::default();
        register(&mut reg, Exec::new(Mode::Sandboxed(Sandbox::new(std::env::current_exe().unwrap())), Some(data.path().to_path_buf())));
        Self { data, ws, reg }
    }

    fn call(&self, tool: &str, args: serde_json::Value) -> ToolOutput {
        let cancel = AtomicBool::new(false);
        let ctx = ToolContext::new(self.ws.path(), &cancel);
        let out = self.reg.get(tool).unwrap_or_else(|| panic!("no tool {tool}")).run(&args, &ctx);
        println!("  {tool} {args}\n    {}", out.text.replace('\n', "\n    "));
        out
    }
}

/// No network, no daemon work: evaluation only.
fn nix_cli_works_offline() {
    let w = World::new();
    assert!(w.reg.get("install_packages").is_some() && w.reg.get("install_python_packages").is_none());
    let out = w.call("run_command", json!({"command": "nix eval --expr '1 + 2' && nix --version"}));
    assert!(!out.failed && out.text.contains("\n3\n"), "{}", out.text);
    assert!(w.data.path().join("nix/cache").is_dir());
}

/// The client has no network, the daemon substitutes from the binary cache.
fn nix_shell_substitutes_without_network() {
    let w = World::new();
    let out = w.call("run_command", json!({"command": "nix shell nixpkgs#hello --command hello", "timeout_s": 300}));
    assert!(!out.failed && out.text.contains("Hello, world!"), "{}", out.text);
}

fn install_packages_program_and_python_library() {
    let w = World::new();
    let out = w.call("run_command", json!({"command": "echo '{\"a\": [1, 2]}' | jq '.a[1]'"}));
    if out.failed {
        // jq might be on the host already; only check the hint when it isn't.
        assert!(out.text.contains("install_packages") && out.text.contains("[\"jq\"]"), "{}", out.text);
    }
    let out = w.call("install_packages", json!({"packages": ["jq", "matplotlib", "imagemagic-typo"]}));
    assert!(out.failed, "a missing name is reported");
    assert!(out.text.contains("Installed with Nix") && out.text.contains("jq") && out.text.contains("matplotlib"), "{}", out.text);
    assert!(out.text.contains("Not found in nixpkgs: \"imagemagic-typo\""), "{}", out.text);
    let out = w.call("run_command", json!({"command": "echo '{\"a\": [1, 2]}' | jq '.a[1]'"}));
    assert!(!out.failed && out.text.contains("\n2"), "{}", out.text);
    let out = w.call(
        "run_python",
        json!({"code": "import matplotlib\nmatplotlib.use('Agg')\nimport matplotlib.pyplot as plt\nplt.plot([1, 2], [3, 4])\nplt.savefig('p.png')\nprint('ok', matplotlib.__version__)"}),
    );
    assert!(!out.failed && out.text.contains("ok ") && w.ws.path().join("p.png").is_file(), "{}", out.text);
    // Adding another package keeps the earlier ones.
    let out = w.call("install_packages", json!({"packages": ["python3Packages.six"]}));
    assert!(!out.failed, "{}", out.text);
    let out = w.call("run_python", json!({"code": "import six, matplotlib\nprint('both')"}));
    assert!(!out.failed && out.text.contains("both"), "{}", out.text);
    let out = w.call("run_command", json!({"command": "command -v jq"}));
    assert!(!out.failed, "{}", out.text);
}

fn flake_dev_shell_is_loaded_and_cached() {
    let w = World::new();
    let flake = r#"{
  inputs.nixpkgs.url = "nixpkgs";
  outputs = { nixpkgs, ... }:
    let
      forAll = f: nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ] (s: f nixpkgs.legacyPackages.${s});
    in {
      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = [ pkgs.hello ];
          GREETING = "from-the-flake";
          shellHook = ''
            export FROM_HOOK="hook-ran"
            echo "welcome banner"
          '';
        };
      });
    };
}
"#;
    std::fs::write(w.ws.path().join("flake.nix"), flake).unwrap();
    let cmd = json!({"command": "hello -g hi; echo \"$GREETING $FROM_HOOK\"", "timeout_s": 60});
    let out = w.call("run_command", cmd.clone());
    assert!(!out.failed, "{}", out.text);
    assert!(out.text.contains("Loaded the workspace's Nix dev shell from flake.nix"), "{}", out.text);
    assert!(out.text.contains("hi\nfrom-the-flake hook-ran"), "{}", out.text);
    assert!(!out.text.contains("welcome banner"), "the hook's output isn't the command's");
    assert!(w.ws.path().join("flake.lock").is_file(), "nix locked the flake");
    let again = w.call("run_command", cmd.clone());
    assert!(!again.failed && !again.text.contains("Loaded the workspace's"), "cached: {}", again.text);
    // A broken flake: reported once, then commands run without it.
    std::fs::write(w.ws.path().join("flake.nix"), "{ outputs = _: { }; }").unwrap();
    let out = w.call("run_command", json!({"command": "echo still-runs"}));
    assert!(!out.failed && out.text.contains("still-runs") && out.text.contains("failed to load"), "{}", out.text);
    let out = w.call("run_command", json!({"command": "echo still-runs"}));
    assert!(!out.text.contains("failed to load"), "{}", out.text);
}
