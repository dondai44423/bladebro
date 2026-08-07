//! Runtime-structural perception: capture the live DOM as actionable elements.
//!
//! The thesis (decision D7): we read the live DOM and infer semantics from the
//! markup itself — not from the browser's accessibility tree, which was built
//! for screen readers and lies on poorly-annotated pages. A `<div role=button>`
//! with no ARIA name still gets a name from its text; a hidden error `<div>`
//! still appears because it exists in the DOM.
//!
//! We inject one script via `Runtime.evaluate` (with `returnByValue`) that
//! walks the document, computes a *semantic* role + name for each interactive
//! element, filters to visible+actionable ones, and returns a compact array.
//! This is a single CDP round-trip and lets us compute the stability signature
//! in-page (where the real DOM lives), not re-derive it in Rust from a raw tree.

use serde::Deserialize;
use serde_json::json;
use std::time::Duration;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::cdp::CdpSession;
use crate::error::{BladeError, Result};

/// The compact descriptor of one actionable element as captured from the page.
///
/// `ref` is NOT assigned here — the stabilizer (`refs.rs`) assigns stable refs
/// by matching across captures. Here we only carry the identity + signature +
/// live state needed to (a) stabilize and (b) act later.
#[derive(Debug, Clone, Deserialize)]
pub struct RawElement {
    pub tag: String,
    /// Inferred semantic role: `button`/`link`/`textbox`/`checkbox`/`radio`/
    /// `combobox`/`tab`/`menuitem`/`switch`/`slider`/`file`/`color`/`generic`.
    pub role: String,
    #[serde(default)]
    pub name: String,
    /// Input `type` for `<input>`, the `<select>` multiple flag, etc.
    #[serde(default, rename = "type")]
    pub element_type: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub disabled: bool,
    /// Present only for checkbox/radio.
    #[serde(default)]
    pub checked: Option<bool>,
    #[serde(default)]
    pub href: Option<String>,
    #[serde(default)]
    pub placeholder: Option<String>,
    /// `required` attribute on form elements.
    #[serde(default)]
    pub required: bool,
    /// `aria-haspopup` is set — this element opens a menu/dropdown.
    #[serde(default, rename = "haspopup")]
    pub has_popup: bool,
    /// Nearest ancestor landmark role (nav/main/banner/footer/aside/search/dialog).
    /// `None` = default content area.
    #[serde(default)]
    pub landmark: Option<String>,
    /// `[x, y, w, h]` viewport-relative CSS pixels (already adjusted for
    /// ancestor iframe offsets).
    #[serde(rename = "box")]
    pub box_: [f64; 4],
    /// Stability signature: `role|name|ordinal` where ordinal is the index of
    /// this element among same role+name elements in document order. Stable
    /// across re-renders that preserve the element set, even if DOM order of
    /// *other* elements shifts.
    pub sig: String,
    /// Frame path: list of iframe indices from the top document down.
    /// Empty `[]` = top document. `[0]` = first iframe in top doc.
    /// `[1, 0]` = first iframe inside the second iframe in top doc.
    #[serde(default)]
    pub frame: Vec<usize>,
    /// True if this element lives inside an open shadow root (W2). Debug aid;
    /// coordinate and sig handling are identical to light-DOM elements since
    /// shadow DOM does not create a new coordinate space (unlike iframes).
    #[serde(default)]
    pub shadow: bool,
    /// Structural identity fingerprint: FNV-1a hash over (ancestor chain +
    /// tag + first-3-child tags + type/name/testid attrs). Survives DOM
    /// re-renders that preserve structure even when text content changes
    /// (React/Vue/Angular re-renders, counter updates, live region swaps).
    /// Zero for scars from pre-fingerprint builds (backwards compat).
    #[serde(default)]
    pub fingerprint: u64,
}

/// The full capture: page identity + every actionable element.
#[derive(Debug, Clone, Deserialize)]
pub struct PageCapture {
    pub url: String,
    #[serde(default)]
    pub title: String,
    /// `document.readyState`: `loading` / `interactive` / `complete`.
    #[serde(default, rename = "readyState")]
    pub ready_state: String,
    /// DOM mutations observed since the previous capture (0 unless the
    /// mutation watcher was installed before an action). Lets verdicts
    /// detect effects on non-actionable content (text swaps, counters).
    #[serde(default)]
    pub muts: i64,
    #[serde(default)]
    pub elements: Vec<RawElement>,
}

impl PageCapture {
    /// A coarse page phase derived from `readyState` + element presence.
    /// Refined later by scene detection (modal/error awareness).
    pub fn phase(&self) -> &'static str {
        match self.ready_state.as_str() {
            "loading" => "loading",
            // "interactive" means DOMContentLoaded has fired — the DOM is
            // ready for interaction even if subresources (images, fonts)
            // are still loading. Treating it as "ready" avoids false
            // "loading" on SPAs that never reach "complete".
            "interactive" => "ready",
            _ => "ready",
        }
    }
}

// ---- shared JS fragments (used by both capture + find-by-sig scripts) ----

/// CSS selector for all potentially-actionable elements.
pub const JS_SELECTOR: &str = r#"a[href], button, input, select, textarea, summary, [contenteditable=""], [contenteditable="true"], [role="button"], [role="link"], [role="checkbox"], [role="radio"], [role="tab"], [role="menuitem"], [role="switch"], [role="textbox"], [role="combobox"], [onclick]"#;

/// Visibility check: returns false for zero-size, display:none, visibility:hidden, opacity:0.
pub const JS_VIS_FN: &str = r#"const vis=n=>{const r=n.getBoundingClientRect();if(r.width===0||r.height===0)return false;const s=getComputedStyle(n);if(s.display==='none'||s.visibility==='hidden'||s.opacity==='0')return false;return true;};"#;

/// Escapes a string for safe interpolation into a CSS attribute selector.
pub const JS_ESC_FN: &str = r#"const esc=s=>(s||'').replace(/["\\]/g,c=>'\\'+c);"#;

