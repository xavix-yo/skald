use axum::{
    extract::{Query, State},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::db;
use crate::server::AppState;
use super::ApiError;

const KEY: &str = "DEBUG_MODE";

#[derive(Serialize)]
pub struct DebugModeResponse {
    pub enabled: bool,
}

#[derive(Deserialize)]
pub struct DebugModeBody {
    pub enabled: bool,
}

pub async fn get_debug_mode(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, ApiError> {
    let value = db::config::get(&state.db, KEY).await?;
    let enabled = value.as_deref() == Some("true");
    Ok(Json(DebugModeResponse { enabled }))
}

pub async fn set_debug_mode(
    State(state): State<AppState>,
    Json(body):   Json<DebugModeBody>,
) -> Result<impl IntoResponse, ApiError> {
    let value = if body.enabled { "true" } else { "false" };
    db::config::set(&state.db, KEY, value).await?;
    Ok(Json(DebugModeResponse { enabled: body.enabled }))
}

// ── LLM requests log ─────────────────────────────────────────────────────────

const PAGE_SIZE: i64 = 20;

#[derive(Deserialize)]
pub struct LlmRequestsQuery {
    pub agent_id: Option<String>,
    pub source:   Option<String>,
    pub from:     Option<String>,
    pub to:       Option<String>,
    pub page:     Option<i64>,
}

#[derive(Serialize)]
pub struct LlmRequestItem {
    pub id:                    i64,
    pub agent_id:              Option<String>,
    pub source:                Option<String>,
    pub model_name:            String,
    pub created_at:            String,
    pub input_tokens:          Option<i64>,
    pub output_tokens:         Option<i64>,
    pub cache_read_tokens:     Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    pub duration_ms:           i64,
    pub error_text:            Option<String>,
}

#[derive(Serialize)]
pub struct LlmRequestsResponse {
    pub items:     Vec<LlmRequestItem>,
    pub total:     i64,
    pub page:      i64,
    pub page_size: i64,
}

pub async fn list_llm_requests(
    State(state): State<AppState>,
    Query(params): Query<LlmRequestsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let page   = params.page.unwrap_or(1).max(1);
    let offset = (page - 1) * PAGE_SIZE;

    // Bind optional filters twice each: once for the IS NULL check, once for the
    // equality check. SQLite evaluates `? IS NULL` against the bound value itself.
    let items = sqlx::query_as::<_, (i64, Option<String>, Option<String>, String, String, Option<i64>, Option<i64>, Option<i64>, Option<i64>, i64, Option<String>)>(
        "SELECT
             r.id,
             s.agent_id,
             s.source,
             r.model_name,
             r.created_at,
             r.input_tokens,
             r.output_tokens,
             r.cache_read_tokens,
             r.cache_creation_tokens,
             r.duration_ms,
             r.error_text
         FROM llm_requests r
         LEFT JOIN chat_sessions s ON s.id = r.session_id
         WHERE (? IS NULL OR s.agent_id = ?)
           AND (? IS NULL OR s.source   = ?)
           AND (? IS NULL OR r.created_at >= ?)
           AND (? IS NULL OR r.created_at <= ?)
         ORDER BY r.created_at DESC
         LIMIT ? OFFSET ?",
    )
    .bind(&params.agent_id).bind(&params.agent_id)
    .bind(&params.source).bind(&params.source)
    .bind(&params.from).bind(&params.from)
    .bind(&params.to).bind(&params.to)
    .bind(PAGE_SIZE)
    .bind(offset)
    .fetch_all(&*state.db)
    .await?
    .into_iter()
    .map(|(id, agent_id, source, model_name, created_at, input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, duration_ms, error_text)| {
        LlmRequestItem { id, agent_id, source, model_name, created_at, input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, duration_ms, error_text }
    })
    .collect::<Vec<_>>();

    let total = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)
         FROM llm_requests r
         LEFT JOIN chat_sessions s ON s.id = r.session_id
         WHERE (? IS NULL OR s.agent_id = ?)
           AND (? IS NULL OR s.source   = ?)
           AND (? IS NULL OR r.created_at >= ?)
           AND (? IS NULL OR r.created_at <= ?)",
    )
    .bind(&params.agent_id).bind(&params.agent_id)
    .bind(&params.source).bind(&params.source)
    .bind(&params.from).bind(&params.from)
    .bind(&params.to).bind(&params.to)
    .fetch_one(&*state.db)
    .await?;

    Ok(Json(LlmRequestsResponse { items, total, page, page_size: PAGE_SIZE }))
}
