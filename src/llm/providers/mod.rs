pub mod anthropic;
pub mod deepseek;
pub mod elevenlabs;
pub mod lm_studio;
pub mod ollama;
pub mod openai;
pub mod openrouter;

use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use crate::config::LlmProvider;
use crate::llm::LlmProviderRecord;

// ── ModelType ─────────────────────────────────────────────────────────────────

/// The kinds of models a provider can supply.
/// Hardcoded per provider implementation — not stored in the DB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelType {
    /// Text-in / text-out chat / completion models.
    Llm,
    /// Audio-in / text-out transcription models.
    Transcribe,
    /// Text-in / image-out generation models.
    ImageGenerate,
    /// Text-in / audio-out speech synthesis models.
    Tts,
}

// ── Common metadata ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
pub struct RemoteModelInfo {
    pub id:                       String,
    pub name:                     String,
    pub context_length:           Option<u64>,
    pub max_completion_tokens:    Option<u64>,
    /// Date string like "2024-09-01" — when the model's training data ends.
    pub knowledge_cutoff:         Option<String>,
    /// Supported capabilities (e.g. "function_calling", "vision", "streaming",
    /// "structured_output", "prompt_caching"). Mapped from provider-specific
    /// fields (e.g. OpenRouter's "supported_parameters").
    pub capabilities:             Vec<String>,
    /// Input (prompt) price per million tokens (USD).
    pub price_input_per_million:  Option<f64>,
    /// Output (completion) price per million tokens (USD).
    pub price_output_per_million: Option<f64>,
}

// ── Trait ─────────────────────────────────────────────────────────────────────

/// Provider-level capabilities: metadata queries that go beyond chat completions.
/// Returns `None` when a provider does not support the operation.
#[async_trait]
pub trait ProviderCaps: Send + Sync {
    /// Model types this provider supports. Statically declared per implementation.
    fn supported_types(&self) -> &'static [ModelType];

    /// Fetch the list of models available on this provider, with metadata.
    async fn list_models(&self) -> anyhow::Result<Option<Vec<RemoteModelInfo>>>;

    /// Fetch metadata for a single model by its provider-specific ID.
    /// Returns `None` if the provider does not support per-model queries.
    /// The default implementation returns `None`.
    async fn model_info(&self, _model_id: &str) -> anyhow::Result<Option<RemoteModelInfo>> {
        Ok(None)
    }
}

// ── Factory ───────────────────────────────────────────────────────────────────

pub fn build_caps(record: &LlmProviderRecord) -> Result<Arc<dyn ProviderCaps>> {
    let caps: Arc<dyn ProviderCaps> = match record.provider {
        LlmProvider::OpenRouter => {
            let api_key = record.api_key.clone()
                .ok_or_else(|| anyhow!("OpenRouter provider '{}' has no API key configured", record.name))?;
            Arc::new(openrouter::OpenRouterProvider::new(api_key))
        }
        LlmProvider::Ollama => Arc::new(ollama::OllamaProvider::new(
            record.base_url.clone().unwrap_or_else(|| "http://localhost:11434".into()),
        )),
        LlmProvider::LmStudio => Arc::new(lm_studio::LmStudioProvider::new(
            record.base_url.clone().unwrap_or_else(|| "http://localhost:1234/v1".into()),
        )),
        LlmProvider::OpenAi   => Arc::new(openai::OpenAiProvider),
        LlmProvider::Anthropic => {
            let api_key = record.api_key.clone()
                .ok_or_else(|| anyhow!("Anthropic provider '{}' has no API key configured", record.name))?;
            Arc::new(anthropic::AnthropicProvider::new(api_key))
        }
        LlmProvider::DeepSeek => {
            let api_key = record.api_key.clone()
                .ok_or_else(|| anyhow!("DeepSeek provider '{}' has no API key configured", record.name))?;
            Arc::new(deepseek::DeepSeekProvider::new(api_key))
        }
        LlmProvider::ElevenLabs => Arc::new(elevenlabs::ElevenLabsProvider),
    };
    Ok(caps)
}
