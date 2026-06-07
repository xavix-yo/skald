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
| --- | --- | --- |
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
| --- | --- | --- |
| `skald` | `.` (root) | Main application binary |
| `core-api` | `crates/core-api/` | Shared types and traits: `ServerEvent`, `GlobalEvent`, `ChatHubApi`, `Tool`, `Memory`, `ChatbotClient`, `Transcribe`, `TextToSpeech`, `ImageGenerate`, `SecretsApi`, `LocationManager`, `InterfaceTool`, `Plugin`, `PluginContext`, `ApiProvider`, `ApiProviderRegistry`, `RemoteAccess`. Also owns all DB record types: `LlmProviderRecord`, `LlmModelRecord`, `TtsModelRecord`, `TranscribeModelRecord`, `ImageGenerateModelRecord`. |
| `llm-client` | `crates/llm-client/` | OpenAI-compatible, Anthropic, Ollama, LmStudio implementations of `ChatbotClient`. Depends on `core-api` and re-exports the trait and associated types for backward compatibility. |
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
| --- | --- | --- |
| `src/main.rs` | Thin entry point: tracing → `Config` → `into_split` → plugins → `Skald::new` → `WebFrontend::start` → shutdown | [architecture.md](architecture.md) |
| `src/core/skald.rs` | `Skald` — headless application core; owns all managers; `new(cfg, plugins)` / `shutdown()` | [architecture.md](architecture.md) |
| `src/core/config.rs` | `CoreConfig` + core config types (`DbConfig`, `LlmConfig`, `TicConfig`, `CompactionConfig`, …) | [architecture.md](architecture.md) |
| `src/frontend/config.rs` | `FrontendConfig` (`ServerConfig`, `WebConfig`, `timezone`) | [architecture.md](architecture.md) |
| `src/core/session/handler/` | Core LLM loop, tool dispatch, approval | [session.md](session.md) |
| `src/core/session/handler/message_builder.rs` | `MessageBuilder` — pure service for building OpenAI message arrays, testable in isolation | [session.md](session.md) |
| `src/core/session/manager.rs` | Session factory | [session.md](session.md) |
| `src/core/agents.rs` | Agent discovery, prompt loading | [agents.md](agents.md) |
| `src/core/tools/` | Built-in tool registry | [tools.md](tools.md) |
| `src/core/tools/tool_names.rs` | Centralised tool name constants (`CALL_AGENT`, `RESTART`, …) | [tools.md](tools.md) |
| `src/core/tool_catalog.rs` | `ToolCatalog`: unified listing façade for built-in + MCP tools (wraps ToolRegistry + McpManager) | [tools.md](tools.md) |
| `src/core/provider/` | `ProviderRegistry` (implements `ApiProviderRegistry`) — thin wrapper around `core-api::provider`. All types re-exported for internal use. | [llm-clients.md](llm-clients.md) |
| `src/core/service_manager.rs` | `ServiceManager` trait — lightweight umbrella for all model managers | [llm-clients.md](llm-clients.md) |
| `src/core/chatbot/` | LLM provider clients | [llm-clients.md](llm-clients.md) |
| `src/core/llm/manager.rs` | LLM selection, health tracking | [llm-clients.md](llm-clients.md) |
| `src/core/chat_event_bus.rs` | In-process broadcast bus for chat turns and compaction events | [chat-event-bus.md](chat-event-bus.md) |
| `src/core/compactor.rs` | Context compaction — summarises old history to reduce token usage | [compaction.md](compaction.md) |
| `src/core/memory/` | Pluggable long-term memory layer (trait + manager) | [memory.md](memory.md) |
| `src/core/chat_hub/` | Central chat orchestrator, notification pipeline | [architecture.md](architecture.md) |
| `src/core/tic/` | Background MCP event processor (TicManager) | [architecture.md](architecture.md) |
| `src/core/mcp/` | MCP server management, push notification ingestion | [mcp.md](mcp.md) |
| `src/core/cron/` | Scheduled job scheduler | [cron.md](cron.md) |
| `src/core/plugin/` | Plugin system (PluginManager) | [plugins.md](plugins.md) |
| `src/core/secrets.rs` | SecretsStore — centralised token/key store over SQLite | [secrets.md](secrets.md) |
| `src/core/transcribe/` | TranscribeManager, OpenAiAudioTranscriber, ElevenLabsTranscriber. Traits and record types re-exported from `core-api`. | [transcribe-providers.md](transcribe-providers.md) |
| `src/core/tts/` | TtsManager (DB-backed + plugin slots), OpenAiTtsSynthesiser, ElevenLabsTtsSynthesiser. Traits and record types re-exported from `core-api`. | [tts-providers.md](tts-providers.md) |
| `src/core/image_generate/` | ImageGenerate trait, ImageGeneratorManager (DB-backed + plugin slots), OpenRouterImageGenerator | [image-generate.md](image-generate.md) |
| `src/core/inbox.rs` | `Inbox`: unified façade for pending approvals + clarifications (wraps ApprovalManager, ClarificationManager, ChatHub) | [approval.md](approval.md) |
| `src/core/db/` | SQLite schema and queries | [database.md](database.md) |
| `src/core/events.rs` | WS protocol types | [frontend.md](frontend.md) |
| `src/frontend/mod.rs` | `WebFrontend` — wires `router_factory`, starts plugins, runs Axum | [architecture.md](architecture.md) |
| `src/frontend/server.rs` | `WebServer` — Axum router, TcpListener, `WebServerHandle` | [architecture.md](architecture.md) |
| `src/frontend/api/` | 18 HTTP + WebSocket handlers — `State<Arc<Skald>>` | [frontend.md](frontend.md) |
| `src/config.rs` | `Config` (YAML aggregate: `ServerConfig`, `WebConfig` + re-exports from `core::config`) + `Config::into_split()` | [logging-config.md](logging-config.md) |
| `crates/plugin-honcho/` | Honcho memory sink (standalone crate) | [honcho.md](honcho.md) |
| `crates/plugin-tailscale-remote/` | Remote connectivity via Tailscale mesh (standalone crate) | [remote.md](remote.md) |
| `crates/plugin-transcribe-whisper-local/` | Local STT via whisper.cpp (standalone crate) | [whisper-local.md](whisper-local.md) |
| `crates/plugin-telegram-bot/` | Private Telegram bot (standalone crate) | [telegram.md](telegram.md) |
| `crates/plugin-tts-orpheus-3b/` | Orpheus TTS 3B — local TTS via Python subprocess (standalone crate) | [tts-providers.md](tts-providers.md) |
| `crates/plugin-tts-kokoro/` | Kokoro ONNX — lightweight local TTS, multilingual (standalone crate) | [tts-providers.md](tts-providers.md) |
| `crates/honcho-client/` | Honcho v3 REST API client (standalone crate) | [honcho.md](honcho.md) |
| `web/components/` | Lit frontend components | [frontend.md](frontend.md) |
| `run.sh` | Supervisor loop | [self-rewriting.md](self-rewriting.md) |

