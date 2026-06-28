//! Per-source input inbox for ChatHub.
//!
//! Each interactive source (telegram, web, mobile…) gets one `SourceInbox` and a
//! single consumer task (spawned lazily in `ChatHub`). Messages that arrive while
//! a turn is running accumulate here and are **coalesced** into one follow-up turn
//! instead of triggering N separate turns. A single consumer per source also makes
//! delivery strictly FIFO, removing the ordering race of the old detached-spawn
//! dispatch.
//!
//! Serialization of the turns themselves still lives in
//! `ChatSessionHandler.processing`; this inbox sits in front of it, adding ordering
//! and coalescing.

use std::collections::VecDeque;
use std::sync::atomic::AtomicU64;

use tokio::sync::{Mutex, Notify};

use core_api::chat_hub::SendMessageOptions;
use core_api::message_meta::MessageMetadata;

/// One queued user message awaiting dispatch.
pub(super) struct QueuedMessage {
    pub prompt: String,
    pub opts:   SendMessageOptions,
}

/// Pending queue + wake signal for a single source.
#[derive(Default)]
pub(super) struct SourceInbox {
    pub pending: Mutex<VecDeque<QueuedMessage>>,
    pub notify:  Notify,
    /// Bumped by `ChatHub::cancel` (after clearing `pending`) so the consumer can
    /// drop a unit it drained microseconds before a `/stop`.
    pub cancel_epoch: AtomicU64,
}

/// Pops the next dispatch unit from `pending`, coalescing the leading run of
/// non-synthetic messages into a single prompt joined by blank lines.
///
/// - Empty queue → `None`.
/// - A synthetic message (notification/TIC) is always returned alone, never merged
///   with user text.
/// - Otherwise, consecutive non-synthetic messages are concatenated with `"\n\n"`
///   and the most recent message's `opts` is used (interface tools / system context
///   are identical across a single source's batch).
pub(super) fn build_unit(
    pending: &mut VecDeque<QueuedMessage>,
) -> Option<(String, SendMessageOptions)> {
    let first = pending.pop_front()?;
    if first.opts.is_synthetic {
        return Some((first.prompt, first.opts));
    }

    let mut prompts = vec![first.prompt];
    let mut opts = first.opts;
    // Accumulate attachments across the whole coalesced run: `opts` keeps the most
    // recent message's options, so without this union the files attached to an
    // earlier message (sent moments before the latest) would be silently dropped.
    let mut attachments = opts.metadata.take().map(|m| m.attachments).unwrap_or_default();
    while pending.front().is_some_and(|m| !m.opts.is_synthetic) {
        let m = pending.pop_front().unwrap();
        prompts.push(m.prompt);
        opts = m.opts; // keep the most recent opts
        if let Some(meta) = opts.metadata.take() {
            attachments.extend(meta.attachments);
        }
    }
    if !attachments.is_empty() {
        opts.metadata = Some(MessageMetadata { attachments });
    }
    Some((prompts.join("\n\n"), opts))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(prompt: &str, synthetic: bool) -> QueuedMessage {
        QueuedMessage {
            prompt: prompt.to_string(),
            opts: SendMessageOptions { is_synthetic: synthetic, ..Default::default() },
        }
    }

    #[test]
    fn empty_queue_yields_none() {
        let mut q = VecDeque::new();
        assert!(build_unit(&mut q).is_none());
    }

    #[test]
    fn coalesces_consecutive_user_messages_with_blank_lines() {
        let mut q = VecDeque::from(vec![msg("hello", false), msg("also this", false)]);
        let (prompt, _) = build_unit(&mut q).unwrap();
        assert_eq!(prompt, "hello\n\nalso this");
        assert!(q.is_empty());
    }

    #[test]
    fn synthetic_message_is_returned_alone() {
        let mut q = VecDeque::from(vec![msg("notification", true), msg("user text", false)]);
        let (prompt, opts) = build_unit(&mut q).unwrap();
        assert_eq!(prompt, "notification");
        assert!(opts.is_synthetic);
        // The user message remains for the next unit.
        assert_eq!(q.len(), 1);
    }

    fn msg_with_attachment(prompt: &str, path: &str) -> QueuedMessage {
        use core_api::message_meta::{Attachment, MessageMetadata};
        QueuedMessage {
            prompt: prompt.to_string(),
            opts: SendMessageOptions {
                metadata: Some(MessageMetadata {
                    attachments: vec![Attachment {
                        path: path.to_string(),
                        name: path.to_string(),
                        mimetype: None,
                        filesize: None,
                    }],
                }),
                ..Default::default()
            },
        }
    }

    #[test]
    fn coalescing_unions_attachments_across_user_messages() {
        let mut q = VecDeque::from(vec![
            msg_with_attachment("first", "a.pdf"),
            msg_with_attachment("second", "b.pdf"),
        ]);
        let (prompt, opts) = build_unit(&mut q).unwrap();
        assert_eq!(prompt, "first\n\nsecond");
        let paths: Vec<_> = opts.metadata.unwrap().attachments.into_iter().map(|a| a.path).collect();
        assert_eq!(paths, vec!["a.pdf", "b.pdf"]);
    }

    #[test]
    fn user_run_stops_at_a_synthetic_boundary() {
        let mut q = VecDeque::from(vec![
            msg("a", false),
            msg("b", false),
            msg("notification", true),
        ]);
        let (prompt, _) = build_unit(&mut q).unwrap();
        assert_eq!(prompt, "a\n\nb");
        assert_eq!(q.len(), 1); // the synthetic message is left for the next unit
    }
}
