<!-- Generated from docs/environments.md by `cargo xtask help-docs`. Edit docs/environments.md, not this copy. -->
# Workspace environments: getting tools on every OS

Last updated: 2026-11-10 (workspace environments). Code: `crates/exec/src/env/`. Overview: `docs/architecture.md`
("Workspace environments").

## Goal

A model that hits `jq: not found` or `ModuleNotFoundError` should get the tool in **one
obvious call** and carry on. It shouldn't flail with `pip install`, `apt` or `sudo`. Small
models follow tools and concrete next steps much better than prose, so the design is:

- **One tool, `install_packages { packages: [...] }`.** It handles programs and Python
  libraries, keeps them **per workspace** and needs no root. They're on `PATH` (and in
  `run_python`'s `python3`) for every later run.
- **Project environments the user agreed to.** If the workspace says how it wants to be
  built (`flake.nix`, `shell.nix`, `pixi.toml`, `mise.toml`, `.venv`,
  `.devcontainer/devcontainer.json`), byob asks once, then every command runs inside that
  environment without the model having to know about it. The providers are a registry
  (`EnvProvider`): byob's own come with the bundled `environments` plugin, and any plugin can
  add one in its manifest (`environments`: a file to detect and a command printing the
  environment as JSON; see `docs/architecture.md`).
- **Hints on failure.** A failed run that's missing a dependency ends with a single line
  naming the exact call, e.g. `Call install_packages with {"packages": ["imagemagick"]}`.
- A two-sentence system prompt paragraph, and no long prose.

## Resolution order

For each `run_command` / `run_python` call:

1. **Where does it run?** The workspace's **environment** setting decides (New workspace
   form, or its settings): **Container** runs every command in the workspace's container,
   **This machine** and **Nix** never do, and **Auto** (the default) asks once when the
   folder has a devcontainer.json. A chat can override it with This machine or Container.
   Otherwise here, in byob's sandbox. In a container, steps 2–4 don't apply: the container
   is the environment, and `install_packages` installs into it.
2. **The workspace's own environment** (`flake.nix` / `shell.nix`, `pixi.toml`, mise,
   `.venv`), once the user agreed to load it. The `.venv` needs no question (nothing runs to
   load it).
3. **Packages from `install_packages`**, per workspace in byob's data folder: **Nix** where
   it's installed, else **pixi** (conda-forge). With neither: `install_python_packages` (uv)
   for Python libraries, and failed runs tell the user how to get pixi.
