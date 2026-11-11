"""byob_plugin: the small helper for byob plugin backends (shipped with byob).

A backend is a long-lived program byob starts for a plugin. It reads one JSON message per
line on stdin and writes one per line on stdout (docs/plugins-backend.md). This module does
that loop for you:

    import byob_plugin as byob

    count = 0

    @byob.view("counter")           # declared in byob-plugin.json "views"
    def counter():
        return {"type": "row", "children": [
            {"type": "text", "text": f"Count: {count}"},
            {"type": "button", "label": "+", "action": "inc"}]}

    @byob.on_event("counter")       # a click, Enter in a field, a cell edit in that view
    def clicked(event):             # event["action"], event["values"], event["key"]
        global count
        if event["action"] == "inc":
            count += 1              # byob redraws every view after this

    @byob.tool
    def add(n: int = 1):
        global count
        count += n
        return f"count is now {count}"

    @byob.on("turn_finished")
    def done(event):
        byob.log("a turn finished in", event.get("chat"))

    @byob.hook("before_tool_call")  # declared in byob-plugin.json "hooks"
    def guard(call):                # call["tool"], call["args"], call["access"], ...
        if call["tool"] == "delete_file":
            return byob.deny("Deleting is off in this workspace.")
        # returning None carries on (the permission gate decides)

    byob.run()

After every view event, tool call and UI message the helper calls each @view function again
and sends the result; byob sends clients only what changed. Call byob.refresh() after
changing state anywhere else (a thread, an engine event).

In a backend (once run() is called), print() goes to byob's log of the plugin (stderr).
"""

import inspect
import json
import sys
import threading
import traceback

_out = sys.stdout  # the protocol's stream; run() sends print() to stderr instead
_lock = threading.Lock()

VERSION = 1
init = {}  # byob's init message: plugin, byob, caps, settings, dir, views
settings = {}  # the plugin's settings that aren't secret (secret ones are environment variables)

_tools = {}
_events = {}
_messages = []
_starts = []
_views = []  # legacy @on_view handlers: fn(site, key, client)
_view_fns = {}  # view id -> fn() or fn(key)
_event_fns = {}  # view id (None: every view) -> [fn(event)]
_keys = {}  # view id -> keys drawn so far ("" for pane, settings, status; one per card)
_hooks = {}  # event -> fn(payload)
_cards = [0]
_context = {}  # the tool request being served: name, workspace, chat
_artifacts = {}  # (view id, key) -> the artifact an "artifact" site view shows
MAX_ARTIFACT_KEYS = 16  # artifact views drawn again after changes (the newest ones)
_web_fns = {}  # web view id (None: every one) -> fn(msg, event)


def _send(msg):
    line = json.dumps(msg, separators=(",", ":"), default=str)
    with _lock:
        _out.write(line + "\n")
        _out.flush()


def tool(fn=None, *, name=None):
    """A tool the model can call (declare it in byob-plugin.json with "run": "backend").
    It gets the model's arguments as keyword arguments and returns text (or a dict, or
    byob.card(...) to show a card on its step)."""
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


def hook(event):
    """Answer the turn pipeline's hook `event` (before_prompt, before_request,
    before_tool_call, after_tool_call, on_turn_end; declare it in byob-plugin.json "hooks"
    with no "native"). fn(payload) returns None to carry on, or one of modify(...),
    stop(reason), allow(), deny(message), ask(message)."""
    def register(f):
        _hooks[event] = f
        return f
    return register


def modify(**fields):
    """A hook reply that changes things: before_prompt add=[text] / sections=[{id, text}],
    before_request messages=[...] / tools=[names], before_tool_call args={...},
    after_tool_call result=text / card=tree."""
    return dict(fields, action="modify")


def stop(reason=""):
    """A hook reply: no later hooks (before_request: the turn ends with `reason`)."""
    return {"action": "stop", "reason": str(reason)}


def allow():
    """before_tool_call: run the call without asking."""
    return {"action": "allow"}


def deny(message):
    """before_tool_call: don't run the call; the model gets `message`."""
    return {"action": "deny", "message": str(message)}


def ask(message=""):
    """before_tool_call: a person decides on an approval card (`message` on it)."""
    return {"action": "ask", "message": str(message)}


def on_message(fn):
    """Call fn(msg, client) for each plugin-channel message from a client."""
    _messages.append(fn)
    return fn


def on_start(fn):
    """Call fn(init) once byob has started the backend."""
    _starts.append(fn)
    return fn


def on_view(fn):
    """Low level: call fn(site, key, client) for views without a @view function."""
    _views.append(fn)
    return fn