/// Resolves the nearest ancestor landmark role for an element.
/// Returns a short label (nav/main/banner/footer/aside/search/dialog) or
/// an aria-label for labelled regions, or null for the default content area.
pub const JS_LANDMARK_FN: &str = r#"const landmarkOf=n=>{let el=n;while(el&&el!==document.body&&el!==document.documentElement){const tag=el.tagName.toLowerCase();const role=el.getAttribute('role');if(tag==='nav'||role==='navigation')return'nav';if(tag==='main'||role==='main')return'main';if(tag==='header'||role==='banner')return'banner';if(tag==='footer'||role==='contentinfo')return'footer';if(tag==='aside'||role==='complementary')return'aside';if(role==='search')return'search';if(tag==='dialog'||role==='dialog'||role==='alertdialog')return'dialog';if(tag==='section'&&el.getAttribute('aria-label'))return el.getAttribute('aria-label').trim().slice(0,20);el=el.parentElement;}return null;};"#;

/// Infers semantic role from markup (tag + role attribute + input type).
pub const JS_ROLE_FN: &str = r#"const roleMap={button:'button',link:'link',checkbox:'checkbox',radio:'radio',tab:'tab',menuitem:'menuitem',switch:'switch',textbox:'textbox',combobox:'combobox',listbox:'listbox',option:'option'};function role(n){const a=n.getAttribute('role');if(a&&roleMap[a])return roleMap[a];const t=n.tagName.toLowerCase();if(t==='a')return'link';if(t==='button'||t==='summary')return'button';if(t==='select')return'combobox';if(t==='textarea'||n.isContentEditable)return'textbox';if(t==='input'){const ty=(n.type||'text').toLowerCase();if(ty==='checkbox')return'checkbox';if(ty==='radio')return'radio';if(ty==='submit'||ty==='button'||ty==='reset'||ty==='image')return'button';if(ty==='range')return'slider';if(ty==='file')return'file';if(ty==='color')return'color';if(ty==='hidden')return'hidden';return'textbox';}return'generic';}"#;

/// Resolves an element's accessible name through the full fallback chain.
/// `includeValue` controls whether the element's `value` is used as a last
/// resort — it should be `true` for display (the agent sees the current value)
/// but `false` for the stability signature (typing changes the value, which
/// must NOT change the element's identity).
// Label map cache, one per document, held in a WeakMap so nothing is added
// to the DOM (an expando would be a stealth tell). Built lazily on first use
// and reused — this removes the per-element `querySelector('label[for=…]')`
// that made name resolution O(n²) on id-heavy pages, which is what lets the
// capture compute names for ALL elements (needed for stable sigs) cheaply.
pub const JS_LABEL_CACHE: &str = r#"const __BLC=new WeakMap();function getLabels(doc){let m=__BLC.get(doc);if(!m){m={};try{for(const l of doc.querySelectorAll('label[for]')){const f=l.getAttribute('for');if(f&&!(f in m)){const t=(l.textContent||'').trim();if(t)m[f]=t.replace(/\s+/g,' ').slice(0,120);}}}catch(e){}__BLC.set(doc,m);}return m;}"#;

pub const JS_NAME_FN: &str = r#"function name(n,includeValue){const al=n.getAttribute('aria-label');if(al&&al.trim())return al.trim().replace(/\s+/g,' ').slice(0,120);const doc=n.ownerDocument;const lb=n.getAttribute('aria-labelledby');if(lb&&doc){const e=doc.getElementById(lb);if(e&&(e.textContent||' ').trim())return e.textContent.trim().replace(/\s+/g,' ').slice(0,120);}const id=n.id;if(id&&doc){const lt=getLabels(doc)[id];if(lt)return lt;}const cl=n.closest('label');if(cl&&(cl.textContent||' ').trim())return cl.textContent.trim().replace(/\s+/g,' ').slice(0,120);const ti=n.title;if(ti&&ti.trim())return ti.trim().slice(0,120);const ph=n.placeholder;if(ph&&ph.trim())return ph.trim().slice(0,120);const ac=n.getAttribute('autocomplete');if(ac&&ac.trim()){const tok=ac.trim().split(/\s+/).pop();if(tok&&tok!=='off'&&tok!=='on')return tok.replace(/-/g,' ').slice(0,80);}const ty=n.type;if(ty==='password')return'password';if(ty==='email')return'email';if(ty==='search')return'search';if(ty==='tel')return'phone';if(ty==='url')return'url';const nm=n.getAttribute('name');if(nm&&nm.trim()){const h=nm.trim().replace(/[_\-]/g,' ').replace(/\s+/g,' ').trim();if(h.length>1&&h.length<=60)return h.slice(0,60);}const tc=n.textContent;if(tc&&tc.trim())return tc.trim().replace(/\s+/g,' ').slice(0,120);const alt=n.getAttribute('alt');if(alt&&alt.trim())return alt.trim().slice(0,120);if(includeValue){const val=n.value;if(val&&typeof val==='string'&&val.trim()&&n.tagName==='INPUT')return val.trim().slice(0,60);}return'';}"#;

/// The shared preamble: just the selector + helper functions.
/// Each script (capture, find-by-sig) sets up its own document context,
/// node list, and counts — necessary because frame walking needs different
/// setup per use case.
pub static JS_PREAMBLE: LazyLock<String> = LazyLock::new(|| {
    "const sel='".to_string()
        + JS_SELECTOR
        + "';"
        + JS_VIS_FN
        + JS_ESC_FN
        + JS_ROLE_FN
        + JS_LABEL_CACHE
        + JS_NAME_FN
        + JS_LANDMARK_FN
        + JS_DEEP_ALL
        + JS_FNV_FN
        + JS_NC_FN
});

