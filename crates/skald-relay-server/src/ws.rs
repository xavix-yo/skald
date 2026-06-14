//! Per-connection WebSocket handler (relay.md §4, relay-protocol.md §4-8).
//!
//! One Tokio task per socket. The socket is split: a dedicated writer task owns
//! the sink and drains a `mpsc::Sender<WsOut>` (also stored in the routing
//! registry so peers can deliver to us); the reader task drives the state
//! machine: `challenge → auth(role) → authed forward loop`, with keepalive.

use std::net::IpAddr;
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use futures_util::SinkExt;
use futures_util::stream::{SplitStream, StreamExt};
use rand::RngCore;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::AppState;
use crate::auth::{decode_hex, namespace_id, verify_challenge};
use crate::limits::{
    self, CHALLENGE_TIMEOUT_SECS, IDLE_TIMEOUT_SECS, MAX_FRAME_BYTES, PAIRING_TTL_DEFAULT,
    PAIRING_TTL_MAX, PING_INTERVAL_SECS, QUEUE_MAX_PER_DEST,
};
use crate::push::{Platform, PushItem};
use crate::routing::{ConnHandle, WsOut};
use crate::store::now_ms;
use crate::types::{
    AuthFrame, AuthorizeFrame, Incoming, MessageIn, Outgoing, PairingStartFrame, codes,
};

/// Role of an authenticated, long-lived connection (pairing is short-lived and
/// handled inline, so it is not part of this enum).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Agent,
    Client,
}

/// Whether the reader loop should keep going or close.
enum Flow {
    Continue,
    Close,
}

/// Entry point: drive one accepted WebSocket to completion.
pub async fn handle_socket(socket: WebSocket, state: AppState, peer: IpAddr) {
    let (mut sink, mut stream) = socket.split();
    let (out_tx, mut out_rx) = mpsc::channel::<WsOut>(64);
    let cancel = CancellationToken::new();
    let id = state.next_conn_id();

    // Writer task: the only owner of the sink.
    let writer = tokio::spawn(async move {
        while let Some(item) = out_rx.recv().await {
            let res = match item {
                WsOut::Frame(f) => {
                    let txt = serde_json::to_string(&f).unwrap_or_else(|_| "{}".into());
                    sink.send(Message::Text(txt.into())).await
                }
                WsOut::Pong(p) => sink.send(Message::Pong(p.into())).await,
                WsOut::Close => {
                    let _ = sink.send(Message::Close(None)).await;
                    break;
                }
            };
            if res.is_err() {
                break;
            }
        }
    });

    // The reader/state-machine runs here; on return we tear the connection down.
    run_connection(&mut stream, &out_tx, &cancel, &state, id, peer).await;

    // Drop our sender so the writer finishes, then await it.
    drop(out_tx);
    let _ = writer.await;
    cancel.cancel();
}

/// Handshake then dispatch by role.
async fn run_connection(
    stream: &mut SplitStream<WebSocket>,
    out_tx: &mpsc::Sender<WsOut>,
    cancel: &CancellationToken,
    state: &AppState,
    id: u64,
    peer: IpAddr,
) {
    // --- challenge (relay speaks first) ---
    let mut challenge = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut challenge);
    if out_tx
        .send(WsOut::Frame(Outgoing::Challenge {
            nonce: hex::encode(challenge),
        }))
        .await
        .is_err()
    {
        return;
    }

    // --- await the auth frame within the challenge timeout ---
    let auth = match read_auth(stream, Duration::from_secs(CHALLENGE_TIMEOUT_SECS)).await {
        AuthRead::Frame(a) => *a,
        AuthRead::Timeout => {
            let _ = out_tx
                .send(WsOut::Frame(Outgoing::auth_error(
                    codes::CHALLENGE_TIMEOUT,
                    "no auth in time",
                )))
                .await;
            let _ = out_tx.send(WsOut::Close).await;
            return;
        }
        AuthRead::Bad => {
            let _ = out_tx
                .send(WsOut::Frame(Outgoing::auth_error(
                    codes::BAD_REQUEST,
                    "expected auth frame",
                )))
                .await;
            let _ = out_tx.send(WsOut::Close).await;
            return;
        }
        AuthRead::Closed => return,
    };

    let Some(sig) = decode_hex::<64>(&auth.signature) else {
        let _ = out_tx
            .send(WsOut::Frame(Outgoing::auth_error(
                codes::BAD_REQUEST,
                "bad signature encoding",
            )))
            .await;
        let _ = out_tx.send(WsOut::Close).await;
        return;
    };

    match auth.role.as_str() {
        "agent" => {
            auth_agent(
                stream, out_tx, cancel, state, id, &challenge, &sig, auth, peer,
            )
            .await
        }
        "pairing" => auth_pairing(out_tx, state, &challenge, &sig, auth, peer).await,
        "client" => {
            auth_client(
                stream, out_tx, cancel, state, id, &challenge, &sig, auth, peer,
            )
            .await
        }
        _ => {
            let _ = out_tx
                .send(WsOut::Frame(Outgoing::auth_error(
                    codes::BAD_REQUEST,
                    "unknown role",
                )))
                .await;
            let _ = out_tx.send(WsOut::Close).await;
        }
    }
}

