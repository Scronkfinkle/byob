# Workspace environments: getting tools on every OS

Last updated: 2026-11-10. Code: `crates/exec/src/env/`. Overview: `docs/architecture.md`
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
  environment without the model having to know about it.
- **Hints on failure.** A failed run that's missing a dependency ends with a single line
  naming the exact call, e.g. `Call install_packages with {"packages": ["imagemagick"]}`.
- A two-sentence system prompt paragraph, and no long prose.

## Resolution order

For each `run_command` / `run_python` call:

1. **Where does it run?** In the workspace's **container** if the user opted in (the
   question for a workspace with a devcontainer.json, or the Settings checkbox for any
   workspace), otherwise here, in byob's sandbox. In a container, steps 2–4 don't apply:
   the container is the environment, and `install_packages` installs into it.
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
- **Settings → Workspace environment** (desktop and browser) shows the workspace's project
  files, the remembered answers, the container and its state, with **Load it / Don't load it
  / Ask again**, a **Run commands in a container** checkbox, and **Stop / Remove** for the
  container.
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

## Done

| Piece | Linux | macOS | Windows |
|---|---|---|---|
| `install_packages` via Nix (buildEnv per workspace, pinned nixpkgs, GC root) | yes, tested | should work, untested (needs Nix) | — |
| `install_packages` via pixi where there's no Nix (manifest per workspace in byob's data folder) | yes, tested (Nix hidden) | should work, untested | untested (`run_command` needs `sh`) |
| Ask once per workspace before loading a project environment; Settings to change it | yes, tested | yes | yes |
| `flake.nix` / `shell.nix` dev shell, cached, shellHook applied | yes, tested | untested | — |
| `nix shell nixpkgs#x --command …` inside the sandbox, without network | yes, tested | Seatbelt socket rule unit-tested only | — |
| pixi project activation (`pixi shell-hook --json`) | tested (empty env) | untested | untested |
| mise project activation (`mise install`, `mise env --json`) | tested (`[env]` only) | untested | untested |
| `.venv` / `venv` on `PATH` | yes | yes | yes (`Scripts`) |
| Missing-dependency hints (pixi advice where there's no installer) | yes | yes | yes |
| `install_python_packages` (uv) when there's no Nix or pixi | unchanged | unchanged | unchanged |
| `install_system_packages` (Homebrew / winget), always confirmed | fake `brew` tested | should work, untested | untested |
| Docker/Podman container per workspace, devcontainer.json | Docker tested | Docker Desktop: should work, untested | — (paths can't be mounted at the same path) |

### Decisions, and what was rejected

- **Cache `nix print-dev-env` instead of wrapping each command in `nix develop -c`.** Wrapping
  would evaluate the flake on every call (around a second even with a warm eval cache), and
  `run_python` couldn't find the shell's `python3` beforehand. byob builds the environment
  once with `--profile` (that gives a GC root, and the profile *is* the JSON). It runs the
  `shellHook` once in bash, diffs `env -0` to keep the variables the hook changed (that's
  how direnv works too), and caches the result keyed on `flake.nix`/`flake.lock`/`shell.nix`
  contents.
- **Installed packages live in byob's data folder, not in a workspace `flake.nix` or
  `pixi.toml`.** Writing a flake for the model would mean editing user repos and editing Nix
  syntax with a weak model. A flake in a git repo is invisible to Nix until it's `git add`ed,
  and in a non-git folder a `path:` flake copies the whole workspace (data files included)
  into the store. The per-workspace `buildEnv` (or pixi manifest) avoids all of that. A user
  who wants it reproducible can still ask for a flake or a pixi project, and byob then loads
  it (after asking).
- **No `nix profile install` or `nix-env -i`.** They change the user's global profile and
  aren't scoped to a workspace.
- **With Nix, Python libraries come from nixpkgs, not uv/PyPI wheels.** On NixOS, manylinux
  wheels with native code (numpy, matplotlib) don't load without nix-ld, and mixing the two
  breaks. `install_python_packages` is only registered when there's no `install_packages`.
- **Nix before pixi** wherever Nix is installed: conda-forge binaries don't run on NixOS
  without nix-ld, and Nix's store is already there.
- **The client gets no network; the daemon fetches.** byob sets `substitute = true` because
  the client otherwise disables substitution when it sees no network, and it points the
  registry at a nixpkgs pin. Then `nix shell nixpkgs#x` in an ordinary (network-off) run
  works. Loading a project environment and installing do run with network: flake inputs
  are fetched client-side, and pixi/mise download themselves.
- **Package-manager runs have no address-space limit** (`Job::big_memory`): pixi maps more
  than the sandbox's 8 GB `RLIMIT_AS`. They still run in the sandbox, and installed
  environments are readable there (not only their `bin` on `PATH`), so their libraries load.
- **The question is a card, not a setting you must find first.** Alternatives: load
  silently (the old behaviour; surprising network use and lock files in the user's repo), or
  only `BYOB_WORKSPACE_ENV` (not discoverable). Answers are per workspace *path*: moving a
  folder asks again. Rejected: keying on the project file's contents (a `git pull` would ask
  again).
- **Homebrew/winget are never a default.** They're system-wide, mutate `/opt/homebrew` (or
  run installers that may want admin rights), aren't scoped to a workspace and must run
  outside the sandbox. So they're a separate tool, offered only where brew/winget exists,
  that refuses names `install_packages` didn't report missing while there is a workspace
  installer, and refuses in container workspaces. Homebrew's `bin` is put on sandboxed
  runs' `PATH` so what it installed is found. winget installs may only be on `PATH` after a
  restart.
- **No pixi download.** Where neither Nix nor pixi exists, hints tell the user how to install
  pixi (`curl -fsSL https://pixi.sh/install.sh | sh`, `brew install pixi`, `winget install
  prefix-dev.pixi`). A one-click, checksum-verified download into `<data>/bin` behind an
  approval card is a possible later feature.

## Containers (Docker / Podman)

Most people who want reproducible tooling reach for dev containers; Nix is one of the few
exceptions. So byob can run a workspace's `run_command` / `run_python` in a container.

- **Opt-in per workspace.** A workspace with `.devcontainer/devcontainer.json` (or
  `.devcontainer.json`) gets the ask-once question; any workspace can be switched on in
  Settings → Workspace environment. Without a devcontainer.json the image is
  `python:3-slim` (Debian, apt, Python 3), or `containers.image` in settings.json.
- **One long-lived container per workspace**, created on the first command, reused across
  commands and byob restarts. It's named `byob-<folder>-<hash of the path>` and labelled
  `byob.managed=1`, `byob.workspace=<path>`, `byob.config=<stamp>`; a changed
  devcontainer.json, Dockerfile or image setting recreates it (the model is told installed
  packages are gone). Find byob's containers with
  `docker ps -a --filter label=byob.managed=1`. Settings can stop it (it starts again on the
  next command) or remove it. byob has no list of workspaces to remove, so there's no
  automatic removal; that's a later feature if workspaces become first-class.
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

## Plan for the rest

- **Windows.** There is no sandbox on Windows yet (`sandbox/src/other.rs`), and
  `run_command` uses `sh -c`, which Windows doesn't have (it needs PowerShell/`cmd`
  handling). pixi then works the same way (win-64 has jq, ffmpeg, nodejs, the Python
  stack); winget is already the last resort. scoop (user-level, no admin) could be a
  fallback for CLI tools conda-forge lacks. Containers need a path mapping.
