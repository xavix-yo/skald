use std::sync::Arc;

use teloxide::prelude::*;
use teloxide::types::{ChatAction, ParseMode};
use tracing::{error, info};

use core_api::chat_hub::{ChatHubApi as _, SendMessageOptions};
use core_api::location::GpsCoord;

use super::TELEGRAM_FORMAT_CONTEXT;
use super::TgShared;
use super::attachments::TelegramAttachment;
use super::auth::{handle_pairing, load_wl};

// ── Available commands help text (shared by /help and unknown-command replies) ──
const HELP_TEXT: &str = "<b>Available commands</b>\n\n\
     /clear — start a new conversation\n\
     /new — alias for /clear\n\
     /stop — interrupt the agent mid-turn\n\
     /context — show last turn's token usage\n\
     /cost — show total spend for this session (USD)\n\
     /compact — force context compaction\n\
     /resetmcp — remove all activated MCP tools from the session\n\
     /sethome — receive agent notifications here\n\
     /help — this message";

// ── Incoming message classification ───────────────────────────────────────────
//
// To add a new media type: add a variant to IncomingEvent, handle it in
// classify_message, then dispatch it in message_handler.

pub(crate) enum IncomingEvent {
    Text(String),
    Command { name: String, args: Vec<String> },
    Voice { file_id: String },
    Attachment(TelegramAttachment),
}

pub(crate) fn classify_message(msg: &Message) -> Option<IncomingEvent> {
    if let Some(voice) = msg.voice() {
        return Some(IncomingEvent::Voice { file_id: voice.file.id.to_string() });
    }

    if let Some(doc) = msg.document() {
        return Some(IncomingEvent::Attachment(TelegramAttachment::Document {
            file_id:   doc.file.id.to_string(),
            file_name: doc.file_name.clone().unwrap_or_else(|| "attachment".to_string()),
            mime_type: doc.mime_type.as_ref().map(|m| m.to_string()),
            caption:   msg.caption().map(str::to_string),
        }));
    }

    if let Some(photos) = msg.photo() {
        if let Some(largest) = photos.last() {
            return Some(IncomingEvent::Attachment(TelegramAttachment::Photo {
                file_id: largest.file.id.to_string(),
                caption: msg.caption().map(str::to_string),
            }));
        }
    }

    if let Some(loc) = msg.location() {
        return Some(IncomingEvent::Attachment(TelegramAttachment::Location {
            latitude:  loc.latitude,
            longitude: loc.longitude,
            accuracy:  loc.horizontal_accuracy,
            is_live:   loc.live_period.is_some(),
        }));
    }

    let text = msg.text()?;

    if let Some(entity) = msg.parse_entities().and_then(|mut v| {
        v.retain(|e| matches!(e.kind(), teloxide::types::MessageEntityKind::BotCommand));
        v.into_iter().next()
    }) {
        let full = entity.text().trim_start_matches('/');
        let mut parts = full.splitn(2, ' ');
        let name = parts.next().unwrap_or("").to_ascii_lowercase();
        let name = name.split('@').next().unwrap_or(&name).to_string();
        let rest = parts.next().unwrap_or("").trim().to_string();
        let args: Vec<String> = if rest.is_empty() {
            vec![]
        } else {
            rest.split_whitespace().map(str::to_string).collect()
        };
        return Some(IncomingEvent::Command { name, args });
    }

    Some(IncomingEvent::Text(text.to_string()))
}

// ── Message handler ───────────────────────────────────────────────────────────

