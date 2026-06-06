use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use sqlx::SqlitePool;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::approval::ApprovalManager;
use crate::session::handler::ApprovalDecision;
use crate::db::{chat_history, chat_sessions_stack, config, sources};
use crate::events::{GlobalEvent, ServerEvent};
use crate::session::handler::ChatSessionHandler;
use crate::session::manager::ChatSessionManager;

pub use core_api::chat_hub::{ChatHubApi, SendMessageOptions};
pub use core_api::interface_tool::InterfaceTool;

pub const HOME_SOURCE_KEY:     &str = "source_home";
pub const DEFAULT_HOME_SOURCE: &str = "web";

// Global broadcast channel capacity.
const EVENTS_CAPACITY: usize = 512;

// Central notification queue capacity (inbound from background agents).
const NOTIFY_CAPACITY: usize = 64;

// How long to wait after the first notification before draining, to batch bursts.
const NOTIFY_BATCH_WINDOW_MS: u64 = 200;

// ── ChatHub ───────────────────────────────────────────────────────────────────

pub struct ChatHub {
    db:          Arc<SqlitePool>,
    session_mgr: Arc<ChatSessionManager>,
    approval:    Arc<ApprovalManager>,
    /// Single global broadcast bus. All events from all sources flow here,
    /// wrapped in GlobalEvent with source/session_id tags. Subscribers filter.
    global_tx:   broadcast::Sender<GlobalEvent>,
    /// Central inbound notification queue from background agents.
    /// Consumer task is spawned in new() and drains this channel.
    notify_tx:   mpsc::Sender<String>,
}

impl ChatHub {
    pub fn new(
        db:          Arc<SqlitePool>,
        session_mgr: Arc<ChatSessionManager>,
        approval:    Arc<ApprovalManager>,
        shutdown:    CancellationToken,
    ) -> Arc<Self> {
        let (notify_tx, notify_rx) = mpsc::channel::<String>(NOTIFY_CAPACITY);
        let (global_tx, _)         = broadcast::channel::<GlobalEvent>(EVENTS_CAPACITY);

        let hub = Arc::new(Self {
            db,
            session_mgr,
            approval,
            global_tx,
            notify_tx,
        });

        // Spawn the background consumer with a Weak reference so it doesn't
        // prevent ChatHub from being dropped on shutdown.
        tokio::spawn(Self::notification_consumer(Arc::downgrade(&hub), notify_rx, shutdown));

        hub
    }

    // ── Public API ────────────────────────────────────────────────────────────

    /// Register a source. No-op for duplicate registrations.
    /// With the global bus, registration no longer creates a per-source channel.
    pub async fn register(&self, source_id: &str) {
        info!(source_id, "ChatHub: source registered");
    }

    /// Send a user message for a source, running a full LLM turn.
    /// Creates a session lazily if none exists yet.
    /// Events (tool calls, final response, errors) are published to the source's broadcast channel.
    pub async fn send_message(
        &self,
        source_id: &str,
        prompt:    &str,
        opts:      SendMessageOptions,
    ) -> anyhow::Result<()> {
        let session_id = self.get_or_create_session(source_id).await?;
        let source_tag = source_id.to_string();

        // Bridge mpsc from handle_message → global broadcast, tagging with source/session.
        let tx = Self::bridge_to_global(self.global_tx.clone(), source_tag, session_id);

        let handler = self.session_mgr.get_or_create_handler(session_id).await?;
        handler.handle_message(
            prompt,
            opts.client_name,
            opts.extra_system_context,
            opts.extra_system_dynamic,
            opts.tail_reminder,
            opts.interface_tools,
            tx,
            opts.is_synthetic,
        ).await
    }

    /// Returns the session handler for the source's active session, creating one lazily if needed.
    pub async fn session_handler(&self, source_id: &str) -> anyhow::Result<Arc<ChatSessionHandler>> {
        let session_id = self.get_or_create_session(source_id).await?;
        self.session_mgr.get_or_create_handler(session_id).await
    }

