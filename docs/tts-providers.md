# Text-to-Speech Providers

Cloud TTS via OpenAI-compatible or ElevenLabs endpoints, plus plugin-registered local engines.

---

## Architecture

```text
crates/core-api/src/tts.rs
  — TextToSpeech trait (provider interface)
  — TtsProvider trait (resolve active provider)
  — TtsRegistry trait (plugin write-side: register/unregister)

src/tts/
  mod.rs              — TtsModelRecord/Info, re-exports TextToSpeech/TtsProvider/TtsRegistry
  db.rs               — SQL layer for tts_models table
  manager.rs          — TtsManager (DB-aware, owns the table, impls TtsProvider + TtsRegistry)
  openai_tts.rs       — OpenAiTtsSynthesiser: impl TextToSpeech via OpenAI-compatible HTTP JSON
  elevenlabs_tts.rs   — ElevenLabsTtsSynthesiser: impl TextToSpeech via ElevenLabs v1 API
```

Two kinds of providers coexist:

| Kind | Source | Example |
| ---- | ------ | ------- |
| **DB-backed** | `tts_models` table, built from `llm_providers` credentials | `OpenAiTtsSynthesiser`, `ElevenLabsTtsSynthesiser` |
| **Plugin-registered** | Ephemeral — registered at runtime by plugins | `OrpheusTtsPlugin`, `KokoroTtsPlugin` |

`get()` returns the first plugin provider (if any is running), then the first DB-backed provider ordered by `priority ASC`.

---

## Traits (crates/core-api)

```rust
// core_api::tts
#[async_trait]
pub trait TextToSpeech: Send + Sync {
    fn id(&self)           -> &str;
    fn name(&self)         -> &str;
    fn description(&self)  -> Option<&str>;   // default None
    fn instructions(&self) -> Option<&str>;   // default voice style stored in DB
    async fn synthesize(&self, text: &str, instructions: Option<&str>) -> Result<Vec<u8>>;
}

/// Read-side used by callers to get the active provider.
#[async_trait]
pub trait TtsProvider: Send + Sync {
    async fn get(&self) -> Option<Arc<dyn TextToSpeech>>;
}

/// Write-side used by plugins to register/unregister ephemeral providers.
#[async_trait]
pub trait TtsRegistry: Send + Sync {
    async fn register(&self, provider: Arc<dyn TextToSpeech>);
    async fn unregister(&self, id: &str);
}
```

### `instructions` semantics

| Level | Where set | Precedence |
|-------|-----------|------------|
| **DB-level** | `tts_models.instructions` column | Default for this model config |
| **Call-time** | `synthesize(text, Some(override))` | Overrides DB-level for this call |

This lets the LLM (or a plugin) say "respond in a cheerful tone" on a per-turn basis without changing the model's default configuration.

---

## Manager API

```rust
// Async constructor — loads DB models on startup
TtsManager::new(pool: Arc<SqlitePool>) -> Result<Arc<Self>>

// Resolution
tts_manager.get().await    // → Option<Arc<dyn TextToSpeech>>  (plugins first, then DB)

// Plugin registration (ephemeral)
tts_manager.register(Arc::new(synthesiser)).await
tts_manager.unregister("kokoro_local").await

// DB-backed CRUD (called by REST API handlers)
tts_manager.add_model(record).await        // → Result<i64>
tts_manager.update_model(id, record).await
tts_manager.delete_model(id).await         // soft delete
tts_manager.get_model(id).await            // → Option<TtsModelRecord>

// Listings
tts_manager.list_models_info().await       // DB-backed only → Vec<TtsModelInfo>
tts_manager.list_all_info().await          // plugin + DB → Vec<TtsModelInfo>
```

---

## OpenAiTtsSynthesiser

Implemented in `src/tts/openai_tts.rs`.

Calls `POST {base_url}/audio/speech` with a JSON body:

| Field | Value |
|-------|-------|
| `model` | Provider model ID (e.g. `tts-1`, `tts-1-hd`, `gpt-4o-mini-tts`) |
| `input` | Text to synthesise |
| `voice` | `"alloy"` (default — overridable via `instructions`) |
| `response_format` | `"mp3"` |
| `instructions` | Optional natural-language style/tone/speed override |

Returns raw MP3 bytes.

### Supported providers

| Provider | `base_url` | Notes |
| -------- | ---------- | ----- |
| OpenAI | `https://api.openai.com/v1` | Models: `tts-1`, `tts-1-hd`, `gpt-4o-mini-tts` |
| OpenRouter | `https://openrouter.ai/api/v1` | OpenAI-compatible endpoint |

---

## ElevenLabsTtsSynthesiser

Implemented in `src/tts/elevenlabs_tts.rs`.

Calls `POST https://api.elevenlabs.io/v1/text-to-speech/{voice_id}` with auth header `xi-api-key` (not Bearer).

| Field in DB record | Meaning |
| ------------------ | ------- |
| `model_id` | ElevenLabs **voice ID** (e.g. `21m00Tcm4TlvDq8ikWAM`) |
| `instructions` | Injected into LLM system prompt; not sent to ElevenLabs API |

