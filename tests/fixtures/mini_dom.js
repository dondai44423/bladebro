// Test-only DOM stand-in for the injected action scripts (find-by-sig, leaf
// targeting, scroll probe). Just enough of the platform to run the REAL
// assembled expressions under node: attributes, a selector-list matcher
// (tag, #id, .class, [attr], [attr=v], :not(...), :disabled), layout from
// explicit document-space rects, hit-testing in paint order, focus, scroll
// and events. Never shipped; the driver itself does not need node.
//
// Build pages with h(tag, attrs, ...kids) and page({vw, vh, h, w}, ...kids).
// attrs: plain DOM attributes, plus `rect:[x,y,w,h]` (document space),
// `style:{...}` and `props:{...}` (own properties such as clientLeft).
'use strict';
(function () {
  class Text {
    constructor(t) { this.nodeType = 3; this.textContent = String(t); this.parentNode = null; }
  }
  function parseCompound(s) {
    const parts = [];
    let i = 0;
    while (i < s.length) {
      const rest = s.slice(i);
      let m;
      if (rest.startsWith(':not(')) {
        let depth = 0, j = i + 4;
        for (; j < s.length; j++) {
          if (s[j] === '(') depth++;
          else if (s[j] === ')' && --depth === 0) break;
        }
        const inner = parseCompound(s.slice(i + 5, j));
        parts.push((e) => !inner(e));
        i = j + 1;
      } else if (rest.startsWith(':disabled')) {
        parts.push((e) => !!e.disabled);
        i += 9;
      } else if (rest[0] === '[') {
        const j = s.indexOf(']', i);
        const body = s.slice(i + 1, j);
        const eq = body.indexOf('=');
        if (eq < 0) {
          const a = body.trim();
          parts.push((e) => e.hasAttribute(a));
        } else {
          const a = body.slice(0, eq).trim();
          const v = body.slice(eq + 1).trim().replace(/^["']|["']$/g, '');
          parts.push((e) => e.getAttribute(a) === v);
        }
        i = j + 1;
      } else if ((m = /^#([\w-]+)/.exec(rest))) {
        parts.push((e) => e.getAttribute('id') === m[1]);
        i += m[0].length;
      } else if ((m = /^\.([\w-]+)/.exec(rest))) {
        parts.push((e) => (e.getAttribute('class') || '').split(/\s+/).includes(m[1]));
        i += m[0].length;
      } else if (rest[0] === '*') {
        i++;
      } else if ((m = /^[\w-]+/.exec(rest))) {
        const t = m[0].toUpperCase();
        parts.push((e) => e.tagName === t);
        i += m[0].length;
      } else {
        throw new Error('mini_dom: unsupported selector ' + s);
      }
    }
    return (e) => parts.every((p) => p(e));
  }
  function matcher(sel) {
    const list = sel.split(',').map((p) => parseCompound(p.trim()));
    return (e) => e.nodeType === 1 && list.some((f) => f(e));
  }
  function descendants(root) {
    const out = [];
    const walk = (n) => { for (const c of n.childNodes) if (c.nodeType === 1) { out.push(c); walk(c); } };
    walk(root);
    return out;
  }
  class El {
    constructor(tag, attrs, kids) {
      this.nodeType = 1;
      this.tagName = tag.toUpperCase();
      this._a = {};
      this.childNodes = [];
      this.parentNode = null;
      this.style = {};
      this._rect = null;
      this._ls = {};
      this.events = [];
      this.shadowRoot = null;
      this.onclick = null;
      const tg = this.tagName;
      if (['INPUT', 'TEXTAREA', 'SELECT', 'OPTION', 'BUTTON'].includes(tg)) Object.defineProperty(this, 'value', VALUE);
      if (tg === 'INPUT' || tg === 'TEXTAREA') this.value = '';
      for (const k of Object.keys(attrs || {})) {
        const v = attrs[k];
        if (k === 'rect') this._rect = v;
        else if (k === 'style') Object.assign(this.style, v);
        else if (k === 'props') Object.assign(this, v);
        else this.setAttribute(k, v);
      }
      if ((tg === 'INPUT' || tg === 'TEXTAREA') && this.hasAttribute('value')) this.value = this.getAttribute('value');
      for (const c of kids || []) this.append(c);
    }
    append(c) {
      const n = (typeof c === 'string') ? new Text(c) : c;
      n.parentNode = this;
      this.childNodes.push(n);
      return n;
    }
    remove() {
      if (!this.parentNode) return;
      const p = this.parentNode;
      p.childNodes = p.childNodes.filter((c) => c !== this);
      this.parentNode = null;
    }
    getAttribute(k) { return Object.prototype.hasOwnProperty.call(this._a, k) ? this._a[k] : null; }
    setAttribute(k, v) { this._a[k] = String(v); }
    hasAttribute(k) { return Object.prototype.hasOwnProperty.call(this._a, k); }
    removeAttribute(k) { delete this._a[k]; }
    get id() { return this.getAttribute('id') || ''; }
    get className() { return this.getAttribute('class') || ''; }
    get title() { return this.getAttribute('title') || ''; }
    get placeholder() { return this.getAttribute('placeholder') || ''; }
    get type() {
      if (this.tagName === 'INPUT') return (this.getAttribute('type') || 'text').toLowerCase();
      if (this.tagName === 'BUTTON') return (this.getAttribute('type') || 'submit').toLowerCase();
      if (this.tagName === 'SELECT') return 'select-one';
      if (this.tagName === 'TEXTAREA') return 'textarea';
      return undefined;
    }
    get href() { return (this.tagName === 'A' && this.hasAttribute('href')) ? 'http://fixture.test/' + this.getAttribute('href') : undefined; }
    get disabled() {
      if (this.hasAttribute('disabled')) return true;
      if (this.tagName === 'OPTION' && this.parentNode && this.parentNode.tagName === 'OPTGROUP') return this.parentNode.hasAttribute('disabled');
      return false;
    }
    set disabled(v) { if (v) this.setAttribute('disabled', ''); else this.removeAttribute('disabled'); }
    get tabIndex() {
      const t = this.getAttribute('tabindex');
      if (t !== null) return parseInt(t, 10);
      return this._nativeFocusable() ? 0 : -1;
    }
    get isContentEditable() {
      for (let e = this; e && e.nodeType === 1; e = e.parentNode) {
        const c = e.getAttribute('contenteditable');
        if (c === '' || c === 'true') return true;
        if (c === 'false') return false;
      }
      return false;
    }
    get parentElement() { return (this.parentNode && this.parentNode.nodeType === 1) ? this.parentNode : null; }
    get children() { return this.childNodes.filter((c) => c.nodeType === 1); }
    get textContent() { return this.childNodes.map((c) => c.textContent).join(''); }
    get innerText() { return this.textContent; }
    get ownerDocument() {
      let r = this;
      while (r.parentNode) r = r.parentNode;
      return r.nodeType === 9 ? r : null;
    }
    getRootNode() { let r = this; while (r.parentNode) r = r.parentNode; return r; }
    get isConnected() { return this.ownerDocument !== null; }
    contains(o) { for (let e = o; e; e = e.parentNode) if (e === this) return true; return false; }
    matches(sel) { return matcher(sel)(this); }
    closest(sel) { const m = matcher(sel); for (let e = this; e && e.nodeType === 1; e = e.parentNode) if (m(e)) return e; return null; }
    querySelectorAll(sel) { const m = matcher(sel); return descendants(this).filter(m); }
    querySelector(sel) { return this.querySelectorAll(sel)[0] || null; }
    _rendered() {
      for (let e = this; e && e.nodeType === 1; e = e.parentNode) if (e.style.display === 'none') return false;
      return !!this._rect;
    }
    getBoundingClientRect() {
      const doc = this.ownerDocument;
      if (!this._rendered() || !doc) return { x: 0, y: 0, width: 0, height: 0, left: 0, top: 0, right: 0, bottom: 0 };
      const [x0, y0, w, h] = this._rect;
      const x = x0 - doc._sx, y = y0 - doc._sy;
      return { x, y, width: w, height: h, left: x, top: y, right: x + w, bottom: y + h };
    }
    _nativeFocusable() {
      const t = this.tagName;
      if (t === 'A') return this.hasAttribute('href');
      return t === 'INPUT' || t === 'TEXTAREA' || t === 'SELECT' || t === 'BUTTON';
    }
    focus() {
      const doc = this.ownerDocument;
      if (!doc || !this._rendered() || this.disabled) return;
      if (!(this._nativeFocusable() || this.isContentEditable || this.hasAttribute('tabindex'))) return;
      if (doc.activeElement === this) return;
      doc.activeElement = this;
      this.dispatchEvent(new Event('focus'));
    }
    blur() { const doc = this.ownerDocument; if (doc && doc.activeElement === this) doc.activeElement = doc.body; }
    scrollIntoView(o) {
      const doc = this.ownerDocument;
      if (!doc || !this._rendered()) return;
      const [x, y, w, h] = this._rect;
      const block = (o && o.block) || 'start';
      if (block === 'center') doc._sy = y + h / 2 - doc._vh / 2;
      else if (block === 'start') doc._sy = y;
      else if (y < doc._sy) doc._sy = y;
      else if (y + h > doc._sy + doc._vh) doc._sy = y + h - doc._vh;
      const inline = (o && o.inline) || 'nearest';
      if (inline === 'center') doc._sx = x + w / 2 - doc._vw / 2;
      else if (x < doc._sx) doc._sx = x;
      else if (x + w > doc._sx + doc._vw) doc._sx = x + w - doc._vw;
      doc._clamp();
      doc.scrolls++;
    }
    addEventListener(t, f) { (this._ls[t] = this._ls[t] || []).push(f); }
    dispatchEvent(ev) {
      ev.target = ev.target || this;
      this.events.push(ev.type);
      for (let e = this; e; e = e.parentNode) {
        for (const f of (e._ls && e._ls[ev.type]) || []) f.call(e, ev);
        if (ev.type === 'click' && typeof e.onclick === 'function') e.onclick.call(e, ev);
        if (!ev.bubbles) break;
      }
      return true;
    }
    click() { this.dispatchEvent(new MouseEvent('click', { bubbles: true })); }
    setSelectionRange(a, b) { this.selectionStart = a; this.selectionEnd = b; }
    // <select> / <option>
    get options() { return this.tagName === 'SELECT' ? this.querySelectorAll('option') : undefined; }
    get selectedIndex() {
      if (this.tagName !== 'SELECT') return undefined;
      const o = this.options;
      const i = o.findIndex((x) => x._selected);
      return i >= 0 ? i : (o.length ? 0 : -1);
    }
    set selectedIndex(i) { this.options.forEach((x, k) => { x._selected = (k === i); }); }
    get index() { const s = this.closest('select'); return s ? s.options.indexOf(this) : 0; }
    get label() { return this.getAttribute('label') || this.text; }
    get text() { return this.textContent.trim(); }
    get selected() { const s = this.closest('select'); return !!s && s.options[s.selectedIndex] === this; }
  }
  // `value` exists only on form controls (a div has none, which the
  // injected scripts test with `'value' in e`); select/option derive it.
  const VALUE = {
    configurable: true,
    get() {
      if (this.tagName === 'OPTION') return this.hasAttribute('value') ? this.getAttribute('value') : this.text;
      if (this.tagName === 'SELECT') { const o = this.options[this.selectedIndex]; return o ? o.value : ''; }
      return this._v;
    },
    set(v) {
      if (this.tagName === 'SELECT') { const i = this.options.findIndex((o) => o.value === String(v)); this.selectedIndex = i; return; }
      this._v = String(v);
    },
  };
  class Doc {
    constructor(o) {
      this.nodeType = 9;
      this.parentNode = null;
      this._vw = o.vw || 800; this._vh = o.vh || 600;
      this._w = o.w || this._vw; this._h = o.h || this._vh;
      this._sx = 0; this._sy = 0;
      this.scrolls = 0;
      this.documentElement = new El('html', { rect: [0, 0, this._w, this._h] });
      this.documentElement.parentNode = this;
      this.childNodes = [this.documentElement];
      this.body = this.documentElement.append(new El('body', { rect: [0, 0, this._w, this._h] }));
      this.activeElement = this.body;
      const doc = this;
      this.scrollingElement = {
        get scrollTop() { return doc._sy; }, get scrollLeft() { return doc._sx; },
        get scrollHeight() { return doc._h; }, get scrollWidth() { return doc._w; },
        get clientHeight() { return doc._vh; }, get clientWidth() { return doc._vw; },
      };
    }
    _clamp() {
      this._sy = Math.max(0, Math.min(this._sy, this._h - this._vh));
      this._sx = Math.max(0, Math.min(this._sx, this._w - this._vw));
    }
    get ownerDocument() { return null; }
    querySelectorAll(sel) { const m = matcher(sel); return [this.documentElement, ...descendants(this.documentElement)].filter(m); }
    querySelector(sel) { return this.querySelectorAll(sel)[0] || null; }
    getElementById(id) { return this.querySelector('#' + id); }
    contains(o) { return !!o && o.ownerDocument === this; }
    getRootNode() { return this; }
    getSelection() { return globalThis.window.getSelection(); }
    createRange() { return { selectNodeContents() {} }; }
    execCommand() { return false; }
    elementFromPoint(x, y) {
      if (x < 0 || y < 0 || x >= this._vw || y >= this._vh) return null;
      const all = [this.documentElement, ...descendants(this.documentElement)];
      let best = null, bestZ = -Infinity;
      all.forEach((e) => {
        if (!e._rendered() || e.style.pointerEvents === 'none' || getComputedStyle(e).visibility === 'hidden') return;
        const r = e.getBoundingClientRect();
        if (x < r.left || x >= r.right || y < r.top || y >= r.bottom) return;
        const z = Number(e.style.zIndex || 0);
        if (z >= bestZ) { best = e; bestZ = z; }
      });
      return best;
    }
  }
  class Event {
    constructor(type, o) { this.type = type; this.bubbles = !!(o && o.bubbles); this.isTrusted = false; Object.assign(this, o || {}); }
  }
  class MouseEvent extends Event {}
  class PointerEvent extends MouseEvent {}
  function getComputedStyle(e) {
    let vis = 'visible';
    for (let p = e; p && p.nodeType === 1; p = p.parentNode) if (p.style.visibility) { vis = p.style.visibility; break; }
    const s = e.style;
    return {
      display: s.display || 'block', visibility: vis, opacity: s.opacity || '1',
      overflowX: s.overflowX || s.overflow || 'visible', overflowY: s.overflowY || s.overflow || 'visible',
      pointerEvents: s.pointerEvents || 'auto',
      paddingLeft: s.paddingLeft || '0px', paddingTop: s.paddingTop || '0px',
    };
  }
  function h(tag, attrs, ...kids) { return new El(tag, attrs, kids); }
  function page(o, ...kids) {
    const d = new Doc(o || {});
    for (const k of kids) d.body.append(k);
    return d;
  }
  // Make `d` the top-level document the injected expression sees.
  function use(d) {
    const sel = { isCollapsed: true, toString() { return ''; }, selectAllChildren() {}, removeAllRanges() {}, addRange() {} };
    globalThis.document = d;
    globalThis.window = {
      get innerWidth() { return d._vw; }, get innerHeight() { return d._vh; },
      get scrollX() { return d._sx; }, get scrollY() { return d._sy; },
      getSelection() { return sel; },
      HTMLInputElement: { prototype: {} }, HTMLTextAreaElement: { prototype: {} },
    };
    return d;
  }
  let failures = 0;
  function eq(got, want, label) {
    const g = JSON.stringify(got), w = JSON.stringify(want);
    if (g !== w) { failures++; console.log('FAIL ' + label + ': got ' + g + ', want ' + w); }
  }
  function ok(cond, label, detail) {
    if (!cond) { failures++; console.log('FAIL ' + label + (detail === undefined ? '' : ': ' + JSON.stringify(detail))); }
  }
  function done(name) {
    if (!failures) console.log(name + ' pass');
    process.exit(failures ? 1 : 0);
  }
  Object.assign(globalThis, { h, page, use, eq, ok, done, getComputedStyle, Event, MouseEvent, PointerEvent });
})();
