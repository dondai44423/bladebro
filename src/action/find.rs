//! Element finding + model addressing.
//!
//! The finders take a text query, a CSS selector, or a stored signature and
//! return candidates with their LPM sigs; `resolve_ref` and `read_text` are
//! the ref→element utilities every action path uses. Misses carry
//! diagnostics (hidden/unaddressable/shadow counts) — see `find_miss_expr`.

use serde::Deserialize;
use serde_json::json;

use crate::cdp::CdpSession;
use crate::error::{BladeError, Result};
use crate::page::perception::JS_PREAMBLE;
use crate::page::LivePageModel;

use super::verdict::LEAF_TARGET_JS;

/// Result of the find-by-sig script: the element's current box + metadata.
#[derive(Debug, Deserialize)]
pub(super) struct FoundElement {
    pub(super) ok: bool,
    #[serde(default)]
    pub(super) reason: Option<String>,
    #[serde(default, rename = "box")]
    pub(super) box_: Option<[f64; 4]>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(super) tag: Option<String>,
    #[serde(default, rename = "type")]
    #[allow(dead_code)]
    pub(super) element_type: Option<String>,
    #[serde(default)]
    pub(super) disabled: Option<bool>,
    /// Whether the element is the topmost element at its center point.
    /// False when something (e.g. autocomplete overlay) covers it.
    #[serde(default, rename = "isTopmost")]
    pub(super) is_topmost: Option<bool>,
    /// Short accessible description (role "name") of the resolved click
    /// target. When leaf-targeting redirects a container-center click to
    /// the nearest exposed native control, this names the control actually
    /// clicked, so `no-effect` verdicts expose the real target.
    #[serde(default, rename = "hit_tgt")]
    pub(super) hit_tgt: Option<String>,
    /// When the element is NOT topmost at its click point: a description of
    /// the element that actually receives clicks there (or "outside the
    /// viewport"). Turns a silent no-op into "clicks land on X" - the
    /// occlusion diagnostic.
    #[serde(default, rename = "top_desc")]
    pub(super) top_desc: Option<String>,
    /// Text content of the element (for "read" mode). For "check"/"prepare"
    /// mode this is the readback of the EFFECTIVE editing host - the focused
    /// editor when the framework moved focus away from the addressed wrapper
    /// (a facade composer's real contenteditable), else the element itself.
    #[serde(default)]
    pub(super) text: Option<String>,
    /// "check"/"prepare": kind of the effective editing host
    /// ("ce" = contenteditable, "input", "textarea", ...).
    #[serde(default, rename = "hostKind")]
    pub(super) host_kind: Option<String>,
    /// "check"/"prepare": the effective host is the addressed element.
    #[serde(default, rename = "hostIsTgt")]
    pub(super) host_is_tgt: Option<bool>,
    /// "check"/"selhost": selected character count in the host.
    #[serde(default)]
    pub(super) sel: Option<u64>,
    /// "check"/"prepare": the addressed element is gone (framework
    /// remounted it); `text`/`hostKind` still describe the live editor.
    #[serde(default, rename = "tgtMissing")]
    pub(super) tgt_missing: Option<bool>,
    /// Live options of a select whose pick failed (#21): `text` or
    /// `text=value` tokens, capped at 80.
    #[serde(default)]
    pub(super) options: Option<Vec<String>>,
    /// Total option count on the live element at failure time.
    #[serde(default, rename = "ototal")]
    pub(super) options_total: Option<usize>,
    /// "focus" mode: whether the focus call actually landed on the resolved
    /// (possibly descendant) element. The space/enter activation rungs only
    /// press when this is true — a failed focus must not press keys into the
    /// page (Space would scroll it and read as a phantom effect).
    #[serde(default)]
    pub(super) focused: Option<bool>,
}

/// Resolve a ref to its (signature, frame path) in the LPM.
pub(super) fn resolve_ref(lpm: &LivePageModel, ref_id: &str) -> Result<(String, Vec<usize>)> {
    lpm.element(ref_id)
        .map(|e| (e.raw.sig.clone(), e.raw.frame.clone()))
        .ok_or_else(|| BladeError::StaleRef(format!(
            "{ref_id} \u{2014} page may have navigated. Use see or text addressing (act click text=\"...\")"
        )))
}

/// Read the text content of an element by its ref id.
/// Returns up to 5000 chars of innerText.
pub async fn read_text(cdp: &CdpSession, lpm: &LivePageModel, ref_id: &str) -> Result<String> {
    let (sig, frame) = resolve_ref(lpm, ref_id)?;
    let found = find_by_sig(cdp, &sig, &frame, "read", None).await?;
    if !found.ok {
        return Err(BladeError::ElementNotFound(format!("{ref_id} ({sig})")));
    }
    Ok(found.text.unwrap_or_default())
}

/// A text-addressing match result.
#[derive(Debug, Deserialize)]
pub struct TextMatch {
    pub sig: String,
    pub role: String,
    pub name: String,
    #[serde(default)]
    pub frame: Vec<usize>,
    pub score: i64,
    /// Selector addressing: the match exists but fails the visibility gate.
    /// Hidden matches are never addressed directly (a mouse cannot reach
    /// them), but they are returned so callers can say WHY a selector
    /// missed instead of reporting a bare "not found".
    #[serde(default)]
    pub hidden: bool,
    /// Why the match is hidden: `display:none (ancestor ...)`,
    /// `visibility:hidden`, `zero-size`, `not visible`.
    #[serde(default)]
    pub reason: String,
    /// Nearest distinguishing container chain for the match (e.g.
    /// `span.option.error < form.toggle.del-button`). Disambiguates
    /// identical names in multi-match errors and find output.
    #[serde(default)]
    pub ctx: String,
}

