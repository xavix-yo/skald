//! Mobile connector plugin (plugin id `mobile-connector`).
//!
//! Bridges Skald's Inbox (approvals + clarifications) to mobile apps over the
//! relay, implementing the **agent** role of the relay protocol: it owns the
//! namespace and is the sole authority on authorized devices. See
//! `data/iOS-app/v2/relay-protocol.md` for the wire contract.
//!
//! The wire transport is **v2 protobuf binary**: every `RelayFrame` travels as
//! a WebSocket `Message::Binary`. E2E payloads are wrapped in the v2 framing
//! (`version ‖ comp ‖ json`) and sealed with AES-256-GCM under the per-client
//! `aes_key`.
//!
//! Module map:
//! - `identity`  — seed + derived keys + namespace_id
//! - `db`        — `relay_clients` table (devices + anti-replay counters)
//! - `pairing`   — in-memory pairing sessions + QR payload
//! - `payloads`  — E2E JSON payload schemas (inbox_update, responses, …)
//! - `state`     — shared runtime (pairing policy, seal/open, Inbox application)
//! - `ws`        — the permanent reconnecting agent WebSocket (v2 binary)
//! - `router`    — the QR-code HTTP endpoint
//! - `agent`     — the `RelayAgent` control trait
//! - `tools`     — `Tool` impls callable by the host (registered in the main crate)

mod agent;
mod db;
mod identity;
mod pairing;
mod payloads;
mod router;
mod state;
mod tools;
mod ws;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use core_api::plugin::{Plugin, PluginContext};

pub use agent::{ClientInfo, ClientState, PairingHandle, RelayAgent};
pub use tools::mobile_tools;

use identity::Identity;
use state::{RelayConfig, RelayState};

const PLUGIN_ID: &str = "mobile-connector";
const DEFAULT_TTL: u32 = 300;
const MAX_TTL: u32 = 600;

/// The mobile-connector plugin.
pub struct MobileConnectorPlugin {
    running: AtomicBool,
    /// Live runtime state — present only while running. Wrapped in Arc so the
    /// HTTP router (built once at startup) can dynamically point to whichever
    /// `RelayState` is current after a reconfigure (plugin#reload → new state).
    inner: Arc<Mutex<Option<Arc<RelayState>>>>,
    cancel: Mutex<Option<CancellationToken>>,
    handles: Mutex<Vec<JoinHandle<()>>>,
}

impl MobileConnectorPlugin {
    pub fn new() -> Self {
        Self {
            running: AtomicBool::new(false),
            inner: Arc::new(Mutex::new(None)),
            cancel: Mutex::new(None),
            handles: Mutex::new(Vec::new()),
        }
    }

    /// Snapshot the live state, if running. Used by the `RelayAgent` impl and the
    /// router accessor.
    async fn state(&self) -> Option<Arc<RelayState>> {
        self.inner.lock().await.clone()
    }

