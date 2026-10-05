//! Load / settle / re-settle waits — one `awaitPromise` evaluate each,
//! no Rust-side polling loops. Split from the `perception` core.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::json;

use crate::cdp::CdpSession;
use crate::error::Result;

/// Wait for the page to reach at least `interactive` readyState (S18).
/// ONE `Runtime.evaluate` with `awaitPromise` — the wait self-drives in-page
/// via the DOMContentLoaded event instead of N CDP polling round-trips.
/// On main-thread-saturated pages (fingerprint collectors) the old polling
/// loop queued one evaluate per 200ms tick behind long tasks; this queues
/// exactly one. Best-effort: any failure resolves as "proceed".
pub async fn wait_for_load(cdp: &CdpSession, timeout: Duration) -> Result<()> {
    let ms = timeout.as_millis() as u64;
    let expr = format!(
        "new Promise(function(res){{\
            if(document.readyState!=='loading'){{res('ready');return;}}\
            var done=false;function fin(v){{if(!done){{done=true;res(v);}}}}\
            document.addEventListener('DOMContentLoaded',function(){{fin('ready');}},{{once:true}});\
            setTimeout(function(){{fin('timeout');}},{ms});\
        }})",
    );
    let _ = cdp
        .send_with_timeout(
            "Runtime.evaluate",
            Some(json!({
                "expression": expr,
                "returnByValue": true,
                "awaitPromise": true,
            })),
            timeout + Duration::from_secs(3),
        )
        .await;
    Ok(())
}

/// Wait for the DOM to settle after an action (S18). ONE `awaitPromise`
/// evaluate installs a MutationObserver and resolves after ~110ms of DOM
/// quiet (v3.10 speed pass; the 4s of silence before was dead waiting
/// time), or timeout. Replaces the Rust-side polling loop — on heavy-JS
/// pages this cut per-action latency from ~2 minutes to seconds.
///
/// Note: observes childList + characterData only — NOT attributes, since
/// CSS animations mutate style every frame and would perpetually reset the
/// quiet timer, forcing full-timeout waits on every animated page.
pub async fn wait_for_settle(cdp: &CdpSession, timeout: Duration) -> Result<()> {
    wait_for_settle_with_network(cdp, timeout, None).await
}

/// Network-aware settle: in-page DOM quiet (one round-trip), then the
/// in-flight request count drains on the LOCAL atomic (no CDP traffic).
/// `in_flight` is maintained by a background task on [`Page`](crate::page::Page).
/// When `None`, falls back to DOM-only settle.
pub async fn wait_for_settle_with_network(
    cdp: &CdpSession,
    timeout: Duration,
    in_flight: Option<&AtomicUsize>,
) -> Result<()> {
    let ms = timeout.as_millis() as u64;
    let expr = format!(
        "new Promise(function(res){{\
            var t0=performance.now();var last=t0;var done=false;\
            var mo=null;\
            try{{mo=new MutationObserver(function(){{last=performance.now();}});\
            mo.observe(document.documentElement||document,{{childList:true,subtree:true,characterData:true}});}}catch(e){{}}\
            function fin(v){{if(done)return;done=true;try{{if(mo)mo.disconnect();}}catch(e){{}}res(v);}}\
            (function tick(){{\
                var now=performance.now();\
                if(document.readyState!=='loading'&&(now-last)>=110){{fin('settled');return;}}\
                if((now-t0)>={ms}){{fin('timeout');return;}}\
                setTimeout(tick,40);\
            }})();\
        }})",
    );
    let _ = cdp
        .send_with_timeout(
            "Runtime.evaluate",
            Some(json!({
                "expression": expr,
                "returnByValue": true,
                "awaitPromise": true,
            })),
            timeout + Duration::from_secs(3),
        )
        .await;

    // DOM quiet (or we gave up waiting on a saturated page). Now drain the
    // network counter locally — no CDP round-trips, just an atomic read.
    if let Some(counter) = in_flight {
        // Network drain: wait for the post-load burst to finish. We do
        // NOT wait for a fully-quiet network — modern pages never go
        // quiet (analytics beacons, websockets, long-poll) and the count
        // keeps fluctuating, which used to burn the whole 5s deadline
        // (measured: Wikipedia/BBC navigated in 8.5s, floor is 2.5s).
        // Instead: keep waiting only while the count makes progress
        // toward zero (each new low resets the grace timer); break once
        // it has plateaued for GRACE, when it hits zero, or at the hard
        // deadline. Fast by default; agents needing full network quiet
        // can `act wait condition=network` explicitly.
        const GRACE: Duration = Duration::from_millis(280);
        // The drain is a SHORT confirmation window, not a second full
        // settle: on chirpy sites (x.com keeps ~14 requests in flight)
        // trickle completions keep resetting the grace timer, which used
        // to burn the whole learned cap here — on top of the DOM-quiet wait.
        const DRAIN_MAX: Duration = Duration::from_millis(800);
        let hard_deadline = tokio::time::Instant::now() + timeout.min(DRAIN_MAX);
        let mut lowest = counter.load(Ordering::Relaxed);
        let mut last_new_low = tokio::time::Instant::now();
        loop {
            let cur = counter.load(Ordering::Relaxed);
            if cur == 0 {
                break;
            }
            let now = tokio::time::Instant::now();
            if now >= hard_deadline {
                break;
            }
            if cur < lowest {
                lowest = cur;
                last_new_low = now;
            } else if now.duration_since(last_new_low) > GRACE {
                // Plateaued: no new low in GRACE. Either stragglers
                // finished or only persistent connections remain.
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }
    Ok(())
}

/// Bounded post-drain re-quiet for NAVIGATIONS: the drain window can run
/// while a late fetch is still in flight, and the response then mounts a
/// moment later (the "nav returned an empty shell" class). Resolves after
/// ~110ms of DOM quiet; extends (bounded ≤700ms) while a mount is in
/// progress. Nav-only — interaction settles stay snappy.
pub async fn re_settle(cdp: &CdpSession) -> Result<()> {
    const CAP_MS: u64 = 700;
    let expr = format!(
        "new Promise(function(res){{\
         var t0=performance.now();var last=t0;var done=false;var mo=null;\
         try{{mo=new MutationObserver(function(){{last=performance.now();}});\
         mo.observe(document.documentElement||document,{{childList:true,subtree:true,characterData:true}});}}catch(e){{}}\
         function fin(v){{if(done)return;done=true;try{{if(mo)mo.disconnect();}}catch(e){{}}res(v);}}\
         (function tick(){{var now=performance.now();\
           if((now-last)>=110){{fin('quiet');return;}}\
           if((now-t0)>={CAP_MS}){{fin('timeout');return;}}\
           setTimeout(tick,40);}})();\
         }})"
    );
    let _ = cdp
        .send_with_timeout(
            "Runtime.evaluate",
            Some(json!({
                "expression": expr,
                "returnByValue": true,
                "awaitPromise": true,
            })),
            Duration::from_millis(CAP_MS + 3000),
        )
        .await;
    Ok(())
}