// Shadow-piercing collector (W2). `document.querySelectorAll(sel)` cannot see
// inside shadow roots, so actionable elements in open shadow trees (YouTube,
// Salesforce, most Web Component apps) were invisible. deepAll walks the light
// DOM and recurses into every open `shadowRoot`, returning a flat element list.
// Closed shadow roots are unreachable by JS (a hard platform restriction), but
// their host is still visible so coordinate clicks keep working. The ORDER is
// deterministic (light matches, then each shadow tree in host order), and every
// consumer (capture, find_by_sig, find_by_text, marks) uses deepAll, so per-name
// rank sigs stay consistent. Shadow elements share the top document's
// coordinate space, so no iframe-style offset is needed.
pub const JS_DEEP_ALL: &str = r#"function deepAll(root,sel){const out=[];const visit=(c)=>{const m=c.querySelectorAll(sel);for(let i=0;i<m.length;i++)out.push(m[i]);const all=c.querySelectorAll('*');for(let i=0;i<all.length;i++){const s=all[i].shadowRoot;if(s)visit(s);}};visit(root);return out;}"#;

/// FNV-1a 32-bit hash — fast, deterministic, good enough for identity.
/// Chosen over SHA/MD5 because it is pure arithmetic (no string lookup
/// tables), inlined into the capture loop (one pass over the input), and
/// 32 bits is sufficient since the sig ranks are the primary identity.
pub const JS_FNV_FN: &str = r#"function fnv(s){let h=0x811c9dc5;for(let i=0;i<s.length;i++){h^=s.charCodeAt(i);h=Math.imul(h,0x01000193)>>>0}return h}"#;

/// Ancestor chain string: `tag[index]>tag[index]>...` up to 10 levels.
/// Captures the element's structural position in the DOM so that a re-render
/// preserving structure (React/Vue) leaves the fingerprint unchanged even
/// when the element's text, class, or id mutates.
pub const JS_NC_FN: &str = r#"function nc(n){const c=[];let cur=n;for(let d=0;d<10&&cur;d++){c.push(cur.tagName?cur.tagName.toLowerCase():'unk');const p=cur.parentElement;if(p){c.push(''+Array.prototype.indexOf.call(p.children,cur));cur=p}else break}return c.join('>')}"#;

// ---- capture script ----

/// The injected capture script. One expression returning an object via
/// `returnByValue`. Walks same-origin iframes recursively, computing
/// viewport-relative coordinates by accumulating ancestor iframe offsets.
/// Cross-origin iframes are silently skipped (contentDocument throws).
static CAPTURE_SCRIPT: LazyLock<String> = LazyLock::new(|| {
    "(()=>{"
        .to_string()
        + "const d=document;if(!d||!d.body)return null;"
        + &JS_PREAMBLE
        + "const out=[];"
        // Viewport culling (V25c): only capture elements in or near the
        // viewport. An agent can only click what it can see; off-screen
        // elements are reachable via see find= or self-healing refs (click
        // scrolls into view). On link-dense pages (Wikipedia: thousands of
        // <a>) this cuts capture from ~3000 nodes to ~100 — the difference
        // between a 5.6s and a ~0.25s capture — AND shrinks the default
        // model (fewer tokens). Margin is generous to reduce scroll flap.
        + "const VH=window.innerHeight||800;const VM=Math.round(VH*1.5);"
        + "function cap(doc,fp,ox,oy){"
        // Sig scheme (V25c): sig = framePath | role | shortName | rank, where
        // rank is the element's ordinal among SAME-role+name elements in its
        // OWN frame, counted over ALL selector matches (visible or not,
        // on-screen or not). Because the rank is GLOBAL per frame and derived
        // from document order, it is stable across SCROLLS (document order
        // never changes) AND across insertions of differently-named elements
        // — the two properties viewport culling needs to never silently
        // rebind a ref to a different element (D25). We must compute
        // role+name for every element to get the rank; the label cache makes
        // that cheap. Only the EXPENSIVE parts (vis check, box serialization)
        // are culled to in-viewport elements. The frame prefix keeps sigs
        // unique across frames and matches find_by_sig.
        + "const fps=fp.join(',');"
        + "const all=deepAll(doc,sel);"
        + "const counts={};"
        + "for(let i=0;i<all.length;i++){"
        + "const n=all[i];"
        + "const r=role(n);if(r==='hidden')continue;"
        + "const snm=name(n,false);"
        + "const key=r+'\\u0000'+snm;counts[key]=(counts[key]||0)+1;const rank=counts[key];"
        // Cull: only serialize elements in or near the viewport. Off-screen
        // elements were still COUNTED above (stable rank) but skip the box +
        // vis work. Reachable via see find= or self-heal (click scrolls).
        + "const rect=n.getBoundingClientRect();"
        + "const ay=rect.y+oy;"
        + "if(ay+rect.height<-VM||ay>VH+VM)continue;"
        + "if(!vis(n))continue;"
        + "const nm=name(n,true);"
        // Structural identity fingerprint (D48): hash of ancestor chain +
        // tag + first children + identity attrs. Survives re-renders that
        // keep structure but change text/class values (React, Vue, Angular).
        // Computed here (in JS) so we don't serialize ancestor chains.
        + "const _kids=n.children&&n.children.length?Array.from(n.children).slice(0,3).map(c=>c.tagName.toLowerCase()).join(','):'';"
        + "const _cust=(n.type||'')+','+(n.name||'')+','+(n.getAttribute('data-testid')||'');"
        + "const _fp=fnv(nc(n)+','+n.tagName.toLowerCase()+','+_kids+'|'+_cust);"
        + "out.push({tag:n.tagName.toLowerCase(),role:r,name:nm,type:n.type||null,"
        + "value:n.value&&n.value.length<=200?n.value:null,"
        + "disabled:!!n.disabled,"
        + "checked:(r==='checkbox'||r==='radio')?!!n.checked:null,"
        + "href:n.href||null,placeholder:n.placeholder||null,"
        + "required:!!n.required||n.getAttribute('aria-required')==='true',"
        + "haspopup:!!n.getAttribute('aria-haspopup'),"
        + "landmark:landmarkOf(n),"
        + "box:[Math.round(rect.x+ox)||0,Math.round(rect.y+oy)||0,Math.round(rect.width)||0,Math.round(rect.height)||0],"
        + "sig:fps+'|'+r+'|'+snm+'|'+rank,"
        + "fingerprint:_fp,"
        + "shadow:n.getRootNode()!==doc,"
        + "frame:fp});"
        + "}"
        + "const ifs=doc.querySelectorAll('iframe');"
        + "for(let i=0;i<ifs.length;i++){"
        + "try{const fdoc=ifs[i].contentDocument;if(fdoc&&fdoc.body){"
        + "const ir=ifs[i].getBoundingClientRect();"
        + "cap(fdoc,[...fp,i],ox+ir.x,oy+ir.y);"
        + "}}catch(e){}}"
        + "}"
        + "cap(d,[],0,0);"
        + "const __m=window.__blade_muts||0;window.__blade_muts=0;"
        + "return{url:location.href,title:d.title||'',readyState:d.readyState,muts:__m,elements:out};"
        + "})()"
});

