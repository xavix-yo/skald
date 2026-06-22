# Approval Gate (Human-in-the-Loop)

## Overview

`ApprovalManager` is a top-level service (in `Skald`) that intercepts every tool call before execution and decides whether to:

- **Allow** — execute freely (no matching rule, or an explicit `allow` rule)
- **Deny** — block immediately (`deny` rule)
- **Require** — suspend and ask the user for confirmation

It is designed to be extensible: multiple notification channels (web, Telegram), granular policies per agent/source/tool, and future support for resuming interrupted sessions.

---

## Architecture

```
llm_loop.rs
  └─► ApprovalManager.check(session_id, category, agent_id, source, tool_name, args)
        │
        ├─ GateResult::Allow  → execute immediately
        ├─ GateResult::Deny   → fail tool call (not bypassable)
        └─ GateResult::Require
              ├─ (session bypass active?) → GateResult::Allow → execute immediately
              └─► ApprovalManager.register(...)  → (request_id, rx)
                    │  emits ServerEvent::PendingWrite or ApprovalRequired
                    └─► await rx  ← resolved by WS/Telegram via resolve(request_id, decision)
```

`ApprovalManager` lives in `src/core/approval/mod.rs` and is independent of `ChatSessionManager`.

---

## Permission Groups and RunContext

Rules are scoped to **permission groups** (`tool_permission_groups` table). A session's active **RunContext** references a group via its `security_group` field; rules in that group take precedence over rules in the `"default"` group.

**For full RunContext documentation** (fields, resolution, API, project integration) — see [../session/run-context.md](../session/run-context.md) (source of truth).

The `"default"` group is seeded automatically at startup and **cannot be deleted**. Its rules can be freely edited.

---

## Rules

Rules are stored in SQLite in the `approval_rules` table and evaluated in `priority ASC` order (lower number = evaluated first). The first matching rule determines the action. If no rule matches, the fallback is `Require` (default-closed policy).

The **Default** group has a seeded `allow * priority=9999` catch-all rule so standard sessions remain permissive out of the box. Groups created without such a rule are automatically restrictive — any tool not explicitly allowed or denied will require human approval.

### Table Schema

| Column | Type | Description |
| ------ | ---- | ----------- |
| `id` | INTEGER | PK |
| `agent_id` | TEXT (nullable) | Filter on a specific agent. `NULL` = all |
| `source` | TEXT (nullable) | Filter on source: `web`, `telegram`, `cron`. `NULL` = all |
| `tool_pattern` | TEXT | Exact name or glob with `*` suffix (e.g. `mcp__gmail__*`) |
| `path_pattern` | TEXT (nullable) | Glob on the normalised file path (e.g. `data/*`). `NULL` = no path filter |
| `action` | TEXT | `require` \| `allow` \| `deny` |
| `note` | TEXT (nullable) | Descriptive note |
| `priority` | INTEGER | Evaluation order (default 100; system defaults use 10) |
| `group_id` | TEXT | Permission group this rule belongs to (default: `"default"`) |

### Pattern Matching

| Pattern | Matches |
| ------- | ------- |
| `execute_cmd` | only `execute_cmd` |
| `mcp__gmail__*` | all tools from the `gmail` server |
| `mcp__*` | all MCP tools |
| `*` | any tool |

The `path_pattern` field uses the same glob logic, applied to the normalised path (`args["path"]` without leading `/` or `./`). If `path_pattern` is set but the tool has no `path` argument, the rule **does not** match.

### Evaluation Order

1. **RunContext `allow_fs_writes` pre-check** (in `llm_loop.rs`, before `ApprovalManager`): if the tool is a file-write tool and the path matches any entry in `RunContext.allow_fs_writes`, the call is immediately allowed — `ApprovalManager.check()` is not called at all.
2. Hardcoded exception: file-write targeting `memory/` → always `Allow`
3. DB rules for the session's group, then `"default"` group as fallback — sorted by `priority ASC, id ASC` within each tier — first matching rule wins
4. **Session bypass** (in-memory): if the result would be `Require` and an active bypass matches `session_id` + `category`, convert to `Allow`. `Deny` is never bypassed.
5. No matching rule → `Require` (default-closed)

