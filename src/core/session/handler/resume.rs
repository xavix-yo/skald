use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::core::approval::GateResult;
use crate::core::db::{chat_history, chat_llm_tools, chat_sessions_stack};
use crate::core::events::ServerEvent;
use crate::core::tools::{ToolDescriptionLength, tool_names as tn};

use super::{ApprovalDecision, ChatSessionHandler, TurnOutcome};
use super::interface_tools::{AgentRunConfig, InterfaceTool};

impl ChatSessionHandler {
    /// Dispatches a single tool call by name+args without going through the LLM loop.
    /// Used by the REST `resolve` endpoint and by `resume_pending_tools`.
    /// Does NOT update the DB — caller is responsible for `complete` / `fail`.
    pub async fn execute_tool(&self, name: &str, args: Value) -> anyhow::Result<String> {
        if let Some((srv, mcp_tool)) = crate::core::mcp::parse_mcp_tool_name(name) {
            return self.mcp.call(srv, mcp_tool, args).await;
        }
        self.tools.dispatch(name, args)
    }

    /// Resumes the LLM loop for the current session WITHOUT appending a new user message.
    /// Intended for use after pending tool calls have been resolved externally
    /// (e.g. via the REST approve endpoint) so the LLM can produce a final response
    /// or make further tool calls using the now-complete history.
    pub async fn resume_turn(
        &self,
        client_name:          Option<String>,
        extra_system_context: Option<String>,
        interface_tools:      Vec<InterfaceTool>,
        tx:                   mpsc::Sender<ServerEvent>,
    ) -> anyhow::Result<()> {
        let _guard = self.processing.lock().await;
        use std::sync::atomic::Ordering;
        self.cancelled.store(false, Ordering::Relaxed);

        let pool = &self.db;
        let mut config = self.build_agent_config(
            client_name, extra_system_context, None, interface_tools,
        ).await?;
        config.tail_reminder = None;

        let stack = match chat_sessions_stack::active_for_session(pool, self.session_id).await? {
            Some(s) => s,
            None    => {
                warn!(session_id = self.session_id, "resume_turn: no active stack, nothing to resume");
                return Ok(());
            }
        };

        info!(session_id = self.session_id, stack_id = stack.id, depth = stack.depth, "resume_turn start");

        // Resume pending/interrupted tools before running the LLM loop.
        let had_pending = self.resume_pending_tools(stack.id, &config, &tx).await?;

        // Guard: skip if no tools were pending AND the last message is a pure-text
        // assistant response (no tool calls). If the last assistant message HAS tool
        // calls (all done), the LLM still needs to see the results and continue —
        // this happens after an async sub-agent completes and resume_turn is called
        // to run the parent.
        if !had_pending {
            let last = chat_history::last_message_for_stack(pool, stack.id).await?;
            if let Some(ref msg) = last {
                if matches!(msg.role, chat_history::Role::Assistant) {
                    let tool_calls = chat_llm_tools::for_message(pool, msg.id).await?;
                    if tool_calls.is_empty() {
                        info!(session_id = self.session_id, stack_id = stack.id, "resume_turn: last message is pure-text assistant, turn already complete — skipping LLM");
                        return Ok(());
                    }
                    // Has tool calls — LLM must respond to the results; fall through.
                }
            }
        }

        let mut current_outcome = self.run_agent_turn(stack.id, &config, &tx).await?;
        let mut current_stack = stack;

        // Cascade completion upward through parent stacks (handles app-restart recovery
        // when a sub-agent was running — child completes, then parent continues).
        // If run_agent_turn returns WaitingChild, a new async task was spawned and will
        // drive the cascade — break immediately so we don't double-execute.
        if matches!(current_outcome, TurnOutcome::WaitingChild { .. }) {
            info!(session_id = self.session_id, "resume_turn: sub-agent dispatched asynchronously — deferring to child task");
            return Ok(());
        }

        loop {
            let Some(parent_tool_call_id) = current_stack.parent_tool_call_id else { break };

            // Determine the result string to propagate to the parent's call_agent tool.
            let (result_str, is_error) = match &current_outcome {
                TurnOutcome::Final { content, .. } => (content.clone(), false),
                TurnOutcome::Cancelled  => (format!("Sub-agent `{}` was cancelled.", current_stack.agent_id), true),
                TurnOutcome::Exhausted  => (format!("Sub-agent `{}` exhausted tool-call rounds.", current_stack.agent_id), true),
                TurnOutcome::WaitingChild { .. } => unreachable!("WaitingChild handled above"),
            };
            let result_preview = if result_str.len() > 500 {
                format!("{}…", &result_str[..500])
            } else {
                result_str.clone()
            };

            // Complete or fail the parent's call_agent tool call.
            if is_error {
                chat_llm_tools::fail(pool, parent_tool_call_id, &result_str).await?;
            } else {
                chat_llm_tools::complete(pool, parent_tool_call_id, &result_str).await?;
            }

            // Terminate the child stack so active_for_session() returns the parent next.
            let _ = chat_sessions_stack::terminate(pool, current_stack.id).await;

            // Emit events to the frontend.
            if is_error {
                tx.send(ServerEvent::ToolError {
                    tool_call_id: parent_tool_call_id,
                    error: result_str,
                }).await.ok();
            } else {
                tx.send(ServerEvent::ToolDone {
                    tool_call_id: parent_tool_call_id,
                    result: result_str,
                }).await.ok();
            }

            // Now the parent is the deepest active stack.
            let parent_stack = match chat_sessions_stack::active_for_session(pool, self.session_id).await? {
                Some(s) => s,
                None    => {
                    warn!(session_id = self.session_id, "resume_turn cascade: no active stack after child terminated");
                    break;
                }
            };

            tx.send(ServerEvent::AgentDone {
                stack_id:        current_stack.id,
                agent_id:        current_stack.agent_id.clone(),
                parent_agent_id: parent_stack.agent_id.clone(),
                result_preview,
            }).await.ok();

            info!(
                session_id = self.session_id,
                child_stack  = current_stack.id,
                parent_stack = parent_stack.id,
                depth        = parent_stack.depth,
                "resume_turn: cascading to parent stack"
            );

            self.resume_pending_tools(parent_stack.id, &config, &tx).await?;
            current_outcome = self.run_agent_turn(parent_stack.id, &config, &tx).await?;
            current_stack = parent_stack;

            if matches!(current_outcome, TurnOutcome::WaitingChild { .. }) {
                info!(session_id = self.session_id, "resume_turn cascade: sub-agent dispatched asynchronously — deferring to child task");
                return Ok(());
            }
        }

        // current_stack is now the root (depth=0); emit the final event.
        // WaitingChild is handled above — it never reaches here.
        match current_outcome {
            TurnOutcome::Final { content, message_id, input_tokens, output_tokens, truncated, .. } => {
                info!(session_id = self.session_id, "resume_turn done");
                if truncated {
                    warn!(session_id = self.session_id, "response truncated");
                    tx.send(ServerEvent::Truncated { output_tokens }).await.ok();
                }
                tx.send(ServerEvent::Done {
                    message_id,
                    stack_id: current_stack.id,
                    content,
                    input_tokens,
                    output_tokens,
                }).await.ok();
            }
            TurnOutcome::Cancelled => {
                info!(session_id = self.session_id, "resume_turn cancelled");
                tx.send(ServerEvent::Error { message: "Interrotto dall'utente.".to_string() }).await.ok();
            }
            TurnOutcome::Exhausted => {
                error!(session_id = self.session_id, "resume_turn exhausted tool rounds");
                tx.send(ServerEvent::Error {
                    message: "Exceeded tool-call rounds without a final answer.".to_string(),
                }).await.ok();
            }
            TurnOutcome::WaitingChild { .. } => {
                // Handled above before the cascade loop — unreachable here.
            }
        }
        Ok(())
    }