// ---- capture function ----

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
/// evaluate installs a MutationObserver and resolves after 600ms of DOM
/// quiet (or timeout). Replaces the Rust-side polling loop — on heavy-JS
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
                if(document.readyState!=='loading'&&(now-last)>=300){{fin('settled');return;}}\
                if((now-t0)>={ms}){{fin('timeout');return;}}\
                setTimeout(tick,150);\
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
        const GRACE: Duration = Duration::from_millis(1200);
        let hard_deadline = tokio::time::Instant::now() + timeout;
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
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
    }
    Ok(())
}

/// Run the capture script against `cdp` and parse the result.
///
/// Assumes `Runtime` is enabled (the [`Page`](super::Page) handle enables it on
/// attach). Returns the raw capture; ref stabilization + diffing happen later.
pub async fn capture(cdp: &CdpSession) -> Result<PageCapture> {
    let res = cdp
        .send(
            "Runtime.evaluate",
            Some(json!({
                "expression": &*CAPTURE_SCRIPT,
                "returnByValue": true,
                "awaitPromise": false,
            })),
        )
        .await?;

    // Runtime.evaluate → { result: { type, value }, exceptionDetails? }
    if let Some(exc) = res.get("exceptionDetails") {
        let msg = exc
            .get("exception")
            .and_then(|e| e.get("description"))
            .and_then(|d| d.as_str())
            .or_else(|| exc.get("text").and_then(|t| t.as_str()))
            .unwrap_or("unknown page exception");
        return Err(BladeError::Other(format!("page capture failed: {msg}")));
    }

    let value = res
        .get("result")
        .and_then(|r| r.get("value"));

    // The capture script returns null when the page isn't ready yet (no
    // document.body). Return an empty loading capture instead of erroring.
    match value {
        Some(serde_json::Value::Null) | None => Ok(PageCapture {
            url: String::new(),
            title: String::new(),
            ready_state: "loading".into(),
            muts: 0,
            elements: Vec::new(),
        }),
        Some(v) => {
            let mut parsed: PageCapture = serde_json::from_value(v.clone())?;
            // Chrome internal pages (chrome://, chrome-extension://, about:*
            // except about:blank) expose their own shadow-DOM UI, which the
            // W2 shadow piercing would otherwise turn into phantom actionable
            // elements (e.g. chrome://new-tab-page yields ~15). Automating a
            // browser-internal page is never the intent, so surface none.
            if is_internal_page(&parsed.url) {
                parsed.elements.clear();
            }
            Ok(parsed)
        }
    }
}

/// True for browser-internal pages whose own UI must not be captured as
/// actionable elements. `about:blank` is allowed (it is empty anyway).
fn is_internal_page(url: &str) -> bool {
    url.starts_with("chrome://")
        || url.starts_with("chrome-extension://")
        || url.starts_with("devtools://")
        || url.starts_with("edge://")
        || url.starts_with("about:") && url != "about:blank"
}

/// M4: Detect and dismiss consent/cookie banners. Policy: reject (default),
/// accept, or off (via BLADE_CONSENT env). Returns the framework name if dismissed.
pub async fn dismiss_consent(cdp: &CdpSession) -> Result<Option<String>> {
    let policy = std::env::var("BLADE_CONSENT").unwrap_or_else(|_| "reject".to_string());
    if policy == "off" {
        return Ok(None);
    }
    let reject = policy != "accept";
    let reject_js = if reject { "true" } else { "false" };

    let expression = "(()=>{const reject=".to_string()
        + reject_js
        + ";const rs=['#onetrust-reject-all-handler','#CybotCookiebotDialogBodyButtonDecline','#didomi-notice-disagree-button','.qc-cmp2-summary-buttons button[mode=secondary]','#truste-consent-reject'];"
        + "const as=['#onetrust-accept-btn-handler','#CybotCookiebotDialogBodyLevelButtonLevelOptinAllowAll','#didomi-notice-agree-button','.qc-cmp2-summary-buttons button[mode=primary]','#truste-consent-button'];"
        + "const sels=reject?rs:as;for(const sel of sels){const btn=document.querySelector(sel);if(btn){btn.click();return sel;}}"
        + "const dialogs=document.querySelectorAll('[role=dialog],[role=banner],[class*=cookie],[class*=consent],[id*=cookie],[id*=consent]');"
        + "for(const d of dialogs){const text=(d.textContent||'').toLowerCase();if(text.includes('cookie')||text.includes('consent')||text.includes('privacy')){const buttons=[...d.querySelectorAll('button,a')];const pattern=reject?/reject|decline|refuse|deny|necessary|essential/i:/accept|agree|allow|consent/i;const btn=buttons.find(b=>pattern.test(b.textContent||''));if(btn){btn.click();return 'generic';}}}"
        + "return null;})()";

    let res = cdp
        .send(
            "Runtime.evaluate",
            Some(json!({
                "expression": expression,
                "returnByValue": true,
            })),
        )
        .await?;

    if res.get("exceptionDetails").is_some() {
        return Ok(None);
    }
    let framework = res
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
        .map(String::from);
    Ok(framework)
}