def view(view_id):
    """The function that draws view `view_id` (declared in byob-plugin.json "views"). It
    returns a view tree (a dict). A card view's function gets the card's key: def f(key)."""
    def register(f):
        _view_fns[view_id] = f
        return f
    return register


def on_event(view_id=None):
    """Call fn(event) when the user does something in view `view_id` (any view when None).
    event: {"action", "values" (every named field), "key", "view", "site", "client", "edit"?}.
    Return None, or {"values": {"field": value}} to also set fields for that user, e.g.
    {"values": {"new": ""}} to clear an input after Add. Every view is redrawn after it."""
    if callable(view_id):  # @byob.on_event without parentheses
        _event_fns.setdefault(None, []).append(view_id)
        return view_id

    def register(f):
        _event_fns.setdefault(view_id, []).append(f)
        return f
    return register


def _site(view_id):
    for v in init.get("views") or []:
        if isinstance(v, dict) and v.get("id") == view_id:
            return v.get("site")
    return None


class ToolError(Exception):
    """Raise from a tool for an expected failure: the model gets the message as the call's
    error, without a traceback."""


def context():
    """The tool request being served: {"name", "workspace", "chat"} (empty outside one)."""
    return dict(_context)


def artifact(key, view_id=None):
    """The artifact an "artifact"-site view (a renderer: {"renderer": "view:<id>"}) shows
    for `key` ("<artifact id>@<version>"): {"id", "version", "latest", "path" (that
    version's file), "filename", "content_type", "title", "workspace", "chat"}; None if byob
    hasn't said."""
    if view_id is None:
        for (vid, k), a in _artifacts.items():
            if k == key:
                return a
        return None
    return _artifacts.get((view_id, key))


def save_artifact(file, chat, id=None, title=None, filename=None, note="", draft=False):
    """Save `file` (a path; byob copies it) as a new version of artifact `id` in `chat`
    (a new artifact when `id` is None: give `title` and `filename`). `draft=True` (edits the
    user made in a view): consecutive drafts replace one version until the model writes or
    a turn starts. From a tool, return {"text": ..., "artifact": {...}} instead."""
    msg = {"type": "artifact", "chat": chat, "file": str(file), "note": str(note), "draft": bool(draft)}
    for k, v in (("id", id), ("title", title), ("filename", filename)):
        if v is not None:
            msg[k] = v
    _send(msg)


def _remember_key(view_id, key, params):
    keys = _keys.setdefault(view_id, set())
    keys.add(key)
    a = params.get("artifact") if isinstance(params, dict) else None
    if isinstance(a, dict):
        _artifacts[(view_id, key)] = a
        mine = [k for (v, k) in _artifacts if v == view_id]
        while len(mine) > MAX_ARTIFACT_KEYS:
            old = mine.pop(0)
            _artifacts.pop((view_id, old), None)
            keys.discard(old)


def _draw(view_id, key):
    fn = _view_fns[view_id]
    try:
        takes_key = len(inspect.signature(fn).parameters) > 0
    except (TypeError, ValueError):
        takes_key = True
    return fn(key) if takes_key else fn()


def refresh(view_id=None, values=None, client=None):
    """Draw view `view_id` (every @view when None) again and send it to byob."""
    ids = [view_id] if view_id is not None else list(_view_fns)
    for vid in ids:
        if vid not in _view_fns:
            continue
        keys = set(_keys.get(vid) or ())
        if _site(vid) not in ("card", "artifact"):
            keys.add("")
        for key in sorted(keys):
            try:
                tree = _draw(vid, key)
            except Exception:
                traceback.print_exc()
                fault(f"view {vid}: drawing it failed:\n" + _short_traceback())
                continue
            msg = {"type": "view", "id": vid, "key": key, "view": tree}
            if values:
                msg["values"] = values
                msg["client"] = client
            _send(msg)


def card(text, view_id=None, key=None, tree=None):
    """A tool result that shows a card on the tool's step: `tree`, or what @view(view_id)
    draws for `key`. Return it from a "run": "backend" tool."""
    if key is None:
        _cards[0] += 1
        key = f"card{_cards[0]}-{threading.get_ident() & 0xffff:x}"
    key = str(key)
    if view_id is not None:
        _keys.setdefault(view_id, set()).add(key)
        if tree is None and view_id in _view_fns:
            tree = _draw(view_id, key)
    out = {"text": str(text), "card": tree, "card_key": key}
    if view_id is not None:
        out["card_view"] = view_id
    return out


def send(msg, client=None):
    """Send msg (any JSON) to clients over the plugin channel: one client, or all when None."""
    _send({"type": "notify", "msg": msg, "client": client})