enum AuthRead {
    // Boxed: AuthFrame is much larger than the unit variants.
    Frame(Box<AuthFrame>),
    Timeout,
    Bad,
    Closed,
}

/// Read frames until the first text frame, parse it as `auth`. Anything else
/// before `auth` is rejected (only `auth` is accepted pre-`auth_ok`).
async fn read_auth(stream: &mut SplitStream<WebSocket>, within: Duration) -> AuthRead {
    let deadline = tokio::time::sleep(within);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => return AuthRead::Timeout,
            msg = stream.next() => match msg {
                None | Some(Err(_)) => return AuthRead::Closed,
                Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => continue,
                Some(Ok(Message::Close(_))) => return AuthRead::Closed,
                Some(Ok(Message::Binary(_))) => return AuthRead::Bad,
                Some(Ok(Message::Text(t))) => {
                    if t.len() > MAX_FRAME_BYTES {
                        return AuthRead::Bad;
                    }
                    return match serde_json::from_str::<Incoming>(t.as_str()) {
                        Ok(Incoming::Auth(a)) => AuthRead::Frame(Box::new(a)),
                        _ => AuthRead::Bad,
                    };
                }
            }
        }
    }
}

// --------------------------------------------------------------------------
// role: agent
// --------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn auth_agent(
    stream: &mut SplitStream<WebSocket>,
    out_tx: &mpsc::Sender<WsOut>,
    cancel: &CancellationToken,
    state: &AppState,
    id: u64,
    challenge: &[u8; 32],
    sig: &[u8; 64],
    auth: AuthFrame,
    peer: IpAddr,
) {
    let Some(agent_pub) = auth.agent_ed25519_pub.as_deref().and_then(decode_hex::<32>) else {
        return fail_auth(out_tx, codes::BAD_REQUEST, "bad agent_ed25519_pub").await;
    };
    if !verify_challenge(&agent_pub, challenge, sig) {
        return fail_auth(out_tx, codes::INVALID_SIGNATURE, "signature").await;
    }
    let (_ns_raw, ns) = namespace_id(&agent_pub);

    if let Err(e) = state.store.upsert_namespace(&ns, &agent_pub).await {
        tracing::error!(target: "relay::ws", error = %e, "upsert_namespace failed");
        return fail_auth(out_tx, codes::BAD_REQUEST, "internal").await;
    }

    // One agent per namespace: evict the previous connection.
    let handle = ConnHandle {
        id,
        tx: out_tx.clone(),
        cancel: cancel.clone(),
    };
    if let Some(old) = state.registry.register_agent(&ns, handle) {
        old.cancel.cancel();
    }
    tracing::info!(target: "relay::ws", role = "agent", ns = %short(&ns), %peer, "authenticated");

    let _ = out_tx
        .send(WsOut::Frame(Outgoing::AuthOk {
            role: "agent".into(),
            namespace_id: ns.clone(),
        }))
        .await;

    // Re-deliver `client_paired` for any pending clients the agent may have missed.
    if let Ok(pending) = state.store.list_pending_clients(&ns).await {
        for pc in pending {
            let _ = out_tx
                .send(WsOut::Frame(Outgoing::ClientPaired {
                    client_ed25519_pub: hex::encode(pc.ed25519_pub),
                    client_x25519_pub: hex::encode(pc.x25519_pub),
                    platform: pc.platform,
                }))
                .await;
        }
    }

    authed_loop(stream, out_tx, cancel, state, Role::Agent, &ns, agent_pub).await;
    state.registry.remove_agent(&ns, id);
    tracing::info!(target: "relay::ws", role = "agent", ns = %short(&ns), "disconnected");
}

