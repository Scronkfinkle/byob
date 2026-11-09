# yah architecture

Last updated: 2026-11-09

## Shape

yah is a desktop app whose UI can also be opened in any browser (e.g. over Tailscale).
There is one engine; every window or tab is just a client of it.

```
 browser tab (wasm egui) ──WebSocket──┐
                                      ├──►  engine (yah-agent, own thread)
 desktop window (egui) ───channels────┘        ├─ turns: stream from inference URL
                                               ├─ tools, files, python (host-side)
                                               └─ transcript / storage
```

- Clients speak only `yah-protocol` messages (`ClientMsg` / `ServerMsg`), in-process via
  channels or as JSON text frames over `/ws`. The desktop and web UIs are the same code
  (`yah-ui-egui`), so they cannot drift.
- All state lives in the engine. A client that connects late gets `State` + `Transcript`,
  then live deltas. Turns keep running when a tab closes.
- Secrets (API keys) never leave the engine; `EngineState` only says `has_api_key`.

## Concurrency: no async runtime

Deliberately std-only: `std::thread` + `mpsc` channels + `catch_unwind`. A personal
harness serves a handful of clients, so thread-per-connection and thread-per-turn are
cheap. `cargo xtask layers` fails if `tokio`, `async-std` or `smol` enter the dependency
graph of a shipped target.

- engine: one thread owns all state; turn threads and connections talk to it via its inbox.
- inference: blocking `ureq`, SSE read line by line, cancel flag checked between chunks.
- server: `std::net::TcpListener`, hand-rolled HTTP head parsing (`httparse`), sync
  `tungstenite` with a 20 ms read timeout so one thread both reads and writes.
- browser: `ewebsock` callbacks + egui's frame loop; no executor.

## Layers

```
L6  apps/yah (desktop, --serve, --headless) · apps/yah-web (wasm)
L5  server (HTTP + WebSocket) · ui-egui (wasm-safe)
L4  agent: engine, turns, tool loop, permissions, approvals
L3  plugins: manifests, skills, connectors → adapted into Tools
L2  tools (Tool trait, Registry, built-in tools) · mcp (blocking MCP client)
L1  protocol (wasm-safe) · llm (OpenAI-compatible client) · store
L0  types
```

`cargo xtask layers` enforces: dependencies point down; only `ui-egui`, `yah`, `yah-web`
may use GUI crates; browser crates (`types`, `protocol`, `ui-egui`, `yah-web`) may only
depend on each other; banned async runtimes. `cargo xtask wasm` checks the browser crates
build for `wasm32-unknown-unknown`.

## Remote access & security

`yah --serve` binds `127.0.0.1:7470` by default. Expose it with `tailscale serve 7470`,
which terminates TLS and restricts access to your tailnet. Never bind a public interface.

What `tailscale serve` forwards (from its source, `ipn/ipnlocal/serve.go`; not observed
live because Serve is not enabled on the dev tailnet): it connects from loopback, passes the
browser's `Host` through unchanged, always *sets* `X-Forwarded-Host` (and `X-Forwarded-Proto`,
`X-Forwarded-For`), strips any client-sent `Tailscale-*` headers, then adds
`Tailscale-User-Login`/`-Name` for untagged tailnet users. Tagged devices get no identity;
Funnel (public internet) requests get `Tailscale-Funnel-Request: ?1` and no identity.

- **Origin check.** The `/ws` upgrade rejects browser `Origin`s that don't match `Host` (or
  `X-Forwarded-Host`, honoured only from a loopback proxy), so a random website open in your
  browser can't drive the harness (cross-site WebSocket hijacking). Behind `tailscale serve`
  `Host` already equals the tailnet name, so the check holds unchanged.
- **Identity.** `Tailscale-User-*` headers are trusted only when the TCP peer is loopback;
  from any other peer they are attacker-controlled and ignored (and logged). Every WebSocket
  connect/disconnect is logged with who it was (`Ann <ann@…>`, `local user`, `anonymous`).
