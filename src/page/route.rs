//! Client-side route-transition guard. An SPA router moves `location.href`
//! BEFORE the new route's content paints: the previous route's DOM stays
//! mounted and a DOM-quiet settle reads "settled" the whole time. A read that
//! lands in that window answers with the OLD content at the NEW url and no
//! signal (live reddit search: `extract auto` returned the previous query's
//! results, `phase: ready`, every field looking healthy). The guard turns
//! CDP's `Page.navigatedWithinDocument` event into an epoch; a read that finds
//! the epoch moved waits for the app's own transition to render — DOM-quiet +
//! the in-flight drain, a minimum patience measured from the navigation, and
//! a content signature that must hold still — and reports honestly when it
//! could not confirm stability.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::cdp::CdpSession;

use super::wait_for_settle_with_network;

/// Overall budget for one guard pass (a route change that never stabilizes is
/// reported, never waited out forever).
pub const ROUTE_BUDGET: Duration = Duration::from_millis(2500);
/// Minimum patience measured from the navigation event before a stable
/// signature pair is trusted — covers a router render scheduled behind its
/// own fetch completion, which lands after the DOM has already gone quiet.
const PATIENCE_MS: u64 = 800;
/// Gap between signature samples inside one pass.
const TAIL: Duration = Duration::from_millis(350);
/// Per-pass settle cap (DOM quiet + network drain).
const SETTLE_CAP: Duration = Duration::from_millis(1500);
/// How long after a read an "unsettled" observation stays attachable to it.
const NOTE_TTL_MS: u64 = 2000;

/// Outcome of a route guard pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteSettle {
    /// No same-document navigation since the last pass — nothing to wait for.
    Unchanged,
    /// A route change was observed and the content was stable before the pass
    /// returned.
    Settled,
    /// A route change was observed but the content never held still inside the
    /// budget — the read that follows may predate the new route.
    Unsettled,
}

/// Route-change epoch: bumped by the CDP event tap, consumed by the guard.
///
/// Public only because the action engine's `perform_with_network` signature
/// carries it (the action module is a public path); its methods are crate-
/// internal plumbing.
pub struct RouteEpoch {
    /// Incremented on every `Page.navigatedWithinDocument`.
    epoch: AtomicU64,
    /// `epoch` value already accounted for by a guard pass.
    settled: AtomicU64,
    /// Wall-clock millis of the last observed navigation.
    nav_at_ms: AtomicU64,
    /// Wall-clock millis of the last Unsettled outcome (0 = none). Consumed
    /// by the read that owns it, TTL-bounded so it cannot leak into a later
    /// tool call.
    unsettled_at_ms: AtomicU64,
}

impl RouteEpoch {
    pub(crate) fn new() -> Self {
        Self {
            epoch: AtomicU64::new(0),
            settled: AtomicU64::new(0),
            nav_at_ms: AtomicU64::new(0),
            unsettled_at_ms: AtomicU64::new(0),
        }
    }

    /// Record a same-document navigation (a client-side route change).
    pub(crate) fn note_nav(&self) {
        self.nav_at_ms.store(now_ms(), Ordering::Relaxed);
        self.epoch.fetch_add(1, Ordering::Relaxed);
    }

    /// True when a same-document navigation happened since the last guard pass.
    pub(crate) fn armed(&self) -> bool {
        self.epoch.load(Ordering::Relaxed) != self.settled.load(Ordering::Relaxed)
    }
    /// Consume a fresh Unsettled outcome (the read it belongs to must claim it
    /// within `NOTE_TTL_MS`; after that it is stale and dropped).
    pub(crate) fn take_unsettled(&self) -> bool {
        let at = self.unsettled_at_ms.swap(0, Ordering::Relaxed);
        at != 0 && now_ms().saturating_sub(at) <= NOTE_TTL_MS
    }