// --------------------------------------------------------------------------
// role: pairing (short-lived: auth_ok then close)
// --------------------------------------------------------------------------

async fn auth_pairing(
    out_tx: &mpsc::Sender<WsOut>,
    state: &AppState,
    challenge: &[u8; 32],
    sig: &[u8; 64],
    auth: AuthFrame,
    peer: IpAddr,
) {
    let Some(client_pub) = auth
        .client_ed25519_pub
        .as_deref()
        .and_then(decode_hex::<32>)
    else {
        return fail_auth(out_tx, codes::BAD_REQUEST, "bad client_ed25519_pub").await;
    };
    if !verify_challenge(&client_pub, challenge, sig) {
        return fail_auth(out_tx, codes::INVALID_SIGNATURE, "signature").await;
    }
    let (Some(ns), Some(token), Some(client_x), Some(device_token), Some(platform)) = (
        auth.namespace_id.as_deref(),
        auth.pairing_token.as_deref().and_then(decode_hex::<32>),
        auth.client_x25519_pub.as_deref().and_then(decode_hex::<32>),
        auth.device_token.as_deref(),
        auth.platform.as_deref().and_then(Platform::parse),
    ) else {
        return fail_auth(out_tx, codes::BAD_REQUEST, "missing pairing fields").await;
    };

    match state.store.namespace_exists(ns).await {
        Ok(true) => {}
        Ok(false) => return fail_auth(out_tx, codes::NOT_FOUND, "namespace").await,
        Err(e) => {
            tracing::error!(target: "relay::ws", error = %e, "namespace_exists failed");
            return fail_auth(out_tx, codes::BAD_REQUEST, "internal").await;
        }
    }

    // Token must match, be unconsumed, unexpired — consumed atomically here.
    match state.store.consume_pairing_token(ns, &token).await {
        Ok(true) => {}
        Ok(false) => return fail_auth(out_tx, codes::PAIRING_CLOSED, "token").await,
        Err(e) => {
            tracing::error!(target: "relay::ws", error = %e, "consume_pairing_token failed");
            return fail_auth(out_tx, codes::BAD_REQUEST, "internal").await;
        }
    }

    if let Err(e) = state
        .store
        .upsert_pending_client(ns, &client_pub, &client_x, device_token, platform.as_str())
        .await
    {
        tracing::error!(target: "relay::ws", error = %e, "upsert_pending_client failed");
        return fail_auth(out_tx, codes::BAD_REQUEST, "internal").await;
    }

    // Notify the agent (if connected) that a new device paired.
    if let Some(atx) = state.registry.agent_tx(ns) {
        let _ = atx
            .send(WsOut::Frame(Outgoing::ClientPaired {
                client_ed25519_pub: hex::encode(client_pub),
                client_x25519_pub: hex::encode(client_x),
                platform: platform.as_str().into(),
            }))
            .await;
    }

    tracing::info!(target: "relay::ws", role = "pairing", ns = %short(ns), %peer, "paired (pending)");
    let _ = out_tx
        .send(WsOut::Frame(Outgoing::AuthOk {
            role: "pairing".into(),
            namespace_id: ns.into(),
        }))
        .await;
    // The pairing client closes after auth_ok; close cleanly from our side too.
    let _ = out_tx.send(WsOut::Close).await;
}

