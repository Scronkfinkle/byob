# Roadmap

Last updated: 2026-11-09

## Done
- Workspace, nix flake (dev shell + `yah` and `yah-web` packages, gzip web assets), xtask gate.
- Protocol, engine, server; desktop + browser UI sharing one live session; browser auto-reconnect.
- Streaming chat against any OpenAI-compatible URL; endpoint probe drives tool-call mode
  (native / prompt-based fallback / auto-retry), history budget and vision.
- Tool loop, permission gate, approval cards with previews/diffs, steps inline, per-turn undo.
- Persistent multiple chats (SQLite), sidebar, per-device current chat, settings/secrets files,
  always-allow per chat or global.
- Sandboxed file tools; `run_command` / `run_python` through `yah-sandbox` (Landlock + namespaces);
  managed Python via uv; hob-compatible plugins (skills, Python tools, MCP connectors).
- Charts (hob ChartSpec v1, `make_chart` incl. CSV aggregation), inline images, uploads (desktop+web).
- Themes, markdown, remote-access hardening, Tailscale identity allowlist.
- Artifacts: versioned markdown / HTML app / chart / image / text files beside the chat
  (`create_artifact`, `update_artifact` with find/replace edits, `read_artifact`; charts and
  images adopted), cards + side pane, Export, Open in browser; HTML apps run inline in the
  browser client (sandboxed iframe) and on the desktop (Servo in a helper process per live
  app, `host/artifact-host`; live in cards and the pane, liveness pings, Stop/Reload,
  snapshots when out of view).

## Next
1. **Artifact host follow-ups**: GPU rendering (surfman hardware context) instead of
   llvmpipe; `<select>`/date pickers (Servo embedder controls) and IME; a fully offline
   `nix build .#yah-artifact-host` check in CI; macOS/Windows helper builds.
2. **Plugins UI**: list/enable plugins, settings values (secrets), add connector by pasting config.
3. Persist undo journals, attachments and step media; "denied" step status in the store.
4. Media shown where the step happened (currently after the reply); header overflow on narrow widths.
5. Legacy HTTP+SSE MCP transport; OAuth for MCP.
6. macOS sandbox profile (untested); Windows sandboxing.
7. Syntax highlighting; table alignment; markdown for thinking text.
8. Flaky under load: `yah-mcp` `expired_session_is_reported` (passes alone).

## Ideas worth deciding early
- **Endpoint capability probe.** Tool calling differs per server (llama-server needs
  `--jinja`; Ollama/vLLM have their own parsers). Probe at connect (`/models`, `/props`, a
  tiny tool-call test) and fall back to prompt-based tool calling + JSON extraction.
- **Context budget from the server.** hob used a 40k-character history budget for small local
  models; read `n_ctx`/`max_model_len` (silicon reports 262144) and budget by tokens instead.
- **Model roles.** Main model, cheap model for titles/summaries, vision model — each an
  optional URL+model, defaulting to the main one. Vision is just `image_url` parts.
- **Parallel turns.** Servers expose slots (silicon has 4). Let multiple chats/sub-agents run
  concurrently instead of one turn per engine.
- **Remote auth.** Check `Tailscale-User-Login` from `tailscale serve`; allowlist users;
  optional read-only devices. Verify the Origin check behind `tailscale serve` for real.
- **Reconnect.** Browser tabs should auto-reconnect and resync on network blips (phone).
- **Request inspector.** A debug panel showing raw requests/responses per turn — the
  most useful tool when a model or server misbehaves.
- **Smaller web bundle.** 6.9 MB wasm; serve precompressed gzip/brotli (~2 MB).
- **hob compatibility.** Keep `SKILL.md` skills and `mcpServers` config compatible so
  existing skills/connectors work; optional importer for `hob.db` chats.
