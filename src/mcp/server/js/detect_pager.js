(() => {
  const vis = (el) => {
    if (!el || el.disabled || el.getAttribute('aria-disabled') === 'true') return false;
    const st = getComputedStyle(el);
    if (st.display === 'none' || st.visibility === 'hidden' || st.opacity === '0') return false;
    const r = el.getBoundingClientRect();
    return r.width > 0 && r.height > 0;
  };
  const nextTxt = (el) => {
    const raw = (el.innerText || el.textContent || el.getAttribute('aria-label') || '').trim().toLowerCase();
    return /^(next(\s+page)?|older(\s+posts)?|more\s+results|›|»|>|→)$/.test(raw) || /\bnext\s+page\b/.test(raw);
  };
  for (const sel of ['a[rel="next"]', 'li.next > a', '.pager .next a', '.pagination .next a', 'a.next', 'button.next']) {
    for (const el of document.querySelectorAll(sel)) {
      if (vis(el)) {
        return { selector: sel, text: (el.innerText || el.getAttribute('aria-label') || 'next').trim().slice(0, 40) };
      }
    }
  }
  const linkNext = document.querySelector('link[rel="next"][href]');
  if (linkNext) return { selector: null, text: 'next' };
  for (const box of document.querySelectorAll('nav[aria-label*="pagin" i], .pagination, .pager, [role="navigation"][aria-label*="pagin" i]')) {
    for (const el of box.querySelectorAll('a[href], button')) {
      if (vis(el) && nextTxt(el)) {
        return { selector: null, text: (el.innerText || el.getAttribute('aria-label') || 'next').trim().slice(0, 40) };
      }
    }
  }
  return null;
})()
