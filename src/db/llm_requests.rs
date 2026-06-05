//! DB operations for the `llm_requests` table.
//!
//! Every `chat_with_tools` call is logged here by the
//! [`crate::chatbot::logging::LoggingChatbotClient`] wrapper.
//! Rows are retained for `llm.request_log.retention_days` days (default 14).

use anyhow::Result;
use sqlx::SqlitePool;

// ── Row struct ────────────────────────────────────────────────────────────────

pub struct LlmRequestRow {
    pub session_id:            Option<i64>,
    pub stack_id:              Option<i64>,
    pub model_name:            String,
    /// Full HTTP request body sent to the provider (compact JSON, no pretty-print).
    pub request_json:          String,
    /// HTTP request headers as a compact JSON object (api-key redacted).
    pub request_headers:       Option<String>,
    /// Full HTTP response body from the provider (compact JSON).
    pub response_json:         Option<String>,
    /// HTTP response headers as a compact JSON object.
    pub response_headers:      Option<String>,
    /// Error message when the HTTP call itself failed (no response available).
    pub error_text:            Option<String>,
    pub input_tokens:          Option<i64>,
    pub output_tokens:         Option<i64>,
    /// Wall-clock time of the full HTTP round-trip in milliseconds.
    pub duration_ms:           i64,
    /// Tokens served from the provider's prompt cache (already parsed by the client).
    pub cache_read_tokens:     Option<i64>,
    /// Tokens written into the provider's prompt cache (Anthropic only).
    pub cache_creation_tokens: Option<i64>,
}

// ── Writes ────────────────────────────────────────────────────────────────────

pub async fn insert(pool: &SqlitePool, row: LlmRequestRow) -> Result<i64> {
    let id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO llm_requests (
            session_id, stack_id, model_name,
            request_json, request_headers,
            response_json, response_headers,
            error_text, input_tokens, output_tokens, duration_ms,
            cache_read_tokens, cache_creation_tokens
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(row.session_id)
    .bind(row.stack_id)
    .bind(&row.model_name)
    .bind(&row.request_json)
    .bind(&row.request_headers)
    .bind(&row.response_json)
    .bind(&row.response_headers)
    .bind(&row.error_text)
    .bind(row.input_tokens)
    .bind(row.output_tokens)
    .bind(row.duration_ms)
    .bind(row.cache_read_tokens)
    .bind(row.cache_creation_tokens)
    .fetch_one(pool)
    .await?;

    Ok(id)
}

// ── Maintenance ───────────────────────────────────────────────────────────────

/// Deletes rows older than `retention_days` days. Returns the number of deleted rows.
pub async fn cleanup(pool: &SqlitePool, retention_days: u32) -> Result<u64> {
    let cutoff  = format!("-{retention_days} days");
    let deleted = sqlx::query(
        "DELETE FROM llm_requests WHERE created_at < datetime('now', ?)",
    )
    .bind(&cutoff)
    .execute(pool)
    .await?
    .rows_affected();

    Ok(deleted)
}
