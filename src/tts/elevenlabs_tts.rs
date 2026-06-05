/// ElevenLabsTtsSynthesiser — cloud TTS via the ElevenLabs v1 API.
///
/// Endpoint: `POST https://api.elevenlabs.io/v1/text-to-speech/{voice_id}`
/// Auth:     `xi-api-key` header (not Bearer).
/// The `model_id` field in `tts_models` is treated as the ElevenLabs voice ID.
/// The ElevenLabs generation model is fixed to `eleven_multilingual_v2`.
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use tracing::{debug, info};

use super::TextToSpeech;

const EL_BASE_URL: &str = "https://api.elevenlabs.io/v1";
const EL_MODEL:    &str = "eleven_multilingual_v2";

pub struct ElevenLabsTtsSynthesiser {
    id:           String,
    api_key:      String,
    /// ElevenLabs voice ID (stored as `model_id` in the DB record).
    voice_id:     String,
    instructions: Option<String>,
    http:         reqwest::Client,
}

impl ElevenLabsTtsSynthesiser {
    pub fn new(
        id:           impl Into<String>,
        api_key:      impl Into<String>,
        voice_id:     impl Into<String>,
        instructions: Option<String>,
    ) -> Self {
        Self {
            id:       id.into(),
            api_key:  api_key.into(),
            voice_id: voice_id.into(),
            instructions,
            http:     reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl TextToSpeech for ElevenLabsTtsSynthesiser {
    fn id(&self)           -> &str         { &self.id }
    fn name(&self)         -> &str         { &self.id }
    fn instructions(&self) -> Option<&str> { self.instructions.as_deref() }

    async fn synthesize(&self, text: &str, _instructions: Option<&str>) -> Result<Vec<u8>> {
        debug!(
            chars    = text.len(),
            voice_id = %self.voice_id,
            "elevenlabs_tts: synthesising",
        );

        let url = format!("{EL_BASE_URL}/text-to-speech/{}", self.voice_id);

        let body = serde_json::json!({
            "text":     text,
            "model_id": EL_MODEL,
        });

        let resp = self.http
            .post(&url)
            .header("xi-api-key", &self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow!("elevenlabs_tts: request failed: {e}"))?;

        let status = resp.status();

        if !status.is_success() {
            let err: serde_json::Value = resp.json().await.unwrap_or_default();
            let msg = err["detail"]["message"].as_str()
                .or_else(|| err["detail"].as_str())
                .unwrap_or("unknown error");
            anyhow::bail!("elevenlabs_tts: API error {status}: {msg}");
        }

        let audio = resp
            .bytes()
            .await
            .map_err(|e| anyhow!("elevenlabs_tts: failed to read audio bytes: {e}"))?
            .to_vec();

        info!(bytes = audio.len(), voice_id = %self.voice_id, "elevenlabs_tts: synthesis complete");
        Ok(audio)
    }
}
