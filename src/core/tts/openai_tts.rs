/// OpenAiTtsSynthesiser — cloud Text-to-Speech via any OpenAI-compatible
/// audio speech endpoint (OpenAI, …).
///
/// Calls `POST {base_url}/audio/speech` with a JSON body.
/// Returns raw MP3 bytes.
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use tracing::{debug, info};

use super::TextToSpeech;

// ── OpenAiTtsSynthesiser ──────────────────────────────────────────────────────

pub struct OpenAiTtsSynthesiser {
    /// Stable identifier, e.g. `"openai_tts_alloy"`.
    id:           String,
    base_url:     String,
    api_key:      String,
    model:        String,
    /// Default instructions (voice style, tone, speed). Overridable per call.
    instructions: Option<String>,
    http:         reqwest::Client,
}

impl OpenAiTtsSynthesiser {
    pub fn new(
        id:           impl Into<String>,
        base_url:     impl Into<String>,
        api_key:      impl Into<String>,
        model:        impl Into<String>,
        instructions: Option<String>,
    ) -> Self {
        Self {
            id:           id.into(),
            base_url:     base_url.into(),
            api_key:      api_key.into(),
            model:        model.into(),
            instructions,
            http:         reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl TextToSpeech for OpenAiTtsSynthesiser {
    fn id(&self)           -> &str           { &self.id }
    fn name(&self)         -> &str           { &self.id }
    fn instructions(&self) -> Option<&str>   { self.instructions.as_deref() }

    async fn synthesize(&self, text: &str, instructions: Option<&str>) -> Result<Vec<u8>> {
        let effective_instructions = instructions.or(self.instructions.as_deref());

        debug!(
            chars = text.len(),
            model = %self.model,
            has_instructions = effective_instructions.is_some(),
            "openai_tts: synthesising",
        );

        let url = format!("{}/audio/speech", self.base_url.trim_end_matches('/'));

        let mut body = serde_json::json!({
            "model": self.model,
            "input": text,
            // Default voice; providers that support instructions typically parse
            // the voice out of them, but `voice` is required by the OpenAI schema.
            "voice": "alloy",
            "response_format": "mp3",
        });

        if let Some(instr) = effective_instructions {
            body["instructions"] = serde_json::Value::String(instr.to_string());
        }

        let resp = self.http
            .post(&url)
            .bearer_auth(&self.api_key)
            .header("X-Title", core_api::APP_NAME)
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow!("openai_tts: request failed: {e}"))?;

        let status = resp.status();

        if !status.is_success() {
            let err: serde_json::Value = resp
                .json()
                .await
                .unwrap_or_default();
            let msg = err["error"]["message"]
                .as_str()
                .unwrap_or("unknown error");
            anyhow::bail!("openai_tts: API error {status}: {msg}");
        }

        let audio = resp
            .bytes()
            .await
            .map_err(|e| anyhow!("openai_tts: failed to read audio bytes: {e}"))?
            .to_vec();

        info!(
            bytes = audio.len(),
            model = %self.model,
            "openai_tts: synthesis complete",
        );

        Ok(audio)
    }
}
