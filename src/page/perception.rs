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
//!
//! The page-side JS lives in `js/` — one file per `JS_*` const, embedded with
//! `include_str!`; the tests `node --check` every assembled script.
//!
//! Module map: this file is the capture core — the descriptor types
//! (`RawElement` / `SelectOptions` / `PageCapture`), the shared JS fragments,
//! the capture script and `capture`. Children: `wait` (load / DOM-quiet /
//! network-aware settles), `consent` (cookie-wall handling), `block`
//! (challenge detection + remediation ladder), `content` (text / markdown /
//! outline read modes).

use serde::Deserialize;
use serde_json::json;
use std::sync::LazyLock;
use std::time::Duration;

use crate::cdp::CdpSession;
use crate::error::{BladeError, Result};

mod block;
mod consent;
mod content;
mod wait;

pub use self::{
    block::{detect_block, remediation_ladder},
    consent::{dismiss_consent, dismiss_consent_with_stored},
    content::{capture_content, capture_markdown, capture_markdown_scoped, capture_outline},
    wait::{re_settle, wait_for_load, wait_for_settle, wait_for_settle_with_network},
};

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
    /// For `<select>` elements: the captured option list (visible labels +
    /// submitted values), so the agent can pick an option without guessing
    /// (#21). `None` for everything that is not a select.
    #[serde(default)]
    pub options: Option<SelectOptions>,
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
    /// Connected but NOT rendered yet: the empty-computed-style signature of
    /// a light-DOM child not assigned to any slot (site components mid-mount,
    /// e.g. reddit's composer). `box` is the nearest rendered ancestor's; the
    /// element is surfaced so the agent knows it is coming, but it cannot be
    /// clicked until it mounts.
    #[serde(default)]
    pub pending: bool,
    /// Nearest distinguishing container chain (`span.option.error <
    /// form.toggle.del-button`). Shown in filter/find output to disambiguate
    /// identical names; computed live, never part of the sig.
    #[serde(default)]
    pub ctx: String,
}

/// Options of a `<select>` element as captured for the agent (#21): the
/// visible labels plus the submitted values (which are never visible as page
/// text), so `act select` is a choice, not a guess.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct SelectOptions {
    /// Index of the currently selected option (-1 = none).
    #[serde(default)]
    pub sel: i64,
    /// Total option count on the live element (may exceed `items.len()` when
    /// the capture cap kicked in).
    #[serde(default)]
    pub total: usize,
    /// `(text, value)` pairs in DOM order, capped by the capture script.
    #[serde(default)]
    pub items: Vec<(String, String)>,
}