The ElevenLabs generation model is fixed to `eleven_multilingual_v2`. Returns raw MP3 bytes.

Provider type: `elevenlabs` — requires an `xi-api-key` stored in `llm_providers.api_key`. No `base_url` needed.

---

## REST API

| Method | Path | Description |
| ------ | ---- | ----------- |
| `GET` | `/api/tts/models` | All models — plugin-registered first (`from_plugin: true`), then DB-backed |
| `POST` | `/api/tts/models` | Add a new TTS model |
| `GET` | `/api/tts/models/{id}` | Get a DB-backed model record |
| `PUT` | `/api/tts/models/{id}` | Update a DB-backed model |
| `DELETE` | `/api/tts/models/{id}` | Soft-delete a DB-backed model |

Handled by `src/api/tts_models.rs`.

---

## DB: tts_models table

```sql
CREATE TABLE tts_models (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    provider_id  INTEGER NOT NULL REFERENCES llm_providers(id),
    model_id     TEXT    NOT NULL,
    name         TEXT    NOT NULL UNIQUE,
    description  TEXT,                        -- human-readable, shown in UI
    instructions TEXT,                        -- default voice style / tone / speed
    priority     INTEGER NOT NULL DEFAULT 100,
    removed_at   TEXT,
    created_at   TEXT    NOT NULL DEFAULT (datetime('now')),
    UNIQUE(provider_id, model_id)
)
```

---

## Plugin Registration

`TtsRegistry` is exposed on `PluginContext` as `ctx.tts_registry`. Plugin crates depend only on `core-api`.

```rust
use core_api::tts::TextToSpeech;

struct MyTtsSynth { /* ... */ }

#[async_trait]
impl TextToSpeech for MyTtsSynth {
    fn id(&self)   -> &str { "kokoro_local" }
    fn name(&self) -> &str { "Kokoro Local" }
    async fn synthesize(&self, text: &str, instructions: Option<&str>) -> Result<Vec<u8>> {
        // call local engine, return MP3 bytes
    }
}

// In Plugin::start() when enabled:
ctx.tts_registry.register(Arc::new(MyTtsSynth { ... })).await;

// In Plugin::stop():
ctx.tts_registry.unregister("kokoro_local").await;
```

---

## Kokoro TTS (`plugin-tts-kokoro`)

Lightweight local TTS using the Kokoro ONNX model (~310 MB model + ~27 MB voices). No PyTorch or GPU required — runs fully on CPU via ONNX Runtime.

**Crate:** `crates/plugin-tts-kokoro/`
**Plugin ID:** `kokoro_tts`

### How it works

The Python server (`kokoro_server.py`) is embedded in the crate via `include_str!`. On start the plugin writes it to a temp path and spawns it as a FastAPI subprocess. The server downloads `kokoro-v1.0.onnx` and `voices-v1.0.bin` from GitHub Releases on first run, then exposes `POST /synthesize` returning WAV bytes. The plugin registers itself with `TtsManager` and deregisters on stop.

### Setup

```text
toggle_plugin("kokoro_tts", true)
```

Optional config:

```json
{ "voice": "if_sara", "lang": "it", "speed": 1.0 }
```

### Config

| Field | Values | Default |
| ----- | ------ | ------- |
| `voice` | Any Kokoro voice ID (e.g. `if_sara`, `im_nicola`, `af_heart`) | `if_sara` |
| `lang` | BCP-47 language code | `it` |
| `speed` | Speech rate multiplier | `1.0` |

Python deps (in `requirements.txt`): `kokoro-onnx`, `soundfile`.

---

## Orpheus TTS 3B (`plugin-tts-orpheus-3b`)

Local, on-device TTS using the Orpheus 3B model. Runs a Python subprocess for inference.

**Crate:** `crates/plugin-tts-orpheus-3b/`  
**Plugin ID:** `orpheus_tts_3b`

**Note:** the FP16 model is large (~6 GB) and uses significant RAM during inference. Prefer `int8` quantization on memory-constrained machines, or use `plugin-tts-kokoro` as a lighter alternative.

**How it works:** the Python inference server (`orpheus_server.py`) is embedded in the plugin binary via `include_str!`. On start, the plugin writes it to `models/orpheus-3b/orpheus_server.py` and spawns it. The server prints `PORT:<n>` to stdout when ready; the plugin reads that port and registers itself as a `TextToSpeech` provider. On stop, the subprocess is killed.

**Setup:**

```text
set_secret("HUGGINGFACE_TOKEN", "hf_...")
configure_plugin("orpheus_tts_3b", {"quantization": "int8", "voice": "tara"})
toggle_plugin("orpheus_tts_3b", true)
```

**Config:**

| Field | Values | Default |
| ----- | ------ | ------- |
| `quantization` | none / int8 / int4 | int8 |
| `voice` | tara / dan / leah / zac / zoe / mia / julia / leo | tara |

---

## When to Update This File

- A new concrete `TextToSpeech` implementation is added
- `tts_models` schema changes
- A provider gains or loses TTS support
