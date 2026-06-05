use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use indexmap::IndexMap;
use sqlx::SqlitePool;
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::chatbot::ChatbotClient;
use crate::chatbot::anthropic::AnthropicClient;
use crate::chatbot::lm_studio::LmStudioClient;
use crate::chatbot::logging::LoggingChatbotClient;
use crate::chatbot::ollama::OllamaClient;
use crate::chatbot::openai::OpenAiClient;
use crate::config::{LlmProvider, LlmStrength};

use super::providers::{RemoteModelInfo, build_caps};
use super::{ClientStatus, LlmEntry, LlmModelInfo, LlmModelRecord, LlmProviderInfo, LlmProviderRecord};
use super::db;

const FAILURE_DEGRADED: u32 = 3;
const FAILURE_DOWN:     u32 = 5;
const CATALOG_TTL: Duration    = Duration::from_secs(24 * 60 * 60);
const MODEL_META_TTL: Duration = Duration::from_secs(60 * 60); // 1 hour

pub const AUTO_CLIENT: &str = "auto";

struct CachedCatalog {
    models:     Vec<RemoteModelInfo>,
    fetched_at: Instant,
}

struct CachedModelMeta {
    info:       RemoteModelInfo,
    fetched_at: Instant,
}

struct HealthState {
    status:               ClientStatus,
    consecutive_failures: u32,
    last_error:           Option<String>,
}

impl Default for HealthState {
    fn default() -> Self {
        Self { status: ClientStatus::Healthy, consecutive_failures: 0, last_error: None }
    }
}

struct ModelSlot {
    provider: LlmProviderRecord,
    model:    LlmModelRecord,
    entry:    Arc<LlmEntry>,
    health:   HealthState,
}

struct ManagerState {
    /// Keyed by model.name, ordered by priority ASC.
    models:   IndexMap<String, ModelSlot>,
    /// Keyed by provider.id.
    providers: IndexMap<i64, LlmProviderRecord>,
    default:  String,
}

pub struct LlmManager {
    pool:               Arc<SqlitePool>,
    state:              RwLock<ManagerState>,
    /// In-memory model catalog cache, keyed by provider_id. TTL = 24h.
    catalog:            RwLock<HashMap<i64, CachedCatalog>>,
    /// Per-model metadata cache, keyed by model display name. TTL = 1h.
    /// Lazily populated by `resolve()` / `select()`.
    model_meta_cache:   RwLock<HashMap<String, CachedModelMeta>>,
    /// When true, every LLM entry is wrapped with [`LoggingChatbotClient`].
    request_log_enabled: bool,
}

impl LlmManager {
    pub async fn new(pool: Arc<SqlitePool>, request_log_enabled: bool) -> Result<Arc<Self>> {
        let mgr = Arc::new(Self {
            pool,
            state: RwLock::new(ManagerState {
                models:    IndexMap::new(),
                providers: IndexMap::new(),
                default:   String::new(),
            }),
            catalog:          RwLock::new(HashMap::new()),
            model_meta_cache: RwLock::new(HashMap::new()),
            request_log_enabled,
        });
        mgr.reload().await?;
        Ok(mgr)
    }

    // ── Public: resolution ────────────────────────────────────────────────────

    pub async fn resolve(
        &self,
        client_name:       Option<&str>,
        required_scope:    Option<&str>,
        required_strength: Option<LlmStrength>,
    ) -> Result<(String, Arc<LlmEntry>)> {
        let name = match client_name {
            None | Some(AUTO_CLIENT) => {
                let (name, entry) = self.select(required_scope, required_strength).await?;
                self.maybe_refresh_meta(&name).await;
                return Ok((name, entry));
            }
            Some(n) => {
                let state = self.state.read().await;
                if !state.models.contains_key(n) {
                    anyhow::bail!("LLM model '{n}' not found");
                }
                n.to_string()
            }
        };
        self.maybe_refresh_meta(&name).await;
        let state = self.state.read().await;
        let entry = state.models.get(&name).map(|s| s.entry.clone())
            .with_context(|| format!("LLM model '{name}' not found after refresh"))?;
        Ok((name, entry))
    }

