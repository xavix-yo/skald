# Pipe — Relayed Byte-Stream over Skald Relay

> **Implementation reference.** A generic, content-blind, end-to-end-encrypted **byte-stream**
> channel between two members of a namespace, **relayed** (TURN-style) through the Skald relay. It
> sits ON TOP of the existing transport: signaling rides the existing E2E `Message` channel (no new
> `RelayFrame`); the data plane is **one new relay endpoint** (`/v1/pipe`). The relay splices opaque
> ciphertext and never reads it.
>
> **Status (v1, implemented).** Scope = **client↔agent** (the shared E2E key already exists, so the
> ephemeral handshake is authenticated by the channel that carries it). Suite = `x25519-sealed`.
> Compression = `none` (negotiation present for forward-compat). client↔client is deferred (needs a
> signed roster/manifest + a self-authenticating suite — see §7).
>
> Read after: `index.md` → `relay-protocol.md` → `crypto.md` → `framing.md`.

## 1. Why

The message channel (`relay-protocol.md`) is for **discrete** E2E payloads (approvals, clarifications,
health sync): ≤60 msg/min, ≤512 KiB/frame, store-and-forward. It serves **stream-shaped, high-volume**
flows poorly — log tailing, file transfer, audio, remote shell, real-time sensors. The pipe is the
**reusable streaming primitive** for those. It is **TURN's relayed mode**: a control plane brokers a
rendezvous; a separate connection carries a raw encrypted byte stream the relay blindly splices, so
TCP/WS gives reliability/ordering/flow-control for free (no reinvented windowing).

```
        Control plane (existing E2E Message channel)         Data plane (new WSS /v1/pipe)
A ──pipe_invite (live)──▶ R ──▶ B                    A ──▶ R ◀── B   (each dials out; NAT-friendly)
A ◀──pipe_accept (live)── R ◀── B                    R verifies auth, matches by connection_id,
   (ephemeral X25519 exchanged → per-pipe key, PFS)     then splices opaque ciphertext frames
   B offline ⇒ A gets PeerOffline ⇒ abort            A ⇄ B: AES-256-GCM stream, relay sees ciphertext
```

## 2. Control plane — signaling

Pipe signaling rides the **existing** `Message{live=true}` E2E frame. It is **not** a new
`RelayFrame`; the relay stays content-blind. To distinguish it from JSON app payloads on the same
channel, the decrypted plaintext uses a reserved framing header (`crypto.md §1`):

```
FRAMING_VERSION_PIPE (0x02) ‖ COMP_NONE (0x00) ‖ <MsgPack PipeSignal>
```

The receiver peeks the first byte (`crypto::is_pipe_signal`): `0x02` ⇒ route to the pipe layer;
`0x01` ⇒ the existing JSON app path, unchanged. `live=true` is required — a stale "please connect" is
useless; if the peer is offline the initiator gets `PeerOffline` (`relay-protocol.md §6.4`) and aborts.

**Wire format = MsgPack** (`rmp-serde`, named maps). `PipeSignal` is externally tagged
(`{ "Invite": {…} }`) so a blob is self-describing. Byte fields are length-validated on decode.

| Message | Fields |
|---------|--------|
| `Invite` | `connection_id` (32B), `suite`, `handshake` (opaque; initiator ephemeral X25519 pub for `x25519-sealed`), `stream_type` (app-defined), `compress` (advertised list), `headers` (arbitrary `String→String`) |
| `Accept` | `connection_id`, `suite`, `handshake` (responder ephemeral pub), `compress` (selected codec) |
| `Reject` | `connection_id`, `reason` |

- **`connection_id`**: 32 random bytes, single-use, short-lived. The rendezvous key, known only to A
  and B (sent E2E). **Not** a security boundary on its own — the data-plane signature (§3) is.
- **`suite`** is a discriminator and **`handshake` is opaque**: adding a Noise suite (§7) is a new
  variant with the **same wire shape**. Signaling is **symmetric** (initiator/responder by role,
  never agent-vs-client) so client↔client is not blocked.
- **`headers`**: app metadata for the stream (filename/size for a transfer, filters for a log tail).

By `pipe_accept` both sides have the peer's ephemeral pubkey and derive the per-pipe key (§4).

## 3. Data plane — `WSS /v1/pipe`

