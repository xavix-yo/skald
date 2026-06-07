use std::sync::Arc;

use anyhow::Result;
use sqlx::SqlitePool;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use core_api::remote::RemoteAccess;
use core_api::system_bus::SystemEventBus;

use super::approval::ApprovalManager;
use super::chat_event_bus::ChatEventBus;
use super::config_store::GlobalConfigManager;
use super::chat_hub::ChatHub;
use super::clarification::ClarificationManager;
use super::inbox::Inbox;
use super::compactor::ContextCompactor;
use super::cron::TaskManager;
use super::image_generate::ImageGeneratorManager;
use super::llm::LlmManager;
use super::location::LocationManager;
use super::memory::MemoryManager;
use super::mcp::McpManager;
use super::plugin::PluginManager;
use super::provider::ProviderRegistry;
use super::secrets::SecretsStore;
use super::session::manager::ChatSessionManager;
use super::tic::TicManager;
use super::tool_catalog::ToolCatalog;
use super::tools::ToolRegistry;
use super::transcribe::TranscribeManager;
use super::tts::TtsManager;
use super::config::CoreConfig;
use core_api::plugin::Plugin;

pub struct Skald {
    pub(crate) db:               Arc<SqlitePool>,
    pub config:                  Arc<GlobalConfigManager>,
    pub(crate) system_bus:       Arc<SystemEventBus>,
    pub provider_registry:       Arc<ProviderRegistry>,
    pub llm_manager:             Arc<LlmManager>,
    pub secrets:                 Arc<SecretsStore>,
    pub mcp:                     Arc<McpManager>,
    pub cron:                    Arc<TaskManager>,
    pub plugin_manager:          Arc<PluginManager>,
    pub tools:                   Arc<ToolRegistry>,
    pub approval:                Arc<ApprovalManager>,
    pub image_generator_manager: Arc<ImageGeneratorManager>,
    pub inbox:                   Inbox,
    pub(crate) event_bus:        Arc<ChatEventBus>,
    pub memory_manager:          Arc<MemoryManager>,
    pub clarification:           Arc<ClarificationManager>,
    pub manager:                 Arc<ChatSessionManager>,
    pub catalog:                 ToolCatalog,
    pub chat_hub:                Arc<ChatHub>,
    pub transcribe_manager:      Arc<TranscribeManager>,
    pub tts_manager:             Arc<TtsManager>,
    pub tic_manager:             Arc<TicManager>,
    pub location_manager:        Arc<LocationManager>,
    pub remote:                  Arc<RwLock<Option<Arc<dyn RemoteAccess>>>>,
    pub shutdown_token:          CancellationToken,
    bg_handles:                  std::sync::Mutex<Option<Vec<JoinHandle<()>>>>,
}