pub(crate) async fn message_handler(
    bot:    Bot,
    msg:    Message,
    shared: Arc<TgShared>,
) -> ResponseResult<()> {
    let chat_id = msg.chat.id;

    // Whitelist check — re-read the file on every message so agent edits are
    // picked up without a plugin restart.
    let wl = load_wl(&shared.secrets_dir).await;
    if !wl.whitelist.contains(&chat_id.0) {
        handle_pairing(&bot, chat_id, &shared).await;
        return Ok(());
    }

    // Track the last active chat_id so the persistent forwarder knows
    // where to send background notifications.
    *shared.home_chat_id.lock().await = Some(chat_id);

    let Some(incoming) = classify_message(&msg) else {
        bot.send_message(chat_id, "Unsupported message format.").await.ok();
        return Ok(());
    };

    match incoming {
        IncomingEvent::Command { ref name, .. } if name == "clear" || name == "new" => {
            handle_clear(&bot, chat_id, &shared).await;
        }
        IncomingEvent::Command { ref name, .. } if name == "sethome" => {
            match shared.chat_hub.set_home("telegram").await {
                Ok(_) => {
                    info!("telegram: set as home source");
                    bot.send_message(chat_id, "🏠 Telegram set as <b>home</b>. Agent notifications will be delivered here.")
                        .parse_mode(ParseMode::Html)
                        .await
                        .ok();
                }
                Err(e) => {
                    bot.send_message(chat_id, format!("⚠️ Error: {e}")).await.ok();
                }
            }
        }
        IncomingEvent::Command { ref name, .. } if name == "help" => {
            bot.send_message(chat_id, HELP_TEXT)
                .parse_mode(ParseMode::Html)
                .await
                .ok();
        }
        IncomingEvent::Command { ref name, .. } if name == "stop" => {
            handle_stop(&bot, chat_id, &shared).await;
        }
        IncomingEvent::Command { ref name, .. } if name == "context" => {
            handle_context(&bot, chat_id, &shared).await;
        }
        IncomingEvent::Command { ref name, .. } if name == "cost" => {
            handle_cost(&bot, chat_id, &shared).await;
        }
        IncomingEvent::Command { ref name, .. } if name == "compact" => {
            handle_compact(&bot, chat_id, &shared).await;
        }
        IncomingEvent::Command { ref name, .. } if name == "resetmcp" => {
            handle_reset_mcp(&bot, chat_id, &shared).await;
        }
        // Any other command is unknown — never forward a `/...` prompt to the LLM.
        IncomingEvent::Command { ref name, .. } => {
            bot.send_message(
                chat_id,
                format!("Unknown command: /{name}\n\n{HELP_TEXT}"),
            )
            .parse_mode(ParseMode::Html)
            .await
            .ok();
        }
        IncomingEvent::Voice { file_id } => {
            handle_voice(&bot, chat_id, file_id, &shared).await;
        }
        IncomingEvent::Attachment(attachment) => {
            handle_attachment(bot, chat_id, attachment, shared).await;
        }
        _ => {
            let text = match &incoming {
                IncomingEvent::Text(t) => t.clone(),
                IncomingEvent::Command { .. }
                | IncomingEvent::Voice { .. }
                | IncomingEvent::Attachment(_) => unreachable!(),
            };

            // If a clarification question is pending, treat any text as the answer.
            {
                let mut pq = shared.pending_question.lock().await;
                if let Some(pq_inner) = pq.take() {
                    let request_id = pq_inner.request_id;
                    let question_msg_id = pq_inner.message_id;
                    drop(pq);
                    shared.chat_hub.resolve_question("telegram", request_id, text.clone()).await;
                    tracing::info!(request_id, %text, "telegram: clarification answered via text");
                    bot.edit_message_reply_markup(chat_id, question_msg_id)
                        .reply_markup(teloxide::types::InlineKeyboardMarkup::new(vec![vec![
                            teloxide::types::InlineKeyboardButton::callback(
                                format!("✅ {}", super::helpers::escape_html(&text)),
                                "noop",
                            ),
                        ]]))
                        .await
                        .ok();
                    return Ok(());
                }
            }

            handle_llm_message(bot, chat_id, text, shared).await;
        }
    }

    Ok(())
}

// ── /clear command ────────────────────────────────────────────────────────────

