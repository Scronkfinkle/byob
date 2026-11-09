# yah — yet another harness

A native Rust desktop AI coworker. A rewrite of hob that points at **any OpenAI-compatible
inference URL** (llama-server, vLLM, Ollama, LM Studio, hosted APIs) instead of bundling
llama.cpp — and whose UI you can also open in any browser, e.g. from your phone over Tailscale.

## Use

```sh
yah                                        # desktop window
yah --serve                                # + browser UI on http://127.0.0.1:7470
yah --headless                             # browser UI only, no window
yah --inference-url http://host:8080/v1 --model local
YAH_API_KEY=sk-... yah                     # for hosted endpoints

tailscale serve 7470                       # reach it from any device on your tailnet
yah --headless --allow-user you@example.com # only admit that Tailscale login
```

## Develop

```sh
nix develop          # or: direnv allow
cargo run -p yah     # launch the app
cargo xtask web      # build the browser UI into apps/yah-web/dist
cargo xtask ci       # full gate: fmt, clippy, tests, layers, no-tokio, wasm
nix build            # release package (yah + bundled web UI + the artifact host)

# HTML artifacts inline on the desktop: the Servo helper is its own workspace (AGENTS.md §3)
(cd host/artifact-host && cargo build --release)   # scripts/headless.sh picks it up

# desktop GUI on a virtual display, scripted, with a screenshot
scripts/headless.sh --inference-url http://silicon:9931/v1 --model local --send "hi" --screenshot /tmp/yah.png

# live test against a real server
YAH_LIVE_URL=http://silicon:9931/v1 cargo test -p yah-llm -- --ignored
```

See `AGENTS.md` for rules, `docs/architecture.md` for the design, `ROADMAP.md` for what's next.

Licensed under MIT OR Apache-2.0.
