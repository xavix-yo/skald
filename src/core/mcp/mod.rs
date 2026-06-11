use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub use mcp_client::{
    McpServerClient, McpServerConfig, McpServerInfo, McpServerStatus, McpTool, McpTransport as McpTransportKind,
    parse_mcp_tool_name,
    http_server::McpHttpServer,
    server::{McpNotification, McpServer},
};

use mcp_client::McpTransport;

const SERVER_START_TIMEOUT_SECS: u64 = 120;

// ── McpManager ───────────────────────────────────────────────────────────────

pub struct McpManager {
    pool:            Arc<SqlitePool>,
    servers:         RwLock<HashMap<String, Arc<dyn McpServerClient>>>,
    errors:          RwLock<HashMap<String, String>>,
    notification_tx: mpsc::UnboundedSender<McpNotification>,
}

impl McpManager {
    pub fn new(pool: Arc<SqlitePool>, shutdown: CancellationToken) -> Self {
        let (notification_tx, notification_rx) = mpsc::unbounded_channel::<McpNotification>();

        let pool_bg = pool.clone();
        tokio::spawn(Self::notification_consumer(pool_bg, notification_rx, shutdown));

        Self {
            pool,
            servers: RwLock::new(HashMap::new()),
            errors:  RwLock::new(HashMap::new()),
            notification_tx,
        }
    }

