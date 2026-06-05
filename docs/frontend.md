# Frontend

## WebSocket Endpoint

`GET /api/ws?source=<string>`

`source` identifies the client type: `web` (default, desktop copilot) or `mobile` (mobile chat page). The same endpoint serves both; ChatHub maintains independent sessions per source.

One connection per source. The connection is upgraded by Axum's WS handler in `src/api/ws.rs`. The client sends one `ClientMessage`, receives a stream of `ServerEvent`s, then can send additional messages (cancel, approval) while events are in flight.

History for a source: `GET /api/<source>/messages` (or the legacy alias `/api/web/messages`).

---

## ClientMessage Fields

| Field | Type | Description |
|---|---|---|
| `content` | `String` | The user's prompt text |
| `client` | `Option<String>` | Named LLM model override (or `"auto"`) |

---

## ServerEvent Types

All events are JSON objects with a `"type"` tag (snake_case).

| type | Key fields | When emitted |
|---|---|---|
| `tool_start` | `tool_call_id`, `message_id`, `name`, `arguments` | Tool call recorded, about to execute |
| `tool_done` | `tool_call_id`, `result` | Tool executed successfully |
| `tool_error` | `tool_call_id`, `error` | Tool execution failed |
| `agent_start` | `stack_id`, `parent_tool_call_id`, `agent_id`, `depth` | Sub-agent stack frame opened |
| `agent_done` | `stack_id` | Sub-agent stack frame closed |
| `thinking` | `message_id`, `content`, `input_tokens`, `output_tokens` | LLM produced text before tool calls |
| `pending_write` | `request_id`, `tool_call_id`, `path`, `old_content`, `new_content` | Approval required for write/command |
| `agent_question` | `request_id`, `tool_call_id`, `title`, `question`, `suggested_answers` | Sub-agent needs user clarification |
| `file_changed` | `path` | A tool wrote to a file |
| `done` | `message_id`, `stack_id`, `content`, `input_tokens`, `output_tokens` | Turn complete, final response |
| `truncated` | `output_tokens` | LLM hit token limit (`finish_reason=length`) |
| `error` | `message` | Fatal error (session handler failed) |
| `model_fallback` | `from`, `to`, `reason` | Active model swapped to fallback automatically |
| `llm_failed` | `tried`, `last_error` | All LLM fallback attempts exhausted |
| `approval_required` | `request_id`, `tool_call_id`, `tool_name`, `arguments` | Non-file tool call requires user approval |
| `approval_resolved` | `request_id`, `approved` | Approval resolved (any source); all clients update their UI |
| `user_message` | `content` | User message broadcast to other clients of the same source |
| `new_session` | `session_id` | Session was cleared (`/new`, `/clear`); clients reset their message list |

---

## Slash Commands (Web Copilot)

The web copilot supports the following slash commands, intercepted server-side in
`src/api/ws.rs` before reaching the LLM:

| Command | Effect |
|---|---|
| `/new` | Create a new chat session (handled client-side, clears context) |
| `/help` | Show available commands |
| `/context` | Show last turn's token usage (`↑X tok · ↓Y tok`) |
| `/compact` | Force context compaction (bypasses the token threshold) |
| `/sethome` | Set web as the home source for background notifications |

Unknown commands are forwarded to the LLM as regular text.

---

## Tool Call Status Lifecycle

Tool calls in `chat_llm_tools` progress through these states:

| DB status  | Meaning | Frontend `build_items` |
|------------|---------|------------------------|
| `running`  | Tool executing — no user action required | `status: 'error', error: 'Interrupted.'` (shown after page refresh/restart) |
| `pending`  | Blocked on explicit user input (approval gate `Require`, or `ask_user_clarification`) | `status: 'pending'` → shows approval/clarification form |
| `done`     | Completed successfully | `status: 'done'` |
| `failed`   | Terminated with error | `status: 'error'` |

On **page refresh** or **app restart**, the frontend detects pending/interrupted tools in history (`_hasPendingTools` flag set in `_loadHistory`). On `ws.onopen` it sends `{"type":"resume"}`, which triggers `resume_turn()` → `resume_pending_tools()`:
- `running` tools → re-executed through the approval gate
- `pending` tools (approval) → approval channel re-registered, `approval_required` re-emitted with new `request_id`
- `pending` tools (`ask_user_clarification`) → question re-asked via `dispatch_ask_user_clarification`
- `call_agent` tools → skipped here; child stack is resumed by `resume_turn()` cascade (see below)

`resume_turn()` also cascades upward when a sub-agent stack completes: it terminates the child, marks the parent's `call_agent` tool as `done`, then continues running the parent stack until the root emits `Done`.

---

## Approval Flow

1. Server emits `pending_write` with `request_id`, `path`, `old_content`, `new_content`.
2. Frontend shows a diff and prompts the user.
3. User approves → client sends: `{"type":"approve_write","request_id":<N>}`
4. User rejects → client sends: `{"type":"reject_write","request_id":<N>,"note":"<optional reason>"}`
5. Server receives the message via `handle_approval_msg()`, calls `handler.resolve_approval(request_id, decision)`.
6. The `oneshot` channel unblocks in `run_agent_turn`, execution proceeds or is skipped.

Before blocking on the approval channel, the server sets `status='pending'` in `chat_llm_tools` via `set_approval_pending()`. This is what distinguishes "waiting for user" from "tool was executing when the session was interrupted" (`running`).

