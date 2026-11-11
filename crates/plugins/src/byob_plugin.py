"""byob_plugin: the small helper for byob plugin backends (shipped with byob).

A backend is a long-lived program byob starts for a plugin. It reads one JSON message per
line on stdin and writes one per line on stdout (docs/plugins-backend.md). This module does
that loop for you:

    import byob_plugin as byob

    count = 0

    @byob.tool
    def add(n: int = 1):
        global count
        count += n
        byob.send_view({"type": "text", "text": f"count: {count}"})
        return f"count is now {count}"

    @byob.on_message
    def clicked(msg, client):
        byob.send({"echo": msg}, client=client)

    @byob.on("turn_finished")
    def done(event):
        byob.log("a turn finished in", event.get("chat"))

    byob.run()

print() goes to byob's log of the plugin (stderr), never to byob itself.
"""

import json
import sys
import threading
import traceback

_out = sys.stdout
sys.stdout = sys.stderr  # stray print()s must not break the protocol
_lock = threading.Lock()

VERSION = 1
init = {}  # byob's init message: plugin, byob, caps, settings, dir
settings = {}  # the plugin's settings that aren't secret (secret ones are environment variables)

_tools = {}
_events = {}
_messages = []
_starts = []
_views = []


def _send(msg):
    line = json.dumps(msg, separators=(",", ":"), default=str)
    with _lock:
        _out.write(line + "\n")
        _out.flush()


def tool(fn=None, *, name=None):
    """A tool the model can call (declare it in byob-plugin.json with "run": "backend").
    It gets the model's arguments as keyword arguments and returns text (or a dict)."""
    def register(f):
        _tools[name or f.__name__] = f
        return f
    return register(fn) if fn is not None else register


def on(event):
    """Call fn(event) for an engine event: chat_opened, turn_started, turn_finished,
    step_done, artifact_updated. byob subscribes for you."""
    def register(f):
        _events.setdefault(event, []).append(f)
        return f
    return register


def on_message(fn):
    """Call fn(msg, client) for each message the plugin's UI sends."""
    _messages.append(fn)
    return fn


def on_start(fn):
    """Call fn(init) once byob has started the backend."""
    _starts.append(fn)
    return fn


def on_view(fn):
    """Call fn(site, key, client) when a client asks for a view; return the view."""
    _views.append(fn)
    return fn


def send(msg, client=None):
    """Send msg (any JSON) to the plugin's UI: one client, or every client when None."""
    _send({"type": "notify", "msg": msg, "client": client})


def send_view(view, key="main", site="pane", client=None):
    """Show `view` (a view tree, a dict) at a render site under `key`."""
    _send({"type": "view", "site": site, "key": key, "view": view, "client": client})


def notice(text, level="info"):
    """A short notice in every client ("info" or "error")."""
    _send({"type": "notice", "text": str(text), "level": level})


def log(*parts):
    """A line in byob's log of this plugin (plugin_status shows the recent ones)."""
    _send({"type": "log", "text": " ".join(str(p) for p in parts)})


def fault(text):
    """Report a problem: plugin_status shows it to the model so it can fix the plugin."""
    _send({"type": "fault", "text": str(text)})


def subscribe(*events):
    """Get these engine events from now on (see on())."""
    _send({"type": "subscribe", "events": list(events)})


def _short_traceback():
    return "".join(traceback.format_exc().splitlines(True)[-12:]).strip()


def _call(handlers, *args):
    for h in handlers:
        try:
            h(*args)
        except Exception:
            traceback.print_exc()
            fault(_short_traceback())


def _request(msg):
    rid = msg.get("id")
    method = msg.get("method")
    params = msg.get("params") or {}
    try:
        if method == "tool":
            fn = _tools.get(params.get("name"))
            if fn is None:
                raise LookupError(f"this backend has no tool {params.get('name')!r} (known: {', '.join(_tools) or 'none'})")
            args = params.get("args") or {}
            result = fn(**args) if isinstance(args, dict) else fn(args)
        elif method == "view":
            if not _views:
                raise LookupError("this backend has no @on_view handler")
            result = _views[0](params.get("site"), params.get("key"), params.get("client"))
        else:
            raise LookupError(f"unknown request {method!r}")
        _send({"type": "reply", "id": rid, "result": result})
    except Exception:
        traceback.print_exc()
        _send({"type": "reply", "id": rid, "error": _short_traceback()})


def run(handlers=None):
    """Serve byob until it closes stdin. `handlers` may also map names to functions:
    {"tool:add": add, "message": fn, "start": fn, "view": fn, "turn_finished": fn}."""
    for key, fn in (handlers or {}).items():
        if key.startswith("tool:"):
            _tools[key[5:]] = fn
        elif key == "message":
            _messages.append(fn)
        elif key == "start":
            _starts.append(fn)
        elif key == "view":
            _views.append(fn)
        else:
            _events.setdefault(key, []).append(fn)
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except ValueError:
            continue
        if not isinstance(msg, dict):
            continue
        kind = msg.get("type")
        if kind == "init":
            init.clear()
            init.update(msg)
            settings.clear()
            settings.update(msg.get("settings") or {})
            if _events:
                subscribe(*_events)
            _call(_starts, msg)
        elif kind == "event":
            if msg.get("event") == "client":
                _call(_messages, msg.get("msg"), msg.get("client"))
            else:
                _call(_events.get(msg.get("event"), []), msg)
        elif kind == "request":
            _request(msg)
        elif kind == "shutdown":
            break
        # anything else is from a newer byob: ignored
