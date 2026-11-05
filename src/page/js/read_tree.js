// Read-only traversal crosses frame documents; ref collection must keep its
// per-document walk so frame paths and duplicate-name ranks stay unchanged.
function walkReadTree(root, visit) {
  if (!root) return;
  const stack = [root];
  let nodes = 0, roots = 0;
  while (stack.length && nodes++ < 20000) {
    const n = stack.pop();
    if (n.nodeType === 3) {
      if (visit(n) === false) return;
      continue;
    }
    if (n.nodeType !== 1 && n.nodeType !== 9 && n.nodeType !== 11) continue;
    if (n.nodeType === 1) {
      // Inline SVG children report lowercase tagNames, so the match is
      // case-insensitive (SVG <style> CSS stays out of text conditions).
      // <svg> itself is walked: SVG-hosted controls (role=button icons,
      // SVG <a> links) are in the capture model, and element conditions
      // must find what the model shows. Only SVG's unrendered text carriers
      // (<title>, <desc>, <metadata>) are skipped with it.
      if (/^(SCRIPT|STYLE|NOSCRIPT|TEMPLATE|META|LINK)$/i.test(n.tagName) || n.hidden) continue;
      if (n.namespaceURI === 'http://www.w3.org/2000/svg' && /^(TITLE|DESC|METADATA)$/i.test(n.tagName)) continue;
      const style = n.ownerDocument.defaultView.getComputedStyle(n);
      if (style.display === 'none' || style.visibility === 'hidden' || style.opacity === '0') continue;
      if (visit(n) === false) return;
      if (n.tagName === 'IFRAME') {
        try {
          const doc = n.contentDocument;
          if (doc && doc.documentElement && roots++ < 400) stack.push(doc.documentElement);
        } catch (_) {}
        continue;
      }
      if (n.shadowRoot && roots++ < 400) stack.push(n.shadowRoot);
    }
    // A wide DOM must not allocate an unbounded pending-node stack.
    const children = n.childNodes;
    const count = Math.min(children.length, 20000 - nodes - stack.length);
    for (let i = count - 1; i >= 0; i--) stack.push(children[i]);
  }
}