impl SelectOptions {
    /// Agent-facing option tokens: `text`, or `text=value` when the submitted
    /// value differs from the visible text; the selected option is prefixed
    /// `»`. Shows at most `max` options.
    ///
    /// Returns `(tokens, hidden)` — `hidden` is how many options exist beyond
    /// the ones shown.
    pub fn tokens(&self, max: usize) -> (Vec<String>, usize) {
        let mut out = Vec::new();
        for (i, (t, v)) in self.items.iter().take(max).enumerate() {
            let base = match (t.is_empty(), v.is_empty() || v == t) {
                (true, true) => continue, // nothing on either side to show
                (true, false) => format!("={v}"),
                (false, true) => t.clone(),
                (false, false) => format!("{t}={v}"),
            };
            out.push(if i as i64 == self.sel {
                format!("»{base}")
            } else {
                base
            });
        }
        let hidden = self.total.saturating_sub(out.len());
        (out, hidden)
    }
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
pub const JS_VIS_FN: &str = include_str!("js/vis_fn.js");

/// Escapes a string for safe interpolation into a CSS attribute selector.
pub const JS_ESC_FN: &str = include_str!("js/esc_fn.js");

/// Resolves the nearest ancestor landmark role for an element.
/// Returns a short label (nav/main/banner/footer/aside/search/dialog) or
/// an aria-label for labelled regions, or null for the default content area.
pub const JS_LANDMARK_FN: &str = include_str!("js/landmark_fn.js");

/// Infers semantic role from markup (tag + role attribute + input type).
pub const JS_ROLE_FN: &str = include_str!("js/role_fn.js");

// Label map cache, one per document, held in a WeakMap so nothing is added
// to the DOM (an expando would be a stealth tell). Built lazily on first use
// and reused — this removes the per-element `querySelector('label[for=…]')`
// that made name resolution O(n²) on id-heavy pages, which is what lets the
// capture compute names for ALL elements (needed for stable sigs) cheaply.
pub const JS_LABEL_CACHE: &str = include_str!("js/label_cache.js");

/// Resolves an element's accessible name through the full fallback chain.
/// `includeValue` controls whether the element's `value` is used as a last
/// resort — it should be `true` for display (the agent sees the current value)
/// but `false` for the stability signature (typing changes the value, which
/// must NOT change the element's identity).
pub const JS_NAME_FN: &str = include_str!("js/name_fn.js");

/// Nearest distinguishing container chain ("ctx"): up to two hops of
/// notable ancestors (data-*/id/aria-label/class), used to disambiguate
/// identical names - `a.yes` under `form.toggle.del-button` vs under
/// `form.toggle.sendreplies-button`. Rendered in filter/find output only;
/// the default model view stays lean. Composed-tree walk (crosses shadow
/// hosts), capped, best-effort.
pub const JS_CTX_FN: &str = include_str!("js/ctx_fn.js");

/// Bounded visible-tree traversal for reads/conditions, including same-origin
/// frames. Separate from deepAll: refs depend on per-document collection.
pub(crate) const JS_READ_TREE: &str = include_str!("js/read_tree.js");

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
        + JS_CTX_FN
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
pub const JS_DEEP_ALL: &str = include_str!("js/deep_all.js");

/// FNV-1a 32-bit hash — fast, deterministic, good enough for identity.
/// Chosen over SHA/MD5 because it is pure arithmetic (no string lookup
/// tables), inlined into the capture loop (one pass over the input), and
/// 32 bits is sufficient since the sig ranks are the primary identity.
pub const JS_FNV_FN: &str = include_str!("js/fnv_fn.js");

/// Ancestor chain string: `tag[index]>tag[index]>...` up to 10 levels.
/// Captures the element's structural position in the DOM so that a re-render
/// preserving structure (React/Vue) leaves the fingerprint unchanged even
/// when the element's text, class, or id mutates.
pub const JS_NC_FN: &str = include_str!("js/nc_fn.js");

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
        + "var pendN=0;"
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
        // Pending-mount capture (W3): an editable control that is CONNECTED
        // but not rendered - the empty-computed-style signature of a light
        // child not assigned to any slot (site components mid-hydration,
        // e.g. reddit's shreddit-composer). Anchor it to the nearest rendered
        // ancestor and surface it with pending:true so the agent knows the
        // control exists and is coming (it cannot be clicked yet). Capped:
        // the class is transient and must never flood the model.
        + "if(!vis(n)){"
        + "if(pendN<5&&r==='textbox'){try{var _pd='';try{_pd=getComputedStyle(n).display;}catch(_e){}"
        + "if(_pd===''){var _anc=n,_ab=null;for(var _ai=0;_ai<8&&_anc;_ai++){var _ar=null;try{_ar=_anc.getBoundingClientRect();}catch(_e2){}if(_ar&&_ar.width>0&&_ar.height>0){_ab=_ar;break;}_anc=_anc.parentElement||(_anc.getRootNode&&_anc.getRootNode().host)||null;}"
        + "if(_ab){var _ayp=_ab.y+oy;if(_ayp+_ab.height>=-VM&&_ayp<=VH+VM){var _pnm=name(n,true);if(_pnm){"
        + "var _pkids=n.children&&n.children.length?Array.from(n.children).slice(0,3).map(function(c){return c.tagName.toLowerCase();}).join(','):'';"
        + "var _pcst=(n.type||'')+','+(n.name||'')+','+(n.getAttribute('data-testid')||'');"
        + "var _pfp=fnv(nc(n)+','+n.tagName.toLowerCase()+','+_pkids+'|'+_pcst);pendN++;"
        + "out.push({tag:n.tagName.toLowerCase(),role:r,name:_pnm,type:n.type||null,value:null,disabled:false,checked:null,href:null,placeholder:n.placeholder||null,required:false,haspopup:false,landmark:landmarkOf(n),box:[Math.round(_ab.x+ox)||0,Math.round(_ab.y+oy)||0,Math.round(_ab.width)||0,Math.round(_ab.height)||0],sig:fps+'|'+r+'|'+snm+'|'+rank,fingerprint:_pfp,shadow:n.getRootNode()!==doc,frame:fp,ctx:ctxOf(n),pending:true});"
        + "}}}}}catch(_e3){}}"
        + "continue;}"
        + "const nm=name(n,true);"
        // Structural identity fingerprint (D48): hash of ancestor chain +
        // tag + first children + identity attrs. Survives re-renders that
        // keep structure but change text/class values (React, Vue, Angular).
        // Computed here (in JS) so we don't serialize ancestor chains.
        + "const _kids=n.children&&n.children.length?Array.from(n.children).slice(0,3).map(c=>c.tagName.toLowerCase()).join(','):'';"
        + "const _cust=(n.type||'')+','+(n.name||'')+','+(n.getAttribute('data-testid')||'');"
        + "const _fp=fnv(nc(n)+','+n.tagName.toLowerCase()+','+_kids+'|'+_cust);"
        + "out.push({tag:n.tagName.toLowerCase(),role:r,name:nm,type:n.type||null,"
        + "value:n.isContentEditable?((n.innerText||n.textContent||'').replace(/\\s+/g,' ').trim().slice(0,200)||null):(n.value&&n.value.length<=200?n.value:null),"
        + "disabled:!!n.disabled,"
        + "checked:(r==='checkbox'||r==='radio')?!!n.checked:null,"
        + "href:(function(){var h=n.getAttribute&&n.getAttribute('href');if(h==null)return null;if(h.charAt(0)==='#')return h;return n.href||null;})(),placeholder:n.placeholder||null,"
        + "options:n.tagName==='SELECT'?{sel:n.selectedIndex,total:n.options.length,items:[...n.options].slice(0,80).map(o=>[(o.label||o.text||'').trim().replace(/\\s+/g,' ').split('|').join('¦').slice(0,60),(o.value||'').split('|').join('¦').slice(0,40)]).filter(p=>p[0]||p[1])}:null,"
        + "required:!!n.required||n.getAttribute('aria-required')==='true',"
        + "haspopup:!!n.getAttribute('aria-haspopup'),"
        + "landmark:landmarkOf(n),"
        + "box:[Math.round(rect.x+ox)||0,Math.round(rect.y+oy)||0,Math.round(rect.width)||0,Math.round(rect.height)||0],"
        + "sig:fps+'|'+r+'|'+snm+'|'+rank,"
        + "fingerprint:_fp,"
        + "shadow:n.getRootNode()!==doc,"
        + "frame:fp,ctx:ctxOf(n),pending:false});"
        + "}"
        + "const ifs=doc.querySelectorAll('iframe');"
        + "for(let i=0;i<ifs.length;i++){"
        + "try{const fdoc=ifs[i].contentDocument;if(fdoc&&fdoc.body){"
        + "const ir=ifs[i].getBoundingClientRect();"
        + "cap(fdoc,[...fp,i],ox+ir.x,oy+ir.y);"
        + "}}catch(e){}}"
        + "}"
        + "cap(d,[],0,0);"
        + "const __mc=window[Symbol.for('m')];const __m=__mc?__mc.n:0;if(__mc)__mc.n=0;"
        + "return{url:location.href,title:d.title||'',readyState:d.readyState,muts:__m,elements:out};"
        + "})()"
});