    /// Create a new session for the source, discarding the previous one.
    pub async fn clear(&self, source_id: &str) -> anyhow::Result<i64> {
        let (session_id, _) = self.session_mgr.create_session("main", source_id, true, false).await?;
        sources::upsert(&self.db, source_id, session_id).await?;
        info!(source_id, session_id, "ChatHub: session cleared");
        let _ = self.global_tx.send(GlobalEvent {
            source:     Some(source_id.to_string()),
            session_id: Some(session_id),
            event:      ServerEvent::NewSession { session_id },
        });
        Ok(session_id)
    }

    /// Subscribe to the global event bus. The `source_id` parameter is accepted
    /// for API compatibility but filtering by source is the caller's responsibility.
    pub fn events(&self, _source_id: &str) -> broadcast::Receiver<GlobalEvent> {
        self.global_tx.subscribe()
    }

    /// Emit an event directly on the global bus (for system events without a session).
    pub fn emit(&self, event: GlobalEvent) {
        let _ = self.global_tx.send(event);
    }

    /// Set which source is the "home" for background agent notifications.
    pub async fn set_home(&self, source_id: &str) -> anyhow::Result<()> {
        config::set(&self.db, HOME_SOURCE_KEY, source_id).await?;
        info!(source_id, "ChatHub: home source set");
        Ok(())
    }

    /// Returns the current home source id, falling back to `web` if not configured.
    pub async fn home_source(&self) -> anyhow::Result<String> {
        Ok(config::get(&self.db, HOME_SOURCE_KEY)
            .await?
            .unwrap_or_else(|| DEFAULT_HOME_SOURCE.to_string()))
    }

    /// Returns token usage for the last message in the source's active session.
    /// Returns `(input_tokens, output_tokens)` — both are `None` when no
    /// messages exist or the provider did not report usage.
    pub async fn context_info(&self, source_id: &str) -> anyhow::Result<(Option<i64>, Option<i64>)> {
        let session_id = self.get_or_create_session(source_id).await?;
        let stack = match chat_sessions_stack::active_for_session(&self.db, session_id).await? {
            Some(s) => s,
            None => return Ok((None, None)),
        };
        let last = chat_history::last_message_for_stack(&self.db, stack.id).await?;
        Ok(last.map_or((None, None), |m| (m.input_tokens, m.output_tokens)))
    }

    /// Force compaction of the source's active session history.
    /// Bypasses the token threshold; returns `true` if compaction occurred.
    pub async fn force_compact(&self, source_id: &str) -> anyhow::Result<bool> {
        let handler = self.session_handler(source_id).await?;
        handler.force_compact().await
    }

    /// Resume any interrupted turn for a source's active session.
    /// Calls `resume_turn` which re-executes pending tool calls (approval or
    /// clarification) and re-runs the LLM loop if needed.
    /// Safe to call unconditionally — returns immediately if there is nothing to resume.
    /// Events are published to the global broadcast bus so existing subscribers
    /// (e.g. Telegram's persistent_forwarder) receive them without a WS connection.
    pub async fn resume(&self, source_id: &str) -> anyhow::Result<()> {
        let session_id = match sources::active_session_id(&self.db, source_id).await? {
            Some(sid) => sid,
            None      => return Ok(()), // no prior session, nothing to resume
        };
        let source_tag = source_id.to_string();
        let tx = Self::bridge_to_global(self.global_tx.clone(), source_tag, session_id);
        let handler = self.session_mgr.get_or_create_handler(session_id).await?;
        handler.resume_turn(None, None, vec![], tx).await
    }

    /// Queue a notification briefing from a background agent.
    /// The consumer task aggregates pending briefings and dispatches them to the home source.
    pub async fn notify(&self, briefing: String) -> anyhow::Result<()> {
        if self.notify_tx.send(briefing).await.is_err() {
            warn!("ChatHub::notify: notification queue full or receiver dropped");
        }
        Ok(())
    }

