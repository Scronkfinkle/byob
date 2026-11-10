# byob architecture

Last updated: 2026-11-10 (message queue)

## Shape

byob is a desktop app whose UI can also be opened in any browser (e.g. over Tailscale).
There is one engine; every window or tab is just a client of it.

```
 browser tab (wasm egui) ──WebSocket──┐
                                      ├──►  engine (byob-agent, own thread)
 desktop window (egui) ───channels────┘        ├─ turns: stream from inference URL
                                               ├─ tools, files, python (host-side)
                                               └─ transcript / storage
```

- Clients speak only `byob-protocol` messages (`ClientMsg` / `ServerMsg`), in-process via
  channels or as JSON text frames over `/ws`. The desktop and web UIs are the same code
  (`byob-ui-egui`), so they cannot drift.
- All state lives in the engine. A client that connects late gets `State` + `Transcript`,
  then live deltas. Turns keep running when a tab closes.
- Secrets (API keys) never leave the engine; `EngineState` only says `has_api_key`.

## Concurrency: no async runtime

Deliberately std-only: `std::thread` + `mpsc` channels + `catch_unwind`. A personal
harness serves a handful of clients, so thread-per-connection and thread-per-turn are
cheap. `cargo xtask layers` fails if one of our crates depends directly on `tokio`, `async-std` or
`smol` (libraries that use a runtime internally are fine; see AGENTS.md).

- engine: one thread owns all state; turn threads and connections talk to it via its inbox.
- inference: blocking `ureq`, SSE read line by line, cancel flag checked between chunks.
- server: `std::net::TcpListener`, hand-rolled HTTP head parsing (`httparse`), sync
  `tungstenite` with a 20 ms read timeout so one thread both reads and writes.
- browser: `ewebsock` callbacks + egui's frame loop; no executor.

## Layers

```
L6  apps/byob (desktop, --serve, --headless) · apps/byob-web (wasm)
L5  server (HTTP + WebSocket) · ui-egui (wasm-safe)
L4  agent: engine, turns, tool loop, permissions, approvals
L3  plugins: manifests, skills, connectors → adapted into Tools · exec (run_command, run_python, install_packages, workspace environments)
L2  tools (Tool trait, Registry, built-in tools) · mcp (blocking MCP client) · sandbox
L1  protocol (wasm-safe) · llm (OpenAI-compatible client) · store (SQLite or folder) · artifact-wire (helper protocol)
L0  types
```

`cargo xtask layers` enforces: dependencies point down; only `ui-egui`, `byob`, `byob-web`
may use GUI crates; browser crates (`types`, `protocol`, `ui-egui`, `byob-web`) may only
depend on each other; banned async runtimes; no Servo/SpiderMonkey (they live only in the
separate `host/artifact-host` workspace). `cargo xtask wasm` checks the browser crates
build for `wasm32-unknown-unknown`.

## Remote access & security

