use axum::{
    Json,
    extract::{Path, State},
};

use crate::core::db::scheduled_jobs;
use std::sync::Arc;
use crate::core::skald::Skald;
use super::ApiError;

#[derive(serde::Serialize)]
pub struct JobResponse {
    pub id:          i64,
    pub title:       String,
    pub description: String,
    pub cron:        String,
    pub prompt:      String,
    pub agent_id:    String,
    pub enabled:     bool,
    pub single_run:  bool,
    pub kind:        String,
    pub last_run_at: Option<String>,
    pub next_run_at: Option<String>,
    pub created_at:  String,
}

pub async fn list(State(skald): State<Arc<Skald>>) -> Result<Json<Vec<JobResponse>>, ApiError> {
    let jobs = scheduled_jobs::list(&skald.db).await?;
    Ok(Json(jobs.into_iter().map(|j| JobResponse {
        id:          j.id,
        title:       j.title,
        description: j.description,
        cron:        j.cron,
        prompt:      j.prompt,
        agent_id:    j.agent_id,
        enabled:     j.enabled,
        single_run:  j.single_run,
        kind:        j.kind,
        last_run_at: j.last_run_at,
        next_run_at: j.next_run_at,
        created_at:  j.created_at,
    }).collect()))
}

pub async fn delete_job(
    Path(id): Path<i64>,
    State(skald): State<Arc<Skald>>,
) -> Result<(), ApiError> {
    let found = scheduled_jobs::delete(&skald.db, id).await?;
    if found { Ok(()) } else { Err(ApiError::not_found(format!("job {id} not found"))) }
}

pub async fn toggle(
    Path(id): Path<i64>,
    State(skald): State<Arc<Skald>>,
    Json(body): Json<serde_json::Value>,
) -> Result<(), ApiError> {
    let enabled = body["enabled"]
        .as_bool()
        .ok_or_else(|| ApiError::bad_request("'enabled' boolean required"))?;
    let found = scheduled_jobs::set_enabled(&skald.db, id, enabled).await?;
    if found { Ok(()) } else { Err(ApiError::not_found(format!("job {id} not found"))) }
}