    /// Synchronous variant of `notify` for use inside `Tool::execute` (sync context).
    /// Uses `try_send` — drops the briefing if the channel is full rather than blocking.
    pub fn notify_sync(&self, briefing: String) {
        if self.notify_tx.try_send(briefing).is_err() {
            warn!("ChatHub::notify_sync: notification channel full or closed — briefing dropped");
        }
    }

    /// Cancel the active LLM turn for the source's session, clearing any pending
    /// approvals and clarification questions. No-op if no session is active.
    pub async fn cancel(&self, source_id: &str) {
        match self.session_handler(source_id).await {
            Ok(handler) => {
                handler.cancel();
                handler.cancel_pending_approvals().await;
                handler.cancel_pending_questions().await;
                info!(source_id, "ChatHub: cancel requested");
            }
            Err(e) => {
                warn!(source_id, error = %e, "ChatHub::cancel: no session to cancel");
            }
        }
    }

    /// Approve a pending tool-call approval request.
    pub async fn approve(&self, request_id: i64) {
        if let Some(info) = self.approval.resolve(request_id, ApprovalDecision::Approved).await {
            let _ = self.global_tx.send(GlobalEvent {
                source:     Some(info.source),
                session_id: Some(info.session_id),
                event:      ServerEvent::ApprovalResolved { request_id, tool_call_id: info.tool_call_id, approved: true },
            });
        }
    }

    /// Reject a pending tool-call approval request.
    pub async fn reject(&self, request_id: i64, note: String) {
        if let Some(info) = self.approval.resolve(request_id, ApprovalDecision::Rejected { note }).await {
            let _ = self.global_tx.send(GlobalEvent {
                source:     Some(info.source),
                session_id: Some(info.session_id),
                event:      ServerEvent::ApprovalResolved { request_id, tool_call_id: info.tool_call_id, approved: false },
            });
        }
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Spawn a bridge task that forwards events from an mpsc channel to the
    /// global broadcast bus, tagging each event with `source` and `session_id`.
    fn bridge_to_global(
        global_tx:  broadcast::Sender<GlobalEvent>,
        source:     String,
        session_id: i64,
    ) -> mpsc::Sender<ServerEvent> {
        let (tx, mut rx) = mpsc::channel::<ServerEvent>(EVENTS_CAPACITY);
        tokio::spawn(async move {
            tracing::debug!(%source, session_id, "ChatHub: bridge task started");
            while let Some(event) = rx.recv().await {
                tracing::debug!(%source, session_id, event_type = event.type_name(), "ChatHub: bridge forwarding event");
                let _ = global_tx.send(GlobalEvent {
                    source:     Some(source.clone()),
                    session_id: Some(session_id),
                    event,
                });
            }
            tracing::debug!(%source, session_id, "ChatHub: bridge task ended");
        });
        tx
    }

    async fn get_or_create_session(&self, source_id: &str) -> anyhow::Result<i64> {
        if let Some(sid) = sources::active_session_id(&self.db, source_id).await? {
            return Ok(sid);
        }
        let (session_id, _) = self.session_mgr.create_session("main", source_id, true, false).await?;
        sources::upsert(&self.db, source_id, session_id).await?;
        info!(source_id, session_id, "ChatHub: session created lazily");
        Ok(session_id)
    }

    // ── Notification consumer ─────────────────────────────────────────────────

    /// Background task: drains the central notification queue and dispatches
    /// aggregated briefings to the home source as synthetic user messages.
    ///
    /// Serialisation with active LLM turns is free: `ChatSessionHandler::handle_message`
    /// holds `processing: Mutex<()>` for the duration of a turn, so `send_message`
    /// below blocks naturally until the turn completes.
    async fn notification_consumer(hub: Weak<Self>, mut rx: mpsc::Receiver<String>, shutdown: CancellationToken) {
        info!("ChatHub: notification consumer started");

        loop {
            // Block until at least one notification arrives (or shutdown signal).
            let first = tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("ChatHub: notification consumer shutdown");
                    break;
                }
                msg = rx.recv() => match msg {
                    Some(b) => b,
                    None    => break, // notify_tx dropped — ChatHub is shutting down
                }
            };

            // Brief window to let burst notifications accumulate before dispatching.
            tokio::time::sleep(Duration::from_millis(NOTIFY_BATCH_WINDOW_MS)).await;

            // Drain everything else that arrived during the window.
            let mut briefings = vec![first];
            while let Ok(b) = rx.try_recv() {
                briefings.push(b);
            }

            let hub = match hub.upgrade() {
                Some(h) => h,
                None    => break, // ChatHub dropped
            };

            let home = match hub.home_source().await {
                Ok(h)  => h,
                Err(e) => { error!(error = %e, "notification consumer: home_source failed"); continue; }
            };

            let count = briefings.len();
            // Body: one [SYSTEM - NOTIFICATION] header for the whole batch, followed
            // by the decorated briefings joined with blank lines.
            // Each briefing already carries its source prefix (e.g. "TIC sent the
            // following briefing: …") added by the originating notify tool.
            let message = format!("[SYSTEM - NOTIFICATION]\n{}", briefings.join("\n\n"));

            // Behavioural framing injected as a dynamic tail system message (position 5,
            // AFTER conversation history).  Placed here so it:
            //  • is not repeated when multiple briefings are batched
            //  • does not pollute the cacheable static prefix
            //  • benefits from recency bias (model reads it right before generating)
            let notification_framing = "\
[SYSTEM - NOTIFICATION context — do not expose this to the user]\n\
One or more background agents have just surfaced the message(s) above.\n\
The user did NOT send this — do not say things like \"thanks for the reminder\" \
or attribute the information to the user.\n\
Incorporate what you learned naturally, as something you became aware of proactively.\n\
If the conversation is active, weave it into your next reply. \
If idle, open proactively with it."
                .to_string();

            info!(home_source = %home, count, "ChatHub: dispatching notifications");

            let opts = SendMessageOptions {
                is_synthetic:          true,
                extra_system_dynamic:  Some(notification_framing),
                ..Default::default()
            };
            if let Err(e) = hub.send_message(&home, &message, opts).await {
                error!(error = %e, "notification consumer: send_message failed");
            }
        }

