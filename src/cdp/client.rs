//! The CDP client — a request/response multiplexer over a WebSocket.
//!
//! One connection, many concurrent commands. Each command gets a monotonic
//! `id`; the browser's reply is matched back to the awaiting caller via a
//! oneshot channel keyed by id. Asynchronous events are fanned out to any
//! number of subscribers over a broadcast channel.
//!
//! Two background tasks own the socket:
//! - a **writer** draining an mpsc of outgoing [`Message`]s into the sink;
//! - a **reader** parsing frames, routing responses to pending callers and
//!   events to the broadcast bus.
//!
//! When the client is dropped the writer's mpsc closes, the writer sends a
//! `Close` frame and exits, the socket closes, the reader observes end-of-stream
//! and drains every still-pending caller with [`BladeError::Closed`]. No caller
//! ever hangs forever.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;

use crate::cdp::protocol::{CdpEvent, CdpIncoming, CdpMessage, CdpRequest};
use crate::error::{BladeError, Result};

/// Default per-command timeout. CDP commands are usually sub-millisecond; this
/// is a safety bound, not a latency target.
pub(crate) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Broadcast bus capacity for events. Large enough to absorb bursts; a lagging
/// subscriber receives [`broadcast::error::RecvError::Lagged`] which callers
/// treat as "skip and continue".
const EVENT_BUS_CAPACITY: usize = 4096;

/// The result delivered to a pending command caller.
type CdpOutcome = std::result::Result<Value, BladeError>;

/// Map of pending command ids → their response oneshot sender.
type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<CdpOutcome>>>>;

/// A handle to one live CDP WebSocket connection.
///
/// Cheap to clone: the underlying socket is owned by the two background tasks,
/// and clones share the same writer channel, pending map, and event bus.
#[derive(Clone)]
pub struct CdpClient {
    writer_tx: mpsc::Sender<Message>,
    pending: PendingMap,
    events_tx: broadcast::Sender<CdpEvent>,
    next_id: Arc<AtomicU64>,
    /// Set when the reader task ends (browser died / socket closed).
    /// drain_pending runs ONCE at reader exit — sends registered afterward
    /// would wait forever without this flag.
    closed: Arc<std::sync::atomic::AtomicBool>,
}

