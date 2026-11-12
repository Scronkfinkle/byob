# Plugin backends

A plugin backend is one long-lived process per plugin that byob's engine starts, talks to
in newline-delimited JSON, and stops. It keeps state between calls, serves tools without a
process start per call, receives the plugin's UI messages and the engine events it asks for,
and sends views and messages back to clients. Code: `crates/plugins/src/backend.rs` (the
process host), `crates/agent/src/backends.rs` (the engine side), `crates/plugins/src/
byob_plugin.py` (the Python helper), `crates/plugins/src/pyenv.rs` (Python environments).

## Declaring one

```json
{
  "id": "counter",
  "name": "Counter",
  "backend": "backend.py",
  "python": {"requirements": ["requests>=2.31"]},
  "tools": [{"name": "bump", "description": "…", "params": {…}, "run": "backend"}]
}
```

- `"backend": {"native": "<id>"}` is in-tree Rust the app registered (`byob_plugins::native`,
  see `docs/architecture.md`, Native backends): same messages, same lifecycle, run in byob's
  process on a thread of its own.
- `"backend": "backend.py"` is a Python script in the plugin folder, run with the plugin's
  Python. The long form is `{"run": "backend.py", "args": [], "events": []}`, or
  `{"command": "node", "args": ["server.js"]}` for any other program (on `PATH`, or a path
  inside the plugin folder). `events` lists engine events it gets from the start.
- A tool with `"run": "backend"` is served by the backend (a `tool` request) instead of a
  script started per call. Such a tool needs a `backend`. Its calls count as `execute` for
  the permission gate unless it says otherwise with `"access": "read" | "write" | "network"`
  (the bundled `memory` plugin's `remember`/`forget` are `write`).
- `python.requirements` (≤ 64 pip specs; relative `./x.whl` paths are the plugin's): byob
  makes a virtual environment for them under `<data>/plugin-python/venvs/<hash>`, keyed by
  the interpreter and the sorted list, so plugins with the same list share one. It is built
  the first time the backend or a script tool needs it, off the engine thread: `uv venv` +
  `uv pip install` when `uv` is on `PATH`, else `python -m venv` + `pip install`. A failure
  becomes one of the plugin's problems with the installer's output (its last 3000
  characters) and is kept until the plugin changes or plugins are reloaded. Stopping the
  tool call (or the backend) stops the build; that isn't kept. Two byob processes sharing
  the data folder take turns: a build holds `<hash>.lock` (refreshed while it runs; one
  not refreshed for 2 minutes is taken over), and the other uses what it built.
- Every plugin's Python (backend and script tools) gets byob's helper `byob_plugin.py` on
  `PYTHONPATH` (written to `<data>/plugin-python/lib`).

## Lifecycle

- **Bundled plugins** with a backend (or script tools) run from a copy of their files that
  byob writes once to `<data>/bundled-plugins/<id>-<byob version>-<content hash>/` (read-only
  files; a new byob version or new contents make a new folder and remove the old one). That
  folder is the plugin's `dir`.
- **Made** when a composition (a turn, the plugin list) sees the plugin switched on, trusted
  and with its required settings; until then its `backend` state is `off` (missing
  settings) or absent. One backend per plugin copy (its folder): a chat's scratch plugin,
  a workspace's copy and the global copy are separate processes.
- **Started lazily** on first use (a UI message, a subscribed event, a tool call), on a
  thread of its own: preparing (the venv) and spawning never block the engine. Messages
  wait in a queue (≤ 256, ≤ 16 MiB) while it starts; once it runs, at most 256 wait for it
  to read them (more are dropped, with a fault). Stopping it calls off a venv build in
  progress. Working folder: the plugin folder. Environment: byob's, plus the plugin's
  settings (secret ones too), `BYOB_PLUGIN_DIR`, `PYTHONPATH`, `PYTHONUNBUFFERED=1`,
  `PYTHONUTF8=1`, `PYTHONIOENCODING=utf-8`, minus `BYOB_API_KEY`. On Unix it leads a process
  group of its own, killed with it (so are what it started). No sandbox: like VS Code extensions, an
  installed plugin runs with the user's rights; the trust question for plugins that came
  with a workspace still applies before anything runs.
- **Crashes**: when it exits (even if something it started still holds its stdout) or
  closes stdout, pending requests fail with "the backend exited (exit
  code N); its last output: …" (15 lines of stderr), clients get a new plugin list, and it
  is started again after 0.5 s, doubling to 30 s, or at once when used. Five exits in a
  row (a run of 30 s resets the count) and it stays `failed` until the plugin reloads.
- **Stopped** (stdin closed after a `shutdown` message; killed if it hasn't exited within
  0.5 s) when its plugin's files change (live reload: the next composition makes a fresh
  one, and running turns get tools bound to it from their next round), when it is switched
  off, distrusted or removed, when its settings change, on Reload plugins, and when its
  chat is deleted. A tool call still holding the old one gets "the backend was stopped …;
  try again".
