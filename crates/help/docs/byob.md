<!-- Generated from README.md by `cargo xtask help-docs`. Edit README.md, not this copy. -->
# byob

## What it does

- **A real desktop app.** One folder, about a 60 MB download with everything in it (13 MB for
  the slim build), and native file dialogs, drag and drop and keyboard shortcuts. The browser UI is the reverse of the usual arrangement: the same native interface
  compiled to WebAssembly, so a tab or your phone gets the identical app, live on the same session.
- **Your model, your machine.** No bundled inference and no account: any OpenAI-compatible URL,
  or Claude over Anthropic's Messages API.
  First run finds local servers on their usual ports; tool calling falls back to prompt-based
  calls when the server has none.
- **Artifacts beside the chat.** Spreadsheets (xlsx/csv), charts, Markdown documents,
  images and HTML apps, each with its version history. Charts drawn from a spreadsheet stay
  linked to it and redraw when it changes, even when its rows and columns move; select cells
  and press Chart selection to make one, and Charts to see a sheet's charts. HTML apps are
  offline unless they need the web: **Internet: on** in the app's pane (or byob itself, when
  it makes one that fetches live data) lets that app use HTTPS, still sandboxed.
- **Math, typeset.** LaTeX in replies and documents (`$…$` inline, `$$…$$` on its own line)
  is drawn like KaTeX, in the desktop app and the browser (the bundled Math plugin).
- **Hands, with a leash.** File tools, `run_command` (PowerShell on Windows) and `run_python` (sandboxed on Linux); approval
  cards with diffs; undo per turn. Edit any message you sent to start over from there. Or
  let it off the leash: YOLO (Settings → Approvals, or `--yolo`) runs everything without
  asking, and the header says so while it's on.
- **Reads, searches and remembers.** PDF, Word and PowerPoint files; images, for models that can see;
  web search and page reading (Exa by default, or your own SearXNG, Brave or Tavily); notes kept
  across chats in plain Markdown; a step plan for long tasks.
- **Workspaces.** A folder with its own settings and where commands run: this machine, a Nix dev
  shell or a Docker/Podman container. Settings layer global → workspace → chat.
- **Everywhere you are.** `byob --serve` opens that session to browsers too; with
  `tailscale serve` it's on your phone.
- **Everything is a plugin.** The prompt, the core tools, artifacts, spreadsheets, subagents and
  compaction are bundled plugins on the same API anyone gets. Plugins add tools, draw UI (panes,
  header chips, settings, cards), hook every stage of a turn, and run long-lived backends. byob
  can write and fix its own plugins from a chat.

## Install

```sh
cargo install byob                          # with the browser UI built in
cargo install byob --no-default-features    # slim: desktop only, a few MB smaller
nix build github:Scronkfinkle/byob          # or .#byob-slim
```

Release archives for Linux, macOS and Windows are on the
[releases page](https://github.com/Scronkfinkle/byob/releases): `byob-…` has everything (the
browser UI and `byob-artifact-host`, which draws HTML apps inline; keep it next to `byob`), and
`byob-slim-…` is byob alone. `cargo install` doesn't build the renderer; without it, HTML apps
open in your browser with one click. On Linux byob needs no GUI toolkit (file dialogs go through
the XDG desktop portal or zenity; without either, drop files onto the window), and the renderer
draws with the system's OpenGL (Mesa).

On macOS the builds aren't signed by an Apple developer yet. Fetched from a terminal, a release
archive never meets Gatekeeper (use `x86_64` instead of `aarch64` on an Intel Mac):

```sh
v=0.3.11; curl -L https://github.com/Scronkfinkle/byob/releases/download/v$v/byob-$v-aarch64-apple-darwin.tar.gz | tar xz
./byob-$v-aarch64-apple-darwin/byob
```

Or take `byob-…-apple-darwin.dmg` and drag `byob.app` into Applications. Downloaded with a
browser, macOS refuses to open it at first: try once, then System Settings → Privacy & Security →
Open Anyway (on macOS 14 and older, right-click → Open), or run
`xattr -dr com.apple.quarantine /Applications/byob.app`. Opened from Finder, byob starts in your
home folder and finds your shell's tools (Homebrew, Nix).

## Use

