//! Pipe data-plane client (docs/relay/pipe.md §2-3): the [`PipeConnection`]
//! secure byte channel over a `/v1/pipe` WebSocket.
//!
//! The control plane (invite/accept signaling, ephemeral DH) lives in
//! [`crate::state`]; by the time `PipeConnection::connect` runs, both peers have
//! derived the same per-pipe `pipe_key`. This module only does the data plane:
//! dial `/v1/pipe`, prove identity to the relay (`pipe_auth`), then seal/open
//! every frame with AES-256-GCM keyed by `pipe_key`, using a per-direction
//! counter nonce (the relay forwards opaque ciphertext, pipe.md §2.2).

use std::collections::BTreeMap;

use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use skald_relay_common::crypto;
use skald_relay_common::pipe::{self, PipeAuth, PipeChallenge, PipeSuite};
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// An inbound pipe invite surfaced to the application (responder side). The app
/// inspects `from` / `stream_type` / `headers` and then calls
/// `RelayClient::accept_pipe` or `reject_pipe`. The remaining fields carry the
/// handshake state the accept path needs.
#[derive(Debug, Clone)]
pub struct IncomingPipe {
    /// The initiator's ed25519 pubkey.
    pub from: [u8; 32],
    /// App-defined purpose discriminator.
    pub stream_type: String,
    /// Arbitrary app-defined headers from the invite.
    pub headers: BTreeMap<String, String>,
    /// Rendezvous key (echoed in the accept + data-plane auth).
    pub(crate) connection_id: [u8; 32],
    /// Negotiated suite (v1: only `X25519Sealed`).
    pub(crate) suite: PipeSuite,
    /// The initiator's opaque handshake material (its ephemeral X25519 pubkey).
    pub(crate) peer_handshake: Vec<u8>,
}

type ClientWs =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Which end of the pipe this peer is — selects the send/receive nonce
/// directions so the two AES-GCM streams never collide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipeRole {
    /// Sent `pipe_invite`. Sends on the INITIATOR direction.
    Initiator,
    /// Replied with `pipe_accept`. Sends on the RESPONDER direction.
    Responder,
}

/// An end-to-end-encrypted byte channel to a namespace peer, relayed through
/// `/v1/pipe`. The relay never sees plaintext. Half-duplex usage (`send` then
/// `recv` on `&mut self`); the whole socket is owned here.
pub struct PipeConnection {
    ws: ClientWs,
    key: [u8; 32],
    send_dir: [u8; 4],
    recv_dir: [u8; 4],
    send_ctr: u64,
    recv_ctr: u64,
    /// AAD binding every frame to the rendezvous (the 32-byte connection_id).
    aad: [u8; 32],
}

