use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use serde_json::Value;
use tokio::sync::mpsc;
use tracing::info;

use crate::core::db::{chat_history, chat_llm_tools, chat_sessions_stack, scratchpad, stack_mcp_grants};
use crate::core::events::ServerEvent;

use super::{ChatSessionHandler, MAX_AGENT_DEPTH};
use super::interface_tools::{AgentRunConfig, InterfaceTool, ToolFuture};
use super::config::show_mcp_tools_tool_def;

impl ChatSessionHandler {
    /// Handles the synthetic `call_agent` tool call: validates args, creates a
    /// child stack frame, loads any persisted stack-scoped MCP grants (for restart
    /// recovery), runs the sub-agent loop, cleans up stack grants, terminates the
    /// frame, and returns the sub-agent's final assistant message as the tool result.
    pub(super) async fn dispatch_call_agent(
        &self,
        parent_stack_id:     i64,
        parent_config:       &AgentRunConfig,
        parent_tool_call_id: i64,
        args:                &Value,
        tx:                  &mpsc::Sender<ServerEvent>,
    ) -> anyhow::Result<String> {
        let pool = &self.db;

        let target_id = args["agent_id"].as_str()
            .ok_or_else(|| anyhow::anyhow!("call_agent: missing required argument `agent_id`"))?;
        let prompt = args["prompt"].as_str()
            .ok_or_else(|| anyhow::anyhow!("call_agent: missing required argument `prompt`"))?;

        if target_id == parent_config.agent_id {
            anyhow::bail!("call_agent: an agent cannot call itself (`{target_id}`)");
        }
        if target_id == "main" {
            anyhow::bail!("call_agent: the `main` agent is the root entry point and cannot be invoked as a sub-agent");
        }

        // Validate the target exists and load its meta (for client resolution).
        let target_meta = crate::core::agents::load_meta(target_id)
            .map_err(|e| anyhow::anyhow!("call_agent: agent `{target_id}` not found: {e}"))?;

        if target_meta.is_system_agent {
            anyhow::bail!("call_agent: agent `{target_id}` is a system agent and cannot be invoked via call_agent");
        }

        // Depth cap.
        let parent_frame = chat_sessions_stack::find_by_id(pool, parent_stack_id).await?
            .ok_or_else(|| anyhow::anyhow!("call_agent: parent stack frame not found"))?;
        let new_depth = parent_frame.depth + 1;
        if new_depth > MAX_AGENT_DEPTH {
            anyhow::bail!(
                "call_agent: maximum agent depth ({}) exceeded — refusing to recurse further",
                MAX_AGENT_DEPTH
            );
        }

        // Resolve which LLM client the sub-agent will use.
        let explicit_client = args["client"].as_str()
            .or(target_meta.client.as_deref());
        let (resolved_client, _) = self.llm_manager.resolve(
            explicit_client,
            target_meta.scope.as_deref(),
            target_meta.strength,
        ).await.map_err(|e| anyhow::anyhow!("call_agent: {e}"))?;

        // Create the child stack frame.
        let child = chat_sessions_stack::create(
            pool,
            self.session_id,
            target_id,
            Some(prompt),
            new_depth,
            Some(parent_tool_call_id),
        ).await?;

        // ── Stack-scoped MCP grants ─────────────────────────────────────────────
        //
        // Sub-agents start with zero MCP grants and activate what they need via
        // `show_mcp_tools` (stack-scoped, no session leak).
        //
        // On restart recovery: any grants the sub-agent had already persisted to
        // `stack_mcp_grants` are re-loaded here so execution can resume correctly.
        let persisted_grants = stack_mcp_grants::list_for_stack(pool, child.id)
            .await
            .unwrap_or_default();
        let active_mcp_grants: Arc<RwLock<HashSet<String>>> =
            Arc::new(RwLock::new(persisted_grants.into_iter().collect()));

        // Build the child config, then patch in MCP state and inject show_mcp_tools.
        let mut child_config = parent_config.for_sub_agent(target_id.to_string(), resolved_client.clone());

        // Replace the empty grants arc created by for_sub_agent with the one we just
        // populated from DB (so restart recovery works and the tool shares the same arc).
        child_config.active_mcp_grants = Arc::clone(&active_mcp_grants);

        // Add tools that are only available to sub-agents.
        child_config.base_tool_defs.extend(self.tools.openai_definitions_sub_agents_only());
        child_config.base_tool_defs.push(super::ask_user_clarification_tool_def());

        // Apply the same approval-rules visibility filter as for the parent agent.
        // Sub-agents share the same permission group as their session.
        {
            let group_id    = self.tool_group_id().await;
            let gid         = group_id.as_deref().unwrap_or("default");
            let group_rules = crate::core::db::approval_rules::list_for_group(
                pool, Some(gid),
            ).await.unwrap_or_default();
            child_config.base_tool_defs.retain(|def| {
                let name = def["function"]["name"].as_str().unwrap_or("");
                self.approval.is_tool_visible(&group_rules, name)
            });
        }

        // Inject show_mcp_tools (stack-scoped) so the sub-agent can activate MCPs.
        {
            let pool_clone   = Arc::clone(&self.db);
            let session_id   = self.session_id;
            let stack_id     = child.id;
            let mcp_clone    = Arc::clone(&self.mcp);
            let grants_clone = Arc::clone(&active_mcp_grants);

            let show_tool = crate::core::tools::show_mcp_tools::ShowMcpTools {
                pool:              pool_clone,
                session_id,
                stack_id:          Some(stack_id),
                mcp:               mcp_clone,
                active_mcp_grants: grants_clone,
            };
            let show_tool = Arc::new(show_tool);
            child_config.interface_tools.push(InterfaceTool {
                definition: show_mcp_tools_tool_def(),
                handler: Arc::new(move |args| -> ToolFuture {
                    use crate::core::tools::Tool as _;
                    let tool = Arc::clone(&show_tool);
                    Box::pin(async move {
                        tokio::task::spawn_blocking(move || tool.execute(args))
                            .await
                            .map_err(|e| anyhow::anyhow!("show_mcp_tools task panicked: {e}"))?
                    })
                }),
            });
        }
        // ── End stack-scoped MCP grants ─────────────────────────────────────────

        // Append the caller's prompt as the first message of the sub-agent.
        chat_history::append(pool, child.id, &chat_history::Role::Agent, prompt, false, None).await?;

        let prompt_preview = if prompt.len() > 500 {
            format!("{}…", &prompt[..500])
        } else {
            prompt.to_string()
        };

        tx.send(ServerEvent::AgentStart {
            stack_id:            child.id,
            parent_tool_call_id: parent_tool_call_id,
            agent_id:            target_id.to_string(),
            parent_agent_id:     parent_config.agent_id.clone(),
            depth:               new_depth,
            prompt_preview,
        }).await.ok();

        info!(
            session_id   = self.session_id,
            parent_stack = parent_stack_id,
            child_stack  = child.id,
            target_agent = target_id,
            client       = %resolved_client,
            "call_agent: spawning independent sub-agent task"
        );

        // Obtain Arc<Self> via the weak back-reference set by ChatSessionManager.
        let self_arc = self.weak_self.get()
            .and_then(|w| w.upgrade())
            .ok_or_else(|| anyhow::anyhow!("call_agent: handler Arc no longer alive"))?;

        let tx_child       = tx.clone();
        let child_stack_id = child.id;
        let parent_agent_id = parent_config.agent_id.clone();

        tokio::spawn(async move {
            self_arc.run_child_frame(
                child_stack_id,
                parent_tool_call_id,
                parent_agent_id,
                child_config,
                tx_child,
            ).await;
        });

        // Signal to the parent loop: convert to TurnOutcome::WaitingChild and exit,
        // releasing the processing mutex so the child task can proceed.
        Err(anyhow::Error::new(super::AgentFlowSignal::WaitingChild(child_stack_id)))
    }

