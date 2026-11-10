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
L3  plugins: manifests, skills, connectors → adapted into Tools · exec (run_command, run_python)
L2  tools (Tool trait, Registry, built-in tools) · mcp (blocking MCP client) · sandbox
L1  protocol (wasm-safe) · llm (OpenAI-compatible client) · store (SQLite or folder) · artifact-wire (helper protocol)
L0  types
```

`cargo xtask layers` enforces: dependencies point down; only `ui-egui`, `yah`, `yah-web`
may use GUI crates; browser crates (`types`, `protocol`, `ui-egui`, `yah-web`) may only
depend on each other; banned async runtimes; no Servo/SpiderMonkey (they live only in the
separate `host/artifact-host` workspace). `cargo xtask wasm` checks the browser crates
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
- **Artifacts.** `GET /artifact/<chat>/<id>/<version>` serves a version only if it belongs to
  that chat (same auth as page loads). Every response carries its own CSP: `sandbox
  allow-scripts allow-forms` (opaque origin even as a top-level tab, so no access to yah's
  storage, and its `Origin: null` fails the `/ws` check), `default-src 'none'`, inline
  script/style only, `connect-src 'none'`, `frame-ancestors 'self'`. HTML gets a helper script
  first (theme variables from the URL hash, error reports to the host page, WebRTC removed).
  The browser frames it with `sandbox="allow-scripts allow-forms"`. The desktop app starts a
  loopback server that serves only `/artifact/` (random port) the first time you Open one,
  unless `--serve` already runs.
- Not yet: read-only clients.

## Themes

Appearance is per device (`theme::Prefs`, eframe storage: a file natively, `localStorage` in
the browser) and lives entirely in `yah-ui-egui`. A theme is a `presets::Palette` (the
schemes' own colours in a dozen slots) plus a theme file's changes (`file::ThemeDef`);
`Tokens::build` derives every colour role from it, nudges text-like roles to readable
contrast, then applies the user's accent, knobs, high contrast and per-theme overrides.
Widgets read only `Tokens` (colours, `round()`/`pad()`/`text_size()` for the radius, density
and size knobs). Theme files are small JSON (`base` preset + changed roles + knobs);
desktop lists `<config>/themes/*.json`, the browser keeps imported ones in its prefs. HTML
artifacts get the theme as `--yah-*` CSS variables (`THEME_VARS`, additive only).

## Artifacts

Files the model makes to keep, beside the conversation, versioned and exportable. The kind
follows the file name: `.md` (rendered, Preview/Source), `.html` (single-file app), `.chart.json`
(native chart), PNG/JPEG, anything else as text. `create_artifact` / `update_artifact` (find/
replace edits, each `find` must occur exactly once) / `read_artifact` write only new files
under `.yah/artifacts/<id>/` in the chat's workspace — `artifact.json` (chat, kind, title, file
name, versions) and `v<n>.<ext>` per version — so they are auto-allowed. Charts and images
other tools make are adopted (an index pointing at the file where it is). A version an undo
removed drops out of the list; numbers are never reused.

Steps carry `ArtifactRef`s (rebuilt from stored file changes after a restart); clients get the
chat's list with the transcript (`Artifacts`), `ArtifactUpdated` on new versions, and bytes via
`GetArtifact` (for display and Export). Each turn's system prompt lists the chat's artifacts by
id. The pane has tabs (per chat), collapses to a rail of icons with live/paused dots (a
floating button with a count on phones; Ctrl+Backslash toggles), and any artifact can be
minimized to a one-line chip in the chat and the pane ("Collapse all" in the header). Pane
width, open/collapsed, and tabs + minimized artifacts per chat are device prefs
(`yah-ui-egui::pane`, stored like the appearance prefs). HTML apps render through the
`AppView` seam in `yah-ui-egui`: an iframe overlay in the
browser (positioned from Rust each frame, egui popups cut out with a clip-path); on the
desktop, Servo in a helper process (below), or a summary card with Open/Export when the
helper isn't installed.

### Desktop HTML apps: the Servo helper

`apps/yah/src/apphost` implements `AppView` for the desktop. Each live app version runs in
its own `yah-artifact-host` process (`host/artifact-host/`, a separate Cargo workspace so
Servo's MPL-2.0 code and tokio never enter yah; see AGENTS.md §3). The helper is found via
`$YAH_ARTIFACT_HOST` or next to the `yah` executable.

- **Protocol** (`yah-artifact-wire`, stdin/stdout): frames of `u32` length + tag + fields.
  yah → helper: `Load{html, theme, viewport}`, `Resize`, `Input` (mouse, wheel, keys, text),
  `Focus`, `Theme`, `Ping`, `Shutdown`, `Visible(bool)`. Helper → yah: `Ready`, `Features`
  (bits, right after `Ready`), `Frame{w,h,rgba}` (only when the pixels changed),
  `ContentHeight`, `Error{message,line}`, `Title`, `Cursor`, `Pong`. Additions are new tags
  only; yah sends `Visible` only to helpers that announced it in `Features` (an older helper
  would refuse the tag). Bodies are capped at 64 MiB and validated before use.
- **Rendering**: Servo with a software GL context (Mesa llvmpipe via surfman), `paint()`,
  `glReadPixels`, compare with the last frame, send. yah uploads the newest frame into an
  egui texture; a reader thread keeps only the latest frame, a writer thread means a stuck
  helper never blocks the UI.
- **Document**: the same wrapper the server uses (theme defaults + helper script) as a
  `data:` URL with the theme in the hash; theme changes are applied live by script. Script
  errors come back as marked `console.error` lines and feed "Send error to yah".
- **Sandbox**: `data:` origin is opaque (no storage outlives the process); every
  navigation after the document and every non-`data:`/`blob:` load is refused in
  `load_web_resource`; Servo's HTTP(S) proxy points at a dead port as a second layer;
  windows, permissions and dialogs are denied.
- **Lifecycle**: started lazily when a card or the pane draws the app on screen (the pane
  owns it when both do; the card then mirrors the last frame). Click focuses it (arrows and
  Tab go to the app), Esc gives focus back. Out of view for 3 s: the last frame stays as a
  snapshot and the process exits; back in view: a fresh process (page state is lost).
- **Minimized / background** (a minimized chip, a background tab, the pane collapsed to its
  rail): the process keeps running but gets `Visible(false)` — Servo hides the webview and
  the helper stops painting, reading back and measuring — and yah uploads nothing. Restore
  sends `Visible(true)` and the page carries on where it was. After 60 s in the background
  (`BACKGROUND_PAUSE_AFTER`, `apps/yah/src/apphost/life.rs`) it is paused like an
  out-of-view app: snapshot kept, process gone (~300 MB RSS freed per app in a headless
  run). In the browser the iframe goes `display:none` and is emptied after 60 s unseen.
- **Liveness**: a ping every 500 ms, answered by evaluating a no-op on the page's script
  thread. No pong for 3 s (a runaway script), the process exiting, or Stop on the card:
  the process group is SIGKILLed and the app shows "Stopped" with Reload / Open in browser.

## Context compaction and usage

Before every model request (so also between tool rounds) a turn estimates the request; above
the threshold (`settings.json` `compaction.threshold_percent`, default 80% of the window, never
above window − reply reserve; hidden `--compact-at TOKENS` for testing) it first asks the model,
off-stream, for a dense summary (goals, decisions, facts, files/artifact ids, key tool results,
open tasks) of the conversation written out as text. Before the first request the newest
`keep_turns` turns stay verbatim; inside a tool loop everything is summarised and the request is
repeated after it. The request continues as system prompt + summary pair + verbatim tail.
Compactions are stored (SQLite `compactions` table / folder `compaction_added` events:
summary, last message covered) and the next
request (also after a restart) is built from the latest one plus later messages; the transcript
keeps every message and clients draw a divider. `ClientMsg::Compact` / `/compact [focus]`
compacts on demand. A failed or longer-than-before summary changes nothing and
`fit_history` stays the safety net. `ServerMsg::Usage` feeds the header meter: context in use
(the server's last prompt + completion, else an estimate), window, threshold, chat totals.

## Subagents

The main model can hand a self-contained task to a subagent with the `delegate` tool
(`delegate { agent?, task, tools? }`, `crates/agent/src/delegate.rs`). The turn starts a
**child turn** on its own thread: a fresh context (the agent's prompt plus rules for subagents,
and the task; not the chat's history), the same tool loop and registry cut down to the agent
definition's `tools` and the call's `tools`, at most 25 tool calls and 15 minutes
(`settings.json` `subagents`). Children can't delegate (depth 1). The child's final reply,
bounded (12 KB) and headed with what it did (`[subagent explorer · 3 tool calls: read_file ×2,
list_files · 4.1 s]`), is the tool result, so the parent's context grows only by the report.

- **Parallel.** The tool loop runs calls in order, except `delegate`: every delegation of a
  round starts on its own thread first, the other calls run meanwhile, then results go into
  the transcript in call order. One engine-wide counting semaphore (`Slots`) caps running
  children at the endpoint's slots minus one for the main thread (llama.cpp `/props`
  `total_slots`, 2 assumed when unknown, at least 1; `subagents.max_parallel` overrides).
  Extra delegations wait in "queued".
- **Shared world.** Children run in the chat's workspace with its permission policy, grants
  and sandbox. Their steps go through the same gate; approval cards sit in the child's
  transcript (labelled "Subagent explorer asks") and are answered with the usual `Approve`
  from any client. Their file changes join the parent reply's undo journal. Stop sets the
  parent's flag; children check it between steps and a watchdog copies it (and the time
  limit) into the child's own flag for the model stream.
- **Protocol.** The `delegate` step carries a `Delegation` (agent, task, model, state
  queued/running/done/failed/stopped, the child's streamed text and steps, tokens);
  `ServerMsg::Delegation { entry, step, update }` streams `Delta`, `Step` and `Status` changes,
  so a late client gets everything with the transcript. The UI draws it as a framed, nested
  transcript with a status line ("explorer · 3 tools · running"); running ones scroll on their
  own so several are visible at once. The meter adds "main 34k · agents 120k".
- **Storage.** Each child is an agent run linked to its parent message and step `seq`
  (`ChatStore::add_agent_run` and friends): SQLite tables `agent_runs` + `agent_steps`
  (schema v4, cascading with the message), folder `chats/<chat>/agents/<run>/events.jsonl`
  (`run_started`, `run_delta`, `run_step_added`, `run_step_finished`, `run_finished`; parent
  by message number). Runs come back into their steps when a chat loads (one never finished
  shows as stopped/interrupted), are exported with the chat, and count towards the agents'
  token total.
- **Agent definitions** are Claude Code's format: Markdown with `name`, `description`,
  optional `tools` (comma or YAML list; `Read`, `Bash`, … map to yah tools, unknown names are
  ignored) and `model`, unknown keys kept; the body is the prompt. Looked up in
  `<workspace>/.yah/agents`, `<workspace>/.claude/agents`, `<config>/agents` and plugins'
  `agents/`, first name wins; descriptions are listed in the main system prompt. Without
  `agent`, a general-purpose worker runs.
- **Model roles.** `settings.json` `models: { fast: { base_url?, model? } }` (keys in
  `secrets.json` `model_keys`), each defaulting to the main endpoint; an agent's `model:`
  names a role, else Claude Code's `inherit`/`sonnet`/`opus`/`haiku` mean the main model, else
  it is a model id on the main endpoint.

## Running code (sandbox)

`run_command`, `run_python`, plugin Python tools and stdio MCP servers start through
`yah-sandbox`. No `unsafe` means no `pre_exec`, so the app binary doubles as a helper:
`yah __sandbox-exec <policy-json> -- cmd args…` (dispatched first thing in `main`) confines
itself, then runs the command. On Linux: Landlock filesystem allow-list (system dirs
read-only; workspace + a private temp dir writable), `no_new_privs`, rlimits, and — where
unprivileged user namespaces work — an empty network namespace (network off by default)
and a PID namespace whose init (the helper again, `--init`) takes every descendant down
with it. Without namespaces: Landlock TCP deny only (UDP open) and process-group kill.
Commands get a minimal environment (no API keys), `HOME`/`TMPDIR` in the temp dir,
`MPLBACKEND=Agg`. Parent side: wall-clock timeout, head+tail output caps, Stop, killpg.
macOS uses a `sandbox-exec` profile (compile-checked only); elsewhere, and where Landlock is
missing, commands run unsandboxed with a warning. `YAH_SANDBOX=off` turns it off.
MCP servers get a looser policy (read anything, network on, writes confined).

## Inference

`InferenceConfig { base_url, model, api_key }`. `yah-llm` posts to
`{base_url}/chat/completions` with `stream: true`, assembles content, reasoning
(`reasoning_content`/`reasoning`, or inline `<think>` tags) and streamed `tool_calls`,
and reports usage. Because there's no grammar-constrained local engine anymore, the agent
will run the tool-calling rounds itself (cap 30 per turn, as hob did).

## Storage

The engine holds a `Box<dyn yah_store::ChatStore>` (chats, messages incl. streamed deltas,
steps, compactions, grants, undo journal); `open_backend(&Settings, &Paths)` picks the
implementation from `"storage"` in settings.json (`--storage sqlite|folder` and
`--chats-folder DIR` override it for a run). Both are `Send`, owned by the engine thread, and
run the same contract test suite (`crates/store/src/tests/backends.rs`); the engine tests run
on both too (`test_store::folder_too!`). `export(from, to)` copies everything between them
(timestamps, flags, steps, stats, compactions by position, grants, undo journals; chats the
target already has are skipped), so switching backends is a copy, not a migration:
`yah --export-chats-to folder|sqlite` copies from the storage in use.

**Message ids are per run.** The folder backend issues new message ids every time it opens,
so nothing may persist a message (= transcript entry) id: compactions are stored by message
number in the folder, undo journals are in memory (to persist them, key by task id), artifact
metadata and the UI's per-device pane prefs name only chat and artifact ids.

- **`"sqlite"`** (default): `<data>/yah.db`, WAL, versioned schema (`schema.rs`).
- **`"folder"`**: plain files under `"chats_folder"` (default `<data>/chats-folder`, `~/`
  allowed), meant to be grepped and synced (Syncthing, Dropbox, git):

  ```
  <root>/chats/<YYYY-MM-DD>-<slug>-<shortid>/   (UTC creation date, ASCII slug, 6 hex of the id)
      events.jsonl   canonical, append-only, one {"v":1,"ts":ms,"e":kind,…} per line
      chat.md        generated view: YAML frontmatter + transcript; never read back
  <root>/grants.json global grants          <root>/undo.jsonl  undo entries of tasks no message names
  <root>/.index.json listing cache (derived) <root>/.trash/      deleted chat folders
  ```

  Event kinds: `chat_created`, `renamed`, `workspace_set`, `pinned`, `touched`,
  `message_added`, `message_delta` (one per ~250 ms batch), `message_updated`,
  `message_finished` (error, stats, and the final text lengths), `message_interrupted`,
  `message_undone`, `messages_deleted`, `step_added`/`step_finished`/`steps_deleted`,
  `grant`, `undo_op`/`undo_cleared`, `compaction_added` (`n`, `through` = last message
  number covered, summary, token figures, focus, manual), `deleted`. A `messages_deleted`
  drops the compactions covering any removed message (as SQLite does); `chat.md` shows each
  as a "— context compacted —" note after the last message it covers, with the summary in a
  collapsed `<details>`. Messages are numbered per chat (`m`, never
  reused). The folder name never changes after a rename; the title lives in the events and
  the frontmatter.

  **Crash safety.** Each write is whole lines in one `write` + flush; `fsync` on chat
  creation, finished/complete messages and chat-level changes, not per delta. On load a last
  line without `\n` (a cut-short write) is ignored with a warning, and the next append starts
  a fresh line; unparsable lines are skipped with a warning — except a damaged `undo_op`,
  which makes that task's journal an error (half an undo is worse than none). Replies are
  rebuilt by concatenating deltas; `message_finished` records only the final lengths rather
  than repeating the text, so a streamed reply costs its size once on disk and over sync, and
  a lost delta is still detected (warning). `chat.md` and `.index.json` are written via temp
  file + rename.

  **State.** Startup builds an in-memory index of chat summaries from `.index.json`, checking
  each `events.jsonl`'s size + mtime and replaying only folders that changed (2,000 chats:
  ~60 ms cold, ~15 ms warm, ~4 ms per first-page listing incl. rescan; debug build). A chat's
  full state is replayed lazily when first used (64 kept in memory). Message ids encode a
  per-session chat slot + `m`, so they are valid only for the life of the store.

  **External changes.** `list_chats` (first page) rescans `chats/` (one `stat` per folder):
  new folders appear, changed ones are replayed, vanished ones disappear. A write that finds
  the file grew behind our back replays it first. No file-watcher thread yet, so an open chat
  updates on the next listing or write. Not supported: two yah processes writing one root at
  once (lines would interleave), and true concurrent edits of one chat on two machines (the
  sync tool's conflict copy is ignored as a duplicate). Undo backups stay machine-local.

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
| `main/python/*`                                    | `yah-sandbox` + `yah-exec`           |
| `main/accounts/*`                                  | future `yah-accounts`                |
| renderer (React)                                   | `yah-ui-egui`                        |

## GUI choice

egui/eframe 0.37 (wgpu; WebGPU with WebGL2 fallback on the web). gpui, iced, Slint,
Freya, Xilem and Floem were prototyped on 2026-11-09 (`~/code/rust/yah-proto/`). gpui
scored best for chat UX but is Zed-internal with only a third-party crates.io republish;
the others lacked selectable text, accessibility, virtualization, or a clean licence.