### Path Whitelist

There are two ways to pre-authorize writes to a directory:

**Option A — RunContext `allow_fs_writes`** (session-scoped, no DB rule needed):

Set `allow_fs_writes` on the session's `RunContext`. The pre-check fires in `llm_loop.rs` before `ApprovalManager`, so no approval event is emitted at all.

```json
{
  "security_group": "cron_restrictive",
  "allow_fs_writes": ["data/output", "/abs/path/to/dir"]
}
```

Matching semantics: exact file OR recursive directory prefix (no wildcards). `"data/output"` matches `data/output/foo.txt`, `data/output/sub/bar.txt`, etc. Entries can be absolute or relative to the session's `working_directory`.

**Option B — approval_rules DB** (persistent, applies to all sessions in the group):

Add an `allow` rule at a low priority (e.g. 5, before the generic `require` at priority 10):

```sql
INSERT INTO approval_rules (tool_pattern, path_pattern, action, note, priority)
VALUES ('write_file',     'data/*', 'allow', 'auto-allow data/ writes', 5);
INSERT INTO approval_rules (tool_pattern, path_pattern, action, note, priority)
VALUES ('edit_file',      'data/*', 'allow', 'auto-allow data/ writes', 5);
INSERT INTO approval_rules (tool_pattern, path_pattern, action, note, priority)
VALUES ('insert_at_line', 'data/*', 'allow', 'auto-allow data/ writes', 5);
INSERT INTO approval_rules (tool_pattern, path_pattern, action, note, priority)
VALUES ('replace_lines',  'data/*', 'allow', 'auto-allow data/ writes', 5);
```

These rules are inserted automatically on first startup by `seed_data_path_rules()`.

### Default Rules (seeded automatically on first startup with empty DB)

| Tool | Action | Priority |
|------|--------|----------|
| `execute_cmd` | require | 10 |
| `restart` | require | 10 |
| `write_file` | require | 10 |
| `edit_file` | require | 10 |
| `insert_at_line` | require | 10 |
| `replace_lines` | require | 10 |

Default rules are inserted only when the `approval_rules` table is empty. They can be modified or deleted normally.

### Hardcoded Exception

File-writes targeting `memory/` are always auto-approved, regardless of rules. This allows the LLM to manage its own memory autonomously.

---

## Useful Rule Examples

### Require approval for all Gmail tools

```sql
INSERT INTO approval_rules (tool_pattern, action, note, priority)
VALUES ('mcp__gmail__*', 'require', 'Gmail requires approval', 5);
```

### Require approval only for cron jobs (not for web)

```sql
INSERT INTO approval_rules (source, tool_pattern, action, note, priority)
VALUES ('cron', 'mcp__*', 'require', 'All MCP tools from cron require approval', 20);
```

### Always allow a specific tool for a specific agent

```sql
INSERT INTO approval_rules (agent_id, tool_pattern, action, note, priority)
VALUES ('email-assistant', 'mcp__gmail__list_messages', 'allow', 'free read for email-assistant', 1);
```

### Allow free writes to a specific subfolder

```sql
-- For the researcher agent only, allow writes to data/research/ without approval
INSERT INTO approval_rules (agent_id, tool_pattern, path_pattern, action, note, priority)
VALUES ('researcher', 'write_file', 'data/research/*', 'allow', 'researcher writes freely to data/research/', 3);
```

---

## Session Bypass (Temporary Allow-All)

The human can temporarily suppress approval prompts for a session without modifying DB rules. The bypass is **in-memory only** — it disappears on app restart or when the session ends.

### Activation

The bypass is activated by the **human** (not the LLM) from any of these surfaces:

- **Agent Inbox** page (REST `/api/inbox/approvals/:id/resolve` with `bypass_secs`)
- **Copilot chat** (WebSocket `approve_write`/`approve_tool` with `bypass_secs` field)
- **Telegram bot** inline keyboard (⏱ 15 min / 🔄 Session buttons → `ApprovalApi::approve_with_bypass`)

