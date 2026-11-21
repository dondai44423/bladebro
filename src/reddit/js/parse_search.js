// Reddit search-page HTML/DOM parser (Shreddit SDUI + old.reddit.com).
// Pure & observational: works identically on live `document` and detached
// `DOMParser` HTML fragments without calling JSON APIs.
function parseRedditSearch(root, opts) {
  opts = opts || {};
  const origin = opts.origin || (typeof location !== 'undefined' && location.origin) || 'https://www.reddit.com';
  const pageHref = opts.url || (typeof location !== 'undefined' && location.href) || (origin + '/search/');

  function parseCount(v) {
    if (v === null || v === undefined || v === '') return null;
    const s = String(v).trim().replace(/,/g, '');
    const m = s.match(/^(-?\d+(?:\.\d+)?)\s*([kKmMbB])?/);
    if (!m) return null;
    let n = parseFloat(m[1]);
    if (!isFinite(n)) return null;
    const suf = (m[2] || '').toLowerCase();
    if (suf === 'k') n *= 1000;
    else if (suf === 'm') n *= 1000000;
    else if (suf === 'b') n *= 1000000000;
    return Math.round(n);
  }

  function cleanAuthor(raw) {
    const a = String(raw || '').trim().replace(/^\/?u\//i, '');
    if (!a || /^\[?(deleted|removed)\]?$/i.test(a)) return '[deleted]';
    return 'u/' + a;
  }

  function cleanSub(raw) {
    const s = String(raw || '').trim().replace(/^\/?r\//i, '');
    return s ? ('r/' + s) : '';
  }

  function toAbsUrl(href) {
    if (!href) return '';
    const h = String(href).trim();
    if (!h || h.charAt(0) === '#' || /^javascript:/i.test(h)) return '';
    try {
      return new URL(h, pageHref).href;
    } catch (e) {
      return '';
    }
  }

  function toSameOriginPath(href) {
    if (!href) return '';
    const h = String(href).trim();
    if (!h || h.charAt(0) === '#' || /^javascript:/i.test(h)) return '';
    try {
      const u = new URL(h, pageHref);
      const base = new URL(origin);
      if (u.origin !== base.origin) return '';
      return u.pathname + u.search;
    } catch (e) {
      return h.charAt(0) === '/' ? h : '';
    }
  }

  function textOf(el) {
    if (!el) return '';
    const parts = [];
    let nodes = 0;
    function walk(n) {
      if (++nodes > 6000) return;
      if (n.nodeType === 3) {
        if (n.textContent) parts.push(n.textContent);
        return;
      }
      if (n.nodeType !== 1) return;
      const tag = n.tagName || '';
      if (tag === 'SCRIPT' || tag === 'STYLE' || tag === 'NOSCRIPT' || tag === 'SVG' || tag === 'TEMPLATE') return;
      if (n.hasAttribute && n.hasAttribute('hidden')) return;
      if (tag === 'BR') { parts.push(' '); return; }
      const kids = n.childNodes || [];
      for (let i = 0; i < kids.length; i++) walk(kids[i]);
    }
    walk(el);
    return parts.join(' ').replace(/\s+/g, ' ').trim();
  }

  function isPromoted(el) {
    if (!el) return false;
    if (el.tagName === 'SHREDDIT-AD-POST') return true;
    if (el.hasAttribute && (el.hasAttribute('is-promoted') || el.hasAttribute('data-promoted'))) return true;
    if (el.classList && el.classList.contains('promoted')) return true;
    const dt = (el.getAttribute && el.getAttribute('data-testid')) || '';
    if (/(?:^|[-_])ad(?:[-_]|$)|promoted/i.test(dt)) return true;
    if (el.querySelector && el.querySelector('shreddit-ad-post, [data-testid*="promoted"], [data-testid*="ad-badge"]')) return true;
    return false;
  }

  const items = [];
  let skippedAds = 0;
  const seenUrls = new Set();

  function pushItem(it) {
    if (!it || !it.title || !it.url) return;
    const key = it.url.split('#')[0].replace(/\/+$/, '');
    if (seenUrls.has(key)) return;
    seenUrls.add(key);
    items.push(it);
  }

  // ── 1. Shreddit SDUI search units ───────────────────────────────────
  const units = [
    ...root.querySelectorAll('[data-testid="search-post-unit"], [data-testid="sdui-post-unit"], search-telemetry-tracker[data-testid*="search-post"], shreddit-post'),
  ];
  if (units.length > 0) {
    for (let i = 0; i < units.length; i++) {
      const u = units[i];
      if (isPromoted(u)) {
        skippedAds++;
        continue;
      }
      if (u.tagName === 'SHREDDIT-POST') {
        const a = n => u.getAttribute(n) || '';
        const title = a('post-title').trim();
        const url = toAbsUrl(a('permalink'));
        const sub = cleanSub(a('subreddit-prefixed-name') || a('subreddit-name'));
        const author = cleanAuthor(a('author'));
        const score = parseCount(a('score'));
        const comments = parseCount(a('comment-count'));
        const date = a('created-timestamp') || null;
        const domain = a('domain') || null;
        pushItem({ title, url, subreddit: sub, author, score, comments, date, domain, text: null });
        continue;
      }

      const titleLink =
        u.querySelector('a[data-testid="post-title"], a[data-testid="post-title-text"], [data-testid="post-title"] a[href], a[href*="/comments/"]');
      const rawTitle = titleLink ? textOf(titleLink) : '';
      const ariaTitle = titleLink ? (titleLink.getAttribute('aria-label') || '') : '';
      const title = (rawTitle || ariaTitle).trim();
      const url = titleLink ? toAbsUrl(titleLink.getAttribute('href')) : '';
      if (!title || !url) continue;

      let subreddit = '';
      const subLink = u.querySelector('a[href^="/r/"]:not([href*="/comments/"]), a[data-testid="subreddit-name"]');
      if (subLink) {
        const hm = (subLink.getAttribute('href') || '').match(/\/r\/([A-Za-z0-9_+-]+)/);
        subreddit = cleanSub(hm ? hm[1] : textOf(subLink));
      }
      if (!subreddit) {
        const um = url.match(/\/r\/([A-Za-z0-9_+-]+)/);
        if (um) subreddit = cleanSub(um[1]);
      }

      let author = '[deleted]';
      const auLink = u.querySelector('a[href*="/user/"], a[href*="/u/"]');
      if (auLink) {
        const am = (auLink.getAttribute('href') || '').match(/\/(?:user|u)\/([A-Za-z0-9_-]+)/);
        author = cleanAuthor(am ? am[1] : textOf(auLink));
      }

      const full = textOf(u);
      let score = null;
      const counterRow = u.querySelector('[data-testid="search-counter-row"]');
      const counterTxt = counterRow ? textOf(counterRow) : full;
      const sm = counterTxt.match(/(-?\d[\d.,]*\s*[kKmMbB]?)\s*(?:upvotes?|votes?|points?)/i);
      if (sm) score = parseCount(sm[1]);

      let comments = null;
      const cm = counterTxt.match(/(\d[\d.,]*\s*[kKmMbB]?)\s*comments?/i);
      if (cm) comments = parseCount(cm[1]);

      const tm = u.querySelector('faceplate-timeago[ts], time[datetime]');
      const date = tm ? (tm.getAttribute('ts') || tm.getAttribute('datetime') || null) : null;

      let text = null;
      const sn = u.querySelector('[data-testid="search-post-snippet"], [data-testid="post-snippet"], p');
      if (sn) {
        const st = textOf(sn);
        if (st && st !== title && st.length > 5) text = st;
      }

      let domain = null;
      const dm = u.querySelector('a[data-testid="outbound-link"]');
      if (dm) {
        try { domain = new URL(dm.getAttribute('href'), pageHref).hostname; } catch (e) {}
      }

      pushItem({ title, url, subreddit, author, score, comments, date, domain, text });
    }
  } else {
    // ── 2. old.reddit.com search results ──────────────────────────────
    const oldUnits = [...root.querySelectorAll('.search-result.search-result-link, #siteTable .thing.link')];
    for (let i = 0; i < oldUnits.length; i++) {
      const el = oldUnits[i];
      if (isPromoted(el)) {
        skippedAds++;
        continue;
      }
      const tl = el.querySelector('a.search-title, a.title');
      const title = tl ? textOf(tl) : '';
      const rawHref = el.getAttribute('data-permalink') || (tl ? tl.getAttribute('href') : '');
      const url = toAbsUrl(rawHref);
      if (!title || !url) continue;

      const subEl = el.querySelector('a.search-subreddit-link');
      const subAttr = el.getAttribute('data-subreddit') || (subEl ? textOf(subEl) : '');
      const subreddit = cleanSub(subAttr || ((url.match(/\/r\/([A-Za-z0-9_+-]+)/) || [])[1] || ''));
      const auEl = el.querySelector('a.author');
      const author = cleanAuthor(el.getAttribute('data-author') || (auEl ? textOf(auEl) : ''));
      const scEl = el.querySelector('.search-score');
      const score = parseCount(el.getAttribute('data-score') || (scEl ? textOf(scEl) : ''));
      const cmEl = el.querySelector('a.search-comments');
      const comments = parseCount(el.getAttribute('data-comments-count') || (cmEl ? textOf(cmEl) : ''));
      const tmEl = el.querySelector('time[datetime]');
      const date = tmEl ? (tmEl.getAttribute('datetime') || null) : null;
      const domain = el.getAttribute('data-domain') || null;
      const snEl = el.querySelector('.search-result-body');
      const snTxt = snEl ? textOf(snEl) : '';
      const text = snTxt && snTxt !== title ? snTxt : null;

      pushItem({ title, url, subreddit, author, score, comments, date, domain, text });
    }
  }

  // ── 3. Next-page pagination control (Shreddit partial or old.reddit) ─
  let nextUrl = null;
  const partials = [...root.querySelectorAll('faceplate-partial[src]')];
  for (let i = 0; i < partials.length; i++) {
    const fp = partials[i];
    const method = (fp.getAttribute('method') || '').trim().toLowerCase();
    if (method && method !== 'get') continue;
    const path = toSameOriginPath(fp.getAttribute('src'));
    if (path && /\/svc\/shreddit\/search\b/i.test(path)) {
      nextUrl = path;
      break;
    }
  }
  if (!nextUrl) {
    const nextA = root.querySelector('.nextprev a[rel~="next"], a.next-button, .next-button a[href]');
    if (nextA) {
      const path = toSameOriginPath(nextA.getAttribute('href'));
      if (path) nextUrl = path;
    }
  }

  return {
    items,
    skipped_ads: skippedAds,
    next_url: nextUrl,
  };
}
