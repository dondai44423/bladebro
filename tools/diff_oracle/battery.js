// Differential-oracle battery — executed IDENTICALLY in a stock Chrome and in
// a bladebro browser; oracle.py flattens + diffs the two JSON maps.
// Wrapped by the driver as: (async()=>{ <this body> })()
// Keys are flat; every key is compared unless classified EXPECTED by the
// driver's (documented) deviation table.
const fnv = (s) => { let h = 5381; for (let i = 0; i < s.length; i++) h = ((h << 5) + h + s.charCodeAt(i)) >>> 0; return h.toString(16); };
const out = {};

// --- core navigator / document ---
out.ua = navigator.userAgent;
out.platform = navigator.platform;
out.languages = (navigator.languages || []).join(",");
out.webdriver = String(navigator.webdriver);
out.hc = navigator.hardwareConcurrency;
out.dm = navigator.deviceMemory;
out.plugins = Array.from(navigator.plugins).map(p => p.name).join("|");
out.mimeTypes = Array.from(navigator.mimeTypes).map(m => m.type).join("|");
out.chromeObj = !!(window.chrome && window.chrome.app && window.chrome.runtime);
out.chromeKeys = Object.keys(window.chrome || {}).sort().join("|");
out.pdfViewer = navigator.pdfViewerEnabled;
out.cookieEnabled = navigator.cookieEnabled;
out.tz = Intl.DateTimeFormat().resolvedOptions().timeZone;
out.voices = speechSynthesis.getVoices().length;
out.dnt = String(navigator.doNotTrack);
out.gpuApi = "gpu" in navigator;

// --- screen / window geometry ---
out.screenW = screen.width; out.screenH = screen.height;
out.availW = screen.availWidth; out.availH = screen.availHeight;
out.colorDepth = screen.colorDepth; out.pixelDepth = screen.pixelDepth;
out.dpr = devicePixelRatio;
out.outerW = outerWidth; out.outerH = outerHeight;
out.innerW = innerWidth; out.innerH = innerHeight;
out.screenX = screenX; out.screenY = screenY;
out.vis = document.visibilityState;
out.docClientH = document.documentElement.clientHeight;
out.visualH = (window.visualViewport && window.visualViewport.height) || -1;

// --- User-Agent Client Hints ---
try {
  const hv = await navigator.userAgentData.getHighEntropyValues(["platform", "architecture", "bitness", "model", "uaFullVersion"]);
  out.uad = [hv.platform, hv.architecture, hv.bitness, hv.model].join("|");
  out.uadFull = hv.uaFullVersion;
} catch (e) { out.uad = "ERR"; }

// --- permissions native-parity battery ---
const P = navigator.permissions;
const msg = async (f) => {
  try { const p = f(); if (p && p.then) { try { await p; return "resolved"; } catch (e) { return "rej:" + (e.message || e); } } return "sync"; }
  catch (e) { return "throw:" + (e.message || e); }
};
out.permZero = await msg(() => P.query());
out.permStr = await msg(() => P.query("geolocation"));
out.permRecv = await msg(() => P.query.call({}, { name: "geolocation" }));
try { out.permNotifState = (await P.query({ name: "notifications" })).state; } catch (e) { out.permNotifState = "ERR"; }
out.notifPermission = String(Notification.permission);
out.permTostr = Function.prototype.toString.call(P.query);
out.permDesc = (() => {
  const d = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(P), "query");
  return d ? [d.writable, d.enumerable, d.configurable].join(",") : "none";
})();

