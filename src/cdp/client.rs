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
/// treat as "skip and continue". Sized with headroom: a momentarily-slow
/// consumer must not drop `Target.attachedToTarget` events — a dropped attach
/// leaves a `waitForDebuggerOnStart` target paused forever (frozen worker).
const EVENT_BUS_CAPACITY: usize = 16384;

/// Byte bound for a single broadcast event. The bus's only other bound is
/// its slot count; without a byte bound, page-authored bulk payloads
/// (WebSocket frames, SSE data, oversized URLs) are retained up to
/// capacity-fold while any subscriber lags (16384 × 1 MB ≈ 16 GiB).
/// Oversized params are shrunk to fit (see [`shrink_oversized_event`]),
/// keeping the small identity fields consumers read, which caps
/// worst-case ring retention at EVENT_BUS_CAPACITY × MAX_EVENT_BYTES.
const MAX_EVENT_BYTES: usize = 16 * 1024;

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
        let deadline = Duration::from_secs(5);
        let (ws, _resp) = timeout(deadline, tokio_tungstenite::connect_async(ws_url))
            .await
            .map_err(|_| BladeError::Timeout(deadline))?
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
                        Ok(Message::Text(t)) => String::from_utf8_lossy(t.as_bytes()).into_owned(),
                        Ok(Message::Binary(b)) => {
                            // CDP is text; tolerate binary as UTF-8.
                            String::from_utf8_lossy(&b).into_owned()
                        }
                        Ok(Message::Ping(_)) => {
                            // tungstenite auto-pongs at the protocol layer.
                            // The old re-emission as a "__ping" CdpEvent was
                            // non-CDP noise on the bus for every subscriber.
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

        Ok(Self {
            writer_tx,
            pending,
            events_tx,
            next_id,
            closed,
        })
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
        use tokio::io::{AsyncWriteExt, BufReader};

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
                    match crate::framing::read_until_limited(
                        &mut reader,
                        0,
                        &mut buf,
                        64 * 1024 * 1024,
                    )
                    .await
                    {
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

        Ok(Self {
            writer_tx,
            pending,
            events_tx,
            next_id,
            closed,
        })
    }

    /// Send a CDP command and await its result, with the default timeout.
    pub async fn send(&self, method: &str, params: Option<Value>) -> Result<Value> {
        self.send_with_timeout(method, params, DEFAULT_TIMEOUT)
            .await
    }

    /// Send a CDP command with an explicit timeout.
    pub async fn send_with_timeout(
        &self,
        method: &str,
        params: Option<Value>,
        deadline: Duration,
    ) -> Result<Value> {
        self.send_session_with_timeout(None, method, params, deadline)
            .await
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
        let _guard = PendingGuard {
            pending: self.pending.clone(),
            id,
        };
        // Recheck AFTER insert: if the reader died between our first check
        // and the insert, its drain missed us — fail now instead of hanging.
        if self.closed.load(Ordering::Relaxed) {
            return Err(BladeError::Closed);
        }

        let mut req = CdpRequest::new(id, method, params);
        req.session_id = session_id.map(|s| s.to_string());
        let json = serde_json::to_string(&req)?;
        // Queue admission is part of the same deadline: a stalled socket
        // can fill the writer queue before any response wait begins.
        match timeout(deadline, async {
            self.writer_tx
                .send(Message::Text(json.into()))
                .await
                .map_err(|_| BladeError::Closed)?;
            rx.await.map_err(|_| BladeError::Closed)?
        })
        .await
        {
            Ok(outcome) => outcome,
            Err(_) => Err(BladeError::Timeout(deadline)),
        }
        // guard drops here, removing the slot if the reply already arrived
        // (no-op) or if we timed out (cleans the leak).
    }

    /// Subscribe to the event stream. Multiple subscribers are supported.
    pub fn subscribe(&self) -> broadcast::Receiver<CdpEvent> {
        self.events_tx.subscribe()
    }

    /// Has the transport been closed (Chrome died or pipe broke)?
    /// Used by the MCP server to decide whether to self-heal.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// Wait for the next event whose method equals `method`, up to `deadline`.
    ///
    /// Other events are skipped. A lagged bus is recovered transparently.
    pub fn wait_for(
        &self,
        method: &str,
        deadline: Duration,
    ) -> impl std::future::Future<Output = Result<CdpEvent>> {
        // Subscribe NOW, before the caller dispatches the triggering command.
        // An async fn subscribes only when polled, losing events that precede
        // the command's response (cached navigation, downloads, fast pages).
        let mut rx = self.subscribe();
        let method = method.to_string();
        async move {
            match timeout(deadline, async {
                loop {
                    match rx.recv().await {
                        Ok(ev) if ev.method == method => return Ok(ev),
                        Ok(_) => continue,
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => return Err(BladeError::Closed),
                    }
                }
            })
            .await
            {
                Ok(inner) => inner,
                Err(_) => Err(BladeError::Timeout(deadline)),
            }
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

/// Shrink an oversized event's `params` under [`MAX_EVENT_BYTES`] without
/// losing the identity fields consumers key on (`sessionId` of an
/// auto-attached target, a download's `guid`, a paused request's
/// `requestId`, `frame.parentId`, ...). A per-method whitelist missed every
/// event it did not name: an attach with a long data: URL arrived as `{}`
/// and its `waitForDebuggerOnStart` target stayed paused forever.
///
/// Identity fields are short and shallow; the bulk is long page-authored
/// strings (data: URLs, frame payloads, post bodies) or big nested lists
/// and maps (headers, cookies, stack frames). So long strings are capped
/// first, then containers below the top levels are dropped, each step
/// harsher than the last, until the params fit.
fn shrink_oversized_event(params: &mut Value) {
    // (deepest container level kept, per-string byte cap); `params` is
    // level 0. 4096 keeps the URL length the network tracker always got.
    const STEPS: [(usize, usize); 4] = [(usize::MAX, 4096), (1, 4096), (1, 512), (0, 512)];
    for (max_depth, cap) in STEPS {
        clip_event_value(params, 0, max_depth, cap);
        if encodes_within(params, MAX_EVENT_BYTES) {
            return;
        }
    }
    *params = Value::Object(serde_json::Map::new());
}

/// Truncate strings longer than `cap` bytes and drop arrays/objects nested
/// deeper than `max_depth`, in place.
fn clip_event_value(v: &mut Value, depth: usize, max_depth: usize, cap: usize) {
    let too_deep = |c: &Value| depth >= max_depth && (c.is_array() || c.is_object());
    match v {
        Value::String(s) => {
            let keep = crate::platform::truncate_utf8(s, cap).len();
            s.truncate(keep);
        }
        Value::Array(items) => {
            items.retain(|c| !too_deep(c));
            for c in items {
                clip_event_value(c, depth + 1, max_depth, cap);
            }
        }
        Value::Object(map) => {
            map.retain(|_, c| !too_deep(c));
            for c in map.values_mut() {
                clip_event_value(c, depth + 1, max_depth, cap);
            }
        }
        _ => {}
    }
}

/// Does `v` serialize to at most `limit` bytes? Stops at the limit instead
/// of buffering the whole encoding.
fn encodes_within(v: &Value, limit: usize) -> bool {
    struct Budget(usize);
    impl std::io::Write for Budget {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(buf.len())
                .ok_or(std::io::ErrorKind::WriteZero)?;
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Budget(limit), v).is_ok()
}

/// Route one decoded JSON text frame: responses go to their pending caller,
/// events fan out to the broadcast bus. Returns `false` when the pending map
/// is poisoned and the reader should stop. Shared by the WS and pipe readers.
fn route_cdp_text(
    text: &str,
    pending: &PendingMap,
    events_tx: &broadcast::Sender<CdpEvent>,
) -> bool {
    match serde_json::from_str::<CdpMessage>(text) {
        Ok(msg) => match msg.classify() {
            Some(CdpIncoming::Response { id, result, error }) => {
                let outcome = match error {
                    Some(e) => Err(BladeError::Cdp {
                        code: e.code,
                        message: e.message,
                    }),
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
            Some(CdpIncoming::Event(mut ev)) => {
                // Drop on full — never block the reader (Bug 20: a new tab
                // can flood the channel with events nobody consumes, hanging
                // all CDP commands including Target.getTargets).
                // SECURITY: byte-bound the ring. Slot count alone let a
                // hostile page pin retention at capacity × per-event bytes
                // while any subscriber lagged; oversized events keep their
                // small identity fields and drop the bulk payloads nobody
                // consumes.
                if text.len() > MAX_EVENT_BYTES {
                    shrink_oversized_event(&mut ev.params);
                }
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

#[cfg(test)]
mod reliability_tests {
    use super::*;

    #[tokio::test]
    async fn oversized_events_are_shrunk_on_the_bus() {
        let (events_tx, mut rx) = broadcast::channel(8);
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        // A page-authored WS frame with a 100 KB payload.
        let big = "A".repeat(100 * 1024);
        let text = format!(
            "{{\"method\":\"Network.webSocketFrameReceived\",\"params\":{{\"requestId\":\"r1\",\"response\":{{\"payloadData\":\"{big}\"}}}}}}"
        );
        assert!(route_cdp_text(&text, &pending, &events_tx));
        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.method, "Network.webSocketFrameReceived");
        assert!(
            encoded(&ev.params) <= MAX_EVENT_BYTES,
            "bulk payload must not be retained"
        );
        assert_eq!(ev.params["requestId"], "r1");
        // A small event passes through untouched.
        let small = "{\"method\":\"Page.javascriptDialogOpening\",\"params\":{\"type\":\"alert\",\"message\":\"hi\"}}";
        assert!(route_cdp_text(small, &pending, &events_tx));
        let ev = rx.recv().await.unwrap();
        assert_eq!(
            ev.params.get("message").and_then(|m| m.as_str()),
            Some("hi")
        );
        // An oversized requestWillBeSent keeps its identity fields.
        let big_url = "u".repeat(100 * 1024);
        let text = format!(
            "{{\"method\":\"Network.requestWillBeSent\",\"params\":{{\"requestId\":\"r2\",\"type\":\"XHR\",\"request\":{{\"method\":\"POST\",\"url\":\"{big_url}\"}}}}}}"
        );
        assert!(route_cdp_text(&text, &pending, &events_tx));
        let ev = rx.recv().await.unwrap();
        assert_eq!(
            ev.params.get("requestId").and_then(|v| v.as_str()),
            Some("r2")
        );
        assert!(ev
            .params
            .get("request")
            .and_then(|r| r.get("url"))
            .is_some());
    }

    #[tokio::test]
    async fn queue_admission_obeys_command_timeout_and_releases_pending_slot() {
        let (writer_tx, _writer_rx) = mpsc::channel(1);
        writer_tx
            .send(Message::Text("occupied".into()))
            .await
            .unwrap();
        let (events_tx, _) = broadcast::channel(1);
        let client = CdpClient {
            writer_tx,
            pending: Arc::new(Mutex::new(HashMap::new())),
            events_tx,
            next_id: Arc::new(AtomicU64::new(0)),
            closed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let outcome = timeout(
            Duration::from_millis(200),
            client.send_with_timeout("Runtime.evaluate", None, Duration::from_millis(20)),
        )
        .await
        .expect("a saturated writer must not bypass the command deadline");
        assert!(matches!(outcome, Err(BladeError::Timeout(_))));
        assert!(
            client.pending.lock().unwrap().is_empty(),
            "timed-out queue admission must not leak pending calls"
        );
    }

    #[tokio::test]
    async fn websocket_handshake_has_a_deadline() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (_connection, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        let result = timeout(
            Duration::from_secs(6),
            CdpClient::connect(&format!("ws://{address}")),
        )
        .await;
        server.abort();
        assert!(
            matches!(result, Ok(Err(BladeError::Timeout(_)))),
            "a peer that never upgrades must time out: {result:?}"
        );
    }
    /// Route one event frame and return what reached the bus.
    async fn routed(event: serde_json::Value) -> CdpEvent {
        let (events_tx, mut rx) = broadcast::channel(8);
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let text = event.to_string();
        assert!(text.len() > MAX_EVENT_BYTES, "fixture must be oversized");
        assert!(route_cdp_text(&text, &pending, &events_tx));
        rx.recv().await.unwrap()
    }

    fn encoded(v: &Value) -> usize {
        serde_json::to_vec(v).unwrap().len()
    }

    #[tokio::test]
    async fn oversized_events_keep_the_identity_fields_consumers_read() {
        // Regression: an oversized event outside a four-method whitelist
        // reached the bus with params = {}.
        let big = "x".repeat(20 * 1024);

        // Auto-attached worker with a long data: URL — without sessionId the
        // attach handler skips it and the target stays paused forever.
        let ev = routed(serde_json::json!({
            "method": "Target.attachedToTarget", "sessionId": "PAGE",
            "params": {"sessionId": "W1", "waitingForDebugger": true,
                "targetInfo": {"targetId": "T1", "type": "worker", "url": format!("data:text/javascript,{big}")}}
        }))
        .await;
        assert_eq!(ev.params["sessionId"], "W1", "{}", ev.params);
        assert_eq!(ev.params["targetInfo"]["type"], "worker", "{}", ev.params);

        // Download of a large data: URL — guid ties progress events to it.
        let ev = routed(serde_json::json!({
            "method": "Page.downloadWillBegin",
            "params": {"frameId": "F1", "guid": "G1", "url": format!("data:text/csv,{big}"), "suggestedFilename": "report.csv"}
        }))
        .await;
        assert_eq!(ev.params["guid"], "G1", "{}", ev.params);
        assert_eq!(
            ev.params["suggestedFilename"], "report.csv",
            "{}",
            ev.params
        );

        // A subframe navigating to a long URL must still read as a subframe.
        let ev = routed(serde_json::json!({
            "method": "Page.frameNavigated",
            "params": {"type": "Navigation", "frame": {"id": "F2", "parentId": "F1", "loaderId": "L1",
                "url": format!("https://a.test/?q={big}"), "securityOrigin": "https://a.test", "mimeType": "text/html"}}
        }))
        .await;
        assert_eq!(ev.params["frame"]["parentId"], "F1", "{}", ev.params);
        assert!(ev.params["frame"]["url"]
            .as_str()
            .is_some_and(|u| u.starts_with("https://a.test/?q=x")));

        // A paused request with a large body — without requestId the
        // interception task cannot continue it and the request hangs.
        let ev = routed(serde_json::json!({
            "method": "Fetch.requestPaused",
            "params": {"requestId": "interception-1", "frameId": "F1", "resourceType": "XHR",
                "request": {"url": "https://a.test/upload", "method": "POST", "headers": {"Content-Type": "text/plain"}, "postData": big}}
        }))
        .await;
        assert_eq!(ev.params["requestId"], "interception-1", "{}", ev.params);
        assert_eq!(ev.params["resourceType"], "XHR");
        assert_eq!(ev.params["request"]["url"], "https://a.test/upload");
    }

    #[tokio::test]
    async fn shrunk_events_stay_within_the_byte_bound() {
        let long = "y".repeat(5 * 1024);
        // Structural bulk (a big header map / cookie list) is dropped below
        // the levels identity fields live on.
        let headers: serde_json::Map<String, Value> = (0..200)
            .map(|i| (format!("x-h{i}"), Value::String("v".repeat(400))))
            .collect();
        let cookies: Vec<Value> =
            (0..500).map(|i| serde_json::json!({"cookie": {"name": format!("c{i}"), "value": "v".repeat(64)}})).collect();
        let ev = routed(serde_json::json!({
            "method": "Fetch.requestPaused",
            "params": {"requestId": "r9", "resourceType": "Fetch",
                "request": {"url": "https://a.test/x", "method": "GET", "headers": headers}}
        }))
        .await;
        assert!(encoded(&ev.params) <= MAX_EVENT_BYTES);
        assert_eq!(ev.params["requestId"], "r9");
        assert_eq!(ev.params["request"]["url"], "https://a.test/x");
        let ev = routed(serde_json::json!({
            "method": "Network.requestWillBeSentExtraInfo",
            "params": {"requestId": "r10", "associatedCookies": cookies}
        }))
        .await;
        assert!(encoded(&ev.params) <= MAX_EVENT_BYTES);
        assert_eq!(ev.params["requestId"], "r10");
        // Pathological shapes still end inside the bound.
        let wide: serde_json::Map<String, Value> = (0..100)
            .map(|i| (format!("k{i}"), Value::String(long.clone())))
            .collect();
        let ev = routed(serde_json::json!({"method": "X.wide", "params": wide})).await;
        assert!(encoded(&ev.params) <= MAX_EVENT_BYTES);
        let numbers: Vec<Value> = (0..20_000).map(Value::from).collect();
        let ev = routed(
            serde_json::json!({"method": "X.numbers", "params": {"id": 1, "args": numbers}}),
        )
        .await;
        assert!(encoded(&ev.params) <= MAX_EVENT_BYTES);
    }
}