/// Build the find-by-text page script. `include_hidden` keeps invisible
/// matches in the results - the ref HEAL path needs them (a facade
/// composer's hidden wrapper resolves to its live editor downstream),
/// while text addressing and `see find` stay visible-only.
pub(super) fn find_text_expr(query: &str, role_filter: Option<&str>, include_hidden: bool) -> Result<String> {
    let query_js = serde_json::to_string(query)?;
    let role_js = match role_filter {
        Some(r) => serde_json::to_string(r)?,
        None => "null".to_string(),
    };
    let ih_js = if include_hidden { "true" } else { "false" };

    Ok("((query,rf,ih)=>{"
        .to_string()
        + "const d=document;if(!d||!d.body)return[];"
        + &JS_PREAMBLE
        + "const all=deepAll(d,sel);const results=[];const counts={};"
        + "const q=query.toLowerCase();"
        // Rank counted over ALL matches (vis-failing included, role-hidden
        // excluded) — identical to the capture script (V25c). Only vis-passing
        // matches are RETURNED, but the rank counts everything so sigs agree.
        // Frame prefix '' (main doc) + '|' matches capture's fps format.
        + "for(let i=0;i<all.length;i++){const n=all[i];"
        + "const r=role(n);if(r==='hidden')continue;"
        + "const snm=name(n,false);"
        + "const key=r+'\\u0000'+snm;counts[key]=(counts[key]||0)+1;"
        + "if(rf&&r!==rf)continue;"
        + "if(!vis(n)&&!ih)continue;"
        + "const sig='|'+r+'|'+snm+'|'+counts[key];"
        + "const nm=name(n,true);let score=0;"
        + "if(nm===query)score=100;else if(nm.toLowerCase()===q)score=80;"
        + "else if(nm.includes(query))score=70;else if(nm.toLowerCase().includes(q))score=60;"
        + "else{const al=n.getAttribute('aria-label')||'';const ph=n.placeholder||'';const ti=n.title||'';const alt=n.getAttribute('alt')||'';"
        + "if(al.toLowerCase()===q)score=50;else if(ph.toLowerCase()===q)score=45;"
        + "else if(ti.toLowerCase()===q)score=40;else if(alt.toLowerCase()===q)score=35;"
        + "else if(al.toLowerCase().includes(q))score=30;else if(ph.toLowerCase().includes(q))score=25;}"
        + "if(score>0)results.push({sig,score,role:r,name:nm,frame:[],ctx:ctxOf(n)});"
        + "}"
        + "results.sort((a,b)=>b.score-a.score);return results.slice(0,30);})"
        + "(" + &query_js + "," + &role_js + "," + ih_js + ")")
}

/// Find actionable elements by text/label query. Returns up to 30 matches
/// sorted by relevance score. Used by text addressing (M3): `act click text="Sign in"`
/// resolves the text to an element ref without a prior `see` call.
pub async fn find_by_text(
    cdp: &CdpSession,
    query: &str,
    role_filter: Option<&str>,
    include_hidden: bool,
) -> Result<Vec<TextMatch>> {
    if query.trim().is_empty() {
        return Err(BladeError::Other("find query must not be empty".into()));
    }
    let expression = find_text_expr(query, role_filter, include_hidden)?;

    let res = cdp
        .send(
            "Runtime.evaluate",
            Some(json!({
                "expression": expression,
                "returnByValue": true,
            })),
        )
        .await?;

    if let Some(exc) = res.get("exceptionDetails") {
        let msg = exc
            .get("exception")
            .and_then(|e| e.get("description"))
            .and_then(|d| d.as_str())
            .or_else(|| exc.get("text").and_then(|t| t.as_str()))
            .unwrap_or("unknown find-by-text error");
        return Err(BladeError::Other(format!("find-by-text failed: {msg}")));
    }

    let value = res
        .get("result")
        .and_then(|r| r.get("value"))
        .ok_or_else(|| BladeError::Other("find-by-text returned no value".to_string()))?;

    let matches: Vec<TextMatch> = serde_json::from_value(value.clone())?;
    Ok(matches)
}

