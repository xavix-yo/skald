use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use axum::{
    Json,
    extract::{Path, Query, State},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::SqlitePool;

use crate::core::db::{chat_history, chat_llm_tools, chat_sessions, chat_sessions_stack, sources};
use crate::core::db::chat_sessions_stack::SessionStack;
use std::sync::Arc;
use crate::core::skald::Skald;
use crate::core::session::handler::ApprovalDecision;
use crate::core::tools::{ToolRegistry, ToolDescriptionLength, tool_names as tn};

use super::ApiError;

// ── POST /api/sessions — start a new conversation ─────────────────────────────

#[derive(Deserialize)]
pub struct CreateQuery {
    #[serde(default = "default_source")]
    pub source: String,
}

fn default_source() -> String { "web".to_string() }

pub async fn create(
    State(skald): State<Arc<Skald>>,
    Query(q): Query<CreateQuery>,
) -> Result<Json<Value>, ApiError> {
    // Resolve agent + RunContext from the source so project chats reset with the
    // coordinator agent (not the default `main`), then provision a fresh session.
    let (agent, rc) = super::projects::provisioning_for_source(&skald, &q.source).await?;
    skald.chat_hub.provision_session(&q.source, &agent, rc.as_ref(), true).await?;
    Ok(Json(json!({})))
}

// ── GET /api/web/messages ─────────────────────────────────────────────────────

pub async fn web_messages(
    State(skald): State<Arc<Skald>>,
) -> Result<Json<Vec<Value>>, ApiError> {
    messages_for_source(&skald, "web").await
}

// ── GET /api/:source/messages ─────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct SourcePath { pub source: String }

pub async fn source_messages(
    State(skald): State<Arc<Skald>>,
    Path(p): Path<SourcePath>,
) -> Result<Json<Vec<Value>>, ApiError> {
    messages_for_source(&skald, &p.source).await
}

async fn messages_for_source(skald: &Arc<Skald>, source: &str) -> Result<Json<Vec<Value>>, ApiError> {
    let session_id = match sources::active_session_id(&skald.db, source).await? {
        Some(id) => id,
        None     => return Ok(Json(vec![])),
    };

    let main_stack = match chat_sessions_stack::main_for_session(&skald.db, session_id).await? {
        Some(s) => s,
        None    => return Ok(Json(vec![])),
    };

    let subagent_map: HashMap<i64, SessionStack> =
        chat_sessions_stack::all_for_session(&skald.db, session_id)
            .await?
            .into_iter()
            .filter_map(|s| s.parent_tool_call_id.map(|tc_id| (tc_id, s)))
            .collect();

    let mut items: Vec<Value> = Vec::new();
    build_items(&skald.db, &skald.tools, &main_stack, &subagent_map, &mut items).await?;

    Ok(Json(items))
}

// ── POST /api/web/tools/:tool_call_id/resolve — approve/reject pending tool ───

#[derive(Deserialize)]
pub struct ResolveToolPath {
    pub tool_call_id: i64,
}

#[derive(Deserialize)]
pub struct ResolveToolBody {
    /// `"approve"` or `"reject"`
    pub action: String,
    #[serde(default)]
    pub note: String,
}

#[derive(Serialize)]
pub struct ResolveToolResponse {
    pub tool_call_id: i64,
    pub status:       String,
    pub result:       Option<String>,
}

/// Approve or reject a `pending` tool call from an interrupted session.
/// Session is resolved automatically from the "web" source's active session.
pub async fn web_resolve_tool(
    State(skald): State<Arc<Skald>>,
    Path(p): Path<ResolveToolPath>,
    Json(body): Json<ResolveToolBody>,
) -> Result<Json<ResolveToolResponse>, ApiError> {
    let session_id = sources::active_session_id(&skald.db, "web")
        .await?
        .ok_or_else(|| anyhow::anyhow!("no active web session"))?;

    let tc = sqlx::query_as::<_, (i64, String, Option<String>, String)>(
        "SELECT t.id, t.name, t.arguments, t.status
         FROM   chat_llm_tools t
         JOIN   chat_history h ON h.id = t.message_id
         JOIN   chat_sessions_stack ss ON ss.id = h.session_stack_id
         WHERE  t.id = ? AND ss.session_id = ?",
    )
    .bind(p.tool_call_id)
    .bind(session_id)
    .fetch_optional(&*skald.db)
    .await?
    .ok_or_else(|| anyhow::anyhow!(
        "tool_call_id {} not found in current web session", p.tool_call_id
    ))?;

    let (tc_id, tc_name, tc_args_raw, tc_status) = tc;

    if tc_status != "pending" {
        return Err(anyhow::anyhow!(
            "tool_call {} is not pending (status: {})", tc_id, tc_status
        ).into());
    }

    let args: Value = tc_args_raw.as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(Value::Object(Default::default()));

    if body.action == "reject" {
        let note = if body.note.is_empty() {
            "Rejected via API.".to_string()
        } else {
            format!("Rejected via API: {}", body.note)
        };
        let live = skald.approval
            .resolve_for_tool_call(tc_id, ApprovalDecision::Rejected { note: note.clone() })
            .await;
        if !live {
            chat_llm_tools::fail(&skald.db, tc_id, &note).await?;
        }
        return Ok(Json(ResolveToolResponse {
            tool_call_id: tc_id,
            status:       "failed".to_string(),
            result:       Some(note),
        }));
    }

    // `restart` calls process::exit — mark done in DB first.
    if tc_name == tn::RESTART {
        chat_llm_tools::complete(&skald.db, tc_id, "Riavvio avviato.").await?;
        // Use _exit() to skip C atexit handlers (e.g. Metal GPU cleanup in
        // whisper-rs/ggml, which aborts with SIGABRT and yields exit code 134
        // instead of 255 — breaking the run.sh restart supervisor).
        unsafe { libc::_exit(-1) }
    }

    // ── Live path: LLM loop is blocked waiting for approval ──────────────────
    if skald.approval
        .resolve_for_tool_call(tc_id, ApprovalDecision::Approved)
        .await
    {
        return Ok(Json(ResolveToolResponse {
            tool_call_id: tc_id,
            status:       "running".to_string(),
            result:       None,
        }));
    }

    // ── Post-restart path: no loop in memory, execute directly ───────────────
    let handler = skald.chat_hub.session_handler("web").await?;
    match handler.execute_tool(&tc_name, args).await {
        Ok(result) => {
            chat_llm_tools::complete(&skald.db, tc_id, &result).await?;
            Ok(Json(ResolveToolResponse {
                tool_call_id: tc_id,
                status:       "done".to_string(),
                result:       Some(result),
            }))
        }
        Err(e) => {
            let msg = e.to_string();
            chat_llm_tools::fail(&skald.db, tc_id, &msg).await?;
            Err(anyhow::anyhow!(msg).into())
        }
    }
}

// ── GET /api/sessions — list sessions by source (paginated) ──────────────────

#[derive(Deserialize)]
pub struct ListSessionsQuery {
    pub source:   Option<String>,
    #[serde(default = "default_page")]
    pub page:     i64,
    #[serde(default = "default_per_page")]
    pub per_page: i64,
}

fn default_page()     -> i64 { 1 }
fn default_per_page() -> i64 { 20 }

pub async fn list_sessions(
    State(skald): State<Arc<Skald>>,
    Query(q): Query<ListSessionsQuery>,
) -> Result<Json<Value>, ApiError> {
    let per_page = q.per_page.max(1).min(100);
    let offset   = ((q.page.max(1)) - 1) * per_page;
    let src      = q.source.as_deref();

    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM chat_sessions cs
         WHERE (? IS NULL OR cs.source = ?)",
    )
    .bind(src).bind(src)
    .fetch_one(&*skald.db).await?;

    let rows = sqlx::query_as::<_, (i64, String, String, bool, bool, Option<String>, i64, Option<String>)>(
        "SELECT cs.id, cs.source, cs.agent_id, cs.is_ephemeral, cs.is_interactive,
                cs.created_at,
                COUNT(h.id)       AS message_count,
                MAX(h.created_at) AS last_message_at
         FROM   chat_sessions cs
         LEFT   JOIN chat_sessions_stack ss ON ss.session_id = cs.id AND ss.depth = 0
         LEFT   JOIN chat_history h         ON h.session_stack_id = ss.id AND h.status = 'ok'
         WHERE  (? IS NULL OR cs.source = ?)
         GROUP  BY cs.id
         ORDER  BY cs.id DESC
         LIMIT  ? OFFSET ?",
    )
    .bind(src).bind(src)
    .bind(per_page).bind(offset)
    .fetch_all(&*skald.db).await?;

    let items: Vec<Value> = rows.into_iter().map(|(id, source, agent_id, is_ephemeral, is_interactive, created_at, message_count, last_message_at)| {
        json!({
            "id":              id,
            "source":          source,
            "agent_id":        agent_id,
            "is_ephemeral":    is_ephemeral,
            "is_interactive":  is_interactive,
            "created_at":      created_at,
            "message_count":   message_count,
            "last_message_at": last_message_at,
        })
    }).collect();

    Ok(Json(json!({
        "items":    items,
        "total":    total,
        "page":     q.page.max(1),
        "per_page": per_page,
    })))
}

