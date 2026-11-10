//! Workspace environments through the real sandbox: the `nix` CLI inside `run_command`,
//! `install_packages`, a workspace flake's dev shell, and a mise project. Each test is
//! skipped where its tool isn't installed (CI on macOS/Windows has no Nix).
//!
//! The quick Nix test needs only `nix`. Those that build packages run when the system
//! registry pins nixpkgs to a store path (NixOS: nothing to download but binary cache hits)
//! or when `BYOB_TEST_NIX=1`; `BYOB_TEST_NIX=0` skips them. The mise test needs `mise` on
//! `PATH` (e.g. `nix shell nixpkgs#mise`), the pixi test `pixi` (it reads conda-forge's
//! package index, so it needs internet).
//! Like tests/tools.rs, this binary is its own sandbox helper (custom harness).

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use std::sync::Arc;

use byob_exec::env::{Envs, Found, ProjectMode};
use byob_exec::{Exec, Mode, register};
use byob_sandbox::{Sandbox, TempDir, dispatch_helper};
use byob_tools::{Answer, Asker, Question, Registry, ToolContext, ToolOutput};
use serde_json::json;

fn main() -> ExitCode {
    if let Some(code) = dispatch_helper() {
        return code;
    }
    let tests: &[(&str, fn(), Needs)] = &[
        ("nix_cli_works_offline", nix_cli_works_offline, Needs::Nix),
        ("nix_shell_substitutes_without_network", nix_shell_substitutes_without_network, Needs::NixBuilds),
        ("install_packages_program_and_python_library", install_packages_program_and_python_library, Needs::NixBuilds),
        ("flake_dev_shell_is_loaded_and_cached", flake_dev_shell_is_loaded_and_cached, Needs::NixBuilds),
        ("declined_flake_is_not_loaded", declined_flake_is_not_loaded, Needs::Nix),
        ("mise_project_env", mise_project_env, Needs::Mise),
        ("pixi_project_env", pixi_project_env, Needs::Pixi),
        ("pixi_installs_packages_without_nix", pixi_installs_packages_without_nix, Needs::Pixi),
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
    if Sandbox::new(std::env::current_exe().unwrap()).probe().filesystem == "none" {
        println!("skipped: this system can't sandbox");
        return ExitCode::SUCCESS;
    }
    let nix = byob_sandbox::which("nix".as_ref()).is_some() && std::path::Path::new("/nix/store").is_dir();
    let builds = nix
        && match std::env::var("BYOB_TEST_NIX").as_deref() {
            Ok("1") => true,
            Ok("0") => false,
            _ => std::fs::read_to_string("/etc/nix/registry.json")
                .ok()
                .and_then(|r| byob_exec::env::nix::nixpkgs_entry(&r))
                .is_some_and(|e| e["to"]["type"] == "path"),
        };
    let mise = byob_sandbox::which("mise".as_ref()).is_some();
    let pixi = byob_sandbox::which("pixi".as_ref()).is_some();
    let mut failed = 0;
    for (name, test, needs) in tests {
        if filter.is_some_and(|f| if exact { name != f } else { !name.contains(f.as_str()) }) {
            continue;
        }
        let skip = match needs {
            Needs::Nix => (!nix).then_some("Nix is not installed"),
            Needs::NixBuilds => (!builds).then_some("needs Nix with nixpkgs pinned locally, or BYOB_TEST_NIX=1"),
            Needs::Mise => (!mise).then_some("mise is not on PATH"),
            Needs::Pixi => (!pixi).then_some("pixi is not on PATH"),
        };
        if let Some(why) = skip {
            println!("test {name} ... skipped ({why})");
            continue;
        }
        let r = catch_unwind(AssertUnwindSafe(test));
        println!("test {name} ... {}", if r.is_ok() { "ok" } else { "FAILED" });
        failed += usize::from(r.is_err());
    }
    if failed > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

enum Needs {
    Nix,
    NixBuilds,
    Mise,
    Pixi,
}

struct World {
    data: TempDir,
    ws: TempDir,
    reg: Registry,
    /// How questions are answered, and how many were asked.
    asker: Asks,
}

struct Asks {
    answer: Answer,
    asked: AtomicUsize,
}

impl Asker for Asks {
    fn ask(&self, q: &Question) -> Answer {
        println!("  asked: {}", q.title);
        self.asked.fetch_add(1, Ordering::SeqCst);
        self.answer
    }
}

impl World {
    fn new() -> Self {
        let data = TempDir::new("byob-nix-data-").unwrap();
        let ws = TempDir::new("byob-nix-ws-").unwrap();
        let mut reg = Registry::default();
        register(&mut reg, Exec::new(Mode::Sandboxed(Sandbox::new(std::env::current_exe().unwrap())), Some(data.path().to_path_buf())));
        Self { data, ws, reg, asker: Asks { answer: Answer::Yes, asked: AtomicUsize::new(0) } }
    }

    /// Only what `found` lists (e.g. pixi with Nix hidden).
    fn with(found: Found) -> Self {
        let data = TempDir::new("byob-env-data-").unwrap();
        let ws = TempDir::new("byob-env-ws-").unwrap();
        let mut reg = Registry::default();
        let envs = Arc::new(Envs::new(Some(data.path()), found, ProjectMode::Ask));
        let exec = Exec { mode: Mode::Sandboxed(Sandbox::new(std::env::current_exe().unwrap())), data_dir: Some(data.path().to_path_buf()), envs };
        register(&mut reg, exec);
        Self { data, ws, reg, asker: Asks { answer: Answer::Yes, asked: AtomicUsize::new(0) } }
    }

    fn answering(mut self, answer: Answer) -> Self {
        self.asker.answer = answer;
        self
    }

    fn asked(&self) -> usize {
        self.asker.asked.load(Ordering::SeqCst)
    }

    fn call(&self, tool: &str, args: serde_json::Value) -> ToolOutput {
        let cancel = AtomicBool::new(false);
        let ctx = ToolContext::new(self.ws.path(), &cancel).with_asker(&self.asker);
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
    assert_eq!(w.asked(), 1, "asked before loading it");
    let again = w.call("run_command", cmd.clone());
    assert!(!again.failed && !again.text.contains("Loaded the workspace's"), "cached: {}", again.text);
    // A broken flake: reported once, then commands run without it.
    std::fs::write(w.ws.path().join("flake.nix"), "{ outputs = _: { }; }").unwrap();
    let out = w.call("run_command", json!({"command": "echo still-runs"}));
    assert!(!out.failed && out.text.contains("still-runs") && out.text.contains("failed to load"), "{}", out.text);
    let out = w.call("run_command", json!({"command": "echo still-runs"}));
    assert!(!out.text.contains("failed to load"), "{}", out.text);
    assert_eq!(w.asked(), 1, "asked once per workspace");
}

/// The user says no: nothing is evaluated or locked, the model is told once.
fn declined_flake_is_not_loaded() {
    let w = World::new().answering(Answer::No);
    std::fs::write(w.ws.path().join("flake.nix"), "{ outputs = _: { }; }").unwrap();
    let out = w.call("run_command", json!({"command": "echo runs-anyway"}));
    assert!(!out.failed && out.text.contains("runs-anyway") && out.text.contains("chose not to load"), "{}", out.text);
    assert!(!w.ws.path().join("flake.lock").exists());
    let out = w.call("run_command", json!({"command": "echo again"}));
    assert!(!out.text.contains("chose not to load") && w.asked() == 1, "{}", out.text);
    let saved = std::fs::read_to_string(w.data.path().join("envs/workspaces.json")).unwrap();
    assert!(saved.contains("\"project\": false"), "{saved}");
}

/// A mise project that only sets variables (nothing to download).
fn mise_project_env() {
    let w = World::new();
    std::fs::write(w.ws.path().join("mise.toml"), "[env]\nGREETING = \"from-mise\"\n").unwrap();
    let out = w.call("run_command", json!({"command": "echo \"$GREETING\""}));
    assert!(!out.failed && out.text.contains("Loaded the workspace's mise tools from mise.toml") && out.text.contains("from-mise"), "{}", out.text);
    let again = w.call("run_command", json!({"command": "echo \"$GREETING\""}));
    assert!(!again.text.contains("Loaded the workspace's") && again.text.contains("from-mise"), "{}", again.text);
}

/// A pixi project without dependencies (pixi still solves against conda-forge's index).
fn pixi_project_env() {
    let w = World::new();
    let platform = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "aarch64") => "linux-aarch64",
        ("linux", _) => "linux-64",
        ("macos", "aarch64") => "osx-arm64",
        ("macos", _) => "osx-64",
        _ => "win-64",
    };
    let manifest = format!("[workspace]\nname = \"t\"\nchannels = [\"conda-forge\"]\nplatforms = [\"{platform}\"]\n\n[dependencies]\n");
    std::fs::write(w.ws.path().join("pixi.toml"), manifest).unwrap();
    let out = w.call("run_command", json!({"command": "echo \"$PIXI_PROJECT_NAME $CONDA_PREFIX\"", "timeout_s": 60}));
    assert!(!out.failed && out.text.contains("Loaded the workspace's pixi environment from pixi.toml"), "{}", out.text);
    assert!(out.text.contains("t ") && out.text.contains(".pixi/envs/default"), "{}", out.text);
}

/// pixi as the installer, with Nix hidden: conda-forge packages in byob's data folder,
/// on PATH and in run_python's python3; the workspace stays untouched.
fn pixi_installs_packages_without_nix() {
    let w = World::with(Found { pixi: byob_sandbox::which("pixi".as_ref()), ..Default::default() });
    assert!(w.reg.get("install_packages").is_some() && w.reg.get("install_python_packages").is_none());
    let out = w.call("install_packages", json!({"packages": ["jq", "matplotlib", "nosuchpkg-xyz"], }));
    assert!(out.failed, "a missing name is reported");
    assert!(out.text.contains("Installed with pixi (conda-forge) for this workspace: jq, matplotlib"), "{}", out.text);
    assert!(out.text.contains("Not found on conda-forge: \"nosuchpkg-xyz\""), "{}", out.text);
    let out = w.call("run_command", json!({"command": "echo '{\"a\": [1, 2]}' | jq '.a[1]' && command -v jq"}));
    assert!(!out.failed && out.text.contains("\n2\n") && out.text.contains("/pixi/.pixi/envs/default/bin/jq"), "{}", out.text);
    let out = w.call("run_python", json!({"code": "import matplotlib\nmatplotlib.use('Agg')\nimport matplotlib.pyplot as plt\nplt.plot([1, 2])\nplt.savefig('p.png')\nprint('ok', matplotlib.__version__)"}));
    assert!(!out.failed && out.text.contains("ok ") && w.ws.path().join("p.png").is_file(), "{}", out.text);
    // Adding more keeps the earlier ones.
    let out = w.call("install_packages", json!({"packages": ["python3Packages.six"]}));
    assert!(!out.failed, "{}", out.text);
    let out = w.call("run_python", json!({"code": "import six, matplotlib\nprint('both')"}));
    assert!(!out.failed && out.text.contains("both"), "{}", out.text);
    let names: Vec<String> = std::fs::read_dir(w.ws.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(names, vec!["p.png".to_owned()], "nothing of pixi's in the workspace");
    assert!(w.data.path().join("pixi/cache").is_dir());
}
