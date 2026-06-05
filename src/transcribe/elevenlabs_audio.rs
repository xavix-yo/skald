/// ElevenLabsTranscriber — cloud Speech-to-Text via the ElevenLabs Scribe API.
///
/// Endpoint: `POST https://api.elevenlabs.io/v1/speech-to-text`
/// Auth:     `xi-api-key` header (not Bearer).
/// The `model_id` field in `transcribe_models` maps to the ElevenLabs model
/// (e.g. `scribe_v1`).
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use tracing::{debug, info};

use super::Transcribe;

const EL_BASE_URL: &str = "https://api.elevenlabs.io/v1";

pub struct ElevenLabsTranscriber {
    id:       String,
    api_key:  String,
    model_id: String,
    http:     reqwest::Client,
}

impl ElevenLabsTranscriber {
    pub fn new(
        id:       impl Into<String>,
        api_key:  impl Into<String>,
        model_id: impl Into<String>,
    ) -> Self {
        Self {
            id:       id.into(),
            api_key:  api_key.into(),
            model_id: model_id.into(),
            http:     reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl Transcribe for ElevenLabsTranscriber {
    fn id(&self) -> &str { &self.id }

    async fn transcribe(&self, audio: Vec<u8>, format: &str) -> Result<String> {
        debug!(
            bytes    = audio.len(),
            format   = %format,
            model_id = %self.model_id,
            "elevenlabs_transcribe: transcribing",
        );

        let url = format!("{EL_BASE_URL}/speech-to-text");

        let filename = format!("audio.{format}");
        let part = reqwest::multipart::Part::bytes(audio)
            .file_name(filename)
            .mime_str("audio/wav")?;

        let form = reqwest::multipart::Form::new()
            .text("model_id", self.model_id.clone())
            .part("file", part);

        let resp = self.http
            .post(&url)
            .header("xi-api-key", &self.api_key)
            .multipart(form)
            .send()
            .await
            .map_err(|e| anyhow!("elevenlabs_transcribe: request failed: {e}"))?;

        let status = resp.status();

        if !status.is_success() {
            let err: serde_json::Value = resp.json().await.unwrap_or_default();
            let msg = err["detail"]["message"].as_str()
                .or_else(|| err["detail"].as_str())
                .unwrap_or("unknown error");
            anyhow::bail!("elevenlabs_transcribe: API error {status}: {msg}");
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| anyhow!("elevenlabs_transcribe: failed to parse response: {e}"))?;

        let text = body["text"]
            .as_str()
            .ok_or_else(|| anyhow!("elevenlabs_transcribe: missing 'text' in response"))?
            .to_string();

        info!(chars = text.len(), "elevenlabs_transcribe: done");
        Ok(text)
    }
}
