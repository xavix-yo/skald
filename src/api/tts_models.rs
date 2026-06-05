use axum::{Json, extract::State, http::StatusCode};
use serde::Deserialize;

use crate::tts::{TtsModelInfo, TtsModelRecord};
use crate::server::AppState;
use super::ApiError;

// ── GET /api/tts/models ───────────────────────────────────────────────────────

pub async fn list_models(
    State(state): State<AppState>,
) -> Result<Json<Vec<TtsModelInfo>>, ApiError> {
    Ok(Json(state.tts_manager.list_all_info().await))
}

// ── POST /api/tts/models ──────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ModelPayload {
    pub provider_id:  i64,
    pub model_id:     String,
    pub name:         String,
    pub description:  Option<String>,
    pub instructions: Option<String>,
    pub priority:     Option<i32>,
}

impl From<ModelPayload> for TtsModelRecord {
    fn from(p: ModelPayload) -> Self {
        TtsModelRecord {
            id:           0,
            provider_id:  p.provider_id,
            model_id:     p.model_id.clone(),
            name:         if p.name.is_empty() { p.model_id } else { p.name },
            description:  p.description,
            instructions: p.instructions,
            priority:     p.priority.unwrap_or(100),
        }
    }
}

pub async fn create_model(
    State(state): State<AppState>,
    Json(payload): Json<ModelPayload>,
) -> Result<StatusCode, ApiError> {
    state.tts_manager.add_model(TtsModelRecord::from(payload)).await?;
    Ok(StatusCode::CREATED)
}

// ── GET /api/tts/models/{id} ──────────────────────────────────────────────────

pub async fn get_model(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<Json<TtsModelRecord>, ApiError> {
    state.tts_manager.get_model(id).await
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("tts model {id} not found")))
}

// ── PUT /api/tts/models/{id} ──────────────────────────────────────────────────

pub async fn update_model(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
    Json(payload): Json<ModelPayload>,
) -> Result<StatusCode, ApiError> {
    state.tts_manager.update_model(id, TtsModelRecord::from(payload)).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ── DELETE /api/tts/models/{id} ───────────────────────────────────────────────

pub async fn delete_model(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<StatusCode, ApiError> {
    state.tts_manager.delete_model(id).await?;
    Ok(StatusCode::NO_CONTENT)
}
