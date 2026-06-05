use std::time::Duration;

use axum::{Json, extract::State, http::StatusCode};
use serde::{Deserialize, Serialize};

use crate::config::{LlmProvider, LlmStrength};
use crate::llm::providers::RemoteModelInfo;
use crate::llm::{LlmModelInfo, LlmModelRecord, LlmProviderInfo, LlmProviderRecord};
use crate::server::AppState;
use super::ApiError;

// ── GET /api/llm/providers/{id}/models ───────────────────────────────────────

pub async fn provider_models(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<Json<Vec<RemoteModelInfo>>, ApiError> {
    let models = state.manager.llm_manager().list_provider_models(id).await?;
    Ok(Json(models))
}

// ── GET /api/llm/models/selector  (used by the copilot dropdown) ──────────────

#[derive(Serialize)]
pub struct SelectorResponse {
    pub models:  Vec<String>,
    pub default: String,
}

pub async fn selector(
    State(state): State<AppState>,
) -> Result<Json<SelectorResponse>, ApiError> {
    let mgr     = state.manager.llm_manager();
    let models  = mgr.client_names().await;
    let default = mgr.default_name().await;
    Ok(Json(SelectorResponse { models, default }))
}

// ── Providers ─────────────────────────────────────────────────────────────────

pub async fn list_providers(
    State(state): State<AppState>,
) -> Result<Json<Vec<LlmProviderInfo>>, ApiError> {
    Ok(Json(state.manager.llm_manager().list_providers_info().await))
}

#[derive(Deserialize)]
pub struct ProviderPayload {
    pub name:        String,
    #[serde(rename = "type")]
    pub provider:    String,
    pub api_key:     Option<String>,
    pub base_url:    Option<String>,
    pub description: Option<String>,
}

impl TryFrom<ProviderPayload> for LlmProviderRecord {
    type Error = ApiError;
    fn try_from(p: ProviderPayload) -> Result<Self, ApiError> {
        Ok(LlmProviderRecord {
            id:          0, // assigned by DB
            name:        p.name,
            provider:    parse_provider(&p.provider)?,
            api_key:     p.api_key,
            base_url:    p.base_url,
            description: p.description,
        })
    }
}

pub async fn create_provider(
    State(state): State<AppState>,
    Json(payload): Json<ProviderPayload>,
) -> Result<StatusCode, ApiError> {
    let record = LlmProviderRecord::try_from(payload)?;
    state.manager.llm_manager().add_provider(record).await?;
    Ok(StatusCode::CREATED)
}

pub async fn get_provider(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<Json<LlmProviderRecord>, ApiError> {
    state.manager.llm_manager().get_provider(id).await
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("provider {id} not found")))
}

pub async fn update_provider(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
    Json(payload): Json<ProviderPayload>,
) -> Result<StatusCode, ApiError> {
    let record = LlmProviderRecord::try_from(payload)?;
    state.manager.llm_manager().update_provider(id, record).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_provider(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<StatusCode, ApiError> {
    state.manager.llm_manager().delete_provider(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ── Models ────────────────────────────────────────────────────────────────────

pub async fn list_models(
    State(state): State<AppState>,
) -> Result<Json<Vec<LlmModelInfo>>, ApiError> {
    let mgr = state.manager.llm_manager();

    // Warm the catalog cache for every provider concurrently so that price data
    // is available for the join inside list_models_info(). Errors are ignored —
    // a provider that is down or lacks model listing just shows no price.
    let provider_ids: Vec<i64> = mgr.list_providers_info().await
        .into_iter().map(|p| p.id).collect();

    const PER_PROVIDER_TIMEOUT: Duration = Duration::from_secs(5);

    let mut tasks = tokio::task::JoinSet::new();
    for id in provider_ids {
        let mgr = mgr.clone();
        tasks.spawn(async move {
            let _ = tokio::time::timeout(
                PER_PROVIDER_TIMEOUT,
                mgr.list_provider_models(id),
            ).await;
        });
    }
    while tasks.join_next().await.is_some() {}

    Ok(Json(mgr.list_models_info().await))
}

#[derive(Deserialize)]
pub struct ModelPayload {
    pub provider_id:       i64,
    pub model_id:          String,
    pub name:              String,
    pub strength:          Option<String>,
    pub scope:             Option<Vec<String>>,
    pub is_default:        Option<bool>,
    pub priority:          Option<i32>,
    pub extra_params:      Option<serde_json::Value>,
    pub context_length:    Option<i64>,
    pub max_output_tokens: Option<i64>,
    pub knowledge_cutoff:  Option<String>,
    pub capabilities:      Option<Vec<String>>,
}

impl TryFrom<ModelPayload> for LlmModelRecord {
    type Error = ApiError;
    fn try_from(p: ModelPayload) -> Result<Self, ApiError> {
        Ok(LlmModelRecord {
            id:                0,
            provider_id:       p.provider_id,
            model_id:          p.model_id.clone(),
            name:              if p.name.is_empty() { p.model_id } else { p.name },
            strength:          p.strength.as_deref().map(parse_strength).transpose()?,
            scope:             p.scope.unwrap_or_default(),
            is_default:        p.is_default.unwrap_or(false),
            priority:          p.priority.unwrap_or(100),
            extra_params:      p.extra_params,
            context_length:    p.context_length,
            max_output_tokens: p.max_output_tokens,
            knowledge_cutoff:  p.knowledge_cutoff,
            capabilities:      p.capabilities.unwrap_or_default(),
        })
    }
}

pub async fn create_model(
    State(state): State<AppState>,
    Json(payload): Json<ModelPayload>,
) -> Result<StatusCode, ApiError> {
    let record = LlmModelRecord::try_from(payload)?;
    state.manager.llm_manager().add_model(record).await?;
    Ok(StatusCode::CREATED)
}

pub async fn get_model(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<Json<LlmModelRecord>, ApiError> {
    state.manager.llm_manager().get_model(id).await
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("model {id} not found")))
}

pub async fn update_model(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
    Json(payload): Json<ModelPayload>,
) -> Result<StatusCode, ApiError> {
    let record = LlmModelRecord::try_from(payload)?;
    state.manager.llm_manager().update_model(id, record).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_model(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<StatusCode, ApiError> {
    state.manager.llm_manager().delete_model(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn parse_provider(s: &str) -> Result<LlmProvider, ApiError> {
    match s {
        "lm_studio"  => Ok(LlmProvider::LmStudio),
        "ollama"     => Ok(LlmProvider::Ollama),
        "open_ai"    => Ok(LlmProvider::OpenAi),
        "openrouter" => Ok(LlmProvider::OpenRouter),
        "anthropic"  => Ok(LlmProvider::Anthropic),
        "deepseek"   => Ok(LlmProvider::DeepSeek),
        "elevenlabs" => Ok(LlmProvider::ElevenLabs),
        other        => Err(ApiError::bad_request(format!("unknown provider type '{other}'"))),
    }
}

fn parse_strength(s: &str) -> Result<LlmStrength, ApiError> {
    match s {
        "very_low"  => Ok(LlmStrength::VeryLow),
        "low"       => Ok(LlmStrength::Low),
        "average"   => Ok(LlmStrength::Average),
        "high"      => Ok(LlmStrength::High),
        "very_high" => Ok(LlmStrength::VeryHigh),
        other       => Err(ApiError::bad_request(format!("unknown strength '{other}'"))),
    }
}
