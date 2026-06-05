/// TtsManager — DB-aware registry of Text-to-Speech providers.
///
/// Two kinds of providers coexist:
/// - **DB-backed**: rows in `tts_models`, built from `llm_providers` credentials.
///   Managed via `add_model` / `update_model` / `delete_model`. Loaded on startup
///   and after every mutation.
/// - **Plugin-registered**: ephemeral providers registered at runtime by plugins
///   (e.g. a local Kokoro or Piper TTS plugin). Not persisted — they disappear on plugin stop.
///
/// `get()` returns the first plugin provider if any is running, otherwise the
/// first DB-backed provider ordered by `priority ASC`.
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use sqlx::SqlitePool;
use tokio::sync::RwLock;
use tracing::{info, warn};

use async_trait::async_trait;

use crate::config::LlmProvider;
use crate::llm::LlmProviderRecord;
use crate::llm::db as llm_db;

use super::elevenlabs_tts::ElevenLabsTtsSynthesiser;
use super::openai_tts::OpenAiTtsSynthesiser;
use super::{TextToSpeech, TtsModelInfo, TtsModelRecord};
use super::db as tts_db;

pub use core_api::tts::{TtsProvider, TtsRegistry};

// ── Internal state ────────────────────────────────────────────────────────────

struct TtsSlot {
    record:      TtsModelRecord,
    provider:    LlmProviderRecord,
    synthesiser: Arc<dyn TextToSpeech>,
}

struct ManagerState {
    /// DB-backed synthesisers, ordered by priority ASC. Rebuilt on every reload().
    db_slots: Vec<TtsSlot>,
    /// Plugin-registered providers (ephemeral — not in DB).
    plugins:  Vec<Arc<dyn TextToSpeech>>,
}

// ── TtsManager ────────────────────────────────────────────────────────────────

pub struct TtsManager {
    pool:  Arc<SqlitePool>,
    state: RwLock<ManagerState>,
}

impl TtsManager {
    pub async fn new(pool: Arc<SqlitePool>) -> Result<Arc<Self>> {
        let mgr = Arc::new(Self {
            pool,
            state: RwLock::new(ManagerState {
                db_slots: Vec::new(),
                plugins:  Vec::new(),
            }),
        });
        mgr.reload().await?;
        Ok(mgr)
    }

    // ── Resolution ────────────────────────────────────────────────────────────

    /// Returns the first available synthesiser:
    /// plugin-registered providers take precedence over DB-backed ones.
    pub async fn get(&self) -> Option<Arc<dyn TextToSpeech>> {
        let state = self.state.read().await;
        if let Some(p) = state.plugins.first() {
            return Some(Arc::clone(p));
        }
        state.db_slots.first().map(|s| Arc::clone(&s.synthesiser))
    }

    // ── Plugin registration (ephemeral) ───────────────────────────────────────

    /// Register an ephemeral provider. If a provider with the same `id()` is
    /// already present it is replaced.
    pub async fn register(&self, provider: Arc<dyn TextToSpeech>) {
        let mut state = self.state.write().await;
        let id = provider.id().to_string();
        state.plugins.retain(|p| p.id() != id);
        state.plugins.push(provider);
        info!(provider = %id, "tts provider registered (ephemeral)");
    }

    /// Deregister an ephemeral provider by id. No-op if not found.
    pub async fn unregister(&self, id: &str) {
        let mut state = self.state.write().await;
        let before = state.plugins.len();
        state.plugins.retain(|p| p.id() != id);
        if state.plugins.len() < before {
            info!(provider = %id, "tts provider unregistered (ephemeral)");
        }
    }

    // ── Model CRUD (DB-backed) ────────────────────────────────────────────────

    pub async fn add_model(&self, record: TtsModelRecord) -> Result<i64> {
        let id = tts_db::insert(&self.pool, &record).await?;
        self.reload().await?;
        Ok(id)
    }

    pub async fn update_model(&self, id: i64, record: TtsModelRecord) -> Result<()> {
        tts_db::update(&self.pool, id, &record).await?;
        self.reload().await
    }

    pub async fn delete_model(&self, id: i64) -> Result<()> {
        tts_db::soft_delete(&self.pool, id).await?;
        self.reload().await
    }

    pub async fn get_model(&self, id: i64) -> Option<TtsModelRecord> {
        self.state.read().await
            .db_slots.iter()
            .find(|s| s.record.id == id)
            .map(|s| s.record.clone())
    }