4. **Last resort, Homebrew (macOS, Linuxbrew) or winget (Windows)**: the model may call
   `install_system_packages` only for names `install_packages` reported missing (or when
   there's no workspace installer at all), and **a person must approve every call** — no
   "always allow", no `--auto-approve`. The card warns that it installs system-wide, outside
   the workspace and the sandbox, and that byob can't undo it.

## Asking once per workspace

Loading a project environment can take minutes, downloads with network access, runs the
project's setup code (`shellHook`, mise plugins) and may write `flake.lock`, `pixi.lock` or
`.pixi/` into the workspace. A dev container creates a long-lived container and pulls or
builds an image. So before the first load byob asks, with an approval card that offers only
**Allow** and **Deny**:

- A person's answer is saved per workspace path in `<data>/envs/workspaces.json`
  (`{"workspaces": {"/path": {"project": true, "container": false}}}`) and reused after
  restarts.
- Declining runs commands without it. The model is told once per session ("the user chose
  not to load it… don't load it yourself"), so it doesn't try `nix develop` instead.
- **A chosen environment is the answer.** A workspace set to Container (or Nix) never gets
  the question: the person chose it when making the workspace or in its settings, so byob
  records it as the folder's answer too. Nix loads the folder's dev shell without asking.
- **Workspace settings → Environment state** (the workspace's `…` menu in the chat list,
  desktop and browser) shows the folder's project files, the remembered answers, the
  container and its state, with **Load it / Don't load it / Ask again** and **Stop / Remove**
  for the container. Where commands run is the Environment setting above it.
- `BYOB_WORKSPACE_ENV=off` never loads project environments or asks about devcontainers;
  `on` loads them (and uses a devcontainer) without asking; unset or `ask` asks.
- **`--auto-approve` and headless runs**: the permission policy answers the question, so
  `--auto-approve` loads the environment (and uses the devcontainer) — but only for that
  run: automatic answers are never saved, so a test run can't decide for the user. Without
  `--auto-approve`, a headless (browser) run shows the card in the browser like the desktop.
  A call with nobody to ask (no asker) counts as "no" and isn't saved.

How it's built: tools can ask the user mid-call (`ToolContext::ask(&Question)`); the turn
adds a step with `Step::ask` set and the engine gates it like any call, except that grants
never answer it. Tools whose `always_ask` returns a warning (`install_system_packages`) are
gated the same way before they run, and the policy can't allow them either.

## Containers (Docker / Podman)

Most people who want reproducible tooling reach for dev containers; Nix is one of the few
exceptions. So byob can run a workspace's `run_command` / `run_python` in a container.

- **Opt-in per workspace.** Choose **Container** when making a workspace (**+ Workspace**
  in the chat list: name, a fresh or existing folder, where commands run; the form checks
  that Docker or Podman answers) or later in its settings; the first command then runs in
  the container with no question. A workspace left on **Auto** with
  `.devcontainer/devcontainer.json` (or `.devcontainer.json`) gets the ask-once question.
  The image: the workspace's own (the Image field), else its devcontainer.json, else
  `python:3-slim` (Debian, apt, Python 3) or `containers.image` in settings.json. A chat in
  a container workspace can run its commands on this machine instead (its settings), and
  the other way round.
- **One long-lived container per workspace**, created on the first command, reused across
  commands and byob restarts. It's named `byob-<folder>-<hash of the path>` and labelled
  `byob.managed=1`, `byob.workspace=<path>`, `byob.config=<stamp>`; a changed
  devcontainer.json, Dockerfile or image setting recreates it (the model is told installed
  packages are gone). Find byob's containers with
  `docker ps -a --filter label=byob.managed=1`. The workspace's settings can stop it (it
  starts again on the next command) or remove it; removing the workspace with "also delete
  what byob keeps for it" removes its container and installed packages too.
- **The workspace is mounted at the same path** (not `/workspaces/<name>`), so paths in
  output, scripts and the changed-files report mean the same thing inside and out.
  `workspaceFolder` / `workspaceMount` are ignored for that reason.
- **Commands run as the host user** (`docker exec -u uid:gid`; Podman also gets
  `--userns=keep-id`), so files they create aren't owned by root. `HOME` is
  `/tmp/byob-home` in the container. `install_packages` runs as root: `apt-get` / `apk` /
  `dnf` for programs, `pip` for Python libraries (and for names the distribution doesn't
  have), installing `python3` and pip first if needed. The hints stay the same: "call
  install_packages".
- **Network per call.** The container is created on its network and disconnected right
  after; a call with `network: true` (and every install) connects it for the duration,
  shared between overlapping calls. Where disconnecting fails (some rootless Podman
  setups), the model is told every command has internet access.
- **Stop and timeouts reach inside.** Killing the `docker exec` client leaves the process
  running in the container, so each run records its process group (the exec'd process leads
  one) in its temp folder, and byob kills that group inside the container.
- **devcontainer.json support**: `image`, `build.dockerfile` (+ `context`, `args`),
  `containerEnv`, `remoteEnv`, `onCreateCommand` and `postCreateCommand` (string, array or
  object; run once as root at creation, with network). JSONC comments and trailing commas
  are fine. Ignored, and named to the model: `features`, `mounts`, `runArgs`,
  `forwardPorts`, `remoteUser`/`containerUser`, the other lifecycle commands, and anything
  else. `dockerComposeFile` without an image is refused with a clear message.
- **The daemon being absent or down** gives an error naming the problem and the fix
  ("Start it, or turn off Run commands in a container…"), and nothing runs — byob doesn't
  silently fall back to the host when the user asked for the container.
- **Podman is preferred when installed**, else Docker. Rootless Podman runs containers as
  the user, so a container escape is only the user; membership in the `docker` group is
  root-equivalent on the host. `containers.runtime` in settings.json or
  `BYOB_CONTAINER_RUNTIME=docker|podman` choose explicitly. Only Docker is tested; Podman
  uses the same commands (plus `--userns=keep-id` and the `podman` network).
- **The tradeoff:** byob's own sandbox (Landlock, Seatbelt) doesn't apply inside the
  container — the container is the isolation. It sees the workspace and byob's per-run temp
  folder (`<data>/envs/<ws>/ctmp`), nothing else of the host, but it isn't Landlock-narrow:
  the model can do anything the image allows inside it, including installing packages as
  root via `install_packages`.
- Not on Windows yet (the workspace can't be mounted at the same path in Linux containers).

## What is pixi?

[pixi](https://pixi.sh) (prefix.dev, BSD-3) is a single static binary that installs
packages from **conda-forge** into a project folder, without root, on Linux, macOS and
Windows. conda-forge has most of what models reach for — jq, ripgrep, ffmpeg, imagemagick,
pandoc, graphviz, nodejs, poppler, tesseract — and the whole scientific Python stack under
its pip names, with its own Python. It's the closest thing to Nix that works across OSes,
which is why byob uses it as the installer where Nix isn't installed. A workspace that has
its own `pixi.toml` is loaded like a flake (after asking).

