use std::sync::atomic::Ordering;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, trace, warn};

use crate::core::approval::GateResult;
use crate::core::tools::tool_names as tn;
use crate::core::chat_event_bus::ToolCallEvent;
use crate::core::chatbot::{ChatOptions, LlmTurn};
use crate::core::db::{chat_history, chat_llm_tools};
use crate::core::events::ServerEvent;
use crate::core::tools::{is_file_write_tool, ToolDescriptionLength};

use super::{ApprovalDecision, ChatSessionHandler, TurnOutcome};
use super::interface_tools::AgentRunConfig;

impl ChatSessionHandler {
    /// Inner loop of an agent (root or sub). Persists messages to `stack_id`,
    /// emits Thinking/ToolStart/ToolDone/PendingWrite/ApprovalRequired/AgentStart/AgentDone events.
    /// Returns the outcome; the caller decides what to emit on completion
    /// (Done for root, AgentDone+tool-result for sub-agents).
    pub(super) fn run_agent_turn<'a>(
        &'a self,
        stack_id: i64,
        config:   &'a AgentRunConfig,
        token:    &'a CancellationToken,
        tx:       &'a mpsc::Sender<ServerEvent>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<TurnOutcome>> + Send + 'a>> {
        Box::pin(async move {
        let pool = &self.db;

        // Resolve the initial model. `cur_name`/`cur_llm` are updated in-place
        // when the fallback logic switches to a different model mid-turn.
        let mut cur_name = config.client_name.clone();
        let mut cur_llm  = self.llm_manager.get(&cur_name).await
            .ok_or_else(|| anyhow::anyhow!("LLM client '{}' not found", cur_name))?;

        // Scope/strength needed for fallback re-selection.
        let meta         = crate::core::agents::load_meta(&config.agent_id).ok();
        let req_scope    = meta.as_ref().and_then(|m| m.scope.as_deref()).map(str::to_string);
        let req_strength = meta.as_ref().and_then(|m| m.strength);

        // Accumulates tool calls across all rounds for the event bus.
        let mut all_tool_calls: Vec<ToolCallEvent> = Vec::new();

        for round in 0..self.max_tool_rounds {
            if token.is_cancelled() {
                return Ok(TurnOutcome::Cancelled);
            }
            trace!(session_id = self.session_id, stack_id, agent_id = config.agent_id, round, "starting round");

            let active_grants_snapshot = config.active_mcp_grants
                .read()
                .map(|g| g.clone())
                .unwrap_or_default();

            // Messages are (re)built with the current model's prompt_cache flag.
            // On fallback within the same round they are rebuilt again if the new
            // model has a different prompt_cache setting.
            let mut messages = self.build_openai_messages(pool, stack_id, &config.agent_id, config.extra_system.as_deref(), config.extra_system_dynamic.as_deref(), config.tail_reminder.as_deref(), &active_grants_snapshot, &config.system_substitutions, cur_llm.prompt_cache).await?;
            let tool_defs    = config.all_tool_defs();

            // ── LLM call with automatic fallback ──────────────────────────────
            // On a retriable error (5xx / network) we try up to MAX_LLM_ATTEMPTS
            // models in priority order before giving up for this round.
            const MAX_LLM_ATTEMPTS: usize = 3;
            let mut tried_this_round: Vec<String> = vec![cur_name.clone()];

            let turn_result = loop {
                let options = ChatOptions {
                    model:       cur_llm.model.clone(),
                    max_tokens:  None,
                    temperature: None,
                    session_id:  Some(self.session_id),
                    stack_id:    Some(stack_id),
                };

                // Clone the Arc so the in-flight future does not borrow `cur_llm`
                // across the fallback reassignment below. On cancel we drop the
                // future (aborting the request) and return immediately.
                let client = cur_llm.client.clone();
                let call_result = tokio::select! {
                    _ = token.cancelled() => return Ok(TurnOutcome::Cancelled),
                    r = client.chat_with_tools(&messages, &tool_defs, &options) => r,
                };
                match call_result {
                    Ok(t) => {
                        self.llm_manager.mark_success(&cur_name).await;
                        break Ok(t);
                    }
                    Err(e) => {
                        error!(session_id = self.session_id, client = %cur_name, error = %e, "LLM call failed");
                        self.llm_manager.mark_failure(&cur_name, &e.to_string()).await;

                        let can_fallback = tried_this_round.len() < MAX_LLM_ATTEMPTS
                            && is_retriable_llm_error(&e);

                        if can_fallback {
                            let excluded: Vec<&str> = tried_this_round.iter().map(String::as_str).collect();
                            match self.llm_manager.select_excluding(&excluded, req_scope.as_deref(), req_strength).await {
                                Ok((next_name, next_llm)) => {
                                    warn!(session_id = self.session_id, from = %cur_name, to = %next_name, "LLM fallback");
                                    tx.send(ServerEvent::ModelFallback {
                                        from:   cur_name.clone(),
                                        to:     next_name.clone(),
                                        reason: first_line(&e.to_string()),
                                    }).await.ok();
                                    tried_this_round.push(next_name.clone());
                                    cur_name = next_name;
                                    cur_llm  = next_llm;
                                    // Rebuild messages if the new model uses different
                                    // prompt_cache settings (e.g. switching from
                                    // OpenRouter/Anthropic to DeepSeek).
                                    messages = self.build_openai_messages(pool, stack_id, &config.agent_id, config.extra_system.as_deref(), config.extra_system_dynamic.as_deref(), config.tail_reminder.as_deref(), &active_grants_snapshot, &config.system_substitutions, cur_llm.prompt_cache).await?;
                                }
                                Err(_) => {
                                    tx.send(ServerEvent::LlmFailed {
                                        tried:      tried_this_round.clone(),
                                        last_error: e.to_string(),
                                    }).await.ok();
                                    break Err(e);
                                }
                            }
                        } else {
                            tx.send(ServerEvent::LlmFailed {
                                tried:      tried_this_round.clone(),
                                last_error: e.to_string(),
                            }).await.ok();
                            break Err(e);
                        }
                    }
                }
            };
            let turn_result = turn_result?;

            match turn_result {
                LlmTurn::Message(resp) => {
                    let message_id = chat_history::append(
                        pool, stack_id, &chat_history::Role::Assistant, &resp.content, false,
                        resp.reasoning_content.as_deref(),
                    ).await?;
                    chat_history::set_model_db_id(pool, message_id, cur_llm.model_db_id).await?;
                    if let (Some(i), Some(o)) = (resp.input_tokens, resp.output_tokens) {
                        chat_history::set_usage(pool, message_id, i, o, 0, resp.cost).await?;
                    }
                    return Ok(TurnOutcome::Final {
                        content:       resp.content,
                        message_id,
                        input_tokens:  resp.input_tokens,
                        output_tokens: resp.output_tokens,
                        truncated:     resp.truncated,
                        tool_calls:    all_tool_calls,
                    });
                }

                LlmTurn::ToolCalls { content: assistant_text, calls, input_tokens, output_tokens, reasoning_content, cost, .. } => {
                    let message_id = chat_history::append(
                        pool, stack_id, &chat_history::Role::Assistant, &assistant_text, false,
                        reasoning_content.as_deref(),
                    ).await?;
                    chat_history::set_model_db_id(pool, message_id, cur_llm.model_db_id).await?;
                    if let (Some(i), Some(o)) = (input_tokens, output_tokens) {
                        chat_history::set_usage(pool, message_id, i, o, 0, cost).await?;
                    }
                    if !assistant_text.trim().is_empty() || input_tokens.is_some() {
                        tx.send(ServerEvent::Thinking {
                            message_id, content: assistant_text, input_tokens, output_tokens,
                        }).await.ok();
                    }

                    for call in &calls {
                        // Stop before each call so a /stop (or a cancelled sub-agent,
                        // which shares this token) aborts the rest of the round.
                        if token.is_cancelled() {
                            return Ok(TurnOutcome::Cancelled);
                        }
                        let args_str = serde_json::to_string(&call.arguments)
                            .unwrap_or_else(|_| "{}".to_string());
                        let tool_call_id = chat_llm_tools::append(
                            pool, message_id, &call.name, &args_str,
                        ).await?;
                        tx.send(ServerEvent::ToolStart {
                            tool_call_id, message_id,
                            name:        call.name.clone(),
                            arguments:   call.arguments.clone(),
                            label_short: self.tools.describe_call(&call.name, &call.arguments, ToolDescriptionLength::Short),
                            label_full:  self.tools.describe_call(&call.name, &call.arguments, ToolDescriptionLength::Full),
                        }).await.ok();

                        // ── Working directory injection ─────────────────────────────────
                        // Resolve relative paths and inject workdir using the RunContext
                        // effective_working_dir. Original call.arguments are preserved for
                        // ToolStart event / DB logging above; effective_args is used below.
                        let mut effective_args = call.arguments.clone();
                        {
                            let wd = self.run_context.read().await
                                .as_ref()
                                .map(|rc| rc.effective_working_dir());
                            if let Some(wd) = wd {
                                if let Some(path) = effective_args["path"].as_str() {
                                    if !std::path::Path::new(path).is_absolute() {
                                        effective_args["path"] = serde_json::Value::String(
                                            wd.join(path).to_string_lossy().into_owned()
                                        );
                                    }
                                }
                                if call.name == tn::EXECUTE_CMD && effective_args.get("workdir").is_none() {
                                    effective_args["workdir"] = serde_json::Value::String(
                                        wd.to_string_lossy().into_owned()
                                    );
                                }
                            }
                        }

                        // ── Approval gate ──────────────────────────────────────────────
                        let category = self.tools.category_of(&call.name);
                        let group_id = self.tool_group_id().await;
                        let gate = if is_file_write_tool(&call.name) {
                            let path = effective_args["path"].as_str().unwrap_or("");
                            let pre_allowed = self.run_context.read().await
                                .as_ref()
                                .map(|rc| rc.is_write_allowed(path))
                                .unwrap_or(false);
                            if pre_allowed { GateResult::Allow }
                            else {
                                self.approval.check(
                                    self.session_id, category,
                                    &config.agent_id, &self.source, &call.name, &effective_args,
                                    group_id.as_deref(),
                                ).await
                            }
                        } else {
                            self.approval.check(
                                self.session_id, category,
                                &config.agent_id, &self.source, &call.name, &effective_args,
                                group_id.as_deref(),
                            ).await
                        };
                        match gate {
                            GateResult::Deny => {
                                let msg = "Tool call denied by approval policy.".to_string();
                                info!(session_id = self.session_id, tool = %call.name, tool_call_id, "approval: denied");
                                chat_llm_tools::fail(pool, tool_call_id, &msg).await?;
                                tx.send(ServerEvent::ToolError { tool_call_id, error: msg }).await.ok();
                                continue;
                            }
                            GateResult::Require => {
                            if self.auto_deny_approvals.load(Ordering::Relaxed) {
                                let msg = "Tool call auto-denied: this session does not support approval requests.".to_string();
                                info!(session_id = self.session_id, tool = %call.name, tool_call_id, "auto_deny_approvals: denied");
                                chat_llm_tools::fail(pool, tool_call_id, &msg).await?;
                                tx.send(ServerEvent::ToolError { tool_call_id, error: msg }).await.ok();
                                continue;
                            }

                                // Mark as pending before suspending so restart/refresh shows
                                // the approval form (not "Interrupted") and auto-resume re-gates.
                                chat_llm_tools::set_approval_pending(pool, tool_call_id).await?;

                                let ctx_label = self.context_label.read().ok()
                                    .and_then(|g| g.clone());
                                let (request_id, approve_rx) = self.approval.register(
                                    self.session_id, tool_call_id, &call.name,
                                    effective_args.clone(), &config.agent_id, &self.source,
                                    ctx_label.as_deref(), category,
                                ).await;
                                info!(session_id = self.session_id, tool = %call.name, tool_call_id, request_id, "approval: waiting for human");
                                self.emit_approval_event(tx, request_id, tool_call_id, &call.name, &effective_args).await;

                                match approve_rx.await {
                                    Ok(ApprovalDecision::Approved) => {
                                        info!(session_id = self.session_id, request_id, tool = %call.name, "approval: approved");
                                    }
                                    Ok(ApprovalDecision::Rejected { note }) => {
                                        info!(session_id = self.session_id, request_id, tool = %call.name, %note, "approval: rejected");
                                        let msg = if note.is_empty() {
                                            "User rejected this tool call.".to_string()
                                        } else {
                                            format!("User rejected this tool call. Reason: {note}")
                                        };
                                        chat_llm_tools::fail(pool, tool_call_id, &msg).await?;
                                        tx.send(ServerEvent::ToolError { tool_call_id, error: msg }).await.ok();
                                        continue;
                                    }
                                    Err(_) => {
                                        // WS closed while waiting — session is orphaned.
                                        warn!(session_id = self.session_id, request_id, "approval channel closed (WS disconnected), aborting");
                                        return Ok(TurnOutcome::Cancelled);
                                    }
                                }
                            }
                            GateResult::Allow => {}
                        }
                        // ── End approval gate ──────────────────────────────────────────

                        debug!(session_id = self.session_id, tool = %call.name, tool_call_id, "dispatching");

                        // `restart` calls process::exit — mark the call done in the DB first
                        // so it doesn't reappear as `pending` after the supervisor relaunches.
                        if call.name == tn::RESTART {
                            info!(session_id = self.session_id, tool_call_id, "restart approved — marking done then exiting");
                            chat_llm_tools::complete(pool, tool_call_id, "Riavvio avviato.").await?;
                            tx.send(ServerEvent::ToolDone { tool_call_id, result: "Riavvio avviato.".to_string() }).await.ok();
                            // Use _exit() to skip C atexit handlers (e.g. Metal GPU cleanup in
                            // whisper-rs/ggml, which aborts with SIGABRT and yields exit code 134
                            // instead of 255 — breaking the run.sh restart supervisor).
                            unsafe { libc::_exit(-1) }
                        }

                        let dispatch_result: anyhow::Result<String> = if
                            (call.name == "execute_task" && effective_args["mode"].as_str() == Some("sync") && effective_args.get("agent_id").is_some())
                            || call.name == tn::RUN_SUBTASK
                        {
                            self.dispatch_sub_agent(stack_id, config, tool_call_id, &effective_args, token, tx).await
                        } else if call.name == tn::EXECUTE_CMD {
                            // Cancellable path: a /stop drops this future, and
                            // `kill_on_drop(true)` kills the spawned shell process.
                            tokio::select! {
                                _ = token.cancelled() => Err(anyhow::anyhow!("execute_cmd interrotto dall'utente")),
                                r = crate::core::tools::exec::run_from_args(&effective_args) => r,
                            }
                        } else if call.name == tn::UPDATE_SCRATCHPAD {
                            self.dispatch_update_scratchpad(&effective_args).await
                        } else if call.name == tn::WRITE_TODOS {
                            self.dispatch_write_todos(&effective_args).await
                        } else if call.name == tn::ASK_USER_CLARIFICATION {
                            self.dispatch_ask_user_clarification(tool_call_id, &effective_args, tx).await
                        } else if call.name == "task_completed" {
                            // Defensive stub: if the LLM somehow calls this itself, return a hint.
                            // Real delivery is via inject_async_result (synthetic message from the system).
                            let task_id = effective_args["task_id"].as_i64().unwrap_or(0);
                            Ok(format!(r#"{{"status":"not_ready","task_id":{task_id},"message":"This tool is invoked by the system, not by you. Do not call it again — the result will arrive automatically as a new message in this conversation."}}"#))
                        } else if let Some(tool) = config.interface_tools.iter().find(|t| t.name() == call.name) {
                            (tool.handler)(effective_args.clone()).await
                        } else if let Some(tool) = config.memory_tools.iter().find(|t| t.name() == call.name) {
                            tool.execute_async(effective_args.clone()).await
                        } else if let Some(tool) = config.image_tools.iter().find(|t| t.name() == call.name) {
                            tool.execute_async(effective_args.clone()).await
                        } else {
                            self.execute_tool(&call.name, effective_args.clone()).await
                        };

                        match dispatch_result {
                            Ok(result) => {
                                debug!(session_id = self.session_id, tool = %call.name, tool_call_id, result_len = result.len(), "tool done");
                                chat_llm_tools::complete(pool, tool_call_id, &result).await?;
                                if is_file_write_tool(&call.name) {
                                    if let Some(p) = effective_args["path"].as_str() {
                                        let path = crate::core::approval::normalize_path(p);
                                        tx.send(ServerEvent::FileChanged { path }).await.ok();
                                    }
                                }
                                tx.send(ServerEvent::ToolDone { tool_call_id, result: result.clone() }).await.ok();
                                all_tool_calls.push(ToolCallEvent {
                                    name:      call.name.clone(),
                                    arguments: Some(serde_json::to_string(&effective_args).unwrap_or_default()),
                                    result:    Some(result),
                                    status:    "done".to_string(),
                                });
                            }
                            Err(err) => {
                                // WS disconnected while waiting for a clarification answer.
                                // Tool stays 'pending' in DB — resume_pending_tools re-dispatches on reconnect.
                                if matches!(err.downcast_ref::<super::AgentFlowSignal>(), Some(super::AgentFlowSignal::QuestionChannelClosed)) {
                                    warn!(session_id = self.session_id, tool_call_id, "clarification channel closed — aborting turn (tool stays pending)");
                                    return Ok(TurnOutcome::Cancelled);
                                }
                                let msg = err.to_string();
                                warn!(session_id = self.session_id, tool = %call.name, tool_call_id, error = %msg, "tool failed");
                                chat_llm_tools::fail(pool, tool_call_id, &msg).await?;
                                tx.send(ServerEvent::ToolError { tool_call_id, error: msg.clone() }).await.ok();
                                all_tool_calls.push(ToolCallEvent {
                                    name:      call.name.clone(),
                                    arguments: Some(serde_json::to_string(&effective_args).unwrap_or_default()),
                                    result:    Some(msg),
                                    status:    "failed".to_string(),
                                });
                            }
                        }
                    }
                }
            }
        }

        Ok(TurnOutcome::Exhausted)
        }) // end Box::pin
    }
}

fn is_retriable_llm_error(e: &anyhow::Error) -> bool {
    let msg = e.to_string().to_lowercase();
    // Never retry client errors — the request itself is malformed or unauthorized.
    // 400 is excluded: some providers reject valid requests that others accept
    // (e.g. DeepSeek requires reasoning_content echo, OpenAI does not), so
    // retrying on a different model can succeed.
    for code in ["401", "403", "404", "422"] {
        if msg.contains(code) {
            return false;
        }
    }
    true
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or(s).to_string()
}