- **Allowlist.** `--allow-user LOGIN` (repeatable) admits only those logins, for page loads
  and WebSocket upgrades alike; others get a 403 page naming the flag. Direct loopback
  requests that carry no proxy headers (you, on this machine) are still admitted; anything
  else without an identity — non-loopback peers, tagged devices, other proxies — is not.
  Without `--allow-user`, the tailnet is the boundary, as before.
- **Funnel is always refused**: it would put the harness on the public internet.
- **Resource limits.** 64 concurrent connections (extra ones get a 503 from the accept loop);
  the whole request head must arrive within 10 s (a per-read timeout alone lets slowloris
  trickle bytes forever); writes time out after 10 s; WebSockets are pinged after 20 s of
  silence and dropped after 60 s with nothing received (browsers answer pings themselves).
- **Static files.** Plain path components only (no `..`), fixed content types, `nosniff`,
  `Referrer-Policy: no-referrer`. `cargo xtask web` writes `*.gz` next to the wasm and JS
  (6.9 MB → 2.5 MB); they're served with `Content-Encoding: gzip` to clients that accept it,
  unless older than the source. File names aren't content-hashed, so everything is
  `Cache-Control: no-cache` with per-variant ETags (cheap 304 revalidation); errors are `no-store`.
- Input caps: 16 KiB request head, 4 MiB WebSocket frames, 4 MiB SSE lines, 64 tool calls.
- **Reconnect.** The browser's `WsConnection` redials on close/error/hung dial with backoff
  (0.5 s doubling to 10 s; reset once a link has stayed up 5 s), replays `Hello` so the engine
  resends `State` + `Transcript`, and queues messages typed while offline (up to 64). The tab
  shows one "reconnecting…" banner rather than a notice per retry.
- Not yet: read-only clients.

## Inference

`InferenceConfig { base_url, model, api_key }`. `yah-llm` posts to
`{base_url}/chat/completions` with `stream: true`, assembles content, reasoning
(`reasoning_content`/`reasoning`, or inline `<think>` tags) and streamed `tool_calls`,
and reports usage. Because there's no grammar-constrained local engine anymore, the agent
will run the tool-calling rounds itself (cap 30 per turn, as hob did).

## hob → yah mapping

| hob (TypeScript)                                   | yah                                  |
|----------------------------------------------------|--------------------------------------|
| `shared/*` (agent, types, chart, effort)           | `yah-types`, `yah-protocol`          |
| `engine/*`, `main/engine.ts`, setup/downloader/catalog/hardware/gguf, `packages/picker`, `catalog/` | **removed** → `yah-llm` (HTTP client to a user-supplied URL) |
| `main/vision/*` (spawned llama-server)             | **removed** → `image_url` parts to the same endpoint |
| `main/ipc.ts`, preload, Electron IPC               | `yah-protocol` + `yah-server`        |
| `main/agent/*`                                     | `yah-agent`                          |
| `main/tools/*`                                     | `yah-tools`                          |
| `main/store.ts`, `settings.ts`, `paths.ts`         | `yah-store`                          |
| `main/plugins/*` (skills, MCP, mcpb)               | future `yah-plugins`, `yah-mcp` (hand-rolled JSON-RPC, no tokio) |
| `main/python/*`, `search/*`                        | future `yah-sandbox`                 |
| `main/accounts/*`                                  | future `yah-accounts`                |
| renderer (React)                                   | `yah-ui-egui`                        |

## GUI choice

egui/eframe 0.37 (wgpu; WebGPU with WebGL2 fallback on the web). gpui, iced, Slint,
Freya, Xilem and Floem were prototyped on 2026-11-09 (`~/code/rust/yah-proto/`). gpui
scored best for chat UX but is Zed-internal with only a third-party crates.io republish;
the others lacked selectable text, accessibility, virtualization, or a clean licence.