// --- WebGL1 ---
const mk = (t) => { try { const c = document.createElement("canvas"); return c.getContext(t); } catch (e) { return null; } };
const g = mk("webgl");
out.gl1 = !!g;
if (g) {
  const e = g.getExtension("WEBGL_debug_renderer_info");
  out.glVendor = String(g.getParameter(g.VENDOR));
  out.glRenderer = String(g.getParameter(g.RENDERER));
  out.glUnmaskedVendor = e ? String(g.getParameter(e.UNMASKED_VENDOR_WEBGL)) : "noext";
  out.glUnmaskedRenderer = e ? String(g.getParameter(e.UNMASKED_RENDERER_WEBGL)) : "noext";
  out.glMaxTex = g.getParameter(3379);
  out.glMaxCombined = g.getParameter(35661);
  const ex = g.getSupportedExtensions();
  out.glExtCount = ex.length;
  out.glExtHash = fnv(ex.slice().sort().join(","));
  out.glExtHasLod = ex.indexOf("EXT_shader_texture_lod") > -1;
  out.glExtHasPolygon = ex.indexOf("WEBGL_polygon_mode") > -1;
  out.glLodCall = String(g.getExtension("EXT_shader_texture_lod"));
  const prec = (p) => { const o = g.getShaderPrecisionFormat(g.FRAGMENT_SHADER, p); return [o.rangeMin, o.rangeMax, o.precision].join(","); };
  out.glPrecH = prec(36338);
  out.glPrecM = prec(36337);
  out.glPrecL = prec(36336);
  out.glPrecI = prec(36341);
  out.glCtxAttrs = JSON.stringify(g.getContextAttributes());
}

// --- WebGL2 ---
const g2 = mk("webgl2");
out.gl2 = !!g2;
if (g2) {
  out.gl2MaxSamples = g2.getParameter(36183);
  const ex2 = g2.getSupportedExtensions();
  out.gl2ExtCount = ex2.length;
  out.gl2HasOvr = ex2.indexOf("OVR_multiview2") > -1;
}

// --- canvas + audio fingerprints ---
try {
  const c = document.createElement("canvas"); c.width = 240; c.height = 60;
  const x = c.getContext("2d"); x.textBaseline = "top"; x.font = "16px Arial";
  x.fillStyle = "#f60"; x.fillRect(10, 10, 80, 20); x.fillStyle = "#069";
  x.fillText("Bladebro,\ud83d\ude00", 12, 24);
  out.canvas = fnv(c.toDataURL());
} catch (e) { out.canvas = "ERR"; }
try {
  const ac = new OfflineAudioContext(1, 5000, 44100);
  const o = ac.createOscillator(); o.frequency.value = 1000;
  const k = ac.createDynamicsCompressor(); o.connect(k); k.connect(ac.destination); o.start(0);
  const b = await ac.startRendering(); const d = b.getChannelData(0); let s = 0;
  for (let i = 100; i < 1100; i++) s += Math.abs(d[i]);
  out.audio = s.toFixed(6);
} catch (e) { out.audio = "ERR"; }

// --- worker GL (cross-context propagation) ---
try {
  out.workerGl = await new Promise((resolve) => {
    const code = `onmessage = () => { let r = "null"; try { const g = new OffscreenCanvas(8, 8).getContext("webgl"); if (g) { const e = g.getExtension("WEBGL_debug_renderer_info"); r = e ? String(g.getParameter(e.UNMASKED_RENDERER_WEBGL)) : "noext"; } } catch (e) { r = "err"; } postMessage(r); };`;
    const w = new Worker(URL.createObjectURL(new Blob([code], { type: "application/javascript" })));
    const to = setTimeout(() => resolve("timeout"), 8000);
    w.onmessage = (ev) => { clearTimeout(to); w.terminate(); resolve(String(ev.data)); };
    w.postMessage(0);
  });
} catch (e) { out.workerGl = "ERR"; }

// --- same-origin iframe GL ---
try {
  out.iframeGl = await new Promise((resolve) => {
    const f = document.createElement("iframe");
    f.src = "about:blank";
    f.onload = () => {
      try {
        const w = f.contentWindow;
        const gg = w.document.createElement("canvas").getContext("webgl");
        const e = gg ? gg.getExtension("WEBGL_debug_renderer_info") : null;
        resolve(gg ? (e ? String(gg.getParameter(e.UNMASKED_RENDERER_WEBGL)) : "noext") : "null");
      } catch (err) { resolve("err"); }
    };
    document.body.appendChild(f);
    setTimeout(() => resolve("timeout"), 3000);
  });
} catch (e) { out.iframeGl = "ERR"; }

// --- error stack shape ---
out.stack1 = String((new Error("x")).stack).split("\n")[1] || "";

return JSON.stringify(out);
