# Session & Message Handling

## ChatSessionHandler Fields

| Field | Type | Purpose |
| --- | --- | --- |
| `session_id` | `i64` | DB session identifier |
| `db` | `Arc<SqlitePool>` | Persistent storage |
| `llm_manager` | `Arc<LlmManager>` | Resolves which LLM client to use |
| `max_history_messages` | `usize` | Max messages kept in context when compaction is disabled; ignored when compaction is configured |
| `max_tool_rounds` | `usize` | Max LLM rounds per turn before `Exhausted` |
| `max_tool_result_chars` | `Option<usize>` | When set, tool results from previous turns that exceed this char count are replaced with a placeholder in the LLM context. DB content is unchanged. See [Tool Result Hiding](#tool-result-hiding). |
| `agent_id` | `String` | Agent owning this session (default: `"main"`) |
| `tools` | `Arc<ToolRegistry>` | Built-in tools |
| `mcp` | `Arc<McpManager>` | MCP tools |
| `approval` | `Arc<ApprovalManager>` | Central approval service (rules + pending registry) |
| `event_bus` | `Arc<ChatEventBus>` | Publishes completed turns (user + assistant) to the in-process event bus |
| `question_registry` | `Arc<Mutex<HashMap<i64, oneshot::Sender<String>>>>` | Pending `ask_user_clarification` channels |
| `processing` | `Mutex<()>` | Prevents concurrent `handle_message` / `resume_turn` calls |
| `cancelled` | `Arc<AtomicBool>` | Set by `cancel()` to abort the tool loop early |
| `weak_self` | `OnceLock<Weak<ChatSessionHandler>>` | Weak back-reference set by `ChatSessionManager` after `Arc` creation; used by `dispatch_call_agent` to obtain `Arc<Self>` for spawning child tasks |

---

## build_agent_config

Private helper called by both `handle_message` and `resume_turn` to avoid duplicating the LLM-resolution and tool-assembly logic.

1. Load `meta.json` for the current `agent_id` (scope, strength).
2. Resolve LLM client key via `LlmManager::resolve(client_name, scope, strength)`.
3. Build `base_tool_defs`: built-in tools + `call_agent` + `update_scratchpad`. **MCP tools are no longer included here** — they are resolved dynamically in `all_tool_defs()` each round based on `active_mcp_grants`.
4. Load session MCP grants from `session_mcp_grants` DB → populate `active_mcp_grants`. If `enabled_mcp_servers` override is provided, merge those names in-memory without touching the DB.
5. Inject `show_mcp_tools` as an `InterfaceTool` (session-scoped, `stack_id = None`). Skipped if `enabled_mcp_servers` override is active.
6. Return `AgentRunConfig { ..., mcp: Arc<McpManager>, active_mcp_grants }`.

---

## handle_message Flow

1. Acquire `processing` mutex (blocks if another message is being processed).
2. Reset `cancelled` flag to `false`.
3. **Memory context** — call `memory_manager.query_context(session_id, user_message)` for **all** sessions (including cron and tic). If a string is returned it is stored as `extra_system_dynamic` — **not** merged into `extra_system_context`. It will be injected as a dynamic tail system message after the conversation history (see *Context Building*). Only the write path filters by `is_interactive`/`is_ephemeral`.
4. Call `build_agent_config(client_name, enabled_mcp_servers, extra_system_static, extra_system_dynamic, interface_tools)` → `AgentRunConfig`. This also calls `memory_manager.tools()` and stores them in `AgentRunConfig::memory_tools`.
5. Get or create the active `chat_sessions_stack` frame.
6. Check for orphaned user message (see below) and mark it `failed` if found.
7. Append the user message to `chat_history` (with `is_synthetic` persisted); capture the returned `user_message_id`.
8. Call `resume_pending_tools(stack_id, &config, &tx)` — re-gates and executes any `pending` tool calls left from an interrupted session.
9. Call `run_agent_turn(stack_id, &config, &tx)` and await outcome.
10. On `Final`: send `Done` event (and `Truncated` if applicable); then publish **two events** to `ChatEventBus` — one `User` event (with `is_synthetic` from the caller) and one `Assistant` event (with all `tool_calls` collected during the turn).
11. On `Cancelled`: send `Error` event ("interrupted by user"). No event bus publication.
12. On `Exhausted`: send `Error` event (tool round limit exceeded). No event bus publication.

`is_synthetic` is a parameter of `handle_message`. It is `true` for TicManager ticks (system-generated messages injected as user turns), `false` for all real user input. Additionally, `ChatHub::notification_consumer` injects synthetic **Assistant** messages with `is_synthetic = true` containing the `read_notification` tool call and reasoning trace — these are not user turns, but share the same flag for UI filtering. The flag is **persisted** to `chat_history.is_synthetic` so that the UI history API (`GET /api/sessions/:id`) can filter those rows out on page reload — synthetic messages never appear in the conversation visible to the user. They are still included in the LLM context (via `build_openai_messages`) so the assistant can see what it previously said in response to a notification.

---

## resume_turn Flow

Called by `ChatHub::resume()` (routed through the global event bus) when the client sends `{"type":"resume"}`. Also called internally by `run_child_frame` after a sub-agent completes, to continue the parent's LLM loop. Continues without appending a new user message.

1. Acquire `processing` mutex.
2. Reset `cancelled` flag to `false`.
3. Call `build_agent_config(...)` → `AgentRunConfig`.
4. Get the active `chat_sessions_stack` frame — if none exists, return immediately.
5. Call `resume_pending_tools(stack_id)`.
6. **Guard**: if no pending tools were found AND the last assistant message has no associated tool calls (pure-text final response), the turn already completed — return immediately. If the last assistant message *does* have tool calls (e.g. a `call_agent` that completed asynchronously), fall through so the LLM can process the results.
7. Call `run_agent_turn(stack_id, &config, &tx)`.
8. If outcome is `WaitingChild` (another async sub-agent was spawned), return immediately — the new child task will drive the cascade.
9. **Cascade loop**: while the current stack has a `parent_tool_call_id`, complete/fail the parent's tool call, terminate the child stack, and run `run_agent_turn` on the parent stack. Repeat until reaching the root (depth = 0) or another `WaitingChild`.
10. At root: same `Final` / `Cancelled` / `Exhausted` handling as `handle_message`.

---

## resume_pending_tools

Called at the start of `handle_message` (and by the REST endpoint after a manual resolve). Finds any `pending` tool calls left from a previous interrupted session, re-runs them through the approval gate, executes approved ones, and fails rejected or denied ones — so `run_agent_turn` sees complete history and can continue cleanly.

Tool dispatch order (same as `run_agent_turn`):

1. MCP tool (name contains `:`).
2. Memory tool (`config.memory_tools`).
3. Built-in tool registry.

`restart` is handled as a special case: it marks the call `done` in the DB before calling `std::process::exit(-1)`.

---

## AgentFlowSignal

`AgentFlowSignal` (`src/core/session/handler/mod.rs`) is a typed `pub(super)` enum used by internal dispatch methods to communicate control-flow outcomes through `anyhow::Error` without sentinel structs:

| Variant | Emitted by | Handled in |
| --- | --- | --- |
| `WaitingChild(i64)` | `dispatch_call_agent` (sub-agent spawned async) | `llm_loop.rs` → returns `TurnOutcome::WaitingChild` |
| `QuestionChannelClosed` | `dispatch_ask_user_clarification` (WS dropped) | `llm_loop.rs` → returns `TurnOutcome::Cancelled`; `resume.rs` → aborts resume |

The dispatcher uses a single `downcast_ref::<AgentFlowSignal>()` + `match` instead of two separate type checks, which is exhaustive and prevents missing a new variant.

---

## run_agent_turn Inner Loop

Called recursively via `Box::pin` to support async recursion without stack overflow.

For each round (up to `max_tool_rounds`):

1. Check `cancelled` flag — return `Cancelled` immediately if set.
2. `build_openai_messages()` — reconstruct full context from DB.
3. Call `llm.client.chat_with_tools(messages, tool_defs, options)`.
4. On `LlmTurn::Message` — persist assistant message, return `Final` (with all `tool_calls` accumulated across rounds).
5. On `LlmTurn::ToolCalls` — for each call:
   - Persist assistant "thinking" message, emit `Thinking` event if non-empty.
   - Record tool call in `chat_llm_tools` (status: `pending`).
   - Emit `ToolStart` event.
   - Run approval gate (see below).
   - Dispatch tool: `call_agent` → `dispatch_call_agent`; `update_scratchpad` → `db::scratchpad::upsert`; `ask_user_clarification` → emit `AgentQuestion`, await answer; MCP tool → `McpManager`; interface tool → closure in `AgentRunConfig`; otherwise → `ToolRegistry`.
   - On success: `ToolDone` event, status → `done`.
   - On error: `ToolError` event, status → `failed`.
6. Loop back — next round rebuilds context with tool results included.
7. If all rounds exhausted: return `Exhausted`.

---

## Approval Gate

The gate is `ApprovalManager.check(session_id, category, agent_id, source, tool_name, args)` → `GateResult`.

**Evaluation order:**

1. Hardcoded exception: file-write tools targeting a path that starts with `memory/` → `Allow` (always auto-approved).
2. Rules from the `approval_rules` table, sorted by `priority ASC` (lower = evaluated first). First match wins.
3. **Session bypass** (in-memory, not persisted): if the result would be `Require` and an active bypass exists for this `session_id` whose `category` matches (or is `None` for all categories), convert to `Allow`. `Deny` is never bypassed.
4. No match → `Allow` (default-open policy).

**Default rules** (seeded at startup if the table is empty):
`execute_cmd`, `restart`, `write_file`, `edit_file`, `insert_at_line`, `replace_lines` → `require`

**Session bypass** is activated by the LLM via the three `approval_bypass_*` interface tools (injected in `build_agent_config`):

| Tool | Effect |
| --- | --- |
| `approval_bypass_session` | Bypasses all `Require` for the rest of this session (no expiry) |
| `approval_bypass_timed(minutes)` | Bypasses all `Require` for N minutes (default 10, max 120) |
| `approval_bypass_category(category, minutes)` | Bypasses `Require` for one `ToolCategory` for N minutes |

The bypass state lives in `ApprovalManager::session_bypasses` (`Mutex<HashMap<i64, Vec<CategoryBypass>>>`). Expired entries are pruned lazily on each `check()` call. All entries for a session are cleared when `cancel_for_session()` is called (WS disconnect). The state is **never persisted** — it is reset on app restart.

**GateResult handling in `run_agent_turn`:**

- `Allow` — execute freely.
- `Deny` — mark tool call `failed`, emit `ToolError`, continue loop.
- `Require` — pause and ask the human:
  1. Register a `oneshot` channel via `ApprovalManager.register(...)` → `(request_id, rx)`.
  2. Call `emit_approval_event(tx, request_id, tool_call_id, name, args)` which selects the event type:
     - **file-write tools** (`write_file`, `edit_file`, `insert_at_line`, `replace_lines`): read current file + compute predicted result concurrently → `PendingWrite { old_content, new_content }`. Falls back to `ApprovalRequired` if the diff cannot be computed.
     - **`execute_cmd`**: `PendingWrite` with `path = "$ execute_cmd"`, `new_content = "$ <command>"`.
     - **`restart`**: `PendingWrite` with restart description.
     - **everything else**: `ApprovalRequired { tool_name, arguments }`.
  3. Await `rx`.
  4. `Approved` → proceed with tool execution.
  5. `Rejected { note }` → mark tool call `failed`, emit `ToolError`, continue loop.
  6. Channel closed (WS disconnected) → return `Cancelled`.

---

## MessageBuilder

`build_openai_messages` is now a thin wrapper that delegates to `MessageBuilder` (`src/core/session/handler/message_builder.rs`). `MessageBuilder` is a self-contained struct with no reference to `ChatSessionHandler`:

```rust
pub struct MessageBuilder {
    pub pool:                  Arc<SqlitePool>,
    pub session_id:            i64,
    pub mcp:                   Arc<McpManager>,
    pub datetime_config:       DatetimeConfig,
    pub max_history_messages:  usize,
    pub max_tool_result_chars: Option<usize>,
    pub compactor:             Option<Arc<ContextCompactor>>,
}
```

This allows the message-building logic to be tested in isolation with an in-memory SQLite database (no full `ChatSessionHandler` required). `ChatSessionHandler::build_openai_messages` constructs a `MessageBuilder` from its own fields and delegates.

---

## Context Building

`build_openai_messages` (backed by `MessageBuilder::build`) assembles the message array in the following order, optimised for prefix KV caching:

### 1. Static system message

Contents: AGENT.md + `inject_memory` files + `extra_system_static` (e.g. Telegram format rules) + MCP list.

When `cache_hints = true` (Anthropic models via OpenRouter), the content is wrapped in a `cache_control: ephemeral` block so the provider caches it as a KV prefix. For all other providers this message is a plain string that never changes turn-to-turn, so the provider's own automatic prefix cache (if any) hits on it.

### 2. Scratchpad system message *(if non-empty)*

The session scratchpad emitted as a separate `[system]` message **before** the conversation. Kept isolated from the static block so a mid-turn `update_scratchpad` call only invalidates this small message, not the large cacheable prefix.

### 3. Compaction summary system message *(if present)*

See *Context Compaction*.

### 4. Conversation history

`chat_history` for the stack. When compaction is **disabled**, the list is truncated to `max_history_messages` (oldest dropped first). When compaction is **enabled**, `max_history_messages` has no effect — the compactor owns the token budget and truncating by count would silently discard history that should be summarised instead. Each assistant entry with tool calls in `chat_llm_tools` is reconstructed with a `tool_calls` array and one `tool` result message per call. Tool result hiding (see below) is applied to results from previous turns.

### 5. Dynamic tail system message

Contains `extra_system_dynamic` (e.g. Honcho long-term memories, retrieved fresh each turn) followed by current date/time, OS, and working directory. Placed **after** the conversation so the stable prefix (messages 1–4) is never invalidated by per-turn changes. The model's recency-biased attention also ensures it reads fresh user context immediately before generating its response.

### 6. Tail reminder system message *(if provided)*

Short anti-drift reminder (e.g. Telegram HTML format rules) at the very end.

---

## Tool Result Hiding

Controlled by `max_tool_result_chars` in `config.yml` (`llm.max_tool_result_chars`).

When set, `build_openai_messages` calls `maybe_hide_tool_result` for every tool result it reconstructs. The replacement happens **only when all three conditions hold**:

1. The result belongs to a **previous turn** — i.e. the assistant message that produced it appears before the last user/agent message in the (truncated) history.
2. `max_tool_result_chars` is `Some(n)`.
3. The result string exceeds `n` characters.

When all three are true, the content sent to the LLM is replaced with:

```text
[Tool response for `<tool_name>` hidden: response was N chars, exceeding the L-char limit. Call the tool again if you need this information.]
```

**What is never affected:**

- The database row — always retains the original content.
- The frontend — always displays the full result.
- Tool results from the **current turn** — always shown in full, regardless of size, so the LLM can work with them within the same turn.

**Current-turn boundary detection:** the last `User` or `Agent` role entry in the truncated history marks the start of the current turn. Any assistant message at a lower index is from a previous turn.

### Scratchpad injection format

```xml
<scratchpad>
  <!-- Temporary notes shared by all agents in this chat session. Not persisted across sessions. -->
  <note key="db_url">postgres://localhost/mydb</note>
  <note key="main_struct">src/session/handler/mod.rs</note>
</scratchpad>
```

Only injected when the `session_scratchpad` table has at least one row for the session.

---

## TurnOutcome Enum

| Variant | Meaning |
| --- | --- |
| `Final { content, message_id, input_tokens, output_tokens, truncated, tool_calls }` | LLM produced a final text response; `tool_calls` carries all `ToolCallEvent`s from all rounds |
| `Cancelled` | `cancelled` flag was set or WS closed during approval |
| `Exhausted` | All `max_tool_rounds` used without a final message |
| `WaitingChild { child_stack_id }` | A sub-agent was spawned asynchronously; the child task will complete the parent's tool call and resume the parent when done |

---

## Concurrency Constraint

Only one `handle_message` / `resume_turn` call can run per `ChatSessionHandler` at a time. The `processing: Mutex<()>` is held for the entire duration. A second call blocks until the first completes or is cancelled.

Sub-agents run as independent `tokio::spawn` tasks but acquire the **same** `processing` mutex. Since the parent exits (releasing the lock) before the child acquires it, parent and child never run concurrently within the same session.

---

## Orphaned Message Handling

If the last message in history has `role = User` or `role = Agent` (no following assistant message), the previous turn was cancelled before the LLM responded. That message is marked `status = failed` and excluded from the context sent to the LLM, preventing user→assistant alternation errors.

---

## AgentRunConfig

Built once per `handle_message` call and passed by reference through the entire agent/sub-agent recursion.

| Field | Purpose |
| --- | --- |
| `agent_id` | ID of the current agent |
| `client_name` | Resolved LLM client key |
| `depth` | Recursion depth: 0 = root, 1+ = sub-agent |
| `base_tool_defs` | Built-in tool definitions only (no MCP — those come from `all_tool_defs()` dynamically) |
| `extra_system` | Optional extra system context (set to `None` for sub-agents) |
| `interface_tools` | Interface-specific tools. For sub-agents contains only `show_mcp_tools`; all other interface tools are dropped |
| `memory_tools` | Memory backend tools (inherited by sub-agents) |
| `mcp` | `Arc<McpManager>` — used by `all_tool_defs()` to resolve MCP tools dynamically |
| `active_mcp_grants` | `Arc<RwLock<HashSet<String>>>` — MCP servers currently granted. Re-read on every round so `show_mcp_tools` in round N makes tools visible in round N+1. Root: session-scoped (from `session_mcp_grants` DB). Sub-agents: stack-scoped (from `stack_mcp_grants` DB), starts empty |

### `all_tool_defs()` — dynamic MCP resolution

Called on every LLM round. Returns `base_tool_defs` + MCP tools for currently-granted servers (re-queried from `McpManager` using `active_mcp_grants`) + memory tools + interface tools.

This means that calling `show_mcp_tools` in round N makes those tools available to the LLM starting from round N+1 of the **same turn** — no cross-turn delay.

### `for_sub_agent()`

Derives a child config: inherits `base_tool_defs`, `memory_tools`, and `mcp`; starts with **empty** `active_mcp_grants`; clears `interface_tools`; increments `depth`.

`dispatch_call_agent` then:

1. Replaces the empty `active_mcp_grants` arc with one pre-populated from `stack_mcp_grants` DB (restart recovery).
2. Appends `sub_agents_only` tools and `ask_user_clarification` to `base_tool_defs`.
3. Injects `show_mcp_tools` (stack-scoped, `stack_id = Some(child.id)`) as the only interface tool.

---

## ask_user_clarification Flow

Available only to sub-agents (depth ≥ 1).

1. Sub-agent calls `ask_user_clarification(question)`.
2. `run_agent_turn` intercepts it before ToolRegistry dispatch.
3. A `oneshot` channel is registered in `question_registry` keyed by `request_id`.
4. `AgentQuestion { request_id, question }` event is emitted to the frontend.
5. Execution is suspended until the client sends `{"type":"answer_question","request_id":<N>,"answer":"..."}`.
6. `resolve_question()` unblocks the channel; the answer is returned as the tool result.
7. On WS disconnect: `cancel_pending_questions()` drops all senders, causing the await to return `Err`, which propagates as a tool error.

---

## WS Resume Event Routing

When the client sends `{"type":"resume"}`, the WS handler calls `ChatHub::resume(&source)` which:

1. Finds the session handler for `source`.
2. Spawns a task running `handler.resume_turn(...)` with an mpsc sender.
3. Bridges every event from that sender to the **global broadcast bus** (tagged with the session's `source`).

All WS connections for the same source (including newly reconnected ones) receive the events via their global bus subscription. This avoids the previous design where events went to a local mpsc channel and were silently lost if the client reconnected while `resume_turn` was in flight.

---

## When to Update This File

- `needs_approval()` rules change (new tool added, path exemption modified)
- The tool-calling loop gains new behavior (new event type, new cancellation path)
- `build_openai_messages` changes (new context injected, truncation logic modified)
- `AgentRunConfig` fields change
- `build_agent_config` changes (new default tool added, resolution logic modified)
