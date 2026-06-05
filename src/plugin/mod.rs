pub use core_api::plugin::{Plugin, PluginContext, RouterFactory};
pub use plugin_comfyui::ComfyUIPlugin;
#[cfg(feature = "whisper-local")]
pub use plugin_transcribe_whisper_local::WhisperLocalPlugin;
pub use plugin_telegram_bot::TelegramPlugin;

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

const PLUGIN_START_TIMEOUT_SECS: u64 = 30;
const PLUGIN_STOP_TIMEOUT_SECS:  u64 = 5;

use anyhow::Result;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tracing::{error, info, warn};

use crate::db::plugins as db;
use crate::server::AppState;

// ── Public plugin info (returned by list_plugins tool and REST API) ───────────

#[derive(Debug, Clone, Serialize)]
pub struct PluginInfo {
    pub id:             String,
    pub name:           String,
    pub description:    String,
    pub enabled:        bool,
    pub running:        bool,
    pub config:         Value,
    pub config_schema:  Value,
    pub runtime_status: Option<Value>,
}

// ── PluginManager ─────────────────────────────────────────────────────────────

pub struct PluginManager {
    plugins:     Vec<Arc<dyn Plugin>>,
    db:          Arc<SqlitePool>,
    state:       OnceLock<Arc<AppState>>,
    /// Last known (enabled, config_json) per plugin id — used by the watcher.
    known_state: Mutex<HashMap<String, (bool, String)>>,
}

impl PluginManager {
    pub fn new(db: Arc<SqlitePool>) -> Self {
        Self {
            plugins:     Vec::new(),
            db,
            state:       OnceLock::new(),
            known_state: Mutex::new(HashMap::new()),
        }
    }

    pub fn register(&mut self, plugin: impl Plugin + 'static) {
        self.plugins.push(Arc::new(plugin));
    }

    pub fn set_state(&self, state: Arc<AppState>) {
        let _ = self.state.set(state);
    }

    fn state(&self) -> Result<Arc<AppState>> {
        self.state.get().cloned()
            .ok_or_else(|| anyhow::anyhow!("PluginManager: state not initialized"))
    }

    fn build_context(&self, state: &AppState) -> PluginContext {
        use crate::server::WebServer;

        let static_dir  = Arc::clone(&state.web_static_dir);
        let state_clone = state.clone();
        let router_factory: RouterFactory = Arc::new(move || {
            WebServer::build_router(&static_dir, state_clone.clone())
        });

        PluginContext {
            chat_hub:                Arc::clone(&state.chat_hub) as _,
            secrets:                 Arc::clone(&state.secrets) as _,
            transcribe:              Arc::clone(&state.transcribe_manager) as _,
            transcribe_registry:     Arc::clone(&state.transcribe_manager) as _,
            image_generate_registry: Arc::clone(&state.image_generator_manager) as _,
            tts_registry:            Arc::clone(&state.tts_manager) as _,
            tts_provider:            Arc::clone(&state.tts_manager) as _,
            location:                Arc::clone(&state.location_manager) as _,
            event_bus:               Arc::clone(&state.event_bus),
            web_port:                state.web_port,
            remote_slot:             Arc::clone(&state.remote),
            router_factory,
        }
    }

    // ── Startup ───────────────────────────────────────────────────────────────

    /// Calls reload() for every plugin that has enabled=true in DB.
    /// Plugins without a DB row are skipped (not yet configured).
    /// After each successful start, registers the plugin's Memory backend (if any)
    /// with `AppState::memory_manager`.
    pub async fn start_enabled(&self) -> Result<()> {
        let state = self.state()?;
        for plugin in &self.plugins {
            let row = db::get(&self.db, plugin.id()).await?;
            let Some(row) = row else { continue };
            if !row.enabled { continue; }
            let config = serde_json::from_str(&row.config).unwrap_or(json!({}));
            let deadline = Duration::from_secs(PLUGIN_START_TIMEOUT_SECS);
            let ctx = self.build_context(&state);
            match timeout(deadline, plugin.reload(true, config, ctx)).await {
                Ok(Ok(())) => {
                    self.known_state.lock().await
                        .insert(plugin.id().to_string(), (true, row.config));
                    info!(plugin = plugin.id(), "plugin started");
                    if let Some(mem) = plugin.memory() {
                        state.memory_manager.register(mem).await;
                    }
                }
                Ok(Err(e)) => error!(plugin = plugin.id(), error = %e, "plugin failed to start"),
                Err(_)     => error!(plugin = plugin.id(), secs = PLUGIN_START_TIMEOUT_SECS, "plugin start timed out"),
            }
        }
        Ok(())
    }

