//! Load / settle / re-settle waits — one `awaitPromise` evaluate each,
//! no Rust-side polling loops. Split from the `perception` core.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

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
        .eval_isolated_with_timeout(&expr, true, true, timeout + Duration::from_secs(3))
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
    let _ = wait_for_settle_and_report_drain(cdp, timeout, in_flight).await?;
    Ok(())
}

/// Outcome of an adaptive settle + network drain pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SettleReport {
    /// True when at least one in-flight request actually completed during the
    /// drain window or DOM settle hit its cap — signals that a bounded
    /// post-drain `re_settle` is worthwhile to catch late fetch mounts.
    pub needs_re_settle: bool,
    /// True when DOM settled cleanly while `in_flight` remained > 0 with zero
    /// completions (persistent WS/SSE/analytics connections). Fed back to the
    /// domain knowledge base to classify chirpy domains.
    pub net_plateaued: bool,
}

/// Same as [`wait_for_settle_with_network`], returning `true` when requests
/// drained or DOM settle timed out.
pub(crate) async fn wait_for_settle_and_report_drain(
    cdp: &CdpSession,
    timeout: Duration,
    in_flight: Option<&AtomicUsize>,
) -> Result<bool> {
    Ok(wait_for_settle_adaptive(cdp, timeout, in_flight, false)
        .await?
        .needs_re_settle)
}

/// Adaptive network-aware settle used by navigation.
///
/// When `chirpy` is true (learned from >=2 consecutive plateaued drains on
/// this domain), the idle-plateau grace tightens (280ms -> 110ms, cap 800ms ->
/// 300ms). Active completions (`cur < lowest`) STILL reset the grace timer and
/// trigger `re_settle`, so real fetches are never cut short.
pub(crate) async fn wait_for_settle_adaptive(
    cdp: &CdpSession,
    timeout: Duration,
    in_flight: Option<&AtomicUsize>,
    chirpy: bool,
) -> Result<SettleReport> {
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
                setTimeout(tick,25);\
            }})();\
        }})",
    );
    let dom_res = cdp
        .eval_isolated_with_timeout(&expr, true, true, timeout + Duration::from_secs(3))
        .await;
    let dom_settled = dom_res
        .ok()
        .and_then(|v| {
            v.get("result")
                .and_then(|r| r.get("value"))
                .and_then(|s| s.as_str())
                .map(|s| s == "settled")
        })
        .unwrap_or(false);

    // DOM quiet (or we gave up waiting on a saturated page). Now drain the
    // network counter locally — no CDP round-trips, just an atomic read.
    let mut actually_drained = false;
    let mut net_plateaued = false;
    if let Some(counter) = in_flight {
        // Network drain: wait for the post-load burst to finish without
        // stalling on persistent connections (analytics beacons, websockets,
        // long-poll). Keep waiting while the count makes progress toward zero
        // (each new low resets the grace timer); break once it plateaus for
        // `grace`, hits zero, or reaches `drain_max`.
        let grace = if chirpy {
            Duration::from_millis(110)
        } else {
            Duration::from_millis(280)
        };
        let drain_max = if chirpy {
            Duration::from_millis(300)
        } else {
            Duration::from_millis(800)
        };
        let hard_deadline = tokio::time::Instant::now() + timeout.min(drain_max);
        let initial = counter.load(Ordering::Relaxed);
        let mut lowest = initial;
        let mut last_new_low = tokio::time::Instant::now();
        loop {
            let cur = counter.load(Ordering::Relaxed);
            if cur == 0 {
                lowest = 0;
                break;
            }
            let now = tokio::time::Instant::now();
            if now >= hard_deadline {
                break;
            }
            if cur < lowest {
                lowest = cur;
                last_new_low = now;
            } else if now.duration_since(last_new_low) > grace {
                // Plateaued: no new low in grace. Either stragglers
                // finished or only persistent connections remain.
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let final_cur = counter.load(Ordering::Relaxed);
        // Only mark drained when at least one request actually finished during
        // the drain window; pure persistent connections (initial == lowest > 0)
        // delivered nothing new and do not need a follow-up re_settle.
        actually_drained = lowest < initial;
        net_plateaued = dom_settled && initial > 0 && lowest >= initial && final_cur > 0;
    }
    Ok(SettleReport {
        needs_re_settle: actually_drained || !dom_settled,
        net_plateaued,
    })
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
           setTimeout(tick,25);}})();\
         }})"
    );
    let _ = cdp
        .eval_isolated_with_timeout(&expr, true, true, Duration::from_millis(CAP_MS + 3000))
        .await;
    Ok(())
}
