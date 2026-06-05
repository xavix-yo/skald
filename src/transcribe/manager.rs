/// TranscribeManager — DB-aware registry of Speech-to-Text providers.
///
/// Two kinds of providers coexist:
/// - **DB-backed**: rows in `transcribe_models`, built from `llm_providers` credentials.
///   Managed via `add_model` / `update_model` / `delete_model`. Loaded on startup
///   and after every mutation (like `LlmManager`).
/// - **Plugin-registered**: ephemeral providers registered at runtime by plugins
///   (e.g. `WhisperLocalPlugin`). Not persisted — they disappear on plugin stop.
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

use super::elevenlabs_audio::ElevenLabsTranscriber;
use super::openai_audio::OpenAiAudioTranscriber;
use super::{Transcribe, TranscribeModelInfo, TranscribeModelRecord};
use super::db as transcribe_db;

pub use core_api::transcribe::{TranscribeProvider, TranscribeRegistry};

// ── Internal state ────────────────────────────────────────────────────────────

struct TranscribeSlot {
    record:      TranscribeModelRecord,
    provider:    LlmProviderRecord,
    transcriber: Arc<dyn Transcribe>,
}

struct ManagerState {
    /// DB-backed transcribers, ordered by priority ASC. Rebuilt on every reload().
    db_slots: Vec<TranscribeSlot>,
    /// Plugin-registered providers (ephemeral — not in DB).
    /// `WhisperLocalPlugin` registers here via `register()`.
    plugins:  Vec<Arc<dyn Transcribe>>,
}

// ── TranscribeManager ─────────────────────────────────────────────────────────

pub struct TranscribeManager {
    pool:  Arc<SqlitePool>,
    state: RwLock<ManagerState>,
}

impl TranscribeManager {
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

    /// Returns the first available transcriber:
    /// plugin-registered providers take precedence over DB-backed ones.
    pub async fn get(&self) -> Option<Arc<dyn Transcribe>> {
        let state = self.state.read().await;
        if let Some(p) = state.plugins.first() {
            return Some(Arc::clone(p));
        }
        state.db_slots.first().map(|s| Arc::clone(&s.transcriber))
    }

    // ── Plugin registration (ephemeral) ───────────────────────────────────────

    /// Register an ephemeral provider. Called by plugins (e.g. WhisperLocalPlugin).
    /// If a provider with the same `id()` is already present it is replaced.
    pub async fn register(&self, provider: Arc<dyn Transcribe>) {
        let mut state = self.state.write().await;
        let id = provider.id().to_string();
        state.plugins.retain(|p| p.id() != id);
        state.plugins.push(provider);
        info!(provider = %id, "transcribe provider registered (ephemeral)");
    }

    /// Deregister an ephemeral provider by id. No-op if not found.
    pub async fn unregister(&self, id: &str) {
        let mut state = self.state.write().await;
        let before = state.plugins.len();
        state.plugins.retain(|p| p.id() != id);
        if state.plugins.len() < before {
            info!(provider = %id, "transcribe provider unregistered (ephemeral)");
        }
    }

    // ── Model CRUD (DB-backed) ────────────────────────────────────────────────

    pub async fn add_model(&self, record: TranscribeModelRecord) -> Result<i64> {
        let id = transcribe_db::insert(&self.pool, &record).await?;
        self.reload().await?;
        Ok(id)
    }

    pub async fn update_model(&self, id: i64, record: TranscribeModelRecord) -> Result<()> {
        transcribe_db::update(&self.pool, id, &record).await?;
        self.reload().await
    }

    pub async fn delete_model(&self, id: i64) -> Result<()> {
        transcribe_db::soft_delete(&self.pool, id).await?;
        self.reload().await
    }

    pub async fn get_model(&self, id: i64) -> Option<TranscribeModelRecord> {
        self.state.read().await
            .db_slots.iter()
            .find(|s| s.record.id == id)
            .map(|s| s.record.clone())
    }

    pub async fn list_models_info(&self) -> Vec<TranscribeModelInfo> {
        self.state.read().await.db_slots.iter().map(|s| TranscribeModelInfo {
            id:            s.record.id,
            provider_id:   s.provider.id,
            provider_name: s.provider.name.clone(),
            model_id:      s.record.model_id.clone(),
            name:          s.record.name.clone(),
            language:      s.record.language.clone(),
            priority:      s.record.priority,
            from_plugin:   false,
        }).collect()
    }

    /// Returns all active providers: plugin-registered first (they have precedence
    /// in `get()`), then DB-backed ordered by priority. Used by the UI.
    pub async fn list_all_info(&self) -> Vec<TranscribeModelInfo> {
        let state = self.state.read().await;

        let plugins = state.plugins.iter().map(|p| TranscribeModelInfo {
            id:            0,
            provider_id:   0,
            provider_name: "Plugin".into(),
            model_id:      p.id().to_string(),
            name:          p.id().to_string(),
            language:      None,
            priority:      0,
            from_plugin:   true,
        });

        let db = state.db_slots.iter().map(|s| TranscribeModelInfo {
            id:            s.record.id,
            provider_id:   s.provider.id,
            provider_name: s.provider.name.clone(),
            model_id:      s.record.model_id.clone(),
            name:          s.record.name.clone(),
            language:      s.record.language.clone(),
            priority:      s.record.priority,
            from_plugin:   false,
        });

        plugins.chain(db).collect()
    }

    // ── Private ───────────────────────────────────────────────────────────────

    async fn reload(&self) -> Result<()> {
        let model_records: Vec<TranscribeModelRecord> =
            transcribe_db::load_all(&self.pool).await?;
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
                        "orphaned transcribe model — provider not found, skipping",
                    );
                    continue;
                }
            };

            match build_transcriber(&provider, &model) {
                Ok(transcriber) => db_slots.push(TranscribeSlot { record: model, provider, transcriber }),
                Err(e) => warn!(model = %model.name, error = %e, "failed to build transcriber, skipping"),
            }
        }

        let slot_count = db_slots.len();

        // Acquire the write lock once, at the end — no more awaits after this.
        // Mirrors LlmManager::reload() to ensure the future stays Send.
        // Preserve existing plugin registrations — only replace db_slots.
        self.state.write().await.db_slots = db_slots;

        info!(db_backed = slot_count, "transcribe manager reloaded");
        Ok(())
    }
}

// ── TranscribeProvider / TranscribeRegistry impls ────────────────────────────

#[async_trait]
impl TranscribeProvider for TranscribeManager {
    async fn get(&self) -> Option<Arc<dyn Transcribe>> {
        TranscribeManager::get(self).await
    }
}

#[async_trait]
impl TranscribeRegistry for TranscribeManager {
    async fn register(&self, provider: Arc<dyn Transcribe>) {
        TranscribeManager::register(self, provider).await
    }

    async fn unregister(&self, id: &str) {
        TranscribeManager::unregister(self, id).await
    }
}

// ── Builder ───────────────────────────────────────────────────────────────────

fn build_transcriber(
    provider: &LlmProviderRecord,
    model:    &TranscribeModelRecord,
) -> Result<Arc<dyn Transcribe>> {
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
            return Ok(Arc::new(ElevenLabsTranscriber::new(
                &model.name,
                api_key,
                &model.model_id,
            )));
        }
        other => anyhow::bail!(
            "provider type '{:?}' does not support audio transcription", other
        ),
    };

    Ok(Arc::new(OpenAiAudioTranscriber::new(
        &model.name,
        base_url,
        api_key,
        &model.model_id,
        model.language.clone(),
    )))
}