    /// If the per-model metadata cache is stale (or missing) for `name`,
    /// fetch fresh data from the provider and update the entry if successful.
    /// Falls back to the old cache / DB value on error.
    async fn maybe_refresh_meta(&self, name: &str) {
        // Fast path: check cache under read lock.
        {
            let cache = self.model_meta_cache.read().await;
            if let Some(entry) = cache.get(name) {
                if entry.fetched_at.elapsed() < MODEL_META_TTL {
                    return;
                }
            }
        }

        // Cache miss or stale: read provider_id and model_id from state.
        let (provider_id, model_id) = {
            let state = self.state.read().await;
            match state.models.get(name) {
                Some(slot) => (slot.provider.id, slot.model.model_id.clone()),
                None => return,
            }
        };

        // Try to fetch fresh metadata from the provider.
        let caps: Arc<dyn super::providers::ProviderCaps> = match self.provider_caps(provider_id).await {
            Some(Ok(c)) => c,
            _ => return,
        };

        let remote: RemoteModelInfo = match caps.model_info(&model_id).await {
            Ok(Some(m)) => m,
            _ => return,
        };

        // Update cache and model slot.
        let now = Instant::now();
        let mut cache = self.model_meta_cache.write().await;
        cache.insert(name.to_string(), CachedModelMeta { info: remote.clone(), fetched_at: now });

        // Update the live entry's context_length if the provider returned it.
        if let Some(ctx) = remote.context_length {
            let mut state = self.state.write().await;
            if let Some(slot) = state.models.get_mut(name) {
                let old_ctx = slot.entry.context_length;
                if Some(ctx as i64) != old_ctx {
                    slot.entry = Arc::new(LlmEntry {
                        context_length: Some(ctx as i64),
                        ..(*slot.entry).clone()
                    });
                }
            }
        }
    }

    pub async fn get(&self, name: &str) -> Option<Arc<LlmEntry>> {
        self.state.read().await.models.get(name).map(|s| s.entry.clone())
    }

    pub async fn default_name(&self) -> String {
        self.state.read().await.default.clone()
    }

    /// Returns ["auto", <model1>, <model2>, …] for the frontend selector.
    pub async fn client_names(&self) -> Vec<String> {
        let mut names = vec![AUTO_CLIENT.to_string()];
        names.extend(self.state.read().await.models.keys().cloned());
        names
    }

    // ── Public: health reporting ──────────────────────────────────────────────

    pub async fn mark_success(&self, name: &str) {
        let mut state = self.state.write().await;
        if let Some(slot) = state.models.get_mut(name) {
            let h = &mut slot.health;
            if h.consecutive_failures > 0 {
                info!(model = name, "LLM model recovered");
            }
            h.consecutive_failures = 0;
            h.last_error           = None;
            h.status               = ClientStatus::Healthy;
        }
    }

    pub async fn mark_failure(&self, name: &str, error: &str) {
        let mut state = self.state.write().await;
        if let Some(slot) = state.models.get_mut(name) {
            let h = &mut slot.health;
            h.consecutive_failures += 1;
            h.last_error = Some(error.to_string());
            h.status = if h.consecutive_failures >= FAILURE_DOWN {
                warn!(model = name, failures = h.consecutive_failures, "LLM model marked DOWN");
                ClientStatus::Down
            } else if h.consecutive_failures >= FAILURE_DEGRADED {
                warn!(model = name, failures = h.consecutive_failures, "LLM model marked DEGRADED");
                ClientStatus::Degraded
            } else {
                ClientStatus::Healthy
            };
        }
    }

    // ── Public: provider CRUD ─────────────────────────────────────────────────

    pub async fn add_provider(&self, record: LlmProviderRecord) -> Result<i64> {
        let id = db::insert_provider(&self.pool, &record).await?;
        self.reload().await?;
        Ok(id)
    }

    pub async fn update_provider(&self, id: i64, record: LlmProviderRecord) -> Result<()> {
        db::update_provider(&self.pool, id, &record).await?;
        self.reload().await
    }

    pub async fn delete_provider(&self, id: i64) -> Result<()> {
        db::delete_provider(&self.pool, id).await?;
        self.reload().await
    }

    pub async fn get_provider(&self, id: i64) -> Option<LlmProviderRecord> {
        self.state.read().await.providers.get(&id).cloned()
    }

    /// Returns the capability interface for the given provider, or None if not found.
    pub async fn provider_caps(&self, id: i64) -> Option<anyhow::Result<Arc<dyn super::providers::ProviderCaps>>> {
        let record = self.state.read().await.providers.get(&id).cloned()?;
        Some(super::providers::build_caps(&record))
    }