    /// Wait for a pending client-side route transition to actually render.
    /// One atomic load when nothing moved.
    pub(crate) async fn settle(
        &self,
        cdp: &CdpSession,
        in_flight: Option<&std::sync::atomic::AtomicUsize>,
        budget: Duration,
    ) -> RouteSettle {
        let epoch = self.epoch.load(Ordering::Relaxed);
        if !self.armed() {
            return RouteSettle::Unchanged;
        }
        let nav_at = self.nav_at_ms.load(Ordering::Relaxed);
        let deadline = tokio::time::Instant::now() + budget;
        let pass = async {
            let mut prev = route_signature(cdp).await;
            loop {
                let _ = wait_for_settle_with_network(cdp, SETTLE_CAP, in_flight).await;
                tokio::time::sleep(TAIL).await;
                let sig = route_signature(cdp).await;
                let patient = now_ms().saturating_sub(nav_at) >= PATIENCE_MS;
                if sig.is_some() && sig == prev && patient {
                    return RouteSettle::Settled;
                }
                prev = sig;
            }
        };
        // Also bounds hung probes and settle calls, whose CDP timeout is longer.
        let outcome = tokio::time::timeout_at(deadline, pass)
            .await
            .unwrap_or(RouteSettle::Unsettled);
        // The epoch we handled (not the latest): a navigation that arrived
        // while we waited keeps the guard armed for the next read.
        self.settled.store(epoch, Ordering::Relaxed);
        if outcome == RouteSettle::Unsettled {
            self.unsettled_at_ms.store(now_ms(), Ordering::Relaxed);
        }
        outcome
    }
}

/// One cheap content fingerprint: url, title, element count, and the head of
/// the main content. A route swap moves at least one of them.
async fn route_signature(cdp: &CdpSession) -> Option<String> {
    let expr = "(()=>{const m=document.querySelector('main')||document.body;\
const t=m?String(m.textContent||'').slice(0,120):'';\
return location.href+'\\u0001'+document.title+'\\u0001'+document.getElementsByTagName('*').length+'\\u0001'+t;})()";
    cdp.send(
        "Runtime.evaluate",
        Some(serde_json::json!({ "expression": expr, "returnByValue": true })),
    )
    .await
    .ok()
    .and_then(|r| {
        r.get("result")
            .and_then(|x| x.get("value"))
            .and_then(|v| v.as_str())
            .map(String::from)
    })
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_epoch_is_not_armed_and_rearms_on_every_navigation() {
        let e = RouteEpoch::new();
        assert!(!e.armed(), "nothing moved yet");
        e.note_nav();
        assert!(e.armed(), "a same-document navigation arms the guard");
        e.note_nav();
        assert!(e.armed());
    }

    #[test]
    fn unsettled_notes_are_consumed_once_and_expire() {
        let e = RouteEpoch::new();
        assert!(!e.take_unsettled(), "nothing recorded");
        e.unsettled_at_ms.store(now_ms(), Ordering::Relaxed);
        assert!(e.take_unsettled(), "fresh observation is claimable");
        assert!(!e.take_unsettled(), "and only once");
        // An observation older than the TTL is dropped, not attached to a
        // later unrelated read.
        let old = now_ms().saturating_sub(NOTE_TTL_MS + 1);
        e.unsettled_at_ms.store(old, Ordering::Relaxed);
        assert!(!e.take_unsettled());
    }
    #[tokio::test]
    async fn unresponsive_probe_cannot_overrun_route_budget() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let _ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            std::future::pending::<()>().await;
        });
        let client = crate::cdp::CdpClient::connect(&format!("ws://{address}"))
            .await
            .unwrap();
        let cdp = CdpSession::root(client);
        let epoch = RouteEpoch::new();
        epoch.note_nav();
        let result = tokio::time::timeout(
            Duration::from_millis(300),
            epoch.settle(&cdp, None, Duration::from_millis(50)),
        )
        .await;
        server.abort();
        assert_eq!(result.unwrap(), RouteSettle::Unsettled);
        assert!(
            epoch.take_unsettled(),
            "timeout must remain visible to the caller"
        );
    }
    #[tokio::test]
    async fn failed_signature_samples_cannot_claim_stable_content() {
        use futures_util::{SinkExt, StreamExt};
        use serde_json::{json, Value};
        use tokio_tungstenite::tungstenite::Message;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let samples = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = samples.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let request: Value = serde_json::from_str(&text).unwrap();
                observed.fetch_add(1, Ordering::Relaxed);
                ws.send(Message::Text(json!({"id":request["id"],"result":{"exceptionDetails":{"text":"context unavailable"}}}).to_string().into())).await.unwrap();
            }
        });
        let client = crate::cdp::CdpClient::connect(&format!("ws://{address}"))
            .await
            .unwrap();
        let cdp = CdpSession::root(client);
        let epoch = RouteEpoch::new();
        epoch.note_nav();
        epoch
            .nav_at_ms
            .store(now_ms().saturating_sub(1000), Ordering::Relaxed);
        let outcome = epoch.settle(&cdp, None, Duration::from_millis(900)).await;
        server.abort();
        assert_eq!(
            outcome,
            RouteSettle::Unsettled,
            "failed probes are not matching content"
        );
        assert!(
            samples.load(Ordering::Relaxed) >= 3,
            "the failure path actually received repeated CDP requests"
        );
    }
}
