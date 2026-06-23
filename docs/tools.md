# Tools

## Tool Trait

```rust
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> Value;            // JSON Schema object
    fn execute(&self, _args: Value) -> Result<String> { /* default: Err */ }
    fn execute_async<'a>(&'a self, args: Value) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;
    fn category(&self) -> ToolCategory;              // access-control grouping
    fn sub_agents_only(&self) -> bool { false }      // default impl — visible only to sub-agents (depth > 0)
    fn root_agent_only(&self) -> bool { false }      // default impl — visible only to root agent (depth == 0)
    fn openai_definition(&self) -> Value { ... }     // default impl, rarely overridden
}
```

**Two execution paths:**

- **Sync tools** implement `execute(&self, args)` only. The default `execute_async` wraps it in a ready future — no changes needed.
- **Async tools** (e.g. `image_generate`, `image_generate_providers_list`) implement `execute_async` directly and omit `execute`. Do NOT use `block_in_place` — override `execute_async` instead.

The dispatcher in `llm_loop.rs` always calls `tool.execute_async(args).await`, so sync and async tools are dispatched uniformly.

**`sub_agents_only`**: if a tool returns `true`, it is excluded from the root agent's tool list and only added to sub-agent configs (depth ≥ 1) in `dispatch_sub_agent`. Default is `false`.

**`root_agent_only`**: if a tool returns `true`, it is included in the root agent's tool list but filtered out from sub-agent configs in `AgentRunConfig::for_sub_agent()`. Default is `false`.

Both flags are mutually exclusive — a tool should never return `true` for both. If it does, it will be invisible to all agents.

---

## ToolCategory

Every tool declares a `ToolCategory`, used for access-control filtering and audit:

| Variant | Used by |
| --- | --- |
| `Filesystem` | File read/write tools (`read_file`, `write_file`, `edit_file`, …) |
| `Shell` | `execute_cmd`, `restart` |
| `Subagent` | `call_agent` (synthetic — not in registry) |
| `Introspection` | `list_items`, `image_generate_providers_list` |
| `Config` | `register_mcp`, `toggle_item`, `execute_task` (InterfaceTool, interactive only), `delete_cron_job`, `configure_plugin`, `image_generate`, `set_secret`, `list_secrets` |

---

## ToolRegistry

`HashMap<String, Arc<dyn Tool>>` with four public methods:

| Method | Purpose |
| --- | --- |
| `register(tool)` | Insert tool keyed by `tool.name()` |
| `openai_definitions()` | Returns definitions for root-agent tools (excludes `sub_agents_only`) |
| `openai_definitions_sub_agents_only()` | Returns definitions for tools where `sub_agents_only() == true` |
| `root_agent_only_names()` | Returns names of all tools where `root_agent_only() == true` — used by `for_sub_agent()` to filter |
| `list_all()` | Returns `(name, description)` for all registered tools (sorted) |
| `category_of(name)` | Returns `Option<ToolCategory>` for a registered tool; `None` for MCP/interface/unknown tools |
| `dispatch(name, args)` | Executes tool by name; errors on unknown name |
| `describe_call(name, args, length)` | Returns a human-readable label for any tool call (including non-registry tools). Falls back to `name` for unknown tools. |

---

## ToolCatalog

`ToolCatalog` (`src/core/tool_catalog.rs`) is a unified façade wrapping `ToolRegistry` + `McpManager`:

| Method | Purpose |
| --- | --- |
| `list_all() -> AllTools` | Returns all built-in tools (registry), synthetic tools (`call_agent`, `update_scratchpad`, `ask_user_clarification`), and MCP tools as a single `AllTools { built_in, mcp }` struct. Used by `GET /api/approval/tools`. |
| `describe_call(name, args, length) -> String` | Pass-through to `ToolRegistry::describe_call()`. |

`AllTools` and `ToolInfo` are `#[derive(Serialize)]` — the frontend handler can return `Json<AllTools>` directly.

---

## Tool Name Constants

All system tool names are centralised in `src/core/tools/tool_names.rs` as `pub const` strings. Import with `use crate::tools::tool_names as tn;`.

| Constant | Value |
| --- | --- |
| `tn::CALL_AGENT` | `"call_agent"` |
| `tn::RESTART` | `"restart"` |
| `tn::UPDATE_SCRATCHPAD` | `"update_scratchpad"` |
| `tn::WRITE_TODOS` | `"write_todos"` |
| `tn::ASK_USER_CLARIFICATION` | `"ask_user_clarification"` |
| `tn::SHOW_MCP_TOOLS` | `"show_mcp_tools"` |
| `tn::NOTIFY` | `"notify"` |
| `tn::READ_NOTIFICATION` | `"read_notification"` |
| `tn::EXECUTE_CMD` | `"execute_cmd"` |

**Rule:** never hardcode these strings in new code — always use the constants. This ensures that a rename is a single-file change and that typos produce a compile error rather than a silent dispatch miss.

---

## Registration Pattern

