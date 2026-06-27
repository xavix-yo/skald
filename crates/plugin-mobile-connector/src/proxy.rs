//! HTTP reverse proxy over the relay pipe (`docs/relay/pipe.md`).
//!
//! A remote client (the native app) opens a relayed byte-stream pipe with
//! `stream_type = "http-local-proxy"` and treats it as a raw TCP connection to
//! Skald's local web server. This loop accepts those pipes and splices each one,
//! byte-for-byte, to a fresh `127.0.0.1:<web_port>` connection — a transparent
//! tunnel (we never parse HTTP, so HTTP/1.1 keep-alive, parallel connections, and
//! the chat WebSocket upgrade all work). The destination is pinned to the local
//! web port: the client cannot choose host/port, so this never becomes an open
//! proxy to other local services.
//!
//! Access is already gated by the relay: only the namespace agent or an authorized
//! client can establish a pipe (`pipe.md §3.1`).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace, warn};

use skald_relay_client::{IncomingPipe, PipeConnection, RelayClient};

use crate::PLUGIN_ID;

/// Pipe `stream_type` this loop handles. The native app sets the same value when
/// opening the pipe.
pub(crate) const HTTP_LOCAL_PROXY_STREAM_TYPE: &str = "http-local-proxy";

/// Read buffer for the local→remote direction. Bounded so a pipe can't buffer
/// unboundedly (the relay also caps the data-plane frame size).
const READ_BUF: usize = 64 * 1024;

/// Monotonic per-connection id for log correlation across concurrent tunnels.
static CONN_SEQ: AtomicU64 = AtomicU64::new(1);

/// Subscribe to inbound pipe invites and reverse-proxy `http-local-proxy` pipes
/// to the local web server. One spawned task per accepted pipe.
pub(crate) async fn run_proxy_loop(
    client: Arc<RelayClient>,
    web_port: u16,
    cancel: CancellationToken,
) {
    let mut rx = client.incoming_pipes();
    debug!(plugin = PLUGIN_ID, web_port, "http-local-proxy: listening for pipe invites");
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            ev = rx.recv() => match ev {
                Ok(incoming) => {
                    trace!(
                        plugin = PLUGIN_ID,
                        stream_type = %incoming.stream_type,
                        from = %hex::encode(incoming.from),
                        "http-local-proxy: pipe invite received",
                    );
                    // Ignore (don't reject) other stream_types: `incoming_pipes` is a
                    // broadcast, so a future consumer may legitimately want them.
                    if incoming.stream_type != HTTP_LOCAL_PROXY_STREAM_TYPE {
                        trace!(plugin = PLUGIN_ID, stream_type = %incoming.stream_type,
                               "http-local-proxy: stream_type not ours, ignoring");
                        continue;
                    }
                    let client = Arc::clone(&client);
                    let child  = cancel.child_token();
                    tokio::spawn(async move {
                        accept_and_proxy(client, incoming, web_port, child).await;
                    });
                }
                Err(RecvError::Lagged(n)) => {
                    warn!(plugin = PLUGIN_ID, skipped = n, "incoming pipes lagged");
                }
                Err(RecvError::Closed) => break,
            }
        }
    }
    debug!(plugin = PLUGIN_ID, "http-local-proxy: invite loop stopped");
}

/// Accept one invite, then proxy it. Runs in its own task.
async fn accept_and_proxy(
    client:   Arc<RelayClient>,
    incoming: IncomingPipe,
    web_port: u16,
    cancel:   CancellationToken,
) {
    let conn = CONN_SEQ.fetch_add(1, Ordering::Relaxed);
    debug!(plugin = PLUGIN_ID, conn, from = %hex::encode(incoming.from),
           "http-local-proxy: accepting pipe");
    let pipe = match client.accept_pipe(&incoming).await {
        Ok(p) => p,
        Err(e) => {
            warn!(plugin = PLUGIN_ID, conn, error = %e, "http-local-proxy: accept_pipe failed");
            return;
        }
    };
    debug!(plugin = PLUGIN_ID, conn, "http-local-proxy: pipe accepted, opening local connection");
    proxy_one(conn, pipe, web_port, cancel).await;
}

/// Splice a pipe to a fresh local TCP connection until either side closes.
///
/// The pipe API is half-duplex (`&mut self` for both `send`/`recv`); the
/// `select!` alternates the two directions in one task — only one `&mut pipe`
/// borrow is live at a time, and both `recv`/`read` are cancel-safe. This is
/// ample for HTTP request/response plus the small, bursty chat-WS frames.
async fn proxy_one(conn: u64, mut pipe: PipeConnection, port: u16, cancel: CancellationToken) {
    let tcp = match TcpStream::connect(("127.0.0.1", port)).await {
        Ok(s) => s,
        Err(e) => {
            warn!(plugin = PLUGIN_ID, conn, port, error = %e, "http-local-proxy: local connect failed");
            pipe.close().await;
            return;
        }
    };
    debug!(plugin = PLUGIN_ID, conn, port, "http-local-proxy: local connection established");
    let (mut rd, mut wr) = tcp.into_split();
    let mut buf = vec![0u8; READ_BUF];
    let mut to_local:  u64 = 0; // remote → local bytes
    let mut to_remote: u64 = 0; // local → remote bytes
    let reason: &str;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => { reason = "cancelled"; break; }
            // remote → local: decrypted client bytes forwarded to the web server.
            r = pipe.recv() => match r {
                Ok(Some(bytes)) => {
                    trace!(plugin = PLUGIN_ID, conn, n = bytes.len(), "http-local-proxy: remote→local");
                    if let Err(e) = wr.write_all(&bytes).await {
                        debug!(plugin = PLUGIN_ID, conn, error = %e, "http-local-proxy: local write failed");
                        reason = "local write error"; break;
                    }
                    to_local += bytes.len() as u64;
                }
                Ok(None) => { reason = "remote closed"; break; }
                Err(e) => {
                    debug!(plugin = PLUGIN_ID, conn, error = %e, "http-local-proxy: pipe recv error");
                    reason = "pipe recv error"; break;
                }
            },
            // local → remote: web-server bytes sealed back over the pipe.
            r = rd.read(&mut buf) => match r {
                Ok(0) => { reason = "local EOF"; break; }
                Ok(n) => {
                    trace!(plugin = PLUGIN_ID, conn, n, "http-local-proxy: local→remote");
                    if let Err(e) = pipe.send(&buf[..n]).await {
                        debug!(plugin = PLUGIN_ID, conn, error = %e, "http-local-proxy: pipe send failed");
                        reason = "pipe send error"; break;
                    }
                    to_remote += n as u64;
                }
                Err(e) => {
                    debug!(plugin = PLUGIN_ID, conn, error = %e, "http-local-proxy: local read error");
                    reason = "local read error"; break;
                }
            },
        }
    }
    pipe.close().await;
    debug!(plugin = PLUGIN_ID, conn, to_local, to_remote, reason,
           "http-local-proxy: pipe closed");
}