The LLM has no tools to activate it — giving the LLM the ability to disable its own oversight would defeat the purpose of the gate.

### Scope

Each bypass entry targets a specific `BypassScope`:

| Scope | What it covers |
| ----- | -------------- |
| `All` | Every tool regardless of category |
| `Category(ToolCategory)` | Only tools with the given registered category (e.g. `Filesystem`, `Shell`) |
| `McpServer(String)` | Only tools from the named MCP server (matched by the `mcp__<server>__` prefix in the tool name) |

A bypass entry also has an optional expiry (`expires_at: Option<Instant>`). `None` means indefinite (session-scoped).

### How It Works

`ApprovalManager` holds `session_bypasses: Mutex<HashMap<i64, Vec<ApprovalBypass>>>`. `check()` receives `session_id`, `category`, and `tool_name`. After rule evaluation, if the result is `Require` and a matching active bypass exists, the result is converted to `Allow`. Expired entries are pruned lazily on each `check()` call.

### Invariants

- `Deny` rules are **never** bypassable.
- The bypass state is cleared when `cancel_for_session()` is called (WS disconnect).
- Multiple bypasses can coexist for the same session (e.g. "all categories: 30 min" + "filesystem: indefinite").
- MCP tools match `McpServer` scope; they are also covered by `All` scope.

### Rust API

```rust
approval.bypass_session(session_id).await;                                         // indefinite, all
approval.bypass_session_for(session_id, Duration::from_secs(600)).await;           // 10 min, all
approval.bypass_session_for_category(session_id, ToolCategory::Shell, Some(Duration::from_secs(600))).await;
approval.bypass_session_for_mcp(session_id, "gmail".into(), Some(Duration::from_secs(1800))).await;
approval.clear_session_bypass(session_id).await;
```

---

## Session Sources (`source`)

| Value | When |
| ----- | ---- |
| `web` | Chat from the web UI |
| `telegram` | Chat from the Telegram bot |
| `cron` | Trigger from scheduled_jobs |

Headless sessions (cron) have no active interface: approval requests are registered as pending and the agent suspends until a response arrives (via web or Telegram).

---

## Pending Approvals

All pending requests are accessible via `Inbox.list_pending()` (which internally calls `ApprovalManager.list_pending()` and `ClarificationManager.list_pending()`), exposed by the `GET /api/inbox` endpoint, and displayed on the **Agent Inbox** frontend page.

Each entry contains:

| Field | Type | Description |
| ----- | ---- | ----------- |
| `request_id` | i64 | Unique ID for resolution |
| `session_id` | i64 | Session that generated the request |
| `tool_call_id` | i64 | Tool call in the DB |
| `tool_name` | String | Name of the tool to execute |
| `arguments` | JSON | Full arguments |
| `agent_id` | String | Agent that called the tool |
| `source` | String | Session source |
| `context_label` | Option\<String\> | Human-readable origin label (e.g. `"CronJob: Daily Digest"`) |
| `created_at` | String | ISO-8601 timestamp |
| `tool_category` | Option\<String\> | Registered tool category (`filesystem`, `shell`, …); `null` for MCP/unknown tools |
| `mcp_server` | Option\<String\> | MCP server name extracted from the tool name (e.g. `"gmail"`); `null` for non-MCP tools |

`context_label` is set by `ChatSessionHandler::set_context_label()` before the run (e.g. `TaskManager` sets `"CronJob: <title>"`). It is read in `llm_loop.rs` and `resume.rs` and passed to `approval.register()`.

---

## Inbox bus events (`GlobalEvent`)

Inbox lifecycle changes are broadcast on the global `GlobalEvent` bus so any subscriber (Telegram, the mobile-connector plugin) can react without polling. Plugins subscribe via `ctx.chat_hub.events(...)`. Four events cover the full Inbox cycle:

| Event (`ServerEvent`) | Emitted by | When |
| --- | --- | --- |
| `ApprovalRequested { request_id, tool_call_id, tool_name }` | `ApprovalManager::register` | A tool call is gated and enters the Inbox |
| `ApprovalResolved { request_id, tool_call_id, approved }` | `ApprovalManager::resolve` **and** `resolve_for_tool_call` | An approval is approved/rejected (from any surface: Inbox REST, WS, mobile, or the inline copilot card) |
| `ClarificationRequested { request_id, title }` | `ClarificationManager::register` | A clarification question enters the Inbox |
| `ClarificationResolved { request_id }` | `ClarificationManager::resolve` | A clarification is answered |

These are distinct from the per-session WS events `ApprovalRequired` (carries full args for the active client) and `AgentQuestion` (the interactive clarification prompt). The `ClarificationManager` now holds a `broadcast::Sender<GlobalEvent>` injected from `Skald::new` (same `event_tx` the `ApprovalManager` uses), mirroring the approval manager.

---

## Agent Inbox

The **Agent Inbox** is the unified web page for managing all pending requests from background sessions (cron, etc.):

- **Approval requests** — tool calls requiring human confirmation (e.g. `execute_cmd`, `write_file`)
- **Clarification requests** — questions posed by the agent via `ask_user_clarification` when it cannot proceed autonomously

### REST API

| Method | Endpoint | Description |
| ------ | -------- | ----------- |
| `GET` | `/api/inbox` | Returns `{ total, approvals, clarifications }` |
| `POST` | `/api/inbox/approvals/:request_id/resolve` | Resolve an approval (see body below) |
| `POST` | `/api/inbox/clarifications/:request_id/resolve` | Body: `{ answer: string }` |

**Resolve approval body:**

```json
{
  "action": "approve" | "reject",
  "note": "",
  "bypass_secs": 900,
  "bypass_scope": "category" | "mcp_server" | "all"
}
```

`bypass_secs` and `bypass_scope` are optional. When present (only on `approve`):

- `bypass_secs = 0` → indefinite bypass (until WS disconnect)
- `bypass_secs = N` → bypass expires after N seconds
- `bypass_scope` defaults to `"category"` if `tool_category` is set, `"mcp_server"` if `mcp_server` is set, otherwise `"all"`

The legacy endpoints `/api/approval/pending` and `/api/approval/resolve/:id` remain active for backwards compatibility.

### Frontend

The page is implemented in `web/components/agent-inbox.js` (`<agent-inbox-page>`). Polls every 8 s when open. The red badge in the sidebar (independent polling every 10 s) shows the total pending count.

See [../frontend.md](../frontend.md) for component details.

---

## Resolution

### From WebSocket (web copilot)

The client sends a JSON message:

```json
{ "type": "approve_tool", "request_id": 42 }
{ "type": "reject_tool",  "request_id": 42, "note": "optional reason" }
```

**Bypass via WebSocket** — include `bypass_secs` on any approve message:

```json
{ "type": "approve_tool", "request_id": 42, "bypass_secs": 900 }   // 15-min bypass
{ "type": "approve_tool", "request_id": 42, "bypass_secs": 0   }   // session bypass (indefinite)
```

`bypass_secs = 0` maps to an indefinite bypass (until session ends); positive values are seconds. The scope (category / MCP server / all) is auto-detected from the pending request, same as the REST endpoint.

The types `approve_write`/`reject_write` are aliases for `approve_tool`/`reject_tool` and work identically.

### From Telegram

The Telegram plugin uses `ApprovalApi::approve_with_bypass` (defined in `crates/core-api/src/approval.rs`, implemented on `ApprovalManager`). The inline keyboard shows four buttons in two rows:

```text
[✅ Approve]  [❌ Reject]
[⏱ 15 min]   [🔄 Session]
```

Tapping **⏱ 15 min** → `approve_with_bypass(request_id, Some(900))`.
Tapping **🔄 Session** → `approve_with_bypass(request_id, None)`.

`approve_with_bypass` calls `ApprovalManager::approve()` then registers the appropriate session bypass (auto-detected scope).

---

## Behaviour on Restart