    /// Returns the remote model catalog for a provider, using a 24h in-memory cache.
    /// After fetching, syncs `context_length`, `max_output_tokens`, `knowledge_cutoff`,
    /// and `capabilities` to existing DB model records.
    pub async fn list_provider_models(&self, id: i64) -> Result<Vec<RemoteModelInfo>> {
        // Fast path: check cache under read lock.
        {
            let cache = self.catalog.read().await;
            if let Some(entry) = cache.get(&id) {
                if entry.fetched_at.elapsed() < CATALOG_TTL {
                    return Ok(entry.models.clone());
                }
            }
        }

        // Cache miss or expired: build caps, fetch, store.
        let caps = self.provider_caps(id).await
            .ok_or_else(|| anyhow::anyhow!("provider {id} not found"))??;

        let models = caps.list_models().await?
            .ok_or_else(|| anyhow::anyhow!("this provider does not support model listing"))?;

        // Sync catalog metadata to existing DB model records.
        for remote in &models {
            db::update_model_metadata(
                &self.pool, id, &remote.id,
                remote.context_length.map(|v| v as i64),
                remote.max_completion_tokens.map(|v| v as i64),
                remote.knowledge_cutoff.as_deref(),
                &remote.capabilities,
            ).await.ok();
        }

        self.catalog.write().await.insert(id, CachedCatalog {
            models:     models.clone(),
            fetched_at: Instant::now(),
        });

        Ok(models)
    }

    pub async fn list_providers_info(&self) -> Vec<LlmProviderInfo> {
        self.state.read().await.providers.values().map(|p| {
            let supported_types = build_caps(p)
                .map(|caps| caps.supported_types().to_vec())
                .unwrap_or_default();
            LlmProviderInfo {
                id:              p.id,
                name:            p.name.clone(),
                provider:        p.provider,
                base_url:        p.base_url.clone(),
                description:     p.description.clone(),
                supported_types,
            }
        }).collect()
    }

    // ── Public: model CRUD ────────────────────────────────────────────────────

    pub async fn add_model(&self, model: LlmModelRecord) -> Result<i64> {
        if model.is_default {
            db::clear_default(&self.pool).await?;
        }
        let id = db::insert_model(&self.pool, &model).await?;
        self.reload().await?;
        Ok(id)
    }

    pub async fn update_model(&self, id: i64, model: LlmModelRecord) -> Result<()> {
        if model.is_default {
            db::clear_default(&self.pool).await?;
        }
        db::update_model(&self.pool, id, &model).await?;
        self.reload().await
    }

    pub async fn delete_model(&self, id: i64) -> Result<()> {
        db::delete_model(&self.pool, id).await?;
        self.reload().await
    }

    pub async fn get_model(&self, id: i64) -> Option<LlmModelRecord> {
        self.state.read().await.models.values()
            .find(|s| s.model.id == id)
            .map(|s| s.model.clone())
    }

    pub async fn list_models_info(&self) -> Vec<LlmModelInfo> {
        let state   = self.state.read().await;
        let catalog = self.catalog.read().await;
        state.models.values().map(|slot| {
            let cached = catalog.get(&slot.provider.id)
                .and_then(|c| c.models.iter().find(|m| m.id == slot.model.model_id));
            LlmModelInfo {
                id:                       slot.model.id,
                provider_id:              slot.provider.id,
                provider_name:            slot.provider.name.clone(),
                model_id:                 slot.model.model_id.clone(),
                name:                     slot.model.name.clone(),
                strength:                 slot.model.strength,
                scope:                    slot.model.scope.clone(),
                is_default:               slot.model.is_default,
                priority:                 slot.model.priority,
                extra_params:             slot.model.extra_params.clone(),
                context_length:           slot.model.context_length,
                max_output_tokens:        slot.model.max_output_tokens,
                knowledge_cutoff:         slot.model.knowledge_cutoff.clone(),
                capabilities:             slot.model.capabilities.clone(),
                status:                   slot.health.status,
                last_error:               slot.health.last_error.clone(),
                price_input_per_million:  cached.and_then(|m| m.price_input_per_million),
                price_output_per_million: cached.and_then(|m| m.price_output_per_million),
            }
        }).collect()
    }

    // ── Private ───────────────────────────────────────────────────────────────