// ---- capture function ----

/// Run the capture script against `cdp` and parse the result.
///
/// Assumes `Runtime` is enabled (the [`Page`](super::Page) handle enables it on
/// attach). Returns the raw capture; ref stabilization + diffing happen later.
pub async fn capture(cdp: &CdpSession) -> Result<PageCapture> {
    // Use a shorter timeout than the default 30s — if the execution
    // context is in transition (e.g. after a form-submit navigation to a
    // JSON response page), Runtime.evaluate can hang. Return a "loading"
    // capture on timeout instead of erroring after 30s.
    let res = cdp
        .send_with_timeout(
            "Runtime.evaluate",
            Some(json!({
                "expression": &*CAPTURE_SCRIPT,
                "returnByValue": true,
                "awaitPromise": false,
            })),
            Duration::from_secs(10),
        )
        .await;

    let res = match res {
        Ok(v) => v,
        Err(_) => {
            // Execution context not ready (page in transition).
            // Return a loading capture instead of erroring.
            return Ok(PageCapture {
                url: String::new(),
                title: String::new(),
                ready_state: "loading".into(),
                muts: 0,
                elements: Vec::new(),
            });
        }
    };

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

    let value = res.get("result").and_then(|r| r.get("value"));

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
/// actionable elements. `about:blank` — in ALL its URL shapes, fragments
/// and queries included (`about:blank#route`) — is an ordinary empty page:
/// agents inject their own DOM into it, so its elements must never be
/// wiped just because a same-document navigation added a fragment.
fn is_internal_page(url: &str) -> bool {
    url.starts_with("chrome://")
        || url.starts_with("chrome-extension://")
        || url.starts_with("devtools://")
        || url.starts_with("edge://")
        || (url.starts_with("about:") && !is_about_blank(url))
}

/// `about:blank` plus its same-document-navigation shapes (fragment/query).
fn is_about_blank(url: &str) -> bool {
    url == "about:blank" || url.starts_with("about:blank#") || url.starts_with("about:blank?")
}

#[cfg(test)]
mod internal_page_tests {
    use super::is_internal_page;

    #[test]
    fn about_blank_shapes_are_capturable() {
        // A pushState fragment on about:blank wiped every captured element
        // ("0 actionable" against a live DOM): all these shapes are
        // ordinary empty pages, never browser chrome.
        assert!(!is_internal_page("about:blank"));
        assert!(!is_internal_page("about:blank#route"));
        assert!(!is_internal_page("about:blank#/spa/route?q=1"));
        assert!(!is_internal_page("about:blank?q=1"));
        assert!(!is_internal_page("http://127.0.0.1/x"));
        assert!(!is_internal_page("https://example.com/#/"));
        assert!(!is_internal_page("file:///tmp/x.html"));
        assert!(!is_internal_page("data:text/html,<b>x</b>"));
    }

    #[test]
    fn browser_internal_pages_stay_uncapturable() {
        assert!(is_internal_page("chrome://newtab"));
        assert!(is_internal_page("chrome-extension://abc/x.html"));
        assert!(is_internal_page("devtools://devtools/x"));
        assert!(is_internal_page("edge://settings"));
        assert!(is_internal_page("about:srcdoc"));
        assert!(is_internal_page("about:newtab"));
    }
}

#[cfg(test)]
mod script_syntax_tests {
    //! Guards the injected page scripts against syntax errors. A broken
    //! capture script disables every page operation at once; `node --check`
    //! catches that at test time instead of live time. Skipped (with a
    //! notice) when node is not installed — the driver itself never needs it.

    use std::process::{Command, Stdio};

    fn node_check(name: &str, js: &str) {
        let has_node = Command::new("node")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !has_node {
            eprintln!("node not available — skipping {name} syntax check");
            return;
        }
        let path = std::env::temp_dir().join(format!("bladebro-js-check-{name}.js"));
        std::fs::write(&path, js).expect("write js fixture");
        let out = Command::new("node")
            .arg("--check")
            .arg(&path)
            .output()
            .expect("run node --check");
        let _ = std::fs::remove_file(&path);
        assert!(
            out.status.success(),
            "{name} has a JS syntax error:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Run a JS file with node; None (with a notice) when node is missing.
    fn node_exec(name: &str, js: &str) -> Option<std::process::Output> {
        let has_node = Command::new("node")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !has_node {
            eprintln!("node not available — skipping {name}");
            return None;
        }
        let path = std::env::temp_dir().join(format!("bladebro-js-run-{name}.js"));
        std::fs::write(&path, js).expect("write js fixture");
        let out = Command::new("node").arg(&path).output().expect("run node");
        let _ = std::fs::remove_file(&path);
        Some(out)
    }

    #[test]
    fn capture_script_is_valid_js() {
        node_check("capture", &super::CAPTURE_SCRIPT);
    }

    #[test]
    fn shadow_awareness_and_condition_scripts_are_valid_js() {
        // The mutation watcher now observes shadow roots and same-origin
        // iframes; the content/outline/condition probes are shadow-piercing
        // too. A syntax slip in any of them disables that layer at runtime,
        // so guard them all at test time.
        node_check("mut-watch", crate::action::MUT_WATCH);
        node_check("content-text", &super::content::content_expr(8000));
        node_check("outline", &super::content::outline_expr());
        node_check(
            "cond-element",
            &crate::action::element_condition_expr("sign in"),
        );
        node_check("cond-text", &crate::action::text_condition_expr("sign in"));
    }

    #[test]
    fn detect_block_script_is_valid_js() {
        node_check("detect-block", super::block::DETECT_BLOCK_SCRIPT);
    }

    #[test]
    fn markdown_scripts_are_valid_js() {
        // Whole-page and scoped markdown share ONE raw script (site branches,
        // findMain, toMd); a syntax slip would break every content read.
        node_check(
            "markdown",
            &super::content::markdown_expr(8000, None).expect("markdown expr builds"),
        );
        node_check(
            "markdown-scoped",
            &super::content::markdown_expr(3000, Some(("0|generic|Aside panel|1", &[0])))
                .expect("scoped markdown expr builds"),
        );
    }

    #[test]
    fn detect_block_rules_match_fixtures() {
        // Runs the REAL detector against fixture documents (stubbed DOM):
        // the reddit wall, the reddit challenge, Cloudflare, a normal page,
        // and a long prose page that MENTIONS the wall (must not classify).
        let src =
            serde_json::to_string(super::block::DETECT_BLOCK_SCRIPT).expect("serialize detector");
        let mut js = String::from("const S = ");
        js.push_str(&src);
        js.push_str(
            r#";
const cases = [
  ["wall", {t:"", x:"You've been blocked by network security. If you think you've been blocked by mistake, file a ticket below and we'll look into it. File a ticket", s:{}}, "reddit"],
  ["wall_whoa", {t:"", x:"Whoa there, pardner! Your request has been blocked due to a network policy.", s:{}}, "reddit"],
  ["challenge", {t:"Reddit", x:"", s:{"input[name=js_challenge]":1}}, "js-challenge"],
  ["challenge_token", {t:"Reddit", x:"", s:{"input[name=jsc_token]":1}}, "js-challenge"],
  ["cloudflare", {t:"Just a moment...", x:"Verifying you are human", s:{}}, "cloudflare"],
  ["normal", {t:"GitHub", x:"Lots of ordinary content", s:{}}, null],
  ["reddit_humanity", {t:"Reddit - Prove your humanity", x:"Prove your humanity We're committed to safety and security. But not for bots.", h:"www.reddit.com", s:{".g-recaptcha":1}}, "reddit-humanity"],
  ["reddit_humanity_other_host", {t:"Reddit - Prove your humanity", x:"Prove your humanity We're committed to safety and security. But not for bots.", h:"example.com", s:{".g-recaptcha":1}}, "recaptcha"],
  ["prose_mention", {t:"Blog", x:"blocked by network security ".repeat(120), s:{}}, null]
];
let fail = 0;
for (const [name, c, want] of cases) {
  globalThis.document = {
    title: c.t,
    body: { innerText: c.x, textContent: c.x },
    querySelectorAll: () => ({ length: 10 }),
    querySelector: (sel) => (c.s[sel] ? {} : null)
  };
  globalThis.location = { hostname: c.h || "example.com" };
  const got = (0, eval)(S);
  if (got !== want) { fail = 1; console.log("FAIL", name, "got", got, "want", want); }
}
if (!fail) console.log("detect_block fixtures pass");
process.exit(fail);
"#,
        );
        if let Some(out) = node_exec("detect-block-fixtures", &js) {
            assert!(
                out.status.success(),
                "detect_block fixture failures:\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}