/// Try a stored consent selector first (one cheap querySelector+click),
/// fall back to full [`dismiss_consent`] detection if it doesn't match.
///
/// This is the knowledge-base integration point: on known sites, a trusted
/// CSS selector (confidence >= 0.7) skips the full 20-line detection JS,
/// saving one large `Runtime.evaluate`. On unknown or changed sites, the
/// full detection runs as usual — zero regression for cold starts.
///
/// Returns the selector that was clicked (stored selector, new selector, or
/// `"generic"`), or `None` if no consent dialog was found.
pub async fn dismiss_consent_with_stored(
    cdp: &CdpSession,
    stored: Option<&str>,
) -> Result<Option<String>> {
    let policy =
        std::env::var("BLADE_CONSENT").unwrap_or_else(|_| "reject".to_string());
    if policy == "off" {
        return Ok(None);
    }

    // Try the stored selector first — skip the full detection JS if it works.
    if let Some(sel) = stored.filter(|s| !s.is_empty() && *s != "generic") {
        let sel_json = serde_json::to_string(sel).unwrap_or_default();
        let expr = format!(
            "(()={{const b=document.querySelector({sel_json});if(b){{b.click();return {sel_json};}}return null;}})()"
        );
        let res = cdp
            .send(
                "Runtime.evaluate",
                Some(json!({
                    "expression": expr,
                    "returnByValue": true,
                })),
            )
            .await?;
        if res.get("exceptionDetails").is_none() {
            if let Some(v) = res
                .get("result")
                .and_then(|r| r.get("value"))
                .and_then(|v| v.as_str())
            {
                if !v.is_empty() {
                    return Ok(Some(v.to_string()));
                }
            }
        }
    }

    // Fall through to full detection.
    dismiss_consent(cdp).await
}

/// M6: Detect block/challenge pages (Cloudflare, DataDome, PerimeterX, reCAPTCHA, Akamai).
/// Returns the block type if detected, or None.
/// S12: remediation ladder — actionable steps for each block type.
/// Appended to ambient events so the agent knows what to try next.
pub fn remediation_ladder(block_type: &str) -> Vec<String> {
    match block_type {
        "cloudflare" => vec![
            "wait 10s \u{2014} Turnstile often auto-passes non-interactively".into(),
            "see \u{2014} check for a visible checkbox, use act click x=X y=Y if found".into(),
            "if interactive challenge: solve manually or use coordinate click on checkbox".into(),
        ],
        "datadome" => vec![
            "blocked by DataDome (ML-based per-site detection)".into(),
            "try: navigate away, wait 30s, return (rate cooldown)".into(),
            "if captcha wall: needs external solver".into(),
        ],
        "perimeterx" => vec![
            "blocked by PerimeterX (behavioral analysis)".into(),
            "try: slower pacing, longer idle hum, more natural session".into(),
            "if captcha: needs external solver".into(),
        ],
        "recaptcha" => vec![
            "reCAPTCHA challenge (v3 score-based or v2 checkbox)".into(),
            "v3: improve score via longer session with human-like behavior".into(),
            "v2: click checkbox via act click x=X y=Y".into(),
        ],
        "akamai" => vec![
            "blocked by Akamai (IP reputation + TLS fingerprint)".into(),
            "try: BLADE_PROXY for a different IP, BLADE_TZ for matching timezone".into(),
        ],
        "rate-limit" => vec![
            "rate limited \u{2014} wait 30-60s before retrying".into(),
            "consider: BLADE_PROXY for a different IP".into(),
        ],
        _ => vec!["unknown block \u{2014} try waiting and retrying".into()],
    }
}

pub async fn detect_block(cdp: &CdpSession) -> Result<Option<String>> {
    let expression = r#"(()=>{const d=document;if(!d)return null;const t=(d.title||'').toLowerCase();const body=(d.body?d.body.textContent||'':'').toLowerCase().slice(0,2000);const q=s=>!!d.querySelector(s);if(t.includes('just a moment')||q('#challenge-form')||q('cf-turnstile')||q('script[src*=challenge-platform]'))return 'cloudflare';if(q('iframe[src*=captcha-delivery]')||body.includes('datadome'))return 'datadome';if(q('#px-captcha')||q('script[src*=px-captcha]'))return 'perimeterx';if(q('.g-recaptcha')||q('iframe[src*=recaptcha]'))return 'recaptcha';if(body.includes('access denied')&&(body.includes('reference')||body.includes('akamai')))return 'akamai';if(body.includes('too many requests')||body.includes('rate limit'))return 'rate-limit';return null;})()"#;

    let res = cdp
        .send(
            "Runtime.evaluate",
            Some(json!({
                "expression": expression,
                "returnByValue": true,
            })),
        )
        .await?;

    if res.get("exceptionDetails").is_some() {
        return Ok(None);
    }
    let block = res
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
        .map(String::from);
    Ok(block)
}

