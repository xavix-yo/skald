//! Push bridge (APNs / FCM). The **normative**, testable part always lives here:
//! the content-in-push vs wake-only decision (relay.md §5, 3500-byte base64
//! threshold) and the JSON payload construction. The actual send to Apple/Google
//! sits behind the [`Pusher`] trait: the default [`LogPusher`] needs no
//! credentials (it logs a redacted decision), so the relay also boots locally.
//! Live senders sit behind the `push-live` feature.

use crate::limits::CONTENT_PUSH_MAX_B64;
use async_trait::async_trait;
use serde_json::{Value, json};

/// Device platform (relay-protocol.md): selects APNs vs FCM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Ios,
    Android,
}

impl Platform {
    pub fn parse(s: &str) -> Option<Platform> {
        match s {
            "ios" => Some(Platform::Ios),
            "android" => Some(Platform::Android),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Platform::Ios => "ios",
            Platform::Android => "android",
        }
    }
}

/// Result of the push-mode decision (relay.md §5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushKind {
    /// The encrypted blob fits the limit: include it (NSE/app decrypts E2E).
    Content,
    /// Blob too large: wake only; the device opens a WS and drains the queue.
    Wake,
}

/// Everything needed to build a push, already in on-the-wire encoding.
#[derive(Debug, Clone)]
pub struct PushItem {
    pub namespace_id: String,
    pub from_hex: String,
    pub nonce_hex: String,
    pub ciphertext_b64: String,
}

impl PushItem {
    /// Normative selection rule: content-in-push if `len(base64(ciphertext)) <=
    /// CONTENT_PUSH_MAX_B64`, otherwise wake-only.
    pub fn kind(&self) -> PushKind {
        if self.ciphertext_b64.len() <= CONTENT_PUSH_MAX_B64 {
            PushKind::Content
        } else {
            PushKind::Wake
        }
    }

    /// APNs payload (relay.md §5.1/5.2). `aps.alert` is a generic fallback:
    /// **never** sensitive content.
    pub fn apns_payload(&self) -> Value {
        match self.kind() {
            PushKind::Content => json!({
                "aps": {
                    "alert": { "title": "Skald", "body": "Azione richiesta" },
                    "badge": 1,
                    "sound": "default",
                    "mutable-content": 1,
                    "category": "skald_inbox"
                },
                "d": {
                    "ns": self.namespace_id,
                    "from": self.from_hex,
                    "n": self.nonce_hex,
                    "c": self.ciphertext_b64
                }
            }),
            PushKind::Wake => json!({
                "aps": {
                    "alert": { "title": "Skald", "body": "Azione richiesta" },
                    "badge": 1,
                    "sound": "default",
                    "content-available": 1
                },
                "d": { "ns": self.namespace_id, "wake": true }
            }),
        }
    }

    /// FCM HTTP v1 payload (relay.md §5.3): **data-only**, high priority, so the
    /// app always handles decryption even in the background.
    pub fn fcm_payload(&self, device_token: &str) -> Value {
        let mut data = serde_json::Map::new();
        data.insert("ns".into(), json!(self.namespace_id));
        match self.kind() {
            PushKind::Content => {
                data.insert("from".into(), json!(self.from_hex));
                data.insert("n".into(), json!(self.nonce_hex));
                data.insert("c".into(), json!(self.ciphertext_b64));
            }
            PushKind::Wake => {
                data.insert("wake".into(), json!("true"));
            }
        }
        json!({
            "message": {
                "token": device_token,
                "android": { "priority": "high" },
                "data": Value::Object(data)
            }
        })
    }
}

/// Push-send abstraction. Implemented by [`LogPusher`] (default) and, behind the
/// `push-live` feature, by the real APNs/FCM senders.
#[async_trait]
pub trait Pusher: Send + Sync {
    async fn notify(&self, device_token: &str, platform: Platform, item: &PushItem);
}

/// Default pusher: sends nothing, only logs a redacted decision. Lets
/// store-and-forward work locally without Apple/Google credentials.
pub struct LogPusher;

#[async_trait]
impl Pusher for LogPusher {
    async fn notify(&self, device_token: &str, platform: Platform, item: &PushItem) {
        let kind = item.kind();
        // Never log the content: only metadata and truncated identifiers.
        tracing::info!(
            target: "relay::push",
            platform = platform.as_str(),
            kind = ?kind,
            ns = %short(&item.namespace_id),
            token = %short(device_token),
            ct_b64_len = item.ciphertext_b64.len(),
            "would deliver push (no push credentials configured: LogPusher)"
        );
    }
}

/// Truncate an identifier for logging (never log full sensitive strings).
fn short(s: &str) -> String {
    let n = s.len().min(8);
    format!("{}…", &s[..n])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(ct_len: usize) -> PushItem {
        PushItem {
            namespace_id: "a".repeat(64),
            from_hex: "b".repeat(64),
            nonce_hex: "c".repeat(24),
            ciphertext_b64: "Z".repeat(ct_len),
        }
    }

    #[test]
    fn threshold_is_inclusive_3500() {
        assert_eq!(item(CONTENT_PUSH_MAX_B64).kind(), PushKind::Content);
        assert_eq!(item(CONTENT_PUSH_MAX_B64 + 1).kind(), PushKind::Wake);
    }

    #[test]
    fn apns_content_has_blob_and_mutable() {
        let p = item(100).apns_payload();
        assert_eq!(p["aps"]["mutable-content"], 1);
        assert_eq!(p["d"]["c"], "Z".repeat(100));
        assert_eq!(p["d"]["n"], "c".repeat(24));
        assert!(p["d"].get("wake").is_none());
    }

    #[test]
    fn apns_wake_has_no_content() {
        let p = item(CONTENT_PUSH_MAX_B64 + 50).apns_payload();
        assert_eq!(p["aps"]["content-available"], 1);
        assert_eq!(p["d"]["wake"], true);
        assert!(p["d"].get("c").is_none());
    }

    #[test]
    fn fcm_is_data_only_high_priority() {
        let p = item(100).fcm_payload("tok123");
        assert_eq!(p["message"]["token"], "tok123");
        assert_eq!(p["message"]["android"]["priority"], "high");
        assert_eq!(p["message"]["data"]["c"], "Z".repeat(100));
        assert!(p["message"].get("notification").is_none());
    }
}