- **Status** (`PluginInfo::backend`, protocol 13.2): `state` (`idle`, `starting`, `running`,
  `crashed`, `failed`, `stopped`, `off`), `starts`, `error`, `faults` (the newest 8) and the
  end of stderr (4 KiB of the 16 KiB kept). The Plugins panel shows it; `plugin_status`
  gives the model the state, error, faults and the last stderr lines, so it can fix its own
  backend.

## Wire format (version 1)

One JSON object per line, UTF-8, both ways; a line is at most 1 MiB (longer lines from the
backend are dropped and recorded as a fault). Every message has a `type`. Unknown types and
unknown fields are ignored by both sides, so either can be newer.

### byob → backend

| type | fields | meaning |
|---|---|---|
| `init` | `v` (1), `plugin`, `name`, `version`, `byob` (app version), `caps` (message types the engine handles), `events` (the engine events there are), `settings` (non-secret values; secret ones only as environment variables), `dir`, `scope` (`chat`/`workspace`/`global`), `chat`, `workspace`, `views` (the declared views: `{id, site, title}`), `restart` (0 on the first start) | always the first line of every process |
| `event` | `event`, plus its fields | `client`: a UI message — `client` (the client's id), `chat` (the chat it looks at), `msg` (any JSON ≤ 256 KiB). `view`: the user did something in one of its views — `view` (id), `site`, `key`, `client`, `chat`, `workspace` (the chat's folder), `action`, `values`, `edit`? (see `docs/views.md`); answer with a `view` message. Engine events (only those subscribed), all with `chat` and `workspace`: `chat_opened` {`client`}, `turn_started` {`entry`}, `turn_finished` {`entry`, `error`, `stopped`}, `step_done` {`entry`, `step`, `tool`, `status`, `title`}, `artifact_updated` {`artifact`, `title`, `content_type`, `version`}. `web`: a web view's page called `byob.send(msg)` — `view`, `key`, `client`, `chat`, `msg` (`{"msg": …}`) |
| `request` | `id` (number), `method`, `params` | answer with a `reply` of the same `id`. `tool`: `params` {`name` (as declared), `args` (the model's arguments), `workspace`, `chat`}; the result is text, `{"text", "files"}` (files relative to the workspace), `{"text", "card": <tree>, "card_view"?, "card_key"?}` (a live card on the tool's step, see below), `{"text", "artifact": {…}}` (saves an artifact version, see Artifacts) or any JSON (shown as JSON). `view`: {`site`, `view` (id), `key`, `client`, `chat`, `workspace`}: a client opened the view and the engine has no tree for it; the result is the tree (or `{"view": tree}`). `hook`: a declared hook point (see Hooks). `web`: {`view`, `key`, `client`, `chat`, `msg`}: a web view's page called `byob.ask(msg)` (`msg` is `{"msg": …}` as the page sent it); the result is the page's answer |
| `cancel` | `id` | the caller gave up (stopped by the user, or 60 s passed); a late reply is dropped |
| `shutdown` | | exit now (stdin closes next) |

Chat plugins only get events of their chat, workspace plugins only of chats in their
workspace.

### backend → byob

| type | fields | meaning |
|---|---|---|
| `reply` | `id`, `result` or `error` (text) | ends a request |
| `view` | `id` (a declared view; or `site`, for the plugin's view at that site), `key` (`""`), `view` (the whole tree) or `patch` (patches), `values`?, `client`? | the view's new state: the engine applies it, diffs it against the last one and sends the patches to every client showing it (`values` only to `client`) |
| `notify` | `msg`, `client`? | `msg` forwarded to clients as `ServerMsg::Plugin { plugin, msg }` |
| `notice` | `text`, `level` (`info`/`error`) | a notice in clients, prefixed with the plugin id |
| `log` | `text` | into its stderr ring and byob's log |
| `fault` | `text` | a problem for the model and the user: kept in its status (`faults`); clients hear of faults at most once a second (the newest, "(N more)") |
| `subscribe` / `unsubscribe` | `events` | engine events to get from now on (unknown names are a fault) |
| `artifact` | `chat`, `file`, `id`?, `title`?, `filename`?, `note`?, `draft`? | save `file`'s bytes as a version of the chat's artifact `id` (a new one without it); see Artifacts |
| `web` | `view`, `key`? (`""`), `msg` (any JSON ≤ 256 KiB), `client`? | to the pages of web view `view` (clients that listed `web_bridge`; a chat plugin's only to those looking at its chat): they get it as a `byob/message` notification |

Without `client`, messages go to every client that listed the `plugin_channel` cap (for a
chat plugin: those looking at its chat). Messages to clients are capped at 256 KiB.
Anything on stdout that isn't a JSON object is a fault and goes into the stderr ring.

## Views

A plugin declares its views in the manifest, and its backend draws them as view trees
(`docs/views.md`):

```json
"views": [
  {"site": "pane", "id": "list", "title": "To-do", "icon": "list-checks"},
  {"site": "status", "id": "count"},
  {"site": "settings", "id": "prefs", "title": "To-do settings"},
  {"site": "card", "id": "item"}
]
```

`site` is `pane` (a tab of the side pane beside the chat, toggled from the header),
`status` (a small item in the header: a chip, a few words), `settings` (a section under the
plugin in Settings → Plugins) or `card` (on the step of one of its backend tools). `id`
(`[a-z0-9_-]`, unique; the site when left out), `title` (the plugin's name when left out),
`icon`. At most 16; views need a `backend`.

The engine owns every view's tree (by plugin folder, view id and key — `""`, or each
card's own): it asks the backend (`request view`) when a client opens one it has no tree
for, keeps the last tree, and turns every new `view` message into patches for the clients
showing it. A tree is checked on the way in; what had to be fixed becomes a fault in the
plugin's status (`plugin_status` shows it to the model) and a warning at the site. Trees
are dropped when the backend is stopped or restarts (a reload, a switch, a crash):
clients open them again and the new process draws them.

A backend tool shows a card on its step by returning `{"text": …, "card": <tree>,
"card_view"?: id, "card_key"?: key}` (the first `card` view when `card_view` is left out;
a fresh key when `card_key` is). The step keeps the tree it had then; clients show the live
one, and the user's clicks on it are `view` events with that key.

## Artifacts

A plugin can own artifact types end to end: declare them (`artifact_types`, `"binary": true`
for files that aren't text, which `create_artifact` & co. then leave alone), draw them with
one of its views (`"renderers": [{"type": …, "renderer": "view:<view id>"}]` naming a view
with `"site": "artifact"`), and write their versions from its backend.

- **The view.** An artifact-site view is drawn in the artifact's card in the chat and in the
  pane, one instance per version: its key is `"<artifact id>@<version>"`. `request view` and
  its `view` events carry `artifact`: `{id, version, latest, path (that version's file,
  absolute), filename, content_type, title, workspace, chat}` (`byob.artifact(key)` in the
  helper). A key that isn't an artifact of the client's chat never reaches the backend.
  Clients keep a place's selection and scroll when it moves on to a newer version.
- **Versions from a tool**: return `{"text": …, "artifact": {"id"?, "title"?, "filename"?,
  "file", "note"?}}` (or `"artifacts": [...]`, ≤ 4). `file` is a path (absolute or in the
  workspace) whose bytes byob copies into `.byob/artifacts/<id>/v<n>.<ext>`; without `id` it
  is a new artifact (`title` and `filename` needed). A `filename` on an existing artifact
  renames it and its type follows the new name (a `.csv` table that becomes an `.xlsx`
  workbook). byob appends `Saved: artifact <id>, version <n> (<file name>).` to the text, and
  the step shows the artifact like any other.
- **Versions from the user** (edits in the view): send `{"type": "artifact", "chat", "id",
  "file", "note"?, "draft": true}` (`byob.save_artifact(file, chat, id=…, draft=True)`).
  Consecutive drafts replace one version until the model writes the artifact or a turn
  starts in the chat; then the next edit makes a new one. Clients get `ArtifactUpdated`
  (the pane moves to the new version); subscribers get `artifact_updated`. A chat plugin
  may only save in its own chat; problems are faults.
### Web views

A `pane` or `card` view may be an HTML page instead of a tree: `{"site": "pane", "id":
"notes", "html": "ui/index.html"}` (a file in the plugin folder; a plugin with only web views
needs no `backend`). It runs on byob's web surface (the Servo helper on the desktop, a
sandboxed iframe in the browser; no network, nothing from URLs) with a small `window.byob`
API over the bridge (`docs/architecture.md`, Web surfaces):

| page | goes to |
|---|---|
| `byob.ready` | a promise of the host context (`theme`, `styles.variables`, `platform`, …) after the handshake |
| `byob.tool(name, args)` | one of the plugin's `"run": "backend"` tools (no approval card: the plugin's own UI); resolves to `{content: [{type: "text", text}], isError}` |
| `byob.ask(msg)` / `byob.send(msg)` | a `web` request (its result is the answer) / a `web` event to the backend |
| `byob.context(text, data?)` | the chat's next system prompt (one per page, the last wins; an empty text clears it) |
| `byob.message(text)` | a user message in the chat (queued if a reply runs) |
| `byob.openLink(url)` | the system browser (http/https) |
| `byob.on(fn)` | `fn(params, method)` for `byob/message` (the backend's `web` messages), `byob/card`, `byob/artifact` |
| `byob.call(method, params)` | any bridge method (the MCP Apps JSON-RPC names) |

A web card: the tool returns `{"text", "card": <any JSON>, "card_view": "<web card view>"}`;
the step gets a `byob.web` view and the page `byob/card` `{data, key}`. An artifact renderer
`{"type": …, "renderer": "web:ui/x.html"}` draws that type with a page, which gets
`byob/artifact` `{id, version, title, content_type, text}`. The theme is in the page as CSS
variables from the start (`--byob-*` and the MCP Apps names, `--color-background-primary`
etc.) and follows changes.

## Hooks

A backend can sit in the turn pipeline (`docs/architecture.md`, Hooks). Declare each hook
point it answers:

```json
"hooks": [
  {"event": "before_tool_call", "tools": ["delete_file"], "fail": "closed", "timeout_ms": 2000},
  {"event": "before_prompt", "priority": 700}
]
```

`event`: `before_prompt`, `before_request`, `before_tool_call`, `after_tool_call`,
`on_turn_end`. `priority` (default 100; lower runs first; byob's prompt sections sit at 0–600,
compaction at 100). `timeout_ms` (default 2000, ≤ 60000). `fail`: `open` (default: a fault or
timeout counts as continue) or `closed` (it stops the stage: a tool call is denied, a request
not made). `tools` limits the tool events to those tools. A hook without `native` needs a
`backend`; it runs while the plugin is on and trusted, on the turn's thread, so it delays
that turn only.

Each one is a `request` with `method: "hook"` and `params` `{event, …}`:

| event | params | reply `modify` fields |
|---|---|---|
| `before_prompt` | `chat`, `workspace`, `sections` [{`id`, `text`}] so far | `add` (texts or {`id`, `text`}, appended), `sections` (the whole new list) |
| `before_request` | `entry`, `round`, `child`, `messages` (OpenAI format; `null` over 512 KiB), `tools` (names), `estimated_tokens`, `context_window` | `messages` (the whole list; images are dropped), `tools` (names to keep) |
| `before_tool_call` | `chat`, `workspace`, `tool`, `args`, `access` (read/write/network/execute), `child` | `args` (an object) |
| `after_tool_call` | `chat`, `workspace`, `tool`, `args`, `result`, `failed`, `child` | `result` (text), `card` (a view tree shown on the step) |
| `on_turn_end` | `entry`, `chat`, `workspace`, `answer`, `error`, `stopped` | (ignored) |

The `result`: `null`, `{}` or `{"action": "continue"}` carries on; `{"action": "modify", …}`
(or an object with fields and no `action`) changes things; `{"action": "stop", "reason"}`
ends the chain (`before_request`: the turn ends with the reason). `before_tool_call` also
takes `{"action": "allow"}` (run without the permission gate; calls that must always be
confirmed still ask), `{"action": "deny", "message"}` (not run; the model gets "Not run:
<message>") and `{"action": "ask", "message"?}` (an approval card a person answers, even with
`--auto-approve`). An `error` reply, a timeout or an unknown action is a fault in the
plugin's status and fails open or closed as declared.

## The Python helper (`byob_plugin`)

```python
import byob_plugin as byob

@byob.tool                    # "run": "backend" tools, called with keyword arguments
def bump(by: int = 1): ...

@byob.view("list")            # draws a declared view: fn() -> tree (cards: fn(key) -> tree)
@byob.on_event("list")        # its events: fn(event); return {"values": {...}} to set fields
@byob.on_message              # plugin-channel messages: fn(msg, client)
@byob.on("turn_finished")     # engine events: fn(event); subscribed for you
@byob.on_start                # fn(init), after every (re)start
@byob.on_web("notes")         # a web view's byob.ask / byob.send: fn(msg, event) -> answer
@byob.hook("before_tool_call")  # a declared hook: fn(params) -> None (continue) or a reply:
                              # byob.allow(), byob.deny(msg), byob.ask(msg), byob.stop(reason),
                              # byob.modify(add=[...], args={...}, result=..., card=tree, ...)

byob.card(text, "item", key)  # a tool result with a card (drawn by @view("item"))
byob.context()                # in a tool: {"name", "workspace", "chat"} of the call
byob.artifact(key)            # an artifact-site view's artifact (see Artifacts)
byob.save_artifact(file, chat, id=None, title=None, filename=None, note="", draft=False)
byob.refresh(view_id=None)    # draw views again and send them (after changing state elsewhere)
byob.send_view(tree, id, key="", values=None, client=None)
byob.send(msg, client=None); byob.notice(text, level="info"); byob.log(*parts)
byob.web_send(view_id, msg, key="", client=None)   # to a web view's pages (byob.on in the page)
byob.fault(text); byob.subscribe(*events); byob.settings; byob.init
byob.run()                    # or run({"tool:bump": f, "message": g, "turn_finished": h})
```

After every view event, tool call and plugin-channel message the helper calls every `@view`
function again (for each key it has drawn) and sends the trees; the engine sends clients
only what changed, so a plugin never has to work out patches. Requests are handled one at
a time on the main thread. A handler's exception becomes the reply's `error` (the last 12
traceback lines) or, for events and drawing, a `fault`. Once `run()` is called, `print()`
goes to stderr, so it can't break the protocol.

## Measured

Python 3.13, Linux: a `tool` request round trip on a running backend averages 21 µs (300
requests); a UI message through the engine to the backend and back to the client 62 µs (50
messages); a web view's `byob.ask` through the engine to the backend and back 117 µs (200,
with a worker thread per call; an engine-only `ping` 4 µs). From inside a page, adding the
web surface: see `docs/architecture.md` (Web surfaces).
Spawning a process per event costs ~15 ms. A venv with one local wheel builds
in 1.5 s (pip, no network) and is found again instantly by the next engine run.

## Not yet

Concurrent requests inside the helper, killing a backend's own children on Windows (no
process groups there), per-backend resource limits; a card's
stored tree isn't updated when the live one changes (the step keeps the first state).