impl CdpClient {
    /// Open a CDP WebSocket connection to `ws_url` and start the I/O tasks.
    ///
    /// `ws_url` is typically a target's `webSocketDebuggerUrl` from
    /// [`crate::cdp::discovery`], e.g. `ws://127.0.0.1:9222/devtools/page/<id>`.
    pub async fn connect(ws_url: &str) -> Result<Self> {
        let (ws, _resp) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| BladeError::Transport(e.to_string()))?;
        Self::from_socket(ws)
    }

    /// Build a client from an already-established WebSocket stream (used by
    /// tests and future transports like a Unix-socket bridge).
    pub fn from_socket<S>(ws: tokio_tungstenite::WebSocketStream<S>) -> Result<Self>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (sink, stream) = ws.split();

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (events_tx, _events_rx) = broadcast::channel(EVENT_BUS_CAPACITY);
        let (writer_tx, writer_rx) = mpsc::channel::<Message>(256);
        let next_id = Arc::new(AtomicU64::new(0));
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Writer: drain the outgoing mpsc into the WebSocket sink.
        {
            let mut sink = sink;
            let mut rx = writer_rx;
            tokio::spawn(async move {
                while let Some(msg) = rx.recv().await {
                    if sink.send(msg).await.is_err() {
                        break;
                    }
                }
                // Best-effort clean close.
                let _ = sink.send(Message::Close(None)).await;
                let _ = sink.close().await;
            });
        }

        // Reader: parse frames, route responses to pending callers, fan out events.
        {
            let pending = pending.clone();
            let events_tx = events_tx.clone();
            let closed_reader = closed.clone();
            tokio::spawn(async move {
                let mut stream = stream;
                while let Some(frame) = stream.next().await {
                    let text = match frame {
                        Ok(Message::Text(t)) => {
                            String::from_utf8_lossy(t.as_bytes()).into_owned()
                        }
                        Ok(Message::Binary(b)) => {
                            // CDP is text; tolerate binary as UTF-8.
                            String::from_utf8_lossy(&b).into_owned()
                        }
                        Ok(Message::Ping(p)) => {
                            // tungstenite auto-pongs, but be defensive.
                            let _ = events_tx
                                .send(CdpEvent {
                                    method: "__ping".into(),
                                    params: Value::Null,
                                    session_id: Some(String::from_utf8_lossy(&p).into_owned()),
                                });
                            continue;
                        }
                        Ok(Message::Close(_)) | Err(_) => break,
                        Ok(_) => continue,
                    };

                    if !route_cdp_text(&text, &pending, &events_tx) {
                        break;
                    }
                }

                closed_reader.store(true, Ordering::Relaxed);
                drain_pending(&pending);
            });
        }

        Ok(Self { writer_tx, pending, events_tx, next_id, closed })
    }

    /// Build a client over `--remote-debugging-pipe` file descriptors (S1:
    /// zero-port CDP). No TCP listener exists, so no page JavaScript can
    /// probe for an open debugging port and no WS handshake residue exists.
    /// Framing: UTF-8 JSON messages delimited by NUL (`\0`).
    ///
    /// `reader` is our end of Chrome's fd 4 (CDP output); `writer` is our
    /// end of Chrome's fd 3 (CDP input).
    /// Unix-only: Windows uses named pipes, which need a different API.
    #[cfg(unix)]
    pub fn from_pipe(
        reader: tokio::net::unix::pipe::Receiver,
        writer: tokio::net::unix::pipe::Sender,
    ) -> Result<Self> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (events_tx, _events_rx) = broadcast::channel(EVENT_BUS_CAPACITY);
        let (writer_tx, writer_rx) = mpsc::channel::<Message>(256);
        let next_id = Arc::new(AtomicU64::new(0));
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Writer: drain the outgoing mpsc into the pipe, NUL-terminated.
        {
            let mut writer = writer;
            let mut rx = writer_rx;
            tokio::spawn(async move {
                while let Some(msg) = rx.recv().await {
                    let text = match msg {
                        Message::Text(t) => t,
                        Message::Close(_) => break,
                        _ => continue,
                    };
                    if writer.write_all(text.as_bytes()).await.is_err()
                        || writer.write_all(b"\0").await.is_err()
                    {
                        break;
                    }
                    let _ = writer.flush().await;
                }
            });
        }

        // Reader: accumulate bytes until NUL, parse, route.
        {
            let pending = pending.clone();
            let events_tx = events_tx.clone();
            let closed_reader = closed.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(reader);
                let mut buf: Vec<u8> = Vec::with_capacity(8192);
                loop {
                    buf.clear();
                    match reader.read_until(0u8, &mut buf).await {
                        Ok(0) => break, // EOF — Chrome exited.
                        Ok(_) => {
                            if buf.last() == Some(&0u8) {
                                buf.pop();
                            }
                            if buf.is_empty() {
                                continue;
                            }
                            let text = String::from_utf8_lossy(&buf).into_owned();
                            if !route_cdp_text(&text, &pending, &events_tx) {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                closed_reader.store(true, Ordering::Relaxed);
                drain_pending(&pending);
            });
        }

        Ok(Self { writer_tx, pending, events_tx, next_id, closed })
    }

    /// Send a CDP command and await its result, with the default timeout.
    pub async fn send(&self, method: &str, params: Option<Value>) -> Result<Value> {
        self.send_with_timeout(method, params, DEFAULT_TIMEOUT).await
    }

    /// Send a CDP command with an explicit timeout.
    pub async fn send_with_timeout(
        &self,
        method: &str,
        params: Option<Value>,
        deadline: Duration,
    ) -> Result<Value> {
        self.send_session_with_timeout(None, method, params, deadline).await
    }

    /// Send a CDP command routed to a sub-session (flat mode). `None` targets
    /// the connection's default session (browser-level on pipe, the page on a
    /// direct WebSocket). Session ids are how one pipe connection multiplexes
    /// many targets.
    pub async fn send_session_with_timeout(
        &self,
        session_id: Option<&str>,
        method: &str,
        params: Option<Value>,
        deadline: Duration,
    ) -> Result<Value> {
        // Fast-fail after browser death: the reader already drained pending
        // slots once at exit, so anything sent now would wait the full
        // timeout for a reply that can never arrive.
        if self.closed.load(Ordering::Relaxed) {
            return Err(BladeError::Closed);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = oneshot::channel::<CdpOutcome>();

        // Register the pending slot BEFORE sending so a lightning-fast reply
        // can't miss it. The guard removes the slot on early drop (cancellation),
        // preventing leaks in the long-lived daemon.
        {
            let mut p = self
                .pending
                .lock()
                .map_err(|e| BladeError::Other(e.to_string()))?;
            p.insert(id, tx);
        }
        let _guard = PendingGuard { pending: self.pending.clone(), id };
        // Recheck AFTER insert: if the reader died between our first check
        // and the insert, its drain missed us — fail now instead of hanging.
        if self.closed.load(Ordering::Relaxed) {
            return Err(BladeError::Closed);
        }

        let mut req = CdpRequest::new(id, method, params);
        req.session_id = session_id.map(|s| s.to_string());
        let json = serde_json::to_string(&req)?;
        self.writer_tx
            .send(Message::Text(json.into()))
            .await
            .map_err(|_| BladeError::Closed)?;

        match timeout(deadline, rx).await {
            Ok(Ok(outcome)) => outcome,
            // Sender dropped (reader ended) → connection closed.
            Ok(Err(_)) => Err(BladeError::Closed),
            Err(_) => Err(BladeError::Timeout(deadline)),
        }
        // guard drops here, removing the slot if the reply already arrived
        // (no-op) or if we timed out (cleans the leak).
    }

    /// Subscribe to the event stream. Multiple subscribers are supported.
    pub fn subscribe(&self) -> broadcast::Receiver<CdpEvent> {
        self.events_tx.subscribe()
    }

    /// Wait for the next event whose method equals `method`, up to `deadline`.
    ///
    /// Other events are skipped. A lagged bus is recovered transparently.
    pub async fn wait_for(&self, method: &str, deadline: Duration) -> Result<CdpEvent> {
        let mut rx = self.subscribe();
        match timeout(deadline, async {
            loop {
                match rx.recv().await {
                    Ok(ev) if ev.method == method => return Ok(ev),
                    Ok(_) => continue,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => {
                        return Err(BladeError::Closed)
                    }
                }
            }
        })
        .await
        {
            Ok(inner) => inner,
            Err(_) => Err(BladeError::Timeout(deadline)),
        }
    }

    // ---- typed convenience wrappers for the domains we actually use ----

    /// Enable a CDP domain (e.g. `"Page"`, `"Runtime"`, `"DOM"`, `"Network"`).
    pub async fn enable(&self, domain: &str) -> Result<Value> {
        self.send(&format!("{domain}.enable"), None).await
    }

    /// Disable a CDP domain.
    pub async fn disable(&self, domain: &str) -> Result<Value> {
        self.send(&format!("{domain}.disable"), None).await
    }
}

/// Route one decoded JSON text frame: responses go to their pending caller,
/// events fan out to the broadcast bus. Returns `false` when the pending map
/// is poisoned and the reader should stop. Shared by the WS and pipe readers.
fn route_cdp_text(text: &str, pending: &PendingMap, events_tx: &broadcast::Sender<CdpEvent>) -> bool {
    match serde_json::from_str::<CdpMessage>(text) {
        Ok(msg) => match msg.classify() {
            Some(CdpIncoming::Response { id, result, error }) => {
                let outcome = match error {
                    Some(e) => Err(BladeError::Cdp { code: e.code, message: e.message }),
                    None => Ok(result),
                };
                let tx = {
                    let mut p = match pending.lock() {
                        Ok(p) => p,
                        Err(_) => return false,
                    };
                    p.remove(&id)
                };
                if let Some(tx) = tx {
                    let _ = tx.send(outcome);
                }
            }
            Some(CdpIncoming::Event(ev)) => {
                // Drop on full — never block the reader (Bug 20: a new tab
                // can flood the channel with events nobody consumes, hanging
                // all CDP commands including Target.getTargets).
                let _ = events_tx.send(ev);
            }
            None => {
                tracing::trace!(target: "bladebro.cdp", "unparseable CDP frame");
            }
        },
        Err(e) => {
            tracing::warn!(target: "bladebro.cdp", error=%e, "non-JSON CDP frame");
        }
    }
    true
}

/// Fail every still-pending caller with [`BladeError::Closed`] (reader ended).
fn drain_pending(pending: &PendingMap) {
    let drained: Vec<oneshot::Sender<CdpOutcome>> = {
        let mut p = match pending.lock() {
            Ok(p) => p,
            Err(_) => return,
        };
        p.drain().map(|(_, v)| v).collect()
    };
    for tx in drained {
        let _ = tx.send(Err(BladeError::Closed));
    }
}

/// RAII guard ensuring a pending command slot is removed from the map when the
/// caller is done (success, error, timeout, or cancellation). Without this, a
/// dropped `send` future would leak its slot in the long-lived daemon.
struct PendingGuard {
    pending: PendingMap,
    id: u64,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if let Ok(mut p) = self.pending.lock() {
            p.remove(&self.id);
        }
    }
}

impl std::fmt::Debug for CdpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CdpClient").finish_non_exhaustive()
    }
}