    pub async fn list_models_info(&self) -> Vec<TtsModelInfo> {
        self.state.read().await.db_slots.iter().map(|s| TtsModelInfo {
            id:            s.record.id,
            provider_id:   s.provider.id,
            provider_name: s.provider.name.clone(),
            model_id:      s.record.model_id.clone(),
            name:          s.record.name.clone(),
            description:   s.record.description.clone(),
            instructions:  s.record.instructions.clone(),
            priority:      s.record.priority,
            from_plugin:   false,
        }).collect()
    }

    /// Returns all active providers: plugin-registered first, then DB-backed.
    pub async fn list_all_info(&self) -> Vec<TtsModelInfo> {
        let state = self.state.read().await;

        let plugins = state.plugins.iter().map(|p| TtsModelInfo {
            id:            0,
            provider_id:   0,
            provider_name: "Plugin".into(),
            model_id:      p.id().to_string(),
            name:          p.name().to_string(),
            description:   p.description().map(str::to_string),
            instructions:  p.instructions().map(str::to_string),
            priority:      0,
            from_plugin:   true,
        });

        let db = state.db_slots.iter().map(|s| TtsModelInfo {
            id:            s.record.id,
            provider_id:   s.provider.id,
            provider_name: s.provider.name.clone(),
            model_id:      s.record.model_id.clone(),
            name:          s.record.name.clone(),
            description:   s.record.description.clone(),
            instructions:  s.record.instructions.clone(),
            priority:      s.record.priority,
            from_plugin:   false,
        });

        plugins.chain(db).collect()
    }

    // ── Private ───────────────────────────────────────────────────────────────

    async fn reload(&self) -> Result<()> {
        let model_records: Vec<TtsModelRecord> =
            tts_db::load_all(&self.pool).await?;
        let provider_records: Vec<LlmProviderRecord> =
            llm_db::load_all_providers(&self.pool).await?;

        let providers: std::collections::HashMap<i64, LlmProviderRecord> =
            provider_records.into_iter().map(|p| (p.id, p)).collect();

        let mut db_slots = Vec::new();

        for model in model_records {
            let provider = match providers.get(&model.provider_id) {
                Some(p) => p.clone(),
                None => {
                    warn!(
                        model = %model.name,
                        provider_id = model.provider_id,
                        "orphaned tts model — provider not found, skipping",
                    );
                    continue;
                }
            };

            match build_synthesiser(&provider, &model) {
                Ok(synthesiser) => db_slots.push(TtsSlot { record: model, provider, synthesiser }),
                Err(e) => warn!(model = %model.name, error = %e, "failed to build tts synthesiser, skipping"),
            }
        }

        let slot_count = db_slots.len();
        self.state.write().await.db_slots = db_slots;

        info!(db_backed = slot_count, "tts manager reloaded");
        Ok(())
    }
}

// ── TtsProvider / TtsRegistry impls ──────────────────────────────────────────

#[async_trait]
impl TtsProvider for TtsManager {
    async fn get(&self) -> Option<Arc<dyn TextToSpeech>> {
        TtsManager::get(self).await
    }
}

#[async_trait]
impl TtsRegistry for TtsManager {
    async fn register(&self, provider: Arc<dyn TextToSpeech>) {
        TtsManager::register(self, provider).await
    }

    async fn unregister(&self, id: &str) {
        TtsManager::unregister(self, id).await
    }
}

// ── Builder ───────────────────────────────────────────────────────────────────

fn build_synthesiser(
    provider: &LlmProviderRecord,
    model:    &TtsModelRecord,
) -> Result<Arc<dyn TextToSpeech>> {
    let (base_url, api_key) = match provider.provider {
        LlmProvider::OpenAi => (
            provider.base_url.clone()
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
            provider.api_key.clone()
                .with_context(|| format!("provider '{}': api_key required for open_ai", provider.name))?,
        ),
        LlmProvider::OpenRouter => (
            provider.base_url.clone()
                .unwrap_or_else(|| "https://openrouter.ai/api/v1".to_string()),
            provider.api_key.clone()
                .with_context(|| format!("provider '{}': api_key required for openrouter", provider.name))?,
        ),
        LlmProvider::ElevenLabs => {
            let api_key = provider.api_key.clone()
                .with_context(|| format!("provider '{}': api_key required for elevenlabs", provider.name))?;
            return Ok(Arc::new(ElevenLabsTtsSynthesiser::new(
                &model.name,
                api_key,
                &model.model_id,
                model.instructions.clone(),
            )));
        }
        other => anyhow::bail!(
            "provider type '{:?}' does not support text-to-speech", other
        ),
    };

    Ok(Arc::new(OpenAiTtsSynthesiser::new(
        &model.name,
        base_url,
        api_key,
        &model.model_id,
        model.instructions.clone(),
    )))
}