    /// Start the runloop and the bus subscriber with the given config.
    async fn start_with(&self, config: Value, ctx: &PluginContext) -> Result<()> {
        // Ensure the DB table exists (idempotent).
        db::init(&ctx.db).await?;

        let relay_url = config["relay_url"].as_str().unwrap_or("").to_string();
        if relay_url.is_empty() {
            warn!(plugin = PLUGIN_ID, "relay_url not configured; plugin idle");
            // Still mark running so toggling works, but do not connect.
        }
        let pairing_ttl = config["pairing_ttl"]
            .as_u64()
            .map(|v| (v as u32).min(MAX_TTL))
            .unwrap_or(DEFAULT_TTL);
        let require_device_confirmation = config["require_device_confirmation"]
            .as_bool()
            .unwrap_or(true);

        let identity = Identity::load_or_create()?;
        info!(
            plugin = PLUGIN_ID,
            namespace = identity.namespace_id_hex(),
            "mobile-connector identity loaded"
        );

        let state = Arc::new(RelayState::new(
            identity,
            Arc::clone(&ctx.db),
            Arc::clone(&ctx.inbox),
            RelayConfig { relay_url: relay_url.clone(), pairing_ttl, require_device_confirmation },
        ));

        let cancel = CancellationToken::new();
        let mut handles = Vec::new();

        // Outbound WS queue. v2 transport: every queued value is the already-
        // encoded `RelayFrame` protobuf bytes, ready to be wrapped in
        // `Message::Binary` by the WS layer.
        let (out_tx, out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        state.set_outbound(out_tx);

        // WS runloop (only if a relay_url is set).
        if !relay_url.is_empty() {
            let st = Arc::clone(&state);
            let c = cancel.clone();
            handles.push(tokio::spawn(async move {
                ws::run_loop(st, out_rx, c).await;
            }));
        }

        // Bus subscriber: re-snapshot the Inbox on the four Inbox events.
        let st = Arc::clone(&state);
        let c = cancel.clone();
        let mut rx = ctx.chat_hub.events(PLUGIN_ID);
        handles.push(tokio::spawn(async move {
            use core_api::events::ServerEvent::*;
            loop {
                tokio::select! {
                    _ = c.cancelled() => break,
                    ev = rx.recv() => match ev {
                        Ok(ge) => {
                            if matches!(
                                ge.event,
                                ApprovalRequested { .. }
                                    | ApprovalResolved { .. }
                                    | ClarificationRequested { .. }
                                    | ClarificationResolved { .. }
                            ) {
                                if let Err(e) = st.broadcast_inbox().await {
                                    warn!(plugin = PLUGIN_ID, error = %e, "inbox broadcast failed");
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            warn!(plugin = PLUGIN_ID, skipped = n, "event bus lagged");
                        }
                        Err(_) => break,
                    }
                }
            }
        }));

        *self.inner.lock().await = Some(state);
        *self.cancel.lock().await = Some(cancel);
        *self.handles.lock().await = handles;
        self.running.store(true, Ordering::Relaxed);
        info!(plugin = PLUGIN_ID, "mobile-connector started");
        Ok(())
    }

    async fn stop_inner(&self) {
        if let Some(c) = self.cancel.lock().await.take() {
            c.cancel();
        }
        if let Some(st) = self.inner.lock().await.take() {
            st.clear_outbound();
        }
        for h in self.handles.lock().await.drain(..) {
            let _ = h.await;
        }
        self.running.store(false, Ordering::Relaxed);
        info!(plugin = PLUGIN_ID, "mobile-connector stopped");
    }
}

impl Default for MobileConnectorPlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Plugin for MobileConnectorPlugin {
    fn id(&self) -> &str { PLUGIN_ID }
    fn name(&self) -> &str { "Mobile Connector" }
    fn description(&self) -> &str {
        "Connects mobile apps to this Skald instance via the relay: bridges the \
         Inbox (approvals + clarifications) to phones with end-to-end encryption."
    }
    fn is_running(&self) -> bool { self.running.load(Ordering::Relaxed) }

    fn config_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "relay_url": {
                    "type": "string",
                    "title": "Relay URL",
                    "description": "wss:// URL of the relay (e.g. wss://relay.skaldagent.net/v2/ws).",
                },
                "pairing_ttl": {
                    "type": "integer",
                    "default": DEFAULT_TTL,
                    "maximum": MAX_TTL,
                    "title": "Pairing TTL (seconds)",
                    "description": "How long a pairing window stays open. Max 600.",
                },
                "require_device_confirmation": {
                    "type": "boolean",
                    "default": true,
                    "title": "Require device confirmation",
                    "description": "Require manual confirmation before a newly paired device is authorized (recommended).",
                }
            }
        })
    }

    fn runtime_status(&self) -> Option<Value> {
        if !self.running.load(Ordering::Relaxed) {
            return None;
        }
        // Synchronous status: report connection flag from the live state.
        let connected = self
            .inner
            .try_lock()
            .ok()
            .and_then(|g| g.as_ref().map(|s| s.is_connected()))
            .unwrap_or(false);
        Some(json!({ "connected": connected }))
    }

    async fn reload(&self, enabled: bool, config: Value, ctx: PluginContext) -> Result<()> {
        match (enabled, self.is_running()) {
            (true, false) => self.start_with(config, &ctx).await,
            (false, true) => { self.stop_inner().await; Ok(()) }
            (true, true) => { self.stop_inner().await; self.start_with(config, &ctx).await }
            (false, false) => Ok(()),
        }
    }

    async fn start(&self, _ctx: PluginContext) -> Result<()> {
        // Lifecycle is driven by reload(enabled, ...); nothing to do here.
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.stop_inner().await;
        Ok(())
    }

    fn http_router(&self) -> Option<axum::Router> {
        // The router is collected once at WebFrontend startup (plugin.md §12.3),
        // but the inner `RelayState` may be replaced on reconfigure.  We hand
        // over the shared `Arc<Mutex<…>>` so the QR route always resolves the
        // *current* state instead of a stale snapshot.
        Some(router::build(Arc::clone(&self.inner)))
    }

    fn as_any(&self) -> &dyn std::any::Any { self }
    fn as_arc_any(self: Arc<Self>) -> Arc<dyn std::any::Any + Send + Sync> { self }
}