// --------------------------------------------------------------------------
// role: client
// --------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn auth_client(
    stream: &mut SplitStream<WebSocket>,
    out_tx: &mpsc::Sender<WsOut>,
    cancel: &CancellationToken,
    state: &AppState,
    id: u64,
    challenge: &[u8; 32],
    sig: &[u8; 64],
    auth: AuthFrame,
    peer: IpAddr,
) {
    let Some(client_pub) = auth
        .client_ed25519_pub
        .as_deref()
        .and_then(decode_hex::<32>)
    else {
        return fail_auth(out_tx, codes::BAD_REQUEST, "bad client_ed25519_pub").await;
    };
    if !verify_challenge(&client_pub, challenge, sig) {
        return fail_auth(out_tx, codes::INVALID_SIGNATURE, "signature").await;
    }
    let Some(ns) = auth.namespace_id.as_deref() else {
        return fail_auth(out_tx, codes::BAD_REQUEST, "missing namespace_id").await;
    };

    match state.store.namespace_exists(ns).await {
        Ok(true) => {}
        Ok(false) => return fail_auth(out_tx, codes::NOT_FOUND, "namespace").await,
        Err(e) => {
            tracing::error!(target: "relay::ws", error = %e, "namespace_exists failed");
            return fail_auth(out_tx, codes::BAD_REQUEST, "internal").await;
        }
    }
    match state.store.is_authorized_client(ns, &client_pub).await {
        Ok(true) => {}
        Ok(false) => return fail_auth(out_tx, codes::UNAUTHORIZED, "client").await,
        Err(e) => {
            tracing::error!(target: "relay::ws", error = %e, "is_authorized_client failed");
            return fail_auth(out_tx, codes::BAD_REQUEST, "internal").await;
        }
    }

    // Push tokens rotate: refresh on each connect.
    if let Some(dt) = auth.device_token.as_deref() {
        let _ = state
            .store
            .update_client_device_token(ns, &client_pub, dt)
            .await;
    }

    let pub_hex = hex::encode(client_pub);
    let handle = ConnHandle {
        id,
        tx: out_tx.clone(),
        cancel: cancel.clone(),
    };
    if let Some(old) = state.registry.register_client(ns, &pub_hex, handle) {
        old.cancel.cancel();
    }
    tracing::info!(target: "relay::ws", role = "client", ns = %short(ns), %peer, "authenticated");

    let _ = out_tx
        .send(WsOut::Frame(Outgoing::AuthOk {
            role: "client".into(),
            namespace_id: ns.into(),
        }))
        .await;

    // Drain anything queued while offline (FIFO).
    deliver_pending(out_tx, state, ns, &client_pub).await;

    let ns_owned = ns.to_string();
    authed_loop(
        stream,
        out_tx,
        cancel,
        state,
        Role::Client,
        &ns_owned,
        client_pub,
    )
    .await;
    state.registry.remove_client(&ns_owned, &pub_hex, id);
    tracing::info!(target: "relay::ws", role = "client", ns = %short(&ns_owned), "disconnected");
}

// --------------------------------------------------------------------------
// authed loop (shared by agent + client)
// --------------------------------------------------------------------------

async fn authed_loop(
    stream: &mut SplitStream<WebSocket>,
    out_tx: &mpsc::Sender<WsOut>,
    cancel: &CancellationToken,
    state: &AppState,
    role: Role,
    ns: &str,
    my_pub: [u8; 32],
) {
    let my_pub_hex = hex::encode(my_pub);
    let mut ping = tokio::time::interval(Duration::from_secs(PING_INTERVAL_SECS));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_seen = Instant::now();
    let mut rate = limits::ConnRate::new();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = out_tx.send(WsOut::Close).await;
                break;
            }
            _ = ping.tick() => {
                if last_seen.elapsed() > Duration::from_secs(IDLE_TIMEOUT_SECS) {
                    let _ = out_tx.send(WsOut::Close).await;
                    break;
                }
                if out_tx.send(WsOut::Frame(Outgoing::Ping)).await.is_err() {
                    break;
                }
            }
            msg = stream.next() => {
                let Some(Ok(m)) = msg else { break };
                last_seen = Instant::now();
                match m {
                    Message::Text(t) => {
                        if t.len() > MAX_FRAME_BYTES {
                            let _ = out_tx.send(WsOut::Frame(Outgoing::error(
                                codes::PAYLOAD_TOO_LARGE, "frame exceeds 64 KiB"))).await;
                            let _ = out_tx.send(WsOut::Close).await;
                            break;
                        }
                        match handle_frame(out_tx, state, role, ns, &my_pub, &my_pub_hex,
                                           &mut rate, t.as_str()).await {
                            Ok(Flow::Continue) => {}
                            Ok(Flow::Close) => break,
                            Err(e) => {
                                tracing::warn!(target: "relay::ws", error = %e, "frame handler error");
                                break;
                            }
                        }
                    }
                    Message::Ping(p) => {
                        let _ = out_tx.send(WsOut::Pong(p.to_vec())).await;
                    }
                    Message::Pong(_) => {} // activity already recorded
                    Message::Binary(_) => {} // not used in v1; ignored
                    Message::Close(_) => break,
                }
            }
        }
    }
    // Keep the namespace alive timestamp fresh on clean disconnect.
    let _ = state.store.touch_namespace(ns).await;
}