```sh
byob                                        # desktop window
byob --serve                                # + browser UI on http://127.0.0.1:7470
byob --headless                             # browser UI only, no window
byob --inference-url http://host:8080/v1 --model local
BYOB_API_KEY=sk-... byob                    # for hosted endpoints
BYOB_API_KEY=sk-ant-... byob --inference-url https://api.anthropic.com/v1 --model claude-opus-5-5
byob --workspace ~/projects/taxes           # start in the workspace for that folder

tailscale serve 7470                        # reach it from any device on your tailnet
byob --headless --allow-user you@example.com # only admit that Tailscale login
```

On first run byob shows **Connect a model**: servers it found on this computer's usual ports
(llama-server 8080, Ollama 11434, LM Studio 1234, vLLM 8000), presets for those and for hosted
APIs, and a Test connection button. What you connect becomes your default model. Settings →
**Models** keeps every model you use: add several from one server at once (byob lists what
the server has), give each a description, test, duplicate, pick the default, choose a
vision helper, and a model for titles and summaries (a small, fast one will do). Subagents run
on the chat's model unless you pick another: a Subagent model in Settings → Models (or per
workspace or chat), an agent file's `model:` (a saved model's name works), or, for models you
mark **Offered to subagents**, the main model itself, choosing by their descriptions. A model's
Advanced settings set its context window, temperature, top_p, max tokens, effort (Claude) and
extra JSON for every request, e.g. `{"chat_template_kwargs": {"enable_thinking": false}}`. The
model chip in the header switches the chat in view to another model.

Claude is spoken to over Anthropic's own Messages API, not its OpenAI compatibility layer: pick
the Anthropic preset (or any URL on `api.anthropic.com`), and byob uses prompt caching, shows
Claude's thinking, and keeps it across a tool loop. For an Anthropic-style endpoint elsewhere (a
gateway), set `"api": "anthropic"` under `inference` in `settings.json`.

Chats are grouped by **workspace**: a folder byob works in, with its own settings. **New chat**
(Ctrl/Cmd+N; Ctrl/Cmd+Shift+O in the browser) starts one in the workspace you're in; its arrow, or
the **+** next to Workspaces, makes a workspace: open a folder you have (or drop one on the
window) or start an empty one; where commands run waits under Advanced. Each chat keeps its
unsent draft. **Search** (Ctrl/Cmd+F) filters the list by chat title and workspace name.
Hover a workspace for **+** (a new chat there) and **⋯** (or right-click it); that menu and the
sliders next to a chat's title show what's inherited and what's set there: model (one of your
saved models), approvals, tool calling, compaction, subagents and their model, extra
instructions.

**Settings** (Ctrl/Cmd+, or the bottom of the chat list) is a screen of its own: the chat list
gives way to Back, a search field (any setting by name, what it does, or a word you'd use for
it; Ctrl/Cmd+F) and the categories: Models (saved models and their roles), Chats (what every
chat starts with: instructions, approvals, limits, context), Plugins and, on this device only,
Appearance (light or dark, themes, accent, messages, theme files), Text & layout (zoom, sizes,
fonts, density, accessibility), Colors and, in the desktop app, System (keep running in the
background, start at login). Back or Esc returns to the chat as you left it.

**In the background** (Settings → System): closing the window hides it to the tray and byob
goes on, so routines keep running (Keep running in the background: Always; or only When
routines are on, or Never). Its tray icon opens it again (a click) or quits it (its menu). Without a tray, open byob again: a second launch brings
the running one's window back (one byob runs per data folder). Start byob when you log in
starts it hidden.

