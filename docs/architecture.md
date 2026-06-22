# Architecture

## Two-Layer Design

```
src/
  core/         ← headless application core (no HTTP, no Axum)
    skald.rs    ← Skald struct: owns all managers, lifecycle (new / shutdown)
    …           ← all domain modules (db, llm, session, cron, plugin, …)
  frontend/     ← web presentation layer
    mod.rs      ← WebFrontend: wires router_factory, starts plugins, runs Axum
    server.rs   ← WebServer (Axum router, TcpListener)
    api/        ← 18 HTTP + WebSocket handlers — State<Arc<Skald>>
  core/config.rs    ← CoreConfig + DbConfig, LlmConfig, TicConfig, … (core-owned types)
  frontend/config.rs ← FrontendConfig + ServerConfig, WebConfig
  config.rs         ← Config (YAML parse only) + into_split()
  main.rs           ← thin: tracing → Config → into_split → plugins → Skald::new → WebFrontend::start → shutdown
```

`Skald` knows nothing about Axum or HTTP. It can be started headlessly. `WebFrontend` is the only component that imports Axum and constructs an HTTP server.

`Config::into_split()` produces a `CoreConfig` (db, llm, tic, cron, timezone) for `Skald::new()` and a `FrontendConfig` (server, web, timezone) for `WebFrontend::new()`. The YAML file structure is unchanged. `timezone` is cloned into both since it is used by the cron scheduler (core) and optionally by the frontend.

Plugin instances are constructed in `main.rs` as `Vec<Arc<dyn Plugin>>` and injected into `Skald::new()` — the core never depends on concrete plugin crates.

---

## Component Map

| Struct | Created by | Held as | Depends on |
| --- | --- | --- | --- |
| `SqlitePool` | `db::init_pool()` | `Arc<SqlitePool>` | — |
| `LlmManager` | `LlmManager::new()` | `Arc<LlmManager>` | `SqlitePool` |
| `McpManager` | `McpManager::new()` | `Arc<McpManager>` | `SqlitePool` |
| `TaskManager` | `TaskManager::new()` | `Arc<TaskManager>` | `SqlitePool`, `ChatSessionManager` (via OnceLock), `ChatHub` (via OnceLock) |
| `ToolRegistry` | `Skald::new()` inline | `Arc<ToolRegistry>` | `McpManager`, `TaskManager`, `PluginManager` |
| `ApprovalManager` | `ApprovalManager::new()` | `Arc<ApprovalManager>` | `SqlitePool` |
| `ClarificationManager` | `ClarificationManager::new()` | `Arc<ClarificationManager>` | — |
| `Inbox` | `Inbox::new()` | (owned by Skald) | `ApprovalManager`, `ClarificationManager`, `ChatHub` |
| `ToolCatalog` | `ToolCatalog::new()` | (owned by Skald) | `ToolRegistry`, `McpManager` |
| `ChatEventBus` | `ChatEventBus::new()` | `Arc<ChatEventBus>` | — |
| `ContextCompactor` | `Skald::new()` (when `llm.compaction` configured) | `Option<Arc<ContextCompactor>>` | `LlmManager`, `ChatEventBus` |
| `ChatSessionManager` | `ChatSessionManager::new()` | `Arc<ChatSessionManager>` | `SqlitePool`, `LlmManager`, `ToolRegistry`, `McpManager`, `ApprovalManager`, `ClarificationManager`, `ChatEventBus`, `ContextCompactor` |
| `ChatHub` | `ChatHub::new()` | `Arc<ChatHub>` | `SqlitePool`, `ChatSessionManager`, `ApprovalManager` |
| `TicManager` | `TicManager::new()` | `Arc<TicManager>` | `SqlitePool`, `ChatHub`, `ChatSessionManager` |
| `Skald` | `Skald::new(&core_cfg, plugins)` | `Arc<Skald>` | all of the above |
| `WebFrontend` | `WebFrontend::new(skald, &frontend_cfg)` | owned by `main` | `Arc<Skald>`, `FrontendConfig` |