/// Build the find-by-selector page script. The selector is matched with
/// `Element.matches()` against every ACTIONABLE element (the same deepAll
/// walk the capture uses, so open shadow roots are searched), and the
/// count/rank math is identical to the capture script - returned sigs are
/// directly adoptable into the model. Hidden matches come back flagged with
/// a reason instead of being dropped: a selector miss must be able to say
/// it found the element but could not reach it.
pub(super) fn find_selector_expr(selector: &str) -> Result<String> {
    let sel_js = serde_json::to_string(selector)?;
    Ok("((sel_user)=>{ "
        .to_string()
        + "const d=document;if(!d||!d.body)return{matches:[]};"
        + "try{d.querySelector(sel_user);}catch(e){return{invalid:String((e&&e.message)||e)};}"
        + &JS_PREAMBLE
        + "const _vrs=function(n){let e=n;for(let i=0;i<14&&e;i++){try{const cs=getComputedStyle(e);if(cs.display==='none')return 'display:none'+(e!==n?' (ancestor '+e.tagName.toLowerCase()+')':'');if(cs.visibility==='hidden')return 'visibility:hidden'+(e!==n?' (ancestor '+e.tagName.toLowerCase()+')':'');}catch(_e){}e=e.parentElement||(e.getRootNode&&e.getRootNode().host);}const r=n.getBoundingClientRect();if(!r.width||!r.height)return 'zero-size';return 'not visible';};"
        + "const all=deepAll(d,sel);const results=[];const counts={};"
        + "for(let i=0;i<all.length;i++){const n=all[i];"
        + "const r=role(n);if(r==='hidden')continue;"
        + "const snm=name(n,false);"
        + "const key=r+'\\u0000'+snm;counts[key]=(counts[key]||0)+1;"
        + "let m=false;try{m=!!(n.matches&&n.matches(sel_user));}catch(_e){m=false;}"
        // Composed-scope fallback (W4): CSS descendant combinators do not
        // cross shadow boundaries, so `host-scope[ident] inner-control`
        // never matches inside a component's shadow tree even though the
        // scope IS a composed ancestor. When the direct match fails and the
        // prefix is a single compound, match the last compound against the
        // element and the prefix against its composed ancestor chain
        // (reddit comment controls: `shreddit-comment[thingid=...]
        // button[aria-label=...]`).
        + "if(!m){try{var _si=sel_user;var _dep=0,_cut=-1;for(var _k=0;_k<_si.length;_k++){var _ch=_si[_k];if(_ch==='['||_ch==='(')_dep++;else if(_ch===']'||_ch===')'){if(_dep>0)_dep--;}else if(_ch===' '&&_dep===0)_cut=_k;}"
        + "if(_cut>0){var _last=_si.slice(_cut+1).trim();var _pref=_si.slice(0,_cut).trim();if(_last&&_pref&&/^[^\\s>+~,]+$/.test(_pref)&&n.matches&&n.matches(_last)){var _ca=n.parentElement||(n.getRootNode&&n.getRootNode().host);for(var _h=0;_h<40&&_ca;_h++){try{if(_ca.matches&&_ca.matches(_pref)){m=true;break;}}catch(_e2){}_ca=_ca.parentElement||(_ca.getRootNode&&_ca.getRootNode().host);}}}}catch(_e){}}"
        + "if(!m)continue;"
        + "const v=vis(n);"
        + "results.push({sig:'|'+r+'|'+snm+'|'+counts[key],score:0,role:r,name:name(n,true),frame:[],hidden:!v,reason:v?'':_vrs(n),ctx:ctxOf(n)});"
        + "}"
        + SELECTOR_DIAG_JS
        + "return{matches:results.slice(0,30),diag:diag};})("
        + &sel_js
        + ")")
}

/// Near-miss diagnostic for a selector that matched nothing actionable: the
/// raw light-DOM count for the full selector, plus the closest live matches
/// for progressively looser suffixes - so a miss on `… span.toggle.del-button
/// a.yes` reports the actual `a.yes` elements and their real containers (the
/// tag in the path may simply have changed). Raw string: JS regexes stay
/// unescaped.
pub(super) const SELECTOR_DIAG_JS: &str = r#"var diag=null;if(!results.length){try{
var rawN=0;try{rawN=d.querySelectorAll(sel_user).length;}catch(_e){}
var sub='',subCount=0,samples=[],rawSamples=[];
var parts=sel_user.trim().split(/\s*>\s*|\s+/);
for(var cut=1;cut<parts.length&&cut<=4&&!subCount;cut++){var s2=parts.slice(cut).join(' ');try{var ms=d.querySelectorAll(s2);if(ms.length){sub=s2;subCount=ms.length;for(var k=0;k<ms.length&&k<3;k++){var e2=ms[k];var t2=(e2.innerText||e2.textContent||'').trim().replace(/\s+/g,' ').slice(0,26);samples.push(e2.tagName.toLowerCase()+(t2?(' "'+t2+'"'):'')+(ctxOf(e2)?(' (in '+ctxOf(e2)+')'):''));}}}catch(_e){}}
if(rawN){try{var rm=d.querySelectorAll(sel_user);for(var k2=0;k2<rm.length&&k2<2;k2++){var e3=rm[k2];var t3=(e3.innerText||'').trim().replace(/\s+/g,' ').slice(0,26);rawSamples.push(e3.tagName.toLowerCase()+(t3?(' "'+t3+'"'):'')+(ctxOf(e3)?(' (in '+ctxOf(e3)+')'):''));}}catch(_e){}}
diag={raw:rawN,sub:sub,subCount:subCount,samples:samples,rawSamples:rawSamples};}catch(_e){diag=null;}}"#;

/// Near-miss diagnostic for a selector that found nothing actionable.
/// `raw` = light-DOM count for the full selector (non-actionable matches);
/// otherwise the closest live matches for the loosest suffix that matches.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct SelectorDiag {
    #[serde(default)]
    pub raw: usize,
    #[serde(default)]
    pub sub: String,
    #[serde(default, rename = "subCount")]
    pub sub_count: usize,
    #[serde(default)]
    pub samples: Vec<String>,
    #[serde(default, rename = "rawSamples")]
    pub raw_samples: Vec<String>,
}