    async fn notification_consumer(
        pool:     Arc<SqlitePool>,
        mut rx:   mpsc::UnboundedReceiver<McpNotification>,
        shutdown: CancellationToken,
    ) {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("mcp: notification consumer shutdown");
                    break;
                }
                msg = rx.recv() => match msg {
                    Some((source, payload)) => {
                        let method  = payload["method"].as_str().unwrap_or("unknown").to_string();
                        let params  = serde_json::to_string(&payload["params"]).unwrap_or_else(|_| "{}".to_string());
                        match crate::core::db::mcp_events::insert(&pool, &source, &method, &params).await {
                            Ok(id) => info!("mcp_event stored: id={id} source={source} method={method}"),
                            Err(e) => warn!("mcp_events insert failed (source={source} method={method}): {e}"),
                        }
                    }
                    None => break,
                }
            }
        }
    }

    fn cfg_from_row(row: &crate::core::db::mcp_servers::McpServerRow) -> McpServerConfig {
        McpServerConfig {
            name:      row.name.clone(),
            transport: match row.transport.as_str() {
                "http" => McpTransport::Http,
                "sse"  => McpTransport::Sse,
                _      => McpTransport::Stdio,
            },
            command: row.command.clone(),
            args:    Some(row.args()).filter(|v| !v.is_empty()),
            env:     Some(row.env()).filter(|m| !m.is_empty()),
            url:     row.url.clone(),
            api_key: row.api_key.clone(),
        }
    }

    async fn start_one(
        cfg: &McpServerConfig,
        notification_tx: Option<mpsc::UnboundedSender<McpNotification>>,
    ) -> Result<Arc<dyn McpServerClient>> {
        match cfg.transport {
            McpTransport::Stdio => {
                McpServer::start(cfg, notification_tx).await
                    .map(|s| Arc::new(s) as Arc<dyn McpServerClient>)
            }
            McpTransport::Http | McpTransport::Sse => {
                McpHttpServer::start(cfg).await
                    .map(|s| Arc::new(s) as Arc<dyn McpServerClient>)
            }
        }
    }

    pub async fn initialize(&self) {
        let rows = match crate::core::db::mcp_servers::all_enabled(&self.pool).await {
            Ok(r) => r,
            Err(e) => { warn!("McpManager::initialize: failed to read DB: {e}"); return; }
        };

        if rows.is_empty() {
            info!("No enabled MCP servers in DB — MCP disabled.");
            return;
        }

        let cfgs: Vec<_> = rows.iter().map(Self::cfg_from_row).collect();
        let handles: Vec<_> = cfgs.into_iter().map(|cfg| {
            let tx = self.notification_tx.clone();
            tokio::spawn(async move {
                info!("MCP server '{}': starting…", cfg.name);
                let result = tokio::time::timeout(
                    Duration::from_secs(SERVER_START_TIMEOUT_SECS),
                    Self::start_one(&cfg, Some(tx)),
                ).await;
                (cfg.name, cfg.transport, result)
            })
        }).collect();

        for handle in handles {
            match handle.await {
                Ok((name, _, Ok(Ok(s)))) => {
                    let tool_names: Vec<_> = s.tools().iter().map(|t| t.name.as_str()).collect();
                    info!("MCP server '{}' ready — {} tool(s): {}", name, tool_names.len(), tool_names.join(", "));
                    self.servers.write().unwrap().insert(name, s);
                }
                Ok((name, _, Ok(Err(e)))) => {
                    warn!("MCP server '{}' failed to start: {e}", name);
                    self.errors.write().unwrap().insert(name, e.to_string());
                }
                Ok((name, _, Err(_))) => {
                    let msg = format!("startup timed out after {SERVER_START_TIMEOUT_SECS}s");
                    warn!("MCP server '{}' {msg}", name);
                    self.errors.write().unwrap().insert(name, msg);
                }
                Err(e) => { warn!("MCP startup task panicked: {e}"); }
            }
        }
    }

    pub async fn register(&self, p: crate::core::db::mcp_servers::UpsertParams<'_>) -> Result<Vec<String>> {
        let name = p.name.to_string();

        crate::core::db::mcp_servers::upsert(&self.pool, p).await?;

        let rows = crate::core::db::mcp_servers::all_enabled(&self.pool).await?;
        let row = rows.into_iter().find(|r| r.name == name)
            .ok_or_else(|| anyhow::anyhow!("register: server '{}' not found after upsert", name))?;
        let cfg = Self::cfg_from_row(&row);

        let client = tokio::time::timeout(
            Duration::from_secs(SERVER_START_TIMEOUT_SECS),
            Self::start_one(&cfg, Some(self.notification_tx.clone())),
        ).await
        .map_err(|_| anyhow::anyhow!("MCP server '{}' timed out during connection", name))?
        .map_err(|e| anyhow::anyhow!("MCP server '{}' failed to start: {e}", name))?;

        let tool_names: Vec<String> = client.tools().iter().map(|t| t.name.clone()).collect();
        self.errors.write().unwrap().remove(&name);
        self.servers.write().unwrap().insert(name, client);

        Ok(tool_names)
    }

    pub async fn unregister(&self, name: &str) -> Result<()> {
        crate::core::db::mcp_servers::delete(&self.pool, name).await?;
        self.servers.write().unwrap().remove(name);
        self.errors.write().unwrap().remove(name);
        Ok(())
    }

    pub async fn set_enabled(&self, name: &str, enabled: bool) -> Result<()> {
        crate::core::db::mcp_servers::set_enabled(&self.pool, name, enabled).await
    }

    pub async fn list(&self) -> Result<Vec<McpServerInfo>> {
        let rows = crate::core::db::mcp_servers::all(&self.pool).await?;
        let servers = self.servers.read().unwrap();
        let errors  = self.errors.read().unwrap();

        let infos = rows.into_iter().map(|row| {
            let status = if !row.enabled {
                McpServerStatus::Disabled
            } else if let Some(s) = servers.get(&row.name) {
                McpServerStatus::Running {
                    tools: s.tools().iter().map(|t| t.name.clone()).collect(),
                }
            } else if let Some(e) = errors.get(&row.name) {
                McpServerStatus::Error { message: e.clone() }
            } else {
                McpServerStatus::Error { message: "not connected".to_string() }
            };
            McpServerInfo {
                name: row.name,
                transport: row.transport,
                description: row.description,
                friendly_name: row.friendly_name,
                status,
            }
        }).collect();

        Ok(infos)
    }

    pub fn tools(&self) -> Vec<McpTool> {
        self.servers.read().unwrap().values()
            .flat_map(|s| s.tools().iter().cloned())
            .collect()
    }

    pub fn tools_for(&self, names: &[String]) -> Vec<McpTool> {
        self.servers.read().unwrap().iter()
            .filter(|(name, _)| names.contains(name))
            .flat_map(|(_, s)| s.tools().iter().cloned())
            .collect()
    }

    pub fn server_infos(&self) -> Vec<Value> {
        self.servers.read().unwrap().iter()
            .map(|(name, s)| json!({
                "name": name,
                "tools": s.tools().iter().map(|t| json!({
                    "name":        t.name,
                    "description": t.description,
                })).collect::<Vec<_>>(),
            }))
            .collect()
    }

    pub async fn call(&self, server: &str, tool: &str, args: Value) -> Result<String> {
        let s = self.servers.read().unwrap()
            .get(server)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("MCP server '{server}' not found"))?;
        s.call_tool(tool, args).await
    }
}
