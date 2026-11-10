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
- **Automatic project environments.** If the workspace says how it wants to be built
  (`flake.nix`, `shell.nix`, `pixi.toml`, `mise.toml`, `.venv`), every command runs inside
  that environment without the model having to know about it.
- **Hints on failure.** A failed run that's missing a dependency ends with a single line
  naming the exact call, e.g. `Call install_packages with {"packages": ["imagemagick"]}`.
- A two-sentence system prompt paragraph, and no long prose.

## Done (Nix, plus pixi/mise/venv activation)

| Piece | Linux | macOS | Windows |
|---|---|---|---|
| `install_packages` via Nix (buildEnv per workspace, pinned nixpkgs, GC root) | yes, tested | should work, untested (needs Nix) | — |
| `flake.nix` / `shell.nix` dev shell, cached, shellHook applied | yes, tested | untested | — |
| `nix shell nixpkgs#x --command …` inside the sandbox, without network | yes, tested | Seatbelt socket rule unit-tested only | — |
| pixi project activation (`pixi shell-hook --json`) | tested (empty env) | untested | untested |
| mise project activation (`mise install`, `mise env --json`) | tested (`[env]` only) | untested | untested |
| `.venv` / `venv` on `PATH` | yes | yes | yes (`Scripts`) |
| Missing-dependency hints | yes | yes | yes |
| `install_python_packages` (uv) when there's no Nix | unchanged | unchanged | unchanged |

### Decisions, and what was rejected

- **Cache `nix print-dev-env` instead of wrapping each command in `nix develop -c`.** Wrapping
  would evaluate the flake on every call (around a second even with a warm eval cache), and
  `run_python` couldn't find the shell's `python3` beforehand. byob builds the environment
  once with `--profile` (that gives a GC root, and the profile *is* the JSON). It runs the
  `shellHook` once in bash, diffs `env -0` to keep the variables the hook changed (that's
  how direnv works too), and caches the result keyed on `flake.nix`/`flake.lock`/`shell.nix`
  contents.
- **Installed packages live in byob's data folder, not in a workspace `flake.nix`.** Writing
  a flake for the model would mean editing user repos and editing Nix syntax with a weak
  model. A flake in a git repo is invisible to Nix until it's `git add`ed, and in a non-git
  folder a `path:` flake copies the whole workspace (data files included) into the store.
  The per-workspace `buildEnv` avoids all of that. A user who wants it reproducible can
  still ask for a flake, and byob then loads it automatically.
- **No `nix profile install` or `nix-env -i`.** They change the user's global profile and
  aren't scoped to a workspace.
- **With Nix, Python libraries come from nixpkgs, not uv/PyPI wheels.** On NixOS, manylinux
  wheels with native code (numpy, matplotlib) don't load without nix-ld, and mixing the two
  breaks. `install_python_packages` is only registered when there's no `install_packages`.
  This avoids a confusing choice for weak models.
- **The client gets no network; the daemon fetches.** byob sets `substitute = true` because
  the client otherwise disables substitution when it sees no network, and it points the
  registry at a nixpkgs pin. Then `nix shell nixpkgs#x` in an ordinary (network-off) run
  works. Loading a project environment and installing do run with network: flake inputs
  are fetched client-side, and pixi/mise download themselves.

## Plan for the rest

### Linux without Nix, and macOS without Nix: pixi as the installer

pixi (BSD-3, single static binary) installs conda-forge packages into a project folder
without root, on Linux, macOS and Windows. conda-forge has most of what models reach for:
jq, ripgrep, ffmpeg, imagemagick, pandoc, graphviz, nodejs, poppler, tesseract, and the
whole scientific Python stack under its pip names. That makes it the closest thing to Nix
that works across OSes.

- `install_packages` → a byob-owned manifest `<data>/envs/<ws>/pixi.toml` (workspace
  untouched), `pixi add --manifest-path … <names>` (add `python` whenever a Python library
  is requested), then activate with `pixi shell-hook --json --manifest-path …`. Cache it like
  the dev shell. The `Project` plumbing, the JSON parsing and the cache already exist
  (`env/project.rs`). What's left is an install provider behind the same `Envs::install`.
- Name mapping: program names mostly match nixpkgs (`imagemagick`, `ripgrep`, `nodejs`).
  `poppler-utils` is `poppler`, and Python libraries use pip names. Do this as a small table
  per backend next to `COMMANDS`/`PYTHON_IMPORTS`. Unknown names: `pixi search` for
  suggestions.
- Bootstrapping: pixi usually isn't installed. Offer a one-time download of the pinned
  release binary into `<data>/bin/` with checksum verification, behind an approval card.
  **Needs a user decision** (see below).
- On NixOS conda-forge binaries don't run (no `/lib64/ld-linux…` without nix-ld), so Nix
  stays first wherever it's present.
- Nix installed on another distro or on macOS (multi-user or single-user) already takes the
  Nix path. Single-user installs get write access to `/nix/store` and `/nix/var/nix`.

### macOS specifics

- Sandbox: Seatbelt denies unix sockets under `(deny network*)`. Policies now list allowed
  sockets (`Policy::unix_sockets`), and the Nix daemon socket gets
  `(allow network-outbound (remote unix-socket (path-literal …)))`. This needs a real
  check on a Mac with Nix.
- **Homebrew: not the default.** `brew install` is system-wide, mutates `/opt/homebrew`,
  isn't workspace-scoped, and must run outside the sandbox. At most an explicit last resort
  with its own approval wording ("install system-wide with Homebrew"), when neither Nix nor
  pixi is available.

### Windows

- Prerequisites first: there is no sandbox on Windows yet (`sandbox/src/other.rs`), and
  `run_command` uses `sh -c`, which Windows doesn't have (it needs PowerShell/`cmd` handling).
- Then pixi (win-64 has jq, ffmpeg, nodejs, the Python stack). The JSON activation already
  works in principle, and `PATH` joining uses the platform separator.
- **scoop** (user-level, no admin) is a fallback for CLI tools that conda-forge lacks.
  **winget** is rejected as a default: installs are system-wide and often need UAC.

### Other project formats

- **devenv** (`devenv.nix`): same shape as the flake (`devenv print-dev-env`). Cheap to add
  as a `Project` when the `devenv` CLI is present.
- **mise as an installer**: `mise use` would edit the workspace's `mise.toml`, and its
  registry (aqua/ubi/GitHub releases) covers CLI tools and runtimes but not Python
  libraries. It's a possible backend after pixi for hosts that already use mise.
- **asdf**: superseded by mise, which reads `.tool-versions` (already activated).
- **devcontainers** (`.devcontainer/devcontainer.json`): these need Docker or Podman, and
  they *wrap* commands (`devcontainer exec …`) instead of exporting variables. So they need
  a second `Project` shape that returns a command prefix, and the sandbox story changes
  (the container is the boundary). Worth doing only on request.
- **uv projects** (`pyproject.toml` + `uv.lock`): the `.venv` layer covers synced projects.
  Running `uv sync` automatically (network) would be a cheap `Project`.

## Decisions for the user

1. Project environments load **automatically, with network**, on the first command in a
   workspace that has `flake.nix`/`shell.nix`/`pixi.toml`/`mise.toml`. This can take minutes
   and may write lock files. Should that ask first (an approval card per workspace), or is
   the `BYOB_WORKSPACE_ENV=off` escape hatch enough?
2. Should byob offer to download pixi on hosts without Nix (Linux, macOS, Windows)?
3. Homebrew/winget as an explicit, approved last resort: wanted, or never?
