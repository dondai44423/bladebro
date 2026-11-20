// Amazon search results adapter across all Amazon TLDs (/s?k=...).
const IS_AMAZON_SEARCH = /(?:^|\.)amazon\.[a-z.]+$/.test(HOST) && (location.pathname === '/s' || location.pathname.startsWith('/s/'));
if (IS_AMAZON_SEARCH) {
  const cards = [...document.querySelectorAll('[data-component-type="s-search-result"][data-asin]')].filter(el => {
    const asin = (el.getAttribute('data-asin') || '').trim();
    return asin.length >= 6 && isShown(el);
  });
  if (cards.length >= 1) {
    const seenAsin = new Set();
    const items = [];
    let sponsored = 0;
    for (const c of cards) {
      if (items.length >= __LIMIT__) break;
      const asin = (c.getAttribute('data-asin') || '').trim();
      if (seenAsin.has(asin)) continue;
      seenAsin.add(asin);
      const h2 = c.querySelector('h2');
      const title = h2 ? vtxt(h2) : '';
      if (!title) continue;
      const o = {
        asin,
        title: title.slice(0, 220),
        url: location.origin + '/dp/' + asin,
      };
      // Price: .a-price .a-offscreen holds exact formatted text in textContent (innerText can be empty).
      const priceEl = c.querySelector('.a-price:not(.a-text-price) .a-offscreen');
      const priceTxt = priceEl ? tcnt(priceEl) : '';
      const pm = priceTxt.match(PRICE);
      if (pm) {
        o.price = pm[0];
      } else {
        const sym = tcnt(c.querySelector('.a-price:not(.a-text-price) .a-price-symbol'));
        const whole = tcnt(c.querySelector('.a-price:not(.a-text-price) .a-price-whole')).replace(/\.$/, '');
        const frac = tcnt(c.querySelector('.a-price:not(.a-text-price) .a-price-fraction'));
        if (sym && whole) o.price = sym + whole + (frac ? '.' + frac : '');
      }
      const origEl = c.querySelector('.a-text-price .a-offscreen');
      const origM = origEl ? tcnt(origEl).match(PRICE) : null;
      if (origM && origM[0] !== o.price) o.original_price = origM[0];

      const starEl = c.querySelector('i[class*="a-icon-star"] span.a-icon-alt, [aria-label*="out of 5"]');
      const starTxt = starEl ? (starEl.getAttribute('aria-label') || tcnt(starEl)) : '';
      const sm = starTxt.match(/(\d+\.?\d*)\s*out of\s*5/i);
      if (sm) o.rating = parseFloat(sm[1]);

      const revEl = c.querySelector('a[href*="#customerReviews"] span, span[aria-label*="ratings"], a[aria-label*="ratings"]');
      if (revEl) {
        const rm = (tcnt(revEl) || revEl.getAttribute('aria-label') || '').match(/(\d[\d.,]*[KkMm]?)/);
        if (rm) {
          const rn = parseCount(rm[1]);
          if (rn !== null) o.reviews = rn;
        }
      }
      const full = vtxt(c);
      const bought = full.match(/(\d[\d.,]*[KkMm]?\+?\s+bought\s+in\s+past\s+\w+)/i);
      if (bought) o.bought = bought[1];
      const del = deliveryOf(c);
      if (del) o.delivery = del;
      const img = c.querySelector('img.s-image');
      if (img && img.src) o.image = img.src;
      const isSp = !!c.querySelector('.puis-sponsored-label-text, [aria-label*="Sponsored"]') || /\bsponsored\b/i.test(full.slice(0, 120));
      if (isSp) {
        o.sponsored = true;
        sponsored++;
      }
      items.push(o);
    }
    if (items.length > 0) {
      const q = new URLSearchParams(location.search).get('k') || '';
      const out = { container: 'amazon-search', count: items.length, items };
      if (q) out.query = q;
      if (sponsored > 0) out.sponsored = sponsored;
      const claim = pageClaim();
      if (claim) {
        out.page_total = claim.total;
        if (claim.total > items.length) {
          out.partial = true;
          if (claim.paged) out.paged = true;
        }
      }
      return JSON.stringify(out);
    }
  }
}
