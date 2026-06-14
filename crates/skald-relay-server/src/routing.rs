//! In-memory registry of live connections (relay.md §4). Maps each namespace to
//! its single agent connection and its set of client connections. Used to
//! forward messages live; when the recipient is absent the caller falls back to
//! store-and-forward + push.
//!
//! Concurrency: a plain `std::sync::Mutex` guards the map. We never hold the
//! lock across an `.await`: lookups clone the cheap `mpsc::Sender` and release
//! the lock before sending. Stale-connection eviction uses a per-connection
//! `CancellationToken` plus a unique id so a connection only ever removes its
//! own entry.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::types::Outgoing;

/// Items sent to a connection's writer task (the task that owns the WS sink).
pub enum WsOut {
    /// A JSON control/data frame.
    Frame(Outgoing),
    /// A WS-level Pong (reply to an inbound WS Ping).
    Pong(Vec<u8>),
    /// Ask the writer to close the socket (eviction / fatal error).
    Close,
}

/// A handle to one live WebSocket's writer task.
#[derive(Clone)]
pub struct ConnHandle {
    /// Unique id of the connection (identity check on self-removal).
    pub id: u64,
    /// Sender into the connection's writer task.
    pub tx: mpsc::Sender<WsOut>,
    /// Cancels the connection (used to evict a replaced/revoked peer).
    pub cancel: CancellationToken,
}

#[derive(Default)]
struct NamespaceConns {
    agent: Option<ConnHandle>,
    /// keyed by client ed25519 pubkey, hex.
    clients: HashMap<String, ConnHandle>,
}

/// Thread-safe registry shared across all connection tasks.
#[derive(Default)]
pub struct Registry {
    inner: Mutex<HashMap<String, NamespaceConns>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register the agent connection for `ns`, returning the previous one (if
    /// any) so the caller can cancel it (one agent per namespace).
    pub fn register_agent(&self, ns: &str, handle: ConnHandle) -> Option<ConnHandle> {
        let mut map = self.inner.lock().unwrap();
        let entry = map.entry(ns.to_string()).or_default();
        entry.agent.replace(handle)
    }

    /// Register a client connection, returning the previous one for the same
    /// pubkey (if any) so the caller can cancel it (one connection per device).
    pub fn register_client(
        &self,
        ns: &str,
        pubkey_hex: &str,
        handle: ConnHandle,
    ) -> Option<ConnHandle> {
        let mut map = self.inner.lock().unwrap();
        let entry = map.entry(ns.to_string()).or_default();
        entry.clients.insert(pubkey_hex.to_string(), handle)
    }

    /// Live sender of the namespace's agent, if connected.
    pub fn agent_tx(&self, ns: &str) -> Option<mpsc::Sender<WsOut>> {
        let map = self.inner.lock().unwrap();
        map.get(ns)
            .and_then(|c| c.agent.as_ref())
            .map(|h| h.tx.clone())
    }

    /// Live sender of a client, if connected.
    pub fn client_tx(&self, ns: &str, pubkey_hex: &str) -> Option<mpsc::Sender<WsOut>> {
        let map = self.inner.lock().unwrap();
        map.get(ns)
            .and_then(|c| c.clients.get(pubkey_hex))
            .map(|h| h.tx.clone())
    }

    /// Remove the agent entry, but only if it is still the connection with `id`.
    pub fn remove_agent(&self, ns: &str, id: u64) {
        let mut map = self.inner.lock().unwrap();
        if let Some(conns) = map.get_mut(ns) {
            if conns.agent.as_ref().is_some_and(|h| h.id == id) {
                conns.agent = None;
            }
            Self::gc_empty(&mut map, ns);
        }
    }

    /// Remove a client entry, but only if it is still the connection with `id`.
    pub fn remove_client(&self, ns: &str, pubkey_hex: &str, id: u64) {
        let mut map = self.inner.lock().unwrap();
        if let Some(conns) = map.get_mut(ns) {
            if conns.clients.get(pubkey_hex).is_some_and(|h| h.id == id) {
                conns.clients.remove(pubkey_hex);
            }
            Self::gc_empty(&mut map, ns);
        }
    }

    /// Evict a client by pubkey regardless of id (revocation). Returns the
    /// handle so the caller can cancel it.
    pub fn evict_client(&self, ns: &str, pubkey_hex: &str) -> Option<ConnHandle> {
        let mut map = self.inner.lock().unwrap();
        let handle = map.get_mut(ns).and_then(|c| c.clients.remove(pubkey_hex));
        Self::gc_empty(&mut map, ns);
        handle
    }

    fn gc_empty(map: &mut HashMap<String, NamespaceConns>, ns: &str) {
        if let Some(conns) = map.get(ns)
            && conns.agent.is_none()
            && conns.clients.is_empty()
        {
            map.remove(ns);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle(id: u64) -> (ConnHandle, mpsc::Receiver<WsOut>) {
        let (tx, rx) = mpsc::channel(4);
        (
            ConnHandle {
                id,
                tx,
                cancel: CancellationToken::new(),
            },
            rx,
        )
    }

    #[test]
    fn agent_replacement_returns_old() {
        let reg = Registry::new();
        let (h1, _r1) = handle(1);
        let (h2, _r2) = handle(2);
        assert!(reg.register_agent("ns", h1).is_none());
        let old = reg.register_agent("ns", h2).expect("old agent");
        assert_eq!(old.id, 1);
        assert!(reg.agent_tx("ns").is_some());
    }

    #[test]
    fn self_removal_respects_identity() {
        let reg = Registry::new();
        let (h1, _r1) = handle(1);
        let (h2, _r2) = handle(2);
        reg.register_agent("ns", h1);
        // A newer connection replaced id=1 with id=2.
        reg.register_agent("ns", h2);
        // The old connection (id=1) cleaning up must NOT drop the new one.
        reg.remove_agent("ns", 1);
        assert!(reg.agent_tx("ns").is_some());
        // The current connection (id=2) removes itself → gone.
        reg.remove_agent("ns", 2);
        assert!(reg.agent_tx("ns").is_none());
    }

    #[test]
    fn evict_client_returns_handle() {
        let reg = Registry::new();
        let (h, _r) = handle(7);
        reg.register_client("ns", "ab", h);
        assert!(reg.client_tx("ns", "ab").is_some());
        let evicted = reg.evict_client("ns", "ab").expect("handle");
        assert_eq!(evicted.id, 7);
        assert!(reg.client_tx("ns", "ab").is_none());
    }
}