impl PipeConnection {
    /// Dial `/v1/pipe`, complete the relay auth handshake, and return the ready
    /// channel. `pipe_key` must already be derived from the signaling ephemeral
    /// DH (same value on both peers).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn connect(
        relay_url: &str,
        signing_key: &ed25519_dalek::SigningKey,
        my_ed_pub: &[u8; 32],
        peer_ed_pub: &[u8; 32],
        namespace_id_raw: &[u8; 32],
        connection_id: &[u8; 32],
        pipe_key: &[u8; 32],
        role: PipeRole,
    ) -> Result<PipeConnection> {
        let url = pipe_url(relay_url);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await?;

        // Relay speaks first: PipeChallenge.
        let nonce = read_challenge(&mut ws).await?;

        // Reply with a signature over PIPE_AUTH_DOMAIN ‖ 0x00 ‖ nonce ‖ cid.
        let sig = crypto::sign_pipe_auth(signing_key, &nonce, connection_id);
        let auth = PipeAuth {
            connection_id: connection_id.to_vec(),
            pubkey: my_ed_pub.to_vec(),
            dest: crypto::sha256(peer_ed_pub).to_vec(),
            namespace_id: namespace_id_raw.to_vec(),
            signature: sig.to_vec(),
        };
        ws.send(WsMessage::Binary(pipe::encode(&auth).into())).await?;

        let (send_dir, recv_dir) = match role {
            PipeRole::Initiator => (crypto::DIR_PIPE_INITIATOR, crypto::DIR_PIPE_RESPONDER),
            PipeRole::Responder => (crypto::DIR_PIPE_RESPONDER, crypto::DIR_PIPE_INITIATOR),
        };
        Ok(PipeConnection {
            ws,
            key: *pipe_key,
            send_dir,
            recv_dir,
            send_ctr: 1,
            recv_ctr: 1,
            aad: *connection_id,
        })
    }

    /// Seal and send one application chunk. The 12-byte nonce is implicit
    /// (per-direction counter), so it is not transmitted.
    pub async fn send(&mut self, plaintext: &[u8]) -> Result<()> {
        let nonce = crypto::build_nonce(self.send_dir, self.send_ctr);
        let sealed = crypto::seal(&self.key, &nonce, &self.aad, plaintext)
            .map_err(|e| anyhow!("pipe seal failed: {e}"))?;
        self.send_ctr += 1;
        self.ws.send(WsMessage::Binary(sealed.into())).await?;
        Ok(())
    }

    /// Receive and open the next application chunk. `Ok(None)` on a clean close.
    /// WS-level pings are answered transparently.
    pub async fn recv(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            let Some(msg) = self.ws.next().await else { return Ok(None) };
            match msg? {
                WsMessage::Binary(data) => {
                    let nonce = crypto::build_nonce(self.recv_dir, self.recv_ctr);
                    let pt = crypto::open(&self.key, &nonce, &self.aad, &data)
                        .map_err(|_| anyhow!("pipe open failed (tag mismatch / desync)"))?;
                    self.recv_ctr += 1;
                    return Ok(Some(pt));
                }
                WsMessage::Ping(p) => self.ws.send(WsMessage::Pong(p)).await?,
                WsMessage::Pong(_) => {}
                WsMessage::Close(_) => return Ok(None),
                WsMessage::Text(_) | WsMessage::Frame(_) => {} // pipe is binary-only
            }
        }
    }

    /// Close the underlying socket.
    pub async fn close(mut self) {
        let _ = self.ws.close(None).await;
    }
}

/// Derive the data-plane URL from the control URL by swapping the path
/// `/v1/ws` → `/v1/pipe` (config stores the full control-plane URL).
fn pipe_url(relay_url: &str) -> String {
    if relay_url.contains("/v1/ws") {
        relay_url.replace("/v1/ws", "/v1/pipe")
    } else {
        format!("{}/v1/pipe", relay_url.trim_end_matches('/'))
    }
}