async fn handle_clear(bot: &Bot, chat_id: ChatId, shared: &Arc<TgShared>) {
    match shared.chat_hub.clear("telegram").await {
        Ok(_) => {
            info!("telegram: session cleared via /clear");
            bot.send_message(chat_id, "🆕 New conversation started.").await.ok();
        }
        Err(e) => {
            error!(error = %e, "telegram: failed to clear session");
            bot.send_message(chat_id, format!("⚠️ Error: {e}")).await.ok();
        }
    }
}

// ── /context command ──────────────────────────────────────────────────────────

async fn handle_context(bot: &Bot, chat_id: ChatId, shared: &Arc<TgShared>) {
    match shared.chat_hub.context_info("telegram").await {
        Ok((input, output)) => {
            let input_str = input.map_or("?".to_string(), |t| t.to_string());
            let output_str = output.map_or("?".to_string(), |t| t.to_string());
            bot.send_message(
                chat_id,
                format!("<i>↑{input_str} tok · ↓{output_str} tok</i>"),
            )
            .parse_mode(ParseMode::Html)
            .await
            .ok();
        }
        Err(e) => {
            bot.send_message(chat_id, format!("⚠️ Error: {e}")).await.ok();
        }
    }
}

// ── /cost command ─────────────────────────────────────────────────────────────

async fn handle_cost(bot: &Bot, chat_id: ChatId, shared: &Arc<TgShared>) {
    match shared.chat_hub.cost_info("telegram").await {
        Ok(Some(c)) => {
            bot.send_message(chat_id, format!("💰 Session cost: ${c:.4}")).await.ok();
        }
        Ok(None) => {
            bot.send_message(chat_id, "💰 No cost recorded for this session.").await.ok();
        }
        Err(e) => {
            bot.send_message(chat_id, format!("⚠️ Error: {e}")).await.ok();
        }
    }
}

// ── /compact command ──────────────────────────────────────────────────────────

async fn handle_compact(bot: &Bot, chat_id: ChatId, shared: &Arc<TgShared>) {
    match shared.chat_hub.force_compact("telegram").await {
        Ok(true) => {
            info!("telegram: manual compaction succeeded");
            bot.send_message(chat_id, "✅ Context compacted.").await.ok();
        }
        Ok(false) => {
            bot.send_message(chat_id, "⏩ Compaction skipped (no messages to summarise or compaction disabled).").await.ok();
        }
        Err(e) => {
            error!(error = %e, "telegram: manual compaction failed");
            bot.send_message(chat_id, format!("⚠️ Compaction failed: {e}")).await.ok();
        }
    }
}

// ── /resetmcp command ─────────────────────────────────────────────────────────

async fn handle_reset_mcp(bot: &Bot, chat_id: ChatId, shared: &Arc<TgShared>) {
    match shared.chat_hub.reset_mcp("telegram").await {
        Ok(()) => {
            info!("telegram: MCP grants reset via /resetmcp");
            bot.send_message(chat_id, "✅ MCP tools removed from the session.").await.ok();
        }
        Err(e) => {
            error!(error = %e, "telegram: /resetmcp failed");
            bot.send_message(chat_id, format!("⚠️ Error: {e}")).await.ok();
        }
    }
}

// ── /stop command ────────────────────────────────────────────────────────────

async fn handle_stop(bot: &Bot, chat_id: ChatId, shared: &Arc<TgShared>) {
    shared.chat_hub.cancel("telegram").await;
    info!("telegram: agent cancelled via /stop");
    bot.send_message(chat_id, "⏹ Agent stopped.").await.ok();
}

// ── LLM dispatch ─────────────────────────────────────────────────────────────

async fn handle_llm_message(
    bot:     Bot,
    chat_id: ChatId,
    text:    String,
    shared:  Arc<TgShared>,
) {
    bot.send_chat_action(chat_id, ChatAction::Typing).await.ok();

    // The persistent_forwarder (spawned once in start()) is always subscribed
    // to the "telegram" broadcast channel and will pick up all events for this
    // turn — including Done → send to Telegram.  No per-message subscription needed.
    let opts = SendMessageOptions {
        extra_system_context: Some(TELEGRAM_FORMAT_CONTEXT.to_string()),
        tail_reminder:        Some(super::TELEGRAM_FORMAT_REMINDER.to_string()),
        interface_tools:      super::tools::interface_tools(bot, chat_id, &*shared.tts).await,
        ..Default::default()
    };

    // send_message only enqueues — the turn runs on ChatHub's per-source consumer —
    // so awaiting inline keeps this message handler responsive.
    if let Err(e) = shared.chat_hub.send_message("telegram", &text, opts).await {
        error!(error = %e, "telegram: enqueue error");
    }
}

