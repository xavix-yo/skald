use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use tokio::{net::TcpListener, task::JoinHandle};
use tower_http::services::ServeDir;

use crate::frontend::api;
use crate::core::skald::Skald;

pub struct WebServer {
    static_dir:     String,
    skald:          Arc<Skald>,
    /// Routers contributed by enabled plugins, nested under `/api/plugin/<id>/`
    /// (plugin.md §12.3). Empty for the mesh-facing router built by the factory.
    plugin_routers: Vec<(String, Router)>,
}

pub struct WebServerHandle {
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
    task:        JoinHandle<()>,
}

impl WebServer {
    pub fn new(
        static_dir:     String,
        skald:          Arc<Skald>,
        plugin_routers: Vec<(String, Router)>,
    ) -> Self {
        Self { static_dir, skald, plugin_routers }
    }

    pub async fn start(self, addr: &str) -> Result<WebServerHandle> {
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("Failed to bind to {addr}"))?;

        let local_addr = listener.local_addr()?;
        println!("Server running at http://{local_addr}/");

        let router = Self::build_router_with_plugins(&self.static_dir, self.skald, self.plugin_routers);

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("Web server encountered a fatal error");
        });

        Ok(WebServerHandle { shutdown_tx, task })
    }

    pub fn build_router(static_dir: &str, skald: Arc<Skald>) -> Router {
        Self::build_router_with_plugins(static_dir, skald, Vec::new())
    }

    /// Like [`build_router`], but also nests plugin-contributed routers under
    /// `/api/plugin/<id>/` (plugin.md §12.3). The plugin routers are stateless
    /// (`Router<()>`) — they close over their own state — so they mount cleanly
    /// alongside the state-carrying app routes.
    pub fn build_router_with_plugins(
        static_dir:     &str,
        skald:          Arc<Skald>,
        plugin_routers: Vec<(String, Router)>,
    ) -> Router {
        // Resolve the app state first so the resulting `Router<()>` can host the
        // stateless plugin routers via `nest`.
        let mut router = Router::new()
            .nest("/api", api::router())
            .with_state(skald);
        for (id, plugin_router) in plugin_routers {
            router = router.nest(&format!("/api/plugin/{id}"), plugin_router);
        }
        // Serve the data/ directory under /data/ (accessible via URL).
        let data_dir = Path::new(static_dir).parent().unwrap_or(Path::new(".")).join("data");
        router = router.nest_service("/data", ServeDir::new(&data_dir));
        router.fallback_service(ServeDir::new(static_dir))
    }
}

impl WebServerHandle {
    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
        let _ = self.task.await;
    }
}
