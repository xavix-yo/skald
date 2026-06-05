use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

const DEFAULT_CONFIG: &str = "default.config.yaml";
const CONFIG: &str = "config.yml";

#[derive(Debug, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub web:    WebConfig,
    pub db:     DbConfig,
    pub llm:    LlmConfig,
    #[serde(default)]
    pub tic:    TicConfig,
    #[serde(default)]
    pub cron:   CronConfig,
    /// Global IANA timezone name (e.g. `"Europe/Rome"`).
    /// Applied to: cron expression evaluation, datetime injected into the LLM context.
    /// When omitted, the server's local system timezone is used everywhere.
    pub timezone: Option<String>,
}

/// Cron scheduler settings.
#[derive(Debug, Default, Deserialize)]
pub struct CronConfig {}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Deserialize)]
pub struct WebConfig {
    pub static_dir: String,
}

#[derive(Debug, Deserialize)]
pub struct DbConfig {
    pub path: String,
}

/// LLM runtime settings (clients are managed via LlmManager / DB, not here).
#[derive(Debug, Deserialize)]
pub struct LlmConfig {
    pub max_history_messages: usize,
    pub max_tool_rounds:      Option<usize>,
    /// When set, tool results from previous turns that exceed this many characters are
    /// replaced at context-build time with a short placeholder. The original result is
    /// always preserved in the database (and shown in the frontend); only what the LLM
    /// sees in subsequent turns is affected. Omit or set to `null` to disable.
    pub max_tool_result_chars: Option<usize>,
    /// Request/response logging configuration. Omit or set `enabled: false` to disable.
    pub request_log:          Option<LlmRequestLogConfig>,
    /// Context compaction settings. Omit to disable automatic compaction.
    pub compaction:           Option<CompactionConfig>,
    /// Controls how the current date/time is injected into each LLM request.
    /// Omit to use the default (exact timestamp on every request).
    #[serde(default)]
    pub datetime:             DatetimeConfig,
}

/// Controls date/time injection in the dynamic tail of each LLM request.
#[derive(Debug, Clone, Deserialize)]
pub struct DatetimeConfig {
    /// Inject the current date/time into the LLM context. Default: true.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// When set, round the injected time down to the nearest N-minute boundary.
    /// For example, `round_minutes: 10` turns "10:56" into "10:50", keeping the
    /// string identical for up to 10 minutes and improving KV cache hit rates.
    /// Omit or set to null to use the exact time.
    pub round_minutes: Option<u32>,
    /// IANA timezone name to use when formatting the injected timestamp.
    /// Populated at startup from the global `timezone` config field.
    /// `None` means use the server's local system timezone.
    #[serde(skip)]
    pub timezone: Option<String>,
}

impl Default for DatetimeConfig {
    fn default() -> Self {
        Self { enabled: true, round_minutes: None, timezone: None }
    }
}

fn default_true() -> bool { true }

/// Context compaction: when enabled, the conversation history is summarised
/// once the LLM context exceeds `threshold_tokens`.  The summary replaces
/// the old messages in subsequent turns, keeping only `keep_recent` raw
/// messages intact for immediate context.
#[derive(Debug, Clone, Deserialize)]
pub struct CompactionConfig {
    /// Trigger compaction when the previous turn consumed more than this many
    /// input tokens.  When the LLM provider does not report token usage (e.g.
    /// LM Studio), a character-count estimate (chars / 4) is used as fallback.
    pub threshold_tokens: u32,
    /// Number of recent messages to keep outside the summary.  Defaults to 6.
    #[serde(default = "default_keep_recent")]
    pub keep_recent:      usize,
    /// Minimum LLM strength to use for generating summaries via AUTO selection.
    /// Omit to use whatever model AUTO picks (typically the default).
    /// Summaries are simple writing tasks — `low` or `average` is usually fine.
    pub strength:         Option<LlmStrength>,
}

fn default_keep_recent() -> usize { 6 }

/// TIC background event processor settings.
#[derive(Debug, Clone, Deserialize)]
pub struct TicConfig {
    /// Interval between ticks, in seconds. Default: 900 (15 minutes).
    #[serde(default = "default_tic_interval_secs")]
    pub interval_secs: u64,
    /// Maximum number of events processed per tick. Default: 50.
    #[serde(default = "default_tic_batch_size")]
    pub batch_size: i64,
}

impl Default for TicConfig {
    fn default() -> Self {
        Self {
            interval_secs: default_tic_interval_secs(),
            batch_size:     default_tic_batch_size(),
        }
    }
}

fn default_tic_interval_secs() -> u64 { 900 }
fn default_tic_batch_size()    -> i64  { 50  }

/// Settings for the LLM request/response log (table `llm_requests`).
#[derive(Debug, Deserialize)]
pub struct LlmRequestLogConfig {
    /// Enable logging. Default: false.
    #[serde(default)]
    pub enabled:        bool,
    /// How many days to keep rows before cleanup. Default: 14.
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
}

fn default_retention_days() -> u32 { 14 }

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmStrength {
    VeryLow,
    Low,
    Average,
    High,
    VeryHigh,
}

/// Discriminator stored in the `llm_providers` DB table and used throughout the
/// provider/manager/builder chain to route credentials to the right client.
///
/// **Legacy name** — despite being called `LlmProvider`, this enum covers *all*
/// external API providers, including those that do only TTS or only Transcription
/// (e.g. ElevenLabs). Renaming it to `ApiProvider` or `ServiceProvider` would be
/// the right call but requires a wide refactor; left for a future pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmProvider {
    LmStudio,
    Ollama,
    OpenAi,
    // Serialized as "openrouter" (not "open_router") to match the DB representation
    // and the frontend's PROVIDER_TYPE_LABELS key. The rename overrides snake_case.
    #[serde(rename = "openrouter")]
    OpenRouter,
    Anthropic,
    #[serde(rename = "deepseek")]
    DeepSeek,
    /// TTS + Transcription only — does not support LLM chat/completion.
    #[serde(rename = "elevenlabs")]
    ElevenLabs,
}

impl Config {
    pub fn load() -> Result<Self> {
        let config_path  = Path::new(CONFIG);
        let default_path = Path::new(DEFAULT_CONFIG);

        if !config_path.exists() {
            std::fs::copy(default_path, config_path)
                .with_context(|| format!("Failed to copy {DEFAULT_CONFIG} to {CONFIG}"))?;
            println!("Created {CONFIG} from {DEFAULT_CONFIG}");
        }

        let content = std::fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read {CONFIG}"))?;

        serde_yaml::from_str(&content).with_context(|| format!("Failed to parse {CONFIG}"))
    }
}
