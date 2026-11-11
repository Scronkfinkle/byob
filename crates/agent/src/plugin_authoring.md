---
name: plugin-authoring
description: How to extend byob with plugins - Python tools (make_tool), skills, MCP servers, subagents, settings and API keys, and keeping a plugin for later chats.
---
# Writing byob plugins

A plugin is a folder with a `byob-plugin.json` manifest plus files. It can add tools (Python
scripts or MCP servers), skills, subagents and one system-prompt line. Plugins live in three
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
    byob.send_view({"type": "text", "text": f"Total: {total}"})  # shown in the plugin's pane
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

## 5. Skills and subagents

- Skill: `skills/<name>/SKILL.md` with frontmatter `name:` and `description:` (when to use it),
  then step-by-step instructions. Listed in the system prompt; loaded with `use_skill`.
- Subagent: `agents/<name>.md` with frontmatter `name:`, `description:`, optional `tools:`
  (comma-separated) and `model:`, then its system prompt. Used with `delegate`.
- Slash command: `commands/<name>.md` with optional frontmatter `description:` and
  `argument-hint:`, then a prompt template. The user types `/<name> args`; you get the
  template with `$ARGUMENTS` (all arguments) and `$1`, `$2`… filled in. Users also type
  `/<skill> task` to ask you to use a skill.

## 6. Status, keeping and removing

- `plugin_status(plugin?)`: scope, on/off, tools, skills, missing settings, problems, last error,
  the backend's state, faults and last output.
- `keep_plugin(plugin, scope="workspace"|"global", new_id?)`: move a chat plugin so later chats
  have it. The user is asked first; only do it when the user wants to keep it. Give a
  descriptive `new_id` when keeping the `scratch` plugin.
- `remove_plugin(plugin)`: delete it (asks unless it's this chat's scratch plugin).