/// Read frames until the relay's [`PipeChallenge`]; return the 32-byte nonce.
async fn read_challenge(ws: &mut ClientWs) -> Result<[u8; 32]> {
    while let Some(msg) = ws.next().await {
        match msg? {
            WsMessage::Binary(data) => {
                let c: PipeChallenge = pipe::decode(&data)
                    .map_err(|e| anyhow!("malformed pipe challenge: {e}"))?;
                return pipe::to_array::<32>(&c.nonce)
                    .ok_or_else(|| anyhow!("pipe challenge nonce is not 32 bytes"));
            }
            WsMessage::Ping(p) => ws.send(WsMessage::Pong(p)).await?,
            WsMessage::Close(_) => return Err(anyhow!("relay closed before pipe challenge")),
            _ => {}
        }
    }
    Err(anyhow!("connection closed before pipe challenge"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_url_swaps_path() {
        assert_eq!(pipe_url("wss://r.example/v1/ws"), "wss://r.example/v1/pipe");
        assert_eq!(pipe_url("ws://127.0.0.1:8080/v1/ws"), "ws://127.0.0.1:8080/v1/pipe");
        assert_eq!(pipe_url("wss://r.example"), "wss://r.example/v1/pipe");
    }
}

/// Data-plane E2E against the **real** relay server (booted in-process): two
/// `PipeConnection`s (initiator + responder, sharing a pre-derived key) dial
/// `/v1/pipe`, get matched, and stream sealed bytes both ways through a relay
/// that only ever sees ciphertext.
#[cfg(test)]
mod net_tests {
    use super::*;
    use std::net::SocketAddr;
    use std::time::Duration;

    use skald_relay_server::config::{Config, PipeConfig};
    use skald_relay_server::{router, AppState};

    async fn spawn_relay() -> (SocketAddr, AppState) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static C: AtomicU64 = AtomicU64::new(0);
        let n = C.fetch_add(1, Ordering::Relaxed);
        let db = std::env::temp_dir().join(format!("relay-pipe-cli-{}-{n}.db", std::process::id()));
        let cfg = Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            db_path: db.to_string_lossy().into(),
            pipe: PipeConfig::default(),
        };
        let state = AppState::build(cfg).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let serve = state.clone();
        tokio::spawn(async move {
            axum::serve(listener, router(serve).into_make_service_with_connect_info::<SocketAddr>())
                .await
                .unwrap();
        });
        (addr, state)
    }

    fn id(seed: u8) -> (ed25519_dalek::SigningKey, [u8; 32]) {
        let dk = crypto::derive_keys(&[seed; 32]);
        (dk.signing_key(), dk.ed25519_pub)
    }

    #[tokio::test]
    async fn pipe_connection_streams_through_real_relay() {
        let (addr, state) = spawn_relay().await;
        let (agent_sk, agent_ed) = id(0xA1);
        let (client_sk, client_ed) = id(0xB2);

        // Seed: agent owns the namespace, client is authorized in it.
        let (ns_raw, ns_hex) = crypto::namespace_id(&agent_ed);
        state.store.upsert_namespace(&ns_hex, &agent_ed).await.unwrap();
        let cx = crypto::derive_keys(&[0xC3; 32]).x25519_pub;
        state.store.upsert_pending_client(&ns_hex, &client_ed, &cx, "", "ios").await.unwrap();
        state.store.apply_authorize(&ns_hex, &[client_ed]).await.unwrap();

        // A pre-shared per-pipe key (in production: ephemeral DH from signaling).
        let key = crypto::derive_pipe_key(&[0x07; 32]);
        let cid = [0x9C; 32];
        let url = format!("ws://{addr}/v1/ws");

        let mut a = PipeConnection::connect(
            &url, &agent_sk, &agent_ed, &client_ed, &ns_raw, &cid, &key, PipeRole::Initiator,
        )
        .await
        .expect("initiator connect");
        tokio::time::sleep(Duration::from_millis(50)).await; // A pending before B
        let mut b = PipeConnection::connect(
            &url, &client_sk, &client_ed, &agent_ed, &ns_raw, &cid, &key, PipeRole::Responder,
        )
        .await
        .expect("responder connect");

        // Bytes both ways.
        a.send(b"ping").await.unwrap();
        assert_eq!(b.recv().await.unwrap().as_deref(), Some(&b"ping"[..]));
        b.send(b"pong").await.unwrap();
        assert_eq!(a.recv().await.unwrap().as_deref(), Some(&b"pong"[..]));

        // A larger blob round-trips intact (seal/open + relay splice).
        let blob = vec![0x5A_u8; 200_000];
        a.send(&blob).await.unwrap();
        assert_eq!(b.recv().await.unwrap(), Some(blob));

        // Closing one tears down the other.
        a.close().await;
        assert_eq!(b.recv().await.unwrap(), None);
    }

    #[tokio::test]
    async fn pipe_wrong_key_fails_to_open() {
        let (addr, state) = spawn_relay().await;
        let (agent_sk, agent_ed) = id(0xD4);
        let (client_sk, client_ed) = id(0xE5);
        let (ns_raw, ns_hex) = crypto::namespace_id(&agent_ed);
        state.store.upsert_namespace(&ns_hex, &agent_ed).await.unwrap();
        let cx = crypto::derive_keys(&[0xF6; 32]).x25519_pub;
        state.store.upsert_pending_client(&ns_hex, &client_ed, &cx, "", "ios").await.unwrap();
        state.store.apply_authorize(&ns_hex, &[client_ed]).await.unwrap();

        let cid = [0x1A; 32];
        let url = format!("ws://{addr}/v1/ws");
        // Mismatched keys: the relay still splices, but `open` must fail (the
        // relay never had the plaintext — confidentiality holds end to end).
        let ka = crypto::derive_pipe_key(&[0x01; 32]);
        let kb = crypto::derive_pipe_key(&[0x02; 32]);

        let mut a = PipeConnection::connect(
            &url, &agent_sk, &agent_ed, &client_ed, &ns_raw, &cid, &ka, PipeRole::Initiator,
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut b = PipeConnection::connect(
            &url, &client_sk, &client_ed, &agent_ed, &ns_raw, &cid, &kb, PipeRole::Responder,
        )
        .await
        .unwrap();

        a.send(b"secret").await.unwrap();
        assert!(b.recv().await.is_err(), "wrong key must fail AEAD open");
    }
}
