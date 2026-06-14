//! Skald Remote Control relay server library (see data/ios-app/relay.md).
//!
//! Exposes the building blocks — [`AppState`], [`router`], [`spawn_gc`] — so the
//! `main` binary stays thin and integration tests can spin up the real server on
//! an ephemeral port.

pub mod auth;
pub mod config;
pub mod limits;
pub mod push;
pub mod routing;
pub mod store;
pub mod types;
pub mod ws;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::{ConnectInfo, State, ws::WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use config::Config;
use limits::{FixedWindow, IP_NEW_CONN_PER_MIN, TRANSPORT_FRAME_CAP, TTL_DAYS};
use push::{LogPusher, Pusher};
use routing::Registry;
use store::Store;

/// Shared, cheaply-cloneable application state handed to every connection.
#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub registry: Arc<Registry>,
    pub ip_limiter: Arc<FixedWindow<IpAddr>>,
    pub pusher: Arc<dyn Pusher>,
    conn_seq: Arc<AtomicU64>,
    pub cfg: Arc<Config>,
}

impl AppState {
    /// Build the full application state: open the store and wire the default
    /// (credential-free) push bridge.
    pub async fn build(cfg: Config) -> anyhow::Result<AppState> {
        // Ensure the DB directory exists (SQLite creates the file, not the dir).
        if let Some(parent) = std::path::Path::new(&cfg.db_path).parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let store = Store::init(&cfg.db_path).await?;
        let pusher: Arc<dyn Pusher> = Arc::new(LogPusher);
        Ok(AppState {
            store,
            registry: Arc::new(Registry::new()),
            ip_limiter: Arc::new(FixedWindow::new(
                Duration::from_secs(60),
                IP_NEW_CONN_PER_MIN,
            )),
            pusher,
            conn_seq: Arc::new(AtomicU64::new(1)),
            cfg: Arc::new(cfg),
        })
    }

    /// Monotonic per-process connection id (used for safe self-removal).
    pub fn next_conn_id(&self) -> u64 {
        self.conn_seq.fetch_add(1, Ordering::Relaxed)
    }
}

/// Build the axum router: `GET /healthz` and the WebSocket upgrade `GET /v1/ws`.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/ws", get(ws_upgrade))
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}

/// `GET /v1/ws` → WebSocket upgrade. Per-IP new-connection quota is enforced
/// here (before upgrade) so unauthenticated floods are cheap to reject.
async fn ws_upgrade(
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<AppState>,
) -> Response {
    let ip = addr.ip();
    if !state.ip_limiter.allow(&ip) {
        tracing::warn!(%ip, "rate_limited: too many new connections");
        return (StatusCode::TOO_MANY_REQUESTS, "rate_limited").into_response();
    }
    ws.max_message_size(TRANSPORT_FRAME_CAP)
        .on_upgrade(move |socket| ws::handle_socket(socket, state, ip))
}

/// Periodic garbage collection: drop messages/namespaces past TTL (relay.md §6)
/// and prune the IP rate-limiter map.
pub fn spawn_gc(state: AppState) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(3600));
        loop {
            tick.tick().await;
            match state.store.gc(TTL_DAYS).await {
                Ok((m, n)) if m > 0 || n > 0 => {
                    tracing::info!(messages = m, namespaces = n, "gc removed expired rows");
                }
                Ok(_) => {}
                Err(e) => tracing::error!(error = %e, "gc failed"),
            }
            state.ip_limiter.prune();
        }
    });
}

/// Resolves when SIGINT or SIGTERM arrives (graceful shutdown trigger).
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received; draining");
}
