use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::core::skald::Skald;
use super::ApiError;

// ── Tool Permission Groups ────────────────────────────────────────────────────

pub async fn list_groups(
    State(skald): State<Arc<Skald>>,
) -> Result<Json<Value>, ApiError> {
    let groups = skald.run_context_manager.list_groups().await?;
    Ok(Json(json!(groups)))
}

#[derive(Deserialize)]
pub struct GroupBody {
    pub id:          String,
    pub name:        String,
    pub description: Option<String>,
}

pub async fn create_group(
    State(skald): State<Arc<Skald>>,
    Json(body): Json<GroupBody>,
) -> Result<Json<Value>, ApiError> {
    skald.run_context_manager.create_group(&body.id, &body.name, body.description.as_deref()).await?;
    Ok(Json(json!({ "id": body.id })))
}

#[derive(Deserialize)]
pub struct GroupPath { pub id: String }

#[derive(Deserialize)]
pub struct GroupUpdateBody {
    pub name:        String,
    pub description: Option<String>,
}

pub async fn update_group(
    State(skald): State<Arc<Skald>>,
    Path(p): Path<GroupPath>,
    Json(body): Json<GroupUpdateBody>,
) -> Result<Json<Value>, ApiError> {
    let found = skald.run_context_manager.update_group(&p.id, &body.name, body.description.as_deref()).await?;
    if !found {
        return Err(ApiError::not_found("permission group not found"));
    }
    Ok(Json(json!({ "ok": true })))
}

pub async fn delete_group(
    State(skald): State<Arc<Skald>>,
    Path(p): Path<GroupPath>,
) -> Result<StatusCode, ApiError> {
    skald.run_context_manager.delete_group(&p.id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct DuplicateGroupBody {
    pub id:   String,
    pub name: String,
}

pub async fn duplicate_group(
    State(skald): State<Arc<Skald>>,
    Path(p): Path<GroupPath>,
    Json(body): Json<DuplicateGroupBody>,
) -> Result<Json<Value>, ApiError> {
    skald.run_context_manager.duplicate_group(&p.id, &body.id, &body.name).await?;
    Ok(Json(json!({ "id": body.id })))
}

// ── Run Contexts ──────────────────────────────────────────────────────────────

pub async fn list_contexts(
    State(skald): State<Arc<Skald>>,
) -> Result<Json<Value>, ApiError> {
    let contexts = skald.run_context_manager.list_contexts().await?;
    Ok(Json(json!(contexts)))
}

#[derive(Deserialize)]
pub struct ContextBody {
    pub id:            String,
    pub name:          String,
    pub description:   Option<String>,
    pub tool_group_id: Option<String>,
}

pub async fn create_context(
    State(skald): State<Arc<Skald>>,
    Json(body): Json<ContextBody>,
) -> Result<Json<Value>, ApiError> {
    skald.run_context_manager.create_context(
        &body.id, &body.name, body.description.as_deref(), body.tool_group_id.as_deref(),
    ).await?;
    Ok(Json(json!({ "id": body.id })))
}

#[derive(Deserialize)]
pub struct ContextPath { pub id: String }

#[derive(Deserialize)]
pub struct ContextUpdateBody {
    pub name:          String,
    pub description:   Option<String>,
    pub tool_group_id: Option<String>,
}

pub async fn update_context(
    State(skald): State<Arc<Skald>>,
    Path(p): Path<ContextPath>,
    Json(body): Json<ContextUpdateBody>,
) -> Result<Json<Value>, ApiError> {
    let found = skald.run_context_manager.update_context(
        &p.id, &body.name, body.description.as_deref(), body.tool_group_id.as_deref(),
    ).await?;
    if !found {
        return Err(ApiError::not_found("run_context not found"));
    }
    Ok(Json(json!({ "ok": true })))
}

pub async fn delete_context(
    State(skald): State<Arc<Skald>>,
    Path(p): Path<ContextPath>,
) -> Result<StatusCode, ApiError> {
    skald.run_context_manager.delete_context(&p.id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ── Session run_context assignment ────────────────────────────────────────────

#[derive(Deserialize)]
pub struct SessionPath { pub session_id: i64 }

#[derive(Deserialize)]
pub struct SetRunContextBody {
    /// `null` removes the run_context assignment (falls back to default).
    pub run_context_id: Option<String>,
}

pub async fn set_session_run_context(
    State(skald): State<Arc<Skald>>,
    Path(p): Path<SessionPath>,
    Json(body): Json<SetRunContextBody>,
) -> Result<Json<Value>, ApiError> {
    let rc_id = body.run_context_id.as_deref();

    // Update the DB record.
    skald.run_context_manager.set_session_run_context(p.session_id, rc_id).await?;

    // If the session handler is live in memory, update it immediately.
    let active = skald.manager.active_handler(p.session_id).await;
    if let Some(handler) = active {
        let new_ctx = match rc_id {
            Some(id) => skald.run_context_manager.get_context(id).await.unwrap_or(None),
            None     => None,
        };
        handler.set_run_context(new_ctx).await;
    }

    Ok(Json(json!({ "ok": true })))
}
