// Synthetic originals make promise identity / synchronous sentinel propagation
// observable without depending on a browser's device inventory or permissions.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
const root = path.resolve(__dirname, '../..');
const helpers = fs.readFileSync(path.join(root, 'src/stealth/js/proxy_helpers.js'), 'utf8');
function install(file, sandbox) {
  vm.runInNewContext(helpers + fs.readFileSync(path.join(root, 'src/stealth/js/' + file), 'utf8'), sandbox);
}
async function permissions() {
  const records = new WeakMap();
  class PermissionStatus {
    constructor(name, state) { records.set(this, { name, state }); }
    get name() { if (!records.has(this)) throw new TypeError('Illegal invocation'); return records.get(this).name; }
    get state() { if (!records.has(this)) throw new TypeError('Illegal invocation'); return records.get(this).state; }
  }
  const sentinel = new TypeError('native validation sentinel');
  const promises = [];
  const permissions = { query(...args) {
    if (this !== permissions || !args.length) throw sentinel;
    const name = String(args[0].name);
    const promise = Promise.resolve(new PermissionStatus(name, 'denied'));
    promises.push(promise);
    return promise;
  } };
  const original = permissions.query;
  const originalState = Object.getOwnPropertyDescriptor(PermissionStatus.prototype, 'state').get;
  const sandbox = { navigator: { permissions }, PermissionStatus, location: { origin: 'https://test.example' } };
  install('permissions_patch.js', sandbox);
  assert.equal(permissions.query, original, 'query must remain the original function');
  assert.throws(() => permissions.query(), e => e === sentinel);
  assert.throws(() => permissions.query.call({}, {}), e => e === sentinel);
  let reads = 0;
  const promise = permissions.query({ get name() { reads++; return 'notifications'; } });
  assert.equal(promise, promises.at(-1), 'native promise identity');
  const status = await promise;
  assert.equal(reads, 1);
  assert.equal(status.state, 'prompt');
  assert.equal(originalState.call(status), 'denied', 'underlying permission remains native');
  assert.deepEqual(Object.getOwnPropertyNames(status), []);
  const getter = Object.getOwnPropertyDescriptor(PermissionStatus.prototype, 'state').get;
  assert.throws(() => getter.call({}), e => e.message === 'Illegal invocation');
  assert.equal(getter.name, originalState.name);
  assert.equal(getter.length, originalState.length);
  assert.equal((await permissions.query({ name: 'geolocation' })).state, 'denied');
  records.get(status).state = 'granted'; assert.equal(status.state, 'granted');
  records.get(status).state = 'denied'; sandbox.location.origin = 'null'; assert.equal(status.state, 'denied');
  let failures = 0;
  assert.throws(() => permissions.query({ get name() { failures++; throw sentinel; } }), e => e === sentinel);
  assert.equal(failures, 1);
  console.log('permissions native identity, timing, getter effects, state and receiver checks passed');
}
async function media() {
  let calls = 0, argsSeen, returned, devices = [];
  const receiver = new WeakSet();
  const sentinel = new TypeError('native media sentinel');
  let sync = false;
  class MediaDevices {
    constructor() { receiver.add(this); }
    get ondevicechange() { if (!receiver.has(this)) throw sentinel; return null; }
    enumerateDevices(...args) {
      calls++; argsSeen = args;
      if (sync) throw sentinel;
      returned = receiver.has(this) ? Promise.resolve(devices) : Promise.reject(sentinel);
      return returned;
    }
    getUserMedia() { throw sentinel; }
  }
  const md = new MediaDevices();
  const gum = md.getUserMedia;
  install('media_patch.js', { navigator: { mediaDevices: md }, MediaDevices });
  const order = [];
  const failed = md.enumerateDevices.call({});
  assert.equal(failed, returned, 'invalid receiver keeps the exact rejected promise');
  failed.catch(e => { assert.equal(e, sentinel); order.push('reject'); });
  queueMicrotask(() => order.push('microtask'));
  await failed.catch(() => {});
  assert.deepEqual(order, ['reject', 'microtask']);
  const before = calls;
  const fallback = await md.enumerateDevices(1, undefined, 'extra');
  assert.equal(calls, before + 1); assert.deepEqual(argsSeen, [1, undefined, 'extra']);
  assert.equal(fallback.length, 2);
  assert.deepEqual(Array.from(fallback, d => [d.kind, d.label, d.deviceId, d.groupId]), [['audioinput', '', '', ''], ['audiooutput', '', '', '']]);
  assert.deepEqual(JSON.parse(JSON.stringify(fallback[0])), { deviceId: '', kind: 'audioinput', label: '', groupId: '' });
  devices = [{ kind: 'videoinput', deviceId: 'native' }];
  assert.equal(await md.enumerateDevices(), devices, 'real devices stay unchanged');
  assert.equal(md.getUserMedia, gum);
  sync = true; assert.throws(() => md.enumerateDevices(), e => e === sentinel);
  console.log('media native rejection timing, identity, arity, dispatch, real devices and fallback checks passed');
}
// A mock window with just the surface the core's unguarded statements touch;
// every other block sits in its own try and skips what the mock lacks.
function mockWindow() {
  const ctx = vm.createContext({});
  const g = vm.runInContext('this', ctx);
  Object.assign(g, {
    window: g,
    document: {},
    navigator: { platform: 'Linux x86_64' },
    MutationObserver: class { observe() {} disconnect() {} },
    setTimeout: () => 0,
    addEventListener() {},
    console: { log() {}, info() {}, warn() {}, error() {}, debug() {} },
  });
  return { ctx, g };
}
// The exact assembled page script (written by the inject.rs lib test).
async function page() {
  const { ctx, g } = mockWindow();
  class SharedWorker {}
  g.SharedWorker = SharedWorker;
  // The tail's cdc_ watcher is the last statement of the script: its
  // observe() call proves every conditional segment before it ran through.
  let observed = 0;
  g.MutationObserver = class { observe() { observed++; } disconnect() {} };
  vm.runInContext(fs.readFileSync(process.argv[3], 'utf8'), ctx);
  assert.equal(observed, 1, 'the script ran through to the tail');
  assert.equal(g.SharedWorker, SharedWorker, 'SharedWorker must stay the native constructor');
  console.log('assembled page script checks passed');
}
function installRtc(sdp) {
  class EventTargetImpl extends EventTarget {}
  const native = { aic: 0 };
  const sentinel = new TypeError('native addIceCandidate sentinel');
  let handler = null;
  class RTCPeerConnection extends EventTargetImpl {
    createOffer() { return Promise.resolve({ type: 'offer', sdp }); }
    createAnswer() { return Promise.resolve({ type: 'answer', sdp }); }
    addIceCandidate() { native.aic++; return Promise.reject(sentinel); }
    get onicecandidate() { return handler; }
    set onicecandidate(v) { handler = v; }
  }
  const aic = RTCPeerConnection.prototype.addIceCandidate;
  install('rtc_patch.js', { EventTarget: EventTargetImpl, RTCPeerConnection });
  return { EventTargetImpl, RTCPeerConnection, native, sentinel, aic };
}
function iceEvent(candidate) { const e = new Event('icecandidate'); e.candidate = candidate; return e; }
async function rtcListeners() {
  const { EventTargetImpl, RTCPeerConnection } = installRtc('');
  const pc = new RTCPeerConnection();
  let n = 0;
  const fn = () => { n++; };
  pc.addEventListener('icecandidate', fn);
  pc.addEventListener('icecandidate', fn);
  pc.dispatchEvent(iceEvent(null));
  assert.equal(n, 1, 'the same listener added twice is registered once (native dedupe)');
  pc.removeEventListener('icecandidate', fn);
  pc.dispatchEvent(iceEvent(null));
  assert.equal(n, 1, 'removeEventListener(fn) removes what addEventListener(fn) registered');
  pc.addEventListener('icecandidate', fn, true);
  pc.addEventListener('icecandidate', fn, false);
  pc.dispatchEvent(iceEvent(null));
  assert.equal(n, 3, 'capture is part of the listener key');
  pc.removeEventListener('icecandidate', fn, { capture: true });
  pc.dispatchEvent(iceEvent(null));
  assert.equal(n, 4);
  pc.removeEventListener('icecandidate', fn);
  pc.dispatchEvent(iceEvent(null));
  assert.equal(n, 4);
  const seen = [];
  pc.addEventListener('icecandidate', e => { if (e.candidate) seen.push(e.candidate.candidate.split(' ')[4]); });
  for (const c of ['1 1 udp 1 203.0.113.5 50000 typ srflx raddr 0.0.0.0 rport 0', '2 1 udp 1 192.168.1.5 50000 typ host',
    '3 1 udp 1 abc.local 50000 typ host', '4 1 udp 1 198.51.100.1 3478 typ relay raddr 0.0.0.0 rport 0']) {
    pc.dispatchEvent(iceEvent({ candidate: 'candidate:' + c }));
  }
  assert.deepEqual(seen, ['abc.local', '198.51.100.1'], 'srflx and raw-IP host candidates stay hidden');
  const other = new EventTargetImpl();
  let m = 0;
  const g = () => { m++; };
  other.addEventListener('icecandidate', g);
  pc.addEventListener('icecandidate', g);
  other.removeEventListener('icecandidate', g);
  other.dispatchEvent(new Event('icecandidate'));
  assert.equal(m, 0, 'a non-RTC target removes the listener it was given');
  pc.dispatchEvent(iceEvent(null));
  assert.equal(m, 1);
  console.log('icecandidate listener dedupe, removal, capture keys and filtering checks passed');
}
async function rtcSdp() {
  const sdp = ['v=0', 'o=- 1 2 IN IP4 127.0.0.1', 's=-', 't=0 0', 'm=application 9 UDP/DTLS/SCTP webrtc-datachannel', 'c=IN IP4 0.0.0.0',
    'a=candidate:1 1 udp 2122260223 192.168.1.5 50000 typ host generation 0',
    'a=candidate:2 1 udp 2122260223 abc.local 50001 typ host generation 0',
    'a=candidate:3 1 udp 1686052607 203.0.113.5 50000 typ srflx raddr 192.168.1.5 rport 50000 generation 0',
    'a=candidate:4 1 udp 41885439 198.51.100.1 3478 typ relay raddr 203.0.113.5 rport 50000 generation 0',
    'a=ice-ufrag:abcd', 'a=end-of-candidates', ''].join('\r\n');
  const { RTCPeerConnection, native, sentinel, aic } = installRtc(sdp);
  const pc = new RTCPeerConnection();
  for (const desc of [await pc.createOffer(), await pc.createAnswer()]) {
    const lines = desc.sdp.split('\r\n');
    assert.equal(lines.pop(), '', 'SDP still ends with CRLF');
    assert.ok(lines.every(l => l.length > 0), 'no blank SDP lines (Chrome rejects them): ' + JSON.stringify(desc.sdp));
    assert.ok(!/typ (host|srflx)/.test(desc.sdp), 'host/srflx candidates removed');
    assert.ok(desc.sdp.includes('typ relay') && desc.sdp.includes('a=ice-ufrag:abcd'), 'other lines kept');
  }
  assert.equal(RTCPeerConnection.prototype.addIceCandidate, aic, 'addIceCandidate stays native');
  await assert.rejects(pc.addIceCandidate({ candidate: 'candidate:1 1 udp 1 192.168.1.5 5 typ host', sdpMid: '0' }), e => e === sentinel);
  assert.equal(native.aic, 1, 'remote host candidates reach the native method');
  console.log('SDP candidate-line removal and native addIceCandidate checks passed');
}
async function noise() {
  class CanvasRenderingContext2D {
    constructor(canvas) { this.canvas = canvas; }
    getImageData() { return { data: Uint8ClampedArray.from(this.canvas.px) }; }
    putImageData(img) { this.canvas.px = Array.from(img.data); this.canvas.writes++; }
    clearRect() { this.canvas.px = [0, 0, 0, 0]; }
    drawImage(src) { if (!(src instanceof HTMLCanvasElement)) throw new TypeError('source'); this.canvas.px = src.px.slice(); }
  }
  class HTMLCanvasElement {
    constructor() { this.width = 300; this.height = 150; this.kind = null; this.ctx = null; this.contexts = 0; this.px = [0, 0, 0, 0]; this.writes = 0; }
    getContext(kind) {
      if (this.kind === null) { this.kind = kind; this.contexts++; this.ctx = kind === '2d' ? new CanvasRenderingContext2D(this) : { kind }; }
      return this.kind === kind ? this.ctx : null;
    }
    toDataURL() { if (!(this instanceof HTMLCanvasElement)) throw new TypeError('Illegal invocation'); return 'data:' + this.px.join(','); }
  }
  const channel = new Float32Array(8);
  class AudioBuffer { getChannelData(i) { if (i !== 0) throw new RangeError('channel index'); return channel; } }
  const an = 3;
  install('noise.js', { HTMLCanvasElement, CanvasRenderingContext2D, AudioBuffer, cn: 1, cn2: 1, an,
    document: { createElement: () => new HTMLCanvasElement() } });
  const fresh = new HTMLCanvasElement();
  fresh.toDataURL();
  assert.equal(fresh.contexts, 0, 'toDataURL must not create a rendering context');
  assert.ok(fresh.getContext('webgl'), 'a later getContext(webgl) still succeeds');
  const painted = new HTMLCanvasElement();
  painted.getContext('2d');
  painted.px = [10, 20, 30, 255];
  const first = painted.toDataURL();
  assert.equal(first, 'data:11,21,30,255', 'painted 2D canvases get the seeded offset');
  assert.equal(painted.toDataURL(), first, 'once');
  const gl = new HTMLCanvasElement();
  gl.getContext('webgl');
  gl.px = [1, 2, 3, 255];
  assert.equal(gl.toDataURL(), 'data:1,2,3,255', 'WebGL canvases are left alone');
  assert.equal(gl.contexts, 1);
  assert.throws(() => HTMLCanvasElement.prototype.toDataURL.call({}), TypeError);
  const buf = new AudioBuffer();
  for (let i = 0; i < 5; i++) buf.getChannelData(0);
  assert.equal(channel[0], Math.fround(an * 1e-7), 'audio offset applied once per channel buffer, no drift');
  assert.equal(channel[1], 0);
  assert.throws(() => buf.getChannelData(1), RangeError);
  console.log('canvas context-free noise and idempotent audio noise checks passed');
}
// The console capture in the assembled core + tail.
async function consoleCapture() {
  const core = fs.readFileSync(path.join(root, 'src/stealth/js/stealth_core.js'), 'utf8')
    .replace('__SEED__', '7').replace('__PROXY_HELPERS__', () => helpers)
    + fs.readFileSync(path.join(root, 'src/stealth/js/stealth_tail.js'), 'utf8');
  const { ctx, g } = mockWindow();
  vm.runInContext(core, ctx);
  const ring = g[Symbol.for('q')];
  const last = () => ring[ring.length - 1].m;
  vm.runInContext('var fired=0,tj=0;console.log({get x(){fired++;return 1;}},[{get y(){fired++;return 2;}}]);console.log({toJSON:function(){tj++;return 1;}});', ctx);
  assert.equal(g.fired, 0, 'logging must not invoke page getters');
  assert.equal(g.tj, 0, 'logging must not invoke page toJSON');
  vm.runInContext('console.log({a:1,get b(){return 2;}})', ctx);
  assert.equal(last(), '{"a":1,"b":"(...)"}', 'accessors are shown, not invoked');
  // Plain values read exactly as the JSON.stringify capture wrote them.
  const old = c => vm.runInContext(`(function(){var a=[${c}],p=[];for(var i=0;i<a.length;i++){var v=a[i];try{p.push(typeof v==='string'?v:JSON.stringify(v));}catch(e){p.push(String(v));}}return p.join(' ').slice(0,400);})()`, ctx);
  for (const c of ['1', "'str'", 'true', 'null', 'undefined', '-0', 'NaN', 'Infinity', '1.5e300', '10n',
    "({a:1,b:[1,2,{c:'x\"y\\n\\u2028'}],d:null,e:true,f:-2.5})", '[1,[2,[3,[]]],{}]',
    "({u:undefined,f:function(){},s:Symbol('s'),k:1})", "[undefined,function(){},Symbol('t'),2]", '[,1]',
    'new Date(0)', 'new Date(NaN)', '({d:new Date(86400000)})', "'a', 1, {x:[1]}, undefined, 'b'",
    "({'k\"ey':'v'})", '(function(){var o=Object.create(null);o.z=1;return o;})()', 'new Uint8Array([1,2])',
    'new Error("x")', 'new Map([[1,2]])', '({a:1n})', '(function(){var o={a:1};o.self=o;return o;})()',
    "'x'.repeat(500)", "{s:'y'.repeat(500)}", "'a'.repeat(399), 'tail'"]) {
    vm.runInContext(`console.log(${c})`, ctx);
    assert.equal(last(), old(c), c);
  }
  // Bounded: a large value costs a few hundred property reads, not all of them.
  vm.runInContext('var reads=0,big=[];for(var i=0;i<200000;i++)big.push({i:i,s:"xxxxxxxx"});var pbig=new Proxy(big,{get:function(t,k,r){reads++;return Reflect.get(t,k,r);},getOwnPropertyDescriptor:function(t,k){reads++;return Reflect.getOwnPropertyDescriptor(t,k);}});console.log(pbig);', ctx);
  assert.equal(last(), vm.runInContext('JSON.stringify(big).slice(0,400)', ctx), 'same first 400 chars');
  assert.ok(g.reads < 1000, `logging a 200k-element array read ${g.reads} properties`);
  console.log('console capture: no page code, plain values unchanged, bounded work');
}
const scenarios = { permissions, media, page, 'rtc-listeners': rtcListeners, 'rtc-sdp': rtcSdp, noise, console: consoleCapture };
// An unknown name must fail: falling back to a default scenario let a
// misspelled scenario in a Rust test pass without checking anything.
const scenario = Object.hasOwn(scenarios, process.argv[2]) ? scenarios[process.argv[2]]
  : async () => { throw new Error(`unknown scenario ${JSON.stringify(process.argv[2])}; known: ${Object.keys(scenarios).join(', ')}`); };
scenario().catch(error => { console.error(error); process.exitCode = 1; });