/// Extract visible text content from the page body, excluding scripts,
/// styles, and hidden elements. Returns at most `budget` characters.
///
/// Uses `innerText` which respects CSS visibility (unlike `textContent`).
/// The text is collapsed to single spaces and truncated to the budget.
pub async fn capture_content(cdp: &CdpSession, budget: usize) -> Result<String> {
    let expr = r#"(()=>{const d=document;if(!d||!d.body)return'';const c=d.body.cloneNode(true);c.querySelectorAll("script,style,noscript,svg,template,link,meta,[class*='dfp'],[id*='dfp'],[class*='advert'],[id*='advert'],[class*='sponsored'],[data-sponsored],[data-ad],[data-ad-slot],[data-ad-client],[data-google-query-id],ins.adsbygoogle,[id*='google_ads'],[class*='ad-container'],[class*='ad-wrapper'],[class*='ad-slot'],[class*='ad-banner'],[class*='ad-feedback'],[class*='adBanner'],[class*='adSense'],[class*='adBlock'],[class*='ad-label'],[class*='ads-label'],[class*='ads-container'],[class*='mol-ads'],[class*='promoted'],[aria-label*='advertisement' i]").forEach(e=>e.remove());const t=(c.innerText||c.textContent||'').replace(/\s+/g,' ').trim();return t.slice(0,__BUDGET__);})()"#.replace("__BUDGET__", &budget.to_string());
    let res = cdp
        .send(
            "Runtime.evaluate",
            Some(json!({
                "expression": expr,
                "returnByValue": true,
            })),
        )
        .await?;

    let text = res
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    Ok(text.to_string())
}

