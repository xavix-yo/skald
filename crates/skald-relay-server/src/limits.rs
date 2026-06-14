//! Normative quotas, timeouts and thresholds (relay-protocol.md §9, relay.md)
//! plus a fixed-window rate limiter. The values here are the spec's reasonable
//! defaults; the relay may expose them via config later.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Maximum size of a WebSocket frame (64 KiB). Above this → `payload_too_large`.
pub const MAX_FRAME_BYTES: usize = 64 * 1024;
/// Hard cap accepted by the transport before forcibly closing the connection.
pub const TRANSPORT_FRAME_CAP: usize = 128 * 1024;

/// Time allowed to receive `auth` after the `challenge`.
pub const CHALLENGE_TIMEOUT_SECS: u64 = 30;
/// No traffic for this long → close the connection.
pub const IDLE_TIMEOUT_SECS: u64 = 120;
/// Keepalive ping interval.
pub const PING_INTERVAL_SECS: u64 = 30;

/// Max queued messages per recipient; above this → `queue_full`.
pub const QUEUE_MAX_PER_DEST: i64 = 200;
/// Threshold (bytes of base64(ciphertext)) at/below which we do content-in-push.
pub const CONTENT_PUSH_MAX_B64: usize = 3500;

/// TTL for the store-and-forward queue and for idle namespaces.
pub const TTL_DAYS: i64 = 7;

/// Pairing window TTL.
pub const PAIRING_TTL_DEFAULT: u64 = 300;
pub const PAIRING_TTL_MAX: u64 = 600;

/// Anti-flood quotas on the public endpoint.
pub const IP_NEW_CONN_PER_MIN: u32 = 30;
pub const CONN_MSG_PER_MIN: u32 = 60;

/// Thread-safe fixed-window rate limiter, generic over the key.
///
/// One `allow()` per event: returns `false` when the current window's quota is
/// exceeded. The window resets automatically once it elapses.
pub struct FixedWindow<K: Eq + Hash + Clone> {
    window: Duration,
    max: u32,
    map: Mutex<HashMap<K, (Instant, u32)>>,
}

impl<K: Eq + Hash + Clone> FixedWindow<K> {
    pub fn new(window: Duration, max: u32) -> Self {
        Self {
            window,
            max,
            map: Mutex::new(HashMap::new()),
        }
    }

    /// Record an event for `key`. `true` = allowed, `false` = quota exceeded.
    pub fn allow(&self, key: &K) -> bool {
        let mut map = self.map.lock().unwrap();
        let now = Instant::now();
        let entry = map.entry(key.clone()).or_insert((now, 0));
        if now.duration_since(entry.0) >= self.window {
            *entry = (now, 0);
        }
        entry.1 += 1;
        entry.1 <= self.max
    }

    /// Opportunistic pruning of expired windows (called by the GC task).
    pub fn prune(&self) {
        let mut map = self.map.lock().unwrap();
        let now = Instant::now();
        map.retain(|_, (start, _)| now.duration_since(*start) < self.window);
    }
}

/// Per-connection (non-shared) rate counter: messages per minute.
pub struct ConnRate {
    window_start: Instant,
    count: u32,
}

impl ConnRate {
    pub fn new() -> Self {
        Self {
            window_start: Instant::now(),
            count: 0,
        }
    }

    /// `true` if under quota, `false` if the connection exceeded `CONN_MSG_PER_MIN`.
    pub fn allow_message(&mut self) -> bool {
        let now = Instant::now();
        if now.duration_since(self.window_start) >= Duration::from_secs(60) {
            self.window_start = now;
            self.count = 0;
        }
        self.count += 1;
        self.count <= CONN_MSG_PER_MIN
    }
}

impl Default for ConnRate {
    fn default() -> Self {
        Self::new()
    }
}
