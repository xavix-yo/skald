//! Serde types for the control frames (relay-protocol.md). Every frame is a
//! WebSocket text frame (JSON) with a `"type"` field. Unknown fields are
//! ignored (forward-compat); unknown types become `Incoming::Unknown`.

use serde::{Deserialize, Serialize};

/// Frames received by the relay (client/agent → relay).
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Incoming {
    Auth(AuthFrame),
    Message(MessageIn),
    Authorize(AuthorizeFrame),
    PairingStart(PairingStartFrame),
    PairingStop,
    Ping,
    Pong,
    /// Any unrecognized `type`: ignored (forward-compatibility).
    #[serde(other)]
    Unknown,
}

/// `auth`: fields vary by `role`; here they are all optional and validated at
/// runtime depending on the role (relay-protocol.md §4.1/4.2/4.3).
#[derive(Debug, Deserialize)]
pub struct AuthFrame {
    pub role: String,
    pub signature: String,
    #[serde(default)]
    pub agent_ed25519_pub: Option<String>,
    #[serde(default)]
    pub namespace_id: Option<String>,
    #[serde(default)]
    pub pairing_token: Option<String>,
    #[serde(default)]
    pub client_ed25519_pub: Option<String>,
    #[serde(default)]
    pub client_x25519_pub: Option<String>,
    #[serde(default)]
    pub device_token: Option<String>,
    #[serde(default)]
    pub platform: Option<String>,
}

/// Inbound `message`. The relay ignores any `from` supplied by the sender: the
/// authoritative `from` is the connection's authenticated pubkey.
#[derive(Debug, Deserialize)]
pub struct MessageIn {
    pub to: String,
    pub nonce: String,
    pub ciphertext: String,
}

/// `authorize`: the list **replaces** the previous one (relay-protocol.md §6).
#[derive(Debug, Deserialize)]
pub struct AuthorizeFrame {
    #[serde(default)]
    pub clients: Vec<String>,
}

/// `pairing_start` (relay-protocol.md §7.1).
#[derive(Debug, Deserialize)]
pub struct PairingStartFrame {
    pub pairing_token: String,
    #[serde(default)]
    pub ttl: Option<u64>,
}

/// Frames sent by the relay (relay → client/agent).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Outgoing {
    Challenge {
        nonce: String,
    },
    AuthOk {
        role: String,
        namespace_id: String,
    },
    AuthError {
        code: String,
        message: String,
    },
    Error {
        code: String,
        message: String,
    },
    Message {
        from: String,
        nonce: String,
        ciphertext: String,
        timestamp: String,
    },
    ClientPaired {
        client_ed25519_pub: String,
        client_x25519_pub: String,
        platform: String,
    },
    AuthorizeOk {
        authorized: i64,
    },
    PairingReady {
        ttl: u64,
    },
    PairingStopOk,
    Ping,
    Pong,
}

impl Outgoing {
    pub fn error(code: &str, message: &str) -> Self {
        Outgoing::Error {
            code: code.into(),
            message: message.into(),
        }
    }
    pub fn auth_error(code: &str, message: &str) -> Self {
        Outgoing::AuthError {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// Normative error codes (relay-protocol.md §11).
pub mod codes {
    pub const CHALLENGE_TIMEOUT: &str = "challenge_timeout";
    pub const INVALID_SIGNATURE: &str = "invalid_signature";
    pub const UNAUTHORIZED: &str = "unauthorized";
    pub const NOT_FOUND: &str = "not_found";
    pub const PAIRING_CLOSED: &str = "pairing_closed";
    pub const RATE_LIMITED: &str = "rate_limited";
    pub const PAYLOAD_TOO_LARGE: &str = "payload_too_large";
    pub const QUEUE_FULL: &str = "queue_full";
    pub const BAD_REQUEST: &str = "bad_request";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_agent_auth() {
        let j = r#"{"type":"auth","role":"agent","agent_ed25519_pub":"ab","signature":"cd","extra":"ignored"}"#;
        match serde_json::from_str::<Incoming>(j).unwrap() {
            Incoming::Auth(a) => {
                assert_eq!(a.role, "agent");
                assert_eq!(a.agent_ed25519_pub.as_deref(), Some("ab"));
            }
            other => panic!("expected auth, got {other:?}"),
        }
    }

    #[test]
    fn unknown_type_is_ignored() {
        let j = r#"{"type":"made_up_future_frame","x":1}"#;
        assert!(matches!(
            serde_json::from_str::<Incoming>(j).unwrap(),
            Incoming::Unknown
        ));
    }

    #[test]
    fn serializes_outgoing_tagged() {
        let s = serde_json::to_string(&Outgoing::AuthOk {
            role: "agent".into(),
            namespace_id: "ff".into(),
        })
        .unwrap();
        assert_eq!(
            s,
            r#"{"type":"auth_ok","role":"agent","namespace_id":"ff"}"#
        );

        let s = serde_json::to_string(&Outgoing::PairingStopOk).unwrap();
        assert_eq!(s, r#"{"type":"pairing_stop_ok"}"#);
    }
}
