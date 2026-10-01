(function(){
var S=__SEED__;
function R(){S^=S<<13;S^=S>>>17;S^=S<<5;return((S>>>0)%256);}
var cn=R()&1,an=R(),cn2=R()&1;

__PROXY_HELPERS__

// cdc_ residue removal (chromedriver artifact — belt and suspenders).
var p=Object.getOwnPropertyNames(document).concat(Object.getOwnPropertyNames(window));
for(var i=0;i<p.length;i++){if(p[i].indexOf('cdc_')===0){try{delete document[p[i]];delete window[p[i]];}catch(e){}}}

// Window outer dims. Xvfb has no window manager, so the native outerWidth
// collapses to innerWidth (a 0 diff is a bot tell: real desktops reserve
// side chrome). Modern Chrome (129+) exposes outerWidth as an OWN
// configurable accessor on the window INSTANCE — a Window.prototype
// override is shadowed and never read. Only correct the tell: when the diff
// is already positive (real WM), the natural value is coherent and stays
// native (coherence over noise, D14). outerHeight is left native: its
// natural diff (title/tab bar, here ~143px) is realistic.
try{
var _w0=window.outerWidth,_i0=window.innerWidth;
if(_w0-_i0<=0){
var _mow=function(th,a,og){og.apply(th,a);return window.innerWidth+16;};
var _ow=Object.getOwnPropertyDescriptor(window,'outerWidth');
if(_ow&&_ow.configurable&&_ow.get){_defGet(window,'outerWidth',_ow.get,_mow);}
else{var _wp=(typeof Window!=='undefined'&&Window.prototype)?Window.prototype:Object.getPrototypeOf(window);_defGet(_wp,'outerWidth',_ogs(_wp,'outerWidth'),_mow);}
}
}catch(e){}

// Screen geometry on Screen.prototype (real location, not the instance).
// Only the *incoherent* case is corrected: a window manager gives the screen
// a real work area (availHeight < height) and that native value then stays
// untouched (D14 — coherence over noise). availWidth is left native always:
// a horizontal panel keeps it equal to width, and a narrow one is an honest
// side dock — masking it would hide a real desk, not a tell.
try{
var _sp=(typeof Screen!=='undefined'&&Screen.prototype)?Screen.prototype:Object.getPrototypeOf(screen);
if(screen.height-screen.availHeight<16){_defGet(_sp,'availHeight',_ogs(_sp,'availHeight'),function(th,a,og){var v=og.apply(th,a);return (screen.height-v<16)?(screen.height-40):v;});}
}catch(e){}

// permissions.query rewrite lives in PERMISSIONS_PATCH (apply()): a
// perfect native relay; only a real-origin 'denied' notifications result
// is rewritten (the headless tell). Opaque origins keep native results.

// Web API polyfills (Linux-headless absence signals). Polyfill targets use
// the shared bound placeholder, so a polyfilled method is shaped exactly
// like a native one (no 'prototype', {length,name}, native toString).
try{
var _np=(typeof Navigator!=='undefined'&&Navigator.prototype)?Navigator.prototype:Object.getPrototypeOf(navigator);
if(!('share' in navigator)){_defFnN(_np,'share',function(){return Promise.reject(new TypeError('Not supported'));},1);}
if(!('canShare' in navigator)){_defFnN(_np,'canShare',function(){return false;},1);}
if(!('ContentIndex' in window)){_defCtor('ContentIndex',function(){throw new TypeError('Illegal constructor');});}
if(!('ContactsManager' in window)){_defCtor('ContactsManager',function(){throw new TypeError('Illegal constructor');});}
if(navigator.connection){
  var _pr=Object.getPrototypeOf(navigator.connection);
  if(_pr&&!('downlinkMax' in _pr)){_defGetN(_pr,'downlinkMax',function(){return Infinity;});}
  if(typeof window.NetworkInformation==='undefined'){try{Object.defineProperty(window,'NetworkInformation',{value:navigator.connection.constructor,writable:true,configurable:true,enumerable:false});}catch(e){}}
}
}catch(e){}

// ── Additional stealth layers ───────────────────────────────

// window.chrome object: stock Chromium exposes chrome.app, chrome.csi(),
// chrome.loadTimes() — verified headful AND headless via the differential
// oracle. Polyfill only what is genuinely missing; chrome.runtime is NOT
// exposed by this engine, so adding it was a deviation, not coverage
// (removed v3.9.12; re-check with tools/diff_oracle when the engine moves).
try{
if(!window.chrome){window.chrome={};}
if(!window.chrome.app){window.chrome.app={isInstalled:false,InstallState:{DISABLED:'disabled',INSTALLED:'installed',NOT_INSTALLED:'not_installed'},RunningState:{CANNOT_RUN:'cannot_run',READY_TO_RUN:'ready_to_run',RUNNING:'running'}};}
if(!window.chrome.csi){window.chrome.csi=_fix(_mkN(function(){return{startE:Date.now(),onloadT:Date.now()+100,pageT:1000,tran:15};}),'csi',0);}
if(!window.chrome.loadTimes){window.chrome.loadTimes=_fix(_mkN(function(){return{commitLoadTime:Date.now()/1000,connectionInfo:'h2',finishDocumentLoadTime:Date.now()/1000+0.1,finishLoadTime:Date.now()/1000+0.2,firstPaintAfterLoadTime:0,firstPaintTime:Date.now()/1000+0.05,navigationType:'Other',npnNegotiatedProtocol:'h2',requestTime:Date.now()/1000-0.5,startLoadTime:Date.now()/1000-0.5,wasAlternateProtocolAvailable:false,wasFetchedViaSpdy:true,wasNpnNegotiated:true};}),'loadTimes',0);}
}catch(e){}

// Speech synthesis voices: NO PATCH (removed v3.9.12). The fixed 3-voice
// Google set was a cluster signature shared by every Bladebro profile;
// the machine truth (0 voices without speech-dispatcher, the real list
// with it) is coherent and unremarkable — the differential oracle shows
// stock Chrome on the same box reports 0.

// Battery API: headless may not have navigator.getBattery.
try{
if(!navigator.getBattery){
  _defFnN(Navigator.prototype,'getBattery',function(){return Promise.resolve({charging:true,chargingTime:0,dischargingTime:Infinity,level:1,onchargingchange:null,onchargingtimechange:null,ondischargingtimechange:null,onlevelchange:null});},0);
}
}catch(e){}

// WebRTC ICE/SDP filtering moved OUT of the core (v3.9): modern Chrome
// already replaces host candidates with mDNS names, and the
// --force-webrtc-ip-handling-policy launch flag handles the network
// layer. Stripping srflx/host candidates from every session BREAKS
// legit WebRTC (empty candidate lists are themselves a detection
// surface) for zero benefit without a proxy. The patch is now applied
// ONLY when BLADE_PROXY is set — the one case where the real IP must
// not appear in candidates. See RTC_PATCH in apply().

// Error-stack normalization REMOVED (v3.9): the wrapper broke every
// `class X extends Error` subclass (instanceof failed), forced early
// stack materialization (defeating page-set prepareStackTrace /
// stackTraceLimit), and copied stackTraceLimit by value. On headful
// Xvfb Chrome, page errors never contain devtools/chrome-extension
// frames anyway — the patch bought nothing at real functional cost.

// Document.visibilityState: should be 'visible' in headful mode.
// Some headless configurations report 'hidden' or 'prerender'.
// Defined on Document.prototype (the real WebIDL location) — an own
// property on the document instance is a descriptor-shape tell.
try{
if(document.visibilityState!=='visible'){
  var _dp=(typeof Document!=='undefined'&&Document.prototype)?Document.prototype:Object.getPrototypeOf(document);
  _defGet(_dp,'visibilityState',_ogs(_dp,'visibilityState'),function(th,a,og){og.apply(th,a);return 'visible';});
  _defGet(_dp,'hidden',_ogs(_dp,'hidden'),function(th,a,og){og.apply(th,a);return false;});
}
}catch(e){}

// performance.timing polyfill REMOVED (v3.9): the fabricated timeline
// was internally incoherent (navigationStart !== performance.timeOrigin,
// loadEventEnd timestamped before load). Modern Chrome's absence or
// presence of performance.timing is what a real Chrome of the same
// version shows — polyfilling it was the anomaly.

// Notification.permission: should be 'default' (not 'denied').
try{
if(typeof Notification!=='undefined'&&Notification.permission==='denied'){
  _defGet(Notification,'permission',_ogs(Notification,'permission'),function(th,a,og){og.apply(th,a);return 'default';});
}
}catch(e){}

// navigator.pdfViewerEnabled: real Chrome has this as true.
// --disable-features=PdfPlugin makes it false (for download support),
// so patch it back to true to maintain the fingerprint.
try{
if(navigator.pdfViewerEnabled===false){
  _defGet(Navigator.prototype,'pdfViewerEnabled',_ogs(Navigator.prototype,'pdfViewerEnabled'),function(th,a,og){og.apply(th,a);return true;});
}
}catch(e){}

// navigator.presentation: REMOVED — adding a fake {} via _defFn creates
// a detectable data descriptor. Real Chrome only exposes this on HTTPS.
// Missing it is normal and less suspicious than a wrong-shaped object.

// navigator.connection: more realistic values for a broadband connection.
// Headless may report unrealistic values or missing properties.
try{
if(navigator.connection){
  var _cp=Object.getPrototypeOf(navigator.connection);
  if(_cp){
    if(!('effectiveType' in _cp))_defGetN(_cp,'effectiveType',function(){return '4g';});
    if(!('rtt' in _cp))_defGetN(_cp,'rtt',function(){return 50;});
    if(!('downlink' in _cp))_defGetN(_cp,'downlink',function(){return 10;});
    if(!('saveData' in _cp))_defGetN(_cp,'saveData',function(){return false;});
  }
}
}catch(e){}

// navigator.scheduling: Chrome has the Scheduling API.
// Use getter (not data property) — real Chrome exposes it as an accessor.
try{
if(!navigator.scheduling&&!('scheduling' in navigator)){
  var _sch={isInputPending:_fix(_mkN(function(){return false;}),'isInputPending',0),isInputPendingOrAvailable:_fix(_mkN(function(){return false;}),'isInputPendingOrAvailable',0)};
  _defGetN(Navigator.prototype,'scheduling',function(){return _sch;});
}
}catch(e){}

// navigator.cookieEnabled: should be true (Chrome default).
try{
if(!navigator.cookieEnabled){
  _defGet(Navigator.prototype,'cookieEnabled',_ogs(Navigator.prototype,'cookieEnabled'),function(th,a,og){og.apply(th,a);return true;});
}
}catch(e){}

try{
var _sp2=(typeof Screen!=='undefined'&&Screen.prototype)?Screen.prototype:Object.getPrototypeOf(screen);
if(screen.colorDepth!==24){_defGet(_sp2,'colorDepth',_ogs(_sp2,'colorDepth'),function(th,a,og){og.apply(th,a);return 24;});}
if(screen.pixelDepth!==24){_defGet(_sp2,'pixelDepth',_ogs(_sp2,'pixelDepth'),function(th,a,og){og.apply(th,a);return 24;});}
}catch(e){}

// V8: console capture for driver introspection (see logs=console).
// Chrome 151 owns the console methods on the INSTANCE (Console.prototype has
// no 'log' — verified against stock), so install where the method actually
// lives: adding a prototype property stock doesn't have is itself a
// differential. The hook is a proxy over the native method — masked in every
// realm, native receiver/argument semantics preserved. The ring buffer lives
// under a Symbol-keyed NON-ENUMERABLE window slot: invisible to for-in,
// Object.keys, and getOwnPropertyNames. 200 entries, resets per document.
try{
var _uk=Symbol.for('q');
var _uxa=[];
function _uxp(l,a){try{var p=[];for(var i=0;i<a.length;i++){var v=a[i];try{p.push(typeof v==='string'?v:JSON.stringify(v));}catch(e){p.push(String(v));}}_uxa.push({l:l,m:p.join(' ').slice(0,400),t:Date.now()});if(_uxa.length>200)_uxa.shift();}catch(e){}}
['log','info','warn','error','debug'].forEach(function(m){
  var _cp=Object.getPrototypeOf(console);
  var _own=Object.getOwnPropertyDescriptor(console,m);
  var _host=_own?console:_cp;
  var _o=_own?_own.value:_cp[m];
  if(typeof _o!=='function')return;
  try{Object.defineProperty(_host,m,{value:_fix(_mk(_o,function(th,a,og){_uxp(m,a);return og.apply(console,a);}),m,_o.length),writable:true,configurable:true,enumerable:true});}catch(e){}
});
window.addEventListener('error',function(e){_uxp('exception',[String(e.message||'')+' @'+String(e.filename||'')+':'+String(e.lineno||'')]);});
window.addEventListener('unhandledrejection',function(e){_uxp('unhandledrejection',[String(e.reason)]);});
try{Object.defineProperty(window,_uk,{value:_uxa,writable:true,configurable:true,enumerable:false});}catch(e){try{window[_uk]=_uxa;}catch(e2){}}

// Issue #9: macOS shows a system dialog "Chrome Helper needs to download
// the font 'Osaka'/'STHeiti'" when on-demand CJK fonts are referenced in
// page CSS but not installed. We inject a stylesheet that defines
// @font-face aliases mapping these font names to system fonts that are
// always available. This intercepts the font request before it reaches
// macOS's CoreText download system. --disable-remote-fonts (launch flag)
// handles the web-font side; this handles the CSS font-family side.
// Note: addScriptToEvaluateOnNewDocument runs before HTML parsing, so
// documentElement/head may be null. Create the element now, append when
// the DOM is ready.
try{
var _fs=document.createElement('style');
_fs.textContent="@font-face{font-family:'Osaka';src:local('Helvetica'),local('Arial'),local('sans-serif');}@font-face{font-family:'STHeiti';src:local('Helvetica'),local('Arial'),local('sans-serif');}@font-face{font-family:'STHeiti Light';src:local('Helvetica'),local('Arial'),local('sans-serif');}@font-face{font-family:'Hiragino Sans';src:local('Helvetica'),local('Arial'),local('sans-serif');}@font-face{font-family:'Hiragino Mincho ProN';src:local('Times New Roman'),local('serif');}";
if(document.documentElement){(document.head||document.documentElement).appendChild(_fs);}
else{document.addEventListener('DOMContentLoaded',function(){try{(document.head||document.documentElement).appendChild(_fs);}catch(e){}});}
}catch(e){}
}catch(e){}

