mod db;
pub mod elevenlabs_audio;
pub mod manager;
pub mod openai_audio;

pub use core_api::transcribe::{Transcribe, TranscribeProvider, TranscribeRegistry};
pub use manager::TranscribeManager;

// ── Record types (DB ↔ manager) ───────────────────────────────────────────────

/// Full model record, mirroring one row in `transcribe_models`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TranscribeModelRecord {
    pub id:          i64,
    pub provider_id: i64,
    pub model_id:    String,
    /// Display alias (also used as the transcriber `id()`).
    pub name:        String,
    /// BCP-47 language hint, e.g. `"it"`. `None` → auto-detect.
    pub language:    Option<String>,
    /// Lower number = tried first by `get()`.
    pub priority:    i32,
}

/// Public model metadata for API responses.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TranscribeModelInfo {
    pub id:            i64,
    pub provider_id:   i64,
    pub provider_name: String,
    pub model_id:      String,
    pub name:          String,
    pub language:      Option<String>,
    pub priority:      i32,
    /// `true` for plugin-registered (ephemeral) providers — not editable via the UI.
    pub from_plugin:   bool,
}
