use async_trait::async_trait;
use anyhow::{anyhow, Result};

use super::{ModelType, ProviderCaps, RemoteModelInfo};

pub struct OpenRouterProvider {
    api_key: String,
    http:    reqwest::Client,
}

impl OpenRouterProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self { api_key: api_key.into(), http: reqwest::Client::new() }
    }

    /// Fetches the raw model list from the OpenRouter catalog endpoint.
    pub async fn fetch_catalog(&self) -> Result<Vec<RemoteModelInfo>> {
        let resp: serde_json::Value = self.http
            .get("https://openrouter.ai/api/v1/models")
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|e| anyhow!("OpenRouter request failed: {e}"))?
            .json()
            .await
            .map_err(|e| anyhow!("OpenRouter response parse failed: {e}"))?;

        let models = resp["data"]
            .as_array()
            .ok_or_else(|| anyhow!("unexpected OpenRouter response shape"))?
            .iter()
            .filter_map(|m| self.parse_model(m))
            .collect();

        Ok(models)
    }
}

impl OpenRouterProvider {
    fn parse_model(&self, m: &serde_json::Value) -> Option<RemoteModelInfo> {
        let id   = m["id"].as_str()?.to_string();
        let name = m["name"].as_str().unwrap_or(&id).to_string();

        let context_length        = m["context_length"].as_u64();
        let max_completion_tokens = m["top_provider"]["max_completion_tokens"].as_u64();
        let knowledge_cutoff      = m["top_provider"]["knowledge_cutoff"].as_str().map(str::to_string);

        let price_input_per_million = m["pricing"]["prompt"].as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .map(|p| p * 1_000_000.0);
        let price_output_per_million = m["pricing"]["completion"].as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .map(|p| p * 1_000_000.0);

        let capabilities = m["supported_parameters"]
            .as_array()
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();

        Some(RemoteModelInfo { id, name, context_length, max_completion_tokens, knowledge_cutoff, capabilities, price_input_per_million, price_output_per_million })
    }
}

#[async_trait]
impl ProviderCaps for OpenRouterProvider {
    fn supported_types(&self) -> &'static [ModelType] {
        &[ModelType::Llm, ModelType::Transcribe, ModelType::ImageGenerate, ModelType::Tts]
    }

    async fn list_models(&self) -> Result<Option<Vec<RemoteModelInfo>>> {
        self.fetch_catalog().await.map(Some)
    }

    async fn model_info(&self, model_id: &str) -> Result<Option<RemoteModelInfo>> {
        let url = format!("https://openrouter.ai/api/v1/models/{model_id}");
        let resp: serde_json::Value = self.http
            .get(&url)
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|e| anyhow!("OpenRouter model_info request failed: {e}"))?
            .json()
            .await
            .map_err(|e| anyhow!("OpenRouter model_info response parse failed: {e}"))?;

        match resp["data"].as_object() {
            Some(_) => Ok(self.parse_model(&resp["data"])),
            None    => Err(anyhow!("unexpected OpenRouter model_info response shape")),
        }
    }
}
