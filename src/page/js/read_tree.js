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
      if (/^(SCRIPT|STYLE|NOSCRIPT|TEMPLATE|SVG|META|LINK)$/.test(n.tagName) || n.hidden) continue;
      const style = n.ownerDocument.defaultView.getComputedStyle(n);
      if (style.display === 'none' || style.visibility === 'hidden' || style.opacity === '0') continue;
      if (visit(n) === false) return;
      if (n.tagName === 'IFRAME') {
        try {
          const doc = n.contentDocument;
          if (doc && doc.body && roots++ < 400) stack.push(doc.body);
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