/// Selector addressing lookup: the actionable matches plus - when nothing
/// matched at all - the near-miss diagnostic, so a miss explains itself
/// instead of reading as "not here".
pub struct SelectorLookup {
    pub matches: Vec<TextMatch>,
    pub diag: Option<SelectorDiag>,
}

/// Resolve a CSS selector to actionable elements (light DOM + open shadow
/// roots). Hidden matches are included (flagged) so callers can explain a
/// miss instead of reporting a bare "not found" - the reddit comment-menu
/// case, where the trigger exists but is desktop-hidden.
pub async fn find_by_selector(cdp: &CdpSession, selector: &str) -> Result<SelectorLookup> {
    if selector.trim().is_empty() {
        return Err(BladeError::Other("selector must not be empty".into()));
    }
    let expression = find_selector_expr(selector)?;
    let res = cdp
        .send(
            "Runtime.evaluate",
            Some(json!({
                "expression": expression,
                "returnByValue": true,
            })),
        )
        .await?;
    if let Some(exc) = res.get("exceptionDetails") {
        let msg = exc
            .get("exception")
            .and_then(|e| e.get("description"))
            .and_then(|d| d.as_str())
            .or_else(|| exc.get("text").and_then(|t| t.as_str()))
            .unwrap_or("unknown selector-match error");
        return Err(BladeError::Other(format!("selector match failed: {msg}")));
    }
    let value = res
        .get("result")
        .and_then(|r| r.get("value"))
        .ok_or_else(|| BladeError::Other("selector match returned no value".to_string()))?;
    if let Some(inv) = value.get("invalid").and_then(|v| v.as_str()) {
        return Err(BladeError::Other(format!("invalid selector \"{selector}\": {inv}")));
    }
    let matches: Vec<TextMatch> =
        serde_json::from_value(value.get("matches").cloned().unwrap_or_else(|| json!([])))?;
    let diag: Option<SelectorDiag> = value
        .get("diag")
        .filter(|d| d.is_object())
        .and_then(|d| serde_json::from_value(d.clone()).ok());
    Ok(SelectorLookup { matches, diag })
}

/// One hidden-match example from the find-miss diagnostic.
#[derive(Debug, Clone, Deserialize)]
pub struct MissExample {
    pub role: String,
    pub name: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub shadow: bool,
}

/// What a find-by-text miss left on the table: matches that exist but were
/// not returned (invisible), with reasons, and how many live in shadow
/// roots. Lets the miss message explain itself instead of reading as
/// "nothing here".
#[derive(Debug, Clone, Deserialize, Default)]
pub struct MissDiag {
    #[serde(default)]
    pub total: usize,
    #[serde(default)]
    pub hidden: usize,
    #[serde(default)]
    pub shadow: usize,
    #[serde(default)]
    pub examples: Vec<MissExample>,
}

/// Build the find-miss diagnostic script: the same name matching as
/// find_by_text, run over deepAll (open shadow roots included), counting
/// invisible matches with a reason for each.
pub(super) fn find_miss_expr(query: &str) -> Result<String> {
    let q_js = serde_json::to_string(query)?;
    Ok("((query)=>{ "
        .to_string()
        + "const d=document;if(!d||!d.body)return{total:0,hidden:0,shadow:0,examples:[]};"
        + &JS_PREAMBLE
        + "const _vrs=function(n){let e=n;for(let i=0;i<14&&e;i++){try{const cs=getComputedStyle(e);if(cs.display==='none')return 'display:none'+(e!==n?' (ancestor '+e.tagName.toLowerCase()+')':'');if(cs.visibility==='hidden')return 'visibility:hidden'+(e!==n?' (ancestor '+e.tagName.toLowerCase()+')':'');}catch(_e){}e=e.parentElement||(e.getRootNode&&e.getRootNode().host);}const r=n.getBoundingClientRect();if(!r.width||!r.height)return 'zero-size';return 'not visible';};"
        + "const q=query.toLowerCase();const all=deepAll(d,sel);"
        + "let total=0;let hidden=0;let shadow=0;const examples=[];"
        + "for(let i=0;i<all.length;i++){const n=all[i];"
        + "const r=role(n);if(r==='hidden')continue;"
        + "const nm=name(n,true);if(!nm)continue;"
        + "const nh=nm.toLowerCase();const al=(n.getAttribute('aria-label')||'').toLowerCase();const ph=(n.placeholder||'').toLowerCase();const ti=(n.title||'').toLowerCase();"
        + "if(!(nh.includes(q)||al.includes(q)||ph.includes(q)||ti.includes(q)))continue;"
        + "total++;const sh=n.getRootNode()!==d;"
        + "if(sh)shadow++;"
        + "if(!vis(n)){hidden++;if(examples.length<3)examples.push({role:r,name:nm,reason:_vrs(n),shadow:sh});}"
        + "}"
        + "return{total:total,hidden:hidden,shadow:shadow,examples:examples};})("
        + &q_js
        + ")")
}

