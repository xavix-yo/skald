# MCP (Model Context Protocol)

## Workspace Location

The MCP protocol layer lives in the standalone crate `crates/mcp-client`:
- `McpServer` — stdio subprocess client
- `McpHttpServer` — streamable HTTP client
- `McpServerClient` trait, `McpTool`, `McpServerConfig`, `McpTransport`

`McpManager` (`src/core/mcp/mod.rs`) remains in the main crate because it owns the `SqlitePool` and calls `crate::db::mcp_events` / `crate::db::mcp_servers`.

---

## What MCP Is Here

MCP allows external processes or HTTP services to expose tools to the LLM. The app connects to MCP servers at startup (or on demand via `register_mcp`), discovers their tools, and makes them available alongside built-in tools.

---

## McpManager Internals

```rust
McpManager {
  pool:    Arc<SqlitePool>
  servers: RwLock<HashMap<String, Arc<dyn McpServerClient>>>  // running servers
  errors:  RwLock<HashMap<String, String>>                    // startup failures
}
```

Initialization runs in a background `tokio::spawn` task. The manager is available immediately; servers connect asynchronously. A server failing to start is recorded in `errors` and does not block the app.

---

## Transports

| Transport | When to use | Required fields |
| --- | --- | --- |
| `stdio` | Local process (spawn subprocess) | `command`, optionally `args`, `env` |
| `http` | Remote HTTP server (streamable MCP) | `url`, optionally `api_key` |
| `sse` | Alias for `http` (backward compat) | same as `http` |

`${VAR}` interpolation is supported in `env` values and `api_key`.

---

## Tool Naming Convention

MCP tools are exposed to the LLM as **`mcp__<server_name>__<tool_name>`**.

Examples:

- Server `tavily`, tool `search` → `mcp__tavily__search`
- Server `fetch`, tool `get` → `mcp__fetch__get`

`parse_mcp_tool_name(name)` in `src/core/mcp/mod.rs` splits on `__` to extract server and tool names. This is how `run_agent_turn` routes MCP calls.

---

## Registering a Server

All MCP servers are stored in the **`mcp_servers` table** in SQLite. There is no static config file.

**Live registration** via `register_mcp` tool:

- LLM calls `register_mcp` with name, transport, connection details, and optionally `description` and `friendly_name`
- `McpManager::register()` does DB upsert + live `start_one()` connect
- Server is immediately available without a restart

**Tool parameters:**

| Parameter | Required | Type | Description |
| --- | --- | --- | --- |
| `name` | yes | string | Unique name for this MCP server (used to reference it in tool calls) |
| `transport` | yes | string | `stdio`, `http`, or `sse` |
| `command` | stdio only | string | Executable to spawn |
| `args` | stdio only | string[] | Command-line arguments |
| `env` | stdio only | object | Extra environment variables |
| `url` | http/sse only | string | Base URL of the remote server |
| `api_key` | http/sse only | string | API key (sent as `Authorization: Bearer <key>`) |
| `description` | no | string | Short description of what the server provides (shown in `list_mcp`) |
| `friendly_name` | no | string | Human-readable display name for UI (e.g. "Google Calendar") |

**Startup timeout**: **`SERVER_START_TIMEOUT_SECS = 120`**. Servers that don't respond within 120 s are recorded as errors.

---

## Enabling / Disabling Servers

Use the built-in tool **`toggle_mcp`** to enable or disable an MCP server by name:

```text
toggle_mcp(name="gcal", enabled=false)  # disable
toggle_mcp(name="gcal", enabled=true)   # enable
```

**Important:** Toggling updates the `enabled` flag in the database, but **a restart is required** for the change to take effect on running servers. Disabled servers won't connect on next restart.

Use `list_mcp` to see current server names and statuses.

---

## Example: Google Calendar MCP Server

A custom Python MCP server (`scripts/gcal_mcp_server.py`) provides full read/write access to Google Calendar:

