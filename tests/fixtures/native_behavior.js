// Shared by audit and the stock-vs-driver oracle. Invalid inputs never request
// camera/microphone/screen access. Observe the call BEFORE awaiting: await/catch
// alone loses the distinction between a throw and a rejected native Promise.
async function nativeBehaviorProbe() {
  const rows = {}, checks = {};
  async function observe(call) {
    const order = [];
    let result;
    try { result = call(); }
    catch (e) { return { phase: 'throw', name: e.name, message: e.message, typeError: e instanceof TypeError }; }
    if (!(result instanceof Promise)) return { phase: 'return', type: typeof result };
    const settled = result.then(
      () => ({ phase: 'resolve' }),
      e => ({ phase: 'reject', name: e.name, message: e.message, typeError: e instanceof TypeError })
    ).then(value => { order.push(value.phase); return value; });
    // Register against the original promise: a wrapper's extra rejection hop
    // moves this callback behind the queued microtask even if messages match.
    result.then(() => order.push('fulfilled'), () => order.push('rejected'));
    queueMicrotask(() => order.push('microtask'));
    const value = await settled;
    value.order = order.filter(x => x === 'rejected' || x === 'fulfilled' || x === 'microtask').join(',');
    return value;
  }
  async function invalid(label, call, phase) {
    const row = rows[label] = await observe(call);
    checks[label] = row.phase === phase && row.typeError === true;
    if (phase === 'reject') checks[label + '.microtask'] = row.order === 'rejected,microtask';
  }
  const p = navigator.permissions, m = navigator.mediaDevices;
  for (const [label, args] of [
    ['missing', []], ['empty', [{}]], ['null', [null]],
    ['undefined', [undefined]], ['number', [5]], ['string', ['geolocation']]
  ]) {
    await invalid('query.' + label, () => Reflect.apply(p.query, p, args), 'reject');
    await invalid('getUserMedia.' + label, () => Reflect.apply(m.getUserMedia, m, args), 'reject');
  }
  await invalid('query.receiver', () => p.query.call({}, { name: 'geolocation' }), 'reject');
  await invalid('getUserMedia.receiver', () => m.getUserMedia.call({}, {}), 'reject');
  await invalid('enumerateDevices.receiver', () => m.enumerateDevices.call({}), 'reject');
  await invalid('getSupportedConstraints.receiver', () => m.getSupportedConstraints.call({}), 'throw');
  if (m.getDisplayMedia) {
    await invalid('getDisplayMedia.receiver', () => m.getDisplayMedia.call({}, { video: false }), 'reject');
  }
  for (const [label, call] of [
    ['querySelector', () => document.querySelector()], ['matchMedia', () => matchMedia()],
    ['sendBeacon', () => navigator.sendBeacon()], ['geolocation', () => navigator.geolocation.getCurrentPosition()],
    ['setItem', () => localStorage.setItem()], ['createElement', () => document.createElement()]
  ]) await invalid('control.' + label, call, 'throw');

  for (const name of ['geolocation', 'notifications']) {
    let reads = 0;
    const row = rows['getter.' + name] = { outcome: await observe(() => p.query({ get name() { reads++; return name; } })) };
    row.reads = reads;
    checks['getter.' + name] = row.outcome.phase === 'resolve' && reads === 1;
    const trace = [];
    const descriptor = new Proxy({ name }, { get(t, k, r) { trace.push(String(k)); return Reflect.get(t, k, r); } });
    const proxyRow = rows['proxy.' + name] = { outcome: await observe(() => p.query(descriptor)), trace };
    checks['proxy.' + name] = proxyRow.outcome.phase === 'resolve' && trace.join(',') === 'name';
    const status = await p.query({ name });
    const own = rows['statusOwn.' + name] = Object.getOwnPropertyNames(status).sort();
    checks['statusOwn.' + name] = own.length === 0;
  }
  let reads = 0;
  const sentinel = new Error('descriptor getter sentinel');
  let same = false;
  const thrown = rows.getterThrows = await observe(() => {
    const result = p.query({ get name() { reads++; throw sentinel; } });
    result.catch(e => { same = e === sentinel; });
    return result;
  });
  thrown.reads = reads; thrown.same = same;
  checks.getterThrows = thrown.phase === 'reject' && thrown.name === 'Error' && same && reads === 1;

  const state = Object.getOwnPropertyDescriptor(PermissionStatus.prototype, 'state');
  await invalid('permissionState.receiver', () => state.get.call({}), 'throw');
  rows.stateShape = [state.enumerable, state.configurable, state.get.name, state.get.length,
    Object.getOwnPropertyNames(state.get).sort().join(','), /\[native code\]/.test(String(state.get))];
  checks.stateShape = JSON.stringify(rows.stateShape) === JSON.stringify([true, true, 'get state', 0, 'length,name', true]);
  return { rows, checks };
}
