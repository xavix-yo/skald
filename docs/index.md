# Skald — Documentation

## Documentation Rule (MANDATORY)

**Every source code change — made by a human or by an LLM — must be accompanied by an update to the relevant doc file(s). No exception.**

This includes:
- Adding or removing a tool → update [tools.md](tools.md)
- Changing a table schema → update [database.md](database.md)
- Modifying the approval gate or tool loop → update [session.md](session.md)
- Adding a new agent → update [agents.md](agents.md)
- Any change to the WS protocol → update [frontend.md](frontend.md)

---

## Key paths (agent: read this first)

| Resource | Default path | Override |
|---|---|---|
| **SQLite database** | `./database.db` | `db.path` in `config.yml` |
| **Config file** | `./config.yml` | — (copy from `default.config.yaml`) |
| **Secrets folder** | `./secrets/` | — |
| **Model cache** | `./models/` | — |
| **Log files** | `./logs/` | — |
| **Static web assets** | `./web/` | `web.static_dir` in `config.yml` |

When looking for the database, **always use `./database.db`** unless `config.yml` says otherwise.

---

## Project Summary

A local chat server (Axum + Tokio + SQLite) where an LLM handles user queries via tool calls. The app can rewrite and restart its own source code autonomously. Multiple specialized agents collaborate via a recursive sub-agent system. External tools are integrated via MCP (Model Context Protocol). Entry point: `run.sh`.

## Workspace Structure

The project is a Cargo workspace. Extracted crates live in `crates/`:

| Crate | Path | Notes |
|---|---|---|
| `skald` | `.` (root) | Main application binary |
| `core-api` | `crates/core-api/` | Shared types and traits: `ServerEvent`, `GlobalEvent`, `ChatHubApi`, `Tool`, `Memory`, `Transcribe`, `TextToSpeech`, `SecretsApi`, `LocationManager`, `InterfaceTool`, `Plugin`, `PluginContext`, `RemoteAccess` |
| `llm-client` | `crates/llm-client/` | `ChatbotClient` trait + provider implementations (Anthropic, OpenAI, Ollama, LmStudio) |
| `mcp-client` | `crates/mcp-client/` | MCP protocol layer: `McpServer` (stdio), `McpHttpServer`, `McpServerClient` trait, config types |
| `honcho-client` | `crates/honcho-client/` | Honcho v3 REST API client — zero dependencies on the main crate |
| `plugin-honcho` | `crates/plugin-honcho/` | Honcho memory sink plugin |
| `plugin-tailscale-remote` | `crates/plugin-tailscale-remote/` | Remote connectivity via Tailscale mesh |
| `plugin-transcribe-whisper-local` | `crates/plugin-transcribe-whisper-local/` | Local STT via whisper.cpp (Metal-accelerated) |
| `plugin-telegram-bot` | `crates/plugin-telegram-bot/` | Private Telegram bot interface |
| `plugin-tts-orpheus-3b` | `crates/plugin-tts-orpheus-3b/` | Local TTS via Orpheus 3B (Python subprocess) |
| `plugin-tts-kokoro` | `crates/plugin-tts-kokoro/` | Local TTS via Kokoro ONNX (lightweight, multilingual) |

To add a new extracted crate: create `crates/<name>/`, add it to the `[workspace].members` list in the root `Cargo.toml`, then add a `path` dependency in `[dependencies]`.

---

## Module Map

| Source path | Role | Doc |
|---|---|---|
| `src/main.rs` | Startup, wiring | [architecture.md](architecture.md) |
| `src/session/handler/` | Core LLM loop, tool dispatch, approval | [session.md](session.md) |
| `src/session/manager.rs` | Session factory | [session.md](session.md) |
| `src/agents.rs` | Agent discovery, prompt loading | [agents.md](agents.md) |
| `src/tools/` | Built-in tool registry | [tools.md](tools.md) |
| `src/chatbot/` | LLM provider clients | [llm-clients.md](llm-clients.md) |
| `src/llm/manager.rs` | LLM selection, health tracking | [llm-clients.md](llm-clients.md) |
| `src/chat_event_bus.rs` | In-process broadcast bus for chat turns and compaction events | [chat-event-bus.md](chat-event-bus.md) |
| `src/compactor.rs` | Context compaction — summarises old history to reduce token usage | [compaction.md](compaction.md) |
| `src/memory/` | Pluggable long-term memory layer (trait + manager) | [memory.md](memory.md) |
| `src/chat_hub/` | Central chat orchestrator, notification pipeline | [architecture.md](architecture.md) |
| `src/tic/` | Background MCP event processor (TicManager) | [architecture.md](architecture.md) |
| `src/mcp/` | MCP server management, push notification ingestion | [mcp.md](mcp.md) |
| `src/cron/` | Scheduled job scheduler | [cron.md](cron.md) |
| `src/plugin/` | Plugin system (PluginManager) | [plugins.md](plugins.md) |
| `crates/plugin-honcho/` | Honcho memory sink (standalone crate) | [honcho.md](honcho.md) |
| `crates/plugin-tailscale-remote/` | Remote connectivity via Tailscale mesh (standalone crate) | [remote.md](remote.md) |
| `crates/plugin-transcribe-whisper-local/` | Local STT via whisper.cpp (standalone crate) | [whisper-local.md](whisper-local.md) |
| `crates/plugin-telegram-bot/` | Private Telegram bot (standalone crate) | [telegram.md](telegram.md) |
| `crates/plugin-tts-orpheus-3b/` | Orpheus TTS 3B — local TTS via Python subprocess (standalone crate) | [tts-providers.md](tts-providers.md) |
| `crates/plugin-tts-kokoro/` | Kokoro ONNX — lightweight local TTS, multilingual (standalone crate) | [tts-providers.md](tts-providers.md) |
| `crates/honcho-client/` | Honcho v3 REST API client (standalone crate) | [honcho.md](honcho.md) |
| `src/secrets.rs` | SecretsStore — centralised token/key store over SQLite | [secrets.md](secrets.md) |
| `src/transcribe/` | Transcribe trait, TranscribeManager, OpenAiAudioTranscriber, ElevenLabsTranscriber | [transcribe-providers.md](transcribe-providers.md) |
| `src/tts/` | TextToSpeech trait, TtsManager (DB-backed + plugin slots), OpenAiTtsSynthesiser, ElevenLabsTtsSynthesiser | [tts-providers.md](tts-providers.md) |
| `src/image_generate/` | ImageGenerate trait, ImageGeneratorManager (DB-backed + plugin slots), OpenRouterImageGenerator | [image-generate.md](image-generate.md) |
| `src/db/` | SQLite schema and queries | [database.md](database.md) |
| `src/events.rs` | WS protocol types | [frontend.md](frontend.md) |
| `src/config.rs` | Config file loading; `LlmProvider` enum (legacy name — covers all API providers including TTS-only ones like ElevenLabs) | [logging-config.md](logging-config.md) |
| `web/components/` | Lit frontend components | [frontend.md](frontend.md) |
| `run.sh` | Supervisor loop | [self-rewriting.md](self-rewriting.md) |