/// Run [`find_miss_expr`] against the page. Returns zeroed diagnostics on
/// any transport hiccup - this is a best-effort explainer for a miss, never
/// a new failure of its own.
pub async fn find_miss_diag(cdp: &CdpSession, query: &str) -> Result<MissDiag> {
    if query.trim().is_empty() {
        return Ok(MissDiag::default());
    }
    let expression = find_miss_expr(query)?;
    let res = cdp
        .send(
            "Runtime.evaluate",
            Some(json!({
                "expression": expression,
                "returnByValue": true,
            })),
        )
        .await?;
    let value = res
        .get("result")
        .and_then(|r| r.get("value"))
        .cloned()
        .unwrap_or_default();
    if !value.is_object() {
        return Ok(MissDiag::default());
    }
    Ok(serde_json::from_value(value).unwrap_or_default())
}

/// Where a text query actually lives when no actionable element matched:
/// the deepest containing element (with its container chain from the page's
/// own markup) and the actionable elements inside that container. Lets a
/// text-present miss explain itself in one step instead of leaving the
/// agent to guess whether leftover body text means the thing survived
/// (the reddit header-pill false alarm; the "are you sure?" confirm row).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TextLocation {
    #[serde(default)]
    pub desc: String,
    #[serde(default)]
    pub near: Vec<String>,
    #[serde(default)]
    pub snippet: String,
}

pub(super) fn text_locate_expr(query: &str) -> Result<String> {
    let q_js = serde_json::to_string(query)?;
    Ok("((q)=>{".to_string()
        + "const d=document;if(!d||!d.body)return null;q=q.toLowerCase();"
        + &JS_PREAMBLE
        + TEXT_LOCATE_BODY
        + "})("
        + &q_js
        + ")")
}

/// Body of [`text_locate_expr`] (raw string: JS regexes stay unescaped).
pub(super) const TEXT_LOCATE_BODY: &str = r#"const cands=[];
const walk=(root)=>{for(const el of root.querySelectorAll('*')){let t='';try{t=el.innerText||'';}catch(_e){continue;}if(t&&t.toLowerCase().indexOf(q)>-1)cands.push(el);const s=el.shadowRoot;if(s)walk(s);}};
walk(d);
if(!cands.length)return null;
let deep=null;
for(const e of cands){let has=false;for(const c of cands){if(c!==e&&e.contains(c)){has=true;break;}}if(!has){deep=e;break;}}
if(!deep)deep=cands[0];
const did=deep.getAttribute?deep.getAttribute('id'):null;const own=did?('#'+String(did).slice(0,28)):((typeof deep.className==='string'&&deep.className.trim())?('.'+String(deep.className).trim().split(/\s+/).slice(0,2).join('.')):'');const desc=(deep.tagName?deep.tagName.toLowerCase():'')+own+(ctxOf(deep)?(' (in '+ctxOf(deep)+')'):'');
const near=[];let scope=deep;
for(let i=0;i<3&&scope;i++){
let acts=[];try{acts=[...scope.querySelectorAll('a[href],button,[role=button],[role=menuitem],input:not([type=hidden]),select,textarea')];}catch(_e){}
const vacts=acts.filter(function(a){const r=a.getBoundingClientRect();return r.width>0&&r.height>0;});
if(vacts.length){for(const a of vacts.slice(0,4)){const nm2=(a.getAttribute('aria-label')||a.innerText||a.value||'').trim().replace(/\s+/g,' ').slice(0,30);near.push(a.tagName.toLowerCase()+(nm2?(' "'+nm2+'"'):''));}break;}
scope=scope.parentElement||(scope.getRootNode&&scope.getRootNode().host);}
let snippet='';try{const bt=d.body.innerText||'';const i=bt.toLowerCase().indexOf(q);if(i>=0)snippet=bt.slice(Math.max(0,i-60),i+q.length+60).replace(/\s+/g,' ').trim();}catch(_e){}
return{desc,near,snippet};"#;