| Tool | Description |
| --- | --- |
| `list_calendars` | Lists all calendars accessible to the authenticated user |
| `list_events` | Lists events with filters: `calendar_id`, `start_time`, `end_time`, `max_results`, `full_text`, `time_zone` |
| `get_event` | Returns a single event by `event_id` |
| `create_event` | Creates a new event (`summary`, `start`, `end`, optional description/location/attendees/recurrence) |
| `update_event` | Updates an existing event — only fields provided are changed |
| `delete_event` | Permanently deletes an event by `event_id` |
| `respond_to_event` | Sets RSVP status (`accepted`, `declined`, `tentative`, `needsAction`) |

**Credentials:** Stored in `./secrets/google_creds.json`. Run `python3 scripts/gcal_oauth_setup.py` to authenticate (requires `https://www.googleapis.com/auth/calendar` scope). Token refresh is handled automatically.

**Register:**

```text
register_mcp(name="gcal", transport="stdio", command="python3", args=["scripts/gcal_mcp_server.py"])
```

**Disable when not needed:**

```text
toggle_mcp(name="gcal", enabled=false)
restart
```

---

## Push Notifications from MCP Servers

MCP servers can send **unsolicited events** to the app by writing JSON-RPC notification messages (no `id` field) to stdout. The app persists them to SQLite and processes them in batches via the TIC background agent.

### Protocol

A notification is a JSON-RPC 2.0 message without `id`:

```json
{"jsonrpc": "2.0", "method": "event/new_email", "params": {"subject": "...", "from": "..."}}
```

### How it flows

```text
MCP server writes notification to stdout
  → McpServer reader loop detects msg with no "id"
  → sends (server_name, msg) over notification_tx channel
  → McpManager::notification_consumer persists to mcp_events table
  → TicManager (every `tic.interval_secs`, default 900 s) fetches pending events, runs TIC agent
  → TIC calls notify(briefing) if user action is needed
```

### Implementing notifications in an MCP server

**Node.js (WhatsApp)**:

```js
function notify(method, params) {
    process.stdout.write(JSON.stringify({jsonrpc:'2.0', method, params}) + '\n');
}
client.on('message', async (msg) => {
    if (msg.fromMe) return;
    notify('event/whatsapp_message', { from: msg.from, body: msg.body });
});
```

**Python (Gmail, GCal)** — use a lock to avoid interleaving with MCP responses:

```python
import threading
_stdout_lock = threading.Lock()

def _emit_notification(method, params):
    msg = json.dumps({"jsonrpc": "2.0", "method": method, "params": params})
    with _stdout_lock:
        sys.stdout.write(msg + "\n")
        sys.stdout.flush()
```

Start a daemon polling thread in `main()` before entering the MCP serve loop. The MCP serve loop must also acquire `_stdout_lock` before writing responses.

### Implemented notification sources

| Source | Method | Trigger | Poll interval |
| --- | --- | --- | --- |
| `whatsapp` | `event/whatsapp_message` | Inbound WhatsApp message | Real-time (event) |
| `gmail` | `event/new_email` | New email in INBOX | 60 s (History API) |
| `gcal` | `event/new_calendar_event` | New calendar event created | 300 s (Events API) |

---

---

## Lazy MCP Tool Loading

By default, injecting all MCP tool definitions into every LLM turn is expensive — 30+ tools can consume 10,000+ tokens per turn. Lazy loading solves this by only including tools for servers that have been explicitly activated.

### How It Works