    pub async fn stop_all(&self) {
        for plugin in &self.plugins {
            if plugin.is_running() {
                let deadline = Duration::from_secs(PLUGIN_STOP_TIMEOUT_SECS);
                match timeout(deadline, plugin.stop()).await {
                    Ok(Ok(()))  => info!(plugin = plugin.id(), "plugin stopped"),
                    Ok(Err(e))  => error!(plugin = plugin.id(), error = %e, "plugin stop error"),
                    Err(_)      => warn!(plugin = plugin.id(), secs = PLUGIN_STOP_TIMEOUT_SECS, "plugin stop timed out"),
                }
            }
        }
    }

    // ── Config update (called by REST API) ────────────────────────────────────

    /// Persists the new config to DB, then calls reload() immediately.
    pub async fn update_config(&self, id: &str, enabled: bool, config: Value) -> Result<()> {
        let plugin = self.find(id)?;
        let config_json = serde_json::to_string(&config)?;
        db::upsert(&self.db, id, enabled, &config_json).await?;
        let state = self.state()?;
        plugin.reload(enabled, config, self.build_context(&state)).await?;
        self.known_state.lock().await
            .insert(id.to_string(), (enabled, config_json));
        info!(plugin = id, enabled, "plugin config updated");
        Ok(())
    }

    /// Toggle only the enabled flag, keeping existing config.
    pub async fn toggle(&self, id: &str, enabled: bool) -> Result<()> {
        let row = db::get(&self.db, id).await?
            .unwrap_or_else(|| crate::db::plugins::PluginRow {
                id:      id.to_string(),
                enabled,
                config:  "{}".to_string(),
            });
        let config: Value = serde_json::from_str(&row.config).unwrap_or(json!({}));
        self.update_config(id, enabled, config).await
    }

    // ── Background config watcher ─────────────────────────────────────────────

    /// Spawns a Tokio task that polls the DB every 30 s and calls reload()
    /// on any plugin whose (enabled, config) has changed since last check.
    /// This is the fallback path; normal updates go through update_config().
    pub fn start_config_watcher(self: &Arc<Self>) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            interval.tick().await; // skip immediate first tick
            loop {
                interval.tick().await;
                if let Err(e) = this.check_and_reload().await {
                    error!(error = %e, "plugin config watcher error");
                }
            }
        });
    }

    async fn check_and_reload(&self) -> Result<()> {
        let rows = db::list(&self.db).await?;
        let state = self.state()?;

        // Collect what needs reloading while holding the lock briefly.
        let to_reload: Vec<_> = {
            let known = self.known_state.lock().await;
            rows.into_iter()
                .filter(|row| {
                    known.get(&row.id)
                        .map_or(true, |(e, c)| *e != row.enabled || c != &row.config)
                })
                .collect()
        };

        for row in to_reload {
            let Ok(plugin) = self.find(&row.id) else { continue };
            let config = serde_json::from_str(&row.config).unwrap_or(json!({}));
            let ctx = self.build_context(&state);
            match plugin.reload(row.enabled, config, ctx).await {
                Ok(()) => {
                    self.known_state.lock().await
                        .insert(row.id.clone(), (row.enabled, row.config));
                    info!(plugin = row.id, "plugin reloaded by config watcher");
                    if row.enabled {
                        if let Some(mem) = plugin.memory() {
                            state.memory_manager.register(mem).await;
                        }
                    }
                }
                Err(e) => error!(plugin = row.id, error = %e, "plugin reload failed"),
            }
        }
        Ok(())
    }

    // ── Queries ───────────────────────────────────────────────────────────────

    pub async fn list(&self) -> Result<Vec<PluginInfo>> {
        let mut out = Vec::new();
        for plugin in &self.plugins {
            let row = db::get(&self.db, plugin.id()).await?;
            let (enabled, config_json) = row
                .map(|r| (r.enabled, r.config))
                .unwrap_or((false, "{}".to_string()));
            out.push(PluginInfo {
                id:             plugin.id().to_string(),
                name:           plugin.name().to_string(),
                description:    plugin.description().to_string(),
                enabled,
                running:        plugin.is_running(),
                config:         serde_json::from_str(&config_json).unwrap_or(json!({})),
                config_schema:  plugin.config_schema(),
                runtime_status: plugin.runtime_status(),
            });
        }
        Ok(out)
    }

    pub fn get_plugin_typed<T: Plugin + 'static>(&self, id: &str) -> Option<Arc<T>> {
        self.plugins.iter()
            .find(|p| p.id() == id)
            .and_then(|p| Arc::clone(p).as_arc_any().downcast::<T>().ok())
    }

    fn find(&self, id: &str) -> Result<Arc<dyn Plugin>> {
        self.plugins.iter()
            .find(|p| p.id() == id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("plugin not found: {id}"))
    }
}