    /// Called at the start of `handle_message` (and by the REST endpoint after a manual
    /// resolve). Finds any `pending` tool calls left from a previous interrupted session,
    /// re-runs them through the approval gate, executes approved ones, and fails rejected
    /// or denied ones — so `run_agent_turn` sees complete history and can continue cleanly.
    pub async fn resume_pending_tools(
        &self,
        stack_id: i64,
        config:   &AgentRunConfig,
        tx:       &mpsc::Sender<ServerEvent>,
    ) -> anyhow::Result<bool> {
        let pool    = &self.db;
        let pending = chat_llm_tools::pending_for_stack(pool, stack_id).await?;
        if pending.is_empty() {
            return Ok(false);
        }

        info!(
            session_id = self.session_id, stack_id,
            count = pending.len(), "resuming pending tool calls"
        );

        for tc in pending {
            let args: Value = tc.arguments.as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(Value::Object(Default::default()));

            // `call_agent` with 'running' status means a sub-agent stack was active.
            // The cascade in resume_turn() handles it by running the child stack to
            // completion and propagating the result up — skip it here.
            if tc.name == tn::CALL_AGENT {
                info!(session_id = self.session_id, tool_call_id = tc.id, "resume: skipping call_agent (handled by stack cascade)");
                continue;
            }

            // `ask_user_clarification` is a synthetic tool (not in the registry).
            // Re-dispatch it directly so the question is re-asked to the user.
            if tc.name == tn::ASK_USER_CLARIFICATION {
                info!(session_id = self.session_id, tool_call_id = tc.id, "resume: re-asking clarification question");
                tx.send(ServerEvent::ToolStart {
                    tool_call_id: tc.id,
                    message_id:   tc.message_id,
                    name:         tc.name.clone(),
                    arguments:    args.clone(),
                    label_short:  self.tools.describe_call(&tc.name, &args, ToolDescriptionLength::Short),
                    label_full:   self.tools.describe_call(&tc.name, &args, ToolDescriptionLength::Full),
                }).await.ok();
                let result = self.dispatch_ask_user_clarification(tc.id, &args, tx).await;
                match result {
                    Ok(answer) => {
                        chat_llm_tools::complete(pool, tc.id, &answer).await?;
                        tx.send(ServerEvent::ToolDone { tool_call_id: tc.id, result: answer }).await.ok();
                    }
                    Err(e) if matches!(e.downcast_ref::<super::AgentFlowSignal>(), Some(super::AgentFlowSignal::QuestionChannelClosed)) => {
                        // WS disconnected again mid-resume. Tool stays 'pending' — next resume re-asks.
                        warn!(session_id = self.session_id, tool_call_id = tc.id, "clarification channel closed during resume — aborting");
                        return Ok(true);
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        chat_llm_tools::fail(pool, tc.id, &msg).await?;
                        tx.send(ServerEvent::ToolError { tool_call_id: tc.id, error: msg }).await.ok();
                    }
                }
                continue;
            }

            // Announce the tool is being re-tried.
            tx.send(ServerEvent::ToolStart {
                tool_call_id: tc.id,
                message_id:   tc.message_id,
                name:         tc.name.clone(),
                arguments:    args.clone(),
                label_short:  self.tools.describe_call(&tc.name, &args, ToolDescriptionLength::Short),
                label_full:   self.tools.describe_call(&tc.name, &args, ToolDescriptionLength::Full),
            }).await.ok();

            // Re-run through the approval gate with current rules.
            let category = self.tools.category_of(&tc.name);
            let gate = self.approval.check(
                self.session_id, category,
                &config.agent_id,
                &self.source,
                &tc.name,
                &args,
            ).await;

            match gate {
                GateResult::Deny => {
                    let msg = "Tool call denied by approval policy.".to_string();
                    chat_llm_tools::fail(pool, tc.id, &msg).await?;
                    tx.send(ServerEvent::ToolError { tool_call_id: tc.id, error: msg }).await.ok();
                    continue;
                }

                GateResult::Require => {
                    // Mark as pending before suspending (idempotent if already 'pending').
                    chat_llm_tools::set_approval_pending(pool, tc.id).await?;

                    let ctx_label = self.context_label.read().ok().and_then(|g| g.clone());
                    let (request_id, rx) = self.approval.register(
                        self.session_id, tc.id, &tc.name, args.clone(),
                        &config.agent_id, &self.source,
                        ctx_label.as_deref(),
                    ).await;

                    info!(session_id = self.session_id, tool = tc.name, request_id, "resume: waiting for approval");
                    self.emit_approval_event(tx, request_id, tc.id, &tc.name, &args).await;

                    match rx.await {
                        Ok(ApprovalDecision::Approved) => { /* fall through to execution */ }
                        Ok(ApprovalDecision::Rejected { note }) => {
                            let msg = if note.is_empty() {
                                "User rejected this tool call.".to_string()
                            } else {
                                format!("User rejected. Reason: {note}")
                            };
                            chat_llm_tools::fail(pool, tc.id, &msg).await?;
                            tx.send(ServerEvent::ToolError {
                                tool_call_id: tc.id, error: msg,
                            }).await.ok();
                            continue;
                        }
                        Err(_) => {
                            warn!(session_id = self.session_id, "approval channel closed during resume");
                            return Ok(true); // pending still, WS disconnected
                        }
                    }
                }

                GateResult::Allow => { /* execute freely */ }
            }

            // `restart` calls process::exit and never returns — mark done first.
            if tc.name == tn::RESTART {
                info!(session_id = self.session_id, tool_call_id = tc.id, "restart approved (resume) — marking done then exiting");
                chat_llm_tools::complete(pool, tc.id, "Riavvio avviato.").await?;
                tx.send(ServerEvent::ToolDone {
                    tool_call_id: tc.id,
                    result: "Riavvio avviato.".to_string(),
                }).await.ok();
                std::process::exit(-1);
            }

            // Execute the tool — check memory tools first, then registry.
            let tool_result = if let Some(tool) = config.memory_tools.iter().find(|t| t.name() == tc.name) {
                tool.execute_async(args.clone()).await
            } else {
                self.execute_tool(&tc.name, args).await
            };
            match tool_result {
                Ok(result) => {
                    chat_llm_tools::complete(pool, tc.id, &result).await?;
                    tx.send(ServerEvent::ToolDone {
                        tool_call_id: tc.id, result,
                    }).await.ok();
                }
                Err(e) => {
                    let msg = e.to_string();
                    chat_llm_tools::fail(pool, tc.id, &msg).await?;
                    tx.send(ServerEvent::ToolError {
                        tool_call_id: tc.id, error: msg,
                    }).await.ok();
                }
            }
        }

        Ok(true)
    }
}
