# ChatHub

`ChatHub` (`src/core/chat_hub/`) is the single entry point for **interactive, user-facing chat sessions** — web, mobile, Telegram, project chats. It owns the `source → session` mapping, serializes and **coalesces** incoming user messages per source, runs each turn through a `ChatSessionHandler`, and bridges every turn's events onto the global broadcast bus that connected clients subscribe to.

## What it is — and is not

ChatHub manages **one live, persistent session per `source`**, addressed by source id through the `sources` table. It is **not** a runner for background / non-interactive agents:

- Cron jobs, TIC ticks, and async sub-agent tasks go through `TaskManager` / `ChatSessionManager` directly. They are not user-facing, have no broadcast audience, and must not appear in the `sources` table.
- The one bridge from background → interactive is **notifications**: a background agent calls `ChatHub::notify(...)`, and the notification consumer delivers an aggregated briefing to the *home* source (see below).

Keep that boundary: routing a non-interactive agent through ChatHub is a misuse.

## Source → session mapping

The `sources` table (`src/core/db/sources.rs`) maps each `source_id` to its `active_session_id`. `get_or_create_session` looks it up and lazily creates a session on first use; `provision_session(reset=true)` discards the current session and starts a fresh one (emitting a `NewSession` event). `clear(source)` is the thin wrapper used by `/clear` / "new conversation".

## Per-source inbox (serialization + coalescing)

Each source gets one **`SourceInbox`** and one **consumer task**, created lazily on the first message (`src/core/chat_hub/inbox.rs`). This sits *in front of* the handler's `processing` mutex and gives two properties the old per-message detached-spawn dispatch lacked:

1. **FIFO ordering** — a single consumer per source means arrival order = execution order. (Previously each message was a detached `tokio::spawn` racing for the `processing` lock, so order was not guaranteed.)
2. **Coalescing** — messages that arrive while a turn is running accumulate in the inbox and are merged into **one** follow-up turn instead of triggering N separate turns.

### Flow

```text
send_message(source, prompt, opts)
  → push QueuedMessage onto the source's inbox, notify, return immediately
        (turn errors surface via the Error event on the bus, not the return value)

[per-source consumer task]
  → wait for notify (+ optional debounce window)
  → loop: build_unit(pending) → dispatch_turn(...) until the queue is empty
        dispatch_turn = resolve session/handler, bridge events to the global bus,
        inject execute_task, call handler.handle_message (takes the processing lock)
```

Because the consumer holds the inbox lock **only** while building a unit (never during the turn), messages keep arriving into `pending` during a turn and are drained on the next iteration — this is what realizes *coalesce-while-busy* with no polling.

### Coalescing rules (`build_unit`)

- Empty queue → nothing to do.
- A **synthetic** message (`opts.is_synthetic` — notifications / TIC) is always dispatched **alone**, never merged with user text.
- Otherwise, the leading run of consecutive **non-synthetic** messages is concatenated into one prompt joined by `"\n\n"`, using the most recent message's `opts` (interface tools / system context are identical across a single source's batch).

### Idle debounce

`SOURCE_COALESCE_DEBOUNCE_MS` (in `mod.rs`) defaults to **0** = pure coalesce-while-busy: a message to an idle source dispatches immediately. Raising it also batches messages sent rapidly to an *idle* source, at the cost of that latency on the first message of a burst.

### `/stop` and `/clear`

- **`cancel(source)`** (`/stop`, stop button) clears the inbox's pending queue *and* cancels the in-flight turn (`handler.cancel()`). A `cancel_epoch` counter guards the tiny window where the consumer drained a unit microseconds before the stop — the stale unit is dropped instead of dispatched.
- **`clear(source)` / `provision_session(reset=true)`** (`/clear`, new conversation) also drops messages queued for the discarded session.

## Event bus

ChatHub owns a single global broadcast channel (`global_tx`, capacity 512). Every turn gets a fresh mpsc sender via `bridge_to_global`, which forwards the handler's `ServerEvent`s onto the global bus wrapped in a `GlobalEvent { source, session_id, event }`. Subscribers (`events(source)`) filter by source themselves — e.g. the WebSocket handler and the Telegram `persistent_forwarder`. `emit(...)` posts a sessionless event directly.

## Notifications (background → home source)

`notify(briefing)` / `notify_sync(briefing)` push onto a central mpsc queue. The `notification_consumer` task batches bursts over `NOTIFY_BATCH_WINDOW_MS` (200 ms), then delivers them to the *home* source (`set_home` / `HOME_SOURCE_KEY`, default `web`) by appending a synthetic Assistant message with a pre-completed `read_notification` tool call and calling `resume(...)`. This path uses `resume`, not `send_message`, so it does not go through the per-source inbox.

## API surface (`ChatHubApi` trait)

Defined in `crates/core-api/src/chat_hub.rs`, implemented on `ChatHub`:

| Method | Purpose |
|--------|---------|
| `send_message` | Enqueue a user message for a source (coalesced, async) |
| `register` | Register a source (no-op with the global bus) |
| `clear` | New session for the source, discard the previous one |
| `cancel` | Stop the in-flight turn + clear the queued backlog |
| `resume` | Resume an interrupted turn (pending tools / async result injection) |
| `reset_mcp` | Revoke session-scoped MCP grants |
| `set_home` | Set which source receives background notifications |
| `context_info` / `cost_info` | Last-turn token usage / total session spend |
| `force_compact` | Force context compaction now |
| `events` | Subscribe to the global event bus |
| `resolve_question` | Answer a pending `ask_user_clarification` |
| `approve` / `reject` | Resolve a pending tool-call approval |

## Forward-looking: mid-turn injection

The per-source inbox centralizes pending user messages, which is the seam for a **future** capability: delivering a user message *into* an in-flight turn instead of waiting for it to finish. The LLM loop already rebuilds history fresh each round (`llm_loop.rs`), and `inject_async_result` (`src/core/cron/mod.rs`) is the existing precedent for appending to `chat_history` + `resume()` without a second `processing` lock. A future round-boundary hook in `run_agent_turn` could drain the inbox mid-turn, respecting role alternation (never inject between an assistant tool-call and its tool results). **Not implemented yet** — designed for.

## Relevant files

| Path | Role |
|------|------|
| `src/core/chat_hub/mod.rs` | `ChatHub`: API, dispatch, event bridge, notification consumer, per-source consumer |
| `src/core/chat_hub/inbox.rs` | `SourceInbox`, `QueuedMessage`, `build_unit` (coalescing) + tests |
| `src/core/db/sources.rs` | `source → active_session_id` mapping |
| `crates/core-api/src/chat_hub.rs` | `ChatHubApi` trait + `SendMessageOptions` |
| `src/core/session/handler/` | `ChatSessionHandler` — the turn itself (see [session.md](session.md)) |

## When to update this file

- New `ChatHubApi` methods or changed dispatch flow
- Changes to inbox coalescing rules or the debounce constant
- Changes to `/stop` / `/clear` queue semantics
- Implementing mid-turn injection
