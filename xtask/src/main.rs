//! Repo automation. `cargo xtask <cmd>`:
//!   layers  — enforce the crate layering in docs/architecture.md
//!   web     — build the browser UI into apps/byob-web/dist (wasm-bindgen + wasm-opt)
//!   wasm    — check that the browser-side crates build for wasm32
//!   ci      — fmt, clippy, tests, doc tests, layers, wasm (the full local gate)

use std::process::{Command, ExitCode};

/// Every workspace crate and its layer. Dependencies may only point downward.
const LAYERS: &[(&str, u8)] = &[
    ("byob-types", 0),
    ("byob-web-assets", 0),
    ("byob-bundled", 0),
    ("byob-protocol", 1),
    ("byob-artifact-wire", 1),
    ("byob-llm", 1),
    ("byob-store", 1),
    ("byob-tools", 2),
    ("byob-mcp", 2),
    ("byob-sandbox", 2),
    ("byob-exec", 3),
    ("byob-plugins", 3),
    ("byob-agent", 4),
    // the native sheets backend: engine-side, registered by the app (never in the UI or wasm)
    ("byob-sheets", 4),
    // the native web tools backend (web_search, web_fetch): engine-side, registered by the app
    ("byob-web-tools", 4),
    ("byob-server", 5),
    ("byob-ui-egui", 5),
    ("byob", 6),
    ("byob-web", 6),
    ("xtask", 9),
];

/// Only these crates may depend on GUI crates.
const UI_CRATES: &[&str] = &["byob-ui-egui", "byob", "byob-web"];
const UI_DEPS: &[&str] = &["egui", "eframe", "egui-wgpu", "winit", "rfd", "ewebsock"];

/// Crates that run in the browser. They may only depend on each other (not on the engine).
const WASM_CRATES: &[&str] = &["byob-types", "byob-protocol", "byob-ui-egui", "byob-web"];

/// byob's own crates aren't async: none may depend *directly* on an async runtime.
/// (Libraries that use one internally are fine.)
const BANNED: &[&str] = &["tokio", "async-std", "smol"];

/// Servo and SpiderMonkey (MPL-2.0, and they bring tokio) live only in the separate
/// `host/artifact-host` workspace, which byob runs as a child process; never link them here.
const HELPER_ONLY: &[&str] = &["servo", "libservo", "mozjs", "mozjs_sys", "servo-script", "servo-constellation", "byob-artifact-host"];

fn main() -> ExitCode {
    let cmd = std::env::args().nth(1).unwrap_or_default();
    let result = match cmd.as_str() {
        "layers" => layers(),
        "wasm" => wasm(),
        "web" => web(),
        "ci" => ci(),
        _ => Err(format!("unknown command {cmd:?}; expected one of: layers, wasm, web, ci")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::FAILURE
        }
    }
}

fn layer_of(name: &str) -> Option<u8> {
    LAYERS.iter().find(|(n, _)| *n == name).map(|(_, l)| *l)
}