A **second WebSocket**, separate from the control WS, binary frames carrying **raw bytes** (no
protobuf). Chosen over HTTP `CONNECT` / raw TCP for reachability: 443/TLS, traverses CDN / L7 LB /
mobile carriers, camouflaged as a normal WS. The socket **is** the tunnel (one connection per pipe);
the control WS stays separate and alive.

### 3.1 Auth handshake (relay-mediated, MsgPack)
Mirrors the main WS "relay speaks first":
```
A → WSS /v1/pipe
R → PipeChallenge { nonce: 32B }                       (relay speaks first)
A → PipeAuth {
      connection_id, pubkey (ed25519, 32B),
      dest = SHA256(peer_ed25519_pub) (32B),            (declares intended counterparty)
      namespace_id (raw 32B),
      signature = sign_ed25519(priv, PIPE_AUTH_DOMAIN ‖ 0x00 ‖ nonce ‖ connection_id) (64B)
    }
R verifies, in order:
  1. signature valid under pubkey (verify_strict)                  → else close
  2. pubkey is the agent of namespace_id, OR an authorized client  → else close
  3. (on the second side) cross-refs match (§3.2)                  → else close both
```
The reply is a **signature**, not an echo — it proves control of `pubkey`, exactly like the main WS
auth. `connection_id` is **not** trusted as identity.

### 3.2 Matching & splice (relay state machine)
```
challenge → pipe_auth → pending → matched → streaming → teardown
```
- **pending**: first authenticated side for `connection_id` is parked (TTL); the namespace pipe count
  is incremented.
- **matched**: second side authenticates → relay verifies the cross-refs
  `SHA256(A.pubkey)==B.dest AND SHA256(B.pubkey)==A.dest AND same namespace`, then hands the second
  side's socket halves to the first.
- **streaming**: the first side owns a bidirectional forward loop of binary-frame payloads. The relay
  reads nothing else; WS-level pings are answered on the originating leg; data is rate-limited per
  direction.
- **teardown**: either side closing/erroring tears down both (no orphans). *(v1 closes both; FIN
  half-close propagation is a future refinement.)*

### 3.3 Relay limits (NORMATIVE; env-overridable)
The relay becomes a **stateful connection proxy** (TURN resource model): fd+buffers per pipe, idle
reaping, pending TTL, per-namespace concurrency cap, backpressure (no unbounded buffering).

| Limit | Env var | Default | Why |
|-------|---------|---------|-----|
| Pending half-open TTL | `RELAY_PIPE_PENDING_TTL_SECS` | 30 s | A dialed, B never showed → reap. |
| Idle pipe timeout | `RELAY_PIPE_IDLE_TIMEOUT_SECS` | 120 s | Reclaim dead pipes. |
| Max concurrent pipes / namespace | `RELAY_PIPE_MAX_PER_NS` | 8 | Bound proxy resource use. |
| Max data-plane frame | `RELAY_PIPE_MAX_FRAME_BYTES` | 1 MiB | Bulk transfer; separate from the message-channel quota. |
| Bandwidth cap (per connection, per direction) | `RELAY_PIPE_MAX_BPS` | 0 (unlimited) | Token bucket; stops the pipe being a free unmetered tunnel. |

## 4. Secure channel — reused AES-256-GCM, ephemeral DH (PFS)

The A↔B stream reuses the **existing** crypto primitives (`crypto.md`), not Noise/TLS — the same
AES-256-GCM / X25519 / HKDF stack already interop-tested against the iOS client:

- **Per-pipe key**: each side samples a fresh ephemeral X25519, exchanges the pubkey in the signaling,
  and computes `pipe_key = HKDF(ECDH(eph), salt=PIPE_KDF_SALT, info=PIPE_KDF_INFO)`. Ephemeral DH ⇒
  **Perfect Forward Secrecy** per pipe (closes the gap in `index.md §4.3` for this channel).
- **Authentication**: the ephemeral pubkeys travel **inside the E2E-sealed signaling**, so for
  client↔agent they are authenticated by the existing channel — no signatures needed in the
  handshake (that is the `x25519-sealed` suite). client↔client (no pre-shared key) needs a
  self-authenticating suite (§7).
