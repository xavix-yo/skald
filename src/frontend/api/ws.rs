use std::sync::Arc;

use axum::{
    extract::{
        Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::IntoResponse,
};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::core::chat_hub::SendMessageOptions;
use crate::core::events::{ClientMessage, GlobalEvent, ServerEvent};
use crate::core::skald::Skald;

#[derive(Deserialize)]
pub struct WsParams {
    source: Option<String>,
}

const WEB_FORMAT_CONTEXT: &str = "\
You are responding in a web chat interface. Use standard Markdown formatting for all responses.\n\
\n\
IMAGES: If image generation is active, you can display images to the user using standard Markdown \
image syntax with the URL. Always set a max-width style to avoid the image taking up the full screen width, \
e.g. <img src=\"URL\" style=\"max-width:480px\">. \
The URL returned by image_generate already points to the correct endpoint — use it as-is. \
Do NOT append \".png\" or any extension to the URL.";

const HELP_TEXT: &str = "\
**Available commands**\n\n\
**/clear** — start a new conversation\n\
**/new** — alias for /clear\n\
**/context** — show last turn's token usage\n\
**/cost** — show total spend for this session (USD)\n\
**/compact** — force context compaction\n\
**/resetmcp** — remove all activated MCP tools from the session\n\
**/sethome** — set web as the destination for agent notifications\n\
**/help** — this message";

// ── Upgrade ───────────────────────────────────────────────────────────────────

pub async fn handler(
    ws:            WebSocketUpgrade,
    Query(params): Query<WsParams>,
    State(skald):  State<Arc<Skald>>,
) -> impl IntoResponse {
    let source = params.source.unwrap_or_else(|| "web".to_string());
    ws.on_upgrade(move |socket| handle_socket(socket, skald, source))
}

// ── Socket loop ───────────────────────────────────────────────────────────────

async fn handle_socket(mut socket: WebSocket, skald: Arc<Skald>, source: String) {
    let session_handler = match skald.chat_hub.session_handler(&source).await {
        Ok(h)  => h,
        Err(e) => {
            let _ = socket.send(to_msg(&ServerEvent::Error { message: e.to_string() })).await;
            return;
        }
    };

    info!(source, "WebSocket connected");

    let mut rx = skald.chat_hub.events(&source);

    // Tell this (possibly reloaded) client whether a turn is already running for
    // its session, so it can restore the STOP button. Sent after subscribing to
    // `rx`, so a turn that finishes right after still delivers its Done via `rx`.
    let _ = socket.send(to_msg(&ServerEvent::TurnRunning {
        running: session_handler.is_processing(),
    })).await;

    loop {
        tokio::select! {
            // ── Inbound: message from the browser ────────────────────────────
            msg = socket.recv() => {
                let text = match msg {
                    Some(Ok(Message::Text(t)))  => t,
                    Some(Ok(Message::Close(_))) | None => return,
                    _ => continue,
                };

                // ── resume ────────────────────────────────────────────────────
                if is_resume_msg(&text) {
                    info!("web WS: resume requested");
                    let hub = Arc::clone(&skald.chat_hub);
                    let src = source.clone();
                    tokio::spawn(async move {
                        if let Err(e) = hub.resume(&src).await {
                            tracing::error!(error = %e, source = %src, "resume failed");
                        }
                    });
                    continue;
                }

                // ── cancel / approval / question (mid-turn controls) ──────────
                if is_cancel_msg(&text) {
                    info!("web WS: cancel requested");
                    session_handler.cancel();
                    session_handler.cancel_pending_approvals().await;
                    session_handler.cancel_pending_questions().await;
                    continue;
                }
                if handle_approval_msg(&text, &skald.chat_hub).await { continue; }
                if handle_question_answer_msg(&text, &session_handler).await { continue; }
                if handle_data_msg(&text, &skald) { continue; }

                // ── /sethome ──────────────────────────────────────────────────
                let client_msg: ClientMessage = match serde_json::from_str(&text) {
                    Ok(m)  => m,
                    Err(e) => {
                        let _ = socket.send(to_msg(&ServerEvent::Error {
                            message: format!("invalid message: {e}"),
                        })).await;
                        continue;
                    }
                };

                let cmd = client_msg.content.trim();

                if cmd == "/sethome" {
                    let msg = match skald.chat_hub.set_home(&source).await {
                        Ok(_)  => "🏠 Web impostato come **home**. Le notifiche degli agenti arriveranno qui.".to_string(),
                        Err(e) => format!("⚠️ Errore: {e}"),
                    };
                    let _ = socket.send(to_msg(&ServerEvent::Done {
                        message_id:    0,
                        stack_id:      0,
                        content:       msg,
                        input_tokens:  None,
                        output_tokens: None,
                    })).await;
                    continue;
                }

                if cmd == "/help" {
                    let _ = socket.send(to_msg(&ServerEvent::Done {
                        message_id:    0,
                        stack_id:      0,
                        content:       HELP_TEXT.to_string(),
                        input_tokens:  None,
                        output_tokens: None,
                    })).await;
                    continue;
                }

                if cmd == "/context" {
                    match skald.chat_hub.context_info(&source).await {
                        Ok((input, output)) => {
                            let input_str = input.map_or("?".to_string(), |t| t.to_string());
                            let output_str = output.map_or("?".to_string(), |t| t.to_string());
                            let _ = socket.send(to_msg(&ServerEvent::Done {
                                message_id:    0,
                                stack_id:      0,
                                content:       format!("↑{input_str} tok · ↓{output_str} tok"),
                                input_tokens:  None,
                                output_tokens: None,
                            })).await;
                        }
                        Err(e) => {
                            let _ = socket.send(to_msg(&ServerEvent::Error { message: e.to_string() })).await;
                        }
                    }
                    continue;
                }

                if cmd == "/cost" {
                    match skald.chat_hub.cost_info(&source).await {
                        Ok(Some(c)) => {
                            let _ = socket.send(to_msg(&ServerEvent::Done {
                                message_id:    0,
                                stack_id:      0,
                                content:       format!("💰 Costo sessione: ${c:.4}"),
                                input_tokens:  None,
                                output_tokens: None,
                            })).await;
                        }
                        Ok(None) => {
                            let _ = socket.send(to_msg(&ServerEvent::Done {
                                message_id:    0,
                                stack_id:      0,
                                content:       "💰 Nessun costo registrato per questa sessione.".to_string(),
                                input_tokens:  None,
                                output_tokens: None,
                            })).await;
                        }
                        Err(e) => {
                            let _ = socket.send(to_msg(&ServerEvent::Error { message: e.to_string() })).await;
                        }
                    }
                    continue;
                }

                if cmd == "/compact" {
                    match skald.chat_hub.force_compact(&source).await {
                        Ok(true) => {
                            let _ = socket.send(to_msg(&ServerEvent::Done {
                                message_id:    0,
                                stack_id:      0,
                                content:       "✅ Contesto compattato.".to_string(),
                                input_tokens:  None,
                                output_tokens: None,
                            })).await;
                        }
                        Ok(false) => {
                            let _ = socket.send(to_msg(&ServerEvent::Done {
                                message_id:    0,
                                stack_id:      0,
                                content:       "⏩ Compattazione saltata (nessun messaggio da riassumere o compattazione disabilitata).".to_string(),
                                input_tokens:  None,
                                output_tokens: None,
                            })).await;
                        }
                        Err(e) => {
                            let _ = socket.send(to_msg(&ServerEvent::Error { message: e.to_string() })).await;
                        }
                    }
                    continue;
                }

                if cmd == "/resetmcp" {
                    match skald.chat_hub.reset_mcp(&source).await {
                        Ok(()) => {
                            let _ = socket.send(to_msg(&ServerEvent::Done {
                                message_id:    0,
                                stack_id:      0,
                                content:       "✅ MCP tools removed from the session.".to_string(),
                                input_tokens:  None,
                                output_tokens: None,
                            })).await;
                        }
                        Err(e) => {
                            let _ = socket.send(to_msg(&ServerEvent::Error { message: e.to_string() })).await;
                        }
                    }
                    continue;
                }

                // ── Unknown command ───────────────────────────────────────────
                // Any other `/...` prompt is an unrecognised command — never
                // forward it to the LLM. Reply with a not-found notice + help.
                if cmd.starts_with('/') {
                    let first = cmd.split_whitespace().next().unwrap_or(cmd);
                    let _ = socket.send(to_msg(&ServerEvent::Done {
                        message_id:    0,
                        stack_id:      0,
                        content:       format!("Unknown command: {first}\n\n{HELP_TEXT}"),
                        input_tokens:  None,
                        output_tokens: None,
                    })).await;
                    continue;
                }

                // ── Regular LLM message ───────────────────────────────────────
                let content = client_msg.content.clone();

                // Broadcast to all clients on the same source so they see the
                // user message in real-time (other tabs, mobile, etc.).
                skald.chat_hub.emit(GlobalEvent {
                    source:     Some(source.clone()),
                    session_id: None,
                    event:      ServerEvent::UserMessage { content: content.clone() },
                });

                let opts = SendMessageOptions {
                    client_name:          client_msg.client.clone(),
                    extra_system_context: Some(WEB_FORMAT_CONTEXT.to_string()),
                    ..Default::default()
                };
                // send_message only enqueues — the turn runs on ChatHub's per-source
                // consumer — so awaiting inline keeps this WS read loop responsive.
                if let Err(e) = skald.chat_hub.send_message(&source, &content, opts).await {
                    tracing::error!(error = %e, source = %source, "send_message enqueue failed");
                }
            }

            // ── Outbound: event from ChatHub → forward to browser ─────────────
            event = rx.recv() => {
                match event {
                    Ok(ge) => {
                        // Forward events for this connection's source.
                        // ApprovalResolved is forwarded regardless of source so the
                        // copilot can react to approvals resolved from other clients.
                        let forward = ge.source.as_deref() == Some(source.as_str())
                            || matches!(ge.event, ServerEvent::ApprovalResolved { .. });
                        if !forward { continue; }
                        debug!(event_type = ge.event.type_name(), "sending event to client");
                        if socket.send(to_msg(&ge.event)).await.is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(skipped = n, "web WS: event stream lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────


fn is_cancel_msg(text: &str) -> bool {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v["type"].as_str().map(|s| s == "cancel"))
        .unwrap_or(false)
}

fn is_resume_msg(text: &str) -> bool {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v["type"].as_str().map(|s| s == "resume"))
        .unwrap_or(false)
}

/// Returns true if the message was an approval/rejection (caller should `continue`).
async fn handle_approval_msg(
    text:      &str,
    chat_hub:  &Arc<crate::core::chat_hub::ChatHub>,
) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return false };
    let Some(request_id) = v["request_id"].as_i64() else { return false };
    match v["type"].as_str() {
        Some("approve_write") | Some("approve_tool") => {
            // Optional bypass: `bypass_secs` present → approve + bypass.
            // Value 0 means indefinite (session); any positive value is seconds.
            if let Some(bypass_secs) = v["bypass_secs"].as_u64() {
                let secs = if bypass_secs == 0 { None } else { Some(bypass_secs) };
                chat_hub.approval.approve_with_bypass(request_id, secs).await;
            } else {
                chat_hub.approve(request_id).await;
            }
        }
        Some("reject_write") | Some("reject_tool") => {
            let note = v["note"].as_str().unwrap_or("").to_string();
            chat_hub.reject(request_id, note).await;
        }
        _ => return false,
    };
    true
}

/// Returns true if the message was a question answer (caller should `continue`).
async fn handle_question_answer_msg(
    text:    &str,
    handler: &Arc<crate::core::session::handler::ChatSessionHandler>,
) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return false };
    if v["type"].as_str() != Some("answer_question") { return false }
    let Some(request_id) = v["request_id"].as_i64() else { return false };
    let answer = v["answer"].as_str().unwrap_or("").to_string();
    handler.resolve_question(request_id, answer).await;
    true
}

/// Returns true if the message was an inbound data push (caller should `continue`).
/// Dispatches `{"type":"data","stream":"...","payload":{...}}` to the appropriate manager.
fn handle_data_msg(text: &str, skald: &Arc<Skald>) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return false };
    if v["type"].as_str() != Some("data") { return false }

    let Ok(msg) = serde_json::from_value::<crate::core::events::InboundDataMessage>(v) else {
        return true;
    };

    match msg.stream.as_str() {
        "location" => {
            let lat = msg.payload["lat"].as_f64();
            let lng = msg.payload["lng"].as_f64();
            let acc = msg.payload["accuracy"].as_f64();
            let live = msg.payload["is_live"].as_bool().unwrap_or(true);
            if let (Some(lat), Some(lng)) = (lat, lng) {
                skald.location_manager.update(
                    "remote",
                    crate::core::location::GpsCoord { latitude: lat, longitude: lng },
                    acc,
                    live,
                );
                tracing::debug!(lat, lng, "location updated from remote client");
            } else {
                tracing::warn!(stream = "location", "missing lat/lng in payload");
            }
        }
        other => tracing::warn!(stream = other, "unknown data stream, ignoring"),
    }

    true
}

fn to_msg(event: &ServerEvent) -> Message {
    Message::Text(event.to_json().into())
}
