---
name: plugin-authoring
description: How to extend byob with plugins - Python tools (make_tool), backends, views (a pane, a header chip, a settings section, step cards), skills, MCP servers, subagents, settings and API keys, and keeping a plugin for later chats.
---
# Writing byob plugins

A plugin is a folder with a `byob-plugin.json` manifest plus files. It can add tools (Python
scripts or MCP servers), UI (views: a side pane, a header chip, a settings section, cards on
steps), skills, subagents and one system-prompt line. Plugins live in three
places; the same id resolves chat > workspace > global:

- this chat's scratch plugins: what you make with `make_tool` (no approval needed);
- the workspace: `.byob/plugins/<id>/` in the workspace folder (shared with the repo);
- everywhere: the user's global plugins folder.

New tools are callable from your **next step** in this same reply. Prompt lines and skills of
a plugin you change apply from the next message.

## 1. A Python tool: use make_tool

```
make_tool(
  name="weather",
  description="Current weather for a city",
  code='''
import json, urllib.parse, urllib.request

def run(city: str, units: str = "metric") -> dict:
    """Current weather.

    Args:
        city: City name, e.g. Paris
        units: metric or imperial
    """
    url = "https://wttr.in/" + urllib.parse.quote(city) + "?format=j1"
    with urllib.request.urlopen(url, timeout=20) as r:
        data = json.load(r)
    return {"city": city, "temp_c": data["current_condition"][0]["temp_C"]}
''',
  try_args={"city": "Paris"},
  network=True,
)
```

Rules:
- Define `def run(arg: type = default, ...)`. The parameters' type hints (`str`, `int`,
  `float`, `bool`, `list[str]`, `dict`, `Literal["a", "b"]`, `Optional[x]`), defaults and the
  docstring's `Args:` lines become the tool's JSON schema. Parameters without a default are
  required.
- Return a string, or anything JSON-serialisable (dict, list, number). Print nothing else to
  stdout. Raise an exception to report an error (the model sees the traceback).
- Standard library only, unless the workspace environment has the package installed. Files
  you write go to the workspace folder (the current directory). The plugin folder is
  read-only while the tool runs.
- `network=True` if the code uses the internet; otherwise it runs offline.
- Always pass `try_args`: you get the output or the end of the traceback. Fix the code and call
  `make_tool` again with the same name to replace the tool.
- API keys and other secrets: list them in `settings=["WEATHER_API_KEY"]` and read them with
  `os.environ["WEATHER_API_KEY"]`. Never put a secret in the code or ask the user to paste it
  into the chat: the user fills it in on the plugin's card. `plugin_status` says what's missing.
- Several tools can share one plugin: pass the same `plugin="my-plugin"` (lowercase letters,
  digits, dashes). Without it, tools go into this chat's plugin `scratch`.

## 2. Editing files

- `edit_plugin(plugin, file, find, replace)`: replace one exact snippet in a plugin file.
- `write_plugin_file(plugin, path, content)`: create or overwrite any file in a plugin.
Both reload the plugin and report the loader's problems verbatim. A tool script is
`tools/<name>.py`; keep the `# --- byob runner` footer at its end.
A plugin that doesn't load (a broken manifest), in any scope, is still reachable by its
folder name: `plugin_status(plugin)` says why and lists its files, `plugin_status(plugin,
file)` reads one, and these two fix it in place. Don't make a new plugin beside it.

## 3. The manifest (byob-plugin.json)

```json
{
  "id": "weather",
  "name": "Weather",
  "version": "0.1.0",
  "description": "Weather lookups",
  "prompt": "Use the weather tool for questions about current weather.",
  "permissions": {"network": true},
  "settings": [
    {"key": "WEATHER_API_KEY", "label": "API key", "secret": true, "required": true}
  ],
  "tools": [
    {"name": "weather", "description": "Current weather for a city",
     "params": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]},
     "run": "tools/weather.py"}
  ],
  "mcpServers": {
    "files": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]},
    "remote": {"url": "https://example.com/mcp", "headers": {"Authorization": "Bearer ${API_TOKEN}"}}
  }
}
```

- `id`: lowercase letters, digits, dashes. Tool names: letters, digits, `_`.
- A script tool gets its arguments as JSON on stdin and prints its result; settings arrive as
  environment variables (also `BYOB_PLUGIN_DIR`, `BYOB_CHAT_DIR`).
- MCP servers: `command`/`args`/`env` for a local program (stdio), or `url` (+ `headers`) for a
  remote one. `${KEY}` is replaced by the setting `KEY`. Their tools appear as
  `<plugin>__<tool>` a moment after the server starts.
- Python packages: `"python": {"requirements": ["requests>=2.31"]}`. byob installs them into
  the plugin's own environment the first time it runs; if that fails, `plugin_status` shows
  pip's output as a problem.

## 4. A backend: Python that keeps running

A script tool starts fresh for every call. A **backend** is one Python program that byob
starts when the plugin is first used and keeps running: it keeps its state in variables, it
can serve tools (fast), answer the plugin's UI, and hear about turns. Write it with byob's
helper module `byob_plugin` (always importable):

`byob-plugin.json`:
```json
{
  "id": "counter",
  "name": "Counter",
  "backend": "backend.py",
  "tools": [
    {"name": "bump", "description": "Add to the counter and say the total",
     "params": {"type": "object", "properties": {"by": {"type": "integer"}}},
     "run": "backend"}
  ]
}
```

`backend.py`:
```python
import byob_plugin as byob

total = 0

@byob.tool
def bump(by: int = 1):
    global total
    total += by
    return f"total is {total}"

@byob.on_message            # a message from the plugin's UI
def ui(msg, client):
    byob.send({"total": total}, client=client)

@byob.on("turn_finished")   # also: chat_opened, turn_started, step_done, artifact_updated
def turn_done(event):
    byob.log("turn", event["entry"], "finished in chat", event["chat"])

byob.run()
```

