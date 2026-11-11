# byob extensibility demos

Hands-on tours of what the `exp/extensibility` branch added: view trees at render sites, long-lived
plugin backends, bundled plugins (fork to edit), turn hooks, environment providers, web
views and MCP Apps, spreadsheets.

```sh
demos/run.sh <demo>             # desktop window
demos/run.sh <demo> --serve     # desktop window + browser UI at http://127.0.0.1:7470
demos/run.sh <demo> --browser   # browser UI only (byob --headless); open http://127.0.0.1:7470
demos/run.sh <demo> --keep      # keep the last run's chats/plugins (default: start fresh)
demos/screenshots.sh [shot...]  # regenerate demos/screenshots/*.png headlessly (Xvfb)
```

- **Throwaway profiles.** Every run uses `BYOB_HOME=/tmp/byob-demo/<demo>/home` and the workspace
  `/tmp/byob-demo/<demo>/workspace`, wiped at the start unless `--keep`. Your
  `~/.config/byob` and `~/.local/share/byob` are never read or written (the window state goes
  to `$BYOB_HOME/window.ron` too).
- **Model.** `BYOB_DEMO_URL` (default `http://silicon:9931/v1`) and `BYOB_DEMO_MODEL` (default
  `local`); `BYOB_API_KEY` for hosted APIs. run.sh prints the suggested prompts for each demo
  and types the first one into the composer (press Enter to send it; the window does nothing
  until you do).
- **Builds.** run.sh builds `target/release/byob` (a no-op when it's current; enters
  `nix develop` if `cargo` isn't on PATH). For `--serve`/`--browser` it first runs
  `cargo xtask web` if the browser UI hasn't been built (it's compiled into the binary).
- **HTML on the desktop** (MCP App cards, plugin web views, HTML apps) needs the Servo helper:
  `BYOB_ARTIFACT_HOST`, else `host/artifact-host/target/release/byob-artifact-host`, else
  `/tmp/byob-helper-bridge/byob-artifact-host`. Without it they show "Open in browser"; the
  browser UI draws them in iframes either way.