`byob --serve` binds `127.0.0.1:7470` by default. Expose it with `tailscale serve 7470`,
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
  allow-scripts allow-forms` (opaque origin even as a top-level tab, so no access to byob's
  storage, and its `Origin: null` fails the `/ws` check), `default-src 'none'`, inline
  script/style only, `connect-src 'none'`, `frame-ancestors 'self'`. HTML gets a helper script
  first (theme variables from the URL hash, error reports to the host page, WebRTC removed).
  The browser frames it with `sandbox="allow-scripts allow-forms"`. The desktop app starts a
  loopback server that serves only `/artifact/` (random port) the first time you Open one,
  unless `--serve` already runs.
- Not yet: read-only clients.

## Themes

Appearance is per device (`theme::Prefs`, eframe storage: a file natively, `localStorage` in
the browser) and lives entirely in `byob-ui-egui`. A theme is a `presets::Palette` (the
schemes' own colours in a dozen slots) plus a theme file's changes (`file::ThemeDef`);
`Tokens::build` derives every colour role from it, nudges text-like roles to readable
contrast, then applies the user's accent, knobs, high contrast and per-theme overrides.
Widgets read only `Tokens` (colours, `round()`/`pad()`/`text_size()` for the radius, density
and size knobs). Theme files are small JSON (`base` preset + changed roles + knobs);
desktop lists `<config>/themes/*.json`, the browser keeps imported ones in its prefs. HTML
artifacts get the theme as `--byob-*` CSS variables (`THEME_VARS`, additive only).

## Artifacts

Files the model makes to keep, beside the conversation, versioned and exportable. The kind
follows the file name: `.md` (rendered, Preview/Source), `.html` (single-file app), `.chart.json`
(native chart), PNG/JPEG, anything else as text. `create_artifact` / `update_artifact` (find/
replace edits, each `find` must occur exactly once) / `read_artifact` write only new files
under `.byob/artifacts/<id>/` in the chat's workspace — `artifact.json` (chat, kind, title, file
name, versions) and `v<n>.<ext>` per version — so they are auto-allowed. Charts and images
other tools make are adopted (an index pointing at the file where it is). A version an undo
removed drops out of the list; numbers are never reused.

Steps carry `ArtifactRef`s (rebuilt from stored file changes after a restart); clients get the
chat's list with the transcript (`Artifacts`), `ArtifactUpdated` on new versions, and bytes via
`GetArtifact` (for display and Export). Each turn's system prompt lists the chat's artifacts by
id. The pane has tabs (per chat), collapses to a rail of icons with live/paused dots (a
floating button with a count on phones; Ctrl+Backslash toggles), and any artifact can be
minimized to a one-line chip in the chat and the pane ("Collapse all artifacts" in the header's ⋯ menu). On
wide windows the pane can be maximized over the chat and the chat list (its maximize button,
Ctrl+Shift+F; Restore, Esc or the shortcut again go back — Esc first leaves an app or field
that has keyboard focus); it is the same pane, so a running app is only resized, and the
state is per session. Pane
width, open/collapsed, and tabs + minimized artifacts per chat are device prefs
(`byob-ui-egui::pane`, stored like the appearance prefs). HTML apps render through the
`AppView` seam in `byob-ui-egui`: an iframe overlay in the
browser (positioned from Rust each frame, egui popups cut out with a clip-path); on the
desktop, Servo in a helper process (below), or a summary card with Open/Export when the
helper isn't installed.

### Desktop HTML apps: the Servo helper

`apps/byob/src/apphost` implements `AppView` for the desktop. Each live app version runs in
its own `byob-artifact-host` process (`host/artifact-host/`, a separate Cargo workspace so
Servo's MPL-2.0 code and tokio never enter byob; see AGENTS.md §3). The helper is found via
`$BYOB_ARTIFACT_HOST` or next to the `byob` executable.

- **Protocol** (`byob-artifact-wire`, stdin/stdout): frames of `u32` length + tag + fields.
  byob → helper: `Load{html, theme, viewport}`, `Resize`, `Input` (mouse, wheel, keys, text),
  `Focus`, `Theme`, `Ping`, `Shutdown`, `Visible(bool)`. Helper → byob: `Ready`, `Features`
  (bits, right after `Ready`), `Frame{w,h,rgba}` (only when the pixels changed),
  `ContentHeight`, `Error{message,line}`, `Title`, `Cursor`, `Pong`. Additions are new tags
  only; byob sends `Visible` only to helpers that announced it in `Features` (an older helper
  would refuse the tag). Bodies are capped at 64 MiB and validated before use.
- **Rendering**: Servo with a software GL context (Mesa llvmpipe via surfman), `paint()`,
  `glReadPixels`, compare with the last frame, send. byob uploads the newest frame into an
  egui texture; a reader thread keeps only the latest frame, a writer thread means a stuck
  helper never blocks the UI.
- **Document**: the same wrapper the server uses (theme defaults + helper script) as a
  `data:` URL with the theme in the hash; theme changes are applied live by script. Script
  errors come back as marked `console.error` lines and feed "Send error to byob". The helper
  puts one Servo workaround first in the document (`SELECTION_FIX`): Servo 0.8 paints a
  replaced element (canvas, img, …) with the selection tint when a collapsed caret sits
  inside it — which a plain click on a `<canvas>` causes — so a caret landing there is moved
  to just before the element.
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
  the helper stops painting, reading back and measuring — and byob uploads nothing. Restore
  sends `Visible(true)` and the page carries on where it was. After 60 s in the background
  (`BACKGROUND_PAUSE_AFTER`, `apps/byob/src/apphost/life.rs`) it is paused like an
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

## Message queue

While a reply (or a manual compaction) runs in a chat, `SendMessage` goes into that chat's
queue in the engine (`byob-agent` `queue.rs`) instead of being refused, attachments included.
The queue belongs to the chat, not to a client: every window and tab looking at the chat gets
it as `ServerMsg::Queue` (after the transcript and on every change) and may edit it, and it
keeps going when the window that filled it closes.

- A turn that ends normally starts the next queued message as a new turn, in order. The
  next turn's own auto-compaction runs before its first request, as for any turn; a queued
  `/compact` runs as a manual compaction.
- **Stop** or a **failed** reply pauses the queue (`ChatQueue::paused`: stopped / failed). It
  waits, unchanged, until someone presses Resume (`ResumeQueue`), Send now on an item, or
  Retry on the failed reply. A message sent to the free chat meanwhile goes at once; the
  queue stays paused. A queued message that can't start (e.g. an invalid endpoint) goes back
  to the front and pauses the queue as failed.
- A turn **waiting for an approval** is still running; the queue just waits.
- `Unqueue`, `EditQueued` (in place; empty removes), `MoveQueued` and `SendQueuedNow` (move to
  the front, resume, and stop the running turn without pausing) act on the chat in view;
  unknown ids get a notice.
- Caps: 20 messages, 256 KB of text each, 1 MB per chat; attachments must be uploads.
- Queued messages are not in the transcript (or `chat.md`) until sent; the title generator,
  Retry and the usage meter only ever see sent messages.
- In memory only: an engine restart drops every queue. Deleting the chat (or removing its
  workspace) drops its queue.

The composer shows the queue as compact rows above the input: Enter (or **Queue**) adds while
a reply runs, with **Stop** beside it; clicking a row edits it in the composer (the draft
there is set aside until Save or Esc).

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
  optional `tools` (comma or YAML list; `Read`, `Bash`, … map to byob tools, unknown names are
  ignored) and `model`, unknown keys kept; the body is the prompt. Looked up in
  `<workspace>/.byob/agents`, `<workspace>/.claude/agents`, `<config>/agents` and plugins'
  `agents/`, first name wins; descriptions are listed in the main system prompt. Without
  `agent`, a general-purpose worker runs.
- **Model roles.** `settings.json` `models: { fast: { base_url?, model? } }` (keys in
  `secrets.json` `model_keys`), each defaulting to the main endpoint; an agent's `model:`
  names a role, else Claude Code's `inherit`/`sonnet`/`opus`/`haiku` mean the main model, else
  it is a model id on the main endpoint.

## Plugins

A plugin is a folder with a `byob-plugin.json` (format and loader: `crates/plugins`) that adds
Python tools (`tools[].run`, JSON on stdin), MCP servers (`mcpServers`), skills
(`skills/<name>/SKILL.md`), subagents (`agents/*.md`), slash commands (`commands/*.md`,
Claude Code's format), settings (`settings`, passed as environment variables) and one
system-prompt line (`prompt`). The engine loads them itself
(`crates/agent/src/extensions.rs`, `plugins.rs`); the app only says where to look
(`PluginsConfig`) and registers the base tools (file, artifact, code-running) once.

**Scopes.** Three sets of folders, most specific first; the same id resolves chat > workspace
> global, and the losing copies are listed as `shadowed_by`:

| scope | folder | written by |
|---|---|---|
| chat (scratch) | `<data>/chats/<chat>/plugins/<id>` (gone with the chat) | the model (`make_tool` & co.), never asked |
| workspace | `<workspace>/.byob/plugins/<id>` (+ standalone skills `.byob/skills`) | committed with the repo; the model's file tools may edit `.byob/{plugins,agents,skills}` |
| global | `<config>/plugins/<id>`, `settings.json` `plugin_dirs`, `--plugin DIR` (+ `<config>/skills`) | the user, or `keep_plugin … global` |

**Switches.** `Layer::plugins: {id: bool}` at every settings level (`settings.json`
`plugins`, the workspace's and the chat's own config); the most specific level that names a
plugin wins, unnamed plugins are on (`Effective::plugin_on`). They change only through
`SetPluginEnabled`; `SetConfig` keeps them. Built-in and authoring tools are not plugins: they
can't be switched off, and a plugin tool whose name is taken (by them or another plugin) gets
`<plugin>__<tool>`.

**Loading and live reload.** Each set keeps a fingerprint (names, sizes, mtimes of its files)
and is read again only when it changes: at turn start, after an authoring tool or a file
tool touched `.byob/plugins`, on `ReloadPlugins`. A turn composes its tools (base + switched-on
plugins' tools + `use_skill`) and its prompt part (plugin prompt lines, the skills list, one
line about `make_tool`, then the workspace's `AGENTS.md`, capped at 16 KB) in `begin_turn_in`.
The turn holds its tools in a swappable `ToolSet`; each round takes a snapshot when it starts,
so a reload never changes tools under a running call, and a tool made in round *n* is callable
in round *n+1*. The system prompt stays as it was until the next turn (KV cache).

**MCP servers** start on worker threads (`Input::Connected` brings the result back); until
then they show as `connecting` and their tools join from the next round. Connections are
pooled by (plugin, server, configuration with settings filled in, folder, workspace for local
servers): unchanged servers survive reloads, a changed plugin's servers restart. Local
(stdio) servers run in the sandbox of the chat's workspace (`McpSandbox`).

**Trust.** Chat plugins and global ones run. A workspace plugin runs its code only if byob
wrote it (`make_tool`/`keep_plugin` into the workspace, or a manifest created by the file
tools) or the user trusted it; the record lives in `workspaces.json`
(`plugin_trust: {id: made|trusted|declined}`), never in the repo. A plugin that arrived with
the folder (clone, pull) brings only its skills, prompt line and subagents until then; the
first turn in that workspace asks once (an approval card on a `plugins` step; `--auto-approve`
answers yes), and `TrustPlugins` decides from the UI. When a plugin's files change, the
"always allow" grants of its tools are withdrawn (globally and in the chats that see it).

**Settings and secrets.** Values come from `settings.json` `plugin_settings.<id>` and, for
settings marked `secret`, `secrets.json` `plugin_settings."plugin:<id>"`. They reach only the
plugin's own processes (environment variables of its scripts, `${KEY}` in MCP server
configs). Clients and the model only see whether each is set.

### For clients (protocol 12)

`ServerMsg::Extensions(Extensions { chat, workspace, plugins: Vec<PluginInfo>, errors })` comes
after `ChatOpened` (before `Artifacts`), on `GetExtensions`, and whenever plugins change (a
reload, a switch, a setting, a server connecting, an authoring tool). It describes the chat in
view (or the new chat a client is about to start). `PluginInfo`:

| field | meaning |
|---|---|
| `id`, `name`, `version`, `description`, `icon` | from the manifest |
| `scope` | `chat` / `workspace` / `global` |
| `origin` | `bundled` / `user` / `agent` (byob made it) / `repo` (came with the workspace) |
| `dir` | folder, for display |
| `enabled`, `enabled_from` | the switch and the level that set it (`default`: nobody, on) |
| `trusted`, `declined` | may its code run; did the user say no (repo plugins) |
| `shadowed_by` | a more specific copy with the same id is used instead |
| `tools` | model-facing tool names in use (empty when off or untrusted) |
| `skills`, `agents`, `commands` | names (`commands`: its `commands/*.md`) |
| `servers` | `{ name, state: connecting/ready/failed/off, tools, error }` |
| `settings` | `{ key, label, help, secret, required, set }`, never the value |
| `network` | its tools may use the internet |
| `problems` | the loader's messages (missing script, bad skill, "needs settings: …") |
| `last_error` | the last failed run of one of its tools in this engine run |
| `fingerprint` | changes whenever a file of it changes |

`errors: Vec<PluginLoadError { scope, path, message }>`: folders that couldn't load at all.

`slash: Vec<SlashItem { name, kind: builtin|command|skill, description, hint, scope, plugin }>`:
the composer's `/` menu (`/compact`, then switched-on plugins' commands, then every skill the
chat sees; a name once). A message starting with `/name args` is expanded when its turn starts
(`extensions::expand_slash`): a command's template with `$ARGUMENTS` and `$1`…`$9` filled in
(arguments it doesn't mention are appended, as in Claude Code), a skill as "use the `name`
skill (call `use_skill` first)" plus the arguments. Only the model sees the expansion; the
transcript keeps what was typed, and later turns send the typed text.

Client messages (all validated; ids are plugin ids `[a-z0-9-]{1,48}`):

- `GetExtensions` → `Extensions`.
- `SetPluginEnabled { scope: ConfigScope, plugin, enabled: Option<bool> }` (`None` inherits).
- `SetPluginSetting { plugin, key, value: Option<String> }` (≤ 8192 chars; `None` clears). The
  key must be one the plugin declares.
- `KeepPlugin { plugin, scope: workspace|global, new_id? }`: the user keeps the chat's copy
  (moved from scratch); no approval card, the user asked.
- `RemovePlugin { plugin, scope }`: deletes that copy (not bundled ones).
- `ReloadPlugins`: read every folder again; failed servers retry.
- `TrustPlugins { workspace, plugins (empty: all from the folder), trust }`.

**Plugin cards.** A finished step of an authoring tool carries `Step::plugin:
Option<Box<PluginCard>>` with `{ plugin, scope, action: made/edited/kept/removed, tools, added,
removed, test: Option<{ ok, output }>, problems, missing_settings }`. The card is stored with the
step (`steps.card`, SQLite schema v5; `card` on the folder store's `step_finished` line), so a
transcript loaded from storage shows it too. The trust question is an ordinary approval card on
a step named `plugins` with `ask.text` listing the plugins.

**In the UI** (`crates/ui-egui/src/plugins.rs`, `slash.rs`; desktop and browser alike):

- the plugin card under such a step: scope, tools added/removed, the test run (a failure's
  output open), problems, missing settings as fields (secret ones masked; the typed value is
  dropped once `SetPluginSetting` is sent, and the field turns into "set ✓" when the next
  `Extensions` says so), Keep in this workspace / everywhere (the scratch plugin gets a name,
  from its first tool) and Files. Only a plugin's latest card in the chat offers actions;
- Settings → Plugins (also "Plugins…" in the ⋯ menu, the header chip, `--open-panel plugins`):
  load errors, then each scope's plugins with origin, switch (acting at the level that decides
  it; Reset; "Switch…" for any level), tools, skills, subagents, commands, servers, settings,
  problems, folder, keep / trust / remove (confirmed) and Reload. A workspace's settings list
  its own plugins with switches for the workspace; the chat settings popover has per-chat
  switches;
- a first-open card above the chat when the workspace's folder brought plugins nobody decided
  on (Enable all / Choose… / Not now → `TrustPlugins`). It hides while the turn's `plugins`
  approval card is waiting, and either answer makes the other go away (the engine's next
  `Extensions` has them decided);
- a header puzzle chip: plugins on, with a red badge for load errors, problems, a last error or
  a failed server; it opens the panel on the first plugin in trouble;
- the composer's `/` menu while the message is `/` and a name: ↑/↓, Enter or Tab inserts
  `/name `, Esc closes; Enter sends as usual when it is closed.

### The model's side

One system-prompt line points at `make_tool` and the built-in `plugin-authoring` skill
(`crates/agent/src/plugin_authoring.md`: manifest format, a Python tool, MCP servers, skills,
subagents, settings). Tools (`crates/agent/src/authoring.rs`):

- `make_tool(name, description?, code, try_args?, network?, settings?, plugin?)`: `code`
  defines `def run(arg: type = default, …)`; the schema comes from the type hints, defaults and
  the docstring's `Args:` lines (`byob_plugins::authoring::derive_schema`, a Python shim that
  parses, never runs, the code). Writes `tools/<name>.py` (code + a runner footer) and the
  manifest entry into the chat's `scratch` plugin (or `plugin`), reloads, and runs `try_args`
  if given: the output, or the last 40 lines of the traceback. Access: read without
  `try_args`, execute with it.
- `edit_plugin(plugin, file, find, replace)` (one exact match) and
  `write_plugin_file(plugin, path, content)` (a new id starts a chat plugin): reload and return
  the loader's problems verbatim.
- `plugin_status(plugin?, file?)`: scope, switch, origin, tools/skills/agents, servers,
  settings set/missing, problems, last error; with `file`, that file's text.
- `keep_plugin(plugin, scope: workspace|global, new_id?)`: moves it with a file list on the
  approval card; workspace follows the approvals policy, global must be allowed by a person.
- `remove_plugin(plugin)`: chat copies go at once, others ask.
- `save_skill(…, scope: chat|workspace|global)`: chat → the scratch plugin's `skills/`,
  workspace → `.byob/skills`, global → `<config>/skills`.

Changes to a workspace's or the global plugins (edit, write, make into a named plugin) ask
first; the chat's own scratch plugins never do.

## Running code (sandbox)

`run_command`, `run_python`, plugin Python tools and stdio MCP servers start through
`byob-sandbox`. No `unsafe` means no `pre_exec`, so the app binary doubles as a helper:
`byob __sandbox-exec <policy-json> -- cmd args…` (dispatched first thing in `main`) confines
itself, then runs the command. On Linux: Landlock filesystem allow-list (system dirs
read-only; workspace + a private temp dir writable), `no_new_privs`, rlimits, and — where
unprivileged user namespaces work — an empty network namespace (network off by default)
and a PID namespace whose init (the helper again, `--init`) takes every descendant down
with it. Without namespaces: Landlock TCP deny only (UDP open) and process-group kill.
Commands get a minimal environment (no API keys), `HOME`/`TMPDIR` in the temp dir,
`MPLBACKEND=Agg`. Parent side: wall-clock timeout, head+tail output caps, Stop, killpg.
macOS uses a `sandbox-exec` profile (compile-checked only); elsewhere, and where Landlock is
missing, commands run unsandboxed with a warning. `BYOB_SANDBOX=off` turns it off.
MCP servers get a looser policy (read anything, network on, writes confined).

### Workspace environments (getting tools)

`byob_exec::env::Envs` prepares every `run_command` / `run_python` call. First it decides
*where* the call runs: in the workspace's **container** (Docker/Podman: the workspace's
Container environment, a chat's override, or asked once when there is a
`.devcontainer/devcontainer.json` and the environment is Auto; `env/container.rs`,
`env/devcontainer.rs`), or here in the sandbox. Here, it stacks layers (`PATH` entries,
variables, sandbox access) onto that clean environment, first match winning:

1. **Installed packages** from `install_packages`, per workspace under
   `<data>/envs/<name>-<hash>/`. With Nix: one `buildEnv` (programs, plus one
   `python3.withPackages` for Python libraries) behind a GC-root out-link, nixpkgs pinned in
   `<data>/nix/nixpkgs.json`; names are checked first (`nix eval`; unknown ones get similar
   names back), pip-style and import names are mapped (`sklearn` → `scikit-learn`, `convert` →
   `imagemagick`). Without Nix: pixi (conda-forge) into a byob-owned manifest, activated with
   `pixi shell-hook --json` and cached (`env/pixi.rs`). With neither, `install_python_packages`
   (uv) is offered instead and failed runs say how to get pixi.
2. **A workspace `.venv` / `venv`** (any OS).
3. **Project environments**, loaded once the user agreed (asked once per workspace through
   an approval card, answer kept in `<data>/envs/workspaces.json`; `env/choices.rs`), then
   loaded once (sandboxed, with network, may write the workspace, e.g. `flake.lock`) and
   cached in the workspace's state folder until their files change: `flake.nix` /
   `shell.nix` (`nix print-dev-env --profile`, so GC-rooted; the `shellHook` runs once and its
   variable changes are kept), `pixi.toml` (`pixi shell-hook --json`), mise (`mise env
   --json`). A failure is reported once in the tool output; commands still run.
   `BYOB_WORKSPACE_ENV=off` skips this step, `on` loads without asking.
4. **Support for the `nix` CLI** in every run: `/nix/var/nix` and `/etc/nix` readable, the
   daemon socket reachable (Seatbelt needs an explicit rule; Linux allows it), a shared
   `NIX_CACHE_HOME`, and `NIX_CONFIG` with flakes on, `substitute = true` (the client turns
   substitution off when it sees no network, but the daemon downloads), and `flake-registry`
   pointing at byob's registry pinning nixpkgs (seeded from `/etc/nix/registry.json`), since
   the client fetches the global registry itself. So `nix shell nixpkgs#foo --command …` works
   in a run without network.

Failed runs get a one-line hint naming the call to make (`jq: not found`,
`ModuleNotFoundError`, `pip install`/`apt`/`sudo` attempts). The last resort,
`install_system_packages` (Homebrew/winget, system-wide), is a tool whose `always_ask` makes
every call an approval card that no grant or `--auto-approve` answers. Tools ask such
questions mid-call through `ToolContext::ask` (a `Step` with `ask` set, Allow/Deny only);
A workspace's settings window shows and changes the answers through `ClientMsg::WorkspaceEnv` and
the `byob_tools::EnvControl` the engine is given. Details and plans: `docs/environments.md`.

## Workspaces and layered settings

A **workspace** is a folder with a name and its own settings. Chats belong to the workspace
of the folder they were created in (`Chat::workspace`, unchanged since before workspaces
existed): any folder seen on a chat becomes a workspace by itself, named after the folder, so
old chats group without a migration. Workspaces live in byob's own files, never in the folder:
`<config>/workspaces.json` (`{id, name, folder, created_at, config}`; the id is derived from
the folder path) and `<data>/chat-settings.json` (each chat's own settings). Keys of
workspaces and chats with their own endpoint are in `secrets.json` `scoped_keys`
(`workspace:<id>`, `chat:<id>`) and never reach clients.

**Settings resolve global → workspace → chat** with one pure function,
`byob_types::config::resolve` (each level a `Layer` of `Option`s; the result says where every
value came from). The engine asks it everywhere it needs a chat's settings
(`byob-agent::workspaces`, `Engine::chat_settings`): the turn (endpoint and key, tool mode,
approvals → permission policy, compaction, subagents, system prompt), manual compaction, the
usage meter, the header (`EngineState.chat_endpoint`), the environment of `run_command`.

| Setting | Global (settings.json) | Workspace | Chat |
|---|---|---|---|
| Endpoint: URL + model (+ key, engine-side) | `inference` | ✓ | ✓ |
| Tool calling (auto / native / prompt) | `tool_mode` | ✓ | ✓ |
| Approvals (ask / allow edits / allow all / read only) | `approvals` | ✓ | ✓ |
| Compaction (on/off, threshold %) | `compaction` | ✓ | ✓ |
| Subagents on/off | `subagents.enabled` | ✓ | ✓ |
| Extra instructions | `instructions` | ✓ | ✓ |
| Environment (Auto / this machine / Nix / container) | — | ✓ | this machine or container |
| Container image | `containers.image` (default) | ✓ (else devcontainer.json) | — |
| Name, folder, project-env answers, installed packages, container | — | ✓ only | — |

- **Most specific wins**, per field. **Instructions are appended**, never replaced: global,
  then workspace, then chat, all in the system prompt.
- **Keys follow their server.** A key belongs to the URL its level points at and is used only
  when the effective URL has the same scheme, host and port (the most specific such level
  wins); changing a level's URL to another server drops its key.
- **Global only**: theme, storage, plugin folders, server options, container runtime, the
  workspaces folder (`workspaces_root`, default `~/byob-workspaces`), model roles, subagent
  limits and slots (one engine-wide semaphore, so not per chat).
- **Testing flags beat every level** for the run: `--auto-approve`, `--tool-mode`; and
  `--inference-url` / `--model` stand in for the global endpoint.
- **Environment.** Auto is today's behaviour (asked once per folder). Host never uses a
  container; Nix loads the folder's dev shell without asking; Container runs every command
  in the workspace's container, no question (the choice is also recorded as the folder's
  answer). A chat can move its commands to this machine or into the container; the Nix
  shell still follows the workspace. The plan travels as `byob_tools::EnvPlan` in the
  `ToolContext` (subagents get the same).

**Protocol** (version 10): `NewChat { workspace? }`, `CreateWorkspace { name, folder:
Existing{path} | Create, config }` (answered with `WorkspaceCreated`; the client then looks at
a new chat there), `RenameWorkspace`, `RemoveWorkspace { clean }` (deletes its chats from
byob, never the folder; `clean` also forgets its answers, installed packages and container),
`GetConfig` / `SetConfig { scope: global | workspace | chat, layer, api_key? }` (answered with
`Config`: own layer, effective values with sources), `ListFolders { path? }` (the engine's
subfolders, hidden ones left out, at most 500: a browser picks folders on the engine's
machine), `CheckContainers`. `Workspaces` lists them with chat counts, most recently active
first. Clients' messages are size-checked first (`ClientMsg::check_sizes`); the engine checks
the rest (unknown ids, settings that don't exist at a level, relative or missing folders, the
filesystem root, image references).

**UI.** The chat list groups chats by workspace (collapsible; most recently active first; a
`+` per group; a `…` menu: New chat here, Workspace settings, Rename, Remove), pinned chats
stay on top. Groups fill as the recent-first pages arrive; a group with more chats than loaded
offers "N more…". **+ Workspace** opens the form: name, a fresh folder in the workspaces folder
or an existing one (Browse… lists the engine's folders, Choose… is the desktop's own dialog),
and where commands run (Container: devcontainer.json or an image, and a check that Docker or
Podman answers). Workspace settings and the chat settings popover (the sliders button next to
the chat title, or ⋯ → Chat settings) show each value's source with a Reset per field; the app
Settings (gear in the header and at the bottom of the chat list, Ctrl/Cmd+,) has the global
defaults. Moving a chat to another workspace isn't offered: its transcript, artifacts
(`.byob/artifacts` in the folder), uploads and undo backups refer to the old folder.

## Inference

`InferenceConfig { base_url, model, api_key }`. `byob-llm` posts to
`{base_url}/chat/completions` with `stream: true`, assembles content, reasoning
(`reasoning_content`/`reasoning`, or inline `<think>` tags) and streamed `tool_calls`,
and reports usage. Because there's no grammar-constrained local engine anymore, the agent
will run the tool-calling rounds itself (cap 30 per turn, as hob did).

### Connecting (first run)

byob brings no model, so the engine keeps track of whether its endpoint answers
(`byob-agent::connect`): a quick `GET /models` (5 s) at startup and whenever the endpoint
is set, and the outcome of every reply, give `EngineState.link` (`checking` / `ok` /
`failed` with a `Problem`: unreachable, bad key, wrong path, missing model, rate limit,
server error, each with a headline and advice in `byob-protocol::connect`). The first
endpoint that works is saved as `settings.json` `inference.last_good`; `onboarded` (that,
or `--inference-url`) decides whether an empty chat shows the welcome screen. Until then
the engine also looks for llama-server, Ollama, LM Studio and vLLM on their usual loopback
ports (800 ms each, in parallel; `ClientMsg::DetectEndpoints` repeats it).

The welcome screen and Settings share one form (`byob-ui-egui::connect`): presets, URL,
model (a picker once the server lists models), key, **Test connection**
(`ClientMsg::TestInference` → `ServerMsg::InferenceTested`: models, problem, a short summary
from a brief probe without generation) and **Connect** (`SetInference`). The engine does
all requests, so the browser works the same and `127.0.0.1` means the engine's machine. A
stored key follows a URL change only to the same scheme/host/port (`api_key: ""` removes
it); lengths are capped. A reply that failed with a connection problem shows a card with
Connection settings and, if it produced nothing, Retry (`ClientMsg::Retry` deletes the
failed pair and sends the message again). The header's status chip (dot + model + server)
opens the connection settings; the header lays out Settings first so it is never pushed
off screen.

## Storage

The engine holds a `Box<dyn byob_store::ChatStore>` (chats, messages incl. streamed deltas,
steps, compactions, grants, undo journal); `open_backend(&Settings, &Paths)` picks the
implementation from `"storage"` in settings.json (`--storage sqlite|folder` and
`--chats-folder DIR` override it for a run). Both are `Send`, owned by the engine thread, and
run the same contract test suite (`crates/store/src/tests/backends.rs`); the engine tests run
on both too (`test_store::folder_too!`). `export(from, to)` copies everything between them
(timestamps, flags, steps, stats, compactions by position, grants, undo journals; chats the
target already has are skipped), so switching backends is a copy, not a migration:
`byob --export-chats-to folder|sqlite` copies from the storage in use.

**Message ids are per run.** The folder backend issues new message ids every time it opens,
so nothing may persist a message (= transcript entry) id: compactions are stored by message
number in the folder, undo journals are in memory (to persist them, key by task id), artifact
metadata and the UI's per-device pane prefs name only chat and artifact ids.

- **`"sqlite"`** (default): `<data>/byob.db`, WAL, versioned schema (`schema.rs`).
- **`"folder"`**: plain files under `"chats_folder"` (default `<data>/chats-folder`, `~/`
  allowed), meant to be grepped and synced (Syncthing, Dropbox, git):

  ```
  <root>/chats/<YYYY-MM-DD>-<slug>-<shortid>/   (UTC creation date, ASCII slug, 6 hex of the id)
      events.jsonl   canonical, append-only, one {"v":1,"ts":ms,"e":kind,…} per line
      chat.md        generated view: YAML frontmatter + transcript; never read back
  <root>/grants.json global grants          <root>/undo.jsonl  undo entries of tasks no message names
  <root>/.index.json listing cache (derived); deleting a chat removes its folder for good
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
  updates on the next listing or write. Not supported: two byob processes writing one root at
  once (lines would interleave), and true concurrent edits of one chat on two machines (the
  sync tool's conflict copy is ignored as a duplicate). Undo backups stay machine-local.

## hob → byob mapping

| hob (TypeScript)                                   | byob                                  |
|----------------------------------------------------|--------------------------------------|
| `shared/*` (agent, types, chart, effort)           | `byob-types`, `byob-protocol`          |
| `engine/*`, `main/engine.ts`, setup/downloader/catalog/hardware/gguf, `packages/picker`, `catalog/` | **removed** → `byob-llm` (HTTP client to a user-supplied URL) |
| `main/vision/*` (spawned llama-server)             | **removed** → `image_url` parts to the same endpoint |
| `main/ipc.ts`, preload, Electron IPC               | `byob-protocol` + `byob-server`        |
| `main/agent/*`                                     | `byob-agent`                          |
| `main/tools/*`                                     | `byob-tools`                          |
| `main/store.ts`, `settings.ts`, `paths.ts`         | `byob-store`                          |
| `main/plugins/*` (skills, MCP, mcpb)               | future `byob-plugins`, `byob-mcp` (hand-rolled JSON-RPC, no tokio) |
| `main/python/*`                                    | `byob-sandbox` + `byob-exec`           |
| `main/accounts/*`                                  | future `byob-accounts`                |
| renderer (React)                                   | `byob-ui-egui`                        |

## GUI choice

egui/eframe 0.37 (wgpu; WebGPU with WebGL2 fallback on the web). gpui, iced, Slint,
Freya, Xilem and Floem were prototyped on 2026-11-09 (prototypes kept outside this repo). gpui
scored best for chat UX but is Zed-internal with only a third-party crates.io republish;
the others lacked selectable text, accessibility, virtualization, or a clean licence.