def send_view(tree, id=None, key="", values=None, client=None, site=None):
    """Show `tree` (a view tree, a dict) as view `id` (declared in byob-plugin.json). byob
    works out what changed and updates every client showing it."""
    msg = {"type": "view", "id": id, "key": key, "view": tree}
    if site is not None:
        msg["site"] = site
    if values:
        msg["values"] = values
        msg["client"] = client
    _send(msg)


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


def on_web(view_id=None):
    """A web view's messages: fn(msg, event). `msg` is what the page sent with
    byob.send(msg) or byob.call(msg); `event` has "view", "key", "client", "chat". For
    byob.call the return value is the page's answer (an exception becomes its error).
    Use as @byob.on_web("panel") (or @byob.on_web for every web view)."""
    if callable(view_id):
        _web_fns[None] = view_id
        return view_id
    def register(f):
        _web_fns[view_id] = f
        return f
    return register


def web_send(view_id, msg, key="", client=None):
    """Send `msg` (any JSON) to the pages of web view `view_id` (all clients showing it, or
    only `client`); the page gets it in byob.on(fn)."""
    out = {"type": "web", "view": view_id, "key": key, "msg": msg}
    if client is not None:
        out["client"] = client
    _send(out)


def _web_handler(msg):
    fn = _web_fns.get(msg.get("view")) or _web_fns.get(None)
    if fn is None:
        raise LookupError(f"web view {msg.get('view')!r}: nothing handles its messages; add @byob.on_web({msg.get('view')!r})")
    params = msg.get("msg") or {}
    return fn(params.get("msg", params) if isinstance(params, dict) else params, msg)


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
            _context.clear()
            _context.update({k: params.get(k) for k in ("name", "workspace", "chat")})
            try:
                result = fn(**args) if isinstance(args, dict) else fn(args)
            finally:
                _context.clear()
        elif method == "hook":
            fn = _hooks.get(params.get("event"))
            result = fn(params) if fn is not None else None
            if result is None:
                result = {"action": "continue"}
        elif method == "web":
            result = _web_handler(params)
        elif method == "view":
            vid, key = params.get("view"), params.get("key") or ""
            if vid in _view_fns:
                _remember_key(vid, key, params)
                result = _draw(vid, key)
            elif _views:
                result = _views[0](params.get("site"), key, params.get("client"))
            else:
                raise LookupError(f"this backend draws no view {vid!r}: add @byob.view({vid!r}) to a function that returns it")
        else:
            raise LookupError(f"unknown request {method!r}")
        _send({"type": "reply", "id": rid, "result": result})
    except ToolError as e:
        _send({"type": "reply", "id": rid, "error": str(e)})
    except Exception:
        traceback.print_exc()
        _send({"type": "reply", "id": rid, "error": _short_traceback()})
    if method == "tool":
        refresh()


def _view_event(msg):
    vid = msg.get("view")
    _remember_key(vid, msg.get("key") or "", msg)
    handlers = _event_fns.get(vid, []) + _event_fns.get(None, [])
    if not handlers:
        fault(f"view {vid}: nothing handles its events; add @byob.on_event({vid!r})")
    values = None
    for h in handlers:
        try:
            r = h(msg)
            if isinstance(r, dict) and isinstance(r.get("values"), dict):
                values = r["values"]
        except Exception:
            traceback.print_exc()
            fault(f"view {vid}: the event handler failed on {msg.get('action')!r}:\n" + _short_traceback())
    refresh(values=values, client=msg.get("client"))


def run(handlers=None):
    """Serve byob until it closes stdin. `handlers` may also map names to functions:
    {"tool:add": add, "message": fn, "start": fn, "view": fn, "turn_finished": fn}."""
    # the protocol is UTF-8 whatever the locale says (Windows: cp1252)
    for stream, errors in ((sys.stdin, "replace"), (_out, "strict"), (sys.stderr, "backslashreplace")):
        try:
            stream.reconfigure(encoding="utf-8", errors=errors)
        except (AttributeError, ValueError, OSError):
            pass
    sys.stdout = sys.stderr  # stray print()s must not break the protocol
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
                refresh()
            elif msg.get("event") == "view":
                _view_event(msg)
            elif msg.get("event") == "web":
                try:
                    _web_handler(msg)
                except Exception:
                    traceback.print_exc()
                    fault(_short_traceback())
                refresh()
            else:
                _call(_events.get(msg.get("event"), []), msg)
        elif kind == "request":
            _request(msg)
        elif kind == "shutdown":
            break
        # anything else is from a newer byob: ignored