All tools are registered in `src/main.rs` before `ChatSessionManager` is built.

**Not in ToolRegistry — synthetic tools intercepted in `run_agent_turn`:**

- `call_agent` — delegates to a sub-agent
- `update_scratchpad` — writes to `session_scratchpad` table; **shared** blackboard injected into every agent in the session; available to all agents
- `write_todos` — **stateless** private task list (TodoWrite-style: the agent re-sends the whole list with statuses on every call); available to all agents. Unlike the scratchpad it is **not** persisted and **not** shared: the formatted checklist lives only in the calling agent's own tool-result history, which is per-stack, so sub-agents and the caller never see it. Handled by `dispatch_write_todos` (`agent_dispatch.rs`); no DB table involved
- `ask_user_clarification` — pauses and asks the user a question; routing depends on session type:
  - **Interactive sessions** (web, Telegram): available to sub-agents only (`depth ≥ 1`); emits `ServerEvent::AgentQuestion`, waits inline
  - **Background sessions** (cron, tic): available at root level (`!is_interactive`); registers with `ClarificationManager`, visible in Agent Inbox; agent suspends until answered
- `show_mcp_tools` — activates MCP servers for the session (lazy loading); injected as an `InterfaceTool` in `build_agent_config` with per-session state; not available to sub-agents
- `notify` — queues a notification briefing to the home conversation via `ChatHub`; **injected as an `InterfaceTool` by the caller** (`TicManager` for TIC, `TaskManager` for background task agents); not in ToolRegistry so ordinary agents cannot call it

**Also not in ToolRegistry:**

- MCP tools — injected dynamically per-request via `McpManager::tools()`

---

## Tool Visibility Filtering (permission groups)

Tools are filtered out of the LLM's tool list when the effective approval rule for the session's **permission group** marks them `Deny`. The group comes from the session's run context (or the built-in `"default"` group). This replaces the removed per-agent `allow_tools` whitelist — see [approval/index.md](approval/index.md).

**MCP tools are never filtered here** — they pass through regardless of the group. The Approval gate governs MCP tool execution.

Filtering happens in `src/core/session/handler/config.rs` (depth 0) and `agent_dispatch.rs` (sub-agents) after assembling `base_tool_defs` (registry + synthetic tools), before extending with MCP tools.

---

## Built-in Tool Catalogue

| Tool name | Module | Category | Approval | Sub-agents only |
| --- | --- | --- | --- | --- |
| `list_files` | `tools::fs` | Filesystem | No | No |
| `read_file` | `tools::fs` | Filesystem | No | No |
| `write_file` | `tools::fs` | Filesystem | Yes (non-memory/) | No |
| `edit_file` | `tools::fs` | Filesystem | Yes (non-memory/) | No |
| `insert_at_line` | `tools::fs` | Filesystem | Yes (non-memory/) | No |
| `replace_lines` | `tools::fs` | Filesystem | Yes (non-memory/) | No |
| `search_file` | `tools::fs` | Filesystem | No | No |
| `grep_files` | `tools::fs` | Filesystem | No | No |
| `get_ast_outline` | `tools::ast_outline` | Filesystem | No | No |
| `execute_cmd` | `tools::exec` | Shell | **Always** | No |
| `restart` | `tools::restart` | Shell | **Always** | No |
| `list_items` | `tools::list_items` | Introspection | No | Merged listing for `type` ∈ {mcp, plugins, cron, agents}. For `agents`, each entry carries `id`, `name`, `description`, optional `instructions` (how to call the agent well, present only when set in `meta.json`), and optional `client`. |
| `register_mcp` | `tools::register_mcp` | Config | No | No |
| `toggle_item` | `tools::toggle_item` | Config | No | Merged enable/disable for `kind` ∈ {mcp, plugin, cron} |
| `execute_task` | InterfaceTool (not in registry) | Config | No | Interactive sessions only; `session_id` and `run_context_id` captured in closure at tool-build time; tasks inherit the parent RunContext |
| `run_subtask` | InterfaceTool (injected in run_job) | — | No | Background sessions only (sync sub-tasks); inherits `run_context_id` from the parent job |
| `read_agent_result` | synthetic | — | No | Interactive only; always returns not_ready; real delivery is async synthetic message |
| `delete_cron_job` | `tools::cron_jobs` | Config | No | No |
| `configure_plugin` | `tools::configure_plugin` | Config | No | No |
| `set_secret` | `tools::set_secret` | Config | No | No |
| `list_secrets` | `tools::list_secrets` | Config | No | No |
| `read_notification` | `tools::read_notification` | Introspection | No | Root only (depth == 0) |
| `image_generate_providers_list` | `tools::image_generate` | Introspection | No | No |
| `image_generate` | `tools::image_generate` | Config | No | No |
| `update_scratchpad` | synthetic | — | No | No |
| `write_todos` | synthetic (stateless) | — | No | No — private per-stack list; not shared with sub-agents or caller |
| `ask_user_clarification` | synthetic | — | No | Interactive: sub-agents only; Background: root level |
| `show_mcp_tools` | synthetic (per-session) | Config | No | No |

