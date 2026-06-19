# Mobile Connector Plugin (`mobile-connector`)

Bridges Skald's **Inbox** (approvals + clarifications) to mobile apps over the
**relay**, implementing the **agent** role of the v2 relay protocol. The plugin is
the namespace owner and the sole authority over authorized devices. Skald is
never exposed on the internet: only this plugin connects out, and only to the
relay.

- Crate: `crates/plugin-mobile-connector`
- Shared crypto + protobuf: `crates/skald-relay-common` (byte-for-byte interop
  with the reference vectors in [relay/test-vectors.md](../relay/test-vectors.md))
- **Protocol documentation** (canonical, in English):
  - [relay/index.md](../relay/index.md) — architecture, actors, threat model
  - [relay/relay-protocol.md](../relay/relay-protocol.md) — protobuf schema, auth, pairing, live channel, presence
  - [relay/framing.md](../relay/framing.md) — E2E plaintext framing (version + compression)
  - [relay/payloads.md](../relay/payloads.md) — JSON payload schemas (inbox_request, inbox_update, …)
  - [relay/crypto.md](../relay/crypto.md) — crypto contract, key derivation, AEAD, anti-replay

---

## Module map

| Module | Role |
|---|---|
| `identity.rs` | Seed load/generate (`data/relay/seed`, `0600`) + derived Ed25519/X25519 keys + `namespace_id` |
| `db.rs` | `relay_clients` table — devices + anti-replay counters (atomic counter helpers) |
| `pairing.rs` | In-memory single-window pairing sessions (`code → session`) + `QrCodeData` |
| `payloads.rs` | E2E JSON payload schemas (`inbox_update`, `notification`, client responses incl. `inbox_request`). Zlib-compressible per v2 framing.md |
| `state.rs` | Shared runtime: pairing policy, per-client `aes_key` cache, seal/open, Inbox application. Presence tracking per namespace |
| `ws.rs` | Permanent reconnecting agent WebSocket (v2 binary transport). Challenge → protobuf `Auth` decode → role dispatch → forward loop. Handles presence events and live (`Message.live=true`) dispatch for `inbox_request` pulls |
| `router.rs` | The QR-code HTTP endpoint (`/pairingqrcode`) |
| `agent.rs` | `RelayAgent` control trait (pairing, list, authorize, revoke) |
| `tools.rs` | The three LLM tools, registered in the main crate's `ToolRegistry` |
| `lib.rs` | `MobileConnectorPlugin` (`Plugin` + `RelayAgent`), lifecycle, bus subscriber |

---

## Configuration

Stored in the `plugins` table (JSON, edited via the plugin UI / `configure_plugin`):

```yaml
relay_url: "wss://relay.skaldagent.net/v1/ws"  # empty ⇒ plugin idle (no WS)
pairing_ttl: 300                                # seconds, max 600
require_device_confirmation: true               # manual confirm new devices (recommended)
```

`enabled` (the standard plugin flag) starts/stops the runloop.

---

## Persistence (plugin.md §9)

| Data | Location | Why |
|---|---|---|
| `seed` (32 B) | filesystem `data/relay/seed`, `0600` | the only persistent secret; keys + `namespace_id` are derived at runtime |
| Pairing session | **in-memory** only | transient (≤ TTL); lost on restart ⇒ just re-pair |
| Devices + `send/recv_counter` | DB `relay_clients` | **must** survive restarts |

### Why counters live in the DB

Skald self-restarts by design. If counters reset to 0 on restart:
- `send_counter → 0` reuses an AES-GCM nonce under the same key (breaks
  confidentiality + integrity for that device).
- `recv_counter → 0` re-opens the replay window.

So `send_counter` is incremented **and persisted before** sealing/sending
(`db::next_send_counter`, a transaction), and `recv_counter` is persisted only
**after** a valid `open`.

### `aes_key` cache

The per-client AES-256-GCM key is `HKDF(X25519(seed_x_priv, client_x_pub))`. It
is derived once and cached in memory (`HashMap<ed25519_pub, aes_key>` in
`RelayState`), never persisted; on a cache miss it is re-derived from the
client's stored `x25519_pub`. The cache entry is dropped on revoke.

---

## Pairing flow

1. The agent calls `mobile_start_pairing(ttl?)` (gated behind approval).
2. The plugin generates a 32-byte `pairing_token` (CSPRNG), sends
   `pairing_start{token, ttl}` to the relay, and registers an in-memory session
   keyed by a separate random `code` (latest-wins: any prior active session is
   marked *Superseded*). It returns the URL
   `/api/plugin/mobile-connector/pairingqrcode?code=<code>`.
