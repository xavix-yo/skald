use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── Client → Server ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ClientMessage {
    pub content: String,
    /// Reserved: selects a named LLM client from config.
    pub client:  Option<String>,
}

/// Typed data push from remote clients (iOS app, etc.).
/// Sent over the existing WebSocket as `{"type":"data","stream":"...","payload":{...}}`.
#[derive(Deserialize)]
pub struct InboundDataMessage {
    pub stream:  String,
    pub payload: Value,
}

// ── Global event envelope ─────────────────────────────────────────────────────

/// Envelope that wraps every event on the global broadcast bus.
/// `source` is `None` for system/background events (cron, tic, plugins).
#[derive(Clone)]
pub struct GlobalEvent {
    pub source:     Option<String>,
    pub session_id: Option<i64>,
    pub event:      ServerEvent,
}

// ── Server → Client ───────────────────────────────────────────────────────────

#[derive(Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    /// A tool call was started. DB status: running.
    ToolStart {
        tool_call_id: i64,
        message_id:   i64,
        name:         String,
        arguments:    Value,
        /// Concise human-readable label (≤60 chars): tool + primary argument.
        label_short:  String,
        /// Verbose human-readable label (≤120 chars): tool + all meaningful arguments.
        label_full:   String,
    },
    /// A tool call completed successfully. DB status: done.
    ToolDone {
        tool_call_id: i64,
        result:       String,
    },
    /// A tool call failed. DB status: error.
    ToolError {
        tool_call_id: i64,
        error:        String,
    },
    /// A sub-agent stack frame was opened.
    AgentStart {
        stack_id:            i64,
        parent_tool_call_id: i64,
        agent_id:            String,
        parent_agent_id:     String,
        depth:               i64,
        /// The prompt sent to the sub-agent (truncated to 500 chars by the sender).
        prompt_preview:      String,
    },
    /// A sub-agent stack frame was closed.
    AgentDone {
        stack_id: i64,
        agent_id: String,
        parent_agent_id: String,
        /// The sub-agent's final response (truncated to 500 chars by the sender).
        result_preview: String,
    },
    /// The assistant response is complete.
    Done {
        message_id:    i64,
        stack_id:      i64,
        content:       String,
        input_tokens:  Option<u32>,
        output_tokens: Option<u32>,
    },
    /// A fatal error occurred processing the request.
    Error {
        message: String,
    },
    /// The LLM was cut off by the token limit (finish_reason="length").
    Truncated {
        output_tokens: Option<u32>,
    },
    /// The LLM produced text alongside tool calls (reasoning before acting).
    Thinking {
        message_id:    i64,
        content:       String,
        input_tokens:  Option<u32>,
        output_tokens: Option<u32>,
    },
    /// A write operation requires user approval before executing (shows a diff).
    PendingWrite {
        request_id:   i64,
        tool_call_id: i64,
        path:         String,
        old_content:  Option<String>,
        new_content:  String,
    },
    /// A non-file tool call requires user approval before executing.
    /// Used for MCP tools, execute_cmd, restart, and any other tool
    /// that the ApprovalManager flags as `Require`.
    ApprovalRequired {
        request_id:   i64,
        tool_call_id: i64,
        tool_name:    String,
        arguments:    Value,
    },
    /// A sub-agent needs clarification from the user before continuing.
    AgentQuestion {
        request_id:        i64,
        tool_call_id:      i64,
        title:             String,
        question:          String,
        suggested_answers: Vec<String>,
    },
    /// A book file was written by a tool; the frontend should reload if it has it open.
    FileChanged {
        path: String,
    },
    /// The active LLM model failed and the system switched to a fallback automatically.
    ModelFallback {
        from:   String,
        to:     String,
        reason: String,
    },
    /// All LLM fallback attempts were exhausted; the turn could not complete.
    LlmFailed {
        tried:      Vec<String>,
        last_error: String,
    },
    /// A pending approval or pending-write was resolved (approved or rejected).
    /// Emitted on the global bus so all clients (e.g. Telegram) can update their UI.
    ApprovalResolved {
        request_id:   i64,
        tool_call_id: i64,
        approved:     bool,
    },
    /// The active session for a source was replaced (e.g. /new, /clear).
    NewSession {
        session_id: i64,
    },
    /// A user message was received; broadcast so secondary tabs/mobile see it.
    UserMessage {
        content: String,
    },
    /// Sent to a client right after it (re)connects, reporting whether a turn is
    /// currently in flight for its session. Lets a reloaded page restore the
    /// SEND→STOP button state instead of assuming idle.
    TurnRunning {
        running: bool,
    },
}

impl ServerEvent {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("ServerEvent serialization failed")
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Self::ToolStart      { .. } => "tool_start",
            Self::ToolDone       { .. } => "tool_done",
            Self::ToolError      { .. } => "tool_error",
            Self::AgentStart     { .. } => "agent_start",
            Self::AgentDone      { .. } => "agent_done",
            Self::Done           { .. } => "done",
            Self::Error          { .. } => "error",
            Self::Thinking       { .. } => "thinking",
            Self::PendingWrite       { .. } => "pending_write",
            Self::ApprovalRequired   { .. } => "approval_required",
            Self::AgentQuestion      { .. } => "agent_question",
            Self::FileChanged        { .. } => "file_changed",
            Self::Truncated          { .. } => "truncated",
            Self::ModelFallback      { .. } => "model_fallback",
            Self::LlmFailed          { .. } => "llm_failed",
            Self::ApprovalResolved   { .. } => "approval_resolved",
            Self::NewSession         { .. } => "new_session",
            Self::UserMessage        { .. } => "user_message",
            Self::TurnRunning        { .. } => "turn_running",
        }
    }
}
