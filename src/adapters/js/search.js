// Search engine adapters: Google Search, DuckDuckGo (SPA + HTML/Lite), and Bing Search.
const IS_GOOGLE_SEARCH = /(?:^|\.)google\.[a-z.]+$/.test(HOST) && location.pathname === '/search';
if (IS_GOOGLE_SEARCH) {
  const root = document.querySelector('#search, #rso') || document.body;
  const h3s = root ? [...root.querySelectorAll('h3')] : [];
  const seenUrls = new Set();
  const items = [];
  let unwrapped = 0, unresolved = 0;
  for (const h3 of h3s) {
    if (items.length >= __LIMIT__) break;
    if (!isShown(h3)) continue;
    // Skip sitelink sub-tables and People Also Ask accordions.
    if (h3.closest('table, [data-q], [jsname="N760b"]')) continue;
    const a = h3.closest('a[href]') || (h3.parentElement && h3.parentElement.querySelector('a[href]'));
    if (!a || typeof a.href !== 'string') continue;
    const title = vtxt(h3);
    if (!title || title.length < 2) continue;
    const rawHref = a.href;
    const cu = canonicalURL(rawHref);
    const key = cu.split('#')[0];
    if (seenUrls.has(key)) continue;
    seenUrls.add(key);
    const o = { title: title.slice(0, 200), url: cu };
    if (cu !== rawHref) unwrapped++;
    if (wrappedURL(cu)) {
      o.url_wrapped = true;
      unresolved++;
    }
    // Find enclosing result block (semantic ancestor carrying cite + snippet, never huge container).
    let block = a.parentElement;
    for (let i = 0; i < 6 && block && block !== root; i++) {
      if (vtxt(block).length > title.length + 30 && vtxt(block).length < 2500) break;
      block = block.parentElement;
    }
    if (block && block !== root) {
      const cite = block.querySelector('cite');
      if (cite && vtxt(cite)) {
        const du = vtxt(cite).split(/\s*[›>·]\s*/)[0].trim();
        if (du) o.display_url = du.slice(0, 120);
      }
      // Extract snippet: longest non-heading, non-cite text block in result card.
      const bt = vtxt(block);
      const citeTxt = cite ? vtxt(cite) : '';
      let snip = bt.replace(title, '').replace(citeTxt, '').replace(/\s+/g, ' ').trim();
      if (snip.length > 20) o.snippet = snip.slice(0, 320);
      if (isSponsored(block)) o.sponsored = true;
    }
    items.push(o);
  }
  if (items.length > 0) {
    const q = new URLSearchParams(location.search).get('q') || '';
    const out = { container: 'google-search', count: items.length, items };
    if (q) out.query = q;
    if (unwrapped > 0) out.redirects_unwrapped = unwrapped;
    if (unresolved > 0) {
      out.redirects_unresolved = unresolved;
      out.note = unresolved + ' opaque Google redirect URL(s); display_url shows the target domain';
    }
    return JSON.stringify(out);
  }
}

if (hostMatches('duckduckgo.com')) {
  const spaCards = [...document.querySelectorAll('article[data-testid="result"]')].filter(isShown);
  const htmlCards = spaCards.length === 0 ? [...document.querySelectorAll('.results .result, .web-result')].filter(isShown) : [];
  const cards = spaCards.length > 0 ? spaCards : htmlCards;
  if (cards.length >= 1) {
    const seenUrls = new Set();
    const items = [];
    let unwrapped = 0;
    for (const card of cards) {
      if (items.length >= __LIMIT__) break;
      const a = card.querySelector('a[data-testid="result-title-a"], a.result__a, h2 a[href]');
      if (!a || typeof a.href !== 'string') continue;
      const title = vtxt(a);
      if (!title) continue;
      const rawHref = a.href;
      const cu = canonicalURL(rawHref);
      const key = cu.split('#')[0];
      if (seenUrls.has(key)) continue;
      seenUrls.add(key);
      const o = { title: title.slice(0, 200), url: cu };
      if (cu !== rawHref) unwrapped++;
      try {
        o.domain = new URL(cu).hostname;
      } catch (e) {}
      const snip = card.querySelector('[data-result="snippet"], .result__snippet');
      if (snip && vtxt(snip)) o.snippet = vtxt(snip).slice(0, 320);
      if (isSponsored(card)) o.sponsored = true;
      items.push(o);
    }
    if (items.length > 0) {
      const q = new URLSearchParams(location.search).get('q') || document.querySelector('input[name="q"]')?.value || '';
      const out = { container: 'ddg-search', count: items.length, items };
      if (q) out.query = q;
      if (unwrapped > 0) out.redirects_unwrapped = unwrapped;
      return JSON.stringify(out);
    }
  }
}

if (hostMatches('bing.com') && location.pathname === '/search') {
  const cards = [...document.querySelectorAll('li.b_algo')].filter(isShown);
  if (cards.length >= 1) {
    const seenUrls = new Set();
    const items = [];
    let unwrapped = 0, unresolved = 0;
    for (const card of cards) {
      if (items.length >= __LIMIT__) break;
      const a = card.querySelector('h2 a[href]');
      if (!a || typeof a.href !== 'string') continue;
      const title = vtxt(a);
      if (!title) continue;
      const rawHref = a.href;
      const cu = canonicalURL(rawHref);
      const key = cu.split('#')[0];
      if (seenUrls.has(key)) continue;
      seenUrls.add(key);
      const o = { title: title.slice(0, 200), url: cu };
      if (cu !== rawHref) unwrapped++;
      if (wrappedURL(cu)) {
        o.url_wrapped = true;
        unresolved++;
      } else {
        try {
          o.domain = new URL(cu).hostname;
        } catch (e) {}
      }
      const cite = card.querySelector('cite');
      if (!o.domain && cite && vtxt(cite)) o.display_url = vtxt(cite).slice(0, 120);
      const snip = card.querySelector('.b_caption p, p');
      if (snip && vtxt(snip)) o.snippet = vtxt(snip).slice(0, 320);
      items.push(o);
    }
    if (items.length > 0) {
      const q = new URLSearchParams(location.search).get('q') || '';
      const out = { container: 'bing-search', count: items.length, items };
      if (q) out.query = q;
      if (unwrapped > 0) out.redirects_unwrapped = unwrapped;
      if (unresolved > 0) out.redirects_unresolved = unresolved;
      return JSON.stringify(out);
    }
  }
}