        info!("ChatHub: notification consumer stopped");
    }
}

// ── ChatHubApi impl ───────────────────────────────────────────────────────────

#[async_trait]
impl ChatHubApi for ChatHub {
    async fn register(&self, source_id: &str) {
        self.register(source_id).await
    }

    async fn send_message(
        &self,
        source_id: &str,
        prompt: &str,
        opts: SendMessageOptions,
    ) -> anyhow::Result<()> {
        self.send_message(source_id, prompt, opts).await
    }

    async fn clear(&self, source_id: &str) -> anyhow::Result<i64> {
        self.clear(source_id).await
    }

    fn events(&self, source_id: &str) -> broadcast::Receiver<GlobalEvent> {
        self.events(source_id)
    }

    async fn set_home(&self, source_id: &str) -> anyhow::Result<()> {
        self.set_home(source_id).await
    }

    async fn context_info(&self, source_id: &str) -> anyhow::Result<(Option<i64>, Option<i64>)> {
        self.context_info(source_id).await
    }

    async fn force_compact(&self, source_id: &str) -> anyhow::Result<bool> {
        self.force_compact(source_id).await
    }

    async fn resume(&self, source_id: &str) -> anyhow::Result<()> {
        self.resume(source_id).await
    }

    async fn approve(&self, request_id: i64) {
        self.approve(request_id).await
    }

    async fn reject(&self, request_id: i64, note: String) {
        self.reject(request_id, note).await
    }

    async fn resolve_question(&self, source_id: &str, request_id: i64, answer: String) {
        if let Ok(handler) = self.session_handler(source_id).await {
            handler.resolve_question(request_id, answer).await;
        } else {
            warn!(source_id, request_id, "ChatHubApi::resolve_question: no session handler");
        }
    }

    async fn cancel(&self, source_id: &str) {
        self.cancel(source_id).await
    }
}
