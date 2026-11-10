> [!NOTE]
> Heads up: this project, README included, is influenced heavily by AI agents. If LLM-flavoured
> prose isn't your thing, fair warning. Human-written docs will come once things settle.

# byob — bring your own brain

A native Rust desktop AI coworker. A rewrite of hob that points at **any OpenAI-compatible
inference URL** (llama-server, vLLM, Ollama, LM Studio, hosted APIs) instead of bundling
llama.cpp — and whose UI you can also open in any browser, e.g. from your phone over Tailscale.

## Install

```sh
cargo install byob                          # with the browser UI built in
cargo install byob --no-default-features    # slim: desktop only, a few MB smaller
nix build github:Scronkfinkle/byob          # or .#byob-slim
```

Release archives for Linux, macOS and Windows come in the same two flavours
(`byob-…` and `byob-slim-…`) on the GitHub releases page. On Linux they need no GUI
toolkit: file dialogs go through the XDG desktop portal (or zenity); without either, drop
files onto the window.

## Use

```sh
byob                                        # desktop window
byob --serve                                # + browser UI on http://127.0.0.1:7470
byob --headless                             # browser UI only, no window
byob --inference-url http://host:8080/v1 --model local
BYOB_API_KEY=sk-... byob                     # for hosted endpoints

tailscale serve 7470                       # reach it from any device on your tailnet
byob --headless --allow-user you@example.com # only admit that Tailscale login
```

On first run byob shows a **Connect a model** screen: it lists servers it found on this
computer's usual ports (llama-server 8080, Ollama 11434, LM Studio 1234, vLLM 8000), has
presets for those and for hosted APIs (which need a key), and a Test connection button.
Once something answers it goes straight to your chats; the connection lives at the top of
Settings, or click the status dot in the header.

## Develop

```sh
nix develop          # or: direnv allow
cargo run -p byob     # launch the app
cargo xtask web      # build the browser UI; the next `cargo build` embeds it
cargo xtask ci       # full gate: fmt, clippy, tests, layers, no-tokio, wasm
nix build            # release package (byob + bundled web UI + the artifact host)

# HTML artifacts inline on the desktop: the Servo helper is its own workspace (AGENTS.md §3)
(cd host/artifact-host && cargo build --release)   # scripts/headless.sh picks it up

# desktop GUI on a virtual display, scripted, with a screenshot
scripts/headless.sh --inference-url http://localhost:8080/v1 --model local --send "hi" --screenshot /tmp/byob.png

# live test against a real server
BYOB_LIVE_URL=http://localhost:8080/v1 cargo test -p byob-llm -- --ignored
```

See `AGENTS.md` for rules, `docs/architecture.md` for the design, `ROADMAP.md` for what's next.

Licensed under MIT OR Apache-2.0.