3. The copilot renders the URL as an image. The endpoint serves a PNG of the QR
   while the session is **Active**, else a placeholder (`QR scaduto` /
   `QR già usato`). The QR payload is the normative `QrCodeData` JSON (never on
   disk, never in the URL).
4. The client scans, connects as `role:"pairing"`, the relay consumes the token
   and forwards `client_paired` to the agent.
5. On `client_paired`: derive + cache `aes_key`, persist the client as
   **Pending** (counters 0), mark the session **Consumed**, then apply the
   policy:
   - `require_device_confirmation = false` ⇒ auto-authorize.
   - `require_device_confirmation = true` ⇒ leave Pending; the human authorizes
     via the control surface (a `notification` is pushed to existing devices).

`authorize` always reflects the full local set (replacement semantics): adding a
device sends the complete list including it; revoking sends it without.

---

## Message flows

- **Inbox → clients:** the bus subscriber reacts to the four Inbox events
  (`approval_requested`, `approval_resolved`, `clarification_requested`,
  `clarification_resolved`), builds an `InboxSnapshot` via `inbox.list_pending()`,
  and sends a sealed `inbox_update` to every Authorized client. Each approval
  carries a humanised `summary` (from `Tool::describe(Short)`, computed in
  `Inbox::list_pending`) for the card/notification plus the raw `arguments`
  (untruncated) for the detail dialog — so the user sees the full `execute_cmd`
  command, not a truncated label. Each clarification carries its
  `suggested_answers`.
- **Clients → Inbox:** inbound `message` is checked (`from` ∈ Authorized,
  nonce direction + counter > `recv_counter`), opened, and dispatched by `kind`:
  `approval_response` → `inbox.approve/reject`, `clarification_response` →
  `inbox.answer`, `hello` → persist `device_info`, `inbox_request` → send a
  **targeted** `inbox_update` back to `from` only (see below), `logout` → revoke.
  After any response the Inbox is re-snapshotted. `request_id` is mapped
  `string ↔ i64` (non-parsing ids are dropped). Inbox ops are idempotent by
  `request_id`.
- **Reconnect snapshot (`inbox_request`):** the relay does **not** notify the
  agent when a client reconnects, so the client sends `inbox_request` on the
  **live channel** (`Message.live=true`) after every `auth_ok` (e.g. when the app
  is opened from a push). The agent replies with an `inbox_update` sealed to the
  requester only — not a broadcast — so other devices are not needlessly
  re-aligned. A pull of stale state is useless, so the live channel is correct:
  if the agent is offline, the client gets `PeerOffline` immediately instead of
  waiting. Side-effect-free and idempotent (by `request_id`). See
  `data/ios-app/v2/relay-protocol.md` §3.1.

---

## LLM tools (plugin.md §11)

| Tool | Effect | Approval |
|---|---|---|
| `mobile_start_pairing(ttl?)` | Open the pairing window, return the QR URL | **Gated** (a default `require` rule is seeded, like `execute_cmd`/`restart`) |
| `mobile_list_devices()` | List devices (state, platform, device_info, last_seen) | read-only |
| `mobile_revoke_device(pubkey)` | Revoke a device by hex ed25519 pubkey | `Config` category |

These tools are not contributed through the `Plugin` trait (which has no
`tools()` method). They are registered in `Skald::new` (`src/core/skald.rs`):
the plugin is fetched via `get_plugin_typed::<MobileConnectorPlugin>()`, cast to
`Arc<dyn RelayAgent>`, and bound into the tools via
`plugin_mobile_connector::mobile_tools(agent)` → `ToolRegistry::register_arc`.

`mobile_start_pairing`'s approval gate is the default rule seeded in
`ApprovalManager::seed_defaults` (`src/core/approval/mod.rs`): opening a window
emits a secret (the QR) into chat, so it must be a deliberate human action, not
LLM-triggerable via prompt injection.

---

## HTTP endpoint

`GET /api/plugin/mobile-connector/pairingqrcode?code=<random>` — runtime PNG of
the QR (or placeholder), behind Skald's normal auth. Mounted by `WebFrontend`
via `Plugin::http_router()` (the router closes over the live `RelayState`). The
`code` is a non-enumerable capability; a URL leaked into `chat_history`
self-revokes once the window closes.