---

## Critical Constants

| Constant | Value | Location |
| --- | --- | --- |
| `MAX_AGENT_DEPTH` | **5** | `src/core/session/handler/mod.rs` |
| `DEFAULT_MAX_TOOL_ROUNDS` | **20** | `src/core/session/handler/mod.rs` |
| `FAILURE_DEGRADED` | **3** consecutive failures | `src/core/llm/manager.rs` |
| `FAILURE_DOWN` | **5** consecutive failures | `src/core/llm/manager.rs` |
| Cron scheduler tick | **30 s** | `src/core/cron/mod.rs` |
| Cron fire-check window | **90 s** | `src/core/cron/mod.rs` |
| MCP startup timeout | **120 s** | `src/core/mcp/mod.rs` |
| TIC tick interval | **900 s** default | `config.yml` → `tic.interval_secs` |
| TIC batch size | **50 events** default | `config.yml` → `tic.batch_size` |
| Notification batch window | **200 ms** | `src/core/chat_hub/mod.rs` |

---

## Navigation

- [architecture.md](architecture.md) — component wiring, startup sequence, request lifecycle
- [chat-event-bus.md](chat-event-bus.md) — ChatEventBus, event types, publication rules, adding consumers
- [self-rewriting.md](self-rewriting.md) — restart mechanism, safe self-modification workflow
- [session.md](session.md) — ChatSessionHandler, tool loop, approval gate
- [agents.md](agents.md) — agent discovery, meta.json, call_agent, depth limit
- [tools.md](tools.md) — Tool trait, ToolRegistry, built-in catalogue
- [llm-clients.md](llm-clients.md) — ChatbotClient trait, LlmManager, ApiProvider, ProviderRegistry, AUTO selection
- [compaction.md](compaction.md) — context compaction: trigger, summarisation flow, DB schema, config
- [mcp.md](mcp.md) — McpManager, transports, naming convention, enable/disable
- [gcal-mcp.md](gcal-mcp.md) — Google Calendar read-only MCP server (custom Python)
- [gmail-mcp.md](gmail-mcp.md) — Gmail read+modify MCP server (custom Python)
- [gmaps-mcp.md](gmaps-mcp.md) — Google Maps transit/directions MCP server (custom Python)
- [whatsapp-mcp.md](whatsapp-mcp.md) — WhatsApp read+send MCP server (custom Node.js)
- [approval.md](approval.md) — ApprovalManager: human-in-the-loop, regole, pending approvals
- [cron.md](cron.md) — TaskManager, cron jobs & immediate tasks, 7-field cron syntax, job lifecycle
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
