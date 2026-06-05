mod agents;
mod api;
mod approval;
mod chat_event_bus;
mod chat_hub;
mod clarification;
mod chatbot;
mod compactor;
mod config;
mod cron;
mod db;
mod events;
mod image_generate;
mod llm;
mod location;
mod mcp;
mod memory;
mod plugin;
mod server;
mod session;
mod tic;
mod tools;
mod secrets;
mod transcribe;
mod tts;

use std::sync::Arc;

use anyhow::Result;
use tracing::{debug, error, info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use config::Config;
use cron::CronTaskManager;
use image_generate::ImageGeneratorManager;
use llm::LlmManager;
use location::LocationManager;
use memory::MemoryManager;
use plugin::PluginManager;
use server::{AppState, WebServer};
use session::manager::ChatSessionManager;
use tools::ToolRegistry;
use tools::fs as fs_tools;
use secrets::SecretsStore;
use transcribe::TranscribeManager;
use tts::TtsManager;

const APP_NAME: &str = env!("CARGO_PKG_NAME");

fn main() -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async_main())
}

async fn async_main() -> Result<()> {
    std::fs::create_dir_all("logs")?;
    let file_appender = tracing_appender::rolling::daily("logs", format!("{APP_NAME}.log"));
    let (non_blocking, _log_guard) = tracing_appender::non_blocking(file_appender);

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false),
        )
        .init();

    info!(version = env!("CARGO_PKG_VERSION"), "starting {APP_NAME}");

    let config = match Config::load() {
        Ok(c)  => { debug!("config loaded"); c }
        Err(e) => { error!(error = %e, "failed to load config"); return Err(e); }
    };

    let pool = Arc::new(db::init_pool(&config.db.path).await?);
    info!(path = %config.db.path, "database ready");

    let discovered = agents::discover()?;
    info!(
        count = discovered.len(),
        agents = discovered.iter().map(|a| a.id.as_str()).collect::<Vec<_>>().join(", "),
        "agents discovered"
    );

    let request_log_cfg     = config.llm.request_log.as_ref();
    let request_log_enabled = request_log_cfg.map_or(false, |r| r.enabled);
    let llm_manager = LlmManager::new(Arc::clone(&pool), request_log_enabled).await?;
    let client_count = llm_manager.client_names().await.len().saturating_sub(1);
    let default_client = llm_manager.default_name().await;
    info!(clients = client_count, default = %default_client, "LLM clients loaded");

    // LLM request log cleanup — run once at boot, then every hour.
    if request_log_enabled {
        let retention_days = request_log_cfg.map_or(14, |r| r.retention_days);
        let cleanup_pool   = Arc::clone(&pool);
        tokio::spawn(async move {
            loop {
                match db::llm_requests::cleanup(&cleanup_pool, retention_days).await {
                    Ok(n) if n > 0 => info!(deleted = n, retention_days, "llm_requests: cleanup done"),
                    Ok(_)          => {}
                    Err(e)         => warn!(error = %e, "llm_requests: cleanup failed"),
                }
                tokio::time::sleep(tokio::time::Duration::from_secs(3600)).await;
            }
        });
    }

    let secrets = SecretsStore::new(Arc::clone(&pool));
    info!("secrets store ready");

    let mcp = Arc::new(mcp::McpManager::new(Arc::clone(&pool)));
    let mcp_init = Arc::clone(&mcp);
    tokio::spawn(async move { mcp_init.initialize().await; });

    // CronTaskManager is created first (no session yet) so cron tools can
    // be registered into ToolRegistry before ChatSessionManager is built.
    let cron_tz = config.timezone.as_deref().and_then(|s| {
        match s.parse::<chrono_tz::Tz>() {
            Ok(tz)  => { info!("timezone: using {s}"); Some(tz) }
            Err(_)  => { warn!("timezone: unknown value '{s}', falling back to local time"); None }
        }
    });
    let cron = CronTaskManager::new(Arc::clone(&pool), cron_tz);

    // Build PluginManager — always register all available plugins regardless of
    // config. start_enabled() will only start those with enabled=true in DB.
    let mut plugin_manager = PluginManager::new(Arc::clone(&pool));
    plugin_manager.register(plugin_honcho::HonchoPlugin::new());
    plugin_manager.register(plugin::TelegramPlugin::new("secrets"));
    #[cfg(feature = "whisper-local")]
    plugin_manager.register(plugin::WhisperLocalPlugin::new());
    plugin_manager.register(plugin_tailscale_remote::RemotePlugin::new());
    plugin_manager.register(plugin::ComfyUIPlugin::new());
    plugin_manager.register(plugin_tts_orpheus_3b::OrpheusTtsPlugin::new());
    plugin_manager.register(plugin_tts_kokoro::KokoroTtsPlugin::new());
    info!("plugins registered");
    let plugin_manager = Arc::new(plugin_manager);

    let mut tool_registry = ToolRegistry::new();
    fs_tools::register_all(&mut tool_registry);
    tool_registry.register(tools::ast_outline::AstOutline::new());
    tool_registry.register(tools::exec::ExecuteCmd);
    tool_registry.register(tools::restart::Restart);
    tool_registry.register(tools::list_agents::ListAgents);
    tool_registry.register(tools::list_mcp::ListMcp::new(Arc::clone(&mcp)));
    tool_registry.register(tools::register_mcp::RegisterMcp::new(Arc::clone(&mcp)));
    tool_registry.register(tools::toggle_mcp::ToggleMcp::new(Arc::clone(&mcp)));
    tool_registry.register(tools::cron_jobs::ListCronJobs(Arc::clone(&cron)));
    tool_registry.register(tools::cron_jobs::AddCronJob(Arc::clone(&cron)));
    tool_registry.register(tools::cron_jobs::DeleteCronJob(Arc::clone(&cron)));
    tool_registry.register(tools::cron_jobs::ToggleCronJob(Arc::clone(&cron)));
    tool_registry.register(tools::list_plugins::ListPlugins(Arc::clone(&plugin_manager)));
    tool_registry.register(tools::set_secret::SetSecret(Arc::clone(&secrets)));
    tool_registry.register(tools::list_secrets::ListSecrets(Arc::clone(&secrets)));
    tool_registry.register(tools::toggle_plugin::TogglePlugin(Arc::clone(&plugin_manager)));
    tool_registry.register(tools::configure_plugin::ConfigurePlugin(Arc::clone(&plugin_manager)));

    debug!("tool registry built");

    let approval = Arc::new(approval::ApprovalManager::new(Arc::clone(&pool)));
    if let Err(e) = approval.seed_defaults().await {
        warn!(error = %e, "failed to seed default approval rules (non-fatal)");
    }
    // One-time migration: add allow rules for data/* — remove this call after first run.
    if let Err(e) = approval.seed_data_path_rules().await {
        warn!(error = %e, "failed to seed data path allow rules (non-fatal)");
    }
    info!("approval manager ready");

    let tools = Arc::new(tool_registry);

    let image_generator_manager = ImageGeneratorManager::new(Arc::clone(&pool), "data").await?;
    info!(
        db_backed = image_generator_manager.list_models_info().await.len(),
        "image generator manager ready",
    );

    let event_bus = Arc::new(chat_event_bus::ChatEventBus::new());
    info!("chat event bus ready");

    let memory_manager = Arc::new(MemoryManager::new());
    info!("memory manager ready");

    // Build the context compactor if configured.
    let compactor = config.llm.compaction.as_ref().map(|cfg| {
        info!(
            threshold_tokens = cfg.threshold_tokens,
            keep_recent      = cfg.keep_recent,
            ?cfg.strength,
            "context compactor enabled"
        );
        Arc::new(compactor::ContextCompactor::new(
            cfg.clone(),
            Arc::clone(&llm_manager),
            Arc::clone(&event_bus),
        ))
    });
    if compactor.is_none() {
        info!("context compactor disabled (no compaction config)");
    }

    let clarification = clarification::ClarificationManager::new();

    let manager = Arc::new(ChatSessionManager::new(
        Arc::clone(&pool),
        Arc::clone(&llm_manager),
        config.llm.max_history_messages,
        config.llm.max_tool_rounds.unwrap_or(crate::session::handler::DEFAULT_MAX_TOOL_ROUNDS),
        config.llm.max_tool_result_chars,
        crate::config::DatetimeConfig { timezone: config.timezone.clone(), ..config.llm.datetime },
        Arc::clone(&tools),
        Arc::clone(&mcp),
        Arc::clone(&approval),
        Arc::clone(&clarification),
        Arc::clone(&event_bus),
        Arc::clone(&memory_manager),
        Arc::clone(&image_generator_manager),
        compactor,
    ));

    // Wire the session manager into the cron scheduler now that both exist.
    cron.set_session(Arc::clone(&manager));
    Arc::clone(&cron).start();
    info!("cron scheduler started");

    let transcribe_manager = TranscribeManager::new(Arc::clone(&pool)).await?;
    info!(
        db_backed = transcribe_manager.list_models_info().await.len(),
        "transcribe manager ready",
    );

    let tts_manager = TtsManager::new(Arc::clone(&pool)).await?;
    info!(
        db_backed = tts_manager.list_models_info().await.len(),
        "tts manager ready",
    );

    let chat_hub = chat_hub::ChatHub::new(Arc::clone(&pool), Arc::clone(&manager), Arc::clone(&approval));
    // Always-present sources registered at startup.
    chat_hub.register("web").await;
    chat_hub.register("talk").await;
    cron.set_hub(Arc::clone(&chat_hub));
    info!("ChatHub initialised");

    // TIC background scheduler — processes pending MCP events every 15 minutes.
    // Bypasses ChatHub — it is not a user-facing source.
    let tic_manager = tic::TicManager::new(
        Arc::clone(&pool),
        Arc::clone(&manager),
        Arc::clone(&chat_hub),
        config.tic.clone(),
    );
    Arc::clone(&tic_manager).start();
    info!("TicManager started");

    let web_static_dir: Arc<str> = Arc::from(config.web.static_dir.as_str());
    let web_port = config.server.port;

    let state = AppState {
        manager,
        chat_hub,
        db: pool,
        mcp,
        cron,
        plugin_manager:          Arc::clone(&plugin_manager),
        location_manager:        Arc::new(LocationManager::new()),
        approval,
        clarification,
        tools,
        secrets,
        transcribe_manager,
        tts_manager,
        image_generator_manager,
        tic_manager,
        event_bus,
        memory_manager,
        remote:          Arc::new(tokio::sync::RwLock::new(None)),
        web_static_dir:  Arc::clone(&web_static_dir),
        web_port,
    };
    // Wire PluginManager ↔ AppState, start enabled plugins, then watch for config changes.
    plugin_manager.set_state(Arc::new(state.clone()));
    if let Err(e) = plugin_manager.start_enabled().await {
        error!(error = %e, "plugin startup error");
    }
    plugin_manager.start_config_watcher();

    let addr = format!("{}:{}", config.server.host, config.server.port);
    let server = WebServer::new(config.server, config.web.static_dir, state);
    let handle = server.start().await?;
    info!(%addr, "server listening");

    tokio::signal::ctrl_c().await?;
    warn!("SIGINT received — shutting down");
    plugin_manager.stop_all().await;
    handle.shutdown().await;
    info!("shutdown complete");

    Ok(())
}