**My phone** (Plugins → Connects to → Telegram, or Settings → Plugins): byob can text you, on
Telegram for now (free; a bot of your own: its page walks you through @BotFather's /newbot and
the token). Pair my phone shows a
link and a one-time code to send from the phone. Then a routine's Tell me offers Text me (or say
"text me when it finds one"): its findings arrive as texts, what it asks as buttons (Allow, Deny
or Always allow; a question's choices; for what must always be confirmed, like sending mail, the
text says everything it will do), and a reply to a
text continues that chat (each chat is its own reply thread), with the answer texted back.
`/new` starts a new chat from the phone (`/new` and what it's about, or `/new` alone); `/chats`
lists the latest and `/find` and some words looks for one, and a tap picks it up there. byob
only asks Telegram for the bot's messages (no open port); people who aren't paired get nothing.
At most 10 texts a day per routine; the rest arrive together, later. Or, one way and with no
account, push notifications through the ntfy app (pick a topic name; you answer in byob).

**What's shown** (Settings → Appearance, shared by every window and browser tab): each detail
of the interface (who wrote a message, the thinking and its length, tool steps, Undo, a reply's
tokens, time and speed, artifact badges, code blocks' language and Copy, the paperclip,
suggestions, the context numbers, shortcut hints, the wordmark, the server's address, plugins'
header items) is shown, shown on hover, or hidden, and the header and a reply's details can be
reordered. Everything / Balanced / Minimal set them all at once. Or ask byob: "hide the token
counts and show the speed" or "make it cleaner" changes it in every window (the `ui_layout`
tool of the bundled Appearance plugin). The look itself can be asked for too: "make it warmer,
rounder, with bigger text" changes the theme, accent, corners, text size, bubbles, padding, line
spacing or any color of the window showing the chat (`ui_look`; looks stay per device, like the
settings). "Undo that" puts back the look or layout from before byob's last change. byob can
look at the window too (`ui_screenshot`, with a model that sees images): to check a look it
changed, or to see what you mean by "that thing at the top".

**Backdrop and glass** (experimental; Settings → Appearance → Backdrop and glass, or ask
byob, e.g. "put a rainy city behind the chat"): a **backdrop**, a shader scene drawn behind the panels, which turn to glass
over it (byob writes it in WGSL for the place or mood you describe, checked before it runs and
refused with the line and reason when it doesn't compile); **frosted glass** (the scene blurred
behind the panels, when a backdrop was on as byob started); the **arrangement**: docked as
usual, or free, with the header, chat list, chat, message box and panes placed anywhere, each in
a glass, metal or bare frame and labeled as you like (floating and focus to start from; ⋯ → Arrange
panels… to drag, resize or hide them; Dock everything to go back); and **controls**: standard,
HUD, plate or pill buttons. A new backdrop that byob closes on within a few seconds is switched
off at the next start; `BYOB_NO_BACKDROP=1` starts without any. From a terminal: `--backdrop
FILE.wgsl|off`, `--arrangement NAME`, `--look KEY=VALUE`.

**Fluid editing** (press F2, or ask byob, e.g. "make the send button bigger and put the model
picker in the header"): every element on screen, down to a single button or line of text, can
be moved, scaled, recolored, retitled, faded, hidden, shown only on hover or set gently moving,
and new elements (a clock, a timer, a to-do list, buttons that send a prompt) can be put beside
any of them. In edit mode, point at anything to see what it is, click to select it, drag to move
it (with Alt, drop it on another element's edge to keep it beside that one, or on its middle to
swap the two), drag a corner to scale it,
and change the rest in the inspector; arrows nudge, H hides, R resets, Ctrl+Z undoes, Esc
leaves. Arrangements can be saved as scenes to switch back to. byob changes the same things with
the Appearance plugin's `ui_fluid` tool, and plugins can bring changes of their own (Edit mode's
**Plugins' changes** switch leaves them out until the next start). Changes stay per device, like
the look. **Reset all** in edit mode puts everything back; `BYOB_NO_FLUID=1` starts without any
change drawn, keeping them for later.

## Plugins

Settings → Plugins lists what the chat in view can use. Any bundled plugin can be forked to your
own plugins and edited (**Fork to edit**), or just ask byob to change or write one: it has
authoring tools, and a broken plugin's errors come back to the model so it can fix it.

- [docs/views.md](docs/views.md): view trees, the UI plugins draw (panes, chips, cards, grids).
- [docs/plugins-backend.md](docs/plugins-backend.md): long-lived backends (Python or Rust) and hooks.
- [docs/environments.md](docs/environments.md): where commands run, and adding your own.
- [demos/](demos/README.md): hands-on tours, each in a throwaway profile (`demos/run.sh todo`).
- [examples/plugins/todo-demo](examples/plugins/todo-demo): a small plugin with a backend, a pane and cards to copy from.