/// Parse and dispatch one authed text frame.
#[allow(clippy::too_many_arguments)]
async fn handle_frame(
    out_tx: &mpsc::Sender<WsOut>,
    state: &AppState,
    role: Role,
    ns: &str,
    my_pub: &[u8; 32],
    my_pub_hex: &str,
    rate: &mut limits::ConnRate,
    text: &str,
) -> anyhow::Result<Flow> {
    let incoming = match serde_json::from_str::<Incoming>(text) {
        Ok(i) => i,
        Err(_) => {
            let _ = out_tx
                .send(WsOut::Frame(Outgoing::error(
                    codes::BAD_REQUEST,
                    "malformed json",
                )))
                .await;
            return Ok(Flow::Continue);
        }
    };

    match incoming {
        Incoming::Message(m) => {
            if !rate.allow_message() {
                let _ = out_tx
                    .send(WsOut::Frame(Outgoing::error(
                        codes::RATE_LIMITED,
                        "too many messages",
                    )))
                    .await;
                let _ = out_tx.send(WsOut::Close).await;
                return Ok(Flow::Close);
            }
            forward_message(out_tx, state, ns, my_pub_hex, my_pub, m).await?;
        }
        Incoming::Authorize(a) if role == Role::Agent => {
            handle_authorize(out_tx, state, ns, a).await?
        }
        Incoming::PairingStart(p) if role == Role::Agent => {
            handle_pairing_start(out_tx, state, ns, p).await?
        }
        Incoming::PairingStop if role == Role::Agent => {
            state.store.pairing_stop(ns).await?;
            let _ = out_tx.send(WsOut::Frame(Outgoing::PairingStopOk)).await;
        }
        Incoming::Authorize(_) | Incoming::PairingStart(_) | Incoming::PairingStop => {
            let _ = out_tx
                .send(WsOut::Frame(Outgoing::error(
                    codes::BAD_REQUEST,
                    "frame not allowed for role",
                )))
                .await;
        }
        Incoming::Ping => {
            let _ = out_tx.send(WsOut::Frame(Outgoing::Pong)).await;
        }
        Incoming::Pong | Incoming::Unknown | Incoming::Auth(_) => {} // ignored
    }
    Ok(Flow::Continue)
}

/// Route an E2E message: live if the recipient is connected, else
/// store-and-forward (+ push for offline clients).
async fn forward_message(
    out_tx: &mpsc::Sender<WsOut>,
    state: &AppState,
    ns: &str,
    from_hex: &str,
    from_pub: &[u8; 32],
    m: MessageIn,
) -> anyhow::Result<()> {
    let (Some(to), Some(nonce)) = (decode_hex::<32>(&m.to), decode_hex::<12>(&m.nonce)) else {
        let _ = out_tx
            .send(WsOut::Frame(Outgoing::error(
                codes::BAD_REQUEST,
                "bad to/nonce",
            )))
            .await;
        return Ok(());
    };
    let Ok(ct_bytes) = B64.decode(m.ciphertext.as_bytes()) else {
        let _ = out_tx
            .send(WsOut::Frame(Outgoing::error(
                codes::BAD_REQUEST,
                "bad ciphertext",
            )))
            .await;
        return Ok(());
    };
    let to_hex = hex::encode(to);

    // Resolve the recipient within the namespace.
    let agent_pub = state.store.agent_pub(ns).await?;
    let is_agent_dest = agent_pub.as_ref() == Some(&to);
    let is_client_dest = !is_agent_dest && state.store.is_authorized_client(ns, &to).await?;
    if !is_agent_dest && !is_client_dest {
        let _ = out_tx
            .send(WsOut::Frame(Outgoing::error(codes::NOT_FOUND, "recipient")))
            .await;
        return Ok(());
    }

    // Try live delivery first. `from`/`nonce` emitted as canonical lowercase
    // hex; `ciphertext` passed through verbatim (never altered).
    let live = if is_agent_dest {
        state.registry.agent_tx(ns)
    } else {
        state.registry.client_tx(ns, &to_hex)
    };
    let frame = Outgoing::Message {
        from: from_hex.to_string(),
        nonce: hex::encode(nonce),
        ciphertext: m.ciphertext.clone(),
        timestamp: now_iso(),
    };
    if let Some(tx) = live
        && tx.send(WsOut::Frame(frame)).await.is_ok()
    {
        return Ok(());
    }
    // writer gone: fall through to store-and-forward.

    // Offline: enqueue, then push if the recipient is a client.
    let ok = state
        .store
        .enqueue(ns, &to, from_pub, &nonce, &ct_bytes, QUEUE_MAX_PER_DEST)
        .await?;
    if !ok {
        let _ = out_tx
            .send(WsOut::Frame(Outgoing::error(
                codes::QUEUE_FULL,
                "recipient queue full",
            )))
            .await;
        return Ok(());
    }

    if is_client_dest
        && let Some(client) = state.store.get_client(ns, &to).await?
        && let (Some(dt), Some(plat)) = (client.device_token, Platform::parse(&client.platform))
    {
        let item = PushItem {
            namespace_id: ns.to_string(),
            from_hex: from_hex.to_string(),
            nonce_hex: hex::encode(nonce),
            ciphertext_b64: m.ciphertext,
        };
        state.pusher.notify(&dt, plat, &item).await;
    }
    Ok(())
}