    async fn reload(&self) -> Result<()> {
        let provider_records = db::load_all_providers(&self.pool).await?;
        let model_records    = db::load_all_models(&self.pool).await?;

        let providers: IndexMap<i64, LlmProviderRecord> = provider_records
            .into_iter()
            .map(|p| (p.id, p))
            .collect();

        let mut models: IndexMap<String, ModelSlot> = IndexMap::new();
        let mut default = String::new();

        for model in model_records {
            let provider = match providers.get(&model.provider_id) {
                Some(p) => p.clone(),
                None    => {
                    warn!(model = %model.name, provider_id = model.provider_id, "orphaned model — provider not found, skipping");
                    continue;
                }
            };

            let log_pool = if self.request_log_enabled {
                Some(Arc::clone(&self.pool))
            } else {
                None
            };

            let entry = match build_entry(&provider, &model, model.id, log_pool) {
                Ok(e)  => Arc::new(e),
                Err(e) => {
                    warn!(model = %model.name, error = %e, "failed to build LLM entry, skipping");
                    continue;
                }
            };

            if model.is_default || default.is_empty() {
                default = model.name.clone();
            }

            models.insert(model.name.clone(), ModelSlot {
                provider,
                model,
                entry,
                health: HealthState::default(),
            });
        }

        let mut state = self.state.write().await;
        // Preserve health state for models that already existed.
        for (name, slot) in state.models.iter() {
            if let Some(new_slot) = models.get_mut(name) {
                new_slot.health.status               = slot.health.status;
                new_slot.health.consecutive_failures = slot.health.consecutive_failures;
                new_slot.health.last_error           = slot.health.last_error.clone();
            }
        }
        state.models    = models;
        state.providers = providers;
        state.default   = default;
        Ok(())
    }

    /// Like `select`, but skips any model whose name is in `excluded`.
    /// Used by the fallback logic in `llm_loop` to try the next best model
    /// after the current one fails.
    pub async fn select_excluding(
        &self,
        excluded:          &[&str],
        required_scope:    Option<&str>,
        required_strength: Option<LlmStrength>,
    ) -> Result<(String, Arc<LlmEntry>)> {
        let state = self.state.read().await;
        let mut slots: Vec<(&String, &ModelSlot)> = state.models.iter()
            .filter(|(name, _)| !excluded.contains(&name.as_str()))
            .collect();
        if slots.is_empty() {
            anyhow::bail!("no alternative LLM models available");
        }
        sort_slots_for_agent(&mut slots, required_scope, required_strength);
        if let Some((name, slot)) = slots.iter().find(|(_, s)| s.health.status != ClientStatus::Down) {
            return Ok((name.to_string(), slot.entry.clone()));
        }
        if let Some((name, slot)) = slots.first() {
            warn!(model = %name, "all alternative LLM models are DOWN — using best available");
            return Ok((name.to_string(), slot.entry.clone()));
        }
        anyhow::bail!("no alternative LLM models available");
    }

    async fn select(
        &self,
        required_scope:    Option<&str>,
        required_strength: Option<LlmStrength>,
    ) -> Result<(String, Arc<LlmEntry>)> {
        let state = self.state.read().await;

        if state.models.is_empty() {
            anyhow::bail!("no LLM models configured — add one via the UI");
        }

        // Build a sorted order using the same logic as the UI preview, then pick
        // the first slot that is not Down. Emergency fallback if all are Down.
        let mut slots: Vec<(&String, &ModelSlot)> = state.models.iter().collect();
        sort_slots_for_agent(&mut slots, required_scope, required_strength);

        if let Some((name, slot)) = slots.iter().find(|(_, s)| s.health.status != ClientStatus::Down) {
            return Ok((name.to_string(), slot.entry.clone()));
        }

        // Emergency: all Down — use the first in sorted order (strongest match).
        if let Some((name, slot)) = slots.first() {
            warn!(model = %name, "all LLM models are DOWN — using strongest as emergency fallback");
            return Ok((name.to_string(), slot.entry.clone()));
        }

        anyhow::bail!("no LLM models available");
    }
}

/// Sort a slice of model infos in the order the system would try them for an
/// agent with the given scope/strength requirements. Does not filter anything out;
/// well-matched models come first, fallbacks at the end.
///
/// Order within each tier is by `priority ASC` (lower number = higher priority).
///
/// Tiers:
///   0 — strength ≥ required AND scope matches
///   1 — strength ≥ required (scope relaxed)
///   2 — anything else (no requirement match)
pub fn sort_models_for_agent(
    mut models: Vec<LlmModelInfo>,
    scope:      Option<&str>,
    strength:   Option<LlmStrength>,
) -> Vec<LlmModelInfo> {
    models.sort_by_key(|m| (model_tier(m.strength, m.scope.as_slice(), scope, strength), m.priority));
    models
}