Approval requests are in-memory. On app restart:

- Pending approvals are lost
- Tool calls in `pending` state in the DB are shown to the LLM as "interrupted, please retry"
- The LLM re-calls the tools → they pass through the gate again → a new approval request is generated

This is the current behaviour. Future work may add persistence of pending approvals in SQLite to support transparent resumption.

---

## Tool Visibility Filtering

Beyond the execution-time approval gate, tools are filtered at **invitation time** — before being included in the LLM context. This reduces token usage and prevents the LLM from attempting to call tools it cannot execute.

### Semantics

`ApprovalManager.is_tool_visible(rules, tool_name)` checks the pre-loaded rules synchronously:

- If the first matching rule has action `Deny` → tool is hidden from the LLM
- All other cases (Allow, Require, or no match) → tool is visible

Only `tool_pattern` is considered (path/agent/source filters are ignored for visibility — those are execution-time concerns).

### Where it runs

1. **Parent agent** (`src/core/session/handler/config.rs`, `build_agent_config`): rules are loaded once with `list_for_group`, then `base_tool_defs.retain(...)` filters the list before building `AgentRunConfig`.
2. **Sub-agents** (`src/core/session/handler/agent_dispatch.rs`, `dispatch_sub_agent`): same filter applied after sub-agent-only tools are added.

Sub-agents share the parent session's permission group. The execution-time `ApprovalManager.check()` gate remains active as a second enforcement layer.

### Tool Visibility API

```rust
// Sync: applied to pre-loaded rules slice
approval.is_tool_visible(rules: &[ApprovalRule], tool_name: &str) -> bool

// Async: one DB round-trip, returns the matched RuleAction (or None if no rule matches)
approval.check_tool_visibility(group_id: &str, tool_name: &str) -> Option<RuleAction>

// Via RunContextManager (resolves group_id from run_context_id automatically)
run_context_manager.check_tool_visibility(run_context_id: Option<&str>, tool_name: &str) -> Option<RuleAction>
```

---

## Group Duplication

`POST /api/tool-permission-groups/{id}/duplicate`

Body: `{ "id": "<new_group_id>", "name": "<new display name>" }`

Creates a new permission group that is an exact copy of the source group's rules. The operation is atomic: the new group row and all copied rules are inserted in a single SQLite transaction. The new group inherits the source's `description`.

Implemented in `RunContextManager::duplicate_group` (`src/core/run_context/mod.rs`).

---

## AllTools Response (`GET /api/approval/tools`)

The endpoint returns `AllTools`:

```json
{
  "built_in": [ { "name": "read_file", "description": "...", "source": "built-in", "server": null } ],
  "mcp": [ { "name": "mcp__gmail__list_messages", "description": "...", "source": "mcp", "server": "gmail" } ],
  "mcp_servers": {
    "gmail": { "friendly_name": "Gmail", "description": "Read and send Gmail messages" }
  }
}
```

`mcp_servers` is keyed by the MCP server's internal name (matching `server` fields in `mcp` entries). The frontend uses it to group MCP tools under their server's `friendly_name` and display the server `description` as a section subtitle.

---

## Module Structure