### Circular Dependencies

**`TaskManager` ↔ `ChatSessionManager`**: `TaskManager` needs `ChatSessionManager` to dispatch jobs, but `ChatSessionManager` is built after `ToolRegistry` which holds `Arc<TaskManager>`. Broken with `std::sync::OnceLock`: `TaskManager` is created first, `set_session()` is called after `ChatSessionManager` exists.

**`TaskManager` ↔ `ChatHub`**: Same pattern — `ChatHub` is built after `cron.start()`. `set_hub()` is called immediately after `ChatHub::new()`. The cron tick loop starts 30 s after `start()`, so hub is always ready by the first real job dispatch.

**`PluginManager` ↔ `Skald`**: `PluginManager` is constructed early (to register tools), then `set_skald(Arc<Skald>)` is called after `Arc::new(Skald { … })`. `set_router_factory(RouterFactory)` is called by `WebFrontend::start()` before `start_enabled()`.

---

## Startup Sequence

### `main.rs`
1. Init tracing (`tracing-appender` daily rolling to `logs/`)
2. `Config::load()` → `config.into_split()` → `(CoreConfig, FrontendConfig)`
3. Build `Vec<Arc<dyn Plugin>>` — all plugin instances constructed here
4. `Skald::new(&core_cfg, plugins)` — see sequence below
5. `WebFrontend::new(skald, &frontend_cfg)` + `.start()` — see sequence below
6. Await `ctrl_c`
7. `skald.shutdown()` + `handle.shutdown()`

### Inside `Skald::new(&core_cfg)`
1. `db::init_pool()` — opens SQLite, runs `create_tables()` (idempotent)
2. `SystemEventBus::new()`
3. `agents::discover()` — scans `agents/*/` for `meta.json` + `AGENT.md`
4. `ProviderRegistry::new()` + register 6 built-in LLM providers
5. `LlmManager::new()` — loads providers and models from DB
6. Spawn LLM request log cleanup task (if configured)
7. `SecretsStore::new()`
8. `McpManager::new()` + background `initialize()` — connects MCP servers from DB
9. `TaskManager::new()` — creates scheduler (not started yet)
10. `PluginManager::new()` — plugins registered, not yet started
11. `ToolRegistry` built — all built-in tools registered
12. `ApprovalManager::new()` — loads approval rules from DB; seeds defaults
13. `ImageGeneratorManager::new(pool, "data")` — image generation provider registry
14. `ChatEventBus::new()`
15. `MemoryManager::new()`
16. `ContextCompactor::new()` (if `llm.compaction` configured)
17. `ClarificationManager::new()`
18. `ChatSessionManager::new()` — session factory wired up
19. `cron.set_session()` — breaks TaskManager circular dep
20. `TranscribeManager::new()`, `TtsManager::new()`
21. `ChatHub::new()` — spawns notification consumer task
22. `cron.set_hub(chat_hub)` — wires ChatHub into TaskManager
23. `Inbox::new(approval, clarification, chat_hub)` — unified pending-requests façade
24. `ToolCatalog::new(tools, mcp)` — unified tool listing façade
25. `cron.start(shutdown_token)` + `tic_manager.start(shutdown_token)` — background loops begin; handles stored in `bg_handles`
26. `Arc::new(Skald { … })` assembled
27. `plugin_manager.set_skald(Arc::clone(&skald))` — post-construction wiring

### Inside `WebFrontend::start()`
28. `plugin_manager.set_router_factory(factory)` — provides Axum router factory to plugins
29. `plugin_manager.set_web_port(port)` — provides HTTP port to plugins (e.g. Tailscale)
30. `plugin_manager.start_enabled()` — starts Telegram and other enabled plugins
31. `plugin_manager.start_config_watcher(shutdown_token)` — polls DB every 30 s
32. `WebServer::start(addr)` — Axum HTTP+WS server begins listening