- `"run": "backend"` tools call the function of the same name with the model's arguments
  as keyword arguments; return text (or `{"text": ..., "files": [...]}`).
- Never write to stdout yourself: it carries byob's messages. `print()` goes to the
  backend's log once `byob.run()` runs. `byob.log(...)` adds a line; `byob.fault(text)`
  reports a problem; `byob.notice(text)` shows a short notice to the user.
- `byob.settings` has the plugin's settings (secret ones only as environment variables).
- If it crashes, the tool call fails with its last output and byob starts it again;
  `plugin_status(plugin)` shows `backend: crashed`, the error and the last lines of its
  output. Fix the file with `edit_plugin`: the backend restarts with the new code.

## 5. Views: a pane, a header chip, a settings section, cards

A backend can show UI. Declare each view in the manifest with a `site` and an `id`, and draw
it in `backend.py` with a function that returns a **view tree** (a dict; the node types are
below). byob draws it in the user's theme and sends you what the user clicks.

Sites: `"pane"` (a tab beside the chat; the user opens it from its title in the header),
`"status"` (a small item in the header: use one `chip` or `text`), `"settings"` (a section
under the plugin in Settings → Plugins), `"card"` (on the step of one of its tools).

A to-do pane and a chip with the count. Write both files with `write_plugin_file`:

`byob-plugin.json`:
```json
{
  "id": "todo",
  "name": "To-do",
  "backend": "backend.py",
  "views": [
    {"site": "pane", "id": "list", "title": "To-do"},
    {"site": "status", "id": "count"}
  ]
}
```

`backend.py`:
```python
import byob_plugin as byob

items = []

@byob.view("list")                 # draws the view with id "list"
def todo_list():
    rows = []
    for i, text in enumerate(items):
        rows.append({"type": "row", "children": [
            {"type": "text", "text": text},
            {"type": "button", "label": "Done", "action": f"done:{i}"}]})
    return {"type": "column", "children": [
        {"type": "row", "children": [
            {"type": "input", "name": "new", "placeholder": "New item", "submit": "add"},
            {"type": "button", "label": "Add", "action": "add", "primary": True}]},
        *rows]}

@byob.on_event("list")             # a click or Enter in that view
def list_event(event):
    action = event["action"]
    if action == "add":
        text = event["values"].get("new", "").strip()
        if text:
            items.append(text)
        return {"values": {"new": ""}}   # clear the input
    if action.startswith("done:"):
        items.pop(int(action[5:]))

@byob.view("count")                # the chip in the header
def count():
    return {"type": "chip", "text": f"{len(items)} to do", "tone": "accent"}

byob.run()
```

How it works:
- `@byob.view("list")` draws view `list`. After every event (and every tool call) byob calls
  all your view functions again and updates what changed, so just change your variables in
  `@byob.on_event` and return nothing. Return `{"values": {"new": ""}}` to clear a field.
- An event is `{"action": ..., "values": {...}, "key": ...}`: `action` is the button's
  `action` (or an input's `submit`), `values` has the text of every `input` (by `name`),
  every `toggle` (true/false) and `select`. Put an index in the action (`"done:3"`) to tell
  rows apart.
- Nodes: `column`/`row`/`card` (with `children`; card has a `title`), `text` (`text`,
  `style`: body/heading/strong/small/mono, `tone`), `markdown`, `chip` (`text`, `tone`),
  `button` (`label`, `action`, `primary`/`danger`), `input` (`name`, `placeholder`,
  `submit`), `toggle` (`name`, `label`, `value`, `action`), `select` (`name`, `options`,
  `value`, `action`), `progress` (`value` 0..1), `table`, `chart`, `image`, `separator`.
  `tone`: normal, dim, accent, success, warning, danger. Every node needs its `"type"`.
- A card on a tool's step: declare `{"site": "card", "id": "result"}`, draw it with
  `@byob.view("result")` taking the card's key (`def result(key):`), and end the tool with
  `return byob.card("text for you", "result", key="some-id")`. Clicks on it are events of
  view `result` with that `key`.
- Give view functions, event handlers and tools different Python names: a later `def`
  with the same name replaces the earlier one.
- Mistakes in a tree don't break anything: byob draws what it can and `plugin_status`
  lists what it had to fix (`backend fault: view list: …`) and any traceback. Fix the file
  with `edit_plugin`; the backend restarts with the new code and the views redraw.

## 6. Skills and subagents

- Skill: `skills/<name>/SKILL.md` with frontmatter `name:` and `description:` (when to use it),
  then step-by-step instructions. Listed in the system prompt; loaded with `use_skill`.
- Subagent: `agents/<name>.md` with frontmatter `name:`, `description:`, optional `tools:`
  (comma-separated) and `model:`, then its system prompt. Used with `delegate`.
- Slash command: `commands/<name>.md` with optional frontmatter `description:` and
  `argument-hint:`, then a prompt template. The user types `/<name> args`; you get the
  template with `$ARGUMENTS` (all arguments) and `$1`, `$2`… filled in. Users also type
  `/<skill> task` to ask you to use a skill.

## 7. Status, keeping and removing

- `plugin_status(plugin?)`: scope, on/off, tools, views, skills, missing settings, problems,
  last error, the backend's state, faults and last output.
- `keep_plugin(plugin, scope="workspace"|"global", new_id?)`: move a chat plugin so later chats
  have it. The user is asked first; only do it when the user wants to keep it. Give a
  descriptive `new_id` when keeping the `scratch` plugin.
- `remove_plugin(plugin)`: delete it (asks unless it's this chat's scratch plugin).