| File | Role |
| ---- | ---- |
| `crates/core-api/src/approval.rs` | `ApprovalApi` trait — `approve`, `reject`, `approve_with_bypass`; exposed to plugins via `PluginContext` |
| `src/core/approval/mod.rs` | `ApprovalManager`, `GateResult`, `ApprovalRule`, `PendingApprovalInfo`, `CategoryBypass`, session bypass methods; `is_tool_visible` (sync); `check_tool_visibility` (async); `impl ApprovalApi` |
| `src/core/clarification/mod.rs` | `ClarificationManager`, `PendingClarificationInfo` |
| `src/core/inbox.rs` | `Inbox`: unified façade for pending approvals + clarifications (wraps ApprovalManager, ClarificationManager, ChatHub) |
| `src/core/run_context/mod.rs` | `RunContext` domain object: fields `security_group`, `system_prompt`, `allow_fs_writes`, `working_directory` + applicative methods `tool_group_id()`, `extra_system_prompt()`, `effective_working_dir()`, `is_write_allowed()`. `RunContextManager`: CRUD for permission groups; `duplicate_group` (atomic); `check_tool_visibility`. |
| `src/core/db/approval_rules.rs` | SQLite queries: list, insert, update, delete |
| `src/core/db/mod.rs` | `approval_rules` table creation |
| `src/core/session/handler/config.rs` | Loads rules once with `list_for_group`, calls `approval.is_tool_visible` to filter `base_tool_defs` for the parent agent |
| `src/core/session/handler/agent_dispatch.rs` | Same visibility filter applied to sub-agent `base_tool_defs` after sub-agent-only tools are added |
| `src/core/session/handler/llm_loop.rs` | Resolves `category` via `ToolRegistry::category_of`, calls `approval.check(session_id, category, ...)` + `approval.register()` |
| `src/core/session/handler/resume.rs` | Same `check()` call as `llm_loop.rs` for pending tool re-gating |
| `src/core/session/handler/mod.rs` | `ChatSessionHandler` holds `Arc<ApprovalManager>`, `Arc<ClarificationManager>`, `context_label: RwLock<Option<String>>` |
| `src/frontend/api/inbox.rs` | `/api/inbox` endpoint + resolve for approval and clarification (uses `skald.inbox`) |
| `src/frontend/api/approval.rs` | Approval rules CRUD + `/api/approval/pending` + `/api/approval/tools` (returns `AllTools` with `mcp_servers` metadata map) |
| `src/frontend/api/run_context.rs` | `POST /api/tool-permission-groups/{id}/duplicate` handler |
| `web/components/approval-groups.js` | Groups list page (`<approval-groups-page>`): create, rename, duplicate, delete groups; fires `approval-navigate` event |
| `web/components/approval-rules.js` | Per-group rules view (`<approval-rules-page>`): rule matrix + override/low-priority panels + default action bar; listens to `approval-navigate` |
| `src/frontend/api/ws.rs` | Handles `approve_tool`/`reject_tool`/`approve_write`/`reject_write`; optional `bypass_secs` field activates `approve_with_bypass` |
| `src/core/events.rs` | `ServerEvent::ApprovalRequired` (generic tools) and `PendingWrite` (files with diff) |

---

## Frontend — Approval Rules page

The UI is split into two Lit components that communicate via the `approval-navigate` custom event (see [frontend.md](frontend.md) for the event protocol).

**`<approval-groups-page>`** (`web/components/approval-groups.js`): lists all `tool_permission_groups`. Each group card shows its name, description, and rule count. Groups can be added, renamed, duplicated, or deleted; the `"default"` group cannot be deleted. Clicking a group fires `approval-navigate` with the group object and hides itself.

**`<approval-rules-page>`** (`web/components/approval-rules.js`): per-group rules view with four panels:

| Panel | Priority range | Purpose |
| --- | --- | --- |
| Overrides | `< 0` | Wildcard/path rules evaluated before any per-tool entry |
| Per-tool matrix | `= 0` | Simple 4-chip toggle (—/Allow/Require/Deny) per tool, grouped by category/MCP server |
| Low Priority | `1…999998` | Wildcard/path rules as a safety net, evaluated after the matrix |
| Default Action | `999999` | Catch-all `*` rule with no filters; inline selector; missing = no catch-all |

MCP tools are grouped under their server's `friendly_name` (from `mcp_servers` in the `GET /api/approval/tools` response). The server `description` is shown as a subtitle.

The **Agent Profiles** page (`web/components/agent-profiles.js`, `<agent-profiles-page>`) is a separate sidebar entry that manages `run_contexts`. Each profile links a session to a permission group via a dropdown. The `"default"` profile cannot be deleted. See [../session.md](../session.md) for the resolution chain.

---

## When to Update This File

- New action types in rules
- New notification channel added (e.g. Telegram)
- Pending approval persistence added to DB
- New fields in `PendingApprovalInfo` or `PendingClarificationInfo`
- New Agent Inbox APIs
