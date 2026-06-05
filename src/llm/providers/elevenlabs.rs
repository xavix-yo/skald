use async_trait::async_trait;
use anyhow::Result;

use super::{ModelType, ProviderCaps, RemoteModelInfo};

/// ElevenLabs supports TTS and Transcription only — no LLM chat/completion.
pub struct ElevenLabsProvider;

#[async_trait]
impl ProviderCaps for ElevenLabsProvider {
    fn supported_types(&self) -> &'static [ModelType] {
        &[ModelType::Tts, ModelType::Transcribe]
    }

    async fn list_models(&self) -> Result<Option<Vec<RemoteModelInfo>>> {
        Ok(None)
    }
}