| demo | shows | model needed |
|---|---|---|
| [todo](#todo) | a plugin's pane, header chip, settings section, step cards; two clients in sync | for the cards |
| [self-plugin](#self-plugin) | byob writing its own plugins (pane, chip, hook), then fixing them | yes |
| [hooks](#hooks) | `before_tool_call` (forced ask / deny), `before_prompt`, `after_tool_call` cards | yes |
| [env-provider](#env-provider) | a third-party workspace environment + the consent card | yes |
| [mcp-app](#mcp-app) | an MCP App card (Refresh/Ask) + a plugin HTML pane | yes |
| [sheets](#sheets) | the spreadsheet plugin: native backend, editable grid | yes |
| [fork](#fork) | fork a built-in plugin (html-apps), change its guidance, see the effect | yes |

Plugins used here: `examples/plugins/todo-demo` and `demos/plugins/*` (guard, toolbox-env,
weather, notes). They're loaded with `--plugin`, so they show as "yours" in Settings → Plugins.

---

## todo

**Shows:** one Python backend (`examples/plugins/todo-demo/backend.py`) drawing four view sites:
a **pane** (To-do), a **header chip** ("2 to do"), a **settings section** and a **card** on every
`todo_add` step. The engine owns the state and broadcasts views, so every client stays in sync.

```sh
demos/run.sh todo --serve     # then also open http://127.0.0.1:7470
```

1. Click **To-do** in the header: the pane opens. Type an item, press Enter. The chip counts it.
2. Send: `Add three things to my to-do list: buy figs, call the dentist, file the taxes.`
   Each `todo_add` step gets a live card with **Mark done**; clicking it ticks the item in the
   pane and the chip changes.
3. **Settings → Plugins → To-do (demo)**: the plugin's own settings section (Hide finished,
   Clear finished) drawn as a view tree.
4. With the browser open beside the window: tick an item in one, watch the other follow.
   (Typing in an input stays local to each client until you submit.)

Look for: no HTML anywhere; everything is native egui (desktop) and the same tree in wasm
(browser). Screenshots: `todo.png`, `todo-settings.png`.

## self-plugin

**Shows:** an empty profile; byob writes plugins for itself with `make_tool`,
`write_plugin_file`, `edit_plugin`, and checks them with `plugin_status`.

```sh
demos/run.sh self-plugin
```

Try, one at a time (approve the plugin writes when asked):
- `Make me a plugin with a counter pane: a big number with + and - buttons and a Reset button.`
  → a new pane title appears in the header; click it and use the buttons.
- `Add a status chip to the header that shows the current time (HH:MM).`
  (The authoring guide tells it to tick with a thread + `byob.refresh`.)
- `Make a hook plugin that blocks every delete_file call with the message 'deleting is off'.`
  then, in a **new message**: `Delete some-file.txt` → "Not run: deleting is off".
- Break it on purpose: `Change the counter's + button so it raises an exception.` Click +.
  Then: `Check plugin_status for the counter plugin and fix it.` byob reads the fault/traceback
  and repairs the file; the pane redraws.
- `Keep the counter plugin globally.` (`keep_plugin`; you're asked) → it's in Settings → Plugins
  under Everywhere, and in every new chat.

Look for: plugins start in the chat's scope (Settings → Plugins → This chat), hook changes apply
from the next message. Screenshot: `self-plugin.png`.

## hooks

**Shows:** `demos/plugins/guard`, one Python backend sitting in the turn pipeline, run with
`--auto-approve` to prove the point:
- `before_tool_call` on `write_file`: **ask** → an approval card even though auto-approve is on;
  on `delete_file`: **deny** → "Not run: Deleting files is turned off…".
- `before_prompt`: adds "Files in this workspace are guarded…" to the system prompt.
- `after_tool_call` on file/command tools: a **Guard** card (checked/failed chip) on the step.
- a header chip counting its decisions.

```sh
demos/run.sh hooks
```

1. Send: `Read notes.txt, then write a copy of it to backup.txt, then delete notes.txt.`
2. The `read_file` step has a Guard card. The write stops on an approval card ("The guard
   plugin wants a person to confirm…"); Allow it.
3. The delete is refused; the model tells you so. The chip reads e.g. "guard: 1 asked · 1 denied ·
   2 checked".
4. Ask: `What does your system prompt say about files here?` → it quotes the guard line.
5. Settings → Plugins → Guard: switch it off, ask again: writes go through (auto-approve).

Screenshot: `hooks.png`.

## env-provider

**Shows:** `demos/plugins/toolbox-env` declares an `environments` provider: when the workspace
has a `tools.txt`, its `env.py` makes `.toolbox/bin/<tool>` scripts and prints the environment
(`PATH`, `TOOLBOX_TOOLS`) as JSON. byob asks once per workspace before loading it, then every
`run_command` runs with it. The workspace is seeded with a `tools.txt` (greet, deploy-check,
weather-cli).

```sh
demos/run.sh env-provider
```

1. Send: `Run the greet and deploy-check commands and tell me what they print.`
2. Approve `run_command`, then the **environment card** ("Load this workspace's toolbox (tools.txt)", Allow /
   Deny only). Allow → output `[toolbox] hello from a tool …`.
3. Deny instead (fresh run) → `greet: not found`, and the model is told not to load it itself.
4. The workspace's **… menu → Workspace settings → Environment state** shows the answer, with
   Load it / Don't load it / Ask again.
5. Edit `tools.txt` (add `ping: pong`) → the cache key changes; the next command reloads it.

With `--auto-approve` the environment loads for that run only (never saved); with the
"Allow everything" approvals setting it loads without the card too. Screenshots:
`env-provider.png` (auto-approved), `env-provider-consent.png` (the cards).

## mcp-app

**Shows:** two kinds of HTML surface through the web bridge:
- `demos/plugins/weather`: a stdio **MCP server** whose `show_weather` tool has a UI
  (`_meta.ui.resourceUri`). byob draws it as a live card on the step (MCP Apps protocol:
  `ui/initialize`, theme variables, `tools/call`, `ui/message`, model context).
- `demos/plugins/notes`: a plugin **pane that is an HTML page** (`"html": "ui/index.html"`)
  talking to its Python backend through `window.byob` (`byob.tool`, `byob.ask`, `byob.send`,
  `byob.context`), plus a web card on `show_notes`.

```sh
demos/run.sh mcp-app          # desktop needs the Servo helper; or use --serve / --browser
```

1. Send: `Show me the weather in Lisbon.` The step gets the weather card: temperature, the
   bridge timings, theme variables taken from byob's theme.
2. Click **Refresh (app-only tool)**: the card calls the server's `refresh` tool (hidden from the
   model) and the "via tools/call N" counter goes up. Click **Ask byob about it**: a user message
   ("What should I wear in Lisbon?") is posted into the chat.
3. Click **Notes** in the header: the HTML pane shows the notes, `2 + 3` computed by the backend,
   and its round-trip times. Add a note; then send `Show my notes`: a web card lists it.
4. Switch the theme (Settings → Appearance): both pages restyle from `--byob-*` variables.

Screenshot: `mcp-app.png`.

## sheets

**Shows:** spreadsheets as a bundled plugin, `sheets`, whose backend is Rust inside byob
(IronCalc, in-process: nothing to install). Its tools write formulas and formats; its `grid`
view is an editable spreadsheet drawn natively. The workspace is seeded with `sales.csv`
(24 rows: month, region, units, revenue).

```sh
demos/run.sh sheets
```

1. Send: `Open sales.csv as a spreadsheet and add a column with the month-over-month growth % of revenue for each region.`
   → the CSV opens as a spreadsheet beside the chat; formulas, a `0%` format. Edit a cell in
   the grid; scroll; the original `sales.csv` is untouched.
2. Send: `Build a monthly budget spreadsheet with categories, amounts and a summary by category.`
   (also the empty chat's **Make a budget** starter) → an `.xlsx` artifact with formulas.
3. **Settings → Plugins → Spreadsheets**: it declares `"provides": "spreadsheets"`, so a
   plugin of yours that provides the same can replace it ("Use this one").

Screenshot: `sheets.png`.

## fork

**Shows:** byob's own features are plugins. Fork the bundled `html-apps` (its `guidance.md` is
the prompt's rules for HTML apps), change the guidance, and the next app follows it.

```sh
demos/run.sh fork             # HTML apps render inline with the Servo helper (or --serve)
```

1. **Settings → Plugins → HTML apps → Fork to edit** (or ask byob to `keep_plugin` it). It's
   now under Everywhere (`$BYOB_HOME/plugins/html-apps`), and the built-in one is hidden behind it.
2. Send: `Change your html-apps plugin's guidance.md so every app always has a dark/light theme toggle button.`
   (byob uses `edit_plugin` on the fork.) Or skip step 1 and ask it to fork too: `keep_plugin`
   always asks you first, even with `--auto-approve`.
3. In a new message: `Make me a tip calculator app.` → the app has a theme toggle. Compare with
   a fresh `demos/run.sh fork` without the fork.
4. Remove the fork (Settings → Plugins → Remove…) → the built-in comes back.

Screenshots: `fork-settings.png`, `fork.png`.

---

## Screenshots

`demos/screenshots.sh` runs each shot through `scripts/headless.sh` (Xvfb, debug build, no
window on your screen) with the same throwaway profiles, sending the prompts above where the
model is needed (one shot at a time; ~1–4 min each on silicon). Shots: `todo todo-settings
self-plugin hooks env-provider env-provider-consent mcp-app sheets fork-settings fork`. Model output varies run to run; the logs are in `/tmp/byob-demo/<demo>/`.