async fn handle_authorize(
    out_tx: &mpsc::Sender<WsOut>,
    state: &AppState,
    ns: &str,
    a: AuthorizeFrame,
) -> anyhow::Result<()> {
    let mut keys = Vec::with_capacity(a.clients.len());
    for k in &a.clients {
        let Some(b) = decode_hex::<32>(k) else {
            let _ = out_tx
                .send(WsOut::Frame(Outgoing::error(
                    codes::BAD_REQUEST,
                    "bad client pubkey",
                )))
                .await;
            return Ok(());
        };
        keys.push(b);
    }
    let (count, revoked) = state.store.apply_authorize(ns, &keys).await?;
    // Disconnect revoked clients that are currently live.
    for r in revoked {
        if let Some(old) = state.registry.evict_client(ns, &hex::encode(r)) {
            old.cancel.cancel();
        }
    }
    let _ = out_tx
        .send(WsOut::Frame(Outgoing::AuthorizeOk { authorized: count }))
        .await;
    Ok(())
}

async fn handle_pairing_start(
    out_tx: &mpsc::Sender<WsOut>,
    state: &AppState,
    ns: &str,
    p: PairingStartFrame,
) -> anyhow::Result<()> {
    let Some(token) = decode_hex::<32>(&p.pairing_token) else {
        let _ = out_tx
            .send(WsOut::Frame(Outgoing::error(
                codes::BAD_REQUEST,
                "bad pairing_token",
            )))
            .await;
        return Ok(());
    };
    let ttl = p
        .ttl
        .unwrap_or(PAIRING_TTL_DEFAULT)
        .clamp(1, PAIRING_TTL_MAX);
    let expiry = now_ms() + (ttl as i64) * 1000;
    state.store.pairing_start(ns, &token, expiry).await?;
    let _ = out_tx
        .send(WsOut::Frame(Outgoing::PairingReady { ttl }))
        .await;
    Ok(())
}

/// Drain the recipient's queue in FIFO order, deleting each after delivery.
async fn deliver_pending(
    out_tx: &mpsc::Sender<WsOut>,
    state: &AppState,
    ns: &str,
    to_pub: &[u8; 32],
) {
    let pending = match state.store.fetch_pending(ns, to_pub).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(target: "relay::ws", error = %e, "fetch_pending failed");
            return;
        }
    };
    for qm in pending {
        let frame = Outgoing::Message {
            from: hex::encode(qm.from_pub),
            nonce: hex::encode(qm.nonce),
            ciphertext: B64.encode(&qm.ciphertext),
            timestamp: iso_from_ms(qm.created_at),
        };
        if out_tx.send(WsOut::Frame(frame)).await.is_err() {
            return; // peer gone; leave the rest queued
        }
        if let Err(e) = state.store.delete_pending(qm.id).await {
            tracing::error!(target: "relay::ws", error = %e, "delete_pending failed");
            return;
        }
    }
}

// --------------------------------------------------------------------------
// helpers
// --------------------------------------------------------------------------

async fn fail_auth(out_tx: &mpsc::Sender<WsOut>, code: &str, message: &str) {
    let _ = out_tx
        .send(WsOut::Frame(Outgoing::auth_error(code, message)))
        .await;
    let _ = out_tx.send(WsOut::Close).await;
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn iso_from_ms(ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .unwrap_or_else(chrono::Utc::now)
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Truncate a namespace_id / pubkey for logging.
fn short(s: &str) -> String {
    let n = s.len().min(8);
    format!("{}…", &s[..n])
}
