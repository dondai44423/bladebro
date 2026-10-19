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
(process.argv[2] === 'media' ? media() : permissions()).catch(error => { console.error(error); process.exitCode = 1; });
