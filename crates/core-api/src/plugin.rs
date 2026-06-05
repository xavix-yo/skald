use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::RwLock;

use crate::bus::ChatEventBus;
use crate::chat_hub::ChatHubApi;
use crate::image_generate::ImageGenerateRegistry;
use crate::location::LocationUpdater;
use crate::memory::Memory;
use crate::remote::RemoteAccess;
use crate::secrets::SecretsApi;
use crate::transcribe::{TranscribeProvider, TranscribeRegistry};
use crate::tts::{TtsProvider, TtsRegistry};

/// Closure that builds a fresh Axum router (e.g. for the mesh-facing server).
pub type RouterFactory = Arc<dyn Fn() -> axum::Router + Send + Sync>;

/// All deps a plugin may need — passed to [`Plugin::start`] and [`Plugin::reload`].
///
/// Fields are `Arc<dyn Trait>` sourced from `core-api`.  Plugins use only the
/// fields relevant to them; unused fields are ignored.
/// `router_factory` and `remote_slot` are networking-specific — used only by
/// `RemotePlugin`.
#[derive(Clone)]
pub struct PluginContext {
    pub chat_hub:                Arc<dyn ChatHubApi>,
    pub secrets:                 Arc<dyn SecretsApi>,
    pub transcribe:              Arc<dyn TranscribeProvider>,
    pub transcribe_registry:     Arc<dyn TranscribeRegistry>,
    pub image_generate_registry: Arc<dyn ImageGenerateRegistry>,
    pub tts_registry:            Arc<dyn TtsRegistry>,
    pub tts_provider:            Arc<dyn TtsProvider>,
    pub location:                Arc<dyn LocationUpdater>,
    pub event_bus:               Arc<ChatEventBus>,
    pub web_port:                u16,
    pub remote_slot:             Arc<RwLock<Option<Arc<dyn RemoteAccess>>>>,
    pub router_factory:          RouterFactory,
}

/// Plugin lifecycle contract.
///
/// Each plugin implements this trait. The `PluginManager` in the main crate
/// manages their lifecycle and passes a `PluginContext` on every start/reload.
#[async_trait]
pub trait Plugin: Send + Sync {
    fn id(&self)          -> &str;
    fn name(&self)        -> &str;
    fn description(&self) -> &str;
    fn is_running(&self)  -> bool;

    /// JSON Schema describing the plugin's config fields.
    fn config_schema(&self) -> Value { serde_json::json!({}) }

    /// Called whenever the enabled flag or config changes — including at startup.
    /// The plugin is responsible for diffing state and restarting only what changed.
    async fn reload(&self, enabled: bool, config: Value, ctx: PluginContext) -> Result<()>;

    async fn start(&self, ctx: PluginContext) -> Result<()>;
    async fn stop(&self) -> Result<()>;

    /// Runtime state surfaced to the UI and to agents (e.g. mesh IP).
    fn runtime_status(&self) -> Option<Value> { None }

    /// Returns a [`Memory`] backend if this plugin provides one.
    fn memory(&self) -> Option<Arc<dyn Memory>> { None }

    fn as_any(&self) -> &dyn std::any::Any;
    fn as_arc_any(self: Arc<Self>) -> Arc<dyn std::any::Any + Send + Sync>;
}