fn layers() -> Result<(), String> {
    let out = Command::new(env!("CARGO")).args(["metadata", "--format-version=1", "--no-deps"]).output().map_err(|e| format!("cargo metadata: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    let meta: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| format!("parse metadata: {e}"))?;
    let packages = meta.get("packages").and_then(|p| p.as_array()).ok_or("metadata has no packages")?;

    let mut errors = Vec::new();
    for pkg in packages {
        let name = pkg.get("name").and_then(|n| n.as_str()).unwrap_or_default();
        let Some(layer) = layer_of(name) else {
            errors.push(format!("{name}: not assigned a layer in xtask LAYERS"));
            continue;
        };
        let deps = pkg.get("dependencies").and_then(|d| d.as_array()).cloned().unwrap_or_default();
        for dep in deps {
            if dep.get("kind").and_then(|k| k.as_str()) == Some("dev") {
                continue;
            }
            let dep_name = dep.get("name").and_then(|n| n.as_str()).unwrap_or_default();
            if let Some(dep_layer) = layer_of(dep_name)
                && dep_layer >= layer
            {
                errors.push(format!("{name} (L{layer}) -> {dep_name} (L{dep_layer}): dependencies must point to a lower layer"));
            }
            if BANNED.contains(&dep_name) {
                errors.push(format!("{name} depends directly on async runtime {dep_name}; byob's own code isn't async (AGENTS.md)"));
            }
            if UI_DEPS.contains(&dep_name) && !UI_CRATES.contains(&name) {
                errors.push(format!("{name} depends on UI crate {dep_name}; only {UI_CRATES:?} may"));
            }
            if WASM_CRATES.contains(&name) && layer_of(dep_name).is_some() && !WASM_CRATES.contains(&dep_name) {
                errors.push(format!("{name} runs in the browser and must not depend on {dep_name}"));
            }
        }
    }
    errors.extend(banned_deps()?);
    if errors.is_empty() {
        println!("layers: ok");
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

/// Fail if Servo/SpiderMonkey appear in the dependency graph of a target we ship
/// (host and wasm32-unknown-unknown). `--target all` would flag e.g. emscripten-only deps.
fn banned_deps() -> Result<Vec<String>, String> {
    let mut found = Vec::new();
    for target in [None, Some("wasm32-unknown-unknown")] {
        let mut args = vec!["tree", "--workspace", "-e", "normal,build", "--prefix", "none", "--format", "{p}"];
        if let Some(t) = target {
            args.extend(["--target", t]);
        }
        let out = Command::new(env!("CARGO")).args(&args).output().map_err(|e| format!("cargo tree: {e}"))?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).into_owned());
        }
        let tree = String::from_utf8_lossy(&out.stdout);
        for name in tree.lines().filter_map(|l| l.split_whitespace().next()) {
            let on = target.unwrap_or("host");
            if HELPER_ONLY.contains(&name) {
                found.push(format!("`{name}` in byob's dependency graph for {on}: Servo/SpiderMonkey belong only in host/artifact-host (see AGENTS.md)"));
            }
        }
    }
    found.extend(helper_excluded()?);
    found.sort();
    found.dedup();
    Ok(found)
}

/// The helper workspace must stay out of this one (its own lock file and feature resolution).
fn helper_excluded() -> Result<Vec<String>, String> {
    let root = std::fs::read_to_string("Cargo.toml").map_err(|e| format!("read Cargo.toml: {e}"))?;
    let excluded = root.lines().any(|l| l.trim_start().starts_with("exclude") && l.contains("\"host/artifact-host\""));
    let own = std::fs::read_to_string("host/artifact-host/Cargo.toml").map_or(true, |t| t.lines().any(|l| l.trim() == "[workspace]"));
    let mut errors = Vec::new();
    if !excluded {
        errors.push("Cargo.toml must `exclude = [\"host/artifact-host\"]` from the workspace".to_owned());
    }
    if !own {
        errors.push("host/artifact-host/Cargo.toml must be its own `[workspace]`".to_owned());
    }
    Ok(errors)
}

fn wasm() -> Result<(), String> {
    let mut args = vec!["check", "--target", "wasm32-unknown-unknown"];
    for krate in WASM_CRATES {
        args.extend(["-p", krate]);
    }
    run(&args)
}

/// Build the browser bundle: cargo (wasm32, release) -> wasm-bindgen -> wasm-opt -> dist/.
fn web() -> Result<(), String> {
    let lock = std::fs::read_to_string("Cargo.lock").map_err(|e| format!("read Cargo.lock: {e}"))?;
    let want = lock
        .split("[[package]]")
        .find(|p| p.contains("name = \"wasm-bindgen\"\n"))
        .and_then(|p| p.lines().find_map(|l| l.strip_prefix("version = \"")))
        .map(|v| v.trim_end_matches('"').to_owned())
        .ok_or("wasm-bindgen not found in Cargo.lock")?;
    let have = Command::new("wasm-bindgen").arg("--version").output().map_err(|e| format!("wasm-bindgen-cli not found (use `nix develop`): {e}"))?;
    let have = String::from_utf8_lossy(&have.stdout);
    if !have.split_whitespace().any(|w| w == want) {
        return Err(format!("wasm-bindgen-cli is {}, Cargo.lock needs {want}", have.trim()));
    }

    run(&["build", "--release", "--target", "wasm32-unknown-unknown", "-p", "byob-web"])?;
    let dist = "apps/byob-web/dist";
    let _ = std::fs::remove_dir_all(dist);
    std::fs::create_dir_all(dist).map_err(|e| format!("create {dist}: {e}"))?;
    tool("wasm-bindgen", &["--target", "web", "--no-typescript", "--out-dir", dist, "target/wasm32-unknown-unknown/release/byob-web.wasm"])?;
    let wasm = format!("{dist}/byob-web_bg.wasm");
    if tool("wasm-opt", &["-O2", "--enable-bulk-memory", "--enable-nontrapping-float-to-int", "-o", &wasm, &wasm]).is_err() {
        eprintln!("xtask: wasm-opt failed or missing; shipping unoptimized wasm");
    }
    // Stamp the protocol major it speaks, so byob can refuse a UI it can't talk to (byob-server's webui.rs).
    let index = std::fs::read_to_string("apps/byob-web/index.html").map_err(|e| format!("read index.html: {e}"))?;
    let stamp = format!("<meta charset=\"utf-8\" />\n    <meta name=\"byob-protocol\" content=\"{}\" />", protocol_major()?);
    if !index.contains("<meta charset=\"utf-8\" />") {
        return Err("apps/byob-web/index.html lost its `<meta charset=\"utf-8\" />`, where the protocol stamp goes".into());
    }
    std::fs::write(format!("{dist}/index.html"), index.replacen("<meta charset=\"utf-8\" />", &stamp, 1)).map_err(|e| format!("write index.html: {e}"))?;
    // Precompressed copies; byob-server sends `<file>.gz` to clients that accept gzip
    // (the wasm shrinks ~3x). Optional: without them it serves the plain files.
    let (wasm_js, wasm_bin) = (format!("{dist}/byob-web.js"), format!("{dist}/byob-web_bg.wasm"));
    if tool("gzip", &["-9", "--keep", "--force", "--no-name", &wasm_bin, &wasm_js]).is_err() {
        eprintln!("xtask: gzip failed or missing; the web UI will be served uncompressed");
    }
    // Also stage it for embedding: byob-web-assets compiles assets/ into byob (feature `web`).
    let embed = "crates/web-assets/assets";
    let _ = std::fs::remove_dir_all(embed);
    std::fs::create_dir_all(embed).map_err(|e| format!("create {embed}: {e}"))?;
    for entry in std::fs::read_dir(dist).map_err(|e| format!("read {dist}: {e}"))?.flatten() {
        let to = std::path::Path::new(embed).join(entry.file_name());
        std::fs::copy(entry.path(), &to).map_err(|e| format!("copy to {}: {e}", to.display()))?;
    }
    println!("web: built {dist}/ and staged it in {embed}/ — `cargo run -p byob -- --serve` embeds it");
    Ok(())
}

fn tool(program: &str, args: &[&str]) -> Result<(), String> {
    println!("==> {program} {}", args.join(" "));
    let status = Command::new(program).args(args).status().map_err(|e| format!("{program}: {e}"))?;
    if status.success() { Ok(()) } else { Err(format!("{program} failed")) }
}

fn run(args: &[&str]) -> Result<(), String> {
    println!("==> cargo {}", args.join(" "));
    let status = Command::new(env!("CARGO")).args(args).status().map_err(|e| format!("cargo {}: {e}", args.join(" ")))?;
    if status.success() { Ok(()) } else { Err(format!("cargo {} failed", args.join(" "))) }
}

fn ci() -> Result<(), String> {
    run(&["fmt", "--all", "--check"])?;
    run(&["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"])?;
    let has_nextest = Command::new(env!("CARGO")).args(["nextest", "--version"]).output().is_ok_and(|o| o.status.success());
    if has_nextest {
        run(&["nextest", "run", "--workspace", "--no-fail-fast"])?;
    } else {
        run(&["test", "--workspace", "--lib", "--bins", "--tests"])?;
    }
    run(&["test", "--workspace", "--doc"])?;
    layers()?;
    wasm()
}

/// `byob_protocol::PROTOCOL_MAJOR`, read from its source (xtask doesn't build the crate).
fn protocol_major() -> Result<u32, String> {
    let file = "crates/protocol/src/version.rs";
    let src = std::fs::read_to_string(file).map_err(|e| format!("read {file}: {e}"))?;
    src.lines()
        .find_map(|l| l.trim().strip_prefix("pub const PROTOCOL_MAJOR: u32 = "))
        .and_then(|v| v.trim_end_matches(';').trim().parse().ok())
        .ok_or_else(|| format!("couldn't find `pub const PROTOCOL_MAJOR: u32 = N;` in {file}"))
}