---

### Key Parameter Notes (recent additions)

| Tool | New parameters | Notes |
| --- | --- | --- |
| `execute_cmd` | `workdir` (absolute path), `timeout` (1–600 s, default 120) | Output truncated at 100 KB. Description tells LLM to use dedicated tools (`read_file`, `grep_files`, etc.) instead of shell equivalents. |
| `edit_file` | `replace_all` (bool, default false) | Replaces every occurrence when true; otherwise requires unique match. Description tells LLM to use instead of `sed`/`awk`. |
| `grep_files` | `output_mode` (`content`/`files_only`/`count`), `context_lines` (0–10), `offset` (pagination) | Description tells LLM to use instead of `grep`/`rg`. Result paths are relative to the queried directory (stripped of the resolved root), consistent with `list_files`. |
| `get_ast_outline` | `path` | Returns top-level definitions (functions, classes, structs, methods) without bodies. **tree-sitter 0.26** backend for: `.py .js .mjs .ts .tsx .go .java .c .h .cpp .cc .hpp .swift .lua .rb .sh .ex .exs .json .yaml .yml .html .css`. **syn** backend for `.rs`. Text/regex fallback for `.kt .toml .md .sql` (grammar crates incompatible with tree-sitter 0.26 at time of writing). |

---

## Tool Display Labels

Every `Tool` implementation can override `describe(&self, args: &Value, length: ToolDescriptionLength) -> String` to produce a compact human-readable label shown in the UI and on Telegram instead of the raw tool name.

| Length | Max chars | Example |
| --- | --- | --- |
| `Short` | 60 | `execute_cmd \`git\`` |
| `Full` | 120 | `execute_cmd \`git commit -m "feat: ..."\`` |

Constants `MAX_LABEL_SHORT` and `MAX_LABEL_FULL` are defined in `src/core/tools/mod.rs`. `truncate_label(s, max)` truncates at char boundary appending `…`.

The default implementation returns `self.name()`, so all tools work without implementing `describe`. Built-in tools (fs, exec) have explicit implementations; MCP and plugin tools fall back to the tool name.

`ToolRegistry::describe_call(name, args, length)` is the single call-site used by `llm_loop.rs`, `resume.rs`, and the `/api/{source}/messages` history endpoint. It also handles synthetic tools (`call_agent`) that are not in the registry.

Labels are emitted in `ServerEvent::ToolStart` as `label_short` and `label_full` and included in history responses so the frontend always has them.

---

## FS Path Resolution

`tools::fs::resolve(path)`:

- If path starts with `/` → used as absolute path
- Otherwise → resolved relative to CWD (project root when running via `run.sh`)

Paths starting with `memory/` bypass the approval gate for write tools.

### Security-aware canonicalization

`tools::fs::canonicalize_for_policy(path, base)` resolves a path to its canonical absolute
form (resolving `..` and symlinks of the longest existing ancestor) for security
prefix-matching. It is shared by the RunContext fast-paths (`is_write_allowed` /
`is_read_allowed`) and `approval::normalize_path`, so traversal/symlink tricks like
`docs/../secrets/x` cannot evade an allow grant or a deny rule. `path_under(child, base)`
does the component-wise prefix test.

### Read auto-allow & `secrets/` deny

Read tools (`read_file`, `grep_files`, `list_files`, `search_file`, `get_ast_outline`) are
auto-allowed without a prompt when the path is under the working dir, `docs/`, `skills/`,
`allow_fs_reads`, or `allow_fs_writes` (the RunContext read fast-path). The `secrets/`
directory is denied for these tools via seeded `deny` rules; the recursive ones
(`grep_files`, `list_files`) additionally list `secrets` in their `SKIP_DIRS` so a search
rooted higher up never descends into it. See [approval/index.md](approval/index.md) and
[session/run-context.md](session/run-context.md).

---

## Adding a Tool

1. Create a struct in `src/core/tools/` (new file or existing module).
2. `impl Tool` for the struct — include `fn category()`.
3. Register in `src/main.rs`: `tool_registry.register(MyTool::new(...))`.
4. If the tool should only be visible to certain agent depths, implement `sub_agents_only()` or `root_agent_only()` instead of using `InterfaceTool` injection.
5. If the tool needs `ChatHub`, a per-session resource, or should only be visible to specific callers, do **not** add it to `ToolRegistry` — implement it as an `InterfaceTool` and inject it at the call site (see `tools::notify::make_tool`).
6. If the tool needs user approval before executing, add an `approval_rules` row (or let the admin add one). The approval gate (`ApprovalManager::check`) is rule-driven — no code change required unless the default-open policy is not suitable.
7. Update this doc (catalogue table).

---

## When to Update This File

- A tool is added, removed, or renamed
- The approval rules for a tool change
- The `Tool` trait gains or loses a method
- `ToolCategory` gains a new variant
- The tool visibility (permission-group) filtering logic changes