## Clarification Flow

### Interactive sessions (web / Telegram)

1. A sub-agent calls `ask_user_clarification(title, question, suggested_answers?)`.
2. Server sets `status='pending'` for the tool call, then sends `agent_question` with `request_id`, `title`, `question`, and optional `suggested_answers`.
3. Frontend shows the question and collects a free-text answer (suggested answers shown as clickable chips).
4. Client sends: `{"type":"answer_question","request_id":<N>,"answer":"<user text>"}`
5. Server calls `handler.resolve_question(request_id, answer)`.
6. The answer is returned as the tool result and the sub-agent continues.

On WS disconnect while waiting, `cancel_pending_questions()` drops all channels, causing the awaiting tool call to fail with an error. On reconnect, auto-resume re-asks the question.

### Background sessions (cron / tic)

1. The agent (root or sub-agent) calls `ask_user_clarification(title, question, suggested_answers?)`.
2. `dispatch_ask_user_clarification` sets `status='pending'` then registers with `ClarificationManager` (in-memory, in-process).
3. The entry appears in `GET /api/inbox` under `clarifications`.
4. User answers via the Agent Inbox page → `POST /api/inbox/clarifications/:request_id/resolve`.
5. The `oneshot` channel unblocks, answer is returned as tool result, agent continues.

Cancel message (abort current turn): `{"type":"cancel"}`

---

## Lit Component Inventory

| File | Element | Responsibility |
|---|---|---|
| `web/lib/chat-session.js` | `ChatSession` (base) | Shared WS logic, message state, all approval/LLM event handling. Subclasses override `_wsSource`, `_getInputContent`, `_clearInput`, `_scrollToBottom`, `_onMessagePushed` |
| `web/components/copilot.js` | `<app-copilot>` | Desktop copilot panel (`_wsSource='web'`); resize, composer input with model pill and auto-resize textarea |
| `web/components/shared/chat-page.js` | `<chat-page>` | Mobile chat page (`_wsSource='mobile'`); extends `ChatSession` with mobile-specific layout |
| `web/components/copilot-render.js` | (helpers) | Renders messages, tool call blocks, diffs — shared by copilot and chat-page |
| `web/components/sidebar.js` | `<app-sidebar>` | Navigation sidebar; polls `/api/inbox` every 10 s for badge count |
| `web/components/topbar.js` | `<app-topbar>` | Top navigation bar |
| `web/components/editor.js` | (editor panel) | File editor with cursor/selection tracking |
| `web/components/cron-jobs.js` | `<cron-jobs-page>` | Cron job management UI — columns: Title (+ one-shot badge), Cron, Agent, Last run, Next run, Enabled, Actions |
| `web/components/agent-inbox.js` | `<agent-inbox-page>` | Unified inbox for pending approvals and clarifications from background sessions; polls `/api/inbox` every 8 s when open |
| `web/components/models-hub.js` | `<models-hub-page>` | Models hub — 3-card landing page (LLM / Transcription / Image Generation) with live model counts; internal navigation to sub-sections |
| `web/components/models-llm.js` | `<models-llm-section>` | LLM model management: drag-and-drop priority, catalog picker (OpenRouter/Ollama/…), add/edit/delete |
| `web/components/models-transcribe.js` | `<models-transcribe-section>` | Transcription model CRUD; filters providers by `supported_types.includes('transcribe')` |
| `web/components/models-image.js` | `<models-image-section>` | Image generation model CRUD; filters providers by `supported_types.includes('image_generate')` |
| `web/components/llm-providers.js` | `<llm-providers-page>` | LLM provider management |
| `web/components/agents.js` | `<agents-page>` | Agent discovery and configuration |
| `web/components/approval-rules.js` | `<approval-rules-page>` | Approval rule management |

All components extend `LightElement` from `web/lib/base.js` (Lit-based).

### Agent Inbox page

Approval cards have a yellow left border; clarification cards have a blue left border. Clarification cards show suggested-answer chips (click pre-populates the input) and a free-text input — submit with Enter or the Send button. Approval cards have Approve / Reject buttons (reject prompts for an optional note via `window.prompt`).

---

## Adding a New ServerEvent

1. Add the variant to `ServerEvent` enum in `src/events.rs`.
2. Add the `type_name()` match arm in `src/events.rs`.
3. Emit it at the appropriate point (session handler, ChatHub, or ws.rs).
4. Handle it in `web/lib/chat-session.js` `_handleServerMsg()` — all clients inherit the handler automatically.
5. Update the ServerEvent Types table above.

---

## Debug Mode

A persistent flag stored in the `config` DB table under key `DEBUG_MODE` (`"true"` / `"false"`). The API is in `src/api/dev.rs`.

| Method | Path | Body | Response |
| --- | --- | --- | --- |
| `GET` | `/api/dev/debug_mode` | — | `{ "enabled": bool }` |
| `POST` / `PUT` | `/api/dev/debug_mode` | `{ "enabled": bool }` | `{ "enabled": bool }` |

The frontend reads this flag at startup and uses it to show or hide sections in the sidebar menu that are otherwise invisible in production.

---

## When to Update This File

- A `ServerEvent` variant is added, removed, or its fields change
- `ClientMessage` gains or loses a field
- A new Lit component is added
- The approval message format changes
- The debug-mode endpoint changes