- **macOS.** Seatbelt denies unix sockets under `(deny network*)`; the Nix daemon socket
  has an explicit rule that needs a real check on a Mac with Nix. Homebrew and Docker
  Desktop need a real run.
- **Downloading pixi** on first use (see above), if users want it.
- **Podman** needs a real test (rootless networks, `keep-id`).
- **devenv** (`devenv.nix`): same shape as the flake (`devenv print-dev-env`). Cheap to add
  as a `Project` when the `devenv` CLI is present.
- **mise as an installer**: `mise use` would edit the workspace's `mise.toml`, and its
  registry covers CLI tools and runtimes but not Python libraries. A possible backend after
  pixi for hosts that already use mise.
- **uv projects** (`pyproject.toml` + `uv.lock`): the `.venv` layer covers synced projects;
  running `uv sync` automatically (network) would be a cheap `Project` behind the same
  question.
- **devcontainer features** would need the devcontainer CLI or reimplementing the feature
  install scripts; not planned unless asked for.

## Decisions for the user

1. Answers are keyed by the workspace's path. Should a moved or re-cloned workspace keep
   them (e.g. keyed by git remote), or is asking again right?
2. Containers: Podman is preferred over Docker when both are installed. Keep that, or
   prefer whichever daemon answers?
3. The default container image is `python:3-slim`. A smaller base without Python
   (`debian:stable-slim`) would make `run_python` need an install first.
4. Should byob offer to download pixi on hosts with no installer?