fn sort_slots_for_agent(
    slots:    &mut Vec<(&String, &ModelSlot)>,
    scope:    Option<&str>,
    strength: Option<LlmStrength>,
) {
    slots.sort_by_key(|(_, s)| (
        model_tier(s.model.strength, s.model.scope.as_slice(), scope, strength),
        s.model.priority,
    ));
}

fn model_tier(
    model_strength: Option<LlmStrength>,
    model_scope:    &[String],
    req_scope:      Option<&str>,
    req_strength:   Option<LlmStrength>,
) -> u8 {
    let strength_ok = match (req_strength, model_strength) {
        (Some(req), Some(avail)) => avail >= req,
        (Some(_), None)          => false,
        (None, _)                => true,
    };
    let scope_ok = req_scope.map_or(true, |sc| model_scope.iter().any(|x| x == sc));
    match (strength_ok && scope_ok, strength_ok) {
        (true, _)      => 0,
        (false, true)  => 1,
        _              => 2,
    }
}

/// When `log_pool` is `Some`, every call is transparently logged to `llm_requests`
/// via [`LoggingChatbotClient`]. Pass `None` to disable request logging.
fn build_entry(
    provider:    &LlmProviderRecord,
    model:       &LlmModelRecord,
    model_db_id: i64,
    log_pool:    Option<Arc<SqlitePool>>,
) -> Result<LlmEntry> {
    let extra = model.extra_params.clone();
    let prompt_cache;
    let inner: Arc<dyn ChatbotClient> = match provider.provider {
        LlmProvider::LmStudio => {
            prompt_cache = false;
            Arc::new(LmStudioClient::new(provider.base_url.as_deref()))
        }
        LlmProvider::Ollama => {
            prompt_cache = false;
            Arc::new(OllamaClient::new(provider.base_url.as_deref()))
        }
        LlmProvider::OpenAi => {
            prompt_cache = false;
            let key = provider.api_key.as_deref()
                .with_context(|| format!("provider '{}': api_key required for open_ai", provider.name))?;
            Arc::new(OpenAiClient::new("https://api.openai.com/v1", key, extra.clone(), false))
        }
        LlmProvider::OpenRouter => {
            // Anthropic prompt-caching (cache_control + anthropic-beta header) only
            // works when the model is actually served by Anthropic. For any other
            // provider routed through OpenRouter (Alibaba/DeepSeek, Meta, Mistral,
            // etc.) these hints are silently ignored — and the content-array format
            // for the system message may confuse non-Anthropic inference servers.
            // Check the model ID prefix: OpenRouter names Anthropic models as
            // "anthropic/<model-id>" (e.g. "anthropic/claude-opus-4-5").
            prompt_cache = model.model_id.starts_with("anthropic/");
            let key = provider.api_key.as_deref()
                .with_context(|| format!("provider '{}': api_key required for openrouter", provider.name))?;
            Arc::new(OpenAiClient::new("https://openrouter.ai/api/v1", key, extra.clone(), prompt_cache))
        }
        LlmProvider::Anthropic => {
            prompt_cache = false; // Anthropic direct: separate implementation path
            let key = provider.api_key.as_deref()
                .with_context(|| format!("provider '{}': api_key required for anthropic", provider.name))?;
            Arc::new(AnthropicClient::new(key))
        }
        LlmProvider::DeepSeek => {
            // KV cache is automatic prefix-based — no Anthropic-style markers needed.
            prompt_cache = false;
            let key = provider.api_key.as_deref()
                .with_context(|| format!("provider '{}': api_key required for deepseek", provider.name))?;
            Arc::new(OpenAiClient::new("https://api.deepseek.com/v1", key, extra.clone(), false))
        }
        LlmProvider::ElevenLabs => {
            anyhow::bail!(
                "provider '{}': ElevenLabs does not support LLM chat/completion — \
                 it can only be used for TTS and Transcription models",
                provider.name,
            )
        }
    };

    let client: Arc<dyn ChatbotClient> = match log_pool {
        Some(pool) => Arc::new(LoggingChatbotClient::new(inner, pool, &model.name)),
        None       => inner,
    };

    Ok(LlmEntry {
        client,
        model:          model.model_id.clone(),
        model_db_id,
        strength:       model.strength,
        scope:          model.scope.clone(),
        extra_params:   extra,
        context_length: model.context_length,
        prompt_cache,
    })
}
