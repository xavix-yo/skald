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

## Rules

Rules are stored in SQLite in the `approval_rules` table and evaluated in `priority ASC` order (lower number = evaluated first). The first matching rule determines the action. If no rule matches, the default is `Allow`.

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

### Pattern Matching

| Pattern | Matches |
| ------- | ------- |
| `execute_cmd` | only `execute_cmd` |
| `mcp__gmail__*` | all tools from the `gmail` server |
| `mcp__*` | all MCP tools |
| `*` | any tool |

The `path_pattern` field uses the same glob logic, applied to the normalised path (`args["path"]` without leading `/` or `./`). If `path_pattern` is set but the tool has no `path` argument, the rule **does not** match.

### Evaluation Order

1. Hardcoded exception: file-write targeting `memory/` → always `Allow`
2. DB rules in `priority ASC, id ASC` order — first matching rule wins
3. **Session bypass** (in-memory): if the result would be `Require` and an active bypass matches `session_id` + `category`, convert to `Allow`. `Deny` is never bypassed.
4. No matching rule → `Allow` (default-open)

### Path Whitelist (e.g. `data/`)

To let the LLM write freely to a folder without requiring approval, add an `allow` rule at a low priority (e.g. 5, before the generic `require` at priority 10):

```sql
-- Allow free writes to data/ for all file-write tools
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

The LLM can temporarily suppress approval prompts for a session without modifying DB rules. The bypass is **in-memory only** — it disappears on app restart and when the session ends.

### Activation

The bypass is activated by the **human** (not the LLM) via the REST API (see below). The LLM does not have tools to activate it — giving the LLM the ability to disable its own oversight would defeat the purpose of the gate.

### How It Works

`ApprovalManager` holds a `session_bypasses: Mutex<HashMap<i64, Vec<CategoryBypass>>>` field. Each `CategoryBypass` entry has:

- `category: Option<ToolCategory>` — `None` = all categories; `Some(c)` = only tools of that category
- `expires_at: Option<Instant>` — `None` = no expiry; `Some(t)` = expires at instant `t`

`check()` receives `session_id` and `category` (resolved by `ToolRegistry::category_of(tool_name)` in the caller). After rule evaluation, if the result is `Require` and a matching active bypass exists, the result is converted to `Allow`. Expired entries are pruned lazily on each `check()` call.

### Invariants

- `Deny` rules are **never** bypassable.
- The bypass state is cleared when `cancel_for_session()` is called (WS disconnect).
- Multiple bypasses can coexist for the same session (e.g. "all categories: 30 min" + "filesystem: indefinite").
- MCP and interface tools have `category = None` (not in `ToolRegistry`) — they are only bypassed by an all-category bypass, not by a category-specific one.

### API

```rust
approval.bypass_session(session_id).await;                                  // indefinite, all categories
approval.bypass_session_for(session_id, Duration::from_secs(600)).await;    // 10 min, all categories
approval.bypass_session_for_category(session_id, ToolCategory::Shell, Duration::from_secs(600)).await;
approval.clear_session_bypass(session_id).await;                            // remove all
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
|-------|------|-------------|
| `request_id` | i64 | Unique ID for resolution |
| `session_id` | i64 | Session that generated the request |
| `tool_call_id` | i64 | Tool call in the DB |
| `tool_name` | String | Name of the tool to execute |
| `arguments` | JSON | Full arguments |
| `agent_id` | String | Agent that called the tool |
| `source` | String | Session source |
| `context_label` | Option\<String\> | Human-readable origin label (e.g. `"CronJob: Daily Digest"`); used in Agent Inbox to identify context |
| `created_at` | String | ISO-8601 timestamp |

`context_label` is set by `ChatSessionHandler::set_context_label()` before the run (e.g. `TaskManager` sets `"CronJob: <title>"`). It is read in `llm_loop.rs` and `resume.rs` and passed to `approval.register()`.

---

## Agent Inbox

