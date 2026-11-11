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

- `"backend": "backend.py"` is a Python script in the plugin folder, run with the plugin's
  Python. The long form is `{"run": "backend.py", "args": [], "events": []}`, or
  `{"command": "node", "args": ["server.js"]}` for any other program (on `PATH`, or a path
  inside the plugin folder). `events` lists engine events it gets from the start.
- A tool with `"run": "backend"` is served by the backend (a `tool` request) instead of a
  script started per call. Such a tool needs a `backend`.
- `python.requirements` (≤ 64 pip specs; relative `./x.whl` paths are the plugin's): byob
  makes a virtual environment for them under `<data>/plugin-python/venvs/<hash>`, keyed by
  the interpreter and the sorted list, so plugins with the same list share one. It is built
  the first time the backend or a script tool needs it, off the engine thread: `uv venv` +
  `uv pip install` when `uv` is on `PATH`, else `python -m venv` + `pip install`. A failure
  becomes one of the plugin's problems with the installer's output (its last 3000
  characters) and is kept until the plugin changes or plugins are reloaded.
- Every plugin's Python (backend and script tools) gets byob's helper `byob_plugin.py` on
  `PYTHONPATH` (written to `<data>/plugin-python/lib`).

## Lifecycle

- **Made** when a composition (a turn, the plugin list) sees the plugin switched on, trusted
  and with its required settings; until then its `backend` state is `off` (missing
  settings) or absent. One backend per plugin copy (its folder): a chat's scratch plugin,
  a workspace's copy and the global copy are separate processes.
- **Started lazily** on first use (a UI message, a subscribed event, a tool call), on a
  thread of its own: preparing (the venv) and spawning never block the engine. Messages
  wait in a queue (≤ 256) while it starts. Working folder: the plugin folder. Environment:
  byob's, plus the plugin's settings (secret ones too), `BYOB_PLUGIN_DIR`, `PYTHONPATH`,
  `PYTHONUNBUFFERED=1`, minus `BYOB_API_KEY`. No sandbox: like VS Code extensions, an
  installed plugin runs with the user's rights; the trust question for plugins that came
  with a workspace still applies before anything runs.
- **Crashes**: when stdout closes, pending requests fail with "the backend exited (exit
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
| `init` | `v` (1), `plugin`, `name`, `version`, `byob` (app version), `caps` (message types the engine handles), `events` (the engine events there are), `settings` (non-secret values; secret ones only as environment variables), `dir`, `scope` (`chat`/`workspace`/`global`), `chat`, `workspace`, `restart` (0 on the first start) | always the first line of every process |
| `event` | `event`, plus its fields | `client`: a UI message — `client` (the client's id), `chat` (the chat it looks at), `msg` (any JSON ≤ 256 KiB). Engine events (only those subscribed), all with `chat`: `chat_opened` {`client`}, `turn_started` {`entry`}, `turn_finished` {`entry`, `error`, `stopped`}, `step_done` {`entry`, `step`, `tool`, `status`, `title`}, `artifact_updated` {`artifact`, `title`, `content_type`, `version`} |
| `request` | `id` (number), `method`, `params` | answer with a `reply` of the same `id`. `tool`: `params` {`name` (as declared), `args` (the model's arguments), `workspace`, `chat`}; the result is text, `{"text", "files"}` (files relative to the workspace) or any JSON (shown as JSON). `view`: {`site`, `key`, `client`}, answer with a view |
| `cancel` | `id` | the caller gave up (stopped by the user, or 60 s passed); a late reply is dropped |
| `shutdown` | | exit now (stdin closes next) |

Chat plugins only get events of their chat, workspace plugins only of chats in their
workspace.

### backend → byob

| type | fields | meaning |
|---|---|---|
| `reply` | `id`, `result` or `error` (text) | ends a request |
| `view` | `site` (e.g. `pane`), `key`, `view` (opaque JSON for now; the view tree in P2c), `client`? | forwarded to clients as `ServerMsg::Plugin { plugin, msg: {type: "view", site, key, view} }` |
| `notify` | `msg`, `client`? | `msg` forwarded to clients as `ServerMsg::Plugin { plugin, msg }` |
| `notice` | `text`, `level` (`info`/`error`) | a notice in clients, prefixed with the plugin id |
| `log` | `text` | into its stderr ring and byob's log |
| `fault` | `text` | a problem for the model and the user: kept in its status (`faults`) |
| `subscribe` / `unsubscribe` | `events` | engine events to get from now on (unknown names are a fault) |

Without `client`, messages go to every client that listed the `plugin_channel` cap (for a
chat plugin: those looking at its chat). Messages to clients are capped at 256 KiB.
Anything on stdout that isn't a JSON object is a fault and goes into the stderr ring.

## The Python helper (`byob_plugin`)

```python
import byob_plugin as byob

@byob.tool                    # "run": "backend" tools, called with keyword arguments
def bump(by: int = 1): ...

@byob.on_message              # UI messages: fn(msg, client)
@byob.on("turn_finished")     # engine events: fn(event); subscribed for you
@byob.on_start                # fn(init), after every (re)start
@byob.on_view                 # fn(site, key, client) -> view, for view requests

byob.send(msg, client=None); byob.send_view(view, key="main", site="pane", client=None)
byob.notice(text, level="info"); byob.log(*parts); byob.fault(text); byob.subscribe(*events)
byob.settings; byob.init
byob.run()                    # or run({"tool:bump": f, "message": g, "turn_finished": h})
```

Requests are handled one at a time on the main thread. A handler's exception becomes the
reply's `error` (the last 12 traceback lines) or, for events, a `fault`. Once `run()` is
called, `print()` goes to stderr, so it can't break the protocol.

## Measured

Python 3.13, Linux: a `tool` request round trip on a running backend averages 21 µs (300
requests); a UI message through the engine to the backend and back to the client 62 µs (50
messages). Spawning a process per event costs ~15 ms. A venv with one local wheel builds
in 1.5 s (pip, no network) and is found again instantly by the next engine run.

## Not yet

View requests from clients (`request` `view` is in the wire format and the helper, but the
engine doesn't send it yet: P2c), patches/diffing of views, concurrent requests inside the
helper, process groups (a backend's own children aren't killed with it), backends for
native built-ins, per-backend resource limits.