// ── GET /api/sessions/:id — read-only session detail (debug view) ─────────────

#[derive(Deserialize)]
pub struct SessionIdPath { pub id: i64 }

pub async fn get_session_detail(
    State(skald): State<Arc<Skald>>,
    Path(p): Path<SessionIdPath>,
) -> Result<Json<Value>, ApiError> {
    let session = chat_sessions::find_by_id(&skald.db, p.id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("session {} not found", p.id)))?;

    let created_at: Option<String> = sqlx::query_scalar(
        "SELECT created_at FROM chat_sessions WHERE id = ?",
    )
    .bind(p.id)
    .fetch_optional(&*skald.db)
    .await?;

    let all_stacks = chat_sessions_stack::all_for_session(&skald.db, session.id).await?;

    let subagent_map: HashMap<i64, SessionStack> = all_stacks
        .iter()
        .filter_map(|s| s.parent_tool_call_id.map(|tc_id| (tc_id, s.clone())))
        .collect();

    let main_stack = match all_stacks.into_iter().find(|s| s.depth == 0) {
        Some(s) => s,
        None    => return Ok(Json(json!({
            "session": {
                "id": session.id, "source": session.source,
                "agent_id": session.agent_id, "created_at": created_at,
            },
            "messages": [],
        }))),
    };

    let mut messages: Vec<Value> = Vec::new();
    build_debug_items(&skald.db, &skald.tools, &main_stack, &subagent_map, &mut messages).await?;

    Ok(Json(json!({
        "session": {
            "id":             session.id,
            "source":         session.source,
            "agent_id":       session.agent_id,
            "is_interactive": session.is_interactive,
            "is_ephemeral":   session.is_ephemeral,
            "created_at":     created_at,
        },
        "messages": messages,
    })))
}

