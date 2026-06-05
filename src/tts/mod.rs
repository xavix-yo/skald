mod db;
pub mod elevenlabs_tts;
pub mod manager;
pub mod openai_tts;

pub use core_api::tts::{TextToSpeech, TtsProvider, TtsRegistry};
pub use manager::TtsManager;

// ── Record types (DB ↔ manager) ───────────────────────────────────────────────

/// Full model record, mirroring one row in `tts_models`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TtsModelRecord {
    pub id:           i64,
    pub provider_id:  i64,
    pub model_id:     String,
    /// Display alias (also used as the synthesiser `id()`).
    pub name:         String,
    /// Human-readable description (voice style, language, ideal use cases).
    pub description:  Option<String>,
    /// Default voice instructions (tone, speed, style).
    /// Can be overridden per call via `synthesize(text, Some(override))`.
    pub instructions: Option<String>,
    /// Lower number = tried first by `get()`.
    pub priority:     i32,
}

/// Public model metadata for API responses.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TtsModelInfo {
    pub id:            i64,
    pub provider_id:   i64,
    pub provider_name: String,
    pub model_id:      String,
    pub name:          String,
    pub description:   Option<String>,
    pub instructions:  Option<String>,
    pub priority:      i32,
    /// `true` for plugin-registered (ephemeral) providers — not editable via the UI.
    pub from_plugin:   bool,
}