    /// Handles the `update_scratchpad` built-in.
    pub(super) async fn dispatch_update_scratchpad(
        &self,
        args: &Value,
    ) -> anyhow::Result<String> {
        let key   = args["key"].as_str().unwrap_or("").to_string();
        let value = args["value"].as_str().unwrap_or("").to_string();
        scratchpad::upsert(&self.db, self.session_id, &key, &value).await
            .map(|_| format!("Scratchpad updated: {key}"))
    }

    /// Handles the `ask_user_clarification` built-in.
    ///
    /// Interactive sessions (web, telegram): sends `AgentQuestion` over the WS channel
    /// and waits for the user to answer inline in the chat.
    ///
    /// Background sessions (cron, tic): registers in `ClarificationManager` so the
    /// Agent Inbox page can surface and resolve the request.
    ///
    /// `tool_call_id` is used to mark the DB row as `pending` before blocking,
    /// so page refreshes and app restarts can distinguish "waiting for input" from
    /// "was executing" and re-ask the question correctly.
    pub(super) async fn dispatch_ask_user_clarification(
        &self,
        tool_call_id: i64,
        args: &Value,
        tx:   &mpsc::Sender<ServerEvent>,
    ) -> anyhow::Result<String> {
        let title    = args["title"].as_str().unwrap_or("Clarification needed").to_string();
        let question = args["question"].as_str().unwrap_or("?").to_string();
        let suggested: Vec<String> = args["suggested_answers"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();

        // Mark as pending before suspending so restart/refresh can re-ask the question.
        chat_llm_tools::set_approval_pending(&self.db, tool_call_id).await?;

        let context_label = self.context_label.read().ok().and_then(|g| g.clone());

        // Always register in ClarificationManager so the question appears in the
        // Agent Inbox for ALL sessions (both interactive web/telegram and background cron/tic).
        let (request_id, rx) = self.clarification.register(
            self.session_id,
            &self.agent_id,
            &self.source,
            context_label.as_deref(),
            &title,
            &question,
            suggested.clone(),
        ).await;

        tracing::debug!(session_id = self.session_id, request_id, is_interactive = self.is_interactive, source = %self.source, "dispatch_ask_user_clarification: routing");
        if self.is_interactive {
            // For interactive sessions, also send the question over WS so it appears
            // inline in the chat. The user can answer from either the chat or the Inbox.
            info!(session_id = self.session_id, request_id, %question, source = %self.source, "agent asking user for clarification (interactive) — sending AgentQuestion");
            let send_result = tx.send(ServerEvent::AgentQuestion {
                request_id,
                tool_call_id,
                title,
                question,
                suggested_answers: suggested,
            }).await;
            if send_result.is_err() {
                tracing::warn!(session_id = self.session_id, request_id, "AgentQuestion send failed — tx receiver dropped");
            } else {
                info!(session_id = self.session_id, request_id, "AgentQuestion sent to bridge");
            }
        } else {
            info!(session_id = self.session_id, request_id, %question, source = %self.source, "background session waiting for clarification");
        }

        // Wait for the answer (from WS via resolve_question → clarification.resolve,
        // or directly from the Inbox REST endpoint).
        rx.await.map_err(|_| anyhow::Error::new(super::AgentFlowSignal::QuestionChannelClosed))
    }
}