// ── RelayAgent control surface ─────────────────────────────────────────────────

#[async_trait]
impl RelayAgent for MobileConnectorPlugin {
    async fn start_pairing(&self, ttl_secs: u32) -> Result<PairingHandle> {
        let state = self.state().await.ok_or_else(|| anyhow::anyhow!("plugin not running"))?;
        let ttl = if ttl_secs == 0 { state.default_pairing_ttl() } else { ttl_secs.min(MAX_TTL) };
        let started = state.start_pairing(ttl).await?;
        Ok(PairingHandle {
            url: format!("/api/plugin/{PLUGIN_ID}/pairingqrcode?code={}", started.code),
            code: started.code,
            expires_at: started.expires_at,
        })
    }

    async fn stop_pairing(&self) -> Result<()> {
        let state = self.state().await.ok_or_else(|| anyhow::anyhow!("plugin not running"))?;
        state.stop_pairing().await
    }

    fn agent_ed25519_pub(&self) -> [u8; 32] {
        self.inner
            .try_lock()
            .ok()
            .and_then(|g| g.as_ref().map(|s| s.identity().ed25519_pub()))
            .unwrap_or([0u8; 32])
    }

    fn namespace_id(&self) -> String {
        self.inner
            .try_lock()
            .ok()
            .and_then(|g| g.as_ref().map(|s| s.identity().namespace_id_hex().to_string()))
            .unwrap_or_default()
    }

    async fn broadcast_inbox(&self) -> Result<()> {
        let state = self.state().await.ok_or_else(|| anyhow::anyhow!("plugin not running"))?;
        state.broadcast_inbox().await
    }

    async fn broadcast_notification(&self, title: &str, body: &str) -> Result<()> {
        let state = self.state().await.ok_or_else(|| anyhow::anyhow!("plugin not running"))?;
        state.broadcast_notification(title, body).await
    }

    async fn list_clients(&self) -> Vec<ClientInfo> {
        let Some(state) = self.state().await else { return Vec::new() };
        state
            .list_clients()
            .await
            .into_iter()
            .map(|r| ClientInfo {
                ed25519_pub: r.ed25519_pub,
                x25519_pub: r.x25519_pub,
                state: match r.state {
                    db::ClientState::Authorized => ClientState::Authorized,
                    db::ClientState::Pending => ClientState::Pending,
                },
                device_info: r.device_info,
                platform: r.platform,
                last_seen: r.last_seen,
            })
            .collect()
    }

    async fn authorize_client(&self, ed25519_pub: [u8; 32]) -> Result<()> {
        let state = self.state().await.ok_or_else(|| anyhow::anyhow!("plugin not running"))?;
        state.authorize_client(&ed25519_pub).await
    }

    async fn revoke_client(&self, ed25519_pub: [u8; 32]) -> Result<()> {
        let state = self.state().await.ok_or_else(|| anyhow::anyhow!("plugin not running"))?;
        state.revoke_client(&ed25519_pub).await
    }
}
