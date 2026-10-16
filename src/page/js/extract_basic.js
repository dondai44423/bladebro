(function(kind, limit) {
  // The same bounded light/shadow/frame tree agents can interact with. Keep
  // the array contract; never interpolate IDs into CSS label selectors.
  const out = [], stack = [document], seen = new WeakSet();
  let nodes = 0, roots = 0, bounded = false;
  function label(e) {
    if (e.labels && e.labels.length) return e.labels[0].textContent.trim().slice(0, 60);
    const aria = e.getAttribute('aria-label');
    if (aria) return aria.trim().slice(0, 60);
    const ids = (e.getAttribute('aria-labelledby') || '').trim().split(/\s+/);
    const root = e.getRootNode();
    const names = ids.map(id => root.getElementById && root.getElementById(id)).filter(Boolean).map(n => n.textContent.trim());
    if (names.length) return names.join(' ').slice(0, 60);
    if (e.placeholder) return e.placeholder.trim().slice(0, 60);
    const wrap = e.closest('label');
    return wrap ? wrap.textContent.trim().slice(0, 60) : '';
  }
  while (stack.length && nodes++ < 20000) {
    const n = stack.pop();
    if (!n || seen.has(n)) continue;
    seen.add(n);
    if (n.nodeType === 1) {
      if (kind === 'links' && n.tagName === 'A' && n.hasAttribute('href')) {
        const text = (n.textContent || '').trim().slice(0, 80), href = n.href;
        if (text || href) out.push({text, href});
      }
      if (kind === 'forms' && n.tagName === 'FORM') {
        const fields = [];
        for (let i = 0; i < Math.min(n.elements.length, 1000); i++) {
          const e = n.elements[i];
          if (e.tagName !== 'FIELDSET' && e.tagName !== 'BUTTON') fields.push({
            tag: e.tagName.toLowerCase(), type: e.type || null, name: e.name || '', label: label(e)
          });
        }
        if (n.elements.length > 1000) bounded = true;
        out.push({action: n.action, method: (n.method || 'get').toLowerCase(),
          fields});
      }
      if (n.tagName === 'IFRAME' && roots < 400) {
        try { if (n.contentDocument) { stack.push(n.contentDocument); roots++; } } catch (_) {}
      }
      if ((n.tagName === 'IFRAME' || n.shadowRoot) && roots >= 400) bounded = true;
      if (n.shadowRoot && roots < 400) { stack.push(n.shadowRoot); roots++; }
    }
    const children = n.childNodes;
    const count = Math.min(children.length, Math.max(0, 20000 - nodes - stack.length));
    if (count < children.length) bounded = true;
    for (let i = count - 1; i >= 0; i--) stack.push(children[i]);
  }
  const data = out.slice(0, limit), notes = [];
  if (data.length < out.length) notes.push('showing '+data.length+' of '+out.length+' '+kind+'; increase limit to read more');
  if (bounded || stack.length) notes.push('partial scan: bounded to 20000 nodes, 400 shadow/frame roots and 1000 controls per form; scope a smaller page or use custom eval for the remainder');
  return JSON.stringify(notes.length ? {data, note: notes.join('; ')} : data);
})
