# yah — agent & contributor rules

yah ("yet another harness") is a native Rust rewrite of [hob](../../hob) — a desktop AI
coworker — with one big change: **no bundled inference**. Instead of downloading GGUF
models and running llama.cpp, yah talks to any OpenAI-compatible endpoint the user
configures (llama-server, vLLM, Ollama, LM Studio, hosted APIs).

Conventions are adapted from filmcraft. When this file and other docs disagree, this file wins.

## 0. Never crash
- No `unwrap/expect/panic!/unreachable!/todo!/unimplemented!` in production code. Every
  crate root carries:
  `#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]`
- `unsafe_code = "forbid"` workspace-wide.
- Treat every network/model response as hostile: model output, tool-call JSON, SSE frames,
  and HTTP bodies are untrusted. Use `get()`, `checked_*`/`saturating_*`, and cap sizes.
- Recover from lock poisoning: `lock().unwrap_or_else(PoisonError::into_inner)`.
- Worker threads run under `catch_unwind`. Every crash fix ships with a regression test.

## 1. Architecture
- Layering is enforced by `cargo xtask layers` (see `docs/architecture.md`). Dependencies only
  point downward; UI crates (`egui`, `eframe`, `winit`, `rfd`, …) only at L4+.
- The engine (`yah-agent`) is headless and owns all state and the tool-calling loop.
  Clients — the desktop window and browser tabs — talk to it only via `yah-protocol`.
  Never give the UI a direct handle to the engine; add a protocol message instead.
- `yah-ui-egui` and `yah-protocol` must build for `wasm32-unknown-unknown` (`cargo xtask wasm`).
- **No async runtimes** (tokio, async-std, smol) anywhere — enforced by `cargo xtask layers`.
  Use `std::thread` + channels; blocking I/O is fine.
- Remote access is a security boundary: keep the Origin check, input caps, and loopback
  default bind. Secrets never go to clients.
- Native apps are Rust only: no Tauri, Electron, webviews or handwritten JS/TS.

## 2. Workflow
- Dev environment: `nix develop` (or `direnv allow`). Toolchain is pinned by `rust-toolchain.toml`.
- **Use `scripts/headless.sh` for your own GUI runs**, not `cargo run`: it runs the desktop
  app on Xvfb so no window opens on the user's screen. Always pass `--screenshot FILE` so
  the run ends (it waits for the conversation to settle); drive it with `--send TEXT`.
  The mouse does nothing there. For the browser UI, run `yah --headless` and screenshot with
  `chromium --headless=new --use-angle=swiftshader --enable-unsafe-swiftshader`.
- A free local inference server for testing is at `http://silicon:9931/v1` (model `local`,
  4 slots, ~256k context; IP 192.168.1.171). Unit tests use in-process fake servers instead.
- Quality gate before every commit: `cargo xtask ci` (fmt, clippy `-D warnings`, tests, doc tests, layers).
- New crates: put them under `crates/` as `yah-<dir>`, add `[lints] workspace = true`, the
  crate-root deny attribute, and a layer entry in `xtask/src/main.rs`.
- Keep fixes narrow; add a test that fails without the fix. Report only what actually ran.
- Dependencies must be permissively licensed (MIT, Apache-2.0, BSD, ISC, Zlib, OFL, CC0).
- `plan/` is a gitignored scratch notebook.
