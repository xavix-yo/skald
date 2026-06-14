//! End-to-end protocol tests against the real axum server on an ephemeral port,
//! using a WebSocket client that performs the genuine Ed25519 challenge-response
//! (relay-protocol.md §4-7).

use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_tungstenite::tungstenite::Message;

use skald_relay_server::config::Config;
use skald_relay_server::{AppState, router};

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Boot a relay on a random port with a throwaway SQLite file. Returns its addr.
async fn spawn_relay() -> SocketAddr {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let db = std::env::temp_dir().join(format!("relay-it-{nanos}-{}.db", std::process::id()));
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        db_path: db.to_string_lossy().into(),
    };
    let state = AppState::build(cfg).await.expect("build state");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            router(state).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    addr
}

async fn connect(addr: SocketAddr) -> Ws {
    let url = format!("ws://{addr}/v1/ws");
    let (ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .expect("connect");
    ws
}

/// Receive the next JSON frame, skipping keepalive pings and WS control frames.
async fn recv(ws: &mut Ws) -> Value {
    loop {
        match ws.next().await.expect("stream open").expect("ws frame") {
            Message::Text(t) => {
                let v: Value = serde_json::from_str(t.as_str()).expect("json");
                if v["type"] == "ping" {
                    continue;
                }
                return v;
            }
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(f) => panic!("unexpected close: {f:?}"),
            other => panic!("unexpected frame: {other:?}"),
        }
    }
}

async fn send(ws: &mut Ws, v: Value) {
    ws.send(Message::Text(v.to_string().into()))
        .await
        .expect("send");
}

fn sign_challenge(sk: &SigningKey, nonce_hex: &str) -> String {
    let nonce = hex::decode(nonce_hex).unwrap();
    let mut msg = Vec::new();
    msg.extend_from_slice(b"skald-relay-auth-v1");
    msg.push(0);
    msg.extend_from_slice(&nonce);
    hex::encode(sk.sign(&msg).to_bytes())
}

fn namespace_id(pubkey: &[u8; 32]) -> String {
    let mut h = Sha256::new();
    h.update(b"skald-namespace-v1");
    h.update([0u8]);
    h.update(pubkey);
    hex::encode(h.finalize())
}

/// Authenticate as `agent`; returns the connection and the expected namespace_id.
async fn auth_agent(addr: SocketAddr, sk: &SigningKey) -> (Ws, String) {
    let pubkey = sk.verifying_key().to_bytes();
    let mut ws = connect(addr).await;
    let challenge = recv(&mut ws).await;
    assert_eq!(challenge["type"], "challenge");
    let sig = sign_challenge(sk, challenge["nonce"].as_str().unwrap());
    send(
        &mut ws,
        json!({"type":"auth","role":"agent","agent_ed25519_pub":hex::encode(pubkey),"signature":sig}),
    )
    .await;
    let ok = recv(&mut ws).await;
    assert_eq!(ok["type"], "auth_ok");
    assert_eq!(ok["role"], "agent");
    assert_eq!(ok["namespace_id"], namespace_id(&pubkey));
    (ws, namespace_id(&pubkey))
}

#[tokio::test]
async fn agent_handshake_creates_namespace() {
    let addr = spawn_relay().await;
    let sk = SigningKey::from_bytes(&[1u8; 32]);
    let (_agent, _ns) = auth_agent(addr, &sk).await;
}

#[tokio::test]
async fn bad_signature_is_rejected() {
    let addr = spawn_relay().await;
    let sk = SigningKey::from_bytes(&[1u8; 32]);
    let pubkey = sk.verifying_key().to_bytes();
    let mut ws = connect(addr).await;
    let _challenge = recv(&mut ws).await;
    // Sign a wrong message → signature won't verify against the real challenge.
    let bogus = hex::encode(sk.sign(b"not the challenge").to_bytes());
    send(
        &mut ws,
        json!({"type":"auth","role":"agent","agent_ed25519_pub":hex::encode(pubkey),"signature":bogus}),
    )
    .await;
    let err = recv(&mut ws).await;
    assert_eq!(err["type"], "auth_error");
    assert_eq!(err["code"], "invalid_signature");
}

#[tokio::test]
async fn unauthorized_client_is_rejected() {
    let addr = spawn_relay().await;
    let agent_sk = SigningKey::from_bytes(&[1u8; 32]);
    let (_agent, ns) = auth_agent(addr, &agent_sk).await;

    // A client that never paired/was authorized cannot connect as `client`.
    let client_sk = SigningKey::from_bytes(&[2u8; 32]);
    let client_pub = client_sk.verifying_key().to_bytes();
    let mut ws = connect(addr).await;
    let challenge = recv(&mut ws).await;
    let sig = sign_challenge(&client_sk, challenge["nonce"].as_str().unwrap());
    send(
        &mut ws,
        json!({"type":"auth","role":"client","namespace_id":ns,
               "client_ed25519_pub":hex::encode(client_pub),
               "device_token":"dev","platform":"ios","signature":sig}),
    )
    .await;
    let err = recv(&mut ws).await;
    assert_eq!(err["type"], "auth_error");
    assert_eq!(err["code"], "unauthorized");
}

#[tokio::test]
async fn pairing_authorize_and_live_message() {
    let addr = spawn_relay().await;
    let agent_sk = SigningKey::from_bytes(&[1u8; 32]);
    let agent_pub = agent_sk.verifying_key().to_bytes();
    let (mut agent, ns) = auth_agent(addr, &agent_sk).await;

    // 1) Agent opens a pairing window.
    let token = [0x11u8; 32];
    send(
        &mut agent,
        json!({"type":"pairing_start","pairing_token":hex::encode(token),"ttl":300}),
    )
    .await;
    let ready = recv(&mut agent).await;
    assert_eq!(ready["type"], "pairing_ready");

    // 2) Client pairs (separate, short-lived connection).
    let client_sk = SigningKey::from_bytes(&[2u8; 32]);
    let client_pub = client_sk.verifying_key().to_bytes();
    let client_x = [0x33u8; 32]; // opaque X25519 pubkey (relay never uses it)
    {
        let mut pairing = connect(addr).await;
        let challenge = recv(&mut pairing).await;
        let sig = sign_challenge(&client_sk, challenge["nonce"].as_str().unwrap());
        send(
            &mut pairing,
            json!({"type":"auth","role":"pairing","namespace_id":ns,
                   "pairing_token":hex::encode(token),
                   "client_ed25519_pub":hex::encode(client_pub),
                   "client_x25519_pub":hex::encode(client_x),
                   "device_token":"devtok","platform":"ios","signature":sig}),
        )
        .await;
        let ok = recv(&mut pairing).await;
        assert_eq!(ok["type"], "auth_ok");
        assert_eq!(ok["role"], "pairing");
        // pairing connection closes after auth_ok; drop it.
    }

    // 3) Agent is told a device paired.
    let paired = recv(&mut agent).await;
    assert_eq!(paired["type"], "client_paired");
    assert_eq!(paired["client_ed25519_pub"], hex::encode(client_pub));
    assert_eq!(paired["client_x25519_pub"], hex::encode(client_x));

    // 4) Agent authorizes the client.
    send(
        &mut agent,
        json!({"type":"authorize","clients":[hex::encode(client_pub)]}),
    )
    .await;
    let authorized = recv(&mut agent).await;
    assert_eq!(authorized["type"], "authorize_ok");
    assert_eq!(authorized["authorized"], 1);

    // 5) Client connects as authorized.
    let mut client = connect(addr).await;
    let challenge = recv(&mut client).await;
    let sig = sign_challenge(&client_sk, challenge["nonce"].as_str().unwrap());
    send(
        &mut client,
        json!({"type":"auth","role":"client","namespace_id":ns,
               "client_ed25519_pub":hex::encode(client_pub),
               "device_token":"devtok","platform":"ios","signature":sig}),
    )
    .await;
    let ok = recv(&mut client).await;
    assert_eq!(ok["type"], "auth_ok");
    assert_eq!(ok["role"], "client");

    // 6) Agent → client live message; relay stamps the authenticated `from`.
    let nonce_hex = "000000010000000000000001";
    let ciphertext = "aGVsbG8gd29ybGQ="; // opaque to the relay
    send(
        &mut agent,
        json!({"type":"message","to":hex::encode(client_pub),"nonce":nonce_hex,"ciphertext":ciphertext}),
    )
    .await;
    let msg = recv(&mut client).await;
    assert_eq!(msg["type"], "message");
    assert_eq!(msg["from"], hex::encode(agent_pub));
    assert_eq!(msg["nonce"], nonce_hex);
    assert_eq!(msg["ciphertext"], ciphertext); // byte-identical passthrough
    assert!(msg["timestamp"].is_string());

    // 7) Client → agent reply routes back.
    let reply_ct = "cmVwbHk=";
    send(
        &mut client,
        json!({"type":"message","to":hex::encode(agent_pub),"nonce":"000000020000000000000001","ciphertext":reply_ct}),
    )
    .await;
    let back = recv(&mut agent).await;
    assert_eq!(back["type"], "message");
    assert_eq!(back["from"], hex::encode(client_pub));
    assert_eq!(back["ciphertext"], reply_ct);
}