/// Like `build_items` but includes synthetic user messages and reasoning content.
/// Used exclusively by the session-detail debug view.
fn build_debug_items<'a>(
    db:           &'a SqlitePool,
    tools:        &'a ToolRegistry,
    stack:        &'a SessionStack,
    subagent_map: &'a HashMap<i64, SessionStack>,
    items:        &'a mut Vec<Value>,
) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
    Box::pin(async move {
        let messages = chat_history::for_stack_all(db, stack.id).await?;

        for msg in &messages {
            let failed = msg.status == "failed";
            match msg.role {
                chat_history::Role::User => {
                    items.push(json!({
                        "kind":         "user",
                        "content":      msg.content,
                        "failed":       failed,
                        "is_synthetic": msg.is_synthetic,
                        "created_at":   msg.created_at,
                    }));
                }
                chat_history::Role::Agent => {}
                chat_history::Role::Assistant => {
                    let tool_calls = chat_llm_tools::for_message(db, msg.id).await?;
                    if tool_calls.is_empty() {
                        items.push(json!({
                            "kind":          "assistant",
                            "content":       msg.content,
                            "reasoning":     msg.reasoning_content,
                            "failed":        failed,
                            "input_tokens":  msg.input_tokens,
                            "output_tokens": msg.output_tokens,
                            "created_at":    msg.created_at,
                        }));
                    } else {
                        if !msg.content.trim().is_empty() || msg.reasoning_content.is_some() {
                            items.push(json!({
                                "kind":          "thinking",
                                "message_id":    msg.id,
                                "content":       msg.content,
                                "reasoning":     msg.reasoning_content,
                                "failed":        failed,
                                "input_tokens":  msg.input_tokens,
                                "output_tokens": msg.output_tokens,
                                "created_at":    msg.created_at,
                            }));
                        }
                        for tc in &tool_calls {
                            let args: Value = tc.arguments.as_deref()
                                .and_then(|s| serde_json::from_str(s).ok())
                                .unwrap_or(Value::Null);

                            let (status, result, error) = match tc.status.as_str() {
                                "done"    => ("done",    tc.result.clone(), None),
                                "pending" => ("pending", None,              None),
                                "running" => ("error",   None,              Some("Interrupted.".to_string())),
                                _         => ("error",   None,              tc.result.clone()),
                            };

                            let label_short = tools.describe_call(&tc.name, &args, ToolDescriptionLength::Short);
                            let label_full  = tools.describe_call(&tc.name, &args, ToolDescriptionLength::Full);
                            items.push(json!({
                                "kind":         "tool",
                                "tool_call_id": tc.id,
                                "name":         tc.name,
                                "label_short":  label_short,
                                "label_full":   label_full,
                                "arguments":    args,
                                "status":       status,
                                "result":       result,
                                "error":        error,
                            }));

                            if let Some(sub_stack) = subagent_map.get(&tc.id) {
                                items.push(json!({
                                    "kind":     "agent",
                                    "stack_id": sub_stack.id,
                                    "agent_id": sub_stack.agent_id,
                                    "depth":    sub_stack.depth,
                                    "done":     true,
                                }));
                                build_debug_items(db, tools, sub_stack, subagent_map, items).await?;
                                items.push(json!({
                                    "kind":     "agent_end",
                                    "agent_id": sub_stack.agent_id,
                                    "depth":    sub_stack.depth,
                                }));
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    })
}

// ── Recursive message-tree builder ────────────────────────────────────────────

fn build_items<'a>(
    db:           &'a SqlitePool,
    tools:        &'a ToolRegistry,
    stack:        &'a SessionStack,
    subagent_map: &'a HashMap<i64, SessionStack>,
    items:        &'a mut Vec<Value>,
) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
    Box::pin(async move {
        let messages = chat_history::for_stack_all(db, stack.id).await?;

        for msg in &messages {
            let failed = msg.status == "failed";
            match msg.role {
                chat_history::Role::User => {
                    // Skip synthetic messages (TIC notifications, etc.) — they are
                    // injected as user turns for the LLM but must not appear in the UI.
                    if msg.is_synthetic {
                        continue;
                    }
                    items.push(json!({ "kind": "user", "content": msg.content, "failed": failed }));
                }
                chat_history::Role::Agent => {}
                chat_history::Role::Assistant => {
                    let tool_calls = chat_llm_tools::for_message(db, msg.id).await?;
                    if tool_calls.is_empty() {
                        items.push(json!({
                            "kind":          "assistant",
                            "content":       msg.content,
                            "failed":        failed,
                            "input_tokens":  msg.input_tokens,
                            "output_tokens": msg.output_tokens,
                        }));
                    } else {
                        if !msg.content.trim().is_empty() {
                            items.push(json!({
                                "kind":          "thinking",
                                "message_id":    msg.id,
                                "content":       msg.content,
                                "failed":        failed,
                                "input_tokens":  msg.input_tokens,
                                "output_tokens": msg.output_tokens,
                            }));
                        }
                        for tc in &tool_calls {
                            let args: Value = tc.arguments.as_deref()
                                .and_then(|s| serde_json::from_str(s).ok())
                                .unwrap_or(Value::Null);

                            let (status, result, error) = match tc.status.as_str() {
                                "done"    => ("done",    tc.result.clone(), None),
                                // 'pending' means waiting for explicit user input (approval or
                                // clarification) — show the approval form with no error message.
                                "pending" => ("pending", None,              None),
                                // 'running' means the tool was mid-execution when the session was
                                // interrupted — shown as "Interrupted" so the frontend can auto-resume.
                                "running" => ("error",   None,              Some("Interrupted.".to_string())),
                                // 'failed' means the tool completed with a genuine error — show
                                // the actual error message, NOT "Interrupted" (that would trigger
                                // a spurious auto-resume on page refresh).
                                _         => ("error",   None,              tc.result.clone()),
                            };

                            let label_short = tools.describe_call(&tc.name, &args, ToolDescriptionLength::Short);
                            let label_full  = tools.describe_call(&tc.name, &args, ToolDescriptionLength::Full);
                            items.push(json!({
                                "kind":         "tool",
                                "tool_call_id": tc.id,
                                "name":         tc.name,
                                "label_short":  label_short,
                                "label_full":   label_full,
                                "arguments":    args,
                                "status":       status,
                                "result":       result,
                                "error":        error,
                            }));

                            if let Some(sub_stack) = subagent_map.get(&tc.id) {
                                items.push(json!({
                                    "kind":     "agent",
                                    "stack_id": sub_stack.id,
                                    "agent_id": sub_stack.agent_id,
                                    "depth":    sub_stack.depth,
                                    "done":     true,
                                }));
                                build_items(db, tools, sub_stack, subagent_map, items).await?;
                                items.push(json!({
                                    "kind":     "agent_end",
                                    "agent_id": sub_stack.agent_id,
                                    "depth":    sub_stack.depth,
                                }));
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    })
}