/// Run the text-locator against the page. Best-effort: any failure resolves
/// to None and the caller keeps its plain miss message.
pub async fn locate_text(cdp: &CdpSession, query: &str) -> Option<TextLocation> {
    if query.trim().is_empty() {
        return None;
    }
    let expression = text_locate_expr(query).ok()?;
    let res = cdp
        .send(
            "Runtime.evaluate",
            Some(json!({"expression": expression, "returnByValue": true})),
        )
        .await
        .ok()?;
    let v = res.get("result").and_then(|r| r.get("value")).cloned()?;
    if !v.is_object() {
        return None;
    }
    serde_json::from_value(v).ok()
}
/// Build the find-by-sig page script: re-locates an element by signature in
/// the live DOM and returns its box (`box`), performs an in-page action
/// (`prepare`/`focus`/`clear`/`type`/`select`/`click`), or reads it
/// (`check`/`read`/`hover`). Separate from `find_by_sig` so tests can
/// syntax-check it without a live browser.
pub(super) fn find_sig_expr(sig: &str, mode: &str, text: Option<&str>, frame: &[usize]) -> Result<String> {
    let sig_js = serde_json::to_string(sig)?;
    let mode_js = serde_json::to_string(mode)?;
    let text_js = match text {
        Some(t) => serde_json::to_string(t)?,
        None => "null".to_string(),
    };
    let frame_js = serde_json::to_string(frame)?;
    Ok("((sig,mode,text,frame)=>{".to_string()
        + "const d=document;if(!d||!d.body)return null;"
        + &JS_PREAMBLE
        + "let doc=d;let ox=0,oy=0;"
        + "for(let i=0;i<frame.length;i++){"
        + "const ifs=doc.querySelectorAll('iframe');const f=ifs[frame[i]];"
        + "if(!f)return{ok:false,reason:'frame gone'};"
        + "try{doc=f.contentDocument;if(!doc)return{ok:false,reason:'frame inaccessible'};}catch(e){return{ok:false,reason:'cross-origin'};}"
        + "const ir=f.getBoundingClientRect();ox+=ir.x;oy+=ir.y;}"
        + "const all=deepAll(doc,sel);"
        + "const fps=frame.join(',');const counts={};"
        // Editing-host helpers, shared by prepare/clear/check/selhost/type:
        // every input path resolves the EFFECTIVE editing host first - the
        // focused editor when a framework moved focus (facade composers),
        // else the addressed element or its inner editable field.
        + "var _ced=function(e){return !!(e&&(e.isContentEditable||(e.getAttribute&&e.getAttribute('contenteditable')==='true')));};"
        + "var _clr=function(e){try{var _s=window.getSelection();_s.selectAllChildren(e);document.execCommand('delete',false);}catch(_e){}e.dispatchEvent(new Event('input',{bubbles:true}));};"
        + "var _tvr=function(v){return String(v==null?'':v).replace(/\\s+/g,' ').trim();};"
        + "var _read=function(e){if(!e)return '';if('value' in e&&typeof e.value==='string')return _tvr(e.value);return _tvr(e.innerText||e.textContent||'');};"
        + "var _isd=function(e){return !!(e&&(_ced(e)||((e.tagName==='INPUT'||e.tagName==='TEXTAREA')&&e.type!=='hidden')));};"
        + "var _hostOf=function(t){var a=doc.activeElement;if(_isd(a))return a;if(t&&t.querySelector){var ii=_ced(t)?null:t.querySelector('textarea,input:not([type=hidden]),[contenteditable]:not([contenteditable=false])');if(ii)return ii;}return t;};"
        + "var _selLen=function(h){try{if(h&&('selectionStart' in h)&&'value' in h)return Math.abs((h.selectionEnd||0)-(h.selectionStart||0));var s=doc.getSelection&&doc.getSelection();return (s&&!s.isCollapsed)?String(s).length:0;}catch(_e){return 0;}};"
        + "var _nearish=function(n,a){if(!n||!a)return false;if(n.contains(a)||a.contains(n))return true;try{var f1=n.closest?n.closest('form'):null;if(f1&&f1===(a.closest?a.closest('form'):null))return true;}catch(_e){}var p=n.parentElement;for(var i=0;i<4&&p;i++){if(p.contains(a))return true;p=p.parentElement;}return false;};"
        + "var _hiddenHost=function(n){var a=doc.activeElement;if(a&&a!==n&&_isd(a)&&vis(a)&&_nearish(n,a))return a;var p=n.parentElement;for(var i=0;i<3&&p;i++){var q=p.querySelectorAll('[contenteditable]:not([contenteditable=false]),textarea,input:not([type=hidden])');for(var j=0;j<q.length;j++){var r=q[j];if(r!==n&&_isd(r)&&vis(r))return r;}p=p.parentElement;}return null;};"
        + "for(let i=0;i<all.length;i++){const n=all[i];"
        + "const r=role(n);if(r==='hidden')continue;"
        + "const nm=name(n,false);"
        + "const key=r+'\\u0000'+nm;counts[key]=(counts[key]||0)+1;"
        + "const s=fps+'|'+r+'|'+nm+'|'+counts[key];"
        + "if(s==="
        + &sig_js
        + "){if(!vis(n)){var _vd=(mode==='check'||mode==='prepare'||mode==='focus')?_hiddenHost(n):null;if(_vd){if(mode!=='check'){_vd.scrollIntoView({block:'center'});_vd.focus();}if(mode==='focus')return{ok:true};return{ok:true,text:_read(_vd),hostKind:(_ced(_vd)?'ce':((_vd.tagName)?_vd.tagName.toLowerCase():'')),hostIsTgt:false,sel:_selLen(_vd),tgtMissing:true};}return{ok:false,reason:'element hidden'};}const rect=n.getBoundingClientRect();"
        + "const cx=rect.x+rect.width/2;const cy=rect.y+rect.height/2;"
        + "const top=doc.elementFromPoint(cx,cy);"
        + "const isTopmost=top===n||n.contains(top);"
        + "var tgt=n;if(n.getAttribute&&n.getAttribute('role')==='combobox'&&n.tagName!=='SELECT'){var ii=n.querySelector('textarea,input:not([type=hidden])');if(ii)tgt=ii;}"
        + "if(mode==='box'){"
        + "function _lnb(e){return !!(e&&e.tagName&&(e.tagName==='BUTTON'||e.tagName==='A'||e.tagName==='SELECT'||e.tagName==='TEXTAREA'||(e.tagName==='INPUT'&&e.type!=='hidden'))&&(e.tagName!=='A'||!!e.href));}"
        + "function _lbx(e){var _lr=e.getBoundingClientRect();return [Math.round(_lr.x+ox)||0,Math.round(_lr.y+oy)||0,Math.round(_lr.width)||0,Math.round(_lr.height)||0];}"
        + LEAF_TARGET_JS
        + "var _lcbx=_lClick[0]+_lClick[2]/2,_lcby=_lClick[1]+_lClick[3]/2;var _lfc=doc.elementFromPoint(_lcbx-ox,_lcby-oy);"
        + "var _desc=function(e){if(!e)return null;var t=e.tagName.toLowerCase();var idd=(e.getAttribute&&e.getAttribute('id'))?('#'+e.getAttribute('id')):'';var cl=(typeof e.className==='string'&&e.className.trim())?('.'+e.className.trim().split(/\\s+/).slice(0,2).join('.')):'';var ala=(e.getAttribute&&(e.getAttribute('aria-label')||e.getAttribute('title')))||'';var tx=(e.textContent||'').replace(/\\s+/g,' ').trim().slice(0,30);return t+idd+cl+(ala?(' ['+ala+']'):(tx?(' \"'+tx+'\"'):''));};"
        // Composed-tree containment (W1): the hit is inside n's shadow
        // subtree, OR the hit is a composed ancestor HOST of n (a document
        // hit-test retargets to the host for content rendered in a shadow
        // tree). Both mean the point lands within n's own rendering scope;
        // reading them as occlusion sent clicks down the js-only path and
        // reddit's rpl-dropdown menu items silently died there.
        + "var _cmpAnc=function(a,x){var t=x;for(var i=0;i<40&&t;i++){if(t===a)return true;t=t.parentElement||(t.getRootNode&&t.getRootNode().host)||null;}return false;};"
        + "var _ltopOk=(_lfc===n)||_cmpAnc(n,_lfc)||_cmpAnc(_lfc,n);"
        + "var _ltop=_ltopOk?null:(_lfc?_desc(_lfc):'outside the viewport');"
        + "return{ok:true,box:_lClick,tag:n.tagName.toLowerCase(),type:n.type||null,disabled:!!n.disabled,isTopmost:_ltopOk,hit_tgt:(_lhr+' ['+String(_lht).slice(0,60)+']'),top_desc:_ltop};}"
        + "if(mode==='prepare'){tgt.scrollIntoView({block:'center'});tgt.focus();var _ph=_hostOf(tgt);const r3=tgt.getBoundingClientRect();return{ok:true,box:[Math.round(r3.x+ox)||0,Math.round(r3.y+oy)||0,Math.round(r3.width)||0,Math.round(r3.height)||0],disabled:!!tgt.disabled,text:_read(_ph),hostKind:(_ced(_ph)?'ce':((_ph&&_ph.tagName)?_ph.tagName.toLowerCase():'')),hostIsTgt:_ph===tgt,sel:_selLen(_ph),tgtMissing:false};}"
        + "if(mode==='focus'){var _fc=tgt;try{if(tgt.tabIndex<0){var _cd=tgt.querySelector('[tabindex],button,a[href],input,select,textarea');if(_cd)_fc=_cd;}}catch(_e){}try{_fc.focus();}catch(_e){}var _fo=false;try{_fo=(_fc.getRootNode().activeElement===_fc)||(document.activeElement===_fc);}catch(_e){}return{ok:true,focused:_fo};}"
        + "if(mode==='clear'){var _ch=_hostOf(tgt);if(!_ch)return{ok:false,reason:'no editable host'};if(_ced(_ch)){_clr(_ch);}else if('value' in _ch){var _cpr=_ch.tagName==='TEXTAREA'?window.HTMLTextAreaElement.prototype:window.HTMLInputElement.prototype;var _cd=Object.getOwnPropertyDescriptor(_cpr,'value');if(_cd&&_cd.set){_cd.set.call(_ch,'');}else{_ch.value='';}_ch.dispatchEvent(new Event('input',{bubbles:true}));_ch.dispatchEvent(new Event('change',{bubbles:true}));}else{return{ok:false,reason:'not an editable field'};}return{ok:true,text:_read(_ch)};}"
        // js-fallback click (W1): a bare n.click() fires ONE click event and
        // misses pointer-event components entirely (rpl dropdown items closed
        // their menu without running the item's handler). Dispatch the full
        // composed sequence on the deepest reachable target instead - the
        // same event shape a human click produces.
        + "if(mode==='click'){var _krc=n.getBoundingClientRect();var _kcx=_krc.x+_krc.width/2,_kcy=_krc.y+_krc.height/2;var _khit=null;try{_khit=doc.elementFromPoint(_kcx,_kcy);}catch(_e){}"
        + "var _ktgt=n;try{if(_khit&&_khit!==n&&(function(a,x){var t=x;for(var i=0;i<40&&t;i++){if(t===a)return true;t=t.parentElement||(t.getRootNode&&t.getRootNode().host)||null;}return false;})(n,_khit)){_ktgt=_khit;}}catch(_e){}"
        + "var _kmk=function(t){var o={bubbles:true,cancelable:true,composed:true,view:window,clientX:_kcx,clientY:_kcy,button:0};if(t.indexOf('pointer')===0){o.pointerId=1;o.pointerType='mouse';o.isPrimary=true;o.buttons=(t==='pointerdown')?1:0;try{return new PointerEvent(t,o);}catch(_e2){}}o.buttons=(t==='mousedown')?1:0;return new MouseEvent(t,o);};"
        + "['pointerdown','mousedown','pointerup','mouseup','click'].forEach(function(t){try{_ktgt.dispatchEvent(_kmk(t));}catch(_e3){}});"
        + "return{ok:true};}"
        + "if(mode==='check'){var _kh=_hostOf(tgt);return{ok:true,text:_read(_kh),hostKind:(_ced(_kh)?'ce':((_kh&&_kh.tagName)?_kh.tagName.toLowerCase():'')),hostIsTgt:_kh===tgt,sel:_selLen(_kh),tgtMissing:(tgt&&tgt.isConnected===false)?true:false};}"
        + "if(mode==='selhost'){var _sh=_hostOf(tgt);if(!_sh)return{ok:false,reason:'no editable host'};try{_sh.focus();if('setSelectionRange' in _sh&&'value' in _sh){_sh.setSelectionRange(0,(_sh.value||'').length);}else{var _s3=doc.getSelection();var _r3=doc.createRange();_r3.selectNodeContents(_sh);_s3.removeAllRanges();_s3.addRange(_r3);}}catch(_e){}return{ok:true,sel:_selLen(_sh)};}"
        + "if(mode==='type'){var _th=_hostOf(tgt)||tgt;if(!_th||_ced(_th)||!('value' in _th))return{ok:false,reason:'js-type only applies to value fields'};_th.focus();var _tx="
        + &text_js
        + ";var _tpr=_th.tagName==='TEXTAREA'?window.HTMLTextAreaElement.prototype:window.HTMLInputElement.prototype;var _tsd=Object.getOwnPropertyDescriptor(_tpr,'value');if(_tsd&&_tsd.set){_tsd.set.call(_th,_tx);}else{_th.value=_tx;}_th.dispatchEvent(new Event('input',{bubbles:true}));_th.dispatchEvent(new Event('change',{bubbles:true}));return{ok:true,text:_read(_th)};}"
        + "if(mode==='select'){"
        + "if(n.tagName==='SELECT'){"
        + "var opts=[...n.options];var match=opts.find(o=>o.value===" + &text_js + ")||opts.find(o=>o.text.trim()===" + &text_js + ")||opts.find(o=>o.text.trim().toLowerCase()===(" + &text_js + ").toLowerCase())||opts.find(o=>o.value.toLowerCase()===(" + &text_js + ").toLowerCase());"
        + "if(match){n.value=match.value;n.dispatchEvent(new Event('change',{bubbles:true}));n.dispatchEvent(new Event('input',{bubbles:true}));return{ok:true};}"
        + "var __lo=[...n.options].slice(0,80).map(o=>{var t=(o.label||o.text||'').trim().replace(/\\s+/g,' ').split('|').join('¦');var v=(o.value||'').trim().split('|').join('¦');if(t&&v&&t!==v)return t+'='+v;return t||(v?'='+v:'');}).filter(Boolean);return{ok:false,reason:'option not found in select',options:__lo,ototal:n.options.length};"
        + "}"
        + "n.value=" + &text_js + ";n.dispatchEvent(new Event('change',{bubbles:true}));n.dispatchEvent(new Event('input',{bubbles:true}));return{ok:true};}"
        + "if(mode==='read'){var txt=n.innerText||n.textContent||'';if(!txt&&('value' in n)&&n.value)txt=n.value;return{ok:true,text:txt.slice(0,5000)};}"
        + "if(mode==='hover'){n.scrollIntoView({block:'center'});const rect=n.getBoundingClientRect();const cx=rect.x+rect.width/2;const cy=rect.y+rect.height/2;const top=doc.elementFromPoint(cx,cy);const isTopmost=top===n||n.contains(top);return{ok:true,box:[Math.round(rect.x+ox)||0,Math.round(rect.y+oy)||0,Math.round(rect.width)||0,Math.round(rect.height)||0],isTopmost:isTopmost};}"
        + "return{ok:false,reason:'unknown mode'};}}"
        + "if(mode==='check'||mode==='prepare'){var _a3=doc.activeElement;var _h3=_isd(_a3)?_a3:null;if(_h3){if(mode==='prepare')_h3.focus();return{ok:true,text:_read(_h3),hostKind:(_ced(_h3)?'ce':((_h3.tagName)?_h3.tagName.toLowerCase():'')),hostIsTgt:false,sel:_selLen(_h3),tgtMissing:true};}}"
        + "return{ok:false,reason:'not found'};})("
        + &sig_js
        + ","
        + &mode_js
        + ","
        + &text_js
        + ","
        + &frame_js
        + ")")
}

/// Inject the find-by-sig script: re-locates the element by its signature in
/// the live DOM and either returns its box (mode "box") or performs an in-page
/// action (mode "focus", "clear", "select").
pub(super) async fn find_by_sig(
    cdp: &CdpSession,
    sig: &str,
    frame: &[usize],
    mode: &str,
    text: Option<&str>,
) -> Result<FoundElement> {
    let expression = find_sig_expr(sig, mode, text, frame)?;

    let res = cdp
        .send(
            "Runtime.evaluate",
            Some(json!({
                "expression": expression,
                "returnByValue": true,
            })),
        )
        .await?;

    if let Some(exc) = res.get("exceptionDetails") {
        let msg = exc
            .get("exception")
            .and_then(|e| e.get("description"))
            .and_then(|d| d.as_str())
            .or_else(|| exc.get("text").and_then(|t| t.as_str()))
            .unwrap_or("unknown find error");
        return Err(BladeError::Other(format!("find-by-sig failed: {msg}")));
    }

    let value = res
        .get("result")
        .and_then(|r| r.get("value"))
        .ok_or_else(|| BladeError::Other("find-by-sig returned no value".to_string()))?;

    let found: FoundElement = serde_json::from_value(value.clone())?;
    Ok(found)
}