- **Frame crypto**: each chunk is `AES-256-GCM(pipe_key, nonce, aad)`. The 12-byte nonce is
  `DIR (4B) ‖ counter (8B)` with a per-direction counter (`DIR_PIPE_INITIATOR` / `DIR_PIPE_RESPONDER`),
  **not transmitted** (reconstructed by the receiver — strict in-order WS/TCP delivery). `aad =
  connection_id` (binds frames to the rendezvous). Counters start at 1.

The relay never holds `pipe_key`; mismatched keys fail the GCM tag (confidentiality holds even if the
relay mis-splices).

## 5. Compression
Negotiated in the handshake (`compress` advertise→select), per direction, `none | zlib`. **v1 ships
`none` only** — the negotiation field exists for forward-compat. Stateful streaming `zlib` is deferred
(it is a shared-dictionary deflate context, and on a *generic* bus it reintroduces the CRIME/BREACH
class for `stream_type`s mixing attacker-controlled plaintext with secrets).

## 6. Library API (`skald-relay-client`)
```
RelayClient::open_pipe(peer, stream_type, headers) -> PipeConnection      // initiator
RelayClient::incoming_pipes() -> broadcast::Receiver<IncomingPipe>        // responder feed
RelayClient::accept_pipe(&IncomingPipe) -> PipeConnection                 // responder
RelayClient::reject_pipe(&IncomingPipe, reason)
PipeConnection::{ send(&[u8]), recv() -> Option<Vec<u8>>, close() }       // sealed/opened transparently
```
Inbound pipe invites surface on a **separate channel** (`incoming_pipes`), **not** as a `RelayEvent`
variant — so adding the pipe is purely additive and the `plugin-mobile-connector` consumer compiles
unchanged. The relay client owns the pipe control plane end-to-end (it intercepts only the `pipe_*`
signaling kinds; every other payload stays pass-through).

### 6.1 Agent-side consumers (by `stream_type`)
- **`http-local-proxy`** — `plugin-mobile-connector` (`src/proxy.rs`) accepts these pipes and
  reverse-proxies each, byte-for-byte, to the local web server at `127.0.0.1:<web_port>`, letting a
  native WebView render the Skald web UI over the relay (no NAT hole / Tailscale). Destination is
  pinned (not client-chosen); access is already gated by §3.1 (agent or authorized client). See
  [../plugins/mobile-connector.md](../plugins/mobile-connector.md#http-reverse-proxy-http-local-proxy).

## 7. client↔client (deferred)
The data plane is **already** client↔client-capable: the relay authenticates by namespace membership +
cross-dest, not by agent-vs-client. Two things are missing above it, both additive:
1. **Key/identity distribution** — clients don't know each other's keys. Plan: the agent signs a
   **manifest** (versioned roster of authorized members' pubkeys) the relay caches and serves; clients
   verify the agent's ed25519 signature.
2. **Self-authenticating handshake** — without a pre-shared client↔client key the ephemeral exchange
   can't be sealed in an existing channel. Plan: a new `suite` (e.g. `noise-nn+ed25519`) — same
   signaling wire shape, new `handshake` interpretation. The relay does **not** change.

## 8. Verification (implemented)
- **relay-common** (`crypto.rs`, `pipe.rs` tests): MsgPack round-trips; `pipe_auth` sign/verify binds
  nonce + connection_id (and rejects an `AUTH_DOMAIN` signature — domain separation); pipe-key
  symmetry over ephemeral DH; pipe-signal framing peek.
- **relay-server** (`tests/pipe.rs`): two raw WS peers → `pending→matched→streaming→teardown`;
  rejects bad signature, cross-dest mismatch, non-member; teardown closes the peer.
- **relay-client** (`src/pipe.rs` net tests): two `PipeConnection`s stream bytes (incl. a 200 KiB
  blob) both ways through the **real** relay; a wrong key fails AEAD open (relay never had plaintext).
  Signaling routing (`src/state.rs`): invite → `incoming_pipes`, accept/reject → the waiter.
- **Non-regression**: `plugin-mobile-connector` builds unchanged; existing `protocol.rs` /
  `integration.rs` suites still pass.

## 9. Out of scope (deferred)
- Stateful `zlib` compression (negotiation reserved).
- HTTP/2 extended `CONNECT` multiplexing (many pipes over one socket).
- client↔client (§7) and the specific app `stream_type` consumers.
