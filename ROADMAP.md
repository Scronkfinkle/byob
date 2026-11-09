# Roadmap

Last updated: 2026-11-09

## Done
- Workspace, nix flake (dev shell + `yah` and `yah-web` packages, gzip web assets), xtask gate.
- Protocol, engine, server; desktop + browser UI sharing one live session; browser auto-reconnect.
- Streaming chat against any OpenAI-compatible URL; capability probe; token budgeting.
- Tool loop (30-call cap, repeat detection), permission gate, approval cards any client can answer.
- Sandboxed file tools; hob-compatible plugins: skills, Python tools, MCP connectors (blocking client).
- Themes (system/dark/light, accents, sizes, high contrast), markdown rendering.
- Remote access hardening: Tailscale identity allowlist, Origin checks, connection caps, timeouts.
- Persistence: chats/messages/steps in `yah-store` (SQLite), batched stream writes, interrupted-reply
  recovery; multiple chats with a sidebar (per-client current chat, auto titles); "always allow"
  per chat or global; settings.json/secrets.json with CLI overrides.

## Next
1. **Undo** of file changes using the journal + `.yah/trash`.
2. **Approval card polish**: present-tense titles ("Write todo.md?"), readable args/diff instead
   of raw JSON; steps interleaved with text (`Step.at`); stop showing "Thinking…" while waiting.
3. **Plugins UI**: list/enable plugins, settings values (secrets), add connector by pasting config.
4. **Use the probe**: pick native vs prompt-based tool calling, budget history from context size.
5. Legacy HTTP+SSE MCP transport; OAuth for MCP; process-group kill for stdio servers/scripts.
6. Sandbox for Python tools and stdio MCP servers (Landlock on Linux).
7. Syntax highlighting; table alignment; markdown for thinking text.

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
