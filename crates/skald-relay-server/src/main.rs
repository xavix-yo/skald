//! Skald Remote Control relay server — binary entrypoint (see data/ios-app/relay.md).
//!
//! Thin wrapper: load config → build [`AppState`] → start the GC task → serve
//! the axum router until a shutdown signal arrives. All logic lives in the lib.

use std::net::SocketAddr;

use skald_relay_server::config::Config;
use skald_relay_server::{AppState, router, shutdown_signal, spawn_gc};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "skald_relay_server=info,info".into()),
        )
        .init();

    let cfg = Config::from_env();
    let bind = cfg.bind;
    let db_path = cfg.db_path.clone();

    let state = AppState::build(cfg).await?;
    tracing::info!(db = %db_path, "store ready");

    spawn_gc(state.clone());

    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(%bind, "relay listening on /v1/ws");

    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    tracing::info!("relay stopped");
    Ok(())
}