---

## Critical Constants

| Constant | Value | Location |
|---|---|---|
| `MAX_AGENT_DEPTH` | **5** | `src/session/handler/mod.rs` |
| `DEFAULT_MAX_TOOL_ROUNDS` | **20** | `src/session/handler/mod.rs` |
| `FAILURE_DEGRADED` | **3** consecutive failures | `src/llm/manager.rs` |
| `FAILURE_DOWN` | **5** consecutive failures | `src/llm/manager.rs` |
| Cron scheduler tick | **30 s** | `src/cron/mod.rs` |
| Cron fire-check window | **90 s** | `src/cron/mod.rs` |
| MCP startup timeout | **120 s** | `src/mcp/mod.rs` |
| TIC tick interval | **900 s** default | `config.yml` → `tic.interval_secs` |
| TIC batch size | **50 events** default | `config.yml` → `tic.batch_size` |
| Notification batch window | **200 ms** | `src/chat_hub/mod.rs` |

---

## Navigation

- [architecture.md](architecture.md) — component wiring, startup sequence, request lifecycle
- [chat-event-bus.md](chat-event-bus.md) — ChatEventBus, event types, publication rules, adding consumers
- [self-rewriting.md](self-rewriting.md) — restart mechanism, safe self-modification workflow
- [session.md](session.md) — ChatSessionHandler, tool loop, approval gate
- [agents.md](agents.md) — agent discovery, meta.json, call_agent, depth limit
- [tools.md](tools.md) — Tool trait, ToolRegistry, built-in catalogue
- [llm-clients.md](llm-clients.md) — ChatbotClient trait, LlmManager, AUTO selection
- [compaction.md](compaction.md) — context compaction: trigger, summarisation flow, DB schema, config
- [mcp.md](mcp.md) — McpManager, transports, naming convention, enable/disable
- [gcal-mcp.md](gcal-mcp.md) — Google Calendar read-only MCP server (custom Python)
- [gmail-mcp.md](gmail-mcp.md) — Gmail read+modify MCP server (custom Python)
- [gmaps-mcp.md](gmaps-mcp.md) — Google Maps transit/directions MCP server (custom Python)
- [whatsapp-mcp.md](whatsapp-mcp.md) — WhatsApp read+send MCP server (custom Node.js)
- [approval.md](approval.md) — ApprovalManager: human-in-the-loop, regole, pending approvals
- [cron.md](cron.md) — CronTaskManager, 7-field cron syntax, job lifecycle
- [database.md](database.md) — SQLite schema, migration pattern
- [frontend.md](frontend.md) — WebSocket protocol, ServerEvent types, Lit components
- [logging-config.md](logging-config.md) — log levels, config.yml full reference
- [plugins.md](plugins.md) — Plugin trait, PluginManager, TranscribeManager, provider catalogue
- [memory.md](memory.md) — Memory trait, MemoryManager, integration in the LLM loop
- [honcho.md](honcho.md) — Honcho memory plugin: setup, config, filtering, lifecycle
- [telegram.md](telegram.md) — Telegram bot setup, pairing, whitelist, HITL approval
- [whisper-local.md](whisper-local.md) — Local STT via whisper.cpp, model setup, TranscribeManager integration
- [secrets.md](secrets.md) — SecretsApi trait, SecretsStore, well-known keys, security notes
- [transcribe-providers.md](transcribe-providers.md) — Cloud STT via OpenAI-compatible audio API, transcribe_models DB table
- [tts-providers.md](tts-providers.md) — Text-to-Speech: trait, manager, OpenAiTtsSynthesiser, tts_models DB table
- [image-generate.md](image-generate.md) — Image generation: trait, manager, async task system, LLM tools, REST endpoint
- [skills.md](skills.md) — Skills system: reusable Python capability packages
- [notifications.md](notifications.md) — Notification preferences: `data/notifications.md` format, how TIC uses it, how the main agent updates it
- [workspace-crates.md](workspace-crates.md) — Workspace crate catalogue, `core-api` module reference, plugin extraction roadmap

## When to Update This File

- A new source module is added or removed
- A critical constant changes
- A new doc file is added to `docs/`