/// Semantic content extraction: find the main content area and convert it
/// to clean, token-efficient markdown. Strips navigation, footers, ads,
/// scripts, and other noise. Preserves headings, paragraphs, links, lists,
/// code blocks, tables, blockquotes, and images.
///
/// Main content detection: semantic HTML5 (<main>, <article>, [role=main])
/// → common content selectors → text density analysis (highest text-to-link
/// ratio). Layout tables (no <th>) are walked as content; data tables
/// (with <th>) are converted to markdown tables.
///
/// This is the `see mode=content` path: the agent gets clean markdown to
/// READ, not 9KB of ref IDs to parse. Designed for articles, docs, search
/// results — any page where the agent wants the text, not the interactive
/// elements.
pub async fn capture_markdown(cdp: &CdpSession, budget: usize) -> Result<String> {
    let expr = r#"(function(){var __isProd=(function(){var b=document.body;if(!b)return false;var t=(b.innerText||'').toLowerCase();var hp=/[$€£¥₹]\s?\d/.test(t);var cb=document.querySelector('#add-to-cart-button,#buy-now-button,[data-testid*="add-to-cart"],[data-testid*="buy-now"],button[name*="cart"],input[name*="cart"],#add-to-cart,#buy-now');var hc=/add to cart|buy now|add to basket|add to bag|in winkelwagen|au panier/i.test(t);var u=location.href.toLowerCase();var pu=/\/dp\/|\/gp\/product\/|\/product\/|\/itm\/|\/products\/|\/p\//.test(u);var h1=document.querySelector('h1');var hh=h1&&h1.innerText.trim().length>5;return hp&&hh&&(pu||cb||hc);})();if(__isProd){var h1=document.querySelector('h1');var title=h1?h1.innerText.trim():document.title;var md='# '+title+'\n\n';var pe=document.querySelector('#priceblock_ourprice,#priceblock_dealprice,.a-price .a-offscreen,[data-testid*="price"],[class*="price"]:not([class*="was"]):not([class*="original"]):not([class*="save"]),[id*="price"]:not([id*="was"]):not([id*="original"])');var price=null;if(pe){var pm=(pe.innerText||'').match(/[$€£¥₹]\s?\d[\d,]*(?:[.,]\d{1,2})?/);if(pm)price=pm[0];}if(!price){var pm2=(document.body.innerText||'').match(/[$€£¥₹]\s?\d[\d,]*(?:[.,]\d{1,2})?/);if(pm2)price=pm2[0];}if(price)md+='**Price:** '+price;var del=document.querySelector('del,s,[data-testid*="original"],[class*="was-price"],[class*="list-price"],[class*="original-price"]');if(del){var opm=(del.innerText||'').match(/[$€£¥₹]\s?\d[\d,]*(?:[.,]\d{1,2})?/);if(opm)md+=' ~~'+opm[0]+'~~';}if(price)md+='\n';var re=document.querySelector('[aria-label*="star"],[aria-label*="rating"],[data-testid*="rating"],[class*="rating"],[class*="star"],[data-hook*="rating"]');if(re){var al=re.getAttribute('aria-label')||'';var rm=al.match(/(\d+\.?\d*)\s*out of\s*\d+/i)||al.match(/(\d+\.?\d*)/)||(re.innerText||'').match(/(\d+\.?\d*)/);if(rm){md+='**Rating:** '+parseFloat(rm[1])+'/5';var rve=document.querySelector('#acrCustomerReviewText,[data-testid*="review-count"],[class*="review-count"],[data-hook*="review"]');if(rve){var rvm=(rve.innerText||'').match(/(\d[\d,]*)/);if(rvm)md+=' ('+rvm[1]+' ratings)';}md+='\n';}}var bt=(document.body.innerText||'').toLowerCase();if(/in stock|in-store only/.test(bt))md+='**Availability:** In Stock\n';else if(/out of stock|currently unavailable/.test(bt))md+='**Availability:** Out of Stock\n';else{var ol=bt.match(/only\s+(\d+)\s+left/i);if(ol)md+='**Availability:** Only '+ol[1]+' left\n';}var sec=document.querySelector('#feature-bullets,#productOverview_feature_div,#detailBullets_feature_div,[data-feature-name="productDescription"],#productDescription,#aplus,.product-facts-details,[data-testid="featureBullets"]')||(h1||pe||document.body).closest('section,div,main,[role="main"]')||document.body;if(sec){var bullets=sec.querySelectorAll('li,[role="listitem"],span.a-list-item');var fs=[];for(var i=0;i<bullets.length;i++){var bt2=(bullets[i].innerText||'').trim();if(bt2.length>10&&bt2.length<300&&fs.length<10&&!/add to cart|buy now|sign in|subscribe|follow|see more|show more/i.test(bt2))fs.push(bt2);}if(fs.length>0){md+='\n**Key Features:**\n';for(var j=0;j<fs.length;j++)md+='- '+fs[j]+'\n';}}var img=document.querySelector('#landingImage,#imgBlkFront,[data-testid*="product-image"],.product-image img,img[class*="product"]:not([src*="logo"]):not([src*="icon"]):not([src*="sprite"])');if(img&&img.src)md+='\n!['+(img.alt||'product')+']('+img.src+')\n';return md.slice(0,__BUDGET__);}var MAX=__BUDGET__;var SKIP=['SCRIPT','STYLE','NOSCRIPT','SVG','TEMPLATE','META','LINK','BUTTON','INPUT','SELECT','TEXTAREA'];var NOISE=['NAV','FOOTER','HEADER','ASIDE'];var __isRedditPost=(function(){var h=location.hostname;if(!h.includes('reddit.com'))return false;if(!location.href.includes('/comments/'))return false;var h1=document.querySelector('h1');return h1&&h1.innerText.trim().length>5;})();
if(__isRedditPost){var rh1=document.querySelector('h1');var rtitle=rh1?rh1.innerText.trim():document.title;var rmd='# '+rtitle+'\n\n';var rsub=(location.href.match(/\/r\/([\w-]+)/)||[])[1];var rau=document.querySelector('a[href*="/user/"]');var rauthor=rau?(rau.href.match(/\/user\/([\w-]+)/)||[])[1]:null;var rmeta=[];if(rsub)rmeta.push('**r/'+rsub+'**');if(rauthor)rmeta.push('**u/'+rauthor+'**');if(rmeta.length)rmd+=rmeta.join(' | ')+'\n\n';var rmain=findMain(document);if(rmain){var rads=rmain.querySelectorAll('[data-testid="ad"],[class*="promoted"],[class*="Promoted"]');rads.forEach(function(e){e.remove();});var rcb=Math.min(3000,__BUDGET__-rmd.length-100);var rc=toMd(rmain,rcb);if(rc.length>0)rmd+=rc;}return rmd.slice(0,__BUDGET__);}
var __isGithubRepo=(function(){if(location.hostname!=='github.com')return false;var p=location.pathname.split('/').filter(Boolean);if(p.length<2)return false;if(['search','trending','explore','login','signup','settings','notifications','pulls','issues','orgs','features','marketplace','pricing','about','customer-stories','sessions','collections','topics','sponsors','new','dashboard','stars','pull','commit'].includes(p[0]))return false;if(p.length===2)return true;if(p.length>2&&['tree','blob'].includes(p[2]))return true;return false;})();
if(__isGithubRepo){var gp=location.pathname.split('/').filter(Boolean);var gname=gp.length>=2?gp[0]+'/'+gp[1]:'Repository';var gmd='# '+gname+'\n\n';var gdesc=document.querySelector('p.f4,.f4.my-3,.BorderGrid-cell p,[data-pjax] p,[itemprop="about"]');if(!gdesc){var gdm=document.title.match(/:\s*(.+?)\s*·\s*GitHub/);if(gdm)gdesc={innerText:gdm[1]};}if(gdesc&&gdesc.innerText.trim().length>5)gmd+='**Description:** '+gdesc.innerText.trim()+'\n\n';var gsl=document.querySelector('a[href$="/stargazers"]');if(!gsl){var gsb=document.querySelector('button[aria-label*="star"],a[aria-label*="star"]');if(gsb){var gal=gsb.getAttribute('aria-label')||'';var gsm2=gal.match(/(\d[\d.,]*[KkMm]?)/);if(gsm2)gsl={innerText:gsm2[1]};}}var gfl=document.querySelector('a[href$="/forks"]');var gstats=[];if(gsl){var gsm=(gsl.innerText||'').trim();if(gsm)gstats.push('**Stars:** '+gsm);}if(gfl){var gfm=(gfl.innerText||'').trim();if(gfm)gstats.push('**Forks:** '+gfm);}if(gstats.length>0)gmd+=gstats.join(' | ')+'\n\n';var gtopics=document.querySelectorAll('.topic-tag,a[data-octo-dimensions]');if(gtopics.length>0){var gtl=[];for(var gi=0;gi<gtopics.length;gi++){var gt=(gtopics[gi].innerText||'').trim().replace(/^#/,'');if(gt.length>1)gtl.push(gt);}if(gtl.length>0)gmd+='**Topics:** '+gtl.join(', ')+'\n\n';}var greadme=document.querySelector('article.markdown-body,#readme .markdown-body,.markdown-body');if(greadme){var grt=toMd(greadme,Math.min(4000,__BUDGET__-gmd.length-50));if(grt.length>20)gmd+='## README\n\n'+grt;}return gmd.slice(0,__BUDGET__);}
function findMain(d){var m=d.querySelector('main,[role=\"main\"]');if(m&&(m.innerText||'').length>200)return m;var a=d.querySelector('article');if(a&&(a.innerText||'').length>200)return a;var sels=['#content','.content','#main-content','.main-content','.post','.article','.entry-content','.post-body','.article-body','.story-body','#article-body'];for(var i=0;i<sels.length;i++){var el=d.querySelector(sels[i]);if(el&&(el.innerText||'').length>200)return el;}var best=null,bs=0;var c=d.querySelectorAll('div,section,table');for(var j=0;j<c.length;j++){var el=c[j];var t=(el.innerText||'');if(t.length<200)continue;var l=el.querySelectorAll('a');var lt=0;for(var k=0;k<l.length;k++)lt+=(l[k].innerText||'').length;var sc=t.length-lt*2;if(sc>bs){bs=sc;best=el;}}return best||d.body;}function isAd(n){var cl=(typeof n.className==='string'?n.className:'').toLowerCase();var id=(n.id||'').toLowerCase();if(/dfp|advert|sponsored|ad-container|ad-wrapper|ad-slot|ad-banner|ad-feedback|adbanner|adsense|adblock|ad-label|ads-label|ads-container|mol-ads|promoted|adsbygoogle|google-ad|doubleclick|adfeedback/.test(cl))return true;if(/dfp|advert|sponsored|google_ads|doubleclick/.test(id))return true;if(n.hasAttribute('data-ad')||n.hasAttribute('data-ad-slot')||n.hasAttribute('data-ad-client')||n.hasAttribute('data-google-query-id'))return true;if(n.tagName==='INS'&&/adsbygoogle/.test(cl))return true;var al=(n.getAttribute('aria-label')||'').toLowerCase();if(/advertisement/.test(al))return true;return false;}function toMd(root,max){var out='',len=0;function add(s){if(len+s.length>max)s=s.slice(0,max-len);out+=s;len+=s.length;}function walk(n){if(len>=max)return;if(n.nodeType===3){var t=n.textContent.replace(/\s+/g,' ').trim();if(t)add(t+' ');return;}if(n.nodeType!==1)return;var tag=n.tagName;if(SKIP.indexOf(tag)>=0)return;if(n.hidden)return;var st=n.style;if(st&&(st.display==='none'||st.visibility==='hidden'))return;if(NOISE.indexOf(tag)>=0)return;if(isAd(n))return;switch(tag){case 'H1':add('\n# '+n.innerText.trim()+'\n\n');return;case 'H2':add('\n## '+n.innerText.trim()+'\n\n');return;case 'H3':add('\n### '+n.innerText.trim()+'\n\n');return;case 'H4':add('\n#### '+n.innerText.trim()+'\n\n');return;case 'H5':add('\n##### '+n.innerText.trim()+'\n\n');return;case 'H6':add('\n###### '+n.innerText.trim()+'\n\n');return;case 'P':for(var c=0;c<n.childNodes.length;c++)walk(n.childNodes[c]);add('\n\n');return;case 'A':var tx=n.innerText.trim();var hr=n.href;if(tx&&hr&&hr!=='#'&&hr.indexOf('javascript:')!==0){if(tx===hr)add(tx);else add('['+tx+']('+hr+')');}else if(tx)add(tx);return;case 'LI':add('- ');for(var c=0;c<n.childNodes.length;c++)walk(n.childNodes[c]);add('\n');return;case 'UL':case 'OL':for(var c=0;c<n.childNodes.length;c++)walk(n.childNodes[c]);add('\n');return;case 'CODE':if(n.parentElement&&n.parentElement.tagName==='PRE')return;add('`'+n.innerText+'`');return;case 'PRE':add('\n```\n'+n.innerText.trim()+'\n```\n\n');return;case 'BLOCKQUOTE':add('\n> '+n.innerText.trim().replace(/\n/g,'\n> ')+'\n\n');return;case 'IMG':var al=n.alt||'';var sr=n.src||'';if(al)add('!['+al+']('+sr+')');return;case 'TABLE':if(n.querySelector('th,thead')){var rows=n.querySelectorAll('tr');if(rows.length>0&&rows.length<50){for(var i=0;i<rows.length;i++){var cells=rows[i].querySelectorAll('th,td');var row=[];for(var j=0;j<cells.length;j++)row.push(cells[j].innerText.trim().replace(/\|/g,'\\|'));add('| '+row.join(' | ')+' |\n');if(i===0)add('|'+row.map(function(){return'---';}).join('|')+'|\n');}add('\n\n');return;}}for(var c=0;c<n.childNodes.length;c++)walk(n.childNodes[c]);add('\n');return;case 'BR':add('\n');return;case 'HR':add('\n---\n\n');return;case 'STRONG':case 'B':add('**');for(var c=0;c<n.childNodes.length;c++)walk(n.childNodes[c]);add('**');return;case 'EM':case 'I':add('*');for(var c=0;c<n.childNodes.length;c++)walk(n.childNodes[c]);add('*');return;case 'TR':case 'THEAD':case 'TBODY':case 'TFOOT':for(var c=0;c<n.childNodes.length;c++)walk(n.childNodes[c]);add('\n');return;case 'TD':case 'TH':for(var c=0;c<n.childNodes.length;c++)walk(n.childNodes[c]);add(' ');return;default:for(var c=0;c<n.childNodes.length;c++)walk(n.childNodes[c]);}}walk(root);return out.replace(/\n{3,}/g,'\n\n').trim();}var main=findMain(document);if(!main||!main.innerHTML)return'';var md=toMd(main,MAX);var fl=(main.innerText||'').length;if(fl>MAX)md+='\n\n[truncated: '+fl+' chars total, showed '+MAX+']';return md;})()"#.replace("__BUDGET__", &budget.to_string());
    let res = cdp
        .send(
            "Runtime.evaluate",
            Some(json!({
                "expression": expr,
                "returnByValue": true,
            })),
        )
        .await?;
    let text = res
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    Ok(text.to_string())
}

/// Outline extraction: return just the page title + heading hierarchy.
/// Ultra-minimal output for "what's on this page" without reading everything.
/// ~50-200 bytes typically. If no headings, suggests mode=content.
pub async fn capture_outline(cdp: &CdpSession) -> Result<String> {
    let expr = r#"(function(){var title=document.title||'';var hs=document.querySelectorAll('h1,h2,h3,h4,h5,h6');var out='';if(title)out+=title+'\n';if(!hs.length)return out+'(no headings — use see mode=content to read)';for(var i=0;i<hs.length;i++){var h=hs[i];var lvl=parseInt(h.tagName.charAt(1));var txt=h.innerText.trim();if(!txt)continue;for(var j=0;j<lvl-1;j++)out+='  ';out+=txt+'\n';}return out.trim();})()"#;
    let res = cdp
        .send(
            "Runtime.evaluate",
            Some(json!({
                "expression": expr,
                "returnByValue": true,
            })),
        )
        .await?;
    let text = res
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    Ok(text.to_string())
}