impl Skald {
    pub async fn new(pool: Arc<SqlitePool>, config: &CoreConfig, plugins: Vec<Arc<dyn Plugin>>) -> Result<Arc<Self>> {

        let config_store = Arc::new(GlobalConfigManager::new(Arc::clone(&pool)));

        let system_bus = Arc::new(SystemEventBus::new());
        info!("system event bus ready");

        let discovered = super::agents::discover()?;
        info!(
            count = discovered.len(),
            agents = discovered.iter().map(|a| a.id.as_str()).collect::<Vec<_>>().join(", "),
            "agents discovered"
        );

        // ── Provider registry ─────────────────────────────────────────────────
        let mut provider_registry = ProviderRegistry::new(Arc::clone(&system_bus));
        provider_registry.register_builtin(super::llm::providers::openai::OpenAiProvider);
        provider_registry.register_builtin(super::llm::providers::anthropic::AnthropicProvider::new());
        provider_registry.register_builtin(super::llm::providers::openrouter::OpenRouterProvider::new());
        provider_registry.register_builtin(super::llm::providers::ollama::OllamaProvider::new());
        provider_registry.register_builtin(super::llm::providers::lm_studio::LmStudioProvider::new());
        provider_registry.register_builtin(super::llm::providers::deepseek::DeepSeekProvider::new());
        let provider_registry = Arc::new(provider_registry);
        info!("provider registry ready ({} built-in providers)", provider_registry.all().len());

        let requests_log_cfg = config.llm.requests_log.as_ref();
        let log_flags = requests_log_cfg.filter(|r| r.enabled).map(|r| {
            use super::chatbot::logging::LogSaveFlags;
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

        let shutdown_token = CancellationToken::new();

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
                        match super::db::llm_requests::null_request_payload(&cleanup_pool, days).await {
                            Ok(n) if n > 0 => info!(rows = n, days, "llm_requests: nulled request payload"),
                            Ok(_)  => {}
                            Err(e) => warn!(error = %e, "llm_requests: null request payload failed"),
                        }
                    }
                    if let Some(days) = cfg.cleanup_response_payload_after {
                        match super::db::llm_requests::null_response_payload(&cleanup_pool, days).await {
                            Ok(n) if n > 0 => info!(rows = n, days, "llm_requests: nulled response payload"),
                            Ok(_)  => {}
                            Err(e) => warn!(error = %e, "llm_requests: null response payload failed"),
                        }
                    }
                    if let Some(days) = cfg.cleanup_headers_after {
                        match super::db::llm_requests::null_headers(&cleanup_pool, days).await {
                            Ok(n) if n > 0 => info!(rows = n, days, "llm_requests: nulled headers"),
                            Ok(_)  => {}
                            Err(e) => warn!(error = %e, "llm_requests: null headers failed"),
                        }
                    }
                    if let Some(days) = cfg.cleanup_rows_after {
                        match super::db::llm_requests::delete_old_rows(&cleanup_pool, days).await {
                            Ok(n) if n > 0 => info!(deleted = n, days, "llm_requests: deleted old rows"),
                            Ok(_)  => {}
                            Err(e) => warn!(error = %e, "llm_requests: delete old rows failed"),
                        }
                    }
                    // VACUUM reclaims pages freed by DELETE/UPDATE NULL.
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

        let mcp = Arc::new(McpManager::new(Arc::clone(&pool), shutdown_token.clone()));
        let mcp_init = Arc::clone(&mcp);
        tokio::spawn(async move { mcp_init.initialize().await; });

        // TaskManager is created before ToolRegistry so cron tools can
        // be registered before ChatSessionManager is built.
        let cron_tz = config.timezone.as_deref().and_then(|s| {
            match s.parse::<chrono_tz::Tz>() {
                Ok(tz)  => { info!("timezone: using {s}"); Some(tz) }
                Err(_)  => { warn!("timezone: unknown value '{s}', falling back to local time"); None }
            }
        });
        let cron = TaskManager::new(Arc::clone(&pool), cron_tz);

        // Build PluginManager — plugins are injected by the caller (main.rs).
        // start_enabled() is called later by WebFrontend, after the router factory is wired.
        let mut plugin_manager = PluginManager::new(Arc::clone(&pool));
        for plugin in plugins {
            plugin_manager.register_arc(plugin);
        }
        info!("plugins registered");
        let plugin_manager = Arc::new(plugin_manager);

        let mut tool_registry = ToolRegistry::new();
        super::tools::fs::register_all(&mut tool_registry);
        tool_registry.register(super::tools::ast_outline::AstOutline::new());
        tool_registry.register(super::tools::exec::ExecuteCmd);
        tool_registry.register(super::tools::restart::Restart);
        tool_registry.register(super::tools::list_agents::ListAgents);
        tool_registry.register(super::tools::list_mcp::ListMcp::new(Arc::clone(&mcp)));
        tool_registry.register(super::tools::register_mcp::RegisterMcp::new(Arc::clone(&mcp)));
        tool_registry.register(super::tools::toggle_mcp::ToggleMcp::new(Arc::clone(&mcp)));
        tool_registry.register(super::tools::cron_jobs::ListCronJobs(Arc::clone(&cron)));
        tool_registry.register(super::tools::cron_jobs::AddCronJob(Arc::clone(&cron)));
        tool_registry.register(super::tools::cron_jobs::DeleteCronJob(Arc::clone(&cron)));
        tool_registry.register(super::tools::cron_jobs::ToggleCronJob(Arc::clone(&cron)));
        tool_registry.register(super::tools::list_plugins::ListPlugins(Arc::clone(&plugin_manager)));
        tool_registry.register(super::tools::set_secret::SetSecret(Arc::clone(&secrets)));
        tool_registry.register(super::tools::list_secrets::ListSecrets(Arc::clone(&secrets)));
        tool_registry.register(super::tools::toggle_plugin::TogglePlugin(Arc::clone(&plugin_manager)));
        tool_registry.register(super::tools::configure_plugin::ConfigurePlugin(Arc::clone(&plugin_manager)));
        debug!("tool registry built");

        let approval = Arc::new(ApprovalManager::new(Arc::clone(&pool)));
        if let Err(e) = approval.seed_defaults().await {
            warn!(error = %e, "failed to seed default approval rules (non-fatal)");
        }
        if let Err(e) = approval.seed_data_path_rules().await {
            warn!(error = %e, "failed to seed data path allow rules (non-fatal)");
        }
        info!("approval manager ready");

        let tools = Arc::new(tool_registry);

        let image_generator_manager = ImageGeneratorManager::new(
            Arc::clone(&pool),
            Arc::clone(&provider_registry),
            "data",
        ).await?;
        info!(
            db_backed = image_generator_manager.list_models_info().await.len(),
            "image generator manager ready",
        );

        let event_bus = Arc::new(ChatEventBus::new());
        info!("chat event bus ready");

        let memory_manager = Arc::new(MemoryManager::new());
        info!("memory manager ready");

        let compactor = config.llm.compaction.as_ref().map(|cfg| {
            info!(
                threshold_tokens = cfg.threshold_tokens,
                keep_recent      = cfg.keep_recent,
                ?cfg.strength,
                "context compactor enabled"
            );
            Arc::new(ContextCompactor::new(
                cfg.clone(),
                Arc::clone(&llm_manager),
                Arc::clone(&event_bus),
            ))
        });
        if compactor.is_none() {
            info!("context compactor disabled (no compaction config)");
        }

        let clarification = ClarificationManager::new();

        let manager = Arc::new(ChatSessionManager::new(
            Arc::clone(&pool),
            Arc::clone(&llm_manager),
            config.llm.max_history_messages,
            config.llm.max_tool_rounds
                .unwrap_or(crate::core::session::handler::DEFAULT_MAX_TOOL_ROUNDS),
            config.llm.max_tool_result_chars,
            super::config::DatetimeConfig { timezone: config.timezone.clone(), ..config.llm.datetime },
            Arc::clone(&tools),
            Arc::clone(&mcp),
            Arc::clone(&approval),
            Arc::clone(&clarification),
            Arc::clone(&event_bus),
            Arc::clone(&memory_manager),
            Arc::clone(&image_generator_manager),
            compactor,
        ));

        // Wire session manager into cron, then start cron background tasks.
        cron.set_session(Arc::clone(&manager));

        let transcribe_manager = TranscribeManager::new(
            Arc::clone(&pool),
            Arc::clone(&provider_registry),
            Arc::clone(&system_bus),
            shutdown_token.clone(),
        ).await?;
        info!(
            db_backed = transcribe_manager.list_models_info().await.len(),
            "transcribe manager ready",
        );

        let tts_manager = TtsManager::new(
            Arc::clone(&pool),
            Arc::clone(&provider_registry),
            Arc::clone(&system_bus),
            shutdown_token.clone(),
        ).await?;
        info!(
            db_backed = tts_manager.list_models_info().await.len(),
            "tts manager ready",
        );

        let chat_hub = ChatHub::new(
            Arc::clone(&pool),
            Arc::clone(&manager),
            Arc::clone(&approval),
            shutdown_token.clone(),
        );
        chat_hub.register("web").await;
        chat_hub.register("talk").await;
        cron.set_hub(Arc::clone(&chat_hub));
        info!("ChatHub initialised");

        let inbox = Inbox::new(
            Arc::clone(&approval),
            Arc::clone(&clarification),
            Arc::clone(&chat_hub),
        );

        let catalog = ToolCatalog::new(
            Arc::clone(&tools),
            Arc::clone(&mcp),
        );

        let tic_manager = TicManager::new(
            Arc::clone(&pool),
            Arc::clone(&manager),
            Arc::clone(&chat_hub),
            config.tic.clone(),
        );

        // Start background schedulers and collect their handles for graceful shutdown.
        let mut handles: Vec<JoinHandle<()>> = Vec::new();
        handles.extend(Arc::clone(&cron).start(shutdown_token.clone()));
        info!("cron scheduler started");
        handles.push(Arc::clone(&tic_manager).start(shutdown_token.clone()));
        info!("TicManager started");

        let skald = Arc::new(Skald {
            db: pool,
            config: config_store,
            system_bus,
            provider_registry,
            llm_manager,
            secrets,
            mcp,
            cron,
            plugin_manager: Arc::clone(&plugin_manager),
            tools,
            approval,
            image_generator_manager,
            inbox,
            catalog,
            event_bus,
            memory_manager,
            clarification,
            manager,
            chat_hub,
            transcribe_manager,
            tts_manager,
            tic_manager,
            location_manager: Arc::new(LocationManager::new()),
            remote: Arc::new(RwLock::new(None)),
            shutdown_token,
            bg_handles: std::sync::Mutex::new(Some(handles)),
        });

        // Wire plugin manager with the fully constructed Skald instance.
        // start_enabled() and start_config_watcher() are called by WebFrontend::start(),
        // after it provides the router_factory (which requires the static dir and Skald arc).
        plugin_manager.set_skald(Arc::clone(&skald));

        Ok(skald)
    }

    pub fn subscribe_chat_events(&self) -> tokio::sync::broadcast::Receiver<core_api::bus::BusEvent> {
        self.event_bus.subscribe()
    }

    pub fn subscribe_system_events(&self) -> tokio::sync::broadcast::Receiver<core_api::system_bus::SystemEvent> {
        self.system_bus.subscribe()
    }

    pub async fn shutdown(self: Arc<Self>) {
        self.shutdown_token.cancel();

        let handles = self.bg_handles.lock().unwrap().take().unwrap_or_default();
        let bg = async move {
            for h in handles { let _ = h.await; }
        };
        if tokio::time::timeout(tokio::time::Duration::from_secs(10), bg).await.is_err() {
            warn!("background tasks did not finish within 10 s — continuing shutdown");
        }

        self.plugin_manager.stop_all().await;
    }
}