// ── Voice message → transcribe → LLM ─────────────────────────────────────────

async fn handle_voice(
    bot:     &Bot,
    chat_id: ChatId,
    file_id: String,
    shared:  &Arc<TgShared>,
) {
    use teloxide::net::Download;

    let transcriber = match shared.transcriber().await {
        Some(t) => t,
        None => {
            bot.send_message(chat_id, "⚠️ Transcription not available (no transcription provider configured).").await.ok();
            return;
        }
    };

    let file = match bot.get_file(teloxide::types::FileId(file_id)).await {
        Ok(f)  => f,
        Err(e) => {
            error!(error = %e, "telegram: get_file failed");
            bot.send_message(chat_id, "⚠️ Could not download audio file.").await.ok();
            return;
        }
    };

    let mut audio_bytes = Vec::new();
    if let Err(e) = bot.download_file(&file.path, &mut audio_bytes).await {
        error!(error = %e, "telegram: download_file failed");
        bot.send_message(chat_id, "⚠️ Audio download failed.").await.ok();
        return;
    }

    bot.send_chat_action(chat_id, ChatAction::Typing).await.ok();

    let text = match transcriber.transcribe(audio_bytes, "ogg").await {
        Ok(t)  => t,
        Err(e) => {
            error!(error = %e, "telegram: transcription failed");
            bot.send_message(chat_id, format!("⚠️ Transcription failed: {e}")).await.ok();
            return;
        }
    };

    info!(chat_id = chat_id.0, "telegram: voice transcribed, forwarding to LLM");
    let message = format!(
        "[TELEGRAM SYSTEM INFO]\n\
         The user sent a voice message. The following is the audio transcript:\n\n\
         {text}"
    );
    handle_llm_message(bot.clone(), chat_id, message, Arc::clone(shared)).await;
}

// ── Edited message (live location updates) ────────────────────────────────────

pub(crate) async fn edited_message_handler(
    msg:    Message,
    shared: Arc<TgShared>,
) -> ResponseResult<()> {
    if let Some(loc) = msg.location() {
        let coord = GpsCoord { latitude: loc.latitude, longitude: loc.longitude };
        shared.location.update("telegram", coord, loc.horizontal_accuracy, true);
    }
    Ok(())
}

// ── File / media attachment ───────────────────────────────────────────────────

async fn handle_attachment(
    bot:        Bot,
    chat_id:    ChatId,
    attachment: TelegramAttachment,
    shared:     Arc<TgShared>,
) {
    // Update LocationManager immediately, before any LLM dispatch.
    if let TelegramAttachment::Location { latitude, longitude, accuracy, is_live } = &attachment {
        let coord = GpsCoord { latitude: *latitude, longitude: *longitude };
        shared.location.update("telegram", coord, *accuracy, *is_live);
    }

    bot.send_chat_action(chat_id, ChatAction::UploadDocument).await.ok();

    let saved_path = match attachment.download_and_save(&bot, &shared.uploads_dir, chat_id.0).await {
        Ok(p)  => p,
        Err(e) => {
            error!(error = %e, "telegram: failed to save attachment");
            bot.send_message(chat_id, "⚠️ Could not save the attachment.").await.ok();
            return;
        }
    };

    if let Some(ref path) = saved_path {
        info!(chat_id = chat_id.0, path = %path.display(), "telegram: attachment saved, forwarding to LLM");
    }

    let message = attachment.system_info_message(saved_path.as_deref());
    handle_llm_message(bot, chat_id, message, shared).await;
}
