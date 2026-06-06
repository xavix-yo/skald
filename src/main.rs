mod agents;
mod api;
mod provider;
mod service_manager;
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
use provider::ProviderRegistry;
use server::{AppState, WebServer};
use session::manager::ChatSessionManager;
use tools::ToolRegistry;
use tools::fs as fs_tools;
use secrets::SecretsStore;
use transcribe::TranscribeManager;
use tts::TtsManager;
use core_api::system_bus::SystemEventBus;

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

    let system_bus = Arc::new(SystemEventBus::new());
    info!("system event bus ready");

    let discovered = agents::discover()?;
    info!(
        count = discovered.len(),
        agents = discovered.iter().map(|a| a.id.as_str()).collect::<Vec<_>>().join(", "),
        "agents discovered"
    );

    // ── Provider registry ─────────────────────────────────────────────────────
    let mut provider_registry = ProviderRegistry::new(Arc::clone(&system_bus));
    provider_registry.register_builtin(llm::providers::openai::OpenAiProvider);
    provider_registry.register_builtin(llm::providers::anthropic::AnthropicProvider::new());
    provider_registry.register_builtin(llm::providers::openrouter::OpenRouterProvider::new());
    provider_registry.register_builtin(llm::providers::ollama::OllamaProvider::new());
    provider_registry.register_builtin(llm::providers::lm_studio::LmStudioProvider::new());
    provider_registry.register_builtin(llm::providers::deepseek::DeepSeekProvider::new());
    let provider_registry = Arc::new(provider_registry);
    info!("provider registry ready ({} built-in providers)", provider_registry.all().len());

    let requests_log_cfg = config.llm.requests_log.as_ref();
    let log_flags = requests_log_cfg.filter(|r| r.enabled).map(|r| {
        use crate::chatbot::logging::LogSaveFlags;
        LogSaveFlags {
            request_payload:  r.request_payload_save,
            response_payload: r.response_payload_save,
            request_headers:  r.request_header_save,
            response_headers: r.response_header_save,
        }
    });
    let llm_manager = LlmManager::new(Arc::clone(&pool), Arc::clone(&provider_registry), log_flags).await?;
    let client_count = llm_manager.client_names().await.len().saturating_sub(1);
    let default_client = llm_manager.default_name().await;
    info!(clients = client_count, default = %default_client, "LLM clients loaded");

    let shutdown_token = tokio_util::sync::CancellationToken::new();

    // LLM request log cleanup — first run 1 min after startup, then every 12 hours.
    if let Some(cfg) = config.llm.requests_log.clone().filter(|r| r.enabled) {
        let cleanup_pool = Arc::clone(&pool);
        let sd = shutdown_token.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = sd.cancelled() => { return; }
                _ = tokio::time::sleep(tokio::time::Duration::from_secs(60)) => {}
            }
            loop {
                if let Some(days) = cfg.cleanup_request_payload_after {
                    match db::llm_requests::null_request_payload(&cleanup_pool, days).await {
                        Ok(n) if n > 0 => info!(rows = n, days, "llm_requests: nulled request payload"),
                        Ok(_)          => {}
                        Err(e)         => warn!(error = %e, "llm_requests: null request payload failed"),
                    }
                }
                if let Some(days) = cfg.cleanup_response_payload_after {
                    match db::llm_requests::null_response_payload(&cleanup_pool, days).await {
                        Ok(n) if n > 0 => info!(rows = n, days, "llm_requests: nulled response payload"),
                        Ok(_)          => {}
                        Err(e)         => warn!(error = %e, "llm_requests: null response payload failed"),
                    }
                }
                if let Some(days) = cfg.cleanup_headers_after {
                    match db::llm_requests::null_headers(&cleanup_pool, days).await {
                        Ok(n) if n > 0 => info!(rows = n, days, "llm_requests: nulled headers"),
                        Ok(_)          => {}
                        Err(e)         => warn!(error = %e, "llm_requests: null headers failed"),
                    }
                }
                if let Some(days) = cfg.cleanup_rows_after {
                    match db::llm_requests::delete_old_rows(&cleanup_pool, days).await {
                        Ok(n) if n > 0 => info!(deleted = n, days, "llm_requests: deleted old rows"),
                        Ok(_)          => {}
                        Err(e)         => warn!(error = %e, "llm_requests: delete old rows failed"),
                    }
                }
                // VACUUM reclaims pages freed by DELETE/UPDATE NULL — without it the
                // file size does not shrink even after removing hundreds of MB of payloads.
                match sqlx::query("VACUUM").execute(&*cleanup_pool).await {
                    Ok(_)  => info!("llm_requests: VACUUM complete"),
                    Err(e) => warn!(error = %e, "llm_requests: VACUUM failed"),
                }
                tokio::select! {
                    _ = sd.cancelled() => { break; }
                    _ = tokio::time::sleep(tokio::time::Duration::from_secs(12 * 3600)) => {}
                }
            }
        });
    }

    let secrets = SecretsStore::new(Arc::clone(&pool));
    info!("secrets store ready");

    let mcp = Arc::new(mcp::McpManager::new(Arc::clone(&pool), shutdown_token.clone()));
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
    plugin_manager.register(plugin_elevenlabs::ElevenLabsPlugin::new());
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

    let image_generator_manager = ImageGeneratorManager::new(Arc::clone(&pool), Arc::clone(&provider_registry), "data").await?;
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
    let cron_handles = Arc::clone(&cron).start(shutdown_token.clone());
    info!("cron scheduler started");

    let transcribe_manager = TranscribeManager::new(Arc::clone(&pool), Arc::clone(&provider_registry), Arc::clone(&system_bus), shutdown_token.clone()).await?;
    info!(
        db_backed = transcribe_manager.list_models_info().await.len(),
        "transcribe manager ready",
    );

    let tts_manager = TtsManager::new(Arc::clone(&pool), Arc::clone(&provider_registry), Arc::clone(&system_bus), shutdown_token.clone()).await?;
    info!(
        db_backed = tts_manager.list_models_info().await.len(),
        "tts manager ready",
    );

    let chat_hub = chat_hub::ChatHub::new(Arc::clone(&pool), Arc::clone(&manager), Arc::clone(&approval), shutdown_token.clone());
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
    let tic_handle = Arc::clone(&tic_manager).start(shutdown_token.clone());
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
        provider_registry,
        transcribe_manager,
        tts_manager,
        image_generator_manager,
        tic_manager,
        event_bus,
        system_bus,
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
    plugin_manager.start_config_watcher(shutdown_token.clone());

    let addr = format!("{}:{}", config.server.host, config.server.port);
    let server = WebServer::new(config.server, config.web.static_dir, state);
    let handle = server.start().await?;
    info!(%addr, "server listening");

    tokio::signal::ctrl_c().await?;
    warn!("SIGINT received — shutting down");

    // Signal all background loops to exit.
    shutdown_token.cancel();

    // Wait up to 10 s for background tasks to finish any in-flight DB writes.
    let bg_shutdown = async move {
        for h in cron_handles { let _ = h.await; }
        let _ = tic_handle.await;
    };
    if tokio::time::timeout(tokio::time::Duration::from_secs(10), bg_shutdown).await.is_err() {
        warn!("background tasks did not finish within 10 s — continuing shutdown");
    }

    plugin_manager.stop_all().await;
    handle.shutdown().await;
    info!("shutdown complete");

    Ok(())
}