The **Agent Inbox** is the unified web page for managing all pending requests from background sessions (cron, etc.):

- **Approval requests** — tool calls requiring human confirmation (e.g. `execute_cmd`, `write_file`)
- **Clarification requests** — questions posed by the agent via `ask_user_clarification` when it cannot proceed autonomously

### REST API

| Method | Endpoint | Description |
|--------|----------|-------------|
| `GET` | `/api/inbox` | Returns `{ total, approvals, clarifications }` |
| `POST` | `/api/inbox/approvals/:request_id/resolve` | Body: `{ action: "approve"\|"reject", note?: string }` |
| `POST` | `/api/inbox/clarifications/:request_id/resolve` | Body: `{ answer: string }` |

The legacy endpoints `/api/approval/pending` and `/api/approval/resolve/:id` remain active for backwards compatibility.

### Frontend

The page is implemented in `web/components/agent-inbox.js` (`<agent-inbox-page>`). Polls every 8 s when open. The red badge in the sidebar (independent polling every 10 s) shows the total pending count.

See [frontend.md](frontend.md) for component details.

---

## Resolution

### From WebSocket (web frontend)

The client sends a JSON message:

```json
{ "type": "approve_tool", "request_id": 42 }
{ "type": "reject_tool",  "request_id": 42, "note": "optional reason" }
```

The legacy types `approve_write`/`reject_write` continue to work for backwards compatibility.

### From Telegram (future)

`ApprovalManager.resolve(request_id, decision)` is a public call: the Telegram bot can resolve it the same way as the WS handler.

---

## Behaviour on Restart

Approval requests are in-memory. On app restart:

- Pending approvals are lost
- Tool calls in `pending` state in the DB are shown to the LLM as "interrupted, please retry"
- The LLM re-calls the tools → they pass through the gate again → a new approval request is generated

This is the current behaviour. Future work may add persistence of pending approvals in SQLite to support transparent resumption.

---

## Module Structure

| File | Role |
| ---- | ---- |
| `src/core/approval/mod.rs` | `ApprovalManager`, `GateResult`, `ApprovalRule`, `PendingApprovalInfo`, `CategoryBypass`, session bypass methods |
| `src/core/clarification/mod.rs` | `ClarificationManager`, `PendingClarificationInfo` |
| `src/core/inbox.rs` | `Inbox`: unified façade for pending approvals + clarifications (wraps ApprovalManager, ClarificationManager, ChatHub) |
| `src/core/db/approval_rules.rs` | SQLite queries: list, insert, update, delete |
| `src/core/db/mod.rs` | `approval_rules` table creation |
| `src/core/session/handler/llm_loop.rs` | Resolves `category` via `ToolRegistry::category_of`, calls `approval.check(session_id, category, ...)` + `approval.register()` |
| `src/core/session/handler/resume.rs` | Same `check()` call as `llm_loop.rs` for pending tool re-gating |
| `src/core/session/handler/mod.rs` | `ChatSessionHandler` holds `Arc<ApprovalManager>`, `Arc<ClarificationManager>`, `context_label: RwLock<Option<String>>` |
| `src/frontend/api/inbox.rs` | `/api/inbox` endpoint + resolve for approval and clarification (uses `skald.inbox`) |
| `src/frontend/api/approval.rs` | Approval rules CRUD + `/api/approval/pending` + `/api/approval/tools` (uses `skald.catalog` for tool listing) |
| `src/frontend/api/ws.rs` | Handles `approve_tool`/`reject_tool`/`approve_write`/`reject_write` from the client |
| `src/core/events.rs` | `ServerEvent::ApprovalRequired` (generic tools) and `PendingWrite` (files with diff) |

---

## When to Update This File

- New action types in rules
- New notification channel added (e.g. Telegram)
- Pending approval persistence added to DB
- New fields in `PendingApprovalInfo` or `PendingClarificationInfo`
- New Agent Inbox APIs
