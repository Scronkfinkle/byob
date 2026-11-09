# Roadmap

Last updated: 2026-11-09

## Done
- Workspace, nix flake (dev shell + `yah` and `yah-web` packages), xtask gate.
- Protocol, engine, server; desktop + browser UI sharing one live session.
- Streaming chat against any OpenAI-compatible URL (reasoning split out, usage stats, cancel).

## Next
1. **Tool loop** in `yah-agent`: tool specs → streamed `tool_calls` → permission gate →
   run → feed results back; cap 30 calls/turn. Port hob's core file tools.
2. **Approvals over the protocol**: approval cards any client can answer (phone included).
3. **Persistence** (`yah-store`, SQLite): chats, messages, steps, undo journal; multiple chats.
4. **Markdown** rendering (egui_commonmark or own) with code blocks + copy.
5. **Settings persistence**: inference URL/model in a config file; API key in the OS keyring.

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