---

## Request Lifecycle

1. Client opens WebSocket: `GET /api/ws`
2. `handle_socket()` gets or creates `ChatSessionHandler` via `ChatHub::session_handler("web")`
3. Client sends `ClientMessage` JSON over WS
4. `ChatHub::send_message("web", prompt, opts)` **enqueues** the message on the source's inbox and returns; it does not run the turn inline (see *Per-source inbox* in [session.md](session.md))
5. The source's single consumer task drains the inbox, **coalescing** any messages that piled up during an in-flight turn into one prompt (joined by blank lines), and calls `handler.handle_message(...)`
6. `handle_message` acquires `processing: Mutex<()>` (one at a time per session)
7. `run_agent_turn` loop starts (up to `max_tool_rounds` rounds):
   - Build context: `build_openai_messages()` → system prompt + history + tool results
   - Apply permission-group visibility filter (hide tools `Deny`d for the session's run-context group)
   - Call LLM: `llm.client.chat_with_tools()`
   - LLM returns `LlmTurn::Message` → send `Done` event, exit loop
   - LLM returns `LlmTurn::ToolCalls` → for each call:
     - Approval check → optionally send `PendingWrite`, wait for user
     - Dispatch tool → send `ToolStart` / `ToolDone` / `ToolError`
     - `call_agent` → recurse via `dispatch_sub_agent`
8. Main loop sends `Done` event with final content and token counts

---

## Notification Flow (background)

```text
MCP server stdout (JSON-RPC notification, no id field)
  → McpServer reader loop (src/core/mcp/server.rs)
  → notification_tx (mpsc::UnboundedSender)
  → McpManager::notification_consumer
  → db::mcp_events::insert(source, method, payload)

[every tic.interval_secs (default 900 s) — TicManager::run_tick()]
  → mcp_events::pending_limited(tic.batch_size)
  → mcp_events::mark_processed(ids)
  → build_prompt(events)
  → ChatHub::send_message("tic", prompt)   ← ephemeral session
  → TIC agent runs, calls notify(briefing)
  → ChatHub::notify_sync → mpsc channel

[ChatHub::notification_consumer]
  → batching window (200 ms)
  → appends synthetic Assistant message to chat_history (reasoning_content + is_synthetic=true)
  → inserts chat_llm_tools row for read_notification (status='done', result=[briefings])
  → calls hub.resume(home_source)
  → resume_turn picks up the synthetic tool call, runs the LLM loop
  → user sees assistant briefing in home conversation
```

---

## Skald Fields

`Skald` is the headless application core. Fields are exposed to `WebFrontend` handlers via `State<Arc<Skald>>`. High-level facets like `Inbox` encapsulate multiple underlying managers behind a simpler public API.

| Field | Type | Purpose |
| --- | --- | --- |
| `db` | `Arc<SqlitePool>` | Direct DB access |
| `system_bus` | `Arc<SystemEventBus>` | Cross-service event bus |
| `provider_registry` | `Arc<ProviderRegistry>` | LLM/AI provider registry |
| `llm_manager` | `Arc<LlmManager>` | LLM selection, health tracking |
| `secrets` | `Arc<SecretsStore>` | Centralised token/key store |
| `mcp` | `Arc<McpManager>` | MCP server management |
| `cron` | `Arc<TaskManager>` | Scheduled job and immediate task management |
| `plugin_manager` | `Arc<PluginManager>` | Plugin lifecycle |
| `tools` | `Arc<ToolRegistry>` | Built-in tool dispatch |
| `approval` | `Arc<ApprovalManager>` | Human-in-the-loop approval rules |
| `image_generator_manager` | `Arc<ImageGeneratorManager>` | Text-to-image provider registry |
| `inbox` | `Inbox` | Unified façade for pending approvals + clarifications |
| `catalog` | `ToolCatalog` | Unified tool listing façade (built-in + MCP) |
| `event_bus` | `Arc<ChatEventBus>` | In-process broadcast bus for chat turns |
| `memory_manager` | `Arc<MemoryManager>` | Long-term memory provider registry |
| `clarification` | `Arc<ClarificationManager>` | Pending clarification requests |
| `manager` | `Arc<ChatSessionManager>` | Session factory |
| `chat_hub` | `Arc<ChatHub>` | Central chat orchestrator |
| `transcribe_manager` | `Arc<TranscribeManager>` | Speech-to-Text provider registry |
| `tts_manager` | `Arc<TtsManager>` | Text-to-Speech provider registry |
| `tic_manager` | `Arc<TicManager>` | Background MCP event processor |
| `location_manager` | `Arc<LocationManager>` | Named GPS position store |
| `remote` | `Arc<RwLock<Option<Arc<dyn RemoteAccess>>>>` | Active remote-connectivity provider (e.g. Tailscale) |
| `shutdown_token` | `CancellationToken` | Shared cancellation signal for all background tasks |

---

## Graceful Shutdown

On SIGINT, `main.rs` executes:

1. `skald.shutdown()`:
   - `shutdown_token.cancel()` — signals all background loops to exit their `select!`
   - Await `bg_handles` (cron + tic) with 10 s timeout
   - `plugin_manager.stop_all()`
2. `handle.shutdown()` — drains and closes the Axum HTTP server

Background tasks that respond to `shutdown_token.cancelled()`:

| Task | Source |
| --- | --- |
| `TaskManager` scheduler loop | `src/core/cron/mod.rs` |
| `TaskManager` cleanup loop | `src/core/cron/mod.rs` |
| `TicManager` timer loop | `src/core/tic/mod.rs` |
| `PluginManager` config watcher | `src/core/plugin/mod.rs` |
| LLM request log cleanup | `src/core/skald.rs` |
| `McpManager` notification consumer | `src/core/mcp/mod.rs` |
| `ChatHub` notification consumer | `src/core/chat_hub/mod.rs` |
| `TtsManager` API provider reload watcher | `src/core/tts/manager.rs` |
| `TranscribeManager` API provider reload watcher | `src/core/transcribe/manager.rs` |

---

## Workspace Crates

The binary depends on several independent library crates in `crates/`. Each crate has no dependency on the main `skald` crate and can be published or reused standalone.

| Crate | Path | Purpose |
| --- | --- | --- |
| `core-api` | `crates/core-api/` | Shared types and traits: `ServerEvent`, `GlobalEvent`, `InterfaceTool`, `SendMessageOptions`, `ChatHubApi` trait |
| `llm-client` | `crates/llm-client/` | LLM client abstraction (OpenAI-compat, Anthropic, Ollama) |
| `mcp-client` | `crates/mcp-client/` | MCP client (JSON-RPC over stdio/SSE) |
| `honcho-client` | `crates/honcho-client/` | Honcho long-term memory HTTP client |

### `core-api` — plugin extraction boundary

`core-api` is the designated contract crate for plugin independence. A plugin that depends only on `core-api` (instead of the full main crate) can be extracted into its own workspace member without circular dependencies.

See [crates/workspace.md](crates/workspace.md) for the full extraction roadmap.

### Future: `skald-core` crate

`src/core/` is designed as a stepping stone toward extracting the headless core into a standalone `crates/skald-core/` crate. When that happens:
- `src/core/` moves to `crates/skald-core/src/`
- `Skald` becomes the crate's public API
- `src/frontend/` depends on `skald-core` as a path dependency

---

## When to Update This File

- A new field is added to `Skald`
- The startup sequence in `Skald::new()` or `WebFrontend::start()` changes
- The request lifecycle changes (new event type, new loop behavior)
- A new circular dependency and its resolution is introduced
- A new workspace crate is added
