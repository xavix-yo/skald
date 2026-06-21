use std::collections::HashMap;

use async_trait::async_trait;
use tokio::sync::broadcast;

use crate::events::GlobalEvent;
use crate::interface_tool::InterfaceTool;

// ── SendMessageOptions ────────────────────────────────────────────────────────

/// Optional parameters for a [`ChatHubApi::send_message`] call.
#[derive(Default)]
pub struct SendMessageOptions {
    /// Agent to use for this source's session. Defaults to `"main"` if not set.
    /// Only takes effect when a new session is created — ignored for existing sessions.
    pub agent_id: Option<String>,
    /// Named substitutions applied to the agent's system prompt.
    /// Each entry replaces the sentinel `__KEY__` in the loaded prompt text.
    /// Matches `<!-- KEY -->` placeholders that `agents::resolve_includes` converts to sentinels.
    pub system_substitutions: HashMap<String, String>,
    pub client_name: Option<String>,
    /// Extra text prepended to the agent's system prompt for this turn only.
    /// STATIC: safe to cache — use for interface-specific formatting rules that
    /// never change turn-to-turn (e.g. Telegram HTML mode).
    pub extra_system_context: Option<String>,
    /// Extra system message injected AFTER the conversation history for this turn only.
    /// DYNAMIC: not cached — use for per-turn context (e.g. notification framing).
    pub extra_system_dynamic: Option<String>,
    /// Short reminder injected near the tail of the message list to prevent drift.
    pub tail_reminder: Option<String>,
    pub interface_tools: Vec<InterfaceTool>,
    /// True for system-generated messages injected as user turns (notification briefings).
    pub is_synthetic: bool,
}

// ── ChatHubApi ────────────────────────────────────────────────────────────────

/// Abstraction over [`ChatHub`](crate) that plugins and external crates depend on.
///
/// Implementing this trait for `ChatHub` in the main crate is the only coupling
/// point needed: plugins can accept `Arc<dyn ChatHubApi>` and stay independent.
#[async_trait]
pub trait ChatHubApi: Send + Sync {
    /// Register a source. No-op for duplicate registrations.
    async fn register(&self, source_id: &str);

    /// Send a user message for a source, running a full LLM turn.
    /// Creates a session lazily if none exists yet.
    async fn send_message(
        &self,
        source_id: &str,
        prompt: &str,
        opts: SendMessageOptions,
    ) -> anyhow::Result<()>;

    /// Create a new session for the source, discarding the previous one.
    async fn clear(&self, source_id: &str) -> anyhow::Result<i64>;

    /// Subscribe to the global event bus.
    /// Filtering by source is the caller's responsibility.
    fn events(&self, source_id: &str) -> broadcast::Receiver<GlobalEvent>;

    /// Set which source is the "home" for background agent notifications.
    async fn set_home(&self, source_id: &str) -> anyhow::Result<()>;

    /// Returns token usage `(input, output)` for the last message in the source's session.
    async fn context_info(
        &self,
        source_id: &str,
    ) -> anyhow::Result<(Option<i64>, Option<i64>)>;

    /// Total spend (USD) of the source's active session, including synchronous
    /// sub-agent frames and excluding asynchronous tasks (which run in their own
    /// session). `None` when no provider reported a cost.
    async fn cost_info(&self, source_id: &str) -> anyhow::Result<Option<f64>>;

    /// Force compaction of the source's active session history.
    /// Returns `true` if compaction occurred.
    async fn force_compact(&self, source_id: &str) -> anyhow::Result<bool>;

    /// Resume any interrupted turn for a source's active session.
    async fn resume(&self, source_id: &str) -> anyhow::Result<()>;

    /// Approve a pending tool-call approval request.
    async fn approve(&self, request_id: i64);

    /// Reject a pending tool-call approval request.
    async fn reject(&self, request_id: i64, note: String);

    /// Resolve a pending `ask_user_clarification` question.
    /// Collapses `session_handler(source_id).resolve_question(...)` into a single
    /// hub-level call so callers never need to know about `ChatSessionHandler`.
    async fn resolve_question(&self, source_id: &str, request_id: i64, answer: String);

    /// Cancel the active LLM turn for a source, clearing any pending approvals
    /// and clarification questions. No-op if no session is active.
    async fn cancel(&self, source_id: &str);

    /// Revoke all session-scoped MCP grants for a source's active session.
    /// The next LLM turn will start with no MCP tools activated.
    async fn reset_mcp(&self, source_id: &str) -> anyhow::Result<()>;
}