1. At the start of each turn, `build_agent_config` reads `session_mcp_grants` for the current `session_id` and populates `active_mcp_grants` in memory.
2. **MCP tools are no longer part of `base_tool_defs`**. Instead, `AgentRunConfig::all_tool_defs()` re-queries `mcp.tools_for(active_mcp_grants)` on **every LLM round**. This means a `show_mcp_tools` call in round N makes those tools available from round N+1 within the same turn — no cross-turn delay.
3. The system prompt contains a `<!-- MCP_LIST -->` tag (in `AGENT.md`) which is replaced at request time with a dynamic two-section block:

   ```text
   ## MCP servers

   **Available** — call `show_mcp_tools(["name", ...])` to load tools:
   - `tavily`: tavily_crawl, tavily_extract, tavily_search
   - `whatsapp`: whatsapp_get_messages, whatsapp_list_chats, whatsapp_send_message

   **Active** — tools already loaded in context:
   - `gmail`: get_message, list_messages, send_message
   ```

### `<!-- MCP_LIST -->` Tag

Add this tag anywhere in an `AGENT.md` to inject the dynamic MCP availability block at that position. Agents that do not include the tag receive no MCP list injection.

Currently used in: `agents/main/AGENT.md`, `agents/tic/AGENT.md`.

Resolution pipeline:

- `agents::resolve_includes()` — replaces `<!-- MCP_LIST -->` with the `__MCP_LIST__` sentinel.
- `ChatSessionHandler::build_openai_messages()` — replaces `__MCP_LIST__` with the rendered block (via `render_mcp_list()`).

### `show_mcp_tools` Tool

A synthetic interface tool (not in the global `ToolRegistry`):

```text
show_mcp_tools(mcp_names: ["server_name", ...])
```

- Takes an array of MCP server names.
- Updates the in-memory `active_mcp_grants` set immediately.
- **Root agents** (`stack_id = None`): persists grants to `session_mcp_grants` — survives across turns and restarts.
- **Sub-agents** (`stack_id = Some(id)`): persists grants to `stack_mcp_grants` — survives restarts, but **deleted when the stack frame terminates** (no session leak).
- Returns a confirmation string listing which servers were activated and their scope (`session` or `stack <id>`).

**Root agents**: injected in `build_agent_config` as an `InterfaceTool`.
**Sub-agents**: injected in `dispatch_call_agent` — sub-agents always start with zero grants and activate what they need.

### Sub-Agent MCP Isolation

Sub-agents have a fully isolated MCP grant state:

| Aspect | Root agent | Sub-agent |
| --- | --- | --- |
| Initial grants | Loaded from `session_mcp_grants` DB | Empty (starts from zero) |
| `show_mcp_tools` persists to | `session_mcp_grants` | `stack_mcp_grants` |
| Grants survive restart? | Yes | Yes (re-loaded by `dispatch_call_agent`) |
| Grants cleaned up? | No (session lifetime) | Yes (on frame termination) |
| Session contamination? | N/A | None |

Sub-agents that don't include `<!-- MCP_LIST -->` in their `AGENT.md` receive no MCP list injection in the system prompt. The tool definitions are still included dynamically in `all_tool_defs()` based on grants, so they can call tools without the descriptive list — useful for agents with a narrow, pre-known tool set.

### `tic` Agent

`tic` uses lazy loading like any other root agent — it calls `show_mcp_tools` for the servers it needs based on the pending events it receives. This avoids loading all MCP tool definitions on every tick when there may be nothing to process.

### Token Savings

| Situation | Approximate tokens |
| --- | --- |
| All MCP tools always loaded (old behaviour) | ~10,000–20,000 |
| Lazy mode, no grants yet | ~50–100 (compact list only) |
| Lazy mode, gmail + gcal granted | ~2,000–4,000 |

---

## When to Update This File

- A new transport type is added
- The tool naming convention changes
- `SERVER_START_TIMEOUT_SECS` changes
- `register_mcp` tool parameters change (schema, required fields, description, friendly_name)
- `list_mcp` return format changes (McpServerInfo fields)
- A new notification source is implemented
- Lazy loading logic changes (`build_agent_config`, `dispatch_call_agent`, `show_mcp_tools`, grant tables)
- `ClientMessage` loses or gains fields relevant to MCP
